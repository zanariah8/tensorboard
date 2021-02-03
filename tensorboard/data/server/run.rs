/* Copyright 2020 The TensorFlow Authors. All Rights Reserved.

Licensed under the Apache License, Version 2.0 (the "License");
you may not use this file except in compliance with the License.
You may obtain a copy of the License at

    http://www.apache.org/licenses/LICENSE-2.0

Unless required by applicable law or agreed to in writing, software
distributed under the License is distributed on an "AS IS" BASIS,
WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
See the License for the specific language governing permissions and
limitations under the License.
==============================================================================*/

//! Loader for a single run, with one or more event files.

use log::{debug, warn};
use std::collections::{BTreeMap, HashMap, HashSet};
use std::io::Read;
use std::sync::RwLock;
use std::time::{Duration, Instant};

use crate::commit;
use crate::data_compat::{EventValue, GraphDefValue, SummaryValue, TaggedRunMetadataValue};
use crate::event_file::EventFileReader;
use crate::logdir::{EventFileBuf, Logdir};
use crate::proto::tensorboard as pb;
use crate::reservoir::StageReservoir;
use crate::types::{Run, Step, Tag, WallTime};

/// A loader to accumulate reservoir-sampled events in a single TensorBoard run.
///
/// The type `R` represents the types of files read from disk, as in the
/// [`Logdir::File`][crate::logdir::Logdir::File] associated type.
#[derive(Debug)]
pub struct RunLoader<R> {
    /// The run name associated with this loader. Used primarily for logging; the run name is
    /// canonically defined by the map key under which this `RunLoader` is stored in `LogdirLoader`.
    run: Run,
    /// The event files in this run.
    ///
    /// Event files are sorted and read lexicographically by name, which is designed to coincide
    /// with actual start time. See [`EventFile::Dead`] for conditions under which an event file
    /// may be dead. Once an event file is added to this map, it may become dead, but it will not
    /// be removed entirely. This way, we know not to just re-open it again at the next load cycle.
    files: BTreeMap<EventFileBuf, EventFile<R>>,

    /// Whether to compute CRCs for records before parsing as protos.
    checksum: bool,

    /// The data staged by this `RunLoader`. This is encapsulated in a sub-struct so that these
    /// fields can be reborrowed within `reload_files` in a context that already has an exclusive
    /// reference into `self.files`, and hence can't call methods on the whole of `&mut self`.
    data: RunLoaderData,
}

#[derive(Debug)]
enum EventFile<R> {
    /// An event file that may still have more valid data.
    Active(EventFileReader<R>),
    /// An event file that can no longer be read.
    ///
    /// This can be because of a non-recoverable read error (e.g., a bad length checksum), due to
    /// the last-read record being very old (note: not yet implemented), or due to the file being
    /// deleted.
    Dead,
}

/// Holds data staged by a `RunLoader` that will be committed to the `Commit`.
#[derive(Debug, Default)]
struct RunLoaderData {
    /// The earliest event `wall_time` seen in any event file in this run.
    ///
    /// This is `None` if and only if no events have been seen. Its value may decrease as new
    /// events are read, but in practice this is expected to be the wall time of the first
    /// `file_version` event in the first event file.
    start_time: Option<WallTime>,

    /// Reservoir-sampled data and metadata for each time series.
    time_series: HashMap<Tag, StageTimeSeries>,
}

#[derive(Debug)]
struct StageTimeSeries {
    data_class: pb::DataClass,
    metadata: Box<pb::SummaryMetadata>,
    rsv: StageReservoir<StageValue>,
}

/// A value staged in the reservoir.
///
/// This is kept as close as possible to the on-disk event representation, since every record in
/// the stream is converted into this format.
#[derive(Debug)]
struct StageValue {
    wall_time: WallTime,
    payload: EventValue,
}

impl StageTimeSeries {
    fn new(metadata: Box<pb::SummaryMetadata>) -> Self {
        let data_class =
            pb::DataClass::from_i32(metadata.data_class).unwrap_or(pb::DataClass::Unknown);
        let capacity = match data_class {
            pb::DataClass::Scalar => 1000,
            pb::DataClass::Tensor => 100,
            pb::DataClass::BlobSequence => 10,
            _ => 0,
        };
        Self {
            data_class,
            metadata,
            rsv: StageReservoir::new(capacity),
        }
    }

    /// Writes all staged data for this time series into the commit.
    fn commit(&mut self, tag: &Tag, run: &mut commit::RunData) {
        use pb::DataClass;
        match self.data_class {
            DataClass::Scalar => self.commit_to(tag, &mut run.scalars, |ev, _| ev.into_scalar()),
            DataClass::Tensor => {
                warn!(
                    "Tensor time series not yet supported (tag: {:?}, plugin: {:?})",
                    tag.0,
                    self.metadata
                        .plugin_data
                        .as_ref()
                        .map(|p| p.plugin_name.as_str())
                        .unwrap_or("")
                );
            }
            DataClass::BlobSequence => {
                self.commit_to(tag, &mut run.blob_sequences, EventValue::into_blob_sequence)
            }
            _ => (),
        };
    }

