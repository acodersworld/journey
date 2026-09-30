use axum::{
    body::Body,
    extract::{ConnectInfo, Extension, Form, Path, Query, Request, State},
    http::{header, HeaderMap, HeaderValue, Method, StatusCode, Uri},
    middleware::{self, Next},
    response::{Html, IntoResponse, Response},
    routing::{delete, get, post},
    Json, Router,
};
use serde::{Deserialize, Serialize};
use std::{net::{IpAddr, SocketAddr}, time::{Duration, SystemTime, UNIX_EPOCH}};

use crate::{
    auth,
    db::{AccountRole, AuthenticatedAccount, Database, FeedCursor, MediaReference, NewBlock, Post, PostAccess, ShareAccess, SidebarData},
    storage::StorageClient,
};

const DEFAULT_FEED_LIMIT: usize = 10;
const MAX_FEED_LIMIT: usize = 100;

#[derive(Clone)]
pub struct AppState<S: StorageClient> {
    database: Database,
    storage: S,
    security: SiteSecurity,
}

#[derive(Clone)]
pub struct SiteSecurity {
    public_origin: Option<ParsedOrigin>,
    secure_cookie: bool,
    session_lifetime: Duration,
}

#[derive(Clone)]
struct ParsedOrigin {
    key: String,
    scheme: String,
    host: String,
}

#[derive(Clone)]
struct AuthPrincipal {
    username: String,
    role: AccountRole,
    timezone: String,
}

#[derive(Deserialize)]
struct LoginRequest {
    username: String,
    password: String,
}

#[derive(Deserialize)]
struct LoginForm {
    username: String,
    password: String,
    return_to: Option<String>,
}

#[derive(Deserialize)]
struct LoginPageQuery {
    return_to: Option<String>,
}

#[derive(Deserialize)]
struct CreatedPostQuery {
    created: Option<String>,
}

#[derive(Serialize)]
struct CurrentAccountResponse {
    username: String,
    role: AccountRole,
}

#[derive(Serialize)]
struct CreatedShareLinkResponse {
    id: String,
    url: String,
    expires_at_unix: i64,
}

#[derive(Deserialize)]
struct FeedQuery {
    limit: Option<usize>,
    after: Option<String>,
    tag: Option<String>,
    month: Option<String>,
}

#[derive(Deserialize)]
struct TagPageQuery {
    tag: String,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct CreateDraftRequest {
    title: String,
    #[serde(default)]
    summary: Option<String>,
    #[serde(default)]
    tags: Option<Vec<String>>,
    #[serde(default)]
    blocks: Vec<CreateDraftBlock>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct CreateDraftBlock {
    #[serde(default)]
    header: Option<String>,
    #[serde(default)]
    body: Option<String>,
    #[serde(default)]
    blocks: Vec<CreateDraftBlock>,
}

#[derive(Serialize)]
struct CreatedPostResponse {
    id: i64,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct PublishPostRequest {
    #[serde(default)]
    published_at: Option<i64>,
}

impl SiteSecurity {
    pub fn from_env(bind_address: SocketAddr, allow_insecure_cookies: bool) -> Result<Self, String> {
        if allow_insecure_cookies && !bind_address.ip().is_loopback() {
            return Err("--allow-insecure-cookies is only permitted when binding to loopback".to_owned());
        }
        let public_origin = match std::env::var("JOURNEY_SITE_PUBLIC_ORIGIN") {
            Ok(value) => {
                let origin = parse_origin(&value)
                    .ok_or_else(|| "JOURNEY_SITE_PUBLIC_ORIGIN must be an origin URL".to_owned())?;
                if origin.scheme == "http" && !origin.is_loopback() {
                    return Err("JOURNEY_SITE_PUBLIC_ORIGIN may use HTTP only for loopback hosts".to_owned());
                }
                Some(origin)
            }
            Err(std::env::VarError::NotPresent) if bind_address.ip().is_loopback() => None,
            Err(std::env::VarError::NotPresent) => {
                return Err("JOURNEY_SITE_PUBLIC_ORIGIN is required when binding outside loopback".to_owned());
            }
            Err(error) => return Err(format!("could not read JOURNEY_SITE_PUBLIC_ORIGIN: {error}")),
        };
        let session_lifetime_seconds = std::env::var("JOURNEY_SITE_SESSION_TTL_SECONDS")
            .unwrap_or_else(|_| "604800".to_owned())
            .parse::<i64>()
            .map_err(|_| "JOURNEY_SITE_SESSION_TTL_SECONDS must be a positive integer".to_owned())?;
        if session_lifetime_seconds <= 0 {
            return Err("JOURNEY_SITE_SESSION_TTL_SECONDS must be a positive integer".to_owned());
        }
        let secure_cookie = !allow_insecure_cookies;
        Ok(Self {
            public_origin,
            secure_cookie,
            session_lifetime: Duration::from_secs(session_lifetime_seconds as u64),
        })
    }
}

pub fn share_link_origin(bind_address: SocketAddr) -> Result<String, String> {
    match std::env::var("JOURNEY_SITE_PUBLIC_ORIGIN") {
        Ok(value) => {
            let origin = parse_origin(&value)
                .ok_or_else(|| "JOURNEY_SITE_PUBLIC_ORIGIN must be an origin URL".to_owned())?;
            if origin.scheme == "http" && !origin.is_loopback() {
                return Err("JOURNEY_SITE_PUBLIC_ORIGIN may use HTTP only for loopback hosts".to_owned());
            }
            Ok(origin.key)
        }
        Err(std::env::VarError::NotPresent) if bind_address.ip().is_loopback() => {
            Ok(format!("http://{bind_address}"))
        }
        Err(std::env::VarError::NotPresent) => {
            Err("JOURNEY_SITE_PUBLIC_ORIGIN is required to create a link when binding outside loopback".to_owned())
        }
        Err(error) => Err(format!("could not read JOURNEY_SITE_PUBLIC_ORIGIN: {error}")),
    }
}

pub fn router<S: StorageClient>(state: AppState<S>) -> Router {
    let content_routes = Router::new()
        .route("/", get(home::<S>))
        .route("/tags", get(tag_page::<S>))
        .route("/archive/{month}", get(archive_page::<S>))
        .route("/posts/new", get(new_post_page::<S>))
        .route("/api/posts", get(feed::<S>).post(create_draft::<S>))
        .route("/api/drafts", get(drafts::<S>))
        .route("/api/posts/{id}", get(api_full_post::<S>))
        .route("/api/posts/{id}/publish", post(publish_post::<S>))
        .route("/api/posts/{id}/share-links", post(create_share_link::<S>))
        .route("/api/share-links/{id}", delete(revoke_share_link::<S>))
        .route("/posts/{id}/fragment", get(post_fragment::<S>))
        .route("/posts/{id}/share-preview", get(share_preview::<S>))
        .route("/posts/{id}", get(post_page::<S>))
        .route(
            "/posts/{post_id}/blocks/{block_id}/media",
            get(media::<S>).head(media::<S>),
        )
        .route_layer(middleware::from_fn_with_state(state.clone(), authenticate::<S>));

    Router::new()
        .route("/login", get(login_page::<S>).post(login_form::<S>))
        .route("/logout", post(logout_form::<S>))
        .route("/site.css", get(site_css))
        .route("/site.js", get(site_js))
        .route("/api/auth/login", post(login::<S>))
        .route("/api/auth/current", get(current_account::<S>))
        .route("/api/auth/logout", post(logout::<S>))
        .route("/share/{share_link_id}/{secret}", get(open_share_link::<S>))
        .route("/share/{share_link_id}/posts/{post_id}", get(shared_post_page::<S>))
        .route(
            "/share/{share_link_id}/posts/{post_id}/blocks/{block_id}/media",
            get(shared_media::<S>).head(shared_media::<S>),
        )
        .merge(content_routes)
        .with_state(state)
}

async fn authenticate<S: StorageClient>(
    State(state): State<AppState<S>>,
    mut request: Request,
    next: Next,
) -> Response {
    let token = cookie_token(request.headers());
    let Some(token) = token else {
        return unauthenticated_response(&request);
    };
    let account = match lookup_session(&state, &token).await {
        Ok(Some(account)) => account,
        Ok(None) => return unauthenticated_response(&request),
        Err(error) => {
            eprintln!("website session lookup failed: {error}");
            return no_store(StatusCode::INTERNAL_SERVER_ERROR.into_response());
        }
    };
    let mut principal = AuthPrincipal::from(account);
    principal.timezone = timezone_from_cookie(request.headers());
    request.extensions_mut().insert(principal);
    no_store(next.run(request).await)
}

async fn open_share_link<S: StorageClient>(
    State(state): State<AppState<S>>,
    Path((share_link_id, secret)): Path<(String, String)>,
) -> Response {
    let now = unix_time();
    let session_token = auth::new_session_token();
    match state
        .database
        .issue_share_session(
            share_link_id.clone(),
            auth::session_token_digest(&secret),
            auth::session_token_digest(&session_token),
            now,
        )
        .await
    {
        Ok(Some((post_id, expires_at))) => {
            let mut response = redirect_to(&format!("/share/{share_link_id}/posts/{post_id}"));
            set_share_cookie(
                &mut response,
                &state.security,
                &share_link_id,
                &session_token,
                expires_at.saturating_sub(unix_time()),
            );
            no_store(response)
        }
        Ok(None) => share_not_found(),
        Err(error) => {
            eprintln!("website share-link session creation failed: {error}");
            no_store(StatusCode::INTERNAL_SERVER_ERROR.into_response())
        }
    }
}

async fn shared_post_page<S: StorageClient>(
    State(state): State<AppState<S>>,
    Path((share_link_id, post_id)): Path<(String, i64)>,
    headers: HeaderMap,
) -> Response {
    let Some(token) = share_cookie_token(&headers, &share_link_id) else {
        return share_not_found();
    };
    match state
        .database
        .shared_post(
            share_link_id.clone(),
            auth::session_token_digest(&token),
            post_id,
            unix_time(),
        )
        .await
    {
        Ok(Some(post)) => {
            let prefix = format!("/share/{share_link_id}");
            no_store(Html(render_shared_post(&post, &prefix)).into_response())
        }
        Ok(None) => share_not_found(),
        Err(error) => {
            eprintln!("website shared-post lookup failed: {error}");
            no_store(StatusCode::INTERNAL_SERVER_ERROR.into_response())
        }
    }
}

async fn shared_media<S: StorageClient>(
    State(state): State<AppState<S>>,
    Path((share_link_id, post_id, block_id)): Path<(String, i64, i64)>,
    method: Method,
    headers: HeaderMap,
) -> Response {
    let Some(token) = share_cookie_token(&headers, &share_link_id) else {
        return share_not_found();
    };
    let media = match state
        .database
        .shared_media_reference(
            share_link_id,
            auth::session_token_digest(&token),
            post_id,
            block_id,
            unix_time(),
        )
        .await
    {
        Ok(Some(media)) => media,
        Ok(None) => return share_not_found(),
        Err(error) => {
            eprintln!("website shared-media lookup failed: {error}");
            return no_store(StatusCode::INTERNAL_SERVER_ERROR.into_response());
        }
    };
    no_store(proxy_media(&state.storage, media, method, headers).await)
}

async fn share_preview<S: StorageClient>(
    State(state): State<AppState<S>>,
    Path(id): Path<i64>,
    Extension(principal): Extension<AuthPrincipal>,
) -> Response {
    let Some(access) = principal.share_access() else {
        return no_store(StatusCode::FORBIDDEN.into_response());
    };
    match state.database.post_for_share_preview(id, access).await {
        Ok(Some(post)) if post.published => {
            no_store(Html(render_shared_post(&post, "")).into_response())
        }
        Ok(_) => no_store(StatusCode::NOT_FOUND.into_response()),
        Err(error) => {
            eprintln!("website share preview lookup failed: {error}");
            no_store(StatusCode::INTERNAL_SERVER_ERROR.into_response())
        }
    }
}

async fn create_share_link<S: StorageClient>(
    State(state): State<AppState<S>>,
    Path(post_id): Path<i64>,
    Extension(principal): Extension<AuthPrincipal>,
    headers: HeaderMap,
) -> Response {
    let Some(origin) = validated_request_origin(&headers, &state.security) else {
        return no_store((StatusCode::FORBIDDEN, "origin not allowed\n").into_response());
    };
    let Some(access) = principal.share_access() else {
        return no_store(StatusCode::FORBIDDEN.into_response());
    };

    let link_id = auth::new_share_link_id();
    let secret = auth::new_share_link_secret();
    match state
        .database
        .create_share_link(
            link_id,
            post_id,
            auth::session_token_digest(&secret),
            unix_time(),
            access,
        )
        .await
    {
        Ok(Some(link)) => no_store(
            Json(CreatedShareLinkResponse {
                url: format!("{}/share/{}/{}", origin.key, link.id, secret),
                id: link.id,
                expires_at_unix: link.expires_at_unix,
            })
            .into_response(),
        ),
        Ok(None) => no_store(StatusCode::NOT_FOUND.into_response()),
        Err(error) => {
            eprintln!("website share-link creation failed: {error}");
            no_store(StatusCode::INTERNAL_SERVER_ERROR.into_response())
        }
    }
}

async fn revoke_share_link<S: StorageClient>(
    State(state): State<AppState<S>>,
    Path(id): Path<String>,
    Extension(principal): Extension<AuthPrincipal>,
    headers: HeaderMap,
) -> Response {
    if !origin_allowed(&headers, &state.security) {
        return no_store((StatusCode::FORBIDDEN, "origin not allowed\n").into_response());
    }
    let Some(access) = principal.share_access() else {
        return no_store(StatusCode::FORBIDDEN.into_response());
    };
    if !valid_share_link_id(&id) {
        return no_store(StatusCode::NOT_FOUND.into_response());
    }
    match state.database.revoke_share_link(id, unix_time(), access).await {
        Ok(true) => no_store(StatusCode::NO_CONTENT.into_response()),
        Ok(false) => no_store(StatusCode::NOT_FOUND.into_response()),
        Err(error) => {
            eprintln!("website share-link revocation failed: {error}");
            no_store(StatusCode::INTERNAL_SERVER_ERROR.into_response())
        }
    }
}

fn valid_share_link_id(id: &str) -> bool {
    id.len() == 22
        && id
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'-'))
}

fn share_not_found() -> Response {
    no_store((StatusCode::NOT_FOUND, "share link not found\n").into_response())
}

async fn login<S: StorageClient>(
    State(state): State<AppState<S>>,
    ConnectInfo(peer_address): ConnectInfo<SocketAddr>,
    headers: HeaderMap,
    Json(credentials): Json<LoginRequest>,
) -> Response {
    let (account, token) = match authenticate_and_issue_session(
        &state,
        &headers,
        peer_address,
        credentials.username,
        credentials.password,
    )
    .await
    {
        Ok(result) => result,
        Err(response) => return response,
    };

    let mut response = Json(CurrentAccountResponse {
        username: account.username,
        role: account.role,
    })
    .into_response();
    set_session_cookie(&mut response, &state.security, &token);
    no_store(response)
}

async fn login_page<S: StorageClient>(
    State(state): State<AppState<S>>,
    query: Result<Query<LoginPageQuery>, axum::extract::rejection::QueryRejection>,
    headers: HeaderMap,
) -> Response {
    let return_to = query
        .ok()
        .and_then(|Query(query)| query.return_to)
        .as_deref()
        .and_then(valid_return_target)
        .unwrap_or_else(|| "/".to_owned());
    if let Some(token) = cookie_token(&headers) {
        match lookup_session(&state, &token).await {
            Ok(Some(_)) => return redirect_to(&return_to),
            Ok(None) => {}
            Err(error) => {
                eprintln!("website login-page session lookup failed: {error}");
                return no_store(StatusCode::INTERNAL_SERVER_ERROR.into_response());
            }
        }
    }
    no_store(Html(render_login_page(&return_to, "", None)).into_response())
}

async fn login_form<S: StorageClient>(
    State(state): State<AppState<S>>,
    ConnectInfo(peer_address): ConnectInfo<SocketAddr>,
    headers: HeaderMap,
    Form(form): Form<LoginForm>,
) -> Response {
    let return_to = form
        .return_to
        .as_deref()
        .and_then(valid_return_target)
        .unwrap_or_else(|| "/".to_owned());
    let username = form.username.clone();
    let (_account, token) = match authenticate_and_issue_session(
        &state,
        &headers,
        peer_address,
        form.username,
        form.password,
    )
    .await
    {
        Ok(result) => result,
        Err(response) if response.status() == StatusCode::UNAUTHORIZED => {
            return no_store((
                StatusCode::UNAUTHORIZED,
                Html(render_login_page(
                    &return_to,
                    &username,
                    Some("Invalid username or password."),
                )),
            )
                .into_response());
        }
        Err(response) => return response,
    };

    let mut response = redirect_to(&return_to);
    set_session_cookie(&mut response, &state.security, &token);
    no_store(response)
}

async fn authenticate_and_issue_session<S: StorageClient>(
    state: &AppState<S>,
    headers: &HeaderMap,
    peer_address: SocketAddr,
    username: String,
    password: String,
) -> Result<(crate::db::LoginAccount, String), Response> {
    if !origin_allowed(headers, &state.security) {
        return Err(no_store(
            (StatusCode::FORBIDDEN, "origin not allowed\n").into_response(),
        ));
    }
    if username.len() > 64 || password.len() > 1024 {
        return Err(login_failure());
    }
    let username_throttle_key = auth::username_throttle_key(&username);
    let address_throttle_key = auth::address_throttle_key(&peer_address.ip().to_string());
    let now = unix_time();
    for (key, maximum_attempts) in [
        (username_throttle_key.clone(), 5),
        (address_throttle_key.clone(), 30),
    ] {
        match state
            .database
            .record_login_attempt(key, now, Duration::from_secs(15 * 60), maximum_attempts)
            .await
        {
            Ok(true) => {}
            Ok(false) => return Err(login_failure()),
            Err(error) => {
                eprintln!("website login throttle failed: {error}");
                return Err(no_store(StatusCode::INTERNAL_SERVER_ERROR.into_response()));
            }
        }
    }

    let account = match state.database.login_account(username.clone()).await {
        Ok(account) => account,
        Err(error) => {
            eprintln!("website account lookup failed: {error}");
            return Err(no_store(StatusCode::INTERNAL_SERVER_ERROR.into_response()));
        }
    };
    let valid_username = auth::validate_username(&username).is_ok();
    let authenticated = match account.as_ref() {
        Some(account) => {
            let password_matches = auth::verify_password(&password, &account.password_hash);
            valid_username && account.enabled && password_matches
        }
        None => {
            auth::dummy_verify_password(&password);
            false
        }
    };
    if !authenticated {
        return Err(login_failure());
    }
    let account = account.expect("successful authentication has an account");
    for key in [username_throttle_key, address_throttle_key] {
        if let Err(error) = state.database.clear_login_attempts(key).await {
            eprintln!("website login throttle reset failed: {error}");
            return Err(no_store(StatusCode::INTERNAL_SERVER_ERROR.into_response()));
        }
    }

    let token = auth::new_session_token();
    let created_at = unix_time();
    let expires_at = created_at.saturating_add(state.security.session_lifetime);
    if let Err(error) = state
        .database
        .issue_session(
            account.id,
            auth::session_token_digest(&token),
            created_at,
            expires_at,
        )
        .await
    {
        eprintln!("website session creation failed: {error}");
        return Err(no_store(StatusCode::INTERNAL_SERVER_ERROR.into_response()));
    }
    Ok((account, token))
}

async fn current_account<S: StorageClient>(
    State(state): State<AppState<S>>,
    headers: HeaderMap,
) -> Response {
    let Some(token) = cookie_token(&headers) else {
        return no_store(StatusCode::UNAUTHORIZED.into_response());
    };
    match lookup_session(&state, &token).await {
        Ok(Some(account)) => no_store(Json(CurrentAccountResponse {
            username: account.username,
            role: account.role,
        }).into_response()),
        Ok(None) => no_store(StatusCode::UNAUTHORIZED.into_response()),
        Err(error) => {
            eprintln!("website current-account lookup failed: {error}");
            no_store(StatusCode::INTERNAL_SERVER_ERROR.into_response())
        }
    }
}

async fn logout<S: StorageClient>(
    State(state): State<AppState<S>>,
    headers: HeaderMap,
) -> Response {
    if let Err(response) = revoke_current_session(&state, &headers).await {
        return response;
    }
    let mut response = StatusCode::NO_CONTENT.into_response();
    expire_session_cookie(&mut response, &state.security);
    no_store(response)
}

async fn logout_form<S: StorageClient>(
    State(state): State<AppState<S>>,
    headers: HeaderMap,
) -> Response {
    if let Err(response) = revoke_current_session(&state, &headers).await {
        return response;
    }
    let mut response = redirect_to("/login");
    expire_session_cookie(&mut response, &state.security);
    no_store(response)
}

async fn revoke_current_session<S: StorageClient>(
    state: &AppState<S>,
    headers: &HeaderMap,
) -> Result<(), Response> {
    if !origin_allowed(headers, &state.security) {
        return Err(no_store(
            (StatusCode::FORBIDDEN, "origin not allowed\n").into_response(),
        ));
    }
    if let Some(token) = cookie_token(headers) {
        if let Err(error) = state
            .database
            .revoke_session(auth::session_token_digest(&token))
            .await
        {
            eprintln!("website logout failed: {error}");
            return Err(no_store(StatusCode::INTERNAL_SERVER_ERROR.into_response()));
        }
    }
    Ok(())
}

async fn lookup_session<S: StorageClient>(
    state: &AppState<S>,
    token: &str,
) -> Result<Option<AuthenticatedAccount>, String> {
    state
        .database
        .authenticated_account(auth::session_token_digest(token), unix_time())
        .await
}

fn login_failure() -> Response {
    no_store((StatusCode::UNAUTHORIZED, "invalid username or password\n").into_response())
}

fn cookie_token(headers: &HeaderMap) -> Option<String> {
    for cookie_header in headers.get_all(header::COOKIE) {
        let Ok(cookie_header) = cookie_header.to_str() else {
            continue;
        };
        for cookie in cookie_header.split(';').map(str::trim) {
            let Some(token) = cookie.strip_prefix("journey_session=") else {
                continue;
            };
            if token.len() == 43
                && token.bytes().all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'-'))
            {
                return Some(token.to_owned());
            }
        }
    }
    None
}

