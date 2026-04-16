//! Helpers for reading ntex application state and running
//! [`FromRequest`](ntex::web::FromRequest) extractors inside a server
//! function.

use leptos::context::use_context;
use ntex::http::Payload;
use ntex::web::ErrorRenderer;
use send_wrapper::SendWrapper;
use std::fmt::Display;

use crate::request::Request;

/// Returns a clone of the ntex [application state](ntex::web::App::state)
/// of type `T`.
///
/// Returns `None` in two distinct situations:
/// 1. The current reactive context has no [`Request`] — i.e. the function
///    is being called outside an SSR/server-fn handler (or before
///    [`handle_server_fns`](crate::handle_server_fns) / any `render_app_*`
///    installed the request into context).
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
