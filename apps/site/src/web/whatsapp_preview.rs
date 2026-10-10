use axum::{
    body::Body,
    extract::{Path, State},
    http::{header, HeaderMap, Method, StatusCode, Uri},
    response::{Html, IntoResponse, Response},
};
use futures_util::StreamExt;
use std::{
    collections::HashMap,
    io,
    sync::{Arc, Mutex},
    time::{Duration, Instant},
};

use crate::{
    auth,
    db::Post,
    storage::{ImageReductionOptions, StorageClient},
};

use super::{
    escape_html, is_media_type, no_store, parse_origin, share_not_found, unix_time,
    valid_share_link_id, valid_token, AppState, SiteSecurity,
};

const WHATSAPP_PREVIEW_IMAGE_MAX_BYTES: u64 = 599_999;
const WHATSAPP_PREVIEW_IMAGE_MAX_EDGE: u32 = 1_200;

#[derive(Clone)]
pub(super) struct Config {
    image_lifetime: Duration,
}

impl Config {
    pub(super) fn from_ttl_seconds(seconds: i64) -> Result<Self, String> {
        if seconds <= 0 {
            return Err("site.whatsapp_preview_image_ttl_seconds must be a positive integer".to_owned());
        }
        Ok(Self { image_lifetime: Duration::from_secs(seconds as u64) })
    }
}

impl Default for Config {
    fn default() -> Self {
        Self { image_lifetime: Duration::from_secs(10) }
    }
}

#[derive(Clone, Default)]
pub(super) struct PreviewImageAuthorizations {
    entries: Arc<Mutex<HashMap<(String, String), PreviewImageAuthorization>>>,
}

#[derive(Clone, Copy)]
struct PreviewImageAuthorization {
    block_id: i64,
    expires_at: Instant,
}

impl PreviewImageAuthorizations {
    fn insert(
        &self,
        share_link_id: String,
        image_token_digest: String,
        block_id: i64,
        expires_at: Instant,
    ) {
        self.entries
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .insert(
                (share_link_id, image_token_digest),
                PreviewImageAuthorization { block_id, expires_at },
            );
    }

    fn block_id(
        &self,
        share_link_id: &str,
        image_token_digest: &str,
        now: Instant,
    ) -> Option<i64> {
        let key = (share_link_id.to_owned(), image_token_digest.to_owned());
        let mut entries = self.entries
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let authorization = entries.get(&key).copied()?;
        if authorization.expires_at <= now {
            entries.remove(&key);
            None
        } else {
            Some(authorization.block_id)
        }
    }

    fn remove_expired(&self, now: Instant) -> usize {
        let mut entries = self.entries
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let previous_len = entries.len();
        entries.retain(|_, authorization| authorization.expires_at > now);
        previous_len - entries.len()
    }

    #[cfg(test)]
    fn set_expiry(&self, share_link_id: &str, image_token_digest: &str, expires_at: Instant) {
        if let Some(authorization) = self.entries
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .get_mut(&(share_link_id.to_owned(), image_token_digest.to_owned()))
        {
            authorization.expires_at = expires_at;
        }
    }

    #[cfg(test)]
    fn expires_at(&self, share_link_id: &str, image_token_digest: &str) -> Option<Instant> {
        self.entries
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .get(&(share_link_id.to_owned(), image_token_digest.to_owned()))
            .map(|authorization| authorization.expires_at)
    }

    #[cfg(test)]
    fn len(&self) -> usize {
        self.entries
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .len()
    }
}


pub(super) async fn share_link_preview<S: StorageClient>(
    state: &AppState<S>,
    share_link_id: &str,
    secret: &str,
    headers: &HeaderMap,
    uri: &Uri,
) -> Response {
    if !valid_share_link_id(share_link_id) || !valid_token(secret) {
        return share_not_found();
    }
    let origin = match share_preview_origin(headers, uri, &state.security) {
        Some(origin) => origin,
        None => return no_store(StatusCode::BAD_REQUEST.into_response()),
    };
    let (post, share_link_expires_at) = match state.database.whatsapp_share_preview(
        share_link_id.to_owned(),
        auth::session_token_digest(secret),
        unix_time(),
    ).await {
        Ok(Some(preview)) => preview,
        Ok(None) => return share_not_found(),
        Err(error) => {
            log::error!("website WhatsApp share preview lookup failed: {error}");
            return no_store(StatusCode::INTERNAL_SERVER_ERROR.into_response());
        }
    };
    let share_url = format!("{origin}/share/{share_link_id}/{secret}");
    let open_url = format!("{share_url}?open=1");
    let image_url = match first_post_media(&post.blocks) {
        Some(block) => {
            let token = auth::new_session_token();
            let Some(expires_at) = preview_image_expiry(
                Instant::now(),
                unix_time(),
                share_link_expires_at,
                state.security.whatsapp_preview.image_lifetime,
            ) else {
                log::error!("website WhatsApp image capability expiry is out of range");
                return no_store(StatusCode::INTERNAL_SERVER_ERROR.into_response());
            };
            state.preview_image_authorizations.insert(
                share_link_id.to_owned(),
                auth::session_token_digest(&token),
                block.id,
                expires_at,
            );
            Some(format!(
                "{origin}/share/{share_link_id}/whatsapp-preview-image/{token}.jpg"
            ))
        }
        None => None,
    };
    no_store(Html(render_whatsapp_share_preview(
        &post,
        &share_url,
        &open_url,
        image_url.as_deref(),
    )).into_response())
}

