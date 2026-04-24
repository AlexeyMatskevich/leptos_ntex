# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

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

[Unreleased]: https://github.com/AlexeyMatskevich/leptos_ntex/compare/v0.4.0...HEAD
[0.4.0]: https://github.com/AlexeyMatskevich/leptos_ntex/compare/v0.3.0...v0.4.0
[0.3.0]: https://github.com/AlexeyMatskevich/leptos_ntex/compare/v0.2.0...v0.3.0
[0.2.0]: https://github.com/AlexeyMatskevich/leptos_ntex/compare/v0.1.0...v0.2.0
[0.1.0]: https://github.com/AlexeyMatskevich/leptos_ntex/releases/tag/v0.1.0
