use std::hash::Hash;
use std::future::Future;
use bytes::Bytes;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PutOutcome {
    Created,
    Replaced,
}

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

    pub fn to_string(&self) -> String { self.key.to_string() }
}

impl std::borrow::Borrow<str> for Key {
    fn borrow(&self) -> &str {
        &self.key
    }
}

pub trait ObjectInterface: Sync + Send + 'static {
    fn new(
        key: Key,
        content_type: String,
        contents: Bytes,
    ) -> Self;

    /// Returns the object's exact logical key.
    fn key(&self) -> &Key;

    /// Returns the immutable content type.
    fn content_type(&self) -> &str;

    /// Returns the immutable contents.
    fn contents(&self) -> &Bytes;
}

pub trait StoreInterface: Sync + Send + Sized + 'static {
    type Object: ObjectInterface;

    /// Builds a catalogue, rejecting any invalid object definition.
    fn new(
        objects: impl IntoIterator<Item = Self::Object>,
    ) -> Result<Self, String>;

    /// Looks up an exact logical key and cheaply clones its metadata and payload handle.
    fn get(&self, key: &str) -> impl Future<Output = Option<Self::Object>> + Send;

    /// Returns the number of objects in the catalogue.
    fn len(&self) -> impl Future<Output = usize> + Send;

    /// Returns whether the catalogue contains no objects.
    fn is_empty(&self) -> impl Future<Output = bool> + Send;

    /// Atomically inserts or replaces an object by its exact logical key.
    fn put(&self, object: Self::Object) -> impl Future<Output = PutOutcome> + Send;
}

