use std::{
    fmt,
    future::poll_fn,
    task::Poll,
};

use bytes::Bytes;
use http::{
    header,
    Method, Request, Response, StatusCode, Version,
};

use crate::storage_interface::{
    ContentType, Key, ObjectInterface, PutContextInterface, StoreErrorKind, StoreInterface,
};

const MAX_DATA_SEGMENT_SIZE: usize = 64 * 1024;
const NOT_FOUND_BODY: &[u8] = b"not found\n";
const INVALID_KEY_BODY: &[u8] = b"invalid key\n";
const BAD_REQUEST_BODY: &[u8] = b"bad request\n";
const INVALID_CONTENT_TYPE: &[u8] = b"invalid content type\n";
const STORAGE_ERROR_BODY: &[u8] = b"storage error\n";
const METHOD_NOT_ALLOWED_BODY: &[u8] = b"method not allowed\n";

/// Handles one already accepted HTTP/2 request against a shared catalogue.
#[derive(Clone, Debug)]
pub struct Service<S: StoreInterface> {
    store: S,
}

impl<S: StoreInterface> Service<S> {
    /// Creates a service backed by the supplied mutable catalogue.
    pub fn new(store: S) -> Self {
        Self { store }
    }

    /// Handles one accepted HTTP/2 request and sends its complete response.
    pub async fn handle(
        &self,
        request: Request<h2::RecvStream>,
        respond: h2::server::SendResponse<Bytes>,
    ) -> Result<(), ServiceError> {
        match *request.method() {
            Method::PUT => self.handle_put(request, respond).await,
            Method::GET => self.handle_get(request, respond).await,
            _ => send_text_response(
                respond,
                StatusCode::METHOD_NOT_ALLOWED,
                Some("GET, PUT"),
                METHOD_NOT_ALLOWED_BODY,
            )
        }
    }

    fn get_key(request: &Request<h2::RecvStream>) -> &str {
        let path = request.uri().path();
        path.strip_prefix("/objects/").unwrap_or("")
    }

    async fn handle_put(&self, request: Request<h2::RecvStream>, mut respond: h2::server::SendResponse<Bytes>) -> Result<(), ServiceError> {
        let key = match Key::new(Self::get_key(&request)) {
            Ok(key) => key,
            Err(_) => {
                return send_text_response(
                    respond,
                    StatusCode::BAD_REQUEST,
                    None,
                    BAD_REQUEST_BODY,
                );
            }
        };
        let mut content_types = request.headers().get_all(header::CONTENT_TYPE).iter();
        let Some(content_type) = content_types.next() else {
            return send_text_response(
                respond,
                StatusCode::BAD_REQUEST,
                None,
                BAD_REQUEST_BODY,
            );
        };
        if content_types.next().is_some() {
            return send_text_response(
                respond,
                StatusCode::BAD_REQUEST,
                None,
                BAD_REQUEST_BODY,
            );
        }
        let content_type = match ContentType::try_from_header(content_type) {
            Ok(content_type) => content_type,
            Err(_) => {
                return send_text_response(
                    respond,
                    StatusCode::BAD_REQUEST,
                    None,
                    INVALID_CONTENT_TYPE,
                );
            }
        };

        let mut body = request.into_body();

        let mut put_context = match self.store.put_context(content_type).await {
            Ok(ctx) => ctx,
            Err(error) => {
                eprintln!("storage PUT context creation failed: {error}");
                return send_text_response(
                    respond,
                    StatusCode::INTERNAL_SERVER_ERROR,
                    None,
                    STORAGE_ERROR_BODY,
                );
            }
        };

        while let Some(data) = body.data().await {
            let data = data?;
            let length = data.len();

            if let Err(error) = put_context.append(&data).await {
                eprintln!("storage PUT body append failed: {error}");
                return send_text_response(
                    respond,
                    StatusCode::INTERNAL_SERVER_ERROR,
                    None,
                    STORAGE_ERROR_BODY,
                );
            }

            body.flow_control().release_capacity(length)?;
        }

        if let Err(error) = self.store.put(&key, put_context).await {
            eprintln!("storage PUT commit failed for key {key:?}: {error}");
            return send_text_response(
                respond,
                StatusCode::INTERNAL_SERVER_ERROR,
                None,
                STORAGE_ERROR_BODY,
            );
        }

        let response = Response::builder()
            .version(Version::HTTP_2)
            .status(StatusCode::OK)
            .header(header::CONTENT_LENGTH, 0)
            .body(())?;
        respond.send_response(response, true)?;
        Ok(())
    }

    async fn handle_get(&self, request: Request<h2::RecvStream>, mut respond: h2::server::SendResponse<Bytes>) -> Result<(), ServiceError> {
        let key_text = Self::get_key(&request);
        let key = match Key::new(key_text) {
            Ok(key) => key,
            Err(_) => {
                return send_text_response(
                    respond,
                    StatusCode::BAD_REQUEST,
                    None,
                    INVALID_KEY_BODY,
                );
            }
        };
        let read_object = match self.store.get(&key).await {
            Ok(read_object) => read_object,
            Err(error) => {
                if error.kind() == StoreErrorKind::NotFound {
                    return send_text_response(respond, StatusCode::NOT_FOUND, None, NOT_FOUND_BODY);
                }
                eprintln!("storage GET lookup failed for key {key:?}: {error}");
                return send_text_response(
                    respond,
                    StatusCode::INTERNAL_SERVER_ERROR,
                    None,
                    STORAGE_ERROR_BODY,
                );
            }
        };

        let metadata = read_object.metadata();
        let contents = read_object.object().contents();
        let response = Response::builder()
            .version(Version::HTTP_2)
            .status(StatusCode::OK)
            .header(header::CONTENT_TYPE, metadata.content_type().as_header_value())
            .header(header::CONTENT_LENGTH, metadata.payload_length())
            .body(())?;
        if contents.is_empty() {
            respond.send_response(response, true)?;
            return Ok(());
        }

        let mut stream = respond.send_response(response, false)?;
        send_payload(&mut stream, contents).await
    }
}

