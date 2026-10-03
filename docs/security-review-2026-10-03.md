# Security review — 2026-10-03

This review covers the site, storage service, WebSocket transport, and repository deployment files. The deployed home storage service connects to the public Nginx endpoint over WSS; Nginx forwards to the site container over HTTP. That protects the Internet hop. The repository's `ws://` deployment files are explicitly a LAN example. The actual HTTPS Nginx configuration was not available for review.

## Findings, in priority order

### 1. High: shared proxy address can block everyone's login

The site counts login attempts by the TCP peer address, which is Nginx for proxied requests. After 30 attempts in 15 minutes, that shared address is throttled. Five attempts against a known username also lock out that account. See [site login throttling](../apps/site/src/web.rs#L584) and [site listener setup](../apps/site/src/main.rs#L388).

Use a client address supplied and verified through a trusted proxy boundary, then adjust the account throttle so outsiders cannot repeatedly lock out a user. Never trust a client supplied `X-Forwarded-For` directly. [Nginx documents the trusted proxy mechanism](https://nginx.org/en/docs/http/ngx_http_realip_module.html).

### 2. High impact after site compromise: the tunnel gives the public server full control of home storage

Once the WebSocket is authenticated, the inner service accepts listing, arbitrary object writes, and deletion; writes can be unconditional. See [storage connection setup](../apps/storage/src/main.rs#L91), [storage routes](../crates/journey-storage/src/http2_storage_service.rs#L393), and [unconditional write handling](../crates/journey-storage/src/http2_storage_service.rs#L250).

A compromise of the site process or tunnel secret can therefore alter or erase the home object volume despite container isolation. Give the tunnel only the operations the site uses: reads and content addressed, create only media uploads. Keep management and destructive operations on a separate local interface.

### 3. Medium: public requests can force unnecessary SQLite writes and blocking work

Every database operation opens a connection in a blocking task and deletes expired share sessions before doing its actual query. Even a request carrying a correctly shaped but invalid session cookie reaches this path. See [database execution](../apps/site/src/db.rs#L1432) and [cookie parsing](../apps/site/src/web.rs#L733).

Under request floods, this creates write contention and can exhaust the blocking pool. Move expiry cleanup to a periodic task and bound database concurrency.

### 4. Medium: the home management UI exposes Basic credentials over HTTP to the LAN

The default bind is `0.0.0.0:8082`, and the Compose example publishes it on the home LAN. See [storage defaults](../apps/storage/src/config.rs#L62), [storage Compose configuration](../deploy/storage-compose.yml#L7), and [Basic authentication handling](../crates/journey-storage/src/storage_web_interface.rs#L165).

Basic credentials are reversibly encoded, so an active LAN attacker could capture them. Bind this UI to loopback and access it through a VPN or SSH tunnel, or put TLS in front of it. [HTTP authentication guidance](https://developer.mozilla.org/en-US/docs/Web/HTTP/Guides/Authentication) explains the transport requirement.

### 5. Medium: share-link secrets enter access logs, and opening a link creates unlimited sessions

The bearer secret is in `/share/{id}/{secret}` ([share route](../apps/site/src/web.rs#L249)). The supplied Nginx configuration enables access logging without a custom format ([Nginx configuration](../deploy/nginx.conf#L8)); Nginx's default combined format includes the request URL ([Nginx logging documentation](https://nginx.org/en/docs/http/ngx_http_log_module.html)). Each successful opening inserts a new database session without a per-link cap ([share session creation](../apps/site/src/db.rs#L956)).

Redact or suppress this route in every proxy's logs, and reuse or cap share sessions.

## Deployment checks

Ensure production sets `site.public_origin` to its HTTPS origin and leaves `allow_insecure_lan_http` disabled. That flag controls whether session cookies receive `Secure`, independently of the configured origin ([cookie security setting](../apps/site/src/web.rs#L189)). The repository's [LAN example](../deploy/site.toml.example#L7) enables it.

Check the actual TLS proxy for a restrictive Content Security Policy and framing policy; neither appears in the supplied app and Nginx configuration. CSP is an additional browser safeguard, not a substitute for the escaping already present. [OWASP CSP guidance](https://cheatsheetseries.owasp.org/cheatsheets/Content_Security_Policy_Cheat_Sheet.html).

## Existing protections and scope

The code has useful protections: Argon2id password hashing, random session tokens, exact Origin checks on state changing site routes, parameterized SQL, media type restrictions, and non-root containers. No clear authentication bypass or SQL injection was found in the reviewed paths.

This was a static review. No live security tests or dependency audit were run.
