# Website video thumbnails

## Summary

Use the object store's JPEG video thumbnails throughout the website. Stored
videos show a thumbnail in posts, galleries, gallery panels, and drafts. A
video still uploading shows a blank placeholder. Playback and downloads
continue to use the original video.

## Implementation

- Add `?thumbnail=1` to the existing authorized media routes, including
  shared-post media. Resolve access exactly as for the original, then request
  `Object-Representation: thumbnail` from storage. Return `image/jpeg` with
  the storage response length. Reject requests combining thumbnail and
  download.
- Extend the site storage client for thumbnail GET and HEAD requests over
  both h2c and WebSocket transports. Do not add database fields or store
  thumbnail keys.
- Use the thumbnail as the `poster` for standalone and saved draft video
  players, with `preload="none"`. Use a lazy-loaded thumbnail image for
  gallery tiles and gallery-panel previews. Keep original media URLs for
  playback and downloads.
- Remove browser video seeking, canvas capture, and the gallery preview
  queue. Do not decode local videos during upload. If a thumbnail fails to
  load, show a blank video placeholder and keep playback available.

## Verification

- Check thumbnails and playback in the feed, full posts, archives, shared
  posts, drafts, and gallery panels, including content added after page load.
- Confirm that opening a page requests thumbnail JPEGs but does not request
  video bytes until playback; downloads still return originals.
- Check pending uploads, upload completion, thumbnail failure, unauthorized
  media requests, and guest-session expiry. Confirm a failed thumbnail never
  prevents playback.

## Assumptions

- The object store's video-thumbnail endpoint is available when this website
  change is implemented.
- "Blank placeholder" means the existing dark video frame without a
  browser-generated image.
