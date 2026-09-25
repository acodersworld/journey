use std::{
    collections::HashMap,
    sync::Arc,
};

use bytes::Bytes;
use tokio::sync::RwLock;

use crate::storage_interface::{StoreInterface, ObjectInterface, Key, PutOutcome};

/// One immutable object in a [`Store`].
#[derive(Clone, Debug)]
pub struct Object {
    key: Key,
    content_type: String,
    contents: Bytes,
}

impl ObjectInterface for Object {
    /// Creates an object from a validated key and HTTP header value.
    fn new(
        key: Key,
        content_type: String,
        contents: Bytes,
    ) -> Self {
        Self {
            key,
            content_type,
            contents,
        }
    }

    /// Returns the object's exact logical key.
    fn key(&self) -> &Key {
        &self.key
    }

    /// Returns the immutable content type.
    fn content_type(&self) -> &str {
        &self.content_type
    }

    /// Returns the immutable contents.
    fn contents(&self) -> &Bytes {
        &self.contents
    }
}

/// A mutable catalogue of complete objects indexed by exact logical key.
#[derive(Clone, Debug, Default)]
pub struct Store {
    objects: Arc<RwLock<HashMap<Key, Object>>>,
}

impl StoreInterface for Store {
    type Object = Object;

    /// Builds a catalogue, rejecting any invalid object definition.
    fn new(
        objects: impl IntoIterator<Item = Object>,
    ) -> Result<Self, String> {
        let mut catalogue = HashMap::new();
        for object in objects {
            let key = object.key.clone();
            if catalogue.insert(key.clone(), object).is_some() {
                return Err(format!("Duplicate key: {}", key.to_string()));
            }
        }
        Ok(Self {
            objects: Arc::new(RwLock::new(catalogue)),
        })
    }

    /// Looks up an exact logical key and cheaply clones its metadata and payload handle.
    async fn get(&self, key: &str) -> Option<Object> {
        self.objects.read().await.get(key).cloned()
    }

    /// Returns the number of objects in the catalogue.
    async fn len(&self) -> usize {
        self.objects.read().await.len()
    }

    /// Returns whether the catalogue contains no objects.
    async fn is_empty(&self) -> bool {
        self.objects.read().await.is_empty()
    }

    /// Atomically inserts or replaces an object by its exact logical key.
    async fn put(&self, object: Object) -> PutOutcome {
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
