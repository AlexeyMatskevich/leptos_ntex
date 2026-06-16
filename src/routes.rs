//! Route-list generation and executor initialization.
//!
//! Hosts the [`NtexRouteListing`] type used by
//! [`LeptosRoutes`](crate::LeptosRoutes) /
//! [`register_leptos_routes`](crate::register_leptos_routes), the
//! `generate_route_list*` family, and the crate-wide lazy installation of
//! `NtexExecutor` on `any_spawner`.

use hydration_context::SsrSharedContext;
use leptos::{IntoView, context::provide_context, reactive::owner::Owner};
use leptos_meta::ServerMetaContext;
use leptos_router::{
    ExpandOptionals, Method, PathSegment, RouteList, RouteListing, SsrMode, location::RequestUrl,
    static_routes::RegenerationFn,
};
use std::sync::{Arc, Mutex};

use crate::response::ResponseOptions;
use crate::server_fn::NtexExecutor;
use crate::static_routes::StaticRouteGenerator;

#[derive(Copy, Clone, Eq, PartialEq)]
enum ExecutorInitState {
    Unknown,
    NtexInstalled,
    ForeignInstalled,
}

static EXECUTOR_INIT_STATE: Mutex<ExecutorInitState> = Mutex::new(ExecutorInitState::Unknown);

fn init_ntex_executor() -> Result<(), any_spawner::ExecutorError> {
    let mut state = EXECUTOR_INIT_STATE
        .lock()
        .expect("executor init state poisoned");
    match *state {
        ExecutorInitState::NtexInstalled => Ok(()),
        ExecutorInitState::ForeignInstalled => Err(any_spawner::ExecutorError::AlreadySet),
        ExecutorInitState::Unknown => {
            match any_spawner::Executor::init_custom_executor(NtexExecutor) {
                Ok(()) => {
                    *state = ExecutorInitState::NtexInstalled;
                    Ok(())
                }
                Err(err) => {
                    *state = ExecutorInitState::ForeignInstalled;
                    Err(err)
                }
            }
        }
    }
}

/// A route that this application can serve.
///
/// Produced by [`generate_route_list`] and consumed by
/// [`LeptosRoutes::leptos_routes`](crate::LeptosRoutes::leptos_routes) or
/// [`register_leptos_routes`](crate::register_leptos_routes).
#[derive(Clone, Debug, Default)]
pub struct NtexRouteListing {
    pub(crate) path: String,
    pub(crate) mode: SsrMode,
    pub(crate) methods: Vec<Method>,
    pub(crate) regenerate: Vec<RegenerationFn>,
    pub(crate) exclude: bool,
}

trait NtexPath {
    fn to_ntex_path(&self) -> String;
}

impl NtexPath for Vec<PathSegment> {
    fn to_ntex_path(&self) -> String {
        let mut path = String::new();
        for segment in self {
            let raw = segment.as_raw_str();
            if !raw.is_empty() && !raw.starts_with('/') {
                path.push('/');
            }
            match segment {
                PathSegment::Static(s) => path.push_str(s),
                PathSegment::Param(s) => {
                    path.push('{');
                    path.push_str(s);
                    path.push('}');
                }
                PathSegment::Splat(s) => {
                    // ntex tail-segment syntax: `{name}*` matches the whole
                    // remainder of the path. The actix-style `{name:.*}` is a
                    // custom regex on a SINGLE segment in ntex-router, so it
                    // silently stops matching at the next `/` (nested URLs
                    // would fall through to the catch-all/fallback). Leptos
                    // splats are always terminal, which is exactly what the
                    // ntex tail match requires.
                    path.push('{');
                    path.push_str(s);
                    path.push_str("}*");
                }
                PathSegment::Unit => {}
                PathSegment::OptionalParam(_) => {
                    let msg = "to_ntex_path should only be called on expanded paths, \
                         which do not have OptionalParam any longer";
                    #[cfg(feature = "tracing")]
                    tracing::error!("{msg}");
                    #[cfg(not(feature = "tracing"))]
                    eprintln!("{msg}");
                }
            }
        }
        path
    }
}

trait IntoRouteListing {
    fn into_route_listing(self) -> Vec<NtexRouteListing>;
}

