#!/usr/bin/env bash
set -Eeuo pipefail

fixture_dir=$1
mkdir -p "$fixture_dir"

if ! command -v ffmpeg >/dev/null 2>&1; then
  echo "ffmpeg is required" >&2
  exit 1
fi

if command -v convert >/dev/null 2>&1; then
  convert -size 1280x720 gradient:'#15324b-#e5a75b' "$fixture_dir/image.jpg"
else
  echo "ImageMagick convert is required to create image.jpg" >&2
  exit 1
fi

tmp_video="$fixture_dir/video.tmp.mp4"
if ffmpeg -hide_banner -encoders 2>/dev/null | grep -q ' libx264'; then
  video_codec=(-c:v libx264 -preset veryfast -b:v 6M)
else
  video_codec=(-c:v mpeg4 -q:v 2 -b:v 6M)
fi
ffmpeg -hide_banner -loglevel error -y \
  -f lavfi -i 'testsrc2=size=1280x720:rate=30' \
  -t 95 -an "${video_codec[@]}" -movflags +faststart "$tmp_video"
mv "$tmp_video" "$fixture_dir/video.mp4"

video_size=$(stat -c '%s' "$fixture_dir/video.mp4")
if [ "$video_size" -lt $((50 * 1024 * 1024)) ]; then
  echo "generated video is only $video_size bytes; increase its duration or bitrate" >&2
  exit 1
fi

file "$fixture_dir/image.jpg" "$fixture_dir/video.mp4"
sha256sum "$fixture_dir/image.jpg" "$fixture_dir/video.mp4"
