# Independent share links for post blocks

## Summary

Add **Copy link** controls for root blocks and individual images/videos. Any
signed-in user viewing a published post, or guest viewing it through a valid
share session, can create a block link.

Each link is a separate row with its own secret and expiry, calculated from
creation using the existing global share-link lifetime. It remains independent
of the source link's expiry and revocation.

## Storage and authorization

- Add a nullable `block_id` to `share_links`: null retains whole-post access;
  a value grants access to that block. Validate that the block belongs to the
  link's post.
- A root-block link exposes its text and media children. An individual media
  link exposes that item and its caption, excluding its parent and siblings.
- Use the existing random IDs, secret digests, guest-session cookies, expiry
  checks, and revocation mechanism. Guest sessions cannot outlive their own
  block link.
- Allow guests to create further block links only within their current scope.
  A whole-post guest may share any block; a group guest may share that group or
  its children; an individual-media guest may share that item.
- Recheck source-session access, publication state, and target ownership inside
  the creation transaction.
- Post authors with write permission and admins may revoke block links. Other
  users and guests cannot revoke them. Include block targets in existing
  link-management listings.

## API and scoped rendering

- Add signed-in share-link creation at
  `POST /api/posts/{post_id}/blocks/{block_id}/share-links`.
- Add guest share-link creation at
  `POST /share/{source_link_id}/posts/{post_id}/blocks/{block_id}/share-links`,
  authenticated by the source link's session cookie.
- Require the existing same-origin validation for both endpoints. Return the
  established `{id, url, expires_at_unix}` response.
- Keep `/share/{link_id}/{secret}` as the opening URL. After validation, display
  the selected content through the existing guest flow.
- Show post title, author, and publication time as context, followed by only
  the permitted block content.
- Apply block scope to every guest media, thumbnail, download, comment, reply,
  and WhatsApp-preview lookup. Scoped links cannot retrieve whole-post comments
  or unrelated blocks by changing IDs.
- Derive WhatsApp preview text and images from the selected content, with the
  post title as a fallback.
- Preserve ordinary post-link behavior and existing author/admin restrictions
  on creating whole-post links.

## UI

- Add **Copy link** beside each published root block and media item, including
  gallery-panel items, feeds, post previews, and guest pages.
- Keep copy controls separate from gallery-opening buttons and video controls.
- The first click creates and copies a link. Subsequent clicks reuse it while
  it remains unexpired; after expiry, create a new link.
- Share per-target state across duplicate controls on the page, and prevent
  simultaneous clicks from creating duplicate rows.
- Show "Link copied!" beside the button for three seconds in reserved space.
  On failure, show a retryable error; retain a successfully created link if
  only copying failed.

## Migration and verification

- Add an atomic version-11-to-12 migration, retaining the existing version-10
  upgrade chain. Existing links receive null block targets; preserve their
  IDs, secrets, expiry, and sessions.
- Test link creation by signed-in users and guests, group and media scope,
  expiry from creation, and independence after source expiry or revocation.
- Verify inaccessible targets cannot be shared, and block-link guests cannot
  broaden access through media, comments, previews, or further sharing.
- Test author/admin revocation, migration preservation and rollback, and
  unchanged whole-post links.
- Verify copying, retry, expiry renewal, dynamically loaded posts, gallery
  controls, and mobile layout.

## Assumptions

- Links apply only to published content.
- This patch includes backend and UI.
- Block-link visitors receive no account or commenting privileges.
