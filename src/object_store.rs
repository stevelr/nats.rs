// Copyright 2020-2021 The NATS Authors
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

//! Support for Object Store.
//! This feature is experimental and the API may change.

use crate::Stream;
use crate::{header::HeaderMap, jetstream::JetStream, jetstream_types::*, Message};
use chrono::Utc;
use futures::{Future, StreamExt};
use lazy_static::lazy_static;
use regex::Regex;
use serde::{Deserialize, Serialize};
use std::{
    cmp,
    collections::HashSet,
    io,
    pin::Pin,
    sync::Arc,
    task::{Context, Poll},
    time::Duration,
};
use tokio::io::ReadBuf;
use tokio::sync::Mutex;

const DEFAULT_CHUNK_SIZE: usize = 128 * 1024;
const NATS_ROLLUP: &str = "Nats-Rollup";
const ROLLUP_SUBJECT: &str = "sub";

lazy_static! {
    static ref BUCKET_NAME_RE: Regex = Regex::new(r#"\A[a-zA-Z0-9_-]+\z"#).unwrap();
    static ref OBJECT_NAME_RE: Regex = Regex::new(r#"\A[-/_=\.a-zA-Z0-9]+\z"#).unwrap();
}

fn is_valid_bucket_name(bucket_name: &str) -> bool {
    BUCKET_NAME_RE.is_match(bucket_name)
}

fn is_valid_object_name(object_name: &str) -> bool {
    if object_name.is_empty() || object_name.starts_with('.') || object_name.ends_with('.') {
        return false;
    }

    OBJECT_NAME_RE.is_match(object_name)
}

fn sanitize_object_name(object_name: &str) -> String {
    object_name.replace('.', "_").replace(' ', "_")
}

/// Configuration values for object store buckets.
#[derive(Debug, Default, Clone, Serialize, Deserialize)]
pub struct Config {
    /// Name of the storage bucket.
    pub bucket: String,
    /// A short description of the purpose of this storage bucket.
    pub description: Option<String>,
    /// Maximum age of any value in the bucket, expressed in nanoseconds
    pub max_age: Duration,
    /// The type of storage backend, `File` (default) and `Memory`
    pub storage: StorageType,
    /// How many replicas to keep for each value in a cluster, maximum 5.
    pub num_replicas: usize,
}

impl JetStream {
    /// Creates a new object store bucket.
    ///
    /// # Example
    ///
    /// ```
    /// # use nats::object_store::Config;
    /// # #[tokio::main]
    /// # async fn main() -> std::io::Result<()> {
    /// # let client = nats::connect("demo.nats.io").await?;
    /// # let context = nats::jetstream::new(client);
    /// #
    /// let bucket = context.create_object_store(&Config {
    ///   bucket: "create_object_store".to_string(),
    ///   ..Default::default()
    /// }).await?;
    ///
    /// # context.delete_object_store("create_object_store").await?;
    /// # Ok(())
    /// # }
    /// ```
    pub async fn create_object_store(&self, config: &Config) -> io::Result<ObjectStore> {
        if !self.connection.is_server_compatible_version(2, 6, 2).await {
            return Err(io::Error::new(
                io::ErrorKind::Other,
                "object-store requires at least server version 2.6.2",
            ));
        }

        if !is_valid_bucket_name(&config.bucket) {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "invalid bucket name",
            ));
        }

        let bucket_name = config.bucket.clone();
        let stream_name = format!("OBJ_{}", bucket_name);
        let chunk_subject = format!("$O.{}.C.>", bucket_name);
        let meta_subject = format!("$O.{}.M.>", bucket_name);

        self.add_stream(&StreamConfig {
            name: stream_name,
            description: config.description.clone(),
            subjects: vec![chunk_subject, meta_subject],
            max_age: config.max_age,
            storage: config.storage,
            num_replicas: config.num_replicas,
            discard: DiscardPolicy::New,
            allow_rollup: true,
            ..Default::default()
        })
        .await?;

        Ok(ObjectStore::new(bucket_name, self.clone()))
    }

    /// Bind to an existing object store bucket.
    ///
    /// # Example
    ///
    /// ```
    /// # use nats::object_store::Config;
    /// # #[tokio::main]
    /// # async fn main() -> std::io::Result<()> {
    /// # let client = nats::connect("demo.nats.io").await?;
    /// # let context = nats::jetstream::new(client);
    /// #
    /// context.create_object_store(&Config {
    ///   bucket: "object_store".to_string(),
    ///   ..Default::default()
    /// }).await?;
    ///
    /// let bucket = context.object_store("object_store").await?;
    ///
    /// # context.delete_object_store("object_store").await?;
    /// # Ok(())
    /// # }
    /// ```
    pub async fn object_store(&self, bucket_name: &str) -> io::Result<ObjectStore> {
        if !self.connection.is_server_compatible_version(2, 6, 2).await {
            return Err(io::Error::new(
                io::ErrorKind::Other,
                "object-store requires at least server version 2.6.2",
            ));
        }

        if !is_valid_bucket_name(bucket_name) {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "invalid bucket name",
            ));
        }

        let stream_name = format!("OBJ_{}", bucket_name);
        self.stream_info(stream_name).await?;

        Ok(ObjectStore::new(bucket_name.to_string(), self.clone()))
    }

    /// Delete the underlying stream for the named object.
    ///
    /// # Example
    ///
    /// ```
    /// use nats::object_store::Config;
    /// # #[tokio::main]
    /// # async fn main() -> std::io::Result<()> {
    /// # let client = nats::connect("demo.nats.io").await?;
    /// # let context = nats::jetstream::new(client);
    /// #
    /// # let bucket = context.create_object_store(&Config {
    /// #  bucket: "delete_object_store".to_string(),
    /// #  ..Default::default()
    /// # }).await?;
    ///
    /// context.delete_object_store("delete_object_store").await?;
    ///
    /// # Ok(())
    /// # }
    /// ```
    ///
    pub async fn delete_object_store(&self, bucket_name: &str) -> io::Result<()> {
        let stream_name = format!("OBJ_{}", bucket_name);
        self.delete_stream(stream_name).await?;

        Ok(())
    }
}

