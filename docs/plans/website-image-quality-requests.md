# Website image quality requests

## Summary

Use the storage service's quality-only JPEG encoding for displayed images, requesting quality **80** and storage caching.

Apply this to all stored image formats across feeds, galleries, drafts, post previews, archives, and shared posts. Preserve full display dimensions. Transparency becomes white; animated images show their first frame.

Original-download links continue returning original files. Video playback remains unchanged.

## Configuration and storage requests

Add a configurable setting:

```toml
[site]
image_quality = 80
```

Default to 80 when omitted. Validate integer values from 1 through 100 and pass the setting into application state.

Add a typed `StorageClient::get_quality_image` operation accepting the storage key, validated quality, and HEAD flag. Implement it for both h2c and WebSocket clients through their shared request machinery.

Send:

```http
GET /objects/<key>
Object-Image-Quality: 80
Object-Image-Cache: true
```

- Use HEAD when requested.
- Send no representation, dimension, fit, format, byte-limit, or Range headers on quality-only requests.
- Also send `Object-Image-Cache: true` on existing video-thumbnail and WhatsApp reduced-image requests.
- Preserve WhatsApp's existing dimensions and byte limit; do not add the quality header to those requests.
- Keep upload, backup, performance, and original-object requests unchanged.

## Media handling

Select the representation after existing access checks:

| Request | Storage operation |
|---|---|
| Display a stored image | Quality-only JPEG |
| Download an original image | Original object |
| Video poster | Existing thumbnail |
| WhatsApp image preview | Existing reduced image |
| Video playback/download | Existing original/range handling |

Use the shared media proxy so the behavior applies consistently to authenticated and shared views without changing browser URLs.

- Validate successful quality responses as `200` with JPEG content type and a positive content length.
- Return their actual content type and length, rather than the source image's database content type.
- Generated JPEG responses must not advertise original-object byte ranges.
- Preserve streaming, cancellation, security headers, and guest `no-store` behavior.
- If storage rejects conversion with `413` or `415`, fall back to the original image. This preserves access to existing images exceeding decoder limits or using unsupported formats.
- Do not fall back for missing objects, corruption/server errors, malformed successful responses, or transport failures. Follow existing error handling.
- Original downloads preserve the original content type, filename extension, and attachment behavior.

## Website cache integration

Add a quality-image identity containing:

```text
storage key + quality-image representation + quality value
```

- Use it for displayed JPEGs; retain the original identity for downloads.
- Changing configured quality produces a distinct entry.
- Preserve existing thumbnail and WhatsApp cache identities.
- Keep existing cache files readable without clearing or migrating the cache.
- On a quality-cache miss, request the storage derivative and stream it through the existing part-file mechanism.
- Cache only validated successful JPEG responses.
- An original fallback may use the original-image cache, but must never populate a quality-image entry.
- Continue checking authorization before every lookup.

Keep request construction in the storage client and representation selection in media handling. Add no image processing libraries or encoding work to the website.

## Verification and rollout

- Verify exact quality and cache headers over both transports, including HEAD and exclusion of incompatible headers.
- Verify default/configured quality and rejection of invalid configuration.
- Cover displayed images in authenticated, draft, shared, and gallery contexts.
- Confirm JPEG response metadata for PNG, WebP, and animated-image sources.
- Confirm original downloads retain original bytes and metadata.
- Verify quality-dependent cache separation and that existing original entries cannot satisfy quality requests.
- Verify `413`/`415` fallback and absence of fallback for other failures.
- Verify authorization and revocation still apply to cached images.
- Confirm video ranges, thumbnails, and WhatsApp constraints retain their behavior.
- Update configuration examples and current design documentation.

Deploy the storage quality/cache implementation before enabling the updated website binary. No database migration, browser URL change, or frontend layout change is required.
