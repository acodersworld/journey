# Author-owned draft creation backend

## Summary

Add text-only draft creation with a required author. Replace the current
`reader` and `owner` roles with `read` and `write`, and add `admin`. The
manifest importer continues to replace the entire post set. This is a backend
step: editing, deletion, publishing, media upload, and the authoring UI are
later work.

## Post data and HTTP behavior

- Add a required `posts.author_id` reference to `users`. The author of an
  HTTP-created draft is always its signed-in creator; the request cannot
  choose another author.
- Allow `published_at` to be null while a post is unpublished. A future
  publish operation will set it. Existing post responses and draft rendering
  must tolerate the missing date without showing a fake publication date.
- Add `POST /api/posts` for `write` and `admin` accounts. Accept a nonblank
  title, optional summary and tags, and ordered text blocks with at most one
  child level. Permit empty optional fields and an empty block list. Reject
  media fields in this step. Insert the draft and blocks in one transaction;
  return `201 Created`, the new ID, and `Location: /posts/{id}`.
- Add `GET /api/drafts`. A `write` account sees its own drafts; an `admin`
  sees all drafts. Order by newest post ID. A `read` account cannot use this
  endpoint. The published feed remains published-only.
- Apply author-aware access to existing post and media reads: `read` sees
  published posts, `write` also sees its own drafts, and `admin` sees every
  draft. `write` may create or revoke share links for its own published posts;
  `admin` may do so for any published post. Keep guest share links limited to
  published posts. No new authoring UI is added.
- The later edit/delete rule is: the post's author with `write` permission,
  or an `admin`, may edit or delete it. Being credited as author does not give
  a `read` account write permission.

## Roles, import, and development data

- Accept `read`, `write`, and `admin` in account creation, authentication,
  and manifest import. Remove the single-owner restriction so admin accounts
  are not limited to one. Align current role-dependent access and share-link
  controls with the new roles.
- Require an `author` username on every manifest post. Resolve it to an
  existing or newly imported user in the same import transaction. Keep import
  destructive: it deletes and replaces all posts, including HTTP-created
  drafts.
- Update the example manifest to use `user: write`, `user2: read`, and a new
  `admin: admin` account. Attribute example posts to both `user` and `user2`.
  `user2` can be credited as author but cannot edit while its role is `read`.
- **Standing repository rule:** this development site has no schema migration
  process. Incompatible site database changes require recreating the SQLite
  database and running the destructive importer again. Do not propose or add
  legacy-row migrations solely to keep local development data. Reject an
  outdated database with a clear rebuild message. Record this rule in
  `AGENTS.md` during code implementation so it applies to later work.

## Verification

- Create drafts with the correct author, ordered text blocks, and empty
  optional fields; reject blank titles, media fields, and invalid nesting.
- Verify create, draft-list, post-read, media-read, and share-link permissions
  for `read`, `write`, and `admin`, including drafts owned by another user.
- Import example posts attributed to both users; confirm a later import
  replaces HTTP-created drafts and that an outdated database gives the
  rebuild instruction.
- Verify published feed and guest share access remain limited to published
  posts, and draft responses render without a publication date.