fn share_cookie_token(headers: &HeaderMap, share_link_id: &str) -> Option<String> {
    let cookie_name = format!("journey_share_{share_link_id}=");
    for cookie_header in headers.get_all(header::COOKIE) {
        let Ok(cookie_header) = cookie_header.to_str() else {
            continue;
        };
        for cookie in cookie_header.split(';').map(str::trim) {
            let Some(token) = cookie.strip_prefix(&cookie_name) else {
                continue;
            };
            if valid_token(token) {
                return Some(token.to_owned());
            }
        }
    }
    None
}

fn valid_token(token: &str) -> bool {
    token.len() == 43
        && token.bytes().all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'-'))
}

fn set_session_cookie(response: &mut Response, security: &SiteSecurity, token: &str) {
    let secure = if security.secure_cookie { "; Secure" } else { "" };
    let value = format!(
        "journey_session={token}; Path=/; HttpOnly; SameSite=Strict; Max-Age={}{secure}",
        security.session_lifetime.as_secs(),
    );
    if let Ok(value) = HeaderValue::from_str(&value) {
        response.headers_mut().insert(header::SET_COOKIE, value);
    }
}

fn set_share_cookie(
    response: &mut Response,
    security: &SiteSecurity,
    share_link_id: &str,
    token: &str,
    max_age: Duration,
) {
    let secure = if security.secure_cookie { "; Secure" } else { "" };
    let value = format!(
        "journey_share_{share_link_id}={token}; Path=/share/{share_link_id}; HttpOnly; SameSite=Lax; Max-Age={}{secure}",
        max_age.as_secs(),
    );
    if let Ok(value) = HeaderValue::from_str(&value) {
        response.headers_mut().append(header::SET_COOKIE, value);
    }
}

fn expire_session_cookie(response: &mut Response, security: &SiteSecurity) {
    let secure = if security.secure_cookie { "; Secure" } else { "" };
    if let Ok(value) = HeaderValue::from_str(&format!(
        "journey_session=; Path=/; HttpOnly; SameSite=Strict; Max-Age=0{secure}"
    )) {
        response.headers_mut().insert(header::SET_COOKIE, value);
    }
}

fn validated_request_origin(headers: &HeaderMap, security: &SiteSecurity) -> Option<ParsedOrigin> {
    let Some(origin_header) = headers.get(header::ORIGIN) else {
        return None;
    };
    let Ok(origin_value) = origin_header.to_str() else {
        return None;
    };
    let Some(origin) = parse_origin(origin_value) else {
        return None;
    };
    match &security.public_origin {
        Some(configured) if origin.key == configured.key => Some(origin),
        None if origin.scheme == "http" && origin.is_loopback() => Some(origin),
        _ => None,
    }
}

fn origin_allowed(headers: &HeaderMap, security: &SiteSecurity) -> bool {
    validated_request_origin(headers, security).is_some()
}

fn parse_origin(value: &str) -> Option<ParsedOrigin> {
    let uri = value.parse::<Uri>().ok()?;
    let scheme = uri.scheme_str()?.to_ascii_lowercase();
    if scheme != "http" && scheme != "https" {
        return None;
    }
    if uri
        .path_and_query()
        .is_some_and(|path| path.as_str() != "/")
    {
        return None;
    }
    let authority = uri.authority()?;
    if authority.as_str().contains('@') {
        return None;
    }
    let raw_host = authority.host();
    if raw_host.is_empty() {
        return None;
    }
    let host = raw_host.trim_start_matches('[').trim_end_matches(']').to_ascii_lowercase();
    let port = authority.port_u16().unwrap_or(if scheme == "https" { 443 } else { 80 });
    let port_suffix = if (scheme == "https" && port == 443) || (scheme == "http" && port == 80) {
        String::new()
    } else {
        format!(":{port}")
    };
    let authority_host = if host.parse::<IpAddr>().is_ok_and(|address| address.is_ipv6()) {
        format!("[{host}]")
    } else {
        host.clone()
    };
    Some(ParsedOrigin {
        key: format!("{scheme}://{authority_host}{port_suffix}"),
        scheme,
        host,
    })
}

impl ParsedOrigin {
    fn is_loopback(&self) -> bool {
        self.host.eq_ignore_ascii_case("localhost")
            || self
                .host
                .parse::<IpAddr>()
                .is_ok_and(|address| address.is_loopback())
    }
}

impl From<AuthenticatedAccount> for AuthPrincipal {
    fn from(account: AuthenticatedAccount) -> Self {
        Self {
            username: account.username,
            role: account.role,
            timezone: "UTC".to_owned(),
        }
    }
}

impl AuthPrincipal {
    fn post_access(&self) -> PostAccess {
        match self.role {
            AccountRole::Read => PostAccess::Published,
            AccountRole::Write => PostAccess::Author(self.username.clone()),
            AccountRole::Admin => PostAccess::Admin,
        }
    }

    fn share_access(&self) -> Option<ShareAccess> {
        match self.role {
            AccountRole::Read => None,
            AccountRole::Write => Some(ShareAccess::Author(self.username.clone())),
            AccountRole::Admin => Some(ShareAccess::Admin),
        }
    }
}

fn timezone_from_cookie(headers: &HeaderMap) -> String {
    let timezone = headers
        .get(header::COOKIE)
        .and_then(|value| value.to_str().ok())
        .and_then(|cookies| {
            cookies
                .split(';')
                .find_map(|cookie| cookie.trim().strip_prefix("journey_timezone="))
        });
    let Some(timezone) = timezone else {
        return "UTC".to_owned();
    };
    if jiff::tz::TimeZone::get(timezone).is_ok() {
        timezone.to_owned()
    } else {
        "UTC".to_owned()
    }
}

