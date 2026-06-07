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
use ntex::http::StatusCode;
use ntex::web::error::StateExtractorError;
use ntex::web::{self, ErrorRenderer, HttpRequest, HttpResponse, Route};
use or_poisoned::OrPoisoned;
use std::{
    fs,
    future::Future,
    io,
    num::NonZeroUsize,
    path::{Path, PathBuf},
    sync::{LazyLock, RwLock},
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
static STATIC_HEADERS: LazyLock<RwLock<lru::LruCache<String, ResponseParts>>> =
    LazyLock::new(|| {
        let capacity = std::env::var(STATIC_HEADERS_CAPACITY_ENV)
            .ok()
            .and_then(|value| value.parse::<usize>().ok())
            .and_then(NonZeroUsize::new)
            .unwrap_or(STATIC_HEADERS_DEFAULT_CAPACITY);
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

fn was_404(owner: &Owner) -> bool {
    let resp = owner.with(|| expect_context::<ResponseOptions>());
    let status = resp.0.read().or_poisoned().status;
    status == Some(StatusCode::NOT_FOUND)
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
        validate_static_parent(&root, &file_path)?;
        fs::write(file_path, html)?;
        Ok::<(), io::Error>(())
    })
    .await
    .map_err(io::Error::other)??;

    if let Some(snapshot) = snapshot {
        STATIC_HEADERS
            .write()
            .or_poisoned()
            .put(path.to_string(), snapshot);
    }

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

            let opened = ntex::rt::spawn_blocking({
                let root = root.clone();
                let path_buf = path_buf.clone();
                move || {
                    validate_static_file(&root, &path_buf)
                        .and_then(|path| ntex_files::NamedFile::open(path).ok())
                }
            })
            .await
            .ok()
            .flatten();

            let (response_options, html, opened) = if opened.is_none() {
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
                        was_404,
                        regenerate,
                    )
                    .await;
                let response_options = owner
                    .with(use_context::<ResponseOptions>)
                    .map(|options| options.0.read().or_poisoned().clone());
                // On a successful render `ResolvedStaticPath::build` writes
                // the page to disk and returns `None` (the body lives on
                // disk, not in memory), so re-open the freshly written file
                // and serve it. On a 404/error render it returns `Some(html)`
                // and skips the disk write, so we keep the in-memory body.
                // Without this re-open the `html == None` arm below would fall
                // through to a 500 on the first request to an un-pregenerated
                // static route. Mirrors `leptos_axum` (`ServeDir`) and
                // `leptos_actix` (`NamedFile::open`).
                let reopened = if html.is_none() {
                    let root = root.clone();
                    let path_buf = path_buf.clone();
                    ntex::rt::spawn_blocking(move || {
                        validate_static_file(&root, &path_buf)
                            .and_then(|path| ntex_files::NamedFile::open(path).ok())
                    })
                    .await
                    .ok()
                    .flatten()
                } else {
                    None
                };
                (response_options, html, reopened)
            } else {
                // `LruCache::get` updates recency, so it needs a write lock.
                let headers = STATIC_HEADERS
                    .write()
                    .or_poisoned()
                    .get(&orig_path)
                    .cloned();
                (headers, None, opened)
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
            let mut res = NtexResponse(match html {
                Some(html) => HttpResponse::NotFound()
                    .content_type("text/html")
                    .body(html),
                None => opened
                    .map(|named| named.into_response(&req))
                    .unwrap_or_else(|| HttpResponse::InternalServerError().finish()),
            });

            if let Some(options) = response_options {
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

    // ----- static_path: exhaustive spec --------------------------------
    // A pure URL-path -> on-disk-path resolver with two jobs: resolve the
    // happy path (append `.html`; map `/`, the empty path and trailing
    // slashes to `index.html`) and REJECT every traversal / dotfile /
    // smuggling shape. The old tests asserted only rejection (`None`); all
    // eight `Some` leaves below — the `.html` suffixing and the index
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
}