impl IntoRouteListing for RouteListing {
    fn into_route_listing(self) -> Vec<NtexRouteListing> {
        self.path()
            .to_vec()
            .expand_optionals()
            .into_iter()
            .map(|path| {
                let path = path.to_ntex_path();
                let path = if path.is_empty() {
                    "/".to_string()
                } else {
                    path
                };
                NtexRouteListing {
                    path,
                    mode: self.mode().clone(),
                    methods: self.methods().collect(),
                    regenerate: self.regenerate().into(),
                    exclude: false,
                }
            })
            .collect()
    }
}

impl NtexRouteListing {
    /// Creates a route listing from its parts.
    pub fn new(
        path: String,
        mode: SsrMode,
        methods: impl IntoIterator<Item = Method>,
        regenerate: impl Into<Vec<RegenerationFn>>,
    ) -> Self {
        Self {
            path,
            mode,
            methods: methods.into_iter().collect(),
            regenerate: regenerate.into(),
            exclude: false,
        }
    }

    /// The path this route handles.
    pub fn path(&self) -> &str {
        &self.path
    }

    /// The SSR rendering mode for this route.
    pub fn mode(&self) -> SsrMode {
        self.mode.clone()
    }

    /// The HTTP methods this route accepts.
    pub fn methods(&self) -> impl Iterator<Item = Method> + '_ {
        self.methods.iter().copied()
    }
}

/// Walks the Leptos router tree and returns a list of routes that can be
/// registered with ntex using [`LeptosRoutes::leptos_routes`](crate::LeptosRoutes::leptos_routes)
/// or [`register_leptos_routes`](crate::register_leptos_routes).
pub fn generate_route_list<IV>(
    app_fn: impl Fn() -> IV + 'static + Send + Clone,
) -> Vec<NtexRouteListing>
where
    IV: IntoView + 'static,
{
    generate_route_list_with_exclusions_and_ssg(app_fn, None).0
}

/// Like [`generate_route_list`] but also returns a [`StaticRouteGenerator`]
/// for building prerendered HTML files for every [`SsrMode::Static`] route.
pub fn generate_route_list_with_ssg<IV>(
    app_fn: impl Fn() -> IV + 'static + Send + Clone,
) -> (Vec<NtexRouteListing>, StaticRouteGenerator)
where
    IV: IntoView + 'static,
{
    generate_route_list_with_exclusions_and_ssg(app_fn, None)
}

/// Like [`generate_route_list`] but lets you mark certain paths as excluded
/// so a custom handler can be mounted at that route.
pub fn generate_route_list_with_exclusions<IV>(
    app_fn: impl Fn() -> IV + 'static + Send + Clone,
    excluded_routes: Option<Vec<String>>,
) -> Vec<NtexRouteListing>
where
    IV: IntoView + 'static,
{
    generate_route_list_with_exclusions_and_ssg(app_fn, excluded_routes).0
}

/// Combines [`generate_route_list_with_exclusions`] and
/// [`generate_route_list_with_ssg`].
///
/// Exclusions affect only the returned route **listings** (which paths get
/// registered as handlers): an excluded path is marked `exclude` so you can
/// mount a custom handler there. They do **not** narrow the
/// [`StaticRouteGenerator`] — `generate()` still prerenders every
/// `SsrMode::Static` route, including excluded ones. This mirrors the
/// reference `leptos_axum`/`leptos_actix` adapters; build the generator from a
/// filtered app if you need SSG to skip a route.
pub fn generate_route_list_with_exclusions_and_ssg<IV>(
    app_fn: impl Fn() -> IV + 'static + Send + Clone,
    excluded_routes: Option<Vec<String>>,
) -> (Vec<NtexRouteListing>, StaticRouteGenerator)
where
    IV: IntoView + 'static,
{
    generate_route_list_with_exclusions_and_ssg_and_context(app_fn, excluded_routes, || {})
}

