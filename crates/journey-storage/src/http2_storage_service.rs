use std::{
    fmt,
    future::poll_fn,
    num::NonZeroUsize,
    task::Poll,
};

use base64::{engine::general_purpose::URL_SAFE_NO_PAD, Engine};
use bytes::{Bytes, BytesMut};
use http::{
    header,
    Method, Request, Response, StatusCode, Version,
};
use percent_encoding::percent_decode_str;
use serde::Serialize;
use std::sync::Arc;

use crate::storage_interface::{
    ContentType, GetResult, Key, ListCursor, ListRequest, ObjectInterface, ObjectMetadata,
    PutCondition, PutContextInterface, ReadRange, StoreErrorKind, StoreInterface,
};

const MAX_DATA_SEGMENT_SIZE: usize = 64 * 1024;
const NOT_FOUND_BODY: &[u8] = b"not found\n";
const INVALID_KEY_BODY: &[u8] = b"invalid key\n";
const BAD_REQUEST_BODY: &[u8] = b"bad request\n";
const PRECONDITION_FAILED_BODY: &[u8] = b"precondition failed\n";
const RANGE_NOT_SATISFIABLE_BODY: &[u8] = b"range not satisfiable\n";
const INVALID_CONTENT_TYPE: &[u8] = b"invalid content type\n";
const STORAGE_ERROR_BODY: &[u8] = b"storage error\n";
const METHOD_NOT_ALLOWED_BODY: &[u8] = b"method not allowed\n";
const DEFAULT_LIST_LIMIT: usize = 1_000;
const COLLECTION_ALLOW: &str = "GET";
const OBJECT_ALLOW: &str = "GET, HEAD, PUT, DELETE";

#[derive(Clone, Debug, PartialEq, Eq)]
enum Route {
    Collection,
    Object(String),
    EmptyKey,
    Unknown,
}

fn classify_route(path: &str) -> Route {
    if path == "/objects" {
        Route::Collection
    } else if path == "/objects/" {
        Route::EmptyKey
    } else if let Some(key) = path.strip_prefix("/objects/") {
        Route::Object(key.to_owned())
    } else {
        Route::Unknown
    }
}

#[derive(Debug)]
struct ListQuery {
    prefix: String,
    cursor: Option<Key>,
    requested_limit: NonZeroUsize,
}

#[derive(Debug, Serialize)]
struct ListObjectResponse {
    key: String,
    content_type: String,
    size: u64,
}

#[derive(Debug, Serialize)]
struct ListResponse {
    objects: Vec<ListObjectResponse>,
    #[serde(skip_serializing_if = "Option::is_none")]
    next_cursor: Option<String>,
}

fn has_valid_percent_escapes(value: &str) -> bool {
    let bytes = value.as_bytes();
    let mut index = 0;
    while index < bytes.len() {
        if bytes[index] == b'%' {
            if index + 2 >= bytes.len()
                || !bytes[index + 1].is_ascii_hexdigit()
                || !bytes[index + 2].is_ascii_hexdigit()
            {
                return false;
            }
            index += 3;
        } else {
            index += 1;
        }
    }
    true
}

fn decode_query_value(value: &str) -> Result<String, ()> {
    if !has_valid_percent_escapes(value) {
        return Err(());
    }
    percent_decode_str(value)
        .decode_utf8()
        .map(|value| value.into_owned())
        .map_err(|_| ())
}

fn decode_object_key(value: &str) -> Result<Key, ()> {
    if !has_valid_percent_escapes(value) {
        return Err(());
    }
    let decoded = percent_decode_str(value).decode_utf8().map_err(|_| ())?;
    Key::new(&decoded).map_err(|_| ())
}

fn parse_list_query(raw_query: Option<&str>) -> Result<ListQuery, ()> {
    let mut prefix = None;
    let mut limit = None;
    let mut cursor = None;

    for parameter in raw_query.unwrap_or("").split('&') {
        let (name, raw_value) = parameter.split_once('=').ok_or(())?;
        if !has_valid_percent_escapes(name) {
            return Err(());
        }
        let value = decode_query_value(raw_value)?;
        match name {
            "prefix" if prefix.is_none() => prefix = Some(value),
            "limit" if limit.is_none() => limit = Some(value),
            "cursor" if cursor.is_none() => cursor = Some(value),
            "prefix" | "limit" | "cursor" => return Err(()),
            _ => return Err(()),
        }
    }

    let prefix = prefix.ok_or(())?;
    let requested_limit = match limit {
        Some(value) => {
            if value.is_empty() || !value.bytes().all(|byte| byte.is_ascii_digit()) {
                return Err(());
            }
            let value = value.parse::<usize>().map_err(|_| ())?;
            NonZeroUsize::new(value).ok_or(())?
        }
        None => NonZeroUsize::new(DEFAULT_LIST_LIMIT).unwrap(),
    };

    let cursor = match cursor {
        Some(token) => {
            let decoded = URL_SAFE_NO_PAD.decode(token.as_bytes()).map_err(|_| ())?;
            let key_text = String::from_utf8(decoded).map_err(|_| ())?;
            let key = Key::new(&key_text).map_err(|_| ())?;
            Some(key)
        }
        None => None,
    };

    Ok(ListQuery {
        prefix,
        cursor,
        requested_limit,
    })
}

fn parse_range_header(headers: &http::HeaderMap) -> Result<Option<ReadRange>, ()> {
    let mut values = headers.get_all(header::RANGE).iter();
    let Some(value) = values.next() else {
        return Ok(None);
    };
    if values.next().is_some() {
        return Err(());
    }

    let value = value.to_str().map_err(|_| ())?.trim_matches([' ', '\t']);
    let (unit, spec) = value.split_once('=').ok_or(())?;
    if !unit.eq_ignore_ascii_case("bytes") {
        return Ok(None);
    }
    if spec.is_empty() || spec.contains(',') || spec.matches('-').count() != 1 {
        return Err(());
    }

    let (start, end) = spec.split_once('-').ok_or(())?;
    match (start.is_empty(), end.is_empty()) {
        (true, false) => Ok(Some(ReadRange::Suffix {
            length: parse_range_position(end)?,
        })),
        (false, true) => Ok(Some(ReadRange::From {
            start: parse_range_position(start)?,
        })),
        (false, false) => {
            let start = parse_range_position(start)?;
            let end = parse_range_position(end)?;
            if end < start {
                return Err(());
            }
            Ok(Some(ReadRange::Closed { start, end }))
        }
        (true, true) => Err(()),
    }
}

fn parse_range_position(value: &str) -> Result<u64, ()> {
    if value.is_empty() || !value.bytes().all(|byte| byte.is_ascii_digit()) {
        return Err(());
    }
    value.parse::<u64>().map_err(|_| ())
}

fn parse_put_condition(headers: &http::HeaderMap) -> Result<PutCondition, ()> {
    let mut if_match_values = headers.get_all(header::IF_MATCH).iter();
    let if_match = if_match_values.next();
    if if_match_values.next().is_some() {
        return Err(());
    }

    let mut if_none_match_values = headers.get_all(header::IF_NONE_MATCH).iter();
    let if_none_match = if_none_match_values.next();
    if if_none_match_values.next().is_some() || (if_match.is_some() && if_none_match.is_some()) {
        return Err(());
    }

    let Some(value) = if_match.or(if_none_match) else {
        return Ok(PutCondition::Unconditional);
    };
    let value = value.to_str().map_err(|_| ())?.trim_matches([' ', '\t']);
    match (if_match.is_some(), value) {
        (true, "*") => Ok(PutCondition::ReplaceOnly),
        (false, "*") => Ok(PutCondition::CreateOnly),
        _ => Err(()),
    }
}

/// Handles one already accepted HTTP/2 request against a shared catalogue.
#[derive(Debug)]
pub struct Service<S: StoreInterface> {
    store: Arc<S>,
}

impl<S: StoreInterface> Clone for Service<S> {
    fn clone(&self) -> Self {
        Self { store: Arc::clone(&self.store) }
    }
}

impl<S: StoreInterface> Service<S> {
    /// Creates a service backed by the supplied mutable catalogue.
    pub fn new(store: Arc<S>) -> Self {
        Self { store }
    }

