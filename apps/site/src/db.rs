use std::{collections::HashMap, path::PathBuf, sync::Arc, time::Duration};

use rusqlite::{params, types::Type, Connection, OptionalExtension, TransactionBehavior};
use serde::{Deserialize, Serialize};

const POSTS_SCHEMA: &str = "\
CREATE TABLE IF NOT EXISTS posts (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    author_id INTEGER NOT NULL REFERENCES users(id),
    title TEXT NOT NULL,
    published_at INTEGER,
    summary TEXT NOT NULL,
    published INTEGER NOT NULL DEFAULT 1 CHECK (published IN (0, 1)),
    tags TEXT NOT NULL DEFAULT '[]' CHECK (json_valid(tags) AND json_type(tags) = 'array'),
    revision INTEGER NOT NULL DEFAULT 1 CHECK (revision > 0),
    CHECK (published = 0 OR published_at IS NOT NULL)
);
CREATE INDEX IF NOT EXISTS posts_published_order ON posts(published, published_at DESC, id DESC);
CREATE TRIGGER IF NOT EXISTS posts_tags_are_strings_insert
BEFORE INSERT ON posts
WHEN EXISTS (SELECT 1 FROM json_each(NEW.tags) WHERE type != 'text')
BEGIN
    SELECT RAISE(ABORT, 'post tags must be strings');
END;
CREATE TRIGGER IF NOT EXISTS posts_tags_are_strings_update
BEFORE UPDATE OF tags ON posts
WHEN EXISTS (SELECT 1 FROM json_each(NEW.tags) WHERE type != 'text')
BEGIN
    SELECT RAISE(ABORT, 'post tags must be strings');
END;
";

const MEDIA_ASSETS_SCHEMA: &str = "\
CREATE TABLE IF NOT EXISTS media_assets (
    storage_key TEXT PRIMARY KEY,
    content_type TEXT NOT NULL,
    size_bytes INTEGER CHECK (size_bytes IS NULL OR size_bytes >= 0)
);
";

const POST_BLOCKS_SCHEMA: &str = "\
CREATE TABLE IF NOT EXISTS post_blocks (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    post_id INTEGER NOT NULL REFERENCES posts(id) ON DELETE CASCADE,
    parent_id INTEGER,
    position INTEGER NOT NULL CHECK (position >= 0),
    header TEXT,
    body TEXT,
    storage_key TEXT REFERENCES media_assets(storage_key),
    alt_text TEXT,
    UNIQUE (id, post_id),
    FOREIGN KEY (parent_id, post_id) REFERENCES post_blocks(id, post_id) ON DELETE CASCADE,
    CHECK (parent_id IS NULL OR parent_id != id),
    CHECK ((parent_id IS NULL AND storage_key IS NULL) OR (parent_id IS NOT NULL AND storage_key IS NOT NULL))
);
CREATE UNIQUE INDEX IF NOT EXISTS post_blocks_sibling_order
    ON post_blocks(post_id, COALESCE(parent_id, 0), position);
CREATE TRIGGER IF NOT EXISTS post_blocks_parent_must_be_root_insert
BEFORE INSERT ON post_blocks
WHEN NEW.parent_id IS NOT NULL AND EXISTS (
    SELECT 1 FROM post_blocks WHERE id = NEW.parent_id AND parent_id IS NOT NULL
)
BEGIN
    SELECT RAISE(ABORT, 'post blocks may only be nested one level deep');
END;
CREATE TRIGGER IF NOT EXISTS post_blocks_parent_must_be_root_update
BEFORE UPDATE OF parent_id ON post_blocks
WHEN NEW.parent_id IS NOT NULL AND (
    EXISTS (SELECT 1 FROM post_blocks WHERE id = NEW.parent_id AND parent_id IS NOT NULL)
    OR EXISTS (SELECT 1 FROM post_blocks WHERE parent_id = OLD.id)
)
BEGIN
    SELECT RAISE(ABORT, 'post blocks may only be nested one level deep');
END;
";

const AUTH_SCHEMA: &str = "\
CREATE TABLE IF NOT EXISTS users (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    username TEXT NOT NULL COLLATE NOCASE UNIQUE
        CHECK (length(username) BETWEEN 3 AND 32),
    role TEXT NOT NULL CHECK (role IN ('read', 'write', 'admin')),
    password_hash TEXT NOT NULL,
    enabled INTEGER NOT NULL DEFAULT 1 CHECK (enabled IN (0, 1)),
    created_at INTEGER NOT NULL
);
CREATE TABLE IF NOT EXISTS sessions (
    token_digest TEXT PRIMARY KEY CHECK (length(token_digest) = 64),
    user_id INTEGER NOT NULL REFERENCES users(id) ON DELETE CASCADE,
    created_at INTEGER NOT NULL,
    expires_at INTEGER NOT NULL CHECK (expires_at > created_at)
);
CREATE INDEX IF NOT EXISTS sessions_expiry ON sessions(expires_at);
CREATE INDEX IF NOT EXISTS sessions_user ON sessions(user_id);
CREATE TABLE IF NOT EXISTS login_throttles (
    username_digest TEXT PRIMARY KEY CHECK (length(username_digest) = 64),
    window_started INTEGER NOT NULL,
    attempts INTEGER NOT NULL CHECK (attempts > 0)
);
";

const CURRENT_SCHEMA_VERSION: i64 = 8;
const REBUILD_DATABASE_MESSAGE: &str = "site database schema is outdated; recreate the SQLite database and run the destructive importer again";

const SHARE_SCHEMA: &str = "\
CREATE TABLE IF NOT EXISTS site_settings (
    name TEXT PRIMARY KEY,
    integer_value INTEGER NOT NULL CHECK (integer_value > 0)
);
INSERT OR IGNORE INTO site_settings (name, integer_value)
VALUES ('share_link_lifetime_seconds', 86400);
CREATE TABLE IF NOT EXISTS share_links (
    id TEXT PRIMARY KEY,
    post_id INTEGER NOT NULL REFERENCES posts(id) ON DELETE CASCADE,
    token_digest TEXT NOT NULL UNIQUE CHECK (length(token_digest) = 64),
    created_at INTEGER NOT NULL,
    expires_at INTEGER NOT NULL CHECK (expires_at > created_at),
    revoked_at INTEGER
);
CREATE INDEX IF NOT EXISTS share_links_post ON share_links(post_id, created_at DESC);
CREATE INDEX IF NOT EXISTS share_links_expiry ON share_links(expires_at);
CREATE TABLE IF NOT EXISTS share_sessions (
    token_digest TEXT PRIMARY KEY CHECK (length(token_digest) = 64),
    share_link_id TEXT NOT NULL REFERENCES share_links(id) ON DELETE CASCADE,
    created_at INTEGER NOT NULL,
    expires_at INTEGER NOT NULL CHECK (expires_at > created_at)
);
CREATE INDEX IF NOT EXISTS share_sessions_expiry ON share_sessions(expires_at);
CREATE INDEX IF NOT EXISTS share_sessions_link ON share_sessions(share_link_id);
";

#[derive(Clone)]
pub struct Database {
    path: Arc<PathBuf>,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum AccountRole {
    Read,
    Write,
    Admin,
}

impl AccountRole {
    pub fn parse(value: &str) -> Result<Self, String> {
        match value {
            "read" => Ok(Self::Read),
            "write" => Ok(Self::Write),
            "admin" => Ok(Self::Admin),
            _ => Err("account role must be read, write, or admin".to_owned()),
        }
    }

    fn as_str(self) -> &'static str {
        match self {
            Self::Read => "read",
            Self::Write => "write",
            Self::Admin => "admin",
        }
    }
}

impl Default for AccountRole {
    fn default() -> Self {
        Self::Read
    }
}

impl std::fmt::Display for AccountRole {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(self.as_str())
    }
}

#[derive(Clone, Debug, Serialize)]
pub struct AccountView {
    pub username: String,
    pub role: AccountRole,
    pub enabled: bool,
}

#[derive(Clone, Debug)]
pub struct LoginAccount {
    pub id: i64,
    pub username: String,
    pub role: AccountRole,
    pub password_hash: String,
    pub enabled: bool,
}

#[derive(Clone, Debug)]
pub struct ImportedAccount {
    pub username: String,
    pub role: AccountRole,
    pub password_hash: String,
}

