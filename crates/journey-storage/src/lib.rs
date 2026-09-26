//! In-memory HTTP/2 object storage and serving.
//!
//! The current implementation retains complete request bodies and stored
//! objects without upload or catalogue size limits. Restrict access and upload
//! sizes until explicit bounds are in place; untrusted uploads can exhaust
//! available memory.
mod storage_interface;
mod storage_in_memory;
mod http2_storage_service;

pub use storage_interface::{
    ContentType, ContentTypeError, GetResult, Key, ListCursor, ListPage, ListRequest,
    ObjectInterface, ObjectMetadata, PutCondition, PutContextInterface, ReadObject, ReadRange,
    ReadSpan, StoreError, StoreErrorKind, StoreInterface,
};
pub use storage_in_memory::{Object, PutContext, Store, StoreConfig};
pub use http2_storage_service::Service;
