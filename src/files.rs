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
/// streaming remain provided by `ntex-files`. The adapter evaluates request
/// preconditions before ranges, including HEAD and If-Range semantics.
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
    let canon_root: Arc<RootCache> = Arc::new(OnceLock::new());
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
                        let root = cached_site_root(&canon_root, &dir)?;
                        open_static_file(&root, &raw_path, encodings)
                    })
                    .await
                    .unwrap_or_else(|join_err| {
                        warn_blocking_join_failed("static asset open task", &join_err);
                        None
                    });

                    if let Some(opened) = opened {
                        let opened = match opened {
                            Ok(opened) => opened,
                            Err(()) => {
                                let mut response = HttpResponse::NotAcceptable().finish();
                                ensure_encoding_vary(&mut response);
                                return response;
                            }
                        };
                        let mut res = file_response(opened.file, &req, StatusCode::OK);
                        if let Some(content_encoding) = opened.content_encoding {
                            ensure_precompressed_headers(&mut res, content_encoding);
                        }
                        ensure_encoding_vary(&mut res);
                        crate::stream::terminate_on_body_error(&req, res)
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
    if !canon_target.is_file() {
        // Rejects a bare-root request (`""` / `"/"`, where every split
        // segment is empty and `rel` stays empty so `candidate` resolves to
        // `canon_root` itself) as well as any other directory target, so the
        // caller never hands a directory fd to `NamedFile::open` — which
        // would succeed on Unix (`std::fs::File::open` opens directories)
        // and be treated as an openable file instead of falling through to
        // the 404 shell. Matches `ntex_files::Files`, which explicitly
        // guards `path.is_dir()` before opening.
        return None;
    }
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
    identity: bool,
    /// Adapter policy: an explicit identity weight competes with compressed
    /// codings. An unspecified weight leaves identity as a fallback, unless
    /// the wildcard excludes it with `*;q=0`.
    identity_q: Option<f32>,
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
    let mut identity_q = None;

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
                // an invalid declaration must not outrank a valid one. The
                // finite/range policy lives in the shared `parse_qvalue` so the
                // `Accept` (HTML) parser cannot drift from this one.
                if name.trim().eq_ignore_ascii_case("q")
                    && let Some(parsed) = crate::config::parse_qvalue(value)
                {
                    q = parsed;
                }
            }

            if token.eq_ignore_ascii_case("br") {
                br_q = Some(q);
            } else if token.eq_ignore_ascii_case("gzip") {
                gzip_q = Some(q);
            } else if token.eq_ignore_ascii_case("identity") {
                identity_q = Some(q);
            } else if token == "*" {
                wildcard_q = Some(q);
            }
        }
    }

    AcceptedEncodings {
        br: br_q.or(wildcard_q).filter(|q| *q > 0.0),
        gzip: gzip_q.or(wildcard_q).filter(|q| *q > 0.0),
        identity: identity_q.map_or(wildcard_q != Some(0.0), |q| q > 0.0),
        identity_q,
    }
}

/// Returns the canonicalized `dir`, resolving it at most once and caching it
/// in `cache`. The root never changes for the life of the app, so re-running
/// the full realpath walk on every request is unnecessary. Per-request target
/// resolution proposes a relative path; the final capability-relative open
/// enforces containment even if the namespace changes afterward. Blocking I/O —
/// only call from a blocking executor. Caching is lazy because the directory
/// may not exist when the service is built.
/// The served root, resolved and opened once: its canonical path anchors
/// request-path validation and its directory capability opens every file, so
/// a request pays neither a realpath walk nor a directory open.
type RootCache = OnceLock<Arc<crate::fs_boundary::SiteRoot>>;

