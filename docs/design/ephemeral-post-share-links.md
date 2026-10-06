# Ephemeral post share links

Share links provide guest access to one published post and its media. A `write`
account can create and revoke links for its own published posts; an `admin` can
manage links for any published post. The CLI also creates, lists, and revokes
links.

## Stored credentials

`site_settings.share_link_lifetime_seconds` stores the global link lifetime and
defaults to 86,400 seconds. Link creation copies that duration into an absolute
expiry, so later setting changes affect only new links.

`share_links` stores a random public ID, post ID, SHA-256 digest of the link
secret, creation and expiry times, and an optional revocation time. The raw
secret is shown only by the create command. `share_sessions` stores a digest of
an independently generated session token, its link ID, creation time, and an
expiry no later than the link's fixed expiry. Routine database operations
remove expired share sessions and WhatsApp preview-image capabilities.

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

## Account sharing controls

The share panel appears only for write accounts viewing their own published
posts and for admins viewing any published post. Opening the panel loads an
authenticated preview that checks the same author or admin permission and
uses the guest post renderer with regular authenticated media routes; it does
not create a share link or a guest session.

The panel creates a link only after an explicit copy action. Its URL and ID
remain in client memory while the panel is open, and the URL is never shown as
page text. Repeated copies reuse that link. Closing the panel or reloading the
page discards it, so the next panel session creates a new link. Revocation
applies to the current panel link; older links remain available through the
CLI. Create and revoke endpoints require the post's author with `write`
permission or an `admin` session, plus a validated same-site request origin.
They return private no-store responses.

## WhatsApp card previews

The copied `/share/{link-id}/{secret}` URL also serves WhatsApp user agents of
the form `WhatsApp/2.x.x.x`, optionally followed by the `A`, `I`, or `N` client
marker. A valid crawler request receives a small HTML page with escaped title
and summary metadata and absolute `og:url` and, when media exists, `og:image`
URLs. The first image or video thumbnail in post order is used; text-only posts
omit `og:image`. This request does not create a guest session or set a cookie.
Other user agents keep the normal session-cookie and redirect flow.

The image URL is `/share/{link-id}/whatsapp-preview-image/{random-name}.jpg`.
SQLite stores only its SHA-256 digest, share link ID, selected media block, and
expiry in `share_preview_images`. The URL expires after five minutes or at the
share link's expiry, whichever comes first. Each fetch rechecks that the image
capability and share link are live and that the post is still published and
unrevoked. Image requests need no cookie or share secret.

The site asks storage for a JPEG with a 1,200-pixel maximum edge and a
599,999-byte limit, preserving the source aspect ratio without padding. It
uses its reduced-image representation for photos and reduced thumbnail
representation for videos. It checks the response type and declared length,
enforces the size limit while streaming, and applies private no-store caching
to both preview and image responses. The preview page includes an “Open post”
link and navigates browsers to the same share URL with `?open=1`; that flag
selects the regular guest-session flow even in a WhatsApp in-app browser.
WhatsApp may retain a card after its image URL expires or the share link is
revoked.

The capability table is part of site schema version 9. Startup rejects older
databases with the existing rebuild instruction; development databases must
be recreated and the destructive importer run again.