#[derive(Clone, Debug)]
pub struct AuthenticatedAccount {
    pub username: String,
    pub role: AccountRole,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum PostAccess {
    Published,
    Author(String),
    Admin,
}

impl PostAccess {
    fn query_args(&self) -> (bool, Option<&str>) {
        match self {
            Self::Published => (false, None),
            Self::Author(username) => (false, Some(username)),
            Self::Admin => (true, None),
        }
    }
}

#[derive(Clone, Debug)]
pub enum ShareAccess {
    Author(String),
    Admin,
}

#[derive(Clone, Debug, Serialize)]
pub struct PostSummary {
    pub id: i64,
    pub title: String,
    pub published_at: Option<i64>,
    pub summary: String,
}

#[derive(Clone, Debug, Serialize)]
pub struct FeedPage {
    pub posts: Vec<PostSummary>,
    pub next_cursor: Option<String>,
}

#[derive(Clone, Debug, Default)]
pub struct SidebarData {
    pub drafts: Option<Vec<PostSummary>>,
    pub recent_posts: Vec<PostSummary>,
    pub archive_months: Vec<String>,
    pub tags: Vec<String>,
}

#[derive(Clone, Debug)]
pub struct FeedCursor {
    pub published_at: i64,
    pub id: i64,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum PublishPostResult {
    Published,
    NotFound,
    AlreadyPublished,
    MissingText,
    MissingTitle,
    FutureTimestamp,
    InvalidTimestamp,
}

#[derive(Clone, Debug, Serialize)]
pub struct PostBlock {
    pub id: i64,
    pub position: i64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub header: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub body: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub content_type: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub storage_key: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub alt: Option<String>,
    pub children: Vec<PostBlock>,
}

#[derive(Clone, Debug, Serialize)]
pub struct Post {
    #[serde(flatten)]
    pub summary: PostSummary,
    #[serde(skip)]
    pub published: bool,
    #[serde(skip)]
    pub author_username: String,
    pub tags: Vec<String>,
    pub revision: i64,
    pub blocks: Vec<PostBlock>,
}

#[derive(Clone, Debug)]
pub struct NewPost {
    pub author_username: String,
    pub title: String,
    pub published_at: Option<i64>,
    pub summary: String,
    pub tags: Vec<String>,
    pub blocks: Vec<NewBlock>,
}

#[derive(Clone, Debug)]
pub struct NewBlock {
    pub id: Option<i64>,
    pub header: Option<String>,
    pub body: Option<String>,
    pub storage_key: Option<String>,
    pub content_type: Option<String>,
    pub alt: Option<String>,
    pub children: Vec<NewBlock>,
}

#[derive(Clone, Debug)]
pub struct MediaReference {
    pub storage_key: String,
    pub content_type: String,
}

#[derive(Clone, Debug)]
pub enum SaveDraftResult {
    Saved(Post),
    NotFound,
    Conflict,
    Published,
}

#[derive(Clone, Debug)]
pub struct ShareLinkCreated {
    pub id: String,
    pub expires_at_unix: i64,
}

#[derive(Clone, Debug)]
pub struct ShareLinkView {
    pub id: String,
    pub post_id: i64,
    pub expires_at: Duration,
    pub expires_at_utc: Option<String>,
    pub revoked_at: Option<Duration>,
}

impl Database {
    pub fn new(path: PathBuf) -> Self {
        Self { path: Arc::new(path) }
    }

    pub async fn initialize(&self) -> Result<(), String> {
        let (user_version, has_site_tables, published_at_type) = self.run(|connection| {
            let version = connection.pragma_query_value(None, "user_version", |row| row.get::<_, i64>(0))?;
            let has_site_tables = table_exists(connection, "posts")?
                || table_exists(connection, "post_blocks")?
                || table_exists(connection, "users")?
                || table_exists(connection, "sessions")?
                || table_exists(connection, "login_throttles")?
                || table_exists(connection, "share_links")?
                || table_exists(connection, "share_sessions")?
                || table_exists(connection, "site_settings")?
                || table_exists(connection, "media_assets")?;
            let published_at_type = if table_exists(connection, "posts")? {
                connection.query_row(
                    "SELECT type FROM pragma_table_info('posts') WHERE name = 'published_at'",
                    [],
                    |row| row.get::<_, String>(0),
                ).optional()?
            } else {
                None
            };
            Ok((version, has_site_tables, published_at_type))
        }).await?;
        if published_at_type.is_some_and(|column_type| !column_type.eq_ignore_ascii_case("INTEGER")) {
            return Err(REBUILD_DATABASE_MESSAGE.to_owned());
        }
        if (user_version != 0 && user_version != CURRENT_SCHEMA_VERSION)
            || (user_version == 0 && has_site_tables)
        {
            return Err(REBUILD_DATABASE_MESSAGE.to_owned());
        }
        self.run(initialize_schema).await
    }

    #[cfg(test)]
    pub async fn replace_posts(&self, posts: Vec<NewPost>) -> Result<(), String> {
        self.replace_posts_and_add_accounts(
            posts,
            vec![ImportedAccount {
                username: "test-author".to_owned(),
                role: AccountRole::Read,
                password_hash: "unused-test-hash".to_owned(),
            }],
        )
            .await
            .map(|_| ())
    }

    pub async fn replace_posts_and_add_accounts(
        &self,
        posts: Vec<NewPost>,
        accounts: Vec<ImportedAccount>,
    ) -> Result<usize, String> {
        self.initialize().await?;
        let created_at = unix_time();
        let posts = posts
            .into_iter()
            .map(|post| {
                let tags = serde_json::to_string(&post.tags).map_err(|error| error.to_string())?;
                Ok((post, tags))
            })
            .collect::<Result<Vec<_>, String>>()?;
        self.run(move |connection| {
            let transaction = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
            let mut added_accounts = 0;
            {
                let mut insert_account = transaction.prepare(
                    "INSERT INTO users (username, role, password_hash, enabled, created_at) \
                     VALUES (?1, ?2, ?3, 1, ?4) ON CONFLICT(username) DO NOTHING",
                )?;
                for account in accounts {
                    added_accounts += insert_account.execute(params![
                        account.username,
                        account.role.as_str(),
                        account.password_hash,
                        unix_seconds(created_at),
                    ])?;
                }
            }
            let posts = posts
                .into_iter()
                .map(|(post, tags)| {
                    let author_id = transaction
                        .query_row(
                            "SELECT id FROM users WHERE username = ?1 COLLATE NOCASE",
                            [&post.author_username],
                            |row| row.get::<_, i64>(0),
                        )
                        .optional()?
                        .ok_or_else(|| {
                            rusqlite::Error::InvalidParameterName(format!(
                                "post author {:?} does not exist",
                                post.author_username
                            ))
                        })?;
                    Ok((post, tags, author_id))
                })
                .collect::<rusqlite::Result<Vec<_>>>()?;
            transaction.execute("DELETE FROM posts", [])?;
            {
                let mut insert_post = transaction.prepare(
                    "INSERT INTO posts (author_id, title, published_at, summary, published, tags) VALUES (?1, ?2, ?3, ?4, 1, ?5)",
                )?;
                let mut insert_block = transaction.prepare(
                    "INSERT INTO post_blocks (post_id, parent_id, position, header, body, storage_key, alt_text) \
                     VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
                )?;
                for (post, tags, author_id) in posts {
                    insert_post.execute(params![author_id, post.title, post.published_at, post.summary, tags])?;
                    let post_id = transaction.last_insert_rowid();
                    insert_blocks(&transaction, &mut insert_block, post_id, None, post.blocks)?;
                }
            }
            transaction.commit()
                .map(|_| added_accounts)
        })
        .await
    }

    pub async fn create_draft(
        &self,
        author_username: String,
        title: String,
        summary: String,
        tags: Vec<String>,
        blocks: Vec<NewBlock>,
    ) -> Result<i64, String> {
        self.initialize().await?;
        let tags = serde_json::to_string(&tags).map_err(|error| error.to_string())?;
        self.run(move |connection| {
            let transaction = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
            let title = if title.trim().is_empty() { String::new() } else { title };
            let author_id = transaction.query_row(
                "SELECT id FROM users WHERE username = ?1 COLLATE NOCASE",
                [&author_username],
                |row| row.get::<_, i64>(0),
            )?;
            transaction.execute(
                "INSERT INTO posts (author_id, title, published_at, summary, published, tags) \
                 VALUES (?1, ?2, NULL, ?3, 0, ?4)",
                params![author_id, title, summary, tags],
            )?;
            let post_id = transaction.last_insert_rowid();
            {
                let mut insert_block = transaction.prepare(
                    "INSERT INTO post_blocks (post_id, parent_id, position, header, body, storage_key, alt_text) \
                     VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
                )?;
                insert_blocks(&transaction, &mut insert_block, post_id, None, blocks)?;
            }
            transaction.commit()?;
            Ok(post_id)
        })
        .await
    }

