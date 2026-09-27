use std::{
    collections::BTreeMap,
    num::NonZeroUsize,
    ops::Range,
    ops::Bound::{Included, Unbounded},
    sync::Arc,
};

use bytes::{Bytes, BytesMut};
use tokio::sync::RwLock;

use crate::storage_interface::{
    ContentType, Key, ListCursor, ListPage, ListRequest, ObjectMetadata, ObjectInterface, PutCondition,
    GetResult, PutContextInterface, ReadObject, ReadRange, ReadSpan, StoreError, StoreErrorKind,
    StoreInterface,
};

/// One immutable object in a [`Store`].
#[derive(Debug)]
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

/// A per-GET reader over an immutable view of an object's selected bytes.
#[derive(Debug)]
pub struct ObjectReader {
    contents: Bytes,
    cursor: usize,
}

impl ObjectReader {
    fn new(contents: Bytes) -> Self {
        Self { contents, cursor: 0 }
    }
}

impl ObjectInterface for ObjectReader {
    async fn read(
        &mut self,
        buffer: &mut BytesMut,
    ) -> Result<usize, StoreError> {
        let count = buffer.len().min(self.contents.len() - self.cursor);
        buffer[..count]
            .copy_from_slice(&self.contents[self.cursor..self.cursor + count]);
        self.cursor += count;
        Ok(count)
    }
}

pub struct PutContext {
    key: Key,
    content_type: ContentType,
    condition: PutCondition,
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
#[derive(Debug)]
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

    fn resolve_range(range: ReadRange, total: u64) -> Result<Option<ReadSpan>, StoreError> {
        let span = match range {
            ReadRange::Closed { start, end } => {
                if end < start {
                    return Err(StoreError::new(
                        StoreErrorKind::InvalidRequest,
                        "Range end precedes range start",
                    ));
                }
                if start >= total {
                    return Ok(None);
                }
                let end = end.min(total - 1);
                let size = end
                    .checked_sub(start)
                    .and_then(|size| size.checked_add(1))
                    .ok_or_else(|| {
                        StoreError::new(StoreErrorKind::Internal, "Range size overflow")
                    })?;
                ReadSpan::new(start, size)
            }
            ReadRange::From { start } => {
                if start >= total {
                    return Ok(None);
                }
                ReadSpan::new(start, total - start)
            }
            ReadRange::Suffix { length } => {
                if length == 0 || total == 0 {
                    return Ok(None);
                }
                let size = length.min(total);
                ReadSpan::new(total - size, size)
            }
        };
        Ok(Some(span))
    }

    fn span_bounds(span: ReadSpan) -> Result<Range<usize>, StoreError> {
        let offset = usize::try_from(span.offset()).map_err(|error| {
            StoreError::new(
                StoreErrorKind::Internal,
                format!("Range offset does not fit in usize: {error}"),
            )
        })?;
        let size = usize::try_from(span.size()).map_err(|error| {
            StoreError::new(
                StoreErrorKind::Internal,
                format!("Range size does not fit in usize: {error}"),
            )
        })?;
        let end = offset.checked_add(size).ok_or_else(|| {
            StoreError::new(StoreErrorKind::Internal, "Range end does not fit in usize")
        })?;
        Ok(offset..end)
    }
}

impl Default for Store {
    fn default() -> Self {
        Self::new(std::iter::empty()).unwrap()
    }
}

impl StoreInterface for Store {
    type Object = ObjectReader;
    type PutContext = PutContext;

