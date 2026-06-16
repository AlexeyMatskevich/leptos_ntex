# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

### Fixed

- HTML-form server-function **error** responses now keep their error status when
  an unsafe redirect is stripped. The same-origin guard added in 0.6.0
  downgraded a stripped redirect to `200 OK`, but `server_fn` rewrites an
  erroring form post into a `302` back to the (here cross-origin) `Referer` with
  the error encoded in the query — so stripping that `Location` silently turned a
  `500` into a `200`, reporting a failed server function as success. The
  dispatcher now restores `500 Internal Server Error` when the response carries
  `SERVER_FN_ERROR_HEADER`; a genuinely successful form redirect still downgrades
  to `200`.
- A server function reached with a loose `text/html` `Accept` that the strict
  parser refuses (e.g. `Accept: text/html;q=0`) no longer leaks a **cross-origin**
  `Referer` as a redirect `Location`. `server_fn`'s form-redirect fallback fires
  on a loose `contains("text/html")`, so even such a client triggers it and the
  fallback echoes the `Referer`; the same-origin guard strips any cross-origin
  target (and preserves a `500` on the error path rather than promoting it to
  `200`). A **same-origin** form redirect is deliberately left intact, matching
  `leptos_axum` / `leptos_actix`: the fallback is driven solely by `req.accepts()`,
  which user middleware shares, so suppressing only the fallback at the adapter
  layer cannot be done without corrupting a middleware's own redirect — the real
  fix belongs upstream (tightening `server_fn`'s loose `Accept` check).
- Excluding `/` from a routeless application via
  `generate_route_list_with_exclusions*` no longer leaves an active synthetic
  `GET /`. The fallback `/` route synthesized when an app declares no routes
  bypassed the exclusion filter, so a custom root handler mounted at the excluded
  path was shadowed by Leptos's `GET /`. The synthetic fallback is now built
  first and the exclusion filter is applied unconditionally. The same gap exists
  in the upstream `leptos_axum` / `leptos_actix` adapters.
- Server-function WebSocket **text** frames are now validated as UTF-8
  (RFC 6455 §8.1); an invalid payload fails the connection with close code `1007`
  (`Invalid frame payload data`). ntex's `Frame::Text` exposes raw bytes without
  checking, so invalid UTF-8 previously reached server-function decoding instead
  of being rejected at the WebSocket protocol boundary. Unfragmented frames and
  reassembled fragmented messages are both validated — the full reassembled
  buffer, so a multi-byte character split across fragments is accepted, not
  spuriously rejected.
- The server-function WebSocket bridge no longer puts the reserved close code
  `1006` on the wire. The outbound send-failure teardown built a `Close` frame
  with `CloseCode::Abnormal` (1006), which RFC 6455 §7.4.1 reserves for local
  reporting and forbids in a `Close` control frame; it now sends
  `CloseCode::Error` (1011).

## [0.6.0] - 2026-06-10

### Security

- The initial buffer reserved for collecting a request body is now capped at
  64 KiB. The declared `Content-Length` is client-controlled: previously any
  in-limit declaration was reserved up-front in full, so N parallel
  connections each declaring a large body and then trickling bytes could
  reserve N × limit of memory before sending a single payload byte. The
  buffer still grows on demand as real chunks arrive, so honest large uploads
  pay only amortized reallocation — the payload limit itself is unchanged.
- The HTML-form server-function referrer fallback now enforces its documented
  same-origin policy regardless of the request's `Accept` header. The
  same-origin check was previously gated on the strict `Accept` parser, but
  `server_fn`'s own form-redirect fallback uses a loose
  `contains("text/html")` test and can inject the raw `Referer` (or a rewritten
  error URL) as `Location` for `Accept` shapes the strict parser rejects (e.g.
  `text/html;q=0`). The dispatcher now strips any server-fn `Location` that does
  not resolve to the current origin (downgrading the redirect to `200`), closing
  the cross-origin redirect gap for non-browser callers. Application redirects
  via `redirect()` / `ResponseOptions` are applied afterwards and are
  unaffected.

### Fixed

- `SsrMode::Static` on-demand regeneration now serves the freshly written
  `.html` and its captured status/headers as one paired snapshot, opened and
  read under the same per-file write stripe the writer holds. Previously the
  regeneration branch applied the **request-local** `ResponseOptions` to a
  file that was re-opened separately, so under concurrent first requests to
  an ungenerated path a neighbour's freshly written body could ship under
  this request's headers (e.g. a `x-`/`Link`/CSP header contradicting the
  body it describes). The same race exists in the upstream `leptos_axum` /
  `leptos_actix` regeneration paths and was reported as
  leptos-rs/leptos#4772. **Known trade-off:** if the cached header entry is
  evicted between the write and the paired re-open, the body is served with
  default headers — a consistent pair beats a complete-but-mismatched one.
