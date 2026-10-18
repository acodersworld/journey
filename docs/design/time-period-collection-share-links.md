# Time-period collection share links

`POST /api/automation/collection-share-links` creates a share link for published
posts from all authors in a UTC period. `from` is inclusive and `until` is
exclusive. Both must be RFC3339 UTC strings ending in `Z`, with whole seconds
or one to three fractional digits. The title is required and limited to 200
characters after trimming. The endpoint uses HTTP Basic authentication over
HTTPS with a dedicated automation account; browser sessions and Origin headers
are not required. Configure `site.public_origin` so generated URLs use the
public HTTPS origin. For local development over HTTP, start `serve` with
`--allow-insecure-lan-http` or set `site.allow_insecure_lan_http = true` in the
site config. This existing override also permits HTTP for the automation
endpoint; keep it disabled for public deployments.

Provision a separate credential on the site host with
`journey-site --config <path> automation-accounts create <username>`. The CLI
generates a 64-character password from 48 cryptographically random bytes and
prints it once after the account is created. `password <username>` rotates the
password and prints the replacement once. `list`, `disable <username>`, and
`enable <username>` manage these accounts without showing passwords. Only an
Argon2id hash is stored in `automation_accounts`; the plaintext cannot be
retrieved later. These accounts are separate from website users and cannot
sign in to the site. Copy the password into the external script's secret store
when it is printed.

Example request:

```sh
curl --fail-with-body --user "$AUTOMATION_USERNAME:$AUTOMATION_PASSWORD" \
  --header 'Content-Type: application/json' \
  --data '{"title":"Posts for the week","from":"2026-10-05T00:00:00.000Z","until":"2026-10-12T00:00:00.000Z"}' \
  https://example.com/api/automation/collection-share-links
```

For matches, HTTP 201 returns `id`, an absolute secret-bearing `url`,
`post_count`, `expires_at_unix`, and `truncated`. For no matches, HTTP 200
returns zero count, false `truncated`, and null link fields. For example:

```json
{"id":"example-link-id","url":"https://example.com/share/example-link-id/example-secret","post_count":2,"expires_at_unix":1791830400,"truncated":false}
```

The endpoint selects at most 101 matches and creates the link and membership
in one immediate SQLite transaction. It retains the newest 100 in publication
time descending, then post ID descending order. The 101st match sets
`truncated` to true. Membership and order are frozen. A single match uses the
existing single-post guest view; multiple matches use the existing guest feed.
The link lifetime follows the global `share-links lifetime` setting. Admins
manage these links through the existing `share-links list` and `revoke` CLI
commands. Each request creates an independent link; the calling script handles
retries and delivery.

Schema version 12 adds only `automation_accounts`. Upgrades from version 11
preserve posts, existing share links and sessions, website sessions, and
settings. Version 10 retains the existing migration chain. Backup restoration
upgrades a staged database before publishing it. No importer or database
recreation is needed.