    /// Helper for `commit`: writes staged data for this time series into storage for a statically
    /// known data class.
    fn commit_to<V, F: FnMut(EventValue, &pb::SummaryMetadata) -> Result<V, commit::DataLoss>>(
        &mut self,
        tag: &Tag,
        store: &mut commit::TagStore<V>,
        mut enrich: F,
    ) {
        let commit_ts = store
            .entry(tag.clone())
            .or_insert_with(|| commit::TimeSeries::new(self.metadata.clone()));
        let metadata = self.metadata.as_ref();
        self.rsv
            .commit_map(&mut commit_ts.basin, |StageValue { wall_time, payload }| {
                (wall_time, enrich(payload, metadata))
            });
    }
}

/// Minimum time to wait between committing while a run is still loading.
const COMMIT_INTERVAL: Duration = Duration::from_secs(5);

impl<R: Read> RunLoader<R> {
    pub fn new(run: Run) -> Self {
        Self {
            run,
            files: BTreeMap::new(),
            checksum: true,
            data: RunLoaderData::default(),
        }
    }

    /// Sets whether to compute checksums for records before parsing them as protos.
    pub fn checksum(&mut self, yes: bool) {
        self.checksum = yes;
    }

    /// Loads new data given the current set of event files.
    ///
    /// The provided filenames should correspond to the entire set of event files currently part of
    /// this run.
    ///
    /// The given commit must have an entry for this run (the entry may be empty).
    ///
    /// # Panics
    ///
    /// If we need to access `run_data` but the lock is poisoned.
    pub fn reload(
        &mut self,
        logdir: &impl Logdir<File = R>,
        filenames: Vec<EventFileBuf>,
        run_data: &RwLock<commit::RunData>,
    ) {
        let run_name = self.run.0.clone();
        debug!("Starting load for run {:?}", run_name);
        let start = Instant::now();
        self.update_file_set(logdir, filenames);
        let mut n = 0;
        let mut last_commit_time = Instant::now();
        self.reload_files(|run_loader_data, event| {
            run_loader_data.read_event(event);
            n += 1;
            // Reduce overhead of checking elapsed time by only doing it every 100 events.
            if n % 100 == 0 && last_commit_time.elapsed() >= COMMIT_INTERVAL {
                debug!(
                    "Loaded {} events for run {:?} after {:?}",
                    n,
                    run_name,
                    start.elapsed()
                );
                run_loader_data.commit_all(run_data);
                last_commit_time = Instant::now();
            }
        });
        self.data.commit_all(run_data);
        debug!(
            "Finished load for run {:?} ({:?})",
            run_name,
            start.elapsed()
        );
    }

    /// Updates the active key set of `self.files` to match the given filenames.
    ///
    /// After this function returns, `self.files` may still have keys not in `filenames`, but they
    /// will all map to [`EventFile::Dead`].
    fn update_file_set(&mut self, logdir: &impl Logdir<File = R>, filenames: Vec<EventFileBuf>) {
        // Remove any discarded files.
        let new_file_set: HashSet<&EventFileBuf> = filenames.iter().collect();
        for (k, v) in self.files.iter_mut() {
            if !new_file_set.contains(k) {
                *v = EventFile::Dead;
            }
        }

        // Open readers for any new files.
        for filename in filenames {
            use std::collections::btree_map::Entry;
            match self.files.entry(filename) {
                Entry::Occupied(_) => {}
                Entry::Vacant(v) => {
                    let event_file = match logdir.open(v.key()) {
                        Ok(file) => {
                            let mut reader = EventFileReader::new(file);
                            reader.checksum(self.checksum);
                            EventFile::Active(reader)
                        }
                        // TODO(@wchargin): Improve error handling?
                        Err(e) => {
                            warn!("Failed to open event file {:?}: {:?}", v.key(), e);
                            EventFile::Dead
                        }
                    };
                    v.insert(event_file);
                }
            };
        }
    }

    /// Reads data from all active event files, and calls a handler for each event.
    fn reload_files<F: FnMut(&mut RunLoaderData, pb::Event)>(&mut self, mut handle_event: F) {
        for (filename, ef) in self.files.iter_mut() {
            let reader = match ef {
                EventFile::Dead => continue,
                EventFile::Active(reader) => reader,
            };

            loop {
                use crate::event_file::ReadEventError::ReadRecordError;
                use crate::tf_record::ReadRecordError::Truncated;
                let event = match reader.read_event() {
                    Ok(event) => event,
                    Err(ReadRecordError(Truncated)) => break,
                    Err(e) => {
                        // TODO(@wchargin): Improve error handling?
                        warn!("Read error in {}: {:?}", filename.0.display(), e);
                        *ef = EventFile::Dead;
                        break;
                    }
                };
                handle_event(&mut self.data, event);
            }
        }
    }
}

