// ---------------------------------------------------------------------------
// In-crate tests. Kept in the crate (rather than in tests/integration.rs)
// because they touch crate-private helpers — `handle_response_inner`, the
// `ensure_executor_initialized` regression probe, direct access to the
// registration table — that are not exported from the public API.
// ---------------------------------------------------------------------------

use leptos::prelude::*;
use leptos_meta::{MetaTags, provide_meta_context};
use leptos_router::{
    SsrMode,
    components::{Route, Router, Routes},
    path,
    static_routes::StaticRoute,
};

#[component]
fn StaticApp() -> impl IntoView {
    provide_meta_context();

    view! {
        <Router>
            <main>
                <Routes fallback=|| view! { <h1>"Not Found"</h1> }>
                    <Route
                        path=path!("/")
                        ssr=SsrMode::Static(StaticRoute::new())
                        view=|| view! { <h1>"Static Home"</h1> }
                    />
                    <Route
                        path=path!("/about")
                        ssr=SsrMode::Static(StaticRoute::new())
                        view=|| view! { <h1>"Static About"</h1> }
                    />
                </Routes>
            </main>
        </Router>
    }
}

#[component]
fn StaticHeaderApp() -> impl IntoView {
    provide_meta_context();

    view! {
        <Router>
            <main>
                <Routes fallback=|| view! { <h1>"Not Found"</h1> }>
                    <Route
                        path=path!("/headers")
                        ssr=SsrMode::Static(StaticRoute::new())
                        view=|| {
                            if let Some(res) = use_context::<crate::ResponseOptions>() {
                                res.set_status(ntex::http::StatusCode::CREATED);
                                res.insert_header(
                                    ntex::http::header::HeaderName::from_static("x-static-cache"),
                                    ntex::http::header::HeaderValue::from_static("preserved"),
                                );
                            }
                            view! { <h1>"Static Headers"</h1> }
                        }
                    />
                </Routes>
            </main>
        </Router>
    }
}

#[component]
fn MixedApp() -> impl IntoView {
    provide_meta_context();

    view! {
        <Router>
            <main>
                <Routes fallback=|| view! { <h1>"Not Found"</h1> }>
                    <Route
                        path=path!("/out")
                        view=|| view! { <h1>"OutOfOrder"</h1> }
                    />
                    <Route
                        path=path!("/in")
                        ssr=SsrMode::InOrder
                        view=|| view! { <h1>"InOrder"</h1> }
                    />
                    <Route
                        path=path!("/async")
                        ssr=SsrMode::Async
                        view=|| view! { <h1>"Async"</h1> }
                    />
                </Routes>
            </main>
        </Router>
    }
}

fn mixed_shell() -> impl IntoView {
    view! {
        <!DOCTYPE html>
        <html lang="en">
            <head>
                <meta charset="utf-8"/>
                <MetaTags/>
            </head>
            <body>
                <MixedApp/>
            </body>
        </html>
    }
}

#[component]
fn UnitApp() -> impl IntoView {
    provide_meta_context();
    view! {
        <Router>
            <main>
                <Routes fallback=|| view! { <h1>"Not Found"</h1> }>
                    <Route
                        path=path!("/")
                        view=|| view! {
                            <>
                                <h1>"Leptos over ntex"</h1>
                                <p>"SSR is rendered through an adapter derived from leptos_actix."</p>
                            </>
                        }
                    />
                    <Route
                        path=path!("/about")
                        view=|| view! {
                            <>
                                <h1>"About"</h1>
                                <p>"This route is generated from the Leptos router and served by ntex."</p>
                            </>
                        }
                    />
                </Routes>
            </main>
        </Router>
    }
}

fn unit_shell() -> impl IntoView {
    view! {
        <!DOCTYPE html>
        <html lang="en">
            <head>
                <meta charset="utf-8"/>
                <meta name="viewport" content="width=device-width, initial-scale=1"/>
                <MetaTags/>
            </head>
            <body>
                <UnitApp/>
            </body>
        </html>
    }
}

