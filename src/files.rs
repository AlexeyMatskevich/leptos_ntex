//! Static file serving: the `site_pkg_dir` service plus the
//! [`file_and_error_handler`] fallback route.

use futures::{StreamExt, stream::once};
use leptos::{IntoView, config::LeptosOptions};
use leptos_integration_utils::PinnedStream;
use leptos_meta::ServerMetaContext;
use ntex::http::StatusCode;
use ntex::web::{self, ErrorRenderer, HttpRequest, Route};
use ntex::web::error::StateExtractorError;
use std::path::{Component, Path, PathBuf};

use crate::render::provide_contexts;
use crate::request::Request;
use crate::response::{NtexResponse, ResponseOptions};
use crate::routes::ensure_executor_initialized;

/// Creates a file-serving [`ntex_files::Files`] service for the
/// `options.site_pkg_dir` directory under `options.site_root`.
///
/// Handy for registering the JS/WASM/CSS assets produced by `cargo-leptos`:
///
/// ```no_run
/// use ntex::web::App as NtexApp;
/// use leptos::config::LeptosOptions;
/// use leptos_ntex_unofficial::site_pkg_dir_service;
///
/// # fn example(options: LeptosOptions) {
/// let _app = NtexApp::new()
///     .state(options.clone())
///     .service(site_pkg_dir_service::<ntex::web::DefaultError>(&options));
/// # }
/// ```
///
/// The returned [`Files`](ntex_files::Files) can be further configured
/// (`.index_file(...)`, `.use_etag(...)`, etc.) before being mounted.
///
/// ### Custom error types
///
/// `Err::Container: From<ntex_files::FilesError>` is required at mount
/// time (via `.service(...)`), but `ntex_files::FilesError` is not a
/// publicly reachable type, so we cannot express that bound here. With
/// `ntex::web::DefaultError` it works out of the box. If you're using a
/// custom error renderer, either use the default for this subtree or
/// construct `ntex_files::Files::new(...)` manually with your own error
/// plumbing.
pub fn site_pkg_dir_service<Err>(options: &LeptosOptions) -> ntex_files::Files<Err>
where
    Err: ErrorRenderer,
{
    let pkg_segment = options.site_pkg_dir.trim_start_matches('/');
    let prefix = format!("/{pkg_segment}");
    let dir = format!("{}/{pkg_segment}", &*options.site_root);
    ntex_files::Files::new(&prefix, dir)
}

/// A GET [`Route`] that first tries to serve a file from `options.site_root`
/// matching the request URI, and falls back to rendering `shell` with a
/// `404 Not Found` status if the file is missing.
///
/// Useful as a catch-all after route registration:
///
/// ```no_run
/// use ntex::web::App as NtexApp;
/// use leptos_ntex_unofficial::file_and_error_handler;
/// use leptos::config::LeptosOptions;
/// # use leptos::prelude::*;
/// # fn shell(_: LeptosOptions) -> impl IntoView { "" }
///
/// # fn example(options: LeptosOptions) {
/// let _app = NtexApp::new()
///     .state(options)
///     .route("/{tail:.*}", file_and_error_handler::<_, ntex::web::DefaultError>(shell));
/// # }
/// ```
pub fn file_and_error_handler<IV, Err>(
    shell: impl Fn(LeptosOptions) -> IV + 'static + Clone + Send,
) -> Route<Err>
where
    IV: IntoView + 'static,
    Err: ErrorRenderer,
    Err::Container: From<StateExtractorError>,
{
    file_and_error_handler_with_context(|| {}, shell)
}

