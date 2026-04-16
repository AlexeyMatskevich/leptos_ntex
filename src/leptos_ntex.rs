#![forbid(unsafe_code)]
#![deny(missing_docs)]

//! Provides functions to easily integrate Leptos with [ntex].
//!
//! This adapter mirrors the public API of `leptos_actix` so that existing
//! Leptos SSR code can be migrated to ntex with minimal friction. `ntex` is
//! an evolution of `actix-web` by the same author and keeps most of the same
//! ideas, which is why the shapes of the two integrations stay close.
//!
//! For end-to-end usage examples look at the [integration tests][tests] that
//! ship with this crate and at the official [Leptos examples][examples].
//!
//! [tests]: https://github.com/amourlive/leptos_ntex/tree/main/src/lib.rs
//! [examples]: https://github.com/leptos-rs/leptos/tree/main/examples

use bytes::{Bytes as SfBytes, BytesMut as SfBytesMut};
use futures::{Sink, Stream, StreamExt, channel::mpsc, stream::once};
use ntex::http::{Method as HttpMethod, StatusCode};
use hydration_context::SsrSharedContext;
use leptos::{
    IntoView,
    config::LeptosOptions,
    context::{provide_context, use_context},
    hydration::IslandsRouterNavigation,
    prelude::expect_context,
    reactive::{computed::ScopedFuture, owner::Owner},
};
use leptos_integration_utils::{
    BoxedFnOnce, ExtendResponse, PinnedFuture, PinnedStream, build_response,
    static_file_path,
};
use leptos_meta::ServerMetaContext;
use leptos_router::{
    ExpandOptionals, Method, PathSegment, RouteList, RouteListing, SsrMode,
    components::provide_server_redirect,
    location::RequestUrl,
    static_routes::{RegenerationFn, ResolvedStaticPath},
};
use ntex::{
    http::{
        Payload,
        header::{self, HeaderName, HeaderValue},
    },
    util::Bytes as NBytes,
    web::{self, App, ErrorRenderer, HttpRequest, HttpResponse, Route, ServiceConfig},
};
use or_poisoned::OrPoisoned;
use send_wrapper::SendWrapper;
use server_fn::{
    Protocol, ServerFn, ServerFnTraitObj,
    error::{FromServerFnError, IntoAppError},
    middleware::BoxedService,
    redirect::REDIRECT_HEADER,
    request::Req,
    response::{Res, TryRes},
    server::Server,
};
use std::{
    borrow::Cow,
    collections::{HashMap, HashSet},
    fmt::Display,
    fs,
    future::Future,
    io,
    path::Path,
    sync::{Arc, LazyLock, Mutex, RwLock},
};

use ntex::web::error::StateExtractorError;

/// Indexes server functions by their `&'static str` path, so per-request
/// lookup doesn't need to allocate a `String` to build the map key.
/// Multiple methods under the same path are supported by a small `Vec`
/// (typical server-fn apps have one method per path).
type LazyServerFnMap<Req, Res> =
    LazyLock<RwLock<HashMap<&'static str, Vec<(HttpMethod, ServerFnTraitObj<Req, Res>)>>>>;

/// A boxed stream of HTML chunks, as used for progressive streaming of SSR
/// responses. Mirrors the equivalent type alias in `leptos_axum`.
pub type PinnedHtmlStream =
    std::pin::Pin<Box<dyn Stream<Item = io::Result<NBytes>> + Send>>;

/// Describes overrides for the HTTP response headers and status code.
///
/// Typically held inside a [`ResponseOptions`]. Useful for setting cookies or
/// customising the status code from a server function or a component.
#[derive(Debug, Clone, Default)]
pub struct ResponseParts {
    /// When set, overrides any other status code for this response.
    pub status: Option<StatusCode>,
    /// Extra headers to add to the response.
    pub headers: header::HeaderMap,
}

impl ResponseParts {
    /// Inserts a header, overwriting any previous value with the same key.
    pub fn insert_header(&mut self, key: header::HeaderName, value: header::HeaderValue) {
        self.headers.insert(key, value);
    }

    /// Appends a header, leaving any header with the same key intact.
    pub fn append_header(&mut self, key: header::HeaderName, value: header::HeaderValue) {
        self.headers.append(key, value);
    }
}

/// Shared, mutable override for the outgoing HTTP response.
///
/// Injected as a context value during SSR and inside server functions so that
/// user code can change the status and headers of the response.
#[derive(Debug, Clone, Default)]
pub struct ResponseOptions(pub Arc<RwLock<ResponseParts>>);

impl ResponseOptions {
    /// Replaces the inner [`ResponseParts`] wholesale.
    pub fn overwrite(&self, parts: ResponseParts) {
        let mut writable = self.0.write().or_poisoned();
        *writable = parts;
    }

    /// Sets the HTTP status that will be returned for this response.
    pub fn set_status(&self, status: StatusCode) {
        let mut writable = self.0.write().or_poisoned();
        writable.status = Some(status);
    }

    /// Inserts a header, overwriting any previous value with the same key.
    pub fn insert_header(&self, key: header::HeaderName, value: header::HeaderValue) {
        let mut writable = self.0.write().or_poisoned();
        writable.headers.insert(key, value);
    }

    /// Appends a header, leaving any header with the same key intact.
    pub fn append_header(&self, key: header::HeaderName, value: header::HeaderValue) {
        let mut writable = self.0.write().or_poisoned();
        writable.headers.append(key, value);
    }
}

/// A wrapper for an ntex [`HttpRequest`] that can be placed in Leptos's
/// `Send`/`Sync` context API.
///
/// ntex's [`HttpRequest`] is not [`Send`], so it is wrapped in a
/// [`SendWrapper`]. The wrapper panics if dropped on another thread — which
/// can happen during static-route prerendering, where Leptos owners may be
/// moved across thread boundaries.
///
/// ### Cross-thread drop: leak vs. panic
///
/// The [`Drop`] implementation checks [`SendWrapper::valid`] and, when the
/// drop is happening off-thread, calls [`std::mem::forget`] on the inner
/// wrapper instead of panicking. ntex's `HttpRequest` is internally
/// reference-counted, so forgetting the wrapper leaks one `Rc` increment
/// worth of state. This is deliberately preferred over panicking (which
/// would tear down the arbiter).
///
/// The leak size is bounded by the number of `Request` instances whose
/// owning reactive context gets moved across threads before drop — this
/// includes static-route prerendering at SSG time and any user-introduced
/// `spawn_blocking` / cross-thread `Owner` pattern. For in-request SSR on
/// a single arbiter the wrapper always drops on its creator thread and no
/// leak occurs.
///
/// ### Cross-thread clone, deref, into_inner
///
/// The [`Clone`], [`Deref`](std::ops::Deref), [`DerefMut`](std::ops::DerefMut)
/// and [`Request::into_inner`] operations dereference the inner
/// [`SendWrapper`]. If called from a thread other than the one on which
/// the `Request` was constructed they will panic (same invariant as any
/// [`SendWrapper`]). These paths do **not** have the leak-instead-of-panic
/// safety net — only [`Drop`] does. In practice you only hit this when you
/// explicitly move a `Request` onto a different worker and try to read it
/// there, which should be avoided.
#[derive(Debug)]
pub struct Request(Option<SendWrapper<HttpRequest>>);

impl Request {
    /// Wraps an existing ntex request.
    pub fn new(req: &HttpRequest) -> Self {
        Self(Some(SendWrapper::new(req.clone())))
    }

    /// Consumes the wrapper and returns the inner ntex request.
    pub fn into_inner(mut self) -> HttpRequest {
        self.0
            .take()
            .expect("Request should always contain an HttpRequest")
            .take()
    }
}

