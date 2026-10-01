# Journey High-Level Roadmap

**Status:** Active planning
**Created:** 23 September 2026
**Current position:** The transport, filesystem-backed object store, SQLite
post backend, scrolling feed, sidebar, and account sign-in UI are implemented
locally. The authoring backend is next; storage diagnostics and administration
remain deferred.

## 1. Direction

The AWS-to-home prototype demonstrated that Journey can carry concurrent,
bounded HTTP/2 streams through one home-initiated WebSocket connection on a
real deployment. Transport feasibility is no longer the project's main open
question.

The project moves toward the smallest useful Journey application in two
major milestones:

1. A real object-storage service for the home server (the storage crate is
   implemented; integration into the home application remains future work).
2. A basic server-rendered HTML website on AWS, delivered through Nginx over
   HTTPS with HTTP/2.

The storage interface and filesystem-backed implementation are documented in
the [current storage design](design/current-object-storage.md). Earlier
evaluation of existing storage software preceded the decision to build this
store.

## 2. Transport boundary

`journey-websocket` remains in this repository for now. Moving it to a separate
Git repository is not a prerequisite for the next milestone.

It must remain independently extractable:

- It may depend on third-party crates.
- It must not depend on another crate or application in this repository.
- Journey applications may depend on it; the dependency direction must never
  reverse.
- It must remain free of Journey object routes, storage rules, website policy,
  caching, and database concerns.
- Its existing tests must continue to prove the transport without requiring
  the Journey applications.

This keeps later extraction mechanical without delaying product work now.

## 3. Milestone 1: home object-storage service

The [current object storage design](design/current-object-storage.md) describes
the implemented interface, HTTP/2 routes, self-describing object files, and
startup-built in-memory index. SQLite is not used. The
[storage web interface](design/storage-web-interface.md) provides authenticated
object management. Integrity verification, manifest export, and diagnostic
web views remain [deferred](deferred/storage-diagnostics-and-administration.md).
The older
[storage design](design/home-object-storage-design.md) and
[interface design](design/home-object-storage-interface.md) remain historical context.

### 3.1 Define the interface first

Write an implementation-neutral object-storage interface document before
selecting a backend or designing filesystem layouts and wire routes.

At a high level, the interface must account for:

- Client-supplied opaque object keys.
- Clients optionally using a SHA-256 digest as a key without the storage
  service generating or requiring that digest.
- One logical object store organized through key prefixes.
- Streaming object creation and retrieval.
- Object metadata lookup.
- Byte-range reads.
- Prefix-filtered, paginated listing.
- Object deletion.
- Basic metadata such as size, content type, checksum or ETag, creation time,
  and caller-provided values.
- Health and capability discovery.
- Clear unavailable, not-found, invalid-request, cancellation, and storage
  failure behavior.

The interface phase, not this roadmap, must decide:

- Existing-key and replacement semantics.
- Conditional creation and conditional updates.
- Whether metadata can change independently of object bytes.
- Exact listing and pagination semantics.
- Consistency guarantees.
- Deletion guarantees.
- Quotas and admission limits.
- Upload retry and resumability behavior.
- The final wire representation and HTTP routes.

The purpose of this phase is to describe what Journey needs without allowing
the first proposed implementation or an existing product's API to dictate the
requirements.

#### Deliverable

An approved conceptual interface document containing:

- The operation set.
- Inputs and outputs at the semantic level.
- Streaming and cancellation expectations.
- Metadata ownership.
- Error categories.
- A decision table for the unresolved semantics above.

It must not commit Journey to S3, a filesystem layout, a particular Rust
library, or a separate storage server.

### 3.2 Research buy versus build

After the conceptual interface exists, compare existing storage solutions and
reusable libraries against it.

The initial candidate set is:

- A small custom filesystem-backed Journey service.
- Apache OpenDAL with a filesystem or S3-compatible backend.
- Garage behind a localhost-only Journey home agent.
- SeaweedFS behind a localhost-only Journey home agent.
- MinIO Community, including its current build and licensing implications.
- Any additional credible lightweight, single-server candidate discovered
  during the investigation.

