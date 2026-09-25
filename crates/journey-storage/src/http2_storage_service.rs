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

use crate::storage_interface::{StoreInterface, ObjectInterface, Key, PutOutcome};

const MAX_DATA_SEGMENT_SIZE: usize = 64 * 1024;
const NOT_FOUND_BODY: &[u8] = b"not found\n";
const BAD_REQUEST_BODY: &[u8] = b"bad request\n";
const INVALID_CONTENT_TYPE: &[u8] = b"invalid content type\n";
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
        if content_type.as_bytes().is_empty() || content_types.next().is_some() {
            return send_text_response(
                respond,
                StatusCode::BAD_REQUEST,
                None,
                BAD_REQUEST_BODY,
            );
        }
        let Ok(content_type) = content_type.to_str() else {
            return send_text_response(
                respond,
                StatusCode::BAD_REQUEST,
                None,
                INVALID_CONTENT_TYPE,
            );
        };
        let content_type = content_type.to_string();

        let mut body = request.into_body();
        let mut contents = Vec::new();
        while let Some(data) = body.data().await {
            let data = data?;
            let length = data.len();
            contents.extend_from_slice(&data);
            body.flow_control().release_capacity(length)?;
        }

        let object = S::Object::new(key, content_type, Bytes::from(contents));
        let outcome = self.store.put(object).await;
        let status = match outcome {
            PutOutcome::Created => StatusCode::CREATED,
            PutOutcome::Replaced => StatusCode::OK,
        };
        let response = Response::builder()
            .version(Version::HTTP_2)
            .status(status)
            .header(header::CONTENT_LENGTH, 0)
            .body(())?;
        respond.send_response(response, true)?;
        Ok(())
    }

    async fn handle_get(&self, request: Request<h2::RecvStream>, mut respond: h2::server::SendResponse<Bytes>) -> Result<(), ServiceError> {
        let key = Self::get_key(&request);
        let Some(object) = self.store.get(key).await else {
            return send_text_response(respond, StatusCode::NOT_FOUND, None, NOT_FOUND_BODY);
        };

        let response = Response::builder()
            .version(Version::HTTP_2)
            .status(StatusCode::OK)
            .header(header::CONTENT_TYPE, object.content_type())
            .header(header::CONTENT_LENGTH, object.contents().len())
            .body(())?;
        if object.contents().is_empty() {
            respond.send_response(response, true)?;
            return Ok(());
        }

        let mut stream = respond.send_response(response, false)?;
        send_payload(&mut stream, object.contents()).await
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
    use crate::{Key, Object, Store};
    use h2::{client, server};
    use tokio::{
        io::{duplex, DuplexStream},
        task::{JoinHandle, JoinSet},
    };

    const IMAGE: &[u8] = b"small jpeg fixture";
    const VIDEO: &[u8] = b"small mp4 fixture";

    fn object(key: &str, content_type: &str, contents: Bytes) -> Object {
        Object::new(
            Key::new(key).unwrap(),
            content_type.to_string(),
            contents,
        )
    }

    fn sample_store() -> Store {
        Store::new([
            object("image.jpg", "image/jpeg", Bytes::from_static(IMAGE)),
            object("video.mp4", "video/mp4", Bytes::from_static(VIDEO)),
        ])
        .unwrap()
    }

    #[tokio::test]
    async fn catalogue_constructs_and_looks_up_exact_keys() {
        let store = sample_store();

        assert_eq!(store.len().await, 2);
        assert!(!store.is_empty().await);
        assert_eq!(store.get("image.jpg").await.unwrap().contents(), IMAGE);
        assert_eq!(store.get("video.mp4").await.unwrap().contents(), VIDEO);
        assert!(store.get("IMAGE.jpg").await.is_none());
        assert!(store.get("missing").await.is_none());
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
        assert_eq!(Store::new(objects).unwrap_err(), "Duplicate key: same");
    }

    #[tokio::test]
    async fn catalogue_preserves_content_type_as_opaque_metadata() {
        let store = Store::new([object(
            "custom",
            "vendor-specific-type",
            Bytes::from_static(b"contents"),
        )])
        .unwrap();

        assert_eq!(
            store.get("custom").await.unwrap().content_type().as_bytes(),
            b"vendor-specific-type"
        );
    }

    #[tokio::test]
    async fn lookup_clone_shares_payload_allocation() {
        let payload = Bytes::from(vec![7; 32]);
        let original_ptr = payload.as_ptr();
        let store = Store::new([object("payload", "application/octet-stream", payload)]).unwrap();
        let cloned = store.get("payload").await.unwrap();

        assert_eq!(cloned.contents().as_ptr(), original_ptr);
    }

    #[tokio::test]
    async fn store_clones_share_atomic_insertions_and_replacements() {
        let store = sample_store();
        let clone = store.clone();

        assert_eq!(
            store.put(object("new", "text/plain", Bytes::from_static(b"first"))).await,
            PutOutcome::Created
        );
        assert_eq!(
            clone.get("new").await.unwrap().contents().as_ref(),
            b"first"
        );
        assert_eq!(
            clone.put(object("new", "application/json", Bytes::from_static(b"second"))).await,
            PutOutcome::Replaced
        );

        let replaced = store.get("new").await.unwrap();
        assert_eq!(replaced.contents().as_ref(), b"second");
        assert_eq!(replaced.content_type().as_bytes(), b"application/json");
        assert_eq!(store.len().await, 3);
    }

    struct TestConnection {
        sender: client::SendRequest<Bytes>,
        _client_task: JoinHandle<()>,
        _server_task: JoinHandle<()>,
    }

    async fn connection(
        store: Store,
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

    async fn run_test_server(io: DuplexStream, service: Service<Store>) {
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

    #[tokio::test]
    async fn unknown_and_empty_keys_return_not_found() {
        let mut connection = connection(sample_store(), None).await;
        for path in ["/objects/missing", "/objects/"] {
            let response = get(&mut connection.sender, path).await.unwrap();
            assert_eq!(response.status(), StatusCode::NOT_FOUND);
            assert_eq!(
                response.headers()[header::CONTENT_LENGTH],
                NOT_FOUND_BODY.len().to_string()
            );
            assert_eq!(collect(response.into_body()).await.unwrap(), NOT_FOUND_BODY);
        }
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

        assert_eq!(response.status(), StatusCode::CREATED);
        assert_eq!(response.headers()[header::CONTENT_LENGTH], "0");
        assert!(collect(response.into_body()).await.unwrap().is_empty());

        let response = get(&mut connection.sender, "/objects/uploaded.bin")
            .await
            .unwrap();
        assert_success_headers(&response, "application/octet-stream", uploaded.len());
        assert_eq!(collect(response.into_body()).await.unwrap(), uploaded);
        assert_eq!(store.get("uploaded.bin").await.unwrap().contents().as_ref(), uploaded);
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
        assert_eq!(response.status(), StatusCode::CREATED);
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
        let object = store.get("image.jpg").await.unwrap();
        assert_eq!(object.content_type().as_bytes(), b"image/png");
        assert_eq!(object.contents().as_ref(), b"new image");
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
        assert_eq!(response.status(), StatusCode::CREATED);
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
        assert_eq!(collect(empty_type.into_body()).await.unwrap(), BAD_REQUEST_BODY);

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

        assert_eq!(store.len().await, 2);
        assert!(store.get("missing-type").await.is_none());
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
        assert_eq!(store.len().await, 2);
        assert!(store.get("duplicate-type").await.is_none());
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
        let original = store.get("image.jpg").await.unwrap();
        assert_eq!(original.content_type().as_bytes(), b"image/jpeg");
        assert_eq!(original.contents().as_ref(), IMAGE);
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

        let original = store.get("image.jpg").await.unwrap();
        assert_eq!(original.content_type().as_bytes(), b"image/jpeg");
        assert_eq!(original.contents().as_ref(), IMAGE);
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
        let replacement = store.get("image.jpg").await.unwrap();
        assert_eq!(replacement.content_type().as_bytes(), b"image/png");
        assert_eq!(replacement.contents().as_ref(), b"replacement bytes");
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
        let created = [first_response.status(), second_response.status()]
            .into_iter()
            .filter(|status| *status == StatusCode::CREATED)
            .count();
        assert_eq!(created, 1);
        assert!(
            (first_response.status() == StatusCode::CREATED
                && second_response.status() == StatusCode::OK)
                || (first_response.status() == StatusCode::OK
                    && second_response.status() == StatusCode::CREATED)
        );
        assert!(collect(first_response.into_body()).await.unwrap().is_empty());
        assert!(collect(second_response.into_body()).await.unwrap().is_empty());

        let final_object = store.get("shared").await.unwrap();
        let is_first = final_object.contents().as_ref() == b"first-candidate"
            && final_object.content_type().as_bytes() == b"text/plain";
        let is_second = final_object.contents().as_ref() == b"second-candidate"
            && final_object.content_type().as_bytes() == b"application/json";
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
