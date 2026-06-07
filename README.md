# leptos-ntex-unofficial

[![Crates.io](https://img.shields.io/crates/v/leptos-ntex-unofficial.svg)](https://crates.io/crates/leptos-ntex-unofficial)
[![docs.rs](https://img.shields.io/docsrs/leptos-ntex-unofficial)](https://docs.rs/leptos-ntex-unofficial)
[![License](https://img.shields.io/crates/l/leptos-ntex-unofficial.svg)](#license)

**Unofficial** SSR integration that lets you run a [Leptos](https://leptos.dev)
application on top of the [ntex](https://ntex.rs) web framework.

> This is a community adapter. It is **not** affiliated with the Leptos project
> or the ntex project. The officially-maintained adapters live in the
> [Leptos monorepo](https://github.com/leptos-rs/leptos/tree/main/integrations)
> as `leptos_actix` and `leptos_axum`. This crate takes `leptos_actix` as its
> starting point and ports the public API to ntex.

## Quick start

Add the dependency:

```toml
[dependencies]
leptos = { version = "0.8", features = ["ssr", "nonce"] }
leptos-ntex-unofficial = "0.4"
ntex = "3"
```

Define an `App`, a `shell`, and wire them up through `register_leptos_routes`:

```rust,no_run
use leptos::prelude::*;
use leptos_meta::{MetaTags, provide_meta_context};
use leptos_ntex_unofficial::{generate_route_list, register_leptos_routes};
use leptos_router::{components::{Route, Router, Routes}, path};
use ntex::web::{self, App as NtexApp};

#[component]
fn App() -> impl IntoView {
    provide_meta_context();
    view! {
        <Router>
            <Routes fallback=|| view! { <h1>"Not Found"</h1> }>
                <Route path=path!("/") view=|| view! { <h1>"Hello, ntex!"</h1> }/>
            </Routes>
        </Router>
    }
}

fn shell() -> impl IntoView {
    view! {
        <!DOCTYPE html>
        <html><head><MetaTags/></head><body><App/></body></html>
    }
}

#[ntex::main]
async fn main() -> std::io::Result<()> {
    let routes = generate_route_list(App);
    web::server(move || {
        let routes = routes.clone();
        async move {
            NtexApp::new().configure(move |cfg| {
                register_leptos_routes(cfg, routes.clone(), shell);
            })
        }
    })
    .bind(("127.0.0.1", 3000))?
    .run()
    .await
}
```

A runnable version of the same example is in
[`examples/basic.rs`](examples/basic.rs) — try it with:

```sh
cargo run --example basic
```

## Recommended ntex wiring

Most applications want three ntex registrations:

1. app state containing `LeptosOptions`;
2. a static bundle service for `site_pkg_dir`;
3. generated Leptos routes plus a final file/404 fallback.

```rust,no_run
use leptos::{config::LeptosOptions, prelude::*};
use leptos_ntex_unofficial::{
    file_and_error_handler, register_leptos_routes, site_pkg_dir_service,
    LeptosServerFnConfig, NtexRouteListing,
};
use ntex::web::App as NtexApp;

# fn app() -> impl IntoView { "" }
# fn shell(_: LeptosOptions) -> impl IntoView { app() }
# fn example(options: LeptosOptions, routes: Vec<NtexRouteListing>) {
let _app = NtexApp::new()
    .state(options.clone())
    .state(
        LeptosServerFnConfig::new()
            .with_payload_limit(8 * 1024 * 1024)
            .with_ws_channel_buffer(512),
    )
    .service(site_pkg_dir_service::<ntex::web::DefaultError>(&options))
    .configure(move |cfg| {
        register_leptos_routes(cfg, routes.clone(), app);
    })
    .route(
        "/{tail}*",
        file_and_error_handler::<_, ntex::web::DefaultError>(shell),
    );
# }
```

`site_pkg_dir_service` is intended for generated assets such as JS, WASM,
CSS, and their precompressed `.br` / `.gz` siblings. `file_and_error_handler`
is the catch-all fallback: it first serves a safe file hit from `site_root`,
then renders the Leptos shell with `404 Not Found` when no file exists.

## Public API at a glance

| Item | Purpose |
|---|---|
| `generate_route_list(app_fn)` | Enumerate Leptos routes for registration |
| `generate_route_list_with_ssg` | Same, also returns a static-site generator |
| `LeptosRoutes::leptos_routes` | Extension trait on `ntex::web::App` to mount routes |
| `register_leptos_routes` | `ServiceConfig`-style alternative for composable setup |
| `handle_server_fns` | Returns a `Route` that dispatches all registered server functions |
| `file_and_error_handler` | Serves files from `site_root` and falls back to a shell on 404 |
| `site_pkg_dir_service` | Serves `cargo-leptos`-produced JS/WASM/CSS bundle, including `.br`/`.gz` siblings |
| `NtexServerFnBackend` | Use as `server = leptos_ntex_unofficial::NtexServerFnBackend` on `#[server]` |
| `extract`, `extract_with_err` | Extract ntex extractors (e.g. `HttpRequest`) from a server function |
| `redirect(path)` | Issue a redirect from inside a server function |
| `ResponseOptions` | Mutate response headers/status from inside a server function |
| `LeptosServerFnConfig` | Configure payload limit, WebSocket buffer, WS subprotocol |
| `try_init_executor` | Eagerly install the ntex-backed Leptos executor |
| `register_explicit` | Manually register server functions when `inventory` is unavailable |
| `server_fn_paths`, `get_server_fn_service` | Advanced hooks for custom server-fn routing |

See the [API docs](https://docs.rs/leptos-ntex-unofficial) for the full list,
signatures, and runnable snippets.

## Feature flags

| Feature | Effect |
|---|---|
| `tracing` | Emit `tracing` spans around route rendering and server-fn dispatch |
| `islands-router` | Forwards to `leptos/islands-router` |

Nothing is enabled by default.

## Configuring server-fn limits

`LeptosServerFnConfig` is read out of ntex application state at request time:

```rust,no_run
use leptos_ntex_unofficial::{handle_server_fns, LeptosServerFnConfig};
use ntex::web::App as NtexApp;

let _app = NtexApp::new()
    .state(
        LeptosServerFnConfig::new()
            .with_payload_limit(8 * 1024 * 1024) // 8 MiB
            .with_ws_channel_buffer(512)
            .with_ws_subprotocol("graphql-ws"),
    )
    .route("/api/{tail}*", handle_server_fns());
```

If you don't register a `LeptosServerFnConfig`, the defaults from
[`DEFAULT_PAYLOAD_LIMIT`](https://docs.rs/leptos-ntex-unofficial/latest/leptos_ntex_unofficial/constant.DEFAULT_PAYLOAD_LIMIT.html)
and
[`DEFAULT_WS_CHANNEL_BUFFER`](https://docs.rs/leptos-ntex-unofficial/latest/leptos_ntex_unofficial/constant.DEFAULT_WS_CHANNEL_BUFFER.html)
are used.

Configured WebSocket subprotocols are only echoed when the client offered the
same protocol in `Sec-WebSocket-Protocol`. For dynamic negotiation, use a
custom ntex WebSocket handler and `ntex::web::ws::subprotocols`.

### Proxy headers

ntex's `ConnectionInfo` trusts `Forwarded`, `X-Forwarded-Host`, and
`X-Forwarded-Proto` when resolving the request host and scheme. If the
application runs behind a reverse proxy, configure the proxy to strip any
client-supplied forwarding headers and set trusted values itself before the
request reaches ntex. This matters for same-origin decisions such as the
HTML-form server-function referrer fallback.

## Development

The shortest local feedback loop is:

```sh
cargo fmt --all -- --check
cargo test
cargo test --all-features
cargo clippy --all-targets --all-features -- -D warnings
RUSTDOCFLAGS="-D warnings" cargo doc --all-features --no-deps
```

See [`CONTRIBUTING.md`](CONTRIBUTING.md) for the repository workflow and
release checklist.

## Migrating from `leptos_actix`

The public API intentionally mirrors `leptos_actix`, so most code ports 1:1 by
switching the crate name and the `server = ...` backend:

| `leptos_actix` | `leptos-ntex-unofficial` |
|---|---|
| `use leptos_actix::{...}` | `use leptos_ntex_unofficial::{...}` |
| `server = leptos_actix::ActixServerFnBackend` | `server = leptos_ntex_unofficial::NtexServerFnBackend` |
| `actix-web` types (`HttpRequest`, `HttpResponse`) | `ntex::web::HttpRequest`, `ntex::web::HttpResponse` |
| `actix-files::Files` | `ntex-files::NamedFile` behind `site_pkg_dir_service` / `file_and_error_handler` |

A detailed port log is kept in
[`ACTIX_TO_NTEX_NOTES.md`](https://github.com/AlexeyMatskevich/leptos_ntex/blob/master/ACTIX_TO_NTEX_NOTES.md)
in the repository.

## License

Dual-licensed under either of

* MIT license ([LICENSE-MIT](LICENSE-MIT) or <https://opensource.org/licenses/MIT>)
* Apache License 2.0 ([LICENSE-APACHE](LICENSE-APACHE) or <https://www.apache.org/licenses/LICENSE-2.0>)

at your option.

## Contributing

Bug reports and pull requests are welcome at
<https://github.com/AlexeyMatskevich/leptos_ntex>.

Unless explicitly stated otherwise, any contribution intentionally submitted
for inclusion in this crate by you, as defined in the Apache-2.0 license, shall
be dual licensed as above, without any additional terms or conditions.
