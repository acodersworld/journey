# Full-post viewer overlay

## Summary

Add **Open post** to All posts entries. Display the complete inline post in a
dismissible overlay while preserving the underlying feed and scroll position.

Reuse the existing share-preview-style rendering inside a same-origin iframe.
This feature can be implemented independently of
[All posts media mosaics](all-posts-media-mosaics.md).

## Inline post page

Add an authenticated `/posts/{id}/view` route:

- Permit accounts authorized to read that published post.
- Render all post text, media, headings, and captions using the existing full
  inline presentation.
- Use no mosaics or recursive Open post action.
- Reuse galleries, video controls, existing media URLs, and local-time
  rendering.
- Do not show share-management controls.
- Preserve existing share-management permissions and `/share-preview`
  authorization.
- Include a normal link back to All posts for direct visits.
- Apply existing private response and authorization policies.

## Open post action and overlay

Add Open post to every All posts entry, including text-only and single-photo
posts. Include it in dynamically loaded entries.

Render a real link to the inline post page. Intercept ordinary clicks to open
a native dialog containing an iframe. Modified clicks and JavaScript-disabled
use follow the normal link.

The overlay must:

- Provide a persistent top-right close button and Escape dismissal.
- Use a large centered desktop panel and the available viewport on mobile.
- Show loading, retryable error, and standalone-link states.
- Preserve the underlying feed DOM and scroll position.
- Pause automatic feed loading while open and resume it on close.
- Pause underlying videos when opened.
- Preserve the single-playing-video rule within the embedded post.
- Let Escape close an embedded gallery before closing the outer viewer.
- Unload the iframe on close to stop playback and pending requests.
- Restore body scrolling and focus to the opening link without scrolling the
  feed.
- Ignore stale iframe load events after closure or a subsequent opening.

Keep the viewer implementation in focused frontend helpers/assets, with
targeted integration into rendering and existing controls. It must work with
the current feed or the mosaic feed.

The collection-sharing feature may later reuse this viewer with its own
collection-authorized inline post URLs. That guest-access integration belongs
to the [time-period collection plan](time-period-collection-share-links.md),
not this feature. No database migration, storage protocol extension, or
stored-post changes are required here.

## Verification

- Verify published-post access for read accounts and denial of unauthorized
  or draft access.
- Confirm existing share-management permissions are unchanged.
- Test initial and dynamically loaded Open post actions.
- Check loading failures, retry, direct visits, modified clicks, and
  JavaScript-disabled navigation.
- Verify desktop/mobile sizing, keyboard operation, nested galleries, and
  Escape behavior.
- Verify retained feed position, focus restoration, automatic-loading
  suspension/resumption, and media cleanup.
- Confirm the viewer displays every media item and caption without mosaics.
