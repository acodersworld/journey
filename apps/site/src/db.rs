use std::{path::PathBuf, sync::Arc, time::Duration};

use rusqlite::{params, Connection, OptionalExtension, TransactionBehavior};
use serde::Serialize;

const SCHEMA: &str = "\
CREATE TABLE IF NOT EXISTS posts (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    title TEXT NOT NULL CHECK (length(trim(title)) > 0),
    published_at TEXT NOT NULL,
    summary TEXT NOT NULL,
    published INTEGER NOT NULL DEFAULT 1 CHECK (published IN (0, 1))
);
CREATE TABLE IF NOT EXISTS post_blocks (
    post_id INTEGER NOT NULL REFERENCES posts(id) ON DELETE CASCADE,
    position INTEGER NOT NULL CHECK (position >= 0),
    kind TEXT NOT NULL CHECK (kind IN ('paragraph', 'heading', 'image', 'video')),
    text TEXT,
    level INTEGER,
    storage_key TEXT,
    content_type TEXT,
    alt_text TEXT,
    caption TEXT,
    PRIMARY KEY (post_id, position),
    CHECK (
        (kind = 'paragraph' AND text IS NOT NULL AND level IS NULL AND storage_key IS NULL AND content_type IS NULL AND alt_text IS NULL AND caption IS NULL)
        OR (kind = 'heading' AND text IS NOT NULL AND level BETWEEN 1 AND 6 AND storage_key IS NULL AND content_type IS NULL AND alt_text IS NULL AND caption IS NULL)
        OR (kind = 'image' AND text IS NULL AND level IS NULL AND storage_key IS NOT NULL AND content_type IS NOT NULL)
        OR (kind = 'video' AND text IS NULL AND level IS NULL AND storage_key IS NOT NULL AND content_type IS NOT NULL)
    )
);
CREATE INDEX IF NOT EXISTS posts_published_order ON posts(published, published_at DESC, id DESC);
";

#[derive(Clone)]
pub struct Database {
    path: Arc<PathBuf>,
}

#[derive(Clone, Debug, Serialize)]
pub struct PostSummary {
    pub id: i64,
    pub title: String,
    pub published_at: String,
    pub summary: String,
}

#[derive(Clone, Debug, Serialize)]
pub struct PostBlock {
    pub position: i64,
    #[serde(rename = "type")]
    pub kind: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub text: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub level: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub content_type: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub alt: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub caption: Option<String>,
}

#[derive(Clone, Debug, Serialize)]
pub struct Post {
    #[serde(flatten)]
    pub summary: PostSummary,
    pub blocks: Vec<PostBlock>,
}

#[derive(Clone, Debug)]
pub struct NewPost {
    pub title: String,
    pub published_at: String,
    pub summary: String,
    pub blocks: Vec<NewBlock>,
}

#[derive(Clone, Debug)]
pub struct NewBlock {
    pub kind: String,
    pub text: Option<String>,
    pub level: Option<i64>,
    pub storage_key: Option<String>,
    pub content_type: Option<String>,
    pub alt: Option<String>,
    pub caption: Option<String>,
}

#[derive(Clone, Debug)]
pub struct MediaReference {
    pub kind: String,
    pub storage_key: String,
    pub content_type: String,
}

impl Database {
    pub fn new(path: PathBuf) -> Self {
        Self { path: Arc::new(path) }
    }

    pub async fn initialize(&self) -> Result<(), String> {
        self.run(|connection| connection.execute_batch(SCHEMA)).await
    }

    pub async fn replace_posts(&self, posts: Vec<NewPost>) -> Result<(), String> {
        self.initialize().await?;
        self.run(move |connection| {
            let transaction = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
            transaction.execute("DELETE FROM posts", [])?;
            {
                let mut insert_post = transaction.prepare(
                    "INSERT INTO posts (title, published_at, summary, published) VALUES (?1, ?2, ?3, 1)",
                )?;
                let mut insert_block = transaction.prepare(
                    "INSERT INTO post_blocks (post_id, position, kind, text, level, storage_key, content_type, alt_text, caption) \
                     VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)",
                )?;
                for post in posts {
                    insert_post.execute(params![post.title, post.published_at, post.summary])?;
                    let post_id = transaction.last_insert_rowid();
                    for (position, block) in post.blocks.into_iter().enumerate() {
                        insert_block.execute(params![
                            post_id,
                            position as i64,
                            block.kind,
                            block.text,
                            block.level,
                            block.storage_key,
                            block.content_type,
                            block.alt,
                            block.caption,
                        ])?;
                    }
                }
            }
            transaction.commit()
        })
        .await
    }

