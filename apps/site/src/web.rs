use axum::{
    body::Body,
    extract::{Path, Query, State},
    http::{header, HeaderMap, Method, StatusCode},
    response::{IntoResponse, Response},
    routing::get,
    Json, Router,
};
use serde::Deserialize;

use crate::{db::Database, storage::StorageClient};

const DEFAULT_FEED_LIMIT: usize = 20;
const MAX_FEED_LIMIT: usize = 100;

#[derive(Clone)]
pub struct AppState<S: StorageClient> {
    database: Database,
    storage: S,
}

#[derive(Deserialize)]
struct FeedQuery {
    limit: Option<usize>,
}

pub fn router<S: StorageClient>(state: AppState<S>) -> Router {
    Router::new()
        .route("/api/posts", get(feed::<S>))
        .route("/api/posts/{id}", get(full_post::<S>))
        .route("/posts/{id}", get(full_post::<S>))
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
    match state.database.feed(limit).await {
        Ok(posts) => Json(posts).into_response(),
        Err(error) => {
            eprintln!("website feed query failed: {error}");
            StatusCode::INTERNAL_SERVER_ERROR.into_response()
        }
    }
}

async fn full_post<S: StorageClient>(
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