pub(super) async fn image<S: StorageClient>(
    State(state): State<AppState<S>>,
    Path((share_link_id, random_name)): Path<(String, String)>,
    method: Method,
) -> Response {
    let Some(token) = random_name.strip_suffix(".jpg") else {
        return share_not_found();
    };
    if !valid_share_link_id(&share_link_id) || !valid_token(token) {
        return share_not_found();
    }
    let Some(block_id) = state.preview_image_authorizations.block_id(
        &share_link_id,
        &auth::session_token_digest(token),
        Instant::now(),
    ) else {
        return share_not_found();
    };
    let media = match state.database.whatsapp_preview_media(
        share_link_id.clone(),
        block_id,
        unix_time(),
    ).await {
        Ok(Some(media)) => media,
        Ok(None) => return share_not_found(),
        Err(error) => {
            log::error!("website WhatsApp preview image lookup failed: {error}");
            return no_store(StatusCode::INTERNAL_SERVER_ERROR.into_response());
        }
    };
    let options = ImageReductionOptions::new(
        WHATSAPP_PREVIEW_IMAGE_MAX_EDGE,
        Some(WHATSAPP_PREVIEW_IMAGE_MAX_BYTES),
    ).expect("WhatsApp preview image reduction options are valid");
    let head = method == Method::HEAD;
    let stored = match state.storage.get_reduced_image(
        &media.storage_key,
        options,
        is_media_type(&media.content_type, "video/"),
        head,
    ).await {
        Ok(stored) if stored.status == StatusCode::OK => stored,
        Ok(stored) => {
            log::error!(
                "website WhatsApp preview image storage returned an unexpected status: share_link_id={share_link_id} storage_key={:?} source_content_type={:?} status={} response_content_type={:?} response_content_length={:?}",
                media.storage_key,
                media.content_type,
                stored.status,
                stored.headers.get(header::CONTENT_TYPE),
                stored.headers.get(header::CONTENT_LENGTH),
            );
            return no_store(StatusCode::BAD_GATEWAY.into_response());
        }
        Err(error) => {
            log::error!(
                "website WhatsApp preview image storage request failed: share_link_id={share_link_id} storage_key={:?} source_content_type={:?}: {error}",
                media.storage_key,
                media.content_type,
            );
            return no_store(StatusCode::BAD_GATEWAY.into_response());
        }
    };
    let content_type = stored.headers.get(header::CONTENT_TYPE)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.split(';').next())
        .map(str::trim);
    if content_type != Some("image/jpeg") {
        log::error!(
            "website WhatsApp preview image storage returned an unexpected content type: share_link_id={share_link_id} storage_key={:?} source_content_type={:?} response_content_type={:?} response_content_length={:?}",
            media.storage_key,
            media.content_type,
            stored.headers.get(header::CONTENT_TYPE),
            stored.headers.get(header::CONTENT_LENGTH),
        );
        return no_store(StatusCode::BAD_GATEWAY.into_response());
    }
    let content_length = match stored.headers.get(header::CONTENT_LENGTH) {
        Some(value) => match value.to_str().ok().and_then(|value| value.parse::<u64>().ok()) {
            Some(length) if (1..=WHATSAPP_PREVIEW_IMAGE_MAX_BYTES).contains(&length) => length,
            _ => {
                log::error!(
                    "website WhatsApp preview image storage returned an invalid content length: share_link_id={share_link_id} storage_key={:?} source_content_type={:?} response_content_length={:?} maximum={WHATSAPP_PREVIEW_IMAGE_MAX_BYTES}",
                    media.storage_key,
                    media.content_type,
                    stored.headers.get(header::CONTENT_LENGTH),
                );
                return no_store(StatusCode::BAD_GATEWAY.into_response());
            }
        },
        None => {
            log::error!(
                "website WhatsApp preview image storage omitted content length: share_link_id={share_link_id} storage_key={:?} source_content_type={:?} response_content_type={:?} maximum={WHATSAPP_PREVIEW_IMAGE_MAX_BYTES}",
                media.storage_key,
                media.content_type,
                stored.headers.get(header::CONTENT_TYPE),
            );
            return no_store(StatusCode::BAD_GATEWAY.into_response());
        }
    };
    let mut builder = Response::builder()
        .status(StatusCode::OK)
        .header(header::CONTENT_TYPE, "image/jpeg")
        .header(header::X_CONTENT_TYPE_OPTIONS, "nosniff");
    builder = builder.header(header::CONTENT_LENGTH, content_length);
    let body = if head {
        Body::empty()
    } else {
        let body_share_link_id = share_link_id.clone();
        let mut streamed_bytes = 0_u64;
        let body = stored.body.map(move |chunk| match chunk {
            Ok(bytes) => {
                streamed_bytes = streamed_bytes.saturating_add(bytes.len() as u64);
                if streamed_bytes > WHATSAPP_PREVIEW_IMAGE_MAX_BYTES {
                    log::error!(
                        "website WhatsApp preview image stream exceeded its byte limit: share_link_id={body_share_link_id} bytes={streamed_bytes} maximum={WHATSAPP_PREVIEW_IMAGE_MAX_BYTES}"
                    );
                    Err(io::Error::other("reduced preview image exceeded its byte limit"))
                } else {
                    Ok(bytes)
                }
            }
            Err(error) => {
                log::error!(
                    "website WhatsApp preview image stream failed: share_link_id={body_share_link_id}: {error}"
                );
                Err(error)
            }
        });
        Body::from_stream(body)
    };
    no_store(builder.body(body).unwrap_or_else(|_| StatusCode::INTERNAL_SERVER_ERROR.into_response()))
}


