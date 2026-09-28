# Scrollable website feed

**Status:** Implementation plan

## Goal

Build a basic public website from the existing `journey-site` post backend.
Show ten post previews initially, load ten more only when the reader scrolls
to the end of the loaded feed, and let readers expand a preview to see its
full post. Use server-rendered HTML and a small browser script; React is not
needed for this first UI.

## Backend and routes

- Add keyset pagination to `GET /api/posts` while retaining its maximum limit
  of 100. The default page size becomes 10. Return
  `{ "posts": [...], "next_cursor": string | null }`, ordered by
  `published_at DESC, id DESC`. Accept an optional `after` cursor containing
  the last post's publication date and ID; reject malformed cursors with
  `400`. Fetch one extra row to determine whether another page exists, so
  reaching the end does not require another request. The cursor is a position
  in the current ordering, not a snapshot across imports.
- Serve `GET /` as a semantic HTML page containing the first ten previews and
  the next cursor for browser enhancement. Each preview shows title,
  publication date, summary, an expand control, and a direct link to the
  post.
- Keep `GET /api/posts/{id}` as JSON. Change `GET /posts/{id}` to render a
  complete HTML post so direct links work without the feed script. Use the
  existing post-block media URL for images and videos. Escape post text when
  rendering server-side HTML.

## Browser behavior

- Observe a sentinel after the loaded previews. Request the next page only
  after the reader has scrolled downward and the sentinel enters view; do not
  immediately fetch extra pages when the initial ten previews fit on screen.
  Provide a visible **Load more** button as a fallback for short pages or
  observer failures. Prevent concurrent requests and stop when
  `next_cursor` is null.
- On first expansion, fetch only that post from `/api/posts/{id}` and render
  its blocks inline. Reuse loaded content on later expansions. Use DOM text
  APIs for user content, image lazy loading, and native video controls.
- Show loading, retryable error, and end-of-feed states. An expansion failure
  leaves its preview and direct post link usable.

## Verification

- Test feed ordering and keyset pagination across equal publication dates,
  malformed cursors, page boundaries, and the end of the feed. Verify the
  first page contains ten posts and no later page is queried during the
  initial HTML request.
- Test full-post HTML escaping and media URLs. In a browser, check that
  scrolling appends the next ten previews, a short first page waits for a
  scroll or button click, expansion fetches one post, and failed requests can
  be retried.

After implementation, update `docs/design/website-post-backend.md` with the
pagination and HTML routes, clarify the older React description in
`docs/design/architecture.md`, and remove this completed plan.