    async fn get(
        &self,
        key: &Key,
        range: Option<ReadRange>,
    ) -> Result<GetResult<ObjectReader>, StoreError> {
        let objects = self.objects.read().await;
        let object = objects.get(key).ok_or_else(|| {
            StoreError::new(StoreErrorKind::NotFound, format!("Object not found: {key}"))
        })?;
        let metadata = Self::metadata_for(key, object)?;
        let Some(range) = range else {
            return Ok(GetResult::Found(ReadObject::new(
                metadata,
                ObjectReader::new(object.contents.clone()),
            )));
        };
        let Some(selected_span) = Self::resolve_range(range, metadata.payload_length())? else {
            return Ok(GetResult::Unsatisfiable {
                complete_length: metadata.payload_length(),
            });
        };
        let contents = object.contents.clone().slice(Self::span_bounds(selected_span)?);
        Ok(GetResult::Found(ReadObject::with_selected_span(
            metadata,
            ObjectReader::new(contents),
            selected_span,
        )))
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
        let effective_limit = self
            .config
            .max_list_page_size
            .min(request.requested_limit());
        let lower_bound = match request.cursor() {
            Some(cursor) if cursor.start_key().as_str() > request.prefix() => {
                Included(cursor.start_key().as_str())
            }
            Some(_) | None => Included(request.prefix()),
        };
        let mut metadata = Vec::new();
        let mut next_page_key = None;
        for (key, object) in objects.range::<str, _>((lower_bound, Unbounded)) {
            if !key.as_str().starts_with(request.prefix()) {
                break;
            }
            if metadata.len() == effective_limit.get() {
                next_page_key = Some(key.clone());
                break;
            }
            metadata.push(Self::metadata_for(key, object)?);
        }

        let next_cursor = next_page_key.map(ListCursor::new);

        Ok(ListPage::new(metadata, next_cursor))
    }

    async fn delete(&self, key: &Key) -> Result<(), StoreError> {
        self.objects.write().await.remove(key);
        Ok(())
    }

    async fn put_context(
        &self,
        key: Key,
        content_type: ContentType,
        condition: PutCondition,
    ) -> Result<PutContext, StoreError> {
        Ok(PutContext {
            key,
            content_type,
            condition,
            bytes: vec![],
        })
    }

