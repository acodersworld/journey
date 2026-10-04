use bytes::{Bytes, BytesMut};
use http::HeaderValue;
use std::{
    fmt,
    future::Future,
    hash::Hash,
    num::NonZeroUsize,
};

const MAX_KEY_LENGTH: usize = 1_024;
const SHA256_HEX_LENGTH: usize = 64;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ContentType(HeaderValue);

impl ContentType {
    pub fn try_from_header(value: &HeaderValue) -> Result<Self, ContentTypeError> {
        if value.is_empty() || value.as_bytes().len() > 128 || value.to_str().is_err() {
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
        if key.len() > MAX_KEY_LENGTH {
            return Err("Key exceeds 1024 UTF-8 bytes".to_string());
        }
        if key.ends_with('/') {
            return Err("Key cannot end with '/'".to_string());
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
    UnsupportedMediaType,
    Unavailable,
    Internal,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PutCondition {
    Unconditional,
    CreateOnly,
    ReplaceOnly,
}

/// The generated suffix returned by a content-addressed upload.
#[derive(Clone, Debug, Hash, PartialEq, Eq, PartialOrd, Ord)]
pub struct ObjectName(String);

impl ObjectName {
    /// Creates a generated suffix from a 64-character lowercase hexadecimal digest.
    pub fn new(name: &str) -> Result<Self, String> {
        if name.len() != SHA256_HEX_LENGTH
            || !name
                .bytes()
                .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
        {
            return Err("Object name must be a 64-character lowercase hexadecimal digest".to_string());
        }
        Ok(Self(name.to_string()))
    }

    pub(crate) fn from_sha256(digest: &[u8; 32]) -> Self {
        const HEX: &[u8; 16] = b"0123456789abcdef";
        let mut name = String::with_capacity(SHA256_HEX_LENGTH);
        for &byte in digest {
            name.push(HEX[(byte >> 4) as usize] as char);
            name.push(HEX[(byte & 0x0f) as usize] as char);
        }
        Self(name)
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for ObjectName {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.0)
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum PutKey {
    Supplied(Key),
    Sha256 { prefix: String },
}

pub(crate) fn validate_generated_prefix(prefix: &str) -> Result<(), String> {
    if !prefix.is_empty() && !prefix.ends_with('/') {
        return Err("Generated key prefix must end with '/'".to_string());
    }
    let key_length = prefix
        .len()
        .checked_add(SHA256_HEX_LENGTH)
        .ok_or_else(|| "Generated key exceeds 1024 UTF-8 bytes".to_string())?;
    if key_length > MAX_KEY_LENGTH {
        return Err("Generated key exceeds 1024 UTF-8 bytes".to_string());
    }
    Ok(())
}

pub(crate) fn key_from_sha256(prefix: &str, digest: &[u8; 32]) -> Result<Key, String> {
    validate_generated_prefix(prefix)?;
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut key = String::with_capacity(prefix.len() + SHA256_HEX_LENGTH);
    key.push_str(prefix);
    for &byte in digest {
        key.push(HEX[(byte >> 4) as usize] as char);
        key.push(HEX[(byte & 0x0f) as usize] as char);
    }
    Key::new(&key)
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

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ImageReductionSize {
    MaxEdge(u32),
    BoundingBox { width: u32, height: u32 },
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ImageFit {
    Contain,
    Pad,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ImageOutputFormat {
    Jpeg,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ImageReductionRequest {
    size: ImageReductionSize,
    fit: ImageFit,
    format: Option<ImageOutputFormat>,
    max_bytes: Option<u64>,
}

impl ImageReductionRequest {
    pub fn new(
        size: ImageReductionSize,
        fit: ImageFit,
        format: Option<ImageOutputFormat>,
        max_bytes: Option<u64>,
    ) -> Result<Self, String> {
        let valid_dimension = |value: u32| (1..=2_048).contains(&value);
        match size {
            ImageReductionSize::MaxEdge(edge) if !valid_dimension(edge) => {
                return Err("Image dimensions must be between 1 and 2048 pixels".to_owned());
            }
            ImageReductionSize::BoundingBox { width, height }
                if !valid_dimension(width) || !valid_dimension(height) =>
            {
                return Err("Image dimensions must be between 1 and 2048 pixels".to_owned());
            }
            _ => {}
        }
        if fit == ImageFit::Pad && !matches!(size, ImageReductionSize::BoundingBox { .. }) {
            return Err("Padded image output requires width and height".to_owned());
        }
        if max_bytes == Some(0) {
            return Err("Image byte limit must be positive".to_owned());
        }
        if max_bytes.is_some() && format != Some(ImageOutputFormat::Jpeg) {
            return Err("Image byte limit requires explicit JPEG output".to_owned());
        }
        Ok(Self { size, fit, format, max_bytes })
    }

    pub fn size(&self) -> ImageReductionSize {
        self.size
    }

    pub fn fit(&self) -> ImageFit {
        self.fit
    }

    pub fn format(&self) -> Option<ImageOutputFormat> {
        self.format
    }

    pub fn max_bytes(&self) -> Option<u64> {
        self.max_bytes
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ReducedImage {
    bytes: Bytes,
    content_type: &'static str,
}

impl ReducedImage {
    pub(crate) fn new(bytes: Bytes, content_type: &'static str) -> Self {
        Self { bytes, content_type }
    }

    pub fn bytes(&self) -> &Bytes {
        &self.bytes
    }

    pub fn content_type(&self) -> &'static str {
        self.content_type
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

    pub fn object_mut(&mut self) -> &mut O {
        &mut self.object
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
    start_key: Key,
}

impl ListCursor {
    pub(crate) fn new(start_key: Key) -> Self {
        Self { start_key }
    }

    pub(crate) fn start_key(&self) -> &Key {
        &self.start_key
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
    /// Reads at most the buffer's initialized length without changing that length.
    ///
    /// A zero-length buffer returns `Ok(0)` without moving the cursor. With a
    /// positive-length buffer, `Ok(0)` means the selected object is exhausted.
    /// Successful reads advance the reader by the returned count; short
    /// positive reads are valid, and errors must not advance the reader.
    fn read(
        &mut self,
        buffer: &mut BytesMut,
    ) -> impl Future<Output = Result<usize, StoreError>> + Send;
}

pub trait PutContextInterface {
    fn append(&mut self, bytes: &Bytes) -> impl Future<Output = Result<(), StoreError>> + Send;
}

pub trait StoreInterface: Send + Sync + Sized + 'static {
    type Object: ObjectInterface;
    type PutContextWithKey: PutContextInterface + Send + Sync + 'static;
    type PutContextWithGeneratedName: PutContextInterface + Send + Sync + 'static;

    fn get(
        &self,
        key: &Key,
        range: Option<ReadRange>,
    ) -> impl Future<Output = Result<GetResult<Self::Object>, StoreError>> + Send;

    fn stat(&self, key: &Key) -> impl Future<Output = Result<ObjectMetadata, StoreError>> + Send;

    /// Returns a generated JPEG thumbnail for a supported stored video.
    fn get_thumbnail(&self, key: &Key) -> impl Future<Output = Result<Bytes, StoreError>> + Send {
        async move {
            let _ = key;
            Err(StoreError::new(StoreErrorKind::Internal, "Video thumbnails are unavailable"))
        }
    }

    /// Produces a still-image representation from the payload stored at `key`.
    fn get_reduced_image(
        &self,
        key: &Key,
        request: ImageReductionRequest,
    ) -> impl Future<Output = Result<ReducedImage, StoreError>> + Send {
        async move {
            use crate::storage_image_reduction::{
                MAX_IMAGE_SOURCE_BYTES, acquire_image_reduction_slot, reduce_image_with_slot,
            };

            let read = self.get(key, None).await?;
            let mut object = match read {
                GetResult::Found(object) => object,
                GetResult::Unsatisfiable { .. } => {
                    return Err(StoreError::new(StoreErrorKind::Internal, "Complete image read was unsatisfiable"));
                }
            };
            let metadata = object.metadata().clone();
            let content_type = metadata
                .content_type()
                .as_header_value()
                .to_str()
                .map_err(|error| StoreError::new(StoreErrorKind::Corrupt, format!("Invalid stored content type: {error}")))?;
            let media_type = content_type.split(';').next().unwrap_or("").trim();
            if !media_type.get(..6).is_some_and(|prefix| prefix.eq_ignore_ascii_case("image/")) {
                return Err(StoreError::new(
                    StoreErrorKind::UnsupportedMediaType,
                    format!("Object {key} is not an image"),
                ));
            }
            if metadata.payload_length() > MAX_IMAGE_SOURCE_BYTES {
                return Err(StoreError::new(
                    StoreErrorKind::Capacity,
                    format!("Source image exceeds the {MAX_IMAGE_SOURCE_BYTES}-byte decoding limit"),
                ));
            }
            let payload_length = usize::try_from(metadata.payload_length()).map_err(|error| {
                StoreError::new(StoreErrorKind::Capacity, format!("Source image is too large: {error}"))
            })?;
            let permit = acquire_image_reduction_slot().await?;
            let mut bytes = Vec::with_capacity(payload_length);
            let mut buffer = BytesMut::from(vec![0_u8; 1024 * 1024].as_slice());
            loop {
                let count = object.object_mut().read(&mut buffer).await?;
                if count == 0 {
                    break;
                }
                if count > buffer.len()
                    || bytes.len().checked_add(count).is_none_or(|length| length > payload_length)
                {
                    return Err(StoreError::new(StoreErrorKind::Corrupt, "Image payload exceeded its stored length"));
                }
                bytes.extend_from_slice(&buffer[..count]);
            }
            if bytes.len() != payload_length {
                return Err(StoreError::new(StoreErrorKind::Corrupt, "Image payload ended before its stored length"));
            }
            reduce_image_with_slot(Bytes::from(bytes), media_type.to_owned(), request, permit).await
        }
    }

    fn list(&self, request: ListRequest) -> impl Future<Output = Result<ListPage, StoreError>> + Send;

    fn delete(&self, key: &Key) -> impl Future<Output = Result<(), StoreError>> + Send;

    fn put_context_with_key(
        &self,
        key: Key,
        content_type: ContentType,
        condition: PutCondition,
    ) -> impl Future<Output = Result<Self::PutContextWithKey, StoreError>> + Send;

    /// Atomically publishes an object at its caller-supplied full key.
    fn put_with_key(
        &self,
        put_context: Self::PutContextWithKey,
    ) -> impl Future<Output = Result<(), StoreError>> + Send;

    fn put_context_with_generated_name(
        &self,
        prefix: String,
        content_type: ContentType,
        condition: PutCondition,
    ) -> impl Future<Output = Result<Self::PutContextWithGeneratedName, StoreError>> + Send;

    /// Atomically publishes an object at `prefix + payload_sha256` and returns
    /// the generated suffix.
    fn put_with_generated_name(
        &self,
        put_context: Self::PutContextWithGeneratedName,
    ) -> impl Future<Output = Result<ObjectName, StoreError>> + Send;
}