    pub async fn save_draft(
        &self,
        id: i64,
        author_username: String,
        is_admin: bool,
        expected_revision: i64,
        title: String,
        summary: String,
        tags: Vec<String>,
        blocks: Vec<NewBlock>,
    ) -> Result<SaveDraftResult, String> {
        self.initialize().await?;
        let tags = serde_json::to_string(&tags).map_err(|error| error.to_string())?;
        self.run(move |connection| {
            let transaction = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
            let post = transaction.query_row(
                "SELECT p.revision, p.published, u.username FROM posts AS p \
                 JOIN users AS u ON u.id = p.author_id WHERE p.id = ?1",
                [id],
                |row| Ok((row.get::<_, i64>(0)?, row.get::<_, bool>(1)?, row.get::<_, String>(2)?)),
            ).optional()?;
            let Some((revision, published, owner)) = post else {
                return Ok(SaveDraftResult::NotFound);
            };
            if !is_admin && !owner.eq_ignore_ascii_case(&author_username) {
                return Ok(SaveDraftResult::NotFound);
            }
            if published {
                return Ok(SaveDraftResult::Published);
            }
            if revision != expected_revision {
                return Ok(SaveDraftResult::Conflict);
            }

            let title = if title.trim().is_empty() { String::new() } else { title };
            let existing = {
                let mut statement = transaction.prepare(
                    "SELECT id, parent_id, position FROM post_blocks WHERE post_id = ?1",
                )?;
                let rows = statement.query_map([id], |row| {
                    Ok((row.get::<_, i64>(0)?, row.get::<_, Option<i64>>(1)?, row.get::<_, i64>(2)?))
                })?;
                rows.collect::<rusqlite::Result<Vec<_>>>()?
            };
            let mut existing_by_id = HashMap::new();
            let mut max_position = 0_i64;
            for (block_id, parent_id, position) in existing {
                max_position = max_position.max(position);
                existing_by_id.insert(block_id, parent_id);
            }

            let mut desired_roots = Vec::new();
            let mut desired_children = Vec::new();
            let mut desired_ids = std::collections::HashSet::new();
            for root in &blocks {
                if root.storage_key.is_some() {
                    return Err(rusqlite::Error::InvalidParameterName("visible blocks cannot reference media assets".to_owned()));
                }
                if let Some(block_id) = root.id {
                    if !desired_ids.insert(block_id) || existing_by_id.get(&block_id) != Some(&None) {
                        return Err(rusqlite::Error::InvalidParameterName("draft block ID is invalid or duplicated".to_owned()));
                    }
                }
                desired_roots.push(root.id);
                for child in &root.children {
                    if child.storage_key.is_none() {
                        return Err(rusqlite::Error::InvalidParameterName("gallery items must reference a media asset".to_owned()));
                    }
                    if let Some(block_id) = child.id {
                        if !desired_ids.insert(block_id) || existing_by_id.get(&block_id).is_none_or(Option::is_none) {
                            return Err(rusqlite::Error::InvalidParameterName("gallery item ID is invalid or duplicated".to_owned()));
                        }
                    }
                    desired_children.push((root.id, child.id));
                }
            }
            let total_desired = i64::try_from(desired_ids.len() + desired_roots.iter().filter(|id| id.is_none()).count() + desired_children.iter().filter(|(_, id)| id.is_none()).count())
                .map_err(|_| rusqlite::Error::IntegralValueOutOfRange(0, i64::MAX))?;
            let offset = max_position
                .checked_add(total_desired)
                .and_then(|value| value.checked_add(1))
                .ok_or(rusqlite::Error::IntegralValueOutOfRange(0, max_position))?;
            transaction.execute(
                "UPDATE post_blocks SET position = position + ?1 WHERE post_id = ?2",
                params![offset, id],
            )?;

            let mut root_ids = Vec::with_capacity(blocks.len());
            let mut insert_block = transaction.prepare(
                "INSERT INTO post_blocks (post_id, parent_id, position, header, body, storage_key, alt_text) \
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
            )?;
            for (position, block) in blocks.iter().enumerate() {
                let block_id = if let Some(block_id) = block.id {
                    transaction.execute(
                        "UPDATE post_blocks SET parent_id = NULL, position = ?2, header = ?3, body = ?4, storage_key = NULL, alt_text = NULL \
                         WHERE id = ?1 AND post_id = ?5",
                        params![block_id, position as i64, block.header, block.body, id],
                    )?;
                    block_id
                } else {
                    insert_block.execute(params![id, None::<i64>, position as i64, block.header, block.body, None::<String>, None::<String>])?;
                    transaction.last_insert_rowid()
                };
                root_ids.push(block_id);
            }
            for (root_index, block) in blocks.iter().enumerate() {
                let parent_id = root_ids[root_index];
                for (position, child) in block.children.iter().enumerate() {
                    let storage_key = child.storage_key.as_ref().unwrap();
                    let content_type = child.content_type.as_deref().ok_or_else(|| {
                        rusqlite::Error::InvalidParameterName("gallery item is missing its content type".to_owned())
                    })?;
                    require_media_asset(&transaction, storage_key, content_type)?;
                    if let Some(block_id) = child.id {
                        transaction.execute(
                            "UPDATE post_blocks SET parent_id = ?2, position = ?3, header = ?4, body = ?5, storage_key = ?6, alt_text = ?7 \
                             WHERE id = ?1 AND post_id = ?8",
                            params![block_id, parent_id, position as i64, child.header, child.body, storage_key, child.alt, id],
                        )?;
                    } else {
                        insert_block.execute(params![id, parent_id, position as i64, child.header, child.body, storage_key, child.alt])?;
                    }
                }
            }

            for (block_id, parent_id) in &existing_by_id {
                let kept = desired_ids.contains(block_id)
                    || (parent_id.is_none() && desired_roots.iter().any(|id| id == &Some(*block_id)));
                if !kept && parent_id.is_some() {
                    transaction.execute("DELETE FROM post_blocks WHERE id = ?1", [block_id])?;
                }
            }
            for (block_id, parent_id) in &existing_by_id {
                if parent_id.is_none() && !desired_roots.iter().any(|id| id == &Some(*block_id)) {
                    transaction.execute("DELETE FROM post_blocks WHERE id = ?1", [block_id])?;
                }
            }
            drop(insert_block);

            let next_revision = revision.checked_add(1)
                .ok_or(rusqlite::Error::IntegralValueOutOfRange(0, revision))?;
            transaction.execute(
                "UPDATE posts SET title = ?2, summary = ?3, tags = ?4, revision = ?5 WHERE id = ?1",
                params![id, title, summary, tags, next_revision],
            )?;
            let saved_post = load_post(&transaction, id, PostAccess::Admin)?
                .ok_or(rusqlite::Error::QueryReturnedNoRows)?;
            transaction.commit()?;
            Ok(SaveDraftResult::Saved(saved_post))
        }).await
    }

    pub async fn draft_root_exists(
        &self,
        post_id: i64,
        block_id: i64,
        author_username: String,
        is_admin: bool,
    ) -> Result<bool, String> {
        self.run(move |connection| {
            connection.query_row(
                "SELECT EXISTS(SELECT 1 FROM post_blocks AS b JOIN posts AS p ON p.id = b.post_id \
                 WHERE p.id = ?1 AND b.id = ?2 AND b.parent_id IS NULL AND p.published = 0 \
                   AND (?3 = 1 OR p.author_id = (SELECT id FROM users WHERE username = ?4 COLLATE NOCASE)))",
                params![post_id, block_id, is_admin, author_username],
                |row| row.get(0),
            )
        }).await
    }

    pub async fn record_media_asset(
        &self,
        storage_key: String,
        content_type: String,
        size_bytes: u64,
    ) -> Result<(), String> {
        self.initialize().await?;
        let size_bytes = i64::try_from(size_bytes).map_err(|_| "media file size is too large".to_owned())?;
        self.run(move |connection| ensure_media_asset(connection, &storage_key, &content_type, Some(size_bytes))).await
    }

    pub async fn create_account(
        &self,
        username: String,
        role: AccountRole,
        password_hash: String,
    ) -> Result<(), String> {
        let created_at = unix_time();
        self.run(move |connection| {
            connection.execute(
                "INSERT INTO users (username, role, password_hash, enabled, created_at) VALUES (?1, ?2, ?3, 1, ?4)",
                params![username, role.as_str(), password_hash, unix_seconds(created_at)],
            )?;
            Ok(())
        })
        .await
    }

    pub async fn accounts(&self) -> Result<Vec<AccountView>, String> {
        self.run(|connection| {
            let mut statement = connection.prepare(
                "SELECT username, role, enabled FROM users ORDER BY role, username COLLATE NOCASE",
            )?;
            let rows = statement.query_map([], |row| {
                Ok(AccountView {
                    username: row.get(0)?,
                    role: account_role_from_db(&row.get::<_, String>(1)?)?,
                    enabled: row.get::<_, i64>(2)? != 0,
                })
            })?;
            rows.collect()
        })
        .await
    }

    pub async fn change_password(
        &self,
        username: String,
        password_hash: String,
    ) -> Result<(), String> {
        self.run(move |connection| {
            let transaction = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
            let changed = transaction.execute(
                "UPDATE users SET password_hash = ?2 WHERE username = ?1 COLLATE NOCASE",
                params![username, password_hash],
            )?;
            if changed == 0 {
                return Err(rusqlite::Error::QueryReturnedNoRows);
            }
            transaction.execute(
                "DELETE FROM sessions WHERE user_id = (SELECT id FROM users WHERE username = ?1 COLLATE NOCASE)",
                [&username],
            )?;
            transaction.commit()
        })
        .await
    }

    pub async fn set_account_enabled(&self, username: String, enabled: bool) -> Result<(), String> {
        self.run(move |connection| {
            let transaction = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
            let changed = transaction.execute(
                "UPDATE users SET enabled = ?2 WHERE username = ?1 COLLATE NOCASE",
                params![username, enabled],
            )?;
            if changed == 0 {
                return Err(rusqlite::Error::QueryReturnedNoRows);
            }
            if !enabled {
                transaction.execute(
                    "DELETE FROM sessions WHERE user_id = (SELECT id FROM users WHERE username = ?1 COLLATE NOCASE)",
                    [&username],
                )?;
            }
            transaction.commit()
        })
        .await
    }

    pub async fn login_account(&self, username: String) -> Result<Option<LoginAccount>, String> {
        self.run(move |connection| {
            connection
                .query_row(
                    "SELECT id, username, role, password_hash, enabled FROM users WHERE username = ?1 COLLATE NOCASE",
                    [username],
                    |row| {
                        Ok(LoginAccount {
                            id: row.get(0)?,
                            username: row.get(1)?,
                            role: account_role_from_db(&row.get::<_, String>(2)?)?,
                            password_hash: row.get(3)?,
                            enabled: row.get::<_, i64>(4)? != 0,
                        })
                    },
                )
                .optional()
        })
        .await
    }

