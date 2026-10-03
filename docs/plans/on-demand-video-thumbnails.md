# On-demand video thumbnails in object storage

## Summary

The private object service can return a JPEG thumbnail through a video's
existing object URL. A caller requests the thumbnail by setting
`Object-Representation: thumbnail`; a request without that header returns the
original object. Website thumbnail use is a later change.

## Object interface

- Support `GET` and `HEAD /objects/<key>` with
  `Object-Representation: thumbnail`. The key is exactly the underlying video's
  key; no separate thumbnail key is exposed.
- Accept thumbnail requests only when the referenced object's stored content
  type is `video/mp4` or `video/quicktime`. Return `415 Unsupported Media Type`
  for other objects. Preserve the existing not-found and corrupt-object
  behavior. Reject unknown or duplicate `Object-Representation` values with
  `400 Bad Request`.
- Reject `Range` on a thumbnail request with `400 Bad Request`. Return
  `Content-Type: image/jpeg` and the JPEG's `Content-Length`; `HEAD` returns
  the same metadata without a body. Add `Vary: Object-Representation` to object
  responses so caches distinguish original and thumbnail representations.

## Generation and cache

- Add `storage.thumbnail_time_ms` to the storage server's TOML configuration,
  defaulting to `0` for the first decodable frame. Requests cannot override
  the server setting. If the configured time exceeds a video's duration, use
  its first decodable frame.
- Use `ffmpeg-next` to decode in a blocking task. Give FFmpeg a seekable
  `Read + Seek` view over the filesystem store's `Arc<File>` using
  `FileExt::read_at`. Translate logical offsets to the video's payload region
  and enforce its payload length, hiding Journey's metadata header. Limit
  concurrent decodes so thumbnail requests cannot exhaust the storage process.
- Encode a JPEG with its aspect ratio preserved and longest edge at most 640
  pixels. Return a decoding failure as an error; never cache a partial image.
- Keep generated JPEGs in a disposable, persistent cache directory inside
  the storage volume but outside indexed objects and upload part files. Include
  the source object's UUID and thumbnail settings in the cache identity so a
  replacement at the same key cannot reuse its predecessor's thumbnail.
  Write cache entries atomically and share one generation job among concurrent
  requests for the same entry. Regenerate missing or invalid cache entries.
- Update `apps/storage/Dockerfile`, which builds the deployed storage image:
  install FFmpeg development packages in its Rust build stage and matching
  runtime libraries in its Debian Trixie stage.

## Verification

- Confirm ordinary and thumbnail GET/HEAD requests use the same key and return
  the corresponding video or JPEG representation.
- Cover `Range`, duplicate and unknown representation headers, non-video and
  missing objects, corrupt or undecodable videos, and a video shorter than the
  configured frame time.
- Confirm concurrent first requests generate once, later requests hit the
  cache, cache deletion triggers regeneration, and replacement at the same key
  selects a new cache entry. Confirm cache reuse after service restart.
- Build the storage Docker image and test thumbnail generation with a real
  MP4. Confirm ordinary object GET and range reads still return the original
  bytes.

## Assumptions

- The thumbnail cache survives service restarts but can be deleted without
  losing stored objects.
- This change adds only the private storage representation. It does not add a
  public thumbnail URL or change the website.
