//
// Copyright (c) Dell Inc., or its subsidiaries. All Rights Reserved.
//
// Licensed under the Apache License, Version 2.0 (the "License");
// you may not use this file except in compliance with the License.
// You may obtain a copy of the License at
//
// http://www.apache.org/licenses/LICENSE-2.0
//

use crate::client_factory::ClientFactory;
use crate::error::*;
use crate::reader_group::reader_group_config::ReaderGroupConfigVersioned;
use crate::table_synchronizer::{deserialize_from, Table, TableSynchronizer, Value};
use pravega_rust_client_shared::{Reader, ScopedSegment, ScopedStream, Segment, SegmentWithRange};
use serde::{Deserialize, Serialize};
use snafu::ResultExt;
use snafu::{ensure, Snafu};
use std::collections::{HashMap, HashSet};
use std::iter::FromIterator;
use tracing::warn;

const ASSUMED_LAG_MILLIS: u64 = 30000;
const DEFAULT_INNER_KEY: &str = "default";

const ASSIGNED: &str = "assigned_segments";
const UNASSIGNED: &str = "unassigned_segments";
const FUTURE: &str = "future_segments";
const DISTANCE: &str = "distance_to_tail";

#[derive(Debug, Snafu)]
pub enum ReaderGroupStateError {
    #[snafu(display("Synchronizer error while performing {}: {}", error_msg, source))]
    SyncError {
        error_msg: String,
        source: SynchronizerError,
    },
}

/// ReaderGroupState encapsulates all readers states.
pub(crate) struct ReaderGroupState {
    /// The sync is a TableSynchronizer that provides API to read or write the internal
    /// reader group state stored on the server side. The internal reader group state contains
    /// the following fields.
    ///
    /// Internal stream that is used to store the ReaderGroupState on the server side.
    /// scoped_synchronizer_stream: ScopedStream,
    ///
    /// Reader group config
    /// config: ReaderGroupConfigVersioned
    ///
    /// This is used to balance the workload among readers in this reader group.
    /// distance_to_tail: HashMap<Reader, u64>
    ///
    /// Maps successor segments to their predecessors. A successor segment will ready to be
    /// read if all its predecessors have been read.
    /// future_segments: HashMap<SegmentWithRange, HashSet<i64>>
    ///
    /// Maps active readers to their currently assigned segments.
    /// assigned_segments:  HashMap<Reader, HashMap<SegmentWithRange, Offset>>
    ///
    /// Segments waiting to be assigned to readers.
    /// unassigned_segments: HashMap<SegmentWithRange, Offset>
    sync: TableSynchronizer,
}

impl ReaderGroupState {
    pub(crate) async fn new(
        scoped_synchronizer_stream: ScopedStream,
        client_facotry: &ClientFactory,
        config: ReaderGroupConfigVersioned,
        segments_to_offsets: HashMap<SegmentWithRange, Offset>,
    ) -> ReaderGroupState {
        let mut sync = client_facotry
            .create_table_synchronizer("ReaderGroupState".to_owned())
            .await;
        sync.insert(move |table| {
            if table.is_empty() {
                table.insert(
                    "scoped_synchronizer_stream".to_owned(),
                    DEFAULT_INNER_KEY.to_owned(),
                    "ScopedStream".to_owned(),
                    Box::new(scoped_synchronizer_stream.clone()),
                );
                table.insert(
                    "config".to_owned(),
                    DEFAULT_INNER_KEY.to_owned(),
                    "ReaderGroupConfigVersioned".to_owned(),
                    Box::new(config.clone()),
                );
                for (segment, offset) in &segments_to_offsets {
                    table.insert(
                        UNASSIGNED.to_owned(),
                        segment.to_string(),
                        "Offset".to_owned(),
                        Box::new(offset.to_owned()),
                    );
                }
            }
            Ok(None)
        })
        .await
        .expect("should initialize table synchronizer");
        ReaderGroupState { sync }
    }