fn no_store(mut response: Response) -> Response {
    response.headers_mut().insert(
        header::CACHE_CONTROL,
        HeaderValue::from_static("private, no-store"),
    );
    response
}

fn unauthenticated_response(request: &Request) -> Response {
    if request.method() == Method::GET {
        if is_content_page_path(request.uri().path()) {
            let return_to = request
                .uri()
                .path_and_query()
                .and_then(|path_and_query| valid_return_target(path_and_query.as_str()))
                .unwrap_or_else(|| "/".to_owned());
            return redirect_to(&format!("/login?return_to={}", encode_url_component(&return_to)));
        }
    }
    no_store(StatusCode::UNAUTHORIZED.into_response())
}

fn valid_return_target(candidate: &str) -> Option<String> {
    if candidate.len() > 2048
        || !candidate.starts_with('/')
        || candidate.starts_with("//")
        || candidate.contains('\\')
        || candidate.contains('#')
        || candidate.chars().any(char::is_control)
    {
        return None;
    }
    let uri = candidate.parse::<Uri>().ok()?;
    if uri.scheme().is_some() || uri.authority().is_some() {
        return None;
    }
    is_content_page_path(uri.path()).then(|| candidate.to_owned())
}

fn is_content_page_path(path: &str) -> bool {
    path == "/"
        || path == "/tags"
        || path == "/posts/new"
        || path
            .strip_prefix("/archive/")
            .is_some_and(|month| valid_archive_month(month, "UTC"))
        || path.strip_prefix("/posts/").is_some_and(|id| {
            id.bytes().all(|byte| byte.is_ascii_digit())
                && id.parse::<i64>().is_ok_and(|parsed_id| {
                    parsed_id > 0 && parsed_id.to_string() == id
                })
        })
}

fn redirect_to(location: &str) -> Response {
    let mut response = StatusCode::SEE_OTHER.into_response();
    if let Ok(location) = HeaderValue::from_str(location) {
        response.headers_mut().insert(header::LOCATION, location);
    }
    no_store(response)
}

fn render_login_page(return_to: &str, username: &str, error: Option<&str>) -> String {
    let error_html = error
        .map(|error| format!("<p class=\"login-error\" role=\"alert\">{}</p>", escape_html(error)))
        .unwrap_or_default();
    let content = format!(
        "<main class=\"login-page\"><section class=\"login-card\" aria-labelledby=\"login-heading\"><header class=\"login-header\"><h1 id=\"login-heading\">Journey</h1><p>Stories from the road.</p></header>{error_html}<form action=\"/login\" method=\"post\"><input type=\"hidden\" name=\"return_to\" value=\"{}\"><div class=\"login-field\"><label for=\"login-username\">Username</label><input id=\"login-username\" name=\"username\" type=\"text\" value=\"{}\" autocomplete=\"username\" autocapitalize=\"none\" spellcheck=\"false\" maxlength=\"64\" required></div><div class=\"login-field\"><label for=\"login-password\">Password</label><input id=\"login-password\" name=\"password\" type=\"password\" autocomplete=\"current-password\" maxlength=\"1024\" required></div><button class=\"login-submit\" type=\"submit\">Sign in</button></form></section></main>",
        escape_html(return_to),
        escape_html(username),
    );
    pretty_html(&format!(
        "<!doctype html><html lang=\"en\"><head>{}</head><body>{}</body></html>",
        html_head("Sign in · Journey"),
        content,
    ))
}

fn unix_time() -> Duration {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
}

async fn feed<S: StorageClient>(
    State(state): State<AppState<S>>,
    Query(query): Query<FeedQuery>,
    Extension(principal): Extension<AuthPrincipal>,
) -> Response {
    let limit = query.limit.unwrap_or(DEFAULT_FEED_LIMIT);
    if !(1..=MAX_FEED_LIMIT).contains(&limit) {
        return (StatusCode::BAD_REQUEST, "limit must be between 1 and 100\n").into_response();
    }
    if query.month.as_deref().is_some_and(|month| !valid_archive_month(month, &principal.timezone)) {
        return (StatusCode::BAD_REQUEST, "month must be a valid YYYY-MM archive month\n").into_response();
    }
    let after = match query.after {
        Some(value) => match parse_cursor(&value) {
            Some(cursor) => Some(cursor),
            None => return (StatusCode::BAD_REQUEST, "malformed after cursor\n").into_response(),
        },
        None => None,
    };
    match state.database.feed_filtered_with_month(
        limit,
        after,
        query.tag,
        query.month,
        principal.timezone,
    ).await {
        Ok(page) => Json(page).into_response(),
        Err(error) => {
            eprintln!("website feed query failed: {error}");
            StatusCode::INTERNAL_SERVER_ERROR.into_response()
        }
    }
}

async fn drafts<S: StorageClient>(
    State(state): State<AppState<S>>,
    Extension(principal): Extension<AuthPrincipal>,
) -> Response {
    let author_username = match principal.role {
        AccountRole::Read => return no_store(StatusCode::FORBIDDEN.into_response()),
        AccountRole::Write => Some(principal.username),
        AccountRole::Admin => None,
    };
    match state.database.drafts(author_username).await {
        Ok(posts) => no_store(Json(posts).into_response()),
        Err(error) => {
            eprintln!("website draft list query failed: {error}");
            no_store(StatusCode::INTERNAL_SERVER_ERROR.into_response())
        }
    }
}

async fn sidebar_data_for_principal(
    database: &Database,
    principal: &AuthPrincipal,
) -> Result<SidebarData, String> {
    let mut sidebar = database.sidebar_data(principal.timezone.clone()).await?;
    sidebar.drafts = match principal.role {
        AccountRole::Read => None,
        AccountRole::Write => Some(database.drafts(Some(principal.username.clone())).await?),
        AccountRole::Admin => Some(database.drafts(None).await?),
    };
    Ok(sidebar)
}

async fn new_post_page<S: StorageClient>(
    State(state): State<AppState<S>>,
    Extension(principal): Extension<AuthPrincipal>,
) -> Response {
    if !matches!(principal.role, AccountRole::Write | AccountRole::Admin) {
        return no_store(StatusCode::FORBIDDEN.into_response());
    }
    let sidebar = match sidebar_data_for_principal(&state.database, &principal).await {
        Ok(sidebar) => sidebar,
        Err(error) => {
            eprintln!("website sidebar query failed: {error}");
            return no_store(StatusCode::INTERNAL_SERVER_ERROR.into_response());
        }
    };
    Html(render_new_post_page(&sidebar, &principal.username, principal.role)).into_response()
}

async fn create_draft<S: StorageClient>(
    State(state): State<AppState<S>>,
    Extension(principal): Extension<AuthPrincipal>,
    headers: HeaderMap,
    Json(request): Json<CreateDraftRequest>,
) -> Response {
    if !matches!(principal.role, AccountRole::Write | AccountRole::Admin) {
        return no_store(StatusCode::FORBIDDEN.into_response());
    }
    if !origin_allowed(&headers, &state.security) {
        return no_store((StatusCode::FORBIDDEN, "origin not allowed\n").into_response());
    }
    if request.title.trim().is_empty() {
        return no_store((StatusCode::BAD_REQUEST, "title must not be blank\n").into_response());
    }
    if request.blocks.iter().any(|block| block.blocks.iter().any(|child| !child.blocks.is_empty())) {
        return no_store((StatusCode::BAD_REQUEST, "post blocks may only be nested one level deep\n").into_response());
    }
    let blocks = request.blocks.into_iter().map(create_draft_block).collect();
    match state
        .database
        .create_draft(
            principal.username,
            request.title,
            request.summary.unwrap_or_default(),
            request.tags.unwrap_or_default(),
            blocks,
        )
        .await
    {
        Ok(id) => {
            let mut response = Json(CreatedPostResponse { id }).into_response();
            *response.status_mut() = StatusCode::CREATED;
            if let Ok(location) = HeaderValue::from_str(&format!("/posts/{id}")) {
                response.headers_mut().insert(header::LOCATION, location);
            }
            no_store(response)
        }
        Err(error) => {
            eprintln!("website draft creation failed: {error}");
            no_store(StatusCode::INTERNAL_SERVER_ERROR.into_response())
        }
    }
}

fn create_draft_block(block: CreateDraftBlock) -> NewBlock {
    NewBlock {
        header: block.header,
        body: block.body,
        storage_key: None,
        content_type: None,
        alt: None,
        children: block.blocks.into_iter().map(create_draft_block).collect(),
    }
}

async fn publish_post<S: StorageClient>(
    State(state): State<AppState<S>>,
    Path(id): Path<i64>,
    Extension(principal): Extension<AuthPrincipal>,
    headers: HeaderMap,
    Json(request): Json<PublishPostRequest>,
) -> Response {
    if !matches!(principal.role, AccountRole::Write | AccountRole::Admin) {
        return no_store(StatusCode::FORBIDDEN.into_response());
    }
    if !origin_allowed(&headers, &state.security) {
        return no_store((StatusCode::FORBIDDEN, "origin not allowed\n").into_response());
    }
    match state
        .database
        .publish_draft(
            id,
            principal.username,
            principal.role == AccountRole::Admin,
            request.published_at,
        )
        .await
    {
        Ok(crate::db::PublishPostResult::Published) => {
            no_store(StatusCode::NO_CONTENT.into_response())
        }
        Ok(crate::db::PublishPostResult::NotFound) => {
            no_store(StatusCode::NOT_FOUND.into_response())
        }
        Ok(crate::db::PublishPostResult::AlreadyPublished) => {
            no_store(StatusCode::CONFLICT.into_response())
        }
        Ok(crate::db::PublishPostResult::MissingText) => no_store((
            StatusCode::BAD_REQUEST,
            "a post needs a text block with a nonblank header or body before it can be published\n",
        ).into_response()),
        Ok(crate::db::PublishPostResult::FutureTimestamp) => no_store((
            StatusCode::BAD_REQUEST,
            "published_at must not be later than the server's current second\n",
        ).into_response()),
        Ok(crate::db::PublishPostResult::InvalidTimestamp) => no_store((
            StatusCode::BAD_REQUEST,
            "published_at is outside the supported timestamp range\n",
        ).into_response()),
        Err(error) => {
            eprintln!("website post publishing failed: {error}");
            no_store(StatusCode::INTERNAL_SERVER_ERROR.into_response())
        }
    }
}

async fn home<S: StorageClient>(
    State(state): State<AppState<S>>,
    Extension(principal): Extension<AuthPrincipal>,
) -> Response {
    match state.database.feed(1, None).await {
        Ok(page) => {
            let initial_post = match page.posts.first() {
                Some(summary) => match state.database.post(summary.id).await {
                    Ok(post) => post,
                    Err(error) => {
                        eprintln!("website home post query failed: {error}");
                        return StatusCode::INTERNAL_SERVER_ERROR.into_response();
                    }
                },
                None => None,
            };
            let next_cursor = initial_post
                .as_ref()
                .and(page.next_cursor.as_deref());
            let sidebar = match sidebar_data_for_principal(&state.database, &principal).await {
                Ok(sidebar) => sidebar,
                Err(error) => {
                    eprintln!("website sidebar query failed: {error}");
                    return StatusCode::INTERNAL_SERVER_ERROR.into_response();
                }
            };
            Html(render_home(
                &sidebar,
                initial_post.as_ref(),
                next_cursor,
                &principal.username,
                principal.role,
                principal.share_access().as_ref(),
            ))
            .into_response()
        }
        Err(error) => {
            eprintln!("website home query failed: {error}");
            StatusCode::INTERNAL_SERVER_ERROR.into_response()
        }
    }
}

async fn tag_page<S: StorageClient>(
    State(state): State<AppState<S>>,
    Query(query): Query<TagPageQuery>,
    Extension(principal): Extension<AuthPrincipal>,
) -> Response {
    match state.database.feed_filtered(1, None, Some(query.tag.clone())).await {
        Ok(page) => {
            let initial_post = match page.posts.first() {
                Some(summary) => match state.database.post(summary.id).await {
                    Ok(post) => post,
                    Err(error) => {
                        eprintln!("website tag post query failed: {error}");
                        return StatusCode::INTERNAL_SERVER_ERROR.into_response();
                    }
                },
                None => None,
            };
            let next_cursor = initial_post
                .as_ref()
                .and(page.next_cursor.as_deref());
            let sidebar = match sidebar_data_for_principal(&state.database, &principal).await {
                Ok(sidebar) => sidebar,
                Err(error) => {
                    eprintln!("website sidebar query failed: {error}");
                    return StatusCode::INTERNAL_SERVER_ERROR.into_response();
                }
            };
            Html(render_tag_feed(
                &sidebar,
                &query.tag,
                initial_post.as_ref(),
                next_cursor,
                &principal.username,
                principal.role,
                principal.share_access().as_ref(),
            ))
                .into_response()
        }
        Err(error) => {
            eprintln!("website tag feed query failed: {error}");
            StatusCode::INTERNAL_SERVER_ERROR.into_response()
        }
    }
}

async fn archive_page<S: StorageClient>(
    State(state): State<AppState<S>>,
    Path(month): Path<String>,
    Extension(principal): Extension<AuthPrincipal>,
) -> Response {
    if !valid_archive_month(&month, &principal.timezone) {
        return StatusCode::NOT_FOUND.into_response();
    }
    let page = match state.database.feed_filtered_with_month(
        1,
        None,
        None,
        Some(month.clone()),
        principal.timezone.clone(),
    ).await {
        Ok(page) => page,
        Err(error) => {
            eprintln!("website archive query failed: {error}");
            return StatusCode::INTERNAL_SERVER_ERROR.into_response();
        }
    };
    let initial_post = match page.posts.first() {
        Some(summary) => match state.database.post(summary.id).await {
            Ok(post) => post,
            Err(error) => {
                eprintln!("website archive post query failed: {error}");
                return StatusCode::INTERNAL_SERVER_ERROR.into_response();
            }
        },
        None => None,
    };
    let next_cursor = initial_post
        .as_ref()
        .and(page.next_cursor.as_deref());
    let sidebar = match sidebar_data_for_principal(&state.database, &principal).await {
        Ok(sidebar) => sidebar,
        Err(error) => {
            eprintln!("website sidebar query failed: {error}");
            return StatusCode::INTERNAL_SERVER_ERROR.into_response();
        }
    };
    Html(render_archive(
        &sidebar,
        &month,
        initial_post.as_ref(),
        next_cursor,
        &principal.username,
        principal.role,
        principal.share_access().as_ref(),
    ))
    .into_response()
}

