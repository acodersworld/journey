# All posts media mosaics

The All posts page renders photos and videos, whether standalone or in a post
group, as square mosaic tiles. Runs of multiple media items inside a group use
the same layout. The site setting `site.mosaic_max_tiles` defaults to eight and
must be positive. The limit applies to each media run. A `+N` overlay on the
last visible tile counts the omitted items.

The initial All posts response selects mosaic rendering on the server. Its
automatic loading requests `/posts/{id}/fragment?mosaic=true`. Fragment
requests without this parameter retain the original gallery presentation, as
do month archives, tag feeds, shared posts, full post pages, and editors.

Each mosaic stores the complete ordered media metadata in an inert HTML
`template`, separate from the visible buttons. Clicking a tile passes that
metadata and the tile's index to the existing gallery dialog. The dialog
creates image elements with deferred URLs and loads them near the scroll
position. Video streams are still created only when the viewer plays a video.
Only visible tiles have preview images in the feed. The existing media URLs,
authorization, image quality behavior, and cache identities remain in use.
No stored post or database format changes are involved.