impl Clone for Request {
    fn clone(&self) -> Self {
        Self::new(self)
    }
}

impl std::ops::Deref for Request {
    type Target = HttpRequest;

    fn deref(&self) -> &Self::Target {
        self.0
            .as_ref()
            .expect("Request should always contain an HttpRequest")
    }
}

impl std::ops::DerefMut for Request {
    fn deref_mut(&mut self) -> &mut Self::Target {
        self.0
            .as_mut()
            .expect("Request should always contain an HttpRequest")
    }
}

impl Drop for Request {
    fn drop(&mut self) {
        if let Some(req) = self.0.take() {
            if req.valid() {
                drop(req);
            } else {
                std::mem::forget(req);
            }
        }
    }
}

struct NtexResponse(HttpResponse);

impl NtexResponse {
    fn take(self) -> HttpResponse {
        self.0
    }
}

impl ExtendResponse for NtexResponse {
    type ResponseOptions = ResponseOptions;

    fn from_stream(stream: impl Stream<Item = String> + Send + 'static) -> Self {
        let pinned = Box::pin(stream.map(|chunk| {
            Ok::<NBytes, io::Error>(NBytes::from(chunk))
        }));
        NtexResponse(
            HttpResponse::Ok()
                .content_type("text/html")
                .streaming(pinned),
        )
    }

    fn extend_response(&mut self, res_options: &Self::ResponseOptions) {
        let taken = std::mem::take(&mut *res_options.0.write().or_poisoned());
        let headers = self.0.headers_mut();
        for (key, value) in taken.headers.iter() {
            headers.append(key.clone(), value.clone());
        }
        if let Some(status) = taken.status {
            *self.0.status_mut() = status;
        }
    }

    fn set_default_content_type(&mut self, content_type: &str) {
        let headers = self.0.headers_mut();
        if !headers.contains_key(header::CONTENT_TYPE) {
            headers.insert(
                header::CONTENT_TYPE,
                HeaderValue::from_str(content_type).unwrap(),
            );
        }
    }
}

/// Redirects the browser from within a server function.
///
/// Depending on the `Accept` header on the current request the function
/// either sets a `302 Found` (for plain `<form>` submissions) or emits a
/// custom [`REDIRECT_HEADER`] that the Leptos client picks up to perform a
/// client-side navigation while still letting the server fn return its
/// payload. The `Location` header is always set.
///
/// Must be called while a [`Request`] and a [`ResponseOptions`] are present
/// in the current reactive context — i.e. from inside a route handler or a
/// server function.
#[cfg_attr(
    feature = "tracing",
    tracing::instrument(level = "trace", fields(error), skip_all)
)]
pub fn redirect(path: &str) {
    if let (Some(req), Some(res)) = (use_context::<Request>(), use_context::<ResponseOptions>()) {
        res.insert_header(
            header::LOCATION,
            HeaderValue::from_str(path).expect("failed to create header"),
        );

        let accepts_html = req
            .headers()
            .get(header::ACCEPT)
            .and_then(|v| v.to_str().ok())
            .map(|v| v.contains("text/html"))
            .unwrap_or(false);

        if accepts_html {
            res.set_status(StatusCode::FOUND);
        } else {
            res.insert_header(
                HeaderName::from_static(REDIRECT_HEADER),
                HeaderValue::from_static(""),
            );
        }
    } else {
        #[cfg(feature = "tracing")]
        tracing::warn!(
            "Couldn't retrieve either Parts or ResponseOptions while trying to redirect()."
        );
    }
}

/// A route that this application can serve.
///
/// Produced by [`generate_route_list`] and consumed by
/// [`LeptosRoutes::leptos_routes`] or [`register_leptos_routes`].
#[derive(Clone, Debug, Default)]
pub struct NtexRouteListing {
    path: String,
    mode: SsrMode,
    methods: Vec<Method>,
    regenerate: Vec<RegenerationFn>,
    exclude: bool,
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
                    path.push('{');
                    path.push_str(s);
                    path.push_str(":.*}");
                }
                PathSegment::Unit => {}
                PathSegment::OptionalParam(_) => {
                    #[cfg(feature = "tracing")]
                    tracing::error!(
                        "to_ntex_path should only be called on expanded paths, \
                         which do not have OptionalParam any longer"
                    );
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
                let path = if path.is_empty() { "/".to_string() } else { path };
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

fn leptos_corrected_path(req: &HttpRequest) -> String {
    let path = req.path();
    let query = req.query_string();
    if query.is_empty() {
        format!("http://leptos{path}")
    } else {
        format!("http://leptos{path}?{query}")
    }
}

fn provide_contexts(req: Request, meta_context: &ServerMetaContext, res_options: &ResponseOptions) {
    provide_context(RequestUrl::new(&leptos_corrected_path(&req)));
    provide_context(meta_context.clone());
    provide_context(res_options.clone());
    provide_context(req);
    provide_server_redirect(redirect);
    leptos::nonce::provide_nonce();
}

fn async_stream_builder<IV>(
    app: IV,
    chunks: BoxedFnOnce<PinnedStream<String>>,
    _supports_ooo: bool,
) -> PinnedFuture<PinnedStream<String>>
where
    IV: IntoView + 'static,
{
    Box::pin(async move {
        let app = if cfg!(feature = "islands-router") {
            app.to_html_stream_in_order_branching()
        } else {
            app.to_html_stream_in_order()
        };
        let app = app.collect::<String>().await;
        let chunks = chunks();
        Box::pin(once(async move { app }).chain(chunks)) as PinnedStream<String>
    })
}

fn ntex_method(method: Method) -> ntex::http::Method {
    match method {
        Method::Get => ntex::http::Method::GET,
        Method::Post => ntex::http::Method::POST,
        Method::Put => ntex::http::Method::PUT,
        Method::Delete => ntex::http::Method::DELETE,
        Method::Patch => ntex::http::Method::PATCH,
    }
}

#[allow(clippy::type_complexity)]
fn handle_response<IV, Err>(
    method: Method,
    additional_context: impl Fn() + 'static + Clone + Send,
    app_fn: impl Fn() -> IV + Clone + Send + 'static,
    stream_builder: fn(
        IV,
        BoxedFnOnce<PinnedStream<String>>,
        bool,
    ) -> PinnedFuture<PinnedStream<String>>,
) -> Route<Err>
where
    Err: ErrorRenderer,
    IV: IntoView + 'static,
{
    ensure_executor_initialized();
    let handler = move |req: HttpRequest| {
        let app_fn = app_fn.clone();
        let add_context = additional_context.clone();
        async move {
            let is_island_router_navigation = cfg!(feature = "islands-router")
                && req.headers().contains_key("Islands-Router");
            let res_options = ResponseOptions::default();
            let (meta_context, meta_output) = ServerMetaContext::new();

            let additional_context = {
                let meta_context = meta_context.clone();
                let res_options = res_options.clone();
                let req = Request::new(&req);
                move || {
                    provide_contexts(req, &meta_context, &res_options);
                    add_context();
                    if is_island_router_navigation {
                        provide_context(IslandsRouterNavigation);
                    }
                }
            };

            let res = NtexResponse::from_app(
                app_fn,
                meta_output,
                additional_context,
                res_options,
                stream_builder,
                !is_island_router_navigation,
            )
            .await;

            res.take()
        }
    };
    Route::<Err>::new().method(ntex_method(method)).to(handler)
}

/// Low-level building block: runs the SSR pipeline for a single request
/// and returns the resulting [`HttpResponse`] future.
///
/// Exposed so that advanced users can compose it inside their own ntex
/// route handlers — for example, to render a shell after a custom
/// static-file fallback, or to add middleware that needs to inspect the
/// response body before sending. The `render_app_*` family doesn't use
/// this directly (they wrap a similar pipeline in a `Route`), but this
/// function mirrors `leptos_axum::handle_response_inner` so existing
/// axum-style code can be adapted.
///
/// The `stream_builder` argument selects how the HTML body is produced:
/// out-of-order / in-order / async. See the [`render_app_to_stream`] family
/// for common choices.
#[allow(clippy::type_complexity)]
pub fn handle_response_inner<IV>(
    additional_context: impl FnOnce() + 'static + Send,
    app_fn: impl FnOnce() -> IV + Send + 'static,
    req: HttpRequest,
    stream_builder: fn(
        IV,
        BoxedFnOnce<PinnedStream<String>>,
        bool,
    ) -> PinnedFuture<PinnedStream<String>>,
) -> PinnedFuture<HttpResponse>
where
    IV: IntoView + 'static,
{
    ensure_executor_initialized();
    Box::pin(SendWrapper::new(async move {
        let is_island_router_navigation = cfg!(feature = "islands-router")
            && req.headers().contains_key("Islands-Router");
        let res_options = ResponseOptions::default();
        let (meta_context, meta_output) = ServerMetaContext::new();

        let cx = {
            let meta_context = meta_context.clone();
            let res_options = res_options.clone();
            let req_ctx = Request::new(&req);
            move || {
                provide_contexts(req_ctx, &meta_context, &res_options);
                additional_context();
                if is_island_router_navigation {
                    provide_context(IslandsRouterNavigation);
                }
            }
        };

        let res = NtexResponse::from_app(
            app_fn,
            meta_output,
            cx,
            res_options,
            stream_builder,
            !is_island_router_navigation,
        )
        .await;

        res.take()
    }))
}

/// Returns an ntex [`Route`] that responds to a request for the given
/// [`Method`] by rendering your app as an out-of-order HTML stream.
///
/// The stream includes fallback content for any `<Suspense/>` nodes, is
/// immediately interactive, and requires some client-side JavaScript.
///
/// ## Provided Context Types
/// - [`ResponseOptions`]
/// - [`Request`]
/// - [`ServerMetaContext`](leptos_meta::ServerMetaContext)
#[cfg_attr(
    feature = "tracing",
    tracing::instrument(level = "trace", fields(error), skip_all)
)]
pub fn render_app_to_stream<IV, Err>(
    app_fn: impl Fn() -> IV + Clone + Send + 'static,
    method: Method,
) -> Route<Err>
where
    Err: ErrorRenderer,
    IV: IntoView + 'static,
{
    render_app_to_stream_with_context(|| {}, app_fn, method)
}

