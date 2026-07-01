//! Static (SSG) route generation and the internal catch-all route used by
//! [`LeptosRoutes`](crate::LeptosRoutes).
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
use leptos_integration_utils::{PinnedFuture, build_response};
use leptos_meta::ServerMetaContext;
use leptos_router::{
    RouteList,
    static_routes::{RegenerationFn, ResolvedStaticPath},
};
use ntex::http::{StatusCode, header};
use ntex::web::error::StateExtractorError;
use ntex::web::{self, ErrorRenderer, HttpRequest, HttpResponse, Route};
use or_poisoned::OrPoisoned;
use std::{
    fs,
    future::Future,
    io,
    num::NonZeroUsize,
    path::{Path, PathBuf},
    sync::{
        LazyLock, Mutex, RwLock,
        atomic::{AtomicU64, Ordering},
    },
};

use crate::render::{async_stream_builder, provide_contexts};
use crate::request::Request;
use crate::response::{NtexResponse, ResponseOptions, ResponseParts};
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

        let (owner, stream) = build_response(
            app_fn.clone(),
            additional_context,
            async_stream_builder,
            false,
        );
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
                            Self::render_route(
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
                                write_static_route(&options, response_options, path.as_ref(), html)
                                    .await
                            }
                        },
                        was_error_status,
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
/// is first rendered, keyed by the resolved on-disk file path (via
/// [`static_header_key`]) so aliased URLs that normalize to the same `.html`
/// — e.g. `/blog/` and `/blog/index` — share one entry. Read on cache hits to
/// replay the original status/headers alongside the on-disk HTML.
///
/// ⚠ **Scope is the entire process.** The key includes the `site_root`, so
/// distinct [`LeptosOptions`] with different roots do not collide; two
/// instances sharing a `site_root` and route path still would — the last
/// writer wins. Mirrors `leptos_actix` behaviour; in practice apps run a
/// single `LeptosOptions` instance per process and the collision is theoretical.
///
/// ⚠ **The captured status/headers are not durable; the HTML body is.**
/// The rendered page is written to disk, but the [`ResponseOptions`]
/// snapshot lives only in this in-memory map. A serving process that never
/// calls [`StaticRouteGenerator::generate`] — e.g. when prerendered
/// artifacts are produced by a separate build/CI step and only served at
/// runtime — starts with an empty map. A [`SsrMode::Static`](leptos_router::SsrMode)
/// route that set a custom non-error status (say `201 Created`) or custom
/// headers during generation is then served as a bare `200 OK` with those
/// dropped, until the path is regenerated on a cache miss in this process.
/// To preserve custom status/headers across restarts, run `generate()` in
/// the serving process at startup. Mirrors `leptos_actix`.
///
/// ⚠ **Bounded LRU cache.** Once the cache is full the least-recently-used
/// entry is evicted (default 1024 entries; override with the
/// `LEPTOS_STATIC_HEADERS_CACHE_SIZE` environment variable — a missing,
/// unparseable, or zero value falls back to the default). This caps memory: a
/// `SsrMode::Static` route registered with a `Param`/`Splat` segment is matched
/// by every distinct request URL, and each successfully rendered path would
/// otherwise insert a *permanent* entry here (and write one `.html` under
/// `site_root`) — a wildcard static route exposed to high-cardinality or
/// adversarial traffic would grow this map without bound. Eviction is graceful
/// for the body — the `.html` is still served from disk — but the evicted path
/// then serves that `.html` as a bare `200 OK`. Regeneration only runs when the
/// file is *absent*, so an evicted entry whose `.html` is still on disk is
/// **not** repopulated — its custom status/headers stay dropped until the
/// process restarts or the file is removed (the same bare-`200` degradation as
/// a process that never ran `generate()`, now also reachable at runtime under
/// cache pressure). The official `leptos_axum` integration adopted the same
/// bounded `lru::LruCache`; `leptos_actix` remains unbounded.
///
/// Recency is updated on **writes** (`put` during regeneration), not on reads:
/// the serve path uses a shared-lock `peek` so that every worker is not
/// serialized behind one global exclusive lock per cache hit. A page that is
/// served frequently but never re-rendered therefore ages by insertion order
/// rather than access order — acceptable because eviction only costs the
/// custom status/headers (the body keeps serving from disk).
static STATIC_HEADERS: LazyLock<RwLock<lru::LruCache<String, ResponseParts>>> =
    LazyLock::new(|| {
        let capacity =
            static_headers_capacity(std::env::var(STATIC_HEADERS_CAPACITY_ENV).ok().as_deref());
        RwLock::new(lru::LruCache::new(capacity))
    });

/// Default upper bound on the number of per-path [`ResponseParts`] entries
/// cached for static routes (see [`STATIC_HEADERS`]).
const STATIC_HEADERS_DEFAULT_CAPACITY: NonZeroUsize = match NonZeroUsize::new(1024) {
    Some(capacity) => capacity,
    None => unreachable!(),
};

/// Environment variable that overrides [`STATIC_HEADERS_DEFAULT_CAPACITY`].
const STATIC_HEADERS_CAPACITY_ENV: &str = "LEPTOS_STATIC_HEADERS_CACHE_SIZE";

/// Resolves the [`STATIC_HEADERS`] LRU capacity from the raw
/// `LEPTOS_STATIC_HEADERS_CACHE_SIZE` value (or `None` if unset): a missing,
/// unparseable, or zero value falls back to
/// [`STATIC_HEADERS_DEFAULT_CAPACITY`]; a valid positive value is used as-is.
/// Extracted from the [`STATIC_HEADERS`] initializer so it is testable without
/// mutating process-wide environment state.
fn static_headers_capacity(raw: Option<&str>) -> NonZeroUsize {
    raw.and_then(|value| value.parse::<usize>().ok())
        .and_then(NonZeroUsize::new)
        .unwrap_or(STATIC_HEADERS_DEFAULT_CAPACITY)
}

