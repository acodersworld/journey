# Edit text in published posts

## Summary

Add an **Edit** button for a published post's author with write permission and
for admins. Reuse the existing editor to correct text, with an explicit
**Save changes** action.

The post stays published, with the same IDs, media, publication time, and share
links.

## Implementation

- Add `/posts/{id}/edit`, protected by author/admin authorization. Readers and
  share-link guests cannot edit.
- Allow editing the main title, summary, tags, block headings and bodies, media
  captions, and alt text.
- Hide block/media add, remove, reorder, duplicate, and upload controls in
  published-edit mode.
- Disable autosave in this mode. Provide **Save changes** and **Cancel**;
  successful saving returns to the post, while failures preserve entered text.
- Add `PUT /api/posts/{id}/text` accepting the revision, post text fields, and
  a flat list of block IDs with their editable text fields.
- Save in one transaction. Require the expected revision, a nonblank main
  title, and exactly the post's existing block IDs. Update only text and tags,
  then increment the revision.
- Apply existing role and same-origin checks. Keep the draft editor's current
  behavior.
- Update the design documentation after implementation.

## Verification

- Correct a published post and confirm readers and shared-link visitors see
  the saved text.
- Verify IDs, media references, block order, author, publication time, and
  share-link credentials remain unchanged.
- Reject unauthorized edits, stale revisions, blank main titles, and invalid
  or duplicate block IDs.
- Confirm Cancel makes no changes and published edits do not autosave.
- Verify ordinary draft editing still works.

## Assumptions

- This first version supports text corrections only.
- Block headings and captions remain optional.
- No schema change or migration is required.