    /// Adds a reader to the reader group state.
    pub(crate) async fn add_reader(&mut self, reader: &Reader) -> Result<(), ReaderGroupStateError> {
        let _res_str = self
            .sync
            .insert(|table| ReaderGroupState::add_reader_internal(table, reader))
            .await
            .context(SyncError {
                error_msg: format!("add reader {:?}", reader),
            })?;
        Ok(())
    }

    // Internal logic of add_reader method. Separate the actual logic with table synchronizer
    // to facilitate the unit test.
    fn add_reader_internal(table: &mut Table, reader: &Reader) -> Result<Option<String>, SynchronizerError> {
        if table.contains_key(ASSIGNED, &reader.to_string()) {
            return Err(SynchronizerError::SyncUpdateError {
                error_msg: format!("Failed to add online reader {:?}: reader already online", reader),
            });
        }

        // add new reader
        let empty_map: HashMap<SegmentWithRange, Offset> = HashMap::new();
        table.insert(
            ASSIGNED.to_owned(),
            reader.to_string(),
            "HashMap<SegmentWithRange, Offset>".to_owned(),
            Box::new(empty_map),
        );

        table.insert(
            "distance_to_tail".to_owned(),
            reader.to_string(),
            "u64".to_owned(),
            Box::new(u64::MAX),
        );
        Ok(None)
    }

    /// Returns the active readers in a vector.
    pub(crate) async fn get_online_readers(&mut self) -> Vec<Reader> {
        self.sync.fetch_updates().await.expect("should fetch updates");
        ReaderGroupState::get_online_readers_internal(self.sync.get_inner_map(ASSIGNED))
    }

    fn get_online_readers_internal(assigned_segments: HashMap<String, Value>) -> Vec<Reader> {
        assigned_segments
            .keys()
            .map(|k| Reader::from(k.to_owned()))
            .collect::<Vec<Reader>>()
    }

    /// Gets the latest positions for the given reader.
    pub(crate) async fn get_reader_positions(
        &mut self,
        reader: &Reader,
    ) -> Result<HashMap<SegmentWithRange, Offset>, SynchronizerError> {
        self.sync.fetch_updates().await.expect("should fetch updates");
        ReaderGroupState::get_reader_positions_internal(reader, self.sync.get_inner_map(ASSIGNED))
    }

    fn get_reader_positions_internal(
        reader: &Reader,
        assigned_segments: HashMap<String, Value>,
    ) -> Result<HashMap<SegmentWithRange, Offset>, SynchronizerError> {
        ReaderGroupState::check_reader_online(&assigned_segments, reader)?;

        Ok(deserialize_from(
            &assigned_segments
                .get(&reader.to_string())
                .expect("reader must exist")
                .data,
        )
        .expect("deserialize reader position"))
    }

    /// Updates the latest positions for the given reader.
    pub(crate) async fn update_reader_positions(
        &mut self,
        reader: &Reader,
        latest_positions: HashMap<SegmentWithRange, Offset>,
    ) -> Result<(), ReaderGroupStateError> {
        let _res_str = self
            .sync
            .insert(|table| {
                ReaderGroupState::update_reader_positions_internal(table, reader, &latest_positions)
            })
            .await
            .context(SyncError {
                error_msg: format!("update reader {:?} to position {:?}", reader, latest_positions),
            })?;
        Ok(())
    }

    fn update_reader_positions_internal(
        table: &mut Table,
        reader: &Reader,
        latest_positions: &HashMap<SegmentWithRange, Offset>,
    ) -> Result<Option<String>, SynchronizerError> {
        let mut owned_segments = ReaderGroupState::get_reader_owned_segments_from_table(table, reader)?;
        if owned_segments.len() != latest_positions.len() {
            warn!(
                "owned segments size {} dose not match latest positions size {}",
                owned_segments.len(),
                latest_positions.len()
            );
        }

        for (segment, offset) in latest_positions {
            owned_segments.entry(segment.to_owned()).and_modify(|v| {
                v.read = offset.read;
                v.processed = offset.processed;
            });
        }
        table.insert(
            ASSIGNED.to_owned(),
            reader.to_string(),
            "HashMap<SegmentWithRange, Offset>".to_owned(),
            Box::new(owned_segments),
        );
        Ok(None)
    }