    pub async fn issue_session(
        &self,
        user_id: i64,
        token_digest: String,
        created_at: Duration,
        expires_at: Duration,
    ) -> Result<(), String> {
        self.run(move |connection| {
            let transaction = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
            let created_at_seconds = unix_seconds(created_at);
            let expires_at_seconds = unix_seconds(expires_at);
            transaction.execute("DELETE FROM sessions WHERE expires_at <= ?1", [created_at_seconds])?;
            transaction.execute(
                "INSERT INTO sessions (token_digest, user_id, created_at, expires_at) VALUES (?1, ?2, ?3, ?4)",
                params![token_digest, user_id, created_at_seconds, expires_at_seconds],
            )?;
            transaction.commit()
        })
        .await
    }

    pub async fn authenticated_account(
        &self,
        token_digest: String,
        now: Duration,
    ) -> Result<Option<AuthenticatedAccount>, String> {
        self.run(move |connection| {
            let now_seconds = unix_seconds(now);
            connection
                .query_row(
                    "SELECT u.username, u.role FROM sessions AS s \
                     JOIN users AS u ON u.id = s.user_id \
                     WHERE s.token_digest = ?1 AND s.expires_at > ?2 AND u.enabled = 1",
                    params![token_digest, now_seconds],
                    |row| {
                        Ok(AuthenticatedAccount {
                            username: row.get(0)?,
                            role: account_role_from_db(&row.get::<_, String>(1)?)?,
                        })
                    },
                )
                .optional()
        })
        .await
    }

    pub async fn revoke_session(&self, token_digest: String) -> Result<(), String> {
        self.run(move |connection| {
            connection.execute("DELETE FROM sessions WHERE token_digest = ?1", [token_digest])?;
            Ok(())
        })
        .await
    }

    pub async fn share_link_lifetime(&self) -> Result<Duration, String> {
        self.run(|connection| {
            connection.query_row(
                "SELECT integer_value FROM site_settings WHERE name = 'share_link_lifetime_seconds'",
                [],
                |row| row.get::<_, i64>(0).map(duration_from_seconds),
            )
        })
        .await
    }

    pub async fn set_share_link_lifetime(&self, lifetime: Duration) -> Result<(), String> {
        if lifetime.as_secs() == 0 || lifetime.as_secs() > i64::MAX as u64 {
            return Err("share link lifetime must be a positive integer number of seconds".to_owned());
        }
        let seconds = unix_seconds(lifetime);
        self.run(move |connection| {
            connection.execute(
                "UPDATE site_settings SET integer_value = ?1 WHERE name = 'share_link_lifetime_seconds'",
                [seconds],
            )?;
            Ok(())
        })
        .await
    }

    pub async fn create_share_link(
        &self,
        id: String,
        post_id: i64,
        token_digest: String,
        created_at: Duration,
        access: ShareAccess,
    ) -> Result<Option<ShareLinkCreated>, String> {
        self.run(move |connection| {
            let transaction = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
            let created_at_seconds = unix_seconds(created_at);
            let (is_admin, author_username) = share_access_args(&access);
            let published = transaction.query_row(
                "SELECT EXISTS(SELECT 1 FROM posts WHERE id = ?1 AND published = 1 \
                 AND (?2 = 1 OR author_id = (SELECT id FROM users WHERE username = ?3 COLLATE NOCASE)))",
                params![post_id, is_admin, author_username],
                |row| row.get::<_, bool>(0),
            )?;
            if !published {
                transaction.commit()?;
                return Ok(None);
            }
            let lifetime_seconds = transaction.query_row(
                "SELECT integer_value FROM site_settings WHERE name = 'share_link_lifetime_seconds'",
                [],
                |row| row.get::<_, i64>(0),
            )?;
            let expires_at_seconds = created_at_seconds
                .checked_add(lifetime_seconds)
                .ok_or(rusqlite::Error::IntegralValueOutOfRange(0, lifetime_seconds))?;
            transaction.execute(
                "INSERT INTO share_links (id, post_id, token_digest, created_at, expires_at) \
                 VALUES (?1, ?2, ?3, ?4, ?5)",
                params![id, post_id, token_digest, created_at_seconds, expires_at_seconds],
            )?;
            transaction.commit()?;
            Ok(Some(ShareLinkCreated {
                id,
                expires_at_unix: expires_at_seconds,
            }))
        })
        .await
    }

    pub async fn share_links(&self) -> Result<Vec<ShareLinkView>, String> {
        self.run(|connection| {
            let mut statement = connection.prepare(
                "SELECT id, post_id, expires_at, \
                 strftime('%Y-%m-%dT%H:%M:%SZ', expires_at, 'unixepoch'), revoked_at \
                 FROM share_links \
                 ORDER BY created_at DESC, id",
            )?;
            let rows = statement.query_map([], |row| {
                Ok(ShareLinkView {
                    id: row.get(0)?,
                    post_id: row.get(1)?,
                    expires_at: duration_from_seconds(row.get(2)?),
                    expires_at_utc: row.get(3)?,
                    revoked_at: row.get::<_, Option<i64>>(4)?.map(duration_from_seconds),
                })
            })?;
            rows.collect()
        })
        .await
    }

    pub async fn revoke_share_link(
        &self,
        id: String,
        revoked_at: Duration,
        access: ShareAccess,
    ) -> Result<bool, String> {
        self.run(move |connection| {
            let (is_admin, author_username) = share_access_args(&access);
            let changed = connection.execute(
                "UPDATE share_links SET revoked_at = COALESCE(revoked_at, ?2) \
                 WHERE id = ?1 AND EXISTS (\
                     SELECT 1 FROM posts AS p WHERE p.id = share_links.post_id AND p.published = 1 \
                       AND (?3 = 1 OR p.author_id = (SELECT id FROM users WHERE username = ?4 COLLATE NOCASE))\
                 )",
                params![id, unix_seconds(revoked_at), is_admin, author_username],
            )?;
            Ok(changed != 0)
        })
        .await
    }

    pub async fn issue_share_session(
        &self,
        share_link_id: String,
        link_token_digest: String,
        session_token_digest: String,
        created_at: Duration,
    ) -> Result<Option<(i64, Duration)>, String> {
        self.run(move |connection| {
            let transaction = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
            let created_at_seconds = unix_seconds(created_at);
            let link = transaction
                .query_row(
                    "SELECT l.post_id, l.expires_at FROM share_links AS l \
                     JOIN posts AS p ON p.id = l.post_id \
                     WHERE l.id = ?1 AND l.token_digest = ?2 AND l.expires_at > ?3 \
                       AND l.revoked_at IS NULL AND p.published = 1",
                    params![share_link_id, link_token_digest, created_at_seconds],
                    |row| Ok((row.get::<_, i64>(0)?, row.get::<_, i64>(1)?)),
                )
                .optional()?;
            let Some((post_id, expires_at_seconds)) = link else {
                transaction.commit()?;
                return Ok(None);
            };
            transaction.execute(
                "INSERT INTO share_sessions (token_digest, share_link_id, created_at, expires_at) \
                 VALUES (?1, ?2, ?3, ?4)",
                params![session_token_digest, share_link_id, created_at_seconds, expires_at_seconds],
            )?;
            transaction.commit()?;
            Ok(Some((post_id, duration_from_seconds(expires_at_seconds))))
        })
        .await
    }

    pub async fn shared_post(
        &self,
        share_link_id: String,
        session_token_digest: String,
        post_id: i64,
        now: Duration,
    ) -> Result<Option<Post>, String> {
        self.run(move |connection| {
            let transaction = connection.transaction()?;
            let now_seconds = unix_seconds(now);
            let authorized = transaction.query_row(
                "SELECT EXISTS(SELECT 1 FROM share_sessions AS s \
                 JOIN share_links AS l ON l.id = s.share_link_id \
                 JOIN posts AS p ON p.id = l.post_id \
                 WHERE s.token_digest = ?1 AND s.share_link_id = ?2 \
                   AND s.expires_at > ?4 AND l.expires_at > ?4 \
                   AND l.revoked_at IS NULL AND p.published = 1 AND p.id = ?3)",
                params![session_token_digest, share_link_id, post_id, now_seconds],
                |row| row.get::<_, bool>(0),
            )?;
            let post = if authorized {
                load_post(&transaction, post_id, PostAccess::Published)?
            } else {
                None
            };
            transaction.commit()?;
            Ok(post)
        })
        .await
    }

    pub async fn shared_media_reference(
        &self,
        share_link_id: String,
        session_token_digest: String,
        post_id: i64,
        block_id: i64,
        now: Duration,
    ) -> Result<Option<MediaReference>, String> {
        self.run(move |connection| {
            let now_seconds = unix_seconds(now);
            connection
                .query_row(
                "SELECT b.storage_key, a.content_type FROM share_sessions AS s \
                 JOIN share_links AS l ON l.id = s.share_link_id \
                 JOIN posts AS p ON p.id = l.post_id \
                 JOIN post_blocks AS b ON b.post_id = p.id \
                 JOIN media_assets AS a ON a.storage_key = b.storage_key \
                     WHERE s.token_digest = ?1 AND s.share_link_id = ?2 \
                       AND s.expires_at > ?5 AND l.expires_at > ?5 \
                       AND l.revoked_at IS NULL AND p.published = 1 \
                       AND p.id = ?3 AND b.id = ?4 AND b.storage_key IS NOT NULL \
                       AND (a.content_type LIKE 'image/%' OR a.content_type LIKE 'video/%')",
                    params![session_token_digest, share_link_id, post_id, block_id, now_seconds],
                    |row| {
                        Ok(MediaReference {
                            storage_key: row.get(0)?,
                            content_type: row.get(1)?,
                        })
                    },
                )
                .optional()
        })
        .await
    }

