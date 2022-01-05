// Copyright 2021 The NATS Authors
// Licensed under the Apache License, Version 2.0 (the "License");
// you may not use this file except in compliance with the License.
// You may obtain a copy of the License at
//
// http://www.apache.org/licenses/LICENSE-2.0
//
// Unless required by applicable law or agreed to in writing, software
// distributed under the License is distributed on an "AS IS" BASIS,
// WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
// See the License for the specific language governing permissions and
// limitations under the License.

#![cfg(feature = "unstable")]
use rand::prelude::*;
use tokio::io::AsyncReadExt;

mod util;

#[tokio::test]
#[ignore]
async fn object_random() {
    let server = util::run_server("tests/configs/jetstream.conf");
    let client = nats::connect(&server.client_url()).await.unwrap();
    let context = nats::jetstream::new(client);

    let bucket = context
        .create_object_store(&nats::object_store::Config {
            bucket: "OBJECTS".to_string(),
            ..Default::default()
        })
        .await
        .unwrap();

    bucket.info("FOO").await.unwrap_err();

    let mut rng = rand::thread_rng();
    let mut bytes = Vec::with_capacity(16 * 1024 * 1024 + 22);
    rng.try_fill_bytes(&mut bytes).unwrap();

    bucket.put("FOO", &mut bytes.as_slice()).await.unwrap();

    let object_info = bucket.info("FOO").await.unwrap();
    assert!(!object_info.nuid.is_empty());
    assert_eq!(object_info.size, bytes.len());

    let mut result = Vec::new();
    bucket
        .get("FOO")
        .await
        .unwrap()
        .read_to_end(&mut result)
        .await
        .unwrap();

    assert_eq!(result, bytes);

    let mut bytes = Vec::with_capacity(16 * 1024 * 1024 + 22);
    rng.try_fill_bytes(&mut bytes).unwrap();
    bucket.put("FOO", &mut bytes.as_slice()).await.unwrap();

    let mut result = Vec::new();
    bucket
        .get("FOO")
        .await
        .unwrap()
        .read_to_end(&mut result)
        .await
        .unwrap();

    assert_eq!(result, bytes);

    let mut result = Vec::new();
    let mut object = bucket.get("FOO").await.unwrap();
    loop {
        let mut buffer = [0; 1];
        if let Ok(n) = object.read(&mut buffer).await {
            if n == 0 {
                break;
            }

            result.extend_from_slice(&buffer[..n]);
        }
    }

    assert_eq!(result, bytes);

    let mut result = Vec::new();
    let mut object = bucket.get("FOO").await.unwrap();
    loop {
        let mut buffer = [0; 2];
        if let Ok(n) = object.read(&mut buffer).await {
            if n == 0 {
                break;
            }

            result.extend_from_slice(&buffer[..n]);
        }
    }

    assert_eq!(result, bytes);

    let mut result = Vec::new();
    let mut object = bucket.get("FOO").await.unwrap();
    loop {
        let mut buffer = [0; 128];
        if let Ok(n) = object.read(&mut buffer).await {
            if n == 0 {
                break;
            }

            result.extend_from_slice(&buffer[..n]);
        }
    }

    assert_eq!(result, bytes);
}

#[tokio::test]
#[ignore]
async fn object_sealed() {
    let server = util::run_server("tests/configs/jetstream.conf");
    let client = nats::connect(&server.client_url()).await.unwrap();
    let context = nats::jetstream::new(client);

    let bucket = context
        .create_object_store(&nats::object_store::Config {
            bucket: "OBJECTS".to_string(),
            ..Default::default()
        })
        .await
        .unwrap();

    bucket.seal().await.unwrap();

    let stream_info = context.stream_info("OBJ_OBJECTS").await.unwrap();
    assert!(stream_info.config.sealed);
}

