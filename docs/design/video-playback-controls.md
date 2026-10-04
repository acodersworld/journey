# Video playback controls

Actual video players use native controls for seeking, volume, and accessibility,
plus a shared overlay button across standalone posts, draft previews, and
opened gallery items. The overlay covers the picture area while reserving the
native control bar. Its icon is hidden during playback while the picture area
remains tappable to pause. The play icon returns when playback pauses or ends.
Playback starts directly from the button activation, and a rejected play request
leaves the play button available.

Gallery thumbnails remain images. Opening their player still begins with a
thumbnail launch button; after the player is created, it uses the same overlay
as every other actual player.
