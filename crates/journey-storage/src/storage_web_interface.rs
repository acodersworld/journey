use std::{
    fmt,
    num::NonZeroUsize,
    str::FromStr,
    sync::Arc,
};

use axum::{
    body::Body,
    extract::{RawQuery, State},
    http::{
        header, uri::Authority, HeaderMap, HeaderValue, Request, StatusCode, Uri,
    },
    middleware::{self, Next},
    response::{Html, IntoResponse, Response},
    routing::{get, put},
    Json, Router,
};
use base64::{engine::general_purpose::{STANDARD, URL_SAFE_NO_PAD}, Engine};
use bytes::{Bytes, BytesMut};
use futures_util::{stream, Stream, StreamExt};
use serde::{de::DeserializeOwned, Deserialize, Serialize};
use sha2::{Digest, Sha256};
use subtle::ConstantTimeEq;
use tokio::net::TcpListener;

use crate::storage_interface::{
    ContentType, GetResult, Key, ListCursor, ListRequest, ObjectInterface, ObjectMetadata,
    PutCondition, PutContextInterface, StoreError, StoreErrorKind, StoreInterface,
};

const INDEX_HTML: &str = include_str!("storage_web_interface.html");
const MAX_LIST_ROWS: usize = 100;
const STREAM_CHUNK_SIZE: usize = 64 * 1024;
const BAD_REQUEST_BODY: &str = "bad request\n";
const FORBIDDEN_BODY: &str = "forbidden\n";
const NOT_FOUND_BODY: &str = "not found\n";
const PRECONDITION_FAILED_BODY: &str = "precondition failed\n";
const STORAGE_ERROR_BODY: &str = "storage error\n";

/// Basic-auth credentials for the storage web interface.
pub struct WebCredentials {
    username: String,
    password: String,
}

impl WebCredentials {
    /// Creates credentials, rejecting an empty username or password.
    pub fn new(
        username: impl Into<String>,
        password: impl Into<String>,
    ) -> Result<Self, WebCredentialsError> {
        let username = username.into();
        let password = password.into();
        if username.is_empty() || password.is_empty() || username.contains(':') {
            return Err(WebCredentialsError);
        }
        Ok(Self { username, password })
    }
}

impl fmt::Debug for WebCredentials {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("WebCredentials([redacted])")
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct WebCredentialsError;

impl fmt::Display for WebCredentialsError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("web username and password must be nonempty; username cannot contain ':'")
    }
}

impl std::error::Error for WebCredentialsError {}

struct WebState<S: StoreInterface> {
    store: Arc<S>,
    credentials: WebCredentials,
}

/// Serves the browser object manager on an already-bound HTTP listener.
///
/// The supplied store is used directly by the web routes. Clone a backend
/// before calling this function when another service must share the store.
pub async fn serve_web_interface<S: StoreInterface>(
    listener: TcpListener,
    store: Arc<S>,
    credentials: WebCredentials,
) -> std::io::Result<()> {
    let address = listener.local_addr()?;
    let state = Arc::new(WebState { store, credentials });
    axum::serve(listener, web_router(state))
        .await
        .map_err(|error| std::io::Error::other(format!("web interface at {address} failed: {error}")))
}

fn web_router<S: StoreInterface>(state: Arc<WebState<S>>) -> Router {
    Router::new()
        .route("/", get(index))
        .route("/api/entries", get(entries::<S>))
        .route("/api/metadata", get(metadata::<S>))
        .route("/api/download", get(download::<S>))
        .route("/api/object", put(upload::<S>).delete(delete_object::<S>))
        .route_layer(middleware::from_fn_with_state(
            Arc::clone(&state),
            authenticate::<S>,
        ))
        .with_state(state)
}

async fn index() -> Html<&'static str> {
    Html(INDEX_HTML)
}

async fn authenticate<S: StoreInterface>(
    State(state): State<Arc<WebState<S>>>,
    request: Request<Body>,
    next: Next,
) -> Response {
    let mut response = if valid_basic_authorization(request.headers(), &state.credentials) {
        next.run(request).await
    } else {
        let mut response = text_response(StatusCode::UNAUTHORIZED, "authentication required\n");
        response.headers_mut().insert(
            header::WWW_AUTHENTICATE,
            HeaderValue::from_static("Basic realm=\"Journey Storage\", charset=\"UTF-8\""),
        );
        response
    };
    response.headers_mut().insert(
        header::CACHE_CONTROL,
        HeaderValue::from_static("no-store"),
    );
    response
}

fn valid_basic_authorization(headers: &HeaderMap, credentials: &WebCredentials) -> bool {
    let mut values = headers.get_all(header::AUTHORIZATION).iter();
    let Some(value) = values.next() else {
        return false;
    };
    if values.next().is_some() {
        return false;
    }
    let Ok(value) = value.to_str() else {
        return false;
    };
    if value.len() > 8 * 1024 {
        return false;
    }
    let Some((scheme, encoded)) = value.split_once(' ') else {
        return false;
    };
    if !scheme.eq_ignore_ascii_case("basic") {
        return false;
    }
    let Ok(decoded) = STANDARD.decode(encoded.as_bytes()) else {
        return false;
    };
    let Some(separator) = decoded.iter().position(|byte| *byte == b':') else {
        return false;
    };

    let username_digest: [u8; 32] = Sha256::digest(&decoded[..separator]).into();
    let password_digest: [u8; 32] = Sha256::digest(&decoded[separator + 1..]).into();
    let expected_username: [u8; 32] = Sha256::digest(credentials.username.as_bytes()).into();
    let expected_password: [u8; 32] = Sha256::digest(credentials.password.as_bytes()).into();
    bool::from(username_digest.ct_eq(&expected_username) & password_digest.ct_eq(&expected_password))
}