/// Returns an ntex [`Route`] that responds by rendering your app as an
/// in-order HTML stream.
///
/// The stream pauses at each `<Suspense/>` node and waits for it to resolve
/// before sending down its HTML. The app becomes interactive only once it
/// has fully loaded.
#[cfg_attr(
    feature = "tracing",
    tracing::instrument(level = "trace", fields(error), skip_all)
)]
pub fn render_app_to_stream_in_order<IV, Err>(
    app_fn: impl Fn() -> IV + Clone + Send + 'static,
    method: Method,
) -> Route<Err>
where
    Err: ErrorRenderer,
    IV: IntoView + 'static,
{
    render_app_to_stream_in_order_with_context(|| {}, app_fn, method)
}

/// Returns an ntex [`Route`] that renders the app asynchronously, emitting a
/// single HTML body once every `async` resource has loaded.
#[cfg_attr(
    feature = "tracing",
    tracing::instrument(level = "trace", fields(error), skip_all)
)]
pub fn render_app_async<IV, Err>(
    app_fn: impl Fn() -> IV + Clone + Send + 'static,
    method: Method,
) -> Route<Err>
where
    Err: ErrorRenderer,
    IV: IntoView + 'static,
{
    render_app_async_with_context(|| {}, app_fn, method)
}

/// Variant of [`render_app_to_stream`] that lets you inject additional values
/// into the reactive context when handling a route.
#[cfg_attr(
    feature = "tracing",
    tracing::instrument(level = "trace", fields(error), skip_all)
)]
pub fn render_app_to_stream_with_context<IV, Err>(
    additional_context: impl Fn() + 'static + Clone + Send,
    app_fn: impl Fn() -> IV + Clone + Send + 'static,
    method: Method,
) -> Route<Err>
where
    Err: ErrorRenderer,
    IV: IntoView + 'static,
{
    render_app_to_stream_with_context_and_replace_blocks(
        additional_context,
        app_fn,
        method,
        false,
    )
}

/// Variant of [`render_app_to_stream_with_context`] that additionally
/// controls whether `<Suspense/>` fragments reading blocking resources are
/// retrojected into the initially served HTML instead of being inserted by
/// client-side JavaScript.
///
/// ⚠ **Currently a no-op:** Leptos's HTML streaming APIs do not yet expose
/// a `replace_blocks` toggle, so this argument is accepted for API parity
/// with `leptos_actix` / `leptos_axum` but has no effect. This means
/// [`SsrMode::PartiallyBlocked`](leptos_router::SsrMode) produces the same
/// HTML stream as [`SsrMode::OutOfOrder`](leptos_router::SsrMode) across
/// all three integrations until upstream Leptos wires this through.
#[cfg_attr(
    feature = "tracing",
    tracing::instrument(level = "trace", fields(error), skip_all)
)]
pub fn render_app_to_stream_with_context_and_replace_blocks<IV, Err>(
    additional_context: impl Fn() + 'static + Clone + Send,
    app_fn: impl Fn() -> IV + Clone + Send + 'static,
    method: Method,
    replace_blocks: bool,
) -> Route<Err>
where
    Err: ErrorRenderer,
    IV: IntoView + 'static,
{
    // TODO(upstream): Leptos's HTML stream APIs (`to_html_stream_out_of_order`,
    // `to_html_stream_in_order`, etc.) do not currently expose a flag to
    // retroject blocking `<Suspense/>` fragments into the initial payload.
    // This argument is accepted for API parity with `leptos_actix` and
    // `leptos_axum` (both of which have the same `_ = replace_blocks;`
    // placeholder) but has no effect here or there until upstream wires it
    // through. Track: https://github.com/leptos-rs/leptos (no dedicated
    // issue — search for "replace_blocks" / "PartiallyBlocked").
    _ = replace_blocks;
    handle_response(
        method,
        additional_context,
        app_fn,
        |app, chunks, supports_ooo| {
            Box::pin(async move {
                let app = if cfg!(feature = "islands-router") {
                    if supports_ooo {
                        app.to_html_stream_out_of_order_branching()
                    } else {
                        app.to_html_stream_in_order_branching()
                    }
                } else if supports_ooo {
                    app.to_html_stream_out_of_order()
                } else {
                    app.to_html_stream_in_order()
                };
                Box::pin(app.chain(chunks())) as PinnedStream<String>
            })
        },
    )
}