    pub async fn record_login_attempt(
        &self,
        username_digest: String,
        now: Duration,
        window: Duration,
        maximum_attempts: i64,
    ) -> Result<bool, String> {
        self.run(move |connection| {
            let transaction = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
            let now_seconds = unix_seconds(now);
            let window_seconds = unix_seconds(window);
            let window_start_seconds = now_seconds.saturating_sub(window_seconds);
            transaction.execute(
                "DELETE FROM login_throttles WHERE window_started <= ?1",
                [window_start_seconds],
            )?;
            let existing = transaction
                .query_row(
                    "SELECT window_started, attempts FROM login_throttles WHERE username_digest = ?1",
                    [&username_digest],
                    |row| Ok((row.get::<_, i64>(0)?, row.get::<_, i64>(1)?)),
                )
                .optional()?;
            if let Some((started_seconds, attempts)) = existing {
                if now_seconds.saturating_sub(started_seconds) <= window_seconds && attempts >= maximum_attempts {
                    transaction.commit()?;
                    return Ok(false);
                }
            }
            transaction.execute(
                "DELETE FROM login_throttles WHERE username_digest = (\
                    SELECT username_digest FROM login_throttles ORDER BY window_started ASC LIMIT 1\
                 ) AND (SELECT COUNT(*) FROM login_throttles) >= 10000",
                [],
            )?;
            match existing {
                Some((started_seconds, attempts)) if now_seconds.saturating_sub(started_seconds) <= window_seconds => {
                    transaction.execute(
                        "UPDATE login_throttles SET attempts = ?2 WHERE username_digest = ?1",
                        params![username_digest, attempts.saturating_add(1)],
                    )?;
                }
                _ => {
                    transaction.execute(
                        "INSERT OR REPLACE INTO login_throttles (username_digest, window_started, attempts) VALUES (?1, ?2, 1)",
                        params![username_digest, now_seconds],
                    )?;
                }
            }
            transaction.commit()?;
            Ok(true)
        })
        .await
    }

    pub async fn clear_login_attempts(&self, username_digest: String) -> Result<(), String> {
        self.run(move |connection| {
            connection.execute(
                "DELETE FROM login_throttles WHERE username_digest = ?1",
                [username_digest],
            )?;
            Ok(())
        })
        .await
    }

    pub async fn feed(
        &self,
        limit: usize,
        after: Option<FeedCursor>,
    ) -> Result<FeedPage, String> {
        self.feed_filtered(limit, after, None).await
    }

    pub async fn feed_filtered(
        &self,
        limit: usize,
        after: Option<FeedCursor>,
        tag: Option<String>,
    ) -> Result<FeedPage, String> {
        self.feed_filtered_with_month(limit, after, tag, None, "UTC".to_owned()).await
    }

    pub async fn feed_filtered_with_month(
        &self,
        limit: usize,
        after: Option<FeedCursor>,
        tag: Option<String>,
        month: Option<String>,
        timezone: String,
    ) -> Result<FeedPage, String> {
        let month_bounds = month
            .as_deref()
            .map(|month| local_month_bounds(month, &timezone))
            .transpose()?;
        self.run(move |connection| {
            let after_date = after.as_ref().map(|cursor| cursor.published_at);
            let after_id = after.as_ref().map(|cursor| cursor.id);
            let month_start = month_bounds.map(|bounds| bounds.0);
            let month_end = month_bounds.map(|bounds| bounds.1);
            let mut statement = connection.prepare(
                "SELECT id, title, published_at, summary FROM posts \
                 WHERE published = 1 \
                   AND (?1 IS NULL OR EXISTS (\
                       SELECT 1 FROM json_each(posts.tags) AS post_tag \
                       WHERE post_tag.type = 'text' AND post_tag.value = ?1 \
                   )) \
                   AND (?2 IS NULL OR published_at < ?2 OR (published_at = ?2 AND id < ?3)) \
                   AND (?4 IS NULL OR published_at >= ?4) \
                   AND (?5 IS NULL OR published_at < ?5) \
                 ORDER BY published_at DESC, id DESC LIMIT ?6",
            )?;
            let rows = statement.query_map(
                params![tag, after_date, after_id, month_start, month_end, (limit + 1) as i64],
                post_summary_from_row,
            )?;
            let mut posts = rows.collect::<rusqlite::Result<Vec<_>>>()?;
            let has_more = posts.len() > limit;
            posts.truncate(limit);
            let next_cursor = if has_more {
                posts.last().and_then(|post| {
                    post.published_at
                        .as_ref()
                        .map(|published_at| format!("{published_at}:{}", post.id))
                })
            } else {
                None
            };
            Ok(FeedPage { posts, next_cursor })
        })
        .await
    }

    pub async fn sidebar_data(&self, timezone: String) -> Result<SidebarData, String> {
        self.run(move |connection| {
            let recent_posts = {
                let mut statement = connection.prepare(
                    "SELECT id, title, published_at, summary FROM posts \
                     WHERE published = 1 ORDER BY published_at DESC, id DESC LIMIT 5",
                )?;
                let rows = statement.query_map([], post_summary_from_row)?;
                rows.collect::<rusqlite::Result<Vec<_>>>()?
            };
            let archive_months = {
                let mut statement = connection.prepare(
                    "SELECT published_at FROM posts WHERE published = 1 ORDER BY published_at DESC, id DESC",
                )?;
                let rows = statement.query_map([], |row| row.get::<_, i64>(0))?;
                let mut months = std::collections::BTreeSet::new();
                for row in rows {
                    let timestamp = jiff::Timestamp::from_second(row?)
                        .map_err(|error| rusqlite::Error::ToSqlConversionFailure(Box::new(error)))?;
                    let zoned = timestamp.in_tz(timezone.as_str())
                        .map_err(|error| rusqlite::Error::ToSqlConversionFailure(Box::new(error)))?;
                    let date = zoned.datetime().date();
                    months.insert(format!("{:04}-{:02}", date.year(), date.month()));
                }
                months.into_iter().rev().collect::<Vec<_>>()
            };
            let tags = {
                let mut statement = connection.prepare(
                    "SELECT tag FROM ( \
                         SELECT DISTINCT post_tag.value AS tag FROM posts \
                         JOIN json_each(posts.tags) AS post_tag \
                         WHERE posts.published = 1 AND post_tag.type = 'text' \
                     ) ORDER BY tag COLLATE NOCASE, tag",
                )?;
                let rows = statement.query_map([], |row| row.get(0))?;
                rows.collect::<rusqlite::Result<Vec<_>>>()?
            };
            Ok(SidebarData { drafts: None, recent_posts, archive_months, tags })
        })
        .await
    }