- Precompressed asset selection (`.br`/`.gz` siblings) now honours
  `Accept-Encoding` q-weights: `Accept-Encoding: gzip;q=1, br;q=0.1` now
  serves the gzip sibling. Previously brotli won whenever it was acceptable
  at all, regardless of the client's declared preference (brotli still wins
  exact q-ties). A malformed q-value — outside the RFC 9110 `0..=1` range,
  or non-numeric/non-finite — is ignored and that encoding's weight falls
  back to `1.0`.
- `SsrMode::Static`/dynamic routes registered with a `*splat` (catch-all)
  segment now match **nested** URLs. `generate_route_list` emitted the
  actix-style `{name:.*}` for splats, which matches only a single segment in
  ntex-router, so nested paths under any wildcard route silently fell through to
  the router fallback (`404`/file handler). Splat segments now use ntex's
  cross-segment tail pattern `{name}*`.
- The server-function WebSocket bridge no longer leaks its bridge task, both
  mpsc channels, and the socket on every closed connection. The outbound bridge
  parked on the server-fn output while holding a clone of the input sender and
  never observed the peer disconnect, so a request/response server fn (one that
  waits for input) and the bridge waited on each other forever — unbounded
  memory growth under connection churn. The bridge now selects on
  `WsSink::on_disconnect()` and releases the sender on disconnect.
- `register_explicit::<T>()` is now idempotent per `(path, method)`: a repeated
  or explicit registration replaces the existing entry instead of appending a
  duplicate, matching the reference `server_fn` registries. Previously, on
  native targets where `inventory` already populated the map, an explicit
  registration was dead (first match won) and `server_fn_paths()` reported
  duplicates.
- Invalid `Location`/`Content-Type` values passed to the server-fn response
  layer are now logged (via `tracing` when enabled, else `eprintln!`) before
  being skipped, instead of silently no-op'ing — and the static-route serve path
  no longer returns an undiagnosable bare `500`/`404` when a `spawn_blocking`
  file task fails.

### Changed

- The declared minimum `ntex` version is now `3.9.6` (was `3.7.2`). The crate
  is only built and tested against `3.9.x`, and `3.7.2` no longer compiles
  against the current semver-compatible `ntex-io`/`ntex-bytes`, so the old lower
  bound advertised support that did not build.

### Performance

- The `site_root` realpath is now resolved once per file-serving handler and
  cached, instead of re-`canonicalize`-ing the whole root on every asset
  request (the per-request target canonicalization that enforces the
  symlink-escape guard is unchanged). Static-route cache hits read the captured
  headers under a shared lock with `peek` (no global exclusive lock per hit) and
  without re-allocating the cache key, and pair the file body with its headers
  under the per-file write stripe so a concurrent regeneration cannot serve a
  body and headers from two different renders.
- Incoming server-function stream chunks and unfragmented WebSocket frames are
  bridged from ntex `Bytes` into `bytes::Bytes` with `from_owner` (zero-copy)
  instead of `copy_from_slice`.

## [0.5.0] - 2026-06-08

### Security

- Bounded the per-process static-route header/status cache (`STATIC_HEADERS`)
  with an LRU (default 1024 entries, override via the
  `LEPTOS_STATIC_HEADERS_CACHE_SIZE` environment variable). A `SsrMode::Static`
  route with a `Param`/`Splat` segment previously inserted a permanent entry
  per distinct path served, so a wildcard static route under high-cardinality
  or adversarial traffic grew the map without bound. Mirrors the bounded
  `lru::LruCache` adopted by `leptos_axum`. **Trade-off:** once an entry is
  evicted, the still-on-disk `.html` is served as a bare `200 OK` with its
  captured custom status/headers dropped and not repopulated (regeneration only
  runs when the file is absent) — large pre-generated sites that exceed the
  cache size should raise `LEPTOS_STATIC_HEADERS_CACHE_SIZE`.