/// Variant of [`render_app_to_stream_in_order`] that lets you inject
/// additional values into the reactive context when handling a route.
#[cfg_attr(
    feature = "tracing",
    tracing::instrument(level = "trace", fields(error), skip_all)
)]
pub fn render_app_to_stream_in_order_with_context<IV, Err>(
    additional_context: impl Fn() + 'static + Clone + Send,
    app_fn: impl Fn() -> IV + Clone + Send + 'static,
    method: Method,
) -> Route<Err>
where
    Err: ErrorRenderer,
    IV: IntoView + 'static,
{
    handle_response(
        method,
        additional_context,
        app_fn,
        |app, chunks, _supports_ooo| {
            Box::pin(async move {
                let app = if cfg!(feature = "islands-router") {
                    app.to_html_stream_in_order_branching()
                } else {
                    app.to_html_stream_in_order()
                };
                Box::pin(app.chain(chunks())) as PinnedStream<String>
            })
        },
    )
}

/// Variant of [`render_app_async`] that lets you inject additional values
/// into the reactive context when handling a route.
#[cfg_attr(
    feature = "tracing",
    tracing::instrument(level = "trace", fields(error), skip_all)
)]
pub fn render_app_async_with_context<IV, Err>(
    additional_context: impl Fn() + 'static + Clone + Send,
    app_fn: impl Fn() -> IV + Clone + Send + 'static,
    method: Method,
) -> Route<Err>
where
    Err: ErrorRenderer,
    IV: IntoView + 'static,
{
    handle_response(method, additional_context, app_fn, async_stream_builder)
}

