# Time-period collections with share links

## Summary

Add a backend endpoint that accepts a title and time bounds, selects published
posts, and creates a link through the existing
[multi-post sharing implementation](../design/manual-multi-post-share-links.md).
Scheduling and email delivery remain in the external script.

Guest feeds, mosaics, media access, sessions, expiry, revocation, and CLI link
management are already implemented. This patch reuses them without frontend or
storage-service changes.

## Automation endpoint

Add `POST /api/automation/collection-share-links`:

```json
{
  "title": "Posts for the week",
  "from": "2026-10-05T00:00:00.000Z",
  "until": "2026-10-12T00:00:00.000Z"
}
```

- Require a nonblank title, using the existing 200-character share-title limit.
- Require UTC RFC3339 datetime strings ending in uppercase `Z` for both
  bounds, with seconds and optionally one to three fractional digits
  (millisecond precision). Accept whole-second values such as
  `2026-10-05T00:00:00Z` and the output of Google Apps Script's
  `Date.toISOString()`. Require `from < until`.
- Reject numeric Unix timestamps, suffixless/local datetimes, numeric timezone
  offsets (including `+00:00`), and fractional precision beyond milliseconds.
- Select published posts from all authors by publication time: `from`
  inclusive, `until` exclusive. Preserve boundary precision when comparing
  against stored whole-second timestamps.
- Return HTTP 201 with `id`, absolute `url`, `post_count`,
  `expires_at_unix`, and a boolean `truncated`. Build the existing
  `/share/{id}/{secret}` URL from configured `site.public_origin`.
- For no matching posts, return HTTP 200 with `post_count: 0`,
  `truncated: false`, and null `id`, `url`, and `expires_at_unix`; create no
  link.
- Retain the existing 100-post limit. If more posts match, create the link
  using the newest 100, ordered by publication time descending and then post
  ID descending. Return `post_count: 100` and `truncated: true`. Otherwise,
  return `truncated: false`. The flag is response metadata so the script can
  report omitted posts; it requires no request flag or persisted schema field.
- Use structured JSON errors, private no-store responses, and safe route
  logging. Never log credentials, authorization headers, or link secrets.
- Each successful request creates an independent link. Retry deduplication
  remains outside this version.

## Dedicated automation credentials

- Authenticate using dedicated username/password credentials through
  [HTTP Basic authentication](https://www.rfc-editor.org/rfc/rfc7617) over
  HTTPS. These credentials grant only this creation operation; they cannot
  log into the website or call account-management APIs.
- Add an `automation_accounts` table storing username, Argon2 password hash,
  enabled state, and creation time. Keep it separate from normal user accounts.
- Add CLI create, list, password-change, enable, and disable commands. Reuse
  username validation, password helpers, and login throttling patterns, with
  automation-specific throttle keys.
- Handle authentication on the automation route itself. Server-to-server
  requests do not require a browser session cookie or browser Origin header.
- Add a versioned, additive migration from the current schema, preserving all
  existing data, including share links and sessions. Retain the earlier
  migration chain. The share-data-loss exception applied only to the previous
  manual multi-post migration.

## Atomic selection and existing link creation

- Select matching posts and create the link and membership in one immediate
  SQLite transaction. Read at most 101 matches in publication-time-descending,
  post-ID-descending order. If a 101st match exists, set `truncated: true` and
  retain only the first 100 for link creation.
- Refactor the existing creation helper to accept the shared transaction as
  needed; keep manual sharing on the same implementation path.
- Freeze membership and order using publication time descending, then post ID
  descending. Reuse existing token generation, digests, and the global
  admin-controlled share-link lifetime.
- Store only the existing link metadata and selected membership. Do not add
  time-bound or automation-creator fields to share-link tables. Automation
  links may use the existing null creator; log the automation username for
  operational attribution, and manage these links through existing admin CLI
  listing and revocation.
- Reuse normal single-post behavior for one match and the mosaic guest feed
  for multiple matches. The supplied title applies to multi-post links, as in
  existing creation behavior.
- Existing guest authorization, current post contents, removal of inaccessible
  members, pagination, image caching, and media scroller behavior remain in
  use. Add no recipient UI or new guest routes.

## Verification and documentation

- Test whole-second and millisecond UTC strings, inclusive/exclusive bounds,
  invalid ranges, multiple authors, and exclusion of drafts. Verify fractional
  bounds do not get rounded down when selecting stored whole-second times.
- Reject numeric timestamps, missing `Z`, timezone offsets including
  `+00:00`, invalid dates, and fractional precision beyond milliseconds.
- Test zero, one, 100, and more than 100 matching posts. Verify empty selections
  create no link, `truncated` is false for up to 100 matches, and larger
  selections create a link containing exactly the newest 100 with
  `truncated: true`. Include publication-time ties to verify the post-ID
  tiebreaker and confirm omitted posts are not accessible through the link.
- Verify atomic selection and creation, frozen order, and unchanged manual
  sharing after refactoring.
- Test invalid/disabled automation credentials, throttling, and denial of
  normal website login or broader account privileges using those credentials.
- Verify generated links through the existing guest flow, including expiry,
  revocation, and denial of unrelated post/media access. Reuse existing UI
  regression coverage rather than rebuilding mosaic tests in this patch.
- Verify migration preserves populated current-schema data, links, sessions,
  settings, and the existing upgrade/backup-restore paths; test rollback.
- Document credential provisioning and an example authenticated request and
  response. Collection editing and specialized email/WhatsApp previews remain
  deferred.
