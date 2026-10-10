# Remove stored gallery alt text

## Summary

Remove the custom alt-text feature from gallery editing, application models,
and storage. Preserve existing posts and media through a versioned database
migration.

## Changes

- Remove Alt text inputs, editor help references, JavaScript state,
  serialization, and duplication handling.
- Remove alt fields from internal post/media models, database queries, and new
  JSON responses.
- Continue accepting the legacy `alt` property in editor requests and import
  manifests as ignored compatibility input, so existing clients and files
  remain usable.
- Keep HTML image `alt` attributes, using the media label when available and an
  empty value otherwise.
- Update documentation and example manifests.

## Migration

- Add a transactional migration to the next schema version that drops
  `post_blocks.alt_text`. Update fresh-database creation accordingly.
- Before dropping the column, verify all its values are null or
  whitespace-only, including drafts. If populated values are found, stop with
  a clear error and leave the database unchanged.
- Preserve post and block IDs, relationships, media references, text,
  ordering, accounts, sessions, comments, and share links.
- Coordinate the schema version with other pending migrations.

## Verification

- Test migration preservation, rollback, repeated startup, and rejection of
  unexpectedly populated alt text.
- Verify draft and published editing, uploads, duplication, and reordering
  work without the field.
- Verify requests from the previous editor and legacy manifests remain
  accepted.
- Check image and gallery rendering after removal.

## Assumptions

- No live content needs the stored alt-text values.
- Removing the custom field does not remove HTML accessibility attributes.