    /// Removes the given reader from the reader group state and puts segments that are previously
    /// owned by the removed reader to the unassigned list for redistribution.
    pub(crate) async fn remove_reader(
        &mut self,
        reader: &Reader,
        owned_segments: HashMap<ScopedSegment, Offset>,
    ) -> Result<(), ReaderGroupStateError> {
        let _res_str = self
            .sync
            .insert(|table| ReaderGroupState::remove_reader_internal(table, reader, &owned_segments))
            .await
            .context(SyncError {
                error_msg: format!(
                    "remove reader {:?} and put owned segments {:?} to unassigned list",
                    reader, owned_segments
                ),
            })?;
        Ok(())
    }

    fn remove_reader_internal(
        table: &mut Table,
        reader: &Reader,
        owned_segments: &HashMap<ScopedSegment, Offset>,
    ) -> Result<Option<String>, SynchronizerError> {
        let assigned_segments = ReaderGroupState::get_reader_owned_segments_from_table(table, reader)?;

        for (segment, pos) in assigned_segments {
            // update offset using owned_segments
            let offset = owned_segments
                .get(&segment.scoped_segment)
                .map_or(pos, |v| v.to_owned());

            table.insert(
                UNASSIGNED.to_owned(),
                segment.to_string(),
                "Offset".to_owned(),
                Box::new(offset),
            );
        }
        table.insert_tombstone(ASSIGNED.to_owned(), reader.to_string())?;
        table.insert_tombstone(DISTANCE.to_owned(), reader.to_string())?;
        Ok(None)
    }

    /// Returns the list of all segments.
    pub(crate) async fn get_segments(&mut self) -> HashSet<ScopedSegment> {
        self.sync.fetch_updates().await.expect("should fetch updates");

        let assigned_segments = self.sync.get_inner_map(ASSIGNED);
        let unassigned_segments = self.sync.get_inner_map(UNASSIGNED);

        let mut set = HashSet::new();

        for v in assigned_segments.values() {
            let segments: HashMap<SegmentWithRange, Offset> =
                deserialize_from(&v.data).expect("deserialize assigned segments");
            set.extend(
                segments
                    .keys()
                    .map(|segment| segment.scoped_segment.clone())
                    .collect::<HashSet<ScopedSegment>>(),
            )
        }

        set.extend(
            unassigned_segments
                .keys()
                .map(|segment| {
                    let segment_str = &*segment.to_owned();
                    SegmentWithRange::from(segment_str).scoped_segment
                })
                .collect::<HashSet<ScopedSegment>>(),
        );
        set
    }

    /// Assigns an unassigned segment to a given reader
    pub(crate) async fn assign_segment_to_reader(
        &mut self,
        reader: &Reader,
    ) -> Result<Option<ScopedSegment>, ReaderGroupStateError> {
        let option = self
            .sync
            .insert(|table| ReaderGroupState::assign_segment_to_reader_internal(table, reader))
            .await
            .context(SyncError {
                error_msg: format!("assign segment to reader {:?}", reader),
            })?;

        if let Some(segment_str) = option {
            Ok(Some(ScopedSegment::from(&*segment_str)))
        } else {
            Ok(None)
        }
    }

