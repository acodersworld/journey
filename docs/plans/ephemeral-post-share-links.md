# Ephemeral post share links

## Summary

Add reusable, revocable links that let a guest view one published post and its
media. Multiple links may exist for the same post. Link lifetime is a global
SQLite setting with a 24-hour default; changing it affects new links only.
This first version manages links through the CLI. Owner-facing controls are
deferred.

## Link and session data

- Add a `share_links` table containing a random public ID, post ID, SHA-256
  digest of the secret token, creation time, fixed expiry, and optional
  revocation time. The post ID references `posts`.
- Generate each link secret from 32 bytes of OS randomness and encode it as
  43 URL-safe Base64 characters without padding. Show the raw secret only when
  the link is created; never store or list it.
- Add a `share_sessions` table containing a digest of an independently
  generated 32-byte session token, the share-link ID, creation time, and
  expiry no later than the link expiry. The session references the link rather
  than storing a list of media permissions.
- Store the global link lifetime in SQLite. Default to 86,400 seconds. On link
  creation, calculate and store its absolute expiry; a later setting change
  does not change existing links.

## CLI and HTTP behavior

- Add CLI commands to set the global lifetime and create, list, and revoke
  links. Creation accepts a published post ID and prints the full URL once.
  Listing shows link IDs, post IDs, expiry, and revocation status without
  revealing secrets. Revocation targets one link, so other links to the same
  post remain active.
- Use a link-specific public path. Opening its URL verifies the secret,
  expiry, revocation, and published post, creates a guest session, sets a
  link-scoped cookie, then redirects to a clean path without the secret.
  Keep the guest post and media routes beneath that path so cookies for
  different links can coexist in one browser.
- The guest cookie is `HttpOnly` and `SameSite=Lax`, with an expiry capped at
  the link's fixed expiry. Set `Secure` on both guest and existing account
  cookies by default. Add `journey-site serve --allow-insecure-cookies` for
  local HTTP development, and reject that flag unless the site binds to
  loopback. Do not use an environment variable for this override.
- On every guest post or media request, validate the session and its link,
  including expiry and revocation. Require the post to remain published and
  each requested media block to belong to that post. Serve current post
  content without the normal sidebar or account controls. Guest sessions
  confer no account privileges.
- Respond consistently to invalid, expired, and revoked links. Support
  existing media `GET`, `HEAD`, and range behavior through guest routes.
  Remove expired guest sessions during routine database operations.

## Verification

- Create multiple links for one post; revoke one and confirm the others work.
- Open links for different posts in one browser and confirm their cookies and
  access do not interfere.
- Check expiry, immediate revocation of existing guest sessions, unpublished
  posts, and attempts to request another post's media.
- Exercise guest HTML, image and video requests, including `HEAD` and ranges.
- Check `Secure` on both cookie types by default and verify that only the
  loopback-only command-line flag permits cookies without `Secure`.
