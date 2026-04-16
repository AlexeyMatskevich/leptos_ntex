#![forbid(unsafe_code)]
#![deny(missing_docs)]
#![doc = include_str!("../README.md")]

use bytes::{Bytes as SfBytes, BytesMut as SfBytesMut};
use futures::{Sink, SinkExt, Stream, StreamExt, channel::mpsc, stream::once};
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
    middleware::{BoxedService, Layer},
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
    path::{Component, Path, PathBuf},
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
                .content_type("text/html; charset=utf-8")
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
    let handler = move |req: HttpRequest| {
        let add_context = additional_context.clone();
        let app_fn = app_fn.clone();
        handle_response_inner(add_context, app_fn, req, stream_builder)
    };
    let route = Route::<Err>::new();
    // RFC 9110 §9.3.2: HEAD == GET without body. ntex's h1 encoder
    // unconditionally strips the body when the request method is HEAD
    // (ntex-3.7.2/src/http/h1/encoder.rs:292-293 forces
    // `TransferEncoding::empty()`), so binding the same handler to
    // both methods produces the correct status and headers with no
    // body on the wire. `Route::method()` pushes onto a Vec that
    // `take_guards()` turns into AND-combined guards on the owning
    // Resource (see ntex/web/route.rs:35-43), which makes it unusable
    // for multi-method matching — use `Any(..).or(..)` instead.
    let route = if matches!(method, Method::Get) {
        route.guard(
            ntex::web::guard::Any(ntex::web::guard::Get())
                .or(ntex::web::guard::Head()),
        )
    } else {
        route.method(ntex_method(method))
    };
    route.to(handler)
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
    // time even though the set would silently fail.
    static INIT: std::sync::Once = std::sync::Once::new();
    INIT.call_once(|| {
        if let Err(err) = any_spawner::Executor::init_custom_executor(NtexExecutor) {
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
/// of [`Request`] may leak or panic — see the `Request` documentation.
///
/// Safe to call repeatedly — the return value is deterministic — but
/// not free: `any_spawner::Executor::init_custom_executor` allocates a
/// boxed executor on every call before attempting its internal
/// `OnceLock::set` (see `any_spawner` 0.3). Once this crate has
/// auto-installed the executor, every subsequent user call allocates,
/// fails with `AlreadySet`, and drops the allocation. Call once at
/// startup — don't put it on a per-request path.
pub fn try_init_executor() -> Result<(), any_spawner::ExecutorError> {
    any_spawner::Executor::init_custom_executor(NtexExecutor)
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
            if !excluded.contains(path)
                && let Some(server_fn) = lookup_server_fn(path, &method)
            {
                let additional_context = additional_context.clone();
                self = self.route(
                    path,
                    handle_specific_server_fn_with_context(server_fn, additional_context),
                );
            }
        }

        for listing in paths.iter().filter(|p| !p.exclude) {
            let path = listing.path();
            let mode = listing.mode();
            let is_static = matches!(mode, SsrMode::Static(_));

            // HEAD is routed through the GET handlers (`handle_static_route`
            // and `handle_response`), both of which register themselves for
            // GET + HEAD. ntex's h1 writer then strips the body at the
            // wire, giving RFC 9110 §9.3.2-compliant headers/status with
            // no body. Listings that only register POST/PUT/etc. will
            // correctly 405 on HEAD instead of returning a bogus 200.

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
            if !excluded.contains(path)
                && let Some(server_fn) = lookup_server_fn(path, &method)
            {
                let additional_context = additional_context.clone();
                router = router.route(
                    path,
                    handle_specific_server_fn_with_context(server_fn, additional_context),
                );
            }
        }

        for listing in paths.iter().filter(|p| !p.exclude) {
            let path = listing.path();
            let mode = listing.mode();
            let is_static = matches!(mode, SsrMode::Static(_));

            // HEAD is handled by the GET route registrations below (see
            // the `App` impl for details).

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
/// use leptos_ntex_unofficial::generate_request_and_parts;
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

/// Default maximum payload size accepted by [`NtexRequest`] when collecting
/// server-function request bodies (2 MiB).
///
/// Matches ntex's own default [`PayloadConfig`](ntex::web::types::PayloadConfig)
/// limit. Override per-app by registering a [`LeptosServerFnConfig`] via
/// [`App::state`](ntex::web::App::state).
pub const DEFAULT_PAYLOAD_LIMIT: usize = 2 * 1024 * 1024;

/// Default channel buffer size for the incoming / outgoing WebSocket
/// mpsc channels used by server-function websockets (2048 messages).
///
/// Override per-app via [`LeptosServerFnConfig::ws_channel_buffer`].
pub const DEFAULT_WS_CHANNEL_BUFFER: usize = 2048;

/// Tunables for the server-function dispatcher.
///
/// Register via [`App::state`](ntex::web::App::state) to override the
/// built-in defaults per application. Missing configuration falls back to
/// [`DEFAULT_PAYLOAD_LIMIT`] and [`DEFAULT_WS_CHANNEL_BUFFER`].
///
/// ```no_run
/// use ntex::web::App as NtexApp;
/// use leptos_ntex_unofficial::{handle_server_fns, LeptosServerFnConfig};
///
/// # fn example() {
/// let _app = NtexApp::new()
///     .state(LeptosServerFnConfig {
///         payload_limit: 8 * 1024 * 1024, // 8 MiB
///         ws_channel_buffer: 512,
///         ws_subprotocol: Some("graphql-ws"),
///     })
///     .route("/api/{tail:.*}", handle_server_fns());
/// # }
/// ```
#[derive(Debug, Clone, Copy)]
pub struct LeptosServerFnConfig {
    /// Maximum accepted payload body size in bytes for non-streaming
    /// server-function requests. Requests exceeding this limit are
    /// rejected with `413 Payload Too Large`, whether the client
    /// declares size up-front via `Content-Length` (rejected before
    /// the server function runs) or streams a body whose length is
    /// unknown to the server (`Transfer-Encoding: chunked`, any body
    /// without `Content-Length`) and exceeds the limit mid-flight
    /// (rejected once the excess byte is observed; the partial
    /// payload is discarded).
    pub payload_limit: usize,
    /// Buffer size for the WebSocket mpsc channels used by streaming
    /// server functions. Larger values allow bursts at the cost of
    /// memory; smaller values apply stronger backpressure upstream.
    pub ws_channel_buffer: usize,
    /// Subprotocol echoed in the `Sec-WebSocket-Protocol` response
    /// header during WebSocket upgrade. `None` negotiates no
    /// subprotocol (the default; matches bare `ws://` clients).
    ///
    /// For dynamic per-request selection — e.g. picking the first
    /// subprotocol the client advertises that the server supports —
    /// read [`ntex::web::ws::subprotocols`] inside a custom WebSocket
    /// handler instead of going through [`handle_server_fns`].
    pub ws_subprotocol: Option<&'static str>,
}

impl Default for LeptosServerFnConfig {
    fn default() -> Self {
        Self {
            payload_limit: DEFAULT_PAYLOAD_LIMIT,
            ws_channel_buffer: DEFAULT_WS_CHANNEL_BUFFER,
            ws_subprotocol: None,
        }
    }
}

fn server_fn_config(req: &HttpRequest) -> LeptosServerFnConfig {
    req.app_state::<LeptosServerFnConfig>().copied().unwrap_or_default()
}

/// Request-scoped sentinel used to promote a `server_fn` oversize-payload
/// error into a real HTTP 413 response in the outer ntex handler.
///
/// `server_fn` 0.8 has no dedicated `RequestTooLarge` error variant, so we
/// cannot influence the HTTP status from inside `try_into_bytes` /
/// `try_into_string` / `try_into_stream`. Instead those methods insert
/// this marker via `HttpRequest::extensions_mut()` before returning the
/// generic `Args` error; the public handler checks the extension after
/// the server-fn pipeline completes and rewrites the response with the
/// correct status. Private — the marker is an internal implementation
/// detail.
#[derive(Copy, Clone)]
struct PayloadTooLarge;

/// Returns true when the request declares `Content-Length` and it exceeds
/// `limit`. The preflight avoids reading the body at all when the client
/// tells us up-front that it is oversize.
fn content_length_exceeds(req: &HttpRequest, limit: usize) -> bool {
    req.headers()
        .get(header::CONTENT_LENGTH)
        .and_then(|v| v.to_str().ok())
        .and_then(|s| s.parse::<usize>().ok())
        .is_some_and(|declared| declared > limit)
}

/// Collects the full request body into memory, enforcing `limit` against
/// the cumulative chunk size. On overflow, stashes a [`PayloadTooLarge`]
/// marker in `req.extensions_mut()` so the outer handler can translate the
/// `server_fn` error into `413 Payload Too Large`.
async fn collect_payload(
    req: &HttpRequest,
    mut payload: Payload,
    limit: usize,
) -> Result<SfBytes, io::Error> {
    let mut buf = SfBytesMut::new();
    while let Some(chunk) = payload.recv().await {
        let chunk = chunk.map_err(io::Error::other)?;
        if buf.len().saturating_add(chunk.len()) > limit {
            req.extensions_mut().insert(PayloadTooLarge);
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!("payload exceeds limit of {limit} bytes"),
            ));
        }
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
            let (req, payload) = self.0.take();
            let limit = server_fn_config(&req).payload_limit;
            collect_payload(&req, payload, limit).await.map_err(|e| {
                // `Args` maps semantically to "error reading arguments
                // from the request", closer to payload-overflow than
                // `Deserialization` (which is defined as a client-side
                // result-parsing error). The outer ntex handler
                // translates the extension marker into 413.
                server_fn::error::ServerFnErrorErr::Args(e.to_string()).into_app_error()
            })
        })
    }

    fn try_into_string(self) -> impl Future<Output = Result<String, Error>> + Send {
        SendWrapper::new(async move {
            let (req, payload) = self.0.take();
            let limit = server_fn_config(&req).payload_limit;
            let bytes = collect_payload(&req, payload, limit).await.map_err(|e| {
                Error::from_server_fn_error(
                    server_fn::error::ServerFnErrorErr::Args(e.to_string()),
                )
            })?;
            String::from_utf8(bytes.to_vec()).map_err(|e| {
                Error::from_server_fn_error(
                    server_fn::error::ServerFnErrorErr::Args(e.to_string()),
                )
            })
        })
    }

    fn try_into_stream(self) -> Result<impl Stream<Item = Result<SfBytes, SfBytes>> + Send, Error> {
        let (req, payload) = self.0.take();
        let limit = server_fn_config(&req).payload_limit;
        // State is `Option<..>`: `None` terminates the stream on the next
        // poll, so a single error frame (limit exceeded or payload error)
        // is emitted and then the stream closes. On overflow we also
        // stash the `PayloadTooLarge` marker on `req.extensions_mut()`
        // so the outer ntex handler can translate the stream's error
        // frame into a 413 response body.
        let stream = futures::stream::unfold(
            Some((req, payload, 0usize, limit)),
            |state| async move {
                let (req, mut payload, so_far, limit) = state?;
                let item = payload.recv().await?;
                match item {
                    Ok(b) => {
                        let next = so_far.saturating_add(b.len());
                        if next > limit {
                            req.extensions_mut().insert(PayloadTooLarge);
                            let err = Error::from_server_fn_error(
                                server_fn::error::ServerFnErrorErr::Args(format!(
                                    "payload exceeds limit of {limit} bytes"
                                )),
                            )
                            .ser();
                            Some((Err(err), None))
                        } else {
                            Some((
                                Ok(SfBytes::copy_from_slice(&b)),
                                Some((req, payload, next, limit)),
                            ))
                        }
                    }
                    Err(e) => {
                        let err = Error::from_server_fn_error(
                            server_fn::error::ServerFnErrorErr::Args(e.to_string()),
                        )
                        .ser();
                        Some((Err(err), None))
                    }
                }
            },
        );
        Ok(SendWrapper::new(stream))
    }

    /// Upgrades the request to a WebSocket connection and returns
    /// `(incoming_stream, outgoing_sink, response)` for the server-fn
    /// runtime.
    ///
    /// The incoming and outgoing mpsc channel capacities default to
    /// [`DEFAULT_WS_CHANNEL_BUFFER`] (2048 messages). Override per-app by
    /// registering a [`LeptosServerFnConfig`] with
    /// [`App::state`](ntex::web::App::state).
    ///
    /// ## Backpressure
    ///
    /// Both channels are bounded (`futures::channel::mpsc`). Producers
    /// (this bridge writing incoming WS frames into the server-fn
    /// receiver; the server-fn writing outgoing frames into the ntex
    /// sink) call `Sink::send().await`, which suspends the task until
    /// the channel has capacity again. A slow consumer therefore stalls
    /// the frame-reader task, which is exactly the backpressure behavior
    /// expected. Smaller buffers apply backpressure sooner; larger
    /// buffers absorb bursts at the cost of memory
    /// (`O(N_connections * buffer * msg_size)` worst case).
    ///
    /// ## Fragmented messages (RFC 6455 §5.4)
    ///
    /// Fragmented WebSocket messages — delivered by ntex as
    /// `Frame::Continuation(Item::{FirstText, FirstBinary, Continue,
    /// Last})` — are reassembled per-connection into a single payload
    /// before being handed to the server-fn. Browser ws-clients and
    /// many library clients fragment large messages automatically, so
    /// dropping continuation frames (as an earlier revision did) would
    /// silently lose data.
    ///
    /// ## Policy-violation close
    ///
    /// When a reassembled fragmented message exceeds
    /// `LeptosServerFnConfig::payload_limit`, this bridge closes the
    /// connection with [`CloseCode::Size`](ntex::ws::CloseCode::Size)
    /// (1009, "Message Too Big" per RFC 6455 §7.4.1) and delivers an
    /// `InputStreamError` to the server-fn receiver. The client gets
    /// a structured reason rather than an abrupt disconnect.
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
        use std::{cell::RefCell, rc::Rc};
        use ntex::ws::{CloseCode, CloseReason};

        #[derive(Copy, Clone)]
        enum FragmentKind {
            Text,
            Binary,
        }

        SendWrapper::new(async move {
            let (request, _payload) = self.0.take();

            let config = server_fn_config(&request);
            let payload_limit = config.payload_limit;
            let (response_stream_tx, response_stream_rx) =
                mpsc::channel::<Result<SfBytes, SfBytes>>(config.ws_channel_buffer);
            let (response_sink_tx, response_sink_rx) =
                mpsc::channel::<SfBytes>(config.ws_channel_buffer);
            let response_sink_rx = Arc::new(Mutex::new(Some(response_sink_rx)));

            let response = web::ws::start::<_, _, &str, web::Error>(
                request,
                config.ws_subprotocol,
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
                                    // Surface the error to the
                                    // server-fn receiver with full
                                    // backpressure semantics; if the
                                    // receiver is also gone there is
                                    // nothing to do.
                                    let _ = outbound_errors
                                        .send(Err(InputStreamError::from_server_fn_error(
                                            server_fn::error::ServerFnErrorErr::Request(
                                                err.to_string(),
                                            ),
                                        )
                                        .ser()))
                                        .await;
                                    let _ = outbound_sink
                                        .send(web::ws::Message::Close(Some(CloseReason {
                                            code: CloseCode::Abnormal,
                                            description: Some(err.to_string()),
                                        })))
                                        .await;
                                    return;
                                }
                            }
                            let _ = outbound_sink.send(web::ws::Message::Close(None)).await;
                        });

                        // Per-connection reassembly buffer for
                        // `Frame::Continuation`. `Rc<RefCell<_>>`
                        // because `fn_factory_with_config` returns a
                        // `!Send` future — one per connection.
                        let fragment: Rc<RefCell<Option<(FragmentKind, SfBytesMut)>>> =
                            Rc::new(RefCell::new(None));

                        Ok::<_, web::Error>(ntex::service::fn_service({
                            let response_stream_tx = response_stream_tx.clone();
                            let fragment = fragment.clone();
                            move |frame: web::ws::Frame| {
                                let mut tx = response_stream_tx.clone();
                                let fragment = fragment.clone();
                                async move {
                                    use web::ws::{Frame, Message};
                                    use ntex::ws::Item;
                                    match frame {
                                        Frame::Ping(bytes) => {
                                            Ok::<Option<Message>, web::Error>(Some(
                                                Message::Pong(bytes),
                                            ))
                                        }
                                        Frame::Pong(_) => Ok(None),
                                        Frame::Close(reason) => {
                                            fragment.borrow_mut().take();
                                            Ok(Some(Message::Close(reason)))
                                        }
                                        Frame::Binary(bytes) => {
                                            // Unfragmented binary; bypass
                                            // the reassembly buffer and
                                            // enforce the limit directly.
                                            if bytes.len() > payload_limit {
                                                let _ = tx
                                                    .send(Err(InputStreamError::from_server_fn_error(
                                                        server_fn::error::ServerFnErrorErr::Args(format!(
                                                            "websocket payload exceeded limit of {payload_limit} bytes (observed {})",
                                                            bytes.len()
                                                        )),
                                                    )
                                                    .ser()))
                                                    .await;
                                                return Ok(Some(close_too_big(payload_limit)));
                                            }
                                            // `send().await` applies
                                            // backpressure when the
                                            // consumer is slow. If the
                                            // receiver is gone, the WS
                                            // is about to close anyway.
                                            let _ = tx
                                                .send(Ok(SfBytes::copy_from_slice(&bytes)))
                                                .await;
                                            Ok(None)
                                        }
                                        Frame::Text(text) => {
                                            if text.len() > payload_limit {
                                                let _ = tx
                                                    .send(Err(InputStreamError::from_server_fn_error(
                                                        server_fn::error::ServerFnErrorErr::Args(format!(
                                                            "websocket payload exceeded limit of {payload_limit} bytes (observed {})",
                                                            text.len()
                                                        )),
                                                    )
                                                    .ser()))
                                                    .await;
                                                return Ok(Some(close_too_big(payload_limit)));
                                            }
                                            let _ = tx
                                                .send(Ok(SfBytes::copy_from_slice(&text)))
                                                .await;
                                            Ok(None)
                                        }
                                        Frame::Continuation(item) => {
                                            // What to do with a First*
                                            // fragment after state + size
                                            // checks. Computed inside one
                                            // borrow window so no `RefMut`
                                            // is held across `.await`.
                                            enum FirstAction {
                                                Installed,
                                                ProtocolViolation,
                                                Overflow(usize),
                                            }
                                            // For Continue: whether to
                                            // extend the buffer, reject as
                                            // protocol error, or reject
                                            // on overflow.
                                            enum ContinueAction {
                                                Extended,
                                                NoOpener,
                                                Overflow(usize),
                                            }
                                            // For Last: take the buffer
                                            // (if any), or reject.
                                            enum LastAction {
                                                Complete(SfBytesMut),
                                                NoOpener,
                                                Overflow(usize),
                                            }
                                            // Inline overflow-emit block:
                                            // builds the `InputStreamError`
                                            // (generic captured by the
                                            // enclosing impl), sends it
                                            // through the receiver with
                                            // backpressure, then returns
                                            // the policy-close message.
                                            macro_rules! overflow_close {
                                                ($total:expr) => {{
                                                    let total = $total;
                                                    let _ = tx
                                                        .send(Err(InputStreamError::from_server_fn_error(
                                                            server_fn::error::ServerFnErrorErr::Args(format!(
                                                                "websocket payload exceeded limit of {payload_limit} bytes (observed {total})"
                                                            )),
                                                        )
                                                        .ser()))
                                                        .await;
                                                    Ok(Some(close_too_big(payload_limit)))
                                                }};
                                            }
                                            let protocol_close = |msg: &'static str| {
                                                Ok::<Option<Message>, web::Error>(Some(
                                                    Message::Close(Some(CloseReason {
                                                        code: CloseCode::Protocol,
                                                        description: Some(msg.into()),
                                                    })),
                                                ))
                                            };

                                            match item {
                                                Item::FirstText(b) => {
                                                    let action = {
                                                        let mut guard = fragment.borrow_mut();
                                                        if guard.is_some() {
                                                            *guard = None;
                                                            FirstAction::ProtocolViolation
                                                        } else if b.len() > payload_limit {
                                                            FirstAction::Overflow(b.len())
                                                        } else {
                                                            let mut buf = SfBytesMut::new();
                                                            buf.extend_from_slice(&b);
                                                            *guard = Some((FragmentKind::Text, buf));
                                                            FirstAction::Installed
                                                        }
                                                    };
                                                    match action {
                                                        FirstAction::Installed => Ok(None),
                                                        FirstAction::ProtocolViolation => protocol_close(
                                                            "new fragmented message started before previous one terminated",
                                                        ),
                                                        FirstAction::Overflow(t) => overflow_close!(t),
                                                    }
                                                }
                                                Item::FirstBinary(b) => {
                                                    let action = {
                                                        let mut guard = fragment.borrow_mut();
                                                        if guard.is_some() {
                                                            *guard = None;
                                                            FirstAction::ProtocolViolation
                                                        } else if b.len() > payload_limit {
                                                            FirstAction::Overflow(b.len())
                                                        } else {
                                                            let mut buf = SfBytesMut::new();
                                                            buf.extend_from_slice(&b);
                                                            *guard = Some((FragmentKind::Binary, buf));
                                                            FirstAction::Installed
                                                        }
                                                    };
                                                    match action {
                                                        FirstAction::Installed => Ok(None),
                                                        FirstAction::ProtocolViolation => protocol_close(
                                                            "new fragmented message started before previous one terminated",
                                                        ),
                                                        FirstAction::Overflow(t) => overflow_close!(t),
                                                    }
                                                }
                                                Item::Continue(b) => {
                                                    let action = {
                                                        let mut guard = fragment.borrow_mut();
                                                        match guard.as_mut() {
                                                            None => ContinueAction::NoOpener,
                                                            Some((_, buf)) => {
                                                                if buf
                                                                    .len()
                                                                    .saturating_add(b.len())
                                                                    > payload_limit
                                                                {
                                                                    let total =
                                                                        buf.len() + b.len();
                                                                    *guard = None;
                                                                    ContinueAction::Overflow(total)
                                                                } else {
                                                                    buf.extend_from_slice(&b);
                                                                    ContinueAction::Extended
                                                                }
                                                            }
                                                        }
                                                    };
                                                    match action {
                                                        ContinueAction::Extended => Ok(None),
                                                        ContinueAction::NoOpener => protocol_close(
                                                            "unexpected continuation frame",
                                                        ),
                                                        ContinueAction::Overflow(t) => overflow_close!(t),
                                                    }
                                                }
                                                Item::Last(b) => {
                                                    let action = {
                                                        let taken = fragment.borrow_mut().take();
                                                        match taken {
                                                            None => LastAction::NoOpener,
                                                            Some((_, mut buf)) => {
                                                                if buf
                                                                    .len()
                                                                    .saturating_add(b.len())
                                                                    > payload_limit
                                                                {
                                                                    LastAction::Overflow(
                                                                        buf.len() + b.len(),
                                                                    )
                                                                } else {
                                                                    buf.extend_from_slice(&b);
                                                                    LastAction::Complete(buf)
                                                                }
                                                            }
                                                        }
                                                    };
                                                    match action {
                                                        LastAction::Complete(buf) => {
                                                            let _ = tx.send(Ok(buf.freeze())).await;
                                                            Ok(None)
                                                        }
                                                        LastAction::NoOpener => protocol_close(
                                                            "unexpected terminal continuation frame",
                                                        ),
                                                        LastAction::Overflow(t) => overflow_close!(t),
                                                    }
                                                }
                                            }
                                        }
                                    }
                                }
                            }
                        }))
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

