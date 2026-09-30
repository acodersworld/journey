# Full posts in monthly archives

## Goal

Monthly archive pages should show complete posts, including text and media,
using the same presentation and on-demand scrolling as the main feed. The
archive month is the visitor's local calendar month, as determined by the
existing timezone cookie.

## Implementation

- Add an optional `month=YYYY-MM` filter to `GET /api/posts`. Validate it with
  the same month rule as `/archive/{month}`. Obtain the visitor's validated
  timezone from the authenticated principal, and use the existing local-month
  boundary calculation to filter publication timestamps to the half-open UTC
  interval. The month filter composes with the existing tag filter and cursor;
  order remains `published_at DESC, id DESC`. Invalid months return `400`.
- Reuse the filtered feed query for the archive page with a limit of one. Load
  that first post's full record, render it with the existing post renderer, and
  put the next cursor and month in the feed element. Keep the archive heading,
  back link, sidebar, and empty-month message.
- Extend the existing feed JavaScript to include the element's month in later
  `/api/posts` requests. Continue fetching the full post fragment for each
  returned summary, appending it, and using the existing intersection observer,
  Load more button, end message, and retry behavior.
- Enable the existing slideshow on archive pages. Remove archive-summary-only
  rendering and any CSS that becomes unused; use the same post styling as the
  main feed. Media routes and permissions do not change.

## Verification

- A month with multiple posts initially renders one complete post, then loads
  only later posts in that month on scroll or Load more; media and slideshow
  work for both initial and appended posts.
- Empty and single-post months show correct status. A post near midnight
  appears in the correct archive for the visitor's timezone, including across
  daylight-saving boundaries.
- Invalid `month` API values return `400`. Main and tag feeds retain their
  existing pagination, and a combined tag and month filter returns only posts
  satisfying both filters.
