//
// Copyright (c) Dell Inc., or its subsidiaries. All Rights Reserved.
//
// Licensed under the Apache License, Version 2.0 (the "License");
// you may not use this file except in compliance with the License.
// You may obtain a copy of the License at
//
// http://www.apache.org/licenses/LICENSE-2.0
//

use std::sync::Arc;
use tokio::sync::mpsc::{Receiver, Sender};
use tracing::{debug, error, info, warn};

use pravega_rust_client_shared::*;
use pravega_wire_protocol::wire_commands::Replies;

use crate::client_factory::ClientFactory;
use crate::error::*;
use crate::reactor::event::{Incoming, ServerReply};
use crate::reactor::segment_selector::SegmentSelector;
use crate::reactor::segment_writer::SegmentWriter;

#[derive(new)]
pub(crate) struct StreamReactor {}

impl StreamReactor {
    pub(crate) async fn run(
        stream: ScopedStream,
        sender: Sender<Incoming>,
        mut receiver: Receiver<Incoming>,
        factory: ClientFactory,
    ) {
        let delegation_token_provider = factory.create_delegation_token_provider(stream.clone()).await;
        let mut selector = SegmentSelector::new(
            stream,
            sender,
            factory.get_config().to_owned(),
            factory.clone(),
            Arc::new(delegation_token_provider),
        );
        // get the current segments and create corresponding event segment writers
        selector.initialize().await;
        info!("starting stream reactor");
        while StreamReactor::run_once(&mut selector, &mut receiver, &factory)
            .await
            .is_ok()
        {}
        info!("stream reactor is closed");
    }

    async fn run_once(
        selector: &mut SegmentSelector,
        receiver: &mut Receiver<Incoming>,
        factory: &ClientFactory,
    ) -> Result<(), &'static str> {
        let event = receiver.recv().await.expect("sender closed, processor exit");
        match event {
            Incoming::AppendEvent(pending_event) => {
                let event_segment_writer = selector.get_segment_writer(&pending_event.routing_key);

                if let Err(e) = event_segment_writer.write(pending_event).await {
                    warn!("failed to write append to segment due to {:?}, reconnecting", e);
                    event_segment_writer.reconnect(factory).await;
                }
                Ok(())
            }
            Incoming::ServerReply(server_reply) => {
                if let Err(e) = StreamReactor::process_server_reply(server_reply, selector, factory).await {
                    receiver.close();
                    drain_recevier(receiver, e.to_owned()).await;
                    Err(e)
                } else {
                    Ok(())
                }
            }
            Incoming::CloseSegmentWriter(info) => {
                let endpoint = info.conn.get_endpoint();
                let pool = factory.get_connection_pool();
                pool.add_connection(endpoint, info.conn);
                info!("segment writer is closed, put connection back to pool");
                selector.remove_segment_event_writer(&info.segment);
                if selector.writers.is_empty() && info.close_reactor {
                    Err("stream reactor is closed")
                } else {
                    Ok(())
                }
            }
            Incoming::CloseReactor => {
                let mut close = true;
                for v in selector.writers.values_mut() {
                    close &= v.try_close();
                }

                if close {
                    Err("stream reactor is closed")
                } else {
                    Ok(())
                }
            }
        }
    }

    async fn process_server_reply(
        server_reply: ServerReply,
        selector: &mut SegmentSelector,
        factory: &ClientFactory,
    ) -> Result<(), &'static str> {
        // it should always have writer because writer will
        // not be removed until it receives SegmentSealed reply
        let writer = selector
            .writers
            .get_mut(&server_reply.segment)
            .expect("should always be able to get event segment writer");
        match server_reply.reply {
            Replies::DataAppended(cmd) => {
                debug!(
                    "data appended for writer {:?}, latest event id is: {:?}",
                    writer.id, cmd.event_number
                );
                writer.ack(cmd.event_number);
                if let Err(e) = writer.write_pending_events().await {
                    warn!(
                        "writer {:?} failed to flush data to segment {:?} due to {:?}, reconnecting",
                        writer.id, writer.segment, e
                    );
                    writer.reconnect(factory).await;
                }
                Ok(())
            }

            Replies::SegmentIsSealed(cmd) => {
                debug!(
                    "segment {:?} sealed: stack trace {}",
                    cmd.segment, cmd.server_stack_trace
                );
                let segment = ScopedSegment::from(&*cmd.segment);
                if let Some(inflight) = selector.refresh_segment_event_writers_upon_sealed(&segment).await {
                    selector.resend(inflight).await;
                    if let Some(mut writer) = selector.remove_segment_event_writer(&segment) {
                        writer.try_close();
                    }
                    Ok(())
                } else {
                    Err("Stream is sealed")
                }
            }

            Replies::NoSuchSegment(cmd) => {
                debug!(
                    "no such segment {:?} due to segment truncation: stack trace {}",
                    cmd.segment, cmd.server_stack_trace
                );
                let segment = ScopedSegment::from(&*cmd.segment);
                if let Some(inflight) = selector.refresh_segment_event_writers_upon_sealed(&segment).await {
                    selector.resend(inflight).await;
                    if let Some(mut writer) = selector.remove_segment_event_writer(&segment) {
                        writer.try_close();
                    }
                    Ok(())
                } else {
                    Err("Stream is sealed")
                }
            }

            Replies::WrongHost(cmd) => {
                warn!(
                    "wrong host {:?} : stack trace {}",
                    cmd.segment, cmd.server_stack_trace
                );
                // reconnect will try to set up connection using updated endpoint
                writer.reconnect(factory).await;
                Ok(())
            }

            _ => {
                error!(
                    "receive unexpected reply {:?}, closing stream reactor",
                    server_reply.reply
                );
                Err("Unexpected reply")
            }
        }
    }
}

