//! Configuration knobs for the server-function dispatcher and helpers
//! that enforce payload limits for non-streaming request bodies.

use bytes::{Bytes as SfBytes, BytesMut as SfBytesMut};
use ntex::http::{Payload, header};
use ntex::web::HttpRequest;
use std::io;

/// Default maximum payload size accepted by
/// [`NtexRequest`](crate::NtexRequest) when collecting server-function
/// request bodies (2 MiB).
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
    /// handler instead of going through
    /// [`handle_server_fns`](crate::handle_server_fns).
    pub ws_subprotocol: Option<&'static str>,
}

impl LeptosServerFnConfig {
    /// Creates a config initialized with the crate defaults.
    ///
    /// This is equivalent to [`Default::default`], but is easier to use in
    /// builder-style app wiring:
    ///
    /// ```no_run
    /// use leptos_ntex_unofficial::LeptosServerFnConfig;
    ///
    /// let config = LeptosServerFnConfig::new()
    ///     .with_payload_limit(8 * 1024 * 1024)
    ///     .with_ws_channel_buffer(512)
    ///     .with_ws_subprotocol("graphql-ws");
    /// ```
    pub const fn new() -> Self {
        Self {
            payload_limit: DEFAULT_PAYLOAD_LIMIT,
            ws_channel_buffer: DEFAULT_WS_CHANNEL_BUFFER,
            ws_subprotocol: None,
        }
    }

    /// Sets the maximum accepted non-streaming server-function request
    /// body size in bytes.
    pub const fn with_payload_limit(mut self, payload_limit: usize) -> Self {
        self.payload_limit = payload_limit;
        self
    }

    /// Sets the bounded channel capacity used by server-function
    /// WebSocket streams.
    pub const fn with_ws_channel_buffer(mut self, ws_channel_buffer: usize) -> Self {
        self.ws_channel_buffer = ws_channel_buffer;
        self
    }

    /// Sets the WebSocket subprotocol this adapter may echo during
    /// upgrade if the client offered the same value.
    pub const fn with_ws_subprotocol(mut self, ws_subprotocol: &'static str) -> Self {
        self.ws_subprotocol = Some(ws_subprotocol);
        self
    }
}

impl Default for LeptosServerFnConfig {
    fn default() -> Self {
        Self::new()
    }
}

pub(crate) fn server_fn_config(req: &HttpRequest) -> LeptosServerFnConfig {
    req.app_state::<LeptosServerFnConfig>()
        .copied()
        .unwrap_or_default()
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
pub(crate) struct PayloadTooLarge;

/// Returns true when the request declares `Content-Length` and it exceeds
/// `limit`. The preflight avoids reading the body at all when the client
/// tells us up-front that it is oversize.
pub(crate) fn content_length_exceeds(req: &HttpRequest, limit: usize) -> bool {
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
pub(crate) async fn collect_payload(
    req: &HttpRequest,
    mut payload: Payload,
    limit: usize,
) -> Result<SfBytes, io::Error> {
    let capacity = req
        .headers()
        .get(header::CONTENT_LENGTH)
        .and_then(|v| v.to_str().ok())
        .and_then(|s| s.parse::<usize>().ok())
        .filter(|declared| *declared <= limit)
        .unwrap_or(0);
    let mut buf = SfBytesMut::with_capacity(capacity);
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
