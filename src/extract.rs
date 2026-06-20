//! Helpers for running [`FromRequest`](ntex::web::FromRequest) extractors
//! inside a server function.

use leptos::context::use_context;
use ntex::http::Payload;
use ntex::web::ErrorRenderer;
use send_wrapper::SendWrapper;
use std::fmt::Display;

use crate::request::Request;

/// Runs an ntex [`FromRequest`](ntex::web::FromRequest) extractor inside a
/// server function, using the default error renderer.
///
/// Note that ntex extractors that consume the request body will not work here —
/// the body is read by the server-function framework itself. This helper is
/// only useful for extractors that operate on the request head
/// (path/query/headers/etc.).
///
/// The error renderer is fixed to [`ntex::web::DefaultError`]. Apps wired
/// with a custom error renderer should use [`extract_with_err`] and spell
/// the renderer type explicitly, or call `T::from_request(&req, payload)`
/// directly after pulling `req` out of context.
///
/// # Errors
///
/// Returns [`ServerFnErrorErr`](server_fn::error::ServerFnErrorErr) (its
/// `ServerError` variant) if the [`Request`] is missing from
/// context — the helper was called outside a server function — or if the
/// extractor itself fails.
///
/// # Panics
///
/// Shares [`extract_with_err`]'s cross-thread hazard: the extractor future
/// wraps the non-`Send` ntex request in a [`SendWrapper`], so it must be
/// awaited on the ntex worker thread that invoked the server function. Awaiting
/// or dropping it on a different thread panics.
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
///
/// # Errors
///
/// Returns [`ServerFnErrorErr`](server_fn::error::ServerFnErrorErr) (its
/// `ServerError` variant) if the [`Request`] is missing from
/// context — the helper was called outside a server function — or if the
/// extractor itself fails.
///
/// # Panics
///
/// The extractor future is wrapped in a [`SendWrapper`] around the non-`Send`
/// ntex request, so — like every server-function entry point in this crate —
/// it must be awaited on the ntex worker thread that invoked the server
/// function. Awaiting or dropping it on a different thread (e.g. after moving
/// it onto a foreign runtime) panics. Inside a normal server function body the
/// future never leaves its worker, so this is only a hazard for code that
/// deliberately relocates it.
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
