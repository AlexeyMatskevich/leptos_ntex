# leptos_actix → leptos_ntex

This document describes what this adapter changes relative to
`leptos_actix`, and why. It is aimed at readers who already know the
actix integration and want to understand the ntex-specific pieces.

Baseline: `integrations/actix/src/lib.rs` from the `leptos` repository.
Several additions are lifted from `integrations/axum/src/lib.rs` when
axum solves a problem more cleanly than actix does.

## Ported from actix

Core public API mirrors `leptos_actix` so that porting an existing
actix app is close to a crate-rename:

- `ResponseParts`, `ResponseOptions`, `Request`, `redirect()`
- `handle_server_fns()` / `handle_server_fns_with_context()`
- `render_app_to_stream*`, `render_app_to_stream_in_order*`,
  `render_app_async*`,
  `render_app_to_stream_with_context_and_replace_blocks`
- `generate_route_list*` family (5 variants)
- `StaticRouteGenerator` and all of its methods
- the `LeptosRoutes` trait with impls for `App<M, T, Err>` and for
  `&mut ServiceConfig<Err>`
- `extract()` helper (for ntex extractors that operate on the request
  head)

Not a 1:1 copy — ntex-specific additions listed below have no actix
or axum counterpart.

Crate-root attributes match too: `#![forbid(unsafe_code)]`,
`#![deny(missing_docs)]`, doc comments on every public item.

## Borrowed from axum

Places where the axum integration has a better primitive than actix
does; all of them adapted to ntex types:

- **`file_and_error_handler` / `file_and_error_handler_with_context`.**
  A GET route that first tries to serve a file from
  `options.site_root`, and on miss renders the shell with `404 Not
  Found`. Uses `ntex_files::NamedFile` for the hit path so MIME, ETag,
  and Last-Modified are automatic. Actix's SSR integration doesn't
  ship an equivalent helper.
- **`site_pkg_dir_service(options)`.** Ready-to-use ntex scope for
  `options.site_pkg_dir` under `options.site_root`, analogous to
  axum's `ServeDir` wrapper. Both this helper and the catch-all
  `file_and_error_handler` serve adjacent `.br` / `.gz` variants when
  the request advertises support, while delegating MIME, ETag,
  Last-Modified, ranges, and conditional requests to
  `ntex_files::NamedFile`.
- **`PinnedHtmlStream`.** Public type alias
  `Pin<Box<dyn Stream<Item = io::Result<NBytes>> + Send>>`.
- **`generate_request_and_parts(req, payload) -> (NtexRequest,
  HttpRequest)`.** Symmetric to the axum helper for decomposing a
  request into its server-fn form plus a cloned head.
- **`handle_response_inner(...)`.** Public low-level entry point for
  running the SSR pipeline on a single request (returns
  `PinnedFuture<HttpResponse>`), so it can be embedded in custom
  route handlers.
- **Async filesystem I/O on the static path.** `write_static_route`
  and `handle_static_route` wrap `fs::write`, `fs::create_dir_all`,
  `Path::exists`, and `NamedFile::open` in `ntex::rt::spawn_blocking`
  so slow filesystems (NFS, FUSE, overloaded disks) don't stall the
  arbiter.

## Original to this crate

Public API that has no counterpart in either `leptos_actix` or
`leptos_axum`:

- **`extract_with_err<T, Err>()`.**  Like `extract()` but
  parameterised over the ntex error renderer. Needed because ntex
  parametrises routes on `Err: ErrorRenderer`, unlike actix/axum.
- **`register_leptos_routes(cfg, ...)`.**  Free-function shortcut for
  `LeptosRoutes` that avoids repeating the verbose
  `App<M, T, Err>` bounds at every call site.
- **`LeptosServerFnConfig`.**  Per-app tunables (`payload_limit`,
  `ws_channel_buffer`, `ws_subprotocol`) registered via
  `App::state`.
- **`register_explicit::<T>()`.**  Manual server-function registration
  for platforms where `inventory` auto-collection doesn't work
  (wasm, edge runtimes) and for test binaries.
- **`server_fn_paths()`.**  Iterator over registered `(path, method)`
  pairs; used internally by `leptos_routes*` and exposed for custom
  routing.
- **`get_server_fn_service()`.**  Looks up the middleware-wrapped
  service for a server function by path and method; called from
  `handle_server_fns` and exposed for advanced compositions.
- **`try_init_executor()`.**  Fallible executor installation that
  returns `AlreadySet` instead of panicking, for apps that mix
  runtimes and want to fail fast at startup.
- **Per-method server-fn routing.** When registered through
  `leptos_routes*`, every `(path, method)` gets its own `Route` with
  a method filter so a wrong method is rejected at the router level
  (ntex may surface this as 404 or 405 depending on resource matching).
  The catch-all `handle_server_fns()` returns `405 Method Not Allowed`
  with `Allow` when the path is known but the method is wrong; unknown
  paths still return 400 with the migration-oriented diagnostic.

## What had to change for ntex

These differences are forced by ntex's type system or runtime model —
not stylistic:

