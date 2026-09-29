# Public site sidebar

## Summary

Add a left sidebar to the public feed and post pages. It starts closed at every
screen width. A small chevron tab opens it; the chevron reverses to close it.

## Implementation changes

- Use an icon-only button with an accessible name, visible focus indicator,
  and accurate expanded state. On wide screens, opening the sidebar gives it a
  column beside the posts; closing it restores the centered reading layout.
  On narrow screens, it opens over the page with a backdrop and closes via the
  chevron, backdrop, or Escape.
- Show the Journey name and current tagline, the five most recent published
  posts, archive months, and tags. Show the first six months and twelve tags
  initially, with controls to expand the complete lists.
- Link each month to a page listing its published post titles, dates, and
  summaries. Link each tag to a full-post scrolling feed filtered by that tag.
  Make tags on posts clickable.
- Extend the existing feed query and `/api/posts` with an optional tag filter.
  Reuse the current post renderer and incremental loading for tag feeds. Share
  the sidebar across the home feed, tag feed, monthly archive, and direct post
  pages.

## Verification

Check chevron state, opening and closing at both screen sizes, keyboard and
focus behavior, recent-post order, expanded lists, monthly results, tag-feed
pagination, and empty results. Confirm unpublished posts do not appear in
sidebar lists or filtered pages.

## Assumptions

The sidebar starts closed on each page load; its state is not saved. Archive
months are newest first. Tags are alphabetic and match their stored spelling
and case. Monthly archive pages list posts rather than full post bodies.