/// Whether a static render produced an ERROR status that must NOT be cached
/// to disk. leptos takes this as its `was_404` / `was_error` hook: when it
/// returns true, leptos sends the rendered HTML back instead of invoking the
/// writer, so the dynamic handler can re-render the live error on demand.
///
/// leptos's own contract is "404, 500, etc." (see the `was_error` comment in
/// leptos_router's static generation), so this skips EVERY 4xx/5xx render —
/// not only 404. A 500 (or any error) render cached as a bare static file
/// would otherwise be served from disk indefinitely, even after the cause
/// cleared. This is a deliberate divergence from `leptos_axum` /
/// `leptos_actix`, whose `was_404` callbacks check only `== NOT_FOUND`.
fn was_error_status(owner: &Owner) -> bool {
    let resp = owner.with(|| expect_context::<ResponseOptions>());
    let status = resp.0.read().or_poisoned().status;
    status.is_some_and(|status| status.is_client_error() || status.is_server_error())
}

/// Whether `status` is one that `NamedFile::into_response` derived from the
/// current request's conditional (`If-None-Match`/`If-Modified-Since`/
/// `If-Match`/`If-Unmodified-Since`) or `Range` headers: `304 Not Modified`,
/// `412 Precondition Failed`, `206 Partial Content`, `416 Range Not
/// Satisfiable`, or `400 Bad Request` for a `Range` header that is not valid
/// text. These are exactly the non-`200` statuses `ntex_files`'
/// `NamedFile::into_response` can produce — it builds on a base `200 OK` and
/// overrides it only for these request-derived cases.
///
/// On the file-serving branch a captured `ResponseOptions` status describes the
/// *full* representation (e.g. an app-set `201`); it must not overwrite one of
/// these, or a conditional/range request would get a wrong status (RFC 9110
/// §13, §14.4, §15.4.5). A plain `200 OK` full serve is not in this set, so the
/// app's status still applies there.
fn is_conditional_or_range_status(status: StatusCode) -> bool {
    matches!(
        status,
        StatusCode::NOT_MODIFIED
            | StatusCode::PRECONDITION_FAILED
            | StatusCode::PARTIAL_CONTENT
            | StatusCode::RANGE_NOT_SATISFIABLE
            | StatusCode::BAD_REQUEST
    )
}

fn static_path(options: &LeptosOptions, path: &str) -> Option<PathBuf> {
    let mut normalized = path.to_string();
    if normalized != "/" && normalized.ends_with('/') {
        normalized.push_str("index");
    }

    let trimmed = normalized.trim_start_matches('/');
    let logical = if trimmed.is_empty() { "index" } else { trimmed };
    let mut parts = Vec::new();
    for segment in logical.split('/') {
        if segment.is_empty() {
            continue;
        }
        let decoded = percent_encoding::percent_decode_str(segment)
            .decode_utf8()
            .ok()?;
        let segment = decoded.as_ref();
        // Blocks `.`, `..` and every dotfile, but lets the exact `.well-known`
        // segment through (RFC 8615) — see `crate::files::is_blocked_dot_segment`.
        if crate::files::is_blocked_dot_segment(segment)
            || segment.contains('\0')
            || segment.contains('/')
            || segment.contains('\\')
        {
            return None;
        }
        parts.push(segment.to_string());
    }

    let last = parts.last_mut()?;
    last.push_str(".html");

    let mut rel = PathBuf::new();
    for part in parts {
        rel.push(part);
    }
    if !rel.components().all(|component| {
        matches!(
            component,
            std::path::Component::Normal(part)
                if !crate::files::is_blocked_dot_segment(&part.to_string_lossy())
        )
    }) {
        return None;
    }
    Some(Path::new(&*options.site_root).join(rel))
}

fn validate_static_parent(root: &Path, file_path: &Path) -> Result<(), io::Error> {
    fs::create_dir_all(root)?;
    let canon_root = root.canonicalize()?;
    let parent = file_path
        .parent()
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "static path has no parent"))?;
    fs::create_dir_all(parent)?;
    let canon_parent = parent.canonicalize()?;
    if !canon_parent.starts_with(&canon_root) {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "static route path escapes site_root",
        ));
    }
    Ok(())
}

fn validate_static_file(root: &Path, file_path: &Path) -> Option<PathBuf> {
    let canon_root = root.canonicalize().ok()?;
    let canon_target = file_path.canonicalize().ok()?;
    canon_target
        .starts_with(&canon_root)
        .then_some(canon_target)
}

/// Process-local counter that makes each atomic-write temp file name unique
/// without a high-resolution clock (a nanosecond timestamp can collide under
/// parallelism — see the temp-path flakiness fixed in this crate's tests).
static TEMP_SEQ: AtomicU64 = AtomicU64::new(0);

/// Atomically writes `contents` to `file_path` by writing a sibling temp file
/// in the same directory and renaming it over the target.
///
/// A plain `fs::write` truncates the target and then writes, so a crash or a
/// concurrent reader mid-write can observe a truncated or empty file — and the
/// serve path only checks that the file *exists* (`NamedFile::open`), not that
/// it is intact, so it would happily serve the partial. `rename(2)` within a
/// directory is atomic on the same filesystem, so a reader sees either the old
/// file or the fully-written new one, never a partial.
///
/// `leptos-rs/leptos#4755` ships this as `leptos_integration_utils::
/// write_file_atomic` (behind an `fs` feature), but it is not in the published
/// `leptos_integration_utils` 0.8.8 this crate depends on, so this is a local
/// equivalent. Mirrors the atomic-write fix in `leptos_actix` / `leptos_axum`.
///
/// The parent directory is assumed to already exist (the caller runs
/// [`validate_static_parent`] first); the temp file is created alongside the
/// target so the rename stays on one filesystem.
fn write_file_atomic(file_path: &Path, contents: &[u8]) -> Result<(), io::Error> {
    let parent = file_path
        .parent()
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "static path has no parent"))?;
    let file_name = file_path
        .file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_default();
    // Dot-prefixed so it is hidden from naive listings and would be rejected by
    // the dotfile guard if ever requested; pid + counter keep it collision-free
    // across concurrent writers.
    let seq = TEMP_SEQ.fetch_add(1, Ordering::Relaxed);
    let tmp_path = parent.join(format!(".{file_name}.tmp.{}.{seq}", std::process::id()));

    // RAII: remove the temp on any early return OR panic between here and a
    // successful rename, so a failed or interrupted write never leaks a scratch
    // file into the served directory. `fs::rename` consumes the temp on
    // success, after which the guard is disarmed (nothing left to remove).
    let mut guard = TempFileGuard {
        path: Some(tmp_path.clone()),
    };
    fs::write(&tmp_path, contents)?;
    fs::rename(&tmp_path, file_path)?;
    guard.disarm();
    Ok(())
}

