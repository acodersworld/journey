use std::{collections::HashMap, path::PathBuf, sync::Arc, time::Duration};

use rusqlite::{params, types::Type, Connection, OptionalExtension, TransactionBehavior};
use serde::{Deserialize, Serialize};

const POSTS_SCHEMA: &str = "\
CREATE TABLE IF NOT EXISTS posts (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    title TEXT NOT NULL CHECK (length(trim(title)) > 0),
    published_at TEXT NOT NULL,
    summary TEXT NOT NULL,
    published INTEGER NOT NULL DEFAULT 1 CHECK (published IN (0, 1)),
    tags TEXT NOT NULL DEFAULT '[]' CHECK (json_valid(tags) AND json_type(tags) = 'array')
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

const POST_BLOCKS_SCHEMA: &str = "\
CREATE TABLE IF NOT EXISTS post_blocks (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    post_id INTEGER NOT NULL REFERENCES posts(id) ON DELETE CASCADE,
    parent_id INTEGER,
    position INTEGER NOT NULL CHECK (position >= 0),
    header TEXT,
    body TEXT,
    storage_key TEXT,
    content_type TEXT,
    alt_text TEXT,
    UNIQUE (id, post_id),
    FOREIGN KEY (parent_id, post_id) REFERENCES post_blocks(id, post_id) ON DELETE CASCADE,
    CHECK (parent_id IS NULL OR parent_id != id),
    CHECK ((storage_key IS NULL) = (content_type IS NULL))
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
    role TEXT NOT NULL CHECK (role IN ('owner', 'reader')),
    password_hash TEXT NOT NULL,
    enabled INTEGER NOT NULL DEFAULT 1 CHECK (enabled IN (0, 1)),
    created_at INTEGER NOT NULL
);
CREATE UNIQUE INDEX IF NOT EXISTS users_single_owner ON users(role) WHERE role = 'owner';
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
    Owner,
    Reader,
}

impl AccountRole {
    pub fn parse(value: &str) -> Result<Self, String> {
        match value {
            "owner" => Ok(Self::Owner),
            "reader" => Ok(Self::Reader),
            _ => Err("account role must be owner or reader".to_owned()),
        }
    }

    fn as_str(self) -> &'static str {
        match self {
            Self::Owner => "owner",
            Self::Reader => "reader",
        }
    }
}