async fn api_full_post<S: StorageClient>(
    State(state): State<AppState<S>>,
    Path(id): Path<i64>,
    Extension(principal): Extension<AuthPrincipal>,
) -> Response {
    match state.database.post_with_access(id, principal.post_access()).await {
        Ok(Some(post)) => Json(post).into_response(),
        Ok(None) => StatusCode::NOT_FOUND.into_response(),
        Err(error) => {
            eprintln!("website post query failed: {error}");
            StatusCode::INTERNAL_SERVER_ERROR.into_response()
        }
    }
}

async fn post_page<S: StorageClient>(
    State(state): State<AppState<S>>,
    Path(id): Path<i64>,
    query: Result<Query<CreatedPostQuery>, axum::extract::rejection::QueryRejection>,
    Extension(principal): Extension<AuthPrincipal>,
) -> Response {
    match state.database.post_with_access(id, principal.post_access()).await {
        Ok(Some(post)) => {
            let sidebar = match sidebar_data_for_principal(&state.database, &principal).await {
                Ok(sidebar) => sidebar,
                Err(error) => {
                    eprintln!("website sidebar query failed: {error}");
                    return StatusCode::INTERNAL_SERVER_ERROR.into_response();
                }
            };
            Html(render_full_post(
                &sidebar,
                &post,
                &principal.username,
                principal.role,
                !post.published
                    && query.ok().is_some_and(|Query(query)| query.created.as_deref() == Some("1")),
                principal.share_access().as_ref(),
            ))
            .into_response()
        }
        Ok(None) => StatusCode::NOT_FOUND.into_response(),
        Err(error) => {
            eprintln!("website post page query failed: {error}");
            StatusCode::INTERNAL_SERVER_ERROR.into_response()
        }
    }
}

async fn post_fragment<S: StorageClient>(
    State(state): State<AppState<S>>,
    Path(id): Path<i64>,
    Extension(principal): Extension<AuthPrincipal>,
) -> Response {
    match state.database.post_with_access(id, principal.post_access()).await {
        Ok(Some(post)) => Html(pretty_html(&render_post(
            &post,
            false,
            principal.share_access().as_ref().is_some_and(|access| can_share_post(&post, access)),
        )))
        .into_response(),
        Ok(None) => StatusCode::NOT_FOUND.into_response(),
        Err(error) => {
            eprintln!("website post fragment query failed: {error}");
            StatusCode::INTERNAL_SERVER_ERROR.into_response()
        }
    }
}

async fn site_css() -> impl IntoResponse {
    ([(header::CONTENT_TYPE, "text/css; charset=utf-8")], SITE_CSS)
}

async fn site_js() -> impl IntoResponse {
    ([(header::CONTENT_TYPE, "text/javascript; charset=utf-8")], SITE_JS)
}

fn parse_cursor(value: &str) -> Option<FeedCursor> {
    let (published_at, encoded_id) = value.split_once(':')?;
    let parsed_published_at = published_at.parse::<i64>().ok()?;
    if parsed_published_at.to_string() != published_at {
        return None;
    }
    let id = encoded_id.parse::<i64>().ok()?;
    if id <= 0 || id.to_string() != encoded_id {
        return None;
    }
    Some(FeedCursor {
        published_at: parsed_published_at,
        id,
    })
}

fn valid_archive_month(value: &str, timezone: &str) -> bool {
    let bytes = value.as_bytes();
    bytes.len() == 7
        && bytes[4] == b'-'
        && bytes
            .iter()
            .enumerate()
            .all(|(index, byte)| index == 4 || byte.is_ascii_digit())
        && value[5..7].parse::<u32>().is_ok_and(|month| (1..=12).contains(&month))
        && crate::db::local_month_bounds(value, timezone).is_ok()
}

fn render_full_post(
    sidebar: &SidebarData,
    post: &Post,
    username: &str,
    role: AccountRole,
    created: bool,
    share_access: Option<&ShareAccess>,
) -> String {
    let confirmation = if created {
        "<p class=\"creation-confirmation\" data-created-confirmation role=\"status\">Draft created. You can view it here.</p>"
    } else {
        ""
    };
    let publish_control = if !post.published
        && matches!(role, AccountRole::Write | AccountRole::Admin)
        && (role == AccountRole::Admin || post.author_username.eq_ignore_ascii_case(username))
    {
        format!(
            "<div class=\"post-publish-area\"><button class=\"post-publish-button\" type=\"button\" data-publish-post=\"{}\" aria-haspopup=\"dialog\" aria-controls=\"publish-dialog\">Publish</button></div>",
            post.summary.id,
        )
    } else {
        String::new()
    };
    let content = format!(
        "<main class=\"site site-post\"><p class=\"back-link\"><a href=\"/\">All posts</a></p>{confirmation}{}{publish_control}</main>",
        render_post(post, true, share_access.is_some_and(|access| can_share_post(post, access))),
    );
    render_site_page(&post.summary.title, sidebar, &content, true, username, role, true)
}

fn render_shared_post(post: &Post, share_prefix: &str) -> String {
    let content = format!(
        "<main class=\"site site-feed\">{}</main>",
        render_post_with_media_prefix(post, true, Some(share_prefix), false),
    );
    pretty_html(&format!(
        "<!doctype html><html lang=\"en\"><head>{}</head><body><div class=\"site-layout\">{content}</div>{}</body></html>",
        html_head(&format!("{} · Journey", post.summary.title)),
        SLIDESHOW_HTML,
    ))
}

fn render_home(
    sidebar: &SidebarData,
    initial_post: Option<&Post>,
    next_cursor: Option<&str>,
    username: &str,
    role: AccountRole,
    share_access: Option<&ShareAccess>,
) -> String {
    let mut feed = format!(
        "<section id=\"feed\" aria-label=\"Posts\" data-next-cursor=\"{}\">",
        escape_html(next_cursor.unwrap_or("")),
    );
    if let Some(post) = initial_post {
        feed.push_str(&render_post(
            post,
            true,
            share_access.is_some_and(|access| can_share_post(post, access)),
        ));
    } else {
        feed.push_str("<p class=\"empty-feed\">No published posts yet.</p>");
    }
    feed.push_str("</section>");
    let content = render_feed_controls("<main class=\"site site-feed\">", &feed, next_cursor);
    render_site_page("Journey", sidebar, &content, true, username, role, true)
}

fn render_tag_feed(
    sidebar: &SidebarData,
    tag: &str,
    initial_post: Option<&Post>,
    next_cursor: Option<&str>,
    username: &str,
    role: AccountRole,
    share_access: Option<&ShareAccess>,
) -> String {
    let mut feed = format!(
        "<section id=\"feed\" aria-label=\"Posts tagged {}\" data-next-cursor=\"{}\" data-tag=\"{}\">",
        escape_html(tag),
        escape_html(next_cursor.unwrap_or("")),
        escape_html(tag),
    );
    if let Some(post) = initial_post {
        feed.push_str(&render_post(
            post,
            true,
            share_access.is_some_and(|access| can_share_post(post, access)),
        ));
    } else {
        feed.push_str("<p class=\"empty-feed\">No published posts have this tag.</p>");
    }
    feed.push_str("</section>");
    let heading = format!(
        "<header class=\"feed-page-heading\"><h1>Posts tagged <span>{}</span></h1></header>",
        escape_html(tag),
    );
    let content = render_feed_controls(
        &format!("<main class=\"site site-feed\">{heading}"),
        &feed,
        next_cursor,
    );
    render_site_page(
        &format!("Posts tagged {tag}"),
        sidebar,
        &content,
        true,
        username,
        role,
        true,
    )
}

fn render_new_post_page(sidebar: &SidebarData, username: &str, role: AccountRole) -> String {
    let content = concat!(
        "<main class=\"site site-new-post\">",
        "<header class=\"new-post-heading\"><p class=\"back-link\"><a href=\"/\">All posts</a></p><h1>Create a draft</h1></header>",
        "<form class=\"draft-form\" id=\"draft-form\" novalidate>",
        "<div class=\"draft-field\"><label for=\"draft-title\">Title <span aria-hidden=\"true\">*</span></label><input id=\"draft-title\" name=\"title\" type=\"text\" autocomplete=\"off\" aria-describedby=\"draft-title-help\" required><p class=\"draft-field-help\" id=\"draft-title-help\">A title is required.</p></div>",
        "<div class=\"draft-field\"><label for=\"draft-summary\">Summary <span class=\"draft-optional\">Optional</span></label><textarea id=\"draft-summary\" name=\"summary\" rows=\"4\"></textarea></div>",
        "<div class=\"draft-field\"><label for=\"draft-tags\">Tags <span class=\"draft-optional\">Optional</span></label><input id=\"draft-tags\" name=\"tags\" type=\"text\" autocomplete=\"off\" aria-describedby=\"draft-tags-help\"><p class=\"draft-field-help\" id=\"draft-tags-help\">Separate tags with commas.</p></div>",
        "<section class=\"draft-block-editor\" aria-labelledby=\"draft-blocks-heading\"><div class=\"draft-block-heading\"><div><h2 id=\"draft-blocks-heading\">Text blocks</h2><p>Arrange blocks and one level of child blocks in the order you want them to appear.</p></div><button class=\"draft-secondary-button\" type=\"button\" data-block-action=\"add-root\">Add text block</button></div><div class=\"draft-root-blocks\" id=\"draft-root-blocks\" data-block-list=\"root\"></div></section>",
        "<p class=\"draft-form-error\" id=\"draft-form-error\" role=\"alert\" hidden></p>",
        "<div class=\"draft-submit-area\"><p>Saved drafts can be opened from the sidebar and published from their post page.</p><button class=\"draft-submit-button\" id=\"draft-submit\" type=\"submit\">Create draft</button></div>",
        "</form></main>",
    );
    render_site_page("Create a draft", sidebar, content, false, username, role, false)
}

fn render_feed_controls(main_open: &str, feed: &str, next_cursor: Option<&str>) -> String {
    let mut content = format!("{main_open}{feed}<div id=\"feed-sentinel\" aria-hidden=\"true\"></div><p id=\"feed-status\" role=\"status\" aria-live=\"polite\">");
    if next_cursor.is_none() {
        content.push_str("You have reached the end of the feed.");
    }
    content.push_str("</p><button id=\"load-more\" type=\"button\"");
    if next_cursor.is_none() {
        content.push_str(" disabled");
    }
    content.push_str(">Load more</button></main>");
    content
}

fn render_archive(
    sidebar: &SidebarData,
    month: &str,
    initial_post: Option<&Post>,
    next_cursor: Option<&str>,
    username: &str,
    role: AccountRole,
    share_access: Option<&ShareAccess>,
) -> String {
    let month_label = month_label(month);
    let mut feed = format!(
        "<section id=\"feed\" aria-label=\"Posts from {}\" data-next-cursor=\"{}\" data-month=\"{}\">",
        escape_html(&month_label),
        escape_html(next_cursor.unwrap_or("")),
        escape_html(month),
    );
    if let Some(post) = initial_post {
        feed.push_str(&render_post(
            post,
            true,
            share_access.is_some_and(|access| can_share_post(post, access)),
        ));
    } else {
        feed.push_str("<p class=\"empty-feed\">No published posts in this month.</p>");
    }
    feed.push_str("</section>");
    let heading = format!(
        "<p class=\"back-link\"><a href=\"/\">All posts</a></p><h1>{}</h1>",
        escape_html(&month_label),
    );
    let content = render_feed_controls(
        &format!("<main class=\"site site-feed site-archive\">{heading}"),
        &feed,
        next_cursor,
    );
    render_site_page(&month_label, sidebar, &content, true, username, role, true)
}

fn render_site_page(
    title: &str,
    sidebar: &SidebarData,
    content: &str,
    include_slideshow: bool,
    username: &str,
    role: AccountRole,
    show_new_post: bool,
) -> String {
    let slideshow = if include_slideshow { SLIDESHOW_HTML } else { "" };
    let new_post_button = if show_new_post
        && matches!(role, AccountRole::Write | AccountRole::Admin)
    {
        "<a class=\"new-post-float\" href=\"/posts/new\">+ New post</a>"
    } else {
        ""
    };
    let html = format!(
        "<!doctype html><html lang=\"en\"><head>{}</head><body><div class=\"site-layout\" id=\"site-layout\"><button class=\"sidebar-toggle\" id=\"sidebar-toggle\" type=\"button\" aria-controls=\"site-sidebar\" aria-expanded=\"false\" aria-label=\"Open sidebar\"><span aria-hidden=\"true\">›</span></button><button class=\"sidebar-backdrop\" id=\"sidebar-backdrop\" type=\"button\" aria-label=\"Close sidebar\" hidden></button>{}{}{}</div>{}{}{}{}</body></html>",
        html_head(title),
        render_sidebar(sidebar),
        render_account_controls(username),
        content,
        new_post_button,
        slideshow,
        PUBLISH_DIALOG_HTML,
        SHARE_DIALOG_HTML,
    );
    pretty_html(&html)
}

fn render_account_controls(username: &str) -> String {
    format!(
        "<div class=\"account-controls\"><span>Signed in as <strong>{}</strong></span><form action=\"/logout\" method=\"post\"><button type=\"submit\">Sign out</button></form></div>",
        escape_html(username),
    )
}