fn is_whatsapp_preview_user_agent(headers: &HeaderMap) -> bool {
    let Some(user_agent) = headers.get(header::USER_AGENT).and_then(|value| value.to_str().ok()) else {
        return false;
    };
    let mut words = user_agent.split_ascii_whitespace();
    while let Some(word) = words.next() {
        let Some(version) = word.strip_prefix("WhatsApp/") else {
            continue;
        };
        let components: Vec<_> = version.split('.').collect();
        let client_marker_matches = match words.clone().next() {
            Some(marker) => matches!(marker, "A" | "I" | "N"),
            None => true,
        };
        if components.len() == 4
            && components[0] == "2"
            && components[1..].iter().all(|component| {
                !component.is_empty() && component.bytes().all(|byte| byte.is_ascii_digit())
            })
            && client_marker_matches
        {
            return true;
        }
    }
    false
}

fn first_post_media(blocks: &[crate::db::PostBlock]) -> Option<&crate::db::PostBlock> {
    for block in blocks {
        if block.storage_key.is_some()
            && block.content_type.as_deref().is_some_and(|content_type| {
                is_media_type(content_type, "image/") || is_media_type(content_type, "video/")
            })
        {
            return Some(block);
        }
        if let Some(media) = first_post_media(&block.children) {
            return Some(media);
        }
    }
    None
}


fn share_preview_origin(headers: &HeaderMap, uri: &Uri, security: &SiteSecurity) -> Option<String> {
    if let Some(origin) = &security.public_origin {
        return Some(origin.key.clone());
    }
    let request_origin = if let Some(authority) = uri.authority() {
        let scheme = uri.scheme_str().unwrap_or("http");
        parse_origin(&format!("{scheme}://{authority}"))
    } else {
        let host = headers.get(header::HOST)?.to_str().ok()?;
        parse_origin(&format!("http://{host}"))
    }?;
    request_origin.is_loopback().then_some(request_origin.key)
}

fn render_whatsapp_share_preview(
    post: &Post,
    share_url: &str,
    open_url: &str,
    image_url: Option<&str>,
) -> String {
    let title = escape_html(&post.summary.title);
    let summary = escape_html(&post.summary.summary);
    let share_url = escape_html(share_url);
    let open_url = escape_html(open_url);
    let image_metadata = image_url.map(|image_url| format!(
        "<meta property=\"og:image\" content=\"{}\"><meta property=\"og:image:type\" content=\"image/jpeg\">",
        escape_html(image_url),
    )).unwrap_or_default();
    format!(
        "<!doctype html><html lang=\"en\"><head><meta charset=\"utf-8\"><meta name=\"viewport\" content=\"width=device-width, initial-scale=1\"><meta property=\"og:type\" content=\"article\"><meta property=\"og:url\" content=\"{share_url}\"><meta property=\"og:title\" content=\"{title}\"><meta property=\"og:description\" content=\"{summary}\"><meta name=\"description\" content=\"{summary}\"><meta name=\"twitter:card\" content=\"summary_large_image\">{image_metadata}<title>{title} · Journey</title></head><body><main><h1>{title}</h1><p>{summary}</p><a id=\"open-post\" href=\"{open_url}\">Open post</a></main><script>window.location.replace(document.getElementById('open-post').href);</script></body></html>"
    )
}


