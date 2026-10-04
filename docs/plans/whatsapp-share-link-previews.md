# WhatsApp previews for Journey share links

## Summary

Use the existing copied share URL for WhatsApp cards. A request with WhatsApp's
documented User-Agent receives post metadata without creating a guest session.
Ordinary browser requests keep the current cookie-and-redirect flow.

## Implementation

- On a valid WhatsApp request to `/share/<id>/<secret>`, return HTML whose
  `<head>` contains absolute Open Graph URLs, the post title, and its summary
  within the first 300 KB. Use the first media item in post order for
  `og:image`. Omit `og:image` for text-only posts. Do not issue a guest cookie
  for this request.
- Issue a random, single-image capability at
  `/share/<id>/whatsapp-preview-image/<random-name>.jpg`. Store its digest,
  selected media block, share link ID, and expiry in SQLite. It expires after
  five minutes or when the share link expires, whichever comes first. Every
  image request also checks that the link remains published and unrevoked.
  Return `404` when any check fails; no cookie or share secret is needed at
  the image URL.
- Fetch the JPEG from the object store through its separately implemented
  image-reduction header interface. That prerequisite must supply a JPEG below
  600 KB, at least 300 pixels wide, with an aspect ratio no wider than 4:1 for
  photos and video thumbnails. This plan makes no object-store changes. The
  site streams the result and rejects an oversized response.
- Match the documented `WhatsApp/2.x.x.x A|I|N` User-Agent form for preview
  behavior. Treat it as a presentation signal only: the share secret
  authorizes token issuance, and the random token authorizes the image. Keep
  the current browser redirect. Include an "Open post" fallback and
  browser-side navigation in the preview HTML so a WhatsApp in-app browser can
  still reach the normal guest-session flow.
- Set `Cache-Control: no-store` on preview HTML and image responses. Increment
  the development database schema version for the temporary-capability table;
  follow the repository's rebuild procedure rather than adding a legacy
  migration.

## Verification

- Test WhatsApp and browser requests against the same copied URL; invalid,
  expired, and revoked links; title and summary escaping; first-photo,
  first-video, and text-only posts; absolute image URL and response limits;
  image requests without cookies; five-minute expiry; and revocation before
  expiry.
- Manually test both preview creation and opening the link in WhatsApp once
  the storage image-reduction prerequisite is available.

## Assumptions

- The five-minute URL protects future image fetches; a card already sent
  through WhatsApp may remain visible after expiry or revocation.
- The object-store image-reduction work is completed separately before this
  plan's implementation.
