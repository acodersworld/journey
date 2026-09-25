//! In-memory HTTP/2 object storage and serving.
//!
//! The current implementation retains complete request bodies and stored
//! objects without upload or catalogue size limits. Restrict access and upload
//! sizes until explicit bounds are in place; untrusted uploads can exhaust
//! available memory.
mod storage_interface;
mod storage_in_memory;
mod http2_storage_service;

pub use storage_interface::{ContentType, ContentTypeError, Key, ObjectInterface, PutContextInterface, StoreInterface};
pub use storage_in_memory::{Object, Store};
pub use http2_storage_service::Service;
