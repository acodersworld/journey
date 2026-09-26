//! In-memory and filesystem-backed HTTP/2 object storage and serving.
//!
//! The in-memory store retains complete request bodies and stored objects.
//! The filesystem store streams uploads and range reads through immutable
//! object files and rebuilds its metadata index at startup.
mod storage_interface;
mod storage_in_memory;
mod storage_filesystem;
mod http2_storage_service;

pub use storage_interface::{
    ContentType, ContentTypeError, GetResult, Key, ListCursor, ListPage, ListRequest,
    ObjectInterface, ObjectMetadata, PutCondition, PutContextInterface, ReadObject, ReadRange,
    ReadSpan, StoreError, StoreErrorKind, StoreInterface,
};
pub use storage_in_memory::{Object, ObjectReader, PutContext, Store, StoreConfig};
pub use storage_filesystem::{FilesystemStore, FilesystemStoreConfig};
pub use http2_storage_service::Service;