/// Builds a policy-violation `Close` message carrying `CloseCode::Size`
/// (1009, "Message Too Big" per RFC 6455 §7.4.1) with a human-readable
/// description. The corresponding `InputStreamError` is emitted at the
/// call site because its generic type is only in scope inside
/// `impl Req for NtexRequest`.
fn close_too_big(limit: usize) -> web::ws::Message {
    web::ws::Message::Close(Some(ntex::ws::CloseReason {
        code: ntex::ws::CloseCode::Size,
        description: Some(format!("message exceeds limit of {limit} bytes")),
    }))
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
/// Pass this as the `server = leptos_ntex_unofficial::NtexServerFnBackend`
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

static REGISTERED_SERVER_FUNCTIONS: LazyServerFnMap<NtexRequest, NtexServerResponse> =
    LazyLock::new(|| {
        let mut map = HashMap::new();
        for obj in server_fn::inventory::iter::<ServerFnTraitObj<NtexRequest, NtexServerResponse>>
            .into_iter()
        {
            map.entry(obj.path())
                .or_insert_with(Vec::new)
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

fn lookup_server_fn(
    path: &str,
    method: &HttpMethod,
) -> Option<ServerFnTraitObj<NtexRequest, NtexServerResponse>> {
    let guard = REGISTERED_SERVER_FUNCTIONS.read().or_poisoned();
    let entries = guard.get(path)?;
    entries
        .iter()
        .find(|(m, _)| m == method)
        .map(|(_, f)| f.clone())
}

/// Looks up the service for the server function registered at the given
/// path and method, applying any middlewares that were attached to it.
///
/// Intended for the catchall [`handle_server_fns`] dispatcher and for
/// advanced compositions. When server functions are mounted through
/// [`LeptosRoutes::leptos_routes`] / [`register_leptos_routes`] the
/// lookup is avoided — each path gets its own handler closing over the
/// pre-resolved [`ServerFnTraitObj`].
pub fn get_server_fn_service(
    path: &str,
    method: &HttpMethod,
) -> Option<BoxedService<NtexRequest, NtexServerResponse>> {
    let server_fn = lookup_server_fn(path, method)?;
    let middleware = server_fn.middleware();
    let mut service = server_fn.boxed();
    for m in middleware {
        service = m.layer(service);
    }
    Some(service)
}

/// Runs a prepared [`BoxedService`] for a server function, setting up the
/// reactive Owner, provided contexts, referrer-based redirect fixup, and
/// `ResponseOptions` extension. Shared between the catchall
/// [`handle_server_fns_with_context`] handler and the per-path
/// [`handle_specific_server_fn_with_context`] handler.
async fn dispatch_server_fn(
    mut service: BoxedService<NtexRequest, NtexServerResponse>,
    req: HttpRequest,
    payload: Payload,
    additional_context: impl FnOnce() + 'static + Send,
) -> HttpResponse {
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

                let mut res = service.run(NtexRequest::from((req, payload))).await;

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
                    // `HeaderMap::get` returns only the first value; use
                    // `get_all` so multi-valued `Location` (rare but
                    // legal via `append_header`) isn't silently dropped.
                    let mut locations =
                        res_options.headers.get_all(header::LOCATION).cloned();
                    if let Some(first) = locations.next() {
                        headers.insert(header::LOCATION, first);
                        for v in locations {
                            headers.append(header::LOCATION, v);
                        }
                        res_options.headers.remove(header::LOCATION);
                    }
                }

                let mut wrapped = NtexResponse(res.take());
                wrapped.extend_response(&res_options);
                wrapped.take()
            })
        })
        .await
}

