use axum::{
    body::Body,
    extract::{ConnectInfo, Extension, Path, Query, Request, State},
    http::{header, HeaderMap, HeaderValue, Method, StatusCode, Uri},
    middleware::{self, Next},
    response::{Html, IntoResponse, Response},
    routing::{get, post},
    Json, Router,
};
use serde::{Deserialize, Serialize};
use std::{net::{IpAddr, SocketAddr}, time::{SystemTime, UNIX_EPOCH}};

use crate::{
    auth,
    db::{AccountRole, AuthenticatedAccount, Database, FeedCursor, Post, PostAccess, PostSummary, SidebarData},
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
    session_lifetime_seconds: i64,
}

#[derive(Clone)]
struct ParsedOrigin {
    key: String,
    scheme: String,
    host: String,
}

#[derive(Clone)]
struct AuthPrincipal {
    role: AccountRole,
}

#[derive(Deserialize)]
struct LoginRequest {
    username: String,
    password: String,
}

#[derive(Serialize)]
struct CurrentAccountResponse {
    username: String,
    role: AccountRole,
}

#[derive(Deserialize)]
struct FeedQuery {
    limit: Option<usize>,
    after: Option<String>,
    tag: Option<String>,
}

#[derive(Deserialize)]
struct TagPageQuery {
    tag: String,
}

impl SiteSecurity {
    pub fn from_env(bind_address: SocketAddr) -> Result<Self, String> {
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
        let secure_cookie = public_origin
            .as_ref()
            .is_some_and(|origin| origin.scheme == "https");
        Ok(Self { public_origin, secure_cookie, session_lifetime_seconds })
    }
}

pub fn router<S: StorageClient>(state: AppState<S>) -> Router {
    let content_routes = Router::new()
        .route("/", get(home::<S>))
        .route("/tags", get(tag_page::<S>))
        .route("/archive/{month}", get(archive_page::<S>))
        .route("/site.css", get(site_css))
        .route("/site.js", get(site_js))
        .route("/api/posts", get(feed::<S>))
        .route("/api/posts/{id}", get(api_full_post::<S>))
        .route("/posts/{id}/fragment", get(post_fragment::<S>))
        .route("/posts/{id}", get(post_page::<S>))
        .route(
            "/posts/{post_id}/blocks/{block_id}/media",
            get(media::<S>).head(media::<S>),
        )
        .route_layer(middleware::from_fn_with_state(state.clone(), authenticate::<S>));

    Router::new()
        .route("/api/auth/login", post(login::<S>))
        .route("/api/auth/current", get(current_account::<S>))
        .route("/api/auth/logout", post(logout::<S>))
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
        return no_store(StatusCode::UNAUTHORIZED.into_response());
    };
    let account = match lookup_session(&state, &token).await {
        Ok(Some(account)) => account,
        Ok(None) => return no_store(StatusCode::UNAUTHORIZED.into_response()),
        Err(error) => {
            eprintln!("website session lookup failed: {error}");
            return no_store(StatusCode::INTERNAL_SERVER_ERROR.into_response());
        }
    };
    request.extensions_mut().insert(AuthPrincipal::from(account));
    no_store(next.run(request).await)
}