impl RunLoaderData {
    /// Commits all staged data into the given run of the commit.
    fn commit_all(&mut self, run_data: &RwLock<commit::RunData>) {
        let mut run = run_data.write().expect("acquiring tags lock");
        run.start_time = self.start_time;
        for (tag, ts) in &mut self.time_series {
            ts.commit(tag, &mut *run);
        }
    }

    /// Reads a single event and stages it for future committing.
    fn read_event(&mut self, e: pb::Event) {
        let step = Step(e.step);
        let wall_time = match WallTime::new(e.wall_time) {
            None => {
                // TODO(@wchargin): Improve error handling.
                warn!(
                    "Dropping event at step {} with invalid wall time {}",
                    e.step, e.wall_time
                );
                return;
            }
            Some(wt) => wt,
        };
        if self.start_time.map_or(true, |start| wall_time < start) {
            self.start_time = Some(wall_time);
        }
        match e.what {
            Some(pb::event::What::GraphDef(graph_bytes)) => {
                let sv = StageValue {
                    wall_time,
                    payload: EventValue::GraphDef(GraphDefValue(graph_bytes)),
                };
                use std::collections::hash_map::Entry;
                let ts = match self
                    .time_series
                    .entry(Tag(GraphDefValue::TAG_NAME.to_string()))
                {
                    Entry::Occupied(o) => o.into_mut(),
                    Entry::Vacant(v) => {
                        v.insert(StageTimeSeries::new(GraphDefValue::initial_metadata()))
                    }
                };
                ts.rsv.offer(step, sv);
            }
            Some(pb::event::What::TaggedRunMetadata(trm_proto)) => {
                let sv = StageValue {
                    wall_time,
                    payload: EventValue::GraphDef(GraphDefValue(trm_proto.run_metadata)),
                };
                use std::collections::hash_map::Entry;
                let ts = match self.time_series.entry(Tag(trm_proto.tag)) {
                    Entry::Occupied(o) => o.into_mut(),
                    Entry::Vacant(v) => {
                        let metadata = TaggedRunMetadataValue::initial_metadata();
                        v.insert(StageTimeSeries::new(metadata))
                    }
                };
                ts.rsv.offer(step, sv);
            }
            Some(pb::event::What::Summary(sum)) => {
                for mut summary_pb_value in sum.value {
                    let summary_value = match summary_pb_value.value {
                        None => continue,
                        Some(v) => SummaryValue(Box::new(v)),
                    };

                    use std::collections::hash_map::Entry;
                    let ts = match self.time_series.entry(Tag(summary_pb_value.tag)) {
                        Entry::Occupied(o) => o.into_mut(),
                        Entry::Vacant(v) => {
                            let metadata =
                                summary_value.initial_metadata(summary_pb_value.metadata.take());
                            v.insert(StageTimeSeries::new(metadata))
                        }
                    };
                    let sv = StageValue {
                        wall_time,
                        payload: EventValue::Summary(summary_value),
                    };
                    ts.rsv.offer(step, sv);
                }
            }
            _ => {}
        }
    }
}

#[cfg(test)]
mod test {
    use super::*;
    use std::fs::File;
    use std::io::BufWriter;

    use crate::commit::Commit;
    use crate::data_compat::plugin_names;
    use crate::disk_logdir::DiskLogdir;
    use crate::types::Run;
    use crate::writer::SummaryWriteExt;