/// Returns an ntex [`Route`] bound to a single, pre-resolved server
/// function — skipping the per-request `HashMap` lookup that
/// [`handle_server_fns_with_context`] performs. Used by
/// [`LeptosRoutes::leptos_routes_with_context`] at registration time so
/// every server-fn endpoint closes over its own [`ServerFnTraitObj`] and
/// middleware list.
///
/// The middleware factory is invoked once at registration and the result
/// is cached behind an [`Arc`] — cheap to clone per request (one atomic
/// increment), no per-request `Vec` allocation for the layer list.
fn handle_specific_server_fn_with_context<Err>(
    server_fn: ServerFnTraitObj<NtexRequest, NtexServerResponse>,
    additional_context: impl Fn() + 'static + Clone + Send,
) -> Route<Err>
where
    Err: ErrorRenderer,
{
    ensure_executor_initialized();
    let method = server_fn.method();
    let middleware: Arc<[Arc<dyn Layer<NtexRequest, NtexServerResponse>>]> =
        server_fn.middleware().into();
    let server_fn = Arc::new(server_fn);

    Route::<Err>::new().method(method).to(
        move |req: HttpRequest, payload: web::types::Payload| {
            let server_fn = server_fn.clone();
            let middleware = middleware.clone();
            let additional_context = additional_context.clone();
            async move {
                let limit = server_fn_config(&req).payload_limit;
                // Preflight: reject oversize bodies declared up-front
                // via Content-Length without reading any of the body.
                if content_length_exceeds(&req, limit) {
                    return oversize_response(limit);
                }
                let mut service = (*server_fn).clone().boxed();
                for m in middleware.iter() {
                    service = m.layer(service);
                }
                let resp = dispatch_server_fn(
                    service,
                    req.clone(),
                    payload.into_inner(),
                    additional_context,
                )
                .await;
                // Promote streaming/chunked overflow (detected by
                // `collect_payload` or the `try_into_stream` adapter
                // through the request-scoped marker) into a real 413.
                if req.extensions().get::<PayloadTooLarge>().is_some() {
                    return oversize_response(limit);
                }
                resp
            }
        },
    )
}

