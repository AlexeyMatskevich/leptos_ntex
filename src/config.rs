//! Configuration knobs for the server-function dispatcher and helpers
//! that enforce payload limits while collecting request bodies.

use bytes::{Bytes as SfBytes, BytesMut as SfBytesMut};
use ntex::http::{Payload, header};
use ntex::web::HttpRequest;
use std::io;

/// Default maximum payload size enforced by
/// [`NtexRequest`](crate::NtexRequest) for server-function requests — buffered
/// and streaming HTTP request bodies, and each server-function WebSocket
/// message (2 MiB).
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
///     .route("/api/{tail}*", handle_server_fns());
/// # }
/// ```
#[derive(Debug, Clone, Copy)]
pub struct LeptosServerFnConfig {
    /// Maximum accepted body size in bytes for a server-function request.
    /// HTTP requests exceeding this limit are rejected with
    /// `413 Payload Too Large`, whether the client declares size up-front
    /// via `Content-Length` (rejected before the server function runs) or
    /// streams a body whose length is unknown to the server
    /// (`Transfer-Encoding: chunked`, any body without `Content-Length`)
    /// and exceeds the limit mid-flight (rejected once the excess byte is
    /// observed; the partial payload is discarded).
    ///
    /// The same limit also bounds each server-function WebSocket message
    /// (a single frame, or a reassembled fragmented message): exceeding it
    /// closes the connection with close code `1009` (`Message Too Big`,
    /// RFC 6455 §7.4.1).
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

    /// Sets the maximum accepted server-function payload size in bytes — for
    /// buffered and streaming HTTP request bodies and for WebSocket messages
    /// (see [`payload_limit`](Self::payload_limit)).
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

/// Returns true when the request declares a `Content-Length` that the
/// preflight should reject up-front: either a parseable value exceeding
/// `limit`, or a **present but malformed** declaration (non-numeric, or a
/// number larger than `usize` can hold). The preflight avoids reading the
/// body at all when the client tells us up-front that it is oversize.
///
/// A malformed declaration is treated as oversize on purpose: a value like
/// `99999999999999999999999999` (beyond `usize::MAX`) or `abc` is a
/// pathological/oversize claim, and accepting it as "no declaration" would
/// only defer the rejection to the body-collection path. A genuinely absent
/// header returns `false` — the streaming `collect_payload` check still
/// bounds an unknown-length body.
pub(crate) fn content_length_exceeds(req: &HttpRequest, limit: usize) -> bool {
    let Some(raw) = req.headers().get(header::CONTENT_LENGTH) else {
        return false;
    };
    match raw.to_str().ok().and_then(|s| s.parse::<usize>().ok()) {
        Some(declared) => declared > limit,
        None => true,
    }
}

/// Parses an RFC 9110 §12.4.2 quality value: a finite number in `0.0..=1.0`.
/// Returns `None` for anything malformed — non-numeric, out of range, or
/// non-finite (`NaN`/`inf`) — so every caller can apply one uniform
/// "malformed → field default" policy. Shared by the `Accept` (HTML) and
/// `Accept-Encoding` parsers so the two cannot drift on how they treat a bad
/// `q` (the range check also rejects `NaN`/`inf`, since neither compares
/// inside `0.0..=1.0`).
pub(crate) fn parse_qvalue(s: &str) -> Option<f32> {
    s.trim()
        .parse::<f32>()
        .ok()
        .filter(|q| (0.0..=1.0).contains(q))
}

/// Upper bound on the buffer capacity reserved up-front from a declared
/// `Content-Length`, before any body byte has actually arrived. The
/// declaration is client-controlled: without a cap, N parallel connections
/// each declaring (up to) the configured limit and then trickling the body
/// would reserve N × limit of memory before sending a single byte. 64 KiB
/// keeps the up-front reservation small (actix-web caps its eager body
/// buffer at 32 KiB for the same reason); the buffer still grows on demand
/// as real chunks arrive, so honest large uploads pay only amortized
/// reallocation, not a changed limit.
pub(crate) const INITIAL_PAYLOAD_CAPACITY_CAP: usize = 64 * 1024;

/// Initial buffer capacity for collecting a request body: the declared
/// `Content-Length` when it is plausible (present and within `limit`),
/// clamped to [`INITIAL_PAYLOAD_CAPACITY_CAP`] so the declaration alone
/// cannot reserve large allocations. An absent or over-limit declaration
/// reserves nothing — the over-limit case is rejected by the preflight /
/// streaming check anyway, so pre-sizing for it would only serve an attacker.
pub(crate) fn initial_payload_capacity(content_length: Option<usize>, limit: usize) -> usize {
    content_length
        .filter(|declared| *declared <= limit)
        .unwrap_or(0)
        .min(INITIAL_PAYLOAD_CAPACITY_CAP)
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
    let declared = req
        .headers()
        .get(header::CONTENT_LENGTH)
        .and_then(|v| v.to_str().ok())
        .and_then(|s| s.parse::<usize>().ok());
    let mut buf = SfBytesMut::with_capacity(initial_payload_capacity(declared, limit));
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
