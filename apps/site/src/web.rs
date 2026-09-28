use axum::{
    body::Body,
    extract::{Path, Query, State},
    http::{header, HeaderMap, Method, StatusCode},
    response::{Html, IntoResponse, Response},
    routing::get,
    Json, Router,
};
use serde::Deserialize;

use crate::{
    db::{Database, FeedCursor, FeedPage, Post, PostSummary},
    storage::StorageClient,
};

const DEFAULT_FEED_LIMIT: usize = 10;
const MAX_FEED_LIMIT: usize = 100;

#[derive(Clone)]
pub struct AppState<S: StorageClient> {
    database: Database,
    storage: S,
}

#[derive(Deserialize)]
struct FeedQuery {
    limit: Option<usize>,
    after: Option<String>,
}

pub fn router<S: StorageClient>(state: AppState<S>) -> Router {
    Router::new()
        .route("/", get(home::<S>))
        .route("/api/posts", get(feed::<S>))
        .route("/api/posts/{id}", get(api_full_post::<S>))
        .route("/posts/{id}", get(post_page::<S>))
        .route(
            "/posts/{post_id}/blocks/{position}/media",
            get(media::<S>).head(media::<S>),
        )
        .with_state(state)
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
    match state.database.feed(limit, after).await {
        Ok(page) => Json(page).into_response(),
        Err(error) => {
            eprintln!("website feed query failed: {error}");
            StatusCode::INTERNAL_SERVER_ERROR.into_response()
        }
    }
}

async fn home<S: StorageClient>(State(state): State<AppState<S>>) -> Response {
    match state.database.feed(DEFAULT_FEED_LIMIT, None).await {
        Ok(page) => Html(render_home(&page)).into_response(),
        Err(error) => {
            eprintln!("website home query failed: {error}");
            StatusCode::INTERNAL_SERVER_ERROR.into_response()
        }
    }
}