fn render_sidebar(sidebar: &SidebarData) -> String {
    let mut html = String::from(
        "<nav class=\"site-sidebar\" id=\"site-sidebar\" aria-label=\"Site navigation\" hidden><header class=\"sidebar-header\"><a class=\"site-name\" href=\"/\">Journey</a><p>Stories from the road.</p></header>",
    );
    if let Some(drafts) = &sidebar.drafts {
        html.push_str("<section class=\"sidebar-section\" aria-labelledby=\"sidebar-drafts-heading\"><h2 id=\"sidebar-drafts-heading\">Drafts</h2><ul class=\"sidebar-list\" id=\"sidebar-drafts\">");
        if drafts.is_empty() {
            html.push_str("<li class=\"sidebar-muted\">No drafts yet.</li>");
        } else {
            for (index, post) in drafts.iter().enumerate() {
                html.push_str(&format!(
                    "<li{}><a href=\"/posts/{}\">{}</a></li>",
                    if index >= 5 { " data-overflow-item=\"true\"" } else { "" },
                    post.id,
                    escape_html(&post.title),
                ));
            }
        }
        html.push_str("</ul>");
        if drafts.len() > 5 {
            html.push_str("<button class=\"sidebar-expand\" type=\"button\" aria-controls=\"sidebar-drafts\" aria-expanded=\"false\" data-sidebar-expand=\"sidebar-drafts\" data-expand-label=\"Show all drafts\" data-collapse-label=\"Show fewer drafts\" hidden>Show all drafts</button>");
        }
        html.push_str("</section>");
    }
    html.push_str("<section class=\"sidebar-section\" aria-labelledby=\"sidebar-recent-heading\"><h2 id=\"sidebar-recent-heading\">Recent posts</h2><ul class=\"sidebar-list\">");
    if sidebar.recent_posts.is_empty() {
        html.push_str("<li class=\"sidebar-muted\">No published posts yet.</li>");
    } else {
        for post in &sidebar.recent_posts {
            let published_at = render_time_element(post.published_at.as_ref());
            html.push_str(&format!(
                "<li><a href=\"/posts/{}\">{}</a>{}</li>",
                post.id,
                escape_html(&post.title),
                published_at,
            ));
        }
    }
    html.push_str("</ul></section><section class=\"sidebar-section\" aria-labelledby=\"sidebar-archive-heading\"><h2 id=\"sidebar-archive-heading\">Archive</h2><ul class=\"sidebar-list\" id=\"sidebar-months\">");
    if sidebar.archive_months.is_empty() {
        html.push_str("<li class=\"sidebar-muted\">No published posts yet.</li>");
    } else {
        for (index, month) in sidebar.archive_months.iter().enumerate() {
            html.push_str(&format!(
                "<li{}><a href=\"/archive/{}\">{}</a></li>",
                if index >= 6 { " data-overflow-item=\"true\"" } else { "" },
                encode_url_component(month),
                escape_html(&month_label(month)),
            ));
        }
    }
    html.push_str("</ul>");
    if sidebar.archive_months.len() > 6 {
        html.push_str("<button class=\"sidebar-expand\" type=\"button\" aria-controls=\"sidebar-months\" aria-expanded=\"false\" data-sidebar-expand=\"sidebar-months\" data-expand-label=\"Show all months\" data-collapse-label=\"Show fewer months\" hidden>Show all months</button>");
    }
    html.push_str("</section><section class=\"sidebar-section\" aria-labelledby=\"sidebar-tags-heading\"><h2 id=\"sidebar-tags-heading\">Tags</h2><ul class=\"sidebar-list sidebar-tags\" id=\"sidebar-tags\">");
    if sidebar.tags.is_empty() {
        html.push_str("<li class=\"sidebar-muted\">No tags yet.</li>");
    } else {
        for (index, tag) in sidebar.tags.iter().enumerate() {
            html.push_str(&format!(
                "<li{}><a href=\"/tags?tag={}\">{}</a></li>",
                if index >= 12 { " data-overflow-item=\"true\"" } else { "" },
                encode_url_component(tag),
                escape_html(tag),
            ));
        }
    }
    html.push_str("</ul>");
    if sidebar.tags.len() > 12 {
        html.push_str("<button class=\"sidebar-expand\" type=\"button\" aria-controls=\"sidebar-tags\" aria-expanded=\"false\" data-sidebar-expand=\"sidebar-tags\" data-expand-label=\"Show all tags\" data-collapse-label=\"Show fewer tags\" hidden>Show all tags</button>");
    }
    html.push_str("</section></nav>");
    html
}

fn month_label(month: &str) -> String {
    const MONTH_NAMES: [&str; 12] = [
        "January", "February", "March", "April", "May", "June", "July", "August", "September",
        "October", "November", "December",
    ];
    let Some((year, encoded_month)) = month.split_once('-') else {
        return month.to_owned();
    };
    let Ok(number) = encoded_month.parse::<usize>() else {
        return month.to_owned();
    };
    if !(1..=12).contains(&number) {
        return month.to_owned();
    }
    format!("{} {year}", MONTH_NAMES[number - 1])
}

fn encode_url_component(value: &str) -> String {
    let mut encoded = String::with_capacity(value.len());
    for byte in value.bytes() {
        if byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'.' | b'_' | b'~') {
            encoded.push(byte as char);
        } else {
            encoded.push_str(&format!("%{byte:02X}"));
        }
    }
    encoded
}

fn render_post(post: &Post, prioritize_first_image: bool, can_manage_shares: bool) -> String {
    render_post_with_media_prefix(post, prioritize_first_image, None, can_manage_shares)
}

fn render_post_with_media_prefix(
    post: &Post,
    prioritize_first_image: bool,
    media_prefix: Option<&str>,
    can_manage_shares: bool,
) -> String {
    let share_control = if can_manage_shares && post.published {
        format!(
            "<button class=\"post-share-button\" type=\"button\" data-share-post=\"{}\" aria-haspopup=\"dialog\" aria-controls=\"share-dialog\">Share</button>",
            post.summary.id,
        )
    } else {
        String::new()
    };
    let published_at = render_time_element(post.summary.published_at.as_ref());
    format!(
        "<article class=\"post\" data-post-id=\"{}\"><header class=\"post-header\"><h1>{}</h1><div class=\"post-date-row\">{}{}</div><p class=\"summary\">{}</p></header>{}{}</article>",
        post.summary.id,
        escape_html(&post.summary.title),
        published_at,
        share_control,
        escape_html(&post.summary.summary),
        render_tags_html(&post.tags, media_prefix.is_some()),
        render_post_blocks_html(post, prioritize_first_image, media_prefix),
    )
}

fn render_time_element(published_at: Option<&i64>) -> String {
    published_at
        .map(|published_at| {
            let Ok(timestamp) = jiff::Timestamp::from_second(*published_at) else {
                return String::new();
            };
            let datetime = timestamp.to_string();
            let Ok(utc) = timestamp.in_tz("UTC") else {
                return String::new();
            };
            let civil = utc.datetime();
            const MONTH_NAMES: [&str; 12] = [
                "January", "February", "March", "April", "May", "June", "July", "August",
                "September", "October", "November", "December",
            ];
            let fallback = format!(
                "{} {}, {} at {:02}:{:02} UTC",
                MONTH_NAMES[civil.month() as usize - 1],
                civil.day(),
                civil.year(),
                civil.hour(),
                civil.minute(),
            );
            format!(
                "<time datetime=\"{}\" data-local-time>{}</time>",
                escape_html(&datetime),
                escape_html(&fallback),
            )
        })
        .unwrap_or_default()
}

fn can_share_post(post: &Post, access: &ShareAccess) -> bool {
    match access {
        ShareAccess::Author(username) => post.author_username.eq_ignore_ascii_case(username),
        ShareAccess::Admin => true,
    }
}

fn render_post_blocks_html(
    post: &Post,
    prioritize_first_image: bool,
    media_prefix: Option<&str>,
) -> String {
    let mut prioritize_first_image = prioritize_first_image;
    render_blocks_html(
        &post.blocks,
        post.summary.id,
        &mut prioritize_first_image,
        media_prefix,
    )
}

fn render_blocks_html(
    blocks: &[crate::db::PostBlock],
    post_id: i64,
    prioritize_first_image: &mut bool,
    media_prefix: Option<&str>,
) -> String {
    let mut html = String::new();
    for block in blocks {
        if block.children.is_empty() {
            html.push_str(&render_standalone_block(block, post_id, prioritize_first_image, media_prefix));
        } else {
            html.push_str(&render_group(block, post_id, prioritize_first_image, media_prefix));
        }
    }
    html
}

fn render_standalone_block(
    block: &crate::db::PostBlock,
    post_id: i64,
    prioritize_first_image: &mut bool,
    media_prefix: Option<&str>,
) -> String {
    let mut html = String::new();
    match block.content_type.as_deref() {
        Some(content_type) if content_type.starts_with("image/") => {
            let media_url = media_url(media_prefix, post_id, block.id);
            let loading = image_loading(prioritize_first_image);
            let alt = block.alt.as_deref().unwrap_or("");
            let label = media_accessible_label(block.header.as_deref(), block.body.as_deref(), alt);
            html.push_str(&format!(
                "<figure class=\"single-media\"><button class=\"gallery-item solo-media\" type=\"button\" data-gallery=\"solo-{post_id}-{}\" data-media-type=\"{}\" data-media-src=\"{}\" data-alt=\"{}\" data-label=\"{}\" data-caption=\"{}\" aria-label=\"Open image: {}\"><img src=\"{}\" alt=\"{}\" loading=\"{}\"{}></button>{}{}</figure>",
                block.id,
                escape_html(content_type),
                media_url,
                escape_html(alt),
                escape_html(block.header.as_deref().unwrap_or("")),
                escape_html(block.body.as_deref().unwrap_or("")),
                escape_html(&label),
                media_url,
                escape_html(alt),
                loading,
                image_fetch_priority(loading),
                render_media_label(block.header.as_deref()),
                render_caption(block.body.as_deref()),
            ));
        }
        Some(content_type) if content_type.starts_with("video/") => {
            let media_url = media_url(media_prefix, post_id, block.id);
            html.push_str(&format!(
                "<figure class=\"single-media\">{}<video controls preload=\"metadata\" aria-label=\"{}\"><source src=\"{}\" type=\"{}\"></video>{}</figure>",
                render_media_label(block.header.as_deref()),
                escape_html(&media_accessible_label(block.header.as_deref(), block.body.as_deref(), "video")),
                media_url,
                escape_html(content_type),
                render_caption(block.body.as_deref()),
            ));
        }
        _ => {
            if let Some(header) = &block.header {
                html.push_str(&format!("<h2>{}</h2>", escape_html(header)));
            }
            if let Some(body) = &block.body {
                html.push_str(&format!("<p class=\"block-copy\">{}</p>", escape_html(body)));
            }
        }
    }
    html
}

fn render_group(
    group: &crate::db::PostBlock,
    post_id: i64,
    prioritize_first_image: &mut bool,
    media_prefix: Option<&str>,
) -> String {
    let mut html = String::from("<section class=\"post-group\">");
    if let Some(header) = &group.header {
        html.push_str(&format!("<h2>{}</h2>", escape_html(header)));
    }
    if let Some(body) = &group.body {
        html.push_str(&format!("<p class=\"group-description\">{}</p>", escape_html(body)));
    }

    let mut index = 0;
    while index < group.children.len() {
        let child = &group.children[index];
        if is_media_block(child) {
            let start = index;
            while index < group.children.len() && is_media_block(&group.children[index]) {
                index += 1;
            }
            html.push_str(&render_gallery(
                &group.children[start..index],
                post_id,
                prioritize_first_image,
                media_prefix,
            ));
        } else {
            if child.header.is_some() || child.body.is_some() {
                html.push_str("<div class=\"group-note\">");
                if let Some(header) = &child.header {
                    html.push_str(&format!("<h3>{}</h3>", escape_html(header)));
                }
                if let Some(body) = &child.body {
                    html.push_str(&format!("<p>{}</p>", escape_html(body)));
                }
                html.push_str("</div>");
            }
            index += 1;
        }
    }
    html.push_str("</section>");
    html
}

fn render_gallery(
    blocks: &[crate::db::PostBlock],
    post_id: i64,
    prioritize_first_image: &mut bool,
    media_prefix: Option<&str>,
) -> String {
    let gallery_id = format!("gallery-{post_id}-{}", blocks[0].id);
    let mut html = format!("<div class=\"gallery\" data-gallery-run=\"{gallery_id}\">");
    for block in blocks {
        let content_type = block.content_type.as_deref().unwrap_or("");
        let media_url = media_url(media_prefix, post_id, block.id);
        let alt = block.alt.as_deref().unwrap_or("");
        let header = block.header.as_deref().unwrap_or("");
        let caption = block.body.as_deref().unwrap_or("");
        let label = media_accessible_label(block.header.as_deref(), block.body.as_deref(), alt);
        html.push_str(&format!(
            "<button class=\"gallery-item\" type=\"button\" data-gallery=\"{gallery_id}\" data-media-type=\"{}\" data-media-src=\"{}\" data-alt=\"{}\" data-label=\"{}\" data-caption=\"{}\" aria-label=\"Open media: {}\">",
            escape_html(content_type),
            media_url,
            escape_html(alt),
            escape_html(header),
            escape_html(caption),
            escape_html(&label),
        ));
        if content_type.starts_with("image/") {
            let loading = image_loading(prioritize_first_image);
            html.push_str(&format!(
                "<img src=\"{}\" alt=\"{}\" loading=\"{}\"{}>",
                media_url,
                escape_html(alt),
                loading,
                image_fetch_priority(loading),
            ));
        } else {
            html.push_str("<span class=\"video-placeholder\" aria-hidden=\"true\"><span class=\"play-icon\">▶</span><span>Video</span></span>");
        }
        html.push_str(&render_media_label(block.header.as_deref()));
        html.push_str(&render_caption_span(block.body.as_deref()));
        html.push_str("</button>");
    }
    html.push_str("</div>");
    html
}