/// Removes its temp file on drop unless [disarmed](TempFileGuard::disarm), so a
/// failed or panicking [`write_file_atomic`] leaves no scratch file behind —
/// including if the surrounding blocking task unwinds mid-write.
struct TempFileGuard {
    path: Option<PathBuf>,
}

impl TempFileGuard {
    fn disarm(&mut self) {
        self.path = None;
    }
}

impl Drop for TempFileGuard {
    fn drop(&mut self) {
        if let Some(path) = self.path.take() {
            let _ = fs::remove_file(path);
        }
    }
}

/// Number of stripes for the static-route write lock.
const STATIC_WRITE_STRIPES: usize = 32;

/// Striped write locks serializing concurrent regenerations of the SAME static
/// path, so its on-disk file and the [`STATIC_HEADERS`] snapshot captured for
/// it always come from one render (closing the file/headers desync a per-path
/// race could otherwise leave). A FIXED set of mutexes — NOT a per-path map,
/// which would reintroduce exactly the unbounded growth the `STATIC_HEADERS`
/// LRU bounds. Distinct paths may share a stripe; that only serializes two
/// unrelated writes occasionally (harmless) and never corrupts.
static STATIC_WRITE_LOCKS: LazyLock<[Mutex<()>; STATIC_WRITE_STRIPES]> =
    LazyLock::new(|| std::array::from_fn(|_| Mutex::new(())));

/// Returns the write stripe for an on-disk `file_path` (same resolved file →
/// same stripe). Keyed on the *resolved* path rather than the request URL, so
/// aliased URLs that [`static_path`] normalizes to the same `.html` (e.g.
/// `/x/` and `/x/index`) serialize against each other instead of racing on the
/// one file they share.
fn static_write_lock(file_path: &Path) -> &'static Mutex<()> {
    use std::hash::{Hash, Hasher};
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    file_path.hash(&mut hasher);
    &STATIC_WRITE_LOCKS[(hasher.finish() as usize) % STATIC_WRITE_STRIPES]
}

/// The [`STATIC_HEADERS`] key for a resolved on-disk file. Keyed on the
/// resolved path (which two aliased URLs normalizing to the same `.html`
/// share), NOT the request URL, so the write and serve sides agree and aliases
/// resolve to one entry. Both sides MUST derive the key through this helper —
/// hence it exists rather than inlining the conversion twice.
fn static_header_key(file_path: &Path) -> String {
    file_path.to_string_lossy().into_owned()
}

/// Opens the resolved on-disk file and reads its captured [`STATIC_HEADERS`]
/// snapshot under the file's write stripe, so the body and the headers always
/// come from ONE render epoch: the writer holds the same stripe across its
/// atomic rename and its header `put`, so taking the stripe here guarantees
/// never a body from render A paired with headers from render B. Used by BOTH
/// the cache-hit path and the post-regeneration re-open — the pairing
/// guarantee is structural, not duplicated. Blocking I/O + sync lock — only
/// call from a blocking executor. The headers read uses a SHARED lock +
/// `peek` (no recency bump) so cache hits never serialize every worker behind
/// one global exclusive lock.
///
/// If the [`STATIC_HEADERS`] entry was LRU-evicted (or never written), the
/// file is served with NO custom status/headers — including on the
/// post-regeneration re-open, where this request's own render did capture a
/// snapshot. Deliberate: falling back to the request-local snapshot there
/// would reintroduce the cross-epoch pairing race this helper exists to
/// close, and eviction ⇒ bare `200` is already the documented degradation of
/// the bounded cache (see [`STATIC_HEADERS`]). A consistent pair beats a
/// complete-but-possibly-mismatched one.
fn open_paired_static_file(
    root: &Path,
    file_path: &Path,
    header_key: &str,
) -> (Option<ntex_files::NamedFile>, Option<ResponseParts>) {
    // The stripe guards `()`, so recover from a poisoned holder instead of
    // cascading its panic.
    let _guard = static_write_lock(file_path)
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let opened = validate_static_file(root, file_path)
        .and_then(|path| ntex_files::NamedFile::open(path).ok());
    let headers = if opened.is_some() {
        STATIC_HEADERS
            .read()
            .or_poisoned()
            .peek(header_key)
            .cloned()
    } else {
        None
    };
    (opened, headers)
}

