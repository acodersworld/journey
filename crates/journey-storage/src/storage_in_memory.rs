use std::{
    collections::BTreeMap,
    num::NonZeroUsize,
    ops::Bound::{Excluded, Included, Unbounded},
    sync::Arc,
};

use bytes::Bytes;
use tokio::sync::RwLock;

use crate::storage_interface::{
    ContentType, Key, ListCursor, ListPage, ListRequest, ObjectMetadata, ObjectInterface,
    PutContextInterface, ReadObject, StoreError, StoreErrorKind, StoreInterface,
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
    fn contents(&self) -> &Bytes {
        &self.contents
    }
}

pub struct PutContext {
    content_type: ContentType,
    bytes: Vec<u8>,
}

impl PutContextInterface for PutContext {
    async fn append(&mut self, bytes: &Bytes) -> Result<(), StoreError> {
        self.bytes.extend_from_slice(bytes);
        Ok(())
    }
}

#[derive(Clone, Debug)]
pub struct StoreConfig {
    max_list_page_size: NonZeroUsize,
}

impl StoreConfig {
    pub fn new(max_list_page_size: NonZeroUsize) -> Self {
        Self { max_list_page_size }
    }

    pub fn max_list_page_size(&self) -> NonZeroUsize {
        self.max_list_page_size
    }
}

impl Default for StoreConfig {
    fn default() -> Self {
        Self {
            max_list_page_size: NonZeroUsize::new(1_000).unwrap(),
        }
    }
}

/// A mutable catalogue of complete objects indexed by exact logical key.
#[derive(Clone, Debug)]
pub struct Store {
    config: StoreConfig,
    objects: Arc<RwLock<BTreeMap<Key, Object>>>,
}

impl Store {
    /// Builds a catalogue from validated keys and objects, rejecting duplicates.
    pub fn new(
        objects: impl IntoIterator<Item = (Key, Object)>,
    ) -> Result<Self, StoreError> {
        Self::with_config(StoreConfig::default(), objects)
    }

    /// Builds a catalogue using an explicit maximum LIST page size.
    pub fn with_config(
        config: StoreConfig,
        objects: impl IntoIterator<Item = (Key, Object)>,
    ) -> Result<Self, StoreError> {
        let mut catalogue = BTreeMap::new();
        for (key, object) in objects {
            if catalogue.insert(key.clone(), object).is_some() {
                return Err(StoreError::new(
                    StoreErrorKind::InvalidRequest,
                    format!("Duplicate key: {key}"),
                ));
            }
        }
        Ok(Self {
            config,
            objects: Arc::new(RwLock::new(catalogue)),
        })
    }

    fn metadata_for(key: &Key, object: &Object) -> Result<ObjectMetadata, StoreError> {
        let payload_length = u64::try_from(object.contents.len()).map_err(|error| {
            StoreError::new(
                StoreErrorKind::Internal,
                format!("Payload length does not fit in u64: {error}"),
            )
        })?;

        Ok(ObjectMetadata::new(
            key.clone(),
            object.content_type.clone(),
            payload_length,
        ))
    }
}

impl Default for Store {
    fn default() -> Self {
        Self::new(std::iter::empty()).unwrap()
    }
}

impl StoreInterface for Store {
    type Object = Object;
    type PutContext = PutContext;

    async fn get(&self, key: &Key) -> Result<ReadObject<Object>, StoreError> {
        let objects = self.objects.read().await;
        let object = objects.get(key).ok_or_else(|| {
            StoreError::new(StoreErrorKind::NotFound, format!("Object not found: {key}"))
        })?;
        let metadata = Self::metadata_for(key, object)?;
        Ok(ReadObject::new(metadata, object.clone()))
    }

    async fn stat(&self, key: &Key) -> Result<ObjectMetadata, StoreError> {
        let objects = self.objects.read().await;
        let object = objects.get(key).ok_or_else(|| {
            StoreError::new(StoreErrorKind::NotFound, format!("Object not found: {key}"))
        })?;
        Self::metadata_for(key, object)
    }

