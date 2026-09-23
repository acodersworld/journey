#!/usr/bin/env bash
set -Eeuo pipefail

: "$JOURNEY_TEST_URL"
: "$JOURNEY_SITE_USERNAME"
: "$JOURNEY_SITE_PASSWORD"
: "$JOURNEY_HOME_USERNAME"
: "$JOURNEY_HOME_PASSWORD"
: "$JOURNEY_CA_CERT"
: "$JOURNEY_IMAGE_SHA256"
: "$JOURNEY_VIDEO_SHA256"
: "$JOURNEY_IMAGE_SIZE"
: "$JOURNEY_VIDEO_SIZE"
: "$JOURNEY_IMAGE_FIXTURE"
: "$JOURNEY_VIDEO_FIXTURE"
: "$JOURNEY_HOME_UPLOAD_DIR"
: "$JOURNEY_AWS_SSH_TARGET"
: "$JOURNEY_HOME_SSH_TARGET"

results_dir=$(mktemp -d /tmp/journey-validation.XXXXXX)
curl_config=$(mktemp)
home_curl_config=$(mktemp)
wrong_curl_config=$(mktemp)
trap 'rm -f "$curl_config" "$home_curl_config" "$wrong_curl_config"' EXIT
chmod 600 "$curl_config" "$home_curl_config" "$wrong_curl_config"

cat >"$curl_config" <<EOF
user = "$JOURNEY_SITE_USERNAME:$JOURNEY_SITE_PASSWORD"
cacert = "$JOURNEY_CA_CERT"
silent
show-error
location
EOF
cat >"$home_curl_config" <<EOF
user = "$JOURNEY_HOME_USERNAME:$JOURNEY_HOME_PASSWORD"
cacert = "$JOURNEY_CA_CERT"
silent
show-error
location
EOF
cat >"$wrong_curl_config" <<EOF
user = "invalid:invalid"
cacert = "$JOURNEY_CA_CERT"
silent
show-error
location
EOF

record() {
  printf '%s\n' "$*" | tee -a "$results_dir/validation.log"
}

status_without_auth() {
  curl --config /dev/null --output /dev/null --write-out '%{http_code}' "$1"
}

status_with_config() {
  curl --config "$1" --output /dev/null --write-out '%{http_code}' "$2"
}

expect_status() {
  label=$1
  expected=$2
  actual=$3
  if [ "$actual" != "$expected" ]; then
    record "FAIL $label status=$actual expected=$expected"
    exit 1
  fi
  record "PASS $label status=$actual"
}

header_value() {
  awk -F': *' -v key="$1" \
    'tolower($1) == tolower(key) { value=$2 } END { sub(/[\r\n]+$/, "", value); print value }' "$2"
}

compare_hash() {
  label=$1
  expected=$2
  path=$3
  actual=$(sha256sum "$path" | awk '{print $1}')
  if [ "$actual" != "$expected" ]; then
    record "FAIL $label sha256=$actual expected=$expected"
    exit 1
  fi
  record "PASS $label sha256=$actual"
}

record_stats() {
  label=$1
  ssh "$JOURNEY_AWS_SSH_TARGET" \
    'docker stats --no-stream --format "{{.Name}} rss={{.MemUsage}}"; docker inspect --format "{{.Name}} restarts={{.RestartCount}} oom={{.State.OOMKilled}}" $(docker ps -q)' \
    >"$results_dir/aws-stats-$label.txt" || true
  ssh "$JOURNEY_HOME_SSH_TARGET" \
    'docker stats --no-stream --format "{{.Name}} rss={{.MemUsage}}"; docker inspect --format "{{.Name}} restarts={{.RestartCount}} oom={{.State.OOMKilled}}" $(docker ps -q)' \
    >"$results_dir/home-stats-$label.txt" || true
  record "stats $label recorded"
}

expect_status 'missing site credentials' 401 \
  "$(status_without_auth "$JOURNEY_TEST_URL/")"
expect_status 'incorrect site credentials' 401 \
  "$(status_with_config "$wrong_curl_config" "$JOURNEY_TEST_URL/")"
expect_status 'home credentials cannot access site' 401 \
  "$(status_with_config "$home_curl_config" "$JOURNEY_TEST_URL/")"
