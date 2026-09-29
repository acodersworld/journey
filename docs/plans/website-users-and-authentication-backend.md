# Website users and authentication: backend phase

## Summary

Add username and password accounts before building the sign-in UI. Every
existing post, feed, sidebar, archive, tag, and media route will require a
session. Readers can view published posts; only the owner can view drafts. A
later temporary link may grant access to one published post, but link issuance
is outside this phase.

## Implementation changes

- Add SQLite `users` and `sessions` tables without changing existing posts.
  Support one owner and multiple readers. Add CLI commands to create accounts,
  list them, change passwords, and disable or enable readers. Read passwords
  interactively without echoing them or placing them in command arguments.
  Password changes and account disabling revoke sessions.
- Add JSON endpoints for login, current account, and logout. Hash passwords
  with Argon2id and issue random, server-stored session tokens; store only
  token digests in SQLite. Sessions expire after **seven days by default**,
  with a configurable lifetime. Use generic login failures and bounded login
  throttling. These choices follow
  [OWASP password storage](https://cheatsheetseries.owasp.org/cheatsheets/Password_Storage_Cheat_Sheet.html)
  and [session guidance](https://cheatsheetseries.owasp.org/cheatsheets/Session_Management_Cheat_Sheet.html).
- Set host-only, `HttpOnly`, `SameSite=Strict` cookies; use `Secure` under HTTPS.
  Configure the site's public origin explicitly for deployment, while allowing
  loopback HTTP for local development. Check the request origin on
  state-changing authentication endpoints; do not rely on `SameSite` alone for
  CSRF protection. See
  [OWASP CSRF guidance](https://cheatsheetseries.owasp.org/cheatsheets/Cross-Site_Request_Forgery_Prevention_Cheat_Sheet.html).
- Require authentication on every existing content route, including `HEAD`
  and ranged media requests. Anonymous requests return `401` during this
  backend phase. Keep feeds, tags, archives, and sidebar lists limited to
  published posts. Direct post, fragment, API, and media URLs may return drafts
  to the owner; readers receive `404` for drafts. Mark authenticated content
  responses `Cache-Control: private, no-store`.
- Keep authorization in a shared post-access check so a future post-scoped
  temporary-link credential can read **published** posts and their media
  without gaining account or draft access. Do not create temporary-link tokens
  or authoring endpoints now.

## Verification

Test schema migration, CLI provisioning and password changes, login failure
and throttling, session expiry and revocation, cookie attributes, and origin
checks. Exercise every content route as anonymous, reader, and owner,
including draft media `GET`, `HEAD`, and range requests. Confirm imports leave
users and sessions intact.

## Assumptions

Usernames identify accounts; there is no email or self-registration. Sessions
have an absolute expiry rather than renewing on activity. The browser sign-in
and account UI will be planned after this backend phase; until then,
authenticated access can be tested with an HTTP client.