    async fn list(&self, request: ListRequest) -> Result<ListPage, StoreError> {
        let objects = self.objects.read().await;
        if request
            .cursor()
            .is_some_and(|cursor| cursor.prefix() != request.prefix())
        {
            return Err(StoreError::new(
                StoreErrorKind::InvalidRequest,
                "List cursor prefix does not match request prefix",
            ));
        }

        let effective_limit = self
            .config
            .max_list_page_size
            .min(request.requested_limit());
        let lower_bound = match request.cursor() {
            Some(cursor) => Excluded(cursor.last_key().as_str()),
            None => Included(request.prefix()),
        };
        let mut metadata = Vec::new();
        let mut last_key = None;
        let mut has_more = false;
        for (key, object) in objects.range::<str, _>((lower_bound, Unbounded)) {
            if !key.as_str().starts_with(request.prefix()) {
                break;
            }
            if metadata.len() == effective_limit.get() {
                has_more = true;
                break;
            }
            metadata.push(Self::metadata_for(key, object)?);
            last_key = Some(key.clone());
        }

        let next_cursor = if has_more {
            last_key.map(|last_key| ListCursor::new(request.prefix().to_string(), last_key))
        } else {
            None
        };

        Ok(ListPage::new(metadata, next_cursor))
    }

    async fn delete(&self, key: &Key) -> Result<(), StoreError> {
        self.objects.write().await.remove(key);
        Ok(())
    }

    async fn put_context(&self, content_type: ContentType) -> Result<PutContext, StoreError> {
        Ok(PutContext {
            content_type,
            bytes: vec![],
        })
    }

