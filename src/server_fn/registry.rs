//! Server-function registry: the one crate-wide `inventory`-backed map
//! and the public helpers used to query and register entries.

use ntex::http::Method as HttpMethod;
use or_poisoned::OrPoisoned;
use server_fn::{Protocol, ServerFn, ServerFnTraitObj, middleware::BoxedService};
use std::{
    collections::HashMap,
    sync::{LazyLock, RwLock},
};

use crate::server_fn::request::NtexRequest;
use crate::server_fn::response::NtexServerResponse;

/// Indexes server functions by their `&'static str` path, so per-request
/// lookup doesn't need to allocate a `String` to build the map key.
/// Multiple methods under the same path are supported by a small `Vec`
/// (typical server-fn apps have one method per path).
pub(crate) type LazyServerFnMap<Req, Res> =
    LazyLock<RwLock<HashMap<&'static str, Vec<(HttpMethod, ServerFnTraitObj<Req, Res>)>>>>;

pub(crate) static REGISTERED_SERVER_FUNCTIONS: LazyServerFnMap<NtexRequest, NtexServerResponse> =
    LazyLock::new(|| {
        let mut map = HashMap::new();
        for obj in server_fn::inventory::iter::<ServerFnTraitObj<NtexRequest, NtexServerResponse>>
            .into_iter()
        {
            let entries = map.entry(obj.path()).or_insert_with(Vec::new);
            // Dedup the inventory pass on `(path, method)` too — NOT just
            // `register_explicit`. Two inventory entries colliding on the same
            // path+method (a duplicate `submit!`, an aliased endpoint) would
            // otherwise accumulate as duplicate `server_fn_paths()` rows and
            // duplicate route registrations, and a later `register_explicit`
            // would replace only the first slot, orphaning the rest.
            upsert(entries, obj.method(), obj.clone());
        }
        RwLock::new(map)
    });

/// Inserts or replaces the entry for `(path, method)` in a path's method Vec,
/// last-writer-wins — the single write-site invariant that keeps both the
/// `inventory` init and [`register_explicit`] from accumulating duplicate
/// `(path, method)` slots. Mirrors the reference `server_fn` axum/actix maps,
/// which key on `(path, method)` and `insert`.
fn upsert(
    entries: &mut Vec<(
        HttpMethod,
        ServerFnTraitObj<NtexRequest, NtexServerResponse>,
    )>,
    method: HttpMethod,
    obj: ServerFnTraitObj<NtexRequest, NtexServerResponse>,
) {
    match entries.iter_mut().find(|(m, _)| *m == method) {
        Some(slot) => slot.1 = obj,
        None => entries.push((method, obj)),
    }
}

/// Explicitly registers a server function with this integration.
///
/// On native targets you normally do not need to call this — the
/// `#[server]` macro emits an `inventory::submit!` entry that this
/// crate picks up at startup, so every
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
    let obj = ServerFnTraitObj::new::<T>(|req| Box::pin(T::run_on_server(req)));
    let method = T::Protocol::METHOD;
    let mut guard = REGISTERED_SERVER_FUNCTIONS.write().or_poisoned();
    let entries = guard.entry(T::PATH).or_default();
    // Idempotent and last-writer-wins via the shared `upsert`: a repeated or
    // explicit registration REPLACES the entry for that `(path, method)`
    // instead of appending a duplicate. Without this, an explicit registration
    // on a native target where `inventory` already populated the map would be
    // dead (the first match wins in `lookup_server_fn`) and `server_fn_paths()`
    // would emit duplicates.
    upsert(entries, method, obj);
}

/// Returns an iterator over the `(path, method)` pairs of every server
/// function that has been registered with this integration.
pub fn server_fn_paths() -> impl Iterator<Item = (&'static str, HttpMethod)> {
    let paths: Vec<_> = REGISTERED_SERVER_FUNCTIONS
        .read()
        .or_poisoned()
        .iter()
        .flat_map(|(path, entries)| entries.iter().map(move |(m, _)| (*path, m.clone())))
        .collect();
    paths.into_iter()
}

pub(crate) fn lookup_server_fn(
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

pub(crate) fn server_fn_methods(path: &str) -> Vec<HttpMethod> {
    let mut methods = Vec::new();
    if let Some(entries) = REGISTERED_SERVER_FUNCTIONS.read().or_poisoned().get(path) {
        for (method, _) in entries {
            if !methods.contains(method) {
                methods.push(method.clone());
            }
        }
    }
    methods
}

/// Looks up the service for the server function registered at the given
/// path and method, applying any middlewares that were attached to it.
///
/// Intended for the catchall [`handle_server_fns`](crate::handle_server_fns)
/// dispatcher and for advanced compositions. When server functions are
/// mounted through [`LeptosRoutes::leptos_routes`](crate::LeptosRoutes::leptos_routes)
/// / [`register_leptos_routes`](crate::register_leptos_routes) the lookup
/// is avoided — each path gets its own handler closing over the
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