    #[test]
    fn test() -> Result<(), Box<dyn std::error::Error>> {
        let logdir = tempfile::tempdir()?;
        let f1_name = logdir.path().join("tfevents.123");
        let f2_name = logdir.path().join("tfevents.456");
        let mut f1 = BufWriter::new(File::create(&f1_name)?);
        let mut f2 = BufWriter::new(File::create(&f2_name)?);

        // Write file versions.
        for (f, wall_time) in &mut [(&mut f1, 1234.0), (&mut f2, 2345.0)] {
            let file_version = pb::Event {
                wall_time: *wall_time,
                what: Some(pb::event::What::FileVersion("brain.Event:2".to_string())),
                ..Default::default()
            };
            f.write_event(&file_version)?;
        }

        // Write some data points across both files.
        let run = Run("train".to_string());
        let tag = Tag("accuracy".to_string());
        f1.write_graph(
            Step(0),
            WallTime::new(1235.0).unwrap(),
            b"<sample model graph>".to_vec(),
        )?;
        f1.write_tagged_run_metadata(
            &Tag("step0000".to_string()),
            Step(0),
            WallTime::new(1235.0).unwrap(),
            b"<sample run metadata>".to_vec(),
        )?;
        f1.write_scalar(&tag, Step(0), WallTime::new(1235.0).unwrap(), 0.25)?;
        f1.write_scalar(&tag, Step(1), WallTime::new(1236.0).unwrap(), 0.50)?;
        f1.write_scalar(&tag, Step(2), WallTime::new(1237.0).unwrap(), 0.75)?;
        f1.write_scalar(&tag, Step(3), WallTime::new(1238.0).unwrap(), 1.00)?;
        // preempt!
        f2.write_scalar(&tag, Step(2), WallTime::new(2346.0).unwrap(), 0.70)?;
        f2.write_scalar(&tag, Step(3), WallTime::new(2347.0).unwrap(), 0.85)?;
        f2.write_scalar(&tag, Step(4), WallTime::new(2348.0).unwrap(), 0.90)?;
        // flush, so that the data's there when we read it
        f1.into_inner()?.sync_all()?;
        f2.into_inner()?.sync_all()?;

        let mut loader = RunLoader::new(run.clone());
        let logdir = DiskLogdir::new(logdir.path().to_path_buf());
        let commit = Commit::new();
        commit
            .runs
            .write()
            .expect("write-locking runs map")
            .insert(run.clone(), Default::default());
        loader.reload(
            &logdir,
            vec![EventFileBuf(f1_name), EventFileBuf(f2_name)],
            &commit.runs.read().unwrap()[&run],
        );

        // Start time should be that of the file version event, even though that didn't correspond
        // to any time series.
        assert_eq!(loader.data.start_time, Some(WallTime::new(1234.0).unwrap()));

        let runs = commit.runs.read().expect("read-locking runs map");
        let run_data: &commit::RunData = &*runs
            .get(&run)
            .expect("looking up data for run")
            .read()
            .expect("read-locking run data map");

        assert_eq!(run_data.scalars.keys().collect::<Vec<_>>(), vec![&tag]);
        let scalar_ts = run_data.scalars.get(&tag).unwrap();
        assert_eq!(
            *scalar_ts.metadata,
            pb::SummaryMetadata {
                plugin_data: Some(pb::summary_metadata::PluginData {
                    plugin_name: plugin_names::SCALARS.to_string(),
                    ..Default::default()
                }),
                data_class: pb::DataClass::Scalar.into(),
                ..Default::default()
            }
        );
        // Points should be as expected (no downsampling at these sizes).
        let scalar = commit::ScalarValue;
        assert_eq!(
            scalar_ts.valid_values().collect::<Vec<_>>(),
            vec![
                (Step(0), WallTime::new(1235.0).unwrap(), &scalar(0.25)),
                (Step(1), WallTime::new(1236.0).unwrap(), &scalar(0.50)),
                (Step(2), WallTime::new(2346.0).unwrap(), &scalar(0.70)),
                (Step(3), WallTime::new(2347.0).unwrap(), &scalar(0.85)),
                (Step(4), WallTime::new(2348.0).unwrap(), &scalar(0.90)),
            ]
        );

        assert_eq!(run_data.blob_sequences.len(), 2);

        let run_graph_tag = Tag(GraphDefValue::TAG_NAME.to_string());
        let graph_ts = run_data.blob_sequences.get(&run_graph_tag).unwrap();
        assert_eq!(
            *graph_ts.metadata,
            pb::SummaryMetadata {
                plugin_data: Some(pb::summary_metadata::PluginData {
                    plugin_name: plugin_names::GRAPHS.to_string(),
                    ..Default::default()
                }),
                data_class: pb::DataClass::BlobSequence.into(),
                ..Default::default()
            }
        );
        assert_eq!(
            graph_ts.valid_values().collect::<Vec<_>>(),
            vec![(
                Step(0),
                WallTime::new(1235.0).unwrap(),
                &commit::BlobSequenceValue(vec![b"<sample model graph>".to_vec()])
            )]
        );

        let run_metadata_tag = Tag("step0000".to_string());
        let run_metadata_ts = run_data.blob_sequences.get(&run_metadata_tag).unwrap();
        assert_eq!(
            *run_metadata_ts.metadata,
            pb::SummaryMetadata {
                plugin_data: Some(pb::summary_metadata::PluginData {
                    plugin_name: plugin_names::GRAPH_TAGGED_RUN_METADATA.to_string(),
                    ..Default::default()
                }),
                data_class: pb::DataClass::BlobSequence.into(),
                ..Default::default()
            }
        );
        assert_eq!(
            run_metadata_ts.valid_values().collect::<Vec<_>>(),
            vec![(
                Step(0),
                WallTime::new(1235.0).unwrap(),
                &commit::BlobSequenceValue(vec![b"<sample run metadata>".to_vec()])
            )]
        );

        Ok(())
    }
}