// A view that reads an async `Resource` under `<Suspense>`. The whole point
// is that the three streaming modes render its `<Suspense>` DIFFERENTLY:
//   * OutOfOrder: emits the FALLBACK in the shell, then streams the resolved
//     fragment out of order (so the body contains BOTH the fallback marker
//     and an out-of-order replacement chunk).
//   * InOrder / Async: block until the resource resolves, so the body has the
//     RESOLVED content in place and NO fallback marker.
// Deleting the InOrder/Async match arm falls back to the OutOfOrder renderer,
// which a fallback-marker assertion then catches.
#[component]
fn SuspendedView() -> impl IntoView {
    let data = Resource::new(
        || (),
        |_| async move {
            // A real delay so the resource is still pending when the shell
            // renders. OutOfOrder then streams the shell + fallback first and
            // the resolved fragment later; InOrder/Async block for the value.
            ntex::time::sleep(ntex::time::Millis(50)).await;
            String::from("RESOLVED-CONTENT")
        },
    );
    view! {
        <Suspense fallback=move || view! { <span>"FALLBACK-MARKER"</span> }>
            {move || Suspend::new(async move {
                let value = data.await;
                view! { <span>{value}</span> }
            })}
        </Suspense>
    }
}

#[component]
fn SuspenseApp() -> impl IntoView {
    provide_meta_context();
    view! {
        <Router>
            <main>
                <Routes fallback=|| view! { <p>"Not Found"</p> }>
                    <Route path=path!("/out") ssr=SsrMode::OutOfOrder view=SuspendedView/>
                    <Route path=path!("/in") ssr=SsrMode::InOrder view=SuspendedView/>
                    <Route path=path!("/async") ssr=SsrMode::Async view=SuspendedView/>
                </Routes>
            </main>
        </Router>
    }
}

// Routes exercising the non-Static `PathSegment` kinds: a `:param` and a
// terminal `*splat`. The splat view must win over the router fallback for
// BOTH single-segment and nested URLs — the regression pinned here is the
// actix-style `{any:.*}` conversion, under which nested URLs fell through
// to the fallback.
#[component]
fn SplatApp() -> impl IntoView {
    provide_meta_context();
    view! {
        <Router>
            <main>
                <Routes fallback=|| view! { <h1>"Not Found"</h1> }>
                    <Route path=path!("/users/:id") view=|| view! { <h1>"User Param"</h1> } />
                    <Route path=path!("/files/*any") view=|| view! { <h1>"Splat Files"</h1> } />
                </Routes>
            </main>
        </Router>
    }
}

fn splat_shell() -> impl IntoView {
    view! {
        <!DOCTYPE html>
        <html lang="en">
            <head>
                <meta charset="utf-8"/>
                <MetaTags/>
            </head>
            <body>
                <SplatApp/>
            </body>
        </html>
    }
}

// Single-variant apps so the `PathSegment`-kind route-list spec can pin each
// kind in its own context: a `:param`, a terminal `*splat`, and an optional
// `:id?` (which `expand_optionals` rewrites into two listings before
// `to_ntex_path` runs).
#[component]
fn ParamRouteApp() -> impl IntoView {
    provide_meta_context();
    view! {
        <Router>
            <Routes fallback=|| view! { <h1>"Not Found"</h1> }>
                <Route path=path!("/users/:id") view=|| view! { <h1>"u"</h1> } />
            </Routes>
        </Router>
    }
}

#[component]
fn SplatRouteApp() -> impl IntoView {
    provide_meta_context();
    view! {
        <Router>
            <Routes fallback=|| view! { <h1>"Not Found"</h1> }>
                <Route path=path!("/files/*any") view=|| view! { <h1>"f"</h1> } />
            </Routes>
        </Router>
    }
}

#[component]
fn OptionalParamRouteApp() -> impl IntoView {
    provide_meta_context();
    view! {
        <Router>
            <Routes fallback=|| view! { <h1>"Not Found"</h1> }>
                <Route path=path!("/users/:id?") view=|| view! { <h1>"u"</h1> } />
            </Routes>
        </Router>
    }
}