- **Crate routing.** `actix_web::{App, Route, HttpRequest,
  HttpResponse}` → the `ntex::web` family.
  `HttpServer::new(...).run().await` → `ntex::web::server(...)` with
  an async factory.
- **Verbose bounds on `LeptosRoutes for App<...>`.** Actix has a
  tighter `App<T>` type; ntex parametrises it on
  `App<M, T: ServiceFactory<..., ntex::service::cfg::SharedCfg>,
  Err: ErrorRenderer>` with an additional
  `Err::Container: From<StateExtractorError>` bound. The
  `register_leptos_routes(cfg, ...)` shortcut exists precisely
  because repeating those bounds at every call site is noisy.
- **Generic error renderer.** We construct routes via explicit
  `Route::<Err>::new().method(...).to(...)` because the
  `web::get()/post()/head()` shortcuts force the type parameter to
  `Route<DefaultError>`, which doesn't compose with a user's custom
  `Err: ErrorRenderer`.
- **`HeaderMap` is not `IntoIterator`.** ntex's `http::HeaderMap`
  doesn't implement an owned `IntoIterator`, so `extend_response`
  drains `ResponseParts` via `std::mem::take` and iterates over
  `iter()` with cloned values.
- **`HttpRequest` / `HttpResponse` are `!Send`.** This drives most of
  the adapter's plumbing:
  - `Request` stores `Option<SendWrapper<HttpRequest>>`.
  - The `server_fn` backend (`NtexRequest`, `NtexServerResponse`,
    `NtexServerFnBackend`) uses `SendWrapper` throughout.
  - `extract()` and `handle_response_inner()` wrap their async
    bodies in `SendWrapper` because `PinnedFuture<HttpResponse>`
    requires `Send`.
- **`Request::Drop` defensive leak.** If the `SendWrapper<HttpRequest>`
  ends up on a different thread (static prerender can migrate
  `Owner`s), dropping would panic. We `std::mem::forget` instead,
  accepting a bounded `Rc` leak in exchange for never tearing down an
  arbiter. Documented in-place on the `Request` type.
- **Executor bridge.** ntex supports three runtimes (`tokio`, `compio`,
  the default `neon`) and `any_spawner` knows about none of them.
  `NtexExecutor` implements `any_spawner::CustomExecutor` by
  delegating `spawn` / `spawn_local` to `ntex::rt::spawn`, which
  abstracts all three backends and keeps the spawned task on the same
  arbiter that handles the request. Installed idempotently (guarded
  by a `std::sync::Once`) from every public entry point; the
  `try_init_executor()` helper lets apps that mix runtimes fail fast
  at startup with `ExecutorError::AlreadySet` rather than discover the
  conflict under load. When the fallback fires, the warning is sent
  to both `tracing::warn!` (when the feature is on) and `eprintln!`
  so the conflict is never silent.

## State threading

ntex handles state differently from axum (which attaches a single
type parameter `S` to the whole `Router`). In ntex, app state is
typed per value via `.state::<T>(value)` and multiple types can
coexist. The adapter surfaces this with two idioms:

- **Reading state from a server fn:**
  `extract::<ntex::web::types::State<T>>().await?`.
- **Propagating state into SSR components:** via
  `leptos_routes_with_context`, the user supplies an
  `additional_context` closure that calls
  `provide_context(state.clone())`. Identical to actix.

## HEAD routing (RFC 9110 §9.3.2)

Dynamic and statically pre-rendered routes bind HEAD to the GET
handler via `guard::Any(guard::Get()).or(guard::Head())`. ntex's h1
encoder then strips the body at the wire when the request method is
HEAD, so status and headers mirror GET byte-for-byte with no body.

`Route::method()` was intentionally not used for this: `take_guards()`
turns it into an AND-combined `MethodGuard` on the owning Resource,
which would prevent multi-method matching on a single route.

A HEAD on a path that isn't registered falls through to the app's
configured 404 instead of returning a synthetic 200 — that keeps
monitoring honest.

## Server-fn dispatch

Two paths, chosen at registration time:

- **Catch-all.** `handle_server_fns()` / `handle_server_fns_with_context()`
  mount at a single wildcard route and look the target up in
  `REGISTERED_SERVER_FUNCTIONS` per request. Useful for `/api/{tail:.*}`
  style prefixes and for migration from the actix shape.
- **Per-path.** When you register via `LeptosRoutes::leptos_routes*`,
  each `(path, method)` gets its own handler closing over a
  pre-resolved `ServerFnTraitObj` with a cached
  `Arc<[Arc<dyn Layer>]>` middleware list. No `HashMap` lookup, no
  per-request `Vec` allocation for the layer list — just one atomic
  increment per request.

Both paths share `dispatch_server_fn(...)`, which sets up the reactive
`Owner`, provided contexts (`Request`, `ResponseOptions`), the
referrer-based 302 fallback for HTML form submissions, and the
`Location` header merge from `ResponseOptions` (singleton response
headers replace earlier values; `Set-Cookie` and other repeatable
headers still append).

## Payload limits and 413

