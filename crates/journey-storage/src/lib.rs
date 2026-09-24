//! In-memory HTTP/2 object storage and serving.
//!
//! The current implementation retains complete request bodies and stored
//! objects without upload or catalogue size limits. Restrict access and upload
//! sizes until explicit bounds are in place; untrusted uploads can exhaust
//! available memory.
mod storage;
mod http2_storage_service;

pub use storage::{Key, Object, PutOutcome, Store};
pub use http2_storage_service::Service;