async fn write_static_route(
    options: &LeptosOptions,
    response_options: Option<ResponseOptions>,
    path: &str,
    html: String,
) -> Result<(), io::Error> {
    let snapshot = response_options.map(|options| options.0.read().or_poisoned().clone());

    let file_path = static_path(options, path)
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "invalid static route path"))?;
    let root = PathBuf::from(&*options.site_root);
    ntex::rt::spawn_blocking(move || {
        // Hold the per-file stripe across BOTH the atomic rename and the header
        // snapshot `put`, so two concurrent regenerations writing the same file
        // cannot interleave rename and cache update. The stripe AND the cache
        // key are both keyed on the resolved `file_path` (not the URL), so
        // aliased URLs that normalize to one `.html` serialize and share a
        // single cache entry. A sync lock on the blocking thread — it never
        // crosses an await (the ntex worker has already offloaded here).
        // Recover from a poisoned stripe rather than `or_poisoned`'s panic: the
        // mutex guards `()`, so a prior panicking holder leaves no inconsistent
        // state and must not cascade into later writers.
        let _write_guard = static_write_lock(&file_path)
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        validate_static_parent(&root, &file_path)?;
        write_file_atomic(&file_path, html.as_bytes())?;
        if let Some(snapshot) = snapshot {
            STATIC_HEADERS
                .write()
                .or_poisoned()
                .put(static_header_key(&file_path), snapshot);
        }
        Ok::<(), io::Error>(())
    })
    .await
    .map_err(io::Error::other)??;

    Ok(())
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
            let Some(path_buf) = static_path(&options, &orig_path) else {
                return HttpResponse::NotFound().finish();
            };
            let root = PathBuf::from(&*options.site_root);

            let header_key = static_header_key(&path_buf);
            let opened_join = ntex::rt::spawn_blocking({
                let root = root.clone();
                let path_buf = path_buf.clone();
                let header_key = header_key.clone();
                move || open_paired_static_file(&root, &path_buf, &header_key)
            })
            .await;
            let (opened, hit_headers) = match opened_join {
                Ok(pair) => pair,
                Err(join_err) => {
                    crate::files::warn_blocking_join_failed("static file open task", &join_err);
                    (None, None)
                }
            };

            let (response_options, html, opened) = if opened.is_none() {
                // Known limitation (regeneration stampede): N concurrent
                // requests to a still-missing `.html` each run a full SSR
                // render here. The per-file stripe in `write_static_route`
                // serializes only the DISK write, not the render, so the
                // final on-disk state is always correct (last writer wins,
                // writes are atomic) — the cost is duplicated CPU right after
                // a cold start / manual delete. In-flight render dedup is
                // intentionally NOT added: a correct version needs an async,
                // per-path coordination primitive held across the render
                // `.await` (a blocking lock across `.await` would risk the
                // very deadlock class this crate guards against), and the
                // degradation is transient and self-correcting.
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
                                write_static_route(&options, response_options, path.as_ref(), html)
                                    .await
                            }
                        },
                        was_error_status,
                        regenerate,
                    )
                    .await;
                // On a successful render `ResolvedStaticPath::build` writes
                // the page to disk and returns `None` (the body lives on
                // disk, not in memory), so re-open the freshly written file
                // and serve it — paired with the header snapshot under the
                // file's write stripe (`open_paired_static_file`), exactly
                // like the cache-hit path. This request's own render captured
                // a snapshot too, but a CONCURRENT regeneration may overwrite
                // the file between this request's write and its re-open;
                // applying the locally captured headers to whatever body is
                // on disk could then mix two render epochs (body from render
                // B under headers from render A). The stripe-paired read
                // always returns one epoch. On a 404/error render `build`
                // returns `Some(html)` and skips the disk write (nothing is
                // cached), so the in-memory body keeps the locally captured
                // `ResponseOptions`. Without the re-open the `html == None`
                // arm below would fall through to a 500 on the first request
                // to an un-pregenerated static route. Mirrors `leptos_axum`
                // (`ServeDir`) and `leptos_actix` (`NamedFile::open`).
                if html.is_none() {
                    let (reopened, paired_headers) = match ntex::rt::spawn_blocking({
                        let root = root.clone();
                        let path_buf = path_buf.clone();
                        let header_key = header_key.clone();
                        move || open_paired_static_file(&root, &path_buf, &header_key)
                    })
                    .await
                    {
                        Ok(pair) => pair,
                        Err(join_err) => {
                            crate::files::warn_blocking_join_failed(
                                "static file re-open task",
                                &join_err,
                            );
                            (None, None)
                        }
                    };
                    (paired_headers, None, reopened)
                } else {
                    let response_options = owner
                        .with(use_context::<ResponseOptions>)
                        .map(|options| options.0.read().or_poisoned().clone());
                    (response_options, html, None)
                }
            } else {
                // Cache hit: body and headers were already paired under the
                // stripe lock in the open task above (see `hit_headers`).
                (hit_headers, None, opened)
            };

            // `Some(html)` only happens on a 404/error render that `build`
            // chose not to cache — emit it as an explicit `404` with the
            // hardcoded `text/html` (Leptos's SSG only produces HTML); a custom
            // status the app set still overrides this via the captured
            // `ResponseParts` applied below. Every other
            // path (cache hit, or a successful regeneration we just
            // re-opened) serves the on-disk file via `NamedFile`, which
            // derives MIME from the extension and adds `Last-Modified` /
            // `ETag`. A 500 here now means the file genuinely could not be
            // opened after a successful write, not the expected
            // success-path (which previously fell through to a bogus 500).
            // Whether this response body is the on-disk file (served via
            // `NamedFile`) vs. the inline 404 HTML. On the file branch
            // `NamedFile` is the authoritative source of the body-framing
            // headers, so the captured snapshot must not override them below.
            let served_from_file = html.is_none();
            let mut res = NtexResponse(match html {
                Some(html) => HttpResponse::NotFound()
                    .content_type("text/html")
                    .body(html),
                None => opened.map(|named| named.into_response(&req)).unwrap_or_else(|| {
                    // The file could not be opened even though the success
                    // path either found a cache hit or just wrote it — log
                    // before returning an otherwise-undiagnosable bare 500.
                    #[cfg(feature = "tracing")]
                    tracing::error!(
                        "static route {orig_path}: file {} could not be opened after render/write",
                        path_buf.display()
                    );
                    #[cfg(not(feature = "tracing"))]
                    eprintln!(
                        "static route {orig_path}: file {} could not be opened after render/write",
                        path_buf.display()
                    );
                    HttpResponse::InternalServerError().finish()
                }),
            });

            if let Some(mut options) = response_options {
                if served_from_file {
                    // `NamedFile::into_response` already derived the
                    // body-framing headers from the file actually on disk
                    // (length, MIME, validators). A captured `ResponseParts`
                    // snapshot is in the `should_replace_header` REPLACE set
                    // for these keys, so a stale or app-set value (e.g. a wrong
                    // `Content-Length`) would otherwise overwrite the
                    // authoritative one and desync the framing from the real
                    // body. Drop them from the snapshot so `NamedFile` wins;
                    // the app's status (on a plain `200` full serve) and its
                    // non-framing headers (cookies, `Cache-Control`, custom
                    // `x-*`) still apply.
                    for key in [
                        header::CONTENT_LENGTH,
                        header::CONTENT_TYPE,
                        header::CONTENT_ENCODING,
                        header::TRANSFER_ENCODING,
                        header::CONTENT_RANGE,
                        header::ACCEPT_RANGES,
                        header::ETAG,
                        header::LAST_MODIFIED,
                    ] {
                        options.headers.remove(key);
                    }
                    // `NamedFile::into_response` also derives the response STATUS
                    // from this request's validators and `Range`: `304`/`412` for
                    // a conditional GET, `206`/`416` for a range request, `400`
                    // for a malformed `Range` header.
                    if is_conditional_or_range_status(res.0.status()) {
                        match options.status {
                            // A captured *success* status describes the full
                            // representation (e.g. an app-set `201`); letting it
                            // overwrite one of the above would break the caching /
                            // range contract — a conditional GET would get `201`
                            // with an empty body instead of `304`. Drop it so
                            // `NamedFile`'s status wins.
                            Some(status) if status.is_success() => {
                                options.status = None;
                            }
                            // A captured redirect (`302` + `Location` from
                            // `redirect()`, which SSG caches because
                            // `was_error_status` skips only 4xx/5xx) is not a
                            // representation status and must still fire. But
                            // `NamedFile`'s `res` carries range/conditional
                            // artifacts (a `206`'s `Content-Range` + partial file
                            // body) that a redirect must not, so rebuild an
                            // empty-bodied response of the captured status; its
                            // non-framing snapshot headers (`Location`, cookies,
                            // custom `x-*`) are applied below. `options.status` is
                            // cleared because the status now lives on `res`.
                            Some(status) => {
                                res = NtexResponse(HttpResponse::build(status).finish());
                                options.status = None;
                            }
                            None => {}
                        }
                    }
                }
                res.extend_response_parts(options);
            }

            res.take()
        }
    };
    // HEAD mirrors GET semantics (RFC 9110 §9.3.2); ntex strips the body
    // at the h1 writer when the request is HEAD. `.method()` is
    // unusable for multi-method routes because `take_guards` converts
    // it into an AND-combined MethodGuard on the owning Resource.
    Route::<Err>::new()
        .guard(ntex::web::guard::Any(ntex::web::guard::Get()).or(ntex::web::guard::Head()))
        .to(handler)
}

