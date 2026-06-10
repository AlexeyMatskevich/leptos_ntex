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
use std::sync::{Arc, OnceLock};

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
    // Canonicalize the served directory ONCE (lazily, on the first request
    // that finds it on disk) and reuse it, instead of re-resolving the whole
    // root realpath on every asset request. Same lifetime/symlink semantics
    // as `ntex_files::Files::new`, which canonicalizes its base at
    // construction: a deploy that swaps the root symlink needs a restart.
    let canon_root: Arc<OnceLock<PathBuf>> = Arc::new(OnceLock::new());
    ntex::web::scope(prefix.clone()).route(
        "/{tail}*",
        Route::<Err>::new()
            .guard(ntex::web::guard::Any(ntex::web::guard::Get()).or(ntex::web::guard::Head()))
            .to(move |req: HttpRequest| {
                let dir = dir.clone();
                let prefix = prefix.clone();
                let canon_root = canon_root.clone();
                async move {
                    let raw_path = req
                        .uri()
                        .path()
                        .strip_prefix(&prefix)
                        .unwrap_or("/")
                        .to_owned();
                    let encodings = accepted_encodings(&req);
                    let opened = ntex::rt::spawn_blocking(move || {
                        let canon_root = cached_canon_root(&canon_root, &dir)?;
                        open_static_file(&canon_root, &raw_path, encodings)
                    })
                    .await
                    .unwrap_or_else(|join_err| {
                        warn_blocking_join_failed("static asset open task", &join_err);
                        None
                    });

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
///     .route("/{tail}*", file_and_error_handler::<_, ntex::web::DefaultError>(shell));
/// # }
/// ```
///
/// # Routing pattern
///
/// Register with the ntex tail pattern **`/{tail}*`**, not actix-web's
/// `/{tail:.*}`. In ntex, `{name:.*}` matches only a *single* path segment, so
/// `/{tail:.*}` would return `404` for every nested request — `/assets/app.css`,
/// `/.well-known/acme-challenge/<token>`, a client-side-routed deep link —
/// before this handler ever runs. `{tail}*` is ntex's cross-segment tail match
/// and is required for this catch-all to see nested paths.
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

/// RFC 8615 well-known URI prefix. The dotfile guards in [`safe_subpath`] and
/// in `static_path` (`crate::static_routes`) reject every decoded path segment
/// beginning with `.` — so `.env`, `.htaccess`, and friends stay hidden —
/// EXCEPT this exact segment, so ACME challenges and `security.txt` under
/// `/.well-known/...` can be served. Only the literal `.well-known` segment is
/// exempt: nested dotfiles (`/.well-known/.secret`) and `..` traversal
/// (`/.well-known/../etc`) remain rejected.
pub(crate) const WELL_KNOWN_SEGMENT: &str = ".well-known";

/// Whether a decoded path segment beginning with `.` must be rejected by the
/// dotfile guard. `.`, `..`, and every dotfile are blocked; only the exact
/// [`WELL_KNOWN_SEGMENT`] is allowed through.
pub(crate) fn is_blocked_dot_segment(segment: &str) -> bool {
    segment.starts_with('.') && segment != WELL_KNOWN_SEGMENT
}

/// Resolves a URL path to a safe absolute filesystem path under `site_root`,
/// returning `None` if the request attempts to escape the root or reference
/// hidden entries.
///
/// Rejects `..` (parent), dotfiles (`.env`), NUL bytes, Windows backslashes,
/// and any non-`Normal` path component. Percent-decodes each URL segment
/// before comparison, so `%2e%2e` and similar encodings cannot bypass the
/// filter. Finally canonicalizes the resolved path and verifies that it stays
/// under the already-canonical `site_root` — defense against symlink-escape
/// and against `Path::join` replacing the root when the request contains an
/// absolute path.
///
/// This is blocking I/O (`canonicalize` for the target). Callers must run it
/// on a blocking executor via [`ntex::rt::spawn_blocking`].
fn safe_subpath(canon_root: &Path, raw_path: &str) -> Option<PathBuf> {
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
        // Blocks `..` and every dotfile, but lets the exact `.well-known`
        // segment through (RFC 8615) — see `is_blocked_dot_segment`.
        if is_blocked_dot_segment(s) || s.contains('\0') {
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
    // path; the same `.well-known` exemption applies here.
    if !rel.components().all(|c| {
        matches!(c, Component::Normal(part) if !is_blocked_dot_segment(&part.to_string_lossy()))
    }) {
        return None;
    }
    let candidate = canon_root.join(&rel);
    let canon_target = candidate.canonicalize().ok()?;
    canon_target.starts_with(canon_root).then_some(canon_target)
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

#[derive(Clone, Copy, Debug, Default)]
struct AcceptedEncodings {
    /// Effective q-weight the client gave `br` — `Some` only when accepted
    /// (an explicit token or the `*` wildcard, with q > 0).
    br: Option<f32>,
    /// Effective q-weight the client gave `gzip` (same rules as `br`).
    gzip: Option<f32>,
}

/// A precompressed sibling variant of a static file (`<file>.br` /
/// `<file>.gz`) and the response metadata serving it implies.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Precompressed {
    Br,
    Gzip,
}

impl Precompressed {
    /// The on-disk sibling extension probed next to the plain file.
    fn extension(self) -> &'static str {
        match self {
            Self::Br => "br",
            Self::Gzip => "gz",
        }
    }

    fn content_encoding(self) -> ContentEncoding {
        match self {
            Self::Br => ContentEncoding::Br,
            Self::Gzip => ContentEncoding::Gzip,
        }
    }

    /// The `Content-Encoding` header value sent with the variant.
    fn header_value(self) -> &'static str {
        match self {
            Self::Br => "br",
            Self::Gzip => "gzip",
        }
    }
}

/// The accepted precompressed variants, most preferred first: ordered by the
/// client's q-weight (RFC 9110 §12.4.2), descending — a client sending
/// `gzip;q=1, br;q=0.1` gets gzip even though a brotli sibling exists. An
/// equal weight tie-breaks to `br` before `gzip` (the smaller transfer).
/// Refused (`q=0`) and unmentioned encodings are absent.
fn precompressed_preference(encodings: AcceptedEncodings) -> Vec<Precompressed> {
    let mut weighted: Vec<(f32, Precompressed)> = Vec::with_capacity(2);
    if let Some(q) = encodings.br {
        weighted.push((q, Precompressed::Br));
    }
    if let Some(q) = encodings.gzip {
        weighted.push((q, Precompressed::Gzip));
    }
    // Stable sort: on an equal q the push order above (br first) is the
    // tie-break. Accepted weights are > 0.0 and never NaN (the parser only
    // stores a finite q within 0..=1, and acceptance requires q > 0), so the
    // `partial_cmp` fallback cannot fire.
    weighted.sort_by(|a, b| b.0.partial_cmp(&a.0).unwrap_or(std::cmp::Ordering::Equal));
    weighted.into_iter().map(|(_, variant)| variant).collect()
}

fn accepted_encodings(req: &HttpRequest) -> AcceptedEncodings {
    let mut br_q = None;
    let mut gzip_q = None;
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
                // Accept only a valid qvalue (RFC 9110 §12.4.2: 0 to 1).
                // Out-of-range (`q=2`), non-finite, and unparseable (`q=abc`)
                // weights are all malformed alike and keep the default 1.0 —
                // an invalid declaration must not outrank a valid one.
                if name.trim().eq_ignore_ascii_case("q")
                    && let Ok(parsed) = value.trim().parse::<f32>()
                    && (0.0..=1.0).contains(&parsed)
                {
                    q = parsed;
                }
            }

            if token.eq_ignore_ascii_case("br") {
                br_q = Some(q);
            } else if token.eq_ignore_ascii_case("gzip") {
                gzip_q = Some(q);
            } else if token == "*" {
                wildcard_q = Some(q);
            }
        }
    }

    AcceptedEncodings {
        br: br_q.or(wildcard_q).filter(|q| *q > 0.0),
        gzip: gzip_q.or(wildcard_q).filter(|q| *q > 0.0),
    }
}

