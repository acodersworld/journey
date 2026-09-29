# Ephemeral post share links

Share links provide guest access to one published post and its media. The CLI
creates, lists, and revokes links; owner-facing controls are deferred.

## Stored credentials

`site_settings.share_link_lifetime_seconds` stores the global link lifetime and
defaults to 86,400 seconds. Link creation copies that duration into an absolute
expiry, so later setting changes affect only new links.

`share_links` stores a random public ID, post ID, SHA-256 digest of the link
secret, creation and expiry times, and an optional revocation time. The raw
secret is shown only by the create command. `share_sessions` stores a digest of
an independently generated session token, its link ID, creation time, and an
expiry no later than the link's fixed expiry. Routine database operations
remove expired share sessions.

## Guest access

The CLI link URL is `/share/{link-id}/{secret}`. A successful open verifies
that the secret matches an unexpired, unrevoked link to a published post,
creates a guest session, sets a link-scoped `HttpOnly; SameSite=Lax` cookie,
and redirects to `/share/{link-id}/posts/{post-id}`. Guest post and media
requests revalidate the session and link, publication state, and media block's
post ownership. Revocation therefore ends existing guest sessions on their
next request. Guest pages use the regular feed container and post renderer for
current content, without the account sidebar or controls. They grant no account
privileges.

Media remains available through guest-scoped routes and uses the same GET,
HEAD, and video range handling as account routes. Cookies have `Secure` by
default, including account cookies. `journey-site serve --allow-insecure-cookies`
disables `Secure` only when the listener binds to loopback.

The `journey-site share-links` commands manage the SQLite lifetime and links.
Link listing contains public IDs, post IDs, expiry as epoch seconds and an ISO
8601 UTC date-time, and revocation status; it never returns link secrets.