fn preview_image_expiry(
    created_at: Instant,
    now: Duration,
    share_link_expires_at: Duration,
    configured_lifetime: Duration,
) -> Option<Instant> {
    created_at.checked_add(configured_lifetime.min(share_link_expires_at.saturating_sub(now)))
}

pub(super) async fn preview_request<S: StorageClient>(
    state: &AppState<S>,
    share_link_id: &str,
    secret: &str,
    headers: &HeaderMap,
    uri: &Uri,
) -> Option<Response> {
    if is_whatsapp_preview_user_agent(headers) && !super::has_query_flag(uri.query(), "open") {
        Some(share_link_preview(state, share_link_id, secret, headers, uri).await)
    } else {
        None
    }
}


pub(crate) fn cleanup_expired<S: StorageClient>(state: &AppState<S>) {
    state.preview_image_authorizations.remove_expired(Instant::now());
}

#[cfg(test)]
mod tests {
    use std::{
        net::{IpAddr, Ipv4Addr, SocketAddr},
        path::Path,
        sync::{Arc, Mutex},
        time::{Duration, Instant, SystemTime, UNIX_EPOCH},
    };

    use axum::{
        body::{to_bytes, Body},
        http::{header, Request, StatusCode},
        response::Response,
    };
    use bytes::Bytes;
    use futures_util::stream;
    use rusqlite::Connection;
    use tower::ServiceExt;

    use crate::{
        auth,
        db::{AccountRole, Database, ImportedAccount, NewBlock, NewPost, ShareAccess},
        storage::{ImageReductionOptions, StorageBody, StorageClient, StorageResponse},
        web::{self, router},
    };

    #[derive(Clone)]
    struct PreviewStorage {
        requests: Arc<Mutex<Vec<(String, bool, ImageReductionOptions)>>>,
        bytes: Bytes,
        declared_length: Option<u64>,
    }

    impl PreviewStorage {
        fn new(bytes: Bytes, declared_length: Option<u64>) -> Self {
            Self {
                requests: Arc::new(Mutex::new(Vec::new())),
                bytes,
                declared_length,
            }
        }
    }

    impl StorageClient for PreviewStorage {
        async fn put_file(&self, _content_type: &str, _path: &Path) -> Result<String, String> {
            unreachable!()
        }

        async fn put_stream(
            &self,
            _content_type: &str,
            _content_length: Option<u64>,
            _body: StorageBody,
            _max_bytes: u64,
        ) -> Result<(String, u64), String> {
            unreachable!()
        }

        async fn get(
            &self,
            _key: &str,
            _range: Option<&str>,
            _head: bool,
            _thumbnail: bool,
        ) -> Result<StorageResponse, String> {
            unreachable!()
        }

        async fn get_reduced_image(
            &self,
            key: &str,
            options: ImageReductionOptions,
            video_thumbnail: bool,
            head: bool,
        ) -> Result<StorageResponse, String> {
            self.requests.lock().unwrap().push((key.to_owned(), video_thumbnail, options));
            let mut headers = http::HeaderMap::new();
            headers.insert(header::CONTENT_TYPE, "image/jpeg".parse().unwrap());
            if let Some(length) = self.declared_length {
                headers.insert(header::CONTENT_LENGTH, length.to_string().parse().unwrap());
            }
            let body: StorageBody = if head {
                Box::pin(stream::empty())
            } else {
                Box::pin(stream::iter([Ok(self.bytes.clone())]))
            };
            Ok(StorageResponse {
                status: StatusCode::OK,
                headers,
                body,
            })
        }
    }

    async fn get_route(app: &axum::Router, target: &str) -> Response {
        app.clone()
            .oneshot(Request::builder().uri(target).body(Body::empty()).unwrap())
            .await
            .unwrap()
    }