fn canonical_under(canon_root: &Path, path: &Path) -> Option<PathBuf> {
    let path = path.canonicalize().ok()?;
    path.starts_with(canon_root).then_some(path)
}

/// Returns the canonicalized `dir`, resolving it at most once and caching it
/// in `cache`. The root never changes for the life of the app, so re-running
/// the full realpath walk on every request is pure waste; the per-request
/// canonicalize of the *target* (in [`safe_subpath`]) is what actually
/// enforces the symlink-escape guard and stays per-request. Blocking I/O —
/// only call from a blocking executor. Caching is lazy because the directory
/// may not exist when the service is built.
fn cached_canon_root(cache: &OnceLock<PathBuf>, dir: &Path) -> Option<PathBuf> {
    if let Some(canon) = cache.get() {
        return Some(canon.clone());
    }
    let canon = dir.canonicalize().ok()?;
    // A racing worker may win `set`. Return the STORED value rather than this
    // call's `canon`, so every caller observes the one cached root even if the
    // root symlink changed between two concurrent first-resolutions. `set`
    // either stores `canon` or fails because a value is already present —
    // either way the cache now holds exactly one root, so `get` yields it.
    let _ = cache.set(canon);
    cache.get().cloned()
}

/// Logs a `spawn_blocking` join failure (a panic in the blocking task, or the
/// blocking pool shutting down) consistently with the rest of the crate — via
/// `tracing` when the feature is on, else stderr — so a serve-path degradation
/// (404 / regeneration / 500) is never completely silent.
pub(crate) fn warn_blocking_join_failed(context: &str, err: &impl std::fmt::Display) {
    #[cfg(feature = "tracing")]
    tracing::error!("{context}: blocking task failed: {err}");
    #[cfg(not(feature = "tracing"))]
    eprintln!("{context}: blocking task failed: {err}");
}