    fn assign_segment_to_reader_internal(
        table: &mut Table,
        reader: &Reader,
    ) -> Result<Option<String>, SynchronizerError> {
        let mut assigned_segments = ReaderGroupState::get_reader_owned_segments_from_table(table, reader)?;
        let unassigned_segments = ReaderGroupState::get_unassigned_segments_from_table(table);

        // unassigned segment does not exist
        if unassigned_segments.is_empty() {
            return Ok(None);
        }

        // naive way to get an unassigned segment
        let mut segments = unassigned_segments
            .keys()
            .map(|k| k.to_owned())
            .collect::<Vec<SegmentWithRange>>();
        let segment = segments.pop().expect("should contain at least one key");
        let offset = unassigned_segments.get(&segment).expect("get offset");

        assigned_segments.insert(segment.clone(), offset.to_owned());

        table.insert(
            ASSIGNED.to_owned(),
            reader.to_string(),
            "HashMap<SegmentWithRange, Offset>".to_owned(),
            Box::new(assigned_segments),
        );
        table.insert_tombstone(UNASSIGNED.to_owned(), segment.to_string())?;

        Ok(Some(segment.scoped_segment.to_string()))
    }

    /// Returns the list of segments assigned to the requested reader.
    pub(crate) async fn get_segments_for_reader(
        &mut self,
        reader: &Reader,
    ) -> Result<HashSet<ScopedSegment>, SynchronizerError> {
        self.sync.fetch_updates().await.expect("should fetch updates");
        let value =
            self.sync
                .get(ASSIGNED, &reader.to_string())
                .ok_or(SynchronizerError::SyncUpdateError {
                    error_msg: format!("reader {} is not online", reader),
                })?;

        let segments: HashMap<SegmentWithRange, Offset> =
            deserialize_from(&value.data).expect("deserialize reader owned segments");
        Ok(segments
            .iter()
            .map(|(k, _v)| k.scoped_segment.clone())
            .collect::<HashSet<ScopedSegment>>())
    }

    /// Releases a currently assigned segment from the given reader.
    pub(crate) async fn release_segment(
        &mut self,
        reader: &Reader,
        segment: &ScopedSegment,
        offset: &Offset,
    ) -> Result<(), ReaderGroupStateError> {
        self.sync
            .insert(|table| ReaderGroupState::release_segment_internal(table, reader, segment, offset))
            .await
            .context(SyncError {
                error_msg: format!(
                    "release segment {:?} with offset {:?} from reader {:?} ",
                    segment, offset, reader
                ),
            })?;
        Ok(())
    }

    /// Find the corresponding segment in the assigned segment list.
    fn release_segment_internal(
        table: &mut Table,
        reader: &Reader,
        segment: &ScopedSegment,
        offset: &Offset,
    ) -> Result<Option<String>, SynchronizerError> {
        let mut assigned_segments = ReaderGroupState::get_reader_owned_segments_from_table(table, reader)?;
        let unassigned_segments = ReaderGroupState::get_unassigned_segments_from_table(table);

        let mut to_remove_list = assigned_segments
            .iter()
            .filter(|&(s, _pos)| s.scoped_segment == *segment)
            .map(|(s, _pos)| s.to_owned())
            .collect::<Vec<SegmentWithRange>>();

        ensure!(
            to_remove_list.len() == 1,
            SyncUpdateError {
                error_msg: format!(
                    "Failed to release segment: should contain only one segment {:?} in assigned list but contain {}",
                    segment,
                    to_remove_list.len()
                )
            }
        );

        let to_remove_segment = to_remove_list.pop().expect("pop found segment");

        ensure!(
            !unassigned_segments.contains_key(&to_remove_segment),
            SyncUpdateError {
                error_msg: format!(
                    "Failed to release segment:: unassigned_segment should not have already contained this released segment {:?}",
                    segment
                )
            }
        );

        assigned_segments
            .remove(&to_remove_segment)
            .expect("should contain the releasing segment");

        table.insert(
            ASSIGNED.to_owned(),
            reader.to_string(),
            "HashMap<SegmentWithRange, Offset>".to_owned(),
            Box::new(assigned_segments),
        );
        table.insert(
            UNASSIGNED.to_owned(),
            to_remove_segment.to_string(),
            "Offset".to_owned(),
            Box::new(offset.to_owned()),
        );
        Ok(None)
    }

