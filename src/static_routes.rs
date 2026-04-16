//! Static (SSG) route generation and the catch-all
//! [`handle_static_route`] used by [`LeptosRoutes`](crate::LeptosRoutes).
//!
//! Hosts [`StaticRouteGenerator`] (which writes every
//! [`SsrMode::Static`](leptos_router::SsrMode) route to disk), the
//! per-process cache of captured `ResponseOptions`, and the on-disk path
//! helpers.

use futures::StreamExt;
use leptos::{
    IntoView,
    config::LeptosOptions,
    context::use_context,
    prelude::expect_context,
    reactive::{computed::ScopedFuture, owner::Owner},
};
use leptos_integration_utils::{ExtendResponse, PinnedFuture, build_response, static_file_path};
use leptos_meta::ServerMetaContext;
use leptos_router::{
    RouteList,
    static_routes::{RegenerationFn, ResolvedStaticPath},
};
use ntex::http::StatusCode;
use ntex::web::{self, ErrorRenderer, HttpRequest, HttpResponse, Route};
use ntex::web::error::StateExtractorError;
use or_poisoned::OrPoisoned;
use std::{
    collections::HashMap,
    fs,
    future::Future,
    io,
    path::Path,
    sync::{LazyLock, RwLock},
};

use crate::render::{async_stream_builder, provide_contexts};
use crate::request::Request;
use crate::response::{NtexResponse, ResponseOptions};
use crate::routes::ensure_executor_initialized;

/// Allows generating prerendered static HTML for every [`SsrMode::Static`](leptos_router::SsrMode)
/// route in the application.
///
/// Produced by [`generate_route_list_with_ssg`](crate::generate_route_list_with_ssg).
/// Call [`StaticRouteGenerator::generate`] once the [`LeptosOptions`] are
/// known — typically at the end of a pre-build step that runs before the
/// server starts serving traffic.
#[allow(clippy::type_complexity)]
pub struct StaticRouteGenerator(
    // Kept alive so that any context values provided during generation stay
    // valid for the duration of the static rendering pipeline.
    #[allow(dead_code)] Owner,
    Box<dyn FnOnce(&LeptosOptions) -> PinnedFuture<()> + Send>,
);

impl StaticRouteGenerator {
    pub(crate) fn render_route<IV: IntoView + 'static>(
        path: String,
        app_fn: impl Fn() -> IV + Clone + Send + 'static,
        additional_context: impl Fn() + Clone + Send + 'static,
    ) -> impl Future<Output = (Owner, String)> {
        let (meta_context, meta_output) = ServerMetaContext::new();
        let additional_context = {
            let add_context = additional_context.clone();
            let request_path = if path.is_empty() {
                "/".to_string()
            } else {
                path.clone()
            };
            move || {
                let mock_req = ntex::web::test::TestRequest::with_uri(&request_path)
                    .header("Accept", "text/html")
                    .to_http_request();
                let res_options = ResponseOptions::default();
                provide_contexts(Request::new(&mock_req), &meta_context, &res_options);
                add_context();
            }
        };

        let (owner, stream) = build_response(app_fn.clone(), additional_context, async_stream_builder, false);
        let sc = owner.shared_context().unwrap();

        async move {
            let stream = stream.await;
            while let Some(pending) = sc.await_deferred() {
                pending.await;
            }

            let html = meta_output
                .inject_meta_context(stream)
                .await
                .collect::<String>()
                .await;
            (owner, html)
        }
    }

    /// Creates a new static route generator from the given list of route
    /// definitions.
    pub fn new<IV>(
        routes: &RouteList,
        app_fn: impl Fn() -> IV + Clone + Send + 'static,
        additional_context: impl Fn() + Clone + Send + 'static,
    ) -> Self
    where
        IV: IntoView + 'static,
    {
        let owner = Owner::new();
        Self(owner.clone(), {
            let routes = routes.clone();
            Box::new(move |options| {
                let options = options.clone();
                let app_fn = app_fn.clone();
                let additional_context = additional_context.clone();
                owner.with(|| {
                    additional_context();
                    Box::pin(ScopedFuture::new(routes.generate_static_files(
                        move |path: &ResolvedStaticPath| {
                            Self::render_route(path.to_string(), app_fn.clone(), additional_context.clone())
                        },
                        move |path: &ResolvedStaticPath, owner: &Owner, html: String| {
                            let options = options.clone();
                            let path = path.to_owned();
                            let response_options = owner.with(use_context);
                            async move {
                                write_static_route(
                                    &options,
                                    response_options,
                                    path.as_ref(),
                                    html,
                                )
                                .await
                            }
                        },
                        was_404,
                    )))
                })
            })
        })
    }

    /// Generates the prerendered HTML files into the configured site root.
    pub async fn generate(self, options: &LeptosOptions) {
        (self.1)(options).await
    }
}

/// Per-process cache of [`ResponseOptions`] captured when a static route
/// is first rendered, keyed by the URL path. Read on cache hits to replay
/// the original status/headers alongside the on-disk HTML.
///
/// ⚠ **Scope is the entire process.** If an application hosts multiple
/// [`LeptosOptions`] with overlapping route paths (e.g. two sites that
/// both expose `/index.html`), entries collide — the last writer wins.
/// Mirrors `leptos_actix` behaviour; in practice apps run a single
/// `LeptosOptions` instance per process and the collision is theoretical.
static STATIC_HEADERS: LazyLock<RwLock<HashMap<String, ResponseOptions>>> =
    LazyLock::new(Default::default);

