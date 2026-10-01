# Browser video first-frame previews

## Goal

Show the first decodable video frame as a preview wherever the site displays a
video: published gallery tiles, standalone videos, slideshow videos, and draft
editor previews. The viewer's browser obtains the frame from the existing video
URL or a selected local file. Do not generate, upload, or store separate
thumbnail objects, and do not change the database or storage interface.

## Implementation

- Replace the video placeholder in published gallery tiles with a paused,
  muted, inline video preview. Keep the play icon as an overlay and retain the
  gallery button's accessible label and click-to-open behavior. The tile video
  is decorative and has no controls; playback remains in the slideshow.
- Start gallery preview loading only when a tile approaches the viewport. Apply
  the same initialization to tiles in posts appended by the scrolling feed.
  Retain the existing placeholder until a frame is ready and whenever loading
  or decoding fails. If `IntersectionObserver` is unavailable, use a bounded
  scroll/resize visibility check rather than loading every video immediately.
- Use a shared browser helper for controlled video players in standalone posts,
  the slideshow, and draft editor previews, including previews of newly
  selected local files. Once metadata is available, seek to a time near the
  beginning only if a frame is not already available and the player is still
  paused. Reveal the frame after `loadeddata` or a completed seek with current
  frame data. Do not interrupt playback or seek a video after the user starts
  it. Keep controls and download links as they currently work.
- Use the existing authenticated media URLs and byte-range handling for stored
  videos. The shared post renderer also covers guest share pages. Keep the
  existing fallback for unsupported videos and failed preview loads.
- Update the roadmap's video-thumbnail item to describe browser first-frame
  previews rather than generated or stored images.

## Verification

- In Firefox, check initial and newly appended feed posts, standalone videos,
  gallery tiles, slideshow videos, guest share pages, and draft previews before
  and after upload.
- Confirm gallery previews issue requests only when near the viewport, never
  autoplay, and retain a usable placeholder if the video cannot be decoded.
- Confirm opening a gallery tile, playing and seeking in each controlled
  player, and downloading the original video still work. Check that a preview
  seek never overrides a user's playback action.
- Update the existing server-rendering assertion for video gallery markup, and
  run the relevant site tests without running an automated formatter.

## Assumptions

“First frame” means the first frame the browser can decode at or very near the
start of the video. Each viewer may make a small range request and decode that
frame locally. Preview state is not persisted.