    /// Removes the completed segments and add its successors for next to read.
    /// This should be called by the reader who's reading the current segment. Since a segment
    /// cannot be read by multiple readers, we can assume this won't be called by multiple processors
    /// at the same time.
    pub(crate) async fn segment_completed(
        &mut self,
        reader: &Reader,
        segment_completed: &SegmentWithRange,
        successors_mapped_to_their_predecessors: &HashMap<SegmentWithRange, Vec<Segment>>,
    ) -> Result<(), ReaderGroupStateError> {
        let _res_str = self
            .sync
            .insert(|table| {
                ReaderGroupState::segment_completed_internal(
                    table,
                    reader,
                    segment_completed,
                    successors_mapped_to_their_predecessors,
                )
            })
            .await
            .context(SyncError {
                error_msg: format!(
                    "segment {:?} assigned to reader {:?} has completed",
                    segment_completed, reader
                ),
            })?;
        Ok(())
    }

    fn segment_completed_internal(
        table: &mut Table,
        reader: &Reader,
        segment_completed: &SegmentWithRange,
        successors_mapped_to_their_predecessors: &HashMap<SegmentWithRange, Vec<Segment>>,
    ) -> Result<Option<String>, SynchronizerError> {
        let mut assigned_segments = ReaderGroupState::get_reader_owned_segments_from_table(table, reader)?;
        let mut future_segments = ReaderGroupState::get_future_segments_from_table(table);

        // remove completed segment from assigned_segment list
        assigned_segments
            .remove(segment_completed)
            .expect("should have assigned this segment to reader");
        table.insert(
            ASSIGNED.to_owned(),
            reader.to_string(),
            "HashMap<SegmentWithRange, Offset>".to_owned(),
            Box::new(assigned_segments),
        );

        // add missing successors to future_segments
        for (segment, list) in successors_mapped_to_their_predecessors {
            if !future_segments.contains_key(segment) {
                let required_to_complete = HashSet::from_iter(list.clone().into_iter());
                table.insert(
                    FUTURE.to_owned(),
                    segment.to_string(),
                    "HashSet<i64>".to_owned(),
                    Box::new(required_to_complete.clone()),
                );
                // need to update the temp map since later operation may depend on it
                future_segments.insert(segment.to_owned(), required_to_complete);
            }
        }

        // remove the completed segment from the dependency list
        for (segment, required_to_complete) in &mut future_segments {
            // the hash set needs update
            if required_to_complete.remove(&segment_completed.scoped_segment.segment) {
                table.insert(
                    FUTURE.to_owned(),
                    segment.to_string(),
                    "HashSet<i64>".to_owned(),
                    Box::new(required_to_complete.to_owned()),
                );
            }
        }

        // find successors that are ready to read. A successor is ready to read
        // once all its predecessors are completed.
        let ready_to_read = future_segments
            .iter()
            .filter(|&(_segment, set)| set.is_empty())
            .map(|(segment, _set)| segment.to_owned())
            .collect::<Vec<SegmentWithRange>>();

        for segment in ready_to_read {
            // add ready to read segments to unassigned_segments
            table.insert(
                UNASSIGNED.to_owned(),
                segment.to_string(),
                "Offset".to_owned(),
                Box::new(Offset::new(0, 0)),
            );
            // remove those from the future_segments
            table.insert_tombstone(FUTURE.to_owned(), segment.to_string())?;
        }
        Ok(None)
    }

    fn get_reader_owned_segments_from_table(
        table: &mut Table,
        reader: &Reader,
    ) -> Result<HashMap<SegmentWithRange, Offset>, SynchronizerError> {
        ReaderGroupState::check_reader_online(&table.get_inner_map(ASSIGNED), reader)?;

        let value = table
            .get(ASSIGNED, &reader.to_string())
            .expect("get reader owned segments");
        let owned_segments: HashMap<SegmentWithRange, Offset> =
            deserialize_from(&value.data).expect("deserialize reader owned segments");
        Ok(owned_segments)
    }