/// Meta and instance information about an object.
#[derive(Debug, Clone, Serialize, Deserialize, Eq, PartialEq)]
pub struct ObjectInfo {
    /// Name of the object
    pub name: String,
    /// A short human readable description of the object.
    pub description: Option<String>,
    /// Link this object points to, if any.
    pub link: Option<ObjectLink>,
    /// Name of the bucket the object is stored in.
    pub bucket: String,
    /// Unique identifier used to uniquely identify this version of the object.
    pub nuid: String,
    /// Size in bytes of the object.
    pub size: usize,
    /// Number of chunks the object is stored in.
    pub chunks: usize,
    /// Date and time the object was last modified.
    pub modified: DateTime,
    /// Digest of the object stream.
    pub digest: String,
    /// Set to true if the object has been deleted.
    pub deleted: bool,
}

/// Meta information about an object.
#[derive(Debug, Default, Clone, Serialize, Deserialize, Eq, PartialEq)]
pub struct ObjectMeta {
    /// Name of the object
    pub name: String,
    /// A short human readable description of the object.
    pub description: Option<String>,
    /// Link this object points to, if any.
    pub link: Option<ObjectLink>,
}

impl From<&str> for ObjectMeta {
    fn from(s: &str) -> ObjectMeta {
        ObjectMeta {
            name: s.to_string(),
            ..Default::default()
        }
    }
}

/// A link to another object, potentially in another bucket.
#[derive(Debug, Default, Clone, Serialize, Deserialize, Eq, PartialEq)]
pub struct ObjectLink {
    /// Name of the object
    pub name: String,
    /// Name of the bucket the object is stored in.
    pub bucket: Option<String>,
}

/// A blob store capable of storing large objects efficiently in streams.
pub struct ObjectStore {
    name: String,
    context: JetStream,
}

#[derive(Clone, Debug, Default)]
struct ObjectBytes {
    remaining_bytes: Vec<u8>,
    skip_next: bool,
}

/// Represents an object stored in a bucket.
pub struct Object {
    info: ObjectInfo,
    subscription: Pin<Box<dyn Stream<Item = Message>>>,
    bytes: Arc<Mutex<ObjectBytes>>,
}

impl Object {
    pub(crate) fn new(
        subscription: Pin<Box<dyn Stream<Item = Message>>>,
        info: ObjectInfo,
    ) -> Self {
        Object {
            subscription,
            info,
            bytes: Arc::new(Mutex::new(ObjectBytes::default())),
        }
    }

    /// Returns information about the object.
    pub fn info(&self) -> &ObjectInfo {
        &self.info
    }
}

