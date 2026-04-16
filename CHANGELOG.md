# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

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
  `site_pkg_dir_service`, `NtexServerFnBackend`, `use_app_state`, `extract`,
  `redirect`, `ResponseOptions`, `LeptosServerFnConfig`.
- Configurable payload limit, WebSocket channel buffer, and WebSocket
  subprotocol via `LeptosServerFnConfig`.
- Static site generation through `generate_route_list_with_ssg`.
- Optional `tracing` feature and `islands-router` feature flag forwarded to
  Leptos.

[Unreleased]: https://github.com/AlexeyMatskevich/leptos_ntex/compare/v0.2.0...HEAD
[0.2.0]: https://github.com/AlexeyMatskevich/leptos_ntex/compare/v0.1.0...v0.2.0
[0.1.0]: https://github.com/AlexeyMatskevich/leptos_ntex/releases/tag/v0.1.0
