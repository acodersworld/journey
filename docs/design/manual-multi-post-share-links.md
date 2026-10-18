# Manual multi-post share links

Share links use one URL and one link-scoped guest session for either a single
post or a fixed set of posts. `share_links` stores kind, optional title,
creator, secret digest, expiry, and revocation. `share_link_posts` stores member
IDs in publication-time-descending, post-ID-descending order. Membership and
order are fixed at creation. Deleted members cascade from membership without
deleting a multi-post link.

The version 10 to 11 migration atomically replaces only `share_links` and
`share_sessions`, then creates the new share tables. This migration deliberately
invalidates pre-upgrade share URLs and guest sessions. It preserves all other
tables and settings. A reopened version 11 database keeps its links. Backup
restoration runs the same migration on the staged restored database before
validation and publishing the restored file.

`POST /api/posts/{id}/share-links` accepts an optional JSON body with
`additional_post_ids` and `title`. No body and an empty additional list keep
single-post behavior. Each member must be published and shareable by the
caller when the link is created. A multi-post link has at most 100 distinct
members and defaults to the title “Shared posts”. Creators with write access
may revoke their own multi-post links; admins may revoke any link.

Opening `/share/{id}/{secret}` creates a link-scoped cookie. Single-post links
redirect to their existing guest page. Multi-post links redirect to
`/share/{id}`. Feed, fragment, post, and media requests check the guest
session, expiry, revocation, publication state, and frozen membership. Feed
pagination uses membership positions, so removed posts do not change the
order or cause remaining posts to be skipped. Guest pages reuse mosaics, the
media scroller, and private no-store responses.