#[component]
fn NestedApp() -> impl IntoView {
    provide_meta_context();
    view! {
        <Router>
            <main>
                <Routes fallback=|| view! { <p>"Not Found"</p> }>
                    <Route path=path!("/outer/inner") view=|| view! { <h1>"Nested"</h1> } />
                </Routes>
            </main>
        </Router>
    }
}

fn suspense_shell() -> impl IntoView {
    view! {
        <!DOCTYPE html>
        <html lang="en">
            <head>
                <meta charset="utf-8"/>
                <MetaTags/>
            </head>
            <body>
                <SuspenseApp/>
            </body>
        </html>
    }
}

#[server(
    name = EchoName,
    prefix = "/api",
    endpoint = "echo_name",
    server = crate::NtexServerFnBackend
)]
async fn echo_name(name: String) -> Result<String, ServerFnError> {
    Ok(format!("Hello, {name}"))
}

#[server(
    name = RedirectToAbout,
    prefix = "/api",
    endpoint = "redirect_to_about",
    server = crate::NtexServerFnBackend
)]
async fn redirect_to_about() -> Result<(), ServerFnError> {
    crate::redirect("/about");
    Ok(())
}

#[server(
    name = EchoWebsocket,
    prefix = "/api",
    endpoint = "echo_websocket",
    protocol = server_fn::Websocket<server_fn::codec::JsonEncoding, server_fn::codec::JsonEncoding>,
    server = crate::NtexServerFnBackend
)]
async fn echo_websocket(
    input: server_fn::BoxedStream<String, ServerFnError>,
) -> Result<server_fn::BoxedStream<String, ServerFnError>, ServerFnError> {
    Ok(input)
}

// Echo websocket whose OUTPUT stream carries a drop probe. The regression
// pinned through it: the websocket bridge task must observe peer
// disconnects — without that signal it keeps a clone of the input sender
// alive forever, the server-fn forwarder never sees EOF, and this stream
// (plus both channels and the task) leaks on every closed connection.
static WS_PROBE_OUTPUT_DROPPED: std::sync::atomic::AtomicBool =
    std::sync::atomic::AtomicBool::new(false);

#[server(
    name = LeakProbeWebsocket,
    prefix = "/api",
    endpoint = "leak_probe_websocket",
    protocol = server_fn::Websocket<server_fn::codec::JsonEncoding, server_fn::codec::JsonEncoding>,
    server = crate::NtexServerFnBackend
)]
async fn leak_probe_websocket(
    input: server_fn::BoxedStream<String, ServerFnError>,
) -> Result<server_fn::BoxedStream<String, ServerFnError>, ServerFnError> {
    use futures::StreamExt;

    struct DropProbe;
    impl Drop for DropProbe {
        fn drop(&mut self) {
            WS_PROBE_OUTPUT_DROPPED.store(true, std::sync::atomic::Ordering::SeqCst);
        }
    }

    let probe = DropProbe;
    let input: std::pin::Pin<
        Box<dyn futures::Stream<Item = Result<String, ServerFnError>> + Send>,
    > = input.into();
    Ok(input
        .map(move |item| {
            let _alive = &probe;
            item
        })
        .into())
}

#[server(
    name = ProbePath,
    prefix = "/api",
    endpoint = "probe_path",
    server = crate::NtexServerFnBackend
)]
async fn probe_path() -> Result<String, ServerFnError> {
    let req: ntex::web::HttpRequest = crate::extract()
        .await
        .map_err(|e| ServerFnError::new(e.to_string()))?;
    Ok(req.path().to_string())
}

#[server(
    name = MultiLocation,
    prefix = "/api",
    endpoint = "multi_location",
    server = crate::NtexServerFnBackend
)]
async fn multi_location() -> Result<(), ServerFnError> {
    let res = leptos::prelude::use_context::<crate::ResponseOptions>()
        .ok_or_else(|| ServerFnError::new("no ResponseOptions in context".to_string()))?;
    res.append_header(
        ntex::http::header::LOCATION,
        ntex::http::header::HeaderValue::from_static("/one"),
    );
    res.append_header(
        ntex::http::header::LOCATION,
        ntex::http::header::HeaderValue::from_static("/two"),
    );
    Ok(())
}