/// Walks the Leptos router tree and returns a list of routes that can be
/// registered with ntex using [`LeptosRoutes::leptos_routes`] or
/// [`register_leptos_routes`].
pub fn generate_route_list<IV>(app_fn: impl Fn() -> IV + 'static + Send + Clone) -> Vec<NtexRouteListing>
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
pub fn generate_route_list_with_exclusions_and_ssg<IV>(
    app_fn: impl Fn() -> IV + 'static + Send + Clone,
    excluded_routes: Option<Vec<String>>,
) -> (Vec<NtexRouteListing>, StaticRouteGenerator)
where
    IV: IntoView + 'static,
{
    generate_route_list_with_exclusions_and_ssg_and_context(app_fn, excluded_routes, || {})
}

fn ensure_executor_initialized() {
    // The outer `Once` keeps this call zero-allocation on the fast path
    // after the first init. `any_spawner::Executor::init_custom_executor`
    // internally `Box::new`s the executor before attempting its own
    // `OnceLock::set`, so calling it per-request would allocate every
    // time even though the set would silently fail. The inner
    // `init_custom_executor` can still fail once — if something *else*
    // installed a global executor before we got here — which is why we
    // swallow the result with `let _ = ...`.
    static INIT: std::sync::Once = std::sync::Once::new();
    INIT.call_once(|| {
        let _ = any_spawner::Executor::init_custom_executor(NtexExecutor);
    });
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

    let mut routes = routes
        .into_inner()
        .into_iter()
        .flat_map(IntoRouteListing::into_route_listing)
        .collect::<Vec<_>>();

    let routes = if routes.is_empty() {
        vec![NtexRouteListing::new(
            "/".to_string(),
            Default::default(),
            [Method::Get],
            vec![],
        )]
    } else {
        if let Some(excluded_routes) = &excluded_routes {
            routes.retain(|p| !excluded_routes.iter().any(|e| e == p.path()))
        }
        routes
    };

    let excluded = excluded_routes.into_iter().flatten().map(|path| NtexRouteListing {
        path,
        mode: Default::default(),
        methods: Vec::new(),
        regenerate: Vec::new(),
        exclude: true,
    });

    (routes.into_iter().chain(excluded).collect(), generator)
}

/// Allows generating prerendered static HTML for every [`SsrMode::Static`]
/// route in the application.
///
/// Produced by [`generate_route_list_with_ssg`]. Call
/// [`StaticRouteGenerator::generate`] once the [`LeptosOptions`] are known —
/// typically at the end of a pre-build step that runs before the server
/// starts serving traffic.
#[allow(clippy::type_complexity)]
pub struct StaticRouteGenerator(
    // Kept alive so that any context values provided during generation stay
    // valid for the duration of the static rendering pipeline.
    #[allow(dead_code)] Owner,
    Box<dyn FnOnce(&LeptosOptions) -> PinnedFuture<()> + Send>,
);

impl StaticRouteGenerator {
    fn render_route<IV: IntoView + 'static>(
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

fn handle_static_route<IV, Err>(
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

            let exists = path_buf.exists();

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
    Route::<Err>::new().method(ntex::http::Method::GET).to(handler)
}

/// Creates a file-serving [`ntex_files::Files`] service for the
/// `options.site_pkg_dir` directory under `options.site_root`.
///
/// Handy for registering the JS/WASM/CSS assets produced by `cargo-leptos`:
///
/// ```no_run
/// use ntex::web::App as NtexApp;
/// use leptos::config::LeptosOptions;
/// use leptos_ntex::leptos_ntex::site_pkg_dir_service;
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
/// use leptos_ntex::leptos_ntex::file_and_error_handler;
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
            let uri_path = req.uri().path().trim_start_matches('/');

            let opened = if uri_path.is_empty() {
                None
            } else {
                let candidate = Path::new(&*options.site_root).join(uri_path);
                ntex::rt::spawn_blocking(move || ntex_files::NamedFile::open(&candidate))
                    .await
                    .ok()
                    .and_then(|r| r.ok())
            };

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

            let mut res = NtexResponse::from_app(
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
    Route::<Err>::new().method(ntex::http::Method::GET).to(handler)
}

/// Adds a list of [`NtexRouteListing`]s and a Leptos app to an ntex router,
/// avoiding the need to use wildcards or to define every route manually.
pub trait LeptosRoutes {
    /// Registers routes that have been either
    /// 1. generated by `leptos_router`, or
    /// 2. emitted to handle a server function.
    fn leptos_routes<IV>(
        self,
        paths: Vec<NtexRouteListing>,
        app_fn: impl Fn() -> IV + Clone + Send + 'static,
    ) -> Self
    where
        IV: IntoView + 'static;

    /// Like [`LeptosRoutes::leptos_routes`], but runs `additional_context`
    /// for every request so you can inject more data into the reactive
    /// context.
    fn leptos_routes_with_context<IV>(
        self,
        paths: Vec<NtexRouteListing>,
        additional_context: impl Fn() + 'static + Clone + Send,
        app_fn: impl Fn() -> IV + Clone + Send + 'static,
    ) -> Self
    where
        IV: IntoView + 'static;
}

impl<M, T, Err> LeptosRoutes for App<M, T, Err>
where
    T: ntex::service::ServiceFactory<
            ntex::web::WebRequest<Err>,
            ntex::service::cfg::SharedCfg,
            Response = ntex::web::WebRequest<Err>,
            Error = Err::Container,
            InitError = (),
        >,
    Err: ErrorRenderer,
    Err::Container: From<StateExtractorError>,
{
    fn leptos_routes<IV>(
        self,
        paths: Vec<NtexRouteListing>,
        app_fn: impl Fn() -> IV + Clone + Send + 'static,
    ) -> Self
    where
        IV: IntoView + 'static,
    {
        self.leptos_routes_with_context(paths, || {}, app_fn)
    }

    fn leptos_routes_with_context<IV>(
        mut self,
        paths: Vec<NtexRouteListing>,
        additional_context: impl Fn() + 'static + Clone + Send,
        app_fn: impl Fn() -> IV + Clone + Send + 'static,
    ) -> Self
    where
        IV: IntoView + 'static,
    {
        let excluded = paths
            .iter()
            .filter(|p| p.exclude)
            .map(|p| p.path.as_str())
            .collect::<HashSet<_>>();

        for (path, method) in server_fn_paths() {
            if !excluded.contains(path) {
                let additional_context = additional_context.clone();
                let handler = handle_server_fns_with_context(additional_context).method(method);
                self = self.route(path, handler);
            }
        }

        for listing in paths.iter().filter(|p| !p.exclude) {
            let path = listing.path();
            let mode = listing.mode();
            let is_static = matches!(mode, SsrMode::Static(_));

            // Register a single HEAD handler per path for non-Static routes
            // (outside the per-method loop, so GET+POST routes don't get two
            // identical HEAD handlers).
            if !is_static {
                self = self.route(
                    path,
                    Route::<Err>::new()
                        .method(ntex::http::Method::HEAD)
                        .to(|| async { HttpResponse::Ok().finish() }),
                );
            }

            for method in listing.methods() {
                let additional_context = additional_context.clone();
                let additional_context_and_method = move || {
                    provide_context(method);
                    additional_context();
                };
                self = if is_static {
                    self.route(
                        path,
                        handle_static_route(
                            additional_context_and_method.clone(),
                            app_fn.clone(),
                            listing.regenerate.clone(),
                        ),
                    )
                } else {
                    self.route(
                            path,
                            match mode {
                                SsrMode::OutOfOrder => render_app_to_stream_with_context(
                                    additional_context_and_method.clone(),
                                    app_fn.clone(),
                                    method,
                                ),
                                SsrMode::PartiallyBlocked => render_app_to_stream_with_context_and_replace_blocks(
                                    additional_context_and_method.clone(),
                                    app_fn.clone(),
                                    method,
                                    true,
                                ),
                                SsrMode::InOrder => render_app_to_stream_in_order_with_context(
                                    additional_context_and_method.clone(),
                                    app_fn.clone(),
                                    method,
                                ),
                                SsrMode::Async => render_app_async_with_context(
                                    additional_context_and_method.clone(),
                                    app_fn.clone(),
                                    method,
                                ),
                                // `SsrMode` is `#[non_exhaustive]`; fall
                                // back to the out-of-order stream renderer
                                // for any future variant so we never panic
                                // at runtime if Leptos adds a new mode.
                                _ => {
                                    #[cfg(feature = "tracing")]
                                    tracing::warn!(
                                        "unknown SsrMode {:?}, falling back to OutOfOrder",
                                        mode
                                    );
                                    render_app_to_stream_with_context(
                                        additional_context_and_method.clone(),
                                        app_fn.clone(),
                                        method,
                                    )
                                }
                            },
                        )
                };
            }
        }

        self
    }
}

impl<Err> LeptosRoutes for &mut ServiceConfig<Err>
where
    Err: ErrorRenderer,
    Err::Container: From<StateExtractorError>,
{
    fn leptos_routes<IV>(
        self,
        paths: Vec<NtexRouteListing>,
        app_fn: impl Fn() -> IV + Clone + Send + 'static,
    ) -> Self
    where
        IV: IntoView + 'static,
    {
        self.leptos_routes_with_context(paths, || {}, app_fn)
    }

    fn leptos_routes_with_context<IV>(
        self,
        paths: Vec<NtexRouteListing>,
        additional_context: impl Fn() + 'static + Clone + Send,
        app_fn: impl Fn() -> IV + Clone + Send + 'static,
    ) -> Self
    where
        IV: IntoView + 'static,
    {
        let mut router = self;

        let excluded = paths
            .iter()
            .filter(|p| p.exclude)
            .map(|p| p.path.as_str())
            .collect::<HashSet<_>>();

        for (path, method) in server_fn_paths() {
            if !excluded.contains(path) {
                let additional_context = additional_context.clone();
                let handler = handle_server_fns_with_context(additional_context).method(method);
                router = router.route(path, handler);
            }
        }

        for listing in paths.iter().filter(|p| !p.exclude) {
            let path = listing.path();
            let mode = listing.mode();
            let is_static = matches!(mode, SsrMode::Static(_));

            // Single HEAD handler per path for non-Static routes.
            if !is_static {
                router = router.route(
                    path,
                    Route::<Err>::new()
                        .method(ntex::http::Method::HEAD)
                        .to(|| async { HttpResponse::Ok().finish() }),
                );
            }

            for method in listing.methods() {
                let additional_context = additional_context.clone();
                let additional_context_and_method = move || {
                    provide_context(method);
                    additional_context();
                };
                if is_static {
                    router = router.route(
                        path,
                        handle_static_route(
                            additional_context_and_method.clone(),
                            app_fn.clone(),
                            listing.regenerate.clone(),
                        ),
                    );
                } else {
                    router = router
                        .route(
                            path,
                            match mode {
                                SsrMode::OutOfOrder => render_app_to_stream_with_context(
                                    additional_context_and_method.clone(),
                                    app_fn.clone(),
                                    method,
                                ),
                                SsrMode::PartiallyBlocked => render_app_to_stream_with_context_and_replace_blocks(
                                    additional_context_and_method.clone(),
                                    app_fn.clone(),
                                    method,
                                    true,
                                ),
                                SsrMode::InOrder => render_app_to_stream_in_order_with_context(
                                    additional_context_and_method.clone(),
                                    app_fn.clone(),
                                    method,
                                ),
                                SsrMode::Async => render_app_async_with_context(
                                    additional_context_and_method.clone(),
                                    app_fn.clone(),
                                    method,
                                ),
                                // `SsrMode` is `#[non_exhaustive]`; fall
                                // back to the out-of-order stream renderer
                                // for any future variant so we never panic
                                // at runtime if Leptos adds a new mode.
                                _ => {
                                    #[cfg(feature = "tracing")]
                                    tracing::warn!(
                                        "unknown SsrMode {:?}, falling back to OutOfOrder",
                                        mode
                                    );
                                    render_app_to_stream_with_context(
                                        additional_context_and_method.clone(),
                                        app_fn.clone(),
                                        method,
                                    )
                                }
                            },
                        );
                }
            }
        }

        router
    }
}

/// Registers the Leptos route list on an ntex [`ServiceConfig`].
///
/// Convenience function that calls [`LeptosRoutes::leptos_routes`] on a
/// configuration object, for use from inside an [`App::configure`] closure.
pub fn register_leptos_routes<IV, Err>(
    cfg: &mut ServiceConfig<Err>,
    paths: Vec<NtexRouteListing>,
    app_fn: impl Fn() -> IV + Clone + Send + 'static,
) where
    Err: ErrorRenderer,
    Err::Container: From<StateExtractorError>,
    IV: IntoView + 'static,
{
    cfg.leptos_routes(paths, app_fn);
}

/// Decomposes an ntex request and its payload into a [`NtexRequest`] (which
/// owns the payload so the server-fn runtime can consume it) and a clone of
/// the request head for your own inspection (headers, method, URI, app
/// state, etc.).
///
/// Mirrors the `leptos_axum::generate_request_and_parts` helper. Because
/// ntex's [`HttpRequest`] already represents the head and is cheap to clone
/// (it is internally reference-counted), this is a simple convenience.
///
/// ```no_run
/// use ntex::http::Payload;
/// use ntex::web::HttpRequest;
/// use leptos_ntex::leptos_ntex::generate_request_and_parts;
///
/// fn example(req: HttpRequest, payload: Payload) {
///     let (server_fn_req, head) = generate_request_and_parts(req, payload);
///     let _ = head.headers().get("authorization");
///     // pass `server_fn_req` to the server-fn runtime
///     drop(server_fn_req);
/// }
/// ```
pub fn generate_request_and_parts(
    req: HttpRequest,
    payload: Payload,
) -> (NtexRequest, HttpRequest) {
    let head = req.clone();
    (NtexRequest::from((req, payload)), head)
}

async fn collect_payload(mut payload: Payload) -> Result<SfBytes, io::Error> {
    let mut buf = SfBytesMut::new();
    while let Some(chunk) = payload.recv().await {
        let chunk = chunk.map_err(io::Error::other)?;
        buf.extend_from_slice(&chunk);
    }
    Ok(buf.freeze())
}

/// Wraps an ntex request + payload pair for use as a server function input.
///
/// Implements [`server_fn::request::Req`] so the generic server-function
/// runtime can pull bytes, strings, streams, and websockets out of the
/// request. ntex's types are not [`Send`], so the pair is wrapped in
/// [`SendWrapper`].
pub struct NtexRequest(pub SendWrapper<(HttpRequest, Payload)>);

impl NtexRequest {
    /// Consumes the wrapper and returns the original ntex request/payload.
    pub fn take(self) -> (HttpRequest, Payload) {
        self.0.take()
    }

    fn header(&self, name: &str) -> Option<Cow<'_, str>> {
        self.0
            .0
            .headers()
            .get(name)
            .map(|h| String::from_utf8_lossy(h.as_bytes()))
    }
}

impl From<(HttpRequest, Payload)> for NtexRequest {
    fn from(value: (HttpRequest, Payload)) -> Self {
        Self(SendWrapper::new(value))
    }
}

impl<Error, InputStreamError, OutputStreamError> Req<Error, InputStreamError, OutputStreamError>
    for NtexRequest
where
    Error: FromServerFnError + Send,
    InputStreamError: FromServerFnError + Send,
    OutputStreamError: FromServerFnError + Send,
{
    type WebsocketResponse = NtexServerResponse;

    fn as_query(&self) -> Option<&str> {
        self.0.0.uri().query()
    }

    fn to_content_type(&self) -> Option<Cow<'_, str>> {
        self.header("Content-Type")
    }

    fn accepts(&self) -> Option<Cow<'_, str>> {
        self.header("Accept")
    }

    fn referer(&self) -> Option<Cow<'_, str>> {
        self.header("Referer")
    }

    fn try_into_bytes(self) -> impl Future<Output = Result<SfBytes, Error>> + Send {
        SendWrapper::new(async move {
            let payload = self.0.take().1;
            collect_payload(payload).await.map_err(|e| {
                server_fn::error::ServerFnErrorErr::Deserialization(e.to_string()).into_app_error()
            })
        })
    }

    fn try_into_string(self) -> impl Future<Output = Result<String, Error>> + Send {
        SendWrapper::new(async move {
            let payload = self.0.take().1;
            let bytes = collect_payload(payload).await.map_err(|e| {
                Error::from_server_fn_error(server_fn::error::ServerFnErrorErr::Deserialization(e.to_string()))
            })?;
            String::from_utf8(bytes.to_vec()).map_err(|e| {
                Error::from_server_fn_error(server_fn::error::ServerFnErrorErr::Deserialization(e.to_string()))
            })
        })
    }

    fn try_into_stream(self) -> Result<impl Stream<Item = Result<SfBytes, SfBytes>> + Send, Error> {
        let payload = self.0.take().1;
        let stream = futures::stream::unfold(payload, |mut payload| async move {
            payload.recv().await.map(|res| {
                let item = res
                    .map(|b| SfBytes::copy_from_slice(&b))
                    .map_err(|e| {
                        Error::from_server_fn_error(
                            server_fn::error::ServerFnErrorErr::Deserialization(e.to_string()),
                        )
                        .ser()
                    });
                (item, payload)
            })
        });
        Ok(SendWrapper::new(stream))
    }

    fn try_into_websocket(
        self,
    ) -> impl Future<
        Output = Result<
        (
            impl Stream<Item = Result<SfBytes, SfBytes>> + Send + 'static,
            impl Sink<SfBytes> + Send + 'static,
            Self::WebsocketResponse,
        ),
        Error,
        >,
    > + Send {
        SendWrapper::new(async move {
            let (request, _payload) = self.0.take();

            let (response_stream_tx, response_stream_rx) =
                mpsc::channel::<Result<SfBytes, SfBytes>>(2048);
            let (response_sink_tx, response_sink_rx) = mpsc::channel::<SfBytes>(2048);
            let response_sink_rx = Arc::new(Mutex::new(Some(response_sink_rx)));

            let response = web::ws::start::<_, _, &str, web::Error>(
                request,
                None,
                ntex::service::fn_factory_with_config(move |sink: web::ws::WsSink| {
                    let response_stream_tx = response_stream_tx.clone();
                    let response_sink_rx = response_sink_rx.clone();

                    async move {
                        let mut response_sink_rx = response_sink_rx
                            .lock()
                            .or_poisoned()
                            .take()
                            .expect("websocket response sink should only be initialized once");

                        let outbound_sink = sink.clone();
                        let mut outbound_errors = response_stream_tx.clone();
                        ntex::rt::spawn(async move {
                            while let Some(incoming) = response_sink_rx.next().await {
                                if let Err(err) = outbound_sink
                                    .send(web::ws::Message::Binary(NBytes::copy_from_slice(&incoming)))
                                    .await
                                {
                                    _ = outbound_errors.start_send(Err(
                                        InputStreamError::from_server_fn_error(
                                            server_fn::error::ServerFnErrorErr::Request(
                                                err.to_string(),
                                            ),
                                        )
                                        .ser(),
                                    ));
                                    break;
                                }
                            }
                            let _ = outbound_sink.send(web::ws::Message::Close(None)).await;
                        });

                        Ok::<_, web::Error>(ntex::service::fn_service(
                            move |frame: web::ws::Frame| {
                                let mut response_stream_tx = response_stream_tx.clone();
                                async move {
                                    match frame {
                                        web::ws::Frame::Ping(bytes) => {
                                            Ok::<Option<web::ws::Message>, web::Error>(Some(
                                                web::ws::Message::Pong(bytes),
                                            ))
                                        }
                                        web::ws::Frame::Binary(bytes) => {
                                            _ = response_stream_tx.start_send(Ok(
                                                SfBytes::copy_from_slice(&bytes),
                                            ));
                                            Ok::<Option<web::ws::Message>, web::Error>(None)
                                        }
                                        web::ws::Frame::Text(text) => {
                                            _ = response_stream_tx.start_send(Ok(
                                                SfBytes::copy_from_slice(&text),
                                            ));
                                            Ok::<Option<web::ws::Message>, web::Error>(None)
                                        }
                                        web::ws::Frame::Close(reason) => {
                                            Ok::<Option<web::ws::Message>, web::Error>(Some(
                                                web::ws::Message::Close(reason),
                                            ))
                                        }
                                        web::ws::Frame::Pong(_) | web::ws::Frame::Continuation(_) => {
                                            Ok::<Option<web::ws::Message>, web::Error>(None)
                                        }
                                    }
                                }
                            },
                        ))
                    }
                }),
            )
            .await
            .map_err(|e| {
                Error::from_server_fn_error(server_fn::error::ServerFnErrorErr::Request(
                    e.to_string(),
                ))
            })?;

            Ok((
                response_stream_rx,
                response_sink_tx,
                NtexServerResponse::from(response),
            ))
        })
    }
}