`server_fn` 0.8 has no dedicated "request too large" error variant, so
translating overflow into a proper `413 Payload Too Large` needs a
crate-specific path:

- Tunables live in `LeptosServerFnConfig` (register via
  `App::state`). Defaults: `payload_limit = 2 MiB`,
  `ws_channel_buffer = 2048`, `ws_subprotocol = None`.
- **Preflight.** If the request declares `Content-Length` and it
  exceeds the limit, the handler returns 413 without reading any of
  the body.
- **Streaming / chunked overflow.** `collect_payload` and the
  `try_into_stream` adapter enforce the limit as bytes arrive. On
  overflow they stash a request-scoped `PayloadTooLarge` marker into
  `req.extensions_mut()` and surface the underlying error as
  `ServerFnErrorErr::Args` (semantically "error reading arguments
  from the request"). The outer handler observes the marker after the
  `server_fn` pipeline returns and rewrites the response to 413 with
  a human-readable body that states the limit.

## WebSocket bridge

The `Req::try_into_websocket` impl on `NtexRequest` upgrades via
`ntex::web::ws::start` and hands the `server_fn` runtime an
`(incoming_stream, outgoing_sink, response)` triple. Notable pieces:

- **Backpressure.** Both incoming and outgoing channels are bounded
  `futures::channel::mpsc`. Producers call `Sink::send().await`, so a
  slow consumer suspends the frame-reader task. Buffer capacity is
  `LeptosServerFnConfig::ws_channel_buffer`.
- **Fragment reassembly (RFC 6455 §5.4).** ntex delivers
  fragmented messages as
  `Frame::Continuation(Item::{FirstText, FirstBinary, Continue,
  Last})`. The bridge keeps a per-connection
  `Rc<RefCell<Option<(FragmentKind, BytesMut)>>>` and hands the
  reassembled payload to the server-fn on `Item::Last`. Without this
  any client that fragments large messages (browsers often do) would
  silently lose data.
- **Limit enforcement.** `payload_limit` is checked on each
  unfragmented frame, on the opening fragment, and on the cumulative
  buffer. On overflow the bridge closes the connection with
  `CloseCode::Size` (1009, "Message Too Big" per RFC 6455 §7.4.1) and
  delivers a typed `InputStreamError` to the server-fn receiver so
  the client gets a structured reason rather than an abrupt
  disconnect. Invalid fragmentation state (e.g. a `Continue` without a
  prior `First*`) closes with `CloseCode::Protocol`.
- **Subprotocol.** Configurable via
  `LeptosServerFnConfig::ws_subprotocol`. For dynamic per-request
  selection, callers can read `ntex::web::ws::subprotocols(&req)`
  inside a custom WebSocket handler instead of going through
  `handle_server_fns`.

## Static-file fallback hardening

`file_and_error_handler` resolves a URL path to an absolute filesystem
path through `safe_subpath`, which:

- splits the URI on `/` and percent-decodes each segment before any
  comparison (so `%2e%2e` cannot bypass the filter);
- rejects `..`, dotfiles (`.env`, `.htaccess`), embedded NUL bytes,
  forward slashes or backslashes that appear after percent-decoding
  a segment;
- requires every resulting path component to be
  `Component::Normal` and not hidden, which blocks absolute-path
  smuggling and encoded separator tricks such as `%2F.env`;
- canonicalizes both the candidate path and `site_root` and requires
  the candidate to stay under the canonical root, which defeats
  symlink-escape.

The whole check runs on `ntex::rt::spawn_blocking` because
`canonicalize` and `NamedFile::open` both perform blocking I/O. Adds
`percent-encoding` as a required dep.

## Feature flags

```toml
[features]
default = []
tracing = ["dep:tracing"]
islands-router = ["leptos/islands-router"]
```

- `tracing` enables `#[tracing::instrument(...)]` on the public route
  functions and `tracing::warn!/error!` inside the redirect path and
  the `OptionalParam` branch. Without the feature those warnings still
  hit stderr via `eprintln!`.
- `islands-router` switches the stream builders to the
  `*_branching()` variants and activates header-based
  island-navigation detection. Note that upstream actix/axum wire this
  through `tachys/islands` / `tachys/mark_branches` directly; here we
  forward through `leptos/islands-router` because this crate sits
  outside the workspace.

## Known remaining divergences

- `LeptosRoutes for App<...>` carries more verbose type bounds than
  actix; the `register_leptos_routes(cfg, ...)` shortcut is the
  ergonomic escape hatch.
- `Request::Drop` leaks an `Rc` increment on cross-thread drops
  instead of panicking. Actix does not have an equivalent trade-off
  because `actix_web::HttpRequest` is `Send`.
- The `replace_blocks` argument on
  `render_app_to_stream_with_context_and_replace_blocks` is accepted
  for API parity with `leptos_actix` / `leptos_axum` but is currently
  a no-op — the Leptos HTML-stream APIs don't yet expose a toggle
  for retrojecting blocking `<Suspense/>` fragments into the initial
  payload. This means
  `SsrMode::PartiallyBlocked` produces the same stream as
  `SsrMode::OutOfOrder` across all three integrations today.
