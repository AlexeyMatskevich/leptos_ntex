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
leptos_meta = { version = "0.8", features = ["ssr"] }
leptos_router = { version = "0.8", features = ["ssr"] }
leptos-ntex-unofficial = "0.7"
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

fn main() -> std::io::Result<()> {
    ntex::rt::System::new(
        "leptos-ntex",
        leptos_ntex_unofficial::RequestRuntime::new(ntex::rt::DefaultRuntime),
    )
    .block_on(run())
}

async fn run() -> std::io::Result<()> {
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
[`examples/basic.rs`](https://github.com/AlexeyMatskevich/leptos_ntex/blob/master/examples/basic.rs) — try it with:

```sh
cargo run --example basic
```

There is also a server-function extractor example in
[`examples/auth_extractor.rs`](https://github.com/AlexeyMatskevich/leptos_ntex/blob/master/examples/auth_extractor.rs):

```sh
cargo check --example auth_extractor --features cookie
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
| `cookie` | Enable ntex's cookie support (`ntex/cookie`) for cookie-reading extractors; required to build `examples/auth_extractor.rs` |

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

### Streaming and connection lifetime

The payload limit applies to incoming HTTP bodies and complete incoming
WebSocket messages. Oversized declared lengths and overflow observed by the
dispatcher before it returns the response produce `413`. A streaming server
function can return its response before it finishes reading its input. A later
input error remains a stream error, even if no response bytes have reached the
client yet; an already sent status cannot change. On HTTP/1 the adapter
terminates a failed response connection; HTTP/2 keeps ntex's native stream-error
handling. A lazy response can therefore end without any response bytes or with
an incomplete original response. Clients must treat a truncated body as a
failed response.

Application headers supplied through `ResponseOptions` survive handshake and
payload-limit errors detected after context setup. These errors retain their
own status, representation metadata and supported WebSocket version header.
Rejections before context setup, including an oversized declared
`Content-Length`, do not run the context callback.

HEAD still constructs the response, including SSR setup and any required
prefetch. Once constructed, the response retains its body size and ownership
for headers and cleanup, but does not poll its body producer.

The WebSocket bridge has one I/O task per connection. It pauses input when the
application stops reading and waits for transport write capacity before taking
more output. `ws_channel_buffer` is a message count, not a connection memory
quota: `futures` channels also reserve one slot per sender, and memory includes
fragment assembly, the current output message, ntex buffers and socket buffers.
Application-created sender clones add reservations. If input stays full and the
application retains its receiver without reading it, ntex may also delay
observing a transport close until reading resumes; there is no automatic
application inactivity timeout. The incoming payload limit
does not limit application-produced output message sizes.

Authenticate requests and check cookie/CSRF and WebSocket Origin policy in ntex
middleware before upgrade. A WebSocket server function runs after the `101`
response has been sent. The adapter does not provide an application Origin
allowlist or authorization policy.

### Static routes and deployment

Generated static HTML is shared by requests for a static route.
Use it only for public representations: request-specific cookies, headers,
nonces or personal data captured during generation are also shared. Use dynamic
SSR for representations that depend on the requesting user.
Generation uses a synthetic request; it does not receive the visitor's cookies.
Values supplied by the app through `additional_context` and `ResponseOptions`
are still captured, including `Set-Cookie`. Setting `Cache-Control: private` or
`no-store` controls downstream HTTP caches; it does not switch a static route to
per-user rendering or prevent the origin from saving its generated artifact.

Deploy each generated HTML file together with its hidden metadata, stored as
`.leptos-static-metadata/<HTML filename>` in the same parent directory. The
metadata entry follows the same filename limits and case/Unicode equivalence
rules as the HTML entry. `.leptos-static-metadata` is reserved for this
protocol: when deploying it separately, preserve the parent directory's case and
Unicode lookup rules, including any per-directory filesystem flags. A dangling
metadata symlink or metadata-directory symlink is a damaged artifact, so it
permits neither guessed metadata nor serving as a metadata-free file. A link
with a missing or unrepresentable target is damaged metadata, even when its own
filename is short. Regeneration can replace a damaged final metadata entry; an
unusable metadata directory, including an ordinary file at that name, cannot be
published into and is never removed or replaced by the adapter. Valid internal
symlinks remain usable.
The directory form preserves the captured status and response
headers, including an explicitly selected media type, across process restarts.
When serving a file, an invalid media type falls back to the type inferred from
its name. Conditional and partial responses preserve `Content-Location`;
`Content-Language` is omitted from `304` and from `206` responses to `If-Range`
requests, where the client already has the representation metadata.
The per-directory publication and scratch bucket lock files may be omitted
from an immutable deployment. The root's `.leptos-static-policy.lock` stores
an installed budget and must be retained if publication may resume. Readers also support complete pairs on read-only storage without
creating lock files; if a concurrent publisher creates a lock, they reopen the
pair under that lock. Existing lock files must remain readable and must never
be removed while cooperating processes are serving or publishing.
Readers verify the HTML digest before applying metadata. Present but unreadable
or inconsistent metadata is not replaced with a guessed status. HTML without
metadata is treated as a legacy plain file, so omitting all hidden files loses
the captured status and headers.

Generation uses capability-relative file operations, exclusive temporary-file
creation, and advisory locks shared by cooperating adapter processes. A reader
verifies that the selected HTML bytes match the digest stored with the metadata.
The two file replacements are not a power-loss transaction: interruption between
them can temporarily make the route unavailable until regeneration. If the old
and new HTML bytes are identical, the new metadata can already be valid with the
old file; this format does not preserve a physical generation identifier.
External writers must deploy complete pairs and honor the same coordination.
Arbitrary in-place editing is outside this guarantee. Live publishers must use
the same publication protocol; backward-readable artifacts do not make concurrent
publication by an older adapter version safe.
The `.leptos-static-publish.lock` in each physical parent directory coordinates
the two replacements, including paths that differ only in case or Unicode form
on filesystems that treat them as aliases. Readers hold it shared only while
opening the two files, then validate their pinned contents after releasing it.
Rendering, digest computation and temporary-file preparation happen outside
this publication lock. Preparation still uses 32 lock buckets per directory
to retain one active metadata serialization buffer and temporary pair per bucket
across cooperating publishers. Unrelated publishers in one bucket can wait for
each other's preparation; completed preparations serialize their commits within
the parent directory.
This adds one publication lock per directory and creates the metadata directory
only when needed. Migrating an existing route to that directory leaves its old
adjacent sidecar in place; readers use the directory entry once it exists. The
adapter does not delete older artifacts automatically.

On-demand generation for a route listing shares pending work and a regeneration
subscription. A persisted HTML hit after restart does not restore a live
subscription: call `generator.generate(&options).await` on the serving process's
ntex runtime before accepting requests, on every startup. Keep that runtime
alive while serving. Existing files are refreshed and the process installs its
own regeneration subscriptions. A separate pre-build process cannot transfer
its callbacks or reactive contexts into the server.

When a cache miss starts a new generation, its app function and
`additional_context` become the rendering callbacks for later regeneration
events. Requests joining a generation already in progress share that result.
An existing subscription retains its initial reactive scope and is not recreated
for a cache miss, including when the initial generation creates a relative
`site_root`. Static file hits do not run rendering callbacks.

Generation work is keyed by the publication directory entry: the physical
parent directory plus the entry's stored spelling as listed by that directory,
without following a final symlink. A missing entry keeps its requested
spelling. Where the directory demonstrably folds ASCII case, spellings fold so
that aliases of one future or replaced entry share one generation and one
regeneration subscription. Distinct symlinks to one target, including
hard-linked symlink entries, are separate work. Not covered: non-ASCII case or
Unicode-normalization aliases of a still-missing entry, and directories whose
case rule cannot be observed (an empty directory at a volume root); such
aliases run separate generations until an entry exists.

URL aliases that map to one HTML file, such as `/x/` and `/x/index`, share that
artifact. Static rendering and its context must describe shared content; use
dynamic SSR for representations that vary by visitor or request.

The [`isr_startup` example](https://github.com/AlexeyMatskevich/leptos_ntex/blob/master/examples/isr_startup.rs) demonstrates this sequence
and a bounded refresh notification channel:

```sh
cargo run --locked --example isr_startup -- target/isr-site
# In another terminal:
curl -X POST http://127.0.0.1:3000/refresh
```

Visit `http://127.0.0.1:3000/`, request a refresh, and reload after regeneration.
The endpoint returns 202 while work is pending; rapid changes are coalesced.
The revision is in-memory demo data and resets after restart. Restarting the
command refreshes the saved page and recreates the listener in the new process.

To serve existing artifacts without startup generation or the refresh endpoint:

```sh
cargo run --locked --example isr_startup -- target/isr-site --prebuilt
```

`--prebuilt` only changes the example's startup. It does not provide a general
no-generation mode: Static handlers can still attempt on-demand rendering when
an artifact is missing or inconsistent. Deploy complete HTML/metadata/lock
artifacts and enforce filesystem permissions separately for read-only storage.
Valid prebuilt artifacts are served without installing a regeneration subscription.

The default has no automatic disk quota or eviction. To opt into admission
limits, open one `StaticRoutePolicy` per process and bind it to the route
listings or their generator before that site root starts generating. Listings
and generators produced by one `generate_route_list*` call share a runtime, as
do all listings and generators composed by hand; binding the policy through
`with_static_policy` or `configure_routes` on either side governs both.
`StaticRoutePolicy::open` may be awaited on any executor: without a running ntex
System the installation runs inline. `StaticStorageLimits`
sets logical named-file bytes and namespace entries for cooperating processes
using the same site root. `StaticWorkLimits` sets process-local active renders,
live regeneration streams and waiting callers. Every limit defaults to `None`;
zero refuses acquisition of that resource. Handles opened independently share
the persisted storage policy but have independent work counters.

The [`isr_startup` example](https://github.com/AlexeyMatskevich/leptos_ntex/blob/master/examples/isr_startup.rs)
also demonstrates policy installation and `try_generate`:

```sh
cargo run --locked --example isr_startup -- target/bounded-isr --bounded
```

Its limits are demonstration values, not recommended capacities or library
defaults. Choose limits for your route set, assets and publication peak.
`try_generate` reports completed paths and typed failures; earlier artifacts and
subscriptions survive a later failure. The existing `generate` method retains
its unit return type and logs failures. A refused capacity on an HTTP cache miss
returns `503`; a configuration fault, such as a publisher that is not bound to
the installed policy, returns `500`. Cache hits continue serving existing
artifacts. No eviction is performed. An ISR trigger waiting for a render slot is
coalesced and resumes when a slot becomes available; filesystem admission
failures are reported and require a later trigger or cache miss after the
condition has been resolved. An error render, such as a `404`, holds its
regeneration subscription like a successful one, so that a later trigger can
publish the page once it exists.

Storage accounting includes existing assets, metadata, hidden locks, staging,
crash remnants and missing parent directories. It reserves the publication peak
while both old and new files exist. This bounds logical file lengths and named
entries, not physical disk blocks, unlinked files still open by readers, or
arbitrary memory allocations made by application rendering. Cached reads do not
scan storage. Managed publications take an exclusive root lock and wait for one
another; the site inventory is scanned once per process and then kept exact
from the directories a publication touches, and a publication generation in the
control record tells cooperating processes when their cached inventory is
stale. Installing a policy never waits behind a publisher and reports `Busy`
instead. The render permit remains held until blocking I/O actually ends, even
after request cancellation. Unlimited publishers use the same root control
file, without the inventory scan, and refuse writing into a managed root unless
they bind its policy.

Installing a storage policy currently requires Unix. The policy is immutable
and stored in `.leptos-static-policy.lock`; all cooperating writers for that root
must use this adapter version and matching limits. Do not remove or replace
that file while publishers are running. Independent managed roots must not
overlap; install only while no publisher uses an overlapping, different root.
Installation coordinates publishers using exactly the same root. External
writers and overlapping roots cannot be used to obtain a global disk quota.
Opening a policy may create the root and control file before reporting an error.
The default serving path continues to support prebuilt read-only artifacts.
The serving helpers do not expose hidden metadata files.

### Runtime and context

Wrap the selected ntex runner in `RequestRuntime`, as in the quick start.
It creates a local request scope for the system and every worker. Handlers
registered without such a scope report the misconfiguration once and answer
every affected request with `500` rather than panicking the worker. The same
wrapper supports ntex's default Neon backend and its Tokio backend; enable
`features = ["tokio"]` on the application's `ntex` dependency to select Tokio.
`RequestRuntime` preserves that selection and does not install a Tokio executor.

`Request` is a clonable, transferable handle to a native request owned by that
scope. Access the native request through `request.with(|http| ...)`: access on
another thread or after scope closure returns `RequestAccessError`. The callback
must finish before its borrowed request data can be released. Return owned data
when it is needed later, for example `request.with(|http| http.path().to_owned())`.
There is no `Deref`/`DerefMut` access. These are breaking changes to the request
context API and application startup; see the migration example below.

The final handle releases its native entry on the origin thread. A final drop on
another thread wakes the runner to collect it; scope shutdown releases remaining
entries even if an application retained handles or the runtime retained a pending
task. Scope closure makes those handles unavailable. This does not forcibly free
native `HttpRequest` clones explicitly obtained by application code.

For synchronous embedding, create a local `RequestScope` before constructing a
`Request`. Its `collect()` method processes foreign-thread retirements; dropping
it closes all of its entries. A manual scope must enclose the request's actual
work. Tokio's `SystemRunner::run_local` bypasses the configured runner, so it also
needs an explicit scope that lives outside the awaited operation. Scopes are
thread-wide, so enclose the whole local run, rather than one yielding request
future that shares the thread with other requests. Such embedding
must arrange `collect()` calls while running if foreign drops should be reclaimed
before scope closure.

Native requests, response bodies and the server-function `SendWrapper` values
still require polling, access and destruction on their origin thread. In
particular, the `Send` return type of `handle_response_inner` does not permit
moving that future onto `tokio::spawn` or a different thread pool.

```rust
use leptos_ntex_unofficial::{Request, RequestScope};

let scope = RequestScope::new();
let native = ntex::web::test::TestRequest::with_uri("/account").to_http_request();
let request = Request::new(&native);
// Previously: request.path().to_owned()
let path = request.with(|http| http.path().to_owned()).unwrap();
assert_eq!(path, "/account");
drop(scope);
assert!(request.with(|http| http.path().to_owned()).is_err());
```

`Request::new` requires an active scope; `Request::try_new` reports its absence
without panicking. `into_inner` returns an owned native request on the origin
thread, and `try_into_inner` reports unavailable access. The application assumes
responsibility for that native value, including its thread and destruction.

Executor installation and server-function registration are process-wide.
`try_init_executor()` detects an already selected executor; it does not validate
that the calling thread is currently running a compatible runtime. Register
server functions consistently across apps sharing a process.

Leptos currently suppresses resource loading with process-wide state while it
enumerates routes. Concurrent enumeration and active SSR in separate apps can
leave a resource pending. Enumerating routes during application startup avoids
that overlap; it is an upstream limitation, not an isolation guarantee supplied
by this adapter.

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
cargo test --locked
cargo test --locked --all-features
cargo clippy --locked --all-targets --all-features -- -D warnings
RUSTDOCFLAGS="-D warnings" cargo doc --locked --all-features --no-deps
```

See [`CONTRIBUTING.md`](https://github.com/AlexeyMatskevich/leptos_ntex/blob/master/CONTRIBUTING.md) for the repository workflow and
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

* MIT license ([LICENSE-MIT](https://github.com/AlexeyMatskevich/leptos_ntex/blob/master/LICENSE-MIT) or <https://opensource.org/licenses/MIT>)
* Apache License 2.0 ([LICENSE-APACHE](https://github.com/AlexeyMatskevich/leptos_ntex/blob/master/LICENSE-APACHE) or <https://www.apache.org/licenses/LICENSE-2.0>)

at your option.

## Contributing

Bug reports and pull requests are welcome at
<https://github.com/AlexeyMatskevich/leptos_ntex>.

Unless explicitly stated otherwise, any contribution intentionally submitted
for inclusion in this crate by you, as defined in the Apache-2.0 license, shall
be dual licensed as above, without any additional terms or conditions.
