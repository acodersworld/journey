use axum::{
    body::Body,
    extract::{Extension, Path, State},
    http::{header, HeaderMap, HeaderValue, Method, StatusCode, Uri},
    response::{IntoResponse, Response},
};
use futures_util::{stream, StreamExt};
use journey_performance::{CHUNK_BYTES, DEFAULT_TRANSFER_BYTES};
use subtle::ConstantTimeEq;

use super::{AccountRole, AppState, AuthPrincipal, StorageClient};
use crate::storage::StorageBody;

struct DownloadOutcome {
    source: &'static str,
    expected: u64,
    received: u64,
    finished: bool,
}

impl DownloadOutcome {
    fn new(source: &'static str, expected: u64) -> Self {
        Self { source, expected, received: 0, finished: false }
    }

    fn finish(&mut self) {
        self.finished = true;
        if self.received == self.expected {
            log::info!("site_performance_completed source={} bytes={}", self.source, self.received);
        } else {
            log::error!("site_performance_failed source={} bytes={} expected={}", self.source, self.received, self.expected);
        }
    }
}

impl Drop for DownloadOutcome {
    fn drop(&mut self) {
        if !self.finished {
            log::info!("site_performance_cancelled source={} bytes={}", self.source, self.received);
        }
    }
}

fn logged_storage_body(body: StorageBody, bytes: u64) -> Body {
    let outcome = DownloadOutcome::new("storage", bytes);
    Body::from_stream(stream::unfold((body, outcome), |(mut body, mut outcome)| async move {
        match body.next().await {
            Some(Ok(chunk)) => {
                outcome.received += chunk.len() as u64;
                Some((Ok(chunk), (body, outcome)))
            }
            Some(Err(error)) => {
                outcome.finished = true;
                log::error!("site_performance_failed source=storage bytes={} error={error}", outcome.received);
                Some((Err(error), (body, outcome)))
            }
            None => {
                outcome.finish();
                None
            }
        }
    }))
}

fn requested_bytes(uri: &Uri) -> Result<u64, Box<Response>> {
    let Some(query) = uri.query() else { return Ok(DEFAULT_TRANSFER_BYTES) };
    let value = query.strip_prefix("bytes=").ok_or_else(|| StatusCode::BAD_REQUEST.into_response())?;
    if value.is_empty() || !value.bytes().all(|byte| byte.is_ascii_digit()) {
        return Err(Box::new(StatusCode::BAD_REQUEST.into_response()));
    }
    value.parse::<u64>().ok().filter(|bytes| *bytes > 0)
        .ok_or_else(|| Box::new(StatusCode::BAD_REQUEST.into_response()))
}

fn allowed<S: StorageClient>(state: &AppState<S>, principal: &AuthPrincipal) -> Result<(), Box<Response>> {
    if state.performance.is_none() {
        return Err(Box::new(StatusCode::NOT_FOUND.into_response()));
    }
    if principal.role != AccountRole::Admin {
        return Err(Box::new(StatusCode::FORBIDDEN.into_response()));
    }
    Ok(())
}

fn capability_allowed<S: StorageClient>(state: &AppState<S>, token: &str) -> Result<(), Box<Response>> {
    if state.performance.is_none() {
        return Err(Box::new(StatusCode::NOT_FOUND.into_response()));
    }
    let Some(expected) = state.performance_token.as_deref() else {
        return Err(Box::new(StatusCode::NOT_FOUND.into_response()));
    };
    if !bool::from(token.as_bytes().ct_eq(expected.as_bytes())) {
        return Err(Box::new(StatusCode::NOT_FOUND.into_response()));
    }
    Ok(())
}

fn headers(bytes: u64) -> HeaderMap {
    let mut headers = HeaderMap::new();
    headers.insert(header::CONTENT_TYPE, HeaderValue::from_static("application/octet-stream"));
    headers.insert(header::CONTENT_LENGTH, HeaderValue::from_str(&bytes.to_string()).unwrap());
    headers.insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store, no-transform"));
    headers
}

pub(super) async fn site<S: StorageClient>(
    State(state): State<AppState<S>>,
    Extension(principal): Extension<AuthPrincipal>,
    method: Method,
    uri: Uri,
) -> Response {
    if let Err(response) = allowed(&state, &principal) { return *response; }
    site_download(state, method, uri).await
}

pub(super) async fn site_capability<S: StorageClient>(
    State(state): State<AppState<S>>,
    Path(token): Path<String>,
    method: Method,
    uri: Uri,
) -> Response {
    if let Err(response) = capability_allowed(&state, &token) { return super::no_store(*response); }
    super::no_store(site_download(state, method, uri).await)
}