#[derive(Deserialize, Default)]
#[serde(default, deny_unknown_fields)]
struct EntriesQuery {
    prefix: String,
    cursor: Option<String>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct KeyQuery {
    key: String,
}

#[derive(Serialize)]
struct EntriesResponse {
    entries: Vec<EntryResponse>,
    #[serde(skip_serializing_if = "Option::is_none")]
    next_cursor: Option<String>,
}

#[derive(Serialize)]
struct EntryResponse {
    name: String,
    key: String,
    kind: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    content_type: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    size: Option<u64>,
}

#[derive(Serialize)]
struct MetadataResponse {
    key: String,
    content_type: String,
    size: u64,
}

async fn entries<S: StoreInterface>(
    State(state): State<Arc<WebState<S>>>,
    RawQuery(raw_query): RawQuery,
) -> Response {
    let query = match parse_query::<EntriesQuery>(raw_query.as_deref()) {
        Ok(query) => query,
        Err(()) => return text_response(StatusCode::BAD_REQUEST, BAD_REQUEST_BODY),
    };
    let prefix = query.prefix;
    if prefix.len() > 1_024 || (!prefix.is_empty() && !prefix.ends_with('/')) {
        return text_response(StatusCode::BAD_REQUEST, BAD_REQUEST_BODY);
    }
    let mut seek_key = match query.cursor {
        Some(token) => match decode_cursor(&token, &prefix) {
            Ok(key) => Some(key),
            Err(()) => return text_response(StatusCode::BAD_REQUEST, BAD_REQUEST_BODY),
        },
        None => None,
    };

    let mut entries = Vec::with_capacity(MAX_LIST_ROWS);
    let mut next_cursor = None;
    loop {
        let request = ListRequest::new(
            prefix.clone(),
            seek_key.clone().map(ListCursor::new),
            NonZeroUsize::new(1).unwrap(),
        );
        let page = match state.store.list(request).await {
            Ok(page) => page,
            Err(error) => return store_error_response("LIST", error),
        };
        let Some(object) = page.objects().first() else {
            break;
        };
        let object_key = object.key().as_str();
        let store_next_key = page.next_cursor().map(|cursor| cursor.start_key().clone());
        let (entry, next_seek, next_is_known) = if object_key == prefix && !prefix.is_empty() {
            (object_entry(object, object_key.to_owned()), store_next_key, true)
        } else {
            let remainder = match object_key.strip_prefix(&prefix) {
                Some(remainder) => remainder,
                None => return text_response(StatusCode::INTERNAL_SERVER_ERROR, STORAGE_ERROR_BODY),
            };
            match remainder.find('/') {
                Some(slash_index) => {
                    let component = &remainder[..slash_index];
                    let folder_prefix = format!("{}{}", prefix, &remainder[..=slash_index]);
                    let seek_text = format!("{}0", &folder_prefix[..folder_prefix.len() - 1]);
                    let next_seek = match Key::new(&seek_text) {
                        Ok(key) => Some(key),
                        Err(_) => return text_response(StatusCode::INTERNAL_SERVER_ERROR, STORAGE_ERROR_BODY),
                    };
                    let name = if component.is_empty() { "/".to_owned() } else { component.to_owned() };
                    (EntryResponse {
                        name,
                        key: folder_prefix,
                        kind: "folder",
                        content_type: None,
                        size: None,
                    }, next_seek, false)
                }
                None => (object_entry(object, remainder.to_owned()), store_next_key, true),
            }
        };
        entries.push(entry);
        if entries.len() == MAX_LIST_ROWS {
            if let Some(candidate) = next_seek {
                let has_more = if next_is_known {
                    true
                } else {
                    let probe = ListRequest::new(
                        prefix.clone(),
                        Some(ListCursor::new(candidate.clone())),
                        NonZeroUsize::new(1).unwrap(),
                    );
                    match state.store.list(probe).await {
                        Ok(page) => !page.objects().is_empty(),
                        Err(error) => return store_error_response("LIST", error),
                    }
                };
                if has_more {
                    next_cursor = Some(URL_SAFE_NO_PAD.encode(candidate.as_str().as_bytes()));
                }
            }
            break;
        }
        seek_key = next_seek;
        if seek_key.is_none() {
            break;
        }
    }

    Json(EntriesResponse { entries, next_cursor }).into_response()
}

fn object_entry(metadata: &ObjectMetadata, name: String) -> EntryResponse {
    EntryResponse {
        name,
        key: metadata.key().as_str().to_owned(),
        kind: "object",
        content_type: Some(metadata.content_type().as_header_value().to_str().unwrap_or("application/octet-stream").to_owned()),
        size: Some(metadata.payload_length()),
    }
}

fn decode_cursor(token: &str, prefix: &str) -> Result<Key, ()> {
    let decoded = URL_SAFE_NO_PAD.decode(token.as_bytes()).map_err(|_| ())?;
    let text = String::from_utf8(decoded).map_err(|_| ())?;
    let key = Key::new(&text).map_err(|_| ())?;
    if !key.as_str().starts_with(prefix) {
        return Err(());
    }
    Ok(key)
}

async fn metadata<S: StoreInterface>(
    State(state): State<Arc<WebState<S>>>,
    RawQuery(raw_query): RawQuery,
) -> Response {
    let query = match parse_query::<KeyQuery>(raw_query.as_deref()) {
        Ok(query) => query,
        Err(()) => return text_response(StatusCode::BAD_REQUEST, BAD_REQUEST_BODY),
    };
    let key = match Key::new(&query.key) {
        Ok(key) => key,
        Err(_) => return text_response(StatusCode::BAD_REQUEST, BAD_REQUEST_BODY),
    };
    match state.store.stat(&key).await {
        Ok(metadata) => Json(metadata_response(&metadata)).into_response(),
        Err(error) => store_error_response("STAT", error),
    }
}

fn metadata_response(metadata: &ObjectMetadata) -> MetadataResponse {
    MetadataResponse {
        key: metadata.key().as_str().to_owned(),
        content_type: metadata.content_type().as_header_value().to_str().unwrap_or("application/octet-stream").to_owned(),
        size: metadata.payload_length(),
    }
}

fn attachment_disposition(filename: &str) -> HeaderValue {
    let fallback: String = filename
        .chars()
        .map(|character| {
            if character.is_ascii_alphanumeric() || "-_. ".contains(character) {
                character
            } else {
                '_'
            }
        })
        .collect();
    let fallback = if fallback.is_empty() { "object" } else { &fallback };
    let mut encoded = String::with_capacity(filename.len());
    for byte in filename.bytes() {
        if byte.is_ascii_alphanumeric() || b"!#$&+-.^_`|~".contains(&byte) {
            encoded.push(byte as char);
        } else {
            const HEX: &[u8; 16] = b"0123456789ABCDEF";
            encoded.push('%');
            encoded.push(HEX[(byte >> 4) as usize] as char);
            encoded.push(HEX[(byte & 0x0f) as usize] as char);
        }
    }
    HeaderValue::from_str(&format!("attachment; filename=\"{}\"; filename*=UTF-8''{}", fallback, encoded))
        .expect("content disposition contains only valid header characters")
}

async fn download<S: StoreInterface>(
    State(state): State<Arc<WebState<S>>>,
    RawQuery(raw_query): RawQuery,
) -> Response {
    let query = match parse_query::<KeyQuery>(raw_query.as_deref()) {
        Ok(query) => query,
        Err(()) => return text_response(StatusCode::BAD_REQUEST, BAD_REQUEST_BODY),
    };
    let key = match Key::new(&query.key) {
        Ok(key) => key,
        Err(_) => return text_response(StatusCode::BAD_REQUEST, BAD_REQUEST_BODY),
    };
    match state.store.get(&key, None).await {
        Ok(GetResult::Found(object)) => {
            let filename = key
                .as_str()
                .rsplit('/')
                .next()
                .filter(|name| !name.is_empty())
                .unwrap_or("object");
            let (metadata, reader) = object.into_parts();
            let stream = object_stream(reader, metadata.payload_length(), key.as_str().to_owned());
            let mut response = Body::from_stream(stream).into_response();
            response.headers_mut().insert(
                header::CONTENT_TYPE,
                metadata.content_type().as_header_value().clone(),
            );
            response.headers_mut().insert(
                header::CONTENT_LENGTH,
                HeaderValue::from_str(&metadata.payload_length().to_string()).unwrap(),
            );
            response.headers_mut().insert(
                header::CONTENT_DISPOSITION,
                attachment_disposition(filename),
            );
            response
        }
        Ok(GetResult::Unsatisfiable { .. }) => text_response(StatusCode::INTERNAL_SERVER_ERROR, STORAGE_ERROR_BODY),
        Err(error) => store_error_response("GET", error),
    }
}

fn object_stream<O: ObjectInterface>(
    object: O,
    payload_length: u64,
    key: String,
) -> impl Stream<Item = Result<Bytes, StoreError>> + Send + 'static {
    stream::try_unfold((object, payload_length, key), |(mut object, mut remaining, key)| async move {
        if remaining == 0 {
            return Ok(None);
        }
        let requested = remaining.min(STREAM_CHUNK_SIZE as u64) as usize;
        let mut buffer = BytesMut::with_capacity(requested);
        buffer.resize(requested, 0);
        let count = match object.read(&mut buffer).await {
            Ok(count) if count > 0 && count <= requested => count,
            Ok(0) => {
                let error = StoreError::new(StoreErrorKind::Corrupt, "Object reader reached EOF before declared length");
                eprintln!("storage web GET read failed for key {key:?}: {error}");
                return Err(error);
            }
            Ok(count) => {
                let error = StoreError::new(
                    StoreErrorKind::Corrupt,
                    format!("Object reader returned {count} bytes for a {requested}-byte buffer"),
                );
                eprintln!("storage web GET read failed for key {key:?}: {error}");
                return Err(error);
            }
            Err(error) => {
                eprintln!("storage web GET read failed for key {key:?}: {error}");
                return Err(error);
            }
        };
        remaining -= count as u64;
        buffer.truncate(count);
        Ok(Some((buffer.freeze(), (object, remaining, key))))
    })
}