    async fn get_route_with_user_agent(app: &axum::Router, target: &str, user_agent: &str) -> Response {
        app.clone()
            .oneshot(
                Request::builder()
                    .uri(target)
                    .header(header::USER_AGENT, user_agent)
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap()
    }


    #[test]
    fn preview_image_authorizations_expire_and_cleanup_at_the_deadline() {
        let now = Instant::now();
        let authorizations = super::PreviewImageAuthorizations::default();
        let clone = authorizations.clone();
        let deadline = now + Duration::from_secs(10);
        authorizations.insert("link".to_owned(), "digest".to_owned(), 42, deadline);

        assert_eq!(clone.block_id("link", "digest", now), Some(42));
        assert_eq!(clone.expires_at("link", "digest"), Some(deadline));
        assert_eq!(clone.block_id("link", "digest", deadline), None);
        assert_eq!(authorizations.len(), 0);

        authorizations.insert("link".to_owned(), "other-digest".to_owned(), 43, deadline);
        assert_eq!(authorizations.remove_expired(deadline - Duration::from_nanos(1)), 0);
        assert_eq!(clone.remove_expired(deadline), 1);
        assert_eq!(authorizations.len(), 0);
    }

    #[test]
    fn preview_image_ttl_defaults_and_is_configurable_but_must_be_positive() {
        assert_eq!(crate::config::SiteSettings::default().whatsapp_preview_image_ttl_seconds, 10);
        let config: crate::config::AppConfig = toml::from_str(
            "[site]\nwhatsapp_preview_image_ttl_seconds = 23\n",
        ).unwrap();
        assert_eq!(config.site.whatsapp_preview_image_ttl_seconds, 23);

        let bind_address = SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), 3000);
        let configured = web::SiteSecurity::from_config(
            bind_address,
            Some("https://journey.example"),
            3600,
            web::DEFAULT_MAX_MEDIA_UPLOAD_BYTES,
            config.site.whatsapp_preview_image_ttl_seconds,
            false,
        ).unwrap();
        assert_eq!(configured.whatsapp_preview.image_lifetime, Duration::from_secs(23));
        assert!(web::SiteSecurity::from_config(
            bind_address,
            Some("https://journey.example"),
            3600,
            web::DEFAULT_MAX_MEDIA_UPLOAD_BYTES,
            0,
            false,
        ).is_err());
    }

    #[test]
    fn preview_image_lifetime_is_capped_by_the_share_link_expiry() {
        let now = Instant::now();
        assert_eq!(
            super::preview_image_expiry(
                now,
                Duration::from_secs(100),
                Duration::from_secs(110),
                Duration::from_secs(10),
            ),
            Some(now + Duration::from_secs(10)),
        );
        assert_eq!(
            super::preview_image_expiry(
                now,
                Duration::from_secs(100),
                Duration::from_secs(103),
                Duration::from_secs(10),
            ),
            Some(now + Duration::from_secs(3)),
        );
        assert_eq!(
            super::preview_image_expiry(
                now,
                Duration::from_secs(100),
                Duration::from_secs(99),
                Duration::from_secs(10),
            ),
            Some(now),
        );
    }


    #[test]
    fn whatsapp_preview_user_agent_accepts_the_observed_version_with_or_without_client_marker() {
        for marker in ["A", "I", "N"] {
            let mut headers = http::HeaderMap::new();
            headers.insert(header::USER_AGENT, format!("WhatsApp/2.24.1.78 {marker}").parse().unwrap());
            assert!(super::is_whatsapp_preview_user_agent(&headers));
        }
        let mut observed_headers = http::HeaderMap::new();
        observed_headers.insert(header::USER_AGENT, "WhatsApp/2.23.20.0".parse().unwrap());
        assert!(super::is_whatsapp_preview_user_agent(&observed_headers));
        for user_agent in [
            "WhatsApp/2.24.1 A",
            "WhatsApp/3.24.1.78 A",
            "WhatsApp/2.24.1.78 X",
            "WhatsApp/2.23.20.0 X",
            "WhatsApp/2.x.1.78 A",
            "Mozilla/5.0",
        ] {
            let mut headers = http::HeaderMap::new();
            headers.insert(header::USER_AGENT, user_agent.parse().unwrap());
            assert!(!super::is_whatsapp_preview_user_agent(&headers));
        }
    }


    #[tokio::test]
    async fn whatsapp_share_previews_issue_revocable_bounded_image_capabilities() {
        fn media(key: &str, content_type: &str) -> NewBlock {
            NewBlock {
                id: None,
                header: None,
                body: None,
                storage_key: Some(key.to_owned()),
                content_type: Some(content_type.to_owned()),
                alt: None,
                children: Vec::new(),
            }
        }

        fn gallery(children: Vec<NewBlock>) -> NewBlock {
            NewBlock {
                id: None,
                header: Some("Gallery".to_owned()),
                body: None,
                storage_key: None,
                content_type: None,
                alt: None,
                children,
            }
        }

        fn post(title: &str, summary: &str, blocks: Vec<NewBlock>) -> NewPost {
            NewPost {
                author_username: "writer".to_owned(),
                title: title.to_owned(),
                published_at: Some(1_767_225_600),
                summary: summary.to_owned(),
                tags: Vec::new(),
                blocks,
            }
        }

        async fn create_link(database: &Database, post_id: i64, created_at: Duration) -> (String, String) {
            let id = auth::new_share_link_id();
            let secret = auth::new_share_link_secret();
            database.create_share_link(
                id.clone(),
                post_id,
                auth::session_token_digest(&secret),
                created_at,
                ShareAccess::Author("writer".to_owned()),
            ).await.unwrap().unwrap();
            (id, secret)
        }

        fn image_url(html: &str) -> String {
            let marker = "property=\"og:image\" content=\"";
            let start = html.find(marker).unwrap() + marker.len();
            let end = html[start..].find('"').unwrap() + start;
            html[start..end].to_owned()
        }

        fn image_token(path: &str) -> &str {
            path.rsplit('/').next().unwrap().strip_suffix(".jpg").unwrap()
        }

        let nonce = SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_nanos();
        let path = std::env::temp_dir().join(format!("journey-site-whatsapp-preview-{}-{nonce}.sqlite3", std::process::id()));
        let database = Database::new(path.clone());
        database.replace_posts_and_add_accounts(
            vec![
                post(
                    "A <title> & \"quote\"",
                    "Summary <b> & \"quote\"",
                    vec![gallery(vec![
                        media("media/first-photo", "image/jpeg"),
                        media("media/second-video", "video/mp4"),
                    ])],
                ),
                post(
                    "Video first",
                    "Video summary",
                    vec![gallery(vec![
                        media("media/first-video", "video/mp4"),
                        media("media/second-photo", "image/jpeg"),
                    ])],
                ),
                post("Text only", "No image here", Vec::new()),
            ],
            vec![ImportedAccount {
                username: "writer".to_owned(),
                role: AccountRole::Write,
                password_hash: "unused-test-hash".to_owned(),
            }],
        ).await.unwrap();
        let storage = PreviewStorage::new(Bytes::from_static(b"jpeg"), Some(4));
        let security = web::SiteSecurity::from_config(
            SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), 3000),
            Some("https://journey.example"),
            3600,
            web::DEFAULT_MAX_MEDIA_UPLOAD_BYTES,
            10,
            false,
        ).unwrap();
        let state = web::state_with_security(database.clone(), storage.clone(), security.clone());
        let app = router(state.clone());
        let now = web::unix_time();
        let (photo_link_id, photo_secret) = create_link(&database, 1, now).await;
        let (video_link_id, video_secret) = create_link(&database, 2, now).await;
        let (text_link_id, text_secret) = create_link(&database, 3, now).await;
        let photo_target = format!("/share/{photo_link_id}/{photo_secret}");
        let video_target = format!("/share/{video_link_id}/{video_secret}");
        let text_target = format!("/share/{text_link_id}/{text_secret}");

        assert_eq!(
            get_route_with_user_agent(&app, &format!("/share/{photo_link_id}/{}", auth::new_share_link_secret()), "WhatsApp/2.23.20.0").await.status(),
            StatusCode::NOT_FOUND,
        );
        let (expired_link_id, expired_secret) = create_link(
            &database,
            1,
            now.saturating_sub(Duration::from_secs(90_000)),
        ).await;
        assert_eq!(
            get_route_with_user_agent(&app, &format!("/share/{expired_link_id}/{expired_secret}"), "WhatsApp/2.23.20.0").await.status(),
            StatusCode::NOT_FOUND,
        );

        const WHATSAPP_USER_AGENT: &str = "WhatsApp/2.23.20.0";
        let photo_preview = get_route_with_user_agent(&app, &photo_target, WHATSAPP_USER_AGENT).await;
        assert_eq!(photo_preview.status(), StatusCode::OK);
        assert_eq!(photo_preview.headers().get(header::CACHE_CONTROL).unwrap(), "private, no-store");
        assert!(photo_preview.headers().get(header::SET_COOKIE).is_none());
        let body = to_bytes(photo_preview.into_body(), 300 * 1024).await.unwrap();
        let photo_html = String::from_utf8(body.to_vec()).unwrap();
        assert!(photo_html.contains("property=\"og:url\" content=\"https://journey.example/share/"));
        assert!(photo_html.contains("property=\"og:title\" content=\"A &lt;title&gt; &amp; &quot;quote&quot;\""));
        assert!(photo_html.contains("property=\"og:description\" content=\"Summary &lt;b&gt; &amp; &quot;quote&quot;\""));
        assert!(!photo_html.contains("property=\"og:image:width\""));
        assert!(!photo_html.contains("property=\"og:image:height\""));
        assert!(photo_html.contains("Open post"));
        assert!(photo_html.contains("?open=1"));
        assert!(photo_html.contains("window.location.replace(document.getElementById('open-post').href)"));
        let photo_image_url = image_url(&photo_html);
        assert!(photo_image_url.starts_with("https://journey.example/share/"));
        let photo_image_path = photo_image_url.strip_prefix("https://journey.example").unwrap();
        let photo_image = get_route(&app, photo_image_path).await;
        assert_eq!(photo_image.status(), StatusCode::OK);
        assert_eq!(photo_image.headers().get(header::CONTENT_TYPE).unwrap(), "image/jpeg");
        assert_eq!(photo_image.headers().get(header::CACHE_CONTROL).unwrap(), "private, no-store");
        assert!(photo_image.headers().get(header::SET_COOKIE).is_none());
        assert_eq!(to_bytes(photo_image.into_body(), usize::MAX).await.unwrap(), Bytes::from_static(b"jpeg"));
        let restarted_state = web::state_with_security(database.clone(), storage.clone(), security.clone());
        assert_eq!(restarted_state.preview_image_authorizations.len(), 0);
        let restarted_app = router(restarted_state);
        assert_eq!(get_route(&restarted_app, photo_image_path).await.status(), StatusCode::NOT_FOUND);
        let requests = storage.requests.lock().unwrap().clone();
        assert_eq!(requests[0].0, "media/first-photo");
        assert!(!requests[0].1);
        assert_eq!(requests[0].2, ImageReductionOptions::new(
            1200,
            Some(599_999),
        ).unwrap());
        let photo_digest = auth::session_token_digest(image_token(photo_image_path));
        let photo_deadline = state.preview_image_authorizations
            .expires_at(&photo_link_id, &photo_digest)
            .unwrap();
        assert!(photo_deadline > Instant::now());
        assert!(photo_deadline.duration_since(Instant::now()) <= Duration::from_secs(10));
        assert_eq!(state.preview_image_authorizations.len(), 1);
        assert_eq!(
            get_route(&app, &format!("/share/{video_link_id}/whatsapp-preview-image/{}.jpg", image_token(photo_image_path))).await.status(),
            StatusCode::NOT_FOUND,
        );
        assert_eq!(
            get_route(&app, &format!("/share/{photo_link_id}/whatsapp-preview-image/{}.jpg", auth::new_session_token())).await.status(),
            StatusCode::NOT_FOUND,
        );
        let retry = get_route(&app, photo_image_path).await;
        assert_eq!(retry.status(), StatusCode::OK);
        let _ = to_bytes(retry.into_body(), usize::MAX).await.unwrap();
        assert_eq!(state.preview_image_authorizations.expires_at(&photo_link_id, &photo_digest), Some(photo_deadline));
        state.preview_image_authorizations.set_expiry(&photo_link_id, &photo_digest, Instant::now());
        assert_eq!(get_route(&app, photo_image_path).await.status(), StatusCode::NOT_FOUND);
        assert_eq!(state.preview_image_authorizations.len(), 0);

        let repeated_preview = get_route_with_user_agent(&app, &photo_target, WHATSAPP_USER_AGENT).await;
        let repeated_body = to_bytes(repeated_preview.into_body(), 300 * 1024).await.unwrap();
        let repeated_html = String::from_utf8(repeated_body.to_vec()).unwrap();
        let repeated_image_path = image_url(&repeated_html).strip_prefix("https://journey.example").unwrap().to_owned();
        assert_ne!(repeated_image_path, photo_image_path);
        assert_eq!(state.preview_image_authorizations.len(), 1);

        let (short_link_id, short_secret) = create_link(
            &database,
            1,
            now.saturating_sub(Duration::from_secs(86_340)),
        ).await;
        let short_target = format!("/share/{short_link_id}/{short_secret}");
        let short_preview = get_route_with_user_agent(&app, &short_target, WHATSAPP_USER_AGENT).await;
        assert_eq!(short_preview.status(), StatusCode::OK);
        let short_body = to_bytes(short_preview.into_body(), 300 * 1024).await.unwrap();
        let short_html = String::from_utf8(short_body.to_vec()).unwrap();
        let short_image_path = image_url(&short_html).strip_prefix("https://journey.example").unwrap().to_owned();
        let short_digest = auth::session_token_digest(image_token(&short_image_path));
        let short_deadline = state.preview_image_authorizations
            .expires_at(&short_link_id, &short_digest)
            .unwrap();
        assert!(short_deadline.duration_since(Instant::now()) <= Duration::from_secs(10));
        assert!(image_url(&short_html).starts_with("https://journey.example/share/"));
        let connection = Connection::open(&path).unwrap();
        connection.execute(
            "UPDATE share_links SET expires_at = ?2 WHERE id = ?1",
            rusqlite::params![short_link_id, web::unix_time().as_secs() as i64 - 1],
        ).unwrap();
        drop(connection);
        assert_eq!(get_route(&app, &short_image_path).await.status(), StatusCode::NOT_FOUND);

        let video_preview = get_route_with_user_agent(&app, &video_target, WHATSAPP_USER_AGENT).await;
        assert_eq!(video_preview.status(), StatusCode::OK);
        assert!(video_preview.headers().get(header::SET_COOKIE).is_none());
        let body = to_bytes(video_preview.into_body(), 300 * 1024).await.unwrap();
        let video_html = String::from_utf8(body.to_vec()).unwrap();
        let video_image_path = image_url(&video_html).strip_prefix("https://journey.example").unwrap().to_owned();
        let video_image = get_route(&app, &video_image_path).await;
        assert_eq!(video_image.status(), StatusCode::OK);
        let _ = to_bytes(video_image.into_body(), usize::MAX).await.unwrap();
        let requests = storage.requests.lock().unwrap().clone();
        let video_request = requests.iter().find(|request| request.0 == "media/first-video").unwrap();
        assert!(video_request.1);
        let connection = Connection::open(&path).unwrap();
        connection.execute("DELETE FROM post_blocks WHERE storage_key = 'media/first-video'", []).unwrap();
        drop(connection);
        assert_eq!(get_route(&app, &video_image_path).await.status(), StatusCode::NOT_FOUND);
        assert!(database.revoke_share_link(
            video_link_id,
            web::unix_time(),
            ShareAccess::Author("writer".to_owned()),
        ).await.unwrap());
        assert_eq!(get_route(&app, &video_image_path).await.status(), StatusCode::NOT_FOUND);

        let (oversized_link_id, oversized_secret) = create_link(&database, 1, web::unix_time()).await;
        let oversized_target = format!("/share/{oversized_link_id}/{oversized_secret}");
        let oversized_preview = get_route_with_user_agent(&app, &oversized_target, WHATSAPP_USER_AGENT).await;
        let oversized_body = to_bytes(oversized_preview.into_body(), 300 * 1024).await.unwrap();
        let oversized_html = String::from_utf8(oversized_body.to_vec()).unwrap();
        let oversized_image_path = image_url(&oversized_html).strip_prefix("https://journey.example").unwrap().to_owned();
        let oversized_storage = PreviewStorage::new(Bytes::from_static(b"jpeg"), Some(600_000));
        let mut oversized_state = state.clone();
        oversized_state.storage = oversized_storage;
        let oversized_app = router(oversized_state);
        let oversized_response = get_route(&oversized_app, &oversized_image_path).await;
        assert_eq!(oversized_response.status(), StatusCode::BAD_GATEWAY);
        assert_eq!(oversized_response.headers().get(header::CACHE_CONTROL).unwrap(), "private, no-store");

        let streamed_oversized_storage = PreviewStorage::new(
            Bytes::from(vec![b'x'; 600_000]),
            Some(1),
        );
        let mut streamed_oversized_state = state.clone();
        streamed_oversized_state.storage = streamed_oversized_storage;
        let streamed_oversized_app = router(streamed_oversized_state);
        let streamed_oversized_response = get_route(&streamed_oversized_app, &oversized_image_path).await;
        assert_eq!(streamed_oversized_response.status(), StatusCode::OK);
        assert!(to_bytes(streamed_oversized_response.into_body(), 700_000).await.is_err());

        let text_preview = get_route_with_user_agent(&app, &text_target, WHATSAPP_USER_AGENT).await;
        assert_eq!(text_preview.status(), StatusCode::OK);
        let body = to_bytes(text_preview.into_body(), 300 * 1024).await.unwrap();
        let text_html = String::from_utf8(body.to_vec()).unwrap();
        assert!(!text_html.contains("property=\"og:image\""));

        let browser = get_route_with_user_agent(&app, &photo_target, "Mozilla/5.0").await;
        assert_eq!(browser.status(), StatusCode::SEE_OTHER);
        assert_eq!(browser.headers().get(header::LOCATION).unwrap().to_str().unwrap(), format!("/share/{photo_link_id}/posts/1"));
        assert!(browser.headers().get(header::SET_COOKIE).is_some());
        let browser_from_whatsapp = get_route_with_user_agent(
            &app,
            &format!("{photo_target}?open=1"),
            WHATSAPP_USER_AGENT,
        ).await;
        assert_eq!(browser_from_whatsapp.status(), StatusCode::SEE_OTHER);
        assert!(browser_from_whatsapp.headers().get(header::SET_COOKIE).is_some());

        let (unpublished_link_id, unpublished_secret) = create_link(&database, 1, web::unix_time()).await;
        let unpublished_target = format!("/share/{unpublished_link_id}/{unpublished_secret}");
        let unpublished_preview = get_route_with_user_agent(&app, &unpublished_target, WHATSAPP_USER_AGENT).await;
        let unpublished_body = to_bytes(unpublished_preview.into_body(), 300 * 1024).await.unwrap();
        let unpublished_html = String::from_utf8(unpublished_body.to_vec()).unwrap();
        let unpublished_image_path = image_url(&unpublished_html).strip_prefix("https://journey.example").unwrap().to_owned();
        let connection = Connection::open(&path).unwrap();
        connection.execute("UPDATE posts SET published = 0 WHERE id = 1", []).unwrap();
        drop(connection);
        assert_eq!(get_route(&app, &unpublished_image_path).await.status(), StatusCode::NOT_FOUND);

        std::fs::remove_file(path).unwrap();
    }
}