async fn site_download<S: StorageClient>(state: AppState<S>, method: Method, uri: Uri) -> Response {
    let bytes = match requested_bytes(&uri) { Ok(bytes) => bytes, Err(response) => return *response };
    let sample = state.performance.as_ref().unwrap();
    log::info!("site_performance_preparation bytes={bytes}");
    let file = match sample.prepare().await {
        Ok(file) => file,
        Err(error) => {
            log::error!("site_performance_preparation_failed error={error}");
            return StatusCode::INTERNAL_SERVER_ERROR.into_response();
        }
    };
    if method == Method::HEAD {
        return (StatusCode::OK, headers(bytes)).into_response();
    }
    let reader = match file.reader().await {
        Ok(reader) => reader,
        Err(error) => {
            log::error!("site_performance_reader_failed error={error}");
            return StatusCode::INTERNAL_SERVER_ERROR.into_response();
        }
    };
    log::info!("site_performance_started bytes={bytes}");
    let outcome = DownloadOutcome::new("site", bytes);
    let body = stream::try_unfold((reader, bytes, file, outcome), |(mut reader, remaining, file, mut outcome)| async move {
        if remaining == 0 {
            outcome.finish();
            return Ok(None);
        }
        let mut buffer = vec![0_u8; remaining.min(CHUNK_BYTES as u64) as usize];
        let count = match reader.read(&mut buffer).await {
            Ok(count) => count,
            Err(error) => {
                outcome.finished = true;
                log::error!("site_performance_failed source=site bytes={} error={error}", outcome.received);
                return Err(error);
            }
        };
        buffer.truncate(count);
        outcome.received += count as u64;
        Ok::<_, std::io::Error>(Some((buffer, (reader, remaining - count as u64, file, outcome))))
    });
    (StatusCode::OK, headers(bytes), Body::from_stream(body)).into_response()
}

pub(super) async fn storage<S: StorageClient>(
    State(state): State<AppState<S>>,
    Extension(principal): Extension<AuthPrincipal>,
    method: Method,
    uri: Uri,
) -> Response {
    if let Err(response) = allowed(&state, &principal) { return *response; }
    storage_download(state, method, uri).await
}

pub(super) async fn storage_capability<S: StorageClient>(
    State(state): State<AppState<S>>,
    Path(token): Path<String>,
    method: Method,
    uri: Uri,
) -> Response {
    if let Err(response) = capability_allowed(&state, &token) { return super::no_store(*response); }
    super::no_store(storage_download(state, method, uri).await)
}

async fn storage_download<S: StorageClient>(state: AppState<S>, method: Method, uri: Uri) -> Response {
    let bytes = match requested_bytes(&uri) { Ok(bytes) => bytes, Err(response) => return *response };
    log::info!("site_storage_performance_started bytes={bytes} method={method}");
    let response = match state.storage.performance(bytes, method == Method::HEAD).await {
        Ok(response) => response,
        Err(error) => {
            log::error!("site_storage_performance_failed error={error}");
            return StatusCode::BAD_GATEWAY.into_response();
        }
    };
    if response.status != StatusCode::OK {
        log::error!("site_storage_performance_failed status={}", response.status);
        return StatusCode::BAD_GATEWAY.into_response();
    }
    if response.headers.get(header::CONTENT_LENGTH).and_then(|value| value.to_str().ok())
        .and_then(|value| value.parse::<u64>().ok()) != Some(bytes) {
        log::error!("site_storage_performance_failed reason=invalid_content_length");
        return StatusCode::BAD_GATEWAY.into_response();
    }
    let response_headers = headers(bytes);
    if method == Method::HEAD {
        (StatusCode::OK, response_headers).into_response()
    } else {
        (StatusCode::OK, response_headers, logged_storage_body(response.body, bytes)).into_response()
    }
}

#[cfg(test)]
mod tests {
    use std::{path::Path, sync::{Arc, atomic::{AtomicUsize, Ordering}}, time::{Duration, SystemTime, UNIX_EPOCH}};
    use axum::{body::{to_bytes, Body}, http::Request};
    use bytes::Bytes;
    use futures_util::stream;
    use tower::ServiceExt;

    use super::*;
    use crate::{auth, db::Database, storage::{StorageBody, StorageResponse}};

    #[derive(Clone)]
    struct MockStorage(Arc<AtomicUsize>);