async fn upload<S: StoreInterface>(
    State(state): State<Arc<WebState<S>>>,
    request: Request<Body>,
) -> Response {
    if !same_http_origin(request.headers(), request.uri()) {
        return text_response(StatusCode::FORBIDDEN, FORBIDDEN_BODY);
    }
    let query = match parse_query::<KeyQuery>(request.uri().query()) {
        Ok(query) => query,
        Err(()) => return text_response(StatusCode::BAD_REQUEST, BAD_REQUEST_BODY),
    };
    let key = match Key::new(&query.key) {
        Ok(key) => key,
        Err(_) => return text_response(StatusCode::BAD_REQUEST, BAD_REQUEST_BODY),
    };
    let condition = match parse_put_condition(request.headers()) {
        Ok(condition) => condition,
        Err(()) => return text_response(StatusCode::BAD_REQUEST, BAD_REQUEST_BODY),
    };
    let mut content_types = request.headers().get_all(header::CONTENT_TYPE).iter();
    let content_type = match content_types.next() {
        Some(value) if content_types.next().is_none() => match ContentType::try_from_header(value) {
            Ok(content_type) => content_type,
            Err(_) => return text_response(StatusCode::BAD_REQUEST, BAD_REQUEST_BODY),
        },
        Some(_) => return text_response(StatusCode::BAD_REQUEST, BAD_REQUEST_BODY),
        None => {
            let value = HeaderValue::from_static("application/octet-stream");
            match ContentType::try_from_header(&value) {
                Ok(content_type) => content_type,
                Err(_) => return text_response(StatusCode::BAD_REQUEST, BAD_REQUEST_BODY),
            }
        }
    };
    let mut context = match state.store.put_context(key, content_type, condition).await {
        Ok(context) => context,
        Err(error) => return store_error_response("PUT context", error),
    };
    let mut chunks = request.into_body().into_data_stream();
    while let Some(chunk) = chunks.next().await {
        let chunk = match chunk {
            Ok(chunk) => chunk,
            Err(error) => {
                eprintln!("storage web PUT body read failed: {error}");
                return text_response(StatusCode::BAD_REQUEST, BAD_REQUEST_BODY);
            }
        };
        if let Err(error) = context.append(&chunk).await {
            return store_error_response("PUT append", error);
        }
    }
    match state.store.put(context).await {
        Ok(()) => StatusCode::NO_CONTENT.into_response(),
        Err(error) => store_error_response("PUT commit", error),
    }
}