    fn get_unassigned_segments_from_table(table: &mut Table) -> HashMap<SegmentWithRange, Offset> {
        table
            .get_inner_map(UNASSIGNED)
            .iter()
            .map(|(k, v)| {
                let segment_str = &*k.to_owned();
                (
                    SegmentWithRange::from(segment_str),
                    deserialize_from(&v.data).expect("deserialize offset"),
                )
            })
            .collect::<HashMap<SegmentWithRange, Offset>>()
    }

    fn get_future_segments_from_table(table: &mut Table) -> HashMap<SegmentWithRange, HashSet<Segment>> {
        table
            .get_inner_map(FUTURE)
            .iter()
            .map(|(k, v)| {
                let segment_str = &*k.to_owned();
                (
                    SegmentWithRange::from(segment_str),
                    deserialize_from(&v.data).expect("deserialize hashset"),
                )
            })
            .collect::<HashMap<SegmentWithRange, HashSet<Segment>>>()
    }

    fn check_reader_online(
        assigned: &HashMap<String, Value>,
        reader: &Reader,
    ) -> Result<(), SynchronizerError> {
        if assigned.contains_key(&reader.to_string()) {
            Ok(())
        } else {
            Err(SynchronizerError::SyncUpdateError {
                error_msg: format!("reader {} is not online", reader),
            })
        }
    }
}

#[derive(new, Serialize, Deserialize, PartialEq, Debug, Clone)]
pub(crate) struct Offset {
    /// The client has read to this offset and handle the result to the application/caller.
    /// But some events before this offset may not have been processed by application/caller.
    /// In case of failure, those unprocessed events may need to be read from application/caller
    /// again.
    read: u64,
    /// The application/caller has processed up to this offset, this is less than or equal to the
    /// read offset.
    processed: u64,
}

#[cfg(test)]
mod test {
    use super::*;
    use crate::table_synchronizer::{serialize, Value};
    use lazy_static::*;
    use ordered_float::OrderedFloat;
    use pravega_rust_client_shared::{Scope, Segment, Stream};

    lazy_static! {
        static ref READER: Reader = Reader::from("test".to_owned());
        static ref SEGMENT: ScopedSegment = ScopedSegment {
            scope: Scope {
                name: "scope".to_string(),
            },
            stream: Stream {
                name: "scope".to_string(),
            },
            segment: Segment {
                number: 0,
                tx_id: None,
            },
        };
        static ref SEGMENT_WITH_RANGE: SegmentWithRange = SegmentWithRange {
            scoped_segment: SEGMENT.clone(),
            min_key: OrderedFloat::from(0.0),
            max_key: OrderedFloat::from(1.0),
        };
    }

    fn set_up() -> Table {
        let offset = Box::new(Offset::new(0, 0));
        let data = serialize(&*offset).expect("serialize value");

        let mut unassigned_segments = HashMap::new();
        unassigned_segments.insert(
            SEGMENT_WITH_RANGE.to_string(),
            Value {
                type_id: "Offset".to_owned(),
                data,
            },
        );

        let mut map = HashMap::new();
        map.insert(UNASSIGNED.to_owned(), unassigned_segments);

        let counter = HashMap::new();
        let table = Table::new(map, counter, vec![], vec![]);

        assert!(table.contains_outer_key(UNASSIGNED));
        assert!(table.contains_key(UNASSIGNED, &SEGMENT_WITH_RANGE.to_string()));

        table
    }

