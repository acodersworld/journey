# Website authentication UI

## Summary

Add a dark-themed sign-in page and top-right account controls. Signing in
returns the reader to the requested page. The HTML form works without
JavaScript.

## Implementation changes

- Make `GET /login`, the site CSS, and the site JavaScript available without a
  session. Render a server-side form with username and password fields. On
  failure, show the backend's generic error, keep the username, and clear the
  password.
- Add form handlers for `POST /login` and `POST /logout`. Reuse the existing
  credential, session, origin-check, and cookie logic rather than duplicating
  authentication rules. Successful login redirects to the requested page;
  logout revokes the session and returns to the login page.
- Redirect unauthenticated **HTML page** requests to `/login` with a return
  target. Accept return targets only for local site content paths; otherwise
  use `/`. This prevents the open-redirect risk described by
  [OWASP](https://cheatsheetseries.owasp.org/cheatsheets/Unvalidated_Redirects_and_Forwards_Cheat_Sheet.html).
  Keep API, fragment, and media requests as `401` responses.
- Add the signed-in username and a Sign out button at the top right of every
  content page. Match the existing dark theme and narrow-screen layout. If an
  incremental feed request receives `401` after a session expires, send the
  reader to sign-in with the current page as the return target.

## Verification

Check direct-link sign-in and return, invalid return targets, wrong
credentials, logout, expired sessions, owner draft links, keyboard form use,
and narrow screens. Confirm login assets load anonymously, while APIs and
media remain protected and reader access to drafts still returns `404`.

## Assumptions

The login page uses the existing Journey name and tagline. There is no
registration, password-reset, account-management, or draft-list UI in this
phase. An already signed-in visitor opening `/login` goes to a valid return
target or the home feed.