#[tokio::test]
#[ignore]
async fn object_delete() {
    eprintln!("object_delete 1");
    let server = util::run_server("tests/configs/jetstream.conf");
    let client = nats::connect(&server.client_url()).await.unwrap();
    let context = nats::jetstream::new(client);

    let bucket = context
        .create_object_store(&nats::object_store::Config {
            bucket: "OBJECTS".to_string(),
            ..Default::default()
        })
        .await
        .unwrap();

    let data = b"abc".repeat(100).to_vec();
    bucket.put("A", &mut data.as_slice()).await.unwrap();
    bucket.delete("A").await.unwrap();

    let stream_info = context.stream_info("OBJ_OBJECTS").await.unwrap();

    // We should have one message left. The delete marker.
    assert_eq!(stream_info.state.messages, 1);

    // Make sure we have a delete marker, this will be there to drive Watch functionality.
    let object_info = bucket.info("A").await.unwrap();
    assert!(object_info.deleted);
}

#[tokio::test]
#[ignore]
async fn object_multiple_delete() {
    let server = util::run_server("tests/configs/jetstream.conf");
    let client = nats::connect(&server.client_url()).await.unwrap();
    let context = nats::jetstream::new(client);

    let bucket = context
        .create_object_store(&nats::object_store::Config {
            bucket: "2OD".to_string(),
            ..Default::default()
        })
        .await
        .unwrap();

    let data = b"A".repeat(100).to_vec();
    bucket.put("A", &mut data.as_slice()).await.unwrap();

    // Hold onto this so we can make sure delete clears all messages, chunks and meta.
    let old_stream_info = context.stream_info("OBJ_2OD").await.unwrap();

    let data = b"B".repeat(200).to_vec();
    bucket.put("B", &mut data.as_slice()).await.unwrap();

    // Now delete B
    bucket.delete("B").await.unwrap();

    let new_stream_info = context.stream_info("OBJ_2OD").await.unwrap();
    assert_eq!(
        new_stream_info.state.messages,
        old_stream_info.state.messages + 1
    );
}

#[tokio::test]
async fn object_names() {
    eprintln!("names 0");
    let server = util::run_server("tests/configs/jetstream.conf");
    let client = nats::connect(&server.client_url()).await.unwrap();
    let context = nats::jetstream::new(client);

    eprintln!("names 1");
    let bucket = context
        .create_object_store(&nats::object_store::Config {
            bucket: "NAMES".to_string(),
            ..Default::default()
        })
        .await
        .unwrap();
    eprintln!("names 2");

    let empty = Vec::new();

    eprintln!("names 3");
    // Test filename like naming.
    bucket.put("foo.bar", &mut empty.as_slice()).await.unwrap();
    eprintln!("names 4");

    // Spaces ok
    bucket.put("foo bar", &mut empty.as_slice()).await.unwrap();
    eprintln!("names 5");

    // Errors
    bucket.put("*", &mut empty.as_slice()).await.unwrap_err();
    eprintln!("names 6");
    bucket.put(">", &mut empty.as_slice()).await.unwrap_err();
    eprintln!("names 7");
    bucket.put("", &mut empty.as_slice()).await.unwrap_err();
    eprintln!("names 8");
}

#[tokio::test]
#[ignore]
async fn object_watch() {
    use futures::stream::StreamExt as _;
    let server = util::run_server("tests/configs/jetstream.conf");
    let client = nats::connect(&server.client_url()).await.unwrap();
    let context = nats::jetstream::new(client);

    let bucket = context
        .create_object_store(&nats::object_store::Config {
            bucket: "WATCH".to_string(),
            ..Default::default()
        })
        .await
        .unwrap();

    let mut watch = bucket.watch().await.unwrap();

    let bytes = vec![];
    bucket.put("foo", &mut bytes.as_slice()).await.unwrap();

    let info = watch.next().await.unwrap();
    assert_eq!(info.name, "foo");
    assert_eq!(info.size, bytes.len());

    let bytes = vec![1, 2, 3, 4, 5];
    bucket.put("bar", &mut bytes.as_slice()).await.unwrap();

    let info = watch.next().await.unwrap();
    assert_eq!(info.name, "bar");
    assert_eq!(info.size, bytes.len());
}