### Added

- `LEPTOS_STATIC_HEADERS_CACHE_SIZE` environment variable to size the
  static-route header cache.

### Fixed

- The file fallback (`file_and_error_handler`) and `site_pkg_dir_service` now
  serve **nested** (multi-segment) paths. ntex's `{name:.*}` route segment
  matches only a single path segment, so the actix-derived `/{tail:.*}` tail
  pattern silently returned `404` for every multi-segment request —
  `/assets/app.css`, wasm-bindgen `pkg/snippets/.../*.js`, and client-routed
  deep links — before the handler ever ran. The crate-internal
  `site_pkg_dir_service` route now uses ntex's cross-segment tail pattern
  `/{tail}*`. **Action required:** update your own catch-all registration from
  `.route("/{tail:.*}", file_and_error_handler(...))` to
  `.route("/{tail}*", ...)`; likewise `handle_server_fns` if any server-function
  endpoint contains a slash.
- The file fallback (`safe_subpath`) and the `SsrMode::Static` path resolver
  (`static_path`) no longer reject the RFC 8615 `/.well-known/` prefix, so ACME
  challenges and `security.txt` can be served. The dotfile guard still hides
  `.env`, `.htaccess`, `..` traversal, NUL bytes, and dotfiles nested *inside*
  `.well-known` (e.g. `/.well-known/.secret`) — only the exact `.well-known`
  segment is exempt, so the security posture for ordinary dotfiles is unchanged.
- The reactive `Owner` for a streaming SSR response is now cleaned up when the
  client disconnects mid-response, not only on full stream drain — the previous
  trailing-stream-item cleanup leaked the owner (and everything it held) on
  early disconnect. Implemented as a local `Drop`-based stopgap over
  `leptos_integration_utils` 0.8.8's `from_app`, with a thread-affinity guard
  (the reactive teardown runs only on the body's origin thread; an off-thread
  drop leaks instead of risking a cross-thread cleanup panic, mirroring the
  `Request` wrapper). Tracks leptos-rs/leptos#4739; removed once upstream ships
  the `Drop`-based cleanup.
- `redirect()` and server-function content negotiation now parse the `Accept`
  header with the `mime` crate instead of a `contains("text/html")` substring
  test, so `text/html;q=0` (explicit refusal) and unrelated ranges that merely
  contain the substring (e.g. `application/x-text/html-fake`) are no longer
  treated as accepting HTML. **Behaviour change:** `Accept: text/html;q=0` now
  takes the non-HTML path. Mirrors the same fix in `leptos_axum` / `leptos_actix`.
- `SsrMode::Static` 404 responses set the `404` status explicitly on the
  rendered-error path rather than relying on the captured `ResponseOptions` to
  carry it (an app-set status still overrides). Defensive — already correct in
  practice.
