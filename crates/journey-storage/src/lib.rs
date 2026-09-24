//! Read-only, in-memory HTTP/2 object serving.
mod storage;
mod http2_storage_service;

pub use storage::{Key, Object, Store};
pub use http2_storage_service::Service;