fn was_404(owner: &Owner) -> bool {
    let resp = owner.with(|| expect_context::<ResponseOptions>());
    let status = resp.0.read().or_poisoned().status;
    status == Some(StatusCode::NOT_FOUND)
}

fn static_path(options: &LeptosOptions, path: &str) -> String {
    if path != "/" && path.ends_with('/') {
        static_file_path(options, &format!("{path}index"))
    } else {
        static_file_path(options, path)
    }
}

async fn write_static_route(
    options: &LeptosOptions,
    response_options: Option<ResponseOptions>,
    path: &str,
    html: String,
) -> Result<(), io::Error> {
    if let Some(options) = response_options {
        STATIC_HEADERS.write().or_poisoned().insert(path.to_string(), options);
    }

    let file_path = static_path(options, path);
    ntex::rt::spawn_blocking(move || {
        let path = Path::new(&file_path);
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)?;
        }
        fs::write(path, html)?;
        Ok::<(), io::Error>(())
    })
    .await
    .map_err(io::Error::other)?
}

pub(crate) fn handle_static_route<IV, Err>(
    additional_context: impl Fn() + 'static + Clone + Send,
    app_fn: impl Fn() -> IV + Clone + Send + 'static,
    regenerate: Vec<RegenerationFn>,
) -> Route<Err>
where
    Err: ErrorRenderer,
    Err::Container: From<StateExtractorError>,
    IV: IntoView + 'static,
{
    ensure_executor_initialized();
    let handler = move |req: HttpRequest, options: web::types::State<LeptosOptions>| {
        let app_fn = app_fn.clone();
        let additional_context = additional_context.clone();
        let regenerate = regenerate.clone();
        async move {
            let options = options.get_ref().clone();
            let orig_path = req.uri().path().to_string();
            let path = static_path(&options, &orig_path);
            let path_buf = Path::new(&path).to_path_buf();

            // `Path::exists()` is a synchronous `stat(2)` — keep it off
            // the arbiter so the io loop isn't blocked on a slow FS
            // (NFS, FUSE, etc.). Mirrors the `spawn_blocking` usage in
            // `write_static_route` and the `NamedFile::open` below.
            let check_path = path_buf.clone();
            let exists = ntex::rt::spawn_blocking(move || check_path.exists())
                .await
                .unwrap_or(false);

            let (response_options, html) = if !exists {
                let path = ResolvedStaticPath::new(&orig_path);
                let (owner, html) = path
                    .build(
                        move |path: &ResolvedStaticPath| {
                            StaticRouteGenerator::render_route(
                                path.to_string(),
                                app_fn.clone(),
                                additional_context.clone(),
                            )
                        },
                        move |path: &ResolvedStaticPath, owner: &Owner, html: String| {
                            let options = options.clone();
                            let path = path.to_owned();
                            let response_options = owner.with(use_context);
                            async move {
                                write_static_route(
                                    &options,
                                    response_options,
                                    path.as_ref(),
                                    html,
                                )
                                .await
                            }
                        },
                        was_404,
                        regenerate,
                    )
                    .await;
                (owner.with(use_context::<ResponseOptions>), html)
            } else {
                let headers = STATIC_HEADERS.read().or_poisoned().get(&orig_path).cloned();
                (headers, None)
            };

            // `SsrMode::Static` routes always emit HTML, so we hardcode
            // `text/html` on the regeneration path (where the body is held
            // in memory). On a cache hit we open the on-disk file via
            // `NamedFile`, which derives MIME from the extension and adds
            // `Last-Modified`/`ETag`. A custom `RegenerationFn` that
            // returns non-HTML bytes would get `text/html` on the first
            // (regenerating) request and the correct MIME thereafter — in
            // practice Leptos's SSG only produces HTML, so this matches
            // `leptos_actix` / `leptos_axum` behavior.
            let mut res = NtexResponse(match html {
                Some(html) => HttpResponse::Ok().content_type("text/html").body(html),
                None => {
                    let opened = ntex::rt::spawn_blocking(move || {
                        ntex_files::NamedFile::open(&path_buf)
                    })
                    .await;
                    match opened {
                        Ok(Ok(named)) => named.into_response(&req),
                        Ok(Err(err)) => HttpResponse::InternalServerError().body(err.to_string()),
                        Err(err) => HttpResponse::InternalServerError().body(err.to_string()),
                    }
                }
            });

            if let Some(options) = response_options {
                res.extend_response(&options);
            }

            res.take()
        }
    };
    // HEAD mirrors GET semantics (RFC 9110 §9.3.2); ntex strips the body
    // at the h1 writer when the request is HEAD. `.method()` is
    // unusable for multi-method routes because `take_guards` converts
    // it into an AND-combined MethodGuard on the owning Resource.
    Route::<Err>::new()
        .guard(
            ntex::web::guard::Any(ntex::web::guard::Get())
                .or(ntex::web::guard::Head()),
        )
        .to(handler)
}
