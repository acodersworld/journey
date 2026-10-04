# Tap-to-play video controls

## Summary

Use the same play interaction for standalone posts, opened galleries, and
draft previews. When a video is paused or ended, a visible play button sits
over its picture. Tapping the picture starts playback. Tapping it while
playing pauses immediately, so the play button stays visible. There is no
two-second timer with this chosen behavior.

## Changes

- Add one reusable video-control initializer for actual players. Keep native
  controls for seeking, volume, and accessibility. Gallery thumbnails are
  images, so only actual players receive video control behavior.
- Put a button over the picture area while leaving the native control bar
  uncovered. Its label and visible icon reflect the video's `play`, `pause`,
  and `ended` events. Call `play()` directly from the user's tap and handle a
  rejected play request by leaving the button visible.
- Apply the initializer to server-rendered standalone videos, draft previews
  created in JavaScript, and gallery players created when a video is opened.
  Replace the gallery's initial "Play video" step with the same behavior once
  its player is created.

## Verification

- Check tap-to-play, tap-to-pause, and replay after the video ends in all
  three views.
- Check that seeking and other native controls remain usable and that
  keyboard activation works.
- Manually verify Firefox for Android and desktop Firefox. Thumbnail image
  loading is outside this change.

## Assumption

"Tap the video area" means the picture above the browser's native control
bar; taps on the control bar retain their normal browser behavior.
