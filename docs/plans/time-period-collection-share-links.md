# Time-period collections with share links

## Summary

Allow an external script to submit a title and time bounds, receive a share
link, and email it. Recipients see the selected posts in the existing
multi-post guest feed, using mosaics and the media scroller.

The website handles collection creation and guest access. Scheduling and email
delivery remain in the external script.

Build on [Manual multi-post sharing](manual-multi-post-share-links.md). This
plan adds automation credentials and time-bound selection to that common
sharing mechanism; it does not introduce separate collection links, sessions,
or guest rendering.

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

- Add automation-account storage and extend common share-link metadata for
  automation creators and time bounds as needed. Reuse `share_links`,
  `share_link_posts`, and `share_sessions` from manual multi-post sharing.
- The common share link stores its title, time bounds, creator, secret digest,
  creation/expiry/revocation times, and an ordered list of selected post IDs.
- Save membership and create the link in one transaction. Freeze order at
  creation using publication time descending, then post ID descending.
- Show current post contents. Later backdated publications do not join the
  collection; unpublished or deleted posts become inaccessible.
- Use the existing global admin-controlled share-link lifetime, measured from
  creation. Reuse existing random ID/secret generation and digest storage.
- Return the existing `/share/{id}/{secret}` URL. Reuse the common link-scoped
  guest cookie and redirect to `/share/{id}` for a multi-post feed; a
  single-post result retains ordinary single-post link behavior.
- Revalidate session, expiry, revocation, membership, and publication state on
  every collection, post, and media request. Collection guests gain no
  access to other posts or drafts.
- Reuse common CLI link listing and revocation, including automation-created
  links. Revocation invalidates access through existing guest sessions.
- Introduce a versioned, additive migration from the deployed schema. Preserve
  existing accounts, posts, media references, links, and sessions; do not
  recreate the database. The permission to discard pre-upgrade share links in
  the manual multi-post migration is specific to that migration and does not
  apply to this later automation upgrade.

## Recipient feed

- Reuse the manual multi-post guest feed, including the supplied title, dark
  theme, post text, mosaics, author bylines, and local publication times.
- Reuse on-demand loading, the Load more fallback, frozen membership/order,
  empty states, and the existing media scroller. Images use existing media
  URLs and caches; no separate cover generation is required.
- Reuse `/share/{id}` feed, fragment, post, and media authorization. Account
  cookies do not authorize guest routes, and the link cannot expose posts
  outside its frozen selection.
- Add no full-post overlay or iframe viewer.
- Apply existing guest caching and logging policies, adding safe route
  templates for the new endpoints. Never log credentials or share secrets.

## Verification and defaults

- Test boundaries, timezone offsets, multiple authors, empty selections,
  frozen membership/order, and visible later edits.
- Test invalid/disabled credentials, throttling, expiry, revocation,
  cross-collection access, unrelated media, and unpublished posts.
- Verify migration preserves populated deployed data and existing single-post
  sharing.
- Verify automation-created links use the common single/multi-post guest
  behavior. Check pagination, mosaics, media scroller behavior, video cleanup,
  and inaccessible or removed members in the guest context.
- Document provisioning and an example API request/response. Explicit manual
  selections are covered by the manual multi-post plan. Collection editing
  and specialized email/WhatsApp previews remain deferred.