fn cached_site_root(cache: &RootCache, dir: &Path) -> Option<Arc<crate::fs_boundary::SiteRoot>> {
    if let Some(root) = cache.get() {
        return Some(root.clone());
    }
    let root = Arc::new(crate::fs_boundary::SiteRoot::open(dir).ok()?);
    // A racing worker may win `set`. Return the STORED value rather than this
    // call's root, so every caller observes the one cached root even if the
    // root symlink changed between two concurrent first-resolutions. `set`
    // either stores `root` or fails because a value is already present —
    // either way the cache now holds exactly one root, so `get` yields it.
    let _ = cache.set(root);
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
    root: &crate::fs_boundary::SiteRoot,
    raw_path: &str,
    accepted_encodings: AcceptedEncodings,
) -> Option<Result<OpenedStaticFile, ()>> {
    let safe = safe_subpath(root.canonical(), raw_path)?;
    let mime = safe
        .extension()
        .and_then(|ext| ext.to_str())
        .map(ntex_files::file_extension_to_mime);

    for variant in precompressed_preference(accepted_encodings) {
        let weight = match variant {
            Precompressed::Br => accepted_encodings.br,
            Precompressed::Gzip => accepted_encodings.gzip,
        }
        .unwrap_or_default();
        if accepted_encodings
            .identity_q
            .is_some_and(|identity| identity > weight)
        {
            continue;
        }
        let compressed = compressed_path(&safe, variant.extension());
        if let Ok(mut file) = root
            .open_file(&compressed)
            .and_then(|file| ntex_files::NamedFile::from_file(file, &safe))
        {
            file = file.set_content_encoding(variant.content_encoding());
            if let Some(mime) = mime.clone() {
                file = file.set_content_type(mime);
            }
            return Some(Ok(OpenedStaticFile {
                file,
                content_encoding: Some(variant.header_value()),
            }));
        }
    }

    if !accepted_encodings.identity {
        return Some(Err(()));
    }
    Some(Ok(OpenedStaticFile {
        file: ntex_files::NamedFile::from_file(root.open_file(&safe).ok()?, &safe).ok()?,
        content_encoding: None,
    }))
}

// Callers add `Vary: Accept-Encoding` once for every negotiated response,
// including identity and refusals, so this only records the selected coding.
fn ensure_precompressed_headers(res: &mut ntex::web::HttpResponse, content_encoding: &'static str) {
    res.headers_mut().insert(
        header::CONTENT_ENCODING,
        HeaderValue::from_static(content_encoding),
    );
}