Garage is aimed at lightweight, self-hosted, S3-compatible storage for small
to medium deployments. SeaweedFS offers a wider S3-compatible management
surface but introduces more storage-system components. OpenDAL is a Rust
library and abstraction layer rather than a storage server. MinIO remains a
relevant S3-compatible comparison, but its current community distribution and
AGPL obligations must be assessed explicitly.

Primary references:

- [Garage overview](https://github.com/deuxfleurs-org/garage/blob/main-v2/README.md)
- [Garage S3 compatibility](https://github.com/deuxfleurs-org/garage/blob/main-v2/doc/book/reference-manual/s3-compatibility.md)
- [SeaweedFS overview](https://github.com/seaweedfs/seaweedfs/blob/master/README.md)
- [Apache OpenDAL services](https://opendal.apache.org/docs/rust/opendal/services/)
- [MinIO community chart and distribution notes](https://charts.min.io/)

Evaluate each candidate for:

- Fit with the approved Journey interface.
- Single-server setup and maintenance effort.
- Idle memory and memory during large streaming transfers.
- Container and operational complexity.
- Range-read and streaming behavior.
- Listing, deletion, and metadata support.
- Data inspectability and the ability to export or migrate all objects.
- Crash and failure recovery.
- License, distribution, and source-availability constraints.
- Rust client or library quality.
- Upgrade stability.
- Ability to replace the backend without changing AWS callers.
- Future backup or replication options without requiring either initially.

Do not expose a complete third-party S3 server through the WebSocket merely
because it already exists. If Journey selects a separate storage product, it
should normally remain reachable only from the home agent over loopback or a
private Docker network:

```text
AWS gateway
    |
    | WSS with inner HTTP/2
    v
Journey home agent
    |
    | loopback or private Docker network
    v
selected object-storage service
```

The home agent remains the narrow remote security and policy boundary.

Paper comparison should reduce the candidate set before proof-of-fit work.
Small experiments may then measure only the remaining candidates against the
same interface scenarios and representative files.

#### Deliverable

A decision record selecting one of:

- An existing storage service.
- An embedded storage abstraction and backend.
- A custom Journey implementation.

The decision record must explain why rejected candidates do not suit the first
single-server deployment.

### 3.3 Implement the selected service

Implementation begins only after the interface and backend decision are
approved.

At a high level:

- Put Journey-specific object behavior in a dedicated workspace crate or
  service, separate from `journey-websocket`.
- Keep the home agent as the only remotely reachable storage boundary.
- Map the approved interface onto the selected backend.
- Preserve bounded streaming, byte ranges, cancellation, and concurrent
  requests.
- Avoid collecting complete objects in memory.
- Add backend-independent conformance tests so a future backend can implement
  the same Journey interface.
- Validate the service on one home server.

The first deployment deliberately uses one storage server with no replication
and no backup. This keeps the first implementation small; it does not imply
that the data is sufficiently protected for irreplaceable production media.

#### Milestone completion

Milestone 1 is complete when:

- The conceptual interface is approved.
- The buy-versus-build decision is recorded.
- The selected backend passes the common conformance suite.
- Management, streaming, listing, range, metadata, and deletion operations
  required by the approved interface work through the home connection.
- Memory remains bounded for objects larger than available RAM.
- Cancellation and storage-unavailable behavior are predictable.
- No object-storage behavior has leaked into `journey-websocket`.

## 4. Original milestone 2: basic HTML website over HTTP/2

This section records the original fixed-page prototype scope. The current
website is an authenticated post feed with a sidebar and account sign-in;
[its implemented design](design/website-post-backend.md) supersedes the
fixed-page behavior below. Posts and their media are not anonymously
accessible.

### 4.1 Website boundary

- Nginx terminates HTTPS and negotiates HTTP/2 with browsers.
- A Rust application renders and returns HTML.
- Nginx proxies to Rust over the private AWS container network.
- The Rust application does not need to implement the browser-facing HTTP/2
  connection itself.
- The page uses objects retrieved through the home object-storage service.
- AWS streams media from home without a persistent media cache initially.

HTTP/2 allows the page, image, and video-range requests to share a multiplexed
browser connection without HTTP/1.1 request serialization. It does not remove
TCP-level head-of-line blocking, so correct byte ranges, cancellation, and
bounded read-ahead remain necessary.

### 4.2 First page

The first website contains one fixed server-rendered demonstration page:

- Normal semantic HTML.
- One image stored in the home object service.
- One browser-playable, web-optimized MP4 stored in the home object service.
- A native video player with seeking.
- Minimal CSS where useful.

This milestone does not introduce:

- React.
- A Node.js runtime.
- SQLite.
- Posts or categories.
- An editor or administration interface.
- User-facing uploads.
- An AWS media cache.

### 4.3 Initial public interface

The exact framework API is decided during implementation, but the public
behavior is limited to:

- `GET /` to render the fixed HTML page.
- `GET` and `HEAD` media requests using an opaque object key.
- One validated byte range for video playback and seeking.
- Preservation of content type, content length, range metadata, and cache
  validators supplied by the object service.
- Bounded not-found and home-unavailable responses.

The website must not expose general object listing, deletion, or management
operations to public browsers.

### 4.4 Acceptance checks

- Nginx negotiates HTTP/2 with a browser.
- The Rust application renders the HTML page.
- The image displays with bytes matching the stored object.
- The MP4 starts without downloading the entire file first.
- Seeking near the beginning, middle, and end produces correct ranges.
- A throttled video does not prevent the image or HTML resources from
  completing.
- Cancelling playback does not close the shared home connection or unrelated
  streams.
- AWS memory does not scale with object size.
- Home unavailability returns a prompt bounded error rather than leaving the
  page request hanging.
- Restoring the home connection restores media without restarting the public
  web service.

## 5. Next website work

The local site now has a SQLite-backed post backend, a server-rendered scrolling
feed, a sidebar, and `read`, `write`, and `admin` sessions with a browser
sign-in form. Every post and media request requires a session. Writers can
create, edit, publish, and upload media to their drafts; admins can manage any
draft. Draft galleries use media child rows backed by shared content-addressed
assets. The next work is ordered as follows:

1. **Video thumbnail previews.** Generate or store preview images and show
   them on gallery tiles; videos remain playable without previews meanwhile.
2. **Automatic gallery slideshow.** Add optional timed advance to the existing
   manual slideshow, with pause controls and sensible video behavior.

Temporary links granting access to one published post remain future work;
they must not grant draft access.

## 6. Later horizons

- A bounded AWS media cache and duplicate-request coalescing.
- Strong home-client challenge authentication and key rotation.
- Home storage backup and restore.
- [Image variants and optional media processing](deferred/website-image-variants.md).
- Operational hardening, observability, and recovery testing.

## 7. Current assumptions

- The successful real AWS prototype closes the transport feasibility question.
- `journey-websocket` remains isolated but in this repository for now.
- Object keys are opaque; clients may supply a key or request a generated name
  within a chosen prefix.
- SHA-256 keys are an application convention, not a storage requirement.
- The first interface uses one logical store with prefix-based organization.
- Basic immutable object metadata is in scope for interface discussion.
- The first deployment is a single home storage server without replication or
  backup.
- The filesystem-backed object store and its write semantics are documented in
  the current object-storage design.
- The website uses server-rendered HTML and small browser scripts.
- Every post and media request requires a session. `read` accounts see
  published posts; `write` accounts can access drafts they authored and admins
  can access every draft. Writers can edit their own drafts, upload media, and
  publish them; admins can edit any draft. Published posts remain read-only.
- Visible draft blocks contain text and ordered media galleries. Gallery
  placements are child rows backed by shared original-file metadata.
- The local website streams original media from the storage service and has no
  AWS media cache.