impl Default for AccountRole {
    fn default() -> Self {
        Self::Reader
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

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum PostAccess {
    Published,
    Owner,
}

impl PostAccess {
    fn includes_drafts(self) -> bool {
        matches!(self, Self::Owner)
    }
}

#[derive(Clone, Debug, Serialize)]
pub struct PostSummary {
    pub id: i64,
    pub title: String,
    pub published_at: String,
    pub summary: String,
}

#[derive(Clone, Debug, Serialize)]
pub struct FeedPage {
    pub posts: Vec<PostSummary>,
    pub next_cursor: Option<String>,
}

#[derive(Clone, Debug, Default)]
pub struct SidebarData {
    pub recent_posts: Vec<PostSummary>,
    pub archive_months: Vec<String>,
    pub tags: Vec<String>,
}

#[derive(Clone, Debug)]
pub struct FeedCursor {
    pub published_at: String,
    pub id: i64,
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
    pub alt: Option<String>,
    pub children: Vec<PostBlock>,
}

#[derive(Clone, Debug, Serialize)]
pub struct Post {
    #[serde(flatten)]
    pub summary: PostSummary,
    pub tags: Vec<String>,
    pub blocks: Vec<PostBlock>,
}

#[derive(Clone, Debug)]
pub struct NewPost {
    pub title: String,
    pub published_at: String,
    pub summary: String,
    pub tags: Vec<String>,
    pub blocks: Vec<NewBlock>,
}

#[derive(Clone, Debug)]
pub struct NewBlock {
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
pub struct ShareLinkCreated {
    pub id: String,
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
        self.run(initialize_schema).await
    }

    #[cfg(test)]
    pub async fn replace_posts(&self, posts: Vec<NewPost>) -> Result<(), String> {
        self.replace_posts_and_add_accounts(posts, Vec::new())
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
            transaction.execute("DELETE FROM posts", [])?;
            {
                let mut insert_post = transaction.prepare(
                    "INSERT INTO posts (title, published_at, summary, published, tags) VALUES (?1, ?2, ?3, 1, ?4)",
                )?;
                let mut insert_block = transaction.prepare(
                    "INSERT INTO post_blocks (post_id, parent_id, position, header, body, storage_key, content_type, alt_text) \
                     VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
                )?;
                for (post, tags) in posts {
                    insert_post.execute(params![post.title, post.published_at, post.summary, tags])?;
                    let post_id = transaction.last_insert_rowid();
                    insert_blocks(&transaction, &mut insert_block, post_id, None, post.blocks)?;
                }
            }
            transaction.commit()
                .map(|_| added_accounts)
        })
        .await
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

    pub async fn set_reader_enabled(&self, username: String, enabled: bool) -> Result<(), String> {
        self.run(move |connection| {
            let transaction = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
            let changed = transaction.execute(
                "UPDATE users SET enabled = ?2 WHERE username = ?1 COLLATE NOCASE AND role = 'reader'",
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
    ) -> Result<Option<ShareLinkCreated>, String> {
        self.run(move |connection| {
            let transaction = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
            let created_at_seconds = unix_seconds(created_at);
            let published = transaction.query_row(
                "SELECT EXISTS(SELECT 1 FROM posts WHERE id = ?1 AND published = 1)",
                [post_id],
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
            Ok(Some(ShareLinkCreated { id }))
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

    pub async fn revoke_share_link(&self, id: String, revoked_at: Duration) -> Result<bool, String> {
        self.run(move |connection| {
            let changed = connection.execute(
                "UPDATE share_links SET revoked_at = COALESCE(revoked_at, ?2) WHERE id = ?1",
                params![id, unix_seconds(revoked_at)],
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
                    "SELECT b.storage_key, b.content_type FROM share_sessions AS s \
                     JOIN share_links AS l ON l.id = s.share_link_id \
                     JOIN posts AS p ON p.id = l.post_id \
                     JOIN post_blocks AS b ON b.post_id = p.id \
                     WHERE s.token_digest = ?1 AND s.share_link_id = ?2 \
                       AND s.expires_at > ?5 AND l.expires_at > ?5 \
                       AND l.revoked_at IS NULL AND p.published = 1 \
                       AND p.id = ?3 AND b.id = ?4 AND b.storage_key IS NOT NULL \
                       AND (b.content_type LIKE 'image/%' OR b.content_type LIKE 'video/%')",
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
        self.run(move |connection| {
            let after_date = after.as_ref().map(|cursor| cursor.published_at.as_str());
            let after_id = after.as_ref().map(|cursor| cursor.id);
            let mut statement = connection.prepare(
                "SELECT id, title, published_at, summary FROM posts \
                 WHERE published = 1 \
                   AND (?1 IS NULL OR EXISTS (\
                       SELECT 1 FROM json_each(posts.tags) AS post_tag \
                       WHERE post_tag.type = 'text' AND post_tag.value = ?1 \
                   )) \
                   AND (?2 IS NULL OR published_at < ?2 OR (published_at = ?2 AND id < ?3)) \
                 ORDER BY published_at DESC, id DESC LIMIT ?4",
            )?;
            let rows = statement.query_map(
                params![tag, after_date, after_id, (limit + 1) as i64],
                post_summary_from_row,
            )?;
            let mut posts = rows.collect::<rusqlite::Result<Vec<_>>>()?;
            let has_more = posts.len() > limit;
            posts.truncate(limit);
            let next_cursor = if has_more {
                posts.last().map(|post| format!("{}:{}", post.published_at, post.id))
            } else {
                None
            };
            Ok(FeedPage { posts, next_cursor })
        })
        .await
    }

    pub async fn sidebar_data(&self) -> Result<SidebarData, String> {
        self.run(|connection| {
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
                    "SELECT DISTINCT substr(published_at, 1, 7) FROM posts \
                     WHERE published = 1 ORDER BY substr(published_at, 1, 7) DESC",
                )?;
                let rows = statement.query_map([], |row| row.get(0))?;
                rows.collect::<rusqlite::Result<Vec<_>>>()?
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
            Ok(SidebarData { recent_posts, archive_months, tags })
        })
        .await
    }

    pub async fn posts_for_month(&self, month: String) -> Result<Vec<PostSummary>, String> {
        self.run(move |connection| {
            let mut statement = connection.prepare(
                "SELECT id, title, published_at, summary FROM posts \
                 WHERE published = 1 AND substr(published_at, 1, 7) = ?1 \
                 ORDER BY published_at DESC, id DESC",
            )?;
            let rows = statement.query_map([month], post_summary_from_row)?;
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
            connection
                .query_row(
                    "SELECT b.storage_key, b.content_type \
                     FROM post_blocks AS b JOIN posts AS p ON p.id = b.post_id \
                     WHERE p.id = ?1 AND (?3 = 1 OR p.published = 1) AND b.id = ?2 \
                       AND b.storage_key IS NOT NULL \
                       AND (b.content_type LIKE 'image/%' OR b.content_type LIKE 'video/%')",
                    params![post_id, block_id, access.includes_drafts()],
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
    if !table_has_column(&transaction, "posts", "tags")? {
        transaction.execute(
            "ALTER TABLE posts ADD COLUMN tags TEXT NOT NULL DEFAULT '[]' \
             CHECK (json_valid(tags) AND json_type(tags) = 'array')",
            [],
        )?;
    }

    if table_exists(&transaction, "post_blocks")? {
        if table_has_column(&transaction, "post_blocks", "kind")? {
            transaction.execute("ALTER TABLE post_blocks RENAME TO post_blocks_legacy", [])?;
            transaction.execute_batch(POST_BLOCKS_SCHEMA)?;
            transaction.execute_batch(
                "INSERT INTO post_blocks (post_id, parent_id, position, header, body, storage_key, content_type, alt_text) \
                 SELECT post_id, NULL, position, \
                     CASE WHEN kind = 'heading' THEN text END, \
                     CASE WHEN kind = 'paragraph' THEN text WHEN kind IN ('image', 'video') THEN caption END, \
                     storage_key, content_type, alt_text \
                 FROM post_blocks_legacy ORDER BY post_id, position; \
                 DROP TABLE post_blocks_legacy;",
            )?;
        } else {
            transaction.execute_batch(POST_BLOCKS_SCHEMA)?;
        }
    } else {
        transaction.execute_batch(POST_BLOCKS_SCHEMA)?;
    }
    transaction.execute_batch(AUTH_SCHEMA)?;
    transaction.execute_batch(SHARE_SCHEMA)?;
    transaction.pragma_update(None, "user_version", 4)?;
    transaction.commit()
}

fn load_post(
    connection: &Connection,
    id: i64,
    access: PostAccess,
) -> rusqlite::Result<Option<Post>> {
    let post_row = connection
        .query_row(
            "SELECT id, title, published_at, summary, tags FROM posts WHERE id = ?1 AND (?2 = 1 OR published = 1)",
            params![id, access.includes_drafts()],
            |row| {
                let tags_json: String = row.get(4)?;
                let tags = serde_json::from_str(&tags_json).map_err(|error| {
                    rusqlite::Error::FromSqlConversionFailure(
                        4,
                        Type::Text,
                        Box::new(error),
                    )
                })?;
                Ok((post_summary_from_row(row)?, tags))
            },
        )
        .optional()?;
    let Some((summary, tags)) = post_row else {
        return Ok(None);
    };

    let (mut blocks, mut children) = {
        let mut statement = connection.prepare(
            "SELECT id, parent_id, position, header, body, content_type, alt_text \
             FROM post_blocks WHERE post_id = ?1 \
             ORDER BY parent_id IS NOT NULL, parent_id, position",
        )?;
        let rows = statement.query_map([id], |row| {
            let parent_id: Option<i64> = row.get(1)?;
            let block = PostBlock {
                id: row.get(0)?,
                position: row.get(2)?,
                header: row.get(3)?,
                body: row.get(4)?,
                content_type: row.get(5)?,
                alt: row.get(6)?,
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
    Ok(Some(Post { summary, tags, blocks }))
}

fn account_role_from_db(value: &str) -> rusqlite::Result<AccountRole> {
    match value {
        "owner" => Ok(AccountRole::Owner),
        "reader" => Ok(AccountRole::Reader),
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

fn table_has_column(
    connection: &Connection,
    table: &str,
    column: &str,
) -> rusqlite::Result<bool> {
    let mut statement = connection.prepare(&format!("PRAGMA table_info({table})"))?;
    let columns = statement.query_map([], |row| row.get::<_, String>(1))?;
    for existing in columns {
        if existing? == column {
            return Ok(true);
        }
    }
    Ok(false)
}

fn insert_blocks(
    connection: &Connection,
    statement: &mut rusqlite::Statement<'_>,
    post_id: i64,
    parent_id: Option<i64>,
    blocks: Vec<NewBlock>,
) -> rusqlite::Result<()> {
    for (position, block) in blocks.into_iter().enumerate() {
        let NewBlock { header, body, storage_key, content_type, alt, children } = block;
        statement.execute(params![
            post_id,
            parent_id,
            position as i64,
            header,
            body,
            storage_key,
            content_type,
            alt,
        ])?;
        let block_id = connection.last_insert_rowid();
        insert_blocks(connection, statement, post_id, Some(block_id), children)?;
    }
    Ok(())
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
        time::{SystemTime, UNIX_EPOCH},
    };

    use rusqlite::Connection;

    use super::{Database, NewBlock, NewPost};

    fn test_database_path() -> PathBuf {
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        std::env::temp_dir().join(format!("journey-site-db-{}-{nonce}.sqlite3", std::process::id()))
    }

    fn post(published_at: &str, title: &str) -> NewPost {
        NewPost {
            title: title.to_owned(),
            published_at: published_at.to_owned(),
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
        assert_eq!(first.next_cursor.as_deref(), Some("2026-03-31:11"));

        let second = database
            .feed(
                10,
                Some(super::FeedCursor {
                    published_at: "2026-03-31".to_owned(),
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
    async fn migration_maps_flat_blocks_and_adds_empty_tags() {
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
        database.initialize().await.unwrap();
        let migrated = database.post(7).await.unwrap().unwrap();
        assert!(migrated.tags.is_empty());
        assert_eq!(migrated.blocks.len(), 4);
        assert_eq!(migrated.blocks[0].header.as_deref(), Some("A heading"));
        assert_eq!(migrated.blocks[1].body.as_deref(), Some("A paragraph"));
        assert_eq!(migrated.blocks[2].body.as_deref(), Some("Image caption"));
        assert_eq!(migrated.blocks[2].content_type.as_deref(), Some("image/jpeg"));
        assert_eq!(migrated.blocks[3].body.as_deref(), Some("Video caption"));
        assert!(migrated.blocks.iter().all(|block| block.children.is_empty()));

        std::fs::remove_file(path).unwrap();
    }

    #[tokio::test]
    async fn post_reads_tags_sibling_order_and_mixed_group_content() {
        let path = test_database_path();
        let database = Database::new(path.clone());
        let group = NewBlock {
            header: Some("Morning".to_owned()),
            body: Some("A quiet start".to_owned()),
            storage_key: Some("media/group-image".to_owned()),
            content_type: Some("image/jpeg".to_owned()),
            alt: Some("A lake at sunrise".to_owned()),
            children: vec![
                NewBlock {
                    header: None,
                    body: Some("The water was still.".to_owned()),
                    storage_key: None,
                    content_type: None,
                    alt: None,
                    children: Vec::new(),
                },
                NewBlock {
                    header: None,
                    body: Some("Birdsong by the dock.".to_owned()),
                    storage_key: Some("media/child-video".to_owned()),
                    content_type: Some("video/mp4".to_owned()),
                    alt: None,
                    children: Vec::new(),
                },
            ],
        };
        let mut new_post = post("2026-01-01", "Tagged post");
        new_post.tags = vec!["Alps".to_owned(), "Morning walk".to_owned()];
        new_post.blocks = vec![
            NewBlock {
                header: None,
                body: Some("Before the group".to_owned()),
                storage_key: None,
                content_type: None,
                alt: None,
                children: Vec::new(),
            },
            group,
            NewBlock {
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
        assert_eq!(post.blocks[0].content_type.as_deref(), Some("image/jpeg"));
        assert_eq!(post.blocks[0].children.len(), 2);
        assert_eq!(post.blocks[0].children[0].position, 0);
        assert_eq!(post.blocks[0].children[1].position, 1);
        assert_eq!(post.blocks[0].children[1].content_type.as_deref(), Some("video/mp4"));
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
        let connection = Connection::open(&path).unwrap();
        connection.execute_batch("PRAGMA foreign_keys = ON;").unwrap();
        connection.execute(
            "INSERT INTO posts (title, published_at, summary) VALUES ('Post', '2026-01-01', 'Summary')",
            [],
        ).unwrap();
        connection.execute(
            "INSERT INTO post_blocks (post_id, position) VALUES (1, 0)",
            [],
        ).unwrap();
        connection.execute(
            "INSERT INTO post_blocks (post_id, parent_id, position) VALUES (1, 1, 0)",
            [],
        ).unwrap();
        let error = connection.execute(
            "INSERT INTO post_blocks (post_id, parent_id, position) VALUES (1, 2, 0)",
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
            header: None,
            body: None,
            storage_key: Some("media/image-key".to_owned()),
            content_type: Some("image/jpeg".to_owned()),
            alt: None,
            children: Vec::new(),
        });
        database.replace_posts(vec![media_post]).await.unwrap();
        let published = database.post(1).await.unwrap().unwrap();
        let block_id = published.blocks[0].id;
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
}