#[cfg(test)]
mod tests {
    use super::*;
    use lets_expect::lets_expect;

    const TEST_SITE_ROOT: &str = "/tmp/leptos_ntex_static_path_test";

    fn options() -> LeptosOptions {
        LeptosOptions::builder()
            .output_name("leptos_ntex_static_path_test")
            .site_root(TEST_SITE_ROOT)
            .site_pkg_dir("pkg")
            .build()
    }

    /// The on-disk path `static_path` is expected to resolve `rel` to, under
    /// the test `site_root`. `static_path` is pure (no filesystem access), so
    /// the directory need not exist for these assertions.
    fn under_site_root(rel: &str) -> Option<PathBuf> {
        Some(Path::new(TEST_SITE_ROOT).join(rel))
    }

    // ----- static_headers_capacity: exhaustive spec ---------------------
    // The `LEPTOS_STATIC_HEADERS_CACHE_SIZE` contract documented on
    // `STATIC_HEADERS`: missing, unparseable, or zero -> the crate default;
    // a valid positive value -> used as-is. Extracted to a plain function of
    // `Option<&str>` so every branch is testable without mutating (or racing
    // on) process-wide environment state.
    lets_expect! {
        expect(static_headers_capacity(raw)) as the_resolved_capacity {
            let raw: Option<&str> = None;

            to falls_back_to_the_default_when_missing {
                equal(STATIC_HEADERS_DEFAULT_CAPACITY)
            }

            when the_value_is_unparseable {
                let raw = Some("not_a_number");
                to falls_back_to_the_default { equal(STATIC_HEADERS_DEFAULT_CAPACITY) }
            }

            when the_value_is_zero {
                let raw = Some("0");
                to falls_back_to_the_default { equal(STATIC_HEADERS_DEFAULT_CAPACITY) }
            }

            when the_value_is_a_valid_positive_number {
                let raw = Some("64");
                to is_used_as_the_capacity {
                    equal(NonZeroUsize::new(64).unwrap())
                }
            }
        }
    }

    // ----- is_conditional_or_range_status: exhaustive spec --------------
    // The predicate that keeps a captured `ResponseOptions` status from
    // clobbering the status `NamedFile` derived from THIS request's
    // conditional / `Range` headers. It must return `true` for exactly the
    // five such statuses (304/412/206/416/400 — the complete non-200 set
    // `NamedFile::into_response` can produce) and `false` for everything the
    // full-serve path produces — a plain `200`, an app-set `201`, an error
    // `500` — so the app's own status still applies on a full serve. A
    // regression dropping or widening any `matches!` arm flips one leaf.
    lets_expect! {
        expect(is_conditional_or_range_status(status)) {
            let status = StatusCode::OK;

            to does_not_protect_a_full_200_serve { be_false }

            when the_status_is_304_not_modified {
                let status = StatusCode::NOT_MODIFIED;
                to is_protected { be_true }
            }

            when the_status_is_412_precondition_failed {
                let status = StatusCode::PRECONDITION_FAILED;
                to is_protected { be_true }
            }

            when the_status_is_206_partial_content {
                let status = StatusCode::PARTIAL_CONTENT;
                to is_protected { be_true }
            }

            when the_status_is_416_range_not_satisfiable {
                let status = StatusCode::RANGE_NOT_SATISFIABLE;
                to is_protected { be_true }
            }

            when the_status_is_400_bad_request_from_a_malformed_range {
                let status = StatusCode::BAD_REQUEST;
                to is_protected { be_true }
            }

            when the_status_is_an_app_set_201_created {
                let status = StatusCode::CREATED;
                to is_not_protected_so_the_app_status_applies { be_false }
            }

            when the_status_is_a_500_server_error {
                let status = StatusCode::INTERNAL_SERVER_ERROR;
                to is_not_protected { be_false }
            }

            // Adjacent to (but distinct from) the protected 400/416 cluster.
            // Pins that this is an EXACT five-way enumeration, not a numeric
            // range check — a range rewrite (e.g. "400..=416") would still
            // pass every leaf above but wrongly protect 405 too.
            when the_status_is_405_method_not_allowed {
                let status = StatusCode::METHOD_NOT_ALLOWED;
                to is_not_protected { be_false }
            }
        }
    }

