# On-demand image reduction in object storage

## Goal

Let the object store return a smaller still image from an existing object, without
creating another object key. This supplies ordinary website image sizes and the
JPEG required by the separate WhatsApp share-preview plan. The original payload
and its metadata remain unchanged. The website video-thumbnail feature uses the
existing thumbnail representation; keep that behavior unchanged while adding
the separate image-reduction path described here.

## Request and response contract

- Use authenticated `GET` and `HEAD /objects/<key>`. Add
  `Object-Representation: reduced-image` for image objects. Extend the existing
  `Object-Representation: thumbnail` path to accept the same reduction options
  for video objects, applied to its generated still frame. Without reduction
  options, the existing video-thumbnail response keeps its current behavior.
- Accept exactly one dimension form: `Object-Image-Max-Edge: <pixels>`, or both
  `Object-Image-Width: <pixels>` and `Object-Image-Height: <pixels>`. The edge
  form bounds the longest output side. Width and height form a bounding box;
  the default fit preserves aspect ratio and does not crop or upscale.
- Accept `Object-Image-Fit: contain` (the default) or `pad`. `pad` fits the
  source inside the requested width and height, then fills the remaining canvas
  with white. It requires both dimensions and produces those exact output
  dimensions. In both modes the image content keeps its aspect ratio.
- Accept optional `Object-Image-Format: jpeg`. Without it, preserve JPEG, PNG,
  or WebP when the installed encoder supports the source format. A reduced
  animated GIF is a still JPEG from its first frame. HEIC/HEIF and any other
  decodable source without a matching encoder fall back to JPEG. An explicit
  JPEG request always returns JPEG. Preserve transparency when the selected
  output format supports it; JPEG composites transparency over white.
- Accept optional `Object-Image-Max-Bytes: <bytes>` only with explicit JPEG
  output. Encode at bounded quality settings until the response fits; if it
  cannot fit within the requested dimensions and permitted quality range,
  return a clear error rather than silently exceeding the cap or changing the
  requested canvas. This header limits the response body, not the input file.
- Bound each requested dimension to 1..=2048 pixels. Reject duplicate,
  conflicting, malformed, or unsupported option headers with `400`. Require a
  dimension form for `reduced-image`; reject image options on the original
  representation. Reject `Range` for generated representations, matching the
  thumbnail behavior. Non-image sources for `reduced-image` and non-video
  sources for `thumbnail` return `415`.
- Return the actual `Content-Type` and `Content-Length`; `HEAD` reports the
  same headers as `GET` without a body. Add every representation and option
  header used to select output to `Vary` so different requests cannot be
  confused by an HTTP cache.

For WhatsApp, the website requests a 600 x 315 padded JPEG with a maximum of
599,999 bytes. Padding makes portrait images at least 300 pixels wide and
keeps the output aspect ratio within WhatsApp's limit. The website's preview
endpoint continues to enforce the response size before streaming it, as its
separate plan specifies. A video source uses the existing thumbnail frame as
the input to the same fit, pad, and JPEG encoding step.

## Processing and resource limits

- Decode from the stored payload without copying it into a second object.
  Apply orientation metadata before sizing. Use the native image facilities
  already being introduced for video thumbnails where practical, but keep
  image reduction behind a separate helper so the existing thumbnail behavior
  remains testable.
- Do not create an image-reduction cache. Each reduced-image request is
  generated on demand. The existing video-thumbnail cache remains responsible
  only for its base frame; do not store each requested image size or quality
  there. A caller may make repeated reductions of that frame.
- Limit concurrent decoding/encoding jobs and decoded pixel count so very
  large inputs cannot exhaust CPU or memory. Run blocking native work outside
  the async request executor. Bound the number of JPEG quality attempts.
- A decoder or encoder failure returns a storage error and is logged; it must
  never alter or delete the original object. Keep output size limits separate
  from the source object length and reject unsupported formats explicitly.

## Code changes

- Define a validated reduction request type in the storage crate and extend
  `StoreInterface` with a method that produces reduced still-image bytes from
  a key and options. Implement it for filesystem and in-memory stores, or
  provide a shared implementation over their existing object-read interface.
- Parse the new headers and dispatch in `http2_storage_service.rs`. Keep the
  public original-object GET/HEAD contract unchanged. Extend the existing
  `thumbnail` handler only enough to apply the requested reduction after the
  video frame has been obtained.
- Add the matching request method in the storage HTTP client so the website
  can request a reduced image without constructing protocol headers itself.
  Do not change the website's media database or responsive-image UI in this
  patch; those are separate work.
- Update object-storage design documentation after implementation, then remove
  this plan as required by `AGENTS.md`.

## Verification

- Test landscape, portrait, square, small, rotated, transparent, animated GIF,
  and available HEIC/HEIF inputs. Check dimensions, content type, orientation,
  and that the original bytes and metadata are unchanged.
- Test both dimension forms, contain and padded output, format preservation and
  JPEG override, byte-cap success and failure, HEAD parity, invalid headers,
  `Range`, and unsupported content types.
- Test the WhatsApp request against both a photo and a video thumbnail: JPEG,
  exactly 600 x 315, less than 600 KB, and at least 300 pixels wide. Verify
  concurrent requests stay within the job limit and no image-variant files are
  created.