/// Wraps an ntex [`HttpResponse`] for use as a server function output.
///
/// Implements [`server_fn::response::TryRes`] and [`server_fn::response::Res`]
/// so the server-function runtime can build responses of the expected
/// content-type. ntex's [`HttpResponse`] is not [`Send`], hence the
/// [`SendWrapper`].
pub struct NtexServerResponse(pub SendWrapper<HttpResponse>);

impl NtexServerResponse {
    /// Consumes the wrapper and returns the inner ntex response.
    pub fn take(self) -> HttpResponse {
        self.0.take()
    }
}

impl From<HttpResponse> for NtexServerResponse {
    fn from(value: HttpResponse) -> Self {
        Self(SendWrapper::new(value))
    }
}

impl<E> TryRes<E> for NtexServerResponse
where
    E: FromServerFnError,
{
    fn try_from_string(content_type: &str, data: String) -> Result<Self, E> {
        Ok(Self(SendWrapper::new(
            HttpResponse::Ok().content_type(content_type).body(data),
        )))
    }

    fn try_from_bytes(content_type: &str, data: SfBytes) -> Result<Self, E> {
        Ok(Self(SendWrapper::new(
            HttpResponse::Ok()
                .content_type(content_type)
                .body(NBytes::copy_from_slice(&data)),
        )))
    }

    fn try_from_stream(
        content_type: &str,
        data: impl Stream<Item = Result<SfBytes, SfBytes>> + Send + 'static,
    ) -> Result<Self, E> {
        let pinned = Box::pin(data.map(|data| {
            data.map(|b| NBytes::copy_from_slice(&b)).map_err(|e| {
                io::Error::other(String::from_utf8_lossy(&e).to_string())
            })
        }));
        Ok(Self(SendWrapper::new(
            HttpResponse::Ok().content_type(content_type).streaming(pinned),
        )))
    }
}