impl tokio::io::AsyncRead for Object {
    fn poll_read(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buffer: &mut ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        {
            let mut bytes = match self.bytes.try_lock() {
                Err(_) => return Poll::Pending,
                Ok(guard) => guard,
            };
            if !bytes.remaining_bytes.is_empty() {
                let len = cmp::min(buffer.remaining(), bytes.remaining_bytes.len());
                buffer.put_slice(&bytes.remaining_bytes[..len]);

                if bytes.remaining_bytes.len() > len {
                    bytes.remaining_bytes = bytes.remaining_bytes[len..].to_vec();
                }
                return Poll::Ready(Ok(()));
            }
            if !bytes.skip_next {
                return Poll::Ready(Ok(()));
            }
            bytes.skip_next = false;
        }
        {
            if self.bytes.try_lock().is_err() {
                return Poll::Pending;
            }
        }
        {
            let mut sub_fut = self.subscription.next();

            match Future::poll(Pin::new(&mut sub_fut), cx) {
                Poll::Pending => Poll::Pending,
                Poll::Ready(Some(message)) => {
                    let mut bytes = match self.bytes.try_lock() {
                        Err(_) => return Poll::Pending,
                        Ok(guard) => guard,
                    };
                    assert_ne!(message.data.len(), 0, "expect non-zero message size");
                    let len = cmp::min(buffer.remaining(), message.data.len());
                    if len == 0 {
                        // if we received a message of 0 bytes (can this happen?),
                        // we should return Poll::Pending. Otherwise, returning Poll::Ready
                        // with no bytes written will be interpreted as end of stream
                        return Poll::Pending;
                    }
                    buffer.put_slice(&message.data[..len]);

                    if message.data.len() > len {
                        bytes
                            .remaining_bytes
                            .extend_from_slice(&message.data[len..]);
                    }

                    if let Some(message_info) = message.jetstream_message_info() {
                        if message_info.pending == 0 {
                            bytes.skip_next = true;
                        }
                    }
                    Poll::Ready(Ok(()))
                }
                Poll::Ready(None) => Poll::Ready(Ok(())),
            }
        }
    }
}

impl ObjectStore {
    /// Instantiates a new object store
    pub(crate) fn new(name: String, context: JetStream) -> Self {
        ObjectStore { name, context }
    }

