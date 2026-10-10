# Remove download controls from shared views

## Summary

Remove visible **Download original** links from guest share-link pages and
share previews, including shared block views.

## Changes

- Suppress download links for standalone images, videos, and gallery items
  when rendering a shared view.
- Include the authenticated share preview, which uses the shared renderer
  with ordinary media URLs.
- Keep download controls on normal signed-in post pages and editors.
- Update the shared-view design documentation.

## Verification

- Confirm shared posts and previews contain no Download original links for
  images or videos.
- Check shared gallery and individual-block views.
- Confirm media display, thumbnails, and playback still work.
- Verify normal post and editor downloads remain available.

## Assumptions

- This removes the visible controls; media endpoints and browser-native
  saving behavior remain available.
- No database schema or migration change is required.
