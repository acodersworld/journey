# Threaded comments backend

**Status:** Implemented backend
**Updated:** 9 October 2026

## Data and behavior

The site stores plain-text comments in SQLite. Each row belongs to one published
post and optionally targets a root block or an image/video block. Replies point
to a live parent comment and inherit its post and block target. The database
checks post, block, and parent relationships in the same transaction as each
write. Comment identity, author, target, parent, and creation time stay fixed.

Deleting a comment clears its body and records a deletion timestamp. Its row
remains as a tombstone so replies keep their parent and target. Tombstones are
listed without body text or public author identity, and cannot be edited or
receive new direct replies. Direct reply counts include tombstones.

Bodies contain 1 to 10,000 Unicode characters and cannot be whitespace-only.
Creation and edit times come from the site server and serialize as UTC ISO 8601
timestamps. Root comments and direct replies are ordered by ascending comment
ID, with cursor pages defaulting to 20 rows and capped at 100.

## API and access

Authenticated routes expose top-level post or block comments, direct replies,
creation, editing, and removal:

- `GET` and `POST /api/posts/{post_id}/comments`
- `GET /api/posts/{post_id}/comments/{comment_id}/replies`
- `PATCH` and `DELETE /api/posts/{post_id}/comments/{comment_id}`

Top-level listing accepts optional `block_id`, `limit`, and `after` query
parameters. Reply listing accepts `limit` and `after`. A reply request may omit
`block_id`; the server derives it from its parent and rejects a conflicting
target. Responses include comment and target IDs, parent ID, UTC timestamps,
deletion state, and direct reply count. `next_cursor` is an ascending-ID cursor
or `null`.

`write` and `admin` sessions may create comments. Authors may edit or remove
their own comments. Admins may remove another author's comment but may not edit
that author's text. The write transaction rechecks the current session, role,
post publication, target, parent state, and ownership. Mutations also use the
site's same-origin check.

Authenticated readers can list comments on published posts. Share-link guests
have equivalent read-only routes beneath
`/share/{link_id}/posts/{post_id}/comments`; each request rechecks the guest
session, link expiry and revocation, and post publication. Comment responses
are private and uncached. Drafts have no comments.

## Schema upgrade

Schema version 11 adds the comments table, target and parent indexes, and
integrity triggers. Startup upgrades deployed version 10 databases in one
`BEGIN IMMEDIATE` transaction, reading `user_version` after acquiring the
write lock. The comments DDL and version update commit together. Fresh version
0 databases receive the full current schema. Unsupported or malformed schema
versions remain unchanged; a failed migration rolls back and reports an
initialization error.