/// Builds a canonical `413 Payload Too Large` response with a human-
/// readable body that states the configured limit.
fn oversize_response(limit: usize) -> HttpResponse {
    HttpResponse::PayloadTooLarge()
        .content_type("text/plain; charset=utf-8")
        .body(format!("payload exceeds limit of {limit} bytes"))
}

/// Returns an ntex [`Route`] that dispatches requests to the registered
/// server functions.
///
/// This can be mounted once at a wildcard path matching the API prefix:
///
/// ```no_run
/// use ntex::web::{self, App as NtexApp};
/// use leptos_ntex_unofficial::handle_server_fns;
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
            let limit = server_fn_config(&req).payload_limit;
            if content_length_exceeds(&req, limit) {
                return oversize_response(limit);
            }
            if let Some(service) = get_server_fn_service(req.path(), req.method()) {
                let resp = dispatch_server_fn(
                    service,
                    req.clone(),
                    payload.into_inner(),
                    additional_context,
                )
                .await;
                if req.extensions().get::<PayloadTooLarge>().is_some() {
                    return oversize_response(limit);
                }
                resp
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
/// use leptos_ntex_unofficial::use_app_state;
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
/// use leptos_ntex_unofficial::extract_with_err;
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

// ---------------------------------------------------------------------------
// In-crate tests. Kept here (rather than in tests/integration.rs) because
// they touch crate-private helpers — `handle_response_inner`, the
// `ensure_executor_initialized` regression probe, direct access to the
// registration table — that are not exported from the public API.
// ---------------------------------------------------------------------------

#[cfg(test)]
use leptos::prelude::*;
#[cfg(test)]
use leptos_meta::{MetaTags, provide_meta_context};
#[cfg(test)]
use leptos_router::{
    components::{Route, Router, Routes},
    path,
    static_routes::StaticRoute,
};

#[cfg(test)]
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

#[cfg(test)]
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

#[cfg(test)]
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

#[cfg(test)]
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

#[cfg(test)]
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

#[cfg(test)]
#[server(
    name = EchoName,
    prefix = "/api",
    endpoint = "echo_name",
    server = crate::NtexServerFnBackend
)]
async fn echo_name(name: String) -> Result<String, ServerFnError> {
    Ok(format!("Hello, {name}"))
}

#[cfg(test)]
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

#[cfg(test)]
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

#[cfg(test)]
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

#[cfg(test)]
#[derive(Clone)]
struct AppConfig {
    greeting: String,
}

#[cfg(test)]
#[server(
    name = ReadConfig,
    prefix = "/api",
    endpoint = "read_config",
    server = crate::NtexServerFnBackend
)]
async fn read_config() -> Result<String, ServerFnError> {
    let cfg = crate::use_app_state::<AppConfig>()
        .ok_or_else(|| ServerFnError::new("AppConfig not registered".to_string()))?;
    Ok(cfg.greeting)
}