/// Failures while building or sending an HTTP/2 response.
#[derive(Debug)]
pub enum ServiceError {
    /// The HTTP/2 stream or connection failed.
    Http2(h2::Error),
    /// An HTTP response could not be constructed.
    Http(http::Error),
    /// The peer closed the stream before it had send capacity.
    StreamClosed,
}

impl fmt::Display for ServiceError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Http2(error) => write!(formatter, "HTTP/2 error: {error}"),
            Self::Http(error) => write!(formatter, "HTTP error: {error}"),
            Self::StreamClosed => formatter.write_str("HTTP/2 response stream closed"),
        }
    }
}

impl std::error::Error for ServiceError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Http2(error) => Some(error),
            Self::Http(error) => Some(error),
            Self::StreamClosed => None,
        }
    }
}

impl From<h2::Error> for ServiceError {
    fn from(error: h2::Error) -> Self {
        Self::Http2(error)
    }
}

impl From<http::Error> for ServiceError {
    fn from(error: http::Error) -> Self {
        Self::Http(error)
    }
}

fn send_text_response(
    mut respond: h2::server::SendResponse<Bytes>,
    status: StatusCode,
    allow: Option<&'static str>,
    body: &'static [u8],
) -> Result<(), ServiceError> {
    let mut builder = Response::builder()
        .version(Version::HTTP_2)
        .status(status)
        .header(header::CONTENT_TYPE, "text/plain; charset=utf-8")
        .header(header::CONTENT_LENGTH, body.len());
    if let Some(allow) = allow {
        builder = builder.header(header::ALLOW, allow);
    }
    let response = builder.body(())?;
    let mut stream = respond.send_response(response, false)?;
    stream.send_data(Bytes::from_static(body), true)?;
    Ok(())
}

