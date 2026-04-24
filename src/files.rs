//! Static file serving: the `site_pkg_dir` service plus the
//! [`file_and_error_handler`] fallback route.

use leptos::{IntoView, config::LeptosOptions, context::provide_context, reactive::owner::Owner};
use leptos_integration_utils::ExtendResponse;
use leptos_meta::ServerMetaContext;
use ntex::http::{
    StatusCode,
    header::{self, ContentEncoding, HeaderValue},
};
use ntex::web::error::StateExtractorError;
use ntex::web::{self, ErrorRenderer, HttpRequest, HttpResponse, Route};
use std::path::{Component, Path, PathBuf};

use crate::render::{async_stream_builder, provide_contexts};
use crate::request::Request;
use crate::response::{NtexResponse, ResponseOptions};
use crate::routes::ensure_executor_initialized;

/// Creates a file-serving ntex scope for the
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
/// If `.br` / `.gz` siblings exist, they are served when the request's
/// `Accept-Encoding` allows them. File responses are still built with
/// [`ntex_files::NamedFile`], so MIME, ETag, Last-Modified, ranges, and
/// conditional requests remain delegated to `ntex-files`.
pub fn site_pkg_dir_service<Err>(options: &LeptosOptions) -> ntex::web::Scope<Err>
where
    Err: ErrorRenderer,
{
    let pkg_segment = options.site_pkg_dir.trim_start_matches('/');
    let prefix = format!("/{pkg_segment}");
    let dir = PathBuf::from(&*options.site_root).join(pkg_segment);
    ntex::web::scope(prefix.clone()).route(
        "/{tail:.*}",
        Route::<Err>::new()
            .guard(ntex::web::guard::Any(ntex::web::guard::Get()).or(ntex::web::guard::Head()))
            .to(move |req: HttpRequest| {
                let dir = dir.clone();
                let prefix = prefix.clone();
                async move {
                    let raw_path = req
                        .uri()
                        .path()
                        .strip_prefix(&prefix)
                        .unwrap_or("/")
                        .to_owned();
                    let accepts_br = accepts_encoding(&req, "br");
                    let accepts_gzip = accepts_encoding(&req, "gzip");
                    let opened = ntex::rt::spawn_blocking(move || {
                        open_static_file(&dir, &raw_path, accepts_br, accepts_gzip)
                    })
                    .await
                    .ok()
                    .flatten();

                    if let Some(opened) = opened {
                        let mut res = opened.file.into_response(&req);
                        if let Some(content_encoding) = opened.content_encoding {
                            ensure_precompressed_headers(&mut res, content_encoding);
                        }
                        res
                    } else {
                        HttpResponse::NotFound().finish()
                    }
                }
            }),
    )
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
        if s.contains('/') || s.contains('\\') {
            return None;
        }
        rel.push(s);
    }
    // Reject anything the percent-decoded segment smuggled in (e.g. a
    // decoded absolute path or `..` lodged inside a segment). Only
    // `Component::Normal` is acceptable for a relative user-controlled
    // path.
    if !rel
        .components()
        .all(|c| matches!(c, Component::Normal(part) if !part.to_string_lossy().starts_with('.')))
    {
        return None;
    }
    let candidate = site_root.join(&rel);
    let canon_root = site_root.canonicalize().ok()?;
    let canon_target = candidate.canonicalize().ok()?;
    canon_target
        .starts_with(&canon_root)
        .then_some(canon_target)
}

struct OpenedStaticFile {
    file: ntex_files::NamedFile,
    content_encoding: Option<&'static str>,
}

fn compressed_path(path: &Path, extension: &str) -> PathBuf {
    let mut path = path.as_os_str().to_os_string();
    path.push(".");
    path.push(extension);
    PathBuf::from(path)
}

