use bytes::Bytes;
use http::HeaderValue;
use std::{
    fmt,
    future::Future,
    hash::Hash,
    num::NonZeroUsize,
};

#[derive(Clone, Debug, PartialEq, Eq)]
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

#[derive(Clone, Debug, Hash, PartialEq, Eq, PartialOrd, Ord)]
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

    pub fn as_str(&self) -> &str {
        &self.key
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

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum StoreErrorKind {
    InvalidRequest,
    NotFound,
    Conflict,
    PreconditionFailed,
    Capacity,
    Corrupt,
    Unavailable,
    Internal,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PutCondition {
    Unconditional,
    CreateOnly,
    ReplaceOnly,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct StoreError {
    kind: StoreErrorKind,
    detail: String,
}

impl StoreError {
    /// Creates a storage error with a stable public category and local detail.
    pub fn new(kind: StoreErrorKind, detail: impl Into<String>) -> Self {
        Self {
            kind,
            detail: detail.into(),
        }
    }

    pub fn kind(&self) -> StoreErrorKind {
        self.kind
    }
}

impl fmt::Display for StoreError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "{:?}: {}", self.kind, self.detail)
    }
}

impl std::error::Error for StoreError {}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ObjectMetadata {
    key: Key,
    content_type: ContentType,
    payload_length: u64,
}

impl ObjectMetadata {
    /// Creates metadata from validated key and content-type values.
    pub fn new(key: Key, content_type: ContentType, payload_length: u64) -> Self {
        Self {
            key,
            content_type,
            payload_length,
        }
    }

    pub fn key(&self) -> &Key {
        &self.key
    }

    pub fn content_type(&self) -> &ContentType {
        &self.content_type
    }

    pub fn payload_length(&self) -> u64 {
        self.payload_length
    }
}

#[derive(Debug)]
pub struct ReadObject<O> {
    metadata: ObjectMetadata,
    object: O,
    selected_span: Option<ReadSpan>,
}

impl<O> ReadObject<O> {
    pub fn new(metadata: ObjectMetadata, object: O) -> Self {
        Self {
            metadata,
            object,
            selected_span: None,
        }
    }

    pub fn with_selected_span(metadata: ObjectMetadata, object: O, selected_span: ReadSpan) -> Self {
        Self {
            metadata,
            object,
            selected_span: Some(selected_span),
        }
    }

    pub fn metadata(&self) -> &ObjectMetadata {
        &self.metadata
    }

    pub fn object(&self) -> &O {
        &self.object
    }

    pub fn selected_span(&self) -> Option<ReadSpan> {
        self.selected_span
    }

    pub fn into_parts(self) -> (ObjectMetadata, O) {
        (self.metadata, self.object)
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ReadRange {
    Closed { start: u64, end: u64 },
    From { start: u64 },
    Suffix { length: u64 },
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ReadSpan {
    offset: u64,
    size: u64,
}

impl ReadSpan {
    pub fn new(offset: u64, size: u64) -> Self {
        Self { offset, size }
    }

    pub fn offset(&self) -> u64 {
        self.offset
    }

    pub fn size(&self) -> u64 {
        self.size
    }
}

#[derive(Debug)]
pub enum GetResult<O> {
    Found(ReadObject<O>),
    Unsatisfiable { complete_length: u64 },
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ListCursor {
    prefix: String,
    last_key: Key,
}

impl ListCursor {
    pub(crate) fn new(prefix: String, last_key: Key) -> Self {
        Self { prefix, last_key }
    }

    pub(crate) fn prefix(&self) -> &str {
        &self.prefix
    }

    pub(crate) fn last_key(&self) -> &Key {
        &self.last_key
    }
}

#[derive(Clone, Debug)]
pub struct ListRequest {
    prefix: String,
    cursor: Option<ListCursor>,
    requested_limit: NonZeroUsize,
}

impl ListRequest {
    pub fn new(
        prefix: impl Into<String>,
        cursor: Option<ListCursor>,
        requested_limit: NonZeroUsize,
    ) -> Self {
        Self {
            prefix: prefix.into(),
            cursor,
            requested_limit,
        }
    }

    pub fn prefix(&self) -> &str {
        &self.prefix
    }

    pub fn cursor(&self) -> Option<&ListCursor> {
        self.cursor.as_ref()
    }

    pub fn requested_limit(&self) -> NonZeroUsize {
        self.requested_limit
    }
}

#[derive(Clone, Debug)]
pub struct ListPage {
    objects: Vec<ObjectMetadata>,
    next_cursor: Option<ListCursor>,
}

impl ListPage {
    pub fn new(objects: Vec<ObjectMetadata>, next_cursor: Option<ListCursor>) -> Self {
        Self {
            objects,
            next_cursor,
        }
    }

    pub fn objects(&self) -> &[ObjectMetadata] {
        &self.objects
    }

    pub fn next_cursor(&self) -> Option<&ListCursor> {
        self.next_cursor.as_ref()
    }

    pub fn into_parts(self) -> (Vec<ObjectMetadata>, Option<ListCursor>) {
        (self.objects, self.next_cursor)
    }
}

pub trait ObjectInterface: Send + Sync + 'static {
    /// Returns the immutable payload bytes.
    fn contents(&self) -> &Bytes;
}

pub trait PutContextInterface {
    fn append(&mut self, bytes: &Bytes) -> impl Future<Output = Result<(), StoreError>> + Send;
}

pub trait StoreInterface: Send + Sync + Sized + 'static {
    type Object: ObjectInterface;
    type PutContext: PutContextInterface + Send + Sync + 'static;

    fn get(
        &self,
        key: &Key,
        range: Option<ReadRange>,
    ) -> impl Future<Output = Result<GetResult<Self::Object>, StoreError>> + Send;

    fn stat(&self, key: &Key) -> impl Future<Output = Result<ObjectMetadata, StoreError>> + Send;

    fn list(&self, request: ListRequest) -> impl Future<Output = Result<ListPage, StoreError>> + Send;

    fn delete(&self, key: &Key) -> impl Future<Output = Result<(), StoreError>> + Send;

    fn put_context(
        &self,
        key: Key,
        content_type: ContentType,
        condition: PutCondition,
    ) -> impl Future<Output = Result<Self::PutContext, StoreError>> + Send;

    /// Atomically creates or replaces an object by the key owned by its context.
    fn put(
        &self,
        put_context: Self::PutContext,
    ) -> impl Future<Output = Result<(), StoreError>> + Send;
}