async fn delete_object<S: StoreInterface>(
    State(state): State<Arc<WebState<S>>>,
    request: Request<Body>,
) -> Response {
    if !same_http_origin(request.headers(), request.uri()) {
        return text_response(StatusCode::FORBIDDEN, FORBIDDEN_BODY);
    }
    let query = match parse_query::<KeyQuery>(request.uri().query()) {
        Ok(query) => query,
        Err(()) => return text_response(StatusCode::BAD_REQUEST, BAD_REQUEST_BODY),
    };
    let key = match Key::new(&query.key) {
        Ok(key) => key,
        Err(_) => return text_response(StatusCode::BAD_REQUEST, BAD_REQUEST_BODY),
    };
    match state.store.delete(&key).await {
        Ok(()) => StatusCode::NO_CONTENT.into_response(),
        Err(error) => store_error_response("DELETE", error),
    }
}

fn parse_put_condition(headers: &HeaderMap) -> Result<PutCondition, ()> {
    let if_match: Vec<_> = headers.get_all(header::IF_MATCH).iter().collect();
    let if_none_match: Vec<_> = headers.get_all(header::IF_NONE_MATCH).iter().collect();
    if if_match.len() > 1 || if_none_match.len() > 1 || (!if_match.is_empty() && !if_none_match.is_empty()) {
        return Err(());
    }
    match (if_match.first(), if_none_match.first()) {
        (Some(value), None) if value.as_bytes() == b"*" => Ok(PutCondition::ReplaceOnly),
        (None, Some(value)) if value.as_bytes() == b"*" => Ok(PutCondition::CreateOnly),
        (None, None) => Err(()),
        _ => Err(()),
    }
}

fn same_http_origin(headers: &HeaderMap, uri: &Uri) -> bool {
    let mut origins = headers.get_all(header::ORIGIN).iter();
    let Some(origin) = origins.next() else {
        return false;
    };
    if origins.next().is_some() {
        return false;
    }
    let Ok(origin) = origin.to_str() else {
        return false;
    };
    let Ok(origin_uri) = Uri::from_str(origin) else {
        return false;
    };
    if !origin_uri.scheme_str().is_some_and(|scheme| scheme.eq_ignore_ascii_case("http"))
        || origin_uri.query().is_some()
        || !(origin_uri.path().is_empty() || origin_uri.path() == "/")
        || uri.scheme_str().is_some_and(|scheme| !scheme.eq_ignore_ascii_case("http"))
    {
        return false;
    }
    let Some(origin_authority) = origin_uri.authority() else {
        return false;
    };
    let mut hosts = headers.get_all(header::HOST).iter();
    let host_authority = match hosts.next() {
        Some(value) if hosts.next().is_none() => {
            let Ok(value) = value.to_str() else {
                return false;
            };
            let Ok(authority) = Authority::from_str(value) else {
                return false;
            };
            Some(authority)
        }
        Some(_) => return false,
        None => None,
    };
    let uri_authority = uri.authority().cloned();
    let request_authority = match (host_authority, uri_authority) {
        (Some(host), Some(uri)) if !authority_matches(&host, &uri) => return false,
        (Some(host), _) => host,
        (None, Some(uri)) => uri,
        (None, None) => return false,
    };
    authority_matches(origin_authority, &request_authority)
}

fn authority_matches(left: &Authority, right: &Authority) -> bool {
    if left.as_str().contains('@') || right.as_str().contains('@') {
        return false;
    }
    if (left.port().is_some() && left.port_u16().is_none())
        || (right.port().is_some() && right.port_u16().is_none())
    {
        return false;
    }
    left.host().eq_ignore_ascii_case(right.host())
        && left.port_u16().unwrap_or(80) == right.port_u16().unwrap_or(80)
}

fn parse_query<T: DeserializeOwned>(raw_query: Option<&str>) -> Result<T, ()> {
    serde_urlencoded::from_str(raw_query.unwrap_or("")).map_err(|_| ())
}