// ---------------------------------------------------------------------------
// Shared helper for the test submodules below.
// ---------------------------------------------------------------------------

/// Throwaway per-test site root under the system temp dir.
fn temp_site_root(name: &str) -> std::path::PathBuf {
    // A nanosecond timestamp alone collides when several tests -- or several
    // parallel `cargo-mutants` processes sharing the same temp dir -- request
    // a root within one clock tick, so the path carries the pid plus a
    // process-scoped atomic counter instead (the same pattern as
    // `files::resolves_under_root`).
    use std::sync::atomic::{AtomicU64, Ordering};
    static UNIQUE: AtomicU64 = AtomicU64::new(0);
    std::env::temp_dir().join(format!(
        "leptos_ntex_{name}_{}_{}",
        std::process::id(),
        UNIQUE.fetch_add(1, Ordering::Relaxed)
    ))
}

// ---------------------------------------------------------------------------
// Route-generation vs. render serialization (test-only workaround).
//
// Upstream leptos races on the PROCESS-GLOBAL `IS_SUPPRESSING_RESOURCE_LOAD`
// flag: `RouteList::generate` (run inside every `generate_route_list*`) flips
// it on while it walks the app, and any `Resource` whose FIRST poll lands in
// that window latches `pending()` forever — permanently hanging an
// in-order/async streaming render that creates a resource (e.g. the
// `SuspendedView` fixture). The bug is in `leptos_server`/`leptos_router`, not
// this crate, and reproduces with no ntex at all; it is NOT fixed here because
// a blocking lock across `.await` in the crate's own code could deadlock real
// single-threaded apps. Instead the test suite serializes the two sides:
// generation takes the WRITE side (brief, synchronous), and the only fixtures
// that build a `Resource` during render take the READ side, so a generation
// window can never overlap a resource's first poll. Drop this once the
// upstream flag is made thread-local / re-checked per poll (filed upstream).
pub(super) static ROUTE_GEN_VS_RENDER: std::sync::RwLock<()> = std::sync::RwLock::new(());

fn gen_write_guard() -> std::sync::RwLockWriteGuard<'static, ()> {
    ROUTE_GEN_VS_RENDER
        .write()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

/// Serialized [`generate_route_list`] — see [`ROUTE_GEN_VS_RENDER`].
pub(super) fn gen_route_list<IV>(
    app_fn: impl Fn() -> IV + 'static + Send + Clone,
) -> Vec<crate::NtexRouteListing>
where
    IV: leptos::IntoView + 'static,
{
    let _guard = gen_write_guard();
    crate::generate_route_list(app_fn)
}

/// Serialized [`generate_route_list_with_ssg`] — see [`ROUTE_GEN_VS_RENDER`].
pub(super) fn gen_route_list_with_ssg<IV>(
    app_fn: impl Fn() -> IV + 'static + Send + Clone,
) -> (Vec<crate::NtexRouteListing>, crate::StaticRouteGenerator)
where
    IV: leptos::IntoView + 'static,
{
    let _guard = gen_write_guard();
    crate::generate_route_list_with_ssg(app_fn)
}

/// Serialized [`generate_route_list_with_exclusions`] — see
/// [`ROUTE_GEN_VS_RENDER`].
pub(super) fn gen_route_list_with_exclusions<IV>(
    app_fn: impl Fn() -> IV + 'static + Send + Clone,
    excluded: Option<Vec<String>>,
) -> Vec<crate::NtexRouteListing>
where
    IV: leptos::IntoView + 'static,
{
    let _guard = gen_write_guard();
    crate::generate_route_list_with_exclusions(app_fn, excluded)
}

mod executor;
mod file_fallback;
mod rendering;
mod server_fn_http;
mod static_routes;
mod unit_specs;
mod websocket;