async fn login<S: StorageClient>(
    State(state): State<AppState<S>>,
    ConnectInfo(peer_address): ConnectInfo<SocketAddr>,
    headers: HeaderMap,
    Json(credentials): Json<LoginRequest>,
) -> Response {
    if !origin_allowed(&headers, &state.security) {
        return no_store((StatusCode::FORBIDDEN, "origin not allowed\n").into_response());
    }
    if credentials.username.len() > 64 || credentials.password.len() > 1024 {
        return login_failure();
    }
    let username_throttle_key = auth::username_throttle_key(&credentials.username);
    let address_throttle_key = auth::address_throttle_key(&peer_address.ip().to_string());
    let now = unix_time_seconds();
    for (key, maximum_attempts) in [
        (username_throttle_key.clone(), 5),
        (address_throttle_key.clone(), 30),
    ] {
        match state
            .database
            .record_login_attempt(key, now, 15 * 60, maximum_attempts)
            .await
        {
            Ok(true) => {}
            Ok(false) => return login_failure(),
            Err(error) => {
                eprintln!("website login throttle failed: {error}");
                return no_store(StatusCode::INTERNAL_SERVER_ERROR.into_response());
            }
        }
    }

    let account = match state.database.login_account(credentials.username.clone()).await {
        Ok(account) => account,
        Err(error) => {
            eprintln!("website account lookup failed: {error}");
            return no_store(StatusCode::INTERNAL_SERVER_ERROR.into_response());
        }
    };
    let valid_username = auth::validate_username(&credentials.username).is_ok();
    let authenticated = match account.as_ref() {
        Some(account) => {
            let password_matches = auth::verify_password(&credentials.password, &account.password_hash);
            valid_username && account.enabled && password_matches
        }
        None => {
            auth::dummy_verify_password(&credentials.password);
            false
        }
    };
    if !authenticated {
        return login_failure();
    }
    let account = account.expect("successful authentication has an account");
    for key in [username_throttle_key, address_throttle_key] {
        if let Err(error) = state.database.clear_login_attempts(key).await {
            eprintln!("website login throttle reset failed: {error}");
            return no_store(StatusCode::INTERNAL_SERVER_ERROR.into_response());
        }
    }

    let token = auth::new_session_token();
    let created_at = unix_time_seconds();
    let expires_at = created_at.saturating_add(state.security.session_lifetime_seconds);
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
        return no_store(StatusCode::INTERNAL_SERVER_ERROR.into_response());
    }

    let mut response = Json(CurrentAccountResponse {
        username: account.username,
        role: account.role,
    })
    .into_response();
    set_session_cookie(&mut response, &state.security, &token);
    no_store(response)
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
    if !origin_allowed(&headers, &state.security) {
        return no_store((StatusCode::FORBIDDEN, "origin not allowed\n").into_response());
    }
    if let Some(token) = cookie_token(&headers) {
        if let Err(error) = state
            .database
            .revoke_session(auth::session_token_digest(&token))
            .await
        {
            eprintln!("website logout failed: {error}");
            return no_store(StatusCode::INTERNAL_SERVER_ERROR.into_response());
        }
    }
    let mut response = StatusCode::NO_CONTENT.into_response();
    expire_session_cookie(&mut response, &state.security);
    no_store(response)
}