/// Resolves a URL path to a safe absolute filesystem path under `site_root`,
/// returning `None` if the request attempts to escape the root or reference
/// hidden entries.
///
/// Rejects `..` (parent), dotfiles (`.env`), NUL bytes, Windows backslashes,
/// and any non-`Normal` path component. Percent-decodes each URL segment
/// before comparison, so `%2e%2e` and similar encodings cannot bypass the
/// filter. Finally canonicalizes the resolved path and verifies that it
/// stays under the canonical `site_root` — defense against symlink-escape
/// and against `Path::join` replacing the root when the request contains an
/// absolute path.
///
/// This is blocking I/O (`canonicalize`). Callers must run it on a blocking
/// executor via [`ntex::rt::spawn_blocking`].
fn safe_subpath(site_root: &Path, raw_path: &str) -> Option<PathBuf> {
    let mut rel = PathBuf::new();
    for segment in raw_path.split('/') {
        if segment.is_empty() {
            continue;
        }
        let decoded = percent_encoding::percent_decode_str(segment)
            .decode_utf8()
            .ok()?;
        let s = decoded.as_ref();
        if s == "." {
            continue;
        }
        if s == ".." || s.starts_with('.') || s.contains('\0') {
            return None;
        }
        if cfg!(windows) && s.contains('\\') {
            return None;
        }
        rel.push(s);
    }
    // Reject anything the percent-decoded segment smuggled in (e.g. a
    // decoded absolute path or `..` lodged inside a segment). Only
    // `Component::Normal` is acceptable for a relative user-controlled
    // path.
    if !rel.components().all(|c| matches!(c, Component::Normal(_))) {
        return None;
    }
    let candidate = site_root.join(&rel);
    let canon_root = site_root.canonicalize().ok()?;
    let canon_target = candidate.canonicalize().ok()?;
    canon_target.starts_with(&canon_root).then_some(canon_target)
}

/// Variant of [`file_and_error_handler`] that injects additional values
/// into the reactive context before rendering the shell on a miss.
pub fn file_and_error_handler_with_context<IV, Err>(
    additional_context: impl Fn() + 'static + Clone + Send,
    shell: impl Fn(LeptosOptions) -> IV + 'static + Clone + Send,
) -> Route<Err>
where
    IV: IntoView + 'static,
    Err: ErrorRenderer,
    Err::Container: From<StateExtractorError>,
{
    ensure_executor_initialized();
    let handler = move |req: HttpRequest, state: web::types::State<LeptosOptions>| {
        let shell = shell.clone();
        let additional_context = additional_context.clone();
        let options = state.get_ref().clone();
        async move {
            let site_root = PathBuf::from(&*options.site_root);
            let uri_path = req.uri().path().to_owned();

            let opened = ntex::rt::spawn_blocking(move || {
                let safe = safe_subpath(&site_root, &uri_path)?;
                ntex_files::NamedFile::open(&safe).ok()
            })
            .await
            .ok()
            .flatten();

            if let Some(named) = opened {
                return named.into_response(&req);
            }

            let res_options = ResponseOptions::default();
            res_options.set_status(StatusCode::NOT_FOUND);
            let (meta_context, meta_output) = ServerMetaContext::new();
            let req_ctx = Request::new(&req);

            let cx = {
                let meta_context = meta_context.clone();
                let res_options = res_options.clone();
                move || {
                    provide_contexts(req_ctx, &meta_context, &res_options);
                    additional_context();
                }
            };

            let app_fn = move || shell(options);

            let mut res = <NtexResponse as leptos_integration_utils::ExtendResponse>::from_app(
                app_fn,
                meta_output,
                cx,
                res_options,
                |app, chunks, _supports_ooo| {
                    Box::pin(async move {
                        let app = if cfg!(feature = "islands-router") {
                            app.to_html_stream_in_order_branching()
                        } else {
                            app.to_html_stream_in_order()
                        };
                        let app = app.collect::<String>().await;
                        let chunks = chunks();
                        Box::pin(once(async move { app }).chain(chunks))
                            as PinnedStream<String>
                    })
                },
                true,
            )
            .await;

            if res.0.status() == StatusCode::OK {
                *res.0.status_mut() = StatusCode::NOT_FOUND;
            }

            res.take()
        }
    };
    // HEAD mirrors GET: same handler, ntex's h1 writer strips the body.
    // Using a union guard instead of `.method()` because `take_guards`
    // turns `.method()` into AND-combined guards — incompatible with
    // multi-method routes.
    Route::<Err>::new()
        .guard(
            ntex::web::guard::Any(ntex::web::guard::Get())
                .or(ntex::web::guard::Head()),
        )
        .to(handler)
}