fn is_media_block(block: &crate::db::PostBlock) -> bool {
    block
        .content_type
        .as_deref()
        .is_some_and(|content_type| content_type.starts_with("image/") || content_type.starts_with("video/"))
}

fn image_loading(prioritize_first_image: &mut bool) -> &'static str {
    if *prioritize_first_image {
        *prioritize_first_image = false;
        "eager"
    } else {
        "lazy"
    }
}

fn image_fetch_priority(loading: &str) -> &'static str {
    if loading == "eager" {
        " fetchpriority=\"high\""
    } else {
        ""
    }
}

fn media_accessible_label(header: Option<&str>, caption: Option<&str>, fallback: &str) -> String {
    header
        .filter(|value| !value.is_empty())
        .or_else(|| caption.filter(|value| !value.is_empty()))
        .unwrap_or_else(|| if fallback.is_empty() { "media" } else { fallback })
        .to_owned()
}

fn render_media_label(label: Option<&str>) -> String {
    label
        .filter(|label| !label.is_empty())
        .map(|label| format!("<span class=\"media-label\">{}</span>", escape_html(label)))
        .unwrap_or_default()
}

fn render_caption_span(caption: Option<&str>) -> String {
    caption
        .filter(|caption| !caption.is_empty())
        .map(|caption| format!("<span class=\"gallery-caption\">{}</span>", escape_html(caption)))
        .unwrap_or_default()
}

fn media_url(media_prefix: Option<&str>, post_id: i64, block_id: i64) -> String {
    match media_prefix {
        Some(prefix) => format!("{prefix}/posts/{post_id}/blocks/{block_id}/media"),
        None => format!("/posts/{post_id}/blocks/{block_id}/media"),
    }
}

fn render_tags_html(tags: &[String], guest: bool) -> String {
    if tags.is_empty() {
        String::new()
    } else {
        format!(
            "<p class=\"tags\"><span class=\"tag-label\">Tags:</span> {}</p>",
            tags.iter()
                .map(|tag| {
                    if guest {
                        format!("<span class=\"tag\">{}</span>", escape_html(tag))
                    } else {
                        format!(
                            "<a class=\"tag\" href=\"/tags?tag={}\">{}</a>",
                            encode_url_component(tag),
                            escape_html(tag),
                        )
                    }
                })
                .collect::<Vec<_>>()
                .join(" "),
        )
    }
}

fn render_caption(caption: Option<&str>) -> String {
    caption
        .filter(|caption| !caption.is_empty())
        .map(|caption| format!("<figcaption>{}</figcaption>", escape_html(caption)))
        .unwrap_or_default()
}

fn html_head(title: &str) -> String {
    format!(
        "<meta charset=\"utf-8\"><meta name=\"viewport\" content=\"width=device-width, initial-scale=1\"><title>{}</title><link rel=\"stylesheet\" href=\"/site.css\"><script src=\"/site.js\" defer></script>",
        escape_html(title),
    )
}

// Format adjacent markup tokens while leaving text nodes unchanged.
fn pretty_html(markup: &str) -> String {
    let mut formatted = String::with_capacity(markup.len() + markup.len() / 3);
    let mut depth = 0usize;
    let mut cursor = 0usize;
    let mut previous_was_tag = false;

    while cursor < markup.len() {
        if markup.as_bytes()[cursor] != b'<' {
            let next_tag = markup[cursor..]
                .find('<')
                .map(|offset| cursor + offset)
                .unwrap_or(markup.len());
            formatted.push_str(&markup[cursor..next_tag]);
            cursor = next_tag;
            previous_was_tag = false;
            continue;
        }

        let mut end = cursor + 1;
        let mut quote = None;
        while end < markup.len() {
            let character = markup.as_bytes()[end];
            match (quote, character) {
                (Some(active), byte) if active == byte => quote = None,
                (None, b'\'' | b'"') => quote = Some(character),
                (None, b'>') => break,
                _ => {}
            }
            end += 1;
        }
        if end == markup.len() {
            formatted.push_str(&markup[cursor..]);
            break;
        }

        let tag = &markup[cursor..=end];
        let closing = tag.starts_with("</");
        let special = tag.starts_with("<!") || tag.starts_with("<?");
        let name_start = if closing { 2 } else { 1 };
        let name_end = tag[name_start..]
            .find(|character: char| !(character.is_ascii_alphanumeric() || character == '-'))
            .map(|offset| name_start + offset)
            .unwrap_or(tag.len() - 1);
        let name = &tag[name_start..name_end];
        let self_closing = tag[..tag.len() - 1].trim_end().ends_with('/');

        if closing {
            depth = depth.saturating_sub(1);
        }
        if previous_was_tag {
            formatted.push('\n');
            for _ in 0..depth {
                formatted.push_str("  ");
            }
        }
        formatted.push_str(tag);

        if !closing
            && !special
            && !self_closing
            && !matches!(name, "area" | "base" | "br" | "col" | "embed" | "hr" | "img" | "input" | "link" | "meta" | "param" | "source" | "track" | "wbr")
        {
            depth += 1;
        }

        cursor = end + 1;
        previous_was_tag = true;
    }

    if !formatted.ends_with('\n') {
        formatted.push('\n');
    }
    formatted
}

const SLIDESHOW_HTML: &str = "<dialog id=\"slideshow\" class=\"slideshow\" aria-label=\"Photo slideshow\"><button class=\"slideshow-close\" type=\"button\" aria-label=\"Close slideshow\">×</button><div class=\"slideshow-stage\"><button class=\"slideshow-nav slideshow-previous\" type=\"button\" aria-label=\"Previous item\">‹</button><div class=\"slideshow-media\" id=\"slideshow-media\"></div><button class=\"slideshow-nav slideshow-next\" type=\"button\" aria-label=\"Next item\">›</button></div><p class=\"slideshow-label\" id=\"slideshow-label\"></p><p class=\"slideshow-caption\" id=\"slideshow-caption\"></p></dialog>";

const PUBLISH_DIALOG_HTML: &str = "<dialog id=\"publish-dialog\" class=\"publish-dialog\" aria-labelledby=\"publish-dialog-heading\"><button class=\"publish-dialog-close\" type=\"button\" aria-label=\"Close publish dialog\">×</button><h2 id=\"publish-dialog-heading\">Publish this post?</h2><form id=\"publish-form\" novalidate><label class=\"publish-override-toggle\"><input id=\"publish-use-time\" type=\"checkbox\">Choose a publication date and time</label><div class=\"publish-time-override\" id=\"publish-time-override\" hidden><label for=\"publish-time\">Local date and time</label><input id=\"publish-time\" type=\"datetime-local\" step=\"60\"></div><p id=\"publish-confirmation\" class=\"publish-confirmation\" role=\"status\" aria-live=\"polite\">The post will be published now using the server time.</p><p id=\"publish-error\" class=\"publish-error\" role=\"alert\" hidden></p><div class=\"publish-dialog-actions\"><button id=\"publish-cancel\" type=\"button\">Cancel</button><button id=\"publish-submit\" type=\"submit\">Publish now</button></div></form></dialog>";

const SHARE_DIALOG_HTML: &str = "<dialog id=\"share-dialog\" class=\"share-dialog\" aria-labelledby=\"share-dialog-heading\"><button class=\"share-dialog-close\" type=\"button\" aria-label=\"Close share panel\">×</button><h2 id=\"share-dialog-heading\">Share post</h2><div class=\"share-preview-actions\"><a id=\"share-full-preview\" href=\"#\" target=\"_blank\" rel=\"noopener\">Open full preview</a></div><div class=\"share-preview-frame\"><iframe id=\"share-preview\" title=\"Guest page preview\" loading=\"lazy\"></iframe></div><div class=\"share-expiry-slot\"><p class=\"share-expiry\" id=\"share-expiry\" hidden></p></div><div class=\"share-controls\"><div class=\"share-copy-row\"><button class=\"share-copy-button\" id=\"share-copy\" type=\"button\">Copy share link</button><span class=\"share-status\" id=\"share-status\" role=\"status\" aria-live=\"polite\"></span></div><div class=\"share-revoke-slot\"><button class=\"share-revoke-button\" id=\"share-revoke\" type=\"button\" hidden>Revoke link</button></div></div></dialog>";

const SITE_CSS: &str = include_str!("../static/site.css");
const SITE_JS: &str = include_str!("../static/site.js");

fn escape_html(value: &str) -> String {
    let mut escaped = String::with_capacity(value.len());
    for character in value.chars() {
        match character {
            '&' => escaped.push_str("&amp;"),
            '<' => escaped.push_str("&lt;"),
            '>' => escaped.push_str("&gt;"),
            '"' => escaped.push_str("&quot;"),
            '\'' => escaped.push_str("&#39;"),
            _ => escaped.push(character),
        }
    }
    escaped
}

async fn media<S: StorageClient>(
    State(state): State<AppState<S>>,
    Path((post_id, block_id)): Path<(i64, i64)>,
    Extension(principal): Extension<AuthPrincipal>,
    method: Method,
    headers: HeaderMap,
) -> Response {
    let media = match state
        .database
        .media_reference_with_access(post_id, block_id, principal.post_access())
        .await
    {
        Ok(Some(media)) => media,
        Ok(None) => return StatusCode::NOT_FOUND.into_response(),
        Err(error) => {
            eprintln!("website media lookup failed: {error}");
            return StatusCode::INTERNAL_SERVER_ERROR.into_response();
        }
    };
    proxy_media(&state.storage, media, method, headers).await
}

async fn proxy_media<S: StorageClient>(
    storage: &S,
    media: MediaReference,
    method: Method,
    headers: HeaderMap,
) -> Response {
    let head = method == Method::HEAD;
    let range = if media.content_type.starts_with("video/") && !head {
        let mut values = headers.get_all(header::RANGE).iter();
        let value = values.next();
        if values.next().is_some() {
            return StatusCode::BAD_REQUEST.into_response();
        }
        match value {
            Some(value) => match value.to_str() {
                Ok(value) => Some(value),
                Err(_) => return StatusCode::BAD_REQUEST.into_response(),
            },
            None => None,
        }
    } else {
        None
    };

    let stored = match storage.get(&media.storage_key, range, head).await {
        Ok(stored) => stored,
        Err(error) => {
            eprintln!("website storage request failed: {error}");
            return StatusCode::BAD_GATEWAY.into_response();
        }
    };
    let mut builder = Response::builder()
        .status(stored.status)
        .header(header::CONTENT_TYPE, media.content_type)
        .header(header::X_CONTENT_TYPE_OPTIONS, "nosniff");
    for name in [header::CONTENT_LENGTH, header::ACCEPT_RANGES, header::CONTENT_RANGE] {
        if let Some(value) = stored.headers.get(&name) {
            builder = builder.header(name, value);
        }
    }
    if head {
        return builder.body(Body::empty()).unwrap_or_else(|_| {
            StatusCode::INTERNAL_SERVER_ERROR.into_response()
        });
    }
    let body = Body::from_stream(stored.body);
    builder.body(body).unwrap_or_else(|_| {
        StatusCode::INTERNAL_SERVER_ERROR.into_response()
    })
}

#[cfg(test)]
pub fn state<S: StorageClient>(database: Database, storage: S) -> AppState<S> {
    AppState {
        database,
        storage,
        security: SiteSecurity {
            public_origin: None,
            secure_cookie: false,
            session_lifetime: Duration::from_secs(7 * 24 * 60 * 60),
        },
    }
}

pub fn state_with_security<S: StorageClient>(
    database: Database,
    storage: S,
    security: SiteSecurity,
) -> AppState<S> {
    AppState { database, storage, security }
}

#[cfg(test)]
mod tests {
    use std::{
        net::{IpAddr, Ipv4Addr, SocketAddr},
        path::{Path, PathBuf},
        time::{Duration, SystemTime, UNIX_EPOCH},
    };

    use rusqlite::Connection;

    use super::{
        escape_html, feed, home, parse_cursor, render_full_post, router, state,
        valid_return_target, AuthPrincipal, FeedQuery,
    };
    use crate::{
        auth,
        db::{AccountRole, Database, NewPost, Post, PostAccess, PostBlock, PostSummary, ShareAccess, SidebarData},
        storage::{StorageClient, StorageResponse},
    };
    use axum::{
        body::{to_bytes, Body},
        extract::{ConnectInfo, Extension, Query, State},
        http::{header, Request, StatusCode},
        response::Response,
    };
    use tower::ServiceExt;

    #[derive(Clone)]
    struct UnusedStorage;

    impl StorageClient for UnusedStorage {
        async fn put_file(&self, _content_type: &str, _path: &Path) -> Result<String, String> {
            unreachable!()
        }

        async fn get(
            &self,
            _key: &str,
            _range: Option<&str>,
            _head: bool,
        ) -> Result<StorageResponse, String> {
            unreachable!()
        }
    }

