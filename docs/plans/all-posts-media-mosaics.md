# All posts media mosaics

## Summary

Display multi-item media groups as compact mosaics on **All posts**. Clicking
a tile opens the existing photo/video scroller.

Reuse existing image URLs and caches. Let the browser scale and crop images
for the tiles. Add no image representations, resizing requests, media
endpoints, or cache variants.

The [full-post viewer overlay](full-post-viewer-overlay.md) is a separate plan.
Either feature can be implemented independently. This plan does not add the
Open post action or its overlay.

## Configuration

```toml
[site]
mosaic_max_tiles = 8
```

Default to eight and require a positive integer. Apply the limit separately to
each media group.

## Rendering and gallery behavior

- Preserve post text, group headings/descriptions, media order, and group
  boundaries.
- Groups containing multiple media items use square tiles, with up to four
  columns on wider screens and two on mobile.
- Use CSS to scale/crop tile previews without changing the underlying image.
- Display at most the configured number of tiles. Overlay `+N` on the final
  visible tile when more items exist; `N` counts omitted items.
- Show existing thumbnails and a play indicator for video tiles. Do not load
  video streams.
- Show no individual media headings or captions in mosaics.
- Keep single-photo groups inline with their headings and captions. Keep
  single-video behavior unchanged.
- Preserve the existing dark theme and keyboard-accessible media controls.

Keep complete ordered gallery metadata separately from visible tiles:

- The scroller includes every item, including omitted ones.
- Open at the clicked item, including the tile carrying `+N`.
- Preserve headings, captions, normal image aspect ratios, lazy loading,
  mobile scrolling, focus restoration, and media cleanup.
- Omitted items must not create image/video elements or initiate media
  requests before the scroller needs them.
- Preserve existing gallery behavior on pages without mosaics.

## Scope and integration

Use an explicit rendering mode for the main feed and its subsequent fragment
requests. Existing fragment requests default to their current presentation.

Only **All posts** uses mosaics. Month archives, tag feeds, shared posts,
full-post pages, previews, and editors retain their existing presentation.

Existing authorization, media requests, quality settings, caches, pagination,
and automatic loading remain in use. No database migration, storage protocol
extension, or stored-post changes are required.

## Verification

- Cover zero, one, two, eight, and more-than-eight items, configurable limits,
  multiple groups, and mixed images/videos.
- Verify tile ordering, `+N`, caption preservation, and selected-item opening.
- Confirm no premature media requests for omitted items and no video-stream
  requests from tiles.
- Verify initial and subsequently loaded All posts entries use mosaics.
- Confirm other page types retain their current presentation.
- Check desktop/mobile layout, keyboard operation, failed-image presentation,
  and closing the scroller.
- Confirm existing media URLs and cache identities are reused.