    // ----- static_path: exhaustive spec --------------------------------
    // A pure URL-path -> on-disk-path resolver with two jobs: resolve the
    // happy path (append `.html`; map `/`, the empty path and trailing
    // slashes to `index.html`) and REJECT every traversal / dotfile /
    // smuggling shape. The old tests asserted only rejection (`None`); all
    // nine `Some` leaves below — the `.html` suffixing and the index
    // resolution — were previously unpinned, so a regression dropping
    // `.html` or mangling the join would not have been caught.
    lets_expect! {
        expect(static_path(&options(), path)) as the_resolved_static_path {
            let path = "/about";

            to resolves_to_the_html_file { equal(under_site_root("about.html")) }

            when the_path_is_the_site_root {
                let path = "/";
                to resolves_to_the_index_file { equal(under_site_root("index.html")) }
            }

            when the_path_is_empty {
                let path = "";
                to resolves_to_the_index_file { equal(under_site_root("index.html")) }
            }

            when the_path_has_a_trailing_slash {
                let path = "/blog/";
                to resolves_to_a_nested_index_file { equal(under_site_root("blog/index.html")) }
            }

            when the_path_has_redundant_leading_slashes {
                let path = "//about";
                to collapses_them_and_resolves_the_html_file {
                    equal(under_site_root("about.html"))
                }
            }

            // The segment loop skips every empty split (`if segment.is_empty()
            // { continue; }`), so a redundant slash is NOT special-cased to the
            // leading position — an interior doubled "/" collapses exactly like
            // a leading one, rather than being rejected as malformed input.
            when the_path_has_a_redundant_interior_slash {
                let path = "/blog//post-1";
                to collapses_it_and_resolves_the_same_file_as_a_single_slash {
                    equal(under_site_root("blog/post-1.html"))
                }
            }

            when the_path_is_nested {
                let path = "/blog/post-1";
                to resolves_to_the_nested_html_file {
                    equal(under_site_root("blog/post-1.html"))
                }
            }

            when a_segment_is_percent_encoded {
                let path = "/foo%20bar";
                to decodes_the_segment_before_resolving { equal(under_site_root("foo bar.html")) }
            }

            when a_segment_decodes_to_non_ascii_utf8 {
                let path = "/r%C3%A9sum%C3%A9";
                to decodes_the_utf8_segment { equal(under_site_root("résumé.html")) }
            }

            when a_segment_is_the_current_directory {
                let path = "/sub/./x";
                to is_rejected { be_none }
            }

            when a_segment_is_a_parent_traversal {
                let path = "/static/../outside";
                to is_rejected { be_none }
            }

            when a_segment_is_an_encoded_parent_traversal {
                let path = "/static/%2e%2e/outside";
                to is_rejected { be_none }
            }

            when an_encoded_parent_traversal_is_uppercase {
                let path = "/a/%2E%2E/b";
                to is_rejected { be_none }
            }

            when a_segment_is_a_dotfile {
                let path = "/static/.env";
                to is_rejected { be_none }
            }

            // RFC 8615: the exact `.well-known` segment is exempted from the
            // dotfile guard (in both this resolver and the file fallback's
            // `safe_subpath`), so ACME challenges / security.txt resolve. The
            // exemption is narrow — see the two rejection leaves below.
            when the_path_is_a_well_known_uri {
                let path = "/.well-known/acme-challenge/token";
                to resolves_under_well_known {
                    equal(under_site_root(".well-known/acme-challenge/token.html"))
                }
            }

            when a_dotfile_is_nested_inside_well_known {
                let path = "/.well-known/.secret";
                to is_still_rejected { be_none }
            }

            when a_traversal_hides_behind_well_known {
                let path = "/.well-known/../etc/passwd";
                to is_still_rejected { be_none }
            }

            when a_segment_contains_a_nul_byte {
                let path = "/a\0b";
                to is_rejected { be_none }
            }

            when a_segment_encodes_a_path_separator {
                let path = "/static/subdir%2F.env";
                to is_rejected { be_none }
            }

            when an_encoded_separator_is_leading {
                let path = "/%2Fetc";
                to is_rejected { be_none }
            }

            when a_segment_contains_a_backslash {
                let path = "/a\\b";
                to is_rejected { be_none }
            }

            when a_segment_is_not_valid_utf8_after_decoding {
                let path = "/file%FFname";
                to is_rejected { be_none }
            }
        }
    }

    /// Throwaway per-test directory under the system temp dir, uniquely named
    /// with pid + a process-local counter (a nanosecond clock alone collides
    /// under parallel test runs — see the temp-path flakiness fixed elsewhere
    /// in this crate).
    fn temp_dir_for(name: &str) -> PathBuf {
        static UNIQUE: AtomicU64 = AtomicU64::new(0);
        std::env::temp_dir().join(format!(
            "leptos_ntex_{name}_{}_{}",
            std::process::id(),
            UNIQUE.fetch_add(1, Ordering::Relaxed)
        ))
    }

    // ----- validate_static_parent: exhaustive spec ----------------------
    // Guards `write_static_route` against writing outside `site_root`: `Ok`
    // when the resolved file's parent is under the root, `Err(InvalidInput)`
    // when the candidate path has no parent at all, and `Err(PermissionDenied)`
    // when the parent — possibly reached only after a symlink is resolved —
    // canonicalizes to somewhere outside the root. Inverting or dropping the
    // `!canon_parent.starts_with(&canon_root)` check would let a symlinked
    // directory escape `site_root` silently, which the last leaf below pins.
    lets_expect! {
        expect(validate_static_parent(&root, &file_path)) as the_validation_result {
            let root = temp_dir_for("validate_parent_root");
            let file_path = root.join("blog").join("post-1.html");

            to allows_a_file_whose_parent_is_under_the_root { be_ok }

            when the_candidate_path_has_no_parent {
                let file_path = PathBuf::from("/");
                to is_rejected_as_invalid_input {
                    make(matches!(
                        validate_static_parent(&root, &file_path),
                        Err(ref e) if e.kind() == io::ErrorKind::InvalidInput
                    )) be_true
                }
            }

            when the_parent_is_a_symlink_that_resolves_outside_the_root {
                let file_path = {
                    fs::create_dir_all(&root).unwrap();
                    let outside = temp_dir_for("validate_parent_outside");
                    fs::create_dir_all(&outside).unwrap();
                    let link = root.join("escape");
                    #[cfg(unix)]
                    std::os::unix::fs::symlink(&outside, &link).unwrap();
                    link.join("page.html")
                };

                to is_rejected_as_permission_denied {
                    make(matches!(
                        validate_static_parent(&root, &file_path),
                        Err(ref e) if e.kind() == io::ErrorKind::PermissionDenied
                    )) be_true
                }
            }
        }
    }

