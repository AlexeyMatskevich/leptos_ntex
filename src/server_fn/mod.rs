//! Ntex-backed server-function runtime.
//!
//! Hosts the ntex-specific [`server_fn::server::Server`] backend
//! ([`NtexServerFnBackend`]), the [`any_spawner::CustomExecutor`]
//! implementation ([`NtexExecutor`]), the request/response newtypes,
//! and the public registry + dispatcher helpers.

pub(crate) mod handlers;
pub(crate) mod registry;
pub(crate) mod request;
pub(crate) mod response;

use ntex::http::Payload;
use ntex::web::HttpRequest;
use server_fn::{error::FromServerFnError, server::Server};
use std::future::Future;

pub use handlers::{handle_server_fns, handle_server_fns_with_context};
pub use registry::{get_server_fn_service, register_explicit, server_fn_paths};
pub use request::NtexRequest;
pub use response::NtexServerResponse;

/// `any_spawner::CustomExecutor` impl that delegates task spawning to
/// `ntex::rt`.
///
/// This makes Leptos's reactive tasks run on whatever ntex runtime the app
/// was compiled against (tokio / compio / default neon), so Suspense work
/// and router navigations stay on the same arbiter as the request that
/// triggered them instead of bouncing onto a separate thread pool.
///
/// Installed automatically from [`generate_route_list`](crate::generate_route_list)
/// and friends; must be called from inside an ntex arbiter (i.e. from
/// within `#[ntex::main]` or `#[ntex::test]`).
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