    pub async fn feed(&self, limit: usize) -> Result<Vec<PostSummary>, String> {
        self.run(move |connection| {
            let mut statement = connection.prepare(
                "SELECT id, title, published_at, summary FROM posts \
                 WHERE published = 1 ORDER BY published_at DESC, id DESC LIMIT ?1",
            )?;
            let rows = statement.query_map([limit as i64], post_summary_from_row)?;
            rows.collect()
        })
        .await
    }

    pub async fn all_summaries(&self) -> Result<Vec<PostSummary>, String> {
        self.run(|connection| {
            let mut statement = connection.prepare(
                "SELECT id, title, published_at, summary FROM posts \
                 WHERE published = 1 ORDER BY published_at DESC, id DESC",
            )?;
            let rows = statement.query_map([], post_summary_from_row)?;
            rows.collect()
        })
        .await
    }

    pub async fn post(&self, id: i64) -> Result<Option<Post>, String> {
        self.run(move |connection| {
            let transaction = connection.transaction()?;
            let summary = transaction
                .query_row(
                    "SELECT id, title, published_at, summary FROM posts WHERE id = ?1 AND published = 1",
                    [id],
                    post_summary_from_row,
                )
                .optional()?;
            let Some(summary) = summary else {
                transaction.commit()?;
                return Ok(None);
            };
            let blocks = {
                let mut statement = transaction.prepare(
                    "SELECT position, kind, text, level, content_type, alt_text, caption \
                     FROM post_blocks WHERE post_id = ?1 ORDER BY position",
                )?;
                let rows = statement.query_map([id], |row| {
                    Ok(PostBlock {
                        position: row.get(0)?,
                        kind: row.get(1)?,
                        text: row.get(2)?,
                        level: row.get(3)?,
                        content_type: row.get(4)?,
                        alt: row.get(5)?,
                        caption: row.get(6)?,
                    })
                })?;
                rows.collect::<rusqlite::Result<Vec<_>>>()?
            };
            transaction.commit()?;
            Ok(Some(Post { summary, blocks }))
        })
        .await
    }

    pub async fn media_reference(
        &self,
        post_id: i64,
        position: i64,
    ) -> Result<Option<MediaReference>, String> {
        self.run(move |connection| {
            connection
                .query_row(
                    "SELECT b.kind, b.storage_key, b.content_type \
                     FROM post_blocks AS b JOIN posts AS p ON p.id = b.post_id \
                     WHERE p.id = ?1 AND p.published = 1 AND b.position = ?2 \
                       AND b.kind IN ('image', 'video')",
                    params![post_id, position],
                    |row| {
                        Ok(MediaReference {
                            kind: row.get(0)?,
                            storage_key: row.get(1)?,
                            content_type: row.get(2)?,
                        })
                    },
                )
                .optional()
        })
        .await
    }

    pub async fn tables(&self) -> Result<Vec<String>, String> {
        self.run(|connection| {
            let mut statement = connection.prepare(
                "SELECT name FROM sqlite_master WHERE type = 'table' AND name NOT LIKE 'sqlite_%' ORDER BY name",
            )?;
            let rows = statement.query_map([], |row| row.get(0))?;
            rows.collect()
        })
        .await
    }

    pub async fn schema(&self) -> Result<Vec<(String, String)>, String> {
        self.run(|connection| {
            let mut statement = connection.prepare(
                "SELECT name, sql FROM sqlite_master \
                 WHERE type IN ('table', 'index') AND name NOT LIKE 'sqlite_%' AND sql IS NOT NULL \
                 ORDER BY name",
            )?;
            let rows = statement.query_map([], |row| {
                Ok((row.get(0)?, row.get::<_, Option<String>>(1)?.unwrap_or_default()))
            })?;
            rows.collect()
        })
        .await
    }

    async fn run<T, F>(&self, operation: F) -> Result<T, String>
    where
        T: Send + 'static,
        F: FnOnce(&mut Connection) -> rusqlite::Result<T> + Send + 'static,
    {
        let path = self.path.as_ref().clone();
        tokio::task::spawn_blocking(move || {
            let mut connection = Connection::open(path).map_err(|error| error.to_string())?;
            connection
                .busy_timeout(Duration::from_secs(5))
                .map_err(|error| error.to_string())?;
            connection
                .pragma_update(None, "foreign_keys", "ON")
                .map_err(|error| error.to_string())?;
            operation(&mut connection).map_err(|error| error.to_string())
        })
        .await
        .map_err(|error| format!("database task failed: {error}"))?
    }
}

fn post_summary_from_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<PostSummary> {
    Ok(PostSummary {
        id: row.get(0)?,
        title: row.get(1)?,
        published_at: row.get(2)?,
        summary: row.get(3)?,
    })
}