#[derive(new)]
pub(crate) struct SegmentReactor {}

impl SegmentReactor {
    pub(crate) async fn run(
        segment: ScopedSegment,
        sender: Sender<Incoming>,
        mut receiver: Receiver<Incoming>,
        factory: ClientFactory,
    ) {
        let delegation_token_provider = factory
            .create_delegation_token_provider(ScopedStream::from(&segment))
            .await;
        let mut writer = SegmentWriter::new(
            segment.clone(),
            sender.clone(),
            factory.get_config().retry_policy.to_owned(),
            Arc::new(delegation_token_provider),
        );
        if let Err(_e) = writer.setup_connection(&factory).await {
            writer.reconnect(&factory).await;
        }

        while SegmentReactor::run_once(&mut writer, &mut receiver, &factory)
            .await
            .is_ok()
        {}
        info!("Segment reactor is closed");
    }

    async fn run_once(
        writer: &mut SegmentWriter,
        receiver: &mut Receiver<Incoming>,
        factory: &ClientFactory,
    ) -> Result<(), &'static str> {
        let event = receiver.recv().await.expect("sender closed, processor exit");
        match event {
            Incoming::AppendEvent(pending_event) => {
                if let Err(e) = writer.write(pending_event).await {
                    warn!("failed to write append to segment due to {:?}, reconnecting", e);
                    writer.reconnect(factory).await;
                }
                Ok(())
            }
            Incoming::ServerReply(server_reply) => {
                if let Err(e) = SegmentReactor::process_server_reply(server_reply, writer, factory).await {
                    // can't use map_err since async closure issue
                    drain_recevier(receiver, e.to_owned()).await;
                    Err(e)
                } else {
                    Ok(())
                }
            }
            Incoming::CloseSegmentWriter(info) => {
                let endpoint = info.conn.get_endpoint();
                let pool = factory.get_connection_pool();
                pool.add_connection(endpoint, info.conn);
                info!(
                    "segment writer {:?} is closed, put connection back to pool",
                    info.id
                );
                if info.close_reactor {
                    // this Err return is to break the caller's event loop
                    Err("close segment reactor")
                } else {
                    Ok(())
                }
            }

            Incoming::CloseReactor => {
                if writer.try_close() {
                    // this Err return is to break the caller's event loop
                    Err("close segment reactor")
                } else {
                    Ok(())
                }
            }
        }
    }

    async fn process_server_reply(
        server_reply: ServerReply,
        writer: &mut SegmentWriter,
        factory: &ClientFactory,
    ) -> Result<(), &'static str> {
        match server_reply.reply {
            Replies::DataAppended(cmd) => {
                debug!(
                    "data appended for writer {:?}, latest event id is: {:?}",
                    writer.id, cmd.event_number
                );
                writer.ack(cmd.event_number);
                if let Err(e) = writer.write_pending_events().await {
                    warn!(
                        "writer {:?} failed to flush data to segment {:?} due to {:?}, reconnecting",
                        writer.id, writer.segment, e
                    );
                    writer.reconnect(factory).await;
                }
                Ok(())
            }

            Replies::SegmentIsSealed(cmd) => {
                debug!(
                    "segment {:?} sealed: stack trace {}",
                    cmd.segment, cmd.server_stack_trace
                );
                Err("Segment is sealed")
            }

            // same handling logic as segment sealed reply
            Replies::NoSuchSegment(cmd) => {
                debug!(
                    "no such segment {:?} due to segment truncation: stack trace {}",
                    cmd.segment, cmd.server_stack_trace
                );
                Err("No such segment")
            }

            Replies::WrongHost(cmd) => {
                debug!(
                    "wrong host {:?} : stack trace {}",
                    cmd.segment, cmd.server_stack_trace
                );
                // reconnect will try to set up connection using updated endpoint
                writer.reconnect(factory).await;
                Ok(())
            }

            _ => {
                error!(
                    "receive unexpected reply {:?}, closing segment reactor",
                    server_reply.reply
                );
                Err("Unexpected reply")
            }
        }
    }
}