impl Res for NtexServerResponse {
    fn error_response(path: &str, err: SfBytes) -> Self {
        Self(SendWrapper::new(
            HttpResponse::InternalServerError()
                .header(server_fn::error::SERVER_FN_ERROR_HEADER, path)
                .body(NBytes::copy_from_slice(&err)),
        ))
    }

    fn content_type(&mut self, content_type: &str) {
        if let Ok(content_type) = HeaderValue::from_str(content_type) {
            self.0.headers_mut().insert(header::CONTENT_TYPE, content_type);
        }
    }

    fn redirect(&mut self, path: &str) {
        if let Ok(path) = HeaderValue::from_str(path) {
            *self.0.status_mut() = StatusCode::FOUND;
            self.0.headers_mut().insert(header::LOCATION, path);
        }
    }
}

/// `any_spawner::CustomExecutor` impl that delegates task spawning to
/// `ntex::rt`.
///
/// This makes Leptos's reactive tasks run on whatever ntex runtime the app
/// was compiled against (tokio / compio / default neon), so Suspense work
/// and router navigations stay on the same arbiter as the request that
/// triggered them instead of bouncing onto a separate thread pool.
///
/// Installed automatically from [`generate_route_list`] and friends; must
/// be called from inside an ntex arbiter (i.e. from within `#[ntex::main]`
/// or `#[ntex::test]`).
pub struct NtexExecutor;

impl any_spawner::CustomExecutor for NtexExecutor {
    fn spawn(&self, fut: any_spawner::PinnedFuture<()>) {
        ntex::rt::spawn(fut);
    }

    fn spawn_local(&self, fut: any_spawner::PinnedLocalFuture<()>) {
        ntex::rt::spawn(fut);
    }

    fn poll_local(&self) {}
}

/// The server-function backend used by `#[server]` macros to target the
/// ntex integration.
///
/// Pass this as the `server = crate::leptos_ntex::NtexServerFnBackend`
/// argument on the `#[server]` attribute so that the server function is
/// dispatched through the ntex runtime.
pub struct NtexServerFnBackend;

impl<Error, InputStreamError, OutputStreamError> Server<Error, InputStreamError, OutputStreamError>
    for NtexServerFnBackend
where
    Error: FromServerFnError + Send + Sync,
    InputStreamError: FromServerFnError + Send + Sync,
    OutputStreamError: FromServerFnError + Send + Sync,
{
    type Request = NtexRequest;
    type Response = NtexServerResponse;

    fn spawn(future: impl Future<Output = ()> + Send + 'static) -> Result<(), Error> {
        ntex::rt::spawn(future);
        Ok(())
    }
}

type RegisteredServerFns =
    HashMap<&'static str, Vec<(HttpMethod, ServerFnTraitObj<NtexRequest, NtexServerResponse>)>>;

static REGISTERED_SERVER_FUNCTIONS: LazyServerFnMap<NtexRequest, NtexServerResponse> =
    LazyLock::new(|| {
        let mut map = RegisteredServerFns::new();
        for obj in server_fn::inventory::iter::<ServerFnTraitObj<NtexRequest, NtexServerResponse>>
            .into_iter()
        {
            map.entry(obj.path())
                .or_default()
                .push((obj.method(), obj.clone()));
        }
        RwLock::new(map)
    });

/// Explicitly registers a server function with this integration.
///
/// On native targets you normally do not need to call this — the
/// `#[server]` macro emits an `inventory::submit!` entry that
/// [`initialize_server_fn_map!`] picks up at startup, so every
/// `#[server(server = NtexServerFnBackend)]` function registers itself
/// automatically. Call this function only on platforms where `inventory`
/// does not work (wasm/edge runtimes like Cloudflare Workers or Deno
/// Deploy), or when you need to register a type defined outside the normal
/// macro flow.
pub fn register_explicit<T>()
where
    T: ServerFn<
            Server: server_fn::server::Server<
                T::Error,
                T::InputStreamError,
                T::OutputStreamError,
                Request = NtexRequest,
                Response = NtexServerResponse,
            >,
        > + 'static,
{
    REGISTERED_SERVER_FUNCTIONS
        .write()
        .or_poisoned()
        .entry(T::PATH)
        .or_default()
        .push((
            T::Protocol::METHOD,
            ServerFnTraitObj::new::<T>(|req| Box::pin(T::run_on_server(req))),
        ));
}

/// Returns an iterator over the `(path, method)` pairs of every server
/// function that has been registered with this integration.
pub fn server_fn_paths() -> impl Iterator<Item = (&'static str, HttpMethod)> {
    let paths: Vec<_> = REGISTERED_SERVER_FUNCTIONS
        .read()
        .or_poisoned()
        .iter()
        .flat_map(|(path, entries)| {
            entries.iter().map(move |(m, _)| (*path, m.clone()))
        })
        .collect();
    paths.into_iter()
}

/// Looks up the service for the server function registered at the given
/// path and method, applying any middlewares that were attached to it.
pub fn get_server_fn_service(
    path: &str,
    method: &HttpMethod,
) -> Option<BoxedService<NtexRequest, NtexServerResponse>> {
    let guard = REGISTERED_SERVER_FUNCTIONS.read().or_poisoned();
    let entries = guard.get(path)?;
    let server_fn = entries.iter().find(|(m, _)| m == method).map(|(_, f)| f)?;
    let middleware = server_fn.middleware();
    let mut service = server_fn.clone().boxed();
    for m in middleware {
        service = m.layer(service);
    }
    Some(service)
}

/// Returns an ntex [`Route`] that dispatches requests to the registered
/// server functions.
///
/// This can be mounted once at a wildcard path matching the API prefix:
///
/// ```no_run
/// use ntex::web::{self, App as NtexApp};
/// use leptos_ntex::leptos_ntex::handle_server_fns;
///
/// # fn main() -> std::io::Result<()> {
/// web::server(|| async {
///     NtexApp::new().route("/api/{tail:.*}", handle_server_fns())
/// })
/// .bind(("127.0.0.1", 3000))?
/// .run();
/// # Ok(())
/// # }
/// ```
///
/// ## Provided Context Types
/// - [`ResponseOptions`]
/// - [`Request`]
#[cfg_attr(
    feature = "tracing",
    tracing::instrument(level = "trace", fields(error), skip_all)
)]
pub fn handle_server_fns<Err>() -> Route<Err>
where
    Err: ErrorRenderer,
{
    handle_server_fns_with_context(|| {})
}

