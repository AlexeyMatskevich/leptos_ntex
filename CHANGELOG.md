# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

## [0.1.0] - 2026-04-16

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

[Unreleased]: https://github.com/AlexeyMatskevich/leptos_ntex/compare/v0.1.0...HEAD
[0.1.0]: https://github.com/AlexeyMatskevich/leptos_ntex/releases/tag/v0.1.0
