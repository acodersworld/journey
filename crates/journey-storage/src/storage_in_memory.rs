use std::{
    collections::HashMap,
    sync::Arc,
};

use bytes::Bytes;
use tokio::sync::RwLock;

use crate::storage_interface::{
    ContentType, Key, ObjectInterface, PutContextInterface, StoreInterface,
};

/// One immutable object in a [`Store`].
#[derive(Clone, Debug)]
pub struct Object {
    content_type: ContentType,
    contents: Bytes,
}

impl Object {
    /// Creates an object from a validated content type.
    pub fn new(
        content_type: ContentType,
        contents: Bytes,
    ) -> Self {
        Self {
            content_type,
            contents,
        }
    }
}

impl ObjectInterface for Object {
    /// Returns the immutable content type.
    fn content_type(&self) -> &ContentType {
        &self.content_type
    }

    /// Returns the immutable contents.
    fn contents(&self) -> &Bytes {
        &self.contents
    }
}

pub struct PutContext {
    content_type: ContentType,
    bytes: Vec<u8>,
}

impl PutContextInterface for PutContext {
    async fn append(&mut self, bytes: &Bytes) -> Result<(), String> {
        self.bytes.extend_from_slice(bytes);
        Ok(())
    }
}

/// A mutable catalogue of complete objects indexed by exact logical key.
#[derive(Clone, Debug, Default)]
pub struct Store {
    objects: Arc<RwLock<HashMap<Key, Object>>>,
}

impl Store {
    /// Builds a catalogue from validated keys and objects, rejecting duplicates.
    pub fn new(
        objects: impl IntoIterator<Item = (Key, Object)>,
    ) -> Result<Self, String> {
        let mut catalogue = HashMap::new();
        for (key, object) in objects {
            if catalogue.insert(key.clone(), object).is_some() {
                return Err(format!("Duplicate key: {key}"));
            }
        }
        Ok(Self {
            objects: Arc::new(RwLock::new(catalogue)),
        })
    }
}

impl StoreInterface for Store {
    type Object = Object;
    type PutContext = PutContext;

    /// Looks up an exact logical key and cheaply clones its metadata and payload handle.
    async fn get(&self, key: &str) -> Result<Option<Object>, String> {
        Ok(self.objects.read().await.get(key).cloned())
    }

    /// Returns the number of objects in the catalogue.
    async fn len(&self) -> usize {
        self.objects.read().await.len()
    }

    /// Returns whether the catalogue contains no objects.
    async fn is_empty(&self) -> bool {
        self.objects.read().await.is_empty()
    }

    async fn put_context(&self, content_type: ContentType) -> Result<PutContext, String> {
        Ok(PutContext {
            content_type,
            bytes: vec![],
        })
    }

    /// Atomically inserts or replaces an object by its exact logical key.
    async fn put(&self, key: &Key, put_context: PutContext) -> Result<(), String> {
        let object = Object {
            content_type: put_context.content_type,
            contents: put_context.bytes.into(),
        };

        let mut objects = self.objects.write().await;
        match objects.entry(key.clone()) {
            std::collections::hash_map::Entry::Occupied(mut entry) => {
                entry.insert(object);
            }
            std::collections::hash_map::Entry::Vacant(entry) => {
                entry.insert(object);
            }
        }

        Ok(())
    }
}