    /// Retrieve the current information for the object.
    ///
    /// # Examples
    ///
    /// ```
    /// # use nats::object_store::Config;
    /// # #[tokio::main]
    /// # async fn main() -> std::io::Result<()> {
    /// # let client = nats::connect("demo.nats.io").await?;
    /// # let context = nats::jetstream::new(client);
    /// #
    /// let bucket = context.create_object_store(&Config {
    ///   bucket: "info".to_string(),
    ///   ..Default::default()
    /// }).await?;
    ///
    /// let bytes = vec![0];
    /// let info = bucket.put("foo", &mut bytes.as_slice()).await?;
    /// assert_eq!(info.name, "foo");
    /// assert_eq!(info.size, bytes.len());
    ///
    /// # context.delete_object_store("info").await?;
    /// # Ok(())
    /// # }
    /// ```
    pub async fn info(&self, object_name: &str) -> io::Result<ObjectInfo> {
        // LoOkup the stream to get the bound subject.
        let object_name = sanitize_object_name(object_name);
        if !is_valid_object_name(&object_name) {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "invalid object name",
            ));
        }

        // Grab last meta value we have.
        let stream_name = format!("OBJ_{}", &self.name);
        let subject = format!("$O.{}.M.{}", &self.name, &object_name);

        let message = self
            .context
            .get_last_message(&stream_name, &subject)
            .await?;
        let object_info = serde_json::from_slice::<ObjectInfo>(&message.data)?;

        Ok(object_info)
    }

    /// Seals the object store from further modifications.
    ///
    pub async fn seal(&self) -> io::Result<()> {
        let stream_name = format!("OBJ_{}", self.name);
        let stream_info = self.context.stream_info(stream_name).await?;

        let mut stream_config = stream_info.config;
        stream_config.sealed = true;

        self.context.update_stream(&stream_config).await?;

        Ok(())
    }

    /// Put will place the contents from the given reader into this object-store.
    ///
    /// # Example
    ///
    /// ```
    /// # use nats::object_store::Config;
    /// # #[tokio::main]
    /// # async fn main() -> std::io::Result<()> {
    /// # let client = nats::connect("demo.nats.io").await?;
    /// # let context = nats::jetstream::new(client);
    /// #
    /// let bucket = context.create_object_store(&Config {
    ///   bucket: "put".to_string(),
    ///   ..Default::default()
    /// }).await?;
    ///
    /// let bytes = vec![0, 1, 2, 3, 4];
    /// let info = bucket.put("foo", &mut bytes.as_slice()).await?;
    /// assert_eq!(bucket.info("foo").await.unwrap(), info);
    ///
    /// # context.delete_object_store("put").await?;
    /// # Ok(())
    /// # }
    /// ```
    pub async fn put<T>(&self, meta: T, data: &mut impl io::Read) -> io::Result<ObjectInfo>
    where
        ObjectMeta: From<T>,
    {
        let object_meta: ObjectMeta = meta.into();
        let object_name = sanitize_object_name(&object_meta.name);
        if !is_valid_object_name(&object_name) {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "invalid object name",
            ));
        }

        // Fetch any existing object info, if ther is any for later use.
        let maybe_existing_object_info = match self.info(&object_name).await {
            Ok(object_info) => Some(object_info),
            Err(_) => None,
        };

        let object_nuid = nuid::next();
        let chunk_subject = format!("$O.{}.C.{}", &self.name, &object_nuid);

        let mut object_chunks = 0;
        let mut object_size = 0;

        let mut buffer = [0; DEFAULT_CHUNK_SIZE];

        loop {
            let n = data.read(&mut buffer)?;
            if n == 0 {
                break;
            }

            object_size += n;
            object_chunks += 1;

            self.context.publish(&chunk_subject, &buffer[..n]).await?;
        }

        // Create a random subject prefixed with the object stream name.
        let subject = format!("$O.{}.M.{}", &self.name, &object_name);
        let object_info = ObjectInfo {
            name: object_name,
            description: object_meta.description,
            link: object_meta.link,
            bucket: self.name.clone(),
            nuid: object_nuid,
            chunks: object_chunks,
            size: object_size,
            digest: "".to_string(),
            modified: Utc::now(),
            deleted: false,
        };

        let data = serde_json::to_vec(&object_info)?;
        let mut headers = HeaderMap::default();
        let entry = headers
            .inner
            .entry(NATS_ROLLUP.to_string())
            .or_insert_with(HashSet::default);

        entry.insert(ROLLUP_SUBJECT.to_string());

        let message = Message::new(&subject, None, data, Some(headers));

        // Publish metadata
        self.context.publish_message(&message).await?;

        // Purge any old chunks.
        if let Some(existing_object_info) = maybe_existing_object_info {
            let stream_name = format!("OBJ_{}", self.name);
            let chunk_subject = format!("$O.{}.C.{}", &self.name, &existing_object_info.nuid);

            self.context
                .purge_stream_subject(&stream_name, &chunk_subject)
                .await?;
        }

        Ok(object_info)
    }

    /// Get an existing objecto by name.
    ///
    /// # Example
    ///
    /// ```
    /// use std::io::Read;
    /// use tokio::io::AsyncReadExt;
    /// # use nats::object_store::Config;
    /// # #[tokio::main]
    /// # async fn main() -> std::io::Result<()> {
    /// # let client = nats::connect("demo.nats.io").await?;
    /// # let context = nats::jetstream::new(client);
    /// #
    /// let bucket = context.create_object_store(&Config {
    ///   bucket: "get".to_string(),
    ///   ..Default::default()
    /// }).await?;
    ///
    /// let bytes = vec![0, 1, 2, 3, 4];
    /// let info = bucket.put("foo", &mut bytes.as_slice()).await?;
    ///
    /// let mut result = Vec::new();
    /// bucket.get("foo").await.unwrap().read_to_end(&mut result).await?;
    ///
    /// # context.delete_object_store("get").await?;
    /// # Ok(())
    /// # }
    /// ```
    pub async fn get(&self, object_name: &str) -> io::Result<Object> {
        let mut object_name = object_name.to_string();
        loop {
            let object_info = self.info(&object_name).await?;
            if let Some(link) = object_info.link {
                object_name = link.name.clone();
                continue;
                //return self.get(&link.name).await;
            }

            let chunk_subject = format!("$O.{}.C.{}", self.name, object_info.nuid);
            let subscription = self
                .context
                .subscribe_with_options(&chunk_subject, &SubscribeOptions::ordered())
                .await?;

            return Ok(Object::new(Box::pin(subscription.stream()), object_info));
        }
    }

    /// Places a delete marker and purges the data stream associated with the key.
    ///
    /// # Example
    ///
    /// ```
    /// use std::io::Read;
    /// # use nats::object_store::Config;
    /// # #[tokio::main]
    /// # async fn main() -> std::io::Result<()> {
    /// # let client = nats::connect("demo.nats.io").await?;
    /// # let context = nats::jetstream::new(client);
    /// #
    /// let bucket = context.create_object_store(&Config {
    ///   bucket: "delete".to_string(),
    ///   ..Default::default()
    /// }).await?;
    ///
    /// let bytes = vec![0, 1, 2, 3, 4];
    /// bucket.put("foo", &mut bytes.as_slice()).await?;
    ///
    /// bucket.delete("foo").await?;
    ///
    /// let info = bucket.info("foo").await?;
    /// assert!(info.deleted);
    /// assert_eq!(info.size, 0);
    /// assert_eq!(info.chunks, 0);
    ///
    /// # context.delete_object_store("delete").await?;
    /// # Ok(())
    /// # }
    /// ```
    pub async fn delete(&self, object_name: &str) -> io::Result<()> {
        let mut object_info = self.info(object_name).await?;
        object_info.chunks = 0;
        object_info.size = 0;
        object_info.deleted = true;

        let data = serde_json::to_vec(&object_info)?;

        let mut headers = HeaderMap::default();
        let entry = headers
            .inner
            .entry(NATS_ROLLUP.to_string())
            .or_insert_with(HashSet::default);

        entry.insert(ROLLUP_SUBJECT.to_string());

        let subject = format!("$O.{}.M.{}", &self.name, &object_name);
        let message = Message::new(&subject, None, data, Some(headers));

        self.context.publish_message(&message).await?;

        let stream_name = format!("OBJ_{}", self.name);
        let chunk_subject = format!("$O.{}.C.{}", self.name, object_info.nuid);

        self.context
            .purge_stream_subject(&stream_name, &chunk_subject)
            .await?;

        Ok(())
    }

    /// Watch for changes in the underlying store and receive meta information updates.
    ///
    /// # Example
    ///
    /// ```no_run
    /// use futures::stream::StreamExt;
    /// # use nats::object_store::Config;
    /// # #[tokio::main]
    /// # async fn main() -> std::io::Result<()> {
    /// # let client = nats::connect("demo.nats.io").await?;
    /// # let context = nats::jetstream::new(client);
    /// #
    /// let bucket = context.create_object_store(&Config {
    ///   bucket: "watch".to_string(),
    ///   ..Default::default()
    /// }).await?;
    ///
    /// let mut watch = bucket.watch().await?;
    ///
    /// let bytes = vec![0, 1, 2, 3, 4];
    /// bucket.put("foo", &mut bytes.as_slice()).await?;
    ///
    /// let info = watch.next().await.unwrap();
    /// assert_eq!(info.name, "foo");
    /// assert_eq!(info.size, bytes.len());
    ///
    /// let bytes = vec![0];
    /// bucket.put("bar", &mut bytes.as_slice()).await?;
    ///
    /// let info = watch.next().await.unwrap();
    /// assert_eq!(info.name, "bar");
    /// assert_eq!(info.size, bytes.len());
    ///
    /// # context.delete_object_store("watch").await?;
    /// # Ok(())
    /// # }
    /// ```
    pub async fn watch(&self) -> io::Result<Pin<Box<dyn Stream<Item = ObjectInfo>>>> {
        let subject = format!("$O.{}.M.>", &self.name);
        let subscription = self
            .context
            .subscribe_with_options(
                &subject,
                &SubscribeOptions::ordered().deliver_last_per_subject(),
            )
            .await?;

        Ok(Box::pin(
            Watch {
                subscription: Box::pin(subscription.stream()),
            }
            .as_stream(),
        ))
    }
}

/// Iterator returned by `watch`
pub struct Watch {
    subscription: Pin<Box<dyn Stream<Item = Message>>>,
}

impl Watch {
    // convert into unpinned stream
    fn as_stream(mut self) -> impl Stream<Item = ObjectInfo> {
        async_stream::stream! {
            while let Some(message) = self.subscription.next().await {
                yield serde_json::from_slice(&message.data).unwrap();
            }
        }
    }
}