fn accepts_encoding(req: &HttpRequest, encoding: &str) -> bool {
    let mut explicit_q = None;
    let mut wildcard_q = None;

    for value in req.headers().get_all(header::ACCEPT_ENCODING) {
        let Ok(value) = value.to_str() else {
            continue;
        };
        for item in value.split(',') {
            let mut parts = item.split(';').map(str::trim);
            let token = parts.next().unwrap_or_default();
            let mut q = 1.0;
            for part in parts {
                let Some((name, value)) = part.split_once('=') else {
                    continue;
                };
                if name.trim().eq_ignore_ascii_case("q")
                    && let Ok(parsed) = value.trim().parse::<f32>()
                {
                    q = parsed;
                }
            }

            if token.eq_ignore_ascii_case(encoding) {
                explicit_q = Some(q);
            } else if token == "*" {
                wildcard_q = Some(q);
            }
        }
    }

    explicit_q.or(wildcard_q).is_some_and(|q| q > 0.0)
}

fn canonical_under(canon_root: &Path, path: &Path) -> Option<PathBuf> {
    let path = path.canonicalize().ok()?;
    path.starts_with(canon_root).then_some(path)
}

fn open_static_file(
    site_root: &Path,
    raw_path: &str,
    accepts_br: bool,
    accepts_gzip: bool,
) -> Option<OpenedStaticFile> {
    let safe = safe_subpath(site_root, raw_path)?;
    let canon_root = site_root.canonicalize().ok()?;
    let mime = safe
        .extension()
        .and_then(|ext| ext.to_str())
        .map(ntex_files::file_extension_to_mime);

    for (extension, encoding, header_value, accepted) in [
        ("br", ContentEncoding::Br, "br", accepts_br),
        ("gz", ContentEncoding::Gzip, "gzip", accepts_gzip),
    ] {
        if !accepted {
            continue;
        }
        if let Some(compressed) = canonical_under(&canon_root, &compressed_path(&safe, extension))
            && let Ok(mut file) = ntex_files::NamedFile::open(&compressed)
        {
            file = file.set_content_encoding(encoding);
            if let Some(mime) = mime.clone() {
                file = file.set_content_type(mime);
            }
            return Some(OpenedStaticFile {
                file,
                content_encoding: Some(header_value),
            });
        }
    }

    Some(OpenedStaticFile {
        file: ntex_files::NamedFile::open(&safe).ok()?,
        content_encoding: None,
    })
}

fn ensure_precompressed_headers(res: &mut ntex::web::HttpResponse, content_encoding: &'static str) {
    res.headers_mut().insert(
        header::CONTENT_ENCODING,
        HeaderValue::from_static(content_encoding),
    );

    let already_present = res
        .headers()
        .get_all(header::VARY)
        .filter_map(|value| value.to_str().ok())
        .flat_map(|value| value.split(','))
        .any(|value| {
            let value = value.trim();
            value == "*" || value.eq_ignore_ascii_case("accept-encoding")
        });
    if !already_present {
        res.headers_mut()
            .append(header::VARY, HeaderValue::from_static("Accept-Encoding"));
    }
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

            let accepts_br = accepts_encoding(&req, "br");
            let accepts_gzip = accepts_encoding(&req, "gzip");
            let opened = ntex::rt::spawn_blocking(move || {
                open_static_file(&site_root, &uri_path, accepts_br, accepts_gzip)
            })
            .await
            .ok()
            .flatten();

            if let Some(opened) = opened {
                let res_options = ResponseOptions::default();
                let req_ctx = Request::new(&req);
                let owner = Owner::new();
                return owner.with(|| {
                    provide_context(req_ctx);
                    provide_context(res_options.clone());
                    additional_context();

                    let mut res = opened.file.into_response(&req);
                    if let Some(content_encoding) = opened.content_encoding {
                        ensure_precompressed_headers(&mut res, content_encoding);
                    }
                    let mut res = NtexResponse(res);
                    res.extend_response(&res_options);
                    res.take()
                });
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
                async_stream_builder,
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
        .guard(ntex::web::guard::Any(ntex::web::guard::Get()).or(ntex::web::guard::Head()))
        .to(handler)
}