async fn drain_recevier(receiver: &mut Receiver<Incoming>, msg: String) {
    receiver.close();
    while let Some(remaining) = receiver.recv().await {
        if let Incoming::AppendEvent(event) = remaining {
            let err = Err(SegmentWriterError::ReactorClosed { msg: msg.clone() });
            event.oneshot_sender.send(err).expect("send error");
        }
    }
}

#[cfg(test)]
pub(crate) mod test {
    use super::*;
    use crate::reactor::event::PendingEvent;
    use crate::reactor::segment_selector::test::create_segment_selector;
    use crate::reactor::segment_writer::test::create_segment_writer;
    use pravega_rust_client_config::connection_type::MockType;
    use tokio::sync::oneshot;

    type EventHandle = oneshot::Receiver<Result<(), SegmentWriterError>>;

    #[test]
    fn test_stream_reactor_happy_run() {
        let mut rt = tokio::runtime::Runtime::new().unwrap();
        let (mut selector, mut receiver, factory) = rt.block_on(create_segment_selector(MockType::Happy));

        // initialize segment selector
        rt.block_on(selector.initialize());
        assert_eq!(selector.writers.len(), 1);

        // write data once and reactor should ack
        rt.block_on(write_once_for_selector(&mut selector, 512));
        let result = rt.block_on(StreamReactor::run_once(&mut selector, &mut receiver, &factory));
        assert!(result.is_ok());

        let close = selector.try_close();
        assert_eq!(close, false);
        let result = rt.block_on(StreamReactor::run_once(&mut selector, &mut receiver, &factory));
        assert!(result.is_err());
    }

    #[test]
    fn test_stream_reactor_wrong_host() {
        let mut rt = tokio::runtime::Runtime::new().unwrap();
        let (mut selector, mut receiver, factory) = rt.block_on(create_segment_selector(MockType::WrongHost));

        // initialize segment selector
        rt.block_on(selector.initialize());
        assert_eq!(selector.writers.len(), 1);

        // write data once, should get wrong host reply and writer should retry
        rt.block_on(write_once_for_selector(&mut selector, 512));
        let result = rt.block_on(StreamReactor::run_once(&mut selector, &mut receiver, &factory));
        assert!(result.is_ok());
    }

    #[test]
    fn test_stream_reactor_stream_is_sealed() {
        let mut rt = tokio::runtime::Runtime::new().unwrap();
        let (mut selector, mut receiver, factory) =
            rt.block_on(create_segment_selector(MockType::SegmentIsSealed));

        // initialize segment selector
        rt.block_on(selector.initialize());
        assert_eq!(selector.writers.len(), 1);

        // write data once, should get segment sealed and reactor will fetch successors to continue
        rt.block_on(write_once_for_selector(&mut selector, 512));
        let result = rt.block_on(StreamReactor::run_once(&mut selector, &mut receiver, &factory));
        assert!(result.is_err());
    }

