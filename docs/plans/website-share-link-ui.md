# Copy share links from the post UI

## Summary

Add an owner-only Share control to each published post in the feed and on its
full page. The control opens a panel with a small, scrollable guest-page
preview, a full-size preview option, and a **Copy share link** button. Opening
or previewing the panel does not create a link.

## Interface and behavior

- Render the Share control on published posts for the owner, including posts
  loaded on demand. Do not offer link creation for drafts or readers.
- Use the existing guest-page layout for an owner-authenticated preview. The
  preview must load the post's media without creating a share link or guest
  session. Show it in a small scrollable frame in the panel, with a way to open
  it full size.
- On the first **Copy share link** click, create a link using the existing
  global expiry setting and copy its URL. Retain the URL only in the open
  panel so subsequent clicks copy the same link. Keep the button label
  **Copy share link** throughout.
- Show **Link copied!** beside the button in a polite live region after each
  successful copy. Clear it after three seconds and restart that timer after
  another successful copy. Keep failure messages visible so the owner can
  retry.
- Show only the current panel's link and its expiry; do not display the URL as
  text or list older links. Closing the panel or reloading the page discards
  the URL. On the next panel opening, the first copy creates a new link.
- Provide **Revoke link** for the current link. After revocation, the next
  **Copy share link** click creates and copies a new link. Older links remain
  manageable through the CLI.

## Backend and failure handling

- Add owner-only HTTP endpoints to create and revoke links, reusing the
  current database operations and token generation. Validate the request
  origin and return creation data with `Cache-Control: no-store`.
- Prevent rapid repeated clicks from creating duplicate links. If the link is
  created but clipboard copying fails, retain that link in the open panel and
  retry copying it without creating another.
- The preview is owner-authenticated and must not grant guest access. The
  actual guest link and its media routes keep their existing behavior.

## Verification

- Check the Share control on full post pages and feed posts, including posts
  loaded on demand, and verify that readers and drafts cannot create links.
- Confirm opening either preview creates no link and that its layout and
  media match the guest page.
- Check first and repeated copies, three-second success feedback, closing and
  reopening the panel, clipboard failure and retry, revocation, and rapid
  repeated clicks.
- Verify owner-only endpoint authorization and origin checks. Confirm that a
  created link opens the intended published post and its media.