    #[test]
    fn test_reader_group_state() {
        // set up
        let mut table = set_up();

        // add a reader
        ReaderGroupState::add_reader_internal(&mut table, &READER).expect("add reader");

        assert!(
            table.contains_outer_key(ASSIGNED),
            "assigned_segments outer map should be created automatically"
        );
        assert!(
            table.contains_key(ASSIGNED, &READER.to_string()),
            "should contain inner key"
        );

        // get online readers
        let mut readers = ReaderGroupState::get_online_readers_internal(table.get_inner_map(ASSIGNED));

        assert_eq!(
            readers.pop().expect("get reader"),
            READER.clone(),
            "should have online reader added"
        );
        assert!(readers.is_empty(), "should have only one reader");

        // assign a segment to a reader
        ReaderGroupState::assign_segment_to_reader_internal(&mut table, &READER)
            .expect("assign segment to reader");
        let segments =
            ReaderGroupState::get_reader_positions_internal(&READER, table.get_inner_map(ASSIGNED))
                .expect("get reader positions");

        assert_eq!(segments.len(), 1, "should have assigned one segment the reader");
        assert_eq!(
            segments.get(&SEGMENT_WITH_RANGE).expect("get segment"),
            &Offset {
                read: 0,
                processed: 0
            },
            "added segment should be as expected"
        );

        // update reader position
        let new_offset = Offset {
            read: 10,
            processed: 0,
        };
        let mut update = HashMap::new();
        update.insert(SEGMENT_WITH_RANGE.clone(), new_offset.clone());

        ReaderGroupState::update_reader_positions_internal(&mut table, &READER, &update)
            .expect("update reader position");
        let segments =
            ReaderGroupState::get_reader_positions_internal(&READER, table.get_inner_map(ASSIGNED))
                .expect("get reader positions");

        assert_eq!(segments.len(), 1, "reader should contain one owned segment");
        assert_eq!(
            segments.get(&SEGMENT_WITH_RANGE).expect("get segment"),
            &new_offset,
            "the offset of owned segment should be updated"
        );

        // segment completed
        let mut successor0 = SEGMENT_WITH_RANGE.clone();
        successor0.scoped_segment.segment.number = 1;
        successor0.max_key = OrderedFloat::from(0.5);

        let mut successor1 = SEGMENT_WITH_RANGE.clone();
        successor1.scoped_segment.segment.number = 2;
        successor1.min_key = OrderedFloat::from(0.5);

        let mut successors_mapped_to_their_predecessors = HashMap::new();
        successors_mapped_to_their_predecessors.insert(
            successor0.clone(),
            vec![Segment {
                number: 0,
                tx_id: None,
            }],
        );
        successors_mapped_to_their_predecessors.insert(
            successor1.clone(),
            vec![Segment {
                number: 0,
                tx_id: None,
            }],
        );

        ReaderGroupState::segment_completed_internal(
            &mut table,
            &READER,
            &SEGMENT_WITH_RANGE,
            &successors_mapped_to_their_predecessors,
        )
        .expect("reader segment completed");
        assert!(table.contains_key(UNASSIGNED, &successor0.to_string()));
        assert!(table.contains_key(UNASSIGNED, &successor1.to_string()));

        ReaderGroupState::assign_segment_to_reader_internal(&mut table, &READER)
            .expect("assign segment to reader");
        ReaderGroupState::assign_segment_to_reader_internal(&mut table, &READER)
            .expect("assign segment to reader");

        // release segment from reader
        let new_offset = Offset {
            read: 10,
            processed: 10,
        };
        ReaderGroupState::release_segment_internal(
            &mut table,
            &READER,
            &successor0.scoped_segment,
            &new_offset,
        )
        .expect("release segment");

        let segments =
            ReaderGroupState::get_reader_positions_internal(&READER, table.get_inner_map(ASSIGNED))
                .expect("get reader positions");
        assert_eq!(
            segments.len(),
            1,
            "reader should contain 1 segment since the other one is released"
        );

        // reader offline
        let mut segments = HashMap::new();
        segments.insert(
            successor0.scoped_segment.clone(),
            Offset {
                read: 0,
                processed: 0,
            },
        );
        segments.insert(
            successor1.scoped_segment.clone(),
            Offset {
                read: 0,
                processed: 0,
            },
        );

        ReaderGroupState::remove_reader_internal(&mut table, &READER, &segments)
            .expect("remove online reader");

        assert_eq!(table.get_inner_map(UNASSIGNED).len(), 2);
    }
}