    // ----- validate_static_file: exhaustive spec ------------------------
    // Guards the SERVE side: `Some(canon_target)` when the resolved file is
    // under `root`, `None` when `root` itself cannot be canonicalized (e.g.
    // missing), and `None` when the target — again possibly only via a
    // symlink — canonicalizes outside `root`. A regression dropping the
    // `.then_some(canon_target)` guard (always `Some` regardless of
    // `starts_with`) would let a symlink-escaped file be served under the
    // wrong `site_root`; the escape leaf below pins that this cannot happen.
    lets_expect! {
        expect(validate_static_file(&root, &file_path)) as the_validated_file {
            // `root` is created (as a directory that exists on disk) ONLY in
            // the happy path's own `let`; the missing-root context below
            // overrides `root` with a path nobody creates, so it stays absent.
            let root = {
                let dir = temp_dir_for("validate_file_root");
                fs::create_dir_all(&dir).unwrap();
                dir
            };
            let file_path = {
                let target = root.join("page.html");
                fs::write(&target, b"hello").unwrap();
                target
            };

            to returns_the_canonicalized_file_under_the_root {
                equal(Some(root.canonicalize().unwrap().join("page.html")))
            }

            when the_root_does_not_exist {
                let root = temp_dir_for("validate_file_missing_root");
                let file_path = root.join("page.html");
                to is_none { be_none }
            }

            when the_file_is_a_symlink_that_resolves_outside_the_root {
                let file_path = {
                    let outside = temp_dir_for("validate_file_outside");
                    fs::create_dir_all(&outside).unwrap();
                    let outside_target = outside.join("secret.html");
                    fs::write(&outside_target, b"secret").unwrap();
                    let link = root.join("escape.html");
                    #[cfg(unix)]
                    std::os::unix::fs::symlink(&outside_target, &link).unwrap();
                    link
                };
                to is_none_because_the_target_escapes_the_root { be_none }
            }
        }
    }

    // ----- static_headers LRU bounding: regression pin -----------------
    // Pins the bounded-cache invariant: a wildcard SsrMode::Static route
    // under high-cardinality traffic must not grow the per-path header map
    // without limit. This pins `lru` (a dependency) eviction semantics at
    // the crate's chosen capacity, so it stays a shallow 3-leaf regression
    // pin, not a rich tree. `ResponseParts` has no `PartialEq`, so we assert
    // on `len()` and non-mutating `peek` presence — never `get`, which would
    // promote recency and perturb the very eviction order under test.
    fn over_filled_cache() -> lru::LruCache<String, ResponseParts> {
        let mut cache = lru::LruCache::new(STATIC_HEADERS_DEFAULT_CAPACITY);
        for i in 0..(STATIC_HEADERS_DEFAULT_CAPACITY.get() + 10) {
            cache.put(format!("/post/{i}"), ResponseParts::default());
        }
        cache
    }

    lets_expect! {
        expect(over_filled_cache().len()) as the_overfilled_cache_size {
            to never_grows_past_capacity { equal(STATIC_HEADERS_DEFAULT_CAPACITY.get()) }
        }
    }

    lets_expect! {
        expect(over_filled_cache().peek(&"/post/0".to_string()).is_some()) as the_earliest_entry {
            to has_been_evicted { be_false }
        }
    }

    lets_expect! {
        expect(
            over_filled_cache()
                .peek(&format!("/post/{}", STATIC_HEADERS_DEFAULT_CAPACITY.get() + 9))
                .is_some()
        ) as the_most_recently_inserted_entry {
            to is_retained { be_true }
        }
    }

    // ----- write_file_atomic: full write, atomic replace, no temp leak --
    // A static page must be written atomically: the target appears with its
    // full contents or not at all, an overwrite fully replaces, and no scratch
    // temp file is left in the served directory. An in-place `fs::write`
    // truncates first, so a crash or a concurrent reader mid-write could
    // observe an empty/partial file (the serve path checks existence, not
    // integrity). The unique temp dir is keyed on pid + a process-local
    // counter — NOT a nanosecond clock, which collides under parallel runs.
    #[test]
    fn write_file_atomic_replaces_fully_and_leaves_no_temp() {
        let seq = TEMP_SEQ.fetch_add(1, Ordering::Relaxed);
        let dir =
            std::env::temp_dir().join(format!("leptos_ntex_atomic_{}_{seq}", std::process::id()));
        fs::create_dir_all(&dir).unwrap();
        let target = dir.join("page.html");

        let first: &[u8] = b"<html><body>hello</body></html>";
        write_file_atomic(&target, first).unwrap();
        assert_eq!(fs::read(&target).unwrap(), first);

        // an overwrite atomically replaces the contents in place
        let second: &[u8] = b"<html><body>updated and rather longer</body></html>";
        write_file_atomic(&target, second).unwrap();
        assert_eq!(fs::read(&target).unwrap(), second);

        // no `.tmp.` scratch file is left behind in the served directory
        let leftovers = fs::read_dir(&dir)
            .unwrap()
            .filter_map(Result::ok)
            .filter(|entry| entry.file_name().to_string_lossy().contains(".tmp."))
            .count();
        assert_eq!(leftovers, 0);

        fs::remove_dir_all(&dir).ok();
    }

