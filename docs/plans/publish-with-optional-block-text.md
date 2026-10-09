# Require only the main post title to publish

## Summary

A nonblank main title is the only required text content. Block titles, bodies,
captions, and the post summary are optional. Title-only posts and media-only
posts can be published.

## Implementation

- Remove the backend's requirement for at least one block containing text.
- Remove `PublishPostResult::MissingText` and its HTTP error response.
- Keep main-title validation, authorization, publication-date validation, and
  already-published checks. The editor's existing title validation already
  matches the intended behavior.
- Update the publishing design documentation. Remove this completed
  requirement from the larger unfinished-media plan when this patch is
  implemented.

## Verification

- Publish a title-only draft successfully.
- Publish a media gallery with empty block headings and captions successfully.
- Reject empty and whitespace-only post titles.
- Confirm authorization and publication-date validation still work.

## Assumptions

- Block headings, bodies, captions, and post summaries are optional.
- No schema change or migration is required; existing live data is preserved.
- Persistent unfinished-upload handling remains a separate plan.