async fn api_full_post<S: StorageClient>(
    State(state): State<AppState<S>>,
    Path(id): Path<i64>,
) -> Response {
    match state.database.post(id).await {
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
) -> Response {
    match state.database.post(id).await {
        Ok(Some(post)) => Html(render_full_post(&post)).into_response(),
        Ok(None) => StatusCode::NOT_FOUND.into_response(),
        Err(error) => {
            eprintln!("website post page query failed: {error}");
            StatusCode::INTERNAL_SERVER_ERROR.into_response()
        }
    }
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

fn render_home(page: &FeedPage) -> String {
    let mut html = format!(
        "<!doctype html><html lang=\"en\"><head>{}</head><body><main class=\"site\"><header><h1>Journey</h1><p>Stories from the road.</p></header><section id=\"feed\" aria-label=\"Posts\" data-next-cursor=\"{}\">",
        html_head("Journey"),
        escape_html(page.next_cursor.as_deref().unwrap_or("")),
    );
    for post in &page.posts {
        html.push_str(&render_preview(post));
    }
    html.push_str("</section><div id=\"feed-sentinel\" aria-hidden=\"true\"></div><p id=\"feed-status\" role=\"status\" aria-live=\"polite\">");
    if page.next_cursor.is_none() {
        html.push_str("You have reached the end of the feed.");
    }
    html.push_str("</p><button id=\"load-more\" type=\"button\"");
    if page.next_cursor.is_none() {
        html.push_str(" disabled");
    }
    html.push_str(">Load more</button></main><script>");
    html.push_str(FEED_SCRIPT);
    html.push_str("</script></body></html>");
    html
}

fn render_preview(post: &PostSummary) -> String {
    format!(
        "<article class=\"post-preview\" data-post-id=\"{}\"><h2>{}</h2><time datetime=\"{}\">{}</time><p class=\"summary\">{}</p><p class=\"post-actions\"><button class=\"expand-post\" type=\"button\" aria-expanded=\"false\" aria-controls=\"post-content-{}\">Expand post</button> <a href=\"/posts/{}\">Open post</a></p><div class=\"post-content\" id=\"post-content-{}\" hidden></div><p class=\"post-status\" role=\"status\" aria-live=\"polite\"></p></article>",
        post.id,
        escape_html(&post.title),
        escape_html(&post.published_at),
        escape_html(&post.published_at),
        escape_html(&post.summary),
        post.id,
        post.id,
        post.id,
    )
}

fn render_full_post(post: &Post) -> String {
    format!(
        "<!doctype html><html lang=\"en\"><head>{}</head><body><main class=\"site\"><p><a href=\"/\">Journey</a></p><article><h1>{}</h1><time datetime=\"{}\">{}</time><p class=\"summary\">{}</p>{}</article></main></body></html>",
        html_head(&post.summary.title),
        escape_html(&post.summary.title),
        escape_html(&post.summary.published_at),
        escape_html(&post.summary.published_at),
        escape_html(&post.summary.summary),
        render_post_blocks_html(post),
    )
}

fn render_post_blocks_html(post: &Post) -> String {
    let mut html = String::new();
    for block in &post.blocks {
        match block.kind.as_str() {
            "paragraph" => html.push_str(&format!("<p>{}</p>", escape_html(block.text.as_deref().unwrap_or("")))),
            "heading" => {
                let level = block.level.unwrap_or(2).clamp(1, 6);
                html.push_str(&format!("<h{level}>{}</h{level}>", escape_html(block.text.as_deref().unwrap_or(""))));
            }
            "image" => {
                let media_url = format!("/posts/{}/blocks/{}/media", post.summary.id, block.position);
                html.push_str(&format!(
                    "<figure><img src=\"{media_url}\" alt=\"{}\" loading=\"lazy\">{}</figure>",
                    escape_html(block.alt.as_deref().unwrap_or("")),
                    render_caption(block.caption.as_deref()),
                ));
            }
            "video" => {
                let media_url = format!("/posts/{}/blocks/{}/media", post.summary.id, block.position);
                html.push_str(&format!(
                    "<figure><video controls preload=\"metadata\"><source src=\"{media_url}\" type=\"{}\"></video>{}</figure>",
                    escape_html(block.content_type.as_deref().unwrap_or("application/octet-stream")),
                    render_caption(block.caption.as_deref()),
                ));
            }
            _ => {}
        }
    }
    html
}

fn render_caption(caption: Option<&str>) -> String {
    caption
        .filter(|caption| !caption.is_empty())
        .map(|caption| format!("<figcaption>{}</figcaption>", escape_html(caption)))
        .unwrap_or_default()
}

fn html_head(title: &str) -> String {
    format!(
        "<meta charset=\"utf-8\"><meta name=\"viewport\" content=\"width=device-width, initial-scale=1\"><title>{}</title><style>body{{margin:0;color:#202421;background:#f5f4ef;font:1rem/1.6 system-ui,sans-serif}}.site{{max-width:48rem;margin:0 auto;padding:2rem 1.25rem}}header{{margin-bottom:2rem}}.post-preview{{padding:1.5rem 0;border-top:1px solid #c9c9c1}}time{{color:#62675f;font-size:.9rem}}.summary{{white-space:pre-wrap}}.post-actions{{display:flex;gap:1rem;align-items:center}}button,a{{font:inherit}}button{{padding:.5rem .8rem}}.post-content{{margin-top:1.5rem}}img,video{{display:block;max-width:100%;height:auto}}figure{{margin:1.5rem 0}}figcaption{{color:#62675f;font-size:.9rem}}#feed-status{{min-height:1.6em}}#feed-sentinel{{height:1px}}</style>",
        escape_html(title),
    )
}

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

const FEED_SCRIPT: &str = r#"
const feed = document.querySelector('#feed');
const status = document.querySelector('#feed-status');
const loadButton = document.querySelector('#load-more');
const sentinel = document.querySelector('#feed-sentinel');
let nextCursor = feed.dataset.nextCursor || null;
let loadingFeed = false;
let lastScrollY = window.scrollY;
let hasScrolledDown = false;

function makeElement(tag, text) {
  const element = document.createElement(tag);
  if (text !== undefined && text !== null) element.textContent = text;
  return element;
}

function makePreview(post) {
  const article = document.createElement('article');
  article.className = 'post-preview';
  article.dataset.postId = String(post.id);
  article.append(makeElement('h2', post.title));
  const time = makeElement('time', post.published_at);
  time.dateTime = post.published_at;
  article.append(time, makeElement('p', post.summary));
  article.lastElementChild.className = 'summary';

  const actions = makeElement('p');
  actions.className = 'post-actions';
  const expand = makeElement('button', 'Expand post');
  expand.type = 'button';
  expand.className = 'expand-post';
  expand.setAttribute('aria-expanded', 'false');
  expand.setAttribute('aria-controls', `post-content-${post.id}`);
  const link = makeElement('a', 'Open post');
  link.href = `/posts/${post.id}`;
  actions.append(expand, link);
  const content = makeElement('div');
  content.className = 'post-content';
  content.id = `post-content-${post.id}`;
  content.hidden = true;
  const postStatus = makeElement('p');
  postStatus.className = 'post-status';
  postStatus.setAttribute('role', 'status');
  postStatus.setAttribute('aria-live', 'polite');
  article.append(actions, content, postStatus);
  feed.append(article);
}

function renderBlocks(article, post) {
  const content = article.querySelector('.post-content');
  for (const block of post.blocks) {
    if (block.type === 'paragraph') {
      content.append(makeElement('p', block.text));
    } else if (block.type === 'heading') {
      const level = Math.min(6, Math.max(1, Number(block.level) || 2));
      content.append(makeElement(`h${level}`, block.text));
    } else if (block.type === 'image' || block.type === 'video') {
      const figure = makeElement('figure');
      const url = `/posts/${post.id}/blocks/${block.position}/media`;
      if (block.type === 'image') {
        const image = makeElement('img');
        image.src = url;
        image.alt = block.alt || '';
        image.loading = 'lazy';
        figure.append(image);
      } else {
        const video = makeElement('video');
        video.controls = true;
        video.preload = 'metadata';
        const source = makeElement('source');
        source.src = url;
        source.type = block.content_type || 'application/octet-stream';
        video.append(source);
        figure.append(video);
      }
      if (block.caption) figure.append(makeElement('figcaption', block.caption));
      content.append(figure);
    }
  }
}

async function expandPost(article, button) {
  const content = article.querySelector('.post-content');
  const postStatus = article.querySelector('.post-status');
  const expanded = button.getAttribute('aria-expanded') === 'true';
  button.setAttribute('aria-expanded', String(!expanded));
  button.textContent = expanded ? 'Expand post' : 'Collapse post';
  content.hidden = expanded;
  if (expanded || article.dataset.contentLoaded === 'true' || article.dataset.contentLoading === 'true') return;

  article.dataset.contentLoading = 'true';
  button.disabled = true;
  postStatus.textContent = 'Loading post…';
  try {
    const response = await fetch(`/api/posts/${article.dataset.postId}`);
    if (!response.ok) throw new Error(`HTTP ${response.status}`);
    const post = await response.json();
    renderBlocks(article, post);
    article.dataset.contentLoaded = 'true';
    postStatus.textContent = '';
  } catch (_) {
    postStatus.textContent = 'Could not load this post. Collapse and expand to retry.';
  } finally {
    article.dataset.contentLoading = 'false';
    button.disabled = false;
  }
}

feed.addEventListener('click', event => {
  const button = event.target.closest('.expand-post');
  if (button) expandPost(button.closest('.post-preview'), button);
});

async function loadNextPage() {
  if (loadingFeed || !nextCursor) return;
  loadingFeed = true;
  loadButton.disabled = true;
  status.textContent = 'Loading more posts…';
  try {
    const url = `/api/posts?limit=10&after=${encodeURIComponent(nextCursor)}`;
    const response = await fetch(url);
    if (!response.ok) throw new Error(`HTTP ${response.status}`);
    const page = await response.json();
    if (!Array.isArray(page.posts)) throw new Error('Invalid feed response');
    for (const post of page.posts) makePreview(post);
    nextCursor = typeof page.next_cursor === 'string' ? page.next_cursor : null;
    feed.dataset.nextCursor = nextCursor || '';
    status.textContent = nextCursor ? '' : 'You have reached the end of the feed.';
  } catch (_) {
    status.textContent = 'Could not load more posts. Use Load more to retry.';
  } finally {
    loadingFeed = false;
    loadButton.disabled = !nextCursor;
  }
}

loadButton.addEventListener('click', loadNextPage);

function sentinelIsVisible() {
  const rect = sentinel.getBoundingClientRect();
  return rect.bottom >= 0 && rect.top <= window.innerHeight;
}

window.addEventListener('scroll', () => {
  if (window.scrollY > lastScrollY) {
    hasScrolledDown = true;
    if (sentinelIsVisible()) loadNextPage();
  }
  lastScrollY = window.scrollY;
}, {passive: true});

if ('IntersectionObserver' in window) {
  const observer = new IntersectionObserver(entries => {
    if (hasScrolledDown && entries.some(entry => entry.isIntersecting)) loadNextPage();
  });
  observer.observe(sentinel);
}

if (!nextCursor) status.textContent = 'You have reached the end of the feed.';
"#;

async fn media<S: StorageClient>(
    State(state): State<AppState<S>>,
    Path((post_id, position)): Path<(i64, i64)>,
    method: Method,
    headers: HeaderMap,
) -> Response {
    let media = match state.database.media_reference(post_id, position).await {
        Ok(Some(media)) => media,
        Ok(None) => return StatusCode::NOT_FOUND.into_response(),
        Err(error) => {
            eprintln!("website media lookup failed: {error}");
            return StatusCode::INTERNAL_SERVER_ERROR.into_response();
        }
    };
    let head = method == Method::HEAD;
    let range = if media.kind == "video" && !head {
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

pub fn state<S: StorageClient>(database: Database, storage: S) -> AppState<S> {
    AppState { database, storage }
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
        db::{Database, NewPost, Post, PostBlock, PostSummary},
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
            }),
        )
        .await;
        assert_eq!(response.status(), StatusCode::BAD_REQUEST);
        assert!(parse_cursor("2026-02-28:4").is_some());
        assert!(parse_cursor("2026-02-28:04").is_none());
        assert!(parse_cursor("2026-02-28:4:5").is_none());
    }

    #[tokio::test]
    async fn initial_home_request_renders_ten_previews_and_a_cursor() {
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
                blocks: Vec::new(),
            })
            .collect();
        database.replace_posts(posts).await.unwrap();

        let response = home(State(state(database, UnusedStorage))).await;
        assert_eq!(response.status(), StatusCode::OK);
        let body = to_bytes(response.into_body(), usize::MAX).await.unwrap();
        let html = String::from_utf8(body.to_vec()).unwrap();
        assert_eq!(html.matches("<article class=\"post-preview\"").count(), 10);
        assert!(html.contains("data-next-cursor=\"2026-01-01:3\""));
        assert!(html.contains("Post 3"));
        assert!(!html.contains("Post 2"));
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
            blocks: vec![
                PostBlock {
                    position: 0,
                    kind: "paragraph".to_owned(),
                    text: Some("<script>body</script>".to_owned()),
                    level: None,
                    content_type: None,
                    alt: None,
                    caption: None,
                },
                PostBlock {
                    position: 1,
                    kind: "image".to_owned(),
                    text: None,
                    level: None,
                    content_type: Some("image/jpeg".to_owned()),
                    alt: Some("photo\" onerror=\"alert(1)".to_owned()),
                    caption: Some("<caption>".to_owned()),
                },
                PostBlock {
                    position: 2,
                    kind: "video".to_owned(),
                    text: None,
                    level: None,
                    content_type: Some("video/mp4".to_owned()),
                    alt: None,
                    caption: None,
                },
            ],
        };
        let html = render_full_post(&post);
        assert!(html.contains("&lt;script&gt;body&lt;/script&gt;"));
        assert!(html.contains("&quot;&gt;&lt;script&gt;alert(1)&lt;/script&gt;"));
        assert!(html.contains("alt=\"photo&quot; onerror=&quot;alert(1)\""));
        assert!(html.contains("<img src=\"/posts/7/blocks/1/media\""));
        assert!(html.contains("<video controls preload=\"metadata\"><source src=\"/posts/7/blocks/2/media\""));
        assert!(html.contains("<figcaption>&lt;caption&gt;</figcaption>"));
        assert!(!html.contains("<script>body</script>"));
        assert_eq!(escape_html("'&\"<>"), "&#39;&amp;&quot;&lt;&gt;");
    }
}
