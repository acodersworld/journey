use bytes::Bytes;
use http::HeaderValue;
use std::{
    fmt,
    future::Future,
    hash::Hash,
};

#[derive(Clone, Debug)]
pub struct ContentType(HeaderValue);

impl ContentType {
    pub fn try_from_header(value: &HeaderValue) -> Result<Self, ContentTypeError> {
        if value.is_empty() || value.to_str().is_err() {
            return Err(ContentTypeError);
        }

        Ok(Self(value.clone()))
    }

    pub fn as_header_value(&self) -> &HeaderValue {
        &self.0
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ContentTypeError;

impl fmt::Display for ContentTypeError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("invalid content type")
    }
}

impl std::error::Error for ContentTypeError {}

#[derive(Clone, Debug, Hash, PartialEq, Eq)]
pub struct Key {
    key: String,
}

impl Key {
    pub fn new(key: &str) -> Result<Self, String> {
        if key.is_empty() {
            return Err("Empty key".to_string());
        }

        Ok(Key { key: key.to_string() })
    }
}

impl std::fmt::Display for Key {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.key)
    }
}

impl std::borrow::Borrow<str> for Key {
    fn borrow(&self) -> &str {
        &self.key
    }
}

pub trait ObjectInterface: Sync + Send + 'static {
    /// Returns the immutable content type.
    fn content_type(&self) -> &ContentType;

    /// Returns the immutable contents.
    fn contents(&self) -> &Bytes;
}

pub trait PutContextInterface {
    fn append(&mut self, bytes: &Bytes) -> impl Future<Output = Result<(), String>> + Send;
}

pub trait StoreInterface: Sync + Send + Sized + 'static {
    type Object: ObjectInterface;
    type PutContext: PutContextInterface + Sync + Send + 'static;

    /// Looks up an exact logical key and cheaply clones its metadata and payload handle.
    fn get(&self, key: &str) -> impl Future<Output = Result<Option<Self::Object>, String>> + Send;

    /// Returns the number of objects in the catalogue.
    fn len(&self) -> impl Future<Output = usize> + Send;

    /// Returns whether the catalogue contains no objects.
    fn is_empty(&self) -> impl Future<Output = bool> + Send;

    fn put_context(&self, content_type: ContentType) -> impl Future<Output = Result<Self::PutContext, String>> + Send;

    /// Atomically inserts or replaces an object by its exact logical key.
    fn put(&self, key: &Key, put_context: Self::PutContext) -> impl Future<Output = Result<(), String>> + Send;
}
