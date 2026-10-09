# Offer photos first when creating a post on mobile

## Summary

Opening a fresh draft on mobile immediately shows an "Add photos to your new
post" chooser with **Choose photos**, **Take photo**, and **Skip**. Selected
photos populate the first block's gallery through the existing upload flow.

## Implementation

- Show the chooser once when initializing a new, unsaved draft. Use the
  existing mobile gallery media query, including phone landscape layouts.
  Existing drafts open directly in the editor.
- Use two file inputs inside the dialog: a multiple-image picker without
  `capture`, and a single-image input requesting `capture="environment"`.
  Open each picker directly from its button's click handler.
- When files are selected, close the chooser, create the first root block,
  and pass the files in selection order to the normal gallery upload queue.
  Show previews and progress immediately; leave the post title and optional
  text blank.
- Skip, close, or Escape dismisses the chooser without creating a block or
  saving a draft. Cancelling the native picker leaves the chooser available.
- Style the chooser consistently with existing dialogs and restore focus to
  the editor after dismissal. The current New post button opens this flow;
  no additional floating button is needed.
- Keep the existing backend, upload endpoints, media model, and manual Add
  media controls. Document the new mobile flow.

## Verification

- On mobile, select several photos and confirm one first-block gallery
  appears in the selected order, with normal upload progress and editing
  controls.
- Test camera capture and cancellation on physical Android and iPhone devices.
- Confirm Skip and picker cancellation create no unwanted saved draft.
- Verify saved drafts and desktop new-post pages do not show the chooser.
- Check upload failures use the existing error behavior and successfully
  uploaded photos remain editable.

## Assumptions

- The shortcut handles photos; videos remain available through the editor's
  normal Add media action.
- Camera capture is a browser request. Devices that do not honor it may offer
  their ordinary image picker.
- This change requires no database schema update.