    impl StorageClient for MockStorage {
        async fn put_file(&self, _: &str, _: &Path) -> Result<String, String> { unreachable!() }
        async fn put_stream(&self, _: &str, _: Option<u64>, _: StorageBody, _: u64) -> Result<(String, u64), String> { unreachable!() }
        async fn get(&self, _: &str, _: Option<&str>, _: bool, _: bool) -> Result<StorageResponse, String> { unreachable!() }
        async fn performance(&self, _: u64, head: bool) -> Result<StorageResponse, String> {
            self.0.fetch_add(1, Ordering::Relaxed);
            let body: StorageBody = if head { Box::pin(stream::empty()) } else {
                Box::pin(stream::once(async { Ok(Bytes::from_static(b"x")) }))
            };
            let mut headers = HeaderMap::new();
            headers.insert(header::CONTENT_LENGTH, HeaderValue::from_static("1"));
            Ok(StorageResponse { status: StatusCode::OK, headers, body })
        }
    }

    #[test]
    fn capability_paths_are_redacted_from_request_logs() {
        assert_eq!(super::super::safe_route_template("/perftest/secret/site"), "/perftest/{token}/site");
        let malformed: Uri = "/perftest/secret/extra".parse().unwrap();
        assert_eq!(super::super::unmatched_request_path_field(&malformed, "unmatched"),
            " path=\"/perftest/[redacted]\"");
    }

    #[tokio::test]
    async fn endpoints_require_enabled_admin_session_before_preparation() {
        let nonce = SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_nanos();
        let path = std::env::temp_dir().join(format!("journey-performance-auth-{}-{nonce}.sqlite3", std::process::id()));
        let database = Database::new(path.clone());
        database.initialize().await.unwrap();
        for (username, role) in [("admin", AccountRole::Admin), ("reader", AccountRole::Read)] {
            database.create_account(username.to_owned(), role, "unused".to_owned()).await.unwrap();
        }
        let mut cookies = Vec::new();
        for username in ["admin", "reader"] {
            let account = database.login_account(username.to_owned()).await.unwrap().unwrap();
            let token = auth::new_session_token();
            let now = super::super::unix_time();
            database.issue_session(account.id, auth::session_token_digest(&token), now,
                now + Duration::from_secs(3600)).await.unwrap();
            cookies.push(format!("journey_session={token}"));
        }
        let calls = Arc::new(AtomicUsize::new(0));
        let storage = MockStorage(Arc::clone(&calls));
        let disabled = super::super::router(super::super::state(database.clone(), storage.clone()));
        let target = "/api/perftest/storage?bytes=1";
        let request = |cookie: Option<&str>| {
            let mut builder = Request::builder().uri(target);
            if let Some(cookie) = cookie { builder = builder.header(header::COOKIE, cookie); }
            builder.body(Body::empty()).unwrap()
        };
        assert_eq!(disabled.clone().oneshot(request(Some(&cookies[0]))).await.unwrap().status(), StatusCode::NOT_FOUND);
        let enabled = super::super::router(super::super::state(database.clone(), storage)
            .with_performance(Arc::new(journey_performance::Sample::default()), "test-token".to_owned()));
        assert_eq!(enabled.clone().oneshot(request(None)).await.unwrap().status(), StatusCode::UNAUTHORIZED);
        assert_eq!(enabled.clone().oneshot(request(Some(&cookies[1]))).await.unwrap().status(), StatusCode::FORBIDDEN);
        let capability_request = |token: &str| Request::builder()
            .uri(format!("/perftest/{token}/storage?bytes=1"))
            .body(Body::empty()).unwrap();
        assert_eq!(enabled.clone().oneshot(capability_request("wrong")).await.unwrap().status(), StatusCode::NOT_FOUND);
        assert_eq!(calls.load(Ordering::Relaxed), 0);
        let response = enabled.clone().oneshot(capability_request("test-token")).await.unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(response.headers()[header::CACHE_CONTROL], "private, no-store, no-transform");
        assert_eq!(to_bytes(response.into_body(), 2).await.unwrap().as_ref(), b"x");
        let site_head = Request::builder().method("HEAD")
            .uri("/perftest/test-token/site?bytes=1").body(Body::empty()).unwrap();
        let response = enabled.clone().oneshot(site_head).await.unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(response.headers()[header::CONTENT_LENGTH], "1");
        let rotated = super::super::router(super::super::state(database.clone(), MockStorage(Arc::clone(&calls)))
            .with_performance(Arc::new(journey_performance::Sample::default()), "new-token".to_owned()));
        assert_eq!(rotated.oneshot(capability_request("test-token")).await.unwrap().status(), StatusCode::NOT_FOUND);
        let response = enabled.oneshot(request(Some(&cookies[0]))).await.unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(response.headers()[header::CACHE_CONTROL], "private, no-store, no-transform");
        assert_eq!(to_bytes(response.into_body(), 2).await.unwrap().as_ref(), b"x");
        assert_eq!(calls.load(Ordering::Relaxed), 2);
        let _ = tokio::fs::remove_file(path).await;
    }
}