    async fn get_route(app: &axum::Router, target: &str) -> Response {
        app.clone()
            .oneshot(
                Request::builder()
                    .uri(target)
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap()
    }

    #[tokio::test]
    async fn draft_http_api_creates_and_lists_author_owned_drafts() {
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let path = std::env::temp_dir().join(format!(
            "journey-site-drafts-{}-{nonce}.sqlite3",
            std::process::id()
        ));
        let database = Database::new(path.clone());
        database.initialize().await.unwrap();
        for (username, role) in [
            ("writer-one", AccountRole::Write),
            ("writer-two", AccountRole::Write),
            ("reader-one", AccountRole::Read),
            ("admin-one", AccountRole::Admin),
        ] {
            database
                .create_account(username.to_owned(), role, "test-hash".to_owned())
                .await
                .unwrap();
        }
        database
            .create_draft(
                "writer-two".to_owned(),
                "Another draft".to_owned(),
                String::new(),
                Vec::new(),
                Vec::new(),
            )
            .await
            .unwrap();

        async fn session_cookie(database: &Database, username: &str) -> String {
            let account = database
                .login_account(username.to_owned())
                .await
                .unwrap()
                .unwrap();
            let token = auth::new_session_token();
            let created_at = super::unix_time();
            database
                .issue_session(
                    account.id,
                    auth::session_token_digest(&token),
                    created_at,
                    created_at + Duration::from_secs(3600),
                )
                .await
                .unwrap();
            format!("journey_session={token}")
        }

        async fn request(
            app: &axum::Router,
            method: &str,
            target: &str,
            cookie: &str,
            body: Option<&str>,
        ) -> Response {
            let mut builder = Request::builder()
                .method(method)
                .uri(target)
                .header(header::COOKIE, cookie);
            if method != "GET" {
                builder = builder
                    .header(header::ORIGIN, "http://127.0.0.1")
                    .header(header::CONTENT_TYPE, "application/json");
            }
            app.clone()
                .oneshot(builder.body(Body::from(body.unwrap_or_default().to_owned())).unwrap())
                .await
                .unwrap()
        }

        let writer_cookie = session_cookie(&database, "writer-one").await;
        let reader_cookie = session_cookie(&database, "reader-one").await;
        let admin_cookie = session_cookie(&database, "admin-one").await;
        let app = router(state(database.clone(), UnusedStorage));
        let unauthenticated_new_post = get_route(&app, "/posts/new").await;
        assert_eq!(unauthenticated_new_post.status(), StatusCode::SEE_OTHER);
        assert_eq!(
            unauthenticated_new_post.headers().get(header::LOCATION).unwrap(),
            "/login?return_to=%2Fposts%2Fnew",
        );
        assert_eq!(
            request(&app, "GET", "/posts/new", &reader_cookie, None)
                .await
                .status(),
            StatusCode::FORBIDDEN,
        );
        let created = request(
            &app,
            "POST",
            "/api/posts",
            &writer_cookie,
            Some(r#"{"title":"My draft","summary":"Short intro","tags":["road","field-notes"],"blocks":[{"header":"First","blocks":[{"body":"Child"}]},{"body":"Second"}]}"#),
        )
        .await;
        assert_eq!(created.status(), StatusCode::CREATED);
        assert_eq!(created.headers().get(header::LOCATION).unwrap(), "/posts/2");
        let body = to_bytes(created.into_body(), usize::MAX).await.unwrap();
        let response: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(response["id"], 2);
        let post = database
            .post_with_access(2, PostAccess::Author("writer-one".to_owned()))
            .await
            .unwrap()
            .unwrap();
        assert_eq!(post.author_username, "writer-one");
        assert_eq!(post.summary.published_at, None);
        assert_eq!(post.summary.summary, "Short intro");
        assert_eq!(post.tags, vec!["road".to_owned(), "field-notes".to_owned()]);
        assert_eq!(post.blocks.len(), 2);
        assert_eq!(post.blocks[0].children[0].body.as_deref(), Some("Child"));
        assert_eq!(post.blocks[1].body.as_deref(), Some("Second"));
        assert!(!super::render_post(&post, true, false).contains("<time"));

        let writer_creation_page = request(&app, "GET", "/posts/new", &writer_cookie, None).await;
        assert_eq!(writer_creation_page.status(), StatusCode::OK);
        let body = to_bytes(writer_creation_page.into_body(), usize::MAX).await.unwrap();
        let writer_creation_html = String::from_utf8(body.to_vec()).unwrap();
        assert!(writer_creation_html.contains("id=\"draft-form\""));
        assert!(writer_creation_html.contains("id=\"draft-title\" name=\"title\" type=\"text\""));
        assert!(writer_creation_html.contains("data-block-action=\"add-root\""));
        assert!(writer_creation_html.contains("Create draft"));
        assert!(writer_creation_html.contains("published from their post page"));
        assert!(!writer_creation_html.contains("class=\"new-post-float\""));

        assert_eq!(
            request(&app, "GET", "/posts/new", &admin_cookie, None)
                .await
                .status(),
            StatusCode::OK,
        );

        let writer_home = request(&app, "GET", "/", &writer_cookie, None).await;
        let body = to_bytes(writer_home.into_body(), usize::MAX).await.unwrap();
        let writer_home_html = String::from_utf8(body.to_vec()).unwrap();
        assert!(writer_home_html.contains("class=\"new-post-float\" href=\"/posts/new\""));
        assert!(writer_home_html.contains("<h2 id=\"sidebar-drafts-heading\">Drafts</h2>"));
        assert!(writer_home_html.contains("href=\"/posts/2\">My draft</a>"));
        assert!(!writer_home_html.contains("Another draft"));

        let reader_home = request(&app, "GET", "/", &reader_cookie, None).await;
        let body = to_bytes(reader_home.into_body(), usize::MAX).await.unwrap();
        let reader_home_html = String::from_utf8(body.to_vec()).unwrap();
        assert!(!reader_home_html.contains("class=\"new-post-float\""));
        assert!(!reader_home_html.contains("sidebar-drafts-heading"));

        let created_post_page = request(&app, "GET", "/posts/2?created=1", &writer_cookie, None).await;
        let body = to_bytes(created_post_page.into_body(), usize::MAX).await.unwrap();
        assert!(String::from_utf8(body.to_vec()).unwrap().contains("data-created-confirmation"));

        let published_feed = request(&app, "GET", "/api/posts", &writer_cookie, None).await;
        let body = to_bytes(published_feed.into_body(), usize::MAX).await.unwrap();
        let published_feed: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert!(published_feed["posts"].as_array().unwrap().is_empty());

        let empty_draft = request(
            &app,
            "POST",
            "/api/posts",
            &admin_cookie,
            Some(r#"{"title":"Empty draft"}"#),
        )
        .await;
        assert_eq!(empty_draft.status(), StatusCode::CREATED);
        let empty_post = database
            .post_with_access(3, PostAccess::Admin)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(empty_post.author_username, "admin-one");
        assert_eq!(empty_post.summary.summary, "");
        assert!(empty_post.tags.is_empty());
        assert!(empty_post.blocks.is_empty());

        let admin_home = request(&app, "GET", "/", &admin_cookie, None).await;
        let body = to_bytes(admin_home.into_body(), usize::MAX).await.unwrap();
        let admin_home_html = String::from_utf8(body.to_vec()).unwrap();
        assert!(admin_home_html.contains("class=\"new-post-float\" href=\"/posts/new\""));
        assert!(admin_home_html.contains("href=\"/posts/1\">Another draft</a>"));
        assert!(admin_home_html.contains("href=\"/posts/2\">My draft</a>"));
        assert!(admin_home_html.contains("href=\"/posts/3\">Empty draft</a>"));

        let writer_drafts = request(&app, "GET", "/api/drafts", &writer_cookie, None).await;
        assert_eq!(writer_drafts.status(), StatusCode::OK);
        let body = to_bytes(writer_drafts.into_body(), usize::MAX).await.unwrap();
        let drafts: Vec<serde_json::Value> = serde_json::from_slice(&body).unwrap();
        assert_eq!(drafts.len(), 1);
        assert_eq!(drafts[0]["id"], 2);
        assert!(drafts[0]["published_at"].is_null());

        let admin_drafts = request(&app, "GET", "/api/drafts", &admin_cookie, None).await;
        assert_eq!(admin_drafts.status(), StatusCode::OK);
        let body = to_bytes(admin_drafts.into_body(), usize::MAX).await.unwrap();
        let drafts: Vec<serde_json::Value> = serde_json::from_slice(&body).unwrap();
        assert_eq!(drafts.iter().map(|draft| draft["id"].as_i64().unwrap()).collect::<Vec<_>>(), vec![3, 2, 1]);

        assert_eq!(
            request(&app, "GET", "/api/drafts", &reader_cookie, None)
                .await
                .status(),
            StatusCode::FORBIDDEN,
        );
        assert_eq!(
            request(
                &app,
                "POST",
                "/api/posts",
                &reader_cookie,
                Some(r#"{"title":"No draft"}"#),
            )
            .await
            .status(),
            StatusCode::FORBIDDEN,
        );
        assert_eq!(
            request(
                &app,
                "POST",
                "/api/posts",
                &writer_cookie,
                Some(r#"{"title":"Media","blocks":[{"path":"photo.jpg"}]}"#),
            )
            .await
            .status(),
            StatusCode::UNPROCESSABLE_ENTITY,
        );
        assert_eq!(
            request(
                &app,
                "POST",
                "/api/posts",
                &writer_cookie,
                Some(r#"{"title":"Forged author","author":"writer-two"}"#),
            )
            .await
            .status(),
            StatusCode::UNPROCESSABLE_ENTITY,
        );
        assert_eq!(
            request(
                &app,
                "POST",
                "/api/posts",
                &writer_cookie,
                Some(r#"{"title":"  "}"#),
            )
            .await
            .status(),
            StatusCode::BAD_REQUEST,
        );
        assert_eq!(
            request(
                &app,
                "POST",
                "/api/posts",
                &writer_cookie,
                Some(r#"{"title":"Too deep","blocks":[{"blocks":[{"blocks":[{"body":"No"}]}]}]}"#),
            )
            .await
            .status(),
            StatusCode::BAD_REQUEST,
        );

        let connection = Connection::open(&path).unwrap();
        connection
            .execute_batch(
                "INSERT INTO posts (author_id, title, published_at, summary, published, tags) \
                 VALUES ((SELECT id FROM users WHERE username = 'writer-one'), 'Writer published', 1767225600, '', 1, '[]'); \
                 INSERT INTO posts (author_id, title, published_at, summary, published, tags) \
                 VALUES ((SELECT id FROM users WHERE username = 'writer-two'), 'Other published', 1767312000, '', 1, '[]');",
            )
            .unwrap();
        drop(connection);
        let writer_link = request(
            &app,
            "POST",
            "/api/posts/4/share-links",
            &writer_cookie,
            None,
        )
        .await;
        assert_eq!(writer_link.status(), StatusCode::OK);
        let body = to_bytes(writer_link.into_body(), usize::MAX).await.unwrap();
        let writer_link: serde_json::Value = serde_json::from_slice(&body).unwrap();
        let writer_link_id = writer_link["id"].as_str().unwrap();
        assert_eq!(
            request(
                &app,
                "POST",
                "/api/posts/5/share-links",
                &writer_cookie,
                None,
            )
            .await
            .status(),
            StatusCode::NOT_FOUND,
        );
        assert_eq!(
            request(
                &app,
                "POST",
                "/api/posts/4/share-links",
                &reader_cookie,
                None,
            )
            .await
            .status(),
            StatusCode::FORBIDDEN,
        );
        assert_eq!(
            request(
                &app,
                "POST",
                "/api/posts/2/share-links",
                &writer_cookie,
                None,
            )
            .await
            .status(),
            StatusCode::NOT_FOUND,
        );
        let admin_link = request(
            &app,
            "POST",
            "/api/posts/5/share-links",
            &admin_cookie,
            None,
        )
        .await;
        assert_eq!(admin_link.status(), StatusCode::OK);
        let body = to_bytes(admin_link.into_body(), usize::MAX).await.unwrap();
        let admin_link: serde_json::Value = serde_json::from_slice(&body).unwrap();
        let admin_link_id = admin_link["id"].as_str().unwrap();
        assert_eq!(
            request(
                &app,
                "DELETE",
                &format!("/api/share-links/{admin_link_id}"),
                &writer_cookie,
                None,
            )
            .await
            .status(),
            StatusCode::NOT_FOUND,
        );
        assert_eq!(
            request(
                &app,
                "DELETE",
                &format!("/api/share-links/{writer_link_id}"),
                &reader_cookie,
                None,
            )
            .await
            .status(),
            StatusCode::FORBIDDEN,
        );
        assert_eq!(
            request(
                &app,
                "DELETE",
                &format!("/api/share-links/{writer_link_id}"),
                &writer_cookie,
                None,
            )
            .await
            .status(),
            StatusCode::NO_CONTENT,
        );
        assert_eq!(
            request(
                &app,
                "DELETE",
                &format!("/api/share-links/{admin_link_id}"),
                &admin_cookie,
                None,
            )
            .await
            .status(),
            StatusCode::NO_CONTENT,
        );
        assert_eq!(
            request(&app, "GET", "/posts/4/share-preview", &writer_cookie, None)
                .await
                .status(),
            StatusCode::OK,
        );
        assert_eq!(
            request(&app, "GET", "/posts/4/share-preview", &reader_cookie, None)
                .await
                .status(),
            StatusCode::FORBIDDEN,
        );
        assert_eq!(
            request(&app, "GET", "/posts/4/share-preview", &admin_cookie, None)
                .await
                .status(),
            StatusCode::OK,
        );

        std::fs::remove_file(path).unwrap();
    }

    #[tokio::test]
    async fn malformed_feed_cursor_returns_bad_request() {
        let response = feed(
            State(state(Database::new(PathBuf::from("unused-test-database")), UnusedStorage)),
            Query(FeedQuery {
                limit: None,
                after: Some("not-a-second:4".to_owned()),
                tag: None,
                month: None,
            }),
            Extension(AuthPrincipal {
                username: "reader".to_owned(),
                role: AccountRole::Read,
                timezone: "UTC".to_owned(),
            }),
        )
        .await;
        assert_eq!(response.status(), StatusCode::BAD_REQUEST);
        assert!(parse_cursor("1772284800:4").is_some());
        assert!(parse_cursor("01772284800:4").is_none());
        assert!(parse_cursor("1772284800:04").is_none());
        assert!(parse_cursor("1772284800:4:5").is_none());
    }

    #[tokio::test]
    async fn login_assets_are_public_and_content_data_stays_protected() {
        let app = router(state(
            Database::new(PathBuf::from("unused-route-test-database")),
            UnusedStorage,
        ));

        for path in [
            "/login?return_to=https%3A%2F%2Fexample.com%2F",
            "/login?return_to=%",
            "/site.css",
            "/site.js",
        ] {
            assert_eq!(get_route(&app, path).await.status(), StatusCode::OK);
        }

        let login = get_route(&app, "/login?return_to=https%3A%2F%2Fexample.com%2F").await;
        let body = to_bytes(login.into_body(), usize::MAX).await.unwrap();
        let html = String::from_utf8(body.to_vec()).unwrap();
        assert!(html.contains("name=\"return_to\" value=\"/\""));

        let home = get_route(&app, "/").await;
        assert_eq!(home.status(), StatusCode::SEE_OTHER);
        assert_eq!(
            home.headers().get("location").unwrap(),
            "/login?return_to=%2F"
        );
        let oversized_page_query = format!("/?q={}", "x".repeat(2050));
        let oversized_page = get_route(&app, &oversized_page_query).await;
        assert_eq!(oversized_page.status(), StatusCode::SEE_OTHER);
        assert_eq!(
            oversized_page.headers().get("location").unwrap(),
            "/login?return_to=%2F"
        );

        for path in [
            "/api/posts",
            "/posts/9/fragment",
            "/posts/9/blocks/1/media",
        ] {
            assert_eq!(get_route(&app, path).await.status(), StatusCode::UNAUTHORIZED);
        }
    }

    #[tokio::test]
    async fn html_login_logout_and_draft_access_use_the_shared_sessions() {
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let path = std::env::temp_dir().join(format!(
            "journey-site-auth-ui-{}-{nonce}.sqlite3",
            std::process::id()
        ));
        let database = Database::new(path.clone());
        database.initialize().await.unwrap();
        database
            .create_account(
                "reader".to_owned(),
                AccountRole::Read,
                auth::hash_password("reader-secret").unwrap(),
            )
            .await
            .unwrap();
        database
            .create_account(
                "owner".to_owned(),
                AccountRole::Write,
                auth::hash_password("owner-secret").unwrap(),
            )
            .await
            .unwrap();
        let connection = Connection::open(&path).unwrap();
        connection
            .execute(
                "INSERT INTO posts (author_id, title, published_at, summary, published, tags) \
                 VALUES ((SELECT id FROM users WHERE username = 'owner'), 'Draft', NULL, 'A draft', 0, '[]')",
                [],
            )
            .unwrap();
        drop(connection);
        let app = router(state(database.clone(), UnusedStorage));
        let peer_address = SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), 8080);

        let mut wrong_login = Request::builder()
            .method("POST")
            .uri("/login")
            .header(header::ORIGIN, "http://127.0.0.1")
            .header(header::CONTENT_TYPE, "application/x-www-form-urlencoded")
            .body(Body::from("username=reader&password=wrong&return_to=%2Fposts%2F1"))
            .unwrap();
        wrong_login
            .extensions_mut()
            .insert(ConnectInfo(peer_address));
        let failure = app.clone().oneshot(wrong_login).await.unwrap();
        assert_eq!(failure.status(), StatusCode::UNAUTHORIZED);
        let body = to_bytes(failure.into_body(), usize::MAX).await.unwrap();
        let html = String::from_utf8(body.to_vec()).unwrap();
        assert!(html.contains("Invalid username or password."));
        assert!(html.contains("<form action=\"/login\" method=\"post\">"));
        assert!(html.contains("<label for=\"login-username\">Username</label>"));
        assert!(html.contains("name=\"username\" type=\"text\" value=\"reader\""));
        assert!(html.contains("name=\"password\" type=\"password\" autocomplete=\"current-password\""));
        assert!(html.contains("autocomplete=\"username\""));
        assert!(html.contains("maxlength=\"1024\" required"));
        assert!(!html.contains("name=\"password\" type=\"password\" value="));

        async fn sign_in(
            app: &axum::Router,
            username: &str,
            password: &str,
            peer_address: SocketAddr,
        ) -> (String, Response) {
            let mut request = Request::builder()
                .method("POST")
                .uri("/login")
                .header(header::ORIGIN, "http://127.0.0.1")
                .header(header::CONTENT_TYPE, "application/x-www-form-urlencoded")
                .body(Body::from(format!(
                    "username={username}&password={password}&return_to=%2Fposts%2F1"
                )))
                .unwrap();
            request
                .extensions_mut()
                .insert(ConnectInfo(peer_address));
            let response = app.clone().oneshot(request).await.unwrap();
            let cookie = response
                .headers()
                .get(header::SET_COOKIE)
                .unwrap()
                .to_str()
                .unwrap()
                .split(';')
                .next()
                .unwrap()
                .to_owned();
            (cookie, response)
        }

        let (reader_cookie, reader_login) =
            sign_in(&app, "reader", "reader-secret", peer_address).await;
        assert_eq!(reader_login.status(), StatusCode::SEE_OTHER);
        assert_eq!(reader_login.headers().get(header::LOCATION).unwrap(), "/posts/1");
        let already_signed_in = app
            .clone()
            .oneshot(
                Request::builder()
                    .uri("/login?return_to=%2Fposts%2F1")
                    .header(header::COOKIE, reader_cookie.as_str())
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(already_signed_in.status(), StatusCode::SEE_OTHER);
        assert_eq!(
            already_signed_in.headers().get(header::LOCATION).unwrap(),
            "/posts/1"
        );
        let draft_as_reader = app
            .clone()
            .oneshot(
                Request::builder()
                    .uri("/posts/1")
                    .header(header::COOKIE, reader_cookie.as_str())
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(draft_as_reader.status(), StatusCode::NOT_FOUND);

        let (owner_cookie, owner_login) =
            sign_in(&app, "owner", "owner-secret", peer_address).await;
        assert_eq!(owner_login.status(), StatusCode::SEE_OTHER);
        let draft_as_owner = app
            .clone()
            .oneshot(
                Request::builder()
                    .uri("/posts/1")
                    .header(header::COOKIE, owner_cookie.as_str())
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(draft_as_owner.status(), StatusCode::OK);
        let owner_body = to_bytes(draft_as_owner.into_body(), usize::MAX).await.unwrap();
        assert!(String::from_utf8(owner_body.to_vec()).unwrap().contains("Draft"));

        let logout = app
            .clone()
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/logout")
                    .header(header::ORIGIN, "http://127.0.0.1")
                    .header(header::COOKIE, reader_cookie.as_str())
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(logout.status(), StatusCode::SEE_OTHER);
        assert_eq!(logout.headers().get(header::LOCATION).unwrap(), "/login");
        assert!(logout
            .headers()
            .get(header::SET_COOKIE)
            .unwrap()
            .to_str()
            .unwrap()
            .contains("Max-Age=0"));

        let current = app
            .clone()
            .oneshot(
                Request::builder()
                    .uri("/api/auth/current")
                    .header(header::COOKIE, reader_cookie.as_str())
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(current.status(), StatusCode::UNAUTHORIZED);

        let expired_token = "a".repeat(43);
        let reader = database.login_account("reader".to_owned()).await.unwrap().unwrap();
        database
            .issue_session(
                reader.id,
                auth::session_token_digest(&expired_token),
                Duration::from_secs(1),
                Duration::from_secs(2),
            )
            .await
            .unwrap();
        let expired = app
            .clone()
            .oneshot(
                Request::builder()
                    .uri("/")
                    .header(header::COOKIE, format!("journey_session={expired_token}"))
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(expired.status(), StatusCode::SEE_OTHER);
        let expired_api = app
            .oneshot(
                Request::builder()
                    .uri("/api/posts")
                    .header(header::COOKIE, format!("journey_session={expired_token}"))
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(expired_api.status(), StatusCode::UNAUTHORIZED);

        std::fs::remove_file(path).unwrap();
    }

    #[tokio::test]
    async fn initial_home_request_renders_the_newest_full_post_and_cursor() {
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let path = std::env::temp_dir().join(format!(
            "journey-site-home-{}-{nonce}.sqlite3",
            std::process::id()
        ));
        let database = Database::new(path.clone());
        let posts = (1..=12)
            .map(|id| NewPost {
                author_username: "test-author".to_owned(),
                title: format!("Post {id}"),
                published_at: Some("2026-01-01T12:00:00Z".parse::<jiff::Timestamp>().unwrap().as_second()),
                summary: format!("Summary {id}"),
                tags: Vec::new(),
                blocks: Vec::new(),
            })
            .collect();
        database.replace_posts(posts).await.unwrap();

        let response = home(
            State(state(database, UnusedStorage)),
            Extension(AuthPrincipal {
                username: "reader".to_owned(),
                role: AccountRole::Read,
                timezone: "UTC".to_owned(),
            }),
        )
        .await;
        assert_eq!(response.status(), StatusCode::OK);
        let body = to_bytes(response.into_body(), usize::MAX).await.unwrap();
        let html = String::from_utf8(body.to_vec()).unwrap();
        assert_eq!(html.matches("<article class=\"post\"").count(), 1);
        assert!(html.contains("data-next-cursor=\"1767268800:12\""));
        assert!(html.contains("Post 12"));
        assert!(!html.contains("data-post-id=\"11\""));
        assert!(html.contains("id=\"load-more\""));

        std::fs::remove_file(path).unwrap();
    }

    #[test]
    fn full_post_html_escapes_text_and_uses_public_media_urls() {
        let post = Post {
            summary: PostSummary {
                id: 7,
                title: "\"><script>alert(1)</script>".to_owned(),
                published_at: Some("2026-01-01T12:00:00Z".parse::<jiff::Timestamp>().unwrap().as_second()),
                summary: "A <summary>".to_owned(),
            },
            published: true,
            author_username: "owner".to_owned(),
            tags: vec!["<tag>".to_owned()],
            blocks: vec![
                PostBlock {
                    id: 17,
                    position: 0,
                    header: Some("A <header>".to_owned()),
                    body: Some("<script>body</script>".to_owned()),
                    content_type: None,
                    alt: None,
                    children: Vec::new(),
                },
                PostBlock {
                    id: 18,
                    position: 1,
                    header: Some("Grouped media".to_owned()),
                    body: Some("<caption>".to_owned()),
                    content_type: Some("image/jpeg".to_owned()),
                    alt: None,
                    children: vec![PostBlock {
                        id: 19,
                        position: 0,
                        header: None,
                        body: Some("Nested body".to_owned()),
                        content_type: None,
                        alt: None,
                        children: Vec::new(),
                    }],
                },
                PostBlock {
                    id: 20,
                    position: 2,
                    header: None,
                    body: None,
                    content_type: Some("video/mp4".to_owned()),
                    alt: None,
                    children: Vec::new(),
                },
                PostBlock {
                    id: 21,
                    position: 3,
                    header: None,
                    body: Some("<caption>".to_owned()),
                    content_type: Some("image/jpeg".to_owned()),
                    alt: Some("photo\" onerror=\"alert(1)".to_owned()),
                    children: Vec::new(),
                },
            ],
        };
        let html = render_full_post(
            &SidebarData::default(),
            &post,
            "owner",
            AccountRole::Admin,
            false,
            Some(&ShareAccess::Admin),
        );
        assert!(html.contains("<p class=\"tags\">"));
        assert!(html.contains("class=\"tag-label\">Tags:</span>"));
        assert!(html.contains("href=\"/tags?tag=%3Ctag%3E\">&lt;tag&gt;</a>"));
        assert!(html.contains("<h2>A &lt;header&gt;</h2>"));
        assert!(html.contains("&lt;script&gt;body&lt;/script&gt;"));
        assert!(html.contains("&quot;&gt;&lt;script&gt;alert(1)&lt;/script&gt;"));
        assert!(html.contains("alt=\"photo&quot; onerror=&quot;alert(1)\""));
        assert!(html.contains("<img src=\"/posts/7/blocks/21/media\""));
        assert!(html.contains("<video controls preload=\"metadata\" aria-label=\"video\">"));
        assert!(html.contains("<source src=\"/posts/7/blocks/20/media\""));
        assert!(html.contains("<figcaption>&lt;caption&gt;</figcaption>"));
        assert!(html.contains("<p>Nested body</p>"));
        assert!(!html.contains("/posts/7/blocks/18/media"));
        assert!(!html.contains("<script>body</script>"));
        assert!(html.contains("Signed in as <strong>owner</strong>"));
        assert!(html.contains("action=\"/logout\" method=\"post\""));
        assert!(!super::render_shared_post(&post, "/share/example").contains("new-post-float"));
        assert_eq!(escape_html("'&\"<>"), "&#39;&amp;&quot;&lt;&gt;");
    }

    #[test]
    fn return_targets_are_limited_to_local_content_pages() {
        assert_eq!(valid_return_target("/"), Some("/".to_owned()));
        assert_eq!(
            valid_return_target("/tags?tag=coast%20walks"),
            Some("/tags?tag=coast%20walks".to_owned())
        );
        assert_eq!(
            valid_return_target("/archive/2026-09"),
            Some("/archive/2026-09".to_owned())
        );
        assert_eq!(valid_return_target("/posts/42"), Some("/posts/42".to_owned()));
        assert_eq!(valid_return_target("/posts/new"), Some("/posts/new".to_owned()));
        assert!(valid_return_target("https://example.com/").is_none());
        assert!(valid_return_target("//example.com/").is_none());
        assert!(valid_return_target("/api/posts").is_none());
        assert!(valid_return_target("/posts/42/fragment").is_none());
        assert!(valid_return_target("/archive/2026-13").is_none());
    }

    #[test]
    fn sidebar_shows_five_newest_drafts_before_the_expand_control() {
        let sidebar = SidebarData {
            drafts: Some((1..=7).rev().map(|id| PostSummary {
                id,
                title: format!("Draft {id}"),
                published_at: None,
                summary: String::new(),
            }).collect()),
            ..SidebarData::default()
        };
        let html = super::render_sidebar(&sidebar);
        assert_eq!(html.matches("data-overflow-item=\"true\"").count(), 2);
        assert!(html.contains("data-sidebar-expand=\"sidebar-drafts\""));
        let newest = html.find("Draft 7").unwrap();
        let fifth = html.find("Draft 3").unwrap();
        let sixth = html.find("Draft 2").unwrap();
        assert!(newest < fifth);
        assert!(fifth < sixth);
        assert!(html.find("sidebar-drafts-heading").unwrap() < html.find("sidebar-recent-heading").unwrap());
    }
}