async fn lookup_session<S: StorageClient>(
    state: &AppState<S>,
    token: &str,
) -> Result<Option<AuthenticatedAccount>, String> {
    state
        .database
        .authenticated_account(auth::session_token_digest(token), unix_time_seconds())
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

fn set_session_cookie(response: &mut Response, security: &SiteSecurity, token: &str) {
    let secure = if security.secure_cookie { "; Secure" } else { "" };
    let value = format!(
        "journey_session={token}; Path=/; HttpOnly; SameSite=Strict; Max-Age={}{secure}",
        security.session_lifetime_seconds,
    );
    if let Ok(value) = HeaderValue::from_str(&value) {
        response.headers_mut().insert(header::SET_COOKIE, value);
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

fn origin_allowed(headers: &HeaderMap, security: &SiteSecurity) -> bool {
    let Some(origin_header) = headers.get(header::ORIGIN) else {
        return false;
    };
    let Ok(origin_value) = origin_header.to_str() else {
        return false;
    };
    let Some(origin) = parse_origin(origin_value) else {
        return false;
    };
    match &security.public_origin {
        Some(configured) => origin.key == configured.key,
        None => origin.scheme == "http" && origin.is_loopback(),
    }
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
        Self { role: account.role }
    }
}

impl AuthPrincipal {
    fn post_access(&self) -> PostAccess {
        match self.role {
            AccountRole::Owner => PostAccess::Owner,
            AccountRole::Reader => PostAccess::Published,
        }
    }
}

fn no_store(mut response: Response) -> Response {
    response.headers_mut().insert(
        header::CACHE_CONTROL,
        HeaderValue::from_static("private, no-store"),
    );
    response
}

fn unix_time_seconds() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
        .min(i64::MAX as u64) as i64
}

async fn feed<S: StorageClient>(
    State(state): State<AppState<S>>,
    Query(query): Query<FeedQuery>,
) -> Response {
    let limit = query.limit.unwrap_or(DEFAULT_FEED_LIMIT);
    if !(1..=MAX_FEED_LIMIT).contains(&limit) {
        return (StatusCode::BAD_REQUEST, "limit must be between 1 and 100\n").into_response();
    }
    let after = match query.after {
        Some(value) => match parse_cursor(&value) {
            Some(cursor) => Some(cursor),
            None => return (StatusCode::BAD_REQUEST, "malformed after cursor\n").into_response(),
        },
        None => None,
    };
    match state.database.feed_filtered(limit, after, query.tag).await {
        Ok(page) => Json(page).into_response(),
        Err(error) => {
            eprintln!("website feed query failed: {error}");
            StatusCode::INTERNAL_SERVER_ERROR.into_response()
        }
    }
}

async fn home<S: StorageClient>(State(state): State<AppState<S>>) -> Response {
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
            let sidebar = match state.database.sidebar_data().await {
                Ok(sidebar) => sidebar,
                Err(error) => {
                    eprintln!("website sidebar query failed: {error}");
                    return StatusCode::INTERNAL_SERVER_ERROR.into_response();
                }
            };
            Html(render_home(&sidebar, initial_post.as_ref(), next_cursor)).into_response()
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
            let sidebar = match state.database.sidebar_data().await {
                Ok(sidebar) => sidebar,
                Err(error) => {
                    eprintln!("website sidebar query failed: {error}");
                    return StatusCode::INTERNAL_SERVER_ERROR.into_response();
                }
            };
            Html(render_tag_feed(&sidebar, &query.tag, initial_post.as_ref(), next_cursor))
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
) -> Response {
    if !valid_archive_month(&month) {
        return StatusCode::NOT_FOUND.into_response();
    }
    let posts = match state.database.posts_for_month(month.clone()).await {
        Ok(posts) => posts,
        Err(error) => {
            eprintln!("website archive query failed: {error}");
            return StatusCode::INTERNAL_SERVER_ERROR.into_response();
        }
    };
    let sidebar = match state.database.sidebar_data().await {
        Ok(sidebar) => sidebar,
        Err(error) => {
            eprintln!("website sidebar query failed: {error}");
            return StatusCode::INTERNAL_SERVER_ERROR.into_response();
        }
    };
    Html(render_archive(&sidebar, &month, &posts)).into_response()
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
    Extension(principal): Extension<AuthPrincipal>,
) -> Response {
    match state.database.post_with_access(id, principal.post_access()).await {
        Ok(Some(post)) => {
            let sidebar = match state.database.sidebar_data().await {
                Ok(sidebar) => sidebar,
                Err(error) => {
                    eprintln!("website sidebar query failed: {error}");
                    return StatusCode::INTERNAL_SERVER_ERROR.into_response();
                }
            };
            Html(render_full_post(&sidebar, &post)).into_response()
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
        Ok(Some(post)) => Html(pretty_html(&render_post(&post, false))).into_response(),
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
    if !valid_publication_date(published_at) {
        return None;
    }
    let id = encoded_id.parse::<i64>().ok()?;
    if id <= 0 || id.to_string() != encoded_id {
        return None;
    }
    Some(FeedCursor {
        published_at: published_at.to_owned(),
        id,
    })
}

fn valid_publication_date(value: &str) -> bool {
    let bytes = value.as_bytes();
    if bytes.len() != 10
        || bytes[4] != b'-'
        || bytes[7] != b'-'
        || !bytes
            .iter()
            .enumerate()
            .all(|(index, byte)| matches!(index, 4 | 7) || byte.is_ascii_digit())
    {
        return false;
    }
    let year = value[0..4].parse::<u32>().ok();
    let month = value[5..7].parse::<u32>().ok();
    let day = value[8..10].parse::<u32>().ok();
    let (Some(year), Some(month), Some(day)) = (year, month, day) else {
        return false;
    };
    let month_days = match month {
        1 | 3 | 5 | 7 | 8 | 10 | 12 => 31,
        4 | 6 | 9 | 11 => 30,
        2 if year % 4 == 0 && (year % 100 != 0 || year % 400 == 0) => 29,
        2 => 28,
        _ => return false,
    };
    day > 0 && day <= month_days
}

fn valid_archive_month(value: &str) -> bool {
    let bytes = value.as_bytes();
    bytes.len() == 7
        && bytes[4] == b'-'
        && bytes
            .iter()
            .enumerate()
            .all(|(index, byte)| index == 4 || byte.is_ascii_digit())
        && value[5..7].parse::<u32>().is_ok_and(|month| (1..=12).contains(&month))
}

fn render_full_post(sidebar: &SidebarData, post: &Post) -> String {
    let content = format!(
        "<main class=\"site site-post\"><p class=\"back-link\"><a href=\"/\">All posts</a></p>{}</main>",
        render_post(post, true),
    );
    render_site_page(&post.summary.title, sidebar, &content, true)
}

fn render_home(
    sidebar: &SidebarData,
    initial_post: Option<&Post>,
    next_cursor: Option<&str>,
) -> String {
    let mut feed = format!(
        "<section id=\"feed\" aria-label=\"Posts\" data-next-cursor=\"{}\">",
        escape_html(next_cursor.unwrap_or("")),
    );
    if let Some(post) = initial_post {
        feed.push_str(&render_post(post, true));
    } else {
        feed.push_str("<p class=\"empty-feed\">No published posts yet.</p>");
    }
    feed.push_str("</section>");
    let content = render_feed_controls("<main class=\"site site-feed\">", &feed, next_cursor);
    render_site_page("Journey", sidebar, &content, true)
}

fn render_tag_feed(
    sidebar: &SidebarData,
    tag: &str,
    initial_post: Option<&Post>,
    next_cursor: Option<&str>,
) -> String {
    let mut feed = format!(
        "<section id=\"feed\" aria-label=\"Posts tagged {}\" data-next-cursor=\"{}\" data-tag=\"{}\">",
        escape_html(tag),
        escape_html(next_cursor.unwrap_or("")),
        escape_html(tag),
    );
    if let Some(post) = initial_post {
        feed.push_str(&render_post(post, true));
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
    render_site_page(&format!("Posts tagged {tag}"), sidebar, &content, true)
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

fn render_archive(sidebar: &SidebarData, month: &str, posts: &[PostSummary]) -> String {
    let mut content = format!(
        "<main class=\"site site-archive\"><p class=\"back-link\"><a href=\"/\">All posts</a></p><h1>{}</h1><section class=\"archive-posts\" aria-label=\"Posts from {}\">",
        escape_html(&month_label(month)),
        escape_html(&month_label(month)),
    );
    if posts.is_empty() {
        content.push_str("<p class=\"empty-feed\">No published posts in this month.</p>");
    } else {
        for post in posts {
            content.push_str(&format!(
                "<article class=\"archive-post\"><h2><a href=\"/posts/{}\">{}</a></h2><time datetime=\"{}\">{}</time><p class=\"summary\">{}</p></article>",
                post.id,
                escape_html(&post.title),
                escape_html(&post.published_at),
                escape_html(&post.published_at),
                escape_html(&post.summary),
            ));
        }
    }
    content.push_str("</section></main>");
    render_site_page(&month_label(month), sidebar, &content, false)
}

fn render_site_page(title: &str, sidebar: &SidebarData, content: &str, include_slideshow: bool) -> String {
    let slideshow = if include_slideshow { SLIDESHOW_HTML } else { "" };
    let html = format!(
        "<!doctype html><html lang=\"en\"><head>{}</head><body><div class=\"site-layout\" id=\"site-layout\"><button class=\"sidebar-toggle\" id=\"sidebar-toggle\" type=\"button\" aria-controls=\"site-sidebar\" aria-expanded=\"false\" aria-label=\"Open sidebar\"><span aria-hidden=\"true\">›</span></button><button class=\"sidebar-backdrop\" id=\"sidebar-backdrop\" type=\"button\" aria-label=\"Close sidebar\" hidden></button>{}{}</div>{}</body></html>",
        html_head(title),
        render_sidebar(sidebar),
        content,
        slideshow,
    );
    pretty_html(&html)
}

fn render_sidebar(sidebar: &SidebarData) -> String {
    let mut html = String::from(
        "<nav class=\"site-sidebar\" id=\"site-sidebar\" aria-label=\"Site navigation\" hidden><header class=\"sidebar-header\"><a class=\"site-name\" href=\"/\">Journey</a><p>Stories from the road.</p></header><section class=\"sidebar-section\" aria-labelledby=\"sidebar-recent-heading\"><h2 id=\"sidebar-recent-heading\">Recent posts</h2><ul class=\"sidebar-list\">",
    );
    if sidebar.recent_posts.is_empty() {
        html.push_str("<li class=\"sidebar-muted\">No published posts yet.</li>");
    } else {
        for post in &sidebar.recent_posts {
            html.push_str(&format!(
                "<li><a href=\"/posts/{}\">{}</a><time datetime=\"{}\">{}</time></li>",
                post.id,
                escape_html(&post.title),
                escape_html(&post.published_at),
                escape_html(&post.published_at),
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

fn render_post(post: &Post, prioritize_first_image: bool) -> String {
    format!(
        "<article class=\"post\" data-post-id=\"{}\"><header class=\"post-header\"><h1>{}</h1><time datetime=\"{}\">{}</time><p class=\"summary\">{}</p></header>{}{}</article>",
        post.summary.id,
        escape_html(&post.summary.title),
        escape_html(&post.summary.published_at),
        escape_html(&post.summary.published_at),
        escape_html(&post.summary.summary),
        render_tags_html(&post.tags),
        render_post_blocks_html(post, prioritize_first_image),
    )
}

fn render_post_blocks_html(post: &Post, prioritize_first_image: bool) -> String {
    let mut prioritize_first_image = prioritize_first_image;
    render_blocks_html(&post.blocks, post.summary.id, &mut prioritize_first_image)
}

fn render_blocks_html(
    blocks: &[crate::db::PostBlock],
    post_id: i64,
    prioritize_first_image: &mut bool,
) -> String {
    let mut html = String::new();
    for block in blocks {
        if block.children.is_empty() {
            html.push_str(&render_standalone_block(block, post_id, prioritize_first_image));
        } else {
            html.push_str(&render_group(block, post_id, prioritize_first_image));
        }
    }
    html
}

fn render_standalone_block(
    block: &crate::db::PostBlock,
    post_id: i64,
    prioritize_first_image: &mut bool,
) -> String {
    let mut html = String::new();
    match block.content_type.as_deref() {
        Some(content_type) if content_type.starts_with("image/") => {
            let media_url = format!("/posts/{post_id}/blocks/{}/media", block.id);
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
            let media_url = format!("/posts/{post_id}/blocks/{}/media", block.id);
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
) -> String {
    let gallery_id = format!("gallery-{post_id}-{}", blocks[0].id);
    let mut html = format!("<div class=\"gallery\" data-gallery-run=\"{gallery_id}\">");
    for block in blocks {
        let content_type = block.content_type.as_deref().unwrap_or("");
        let media_url = format!("/posts/{post_id}/blocks/{}/media", block.id);
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

fn render_tags_html(tags: &[String]) -> String {
    if tags.is_empty() {
        String::new()
    } else {
        format!(
                "<p class=\"tags\"><span class=\"tag-label\">Tags:</span> {}</p>",
            tags.iter()
                .map(|tag| {
                    format!(
                        "<a class=\"tag\" href=\"/tags?tag={}\">{}</a>",
                        encode_url_component(tag),
                        escape_html(tag),
                    )
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

    let stored = match state
        .storage
        .get(&media.storage_key, range, head)
        .await
    {
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
            session_lifetime_seconds: 7 * 24 * 60 * 60,
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
        path::{Path, PathBuf},
        time::{SystemTime, UNIX_EPOCH},
    };

    use super::{
        escape_html, feed, home, parse_cursor, render_full_post, state, FeedQuery,
    };
    use crate::{
        db::{Database, NewPost, Post, PostBlock, PostSummary, SidebarData},
        storage::{StorageClient, StorageResponse},
    };
    use axum::{
        body::to_bytes,
        extract::{Query, State},
        http::StatusCode,
    };

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

    #[tokio::test]
    async fn malformed_feed_cursor_returns_bad_request() {
        let response = feed(
            State(state(Database::new(PathBuf::from("unused-test-database")), UnusedStorage)),
            Query(FeedQuery {
                limit: None,
                after: Some("2026-02-30:4".to_owned()),
                tag: None,
            }),
        )
        .await;
        assert_eq!(response.status(), StatusCode::BAD_REQUEST);
        assert!(parse_cursor("2026-02-28:4").is_some());
        assert!(parse_cursor("2026-02-28:04").is_none());
        assert!(parse_cursor("2026-02-28:4:5").is_none());
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
                title: format!("Post {id}"),
                published_at: "2026-01-01".to_owned(),
                summary: format!("Summary {id}"),
                tags: Vec::new(),
                blocks: Vec::new(),
            })
            .collect();
        database.replace_posts(posts).await.unwrap();

        let response = home(State(state(database, UnusedStorage))).await;
        assert_eq!(response.status(), StatusCode::OK);
        let body = to_bytes(response.into_body(), usize::MAX).await.unwrap();
        let html = String::from_utf8(body.to_vec()).unwrap();
        assert_eq!(html.matches("<article class=\"post\"").count(), 1);
        assert!(html.contains("data-next-cursor=\"2026-01-01:12\""));
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
                published_at: "2026-01-01".to_owned(),
                summary: "A <summary>".to_owned(),
            },
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
        let html = render_full_post(&SidebarData::default(), &post);
        assert!(html.contains("<p class=\"tags\"><span class=\"tag-label\">Tags:</span> <a class=\"tag\" href=\"/tags?tag=%3Ctag%3E\">&lt;tag&gt;</a></p>"));
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
        assert_eq!(escape_html("'&\"<>"), "&#39;&amp;&quot;&lt;&gt;");
    }
}