    pub async fn publish_draft(
        &self,
        id: i64,
        author_username: String,
        is_admin: bool,
        requested_published_at: Option<i64>,
    ) -> Result<PublishPostResult, String> {
        self.initialize().await?;
        self.run(move |connection| {
            let transaction = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
            let post = transaction.query_row(
                "SELECT p.published, p.title, u.username FROM posts AS p \
                 JOIN users AS u ON u.id = p.author_id WHERE p.id = ?1",
                [id],
                |row| Ok((row.get::<_, bool>(0)?, row.get::<_, String>(1)?, row.get::<_, String>(2)?)),
            ).optional()?;
            let Some((published, title, owner)) = post else {
                return Ok(PublishPostResult::NotFound);
            };
            if !is_admin && !owner.eq_ignore_ascii_case(&author_username) {
                return Ok(PublishPostResult::NotFound);
            }
            if published {
                return Ok(PublishPostResult::AlreadyPublished);
            }
            if title.trim().is_empty() {
                return Ok(PublishPostResult::MissingTitle);
            }
            let now = unix_seconds(unix_time());
            if requested_published_at.is_some_and(|published_at| published_at > now) {
                return Ok(PublishPostResult::FutureTimestamp);
            }
            if requested_published_at.is_some_and(|published_at| {
                jiff::Timestamp::from_second(published_at).is_err()
            }) {
                return Ok(PublishPostResult::InvalidTimestamp);
            }
            let has_text = {
                let mut statement = transaction.prepare(
                    "SELECT header, body FROM post_blocks WHERE post_id = ?1",
                )?;
                let rows = statement.query_map([id], |row| {
                    Ok((row.get::<_, Option<String>>(0)?, row.get::<_, Option<String>>(1)?))
                })?;
                let mut has_text = false;
                for row in rows {
                    let (header, body) = row?;
                    if header.as_deref().is_some_and(|value| !value.trim().is_empty())
                        || body.as_deref().is_some_and(|value| !value.trim().is_empty())
                    {
                        has_text = true;
                        break;
                    }
                }
                has_text
            };
            if !has_text {
                return Ok(PublishPostResult::MissingText);
            }
            let published_at = requested_published_at.unwrap_or(now);
            transaction.execute(
                "UPDATE posts SET published = 1, published_at = ?1 WHERE id = ?2 AND published = 0",
                params![published_at, id],
            )?;
            transaction.commit()?;
            Ok(PublishPostResult::Published)
        }).await
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

    pub async fn drafts(&self, author_username: Option<String>) -> Result<Vec<PostSummary>, String> {
        self.run(move |connection| {
            let mut statement = connection.prepare(
                "SELECT id, title, published_at, summary FROM posts \
                 WHERE published = 0 AND (?1 IS NULL OR author_id = (\
                     SELECT id FROM users WHERE username = ?1 COLLATE NOCASE\
                 )) ORDER BY id DESC",
            )?;
            let rows = statement.query_map([author_username], post_summary_from_row)?;
            rows.collect()
        })
        .await
    }

    pub async fn post(&self, id: i64) -> Result<Option<Post>, String> {
        self.post_with_access(id, PostAccess::Published).await
    }

    pub async fn post_with_access(
        &self,
        id: i64,
        access: PostAccess,
    ) -> Result<Option<Post>, String> {
        self.run(move |connection| {
            let transaction = connection.transaction()?;
            let post = load_post(&transaction, id, access)?;
            transaction.commit()?;
            Ok(post)
        })
        .await
    }

    pub async fn post_for_share_preview(
        &self,
        id: i64,
        access: ShareAccess,
    ) -> Result<Option<Post>, String> {
        self.run(move |connection| {
            let transaction = connection.transaction()?;
            let (is_admin, author_username) = share_access_args(&access);
            let post = load_post_with_query(
                &transaction,
                id,
                "p.published = 1 AND (?2 = 1 OR p.author_id = (SELECT id FROM users WHERE username = ?3 COLLATE NOCASE))",
                is_admin,
                author_username,
            )?;
            transaction.commit()?;
            Ok(post)
        })
        .await
    }

    #[cfg(test)]
    pub async fn media_reference(
        &self,
        post_id: i64,
        block_id: i64,
    ) -> Result<Option<MediaReference>, String> {
        self.media_reference_with_access(post_id, block_id, PostAccess::Published).await
    }

    pub async fn media_reference_with_access(
        &self,
        post_id: i64,
        block_id: i64,
        access: PostAccess,
    ) -> Result<Option<MediaReference>, String> {
        self.run(move |connection| {
            let (is_admin, author_username) = access.query_args();
            connection
                .query_row(
                    "SELECT b.storage_key, a.content_type \
                     FROM post_blocks AS b JOIN posts AS p ON p.id = b.post_id \
                     JOIN media_assets AS a ON a.storage_key = b.storage_key \
                     WHERE p.id = ?1 AND (p.published = 1 OR ?3 = 1 OR (\
                         ?4 IS NOT NULL AND p.author_id = (SELECT id FROM users WHERE username = ?4 COLLATE NOCASE)\
                     )) AND b.id = ?2 \
                       AND b.storage_key IS NOT NULL \
                       AND (a.content_type LIKE 'image/%' OR a.content_type LIKE 'video/%')",
                    params![post_id, block_id, is_admin, author_username],
                    |row| {
                        Ok(MediaReference {
                            storage_key: row.get(0)?,
                            content_type: row.get(1)?,
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
            if table_exists(&connection, "share_sessions").map_err(|error| error.to_string())? {
                connection
                    .execute("DELETE FROM share_sessions WHERE expires_at <= ?1", [unix_seconds(unix_time())])
                    .map_err(|error| error.to_string())?;
            }
            operation(&mut connection).map_err(|error| error.to_string())
        })
        .await
        .map_err(|error| format!("database task failed: {error}"))?
    }
}

fn initialize_schema(connection: &mut Connection) -> rusqlite::Result<()> {
    let transaction = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
    transaction.execute_batch(POSTS_SCHEMA)?;
    transaction.execute_batch(MEDIA_ASSETS_SCHEMA)?;
    transaction.execute_batch(POST_BLOCKS_SCHEMA)?;
    transaction.execute_batch(AUTH_SCHEMA)?;
    transaction.execute_batch(SHARE_SCHEMA)?;
    transaction.pragma_update(None, "user_version", CURRENT_SCHEMA_VERSION)?;
    transaction.commit()
}

pub(crate) fn local_month_bounds(month: &str, timezone: &str) -> Result<(i64, i64), String> {
    if month.len() != 7
        || month.as_bytes()[4] != b'-'
        || !month.as_bytes().iter().enumerate().all(|(index, byte)| {
            index == 4 || byte.is_ascii_digit()
        })
    {
        return Err("invalid archive month".to_owned());
    }
    let year = month[..4]
        .parse::<i16>()
        .map_err(|error| error.to_string())?;
    let month_number = month[5..]
        .parse::<i8>()
        .map_err(|error| error.to_string())?;
    let start_date = jiff::civil::Date::new(year, month_number, 1)
        .map_err(|error| error.to_string())?;
    let (next_year, next_month) = if month_number == 12 {
        (year + 1, 1)
    } else {
        (year, month_number + 1)
    };
    let end_date = jiff::civil::Date::new(next_year, next_month, 1)
        .map_err(|error| error.to_string())?;
    let start = start_date
        .at(0, 0, 0, 0)
        .in_tz(timezone)
        .map_err(|error| error.to_string())?
        .timestamp()
        .as_second();
    let end = end_date
        .at(0, 0, 0, 0)
        .in_tz(timezone)
        .map_err(|error| error.to_string())?
        .timestamp()
        .as_second();
    Ok((start, end))
}

fn load_post(
    connection: &Connection,
    id: i64,
    access: PostAccess,
) -> rusqlite::Result<Option<Post>> {
    let (is_admin, author_username) = access.query_args();
    load_post_with_query(
        connection,
        id,
        "p.published = 1 OR ?2 = 1 OR (?3 IS NOT NULL AND p.author_id = (SELECT id FROM users WHERE username = ?3 COLLATE NOCASE))",
        is_admin,
        author_username,
    )
}

fn load_post_with_query(
    connection: &Connection,
    id: i64,
    access_predicate: &str,
    is_admin: bool,
    author_username: Option<&str>,
) -> rusqlite::Result<Option<Post>> {
    let query = format!(
        "SELECT p.id, p.title, p.published_at, p.summary, p.tags, p.published, u.username, p.revision \
         FROM posts AS p JOIN users AS u ON u.id = p.author_id \
         WHERE p.id = ?1 AND ({access_predicate})"
    );
    let post_row = connection
        .query_row(
            &query,
            params![id, is_admin, author_username],
            |row| {
                let tags_json: String = row.get(4)?;
                let tags = serde_json::from_str(&tags_json).map_err(|error| {
                    rusqlite::Error::FromSqlConversionFailure(
                        4,
                        Type::Text,
                        Box::new(error),
                    )
                })?;
                Ok((post_summary_from_row(row)?, tags, row.get::<_, bool>(5)?, row.get::<_, String>(6)?, row.get::<_, i64>(7)?))
            },
        )
        .optional()?;
    let Some((summary, tags, published, author_username, revision)) = post_row else {
        return Ok(None);
    };

    let (mut blocks, mut children) = {
        let mut statement = connection.prepare(
            "SELECT b.id, b.parent_id, b.position, b.header, b.body, a.content_type, b.storage_key, b.alt_text \
             FROM post_blocks AS b LEFT JOIN media_assets AS a ON a.storage_key = b.storage_key \
             WHERE b.post_id = ?1 \
             ORDER BY b.parent_id IS NOT NULL, b.parent_id, b.position",
        )?;
        let rows = statement.query_map([id], |row| {
            let parent_id: Option<i64> = row.get(1)?;
            let block = PostBlock {
                id: row.get(0)?,
                position: row.get(2)?,
                header: row.get(3)?,
                body: row.get(4)?,
                content_type: row.get(5)?,
                storage_key: row.get(6)?,
                alt: row.get(7)?,
                children: Vec::new(),
            };
            Ok((parent_id, block))
        })?;
        let mut blocks = Vec::new();
        let mut children = HashMap::<i64, Vec<PostBlock>>::new();
        for row in rows {
            let (parent_id, block) = row?;
            if let Some(parent_id) = parent_id {
                children.entry(parent_id).or_default().push(block);
            } else {
                blocks.push(block);
            }
        }
        (blocks, children)
    };
    for block in &mut blocks {
        block.children = children.remove(&block.id).unwrap_or_default();
    }
    Ok(Some(Post {
        summary,
        published,
        author_username,
        tags,
        revision,
        blocks,
    }))
}

fn account_role_from_db(value: &str) -> rusqlite::Result<AccountRole> {
    match value {
        "read" => Ok(AccountRole::Read),
        "write" => Ok(AccountRole::Write),
        "admin" => Ok(AccountRole::Admin),
        _ => Err(rusqlite::Error::FromSqlConversionFailure(
            0,
            Type::Text,
            Box::new(std::io::Error::other(format!("unknown account role {value:?}"))),
        )),
    }
}

fn unix_time() -> Duration {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
}

fn unix_seconds(duration: Duration) -> i64 {
    duration.as_secs().min(i64::MAX as u64) as i64
}

fn duration_from_seconds(seconds: i64) -> Duration {
    Duration::from_secs(u64::try_from(seconds).unwrap_or_default())
}

fn table_exists(connection: &Connection, table: &str) -> rusqlite::Result<bool> {
    connection.query_row(
        "SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE type = 'table' AND name = ?1)",
        [table],
        |row| row.get(0),
    )
}

fn share_access_args(access: &ShareAccess) -> (bool, Option<&str>) {
    match access {
        ShareAccess::Author(username) => (false, Some(username)),
        ShareAccess::Admin => (true, None),
    }
}

fn insert_blocks(
    connection: &Connection,
    statement: &mut rusqlite::Statement<'_>,
    post_id: i64,
    parent_id: Option<i64>,
    blocks: Vec<NewBlock>,
) -> rusqlite::Result<()> {
    for (position, block) in blocks.into_iter().enumerate() {
        let NewBlock { id: _, header, body, storage_key, content_type, alt, children } = block;
        if let Some(storage_key) = storage_key.as_deref() {
            let content_type = content_type.as_deref().ok_or_else(|| {
                rusqlite::Error::InvalidParameterName("media placement is missing its content type".to_owned())
            })?;
            ensure_media_asset(connection, storage_key, content_type, None)?;
        }
        statement.execute(params![
            post_id,
            parent_id,
            position as i64,
            header,
            body,
            storage_key,
            alt,
        ])?;
        let block_id = connection.last_insert_rowid();
        insert_blocks(connection, statement, post_id, Some(block_id), children)?;
    }
    Ok(())
}

fn ensure_media_asset(
    connection: &Connection,
    storage_key: &str,
    content_type: &str,
    size_bytes: Option<i64>,
) -> rusqlite::Result<()> {
    connection.execute(
        "INSERT INTO media_assets (storage_key, content_type, size_bytes) VALUES (?1, ?2, ?3) \
         ON CONFLICT(storage_key) DO UPDATE SET size_bytes = COALESCE(media_assets.size_bytes, excluded.size_bytes)",
        params![storage_key, content_type, size_bytes],
    )?;
    let stored_content_type: String = connection.query_row(
        "SELECT content_type FROM media_assets WHERE storage_key = ?1",
        [storage_key],
        |row| row.get(0),
    )?;
    if stored_content_type != content_type {
        return Err(rusqlite::Error::InvalidParameterName(
            "the same media asset was supplied with conflicting content types".to_owned(),
        ));
    }
    Ok(())
}

fn require_media_asset(
    connection: &Connection,
    storage_key: &str,
    content_type: &str,
) -> rusqlite::Result<()> {
    let stored_content_type = connection
        .query_row(
            "SELECT content_type FROM media_assets WHERE storage_key = ?1",
            [storage_key],
            |row| row.get::<_, String>(0),
        )
        .optional()?;
    match stored_content_type {
        Some(stored_content_type) if stored_content_type == content_type => Ok(()),
        Some(_) => Err(rusqlite::Error::InvalidParameterName(
            "the media placement content type does not match its asset".to_owned(),
        )),
        None => Err(rusqlite::Error::InvalidParameterName(
            "the media placement references an asset that has not been uploaded".to_owned(),
        )),
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

#[cfg(test)]
mod tests {
    use std::{
        path::PathBuf,
        time::{Duration, SystemTime, UNIX_EPOCH},
    };

    use rusqlite::Connection;

    use super::{Database, NewBlock, NewPost, PostAccess, ShareAccess};

    fn test_database_path() -> PathBuf {
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        std::env::temp_dir().join(format!("journey-site-db-{}-{nonce}.sqlite3", std::process::id()))
    }

    fn post(published_at: &str, title: &str) -> NewPost {
        NewPost {
            author_username: "test-author".to_owned(),
            title: title.to_owned(),
            published_at: Some(
                format!("{published_at}T12:00:00Z")
                    .parse::<jiff::Timestamp>()
                    .unwrap()
                    .as_second(),
            ),
            summary: format!("Summary for {title}"),
            tags: Vec::new(),
            blocks: Vec::new(),
        }
    }

    #[tokio::test]
    async fn feed_paginates_in_date_and_id_order_across_equal_dates() {
        let path = test_database_path();
        let database = Database::new(path.clone());
        let posts = (1..=8)
            .map(|id| post("2026-04-01", &format!("newer {id}")))
            .chain((9..=12).map(|id| post("2026-03-31", &format!("older {id}"))))
            .collect();
        database.replace_posts(posts).await.unwrap();

        let first = database.feed(10, None).await.unwrap();
        assert_eq!(first.posts.len(), 10);
        assert_eq!(
            first.posts.iter().map(|post| post.id).collect::<Vec<_>>(),
            vec![8, 7, 6, 5, 4, 3, 2, 1, 12, 11]
        );
        let older_timestamp = "2026-03-31T12:00:00Z"
            .parse::<jiff::Timestamp>()
            .unwrap()
            .as_second();
        let expected_cursor = format!("{older_timestamp}:11");
        assert_eq!(first.next_cursor.as_deref(), Some(expected_cursor.as_str()));

        let second = database
            .feed(
                10,
                Some(super::FeedCursor {
                    published_at: older_timestamp,
                    id: 11,
                }),
            )
            .await
            .unwrap();
        assert_eq!(second.posts.iter().map(|post| post.id).collect::<Vec<_>>(), vec![10, 9]);
        assert_eq!(second.next_cursor, None);

        std::fs::remove_file(path).unwrap();
    }

    #[tokio::test]
    async fn outdated_database_requests_rebuild() {
        let path = test_database_path();
        let connection = Connection::open(&path).unwrap();
        connection.execute_batch(
            "CREATE TABLE posts (
                id INTEGER PRIMARY KEY AUTOINCREMENT,
                title TEXT NOT NULL,
                published_at TEXT NOT NULL,
                summary TEXT NOT NULL,
                published INTEGER NOT NULL DEFAULT 1
             );
             CREATE TABLE post_blocks (
                post_id INTEGER NOT NULL REFERENCES posts(id) ON DELETE CASCADE,
                position INTEGER NOT NULL,
                kind TEXT NOT NULL,
                text TEXT,
                level INTEGER,
                storage_key TEXT,
                content_type TEXT,
                alt_text TEXT,
                caption TEXT,
                PRIMARY KEY (post_id, position)
             );
             INSERT INTO posts (id, title, published_at, summary) VALUES (7, 'Old post', '2025-01-02', 'Old summary');
             INSERT INTO post_blocks VALUES (7, 0, 'heading', 'A heading', 3, NULL, NULL, NULL, NULL);
             INSERT INTO post_blocks VALUES (7, 1, 'paragraph', 'A paragraph', NULL, NULL, NULL, NULL, NULL);
             INSERT INTO post_blocks VALUES (7, 2, 'image', NULL, NULL, 'media/image-key', 'image/jpeg', 'An image', 'Image caption');
             INSERT INTO post_blocks VALUES (7, 3, 'video', NULL, NULL, 'media/video-key', 'video/mp4', NULL, 'Video caption');",
        )
        .unwrap();
        drop(connection);

        let database = Database::new(path.clone());
        let error = database.initialize().await.unwrap_err();
        assert!(error.contains("recreate the SQLite database"));
        assert!(error.contains("destructive importer"));

        std::fs::remove_file(path).unwrap();
    }

    #[tokio::test]
    async fn post_reads_tags_sibling_order_and_mixed_group_content() {
        let path = test_database_path();
        let database = Database::new(path.clone());
        let group = NewBlock {
            id: None,
            header: Some("Morning".to_owned()),
            body: Some("A quiet start".to_owned()),
            storage_key: None,
            content_type: None,
            alt: None,
            children: vec![
                NewBlock {
                    id: None,
                    header: None,
                    body: Some("Birdsong by the dock.".to_owned()),
                    storage_key: Some("media/child-video".to_owned()),
                    content_type: Some("video/mp4".to_owned()),
                    alt: Some("A short video".to_owned()),
                    children: Vec::new(),
                },
                NewBlock {
                    id: None,
                    header: None,
                    body: Some("The water was still.".to_owned()),
                    storage_key: Some("media/group-image".to_owned()),
                    content_type: Some("image/jpeg".to_owned()),
                    alt: Some("A lake at sunrise".to_owned()),
                    children: Vec::new(),
                },
            ],
        };
        let mut new_post = post("2026-01-01", "Tagged post");
        new_post.tags = vec!["Alps".to_owned(), "Morning walk".to_owned()];
        new_post.blocks = vec![
            NewBlock {
                id: None,
                header: None,
                body: Some("Before the group".to_owned()),
                storage_key: None,
                content_type: None,
                alt: None,
                children: Vec::new(),
            },
            group,
            NewBlock {
                id: None,
                header: Some("After the group".to_owned()),
                body: None,
                storage_key: None,
                content_type: None,
                alt: None,
                children: Vec::new(),
            },
        ];
        database.replace_posts(vec![new_post]).await.unwrap();

        let connection = Connection::open(&path).unwrap();
        let first_id = connection.query_row(
            "SELECT id FROM post_blocks WHERE post_id = 1 AND parent_id IS NULL AND position = 0",
            [],
            |row| row.get::<_, i64>(0),
        ).unwrap();
        let group_id = connection.query_row(
            "SELECT id FROM post_blocks WHERE post_id = 1 AND parent_id IS NULL AND position = 1",
            [],
            |row| row.get::<_, i64>(0),
        ).unwrap();
        let last_id = connection.query_row(
            "SELECT id FROM post_blocks WHERE post_id = 1 AND parent_id IS NULL AND position = 2",
            [],
            |row| row.get::<_, i64>(0),
        ).unwrap();
        connection.execute("UPDATE post_blocks SET position = 3 WHERE id = ?1", [first_id]).unwrap();
        connection.execute("UPDATE post_blocks SET position = 0 WHERE id = ?1", [group_id]).unwrap();
        connection.execute("UPDATE post_blocks SET position = 1 WHERE id = ?1", [last_id]).unwrap();
        connection.execute("UPDATE post_blocks SET position = 2 WHERE id = ?1", [first_id]).unwrap();
        drop(connection);

        let post = database.post(1).await.unwrap().unwrap();
        assert_eq!(post.tags, vec!["Alps", "Morning walk"]);
        assert_eq!(post.blocks.len(), 3);
        assert_eq!(post.blocks[0].id, group_id);
        assert_eq!(post.blocks[0].position, 0);
        assert_eq!(post.blocks[0].header.as_deref(), Some("Morning"));
        assert_eq!(post.blocks[0].body.as_deref(), Some("A quiet start"));
        assert_eq!(post.blocks[0].content_type, None);
        assert_eq!(post.blocks[0].children.len(), 2);
        assert_eq!(post.blocks[0].children[0].position, 0);
        assert_eq!(post.blocks[0].children[1].position, 1);
        assert_eq!(post.blocks[0].children[0].content_type.as_deref(), Some("video/mp4"));
        assert_eq!(post.blocks[0].children[1].content_type.as_deref(), Some("image/jpeg"));
        assert_eq!(post.blocks[1].id, last_id);
        assert_eq!(post.blocks[1].position, 1);
        assert_eq!(post.blocks[2].id, first_id);
        assert_eq!(post.blocks[2].position, 2);
        assert_eq!(post.blocks[2].body.as_deref(), Some("Before the group"));

        std::fs::remove_file(path).unwrap();
    }

    #[tokio::test]
    async fn database_rejects_blocks_nested_more_than_one_level() {
        let path = test_database_path();
        let database = Database::new(path.clone());
        database.initialize().await.unwrap();
        database
            .create_account("test-author".to_owned(), super::AccountRole::Read, "hash".to_owned())
            .await
            .unwrap();
        let connection = Connection::open(&path).unwrap();
        connection.execute_batch("PRAGMA foreign_keys = ON;").unwrap();
        connection.execute(
            "INSERT INTO posts (author_id, title, published_at, summary) VALUES (1, 'Post', 1767225600, 'Summary')",
            [],
        ).unwrap();
        connection.execute(
            "INSERT INTO media_assets (storage_key, content_type) VALUES ('media/test', 'image/jpeg')",
            [],
        ).unwrap();
        connection.execute(
            "INSERT INTO post_blocks (post_id, position) VALUES (1, 0)",
            [],
        ).unwrap();
        connection.execute(
            "INSERT INTO post_blocks (post_id, parent_id, position, storage_key) VALUES (1, 1, 0, 'media/test')",
            [],
        ).unwrap();
        let error = connection.execute(
            "INSERT INTO post_blocks (post_id, parent_id, position, storage_key) VALUES (1, 2, 0, 'media/test')",
            [],
        ).unwrap_err();
        assert!(error.to_string().contains("one level deep"));

        drop(connection);
        std::fs::remove_file(path).unwrap();
    }

    #[tokio::test]
    async fn media_lookups_use_block_ids_and_require_published_posts() {
        let path = test_database_path();
        let database = Database::new(path.clone());
        let mut media_post = post("2026-01-01", "Published post");
        media_post.blocks.push(NewBlock {
            id: None,
            header: None,
            body: Some("A block".to_owned()),
            storage_key: None,
            content_type: None,
            alt: None,
            children: vec![NewBlock {
                id: None,
                header: None,
                body: None,
                storage_key: Some("media/image-key".to_owned()),
                content_type: Some("image/jpeg".to_owned()),
                alt: None,
                children: Vec::new(),
            }],
        });
        database.replace_posts(vec![media_post]).await.unwrap();
        let published = database.post(1).await.unwrap().unwrap();
        let block_id = published.blocks[0].children[0].id;
        assert_eq!(
            database.media_reference(1, block_id).await.unwrap().unwrap().storage_key,
            "media/image-key"
        );

        let connection = Connection::open(&path).unwrap();
        connection.execute("UPDATE posts SET published = 0 WHERE id = 1", []).unwrap();
        drop(connection);
        assert!(database.media_reference(1, block_id).await.unwrap().is_none());

        std::fs::remove_file(path).unwrap();
    }

    #[tokio::test]
    async fn drafts_are_scoped_to_the_author_and_admin() {
        let path = test_database_path();
        let database = Database::new(path.clone());
        database.initialize().await.unwrap();
        for (username, role) in [
            ("writer-one", super::AccountRole::Write),
            ("writer-two", super::AccountRole::Write),
            ("site-admin", super::AccountRole::Admin),
            ("read-only", super::AccountRole::Read),
        ] {
            database
                .create_account(username.to_owned(), role, "test-hash".to_owned())
                .await
                .unwrap();
        }

        let first_id = database
            .create_draft(
                "writer-one".to_owned(),
                "First draft".to_owned(),
                String::new(),
                Vec::new(),
                vec![
                    NewBlock {
                        id: None,
                        header: Some("First block".to_owned()),
                        body: None,
                        storage_key: None,
                        content_type: None,
                        alt: None,
                        children: vec![NewBlock {
                            id: None,
                            header: None,
                            body: Some("Nested caption".to_owned()),
                            storage_key: Some("media/draft-image".to_owned()),
                            content_type: Some("image/jpeg".to_owned()),
                            alt: Some("Draft image".to_owned()),
                            children: Vec::new(),
                        }],
                    },
                    NewBlock {
                        id: None,
                        header: None,
                        body: Some("Second block".to_owned()),
                        storage_key: None,
                        content_type: None,
                        alt: None,
                        children: Vec::new(),
                    },
                ],
            )
            .await
            .unwrap();
        let second_id = database
            .create_draft(
                "writer-two".to_owned(),
                "Other draft".to_owned(),
                String::new(),
                Vec::new(),
                Vec::new(),
            )
            .await
            .unwrap();

        let first = database
            .post_with_access(first_id, PostAccess::Author("writer-one".to_owned()))
            .await
            .unwrap()
            .unwrap();
        assert!(!first.published);
        assert_eq!(first.summary.published_at, None);
        assert_eq!(first.author_username, "writer-one");
        assert_eq!(first.summary.summary, "");
        assert_eq!(first.blocks.len(), 2);
        assert_eq!(first.blocks[0].position, 0);
        assert_eq!(first.blocks[0].children[0].body.as_deref(), Some("Nested caption"));
        assert_eq!(first.blocks[1].position, 1);
        let media_block_id = first.blocks[0].children[0].id;

        assert!(database
            .post_with_access(first_id, PostAccess::Author("writer-two".to_owned()))
            .await
            .unwrap()
            .is_none());
        assert!(database
            .post_with_access(first_id, PostAccess::Published)
            .await
            .unwrap()
            .is_none());
        assert!(database
            .post_with_access(first_id, PostAccess::Admin)
            .await
            .unwrap()
            .is_some());
        assert!(database
            .media_reference_with_access(
                first_id,
                media_block_id,
                PostAccess::Author("writer-one".to_owned()),
            )
            .await
            .unwrap()
            .is_some());
        assert!(database
            .media_reference_with_access(
                first_id,
                media_block_id,
                PostAccess::Author("writer-two".to_owned()),
            )
            .await
            .unwrap()
            .is_none());
        assert!(database
            .media_reference_with_access(first_id, media_block_id, PostAccess::Admin)
            .await
            .unwrap()
            .is_some());
        assert_eq!(
            database.drafts(Some("writer-one".to_owned())).await.unwrap()[0].id,
            first_id
        );
        assert_eq!(
            database.drafts(None).await.unwrap().iter().map(|post| post.id).collect::<Vec<_>>(),
            vec![second_id, first_id]
        );

        std::fs::remove_file(path).unwrap();
    }

    #[tokio::test]
    async fn share_link_management_is_author_scoped_or_admin_wide() {
        let path = test_database_path();
        let database = Database::new(path.clone());
        database.initialize().await.unwrap();
        for (username, role) in [
            ("writer-one", super::AccountRole::Write),
            ("writer-two", super::AccountRole::Write),
            ("site-admin", super::AccountRole::Admin),
        ] {
            database
                .create_account(username.to_owned(), role, "test-hash".to_owned())
                .await
                .unwrap();
        }
        let connection = Connection::open(&path).unwrap();
        connection
            .execute(
                "INSERT INTO posts (author_id, title, published_at, summary, published, tags) \
                 VALUES ((SELECT id FROM users WHERE username = 'writer-one'), 'Published', 1767225600, 'Summary', 1, '[]')",
                [],
            )
            .unwrap();
        drop(connection);
        let now = Duration::from_secs(1_800_000_000);

        let writer_link = database
            .create_share_link(
                "writer-link".to_owned(),
                1,
                "a".repeat(64),
                now,
                ShareAccess::Author("writer-one".to_owned()),
            )
            .await
            .unwrap();
        assert!(writer_link.is_some());
        assert!(database
            .create_share_link(
                "other-writer-link".to_owned(),
                1,
                "b".repeat(64),
                now,
                ShareAccess::Author("writer-two".to_owned()),
            )
            .await
            .unwrap()
            .is_none());
        assert!(!database
            .revoke_share_link(
                "writer-link".to_owned(),
                now,
                ShareAccess::Author("writer-two".to_owned()),
            )
            .await
            .unwrap());
        assert!(database
            .revoke_share_link(
                "writer-link".to_owned(),
                now,
                ShareAccess::Author("writer-one".to_owned()),
            )
            .await
            .unwrap());

        assert!(database
            .create_share_link(
                "admin-link".to_owned(),
                1,
                "c".repeat(64),
                now,
                ShareAccess::Admin,
            )
            .await
            .unwrap()
            .is_some());
        assert!(database
            .revoke_share_link("admin-link".to_owned(), now, ShareAccess::Admin)
            .await
            .unwrap());

        std::fs::remove_file(path).unwrap();
    }
}
