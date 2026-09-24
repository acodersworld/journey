use std::{
    collections::HashMap,
    sync::Arc,
    hash::Hash,
};

use bytes::Bytes;

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

/// One immutable object in a [`StaticStore`].
#[derive(Clone, Debug)]
pub struct Object {
    key: Key,
    content_type: String,
    contents: Bytes,
}

impl Object {
    /// Creates an object after validating its key and content type.
    pub fn new(
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
    pub fn key(&self) -> &Key {
        &self.key
    }

    /// Returns the immutable content type.
    pub fn content_type(&self) -> &str {
        &self.content_type
    }

    /// Returns the immutable contents.
    pub fn contents(&self) -> &Bytes {
        &self.contents
    }
}

/// A complete, immutable catalogue of objects indexed by exact logical key.
#[derive(Clone, Debug, Default)]
pub struct Store {
    objects: Arc<HashMap<Key, Object>>,
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
            objects: Arc::new(catalogue),
        })
    }

    /// Looks up an exact logical key and cheaply clones its metadata and payload handle.
    pub fn get(&self, key: &str) -> Option<Object> {
        self.objects.get(key).cloned()
    }

    /// Returns the number of objects in the catalogue.
    pub fn len(&self) -> usize {
        self.objects.len()
    }

    /// Returns whether the catalogue contains no objects.
    pub fn is_empty(&self) -> bool {
        self.objects.is_empty()
    }
}


