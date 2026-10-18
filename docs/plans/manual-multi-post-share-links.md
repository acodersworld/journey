# Manual multi-post sharing through existing share links

## Summary

Extend the existing sharing mechanism to support a fixed selection of
published posts.

Authors select their own posts from All posts; admins may select any published
posts. Recipients see the selected posts as a feed using existing mosaics and
the media scroller.

Reuse existing share URLs, creation/revocation endpoints, guest sessions,
cookies, expiry settings, and CLI management.

## Selection and review UI

Add **Share posts** for write/admin accounts on All posts.

- Enter selection mode with checkboxes beside eligible posts.
- Preserve selection while additional posts load.
- Keep media interactions and existing single-post Share controls working.
- Show a sticky selected-count bar with Cancel and Review.
- Disable Review for an empty selection.
- Cancel clears selection and exits selection mode.
- Keep selection in page memory; navigation or reload clears it.
- Allow 1–100 distinct posts.

Review opens a dialog listing selected titles and local publication times,
with Remove and Back to selection controls.

For multiple posts, allow an optional title, defaulting to **Shared posts**.
Always order posts by publication time descending, then post ID descending.

Reuse existing sharing conventions:

- **Copy link** creates and copies on its first click.
- Repeated copies reuse the current unexpired link while the panel remains
  open.
- Retain a created link if clipboard copying fails.
- Show “Link copied!” beside the button for three seconds in reserved space.
- Show expiry and reserve space below for **Revoke link**.
- Prevent duplicate submissions while requests are pending.

Membership and title are fixed after creation. Editing the selection or title
starts a new link on the next copy; previously created links remain valid.
Closing the panel discards its reusable link state.

A selection containing one post uses existing single-post sharing behavior.

## Existing API and share routes

Extend:

```http
POST /api/posts/21/share-links
Content-Type: application/json

{
  "additional_post_ids": [18, 12],
  "title": "Family updates"
}
```

The path identifies the first selected post; the array contains the remaining
posts.

- Preserve existing requests with no body.
- An omitted or empty additional-post array creates an ordinary single-post
  link.
- Validate distinct positive IDs and a maximum total of 100.
- Recheck publication state and sharing permission for every target inside
  one transaction.
- Reject the entire request if any target is inaccessible or unpublished.
- Require existing account authentication and same-origin checks.
- Preserve the existing response fields and status; add `post_count`.
- Use the existing validated URL-origin behavior and global share-link
  lifetime.

Continue using:

```text
/share/{link_id}/{secret}
DELETE /api/share-links/{link_id}
```

Opening a single-post link retains its existing redirect and page. Opening a
multi-post link redirects to `/share/{link_id}`, which serves its feed.

Reuse existing `share_sessions` and link-scoped cookies. Every guest page,
feed, fragment, media, thumbnail, HEAD, and range request must check the
session, link expiry/revocation, membership, and publication state.

Add guest feed-pagination and fragment routes beneath `/share/{link_id}`.
Configure the existing feed loader to use those routes instead of account
endpoints. Guest access failures must not redirect to account login.

Keep existing single-post CLI commands working. Extend listing and revocation
to handle multi-post links, listing their member post IDs without exposing
secrets. Authors retain single-post management rights for their posts;
multi-post creators with write permission may revoke their own links, and
admins may revoke any link.

## Share-link schema and migration boundary

Introduce ordered membership through `share_link_posts`. Generalize
`share_links` to hold link metadata, single/multi-post kind, optional title,
and creator information. Keep `share_sessions` as the common guest-session
mechanism.

Multi-post links must not depend on an anchor post: deleting one selected post
must not delete the entire link.

**Explicitly authorized loss:** this feature's migration may recreate the
existing `share_links` and `share_sessions` tables, discarding existing links
and their guest sessions. All pre-upgrade share URLs may stop working. This is
a specific exception for this migration, not permission to discard share data
in later migrations.

**Nothing else may be lost or reset.** Preserve:

- Posts, blocks, media assets, ordering, references, and stored files.
- Users, passwords, roles, and normal account sessions.
- Comments and any other application data.
- All site settings, including the configured share-link lifetime.

Requirements:

- Add the next schema version with an explicit upgrade from the deployed
  version, currently 10.
- Limit destructive operations to the identified share-link/session tables.
- Run the migration atomically, dropping dependent share sessions before
  links.
- Roll back all changes and the version update on failure.
- Make initialization repeatable; reopening an upgraded database must not
  erase newly created links.
- Fail unsupported upgrades without modifying data or recommending
  destructive database recreation.

Update backup restoration/verification so supported pre-upgrade backups can
be upgraded through the same migration on their restored working files. The
same share-link-loss exception applies; other restored data must remain
intact.

## Recipient feed, verification, and documentation

### Recipient behavior

- Freeze membership and ordering at creation.
- Show current contents, author bylines, and local publication times.
- Later publications never join the link.
- Skip unpublished or deleted members without reordering remaining posts.
- Preserve multi-post presentation even if only one member remains visible.
- Show an empty state when no members remain accessible.
- Reuse mosaic settings, image URLs, caches, media scroller, and video
  handling.
- Show no account sidebar or management controls.
- Permit multiple independent share-link sessions in one browser.
- Apply existing private/no-store policies and redact secrets from logs.

### Verification

Test selection/review/copy behavior, permissions, limits, fixed ordering,
expiry, revocation, concurrent clicks, and newly loaded feed entries.

Verify cross-link, unrelated-post, draft, fragment, thumbnail, and media access
denial; pagination with removed members; and unchanged new single-post sharing
behavior.

Seed a populated deployed database and verify that migration loses only
authorized share-link/session data. Check all unrelated rows, settings,
normal sessions, and foreign-key relationships. Test rollback, repeated
startup, and restoration of pre-upgrade backups.

### Documentation

Keep implementation in focused database, route, and UI modules.

Update the [time-period collection plan](time-period-collection-share-links.md)
to reuse this membership model, `/share` routes, sessions, and mosaic guest
feed. Remove its separate collection-link/session-table design and
compact-list requirement.

Automation credentials, time-bound selection, scheduling, email delivery,
editable collections, and specialized multi-post WhatsApp previews remain
separate work.