fn store_error_response(operation: &str, error: StoreError) -> Response {
    let (status, body) = match error.kind() {
        StoreErrorKind::NotFound => (StatusCode::NOT_FOUND, NOT_FOUND_BODY),
        StoreErrorKind::InvalidRequest => (StatusCode::BAD_REQUEST, BAD_REQUEST_BODY),
        StoreErrorKind::PreconditionFailed => (StatusCode::PRECONDITION_FAILED, PRECONDITION_FAILED_BODY),
        StoreErrorKind::Conflict => (StatusCode::CONFLICT, STORAGE_ERROR_BODY),
        StoreErrorKind::Capacity | StoreErrorKind::Corrupt | StoreErrorKind::Unavailable | StoreErrorKind::Internal => {
            (StatusCode::INTERNAL_SERVER_ERROR, STORAGE_ERROR_BODY)
        }
    };
    if status.is_server_error() {
        eprintln!("storage web {operation} failed: {error}");
    }
    text_response(status, body)
}

fn text_response(status: StatusCode, body: &'static str) -> Response {
    let mut response = Response::new(Body::from(body));
    *response.status_mut() = status;
    response.headers_mut().insert(
        header::CONTENT_TYPE,
        HeaderValue::from_static("text/plain; charset=utf-8"),
    );
    response
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::body::Body;
    use http_body_util::BodyExt;
    use std::{collections::BTreeSet, convert::Infallible, time::{SystemTime, UNIX_EPOCH}};
    use futures_util::stream;
    use std::sync::Arc;
    use tower::ServiceExt;

    use crate::storage_filesystem::{FilesystemStore, FilesystemStoreConfig};
    use crate::storage_in_memory::{Object, Store};

    fn credentials() -> WebCredentials {
        WebCredentials::new("user", "pass").unwrap()
    }

    fn test_router<S: StoreInterface>(store: Arc<S>) -> Router {
        web_router(Arc::new(WebState { store, credentials: credentials() }))
    }

    fn request(method: &str, uri: &str, authenticated: bool) -> Request<Body> {
        let mut builder = Request::builder().method(method).uri(uri).header(header::HOST, "example.test");
        if authenticated {
            builder = builder.header(header::AUTHORIZATION, "Basic dXNlcjpwYXNz");
        }
        builder.body(Body::empty()).unwrap()
    }

    async fn collect(response: Response) -> Bytes {
        response.into_body().collect().await.unwrap().to_bytes()
    }

    async fn seeded_router() -> Router {
        let content_type = ContentType::try_from_header(&HeaderValue::from_static("text/plain")).unwrap();
        let store = Arc::new(Store::new([
            (Key::new("alpha.txt").unwrap(), Object::new(content_type.clone(), Bytes::from_static(b"alpha"))),
            (Key::new("folder/a.txt").unwrap(), Object::new(content_type.clone(), Bytes::from_static(b"nested"))),
            (Key::new("folder/").unwrap(), Object::new(content_type, Bytes::from_static(b"slash key"))),
        ]).unwrap());
        test_router(store)
    }

    #[tokio::test]
    async fn every_page_and_data_route_requires_basic_authentication() {
        let app = seeded_router().await;
        for (method, uri) in [
            ("GET", "/"),
            ("GET", "/api/entries?prefix="),
            ("GET", "/api/metadata?key=alpha.txt"),
            ("GET", "/api/download?key=alpha.txt"),
            ("PUT", "/api/object?key=alpha.txt"),
            ("DELETE", "/api/object?key=alpha.txt"),
        ] {
            let response = app.clone().oneshot(request(method, uri, false)).await.unwrap();
            assert_eq!(response.status(), StatusCode::UNAUTHORIZED, "{method} {uri}");
            assert!(response.headers().contains_key(header::WWW_AUTHENTICATE));
        }
        let mut wrong = request("GET", "/", false);
        wrong.headers_mut().insert(header::AUTHORIZATION, HeaderValue::from_static("Basic dXNlcjp3cm9uZw=="));
        assert_eq!(app.oneshot(wrong).await.unwrap().status(), StatusCode::UNAUTHORIZED);
    }

    #[tokio::test]
    async fn listing_derives_folders_and_direct_slash_object_remains_visible() {
        let app = seeded_router().await;
        let response = app.clone().oneshot(request("GET", "/api/entries?prefix=", true)).await.unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        let page: serde_json::Value = serde_json::from_slice(&collect(response).await).unwrap();
        assert_eq!(page["entries"][0]["key"], "alpha.txt");
        assert_eq!(page["entries"][1]["kind"], "folder");
        assert_eq!(page["entries"][1]["key"], "folder/");

        let response = app.oneshot(request("GET", "/api/entries?prefix=folder%2F", true)).await.unwrap();
        let page: serde_json::Value = serde_json::from_slice(&collect(response).await).unwrap();
        assert_eq!(page["entries"][0]["key"], "folder/");
        assert_eq!(page["entries"][0]["kind"], "object");
        assert_eq!(page["entries"][1]["key"], "folder/a.txt");
    }

    #[tokio::test]
    async fn upload_download_metadata_delete_and_origin_checks_work() {
        let app = seeded_router().await;
        let mut cross_origin = request("PUT", "/api/object?key=new.txt", true);
        *cross_origin.body_mut() = Body::from("new");
        cross_origin.headers_mut().insert(header::ORIGIN, HeaderValue::from_static("http://other.test"));
        assert_eq!(app.clone().oneshot(cross_origin).await.unwrap().status(), StatusCode::FORBIDDEN);

        let mut without_origin = request("PUT", "/api/object?key=new.txt", true);
        without_origin.headers_mut().insert(header::CONTENT_TYPE, HeaderValue::from_static("text/plain"));
        without_origin.headers_mut().insert(header::IF_NONE_MATCH, HeaderValue::from_static("*"));
        *without_origin.body_mut() = Body::from("new");
        assert_eq!(app.clone().oneshot(without_origin).await.unwrap().status(), StatusCode::FORBIDDEN);

        let mut without_condition = request("PUT", "/api/object?key=new.txt", true);
        without_condition.headers_mut().insert(header::ORIGIN, HeaderValue::from_static("http://example.test"));
        without_condition.headers_mut().insert(header::CONTENT_TYPE, HeaderValue::from_static("text/plain"));
        *without_condition.body_mut() = Body::from("new");
        assert_eq!(app.clone().oneshot(without_condition).await.unwrap().status(), StatusCode::BAD_REQUEST);

        let mut upload = request("PUT", "/api/object?key=new.txt", true);
        upload.headers_mut().insert(header::ORIGIN, HeaderValue::from_static("http://example.test"));
        upload.headers_mut().insert(header::CONTENT_TYPE, HeaderValue::from_static("text/plain"));
        upload.headers_mut().insert(header::IF_NONE_MATCH, HeaderValue::from_static("*"));
        *upload.body_mut() = Body::from_stream(stream::iter([
            Ok::<_, Infallible>(Bytes::from_static(b"new ")),
            Ok(Bytes::from_static(b"contents")),
        ]));
        assert_eq!(app.clone().oneshot(upload).await.unwrap().status(), StatusCode::NO_CONTENT);

        let mut duplicate = request("PUT", "/api/object?key=new.txt", true);
        duplicate.headers_mut().insert(header::ORIGIN, HeaderValue::from_static("http://example.test"));
        duplicate.headers_mut().insert(header::CONTENT_TYPE, HeaderValue::from_static("text/plain"));
        duplicate.headers_mut().insert(header::IF_NONE_MATCH, HeaderValue::from_static("*"));
        *duplicate.body_mut() = Body::from("replacement");
        assert_eq!(app.clone().oneshot(duplicate).await.unwrap().status(), StatusCode::PRECONDITION_FAILED);

        let response = app.clone().oneshot(request("GET", "/api/metadata?key=new.txt", true)).await.unwrap();
        let metadata: serde_json::Value = serde_json::from_slice(&collect(response).await).unwrap();
        assert_eq!(metadata["size"], 12);
        assert_eq!(metadata["content_type"], "text/plain");

        let response = app.clone().oneshot(request("GET", "/api/download?key=new.txt", true)).await.unwrap();
        assert_eq!(response.headers()[header::CONTENT_LENGTH], "12");
        assert_eq!(
            response.headers()[header::CONTENT_DISPOSITION],
            "attachment; filename=\"new.txt\"; filename*=UTF-8''new.txt"
        );
        assert_eq!(collect(response).await, Bytes::from_static(b"new contents"));

        let mut replace = request("PUT", "/api/object?key=new.txt", true);
        replace.headers_mut().insert(header::ORIGIN, HeaderValue::from_static("http://example.test"));
        replace.headers_mut().insert(header::CONTENT_TYPE, HeaderValue::from_static("application/x-updated"));
        replace.headers_mut().insert(header::IF_MATCH, HeaderValue::from_static("*"));
        *replace.body_mut() = Body::from("updated");
        assert_eq!(app.clone().oneshot(replace).await.unwrap().status(), StatusCode::NO_CONTENT);

        let mut replace_missing = request("PUT", "/api/object?key=missing.txt", true);
        replace_missing.headers_mut().insert(header::ORIGIN, HeaderValue::from_static("http://example.test"));
        replace_missing.headers_mut().insert(header::CONTENT_TYPE, HeaderValue::from_static("text/plain"));
        replace_missing.headers_mut().insert(header::IF_MATCH, HeaderValue::from_static("*"));
        *replace_missing.body_mut() = Body::from("missing");
        assert_eq!(app.clone().oneshot(replace_missing).await.unwrap().status(), StatusCode::PRECONDITION_FAILED);

        let mut delete = request("DELETE", "/api/object?key=new.txt", true);
        delete.headers_mut().insert(header::ORIGIN, HeaderValue::from_static("http://example.test"));
        assert_eq!(app.clone().oneshot(delete).await.unwrap().status(), StatusCode::NO_CONTENT);

        let mut cross_origin_delete = request("DELETE", "/api/object?key=alpha.txt", true);
        cross_origin_delete.headers_mut().insert(header::ORIGIN, HeaderValue::from_static("http://other.test"));
        assert_eq!(app.clone().oneshot(cross_origin_delete).await.unwrap().status(), StatusCode::FORBIDDEN);
        assert_eq!(app.oneshot(request("GET", "/api/metadata?key=new.txt", true)).await.unwrap().status(), StatusCode::NOT_FOUND);
    }

    #[tokio::test]
    async fn adjacent_names_repeated_slashes_and_url_special_keys_remain_accessible() {
        let content_type = ContentType::try_from_header(&HeaderValue::from_static("text/plain")).unwrap();
        let keys = [
            "a//repeat",
            "adjacent",
            "adjacent-",
            "adjacent.",
            "adjacent/child",
            "ending/",
            "space +%?#雪",
        ];
        let store = Store::new(keys.into_iter().map(|key| (
            Key::new(key).unwrap(),
            Object::new(content_type.clone(), Bytes::from_static(b"value")),
        ))).unwrap();
        let app = test_router(Arc::new(store));

        let root = app.clone().oneshot(request("GET", "/api/entries?prefix=", true)).await.unwrap();
        let root: serde_json::Value = serde_json::from_slice(&collect(root).await).unwrap();
        let root_entries = root["entries"].as_array().unwrap();
        let root_keys: Vec<_> = root_entries.iter().map(|entry| entry["key"].as_str().unwrap()).collect();
        assert_eq!(root_keys, ["a/", "adjacent", "adjacent-", "adjacent.", "adjacent/", "ending/", "space +%?#雪"]);
        assert_eq!(root_entries[1]["kind"], "object");
        assert_eq!(root_entries[4]["kind"], "folder");

        let repeated = app.clone().oneshot(request("GET", "/api/entries?prefix=a%2F", true)).await.unwrap();
        let repeated: serde_json::Value = serde_json::from_slice(&collect(repeated).await).unwrap();
        assert_eq!(repeated["entries"][0]["key"], "a//");
        let repeated_child = app.clone().oneshot(request("GET", "/api/entries?prefix=a%2F%2F", true)).await.unwrap();
        let repeated_child: serde_json::Value = serde_json::from_slice(&collect(repeated_child).await).unwrap();
        assert_eq!(repeated_child["entries"][0]["key"], "a//repeat");

        let folder = app.clone().oneshot(request("GET", "/api/entries?prefix=adjacent%2F", true)).await.unwrap();
        let folder: serde_json::Value = serde_json::from_slice(&collect(folder).await).unwrap();
        assert_eq!(folder["entries"][0]["key"], "adjacent/child");

        let metadata = app.oneshot(request(
            "GET",
            "/api/metadata?key=space%20%2B%25%3F%23%E9%9B%AA",
            true,
        )).await.unwrap();
        let metadata: serde_json::Value = serde_json::from_slice(&collect(metadata).await).unwrap();
        assert_eq!(metadata["key"], "space +%?#雪");
    }

    #[tokio::test]
    async fn upload_and_download_stream_multiple_chunks_exactly() {
        let content_type = ContentType::try_from_header(&HeaderValue::from_static("application/octet-stream")).unwrap();
        let store = Store::new([(
            Key::new("unused").unwrap(),
            Object::new(content_type, Bytes::new()),
        )]).unwrap();
        let app = test_router(Arc::new(store));
        let expected: Vec<u8> = (0..160_000).map(|index| (index % 251) as u8).collect();
        let chunks = expected.chunks(8_192).map(|chunk| {
            Ok::<_, Infallible>(Bytes::copy_from_slice(chunk))
        }).collect::<Vec<_>>();
        let mut upload = request("PUT", "/api/object?key=large.bin", true);
        upload.headers_mut().insert(header::ORIGIN, HeaderValue::from_static("http://example.test"));
        upload.headers_mut().insert(header::CONTENT_TYPE, HeaderValue::from_static("application/octet-stream"));
        upload.headers_mut().insert(header::IF_NONE_MATCH, HeaderValue::from_static("*"));
        *upload.body_mut() = Body::from_stream(stream::iter(chunks));
        assert_eq!(app.clone().oneshot(upload).await.unwrap().status(), StatusCode::NO_CONTENT);

        let response = app.oneshot(request("GET", "/api/download?key=large.bin", true)).await.unwrap();
        let mut body = response.into_body();
        let mut downloaded = BytesMut::new();
        let mut frame_count = 0;
        while let Some(frame) = body.frame().await {
            let frame = frame.unwrap();
            if let Ok(data) = frame.into_data() {
                frame_count += 1;
                downloaded.extend_from_slice(&data);
            }
        }
        assert!(frame_count >= 3);
        assert_eq!(downloaded.as_ref(), expected.as_slice());
    }

    #[tokio::test]
    async fn listing_paginates_more_than_one_hundred_objects_and_nested_subtrees() {
        let content_type = ContentType::try_from_header(&HeaderValue::from_static("application/octet-stream")).unwrap();
        let objects = (0..120).map(|index| {
            let key = format!("item-{index:03}.bin");
            (Key::new(&key).unwrap(), Object::new(content_type.clone(), Bytes::new()))
        }).chain((0..2_000).map(|index| {
            let key = format!("folder/deep-{index:04}.bin");
            (Key::new(&key).unwrap(), Object::new(content_type.clone(), Bytes::new()))
        }));
        let store = Store::new(objects).unwrap();
        let app = test_router(Arc::new(store));
        let first = app.clone().oneshot(request("GET", "/api/entries?prefix=", true)).await.unwrap();
        let first: serde_json::Value = serde_json::from_slice(&collect(first).await).unwrap();
        assert_eq!(first["entries"].as_array().unwrap().len(), 100);
        let cursor = first["next_cursor"].as_str().unwrap();
        let second_uri = format!("/api/entries?prefix=&cursor={cursor}");
        let second = app.clone().oneshot(request("GET", &second_uri, true)).await.unwrap();
        let second: serde_json::Value = serde_json::from_slice(&collect(second).await).unwrap();
        assert_eq!(second["entries"].as_array().unwrap().len(), 21);
        assert!(second.get("next_cursor").is_none());
        let root_keys: Vec<_> = first["entries"].as_array().unwrap().iter()
            .chain(second["entries"].as_array().unwrap())
            .map(|entry| entry["key"].as_str().unwrap().to_owned())
            .collect();
        assert_eq!(root_keys.len(), 121);
        assert_eq!(root_keys.iter().collect::<BTreeSet<_>>().len(), 121);

        let mut cursor = None;
        let mut found = Vec::new();
        loop {
            let uri = match cursor {
                Some(ref cursor) => format!("/api/entries?prefix=folder%2F&cursor={cursor}"),
                None => "/api/entries?prefix=folder%2F".to_owned(),
            };
            let response = app.clone().oneshot(request("GET", &uri, true)).await.unwrap();
            let page: serde_json::Value = serde_json::from_slice(&collect(response).await).unwrap();
            found.extend(page["entries"].as_array().unwrap().iter().map(|entry| entry["key"].as_str().unwrap().to_owned()));
            cursor = page["next_cursor"].as_str().map(str::to_owned);
            if cursor.is_none() {
                break;
            }
        }
        assert_eq!(found.len(), 2_000);
        assert_eq!(found.iter().collect::<BTreeSet<_>>().len(), 2_000);
    }

    #[tokio::test]
    async fn listing_rejects_invalid_prefixes_and_cursors() {
        let app = seeded_router().await;
        assert_eq!(
            app.clone().oneshot(request("GET", "/api/entries?prefix=folder", true)).await.unwrap().status(),
            StatusCode::BAD_REQUEST,
        );
        let outside = URL_SAFE_NO_PAD.encode(b"outside/key");
        let uri = format!("/api/entries?prefix=folder%2F&cursor={outside}");
        assert_eq!(app.clone().oneshot(request("GET", &uri, true)).await.unwrap().status(), StatusCode::BAD_REQUEST);
        let invalid_utf8 = URL_SAFE_NO_PAD.encode([0xff]);
        let uri = format!("/api/entries?prefix=&cursor={invalid_utf8}");
        assert_eq!(app.oneshot(request("GET", &uri, true)).await.unwrap().status(), StatusCode::BAD_REQUEST);
    }

    #[tokio::test]
    async fn filesystem_backend_supports_browser_upload_and_download() {
        let nonce = SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_nanos();
        let root = std::env::temp_dir().join(format!("journey-web-interface-{}-{nonce}", std::process::id()));
        let store = FilesystemStore::open(FilesystemStoreConfig::new(&root)).await.unwrap();
        let app = test_router(Arc::new(store));
        let mut upload = request("PUT", "/api/object?key=nested%2Fitem.bin", true);
        upload.headers_mut().insert(header::ORIGIN, HeaderValue::from_static("http://example.test"));
        upload.headers_mut().insert(header::CONTENT_TYPE, HeaderValue::from_static("application/octet-stream"));
        upload.headers_mut().insert(header::IF_NONE_MATCH, HeaderValue::from_static("*"));
        *upload.body_mut() = Body::from("filesystem payload");
        assert_eq!(app.clone().oneshot(upload).await.unwrap().status(), StatusCode::NO_CONTENT);

        let mut duplicate = request("PUT", "/api/object?key=nested%2Fitem.bin", true);
        duplicate.headers_mut().insert(header::ORIGIN, HeaderValue::from_static("http://example.test"));
        duplicate.headers_mut().insert(header::CONTENT_TYPE, HeaderValue::from_static("text/plain"));
        duplicate.headers_mut().insert(header::IF_NONE_MATCH, HeaderValue::from_static("*"));
        *duplicate.body_mut() = Body::from("replacement");
        assert_eq!(app.clone().oneshot(duplicate).await.unwrap().status(), StatusCode::PRECONDITION_FAILED);

        let metadata = app.clone().oneshot(request("GET", "/api/metadata?key=nested%2Fitem.bin", true)).await.unwrap();
        let metadata: serde_json::Value = serde_json::from_slice(&collect(metadata).await).unwrap();
        assert_eq!(metadata["size"], 18);
        assert_eq!(metadata["content_type"], "application/octet-stream");

        let response = app.clone().oneshot(request("GET", "/api/download?key=nested%2Fitem.bin", true)).await.unwrap();
        assert_eq!(collect(response).await, Bytes::from_static(b"filesystem payload"));

        let mut replace = request("PUT", "/api/object?key=nested%2Fitem.bin", true);
        replace.headers_mut().insert(header::ORIGIN, HeaderValue::from_static("http://example.test"));
        replace.headers_mut().insert(header::CONTENT_TYPE, HeaderValue::from_static("text/plain"));
        replace.headers_mut().insert(header::IF_MATCH, HeaderValue::from_static("*"));
        *replace.body_mut() = Body::from("replaced");
        assert_eq!(app.clone().oneshot(replace).await.unwrap().status(), StatusCode::NO_CONTENT);

        let mut delete = request("DELETE", "/api/object?key=nested%2Fitem.bin", true);
        delete.headers_mut().insert(header::ORIGIN, HeaderValue::from_static("http://example.test"));
        assert_eq!(app.clone().oneshot(delete).await.unwrap().status(), StatusCode::NO_CONTENT);
        assert_eq!(app.clone().oneshot(request("GET", "/api/metadata?key=nested%2Fitem.bin", true)).await.unwrap().status(), StatusCode::NOT_FOUND);
        drop(app);
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn credentials_reject_empty_values_and_redact_debug_output() {
        assert!(WebCredentials::new("", "pass").is_err());
        assert!(WebCredentials::new("user", "").is_err());
        assert!(WebCredentials::new("bad:user", "pass").is_err());
        assert_eq!(format!("{:?}", credentials()), "WebCredentials([redacted])");
    }

    #[test]
    fn same_origin_comparison_normalizes_host_case_and_default_port() {
        let headers = HeaderMap::from_iter([
            (header::HOST, HeaderValue::from_static("Example.Test:80")),
            (header::ORIGIN, HeaderValue::from_static("http://example.test")),
        ]);
        assert!(same_http_origin(&headers, &Uri::from_static("/api/object?key=x")));
        assert!(!same_http_origin(
            &headers,
            &Uri::from_static("https://example.test/api/object?key=x"),
        ));
        assert!(!same_http_origin(
            &headers,
            &Uri::from_static("http://other.test/api/object?key=x"),
        ));
    }
}