- The default `Content-Type` header is no longer set via
  `HeaderValue::from_str(..).unwrap()`: a content type that is not a valid
  header value (e.g. an embedded NUL byte) is now skipped with a warning instead
  of panicking the worker. The sole in-crate caller passes a literal, so this is
  defensive, but it removes the foot-gun and mirrors the same hardening in
  `leptos_actix` / `leptos_axum` (leptos-rs/leptos#4755).
- `SsrMode::Static` pages are now written to disk atomically — to a sibling
  temp file, then `rename`d over the target — instead of an in-place `fs::write`
  that truncates first. A crash or a concurrent reader mid-write could otherwise
  observe a truncated or empty `.html`, which the serve path (checking only that
  the file exists, not that it is intact) would then serve. Mirrors the
  atomic-write fix in `leptos_actix` / `leptos_axum`; upstream adds a
  `leptos_integration_utils::write_file_atomic` for this (leptos-rs/leptos#4755),
  not yet in the published 0.8.8, so this ships as a local equivalent. The temp
  file is removed on any failure or panic (an RAII guard), and concurrent
  regenerations of the *same* path are serialized by a fixed, bounded set of
  striped locks — so two concurrent regenerations of a path can't persist a
  mismatched file/header pair, without reintroducing a per-path map that could
  grow without bound.
- An unknown future `SsrMode` variant (the type is `#[non_exhaustive]`) is now
  served as a logged `500 Internal Server Error` instead of being silently
  rendered as out-of-order. **Behaviour change:** a route whose SSR mode this
  integration cannot render now fails loud rather than degrading to the
  out-of-order streamer. No current mode is affected —
  `OutOfOrder`/`InOrder`/`Async`/`PartiallyBlocked`/`Static` all have dedicated
  handling; only a variant added by a future `leptos_router` would hit this.
  Mirrors `leptos_actix` (leptos-rs/leptos#4755).

### Changed

- Refreshed the dependency lockfile within the existing semver ranges (no
  `Cargo.toml` constraint changes). Most notably the ntex runtime stack moves
  `ntex` 3.7.2 → 3.9.6 (with `ntex-bytes`, `ntex-io`, `ntex-h2`, `ntex-net`,
  `ntex-rt`, `ntex-tls`, `ntex-util` bumped in step), alongside patch updates
  across the transitive tree. The leptos stack and the deliberately pinned
  `leptos_integration_utils` 0.8.8 are unchanged.

## [0.4.2] - 2026-05-30

### Fixed

- `SsrMode::Static` routes returned `500 Internal Server Error` on the first
  request to a path that had not been pre-generated by
  `StaticRouteGenerator::generate`. On a successful render
  `ResolvedStaticPath::build` writes the page to disk and returns no in-memory
  body, so the on-demand regeneration branch now re-opens the freshly written
  file instead of falling through to a 500. Requests served from an
  already-generated file were unaffected. Mirrors `leptos_axum` (`ServeDir`)
  and `leptos_actix` (`NamedFile::open`).
- `redirect()` panicked instead of degrading when given a target that is not a
  valid HTTP header value — a path carrying `CR`, `LF`, `NUL`, or other
  control bytes, reachable from app/request-influenced redirect targets such
  as `<Redirect>` or a `?next=` parameter. It now logs and returns without
  setting `Location` or changing the status, matching the conservative
  `NtexServerResponse::redirect` sibling. This is not a header-injection fix:
  `HeaderValue::from_str` already rejected those bytes; the defect was that the
  rejection became a panic. The same `.expect()` lives in the upstream
  `leptos_axum` / `leptos_actix` `redirect()` helpers and was reported as
  leptos-rs/leptos#4749.

## [0.4.1] - 2026-04-25

### Fixed

- Treat `Cache-Control`, `Expires`, and `Content-Disposition` as
  singleton response headers when applying `ResponseOptions`, so user
  overrides replace existing file/response values instead of appending
  ambiguous duplicates.

### Added

- Regression tests for protocol-relative and scheme-mismatched server-fn
  `Referer` fallback handling.
- Regression test for singleton replacement of `Cache-Control`, `Expires`,
  and `Content-Disposition`.
- README guidance for deployments behind reverse proxies: strip
  client-supplied `Forwarded` / `X-Forwarded-*` headers and set trusted
  values at the proxy before requests reach ntex.

### Changed

- Static file handling now parses `Accept-Encoding` once per request and
  reuses the canonical static root while checking precompressed siblings.

## [0.4.0] - 2026-04-24

### Breaking

- `site_pkg_dir_service(options)` now returns an ntex `Scope<Err>` backed
  by `ntex_files::NamedFile` handlers instead of returning
  `ntex_files::Files<Err>` directly. Normal `.service(...)` usage keeps
  working, but code that configured the returned `Files` value or named
  its concrete type must be updated.
- `handle_server_fns()` now returns `405 Method Not Allowed` with an
  `Allow` header when a server-function path exists but the request uses
  the wrong method. Unknown server-function paths still return the
  migration-oriented 400 diagnostic.
- `ResponseOptions` header merging now replaces singleton/framing headers
  such as `Location`, `Content-Type`, `Content-Length`, `ETag`, and
  `Content-Encoding`; repeatable headers such as `Set-Cookie` still
  append.

### Security

- Hardened custom static path resolution against percent-encoded separator
  tricks such as `%2F.env`, backslash smuggling, dotfiles, NUL bytes,
  parent segments, absolute-path replacement, and symlink escape.
- HTML form referrer fallback for server functions now only redirects to
  relative or same-origin referrers, including when the underlying
  `server_fn` response already populated `Location` from the raw
  `Referer`.
- WebSocket upgrade only echoes the configured subprotocol when the client
  offered the same value in `Sec-WebSocket-Protocol`.

### Added

- Builder-style helpers on `LeptosServerFnConfig`
  (`new`, `with_payload_limit`, `with_ws_channel_buffer`,
  `with_ws_subprotocol`) for less error-prone ntex app wiring.
- `CONTRIBUTING.md` with local verification commands and repository
  development notes.
- README sections for recommended ntex wiring, static-file/fallback
  registration, and local development checks.
- Static file helpers serve adjacent `.br` / `.gz` assets when
  `Accept-Encoding` allows them, including honoring an explicit `q=0`
  over a wildcard, while still delegating MIME, validators, ranges, and
  conditional responses to `ntex_files::NamedFile`.
- Regression coverage for precompressed assets, encoded separator
  traversal, repeated static-header replay, WebSocket subprotocol
  negotiation, server-function 405 responses, referrer fallback
  sanitization, `Accept-Encoding` wildcard precedence, and the config
  builder API.

### Changed

- CI now checks formatting, clippy with all features, all-feature tests,
  and rustdoc warnings instead of only `cargo build` + default tests.
- `LeptosServerFnConfig` examples now use the builder-style API.
- `file_and_error_handler` now runs `additional_context` and provides
  `Request` / `ResponseOptions` context on static file hits, matching the
  fallback rendering path more closely.
- Static-route cache hits now replay an immutable snapshot of captured
  response status and headers instead of storing a one-shot
  `ResponseOptions`.
- Executor initialization is idempotent when this crate installed the ntex
  executor itself, while preserving deterministic `AlreadySet` reporting
  when a foreign executor was installed first.
- `collect_payload` reserves from a valid in-limit `Content-Length`, and
  server-function string conversion avoids an extra bytes copy when
  possible.

### Fixed

- rustdoc warning cleanup so
  `RUSTDOCFLAGS="-D warnings" cargo doc --all-features --no-deps`
  succeeds.
- Documentation around static path hardening and `site_pkg_dir_service`
  now matches the current implementation.
- Static routes no longer regenerate unsafe paths and no longer do a
  blocking `exists()` check before opening the on-disk file.
- Static route header/status snapshots are replayed reliably across
  repeated cache hits.
- `redirect()` and optional route path diagnostics remain visible in
  default builds without the `tracing` feature.

## [0.3.0] - 2026-04-22

### Breaking

- **Removed `use_app_state` / `expect_app_state`.**  These functions
  had no counterpart in `leptos_actix` or `leptos_axum` and were
  incorrectly attributed to the axum adapter.  Use
  `extract::<ntex::web::types::State<T>>().await?` instead, which
  matches the pattern used by both upstream integrations.
- Minimum supported Rust version raised from 1.85 to **1.88**
  (required by leptos 0.8.19 and server_fn 0.8.12).

### Changed

- Bumped `leptos` 0.8.17 → 0.8.19, `server_fn` 0.8.11 → 0.8.12.
- `file_and_error_handler_with_context` now reuses the internal
  `async_stream_builder` instead of duplicating the streaming
  pipeline.
- `ACTIX_TO_NTEX_NOTES.md` rewritten: corrected "Borrowed from axum"
  claims, added "Original to this crate" section documenting items
  unique to this adapter (`extract_with_err`, `register_leptos_routes`,
  `LeptosServerFnConfig`, `register_explicit`, `server_fn_paths`,
  `get_server_fn_service`, `try_init_executor`, per-method routing).

### Added

- 8 new tests: payload boundary precision (exactly-at-limit,
  one-over-limit), WebSocket frame-type coverage (unfragmented binary
  oversize, text echo, text oversize, ping/pong, close echo),
  dotfile-in-subdirectory traversal rejection.
- CI workflow via GitHub Actions (`rust.yml`).

## [0.2.0] - 2026-04-17

### Security

- `file_and_error_handler`: hardened against path traversal. Each URI
  segment is percent-decoded before comparison, dotfiles / `..` /
  embedded NUL bytes are rejected, only `Component::Normal` segments
  survive, and the resolved path is canonicalized and required to
  stay under the canonical `site_root` (defence against symlink
  escape and `Path::join` absolute-path replacement). Adds
  `percent-encoding` as a required dependency.

### Added

- `try_init_executor()` public helper for installing the ntex-backed
  `any_spawner` executor at startup. Returns
  `Err(ExecutorError::AlreadySet)` if another runtime won the
  one-shot install race, so mixed-runtime apps can fail fast rather
  than discover the conflict under load. Lazy auto-install from every
  public entry point is preserved.
- Public per-layer module paths under the crate root (`config`,
  `extract`, `files`, `leptos_routes`, `render`, `request`,
  `response`, `routes`, `server_fn`, `static_routes`). Existing
  re-exports at the crate root remain unchanged.

### Changed

- Source layout: `src/lib.rs` (4217 lines) decomposed into 15 focused
  modules under `src/` and `src/server_fn/`. The crate root is now a
  ~40-line facade of `pub use` re-exports. Public API surface is
  identical; existing `use leptos_ntex_unofficial::*` users require
  no changes.
- Executor-conflict warning now also prints to `eprintln!` so the
  message is visible in builds without the `tracing` feature (was
  previously silent in the default configuration).

### Fixed

- HEAD requests now correctly mirror GET for both dynamic and static
  routes. The previous synthetic `HEAD → 200 OK .finish()` shortcut
  masked real 404s; HEAD now flows through the same handler as GET
  and ntex's h1 encoder strips the body at the wire (RFC 9110 §9.3.2).
  Unregistered paths return the app's configured 404 on HEAD as well.
- WebSocket bridge: proper backpressure via `Sink::send().await` (was
  `start_send`, which dropped frames under load); per-connection
  reassembly of `Frame::Continuation(Item::{First*, Continue, Last})`
  fragments; `payload_limit` enforced on the opening fragment and on
  the cumulative reassembled buffer; policy-violation close with
  `CloseCode::Size` (1009, "Message Too Big" per RFC 6455 §7.4.1) on
  overflow, `CloseCode::Protocol` on invalid fragmentation state.
- Server-function oversize payloads now return a real
  `413 Payload Too Large` response. `Content-Length` is preflighted
  before any body bytes are read, and streaming/chunked overflow is
  detected via a request-scoped `PayloadTooLarge` marker that the
  outer handler promotes into a 413 (previously surfaced as an opaque
  `500` through the server-fn error channel). Overflow error is
  reported as `ServerFnErrorErr::Args` (semantically "error reading
  arguments from the request") instead of `Deserialization`.

### Removed

- `ACTIX_TO_NTEX_NOTES.md` from the repository (migration notes; was
  already excluded from the published crate).

### Added

- Initial unofficial Leptos adapter for ntex, based on `leptos_actix` with
  ideas ported from `leptos_axum`. See `ACTIX_TO_NTEX_NOTES.md` in the
  repository for the full migration log.
- Public API: `generate_route_list`, `LeptosRoutes::leptos_routes`,
  `register_leptos_routes`, `handle_server_fns`, `file_and_error_handler`,
  `site_pkg_dir_service`, `NtexServerFnBackend`, `extract`,
  `redirect`, `ResponseOptions`, `LeptosServerFnConfig`.
- Configurable payload limit, WebSocket channel buffer, and WebSocket
  subprotocol via `LeptosServerFnConfig`.
- Static site generation through `generate_route_list_with_ssg`.
- Optional `tracing` feature and `islands-router` feature flag forwarded to
  Leptos.

[Unreleased]: https://github.com/AlexeyMatskevich/leptos_ntex/compare/v0.6.0...HEAD
[0.6.0]: https://github.com/AlexeyMatskevich/leptos_ntex/compare/v0.5.0...v0.6.0
[0.5.0]: https://github.com/AlexeyMatskevich/leptos_ntex/compare/v0.4.2...v0.5.0
[0.4.2]: https://github.com/AlexeyMatskevich/leptos_ntex/compare/v0.4.1...v0.4.2
[0.4.1]: https://github.com/AlexeyMatskevich/leptos_ntex/compare/v0.4.0...v0.4.1
[0.4.0]: https://github.com/AlexeyMatskevich/leptos_ntex/compare/v0.3.0...v0.4.0
[0.3.0]: https://github.com/AlexeyMatskevich/leptos_ntex/compare/v0.2.0...v0.3.0
[0.2.0]: https://github.com/AlexeyMatskevich/leptos_ntex/compare/v0.1.0...v0.2.0
[0.1.0]: https://github.com/AlexeyMatskevich/leptos_ntex/releases/tag/v0.1.0