async fn send_payload(
    stream: &mut h2::SendStream<Bytes>,
    payload: &Bytes,
) -> Result<(), ServiceError> {
    let mut offset = 0;
    while offset < payload.len() {
        let requested = MAX_DATA_SEGMENT_SIZE.min(payload.len() - offset);
        stream.reserve_capacity(requested);
        let capacity = poll_fn(|context| match stream.poll_capacity(context) {
            Poll::Ready(Some(Ok(capacity))) if capacity > 0 => Poll::Ready(Ok(capacity)),
            Poll::Ready(Some(Ok(_))) | Poll::Pending => Poll::Pending,
            Poll::Ready(Some(Err(error))) => Poll::Ready(Err(ServiceError::Http2(error))),
            Poll::Ready(None) => Poll::Ready(Err(ServiceError::StreamClosed)),
        })
        .await?;
        let amount = requested.min(capacity);
        let end = offset + amount;
        stream.send_data(payload.slice(offset..end), end == payload.len())?;
        offset = end;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        ContentType, Key, ListPage, ListRequest, Object, ObjectMetadata, PutContextInterface,
        ReadObject, Store, StoreError,
    };
    use h2::{client, server};
    use std::{collections::HashMap, num::NonZeroUsize, sync::Arc};
    use tokio::{
        io::{duplex, DuplexStream},
        sync::RwLock,
        task::{JoinHandle, JoinSet},
    };

    const IMAGE: &[u8] = b"small jpeg fixture";
    const VIDEO: &[u8] = b"small mp4 fixture";

    fn validated_content_type(value: &str) -> ContentType {
        ContentType::try_from_header(&value.parse().unwrap()).unwrap()
    }

    fn object(key: &str, content_type: &str, contents: Bytes) -> (Key, Object) {
        (
            Key::new(key).unwrap(),
            Object::new(validated_content_type(content_type), contents),
        )
    }

    fn sample_store() -> Store {
        Store::new([
            object("image.jpg", "image/jpeg", Bytes::from_static(IMAGE)),
            object("video.mp4", "video/mp4", Bytes::from_static(VIDEO)),
        ])
        .unwrap()
    }

    async fn stored_object(store: &Store, key: &str) -> ReadObject<Object> {
        store.get(&Key::new(key).unwrap()).await.unwrap()
    }

    async fn object_count(store: &Store) -> usize {
        store
            .list(ListRequest::new("", None, NonZeroUsize::new(1_000).unwrap()))
            .await
            .unwrap()
            .objects()
            .len()
    }

    const SECRET_STORAGE_ERROR: &str = "secret internal storage detail /private/path";

    #[derive(Clone, Copy, Debug, PartialEq, Eq)]
    enum FailureOperation {
        PutContext,
        Append,
        Commit,
        Get,
    }

    #[derive(Clone, Debug)]
    struct FailureObject {
        content_type: ContentType,
        contents: Bytes,
    }

    impl ObjectInterface for FailureObject {
        fn contents(&self) -> &Bytes {
            &self.contents
        }
    }

    struct FailurePutContext {
        failure: FailureOperation,
        content_type: ContentType,
        contents: Vec<u8>,
    }

    impl PutContextInterface for FailurePutContext {
        async fn append(&mut self, bytes: &Bytes) -> Result<(), StoreError> {
            if self.failure == FailureOperation::Append {
                return Err(StoreError::new(
                    StoreErrorKind::Internal,
                    SECRET_STORAGE_ERROR,
                ));
            }

            self.contents.extend_from_slice(bytes);
            Ok(())
        }
    }

    #[derive(Clone)]
    struct FailureStore {
        failure: FailureOperation,
        objects: Arc<RwLock<HashMap<Key, FailureObject>>>,
    }

    impl FailureStore {
        fn with_existing_object(failure: FailureOperation) -> Self {
            let objects = HashMap::from([(
                Key::new("target").unwrap(),
                FailureObject {
                    content_type: validated_content_type("image/jpeg"),
                    contents: Bytes::from_static(b"original object"),
                },
            )]);
            Self {
                failure,
                objects: Arc::new(RwLock::new(objects)),
            }
        }
    }

    impl StoreInterface for FailureStore {
        type Object = FailureObject;
        type PutContext = FailurePutContext;

        async fn get(&self, key: &Key) -> Result<ReadObject<Self::Object>, StoreError> {
            if self.failure == FailureOperation::Get {
                return Err(StoreError::new(
                    StoreErrorKind::Internal,
                    SECRET_STORAGE_ERROR,
                ));
            }

            let object = self.objects.read().await.get(key).cloned().ok_or_else(|| {
                StoreError::new(StoreErrorKind::NotFound, "missing test object")
            })?;
            let metadata = ObjectMetadata::new(
                key.clone(),
                object.content_type.clone(),
                object.contents.len() as u64,
            );
            Ok(ReadObject::new(metadata, object))
        }

        async fn stat(&self, key: &Key) -> Result<ObjectMetadata, StoreError> {
            let objects = self.objects.read().await;
            let object = objects.get(key).ok_or_else(|| {
                StoreError::new(StoreErrorKind::NotFound, "missing test object")
            })?;
            Ok(ObjectMetadata::new(
                key.clone(),
                object.content_type.clone(),
                object.contents.len() as u64,
            ))
        }

        async fn list(&self, _request: ListRequest) -> Result<ListPage, StoreError> {
            Ok(ListPage::new(Vec::new(), None))
        }

        async fn delete(&self, key: &Key) -> Result<(), StoreError> {
            self.objects.write().await.remove(key);
            Ok(())
        }

        async fn put_context(
            &self,
            content_type: ContentType,
        ) -> Result<Self::PutContext, StoreError> {
            if self.failure == FailureOperation::PutContext {
                return Err(StoreError::new(
                    StoreErrorKind::Internal,
                    SECRET_STORAGE_ERROR,
                ));
            }

            Ok(FailurePutContext {
                failure: self.failure,
                content_type,
                contents: Vec::new(),
            })
        }

        async fn put(&self, key: &Key, put_context: Self::PutContext) -> Result<(), StoreError> {
            if self.failure == FailureOperation::Commit {
                return Err(StoreError::new(
                    StoreErrorKind::Internal,
                    SECRET_STORAGE_ERROR,
                ));
            }

            self.objects.write().await.insert(
                key.clone(),
                FailureObject {
                    content_type: put_context.content_type,
                    contents: put_context.contents.into(),
                },
            );
            Ok(())
        }
    }

    async fn assert_storage_error_response(response: http::Response<h2::RecvStream>) {
        assert_eq!(response.status(), StatusCode::INTERNAL_SERVER_ERROR);
        assert_eq!(
            response.headers()[header::CONTENT_LENGTH],
            STORAGE_ERROR_BODY.len().to_string()
        );
        for value in response.headers().values() {
            assert!(!value
                .as_bytes()
                .windows(SECRET_STORAGE_ERROR.len())
                .any(|window| window == SECRET_STORAGE_ERROR.as_bytes()));
        }
        let body = collect(response.into_body()).await.unwrap();
        assert_eq!(body, STORAGE_ERROR_BODY);
        assert!(!body
            .windows(SECRET_STORAGE_ERROR.len())
            .any(|window| window == SECRET_STORAGE_ERROR.as_bytes()));
    }

    #[tokio::test]
    async fn catalogue_constructs_and_looks_up_exact_keys() {
        let store = sample_store();

        assert_eq!(object_count(&store).await, 2);
        assert_eq!(stored_object(&store, "image.jpg").await.object().contents(), IMAGE);
        assert_eq!(stored_object(&store, "video.mp4").await.object().contents(), VIDEO);
        assert_eq!(
            store.get(&Key::new("IMAGE.jpg").unwrap()).await.unwrap_err().kind(),
            StoreErrorKind::NotFound
        );
        assert_eq!(
            store.get(&Key::new("missing").unwrap()).await.unwrap_err().kind(),
            StoreErrorKind::NotFound
        );
    }

    #[test]
    fn catalogue_rejects_empty_keys() {
        assert_eq!(Key::new("").unwrap_err(), "Empty key");
    }

    #[test]
    fn catalogue_rejects_duplicate_keys() {
        let objects = [
            object("same", "image/jpeg", Bytes::new()),
            object("same", "video/mp4", Bytes::new()),
        ];
        assert_eq!(
            Store::new(objects).unwrap_err().kind(),
            StoreErrorKind::InvalidRequest
        );
    }

    #[tokio::test]
    async fn catalogue_preserves_content_type_as_validated_metadata() {
        let store = Store::new([object(
            "custom",
            "vendor-specific-type",
            Bytes::from_static(b"contents"),
        )])
        .unwrap();

        assert_eq!(
            stored_object(&store, "custom").await.metadata().content_type().as_header_value().as_bytes(),
            b"vendor-specific-type"
        );
    }

    #[tokio::test]
    async fn lookup_clone_shares_payload_allocation() {
        let payload = Bytes::from(vec![7; 32]);
        let original_ptr = payload.as_ptr();
        let store = Store::new([object("payload", "application/octet-stream", payload)]).unwrap();
        let cloned = stored_object(&store, "payload").await;

        assert_eq!(cloned.object().contents().as_ptr(), original_ptr);
    }

    #[tokio::test]
    async fn store_clones_share_atomic_insertions_and_replacements() {
        let store = sample_store();
        let clone = store.clone();

        let key = Key::new("new").unwrap();
        let mut first = store.put_context(validated_content_type("text/plain")).await.unwrap();
        first.append(&Bytes::from_static(b"first")).await.unwrap();
        store.put(&key, first).await.unwrap();
        assert_eq!(
            stored_object(&clone, "new").await.object().contents().as_ref(),
            b"first"
        );
        let mut second = clone.put_context(validated_content_type("application/json")).await.unwrap();
        second.append(&Bytes::from_static(b"second")).await.unwrap();
        clone.put(&key, second).await.unwrap();

        let replaced = stored_object(&store, "new").await;
        assert_eq!(replaced.object().contents().as_ref(), b"second");
        assert_eq!(replaced.metadata().content_type().as_header_value().as_bytes(), b"application/json");
        assert_eq!(object_count(&store).await, 3);
    }

    struct TestConnection {
        sender: client::SendRequest<Bytes>,
        _client_task: JoinHandle<()>,
        _server_task: JoinHandle<()>,
    }

    async fn connection<S: StoreInterface + Clone>(
        store: S,
        client_window: Option<u32>,
    ) -> TestConnection {
        let (client_io, server_io) = duplex(256 * 1024);
        let server_task = tokio::spawn(run_test_server(server_io, Service::new(store)));
        let mut builder = client::Builder::new();
        if let Some(window) = client_window {
            builder
                .initial_window_size(window)
                .initial_connection_window_size(window);
        }
        let (sender, client_connection) = builder.handshake::<_, Bytes>(client_io).await.unwrap();
        let client_task = tokio::spawn(async move {
            let _ = client_connection.await;
        });
        TestConnection {
            sender,
            _client_task: client_task,
            _server_task: server_task,
        }
    }

    async fn run_test_server<S: StoreInterface + Clone>(io: DuplexStream, service: Service<S>) {
        let mut connection = server::handshake(io).await.unwrap();
        let mut handlers = JoinSet::new();
        loop {
            tokio::select! {
                accepted = connection.accept() => match accepted {
                    Some(Ok((request, respond))) => {
                        let service = service.clone();
                        handlers.spawn(async move {
                            let _ = service.handle(request, respond).await;
                        });
                    }
                    Some(Err(_)) | None => break,
                },
                result = handlers.join_next(), if !handlers.is_empty() => {
                    result.unwrap().unwrap();
                }
            }
        }
        while let Some(result) = handlers.join_next().await {
            result.unwrap();
        }
    }

    async fn get(
        sender: &mut client::SendRequest<Bytes>,
        path: &str,
    ) -> Result<http::Response<h2::RecvStream>, h2::Error> {
        let request = Request::builder()
            .method(Method::GET)
            .uri(path)
            .body(())
            .unwrap();
        let (response, _) = sender.send_request(request, true)?;
        response.await
    }

    async fn request(
        sender: &mut client::SendRequest<Bytes>,
        method: Method,
        path: &str,
    ) -> Result<http::Response<h2::RecvStream>, h2::Error> {
        let request = Request::builder().method(method).uri(path).body(()).unwrap();
        let (response, _) = sender.send_request(request, true)?;
        response.await
    }

    async fn send_body(
        stream: &mut h2::SendStream<Bytes>,
        payload: &[u8],
    ) -> Result<(), h2::Error> {
        if payload.is_empty() {
            return send_frame(stream, payload, true).await;
        }

        let mut offset = 0;
        while offset < payload.len() {
            let requested = MAX_DATA_SEGMENT_SIZE.min(payload.len() - offset);
            let end = offset + requested;
            send_frame(stream, &payload[offset..end], end == payload.len()).await?;
            offset = end;
        }
        Ok(())
    }

    async fn send_frame(
        stream: &mut h2::SendStream<Bytes>,
        payload: &[u8],
        end_stream: bool,
    ) -> Result<(), h2::Error> {
        if payload.is_empty() {
            return stream.send_data(Bytes::new(), end_stream);
        }

        let mut offset = 0;
        while offset < payload.len() {
            let requested = payload.len() - offset;
            stream.reserve_capacity(requested);
            let capacity = poll_fn(|context| match stream.poll_capacity(context) {
                Poll::Ready(Some(Ok(capacity))) if capacity > 0 => Poll::Ready(Ok(capacity)),
                Poll::Ready(Some(Ok(_))) | Poll::Pending => Poll::Pending,
                Poll::Ready(Some(Err(error))) => Poll::Ready(Err(error)),
                Poll::Ready(None) => panic!("request stream closed before body completed"),
            })
            .await?;
            let amount = requested.min(capacity as usize);
            let end = offset + amount;
            stream.send_data(
                Bytes::copy_from_slice(&payload[offset..end]),
                end_stream && end == payload.len(),
            )?;
            offset = end;
        }
        Ok(())
    }

    async fn put(
        sender: &mut client::SendRequest<Bytes>,
        path: &str,
        content_type: Option<&str>,
        content_length: Option<&str>,
        payload: &[u8],
    ) -> Result<http::Response<h2::RecvStream>, h2::Error> {
        let mut builder = Request::builder().method(Method::PUT).uri(path);
        if let Some(content_type) = content_type {
            builder = builder.header(header::CONTENT_TYPE, content_type);
        }
        if let Some(content_length) = content_length {
            builder = builder.header(header::CONTENT_LENGTH, content_length);
        }
        let request = builder.body(()).unwrap();
        let (response, mut stream) = sender.send_request(request, payload.is_empty())?;
        if !payload.is_empty() {
            send_body(&mut stream, payload).await?;
        }
        response.await
    }

    async fn collect(mut body: h2::RecvStream) -> Result<Vec<u8>, h2::Error> {
        let mut payload = Vec::new();
        while let Some(chunk) = body.data().await {
            let chunk = chunk?;
            body.flow_control().release_capacity(chunk.len())?;
            payload.extend_from_slice(&chunk);
        }
        Ok(payload)
    }

    fn assert_success_headers(
        response: &http::Response<h2::RecvStream>,
        content_type: &str,
        length: usize,
    ) {
        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(response.headers()[header::CONTENT_TYPE], content_type);
        assert_eq!(
            response.headers()[header::CONTENT_LENGTH],
            length.to_string()
        );
    }

    #[tokio::test]
    async fn known_objects_return_exact_headers_and_payloads() {
        let mut connection = connection(sample_store(), None).await;
        let image = get(&mut connection.sender, "/objects/image.jpg?download=1")
            .await
            .unwrap();
        assert_success_headers(&image, "image/jpeg", IMAGE.len());
        assert_eq!(collect(image.into_body()).await.unwrap(), IMAGE);

        let video = get(&mut connection.sender, "/objects/video.mp4")
            .await
            .unwrap();
        assert_success_headers(&video, "video/mp4", VIDEO.len());
        assert_eq!(collect(video.into_body()).await.unwrap(), VIDEO);
    }

    #[test]
    fn content_type_accepts_and_round_trips_header_value() {
        let header_value = http::HeaderValue::from_static("image/jpeg");
        let content_type = ContentType::try_from_header(&header_value).unwrap();

        assert_eq!(content_type.as_header_value(), &header_value);
    }

    #[test]
    fn content_type_rejects_empty_header_value() {
        assert!(ContentType::try_from_header(&http::HeaderValue::from_static("")).is_err());
    }

    #[test]
    fn content_type_rejects_non_visible_header_value() {
        let header_value = http::HeaderValue::from_bytes(b"text/plain\xff").unwrap();

        assert!(ContentType::try_from_header(&header_value).is_err());
    }

    #[tokio::test]
    async fn put_context_failure_returns_bounded_storage_error_without_replacement() {
        let store = FailureStore::with_existing_object(FailureOperation::PutContext);
        let before = store.objects.read().await.get("target").unwrap().clone();
        let mut connection = connection(store.clone(), None).await;

        let response = put(
            &mut connection.sender,
            "/objects/target",
            Some("image/png"),
            None,
            b"",
        )
        .await
        .unwrap();
        assert_storage_error_response(response).await;

        let after = store.objects.read().await.get("target").unwrap().clone();
        assert_eq!(after.content_type.as_header_value().as_bytes(), before.content_type.as_header_value().as_bytes());
        assert_eq!(after.contents, before.contents);
    }

    #[tokio::test]
    async fn append_failure_returns_bounded_storage_error_without_replacement() {
        let store = FailureStore::with_existing_object(FailureOperation::Append);
        let before = store.objects.read().await.get("target").unwrap().clone();
        let mut connection = connection(store.clone(), None).await;

        let response = put(
            &mut connection.sender,
            "/objects/target",
            Some("image/png"),
            None,
            b"candidate contents",
        )
        .await
        .unwrap();
        assert_storage_error_response(response).await;

        let after = store.objects.read().await.get("target").unwrap().clone();
        assert_eq!(after.content_type.as_header_value().as_bytes(), before.content_type.as_header_value().as_bytes());
        assert_eq!(after.contents, before.contents);
    }

    #[tokio::test]
    async fn commit_failure_returns_bounded_storage_error_without_replacement() {
        let store = FailureStore::with_existing_object(FailureOperation::Commit);
        let before = store.objects.read().await.get("target").unwrap().clone();
        let mut connection = connection(store.clone(), None).await;

        let response = put(
            &mut connection.sender,
            "/objects/target",
            Some("image/png"),
            None,
            b"candidate contents",
        )
        .await
        .unwrap();
        assert_storage_error_response(response).await;

        let after = store.objects.read().await.get("target").unwrap().clone();
        assert_eq!(after.content_type.as_header_value().as_bytes(), before.content_type.as_header_value().as_bytes());
        assert_eq!(after.contents, before.contents);
    }

    #[tokio::test]
    async fn get_lookup_failure_returns_bounded_storage_error() {
        let store = FailureStore::with_existing_object(FailureOperation::Get);
        let mut connection = connection(store, None).await;

        let response = get(&mut connection.sender, "/objects/target")
            .await
            .unwrap();
        assert_storage_error_response(response).await;
    }

    #[tokio::test]
    async fn unknown_keys_return_not_found_and_empty_keys_return_invalid_key() {
        let mut connection = connection(sample_store(), None).await;
        let missing = get(&mut connection.sender, "/objects/missing")
            .await
            .unwrap();
        assert_eq!(missing.status(), StatusCode::NOT_FOUND);
        assert_eq!(
            missing.headers()[header::CONTENT_LENGTH],
            NOT_FOUND_BODY.len().to_string()
        );
        assert_eq!(collect(missing.into_body()).await.unwrap(), NOT_FOUND_BODY);

        let invalid_key = get(&mut connection.sender, "/objects/").await.unwrap();
        assert_eq!(invalid_key.status(), StatusCode::BAD_REQUEST);
        assert_eq!(
            invalid_key.headers()[header::CONTENT_LENGTH],
            INVALID_KEY_BODY.len().to_string()
        );
        assert_eq!(
            collect(invalid_key.into_body()).await.unwrap(),
            INVALID_KEY_BODY
        );
    }

    #[tokio::test]
    async fn put_creates_an_object_and_get_returns_it_immediately() {
        let store = sample_store();
        let mut connection = connection(store.clone(), None).await;
        let uploaded = b"uploaded payload";
        let response = put(
            &mut connection.sender,
            "/objects/uploaded.bin",
            Some("application/octet-stream"),
            None,
            uploaded,
        )
        .await
        .unwrap();

        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(response.headers()[header::CONTENT_LENGTH], "0");
        assert!(collect(response.into_body()).await.unwrap().is_empty());

        let response = get(&mut connection.sender, "/objects/uploaded.bin")
            .await
            .unwrap();
        assert_success_headers(&response, "application/octet-stream", uploaded.len());
        assert_eq!(collect(response.into_body()).await.unwrap(), uploaded);
        assert_eq!(stored_object(&store, "uploaded.bin").await.object().contents().as_ref(), uploaded);
    }

    #[tokio::test]
    async fn large_put_releases_receive_capacity_until_end_stream() {
        let store = Store::default();
        let mut connection = connection(store.clone(), None).await;
        let payload = (0..180_000)
            .map(|value| (value % 251) as u8)
            .collect::<Vec<_>>();
        let response = put(
            &mut connection.sender,
            "/objects/large-upload",
            Some("application/octet-stream"),
            None,
            &payload,
        )
        .await
        .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        assert!(collect(response.into_body()).await.unwrap().is_empty());

        let response = get(&mut connection.sender, "/objects/large-upload")
            .await
            .unwrap();
        assert_success_headers(&response, "application/octet-stream", payload.len());
        assert_eq!(collect(response.into_body()).await.unwrap(), payload);
    }

    #[tokio::test]
    async fn put_replaces_payload_and_content_type_together() {
        let store = sample_store();
        let mut connection = connection(store.clone(), None).await;
        let response = put(
            &mut connection.sender,
            "/objects/image.jpg",
            Some("image/png"),
            Some("9"),
            b"new image",
        )
        .await
        .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(response.headers()[header::CONTENT_LENGTH], "0");
        assert!(collect(response.into_body()).await.unwrap().is_empty());

        let response = get(&mut connection.sender, "/objects/image.jpg")
            .await
            .unwrap();
        assert_success_headers(&response, "image/png", b"new image".len());
        assert_eq!(collect(response.into_body()).await.unwrap(), b"new image");
        let object = stored_object(&store, "image.jpg").await;
        assert_eq!(object.metadata().content_type().as_header_value().as_bytes(), b"image/png");
        assert_eq!(object.object().contents().as_ref(), b"new image");
    }

    #[tokio::test]
    async fn empty_put_body_publishes_an_empty_object() {
        let mut connection = connection(Store::default(), None).await;
        let response = put(
            &mut connection.sender,
            "/objects/empty",
            Some("application/octet-stream"),
            None,
            b"",
        )
        .await
        .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        assert!(collect(response.into_body()).await.unwrap().is_empty());

        let response = get(&mut connection.sender, "/objects/empty")
            .await
            .unwrap();
        assert_success_headers(&response, "application/octet-stream", 0);
        assert!(response.body().is_end_stream());
    }

    #[tokio::test]
    async fn invalid_put_metadata_and_empty_key_return_bad_request_without_mutation() {
        let store = sample_store();
        let mut connection = connection(store.clone(), None).await;

        let missing_type = put(
            &mut connection.sender,
            "/objects/missing-type",
            None,
            None,
            b"",
        )
        .await
        .unwrap();
        assert_eq!(missing_type.status(), StatusCode::BAD_REQUEST);
        assert_eq!(collect(missing_type.into_body()).await.unwrap(), BAD_REQUEST_BODY);

        let empty_type = put(
            &mut connection.sender,
            "/objects/empty-type",
            Some(""),
            None,
            b"",
        )
        .await
        .unwrap();
        assert_eq!(empty_type.status(), StatusCode::BAD_REQUEST);
        assert_eq!(collect(empty_type.into_body()).await.unwrap(), INVALID_CONTENT_TYPE);

        let mut invalid_type_request = Request::builder()
            .method(Method::PUT)
            .uri("/objects/invalid-type")
            .body(())
            .unwrap();
        invalid_type_request.headers_mut().insert(
            header::CONTENT_TYPE,
            http::HeaderValue::from_bytes(b"text/plain\xff").unwrap(),
        );
        let (invalid_type_response, _) = connection
            .sender
            .send_request(invalid_type_request, true)
            .unwrap();
        let invalid_type_response = invalid_type_response.await.unwrap();
        assert_eq!(invalid_type_response.status(), StatusCode::BAD_REQUEST);
        assert_eq!(
            collect(invalid_type_response.into_body()).await.unwrap(),
            INVALID_CONTENT_TYPE
        );

        let empty_key = put(
            &mut connection.sender,
            "/objects/",
            Some("text/plain"),
            None,
            b"",
        )
        .await
        .unwrap();
        assert_eq!(empty_key.status(), StatusCode::BAD_REQUEST);
        assert_eq!(collect(empty_key.into_body()).await.unwrap(), BAD_REQUEST_BODY);

        assert_eq!(object_count(&store).await, 2);
        assert_eq!(
            store.get(&Key::new("missing-type").unwrap()).await.unwrap_err().kind(),
            StoreErrorKind::NotFound
        );
    }

    #[tokio::test]
    async fn duplicate_content_type_returns_bad_request_without_mutation() {
        let store = sample_store();
        let mut connection = connection(store.clone(), None).await;
        let mut request = Request::builder()
            .method(Method::PUT)
            .uri("/objects/duplicate-type")
            .body(())
            .unwrap();
        request.headers_mut().append(
            header::CONTENT_TYPE,
            "text/plain".parse().unwrap(),
        );
        request.headers_mut().append(
            header::CONTENT_TYPE,
            "application/json".parse().unwrap(),
        );
        let (response, _) = connection.sender.send_request(request, true).unwrap();
        let response = response.await.unwrap();
        assert_eq!(response.status(), StatusCode::BAD_REQUEST);
        assert_eq!(collect(response.into_body()).await.unwrap(), BAD_REQUEST_BODY);
        assert_eq!(object_count(&store).await, 2);
        assert_eq!(
            store.get(&Key::new("duplicate-type").unwrap()).await.unwrap_err().kind(),
            StoreErrorKind::NotFound
        );
    }

    #[tokio::test]
    async fn content_length_mismatch_fails_the_stream_without_replacing_object() {
        let store = sample_store();
        let mut connection = connection(store.clone(), None).await;
        let request = Request::builder()
            .method(Method::PUT)
            .uri("/objects/image.jpg")
            .header(header::CONTENT_TYPE, "image/png")
            .header(header::CONTENT_LENGTH, "50")
            .body(())
            .unwrap();
        let (response, mut stream) = connection.sender.send_request(request, false).unwrap();
        let send_result = send_body(&mut stream, b"short body").await;
        drop(stream);
        let response_result = response.await;

        assert!(send_result.is_err() || response_result.is_err());
        if let Ok(response) = response_result {
            assert!(!response.status().is_success());
        }
        let original = stored_object(&store, "image.jpg").await;
        assert_eq!(original.metadata().content_type().as_header_value().as_bytes(), b"image/jpeg");
        assert_eq!(original.object().contents().as_ref(), IMAGE);
    }

    #[tokio::test]
    async fn reset_during_upload_leaves_existing_object_unchanged() {
        let store = sample_store();
        let mut connection = connection(store.clone(), None).await;
        let request = Request::builder()
            .method(Method::PUT)
            .uri("/objects/image.jpg")
            .header(header::CONTENT_TYPE, "image/png")
            .body(())
            .unwrap();
        let (response, mut stream) = connection.sender.send_request(request, false).unwrap();
        send_frame(&mut stream, b"partial upload", false).await.unwrap();
        stream.send_reset(h2::Reason::CANCEL);
        assert!(response.await.is_err());

        let original = stored_object(&store, "image.jpg").await;
        assert_eq!(original.metadata().content_type().as_header_value().as_bytes(), b"image/jpeg");
        assert_eq!(original.object().contents().as_ref(), IMAGE);
    }

    #[tokio::test]
    async fn get_completes_while_put_body_is_still_in_flight_on_same_connection() {
        let store = sample_store();
        let mut connection = connection(store.clone(), None).await;
        let request = Request::builder()
            .method(Method::PUT)
            .uri("/objects/image.jpg")
            .header(header::CONTENT_TYPE, "image/png")
            .body(())
            .unwrap();
        let (put_response, mut put_stream) = connection.sender.send_request(request, false).unwrap();
        send_frame(&mut put_stream, b"replacement ", false).await.unwrap();

        let get_response = get(&mut connection.sender, "/objects/image.jpg")
            .await
            .unwrap();
        assert_eq!(collect(get_response.into_body()).await.unwrap(), IMAGE);

        send_frame(&mut put_stream, b"bytes", true).await.unwrap();
        let put_response = put_response.await.unwrap();
        assert_eq!(put_response.status(), StatusCode::OK);
        assert!(collect(put_response.into_body()).await.unwrap().is_empty());
        let replacement = stored_object(&store, "image.jpg").await;
        assert_eq!(replacement.metadata().content_type().as_header_value().as_bytes(), b"image/png");
        assert_eq!(replacement.object().contents().as_ref(), b"replacement bytes");
    }

    #[tokio::test]
    async fn concurrent_puts_publish_one_complete_payload_with_matching_metadata() {
        let store = Store::default();
        let mut connection = connection(store.clone(), None).await;
        let first_request = Request::builder()
            .method(Method::PUT)
            .uri("/objects/shared")
            .header(header::CONTENT_TYPE, "text/plain")
            .body(())
            .unwrap();
        let second_request = Request::builder()
            .method(Method::PUT)
            .uri("/objects/shared")
            .header(header::CONTENT_TYPE, "application/json")
            .body(())
            .unwrap();
        let (first_response, mut first_stream) = connection.sender.send_request(first_request, false).unwrap();
        let (second_response, mut second_stream) = connection.sender.send_request(second_request, false).unwrap();

        send_frame(&mut first_stream, b"first-", false).await.unwrap();
        send_frame(&mut second_stream, b"second-", false).await.unwrap();
        send_frame(&mut second_stream, b"candidate", true).await.unwrap();
        send_frame(&mut first_stream, b"candidate", true).await.unwrap();

        let first_response = first_response.await.unwrap();
        let second_response = second_response.await.unwrap();
        assert_eq!(first_response.status(), StatusCode::OK);
        assert_eq!(second_response.status(), StatusCode::OK);
        assert!(collect(first_response.into_body()).await.unwrap().is_empty());
        assert!(collect(second_response.into_body()).await.unwrap().is_empty());

        let final_object = stored_object(&store, "shared").await;
        let is_first = final_object.object().contents().as_ref() == b"first-candidate"
            && final_object.metadata().content_type().as_header_value().as_bytes() == b"text/plain";
        let is_second = final_object.object().contents().as_ref() == b"second-candidate"
            && final_object.metadata().content_type().as_header_value().as_bytes() == b"application/json";
        assert!(is_first || is_second);
    }

    #[tokio::test]
    async fn unsupported_methods_return_method_not_allowed() {
        let mut connection = connection(sample_store(), None).await;
        let response = request(
            &mut connection.sender,
            Method::POST,
            "/objects/image.jpg",
        )
        .await
        .unwrap();

        assert_eq!(response.status(), StatusCode::METHOD_NOT_ALLOWED);
        assert_eq!(response.headers()[header::ALLOW], "GET, PUT");
        assert_eq!(
            collect(response.into_body()).await.unwrap(),
            METHOD_NOT_ALLOWED_BODY
        );
    }

    #[tokio::test]
    async fn empty_object_completes_on_response_headers() {
        let store = Store::new([object(
            "empty",
            "application/octet-stream",
            Bytes::new(),
        )])
        .unwrap();
        let mut connection = connection(store, None).await;
        let response = get(&mut connection.sender, "/objects/empty")
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(response.headers()[header::CONTENT_LENGTH], "0");
        assert!(response.body().is_end_stream());
        assert!(collect(response.into_body()).await.unwrap().is_empty());
    }

    #[tokio::test]
    async fn large_payload_is_sent_in_bounded_data_chunks() {
        let payload = Bytes::from((0..180_000).map(|value| (value % 251) as u8).collect::<Vec<_>>());
        let expected = payload.to_vec();
        let store = Store::new([object("large", "application/octet-stream", payload)]).unwrap();
        let mut connection = connection(store, None).await;
        let response = get(&mut connection.sender, "/objects/large")
            .await
            .unwrap();
        assert_success_headers(&response, "application/octet-stream", expected.len());

        let mut body = response.into_body();
        let mut received = Vec::new();
        let mut chunks = 0;
        while let Some(chunk) = body.data().await {
            let chunk = chunk.unwrap();
            assert!(chunk.len() <= MAX_DATA_SEGMENT_SIZE);
            chunks += 1;
            body.flow_control().release_capacity(chunk.len()).unwrap();
            received.extend_from_slice(&chunk);
        }
        assert!(chunks > 1);
        assert_eq!(received, expected);
    }

    #[tokio::test]
    async fn slow_receiver_completes_with_a_small_http2_window() {
        const WINDOW: u32 = 1024;
        let payload = Bytes::from(vec![0x5a; 96 * 1024]);
        let expected = payload.to_vec();
        let store = Store::new([object("slow", "application/octet-stream", payload)]).unwrap();
        let mut connection = connection(store, Some(WINDOW)).await;
        let response = get(&mut connection.sender, "/objects/slow").await.unwrap();

        let mut body = response.into_body();
        let first = body.data().await.unwrap().unwrap();
        assert!(first.len() <= WINDOW as usize);
        body.flow_control().release_capacity(first.len()).unwrap();
        let mut received = first.to_vec();
        while let Some(chunk) = body.data().await {
            let chunk = chunk.unwrap();
            body.flow_control().release_capacity(chunk.len()).unwrap();
            received.extend_from_slice(&chunk);
        }
        assert_eq!(received, expected);
    }

    #[tokio::test]
    async fn concurrent_get_streams_complete_on_one_connection() {
        let mut connection = connection(sample_store(), None).await;
        let (image_future, _) = connection
            .sender
            .send_request(
                Request::builder()
                    .method(Method::GET)
                    .uri("/objects/image.jpg")
                    .body(())
                    .unwrap(),
                true,
            )
            .unwrap();
        let (video_future, _) = connection
            .sender
            .send_request(
                Request::builder()
                    .method(Method::GET)
                    .uri("/objects/video.mp4")
                    .body(())
                    .unwrap(),
                true,
            )
            .unwrap();

        let image = image_future.await.unwrap();
        let video = video_future.await.unwrap();
        assert_eq!(collect(image.into_body()).await.unwrap(), IMAGE);
        assert_eq!(collect(video.into_body()).await.unwrap(), VIDEO);
    }
}
