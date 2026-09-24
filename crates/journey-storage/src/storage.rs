use std::{
    collections::HashMap,
    sync::Arc,
    hash::Hash,
};

use bytes::Bytes;
use http::HeaderValue;
use tokio::sync::RwLock;

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

impl std::borrow::Borrow<str> for Key {
    fn borrow(&self) -> &str {
        &self.key
    }
}

/// One immutable object in a [`Store`].
#[derive(Clone, Debug)]
pub struct Object {
    key: Key,
    content_type: HeaderValue,
    contents: Bytes,
}

impl Object {
    /// Creates an object from a validated key and HTTP header value.
    pub fn new(
        key: Key,
        content_type: HeaderValue,
        contents: Bytes,
    ) -> Self {
        Self {
            key,
            content_type,
            contents,
        }
    }

    /// Returns the object's exact logical key.
    pub fn key(&self) -> &Key {
        &self.key
    }

    /// Returns the immutable content type.
    pub fn content_type(&self) -> &HeaderValue {
        &self.content_type
    }

    /// Returns the immutable contents.
    pub fn contents(&self) -> &Bytes {
        &self.contents
    }
}

/// A mutable catalogue of complete objects indexed by exact logical key.
#[derive(Clone, Debug, Default)]
pub struct Store {
    objects: Arc<RwLock<HashMap<Key, Object>>>,
}

/// The result of atomically publishing an object into a [`Store`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PutOutcome {
    Created,
    Replaced,
}

impl Store {
    /// Builds a catalogue, rejecting any invalid object definition.
    pub fn new(
        objects: impl IntoIterator<Item = Object>,
    ) -> Result<Self, String> {
        let mut catalogue = HashMap::new();
        for object in objects {
            let key = object.key.clone();
            if catalogue.insert(key.clone(), object).is_some() {
                return Err(format!("Duplicate key: {}", key.key.clone()));
            }
        }
        Ok(Self {
            objects: Arc::new(RwLock::new(catalogue)),
        })
    }

    /// Looks up an exact logical key and cheaply clones its metadata and payload handle.
    pub async fn get(&self, key: &str) -> Option<Object> {
        self.objects.read().await.get(key).cloned()
    }

    /// Returns the number of objects in the catalogue.
    pub async fn len(&self) -> usize {
        self.objects.read().await.len()
    }

    /// Returns whether the catalogue contains no objects.
    pub async fn is_empty(&self) -> bool {
        self.objects.read().await.is_empty()
    }

    /// Atomically inserts or replaces an object by its exact logical key.
    pub async fn put(&self, object: Object) -> PutOutcome {
        let mut objects = self.objects.write().await;
        match objects.entry(object.key.clone()) {
            std::collections::hash_map::Entry::Occupied(mut entry) => {
                entry.insert(object);
                PutOutcome::Replaced
            }
            std::collections::hash_map::Entry::Vacant(entry) => {
                entry.insert(object);
                PutOutcome::Created
            }
        }
    }
}