    /// Handles one accepted HTTP/2 request and sends its complete response.
    pub async fn handle(
        &self,
        request: Request<h2::RecvStream>,
        respond: h2::server::SendResponse<Bytes>,
    ) -> Result<(), ServiceError> {
        let route = classify_route(request.uri().path());
        let method = request.method().clone();
        match route {
            Route::Collection => match method {
                Method::GET => {
                    let query = match parse_list_query(request.uri().query()) {
                        Ok(query) => query,
                        Err(()) => {
                            return request_error(
                                respond,
                                false,
                                StatusCode::BAD_REQUEST,
                                None,
                                BAD_REQUEST_BODY,
                            );
                        }
                    };
                    self.handle_list(query, respond).await
                }
                _ => request_error(
                    respond,
                    method == Method::HEAD,
                    StatusCode::METHOD_NOT_ALLOWED,
                    Some(COLLECTION_ALLOW),
                    METHOD_NOT_ALLOWED_BODY,
                ),
            },
            Route::Object(raw_key) => {
                let key = match decode_object_key(&raw_key) {
                    Ok(key) => key,
                    Err(()) => {
                        return if method == Method::HEAD {
                            send_empty_response(respond, StatusCode::BAD_REQUEST, None, None, None)
                        } else {
                            send_text_response(
                                respond,
                                StatusCode::BAD_REQUEST,
                                None,
                                BAD_REQUEST_BODY,
                            )
                        };
                    }
                };
                match method {
                    Method::GET => self.handle_get(&key, request.headers(), respond).await,
                    Method::HEAD => self.handle_head(&key, respond).await,
                    Method::PUT => self.handle_put(request, &key, respond).await,
                    Method::DELETE => self.handle_delete(&key, respond).await,
                    _ => request_error(
                        respond,
                        false,
                        StatusCode::METHOD_NOT_ALLOWED,
                        Some(OBJECT_ALLOW),
                        METHOD_NOT_ALLOWED_BODY,
                    ),
                }
            }
            Route::EmptyKey => match method {
                Method::GET => {
                    send_text_response(respond, StatusCode::BAD_REQUEST, None, INVALID_KEY_BODY)
                }
                Method::HEAD => {
                    send_empty_response(respond, StatusCode::BAD_REQUEST, None, None, None)
                }
                Method::PUT => {
                    send_text_response(respond, StatusCode::BAD_REQUEST, None, BAD_REQUEST_BODY)
                }
                Method::DELETE => {
                    send_text_response(respond, StatusCode::BAD_REQUEST, None, INVALID_KEY_BODY)
                }
                _ => request_error(
                    respond,
                    false,
                    StatusCode::BAD_REQUEST,
                    None,
                    INVALID_KEY_BODY,
                ),
            },
            Route::Unknown => request_error(
                respond,
                method == Method::HEAD,
                StatusCode::NOT_FOUND,
                None,
                NOT_FOUND_BODY,
            ),
        }
    }

