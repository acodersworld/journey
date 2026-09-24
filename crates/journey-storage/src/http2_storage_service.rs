pub use crate::storage::Store;

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

const MAX_DATA_SEGMENT_SIZE: usize = 64 * 1024;
const NOT_FOUND_BODY: &[u8] = b"not found\n";
const METHOD_NOT_ALLOWED_BODY: &[u8] = b"method not allowed\n";

/// Handles one already accepted HTTP/2 GET request against a shared catalogue.
#[derive(Clone, Debug)]
pub struct Service {
    store: Store,
}

impl Service {
    /// Creates a service backed by the supplied immutable catalogue.
    pub fn new(store: Store) -> Self {
        Self { store }
    }

    /// Handles one accepted HTTP/2 request and sends its complete response.
    pub async fn handle(
        &self,
        request: Request<h2::RecvStream>,
        mut respond: h2::server::SendResponse<Bytes>,
    ) -> Result<(), ServiceError> {
        let method = request.method().clone();
        let path = request.uri().path();

        if method != Method::GET {
            return send_text_response(
                respond,
                StatusCode::METHOD_NOT_ALLOWED,
                Some("GET"),
                METHOD_NOT_ALLOWED_BODY,
            );
        }

        let key = path.strip_prefix("/objects/").unwrap_or("");
        let Some(object) = self.store.get(key) else {
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
    use crate::storage::{Key, Object, Store};
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
            content_type.to_owned(),
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

    #[test]
    fn catalogue_constructs_and_looks_up_exact_keys() {
        let store = sample_store();

        assert_eq!(store.len(), 2);
        assert!(!store.is_empty());
        assert_eq!(store.get("image.jpg").unwrap().contents(), IMAGE);
        assert_eq!(store.get("video.mp4").unwrap().contents(), VIDEO);
        assert!(store.get("IMAGE.jpg").is_none());
        assert!(store.get("missing").is_none());
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

    #[test]
    fn catalogue_preserves_content_type_as_opaque_metadata() {
        let store = Store::new([object(
            "custom",
            "vendor-specific-type",
            Bytes::from_static(b"contents"),
        )])
        .unwrap();

        assert_eq!(store.get("custom").unwrap().content_type(), "vendor-specific-type");
    }

    #[test]
    fn lookup_clone_shares_payload_allocation() {
        let payload = Bytes::from(vec![7; 32]);
        let original_ptr = payload.as_ptr();
        let store = Store::new([object("payload", "application/octet-stream", payload)]).unwrap();
        let cloned = store.get("payload").unwrap();

        assert_eq!(cloned.contents().as_ptr(), original_ptr);
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

    async fn run_test_server(io: DuplexStream, service: Service) {
        let mut connection = server::handshake(io).await.unwrap();
        let mut handlers = JoinSet::new();
        loop {
            tokio::select! {
                accepted = connection.accept() => match accepted {
                    Some(Ok((request, respond))) => {
                        let service = service.clone();
                        handlers.spawn(async move {
                            service.handle(request, respond).await.unwrap();
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
        assert_eq!(response.headers()[header::ALLOW], "GET");
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

