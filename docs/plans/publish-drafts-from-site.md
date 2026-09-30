# Publish drafts from the site

## Goal

Let a post's author or an admin publish an existing draft from its detail page.
Publishing is immediate. The default publication instant is the server's current
time; the publisher may instead choose a past date and time. Publication time is
stored as UTC Unix seconds and shown to visitors in their browser's local time.

## Backend and data

- Change `posts.published_at` from nullable date text to nullable `INTEGER` UTC
  Unix seconds. Keep `NULL` for drafts and retain `published` for the existing
  published/draft queries. Update Rust post summaries, full posts, feed cursors,
  and API JSON to use integer seconds. Cursor ordering remains
  `(published_at DESC, id DESC)`; the cursor carries both integer values.
- Add `POST /api/posts/{id}/publish` with JSON body
  `{ "published_at": <unix_seconds> }`; `published_at` may be omitted to use the
  server's current second. Apply the existing origin check and require a `write`
  author of that draft or an `admin`. A `read` account gets `403`; a nonexistent
  post or another writer's draft gets `404`; an already published post gets `409`.
- Validate that the draft has at least one text block, including a nested block,
  with a nonblank header or body. A title alone, empty blocks, and media alone
  cannot be published. Reject an explicitly supplied timestamp later than the
  server's current second with `400`. Perform validation and the state change in
  one database transaction so two publish requests cannot both succeed. On
  success return `204 No Content`.
- Use `jiff` to parse timestamps and calculate local calendar months. Imported
  manifests must specify an ISO 8601
  timestamp with an explicit offset or `Z`; convert it to UTC Unix seconds.
  Update the example manifest and importer tests. Date-only values are invalid.
- This site has no schema migration process. Detect an existing date-text
  `published_at` column at database startup and fail with an instruction to
  recreate the SQLite database and rerun the destructive importer. Do not
  convert old rows in place.

## Dates and archives

- Render publication `<time>` elements with a UTC ISO 8601 `datetime` value and
  readable UTC fallback text. The browser formats them in local time, including
  feed cards, post pages, recent-post links, and archive pages. Pages remain
  intelligible without JavaScript.
- Use a non-secret `journey_timezone` cookie containing the browser's IANA
  timezone. On first visit, or when that timezone changes, JavaScript writes the
  cookie and reloads once so server-rendered archives match the visitor's local
  month. Check that the cookie value can be read back before reloading, avoiding
  a loop when cookies are disabled. The server validates the IANA identifier and
  uses UTC if it is absent or invalid. The cookie is readable by JavaScript and
  scoped to `/` with `SameSite=Lax`.
- Derive sidebar archive months by converting published timestamps to the
  selected timezone. For `/archive/{YYYY-MM}`, calculate the UTC instants at
  the start of that local month and the next local month, then query the
  half-open interval. This keeps month membership correct across timezone and
  daylight-saving changes. Feed order stays ordered by absolute instant.

## Publishing UI

- Show Publish on a draft detail page only to its author or an admin. Clicking
  opens a confirmation dialog. An unchecked date/time override uses server
  time; checking it reveals a browser-local `datetime-local` input initialized
  to the current local minute. Convert an override to UTC Unix seconds in the
  browser. Reject an invalid input and show the chosen instant in the dialog
  before submission. The server remains authoritative for the future-time check.
- Disable duplicate submission while the request is pending, show the returned
  error in the dialog, and reload `/posts/{id}` after success. The published post
  then appears in feeds and archives, disappears from drafts, and offers the
  existing share control.

## Verification

- Publish as author and admin; reject a reader, another author, a nonexistent
  post, a title-only draft, a future override, and a second publish request.
- Verify the default uses server time and a past override preserves its instant;
  verify feed ordering and cursor pagination when timestamps are equal.
- Verify local archive membership near midnight and across daylight-saving
  transitions, UTC fallback for invalid or absent timezone cookies, and no
  reload loop when cookies are blocked.
- Verify importer acceptance of timestamps with explicit offsets, rejection of
  date-only values, and the old-schema rebuild message. Check the dialog's
  default, override, confirmation, error, and success states.