    /// Atomically inserts or replaces an object by its exact logical key.
    async fn put(&self, key: &Key, put_context: PutContext) -> Result<(), StoreError> {
        let object = Object {
            content_type: put_context.content_type,
            contents: put_context.bytes.into(),
        };

        self.objects.write().await.insert(key.clone(), object);
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use http::HeaderValue;

    fn content_type(value: &str) -> ContentType {
        ContentType::try_from_header(&HeaderValue::from_str(value).unwrap()).unwrap()
    }

    fn object(key: &str, contents: &'static [u8]) -> (Key, Object) {
        (
            Key::new(key).unwrap(),
            Object::new(content_type("application/octet-stream"), Bytes::from_static(contents)),
        )
    }

    fn list_request(prefix: &str, limit: usize) -> ListRequest {
        ListRequest::new(prefix, None, NonZeroUsize::new(limit).unwrap())
    }

    #[test]
    fn default_list_page_limit_is_one_thousand() {
        assert_eq!(
            StoreConfig::default().max_list_page_size(),
            NonZeroUsize::new(1_000).unwrap()
        );
    }

    fn names(page: &ListPage) -> Vec<String> {
        page.objects()
            .iter()
            .map(|metadata| metadata.key().as_str().to_string())
            .collect()
    }

    #[tokio::test]
    async fn get_and_stat_return_matching_metadata_and_get_returns_exact_bytes() {
        let store = Store::new([
            object("non-empty", b"payload bytes"),
            object("empty", b""),
        ])
        .unwrap();
        let key = Key::new("non-empty").unwrap();

        let read = store.get(&key).await.unwrap();
        let metadata = store.stat(&key).await.unwrap();
        assert_eq!(read.metadata(), &metadata);
        assert_eq!(read.metadata().key().as_str(), "non-empty");
        assert_eq!(read.metadata().payload_length(), 13);
        assert_eq!(read.object().contents().as_ref(), b"payload bytes");

        let empty = store.stat(&Key::new("empty").unwrap()).await.unwrap();
        assert_eq!(empty.payload_length(), 0);
    }

    #[tokio::test]
    async fn missing_get_and_stat_return_not_found() {
        let store = Store::default();
        let key = Key::new("missing").unwrap();

        assert_eq!(store.get(&key).await.unwrap_err().kind(), StoreErrorKind::NotFound);
        assert_eq!(store.stat(&key).await.unwrap_err().kind(), StoreErrorKind::NotFound);
    }

    #[tokio::test]
    async fn replacement_updates_metadata_and_payload_together() {
        let store = Store::new([object("item", b"old")]).unwrap();
        let key = Key::new("item").unwrap();
        let mut context = store.put_context(content_type("text/plain")).await.unwrap();
        context.append(&Bytes::from_static(b"replacement")).await.unwrap();
        store.put(&key, context).await.unwrap();

        let read = store.get(&key).await.unwrap();
        assert_eq!(read.metadata().payload_length(), 11);
        assert_eq!(read.metadata().content_type().as_header_value(), "text/plain");
        assert_eq!(read.object().contents().as_ref(), b"replacement");
    }

    #[tokio::test]
    async fn listing_filters_prefix_and_orders_utf8_bytes_lexicographically() {
        let store = Store::new([
            object("p/Ω", b"omega"),
            object("p/z", b"z"),
            object("q/a", b"other prefix"),
            object("p/é", b"accent"),
        ])
        .unwrap();
        let page = store.list(list_request("p/", 10)).await.unwrap();

        assert_eq!(names(&page), ["p/z", "p/é", "p/Ω"]);
        assert!(page.next_cursor().is_none());
        assert!(store.list(list_request("absent", 10)).await.unwrap().objects().is_empty());
    }

    #[tokio::test]
    async fn listing_clamps_requested_limit_and_detects_final_full_page() {
        let store = Store::with_config(
            StoreConfig::new(NonZeroUsize::new(2).unwrap()),
            [object("a", b"1"), object("b", b"2"), object("c", b"3"), object("d", b"4")],
        )
        .unwrap();

        let first = store.list(list_request("", 1)).await.unwrap();
        assert_eq!(names(&first), ["a"]);
        assert!(first.next_cursor().is_some());

        let first_clamped = store.list(list_request("", 20)).await.unwrap();
        assert_eq!(names(&first_clamped), ["a", "b"]);
        let cursor = first_clamped.next_cursor().unwrap().clone();
        let final_page = store
            .list(ListRequest::new("", Some(cursor), NonZeroUsize::new(20).unwrap()))
            .await
            .unwrap();
        assert_eq!(names(&final_page), ["c", "d"]);
        assert!(final_page.next_cursor().is_none());
    }

    #[tokio::test]
    async fn unchanged_store_paginates_without_repeats_or_omissions() {
        let store = Store::new([
            object("a", b"1"),
            object("b", b"2"),
            object("c", b"3"),
            object("d", b"4"),
            object("e", b"5"),
        ])
        .unwrap();
        let limit = NonZeroUsize::new(2).unwrap();
        let mut cursor = None;
        let mut visited = Vec::new();

        loop {
            let page = store
                .list(ListRequest::new("", cursor, limit))
                .await
                .unwrap();
            visited.extend(names(&page));
            cursor = page.next_cursor().cloned();
            if cursor.is_none() {
                break;
            }
        }

        assert_eq!(visited, ["a", "b", "c", "d", "e"]);
    }

    #[tokio::test]
    async fn continuation_survives_deleting_the_cursor_key() {
        let store = Store::new([
            object("a", b"1"),
            object("b", b"2"),
            object("c", b"3"),
            object("d", b"4"),
        ])
        .unwrap();
        let first = store.list(list_request("", 2)).await.unwrap();
        let cursor = first.next_cursor().unwrap().clone();
        store.delete(&Key::new("b").unwrap()).await.unwrap();

        let next = store
            .list(ListRequest::new("", Some(cursor), NonZeroUsize::new(2).unwrap()))
            .await
            .unwrap();
        assert_eq!(names(&next), ["c", "d"]);
        assert!(next.next_cursor().is_none());
    }

    #[tokio::test]
    async fn cursor_prefix_mismatch_is_invalid_request() {
        let store = Store::new([object("a/1", b"1"), object("a/2", b"2"), object("b/1", b"3")]).unwrap();
        let first = store.list(list_request("a/", 1)).await.unwrap();
        let cursor = first.next_cursor().unwrap().clone();

        let error = store
            .list(ListRequest::new("b/", Some(cursor), NonZeroUsize::new(1).unwrap()))
            .await
            .unwrap_err();
        assert_eq!(error.kind(), StoreErrorKind::InvalidRequest);
    }

    #[tokio::test]
    async fn empty_listing_has_no_cursor() {
        let store = Store::default();
        let page = store.list(list_request("", 5)).await.unwrap();
        assert!(page.objects().is_empty());
        assert!(page.next_cursor().is_none());
    }

    #[tokio::test]
    async fn delete_is_idempotent_for_present_and_absent_keys() {
        let store = Store::new([object("present", b"payload")]).unwrap();
        let present = Key::new("present").unwrap();
        let absent = Key::new("absent").unwrap();

        store.delete(&present).await.unwrap();
        store.delete(&present).await.unwrap();
        store.delete(&absent).await.unwrap();
        assert_eq!(store.stat(&present).await.unwrap_err().kind(), StoreErrorKind::NotFound);
    }

    #[test]
    fn duplicate_initial_keys_are_invalid_requests() {
        let error = Store::new([object("same", b"first"), object("same", b"second")])
            .unwrap_err();
        assert_eq!(error.kind(), StoreErrorKind::InvalidRequest);
        assert!(error.to_string().contains("Duplicate key: same"));
    }

    #[test]
    fn store_error_exposes_kind_and_local_diagnostic_display() {
        let error = StoreError::new(StoreErrorKind::Internal, "disk detail");
        assert_eq!(error.kind(), StoreErrorKind::Internal);
        assert!(error.to_string().contains("disk detail"));
    }
}
