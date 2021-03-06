//
// Copyright (c) Dell Inc., or its subsidiaries. All Rights Reserved.
//
// Licensed under the Apache License, Version 2.0 (the "License");
// you may not use this file except in compliance with the License.
// You may obtain a copy of the License at
//
// http://www.apache.org/licenses/LICENSE-2.0
//

use crate::pravega_service::PravegaStandaloneServiceConfig;
use crate::utils;
use pravega_client_rust::byte_stream::{ByteStreamReader, ByteStreamWriter};
use pravega_client_rust::client_factory::ClientFactory;
use pravega_client_rust::error::SegmentWriterError;
use pravega_client_rust::event_stream_writer::EventStreamWriter;
use pravega_client_rust::raw_client::RawClient;
use pravega_client_rust::segment_reader::AsyncSegmentReader;
use pravega_connection_pool::connection_pool::ConnectionPool;
use pravega_controller_client::{ControllerClient, ControllerClientImpl};
use pravega_rust_client_config::{connection_type::ConnectionType, ClientConfigBuilder, MOCK_CONTROLLER_URI};
use pravega_rust_client_shared::*;
use pravega_wire_protocol::client_connection::{ClientConnection, ClientConnectionImpl};
use pravega_wire_protocol::commands::{
    Command, EventCommand, GetStreamSegmentInfoCommand, SealSegmentCommand,
};
use pravega_wire_protocol::connection_factory::{ConnectionFactory, SegmentConnectionManager};
use pravega_wire_protocol::wire_commands::{Replies, Requests};
use std::io::{Read, Seek, SeekFrom, Write};
use std::net::SocketAddr;
use tokio::runtime::{Handle, Runtime};
use tracing::info;

pub fn test_byte_stream(config: PravegaStandaloneServiceConfig) {
    // spin up Pravega standalone
    let mut rt = Runtime::new().unwrap();
    let scope_name = Scope::from("testScopeByteStream".to_owned());
    let stream_name = Stream::from("testStreamByteStream".to_owned());
    let config = ClientConfigBuilder::default()
        .controller_uri(MOCK_CONTROLLER_URI)
        .is_auth_enabled(config.auth)
        .is_tls_enabled(config.tls)
        .build()
        .expect("creating config");
    let client_factory = ClientFactory::new(config);
    let handle = client_factory.get_runtime_handle();
    handle.block_on(utils::create_scope_stream(
        client_factory.get_controller_client(),
        &scope_name,
        &stream_name,
        1,
    ));

    let scoped_segment = ScopedSegment {
        scope: scope_name,
        stream: stream_name,
        segment: Segment::from(0),
    };

    let mut writer = client_factory.create_byte_stream_writer(scoped_segment.clone());
    let mut reader = client_factory.create_byte_stream_reader(scoped_segment);

    test_write_and_read(&mut writer, &mut reader);
    test_seek(&mut reader);
    test_truncation(&mut writer, &mut reader, &mut rt);
    test_seal(&mut writer, &mut reader, &mut rt);
}

fn test_write_and_read(writer: &mut ByteStreamWriter, reader: &mut ByteStreamReader) {
    info!("test byte stream write and read");
    let payload1 = vec![1, 1, 1, 1];
    let payload2 = vec![2, 2, 2, 2];
    let expected = [&payload1[..], &payload2[..]].concat();

    let size1 = writer.write(&payload1).expect("write payload1 to byte stream");
    assert_eq!(size1, 4);

    let size2 = writer.write(&payload2).expect("write payload2 to byte stream");
    assert_eq!(size2, 4);
    writer.flush().expect("flush byte stream writer");

    let mut buf: Vec<u8> = vec![0; 8];
    let bytes = reader.read(&mut buf).expect("read from byte stream");
    assert_eq!(bytes, 8);
    assert_eq!(buf, expected);

    info!("test byte stream write and read passed");
}

fn test_seek(reader: &mut ByteStreamReader) {
    info!("test byte stream seek");
    // seek to start
    reader.seek(SeekFrom::Start(0)).expect("seek to start");
    let mut buf: Vec<u8> = vec![0; 4];
    let size = reader.read(&mut buf).expect("read from byte stream");
    assert_eq!(size, 4);
    assert_eq!(buf, vec![1; 4]);

    // seek to current
    reader.seek(SeekFrom::Current(-4)).expect("seek to current");
    let mut buf: Vec<u8> = vec![0; 4];
    let size = reader.read(&mut buf).expect("read from byte stream");
    assert_eq!(size, 4);
    assert_eq!(buf, vec![1; 4]);

    // seek to end
    reader.seek(SeekFrom::End(-4)).expect("seek to current");
    let mut buf: Vec<u8> = vec![0; 4];
    let size = reader.read(&mut buf).expect("read from byte stream");
    assert_eq!(size, 4);
    assert_eq!(buf, vec![2; 4]);
    info!("test byte stream seek passed");
}

fn test_truncation(writer: &mut ByteStreamWriter, reader: &mut ByteStreamReader, rt: &mut Runtime) {
    // truncate
    rt.block_on(writer.truncate_data_before(4)).expect("truncate");

    // read before truncation point
    reader.seek(SeekFrom::Start(0)).expect("seek to start");
    let mut buf: Vec<u8> = vec![0; 4];
    let result = reader.read(&mut buf);
    assert!(result.is_err());

    // get current head
    let head = reader.current_head().expect("get current head");
    assert_eq!(head, 4);

    // read from current head
    reader.seek(SeekFrom::Start(head)).expect("seek to start");
    let mut buf: Vec<u8> = vec![0; 4];
    let size = reader.read(&mut buf).expect("read from byte stream");
    assert_eq!(size, 4);
    assert_eq!(buf, vec![2; 4]);
}

fn test_seal(writer: &mut ByteStreamWriter, reader: &mut ByteStreamReader, rt: &mut Runtime) {
    // Bug: sealed segment cannot read starting offset within last 8 bytes
    let payload = vec![3, 3, 3, 3];
    let size = writer.write(&payload).expect("write payload1 to byte stream");
    assert_eq!(size, 4);

    // seal
    rt.block_on(writer.seal()).expect("seal");

    // read sealed segment
    reader.seek(SeekFrom::Start(4)).expect("seek to start");
    let mut buf: Vec<u8> = vec![0; 4];
    let size = reader.read(&mut buf).expect("read from byte stream");
    assert_eq!(size, 4);
    assert_eq!(buf, vec![2; 4]);

    // read beyond sealed segment
    let mut buf: Vec<u8> = vec![0; 8];
    let result = reader.read(&mut buf);
    assert!(result.is_err())
}