expect_status 'site credentials cannot establish home WebSocket' 401 \
  "$(curl --config "$curl_config" \
    -H 'Connection: Upgrade' -H 'Upgrade: websocket' \
    -H 'Sec-WebSocket-Version: 13' \
    -H 'Sec-WebSocket-Key: dGhlIHNhbXBsZSBub25jZQ==' \
    -H 'Sec-WebSocket-Protocol: h2-over-websocket-v1' \
    --output /dev/null --write-out '%{http_code}' \
    "$JOURNEY_TEST_URL/internal/storage")"
expect_status 'health' 200 "$(status_with_config "$curl_config" "$JOURNEY_TEST_URL/health")"
record_stats idle

image="$results_dir/image.jpg"
video="$results_dir/video.mp4"
curl --config "$curl_config" --fail --output "$image" "$JOURNEY_TEST_URL/media/image.jpg"
curl --config "$curl_config" --fail --output "$video" "$JOURNEY_TEST_URL/media/video.mp4"
compare_hash 'complete image' "$JOURNEY_IMAGE_SHA256" "$image"
compare_hash 'complete video' "$JOURNEY_VIDEO_SHA256" "$video"
[ "$(stat -c '%s' "$image")" = "$JOURNEY_IMAGE_SIZE" ]
[ "$(stat -c '%s' "$video")" = "$JOURNEY_VIDEO_SIZE" ]
record 'PASS complete media sizes'
record_stats complete-media

head_file="$results_dir/video.headers"
curl --config "$curl_config" --head --dump-header "$head_file" \
  --output /dev/null "$JOURNEY_TEST_URL/media/video.mp4"
expect_status 'video HEAD' 200 "$(awk 'NR == 1 { print $2 }' "$head_file")"
[ "$(header_value Content-Length "$head_file")" = "$JOURNEY_VIDEO_SIZE" ]
[ "$(header_value Content-Type "$head_file")" = 'video/mp4' ]
[ "$(header_value Accept-Ranges "$head_file")" = 'bytes' ]
[ -n "$(header_value ETag "$head_file")" ]
record 'PASS video HEAD metadata'

head_range_file="$results_dir/video-range.headers"
curl --config "$curl_config" --head --header 'Range: bytes=0-1023' \
  --dump-header "$head_range_file" --output /dev/null "$JOURNEY_TEST_URL/media/video.mp4"
expect_status 'video range HEAD' 206 "$(awk 'NR == 1 { print $2 }' "$head_range_file")"
[ "$(header_value Content-Length "$head_range_file")" = 1024 ]
[ "$(header_value Content-Range "$head_range_file")" = "bytes 0-1023/$JOURNEY_VIDEO_SIZE" ]
record 'PASS video range HEAD metadata'

range_check() {
  label=$1
  range=$2
  expected=$3
  output="$results_dir/$label.bin"
  headers="$results_dir/$label.headers"
  curl --config "$curl_config" --fail --header "Range: $range" \
    --dump-header "$headers" --output "$output" "$JOURNEY_TEST_URL/media/video.mp4"
  cmp "$expected" "$output"
  record "PASS range $label status=$(awk 'NR == 1 { print $2 }' "$headers")"
}

dd if="$JOURNEY_VIDEO_FIXTURE" of="$results_dir/begin.bin" \
  bs=1 count=1024 status=none
range_check begin 'bytes=0-1023' "$results_dir/begin.bin"
fixture_size=$(stat -c '%s' "$JOURNEY_VIDEO_FIXTURE")
middle_start=$((fixture_size / 2))
dd if="$JOURNEY_VIDEO_FIXTURE" of="$results_dir/middle.bin" \
  bs=1 skip="$middle_start" count=4096 status=none
range_check middle "bytes=$middle_start-$((middle_start + 4095))" "$results_dir/middle.bin"
tail_start=$((fixture_size - 4096))
dd if="$JOURNEY_VIDEO_FIXTURE" of="$results_dir/end.bin" \
  bs=1 skip="$tail_start" count=4096 status=none