    async fn handle_put(
        &self,
        request: Request<h2::RecvStream>,
        key: &Key,
        mut respond: h2::server::SendResponse<Bytes>,
    ) -> Result<(), ServiceError> {
        let condition = match parse_put_condition(request.headers()) {
            Ok(condition) => condition,
            Err(()) => {
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

        let mut put_context = match self
            .store
            .put_context((*key).clone(), content_type, condition)
            .await
        {
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
        let mut body = request.into_body();

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

        if let Err(error) = self.store.put(put_context).await {
            if error.kind() == StoreErrorKind::PreconditionFailed {
                return send_text_response(
                    respond,
                    StatusCode::PRECONDITION_FAILED,
                    None,
                    PRECONDITION_FAILED_BODY,
                );
            }
            eprintln!("storage PUT commit failed: {error}");
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

    async fn handle_get(
        &self,
        key: &Key,
        headers: &http::HeaderMap,
        mut respond: h2::server::SendResponse<Bytes>,
    ) -> Result<(), ServiceError> {
        let range = match parse_range_header(headers) {
            Ok(range) => range,
            Err(()) => {
                return send_text_response(
                    respond,
                    StatusCode::BAD_REQUEST,
                    None,
                    BAD_REQUEST_BODY,
                );
            }
        };
        let get_result = match self.store.get(key, range).await {
            Ok(get_result) => get_result,
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

        let mut read_object = match get_result {
            GetResult::Found(read_object) => read_object,
            GetResult::Unsatisfiable { complete_length } => {
                return send_range_unsatisfiable(respond, complete_length);
            }
        };

        let metadata = read_object.metadata();
        let selected_span = read_object.selected_span();
        let content_length = selected_span
            .map(|span| span.size())
            .unwrap_or_else(|| metadata.payload_length());
        let mut builder = Response::builder()
            .version(Version::HTTP_2)
            .status(if selected_span.is_some() {
                StatusCode::PARTIAL_CONTENT
            } else {
                StatusCode::OK
            })
            .header(header::CONTENT_TYPE, metadata.content_type().as_header_value())
            .header(header::CONTENT_LENGTH, content_length)
            .header(header::ACCEPT_RANGES, "bytes");
        if let Some(span) = selected_span {
            let Some(end) = span
                .offset()
                .checked_add(span.size())
                .and_then(|end| end.checked_sub(1))
                .filter(|end| span.size() > 0 && *end < metadata.payload_length())
            else {
                eprintln!("storage GET returned invalid selected span for key {key:?}");
                return send_text_response(
                    respond,
                    StatusCode::INTERNAL_SERVER_ERROR,
                    None,
                    STORAGE_ERROR_BODY,
                );
            };
            builder = builder.header(
                header::CONTENT_RANGE,
                format!("bytes {}-{end}/{}", span.offset(), metadata.payload_length()),
            );
        }
        let response = builder.body(())?;
        if content_length == 0 {
            respond.send_response(response, true)?;
            return Ok(());
        }

        let mut stream = respond.send_response(response, false)?;
        send_object_reader(
            &mut stream,
            read_object.object_mut(),
            content_length,
            key.as_str(),
        )
        .await
    }

    async fn handle_head(
        &self,
        key: &Key,
        respond: h2::server::SendResponse<Bytes>,
    ) -> Result<(), ServiceError> {
        let metadata = match self.store.stat(key).await {
            Ok(metadata) => metadata,
            Err(error) if error.kind() == StoreErrorKind::NotFound => {
                return send_empty_response(respond, StatusCode::NOT_FOUND, None, None, None);
            }
            Err(error) => {
                eprintln!("storage HEAD lookup failed for key {key:?}: {error}");
                return send_empty_response(
                    respond,
                    StatusCode::INTERNAL_SERVER_ERROR,
                    None,
                    None,
                    None,
                );
            }
        };

        let response = Response::builder()
            .version(Version::HTTP_2)
            .status(StatusCode::OK)
            .header(header::CONTENT_TYPE, metadata.content_type().as_header_value())
            .header(header::CONTENT_LENGTH, metadata.payload_length())
            .header(header::ACCEPT_RANGES, "bytes")
            .body(())?;
        let mut respond = respond;
        respond.send_response(response, true)?;
        Ok(())
    }

    async fn handle_delete(
        &self,
        key: &Key,
        respond: h2::server::SendResponse<Bytes>,
    ) -> Result<(), ServiceError> {
        if let Err(error) = self.store.delete(key).await {
            eprintln!("storage DELETE failed for key {key:?}: {error}");
            return send_text_response(
                respond,
                StatusCode::INTERNAL_SERVER_ERROR,
                None,
                STORAGE_ERROR_BODY,
            );
        }
        send_empty_response(respond, StatusCode::NO_CONTENT, None, None, None)
    }

    async fn handle_list(
        &self,
        query: ListQuery,
        respond: h2::server::SendResponse<Bytes>,
    ) -> Result<(), ServiceError> {
        let cursor = query
            .cursor
            .map(ListCursor::new);
        let request = ListRequest::new(query.prefix, cursor, query.requested_limit);
        let page = match self.store.list(request).await {
            Ok(page) => page,
            Err(error) => {
                eprintln!("storage LIST failed: {error}");
                return send_text_response(
                    respond,
                    StatusCode::INTERNAL_SERVER_ERROR,
                    None,
                    STORAGE_ERROR_BODY,
                );
            }
        };
        let (objects, next_cursor) = page.into_parts();
        let objects = objects
            .into_iter()
            .map(|metadata: ObjectMetadata| {
                let content_type = metadata
                    .content_type()
                    .as_header_value()
                    .to_str()
                    .map_err(|error| error.to_string())?;
                Ok(ListObjectResponse {
                    key: metadata.key().as_str().to_owned(),
                    content_type: content_type.to_owned(),
                    size: metadata.payload_length(),
                })
            })
            .collect::<Result<Vec<_>, String>>();
        let objects = match objects {
            Ok(objects) => objects,
            Err(error) => {
                eprintln!("storage LIST returned invalid content type metadata: {error}");
                return send_text_response(
                    respond,
                    StatusCode::INTERNAL_SERVER_ERROR,
                    None,
                    STORAGE_ERROR_BODY,
                );
            }
        };
        let response = ListResponse {
            objects,
            next_cursor: next_cursor
                .map(|cursor| URL_SAFE_NO_PAD.encode(cursor.start_key().as_str())),
        };
        let payload = match serde_json::to_vec(&response) {
            Ok(payload) => Bytes::from(payload),
            Err(error) => {
                eprintln!("HTTP LIST response serialization failed: {error}");
                return send_text_response(
                    respond,
                    StatusCode::INTERNAL_SERVER_ERROR,
                    None,
                    STORAGE_ERROR_BODY,
                );
            }
        };
        send_json_response(respond, payload).await
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

fn request_error(
    respond: h2::server::SendResponse<Bytes>,
    is_head: bool,
    status: StatusCode,
    allow: Option<&'static str>,
    body: &'static [u8],
) -> Result<(), ServiceError> {
    if is_head {
        send_empty_response(respond, status, allow, None, None)
    } else {
        send_text_response(respond, status, allow, body)
    }
}

fn send_empty_response(
    mut respond: h2::server::SendResponse<Bytes>,
    status: StatusCode,
    allow: Option<&'static str>,
    content_type: Option<&http::HeaderValue>,
    content_length: Option<u64>,
) -> Result<(), ServiceError> {
    let mut builder = Response::builder()
        .version(Version::HTTP_2)
        .status(status);
    if let Some(allow) = allow {
        builder = builder.header(header::ALLOW, allow);
    }
    if let Some(content_type) = content_type {
        builder = builder.header(header::CONTENT_TYPE, content_type);
    }
    if let Some(content_length) = content_length {
        builder = builder.header(header::CONTENT_LENGTH, content_length);
    }
    let response = builder.body(())?;
    respond.send_response(response, true)?;
    Ok(())
}

async fn send_json_response(
    mut respond: h2::server::SendResponse<Bytes>,
    payload: Bytes,
) -> Result<(), ServiceError> {
    let response = Response::builder()
        .version(Version::HTTP_2)
        .status(StatusCode::OK)
        .header(header::CONTENT_TYPE, "application/json")
        .header(header::CONTENT_LENGTH, payload.len())
        .body(())?;
    let mut stream = respond.send_response(response, false)?;
    send_payload(&mut stream, &payload).await
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

fn send_range_unsatisfiable(
    mut respond: h2::server::SendResponse<Bytes>,
    complete_length: u64,
) -> Result<(), ServiceError> {
    let response = Response::builder()
        .version(Version::HTTP_2)
        .status(StatusCode::RANGE_NOT_SATISFIABLE)
        .header(header::ACCEPT_RANGES, "bytes")
        .header(
            header::CONTENT_RANGE,
            format!("bytes */{complete_length}"),
        )
        .header(header::CONTENT_TYPE, "text/plain; charset=utf-8")
        .header(header::CONTENT_LENGTH, RANGE_NOT_SATISFIABLE_BODY.len())
        .body(())?;
    let mut stream = respond.send_response(response, false)?;
    stream.send_data(Bytes::from_static(RANGE_NOT_SATISFIABLE_BODY), true)?;
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

async fn send_object_reader<O: ObjectInterface>(
    stream: &mut h2::SendStream<Bytes>,
    reader: &mut O,
    content_length: u64,
    key: &str,
) -> Result<(), ServiceError> {
    let mut remaining = content_length;
    let mut buffer = BytesMut::new();
    while remaining > 0 {
        let requested = remaining.min(MAX_DATA_SEGMENT_SIZE as u64) as usize;
        stream.reserve_capacity(requested);
        let capacity = stream.capacity();
        let capacity = if capacity > 0 {
            capacity
        } else {
            poll_fn(|context| match stream.poll_capacity(context) {
                Poll::Ready(Some(Ok(capacity))) if capacity > 0 => Poll::Ready(Ok(capacity)),
                Poll::Ready(Some(Ok(_))) | Poll::Pending => Poll::Pending,
                Poll::Ready(Some(Err(error))) => Poll::Ready(Err(ServiceError::Http2(error))),
                Poll::Ready(None) => Poll::Ready(Err(ServiceError::StreamClosed)),
            })
            .await?
        };
        let requested = requested.min(capacity);
        buffer.resize(requested, 0);
        let count = match reader.read(&mut buffer).await {
            Ok(count) if count > 0 && count <= requested => count,
            Ok(0) => {
                eprintln!("storage GET reader reached EOF before declared length for key {key:?}");
                stream.send_reset(h2::Reason::INTERNAL_ERROR);
                return Ok(());
            }
            Ok(count) => {
                eprintln!(
                    "storage GET reader returned {count} bytes for a {requested}-byte buffer for key {key:?}"
                );
                stream.send_reset(h2::Reason::INTERNAL_ERROR);
                return Ok(());
            }
            Err(error) => {
                eprintln!("storage GET read failed for key {key:?}: {error}");
                stream.send_reset(h2::Reason::INTERNAL_ERROR);
                return Ok(());
            }
        };
        let chunk = Bytes::copy_from_slice(&buffer[..count]);
        remaining -= count as u64;
        stream.send_data(chunk, remaining == 0)?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        ContentType, GetResult, Key, ListPage, ListRequest, Object, ObjectMetadata,
        ObjectReader, PutCondition, PutContextInterface, ReadObject, ReadRange, Store, StoreError,
    };
    use h2::{client, server};
    use std::{collections::HashMap, future::Future, num::NonZeroUsize, sync::Arc};
    use tokio::{
        io::{duplex, DuplexStream},
        sync::{Notify, RwLock},
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

    fn sample_store() -> Arc<Store> {
        Arc::new(Store::new([
            object("image.jpg", "image/jpeg", Bytes::from_static(IMAGE)),
            object("video.mp4", "video/mp4", Bytes::from_static(VIDEO)),
        ])
        .unwrap())
    }

    async fn stored_object(store: &Store, key: &str) -> ReadObject<ObjectReader> {
        match store.get(&Key::new(key).unwrap(), None).await.unwrap() {
            GetResult::Found(read_object) => read_object,
            GetResult::Unsatisfiable { .. } => unreachable!(),
        }
    }

    async fn collect_object<O: ObjectInterface>(read: &mut ReadObject<O>) -> Bytes {
        let mut buffer = BytesMut::zeroed(3);
        let mut contents = Vec::new();
        loop {
            let count = read.object_mut().read(&mut buffer).await.unwrap();
            if count == 0 {
                break;
            }
            contents.extend_from_slice(&buffer[..count]);
        }
        Bytes::from(contents)
    }

    async fn collect_stored_object(store: &Store, key: &str) -> Bytes {
        let mut read = stored_object(store, key).await;
        collect_object(&mut read).await
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
        PreconditionFailed,
        Get,
        Read,
        EarlyEof,
        ShortRead,
        OverRead,
        ReadEmpty,
        Stat,
        List,
        Delete,
    }

    #[derive(Clone, Debug)]
    struct FailureObject {
        content_type: ContentType,
        contents: Bytes,
        cursor: usize,
        read_failure: bool,
        early_eof: bool,
        over_read: bool,
        read_limit: usize,
        read_gate: Option<Arc<Notify>>,
    }

    impl ObjectInterface for FailureObject {
        fn read(
            &mut self,
            buffer: &mut BytesMut,
        ) -> impl Future<Output = Result<usize, StoreError>> + Send {
            async move {
                if self.read_failure && self.cursor >= self.read_limit {
                    if let Some(read_gate) = &self.read_gate {
                        read_gate.notified().await;
                    }
                    return Err(StoreError::new(
                        StoreErrorKind::Internal,
                        SECRET_STORAGE_ERROR,
                    ));
                }
                if self.early_eof && self.cursor >= self.read_limit {
                    if let Some(read_gate) = &self.read_gate {
                        read_gate.notified().await;
                    }
                    return Ok(0);
                }
                if self.over_read && self.cursor >= self.read_limit {
                    if let Some(read_gate) = &self.read_gate {
                        read_gate.notified().await;
                    }
                    return Ok(buffer.len() + 1);
                }
                let count = buffer
                    .len()
                    .min(self.contents.len() - self.cursor)
                    .min(self.read_limit);
                buffer[..count]
                    .copy_from_slice(&self.contents[self.cursor..self.cursor + count]);
                self.cursor += count;
                Ok(count)
            }
        }
    }

    struct FailurePutContext {
        key: Key,
        failure: FailureOperation,
        content_type: ContentType,
        condition: PutCondition,
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
        read_gate: Arc<Notify>,
    }

    impl FailureStore {
        fn with_existing_object(failure: FailureOperation) -> Self {
            let objects = HashMap::from([(
                Key::new("target").unwrap(),
                FailureObject {
                    content_type: validated_content_type("image/jpeg"),
                    contents: if failure == FailureOperation::ReadEmpty {
                        Bytes::new()
                    } else {
                        Bytes::from_static(b"original object")
                    },
                    cursor: 0,
                    read_failure: false,
                    early_eof: false,
                    over_read: false,
                    read_limit: if failure == FailureOperation::ReadEmpty {
                        0
                    } else {
                        usize::MAX
                    },
                    read_gate: None,
                },
            )]);
            Self {
                failure,
                objects: Arc::new(RwLock::new(objects)),
                read_gate: Arc::new(Notify::new()),
            }
        }
    }

    impl StoreInterface for FailureStore {
        type Object = FailureObject;
        type PutContext = FailurePutContext;

        async fn get(
            &self,
            key: &Key,
            _range: Option<ReadRange>,
        ) -> Result<GetResult<Self::Object>, StoreError> {
            if self.failure == FailureOperation::Get {
                return Err(StoreError::new(
                    StoreErrorKind::Internal,
                    SECRET_STORAGE_ERROR,
                ));
            }

            let mut object = self.objects.read().await.get(key).cloned().ok_or_else(|| {
                StoreError::new(StoreErrorKind::NotFound, "missing test object")
            })?;
            object.read_failure = matches!(
                self.failure,
                FailureOperation::Read | FailureOperation::ReadEmpty
            );
            object.early_eof = self.failure == FailureOperation::EarlyEof;
            object.over_read = self.failure == FailureOperation::OverRead;
            if matches!(
                self.failure,
                FailureOperation::Read | FailureOperation::EarlyEof | FailureOperation::OverRead
            ) {
                object.read_gate = Some(Arc::clone(&self.read_gate));
            }
            if matches!(
                self.failure,
                FailureOperation::Read
                    | FailureOperation::EarlyEof
                    | FailureOperation::ShortRead
                    | FailureOperation::OverRead
            ) {
                object.read_limit = 2;
            }
            let metadata = ObjectMetadata::new(
                key.clone(),
                object.content_type.clone(),
                object.contents.len() as u64,
            );
            Ok(GetResult::Found(ReadObject::new(metadata, object)))
        }

        async fn stat(&self, key: &Key) -> Result<ObjectMetadata, StoreError> {
            if self.failure == FailureOperation::Stat {
                return Err(StoreError::new(
                    StoreErrorKind::Internal,
                    SECRET_STORAGE_ERROR,
                ));
            }
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
            if self.failure == FailureOperation::List {
                return Err(StoreError::new(
                    StoreErrorKind::Internal,
                    SECRET_STORAGE_ERROR,
                ));
            }
            Ok(ListPage::new(Vec::new(), None))
        }

        async fn delete(&self, key: &Key) -> Result<(), StoreError> {
            if self.failure == FailureOperation::Delete {
                return Err(StoreError::new(
                    StoreErrorKind::Internal,
                    SECRET_STORAGE_ERROR,
                ));
            }
            self.objects.write().await.remove(key);
            Ok(())
        }

        async fn put_context(
            &self,
            key: Key,
            content_type: ContentType,
            condition: PutCondition,
        ) -> Result<Self::PutContext, StoreError> {
            if self.failure == FailureOperation::PutContext {
                return Err(StoreError::new(
                    StoreErrorKind::Internal,
                    SECRET_STORAGE_ERROR,
                ));
            }

            Ok(FailurePutContext {
                key,
                failure: self.failure,
                content_type,
                condition,
                contents: Vec::new(),
            })
        }

        async fn put(&self, put_context: Self::PutContext) -> Result<(), StoreError> {
            if self.failure == FailureOperation::Commit {
                return Err(StoreError::new(
                    StoreErrorKind::Internal,
                    SECRET_STORAGE_ERROR,
                ));
            }
            if self.failure == FailureOperation::PreconditionFailed {
                return Err(StoreError::new(
                    StoreErrorKind::PreconditionFailed,
                    SECRET_STORAGE_ERROR,
                ));
            }

            let key = put_context.key;
            let mut objects = self.objects.write().await;
            let key_is_present = objects.contains_key(&key);
            let condition_satisfied = match put_context.condition {
                PutCondition::Unconditional => true,
                PutCondition::CreateOnly => !key_is_present,
                PutCondition::ReplaceOnly => key_is_present,
            };
            if !condition_satisfied {
                return Err(StoreError::new(
                    StoreErrorKind::PreconditionFailed,
                    "test PUT condition failed",
                ));
            }

            objects.insert(
                key,
                FailureObject {
                    content_type: put_context.content_type,
                    contents: put_context.contents.into(),
                    cursor: 0,
                    read_failure: false,
                    early_eof: false,
                    over_read: false,
                    read_limit: usize::MAX,
                    read_gate: None,
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

    async fn assert_empty_response(response: http::Response<h2::RecvStream>, status: StatusCode) {
        assert_eq!(response.status(), status);
        assert!(response.body().is_end_stream());
        assert!(collect(response.into_body()).await.unwrap().is_empty());
    }

    fn encode_query_component(value: &str) -> String {
        let mut encoded = String::new();
        for byte in value.bytes() {
            if byte.is_ascii_alphanumeric() || b"-_.~".contains(&byte) {
                encoded.push(byte as char);
            } else {
                encoded.push_str(&format!("%{byte:02X}"));
            }
        }
        encoded
    }

    #[test]
    fn route_classification_uses_the_exact_collection_path() {
        assert_eq!(classify_route("/objects"), Route::Collection);
        assert_eq!(classify_route("/objects/"), Route::EmptyKey);
        assert_eq!(classify_route("/objects/a/b"), Route::Object("a/b".to_owned()));
        assert_eq!(classify_route("/other"), Route::Unknown);
        assert_eq!(classify_route("/objects%2Fkey"), Route::Unknown);
    }

    #[test]
    fn object_key_decoding_is_single_pass_and_validates_utf8_and_length() {
        assert_eq!(decode_object_key("a%20b").unwrap().as_str(), "a b");
        assert_eq!(decode_object_key("a%2Fb").unwrap().as_str(), "a/b");
        assert_eq!(decode_object_key("a+b").unwrap().as_str(), "a+b");
        assert_eq!(decode_object_key("a%252Fb").unwrap().as_str(), "a%2Fb");
        assert_eq!(decode_object_key("%E7%8C%AB").unwrap().as_str(), "猫");

        for value in ["%", "%2", "%GG", "%FF"] {
            assert!(decode_object_key(value).is_err(), "accepted {value:?}");
        }
        assert!(decode_object_key(&"x".repeat(1_025)).is_err());
    }

    #[test]
    fn list_query_decodes_values_once_and_keeps_plus_literal() {
        let query = parse_list_query(Some("prefix=photos%2Fsummer+trip%20%2B%25")).unwrap();
        assert_eq!(query.prefix, "photos/summer+trip +%");
        assert_eq!(query.requested_limit.get(), DEFAULT_LIST_LIMIT);
        assert!(query.cursor.is_none());
    }

    #[test]
    fn list_query_rejects_invalid_and_ambiguous_parameters() {
        let valid_cursor = URL_SAFE_NO_PAD.encode("photos/a.jpg");
        let invalid_utf8_cursor = URL_SAFE_NO_PAD.encode([0xff]);
        let cases = [
            None,
            Some(""),
            Some("limit=1"),
            Some("prefix=a&prefix=b"),
            Some("prefix=a&limit=1&limit=2"),
            Some("prefix=a&cursor=x&cursor=y"),
            Some("prefix=a&other=b"),
            Some("prefix=%"),
            Some("prefix=%2"),
            Some("prefix=%GG"),
            Some("prefix=%FF"),
            Some("prefix=a&limit=0"),
            Some("prefix=a&limit=-1"),
            Some("prefix=a&limit=+1"),
            Some("prefix=a&limit=one"),
            Some("prefix=a&limit=184467440737095516160000"),
            Some("prefix=a&cursor=YWJj="),
            Some("prefix=a&cursor=ab+c"),
            Some("prefix=a&cursor=ab/c"),
            Some("prefix=a&cursor=%%%"),
        ];
        for query in cases {
            assert!(parse_list_query(query).is_err(), "accepted {query:?}");
        }
        assert!(parse_list_query(Some("prefix=a&cursor=")).is_err());
        assert!(
            parse_list_query(Some(&format!("prefix=a&cursor={invalid_utf8_cursor}"))).is_err()
        );
        assert!(
            parse_list_query(Some(&format!("prefix=photos/&cursor={valid_cursor}"))).is_ok()
        );
        assert!(
            parse_list_query(Some(&format!("prefix=videos/&cursor={valid_cursor}"))).is_ok()
        );
    }

    #[test]
    fn range_parser_accepts_supported_forms_and_ignores_unknown_units() {
        let cases = [
            ("bytes=0-9", Some(ReadRange::Closed { start: 0, end: 9 })),
            ("BYTES=10-", Some(ReadRange::From { start: 10 })),
            ("bytes=-25", Some(ReadRange::Suffix { length: 25 })),
            (" \tByTeS=2-4\t ", Some(ReadRange::Closed { start: 2, end: 4 })),
            ("items=0-9", None),
        ];
        for (value, expected) in cases {
            let mut headers = http::HeaderMap::new();
            headers.insert(header::RANGE, value.parse().unwrap());
            assert_eq!(parse_range_header(&headers).unwrap(), expected);
        }
    }

    #[test]
    fn range_parser_rejects_malformed_duplicate_and_overflowing_ranges() {
        let cases = [
            "bytes=",
            "bytes=-",
            "bytes=abc-def",
            "bytes=0-1,2-3",
            "bytes=1-2-3",
            "bytes=5-4",
            "bytes=18446744073709551616-",
            "bytes=0-18446744073709551616",
        ];
        for value in cases {
            let mut headers = http::HeaderMap::new();
            headers.insert(header::RANGE, value.parse().unwrap());
            assert!(parse_range_header(&headers).is_err(), "accepted {value:?}");
        }

        let mut headers = http::HeaderMap::new();
        headers.append(header::RANGE, "bytes=0-1".parse().unwrap());
        headers.append(header::RANGE, "bytes=2-3".parse().unwrap());
        assert!(parse_range_header(&headers).is_err());
    }

    #[test]
    fn put_condition_parser_accepts_only_one_wildcard_condition() {
        let mut headers = http::HeaderMap::new();
        assert_eq!(parse_put_condition(&headers).unwrap(), PutCondition::Unconditional);

        headers.insert(header::IF_NONE_MATCH, " \t*\t ".parse().unwrap());
        assert_eq!(parse_put_condition(&headers).unwrap(), PutCondition::CreateOnly);
        headers.remove(header::IF_NONE_MATCH);

        headers.insert(header::IF_MATCH, "*".parse().unwrap());
        assert_eq!(parse_put_condition(&headers).unwrap(), PutCondition::ReplaceOnly);
        headers.append(header::IF_MATCH, "*".parse().unwrap());
        assert!(parse_put_condition(&headers).is_err());

        headers.remove(header::IF_MATCH);
        headers.insert(header::IF_NONE_MATCH, "*".parse().unwrap());
        headers.insert(header::IF_MATCH, "*".parse().unwrap());
        assert!(parse_put_condition(&headers).is_err());

        headers.remove(header::IF_MATCH);
        for value in ["", "\"tag\"", "*, \"tag\""] {
            headers.insert(header::IF_NONE_MATCH, value.parse().unwrap());
            assert!(parse_put_condition(&headers).is_err(), "accepted {value:?}");
        }
    }

    #[tokio::test]
    async fn catalogue_constructs_and_looks_up_exact_keys() {
        let store = sample_store();

        assert_eq!(object_count(&store).await, 2);
        assert_eq!(collect_stored_object(&store, "image.jpg").await.as_ref(), IMAGE);
        assert_eq!(collect_stored_object(&store, "video.mp4").await.as_ref(), VIDEO);
        assert_eq!(
            store.get(&Key::new("IMAGE.jpg").unwrap(), None).await.unwrap_err().kind(),
            StoreErrorKind::NotFound
        );
        assert_eq!(
            store.get(&Key::new("missing").unwrap(), None).await.unwrap_err().kind(),
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
    async fn lookup_returns_a_reader_over_the_stored_payload() {
        let payload = Bytes::from(vec![7; 32]);
        let store = Store::new([object("payload", "application/octet-stream", payload)]).unwrap();
        let mut cloned = stored_object(&store, "payload").await;

        assert_eq!(collect_object(&mut cloned).await.as_ref(), vec![7; 32]);
    }

    #[tokio::test]
    async fn shared_store_handles_share_atomic_insertions_and_replacements() {
        let store = sample_store();
        let clone = store.clone();

        let key = Key::new("new").unwrap();
        let mut first = store
            .put_context(
                key.clone(),
                validated_content_type("text/plain"),
                PutCondition::Unconditional,
            )
            .await
            .unwrap();
        first.append(&Bytes::from_static(b"first")).await.unwrap();
        store.put(first).await.unwrap();
        assert_eq!(
            collect_stored_object(&clone, "new").await.as_ref(),
            b"first"
        );
        let mut second = clone
            .put_context(
                key.clone(),
                validated_content_type("application/json"),
                PutCondition::Unconditional,
            )
            .await
            .unwrap();
        second.append(&Bytes::from_static(b"second")).await.unwrap();
        clone.put(second).await.unwrap();

        let mut replaced = stored_object(&store, "new").await;
        assert_eq!(replaced.metadata().content_type().as_header_value().as_bytes(), b"application/json");
        assert_eq!(object_count(&store).await, 3);
        assert_eq!(collect_object(&mut replaced).await.as_ref(), b"second");
    }

    struct TestConnection {
        sender: client::SendRequest<Bytes>,
        _client_task: JoinHandle<()>,
        _server_task: JoinHandle<()>,
    }

    async fn connection<S: StoreInterface>(
        store: Arc<S>,
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

    async fn run_test_server<S: StoreInterface>(io: DuplexStream, service: Service<S>) {
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
        request_with_headers(sender, Method::GET, path, &[]).await
    }

    async fn request(
        sender: &mut client::SendRequest<Bytes>,
        method: Method,
        path: &str,
    ) -> Result<http::Response<h2::RecvStream>, h2::Error> {
        request_with_headers(sender, method, path, &[]).await
    }

    async fn request_with_headers(
        sender: &mut client::SendRequest<Bytes>,
        method: Method,
        path: &str,
        headers: &[(&str, &str)],
    ) -> Result<http::Response<h2::RecvStream>, h2::Error> {
        let mut builder = Request::builder().method(method).uri(path);
        for (name, value) in headers {
            builder = builder.header(*name, *value);
        }
        let request = builder.body(()).unwrap();
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
        put_with_headers(sender, path, content_type, content_length, &[], payload).await
    }

    async fn put_with_headers(
        sender: &mut client::SendRequest<Bytes>,
        path: &str,
        content_type: Option<&str>,
        content_length: Option<&str>,
        extra_headers: &[(&str, &str)],
        payload: &[u8],
    ) -> Result<http::Response<h2::RecvStream>, h2::Error> {
        let mut builder = Request::builder().method(Method::PUT).uri(path);
        if let Some(content_type) = content_type {
            builder = builder.header(header::CONTENT_TYPE, content_type);
        }
        if let Some(content_length) = content_length {
            builder = builder.header(header::CONTENT_LENGTH, content_length);
        }
        for (name, value) in extra_headers {
            builder = builder.header(*name, *value);
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
        assert_eq!(response.headers()[header::ACCEPT_RANGES], "bytes");
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
    async fn range_get_returns_exact_partial_headers_and_bytes() {
        let store = Store::new([object(
            "item",
            "application/octet-stream",
            Bytes::from_static(b"0123456789"),
        )])
        .unwrap();
        let mut connection = connection(Arc::new(store), None).await;
        let cases: [(&str, &[u8], &str); 7] = [
            ("bytes=0-2", b"012", "bytes 0-2/10"),
            ("bytes=3-5", b"345", "bytes 3-5/10"),
            ("bytes=9-9", b"9", "bytes 9-9/10"),
            ("bytes=7-", b"789", "bytes 7-9/10"),
            ("bytes=-4", b"6789", "bytes 6-9/10"),
            ("bytes=8-100", b"89", "bytes 8-9/10"),
            ("bytes=0-99", b"0123456789", "bytes 0-9/10"),
        ];

        for (range, expected, content_range) in cases {
            let response = request_with_headers(
                &mut connection.sender,
                Method::GET,
                "/objects/item",
                &[("range", range)],
            )
            .await
            .unwrap();
            assert_eq!(response.status(), StatusCode::PARTIAL_CONTENT);
            assert_eq!(response.headers()[header::CONTENT_TYPE], "application/octet-stream");
            assert_eq!(response.headers()[header::CONTENT_LENGTH], expected.len().to_string());
            assert_eq!(response.headers()[header::CONTENT_RANGE], content_range);
            assert_eq!(response.headers()[header::ACCEPT_RANGES], "bytes");
            assert_eq!(collect(response.into_body()).await.unwrap(), expected);
        }
    }

    #[tokio::test]
    async fn unsatisfiable_ranges_return_416_with_complete_length_and_bounded_body() {
        let store = Store::new([
            object("item", "application/octet-stream", Bytes::from_static(b"123")),
            object("empty", "application/octet-stream", Bytes::new()),
        ])
        .unwrap();
        let mut connection = connection(Arc::new(store), None).await;
        for (key, range, complete_length) in [
            ("item", "bytes=3-", 3),
            ("item", "bytes=20-30", 3),
            ("item", "bytes=-0", 3),
            ("empty", "bytes=0-0", 0),
        ] {
            let response = request_with_headers(
                &mut connection.sender,
                Method::GET,
                &format!("/objects/{key}"),
                &[("range", range)],
            )
            .await
            .unwrap();
            assert_eq!(response.status(), StatusCode::RANGE_NOT_SATISFIABLE);
            assert_eq!(response.headers()[header::ACCEPT_RANGES], "bytes");
            assert_eq!(
                response.headers()[header::CONTENT_RANGE],
                format!("bytes */{complete_length}")
            );
            assert_eq!(
                response.headers()[header::CONTENT_LENGTH],
                RANGE_NOT_SATISFIABLE_BODY.len().to_string()
            );
            assert_eq!(
                collect(response.into_body()).await.unwrap(),
                RANGE_NOT_SATISFIABLE_BODY
            );
        }
    }

    #[tokio::test]
    async fn malformed_ranges_return_400_and_unknown_units_return_complete_get() {
        let store = Store::new([object(
            "item",
            "application/octet-stream",
            Bytes::from_static(b"0123456789"),
        )])
        .unwrap();
        let mut connection = connection(Arc::new(store), None).await;
        for range in [
            "bytes=abc",
            "bytes=18446744073709551616-",
            "bytes=4-3",
            "bytes=0-1,4-5",
        ] {
            let response = request_with_headers(
                &mut connection.sender,
                Method::GET,
                "/objects/item",
                &[("range", range)],
            )
            .await
            .unwrap();
            assert_eq!(response.status(), StatusCode::BAD_REQUEST);
            assert_eq!(collect(response.into_body()).await.unwrap(), BAD_REQUEST_BODY);
        }

        let response = request_with_headers(
            &mut connection.sender,
            Method::GET,
            "/objects/item",
            &[("range", "bytes=0-1"), ("range", "bytes=2-3")],
        )
        .await
        .unwrap();
        assert_eq!(response.status(), StatusCode::BAD_REQUEST);
        assert_eq!(collect(response.into_body()).await.unwrap(), BAD_REQUEST_BODY);

        let response = request_with_headers(
            &mut connection.sender,
            Method::GET,
            "/objects/item",
            &[("range", "items=0-1")],
        )
        .await
        .unwrap();
        assert_success_headers(&response, "application/octet-stream", 10);
        assert_eq!(collect(response.into_body()).await.unwrap(), b"0123456789");

        let response = request_with_headers(
            &mut connection.sender,
            Method::GET,
            "/objects/missing",
            &[("range", "bytes=0-1")],
        )
        .await
        .unwrap();
        assert_eq!(response.status(), StatusCode::NOT_FOUND);
        assert_eq!(collect(response.into_body()).await.unwrap(), NOT_FOUND_BODY);
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
        let mut connection = connection(Arc::new(store.clone()), None).await;

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
        let mut connection = connection(Arc::new(store.clone()), None).await;

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
        let mut connection = connection(Arc::new(store.clone()), None).await;

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
        let mut connection = connection(Arc::new(store), None).await;

        let response = get(&mut connection.sender, "/objects/target")
            .await
            .unwrap();
        assert_storage_error_response(response).await;
    }

    #[tokio::test]
    async fn get_reader_errors_and_early_eof_reset_after_headers() {
        for failure in [
            FailureOperation::Read,
            FailureOperation::EarlyEof,
            FailureOperation::OverRead,
        ] {
            let store = FailureStore::with_existing_object(failure);
            let read_gate = Arc::clone(&store.read_gate);
            let mut connection = connection(Arc::new(store), None).await;
            let response = get(&mut connection.sender, "/objects/target")
                .await
                .unwrap();

            assert_eq!(response.status(), StatusCode::OK);
            assert_eq!(response.headers()[header::CONTENT_LENGTH], "15");
            let mut body = response.into_body();
            let prefix = body.data().await.unwrap().unwrap();
            body.flow_control().release_capacity(prefix.len()).unwrap();
            assert_eq!(&prefix[..], b"or");
            read_gate.notify_one();
            assert!(body.data().await.unwrap().is_err());
        }
    }

    #[tokio::test]
    async fn get_continues_after_short_positive_reader_reads() {
        let store = FailureStore::with_existing_object(FailureOperation::ShortRead);
        let mut connection = connection(Arc::new(store), None).await;
        let response = get(&mut connection.sender, "/objects/target")
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(response.headers()[header::CONTENT_LENGTH], "15");
        assert_eq!(collect(response.into_body()).await.unwrap(), b"original object");
    }

    #[tokio::test]
    async fn empty_get_sends_end_stream_without_reading_the_object() {
        let store = FailureStore::with_existing_object(FailureOperation::ReadEmpty);
        let mut connection = connection(Arc::new(store), None).await;
        let response = get(&mut connection.sender, "/objects/target")
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(response.headers()[header::CONTENT_LENGTH], "0");
        assert!(response.body().is_end_stream());
        assert!(collect(response.into_body()).await.unwrap().is_empty());
    }

    #[tokio::test]
    async fn head_uses_stat_and_returns_metadata_without_a_body() {
        let store = Store::new([
            object("image.jpg", "image/jpeg", Bytes::from_static(IMAGE)),
            object("empty", "application/octet-stream", Bytes::new()),
        ])
        .unwrap();
        let mut connection = connection(Arc::new(store), None).await;

        let response = request(&mut connection.sender, Method::HEAD, "/objects/image.jpg")
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(response.headers()[header::CONTENT_TYPE], "image/jpeg");
        assert_eq!(response.headers()[header::CONTENT_LENGTH], IMAGE.len().to_string());
        assert_eq!(response.headers()[header::ACCEPT_RANGES], "bytes");
        assert!(response.body().is_end_stream());
        assert!(collect(response.into_body()).await.unwrap().is_empty());

        let response = request(&mut connection.sender, Method::HEAD, "/objects/empty")
            .await
            .unwrap();
        assert_eq!(response.headers()[header::CONTENT_LENGTH], "0");
        assert_eq!(response.headers()[header::ACCEPT_RANGES], "bytes");
        assert_empty_response(response, StatusCode::OK).await;

        let response = request(&mut connection.sender, Method::HEAD, "/objects/missing")
            .await
            .unwrap();
        assert!(!response.headers().contains_key(header::CONTENT_TYPE));
        assert!(!response.headers().contains_key(header::CONTENT_LENGTH));
        assert_empty_response(response, StatusCode::NOT_FOUND).await;

        let response = request(&mut connection.sender, Method::HEAD, "/objects/")
            .await
            .unwrap();
        assert!(!response.headers().contains_key(header::CONTENT_TYPE));
        assert!(!response.headers().contains_key(header::CONTENT_LENGTH));
        assert_empty_response(response, StatusCode::BAD_REQUEST).await;
    }

    #[tokio::test]
    async fn head_does_not_call_get_and_redacts_stat_failures() {
        let store = FailureStore::with_existing_object(FailureOperation::Get);
        let mut get_failure_connection = connection(Arc::new(store), None).await;
        let response = request(&mut get_failure_connection.sender, Method::HEAD, "/objects/target")
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(response.headers()[header::CONTENT_TYPE], "image/jpeg");
        assert_eq!(response.headers()[header::CONTENT_LENGTH], "15");
        assert_eq!(response.headers()[header::ACCEPT_RANGES], "bytes");
        assert_empty_response(response, StatusCode::OK).await;

        let store = FailureStore::with_existing_object(FailureOperation::Stat);
        let mut connection = connection(Arc::new(store), None).await;
        let response = request(&mut connection.sender, Method::HEAD, "/objects/target")
            .await
            .unwrap();
        assert!(!response.headers().contains_key(header::CONTENT_LENGTH));
        assert_empty_response(response, StatusCode::INTERNAL_SERVER_ERROR).await;
    }

    #[tokio::test]
    async fn head_ignores_valid_malformed_and_duplicate_range_headers() {
        let store = Store::new([object(
            "item",
            "application/octet-stream",
            Bytes::from_static(b"0123456789"),
        )])
        .unwrap();
        let mut connection = connection(Arc::new(store), None).await;

        for ranges in [
            vec![("range", "bytes=2-4")],
            vec![("range", "bytes=malformed")],
            vec![("range", "bytes=0-1"), ("range", "bytes=2-3")],
        ] {
            let response = request_with_headers(
                &mut connection.sender,
                Method::HEAD,
                "/objects/item",
                &ranges,
            )
            .await
            .unwrap();
            assert_eq!(response.status(), StatusCode::OK);
            assert_eq!(response.headers()[header::CONTENT_TYPE], "application/octet-stream");
            assert_eq!(response.headers()[header::CONTENT_LENGTH], "10");
            assert_eq!(response.headers()[header::ACCEPT_RANGES], "bytes");
            assert!(!response.headers().contains_key(header::CONTENT_RANGE));
            assert_empty_response(response, StatusCode::OK).await;
        }
    }

    #[tokio::test]
    async fn delete_is_idempotent_and_returns_no_content_without_length() {
        let store = sample_store();
        let mut connection = connection(store.clone(), None).await;

        let response = request(&mut connection.sender, Method::DELETE, "/objects/image.jpg")
            .await
            .unwrap();
        assert!(!response.headers().contains_key(header::CONTENT_LENGTH));
        assert_empty_response(response, StatusCode::NO_CONTENT).await;
        assert_eq!(
            get(&mut connection.sender, "/objects/image.jpg")
                .await
                .unwrap()
                .status(),
            StatusCode::NOT_FOUND
        );

        for _ in 0..2 {
            let response = request(&mut connection.sender, Method::DELETE, "/objects/image.jpg")
                .await
                .unwrap();
            assert!(!response.headers().contains_key(header::CONTENT_LENGTH));
            assert_empty_response(response, StatusCode::NO_CONTENT).await;
        }

        let response = request(&mut connection.sender, Method::DELETE, "/objects/")
            .await
            .unwrap();
        assert_eq!(collect(response.into_body()).await.unwrap(), INVALID_KEY_BODY);
    }

    #[tokio::test]
    async fn delete_failure_is_bounded_and_preserves_the_object() {
        let store = FailureStore::with_existing_object(FailureOperation::Delete);
        let before = store.objects.read().await.get("target").unwrap().clone();
        let mut connection = connection(Arc::new(store.clone()), None).await;
        let response = request(&mut connection.sender, Method::DELETE, "/objects/target")
            .await
            .unwrap();
        assert_storage_error_response(response).await;
        let after = store.objects.read().await.get("target").unwrap().clone();
        assert_eq!(after.contents, before.contents);
        assert_eq!(after.content_type, before.content_type);
    }

    #[tokio::test]
    async fn list_returns_json_pages_and_continues_after_cursor_key_deletion() {
        let store = Arc::new(Store::new([
            object("photos/a.jpg", "image/jpeg", Bytes::from_static(b"a")),
            object("photos/b plus +.jpg", "image/jpeg", Bytes::from_static(b"bb")),
            object("photos/éété.jpg", "image/jpeg", Bytes::from_static(b"ccc")),
            object("videos/c.mp4", "video/mp4", Bytes::from_static(b"dddd")),
        ])
        .unwrap());
        let mut connection = connection(store.clone(), None).await;

        let first = get(&mut connection.sender, "/objects?prefix=photos%2F&limit=1")
            .await
            .unwrap();
        assert_eq!(first.status(), StatusCode::OK);
        assert_eq!(first.headers()[header::CONTENT_TYPE], "application/json");
        let first_length = first.headers()[header::CONTENT_LENGTH].clone();
        let first_body = collect(first.into_body()).await.unwrap();
        let first_json: serde_json::Value = serde_json::from_slice(&first_body).unwrap();
        assert_eq!(first_json["objects"][0]["key"], "photos/a.jpg");
        assert_eq!(first_json["objects"][0]["size"], 1);
        let cursor = first_json["next_cursor"].as_str().unwrap().to_owned();
        assert_eq!(
            first_length,
            first_body.len().to_string()
        );

        store.delete(&Key::new("photos/a.jpg").unwrap()).await.unwrap();
        let second_path = format!("/objects?prefix=photos%2F&limit=1&cursor={cursor}");
        let second = get(&mut connection.sender, &second_path).await.unwrap();
        let second_body = collect(second.into_body()).await.unwrap();
        let second_json: serde_json::Value = serde_json::from_slice(&second_body).unwrap();
        assert_eq!(second_json["objects"][0]["key"], "photos/b plus +.jpg");
        let second_cursor = second_json["next_cursor"].as_str().unwrap().to_owned();

        let third_path = format!("/objects?prefix=photos%2F&limit=1&cursor={second_cursor}");
        let third = get(&mut connection.sender, &third_path).await.unwrap();
        let third_body = collect(third.into_body()).await.unwrap();
        let third_json: serde_json::Value = serde_json::from_slice(&third_body).unwrap();
        assert_eq!(third_json["objects"][0]["key"], "photos/éété.jpg");
        assert!(third_json.get("next_cursor").is_none());

        let empty = get(&mut connection.sender, "/objects?prefix=absent%2F")
            .await
            .unwrap();
        let empty_json: serde_json::Value = serde_json::from_slice(
            &collect(empty.into_body()).await.unwrap(),
        )
        .unwrap();
        assert_eq!(empty_json, serde_json::json!({"objects": []}));

        let all = get(&mut connection.sender, "/objects?prefix=")
            .await
            .unwrap();
        let all_json: serde_json::Value = serde_json::from_slice(
            &collect(all.into_body()).await.unwrap(),
        )
        .unwrap();
        assert_eq!(all_json["objects"].as_array().unwrap().len(), 3);
    }

    #[tokio::test]
    async fn list_storage_failure_is_bounded_and_redacted() {
        let store = FailureStore::with_existing_object(FailureOperation::List);
        let mut connection = connection(Arc::new(store), None).await;
        let response = get(&mut connection.sender, "/objects?prefix=")
            .await
            .unwrap();
        assert_storage_error_response(response).await;
    }

    #[tokio::test]
    async fn routing_errors_use_resource_specific_allow_and_head_is_bodyless() {
        let mut connection = connection(sample_store(), None).await;
        let collection = request(&mut connection.sender, Method::POST, "/objects")
            .await
            .unwrap();
        assert_eq!(collection.status(), StatusCode::METHOD_NOT_ALLOWED);
        assert_eq!(collection.headers()[header::ALLOW], "GET");
        assert_eq!(collect(collection.into_body()).await.unwrap(), METHOD_NOT_ALLOWED_BODY);

        let object = request(&mut connection.sender, Method::POST, "/objects/image.jpg")
            .await
            .unwrap();
        assert_eq!(object.status(), StatusCode::METHOD_NOT_ALLOWED);
        assert_eq!(object.headers()[header::ALLOW], "GET, HEAD, PUT, DELETE");
        assert_eq!(collect(object.into_body()).await.unwrap(), METHOD_NOT_ALLOWED_BODY);

        let collection_head = request(&mut connection.sender, Method::HEAD, "/objects")
            .await
            .unwrap();
        assert_eq!(collection_head.headers()[header::ALLOW], "GET");
        assert_empty_response(collection_head, StatusCode::METHOD_NOT_ALLOWED).await;

        let object_head = request(&mut connection.sender, Method::HEAD, "/elsewhere")
            .await
            .unwrap();
        assert_empty_response(object_head, StatusCode::NOT_FOUND).await;

        let unknown = get(&mut connection.sender, "/elsewhere")
            .await
            .unwrap();
        assert_eq!(unknown.status(), StatusCode::NOT_FOUND);
        assert_eq!(collect(unknown.into_body()).await.unwrap(), NOT_FOUND_BODY);
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
        assert_eq!(collect_stored_object(&store, "uploaded.bin").await.as_ref(), uploaded);
    }

    #[tokio::test]
    async fn object_paths_decode_logical_keys_for_every_object_method_and_list_prefix() {
        let store = Arc::new(Store::default());
        let mut connection = connection(store.clone(), None).await;
        let cases = [
            ("space key", "space%20key"),
            ("literal%percent", "literal%25percent"),
            ("slash/key", "slash%2Fkey"),
            ("plus+key", "plus+key"),
            ("repeat//slashes", "repeat%2F%2Fslashes"),
            ("猫/雪", "%E7%8C%AB%2F%E9%9B%AA"),
        ];

        for (logical_key, encoded_key) in cases {
            let path = format!("/objects/{encoded_key}");
            let response = put(
                &mut connection.sender,
                &path,
                Some("text/plain"),
                None,
                logical_key.as_bytes(),
            )
            .await
            .unwrap();
            assert_eq!(response.status(), StatusCode::OK, "PUT {logical_key:?}");
            assert!(collect(response.into_body()).await.unwrap().is_empty());

            let response = get(&mut connection.sender, &path).await.unwrap();
            assert_eq!(response.status(), StatusCode::OK, "GET {logical_key:?}");
            assert_eq!(collect(response.into_body()).await.unwrap(), logical_key.as_bytes());

            let queried_path = format!("{path}?version=ignored");
            let response = get(&mut connection.sender, &queried_path).await.unwrap();
            assert_eq!(response.status(), StatusCode::OK, "GET {queried_path}");
            assert_eq!(collect(response.into_body()).await.unwrap(), logical_key.as_bytes());

            let response = request(&mut connection.sender, Method::HEAD, &path)
                .await
                .unwrap();
            assert_eq!(response.status(), StatusCode::OK, "HEAD {logical_key:?}");
            assert_eq!(response.headers()[header::CONTENT_LENGTH], logical_key.len().to_string());
            assert_empty_response(response, StatusCode::OK).await;

            let prefix = encode_query_component(logical_key);
            let response = get(
                &mut connection.sender,
                &format!("/objects?prefix={prefix}"),
            )
            .await
            .unwrap();
            let page: serde_json::Value = serde_json::from_slice(
                &collect(response.into_body()).await.unwrap(),
            )
            .unwrap();
            assert_eq!(page["objects"].as_array().unwrap().len(), 1);
            assert_eq!(page["objects"][0]["key"], logical_key);

            let response = request(&mut connection.sender, Method::DELETE, &path)
                .await
                .unwrap();
            assert_eq!(response.status(), StatusCode::NO_CONTENT, "DELETE {logical_key:?}");
            assert_empty_response(response, StatusCode::NO_CONTENT).await;
        }
    }

    #[tokio::test]
    async fn percent_escaped_keys_are_distinct_and_invalid_object_paths_return_400() {
        let store = Arc::new(Store::default());
        let mut connection = connection(store.clone(), None).await;
        for (path, logical_key, payload) in [
            ("/objects/a%20b", "a b", b"space".as_slice()),
            ("/objects/a%2520b", "a%20b", b"literal percent".as_slice()),
            ("/objects/literal%2Fpart", "literal/part", b"slash".as_slice()),
            ("/objects/literal%252Fpart", "literal%2Fpart", b"literal escape".as_slice()),
        ] {
            let response = put(
                &mut connection.sender,
                path,
                Some("text/plain"),
                None,
                payload,
            )
            .await
            .unwrap();
            assert_eq!(response.status(), StatusCode::OK);
            collect(response.into_body()).await.unwrap();
            assert_eq!(collect_stored_object(&store, logical_key).await.as_ref(), payload);
            let response = get(&mut connection.sender, path).await.unwrap();
            assert_eq!(collect(response.into_body()).await.unwrap(), payload);
        }

        let invalid_paths = [
            "/objects/%".to_owned(),
            "/objects/%2".to_owned(),
            "/objects/%GG".to_owned(),
            "/objects/%FF".to_owned(),
            format!("/objects/{}", "x".repeat(1_025)),
            format!("/objects/{}", encode_query_component(&"é".repeat(513))),
        ];
        for path in &invalid_paths {
            for method in [Method::GET, Method::HEAD, Method::PUT, Method::DELETE] {
                let response = request(&mut connection.sender, method.clone(), path)
                    .await
                    .unwrap();
                assert_eq!(response.status(), StatusCode::BAD_REQUEST, "{method} {path}");
                if method == Method::HEAD {
                    assert_empty_response(response, StatusCode::BAD_REQUEST).await;
                } else {
                    assert_eq!(collect(response.into_body()).await.unwrap(), BAD_REQUEST_BODY);
                }
            }
        }

        for method in [Method::GET, Method::HEAD, Method::PUT, Method::DELETE] {
            let response = request(&mut connection.sender, method.clone(), "/objects/")
                .await
                .unwrap();
            assert_eq!(response.status(), StatusCode::BAD_REQUEST);
            if method == Method::HEAD {
                assert_empty_response(response, StatusCode::BAD_REQUEST).await;
            } else {
                let expected_body = if method == Method::PUT {
                    BAD_REQUEST_BODY
                } else {
                    INVALID_KEY_BODY
                };
                assert_eq!(collect(response.into_body()).await.unwrap(), expected_body);
            }
        }
    }

    #[tokio::test]
    async fn large_put_releases_receive_capacity_until_end_stream() {
        let store = Arc::new(Store::default());
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
        let mut object = stored_object(&store, "image.jpg").await;
        assert_eq!(object.metadata().content_type().as_header_value().as_bytes(), b"image/png");
        assert_eq!(collect_object(&mut object).await.as_ref(), b"new image");
    }

    #[tokio::test]
    async fn conditional_puts_create_replace_and_preserve_failed_preconditions() {
        let store = sample_store();
        let mut connection = connection(store.clone(), None).await;

        let created = put_with_headers(
            &mut connection.sender,
            "/objects/conditional-new",
            Some("text/plain"),
            None,
            &[("if-none-match", "*")],
            b"created",
        )
        .await
        .unwrap();
        assert_eq!(created.status(), StatusCode::OK);
        assert_eq!(created.headers()[header::CONTENT_LENGTH], "0");
        assert!(collect(created.into_body()).await.unwrap().is_empty());

        let create_conflict = put_with_headers(
            &mut connection.sender,
            "/objects/image.jpg",
            Some("image/png"),
            None,
            &[("if-none-match", "*")],
            b"",
        )
        .await
        .unwrap();
        assert_eq!(create_conflict.status(), StatusCode::PRECONDITION_FAILED);
        assert_eq!(
            create_conflict.headers()[header::CONTENT_LENGTH],
            PRECONDITION_FAILED_BODY.len().to_string()
        );
        let failure_body = collect(create_conflict.into_body()).await.unwrap();
        assert_eq!(failure_body, PRECONDITION_FAILED_BODY);
        assert!(!failure_body.windows(b"private".len()).any(|part| part == b"private"));
        let mut unchanged = stored_object(&store, "image.jpg").await;
        assert_eq!(unchanged.metadata().content_type().as_header_value().as_bytes(), b"image/jpeg");
        assert_eq!(collect_object(&mut unchanged).await.as_ref(), IMAGE);

        let replace_conflict = put_with_headers(
            &mut connection.sender,
            "/objects/conditional-missing",
            Some("application/json"),
            None,
            &[("if-match", "*")],
            b"replacement",
        )
        .await
        .unwrap();
        assert_eq!(replace_conflict.status(), StatusCode::PRECONDITION_FAILED);
        assert_eq!(collect(replace_conflict.into_body()).await.unwrap(), PRECONDITION_FAILED_BODY);

        let replaced = put_with_headers(
            &mut connection.sender,
            "/objects/image.jpg",
            Some("image/png"),
            None,
            &[("if-match", "*")],
            b"replacement",
        )
        .await
        .unwrap();
        assert_eq!(replaced.status(), StatusCode::OK);
        assert!(collect(replaced.into_body()).await.unwrap().is_empty());
        let mut current = stored_object(&store, "image.jpg").await;
        assert_eq!(current.metadata().content_type().as_header_value().as_bytes(), b"image/png");
        assert_eq!(collect_object(&mut current).await.as_ref(), b"replacement");
    }

    #[tokio::test]
    async fn precondition_failure_redacts_private_store_detail() {
        let store = FailureStore::with_existing_object(FailureOperation::PreconditionFailed);
        let mut connection = connection(Arc::new(store), None).await;
        let response = put(
            &mut connection.sender,
            "/objects/target",
            Some("text/plain"),
            None,
            b"replacement",
        )
        .await
        .unwrap();

        assert_eq!(response.status(), StatusCode::PRECONDITION_FAILED);
        assert_eq!(
            response.headers()[header::CONTENT_LENGTH],
            PRECONDITION_FAILED_BODY.len().to_string()
        );
        assert_eq!(collect(response.into_body()).await.unwrap(), PRECONDITION_FAILED_BODY);
    }

    #[tokio::test]
    async fn invalid_conditional_put_headers_return_bad_request_before_context_creation() {
        let store = FailureStore::with_existing_object(FailureOperation::PutContext);
        let mut connection = connection(Arc::new(store), None).await;
        let cases: &[&[(&str, &str)]] = &[
            &[("if-match", "*"), ("if-match", "*")],
            &[("if-none-match", "*"), ("if-none-match", "*")],
            &[("if-match", "*"), ("if-none-match", "*")],
            &[("if-match", "\"tag\"")],
            &[("if-none-match", "")],
            &[("if-none-match", "*, \"tag\"")],
        ];

        for extra_headers in cases {
            let mut request = Request::builder()
                .method(Method::PUT)
                .uri("/objects/target")
                .header(header::CONTENT_TYPE, "text/plain")
                .body(())
                .unwrap();
            for (name, value) in *extra_headers {
                request.headers_mut().append(
                    http::HeaderName::from_bytes(name.as_bytes()).unwrap(),
                    http::HeaderValue::from_str(value).unwrap(),
                );
            }
            let (response, _) = connection.sender.send_request(request, true).unwrap();
            let response = response.await.unwrap();
            assert_eq!(response.status(), StatusCode::BAD_REQUEST);
            assert_eq!(collect(response.into_body()).await.unwrap(), BAD_REQUEST_BODY);
        }
    }

    #[tokio::test]
    async fn empty_put_body_publishes_an_empty_object() {
        let mut connection = connection(Arc::new(Store::default()), None).await;
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
            store.get(&Key::new("missing-type").unwrap(), None).await.unwrap_err().kind(),
            StoreErrorKind::NotFound
        );
    }

    #[tokio::test]
    async fn object_metadata_limits_are_enforced_by_http_put_routes() {
        let store = Arc::new(Store::default());
        let mut connection = connection(store.clone(), None).await;
        let maximum_key = format!("/objects/{}", "k".repeat(1_024));
        let maximum_content_type = "x".repeat(128);
        let accepted = put(
            &mut connection.sender,
            &maximum_key,
            Some(&maximum_content_type),
            None,
            b"ok",
        )
        .await
        .unwrap();
        assert_eq!(accepted.status(), StatusCode::OK);
        let _ = collect(accepted.into_body()).await.unwrap();

        let oversized_key = format!("/objects/{}", "k".repeat(1_025));
        let rejected_key = put(
            &mut connection.sender,
            &oversized_key,
            Some("text/plain"),
            None,
            b"",
        )
        .await
        .unwrap();
        assert_eq!(rejected_key.status(), StatusCode::BAD_REQUEST);
        assert_eq!(collect(rejected_key.into_body()).await.unwrap(), BAD_REQUEST_BODY);

        let oversized_content_type = "x".repeat(129);
        let rejected_content_type = put(
            &mut connection.sender,
            "/objects/oversized-content-type",
            Some(&oversized_content_type),
            None,
            b"",
        )
        .await
        .unwrap();
        assert_eq!(rejected_content_type.status(), StatusCode::BAD_REQUEST);
        assert_eq!(
            collect(rejected_content_type.into_body()).await.unwrap(),
            INVALID_CONTENT_TYPE
        );
        assert_eq!(object_count(&store).await, 1);
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
            store.get(&Key::new("duplicate-type").unwrap(), None).await.unwrap_err().kind(),
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
        let mut original = stored_object(&store, "image.jpg").await;
        assert_eq!(original.metadata().content_type().as_header_value().as_bytes(), b"image/jpeg");
        assert_eq!(collect_object(&mut original).await.as_ref(), IMAGE);
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

        let mut original = stored_object(&store, "image.jpg").await;
        assert_eq!(original.metadata().content_type().as_header_value().as_bytes(), b"image/jpeg");
        assert_eq!(collect_object(&mut original).await.as_ref(), IMAGE);
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
        let mut replacement = stored_object(&store, "image.jpg").await;
        assert_eq!(replacement.metadata().content_type().as_header_value().as_bytes(), b"image/png");
        assert_eq!(collect_object(&mut replacement).await.as_ref(), b"replacement bytes");
    }

    #[tokio::test]
    async fn concurrent_puts_publish_one_complete_payload_with_matching_metadata() {
        let store = Arc::new(Store::default());
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
        let mut final_object = final_object;
        let final_contents = collect_object(&mut final_object).await;
        let is_first = final_contents.as_ref() == b"first-candidate"
            && final_object.metadata().content_type().as_header_value().as_bytes() == b"text/plain";
        let is_second = final_contents.as_ref() == b"second-candidate"
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
        assert_eq!(response.headers()[header::ALLOW], OBJECT_ALLOW);
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
        let mut connection = connection(Arc::new(store), None).await;
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
        let mut connection = connection(Arc::new(store), None).await;
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
        let mut connection = connection(Arc::new(store), Some(WINDOW)).await;
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

    #[tokio::test]
    async fn management_and_payload_streams_share_one_connection() {
        let mut connection = connection(sample_store(), None).await;
        let (get_future, _) = connection
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
        let (head_future, _) = connection
            .sender
            .send_request(
                Request::builder()
                    .method(Method::HEAD)
                    .uri("/objects/image.jpg")
                    .body(())
                    .unwrap(),
                true,
            )
            .unwrap();
        let (list_future, _) = connection
            .sender
            .send_request(
                Request::builder()
                    .method(Method::GET)
                    .uri("/objects?prefix=")
                    .body(())
                    .unwrap(),
                true,
            )
            .unwrap();
        let (delete_future, _) = connection
            .sender
            .send_request(
                Request::builder()
                    .method(Method::DELETE)
                    .uri("/objects/video.mp4")
                    .body(())
                    .unwrap(),
                true,
            )
            .unwrap();
        let put_response = put(
            &mut connection.sender,
            "/objects/new.txt",
            Some("text/plain"),
            None,
            b"new object",
        )
        .await
        .unwrap();

        let get_response = get_future.await.unwrap();
        assert_eq!(get_response.status(), StatusCode::OK);
        assert_eq!(collect(get_response.into_body()).await.unwrap(), IMAGE);

        let head_response = head_future.await.unwrap();
        assert_eq!(head_response.status(), StatusCode::OK);
        assert_eq!(head_response.headers()[header::CONTENT_TYPE], "image/jpeg");
        assert_empty_response(head_response, StatusCode::OK).await;

        let list_response = list_future.await.unwrap();
        assert_eq!(list_response.status(), StatusCode::OK);
        let list_json: serde_json::Value = serde_json::from_slice(
            &collect(list_response.into_body()).await.unwrap(),
        )
        .unwrap();
        assert!(list_json["objects"].is_array());

        assert_eq!(put_response.status(), StatusCode::OK);
        assert!(collect(put_response.into_body()).await.unwrap().is_empty());
        let delete_response = delete_future.await.unwrap();
        assert_empty_response(delete_response, StatusCode::NO_CONTENT).await;
    }
}
