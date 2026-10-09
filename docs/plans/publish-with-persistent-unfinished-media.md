# Publish posts without text, after intended media is complete

## Summary

A nonblank title is enough to publish a post, including a title-only post.
Files selected for a draft become persistent unfinished media slots. Publish
stays greyed out, with a reason below the button, until every slot is uploaded
successfully or removed.

## Implementation

- Allow draft media children with no storage key. Their existing header, body,
  and alt fields preserve text entered before an upload finishes. Do not store
  the device filename. Keep the schema change backwards compatible or provide
  a versioned migration from the deployed schema that preserves existing data,
  as required by `AGENTS.md`. Do not recreate the live database or rerun the
  destructive importer to upgrade it.
- When files are selected, save their slots in the draft **before** streaming
  bytes. Keep the current upload endpoint and same-file upload pool. After
  storage confirms an upload, attach its key through the normal draft save;
  consider the slot complete only when that save succeeds. A failed upload or
  save leaves the slot unfinished. Keep it visible with Retry and Remove;
  after a reload, Retry asks the author to select a file again.
- Remove the publish requirement for a block with text. In the publish
  transaction, reject drafts containing any unfinished media slot. Keep the
  nonblank title, ownership, date, and already-published checks.
- In both the draft editor and draft post page, disable Publish for a blank
  title or unfinished slots. Reuse the existing helper below the editor button
  and add the same visible reason below the post-page button. Show unfinished
  slots on draft pages without broken media links; completed media continues
  to render normally.

## Tests

- Publish a title-only post and a media-only post; still reject a blank title.
- Select a file, reload before it finishes, and verify its slot and entered
  text remain while Publish is disabled. Retry successfully, save the
  completed slot, and verify Publish becomes available.
- Verify failed uploads remain visible, Remove clears the restriction, and a
  successful upload followed by a failed draft save remains unfinished.
- Verify the publish API rejects unfinished slots even when called directly,
  and that imported posts with completed media remain publishable.
- Verify the schema upgrade preserves existing posts, media placements,
  accounts, and share links from the deployed database.

## Assumptions

"Successfully uploaded" means storage confirmed the object and the draft
saved its reference. An unfinished slot has no recoverable file bytes after a
reload, so the author must reselect a file or remove the slot.
