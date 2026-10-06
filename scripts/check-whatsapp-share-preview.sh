#!/usr/bin/env bash
set -Eeuo pipefail
umask 077

usage() {
    cat >&2 <<'EOF'
Usage: scripts/check-whatsapp-share-preview.sh SHARE_URL

Fetch a fresh WhatsApp share preview and immediately fetch its og:image.
Use a published post with media and a share URL from the local site, for example:
  scripts/check-whatsapp-share-preview.sh http://127.0.0.1:8080/share/<id>/<secret>

Set WHATSAPP_USER_AGENT or CURL_MAX_TIME to override the defaults.
Diagnostic files are kept in a private temporary directory printed by the script.
EOF
}

if [[ $# -ne 1 ]]; then
    usage
    exit 2
fi

share_url=$1
if [[ "$share_url" != http://* && "$share_url" != https://* ]]; then
    echo "SHARE_URL must use http:// or https://" >&2
    exit 2
fi

for command_name in curl python3; do
    if ! command -v "$command_name" >/dev/null 2>&1; then
        echo "$command_name is required" >&2
        exit 2
    fi
done

# Match the marker-free user-agent observed in the WhatsApp access log.
user_agent=${WHATSAPP_USER_AGENT:-WhatsApp/2.23.20.0}
curl_max_time=${CURL_MAX_TIME:-30}
diagnostic_dir=$(mktemp -d "${TMPDIR:-/tmp}/journey-whatsapp-preview.XXXXXX")
html_headers="$diagnostic_dir/share-headers.txt"
html_body="$diagnostic_dir/share-preview.html"
image_headers="$diagnostic_dir/image-headers.txt"
image_body="$diagnostic_dir/share-preview.jpg"

show_headers() {
    python3 - "$1" <<'PY'
from pathlib import Path
import sys

blocks = []
current = None
for line in Path(sys.argv[1]).read_text(errors="replace").splitlines():
    if line.startswith("HTTP/"):
        current = {"status": line, "headers": {}}
        blocks.append(current)
    elif current is not None and ":" in line:
        name, value = line.split(":", 1)
        current["headers"][name.casefold()] = value.strip()

if not blocks:
    print("  (no HTTP response headers captured)")
else:
    response = blocks[-1]
    print(f"  {response['status']}")
    for name in ("content-type", "content-length", "cache-control"):
        if name in response["headers"]:
            print(f"  {name}: {response['headers'][name]}")
PY
}

echo "Fetching share preview with the WhatsApp user agent"
printf 'User-Agent: %s\n' "$user_agent"
if html_status=$(curl --silent --show-error --max-time "$curl_max_time" \
    --user-agent "$user_agent" \
    --dump-header "$html_headers" \
    --output "$html_body" \
    --write-out '%{http_code}' \
    "$share_url"); then
    :
else
    echo "Could not fetch the share page; captured headers:" >&2
    show_headers "$html_headers" >&2
    echo "Response body: $html_body" >&2
    exit 1
fi

printf 'Share page HTTP status: %s\n' "$html_status"
show_headers "$html_headers"
if [[ "$html_status" != 200 ]]; then
    echo "Expected HTTP 200 from the WhatsApp preview route" >&2
    echo "Response body: $html_body" >&2
    exit 1
fi

if image_url=$(python3 - "$html_body" <<'PY'
from html.parser import HTMLParser
from pathlib import Path
from urllib.parse import urlsplit
import sys

class MetaParser(HTMLParser):
    def __init__(self):
        super().__init__()
        self.values = {}

    def handle_starttag(self, tag, attrs):
        if tag.casefold() != "meta":
            return
        values = dict(attrs)
        key = values.get("property") or values.get("name")
        if key:
            self.values[key.casefold()] = values.get("content", "")

parser = MetaParser()
parser.feed(Path(sys.argv[1]).read_text(errors="replace"))
for name in ("og:title", "og:description", "og:url"):
    print(f"{name} present: {'yes' if parser.values.get(name) else 'no'}", file=sys.stderr)

image_url = parser.values.get("og:image", "")
if not image_url:
    print("og:image is missing; use a post that contains media", file=sys.stderr)
    sys.exit(1)

parts = urlsplit(image_url)
if parts.scheme not in ("http", "https") or not parts.netloc:
    print("og:image is not an absolute HTTP(S) URL", file=sys.stderr)
    sys.exit(1)

print("og:image present: yes (URL redacted)", file=sys.stderr)
print(image_url)
PY
); then
    :
else
    echo "Could not extract a usable og:image URL" >&2
    echo "Preview HTML: $html_body" >&2
    exit 1
fi

echo "Fetching og:image immediately with the same user agent"
if image_status=$(curl --silent --show-error --max-time "$curl_max_time" \
    --user-agent "$user_agent" \
    --dump-header "$image_headers" \
    --output "$image_body" \
    --write-out '%{http_code}' \
    "$image_url"); then
    :
else
    echo "Could not fetch og:image; captured headers:" >&2
    show_headers "$image_headers" >&2
    echo "Image body: $image_body" >&2
    exit 1
fi

printf 'og:image HTTP status: %s\n' "$image_status"
show_headers "$image_headers"
if [[ "$image_status" != 200 ]]; then
    echo "Image fetch failed; response body: $image_body" >&2
    exit 1
fi

if ! python3 - "$image_headers" "$image_body" <<'PY'
from pathlib import Path
import sys

blocks = []
current = None
for line in Path(sys.argv[1]).read_text(errors="replace").splitlines():
    if line.startswith("HTTP/"):
        current = {"status": line, "headers": {}}
        blocks.append(current)
    elif current is not None and ":" in line:
        name, value = line.split(":", 1)
        current["headers"][name.casefold()] = value.strip()

if not blocks:
    raise SystemExit("No image response headers were captured")

response = blocks[-1]
status_fields = response["status"].split()
if len(status_fields) < 2 or status_fields[1] != "200":
    raise SystemExit(f"Unexpected image response status: {response['status']}")

content_type = response["headers"].get("content-type", "").split(";", 1)[0].strip().casefold()
if content_type != "image/jpeg":
    raise SystemExit(f"Expected image/jpeg, received {content_type or '(missing)'}")

body = Path(sys.argv[2]).read_bytes()
if not 1 <= len(body) <= 599_999:
    raise SystemExit(f"Image body size is outside 1..599999 bytes: {len(body)}")
if not body.startswith(b"\xff\xd8\xff"):
    raise SystemExit("Image response does not have a JPEG signature")

header_length = response["headers"].get("content-length")
if header_length is None:
    raise SystemExit("Image response omitted Content-Length")
try:
    advertised_length = int(header_length)
except ValueError:
    raise SystemExit(f"Image response has invalid Content-Length: {header_length!r}")
if advertised_length != len(body):
    raise SystemExit(f"Content-Length is {advertised_length}, received {len(body)} bytes")

print(f"Image validated: JPEG, {len(body)} bytes")
PY
then
    echo "Image validation failed; response body: $image_body" >&2
    exit 1
fi

echo "Diagnostic files: $diagnostic_dir"