fn open_static_file(
    canon_root: &Path,
    raw_path: &str,
    accepted_encodings: AcceptedEncodings,
) -> Option<OpenedStaticFile> {
    let safe = safe_subpath(canon_root, raw_path)?;
    let mime = safe
        .extension()
        .and_then(|ext| ext.to_str())
        .map(ntex_files::file_extension_to_mime);

    for variant in precompressed_preference(accepted_encodings) {
        if let Some(compressed) =
            canonical_under(canon_root, &compressed_path(&safe, variant.extension()))
            && let Ok(mut file) = ntex_files::NamedFile::open(&compressed)
        {
            file = file.set_content_encoding(variant.content_encoding());
            if let Some(mime) = mime.clone() {
                file = file.set_content_type(mime);
            }
            return Some(OpenedStaticFile {
                file,
                content_encoding: Some(variant.header_value()),
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
    // Cache the canonical site root across requests (see `cached_canon_root`).
    let canon_root: Arc<OnceLock<PathBuf>> = Arc::new(OnceLock::new());
    let handler = move |req: HttpRequest, state: web::types::State<LeptosOptions>| {
        let shell = shell.clone();
        let additional_context = additional_context.clone();
        let options = state.get_ref().clone();
        let canon_root = canon_root.clone();
        async move {
            let site_root = PathBuf::from(&*options.site_root);
            let uri_path = req.uri().path().to_owned();

            let encodings = accepted_encodings(&req);
            let opened = ntex::rt::spawn_blocking(move || {
                let canon_root = cached_canon_root(&canon_root, &site_root)?;
                open_static_file(&canon_root, &uri_path, encodings)
            })
            .await
            .unwrap_or_else(|join_err| {
                warn_blocking_join_failed("file fallback open task", &join_err);
                None
            });

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

#[cfg(test)]
mod tests {
    use super::*;
    use lets_expect::lets_expect;

    // ----- safe_subpath dotfile guard: exhaustive spec -----------------
    // The file-fallback path resolver. After narrowing the dotfile guard for
    // RFC 8615, the exact `.well-known` segment resolves while every other
    // dotfile, `..` traversal, and dotfile nested *inside* `.well-known`
    // stays rejected. Exercises the guard through the real `canonicalize()`
    // (a throwaway site root is built per leaf, holding the two probe files).
    fn resolves_under_root(path: &str) -> bool {
        // Unique per call: a nanosecond timestamp alone collides when several
        // of these leaves run concurrently (the clock resolution is coarser
        // than the spawn interval), so two threads would share one temp root
        // and race each other's create/remove. A process-scoped atomic
        // counter makes the path unique regardless of timing.
        use std::sync::atomic::{AtomicU64, Ordering};
        static UNIQUE: AtomicU64 = AtomicU64::new(0);
        let root = std::env::temp_dir().join(format!(
            "leptos_ntex_safe_subpath_{}_{}",
            std::process::id(),
            UNIQUE.fetch_add(1, Ordering::Relaxed)
        ));
        std::fs::create_dir_all(root.join(".well-known")).unwrap();
        std::fs::write(root.join(".well-known/security.txt"), "ok").unwrap();
        std::fs::write(root.join(".env"), "secret").unwrap();
        let canon_root = root.canonicalize().unwrap();
        let resolved = safe_subpath(&canon_root, path).is_some();
        let _ = std::fs::remove_dir_all(&root);
        resolved
    }

    lets_expect! {
        expect(resolves_under_root(path)) as safe_subpath_resolution {
            let path = "/.well-known/security.txt";

            to serves_a_well_known_asset { be_true }

            when the_path_is_an_ordinary_dotfile {
                let path = "/.env";
                to is_rejected { be_false }
            }

            when a_dotfile_is_nested_inside_well_known {
                let path = "/.well-known/.secret";
                to is_rejected { be_false }
            }

            when a_traversal_hides_behind_well_known {
                let path = "/.well-known/../.env";
                to is_rejected { be_false }
            }
        }
    }

    // ----- Accept-Encoding negotiation: ordered-preference spec ---------
    // `accepted_encodings` parses the request's `Accept-Encoding` into
    // per-encoding q-weights; `precompressed_preference` orders the accepted
    // ones, highest q first (RFC 9110 §12.4.2), tie-breaking br before gzip.
    // Axes (one context per non-default state): header presence; br token
    // absent/accepted/refused; gzip token likewise; relative q order (live
    // only when BOTH are accepted — implicit tie / explicit tie / either
    // direction); wildcard absent/accepted/refused/backfilling one missing
    // token; an unrelated token enabling neither (the case a `== "*"` →
    // `!= "*"` slip would wrongly route into the wildcard branch); token and
    // `q` case-insensitivity; a malformed, out-of-range, or non-finite q
    // falling back to 1.0; and the tokens spanning two header lines.
    fn preference(accept_encoding: &[&str]) -> Vec<&'static str> {
        let mut req = ntex::web::test::TestRequest::default();
        for value in accept_encoding {
            req = req.header(header::ACCEPT_ENCODING, *value);
        }
        precompressed_preference(accepted_encodings(&req.to_http_request()))
            .into_iter()
            .map(Precompressed::header_value)
            .collect()
    }

    lets_expect! {
        expect(preference(accept_encoding)) as the_encoding_preference {
            let accept_encoding: &[&str] = &["br, gzip"];

            to prefers_brotli_on_an_implicit_tie { equal(vec!["br", "gzip"]) }

            when no_accept_encoding_is_sent {
                let accept_encoding: &[&str] = &[];
                to accepts_neither { equal(Vec::<&str>::new()) }
            }

            when only_brotli_is_offered {
                let accept_encoding: &[&str] = &["br"];
                to accepts_brotli_alone { equal(vec!["br"]) }
            }

            when only_gzip_is_offered {
                let accept_encoding: &[&str] = &["gzip"];
                to accepts_gzip_alone { equal(vec!["gzip"]) }
            }

            when gzip_outweighs_brotli {
                let accept_encoding: &[&str] = &["gzip;q=1, br;q=0.1"];
                to prefers_gzip { equal(vec!["gzip", "br"]) }
            }

            when brotli_outweighs_gzip {
                let accept_encoding: &[&str] = &["br;q=0.9, gzip;q=0.2"];
                to prefers_brotli { equal(vec!["br", "gzip"]) }
            }

            when both_share_an_explicit_q {
                let accept_encoding: &[&str] = &["gzip;q=0.5, br;q=0.5"];
                to ties_break_to_brotli { equal(vec!["br", "gzip"]) }
            }

            when a_wildcard_is_offered {
                let accept_encoding: &[&str] = &["*"];
                to accepts_both { equal(vec!["br", "gzip"]) }
            }

            when a_wildcard_backfills_only_the_missing_token {
                let accept_encoding: &[&str] = &["br;q=0.2, *;q=0.9"];
                to ranks_the_explicit_q_below_the_wildcard {
                    equal(vec!["gzip", "br"])
                }
            }

            when an_unrelated_encoding_is_offered {
                let accept_encoding: &[&str] = &["deflate"];
                to accepts_neither { equal(Vec::<&str>::new()) }
            }

            when brotli_is_explicitly_refused {
                let accept_encoding: &[&str] = &["br;q=0, gzip"];
                to accepts_gzip_only { equal(vec!["gzip"]) }
            }

            when gzip_is_explicitly_refused {
                let accept_encoding: &[&str] = &["br, gzip;q=0"];
                to accepts_brotli_only { equal(vec!["br"]) }
            }

            when the_wildcard_is_refused {
                let accept_encoding: &[&str] = &["*;q=0"];
                to accepts_neither { equal(Vec::<&str>::new()) }
            }

            when tokens_are_mixed_case {
                let accept_encoding: &[&str] = &["BR, GZip;Q=0.5"];
                to matches_case_insensitively { equal(vec!["br", "gzip"]) }
            }

            when the_q_value_is_malformed {
                let accept_encoding: &[&str] = &["gzip;q=abc, br;q=0.5"];
                to treats_the_malformed_q_as_full_weight {
                    equal(vec!["gzip", "br"])
                }
            }

            when the_q_value_is_out_of_range {
                let accept_encoding: &[&str] = &["gzip;q=2, br;q=1"];
                to treats_it_as_malformed_and_ties_at_full_weight {
                    equal(vec!["br", "gzip"])
                }
            }

            when the_q_value_is_not_finite {
                let accept_encoding: &[&str] = &["gzip;q=nan, br;q=0.5"];
                to treats_it_as_malformed_and_keeps_full_weight {
                    equal(vec!["gzip", "br"])
                }
            }

            when the_encodings_span_two_header_lines {
                let accept_encoding: &[&str] = &["br", "gzip"];
                to merges_both_lines { equal(vec!["br", "gzip"]) }
            }
        }
    }
}