    #[test]
    fn test_segment_reactor_happy_run() {
        let mut rt = tokio::runtime::Runtime::new().unwrap();
        let (mut segment_writer, mut receiver, factory) = create_segment_writer(MockType::Happy);

        // set up connection
        let result = rt.block_on(segment_writer.setup_connection(&factory));
        assert!(result.is_ok());

        // write data once and reactor should ack
        rt.block_on(write_once(&mut segment_writer, 512));
        let result = rt.block_on(SegmentReactor::run_once(
            &mut segment_writer,
            &mut receiver,
            &factory,
        ));
        assert!(result.is_ok());
        assert_eq!(segment_writer.pending_append_num(), 0);
        assert_eq!(segment_writer.inflight_append_num(), 0);

        // shut down reactor
        let close = segment_writer.try_close();
        assert_eq!(close, false);
        let result = rt.block_on(SegmentReactor::run_once(
            &mut segment_writer,
            &mut receiver,
            &factory,
        ));
        assert!(result.is_err());
    }

    #[test]
    fn test_segment_reactor_wrong_host() {
        let mut rt = tokio::runtime::Runtime::new().unwrap();
        let (mut segment_writer, mut receiver, factory) = create_segment_writer(MockType::WrongHost);

        // set up connection
        let result = rt.block_on(segment_writer.setup_connection(&factory));
        assert!(result.is_ok());

        // write data once, should get wrong host reply and writer should retry
        rt.block_on(write_once(&mut segment_writer, 512));
        let result = rt.block_on(SegmentReactor::run_once(
            &mut segment_writer,
            &mut receiver,
            &factory,
        ));
        assert!(result.is_ok());
        assert_eq!(segment_writer.pending_append_num(), 0);
        assert_eq!(segment_writer.inflight_append_num(), 1);
    }

    #[test]
    fn test_segment_reactor_segment_is_sealed() {
        let mut rt = tokio::runtime::Runtime::new().unwrap();
        let (mut segment_writer, mut receiver, factory) = create_segment_writer(MockType::SegmentIsSealed);

        // set up connection
        let result = rt.block_on(segment_writer.setup_connection(&factory));
        assert!(result.is_ok());

        // write data once, should get segment sealed reply and returns error
        rt.block_on(write_once(&mut segment_writer, 512));
        let result = rt.block_on(SegmentReactor::run_once(
            &mut segment_writer,
            &mut receiver,
            &factory,
        ));
        assert!(result.is_err());
    }

    // helper function section
    async fn write_once_for_selector(
        selector: &mut SegmentSelector,
        size: usize,
    ) -> oneshot::Receiver<Result<(), SegmentWriterError>> {
        let (oneshot_sender, oneshot_receiver) = tokio::sync::oneshot::channel();
        let event = PendingEvent::new(Some("routing_key".into()), vec![1; size], oneshot_sender)
            .expect("create pending event");
        let writer = selector.get_segment_writer(&event.routing_key);
        writer.write(event).await.expect("write data");
        oneshot_receiver
    }

    async fn write_once(
        writer: &mut SegmentWriter,
        size: usize,
    ) -> oneshot::Receiver<Result<(), SegmentWriterError>> {
        let (oneshot_sender, oneshot_receiver) = tokio::sync::oneshot::channel();
        let event = PendingEvent::new(Some("routing_key".into()), vec![1; size], oneshot_sender)
            .expect("create pending event");
        writer.write(event).await.expect("write data");
        oneshot_receiver
    }

    fn create_event(size: usize) -> (PendingEvent, EventHandle) {
        let (oneshot_sender, oneshot_receiver) = tokio::sync::oneshot::channel();
        let event = PendingEvent::new(Some("routing_key".into()), vec![1; size], oneshot_sender)
            .expect("create pending event");
        (event, oneshot_receiver)
    }
}