fn ensure_encoding_vary(res: &mut HttpResponse) {
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

/// Applies HTTP preconditions before ranges, containing ntex-files 3.2.0's
/// empty-range panic and its HEAD/If-Range/precondition-order defects. Keep the
/// request projection until a dependency update passes the regression matrix.
pub(crate) fn file_response(
    mut file: ntex_files::NamedFile,
    req: &HttpRequest,
    base_status: StatusCode,
) -> HttpResponse {
    use ntex::http::Method;
    let head_or_get = req.method() == Method::GET || req.method() == Method::HEAD;
    if base_status.is_redirection()
        && [
            header::RANGE,
            header::IF_NONE_MATCH,
            header::IF_MATCH,
            header::IF_MODIFIED_SINCE,
            header::IF_UNMODIFIED_SINCE,
        ]
        .iter()
        .any(|name| req.headers().contains_key(name))
    {
        return HttpResponse::build(base_status).finish();
    }
    let unsupported_modified = file
        .file()
        .metadata()
        .and_then(|meta| meta.modified())
        .is_ok_and(|time| {
            time.duration_since(std::time::UNIX_EPOCH)
                .map_or(true, |duration| duration.as_secs() >= 253_402_300_800)
        });
    if unsupported_modified {
        // ntex-files and httpdate cannot format dates outside 1970..9999.
        // Omit validators rather than panic or invent a modification time.
        file = file.use_etag(false).use_last_modified(false);
    }
    if [
        header::RANGE,
        header::IF_RANGE,
        header::IF_MATCH,
        header::IF_NONE_MATCH,
        header::IF_UNMODIFIED_SINCE,
        header::IF_MODIFIED_SINCE,
    ]
    .iter()
    .all(|name| !req.headers().contains_key(name))
    {
        let mut response = file.into_response(req);
        if response.status() == StatusCode::OK {
            *response.status_mut() = base_status;
        }
        return response;
    }

    let mut request = ntex::web::test::TestRequest::default().method(req.method().clone());
    for (name, value) in req.headers().iter() {
        if !base_status.is_success()
            || (name != header::IF_MODIFIED_SINCE && name != header::IF_UNMODIFIED_SINCE)
            || (name == header::IF_UNMODIFIED_SINCE && req.headers().contains_key(header::IF_MATCH))
            || (name == header::IF_MODIFIED_SINCE && !head_or_get)
        {
            continue;
        }
        request = request.header(name.clone(), value.clone());
    }
    // Entity-tag fields have list semantics; NamedFile only reads one
    // field line, so preserve every member by presenting one combined value.
    if base_status.is_success() {
        for name in [header::IF_MATCH, header::IF_NONE_MATCH] {
            let mut combined = Vec::new();
            for value in req.headers().get_all(&name) {
                if !combined.is_empty() {
                    combined.extend_from_slice(b", ");
                }
                combined.extend_from_slice(value.as_bytes());
            }
            if !combined.is_empty() {
                let Ok(value) = HeaderValue::from_bytes(&combined) else {
                    return HttpResponse::BadRequest().finish();
                };
                request = request.header(name, value);
            }
        }
    }
    let projected = request.to_http_request();
    let range = req
        .headers()
        .get(header::RANGE)
        .filter(|_| req.method() == Method::GET && base_status == StatusCode::OK)
        .filter(|value| {
            value
                .to_str()
                .ok()
                .and_then(|text| text.split_once('='))
                .is_none_or(|(unit, _)| unit.eq_ignore_ascii_case("bytes"))
        });
    if let Some(range) = range {
        let clone = match file
            .file()
            .try_clone()
            .and_then(|clone| ntex_files::NamedFile::from_file(clone, file.path()))
        {
            Ok(clone) => clone
                .use_etag(!unsupported_modified)
                .use_last_modified(!unsupported_modified),
            Err(_) => return HttpResponse::InternalServerError().finish(),
        };
        let preview = clone.into_response(&projected);
        if matches!(
            preview.status(),
            StatusCode::NOT_MODIFIED | StatusCode::PRECONDITION_FAILED
        ) {
            return file.into_response(&projected);
        }
        let if_range_matches = req.headers().get(header::IF_RANGE).is_none_or(|condition| {
            let Ok(condition) = condition.to_str() else {
                return false;
            };
            if condition.starts_with('"') || condition.starts_with("W/") {
                !condition.starts_with("W/")
                    && preview
                        .headers()
                        .get(header::ETAG)
                        .is_some_and(|etag| etag.as_bytes() == condition.as_bytes())
            } else {
                use ntex_files::header::{self as file_header, Header};
                let parsed = condition.parse::<file_header::HttpDate>();
                match parsed {
                    Ok(since) => preview
                        .headers()
                        .get(header::LAST_MODIFIED)
                        .and_then(|value| {
                            file_header::LastModified::parse_header(&file_header::Raw::from(
                                value.as_bytes(),
                            ))
                            .ok()
                        })
                        // RFC 9110 section 13.1.5 requires an exact date match
                        // for If-Range, unlike If-Unmodified-Since.
                        .is_some_and(|file_header::LastModified(modified)| modified == since),
                    _ => false,
                }
            }
        });
        if if_range_matches {
            let size = match file.file().metadata() {
                Ok(meta) => meta.len(),
                Err(_) => return HttpResponse::InternalServerError().finish(),
            };
            let Ok(text) = range.to_str() else {
                return HttpResponse::BadRequest().finish();
            };
            let normalized = text
                .split_once('=')
                .map(|(_, ranges)| format!("bytes={ranges}"))
                .unwrap_or_else(|| text.to_owned());
            let parsed = if size == 0 {
                None
            } else {
                ntex_files::HttpRange::parse(&normalized, size).ok()
            };
            let selected = parsed
                .as_ref()
                .and_then(|ranges| ranges.first())
                .filter(|range| {
                    range.length > 0 && range.start < size && range.length <= size - range.start
                });
            let Some(selected) = selected else {
                let mut response = HttpResponse::RangeNotSatisfiable();
                response.header(header::CONTENT_RANGE, format!("bytes */{size}"));
                return response.finish();
            };
            let mut ranged = ntex::web::test::TestRequest::default().method(Method::GET);
            for (name, value) in projected.headers().iter() {
                ranged = ranged.header(name.clone(), value.clone());
            }
            // ntex-files supports a single range. Normalize the selected range
            // to avoid suffix underflow/empty vectors in its parser.
            ranged = ranged.header(
                header::RANGE,
                format!(
                    "bytes={}-{}",
                    selected.start,
                    selected.start + selected.length - 1
                ),
            );
            let mut response = file.into_response(&ranged.to_http_request());
            if response.status() == StatusCode::OK {
                *response.status_mut() = StatusCode::PARTIAL_CONTENT;
            }
            return response;
        }
    }
    let mut response = file.into_response(&projected);
    if response.status() == StatusCode::NOT_MODIFIED && !head_or_get {
        *response.status_mut() = StatusCode::PRECONDITION_FAILED;
    } else if response.status() == StatusCode::OK {
        *response.status_mut() = base_status;
    }
    response
}

/// Variant of [`file_and_error_handler`] that injects additional values
/// into the reactive context for a file response, a negotiation refusal, or
/// the rendered shell on a miss. The callback runs once after context setup;
/// supplied [`ResponseOptions`] apply to each of these responses.
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
    crate::request::check_registration_scope();
    // Cache the opened site root across requests (see `cached_site_root`).
    let canon_root: Arc<RootCache> = Arc::new(OnceLock::new());
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
                let root = cached_site_root(&canon_root, &site_root)?;
                open_static_file(&root, &uri_path, encodings)
            })
            .await
            .unwrap_or_else(|join_err| {
                warn_blocking_join_failed("file fallback open task", &join_err);
                None
            });

            let req_ctx = match crate::request::scoped_request(&req) {
                Ok(request) => request,
                Err(response) => return response,
            };
            if let Some(opened) = opened {
                let res_options = ResponseOptions::default();
                let _restore = crate::owner::RestoreOwner::capture();
                let owner = crate::owner::OwnerCleanup::new(Owner::new());
                return owner.owner().with(|| {
                    provide_context(req_ctx);
                    provide_context(res_options.clone());
                    additional_context();

                    let mut res = match opened {
                        Ok(opened) => {
                            let mut res = file_response(opened.file, &req, StatusCode::OK);
                            if let Some(content_encoding) = opened.content_encoding {
                                ensure_precompressed_headers(&mut res, content_encoding);
                            }
                            res
                        }
                        Err(()) => HttpResponse::NotAcceptable().finish(),
                    };
                    ensure_encoding_vary(&mut res);
                    let mut res = NtexResponse(res);
                    res.extend_response(&res_options);
                    crate::stream::terminate_on_body_error(&req, res.take())
                });
            }

            let res_options = ResponseOptions::default();
            res_options.set_status(StatusCode::NOT_FOUND);
            let (meta_context, meta_output) = ServerMetaContext::new();

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

    // ----- ensure_precompressed_headers: Vary dedup --------------------
    // The precompressed path sets Content-Encoding and advertises
    // `Vary: Accept-Encoding`, but must NOT duplicate an Accept-Encoding a
    // prior layer already set, and must leave a wildcard `Vary: *` alone. A
    // fresh response (default) can't distinguish the dedup branch from a
    // blanket append — only a pre-existing Vary does — so both non-default
    // contexts carry one. `existing_vary` is a list of separately-appended
    // Vary header LINES (not one comma-joined value) — `.header()` on the
    // builder appends, so this also exercises `get_all`'s cross-line
    // iteration, matching how multiple middlewares each calling `.append`
    // would show up on the wire.
    fn vary_after_precompress(existing_vary: &'static [&'static str]) -> Vec<String> {
        let mut builder = ntex::web::HttpResponse::Ok();
        for v in existing_vary {
            builder.header(header::VARY, HeaderValue::from_static(v));
        }
        let mut res = builder.finish();
        ensure_precompressed_headers(&mut res, "br");
        // Every negotiated response passes through ensure_encoding_vary once.
        ensure_encoding_vary(&mut res);
        res.headers()
            .get_all(header::VARY)
            .filter_map(|v| v.to_str().ok())
            .flat_map(|v| v.split(','))
            .map(|v| v.trim().to_string())
            .collect()
    }

    lets_expect! {
        expect(vary_after_precompress(existing)) as the_precompressed_vary_header {
            let existing: &'static [&'static str] = &[];

            to appends_accept_encoding_once {
                equal(vec!["Accept-Encoding".to_string()])
            }

            when accept_encoding_is_already_advertised {
                let existing: &'static [&'static str] = &["Accept-Encoding"];
                to is_not_duplicated {
                    equal(vec!["Accept-Encoding".to_string()])
                }
            }

            when a_wildcard_vary_is_present {
                let existing: &'static [&'static str] = &["*"];
                to leaves_the_wildcard_untouched {
                    equal(vec!["*".to_string()])
                }
            }

            // The dedup is case-insensitive and scans every comma-separated
            // token, so neither a lowercase value nor `Accept-Encoding` buried
            // in a list gets a duplicate appended.
            when accept_encoding_is_already_advertised_in_lowercase {
                let existing: &'static [&'static str] = &["accept-encoding"];
                to is_not_duplicated_case_insensitively {
                    equal(vec!["accept-encoding".to_string()])
                }
            }

            when accept_encoding_is_one_token_in_a_comma_list {
                let existing: &'static [&'static str] = &["Origin, Accept-Encoding"];
                to is_not_duplicated_within_the_list {
                    equal(vec!["Origin".to_string(), "Accept-Encoding".to_string()])
                }
            }

            // An empty existing Vary value is a distinct no-type-domain
            // state: an already-set-but-vacuous header some middleware might
            // leave behind. It contributes no dedup-relevant token, so the
            // append still happens — asserted as an exact two-element
            // result (not just "contains Accept-Encoding"), pinning that the
            // empty split token is preserved rather than silently dropped.
            when the_existing_vary_value_is_empty {
                let existing: &'static [&'static str] = &[""];
                to still_appends_accept_encoding {
                    equal(vec!["".to_string(), "Accept-Encoding".to_string()])
                }
            }

            when the_existing_vary_value_is_whitespace_only {
                let existing: &'static [&'static str] = &["   "];
                to still_appends_accept_encoding {
                    equal(vec!["".to_string(), "Accept-Encoding".to_string()])
                }
            }

            // Two separately-appended Vary header LINES (as multiple
            // middlewares calling `.append` would produce), not one
            // comma-joined value — exercises `get_all`'s iteration across
            // distinct header instances rather than just `split(',')`
            // within a single line.
            when accept_encoding_is_already_advertised_on_a_separate_vary_line {
                let existing: &'static [&'static str] = &["Origin", "Accept-Encoding"];
                to is_not_duplicated_across_lines {
                    equal(vec!["Origin".to_string(), "Accept-Encoding".to_string()])
                }
            }
        }
    }

    // ----- safe_subpath dotfile guard: exhaustive spec -----------------
    // The file-fallback path resolver. After narrowing the dotfile guard for
    // RFC 8615, the exact `.well-known` segment resolves while every other
    // dotfile, `..` traversal, and dotfile nested *inside* `.well-known`
    // stays rejected. Exercises the guard through the real `canonicalize()`
    // (a throwaway site root is built per leaf, holding the two probe files).
    fn resolves_under_root(path: &str) -> bool {
        let root = crate::tests::temp_site_root("safe_subpath");
        std::fs::create_dir_all(root.join(".well-known")).unwrap();
        std::fs::write(root.join(".well-known/security.txt"), "ok").unwrap();
        std::fs::write(root.join(".env"), "secret").unwrap();
        // Materialize the rejection-leaf targets on disk too, so a rejected
        // path is rejected by the GUARD, not merely because the file is absent
        // (which `canonicalize()` would turn into `None` regardless): a nested
        // dotfile under `.well-known`, and a non-dotfile reachable only via a
        // `..` traversal out of `.well-known`.
        std::fs::write(root.join(".well-known/.secret"), "nested-secret").unwrap();
        std::fs::write(root.join("public.txt"), "public").unwrap();
        // A literal backslash is a legal filename byte on Unix filesystems
        // (unlike `/`), so this materializes the backslash-guard rejection
        // leaf's target on disk too: without the guard, the decoded segment
        // `foo\bar` would actually resolve to this file instead of being
        // rejected by "file absent". Unix-only: on Windows, `\` in a `Path`
        // component is a directory separator, so `root.join("foo\\bar")`
        // would try to create a `foo` subdirectory instead of a same-level
        // file named `foo\bar`, and the backslash leaf's own rejection would
        // pass vacuously there regardless (the guard itself is not
        // platform-specific — only this stronger fixture is).
        #[cfg(unix)]
        std::fs::write(root.join("foo\\bar"), "backslash-target").unwrap();
        let canon_root = root.canonicalize().unwrap();
        safe_subpath(&canon_root, path).is_some()
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
                // Target a NON-dotfile (`public.txt`) reachable only by escaping
                // `.well-known` via `..`, so the rejection is driven by the
                // traversal guard rather than by the target also being a
                // dotfile — `public.txt` exists on disk, so a dropped `..`
                // guard would actually resolve it.
                let path = "/.well-known/../public.txt";
                to is_rejected { be_false }
            }

            // Regression: every split segment of a bare root is empty and
            // skipped, so `rel` stayed empty and `candidate` resolved to
            // `canon_root` itself — a directory, which `canonicalize()`
            // and `starts_with` both accept trivially. Without the
            // `is_file()` guard this returned `Some(canon_root)`, and
            // `open_static_file` would hand that directory to
            // `NamedFile::open`, which succeeds on Unix.
            when the_path_is_the_bare_root_with_a_leading_slash {
                let path = "/";
                to is_rejected { be_false }
            }

            when the_path_is_a_bare_root_with_no_leading_slash {
                let path = "";
                to is_rejected { be_false }
            }

            // The documented backslash-rejection guard (`s.contains('\\')`)
            // has a dedicated branch but was never exercised by any leaf: a
            // percent-encoded backslash smuggled inside a segment must be
            // rejected just like a literal `..` traversal would be.
            when a_percent_encoded_backslash_is_smuggled_in_a_segment {
                let path = "/foo%5Cbar";
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
        expect(preference(accept_encoding)) as the_offered_encoding_names {
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
            when an_unrelated_encoding_is_offered {
                let accept_encoding: &[&str] = &["deflate"];
                to accepts_neither { equal(Vec::<&str>::new()) }
            }
        }
        expect(preference(accept_encoding)) as the_relative_encoding_weights {
            let accept_encoding: &[&str] = &["br, gzip"];
            to prefers_brotli_on_an_implicit_tie { equal(vec!["br", "gzip"]) }
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
            when brotli_is_explicitly_refused {
                let accept_encoding: &[&str] = &["br;q=0, gzip"];
                to accepts_gzip_only { equal(vec!["gzip"]) }
            }
            when gzip_is_explicitly_refused {
                let accept_encoding: &[&str] = &["br, gzip;q=0"];
                to accepts_brotli_only { equal(vec!["br"]) }
            }
        }
        expect(preference(accept_encoding)) as the_encoding_wildcard {
            let accept_encoding: &[&str] = &["*"];
            to accepts_both { equal(vec!["br", "gzip"]) }
            when a_wildcard_backfills_only_the_missing_token {
                let accept_encoding: &[&str] = &["br;q=0.2, *;q=0.9"];
                to ranks_the_explicit_q_below_the_wildcard {
                    equal(vec!["gzip", "br"])
                }
            }
            when the_wildcard_is_refused {
                let accept_encoding: &[&str] = &["*;q=0"];
                to accepts_neither { equal(Vec::<&str>::new()) }
            }
            when brotli_is_refused_but_a_wildcard_is_offered {
                let accept_encoding: &[&str] = &["br;q=0, *;q=1"];
                to keeps_brotli_refused_and_accepts_gzip { equal(vec!["gzip"]) }
            }
            when gzip_is_refused_but_a_wildcard_is_offered {
                let accept_encoding: &[&str] = &["gzip;q=0, *;q=1"];
                to keeps_gzip_refused_and_accepts_brotli { equal(vec!["br"]) }
            }
        }
        expect(preference(accept_encoding)) as the_encoding_header_spelling {
            let accept_encoding: &[&str] = &["br, gzip"];
            to recognizes_canonical_tokens { equal(vec!["br", "gzip"]) }
            when tokens_are_mixed_case {
                let accept_encoding: &[&str] = &["BR, GZip;Q=0.5"];
                to matches_case_insensitively { equal(vec!["br", "gzip"]) }
            }
            when the_encodings_span_two_header_lines {
                let accept_encoding: &[&str] = &["br", "gzip"];
                to merges_both_lines { equal(vec!["br", "gzip"]) }
            }
            when the_quality_param_name_is_uppercase {
                let accept_encoding: &[&str] = &["br;q=0.9, gzip;Q=0.1"];
                to honours_the_uppercase_q_name { equal(vec!["br", "gzip"]) }
            }
        }
        expect(preference(accept_encoding)) as the_malformed_encoding_quality {
            let accept_encoding: &[&str] = &["gzip;q=abc, br;q=0.5"];
            to treats_the_malformed_q_as_full_weight { equal(vec!["gzip", "br"]) }
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
        }
        expect(preference(accept_encoding)) as the_repeated_encoding_token {
            let accept_encoding: &[&str] = &["br;q=0.9, br;q=0"];
            to takes_the_last_weight_and_refuses { equal(Vec::<&str>::new()) }
            when a_token_is_refused_then_re_offered {
                let accept_encoding: &[&str] = &["br;q=0, br;q=0.9"];
                to takes_the_last_weight_and_accepts { equal(vec!["br"]) }
            }
        }
        expect(preference(accept_encoding)) as the_empty_encoding_list_item {
            let accept_encoding: &[&str] = &[",gzip"];
            to ignores_the_leading_empty_item { equal(vec!["gzip"]) }
            when the_header_has_a_trailing_comma {
                let accept_encoding: &[&str] = &["gzip,"];
                to ignores_the_trailing_empty_item_and_does_not_backfill_brotli {
                    equal(vec!["gzip"])
                }
            }
            when the_header_has_a_whitespace_only_item {
                let accept_encoding: &[&str] = &["gzip,   "];
                to ignores_the_whitespace_only_item_and_does_not_backfill_brotli {
                    equal(vec!["gzip"])
                }
            }
        }
    }

    // A header line whose bytes fail `HeaderValue::to_str` (not valid
    // visible ASCII) must be SKIPPED — not treated as a reason to abandon
    // parsing the other, well-formed header lines. Constructed directly via
    // `HeaderValue::from_bytes`, since a `&str` literal can never carry
    // invalid bytes.
    fn preference_with_raw_headers(values: &[HeaderValue]) -> Vec<&'static str> {
        let mut req = ntex::web::test::TestRequest::default();
        for value in values {
            req = req.header(header::ACCEPT_ENCODING, value.clone());
        }
        precompressed_preference(accepted_encodings(&req.to_http_request()))
            .into_iter()
            .map(Precompressed::header_value)
            .collect()
    }

    lets_expect! {
        expect(preference_with_raw_headers(&values)) as the_encoding_preference_with_malformed_header_bytes {
            let values: Vec<HeaderValue> = vec![HeaderValue::from_static("br")];

            to accepts_the_one_well_formed_line { equal(vec!["br"]) }

            when a_non_utf8_line_precedes_a_well_formed_one {
                // `to_str()` rejects any non-visible-ASCII byte (see
                // `HeaderValue::to_str`), so `0xFF` alone is enough to make
                // this line fail to parse as a string.
                let values: Vec<HeaderValue> = vec![
                    HeaderValue::from_bytes(&[0xFF]).unwrap(),
                    HeaderValue::from_static("br"),
                ];
                to skips_only_the_malformed_line_and_still_accepts_the_valid_one {
                    equal(vec!["br"])
                }
            }

            when a_non_utf8_line_follows_a_well_formed_one {
                let values: Vec<HeaderValue> = vec![
                    HeaderValue::from_static("gzip"),
                    HeaderValue::from_bytes(&[0xFF]).unwrap(),
                ];
                to still_accepts_the_earlier_valid_line { equal(vec!["gzip"]) }
            }
        }
    }
}
