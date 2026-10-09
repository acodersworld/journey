# Threaded comments backend

## Summary

Add plain-text comments on published posts, root blocks, and individual
image/video blocks. Support replies to any live comment, with each reply
inheriting its parent's target.

Writers and admins can comment. All viewers authorized to read the post,
including share-link guests, can read its comments. The UI will have a separate
plan.

## Storage and migration

- Add a `comments` table containing ID, post ID, optional block ID, optional
  parent-comment ID, author user ID, body, creation time, optional edit time,
  and optional deletion time.
- Use foreign keys and transaction checks to ensure blocks and parents belong
  to the same post and replies retain the same target. Author, target, parent,
  and creation time are immutable.
- Removing a comment clears its body and leaves a tombstone connecting
  existing replies. Do not remove descendants or accept new replies directly
  to a deleted comment.
- Add indexes for target and parent lookups.
- Provide an atomic, additive migration from the deployed schema version 10
  to version 11. Preserve existing data and IDs. Fresh databases receive the
  complete schema; repeated initialization is safe. Reject unsupported
  versions without modifying them or instructing users to recreate live data.
  Run the migration during database initialization before serving requests;
  read the version after acquiring the transaction's write lock, create the
  table and indexes, update `PRAGMA user_version`, and commit together. Roll
  back the transaction and report a clear startup error on failure.

## API and permissions

- `GET /api/posts/{post_id}/comments` lists top-level comments. Optional
  `block_id` selects a block; omission selects the post itself.
- `GET /api/posts/{post_id}/comments/{comment_id}/replies` lists direct replies.
- `POST /api/posts/{post_id}/comments` accepts `body`, optional `block_id`, and
  optional `parent_comment_id`. Replies inherit their target; conflicting
  block IDs are rejected.
- `PATCH /api/posts/{post_id}/comments/{comment_id}` edits the body; `DELETE`
  removes the comment.
- Commenters may edit or remove their own live comments. Admins may remove
  anyone's comments, but cannot edit another user's text. Mutations require a
  current writer/admin session and the existing same-origin checks.
- Add equivalent read-only listing routes beneath
  `/share/{link_id}/posts/{post_id}`. Recheck the guest session, link expiry,
  revocation, and post publication on every request.
- Validate targets and permissions inside database operations. Comments are
  unavailable on drafts.

## Listing and validation

- List roots and direct replies oldest first using ascending comment IDs. Use
  cursor pagination, defaulting to 20 items with a maximum of 100.
- Return IDs, target, parent, author username, UTC timestamps, deletion state,
  and direct reply count. Tombstones return no body or public author identity.
- Accept 1–10,000 Unicode characters of plain text; reject whitespace-only
  bodies. Preserve line breaks. Generate timestamps on the server.
- Keep responses private and uncached. Return clear validation, authorization,
  missing-target, and deleted-comment errors using the site's existing status
  conventions.

## Verification

- Test post, root-block, and media comments; replies to replies; inherited
  targets; ordering and pagination.
- Test writer/admin creation, owner editing/removal, admin removal, and
  rejection of read-account or guest mutations.
- Verify tombstones preserve replies and deleted comments cannot be edited or
  receive new direct replies.
- Test shared reads after expiry, revocation, and unpublishing.
- Verify the version-10 migration preserves posts, media, accounts, sessions,
  and share links, passes foreign-key checks, and rolls back on failure.

## Assumptions

- No comment attachments, Markdown, reactions, notifications, or frontend
  changes in this patch.
- Thread nesting has no imposed depth limit; reads fetch one level at a time.
- Editing changes the body and edit timestamp without retaining revision
  history.
- The migration supports a forward upgrade. The previous version-10 binary
  rejects version 11; downgrading the executable requires separate
  consideration.