range_check ending "bytes=$tail_start-$((fixture_size - 1))" "$results_dir/end.bin"
tail -c +$((middle_start + 1)) "$JOURNEY_VIDEO_FIXTURE" >"$results_dir/open-ended.bin"
range_check open-ended "bytes=$middle_start-" "$results_dir/open-ended.bin"
range_check end "bytes=$tail_start-" "$results_dir/end.bin"
dd if="$JOURNEY_VIDEO_FIXTURE" of="$results_dir/suffix.bin" \
  bs=1 skip="$tail_start" count=4096 status=none
range_check suffix 'bytes=-4096' "$results_dir/suffix.bin"
expect_status 'malformed range' 400 \
  "$(curl --config "$curl_config" --header 'Range: not-a-range' \
    --output /dev/null --write-out '%{http_code}' "$JOURNEY_TEST_URL/media/video.mp4")"
expect_status 'multiple range' 400 \
  "$(curl --config "$curl_config" --header 'Range: bytes=0-1,2-3' \
    --output /dev/null --write-out '%{http_code}' "$JOURNEY_TEST_URL/media/video.mp4")"
expect_status 'unsatisfiable range' 416 \
  "$(curl --config "$curl_config" --header "Range: bytes=$fixture_size-" \
    --output /dev/null --write-out '%{http_code}' "$JOURNEY_TEST_URL/media/video.mp4")"

upload="$results_dir/upload-256MiB.bin"
dd if=/dev/zero of="$upload" bs=1M count=256 status=none
upload_expected=$(sha256sum "$upload" | awk '{print $1}')
curl --config "$curl_config" --fail \
  --header 'Content-Type: application/octet-stream' --data-binary "@$upload" \
  "$JOURNEY_TEST_URL/api/uploads" >"$results_dir/upload.json"
upload_sha=$(sed -n 's/.*"sha256"[[:space:]]*:[[:space:]]*"\([0-9a-f]\{64\}\)".*/\1/p' "$results_dir/upload.json")
upload_size=$(sed -n 's/.*"size"[[:space:]]*:[[:space:]]*\([0-9][0-9]*\).*/\1/p' "$results_dir/upload.json")
[ "$upload_sha" = "$upload_expected" ]
[ "$upload_size" = 268435456 ]
record "PASS upload sha256=$upload_sha size=$upload_size"
compare_hash 'stored upload' "$upload_expected" "$JOURNEY_HOME_UPLOAD_DIR/$upload_expected"
record_stats upload

throttled="$results_dir/throttled-video.mp4"
curl --config "$curl_config" --limit-rate 64k --max-time 30 \
  --output "$throttled" "$JOURNEY_TEST_URL/media/video.mp4" &
throttled_pid=$!
sleep 1
image_start=$(date +%s)
curl --config "$curl_config" --fail --max-time 5 \
  --output "$results_dir/concurrent-image.jpg" "$JOURNEY_TEST_URL/media/image.jpg"
image_seconds=$(( $(date +%s) - image_start ))
[ "$image_seconds" -le 5 ]
record "PASS concurrent image seconds=$image_seconds"
record_stats concurrent
kill "$throttled_pid" 2>/dev/null || true
wait "$throttled_pid" 2>/dev/null || true
expect_status 'health after cancellation' 200 \
  "$(status_with_config "$curl_config" "$JOURNEY_TEST_URL/health")"
curl --config "$curl_config" --fail --max-time 5 --output /dev/null \
  "$JOURNEY_TEST_URL/media/image.jpg"
record 'PASS fresh media after cancellation'
record_stats cancelled

for repeat in 1 2; do
  curl --config "$curl_config" --fail --output /dev/null \
    "$JOURNEY_TEST_URL/media/video.mp4"
  curl --config "$curl_config" --fail --header 'Content-Type: application/octet-stream' \
    --data-binary "@$upload" --output /dev/null "$JOURNEY_TEST_URL/api/uploads"
done
record 'PASS repeated large transfers'
if find "$JOURNEY_HOME_UPLOAD_DIR" -maxdepth 1 -name '*.part' -print -quit | grep -q .; then
  record 'FAIL temporary upload files remain'
  exit 1
fi
record 'PASS no temporary upload files'
record_stats repeated
record "PASS automated validation results=$results_dir"