/// Variant of [`handle_server_fns`] that injects additional values into the
/// reactive context before dispatching the server function.
///
/// If your server functions expect some piece of context, make sure to
/// provide it both here and in
/// [`LeptosRoutes::leptos_routes_with_context`] (or whichever rendering
/// method you use). During SSR, server functions are called by the rendering
/// method, while subsequent calls from the client are handled by this server
/// function handler — both paths need to see the same context.
#[cfg_attr(
    feature = "tracing",
    tracing::instrument(level = "trace", fields(error), skip_all)
)]
pub fn handle_server_fns_with_context<Err>(
    additional_context: impl Fn() + 'static + Clone + Send,
) -> Route<Err>
where
    Err: ErrorRenderer,
{
    ensure_executor_initialized();
    Route::<Err>::new().to(move |req: HttpRequest, payload: web::types::Payload| {
        let additional_context = additional_context.clone();
        async move {
            if let Some(mut service) = get_server_fn_service(req.path(), req.method()) {
                let owner = Owner::new();
                owner
                    .with(|| {
                        ScopedFuture::new(async move {
                            provide_context(Request::new(&req));
                            let res_options = ResponseOptions::default();
                            provide_context(res_options.clone());
                            additional_context();

                            let accepts_html = req
                                .headers()
                                .get(header::ACCEPT)
                                .and_then(|v| v.to_str().ok())
                                .map(|v| v.contains("text/html"))
                                .unwrap_or(false);
                            let referrer = req.headers().get(header::REFERER).cloned();

                            let mut res = service
                                .run(NtexRequest::from((req, payload.into_inner())))
                                .await;

                            if accepts_html
                                && res.0.headers().get(header::LOCATION).is_none()
                                && let Some(referrer) = referrer
                            {
                                *res.0.status_mut() = StatusCode::FOUND;
                                res.0.headers_mut().insert(header::LOCATION, referrer);
                            }

                            {
                                let mut res_options = res_options.0.write().or_poisoned();
                                let headers = res.0.headers_mut();
                                if let Some(location) = res_options.headers.get(header::LOCATION).cloned() {
                                    headers.insert(header::LOCATION, location);
                                    res_options.headers.remove(header::LOCATION);
                                }
                            }

                            let mut wrapped = NtexResponse(res.take());
                            wrapped.extend_response(&res_options);
                            wrapped.take()
                        })
                    })
                    .await
            } else {
                HttpResponse::BadRequest().body(format!(
                    "Could not find a server function at the route {}. \
\n\nIt's likely that either\n1. The API prefix you specify in the `#[server]` macro doesn't match the prefix at which your server function handler is mounted, or\n2. You are on a platform that doesn't support automatic server function registration and you need to call register_explicit() on the server function type, somewhere in your `main` function.",
                    req.path()
                ))
            }
        }
    })
}

/// Returns a clone of the ntex [application state](ntex::web::App::state)
/// of type `T`.
///
/// Returns `None` in two distinct situations:
/// 1. The current reactive context has no [`Request`] — i.e. the function
///    is being called outside an SSR/server-fn handler (or before
///    [`handle_server_fns`] / any `render_app_*` installed the request
///    into context).
/// 2. The ntex application has no value of type `T` registered via
///    [`App::state`](ntex::web::App::state).
///
/// Both cases are indistinguishable in the return value; if you need to
/// tell them apart, inspect [`use_context`](leptos::context::use_context)
/// `::<Request>()` first.
///
/// Requires `T: Clone` because the underlying ntex state is stored behind
/// a reference and must be cloned out. Ideal for lightweight types like
/// `Arc<...>` configs.
///
/// ```no_run
/// use leptos::prelude::*;
/// use leptos_ntex::leptos_ntex::use_app_state;
///
/// #[derive(Clone)]
/// struct AppConfig { greeting: String }
///
/// fn greet() -> String {
///     use_app_state::<AppConfig>()
///         .map(|cfg| cfg.greeting)
///         .unwrap_or_else(|| "hi".into())
/// }
/// ```
pub fn use_app_state<T>() -> Option<T>
where
    T: Clone + 'static,
{
    use_context::<Request>().and_then(|req| req.app_state::<T>().cloned())
}

/// Like [`use_app_state`] but panics with an informative message if the
/// state is not present. Mirrors the `use_context` / `expect_context`
/// pairing that Leptos itself exposes.
///
/// Use this when you're sure the state has been wired via
/// [`App::state`](ntex::web::App::state) and would rather crash fast than
/// silently produce defaults if it's missing. For optional or
/// default-able state, use [`use_app_state`] instead.
#[track_caller]
pub fn expect_app_state<T>() -> T
where
    T: Clone + 'static,
{
    use_app_state::<T>().unwrap_or_else(|| {
        panic!(
            "expect_app_state::<{}>() called without matching .state() on App \
             (or outside an SSR/server-fn handler where `Request` is in context)",
            std::any::type_name::<T>()
        )
    })
}

/// Helper for using ntex's [`FromRequest`](ntex::web::FromRequest)
/// extractors inside a server function with the default error renderer.
///
/// Any error produced by the extractor is converted to a
/// [`ServerFnErrorErr`](server_fn::error::ServerFnErrorErr). Note that ntex
/// extractors that consume the request body will not work here — the body is
/// read by the server-function framework itself. This helper is only useful
/// for extractors that operate on the request head (path/query/headers/etc.).
///
/// The error renderer is fixed to [`ntex::web::DefaultError`]. Apps wired
/// with a custom error renderer should use [`extract_with_err`] and spell
/// the renderer type explicitly, or call `T::from_request(&req, payload)`
/// directly after pulling `req` out of context.
pub async fn extract<T>() -> Result<T, server_fn::error::ServerFnErrorErr>
where
    T: ntex::web::FromRequest<ntex::web::DefaultError>,
    T::Error: Display,
{
    extract_with_err::<T, ntex::web::DefaultError>().await
}

/// Like [`extract`] but parameterised over the ntex error renderer.
///
/// Use this when your ntex app uses a non-default error renderer. The
/// renderer must be spelled explicitly because Rust does not support
/// default type parameters on free functions.
///
/// ```no_run
/// use leptos::prelude::*;
/// use leptos_ntex::leptos_ntex::extract_with_err;
///
/// # async fn example() -> Result<(), server_fn::ServerFnError> {
/// let req: ntex::web::HttpRequest =
///     extract_with_err::<_, ntex::web::DefaultError>()
///         .await
///         .map_err(|e| server_fn::ServerFnError::new(e.to_string()))?;
/// let _ = req.path();
/// # Ok(())
/// # }
/// ```
pub async fn extract_with_err<T, Err>() -> Result<T, server_fn::error::ServerFnErrorErr>
where
    T: ntex::web::FromRequest<Err>,
    Err: ErrorRenderer,
    <T as ntex::web::FromRequest<Err>>::Error: Display,
{
    let req = use_context::<Request>().ok_or_else(|| {
        server_fn::error::ServerFnErrorErr::ServerError(
            "HttpRequest should have been provided via context".to_string(),
        )
    })?;

    SendWrapper::new(async move {
        let mut payload = Payload::None;
        T::from_request(&req, &mut payload)
            .await
            .map_err(|e| server_fn::error::ServerFnErrorErr::ServerError(e.to_string()))
    })
    .await
}