    /// Atomically inserts or replaces an object by the key owned by its context.
    async fn put(&self, put_context: PutContext) -> Result<(), StoreError> {
        let key = put_context.key;
        let mut objects = self.objects.write().await;
        let key_is_present = objects.contains_key(&key);
        let condition_satisfied = match put_context.condition {
            PutCondition::Unconditional => true,
            PutCondition::CreateOnly => !key_is_present,
            PutCondition::ReplaceOnly => key_is_present,
        };
        if !condition_satisfied {
            return Err(StoreError::new(
                StoreErrorKind::PreconditionFailed,
                format!("PUT condition failed for key: {key}"),
            ));
        }

        let object = Object {
            content_type: put_context.content_type,
            contents: put_context.bytes.into(),
        };

        objects.insert(key, object);
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

    fn found(result: GetResult<ObjectReader>) -> ReadObject<ObjectReader> {
        match result {
            GetResult::Found(read_object) => read_object,
            GetResult::Unsatisfiable { .. } => panic!("complete read was unsatisfiable"),
        }
    }

    async fn collect_object(mut read: ReadObject<ObjectReader>) -> Bytes {
        let mut buffer = BytesMut::zeroed(3);
        let mut contents = Vec::new();
        loop {
            let count = read.object_mut().read(&mut buffer).await.unwrap();
            if count == 0 {
                break;
            }
            contents.extend_from_slice(&buffer[..count]);
        }
        Bytes::from(contents)
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

        let read = found(store.get(&key, None).await.unwrap());
        let metadata = store.stat(&key).await.unwrap();
        assert_eq!(read.metadata(), &metadata);
        assert_eq!(read.selected_span(), None);
        assert_eq!(read.metadata().key().as_str(), "non-empty");
        assert_eq!(read.metadata().payload_length(), 13);
        assert_eq!(collect_object(read).await.as_ref(), b"payload bytes");

        let empty = store.stat(&Key::new("empty").unwrap()).await.unwrap();
        assert_eq!(empty.payload_length(), 0);
    }

    #[tokio::test]
    async fn get_readers_have_independent_cursors_and_zero_length_reads_do_not_advance() {
        let store = Store::new([object("item", b"abcdef")]).unwrap();
        let key = Key::new("item").unwrap();
        let mut first = found(store.get(&key, None).await.unwrap());
        let mut second = found(store.get(&key, None).await.unwrap());
        let mut buffer = BytesMut::zeroed(2);

        assert_eq!(first.object_mut().read(&mut buffer).await.unwrap(), 2);
        assert_eq!(&buffer[..], b"ab");
        assert_eq!(buffer.len(), 2);

        buffer.clear();
        assert_eq!(first.object_mut().read(&mut buffer).await.unwrap(), 0);
        assert!(buffer.is_empty());

        buffer.resize(3, 0);
        assert_eq!(first.object_mut().read(&mut buffer).await.unwrap(), 3);
        assert_eq!(&buffer[..], b"cde");

        buffer.resize(2, 0);
        assert_eq!(second.object_mut().read(&mut buffer).await.unwrap(), 2);
        assert_eq!(&buffer[..], b"ab");
        assert_eq!(second.object_mut().read(&mut buffer).await.unwrap(), 2);
        assert_eq!(&buffer[..], b"cd");
        assert_eq!(second.object_mut().read(&mut buffer).await.unwrap(), 2);
        assert_eq!(&buffer[..], b"ef");
        assert_eq!(second.object_mut().read(&mut buffer).await.unwrap(), 0);
    }

    #[tokio::test]
    async fn ranged_get_resolves_closed_open_ended_suffix_and_clamped_ranges() {
        let contents = Bytes::from_static(b"0123456789");
        let original_ptr = contents.as_ptr();
        let store = Store::new([(
            Key::new("item").unwrap(),
            Object::new(content_type("application/octet-stream"), contents),
        )])
        .unwrap();
        let key = Key::new("item").unwrap();

        let cases = [
            (ReadRange::Closed { start: 2, end: 5 }, 2, 4, b"2345".as_slice()),
            (ReadRange::From { start: 7 }, 7, 3, b"789".as_slice()),
            (ReadRange::Suffix { length: 4 }, 6, 4, b"6789".as_slice()),
            (ReadRange::Closed { start: 6, end: 100 }, 6, 4, b"6789".as_slice()),
            (ReadRange::Suffix { length: 20 }, 0, 10, b"0123456789".as_slice()),
        ];

        for (range, offset, size, expected) in cases {
            let read = match store.get(&key, Some(range)).await.unwrap() {
                GetResult::Found(read) => read,
                GetResult::Unsatisfiable { .. } => panic!("valid range was unsatisfiable"),
            };
            assert_eq!(read.metadata().payload_length(), 10);
            assert_eq!(read.selected_span(), Some(ReadSpan::new(offset, size)));
            assert_eq!(collect_object(read).await.as_ref(), expected);
        }

        let complete = found(store.get(&key, None).await.unwrap());
        assert_eq!(complete.object().contents.as_ptr(), original_ptr);
        let ranged = match store
            .get(&key, Some(ReadRange::Closed { start: 2, end: 5 }))
            .await
            .unwrap()
        {
            GetResult::Found(read) => read,
            GetResult::Unsatisfiable { .. } => panic!("valid range was unsatisfiable"),
        };
        assert_eq!(
            ranged.object().contents.as_ptr(),
            original_ptr.wrapping_add(2)
        );
    }

    #[tokio::test]
    async fn ranged_get_reports_unsatisfiable_length_and_rejects_reversed_bounds() {
        let store = Store::new([object("item", b"123"), object("empty", b"")]).unwrap();

        for (key, range, expected_length) in [
            ("item", ReadRange::Closed { start: 3, end: 4 }, 3),
            ("item", ReadRange::From { start: 30 }, 3),
            ("item", ReadRange::Suffix { length: 0 }, 3),
            ("empty", ReadRange::Closed { start: 0, end: 0 }, 0),
            ("empty", ReadRange::Suffix { length: 1 }, 0),
        ] {
            assert!(matches!(
                store.get(&Key::new(key).unwrap(), Some(range)).await.unwrap(),
                GetResult::Unsatisfiable { complete_length } if complete_length == expected_length
            ));
        }

        let error = store
            .get(
                &Key::new("item").unwrap(),
                Some(ReadRange::Closed { start: 2, end: 1 }),
            )
            .await
            .unwrap_err();
        assert_eq!(error.kind(), StoreErrorKind::InvalidRequest);
    }

    #[tokio::test]
    async fn captured_ranged_get_keeps_its_metadata_span_and_bytes_after_replacement() {
        let store = Store::new([object("item", b"old-version")]).unwrap();
        let key = Key::new("item").unwrap();
        let captured = match store
            .get(&key, Some(ReadRange::Closed { start: 1, end: 4 }))
            .await
            .unwrap()
        {
            GetResult::Found(read) => read,
            GetResult::Unsatisfiable { .. } => panic!("valid range was unsatisfiable"),
        };

        let mut context = store
            .put_context(key.clone(), content_type("text/plain"), PutCondition::Unconditional)
            .await
            .unwrap();
        context.append(&Bytes::from_static(b"new and longer version")).await.unwrap();
        store.put(context).await.unwrap();

        assert_eq!(captured.metadata().payload_length(), 11);
        assert_eq!(captured.metadata().content_type().as_header_value(), "application/octet-stream");
        assert_eq!(captured.selected_span(), Some(ReadSpan::new(1, 4)));
        assert_eq!(collect_object(captured).await.as_ref(), b"ld-v");
        let current = found(store.get(&key, None).await.unwrap());
        assert_eq!(current.metadata().payload_length(), 22);
        assert_eq!(current.metadata().content_type().as_header_value(), "text/plain");
        assert_eq!(collect_object(current).await.as_ref(), b"new and longer version");
    }

    #[tokio::test]
    async fn missing_get_and_stat_return_not_found() {
        let store = Store::default();
        let key = Key::new("missing").unwrap();

        assert_eq!(store.get(&key, None).await.unwrap_err().kind(), StoreErrorKind::NotFound);
        assert_eq!(store.stat(&key).await.unwrap_err().kind(), StoreErrorKind::NotFound);
    }

    #[tokio::test]
    async fn replacement_updates_metadata_and_payload_together() {
        let store = Store::new([object("item", b"old")]).unwrap();
        let key = Key::new("item").unwrap();
        let mut context = store
            .put_context(key.clone(), content_type("text/plain"), PutCondition::Unconditional)
            .await
            .unwrap();
        context.append(&Bytes::from_static(b"replacement")).await.unwrap();
        store.put(context).await.unwrap();

        let read = found(store.get(&key, None).await.unwrap());
        assert_eq!(read.metadata().payload_length(), 11);
        assert_eq!(read.metadata().content_type().as_header_value(), "text/plain");
        assert_eq!(collect_object(read).await.as_ref(), b"replacement");
    }

    #[tokio::test]
    async fn put_conditions_check_presence_atomically_and_preserve_failed_objects() {
        let store = Store::new([object("present", b"original")]).unwrap();
        let present = Key::new("present").unwrap();
        let absent = Key::new("absent").unwrap();
        let missing = Key::new("missing").unwrap();

        let mut create = store
            .put_context(absent.clone(), content_type("text/plain"), PutCondition::CreateOnly)
            .await
            .unwrap();
        create.append(&Bytes::from_static(b"created")).await.unwrap();
        store.put(create).await.unwrap();
        assert_eq!(collect_object(found(store.get(&absent, None).await.unwrap())).await.as_ref(), b"created");

        let empty_create = store
            .put_context(present.clone(), content_type("application/json"), PutCondition::CreateOnly)
            .await
            .unwrap();
        let error = store.put(empty_create).await.unwrap_err();
        assert_eq!(error.kind(), StoreErrorKind::PreconditionFailed);
        let unchanged = found(store.get(&present, None).await.unwrap());
        assert_eq!(unchanged.metadata().content_type().as_header_value(), "application/octet-stream");
        assert_eq!(collect_object(unchanged).await.as_ref(), b"original");

        let empty_replace = store
            .put_context(missing.clone(), content_type("text/plain"), PutCondition::ReplaceOnly)
            .await
            .unwrap();
        assert_eq!(store.put(empty_replace).await.unwrap_err().kind(), StoreErrorKind::PreconditionFailed);
        assert_eq!(store.get(&missing, None).await.unwrap_err().kind(), StoreErrorKind::NotFound);
        assert_eq!(collect_object(found(store.get(&absent, None).await.unwrap())).await.as_ref(), b"created");

        let mut replace = store
            .put_context(present.clone(), content_type("application/json"), PutCondition::ReplaceOnly)
            .await
            .unwrap();
        replace.append(&Bytes::from_static(b"replaced")).await.unwrap();
        store.put(replace).await.unwrap();
        let replaced = found(store.get(&present, None).await.unwrap());
        assert_eq!(replaced.metadata().content_type().as_header_value(), "application/json");
        assert_eq!(collect_object(replaced).await.as_ref(), b"replaced");
    }

    #[tokio::test]
    async fn simultaneous_create_only_puts_publish_exactly_one_object() {
        let store = Store::default();
        let key = Key::new("race").unwrap();
        let first = store
            .put_context(key.clone(), content_type("text/plain"), PutCondition::CreateOnly)
            .await
            .unwrap();
        let second = store
            .put_context(key.clone(), content_type("application/json"), PutCondition::CreateOnly)
            .await
            .unwrap();
        let (first_result, second_result) = tokio::join!(store.put(first), store.put(second));

        assert_ne!(first_result.is_ok(), second_result.is_ok());
        assert_eq!(
            [first_result, second_result]
                .iter()
                .filter(|result| matches!(result, Err(error) if error.kind() == StoreErrorKind::PreconditionFailed))
                .count(),
            1
        );
        assert_eq!(object_count_for_test(&store).await, 1);
    }

    #[tokio::test]
    async fn replacement_and_delete_serialize_without_partial_publication() {
        let store = Store::new([object("race", b"original")]).unwrap();
        let key = Key::new("race").unwrap();
        let mut replacement = store
            .put_context(key.clone(), content_type("text/plain"), PutCondition::Unconditional)
            .await
            .unwrap();
        replacement.append(&Bytes::from_static(b"replacement")).await.unwrap();

        let (put_result, delete_result) = tokio::join!(store.put(replacement), store.delete(&key));
        put_result.unwrap();
        delete_result.unwrap();

        match store.get(&key, None).await {
            Err(error) => assert_eq!(error.kind(), StoreErrorKind::NotFound),
            Ok(result) => {
                let published = found(result);
                assert_eq!(published.metadata().content_type().as_header_value(), "text/plain");
                assert_eq!(collect_object(published).await.as_ref(), b"replacement");
            }
        }
    }

    async fn object_count_for_test(store: &Store) -> usize {
        store.list(list_request("", 10)).await.unwrap().objects().len()
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
    async fn continuation_starts_at_the_next_key_even_if_it_was_deleted() {
        let store = Store::new([
            object("a", b"1"),
            object("b", b"2"),
            object("c", b"3"),
            object("d", b"4"),
        ])
        .unwrap();
        let first = store.list(list_request("", 2)).await.unwrap();
        let cursor = first.next_cursor().unwrap().clone();
        store.delete(&Key::new("c").unwrap()).await.unwrap();

        let next = store
            .list(ListRequest::new("", Some(cursor), NonZeroUsize::new(2).unwrap()))
            .await
            .unwrap();
        assert_eq!(names(&next), ["d"]);
        assert!(next.next_cursor().is_none());
    }

    #[tokio::test]
    async fn cursor_is_a_global_start_key_independent_of_prefix() {
        let store = Store::new([
            object("a/1", b"1"),
            object("a/2", b"2"),
            object("b/1", b"3"),
            object("b/2", b"4"),
        ])
        .unwrap();
        let first = store.list(list_request("a/", 1)).await.unwrap();
        let cursor = first.next_cursor().unwrap().clone();

        let next = store
            .list(ListRequest::new("b/", Some(cursor), NonZeroUsize::new(1).unwrap()))
            .await
            .unwrap();
        assert_eq!(names(&next), ["b/1"]);
        assert_eq!(next.next_cursor().unwrap().start_key().as_str(), "b/2");
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
