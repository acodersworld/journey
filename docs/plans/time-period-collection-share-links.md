# Time-period collections with share links

## Summary

Allow an external script to submit a title and time bounds, receive a share
link, and email it. Recipients see a compact paginated list of posts and open
individual posts in the shared full-post viewer overlay.

The website handles collection creation and guest access. Scheduling and email
delivery remain in the external script.

## Automation API and credentials

- Add `POST /api/automation/collection-share-links`, accepting JSON:

  ```json
  {
    "title": "Posts for the week",
    "from": "2026-10-05T00:00:00Z",
    "until": "2026-10-12T00:00:00Z"
  }
  ```

- Require a nonempty title and valid RFC3339 bounds with explicit offsets, with
  `from < until`. Select published posts from **all authors**, using publication
  time: `from` inclusive, `until` exclusive.
- Authenticate through dedicated automation username/password credentials
  using [HTTP Basic authentication](https://www.rfc-editor.org/rfc/rfc7617) over
  HTTPS. These credentials authorize collection creation only and cannot log
  into the website.
- Add CLI commands to create, list, change passwords, enable, and disable
  automation accounts. Reuse existing username validation, Argon2 password
  hashing, and login throttling patterns.
- Return JSON with `id`, absolute `url`, `post_count`, and `expires_at_unix`.
  Build URLs from configured `site.public_origin`.
- For an empty selection, return HTTP 200 with `post_count: 0` and the other
  fields `null`; create no collection or link. Return HTTP 201 for a created
  collection and structured JSON errors for unsuccessful requests.
- Each successful request creates an independent link. Automatic retry
  deduplication is outside this first version.

## Persistence and guest authorization

- Add separate tables for automation accounts, collection share links,
  collection membership, and collection guest sessions. Preserve existing
  single-post share tables and URLs.
- A collection link stores its title, time bounds, creator, secret digest,
  creation/expiry/revocation times, and an ordered list of selected post IDs.
- Save membership and create the link in one transaction. Freeze order at
  creation using publication time descending, then post ID descending.
- Show current post contents. Later backdated publications do not join the
  collection; unpublished or deleted posts become inaccessible.
- Use the existing global admin-controlled share-link lifetime, measured from
  creation. Reuse existing random ID/secret generation and digest storage.
- Opening `/collection-share/{id}/{secret}` issues a link-scoped guest cookie
  and redirects to `/collection-share/{id}`.
- Revalidate session, expiry, revocation, membership, and publication state on
  every collection, post, and media request. Collection guests gain no
  access to other posts or drafts.
- Add CLI listing and revocation for collection links. Revocation invalidates
  access through existing guest sessions.
- Introduce a versioned, additive migration from the deployed schema. Preserve
  existing accounts, posts, media references, links, and sessions; do not
  recreate the database.

## Collection list and viewer integration

- Use the existing dark theme and a compact list with one entry per selected
  post. Show the supplied collection title, post titles, summaries, authors,
  and publication times converted to the visitor's local time.
- Load entries in pages of 24, with on-demand loading and a Load more fallback.
  No mosaic or cover-image generation is required.
- Reuse the viewer behavior defined in
  [Full-post viewer overlay](full-post-viewer-overlay.md), including its
  same-origin iframe, dismissal, focus restoration, retained underlying scroll
  position, loading/error states, nested galleries, and media cleanup. Do not
  implement a second overlay.
- Add collection-authorized inline post URLs and wire collection entries to
  that shared viewer. These routes must use the collection guest session and
  revalidate expiry, revocation, membership, and publication state; the
  account-authenticated viewer route does not authorize collection guests.
- Provide those inline post pages as real link fallbacks for modified clicks
  and JavaScript-disabled navigation. Render full posts without mosaics,
  retaining captions and collection-authorized media URLs.
- Apply existing guest caching and logging policies, adding safe route
  templates for the new endpoints. Never log credentials or share secrets.

## Verification and defaults

- Test boundaries, timezone offsets, multiple authors, empty selections,
  frozen membership/order, and visible later edits.
- Test invalid/disabled credentials, throttling, expiry, revocation,
  cross-collection access, unrelated media, and unpublished posts.
- Verify migration preserves populated deployed data and existing single-post
  sharing.
- Check desktop and mobile collection lists, pagination, collection-authorized
  inline post pages, real link fallbacks, and integration with the shared
  viewer. Verify dismissal, focus restoration, retained scroll position,
  nested galleries, and video cleanup in the collection guest context.
- Document provisioning and an example API request/response. Explicit post
  selections, collection editing, and specialized email/WhatsApp previews are
  deferred.