#[cfg(test)]
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        generate_route_list, generate_route_list_with_ssg, handle_server_fns, register_explicit,
        register_leptos_routes,
    };
    use leptos::config::LeptosOptions;
    use ntex::http::StatusCode;
    use ntex::web::{App as NtexApp, test};
    use ntex::web::ws;
    use server_fn::serde;
    use server_fn::{ServerFn, redirect::REDIRECT_HEADER};
    use std::{path::PathBuf, time::{SystemTime, UNIX_EPOCH}};

    fn serialize_ws_ok<T: serde::Serialize>(value: &T) -> Vec<u8> {
        let mut bytes = Vec::with_capacity(1);
        bytes.push(0);
        bytes.extend(serde_json::to_vec(value).unwrap());
        bytes
    }

    fn deserialize_ws_ok<T: serde::de::DeserializeOwned>(bytes: &[u8]) -> T {
        assert!(!bytes.is_empty());
        assert_eq!(bytes[0], 0);
        serde_json::from_slice(&bytes[1..]).unwrap()
    }

    fn temp_site_root(name: &str) -> PathBuf {
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        std::env::temp_dir().join(format!("leptos_ntex_{name}_{nonce}"))
    }

    #[ntex::test]
    async fn renders_root_route() {
        register_explicit::<EchoName>();
        register_explicit::<RedirectToAbout>();
        let routes = generate_route_list(UnitApp);
        let app = test::init_service(
            NtexApp::new()
                .route("/api/{tail:.*}", handle_server_fns())
                .configure(|cfg| {
                    register_leptos_routes(cfg, routes.clone(), unit_shell);
                }),
        )
        .await;

        let req = test::TestRequest::with_uri("/").to_request();
        let resp = test::call_service(&app, req).await;
        assert_eq!(resp.status(), StatusCode::OK);

        let body = test::read_body(resp).await;
        let html = String::from_utf8(body.to_vec()).unwrap();
        assert!(html.contains("Leptos over ntex"));
    }

    #[ntex::test]
    async fn renders_about_route() {
        register_explicit::<EchoName>();
        register_explicit::<RedirectToAbout>();
        let routes = generate_route_list(UnitApp);
        let app = test::init_service(
            NtexApp::new()
                .route("/api/{tail:.*}", handle_server_fns())
                .configure(|cfg| {
                    register_leptos_routes(cfg, routes.clone(), unit_shell);
                }),
        )
        .await;

        let req = test::TestRequest::with_uri("/about").to_request();
        let resp = test::call_service(&app, req).await;
        assert_eq!(resp.status(), StatusCode::OK);

        let body = test::read_body(resp).await;
        let html = String::from_utf8(body.to_vec()).unwrap();
        assert!(html.contains("This route is generated from the Leptos router"));
    }

    #[ntex::test]
    async fn handles_server_fn_post() {
        register_explicit::<EchoName>();
        register_explicit::<RedirectToAbout>();
        let routes = generate_route_list(UnitApp);
        let app = test::init_service(
            NtexApp::new()
                .route("/api/{tail:.*}", handle_server_fns())
                .configure(|cfg| {
                    register_leptos_routes(cfg, routes.clone(), unit_shell);
                }),
        )
        .await;

        let req = test::TestRequest::post()
            .uri(EchoName::PATH)
            .header("Content-Type", "application/x-www-form-urlencoded")
            .header("Accept", "application/json")
            .set_payload("name=Alice")
            .to_request();

        let resp = test::call_service(&app, req).await;
        assert_eq!(resp.status(), StatusCode::OK);

        let body = test::read_body(resp).await;
        let text = String::from_utf8(body.to_vec()).unwrap();
        assert!(text.contains("Hello, Alice"));
    }

    #[ntex::test]
    async fn server_fn_redirect_sets_http_redirect_for_html_form() {
        register_explicit::<EchoName>();
        register_explicit::<RedirectToAbout>();
        let routes = generate_route_list(UnitApp);
        let app = test::init_service(
            NtexApp::new()
                .route("/api/{tail:.*}", handle_server_fns())
                .configure(|cfg| {
                    register_leptos_routes(cfg, routes.clone(), unit_shell);
                }),
        )
        .await;

        let req = test::TestRequest::post()
            .uri(RedirectToAbout::PATH)
            .header("Content-Type", "application/x-www-form-urlencoded")
            .header("Accept", "text/html")
            .set_payload("")
            .to_request();

        let resp = test::call_service(&app, req).await;
        assert_eq!(resp.status(), StatusCode::FOUND);
        assert_eq!(
            resp.headers().get("location").and_then(|v| v.to_str().ok()),
            Some("/about")
        );
    }

    #[ntex::test]
    async fn server_fn_redirect_sets_client_redirect_header_for_xhr() {
        register_explicit::<EchoName>();
        register_explicit::<RedirectToAbout>();
        register_explicit::<EchoWebsocket>();
        let routes = generate_route_list(UnitApp);
        let app = test::init_service(
            NtexApp::new()
                .route("/api/{tail:.*}", handle_server_fns())
                .configure(|cfg| {
                    register_leptos_routes(cfg, routes.clone(), unit_shell);
                }),
        )
        .await;

        let req = test::TestRequest::post()
            .uri(RedirectToAbout::PATH)
            .header("Content-Type", "application/x-www-form-urlencoded")
            .header("Accept", "application/json")
            .set_payload("")
            .to_request();

        let resp = test::call_service(&app, req).await;
        assert_eq!(resp.status(), StatusCode::OK);
        assert_eq!(
            resp.headers().get("location").and_then(|v| v.to_str().ok()),
            Some("/about")
        );
        assert!(resp.headers().contains_key(REDIRECT_HEADER));
    }

    #[ntex::test]
    async fn websocket_server_fn_echoes_messages() {
        register_explicit::<EchoName>();
        register_explicit::<RedirectToAbout>();
        register_explicit::<EchoWebsocket>();

        let srv = test::server(async || {
            NtexApp::new().route("/api/{tail:.*}", handle_server_fns())
        })
        .await;

        let conn = srv.ws_at(EchoWebsocket::PATH).await.unwrap();
        let sink = conn.sink();
        let rx = conn.receiver();

        sink.send(ws::Message::Binary(serialize_ws_ok(&"hello").into()))
            .await
            .unwrap();

        let frame = rx.recv().await.unwrap().unwrap();
        match frame {
            ws::Frame::Binary(bytes) => {
                let echoed: String = deserialize_ws_ok(&bytes);
                assert_eq!(echoed, "hello");
            }
            other => panic!("unexpected websocket frame: {other:?}"),
        }

        sink.send(ws::Message::Close(None)).await.unwrap();
    }

    /// Fragmented binary messages (RFC 6455 §5.4) must be reassembled
    /// before delivery to the server-fn. An earlier revision silently
    /// dropped `Frame::Continuation`, losing any large message that a
    /// ws-client chose to fragment.
    #[ntex::test]
    async fn websocket_server_fn_reassembles_fragmented_binary() {
        use ntex::ws::Item;

        register_explicit::<EchoWebsocket>();

        let srv = test::server(async || {
            NtexApp::new().route("/api/{tail:.*}", handle_server_fns())
        })
        .await;

        let conn = srv.ws_at(EchoWebsocket::PATH).await.unwrap();
        let sink = conn.sink();
        let rx = conn.receiver();

        // Build the full payload and split into three chunks.
        let full = serialize_ws_ok(&"fragmented-hello");
        let (head, tail) = full.split_at(4);
        let (mid, last) = tail.split_at(tail.len() / 2);

        sink.send(ws::Message::Continuation(Item::FirstBinary(
            head.to_vec().into(),
        )))
        .await
        .unwrap();
        sink.send(ws::Message::Continuation(Item::Continue(
            mid.to_vec().into(),
        )))
        .await
        .unwrap();
        sink.send(ws::Message::Continuation(Item::Last(
            last.to_vec().into(),
        )))
        .await
        .unwrap();

        let frame = rx.recv().await.unwrap().unwrap();
        match frame {
            ws::Frame::Binary(bytes) => {
                let echoed: String = deserialize_ws_ok(&bytes);
                assert_eq!(echoed, "fragmented-hello");
            }
            other => panic!("unexpected websocket frame: {other:?}"),
        }

        sink.send(ws::Message::Close(None)).await.unwrap();
    }

    /// An oversized opening fragment (`FirstBinary`/`FirstText`) must
    /// be rejected with `CloseCode::Size` *before* the buffer is
    /// established. A prior bug simply pushed the bytes into the buffer
    /// without a size check, so a client could already exceed the limit
    /// on the first frame.
    #[ntex::test]
    async fn websocket_server_fn_first_fragment_over_limit_closes_with_size() {
        use crate::LeptosServerFnConfig;
        use ntex::ws::{CloseCode, Item};

        register_explicit::<EchoWebsocket>();

        let srv = test::server(async || {
            NtexApp::new()
                .state(LeptosServerFnConfig {
                    payload_limit: 16,
                    ws_channel_buffer: 16,
                    ..Default::default()
                })
                .route("/api/{tail:.*}", handle_server_fns())
        })
        .await;

        let conn = srv.ws_at(EchoWebsocket::PATH).await.unwrap();
        let sink = conn.sink();
        let rx = conn.receiver();

        let oversize = [b'A'; 64];
        sink.send(ws::Message::Continuation(Item::FirstBinary(
            oversize.to_vec().into(),
        )))
        .await
        .unwrap();

        let frame = rx.recv().await.unwrap().unwrap();
        match frame {
            ws::Frame::Close(Some(reason)) => {
                assert_eq!(reason.code, CloseCode::Size);
            }
            other => panic!(
                "expected Close(Size) on oversized First* frame, got {other:?}"
            ),
        }
    }

    // Note: the interleaved-First* protocol check in the bridge is
    // defense-in-depth. ntex's own WS codec enforces the same
    // invariant at the frame decoder/encoder layer
    // (`ProtocolError::ContinuationStarted`), so a well-behaved client
    // cannot even send such a frame sequence through `ntex::ws`. The
    // server-side guard catches the case where a custom framer or
    // future codec change would forward the bad sequence to us.

    /// When a reassembled fragmented message exceeds `payload_limit`,
    /// the bridge must close the connection with `CloseCode::Size`
    /// (1009, "Message Too Big" per RFC 6455 §7.4.1) and not forward
    /// the partial payload to the server-fn.
    #[ntex::test]
    async fn websocket_server_fn_oversize_fragmented_closes_with_size() {
        use crate::LeptosServerFnConfig;
        use ntex::ws::{CloseCode, Item};

        register_explicit::<EchoWebsocket>();

        let srv = test::server(async || {
            NtexApp::new()
                .state(LeptosServerFnConfig {
                    payload_limit: 16,
                    ws_channel_buffer: 16,
                    ..Default::default()
                })
                .route("/api/{tail:.*}", handle_server_fns())
        })
        .await;

        let conn = srv.ws_at(EchoWebsocket::PATH).await.unwrap();
        let sink = conn.sink();
        let rx = conn.receiver();

        let blob = [b'A'; 100];
        sink.send(ws::Message::Continuation(Item::FirstBinary(
            blob[..40].to_vec().into(),
        )))
        .await
        .unwrap();
        sink.send(ws::Message::Continuation(Item::Continue(
            blob[40..80].to_vec().into(),
        )))
        .await
        .unwrap();

        // The next frame received must be a Close with CloseCode::Size.
        let frame = rx.recv().await.unwrap().unwrap();
        match frame {
            ws::Frame::Close(Some(reason)) => {
                assert_eq!(reason.code, CloseCode::Size);
            }
            other => panic!(
                "expected Close(Size), got {other:?} (limit enforcement regressed)"
            ),
        }
    }

    #[ntex::test]
    async fn static_route_generator_writes_html() {
        let site_root = temp_site_root("static");
        let (_routes, generator) = generate_route_list_with_ssg(StaticApp);
        let options = LeptosOptions::builder()
            .output_name("leptos_ntex_test")
            .site_root(site_root.to_string_lossy().to_string())
            .site_pkg_dir("pkg")
            .build();

        generator.generate(&options).await;

        let index_path = site_root.join("index.html");
        let about_path = site_root.join("about.html");

        let index_html = std::fs::read_to_string(&index_path).unwrap();
        let about_html = std::fs::read_to_string(&about_path).unwrap();

        assert!(index_html.contains("Static Home"));
        assert!(about_html.contains("Static About"));

        let _ = std::fs::remove_dir_all(&site_root);
    }

    #[ntex::test]
    async fn renders_in_order_ssr_route() {
        let routes = generate_route_list(MixedApp);
        let app = test::init_service(
            NtexApp::new().configure(|cfg| {
                register_leptos_routes(cfg, routes.clone(), mixed_shell);
            }),
        )
        .await;

        let req = test::TestRequest::with_uri("/in").to_request();
        let resp = test::call_service(&app, req).await;
        assert_eq!(resp.status(), StatusCode::OK);

        let body = test::read_body(resp).await;
        let html = String::from_utf8(body.to_vec()).unwrap();
        assert!(html.contains("InOrder"));
    }

    #[ntex::test]
    async fn renders_async_ssr_route() {
        let routes = generate_route_list(MixedApp);
        let app = test::init_service(
            NtexApp::new().configure(|cfg| {
                register_leptos_routes(cfg, routes.clone(), mixed_shell);
            }),
        )
        .await;

        let req = test::TestRequest::with_uri("/async").to_request();
        let resp = test::call_service(&app, req).await;
        assert_eq!(resp.status(), StatusCode::OK);

        let body = test::read_body(resp).await;
        let html = String::from_utf8(body.to_vec()).unwrap();
        assert!(html.contains("Async"));
    }

    /// RFC 9110 §9.3.2: HEAD must mirror GET's status and headers.
    ///
    /// Note: `test::call_service` bypasses the h1 wire encoder, so we
    /// cannot assert an empty response body here (that check lives in
    /// the TCP-based integration tests). What we *can* verify is that
    /// the handler ran and produced the same status and Content-Type
    /// as GET — which is the behavioral regression the old synthetic
    /// `HEAD → 200 OK .finish()` shortcut was hiding.
    #[ntex::test]
    async fn head_request_mirrors_get_status_and_content_type() {
        use crate::LeptosRoutes;

        let routes = generate_route_list(MixedApp);
        let app = test::init_service(
            NtexApp::new().leptos_routes(routes, mixed_shell),
        )
        .await;

        let get_resp =
            test::call_service(&app, test::TestRequest::with_uri("/out").to_request()).await;
        assert_eq!(get_resp.status(), StatusCode::OK);
        let get_headers = get_resp.headers().clone();

        let head_resp = test::call_service(
            &app,
            test::TestRequest::default()
                .method(ntex::http::Method::HEAD)
                .uri("/out")
                .to_request(),
        )
        .await;
        assert_eq!(head_resp.status(), StatusCode::OK);
        assert_eq!(
            head_resp.headers().get(header::CONTENT_TYPE),
            get_headers.get(header::CONTENT_TYPE),
            "HEAD must advertise the same Content-Type as GET"
        );
    }

    /// Same parity assertions via `register_leptos_routes` on a
    /// `ServiceConfig`.
    #[ntex::test]
    async fn head_request_via_service_config_mirrors_get() {
        let routes = generate_route_list(MixedApp);
        let app = test::init_service(NtexApp::new().configure(|cfg| {
            register_leptos_routes(cfg, routes.clone(), mixed_shell);
        }))
        .await;

        let head = test::call_service(
            &app,
            test::TestRequest::default()
                .method(ntex::http::Method::HEAD)
                .uri("/out")
                .to_request(),
        )
        .await;
        assert_eq!(head.status(), StatusCode::OK);
    }

    /// HEAD on an unregistered path must not return 200 — the old
    /// synthetic HEAD handler did exactly that, hiding real 404s from
    /// monitoring. Now HEAD on a missing route falls through to the
    /// default 404 (or the app's configured fallback).
    #[ntex::test]
    async fn head_request_on_missing_route_not_200() {
        use crate::LeptosRoutes;

        let routes = generate_route_list(MixedApp);
        let app = test::init_service(
            NtexApp::new().leptos_routes(routes, mixed_shell),
        )
        .await;

        let resp = test::call_service(
            &app,
            test::TestRequest::default()
                .method(ntex::http::Method::HEAD)
                .uri("/totally-bogus-path")
                .to_request(),
        )
        .await;
        assert_ne!(resp.status(), StatusCode::OK);
    }

    #[ntex::test]
    async fn app_leptos_routes_impl_renders_route() {
        use crate::LeptosRoutes;

        let routes = generate_route_list(UnitApp);
        let app = test::init_service(
            NtexApp::new()
                .route("/api/{tail:.*}", handle_server_fns())
                .leptos_routes(routes, unit_shell),
        )
        .await;

        let req = test::TestRequest::with_uri("/").to_request();
        let resp = test::call_service(&app, req).await;
        assert_eq!(resp.status(), StatusCode::OK);

        let body = test::read_body(resp).await;
        let html = String::from_utf8(body.to_vec()).unwrap();
        assert!(html.contains("Leptos over ntex"));
    }

    #[ntex::test]
    async fn use_app_state_reads_registered_ntex_state() {
        register_explicit::<ReadConfig>();
        let config = AppConfig {
            greeting: "Privet".to_string(),
        };
        let app = test::init_service(
            NtexApp::new()
                .state(config)
                .route("/api/{tail:.*}", handle_server_fns()),
        )
        .await;

        let req = test::TestRequest::post()
            .uri(ReadConfig::PATH)
            .header("Accept", "application/json")
            .set_payload("")
            .to_request();

        let resp = test::call_service(&app, req).await;
        assert_eq!(resp.status(), StatusCode::OK);

        let body = test::read_body(resp).await;
        let text = String::from_utf8(body.to_vec()).unwrap();
        assert!(text.contains("Privet"));
    }

    // `ensure_executor_initialized()` is called implicitly by every
    // public entry point. Exercising it from a plain `#[test]` (i.e.
    // outside the ntex system) catches regressions where the executor
    // init path starts assuming an arbiter — which would break SSG or
    // library-mode usage where no ntex runtime is booted yet.
    #[test]
    fn executor_init_is_safe_without_ntex_system() {
        #[component]
        fn Empty() -> impl IntoView {
            provide_meta_context();
            view! { <h1>"empty"</h1> }
        }
        let _routes = crate::generate_route_list(Empty);
        let _routes2 = crate::generate_route_list(Empty);
    }

    #[test]
    fn try_init_executor_is_idempotent_and_reports_conflicts() {
        // First call either wins (Ok) or loses to a previous lib-test's
        // auto-install (Err). Either way, must not panic.
        let _first = crate::try_init_executor();
        // A second call must always return AlreadySet: the global
        // OnceLock is one-shot, and we rely on that property to report
        // cross-integration conflicts to callers.
        match crate::try_init_executor() {
            Err(any_spawner::ExecutorError::AlreadySet) => {}
            other => panic!(
                "expected Err(AlreadySet) on second call, got {:?}",
                other
            ),
        }
    }

    /// Oversize request detected via streaming (no Content-Length
    /// preflight, because `set_payload` doesn't set the header — the
    /// limit trips mid-read in `collect_payload` and is promoted to 413
    /// by the `PayloadTooLarge` extension marker.
    #[ntex::test]
    async fn payload_limit_streaming_overflow_returns_413() {
        use crate::LeptosServerFnConfig;
        register_explicit::<EchoName>();
        let app = test::init_service(
            NtexApp::new()
                .state(LeptosServerFnConfig {
                    payload_limit: 10,
                    ws_channel_buffer: 32,
                    ..Default::default()
                })
                .route("/api/{tail:.*}", handle_server_fns()),
        )
        .await;

        let oversized = format!("name={}", "A".repeat(100));
        let req = test::TestRequest::post()
            .uri(EchoName::PATH)
            .header("Content-Type", "application/x-www-form-urlencoded")
            .header("Accept", "application/json")
            .set_payload(oversized)
            .to_request();

        let resp = test::call_service(&app, req).await;
        assert_eq!(resp.status(), StatusCode::PAYLOAD_TOO_LARGE);
        let body = test::read_body(resp).await;
        let text = String::from_utf8(body.to_vec()).unwrap();
        assert!(text.contains("10"), "body should mention limit: {text:?}");
    }

    /// Oversize request declared via `Content-Length` is rejected by the
    /// preflight, *before* any body bytes are read. The test uses a
    /// deliberately impossible CL value and a tiny body; if the preflight
    /// did not run, `collect_payload` would still succeed (body is small)
    /// and we would incorrectly return 200.
    #[ntex::test]
    async fn payload_limit_content_length_preflight_returns_413() {
        use crate::LeptosServerFnConfig;
        register_explicit::<EchoName>();
        let app = test::init_service(
            NtexApp::new()
                .state(LeptosServerFnConfig {
                    payload_limit: 32,
                    ws_channel_buffer: 32,
                    ..Default::default()
                })
                .route("/api/{tail:.*}", handle_server_fns()),
        )
        .await;

        let req = test::TestRequest::post()
            .uri(EchoName::PATH)
            .header("Content-Type", "application/x-www-form-urlencoded")
            .header("Accept", "application/json")
            .header("Content-Length", "999999")
            .set_payload("name=Bob")
            .to_request();

        let resp = test::call_service(&app, req).await;
        assert_eq!(resp.status(), StatusCode::PAYLOAD_TOO_LARGE);
    }

    #[ntex::test]
    async fn payload_limit_accepts_small_body() {
        use crate::LeptosServerFnConfig;
        register_explicit::<EchoName>();
        let app = test::init_service(
            NtexApp::new()
                .state(LeptosServerFnConfig {
                    payload_limit: 1024,
                    ws_channel_buffer: 32,
                    ..Default::default()
                })
                .route("/api/{tail:.*}", handle_server_fns()),
        )
        .await;

        let req = test::TestRequest::post()
            .uri(EchoName::PATH)
            .header("Content-Type", "application/x-www-form-urlencoded")
            .header("Accept", "application/json")
            .set_payload("name=Bob")
            .to_request();

        let resp = test::call_service(&app, req).await;
        assert_eq!(resp.status(), StatusCode::OK);
        let body = test::read_body(resp).await;
        let text = String::from_utf8(body.to_vec()).unwrap();
        assert!(text.contains("Hello, Bob"));
    }

    #[ntex::test]
    async fn multi_location_header_is_preserved_through_res_options() {
        register_explicit::<MultiLocation>();
        let app = test::init_service(
            NtexApp::new().route("/api/{tail:.*}", handle_server_fns()),
        )
        .await;

        let req = test::TestRequest::post()
            .uri(MultiLocation::PATH)
            .header("Content-Type", "application/x-www-form-urlencoded")
            .header("Accept", "application/json")
            .set_payload("")
            .to_request();

        let resp = test::call_service(&app, req).await;
        assert_eq!(resp.status(), StatusCode::OK);

        let locations: Vec<String> = resp
            .headers()
            .get_all(ntex::http::header::LOCATION)
            .filter_map(|v| v.to_str().ok().map(str::to_string))
            .collect();
        assert_eq!(locations, vec!["/one".to_string(), "/two".to_string()]);
    }

    #[ntex::test]
    async fn extract_helper_reads_request_path() {
        register_explicit::<ProbePath>();
        let app = test::init_service(
            NtexApp::new().route("/api/{tail:.*}", handle_server_fns()),
        )
        .await;

        let req = test::TestRequest::post()
            .uri(ProbePath::PATH)
            .header("Accept", "application/json")
            .set_payload("")
            .to_request();

        let resp = test::call_service(&app, req).await;
        assert_eq!(resp.status(), StatusCode::OK);

        let body = test::read_body(resp).await;
        let text = String::from_utf8(body.to_vec()).unwrap();
        assert!(text.contains("/api/probe_path"));
    }

    #[ntex::test]
    async fn file_and_error_handler_serves_file_then_falls_back() {
        use crate::file_and_error_handler;

        let site_root = temp_site_root("file_handler");
        std::fs::create_dir_all(&site_root).unwrap();
        std::fs::write(site_root.join("hello.txt"), "world!").unwrap();

        let options = LeptosOptions::builder()
            .output_name("leptos_ntex_file_handler")
            .site_root(site_root.to_string_lossy().to_string())
            .site_pkg_dir("pkg")
            .build();

        let app = test::init_service(
            NtexApp::new()
                .state(options.clone())
                .route(
                    "/{tail:.*}",
                    file_and_error_handler(|_opts: LeptosOptions| {
                        view! { <h1>"Not Found Shell"</h1> }
                    }),
                ),
        )
        .await;

        let req = test::TestRequest::with_uri("/hello.txt").to_request();
        let resp = test::call_service(&app, req).await;
        assert_eq!(resp.status(), StatusCode::OK);
        let body = test::read_body(resp).await;
        assert_eq!(String::from_utf8(body.to_vec()).unwrap(), "world!");

        let req = test::TestRequest::with_uri("/missing").to_request();
        let resp = test::call_service(&app, req).await;
        assert_eq!(resp.status(), StatusCode::NOT_FOUND);
        let body = test::read_body(resp).await;
        let html = String::from_utf8(body.to_vec()).unwrap();
        assert!(html.contains("Not Found Shell"));

        let _ = std::fs::remove_dir_all(&site_root);
    }

    /// Builds a shell-only app with `file_and_error_handler` rooted at
    /// `site_root`, suitable for traversal assertions.
    macro_rules! traversal_app {
        ($site_root:expr) => {{
            use crate::file_and_error_handler;
            let options = LeptosOptions::builder()
                .output_name("leptos_ntex_traversal")
                .site_root($site_root.to_string_lossy().to_string())
                .site_pkg_dir("pkg")
                .build();
            test::init_service(NtexApp::new().state(options).route(
                "/{tail:.*}",
                file_and_error_handler(|_opts: LeptosOptions| {
                    view! { <h1>"Shell"</h1> }
                }),
            ))
            .await
        }};
    }

    /// Verifies that relative-parent traversal does not escape `site_root`.
    /// Writes a "secret" file *outside* the root but inside its parent, then
    /// checks that `/../secret.txt` returns the shell rather than file
    /// contents.
    #[ntex::test]
    async fn traversal_relative_parent_rejected() {
        let parent = temp_site_root("traversal_parent");
        std::fs::create_dir_all(&parent).unwrap();
        std::fs::write(parent.join("secret.txt"), "SECRET").unwrap();
        let site_root = parent.join("public");
        std::fs::create_dir_all(&site_root).unwrap();

        let app = traversal_app!(&site_root);
        let req = test::TestRequest::with_uri("/../secret.txt").to_request();
        let resp = test::call_service(&app, req).await;
        let status = resp.status();
        let body = test::read_body(resp).await;
        let text = String::from_utf8(body.to_vec()).unwrap();
        assert_ne!(
            status,
            StatusCode::OK,
            "traversal must not return 200, got body = {text:?}"
        );
        assert!(!text.contains("SECRET"), "traversal leaked: body = {text:?}");

        let _ = std::fs::remove_dir_all(&parent);
    }

    /// Percent-encoded `..` (`%2e%2e`) must not bypass the traversal filter.
    #[ntex::test]
    async fn traversal_percent_encoded_parent_rejected() {
        let parent = temp_site_root("traversal_pct");
        std::fs::create_dir_all(&parent).unwrap();
        std::fs::write(parent.join("secret.txt"), "PCT_SECRET").unwrap();
        let site_root = parent.join("public");
        std::fs::create_dir_all(&site_root).unwrap();

        let app = traversal_app!(&site_root);
        let req = test::TestRequest::with_uri("/%2e%2e/secret.txt").to_request();
        let resp = test::call_service(&app, req).await;
        let body = test::read_body(resp).await;
        let text = String::from_utf8(body.to_vec()).unwrap();
        assert!(!text.contains("PCT_SECRET"));

        let _ = std::fs::remove_dir_all(&parent);
    }

    /// A root-style URI (leading `/etc/…`) must not pull files from the
    /// real `/etc` — `Path::join` replacement of the root is the classic
    /// exploit vector. Our `safe_subpath` reconstructs the path from split
    /// segments so a bare `/etc/passwd` resolves under `<site_root>/etc/…`.
    #[ntex::test]
    async fn traversal_absolute_path_rejected() {
        let site_root = temp_site_root("traversal_abs");
        std::fs::create_dir_all(&site_root).unwrap();

        let app = traversal_app!(&site_root);
        let req = test::TestRequest::with_uri("/etc/passwd").to_request();
        let resp = test::call_service(&app, req).await;
        let status = resp.status();
        let body = test::read_body(resp).await;
        let text = String::from_utf8(body.to_vec()).unwrap();
        assert_ne!(status, StatusCode::OK);
        assert!(!text.contains("root:"), "leaked /etc/passwd: {text:?}");

        let _ = std::fs::remove_dir_all(&site_root);
    }

    /// Dotfiles (`.env`, `.htaccess`) must not be served by the fallback
    /// handler — matches the convention established by `ntex_files::Files`.
    #[ntex::test]
    async fn traversal_dotfile_rejected() {
        let site_root = temp_site_root("traversal_dot");
        std::fs::create_dir_all(&site_root).unwrap();
        std::fs::write(site_root.join(".env"), "API_KEY=secret").unwrap();

        let app = traversal_app!(&site_root);
        let req = test::TestRequest::with_uri("/.env").to_request();
        let resp = test::call_service(&app, req).await;
        let body = test::read_body(resp).await;
        let text = String::from_utf8(body.to_vec()).unwrap();
        assert!(!text.contains("API_KEY"));

        let _ = std::fs::remove_dir_all(&site_root);
    }

    /// A NUL byte in a path segment must be rejected outright — NUL is
    /// illegal in POSIX paths and typically signals a smuggling attempt.
    #[ntex::test]
    async fn traversal_null_byte_rejected() {
        let site_root = temp_site_root("traversal_nul");
        std::fs::create_dir_all(&site_root).unwrap();
        std::fs::write(site_root.join("ok.txt"), "ok").unwrap();

        let app = traversal_app!(&site_root);
        let req = test::TestRequest::with_uri("/ok%00hidden.txt").to_request();
        let resp = test::call_service(&app, req).await;
        assert_eq!(resp.status(), StatusCode::NOT_FOUND);

        let _ = std::fs::remove_dir_all(&site_root);
    }

    /// Symlink escape: a symlink inside `site_root` pointing outside must
    /// not leak external files. `canonicalize()` + `starts_with(canon_root)`
    /// catches this.
    #[cfg(unix)]
    #[ntex::test]
    async fn traversal_symlink_escape_rejected() {
        let parent = temp_site_root("traversal_symlink");
        std::fs::create_dir_all(&parent).unwrap();
        std::fs::write(parent.join("outside.txt"), "OUTSIDE").unwrap();
        let site_root = parent.join("public");
        std::fs::create_dir_all(&site_root).unwrap();
        let _ = std::os::unix::fs::symlink(
            parent.join("outside.txt"),
            site_root.join("escape.txt"),
        );

        let app = traversal_app!(&site_root);
        let req = test::TestRequest::with_uri("/escape.txt").to_request();
        let resp = test::call_service(&app, req).await;
        let body = test::read_body(resp).await;
        let text = String::from_utf8(body.to_vec()).unwrap();
        assert!(!text.contains("OUTSIDE"), "symlink escape leaked: {text:?}");

        let _ = std::fs::remove_dir_all(&parent);
    }

    /// HEAD on a statically pre-rendered route must mirror GET's status
    /// and Content-Type. (Wire-level body elision is covered by the
    /// TCP-based integration tests.)
    #[ntex::test]
    async fn head_request_on_static_route_mirrors_get() {
        let site_root = temp_site_root("head_static");
        let (routes, generator) = generate_route_list_with_ssg(StaticApp);
        let options = LeptosOptions::builder()
            .output_name("leptos_ntex_head_static")
            .site_root(site_root.to_string_lossy().to_string())
            .site_pkg_dir("pkg")
            .build();

        generator.generate(&options).await;

        let app = test::init_service(NtexApp::new().state(options.clone()).configure(
            |cfg| {
                register_leptos_routes(cfg, routes.clone(), StaticApp);
            },
        ))
        .await;

        let get_resp = test::call_service(
            &app,
            test::TestRequest::with_uri("/").to_request(),
        )
        .await;
        assert_eq!(get_resp.status(), StatusCode::OK);
        let get_headers = get_resp.headers().clone();

        let head_resp = test::call_service(
            &app,
            test::TestRequest::default()
                .method(ntex::http::Method::HEAD)
                .uri("/")
                .to_request(),
        )
        .await;
        assert_eq!(head_resp.status(), StatusCode::OK);
        assert_eq!(
            head_resp.headers().get(header::CONTENT_TYPE),
            get_headers.get(header::CONTENT_TYPE)
        );

        let _ = std::fs::remove_dir_all(&site_root);
    }

    #[ntex::test]
    async fn static_route_served_over_http() {
        let site_root = temp_site_root("http_static");
        let (routes, generator) = generate_route_list_with_ssg(StaticApp);
        let options = LeptosOptions::builder()
            .output_name("leptos_ntex_test_http")
            .site_root(site_root.to_string_lossy().to_string())
            .site_pkg_dir("pkg")
            .build();

        generator.generate(&options).await;

        let app = test::init_service(
            NtexApp::new()
                .state(options.clone())
                .configure(|cfg| {
                    register_leptos_routes(cfg, routes.clone(), StaticApp);
                }),
        )
        .await;

        let req = test::TestRequest::with_uri("/").to_request();
        let resp = test::call_service(&app, req).await;
        assert_eq!(resp.status(), StatusCode::OK);

        let body = test::read_body(resp).await;
        let html = String::from_utf8(body.to_vec()).unwrap();
        assert!(html.contains("Static Home"));

        let _ = std::fs::remove_dir_all(&site_root);
    }

    #[ntex::test]
    async fn server_fn_paths_and_get_service_roundtrip() {
        use crate::{get_server_fn_service, server_fn_paths};

        register_explicit::<EchoName>();
        register_explicit::<RedirectToAbout>();

        let paths: Vec<_> = server_fn_paths().collect();
        assert!(paths.iter().any(|(p, _)| *p == EchoName::PATH));
        assert!(paths.iter().any(|(p, _)| *p == RedirectToAbout::PATH));

        let found = get_server_fn_service(EchoName::PATH, &ntex::http::Method::POST);
        assert!(found.is_some());

        let not_found =
            get_server_fn_service(EchoName::PATH, &ntex::http::Method::GET);
        assert!(not_found.is_none());

        let missing = get_server_fn_service("/api/does_not_exist", &ntex::http::Method::POST);
        assert!(missing.is_none());
    }

    #[ntex::test]
    async fn generate_request_and_parts_returns_cloned_head() {
        use crate::generate_request_and_parts;

        let req = test::TestRequest::default()
            .uri("/some/path?q=1")
            .header("x-custom", "yes")
            .to_http_request();
        let payload = ntex::http::Payload::None;

        let (_server_fn_req, head) = generate_request_and_parts(req.clone(), payload);
        assert_eq!(head.uri().path(), "/some/path");
        assert_eq!(head.uri().query(), Some("q=1"));
        assert_eq!(
            head.headers().get("x-custom").and_then(|v| v.to_str().ok()),
            Some("yes")
        );
    }

    #[ntex::test]
    async fn handle_response_inner_renders_shell() {
        use crate::handle_response_inner;
        use leptos_integration_utils::{BoxedFnOnce, PinnedStream};
        use futures::StreamExt;
        use futures::stream::once as stream_once;

        let app = test::init_service(NtexApp::new().route(
            "/hrinner",
            ntex::web::get().to(|req: ntex::web::HttpRequest| async move {
                handle_response_inner(
                    || {},
                    || view! { <!DOCTYPE html><html><body><h1>"HriHello"</h1></body></html> },
                    req,
                    |app, chunks: BoxedFnOnce<PinnedStream<String>>, _supports_ooo| {
                        Box::pin(async move {
                            let app = app.to_html_stream_in_order().collect::<String>().await;
                            Box::pin(stream_once(async move { app }).chain(chunks()))
                                as PinnedStream<String>
                        })
                    },
                )
                .await
            }),
        ))
        .await;

        let req = test::TestRequest::with_uri("/hrinner").to_request();
        let resp = test::call_service(&app, req).await;
        assert_eq!(resp.status(), StatusCode::OK);
        let body = test::read_body(resp).await;
        let html = String::from_utf8(body.to_vec()).unwrap();
        assert!(html.contains("HriHello"));
    }
}
