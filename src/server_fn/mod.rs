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
pub(crate) mod websocket;

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
/// and friends. Installation itself does not require a running arbiter;
/// spawning work does require an active ntex runtime.
pub struct NtexExecutor;

impl any_spawner::CustomExecutor for NtexExecutor {
    fn spawn(&self, fut: any_spawner::PinnedFuture<()>) {
        ntex::rt::spawn(crate::owner::OwnerContextFuture::from_pin(fut));
    }

    fn spawn_local(&self, fut: any_spawner::PinnedLocalFuture<()>) {
        ntex::rt::spawn(crate::owner::OwnerContextFuture::from_pin(fut));
    }

    fn poll_local(&self) {}
}

/// The server-function backend used by `#[server]` macros to target the
/// ntex integration.
///
/// Pass this as the `server = leptos_ntex_unofficial::NtexServerFnBackend`
/// argument on the `#[server]` attribute so that the server function is
/// dispatched through the ntex runtime.
///
/// Explicit calls to the backend's `Server::spawn` retain the current request
/// context until the task finishes, even after an HTTP response returns. Tasks
/// spawned in a WebSocket dispatch are also cancelled when its connection pump
/// ends. Detecting a transport disconnect can be delayed while input is paused.
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
        use futures::FutureExt;
        let scope = leptos::context::use_context::<websocket::ConnectionScope>();
        let owner = leptos::context::use_context::<std::sync::Weak<crate::owner::OwnerCleanup>>()
            .and_then(|owner| owner.upgrade());
        let future = async move {
            if let Some(scope) = scope {
                futures::pin_mut!(future);
                futures::select_biased! {
                    _ = scope.cancelled.fuse() => {},
                    _ = future.fuse() => {},
                }
            } else {
                future.await;
            }
        };
        let has_request_owner = owner.is_some();
        let task = OwnedTask {
            future: Box::pin(future),
            _owner: owner,
        };
        if has_request_owner {
            // The WebSocket output forwarder keeps the server function's scope
            // after dispatch returns, including through cancellation Drop.
            ntex::rt::spawn(crate::owner::ScopedWork::new(task));
        } else {
            // Standalone backend use has no request scope to retain.
            ntex::rt::spawn(crate::owner::OwnerContextFuture::new(task));
        }
        Ok(())
    }
}

// Explicit field order keeps context available to user future destructors,
// including cancellation before the first poll.
struct OwnedTask<F> {
    future: std::pin::Pin<Box<F>>,
    _owner: Option<std::sync::Arc<crate::owner::OwnerCleanup>>,
}

impl<F: Future<Output = ()>> Future for OwnedTask<F> {
    type Output = ();
    fn poll(
        self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<()> {
        self.get_mut().future.as_mut().poll(cx)
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