pub(crate) fn ensure_executor_initialized() {
    // The outer `Once` keeps this call zero-allocation on the fast path
    // after the first init. `any_spawner::Executor::init_custom_executor`
    // internally `Box::new`s the executor before attempting its own
    // `OnceLock::set`, so calling it per-request would allocate every
    // time even though the set would silently fail.
    static INIT: std::sync::Once = std::sync::Once::new();
    INIT.call_once(|| {
        if let Err(err) = init_ntex_executor() {
            // Another async executor (tokio, glib, futures-executor, or
            // a different Leptos integration) was installed first.
            // `any_spawner` is globally one-shot, so Leptos tasks will
            // now run on *that* executor instead of `ntex::rt`. Apps
            // that want to fail fast should call
            // `try_init_executor()` at startup.
            //
            // Always emit to stderr so the conflict is visible in the
            // default feature configuration (no `tracing` feature), and
            // additionally emit a structured record via `tracing` when
            // that feature is enabled so it integrates with the app's
            // subscriber setup.
            let msg = format!(
                "leptos_ntex_unofficial: another async executor is already installed \
                 via `any_spawner` (error: {err}). Leptos async tasks will run on \
                 that executor instead of ntex::rt. Call \
                 `leptos_ntex_unofficial::try_init_executor()` at startup to surface \
                 this error deterministically. See the `Request` wrapper docs for \
                 cross-thread-drop caveats."
            );
            // `tracing::warn!` only lives when the feature is on;
            // `eprintln!` runs unconditionally so the conflict is not
            // silent in the default build.
            #[cfg(feature = "tracing")]
            tracing::warn!(error = ?err, "{msg}");
            eprintln!("warning: {msg}");
        }
    });
}

/// Attempts to install the ntex-backed executor with [`any_spawner`].
///
/// Most apps never need to call this — every rendering helper and
/// server-function handler in this crate performs the install lazily the
/// first time it runs. Call this explicitly when the app mixes multiple
/// runtimes (e.g. tokio + ntex, or two Leptos integrations) and you want
/// to fail fast at startup rather than discover the conflict under load.
///
/// Returns `Err(ExecutorError::AlreadySet)` if *another* executor won the
/// global one-shot install race. When that happens, Leptos async tasks
/// run on the foreign executor instead of `ntex::rt`; cross-thread drops
/// of [`Request`](crate::Request) may leak or panic — see the `Request`
/// documentation.
///
/// Safe to call repeatedly: once this crate has installed its own
/// executor, later calls return `Ok(())` without touching
/// `any_spawner` again. If a foreign executor was already installed, the
/// `AlreadySet` result is cached so later lazy initialization does not
/// emit misleading diagnostics.
pub fn try_init_executor() -> Result<(), any_spawner::ExecutorError> {
    init_ntex_executor()
}

/// Most general form of route list generation — lets you inject additional
/// values into the reactive context while the routes are being walked.
pub fn generate_route_list_with_exclusions_and_ssg_and_context<IV>(
    app_fn: impl Fn() -> IV + 'static + Send + Clone,
    excluded_routes: Option<Vec<String>>,
    additional_context: impl Fn() + 'static + Send + Clone,
) -> (Vec<NtexRouteListing>, StaticRouteGenerator)
where
    IV: IntoView + 'static,
{
    ensure_executor_initialized();

    let owner = Owner::new_root(Some(Arc::new(SsrSharedContext::new())));
    let (mock_meta, _) = ServerMetaContext::new();
    let routes = owner
        .with(|| {
            provide_context(RequestUrl::new(""));
            provide_context(ResponseOptions::default());
            provide_context(mock_meta);
            additional_context();
            RouteList::generate(&app_fn)
        })
        .unwrap_or_default();

    let generator = StaticRouteGenerator::new(&routes, app_fn.clone(), additional_context.clone());

    let routes = routes
        .into_inner()
        .into_iter()
        .flat_map(IntoRouteListing::into_route_listing)
        .collect::<Vec<_>>();

    // Synthesize the fallback `/` listing when the app declared no routes, so
    // an empty app still serves the shell. Apply exclusions UNCONDITIONALLY
    // afterwards: the previous `else`-only filter let an excluded `/` slip
    // through in the empty case, leaving an active synthetic `/` that shadowed
    // a custom root handler. This is a conscious divergence from the
    // `leptos_axum` / `leptos_actix` reference adapters, which share the same
    // gap — see UPSTREAM_ISSUES_RU.md.
    let mut routes = if routes.is_empty() {
        vec![NtexRouteListing::new(
            "/".to_string(),
            Default::default(),
            [Method::Get],
            vec![],
        )]
    } else {
        routes
    };

    if let Some(excluded_routes) = &excluded_routes {
        routes.retain(|p| !excluded_routes.iter().any(|e| e == p.path()))
    }

    let excluded = excluded_routes
        .into_iter()
        .flatten()
        .map(|path| NtexRouteListing {
            path,
            mode: Default::default(),
            methods: Vec::new(),
            regenerate: Vec::new(),
            exclude: true,
        });

    (routes.into_iter().chain(excluded).collect(), generator)
}