    // The Err path + `TempFileGuard` cleanup — the point of the guard: when the
    // rename fails *after* the temp is written, `write_file_atomic` must return
    // `Err` AND leave no scratch temp behind. A deterministic rename failure is
    // forced by making the target an existing directory (`rename(file, dir)`
    // fails with EISDIR), so the temp exists before the failure and the guard's
    // `Drop` is what must remove it.
    #[test]
    fn write_file_atomic_on_failure_returns_err_and_cleans_temp() {
        let seq = TEMP_SEQ.fetch_add(1, Ordering::Relaxed);
        let dir = std::env::temp_dir().join(format!(
            "leptos_ntex_atomic_fail_{}_{seq}",
            std::process::id()
        ));
        fs::create_dir_all(&dir).unwrap();

        // target is a directory -> the rename of the temp over it fails
        let target = dir.join("page.html");
        fs::create_dir_all(&target).unwrap();

        assert!(write_file_atomic(&target, b"<html></html>").is_err());

        // the guard removed the temp -> no `.tmp.` scratch file remains
        let leftovers = fs::read_dir(&dir)
            .unwrap()
            .filter_map(Result::ok)
            .filter(|entry| entry.file_name().to_string_lossy().contains(".tmp."))
            .count();
        assert_eq!(leftovers, 0);
        // and the target was left untouched
        assert!(target.is_dir());

        fs::remove_dir_all(&dir).ok();
    }

    // The write stripe and the `STATIC_HEADERS` key are both keyed on the
    // resolved file, not the request URL — so two *aliased* URLs that normalize
    // to the same `.html` (`/blog/` and `/blog/index`) serialize against each
    // other AND share one cache entry (no file/header desync across aliases).
    // Distinct files MAY still share a stripe (harmless false contention), so
    // we deliberately do not assert they differ.
    #[test]
    fn aliased_urls_share_one_stripe_and_cache_entry() {
        let opts = options();

        // stripe is stable for a given resolved file
        let direct = static_path(&opts, "/blog/post-1").unwrap();
        assert!(std::ptr::eq(
            static_write_lock(&direct),
            static_write_lock(&direct),
        ));

        // aliased URLs -> same resolved file -> same stripe AND same cache key
        let via_slash = static_path(&opts, "/blog/").unwrap();
        let via_index = static_path(&opts, "/blog/index").unwrap();
        assert_eq!(via_slash, via_index);
        assert!(std::ptr::eq(
            static_write_lock(&via_slash),
            static_write_lock(&via_index),
        ));
        assert_eq!(static_header_key(&via_slash), static_header_key(&via_index));
    }

    /// A unique `LeptosOptions` (distinct `site_root`) per call, so each test
    /// below gets its own `STATIC_HEADERS` key space — `site_root` is part of
    /// the cache key (see `STATIC_HEADERS`'s doc comment), so tests running in
    /// parallel never share an entry.
    fn unique_options(name: &str) -> LeptosOptions {
        let root = temp_dir_for(name);
        LeptosOptions::builder()
            .output_name(name)
            .site_root(root.to_string_lossy().to_string())
            .site_pkg_dir("pkg")
            .build()
    }

    // `write_static_route` + `open_paired_static_file`, called directly
    // (not through the full HTTP handler): `Some(response_options)` must
    // write the file to disk AND populate the `STATIC_HEADERS` cache under
    // the resolved file's key, so a subsequent paired open returns both the
    // file and the captured snapshot. Closes the gap where neither helper had
    // a direct call+assertion in this file.
    #[ntex::test]
    async fn write_static_route_with_response_options_caches_the_snapshot() {
        let options = unique_options("write_static_route_some");
        let response_options = ResponseOptions::default();
        response_options.set_status(StatusCode::CREATED);

        write_static_route(
            &options,
            Some(response_options),
            "/page",
            "hello".to_string(),
        )
        .await
        .unwrap();

        let root = PathBuf::from(&*options.site_root);
        let file_path = static_path(&options, "/page").unwrap();
        let header_key = static_header_key(&file_path);

        let (opened, headers) = open_paired_static_file(&root, &file_path, &header_key);
        assert!(
            opened.is_some(),
            "the file must exist on disk after write_static_route"
        );
        assert_eq!(
            headers.and_then(|parts| parts.status),
            Some(StatusCode::CREATED),
            "the captured status must be readable back through open_paired_static_file"
        );

        let _ = fs::remove_dir_all(&root);
    }

    // The `None` counterpart: `write_static_route` must still write the file
    // when no `ResponseOptions` was captured (a plain default-status render),
    // but must NOT insert a `STATIC_HEADERS` entry for it — a regression that
    // wrote to the cache even on `None` would make an unrelated later request
    // to the same path observe a snapshot that was never actually captured.
    #[ntex::test]
    async fn write_static_route_without_response_options_caches_nothing() {
        let options = unique_options("write_static_route_none");

        write_static_route(&options, None, "/page", "hello".to_string())
            .await
            .unwrap();

        let root = PathBuf::from(&*options.site_root);
        let file_path = static_path(&options, "/page").unwrap();
        let header_key = static_header_key(&file_path);

        let (opened, headers) = open_paired_static_file(&root, &file_path, &header_key);
        assert!(
            opened.is_some(),
            "the file must still be written to disk with no captured ResponseOptions"
        );
        assert!(
            headers.is_none(),
            "no STATIC_HEADERS entry must exist when response_options was None"
        );

        let _ = fs::remove_dir_all(&root);
    }

    // `open_paired_static_file` on a path whose file was never written (no
    // regeneration happened yet) must report a clean miss on BOTH halves of
    // the pair, not just the file — a real cache hit is defined as
    // `opened.is_some()`, so headers must never be populated when the file
    // itself is absent.
    #[ntex::test]
    async fn open_paired_static_file_reports_a_clean_miss_for_an_absent_file() {
        let options = unique_options("open_paired_missing");
        let root = PathBuf::from(&*options.site_root);
        fs::create_dir_all(&root).unwrap();
        let file_path = static_path(&options, "/never-written").unwrap();
        let header_key = static_header_key(&file_path);

        let (opened, headers) = open_paired_static_file(&root, &file_path, &header_key);
        assert!(opened.is_none(), "an absent file must not open");
        assert!(
            headers.is_none(),
            "headers must not be reported for a file that failed to open"
        );

        let _ = fs::remove_dir_all(&root);
    }
}
