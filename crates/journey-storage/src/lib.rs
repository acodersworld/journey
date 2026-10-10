//! In-memory and filesystem-backed object storage with HTTP/2 and browser interfaces.
//!
//! The in-memory store retains complete request bodies and stored objects.
//! The filesystem store streams uploads and range reads through immutable
//! object files and rebuilds its metadata index at startup.
mod storage_interface;
mod storage_in_memory;
mod storage_filesystem;
mod storage_thumbnail;
mod storage_image_reduction;
mod http2_storage_service;
mod storage_web_interface;
mod range_get_logging;

pub use storage_interface::{
    ContentType, ContentTypeError, GetResult, ImageFit, ImageOutputFormat, ImageReductionRequest,
    ImageReductionSize, Key, ListCursor, ListPage, ListRequest, ObjectInterface, ObjectMetadata,
    ObjectName, PutCondition, PutContextInterface, ReadObject, ReadRange, ReadSpan, ReducedImage,
    StoreError, StoreErrorKind, StoreInterface,
};
pub use storage_in_memory::{
    Object, ObjectReader, PutContextWithGeneratedName, PutContextWithKey, Store, StoreConfig,
};
pub use storage_filesystem::{
    FilesystemPutContextWithGeneratedName, FilesystemPutContextWithKey, FilesystemStore,
    FilesystemStoreConfig,
};
pub use http2_storage_service::{Service, ServiceError};
pub use storage_web_interface::{serve_web_interface, WebCredentials, WebCredentialsError};
pub use range_get_logging::{
    start_range_get_aggregator, RangeGetAggregator, RangeGetOutcome, RangeGetTask,
};
