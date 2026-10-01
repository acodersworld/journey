# Vertical scrolling gallery panel

## Goal

Replace the one-item slideshow with a modal panel that presents every item in
the selected gallery as a vertical list. Opening any tile scrolls directly to
that item. Visitors scroll up or down to see the others; the gallery does not
advance automatically.

## Implementation

- Replace the slideshow dialog's single media region, previous/next buttons,
  and slide counter with a scrollable gallery region and a close button. Build
  the region from the existing ordered gallery item data when a tile is
  clicked. Keep each item's heading and caption beside its media. A standalone
  image opens as a one-item panel.
- Size each image or video uniformly so it fits within both the panel width
  and the available viewport height. Preserve aspect ratio without cropping or
  distortion. Keep the close button visible while the gallery region scrolls.
- Load images only as they approach the panel's visible area. Reuse the
  browser-generated first-frame preview for video items. Do not autoplay;
  activate a video source only when the visitor chooses to play it. Keep
  simultaneous preview requests bounded so opening a large gallery does not
  occupy every browser connection. Closing the dialog removes active media
  elements and stops playback and requests.
- Open the dialog with the clicked item in view. Let the panel use native
  vertical wheel, touch, and keyboard scrolling. Remove slide-specific arrow
  keys and horizontal swipe handling. Preserve Escape and backdrop closing,
  keyboard access to controls, and focus restoration to the opening tile.
- Keep post gallery tiles and their existing original-file download links.
  Update the roadmap to replace automatic slideshow work with this panel, and
  update the lasting gallery description in the website design document after
  implementation. No database, storage, or HTTP API changes are needed.

## Verification

- Open a gallery from its first, middle, and last tile; confirm the selected
  item is initially visible and scrolling reaches every item in order.
- Check landscape and portrait media, long headings and captions, mobile
  sizing, keyboard navigation, a standalone image, and guest share pages.
- Confirm video preview, user-initiated playback and seeking, and stopping
  playback on close. Confirm that opening a large gallery does not request all
  media at once.
- Run the relevant site tests and manually inspect the panel in a browser.
  Do not run an automated source formatter.

## Assumptions

The panel is a centered modal overlay rather than a side drawer. Media is
scaled uniformly to fit the available panel area; empty space around an image
or video is acceptable. Gallery videos never start playing automatically.
