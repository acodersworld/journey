# Mobile gallery and single-video playback

The desktop gallery remains a centered dialog with its existing layout. On
mobile, the dialog fills the dynamic viewport and its item region uses native
vertical scrolling. Photos and videos use their intrinsic aspect ratios,
capped at the viewport height, so wide media does not leave empty space before
its heading and caption. The close button overlays the top-right corner inside
the device safe area.

Opening a gallery aligns the selected item at the top of the scroll region.
Gallery media keeps its existing lazy loading and cleanup. Scrolling does not
snap between entries, use custom swipe handling, or invoke the Fullscreen API.

Only one video may play on a page at a time. A captured document-level `play`
event handles existing and dynamically created players alike; it pauses every
other playing video while retaining its current position. Closing the gallery
pauses its players and releases their media sources. It does not resume videos
paused when gallery playback began.
