# Draft creation UI

## Summary

Add a creation page for `write` and `admin` accounts using the current
text-only draft backend. Put a Drafts list in the existing sidebar and a
**+ New post** button fixed to the viewport's bottom-right. This phase does
not add editing, deletion, publishing, media upload, or autosave.

## Navigation and draft discovery

- Show the floating **+ New post** button on signed-in content pages for
  `write` and `admin` accounts. It links to `/posts/new`. Hide it on that page,
  for `read` accounts, and on guest share pages. Keep it usable on narrow
  screens and beneath the sidebar backdrop while the sidebar is open.
- Add a Drafts section near the top of the existing sidebar. List the five
  newest drafts first, then offer an expand control for the rest. Match the
  backend's permissions: writers see only their own drafts, admins see all,
  and readers see no Drafts section. Each title links to the existing
  read-only `/posts/{id}` page.
- Load the sidebar's draft data through the database for the authenticated
  account; do not add an internal HTTP call or a new draft API.

## Creation page

- Serve `/posts/new` only to authenticated `write` and `admin` accounts,
  within the site's existing dark layout and navigation. Render a dedicated
  form with a required title, optional summary, tags, and text blocks.
- Let the user add, remove, and move root blocks and one level of child
  blocks. Each block has optional header and body fields. Preserve the shown
  sibling order when building the JSON body for `POST /api/posts`; do not
  expose media fields or deeper nesting.
- Use **Create draft** as the submit label. State near the action that saved
  drafts can currently be viewed but cannot yet be edited or published.
  Validate a nonblank title before submission, and prevent repeated submits
  while the request is in flight.
- On success, navigate to the new draft's `/posts/{id}` page and show a brief
  one-time creation confirmation. On failure, keep all entered fields and
  blocks in place and show a clear error so the user can retry.

## Verification

- Check button, sidebar, and page visibility and access for `read`, `write`,
  `admin`, unauthenticated users, and guest-share visitors.
- Check the sidebar's five-item expansion and that a new draft appears for
  its author but not in the published feed.
- Check optional metadata, nested block structure and order, moving and
  removing blocks, blank-title feedback, failed submits without data loss,
  and duplicate-submit prevention.
- Confirm successful creation uses the existing API, opens the new read-only
  draft, and shows the creation confirmation.
