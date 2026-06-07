//! HTTP response types and the [`redirect`] helper.
//!
//! Hosts the response side of the integration: the [`ResponseOptions`]
//! override used from inside components/server-fns, the boxed streaming
//! HTML body type, the `ExtendResponse` adapter for ntex, and the
//! public [`redirect`] helper.

use futures::{Stream, StreamExt, stream::once};
use ntex::http::{
    StatusCode,
    header::{self, HeaderName, HeaderValue},
};
use ntex::util::Bytes as NBytes;
use ntex::web::HttpResponse;
use or_poisoned::OrPoisoned;
use server_fn::redirect::REDIRECT_HEADER;
use std::{
    future::Future,
    io,
    pin::Pin,
    sync::{Arc, RwLock},
    task::{Context, Poll},
    thread::ThreadId,
};

use leptos::context::{provide_context, use_context};
use leptos::prelude::ReadValue;
use leptos::reactive::owner::{Owner, Sandboxed};
use leptos::{IntoView, PrefetchLazyFn, WasmSplitManifest};
use leptos_integration_utils::{
    BoxedFnOnce, ExtendResponse, PinnedFuture, PinnedStream, build_response,
};
use leptos_meta::{Link, ServerMetaContextOutput};

use crate::request::Request;

/// A boxed stream of HTML chunks, as used for progressive streaming of SSR
/// responses. Mirrors the equivalent type alias in `leptos_axum`.
pub type PinnedHtmlStream = std::pin::Pin<Box<dyn Stream<Item = io::Result<NBytes>> + Send>>;

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

pub(crate) struct NtexResponse(pub(crate) HttpResponse);

impl NtexResponse {
    pub(crate) fn take(self) -> HttpResponse {
        self.0
    }

    pub(crate) fn extend_response_parts(&mut self, parts: ResponseParts) {
        let headers = self.0.headers_mut();
        for (key, value) in parts.headers.iter() {
            if should_replace_header(key) {
                headers.insert(key.clone(), value.clone());
            } else {
                headers.append(key.clone(), value.clone());
            }
        }
        if let Some(status) = parts.status {
            *self.0.status_mut() = status;
        }
    }
}

fn should_replace_header(key: &HeaderName) -> bool {
    matches!(
        key,
        &header::CONTENT_LENGTH
            | &header::CONTENT_TYPE
            | &header::CONTENT_ENCODING
            | &header::TRANSFER_ENCODING
            | &header::LOCATION
            | &header::ETAG
            | &header::LAST_MODIFIED
            | &header::CACHE_CONTROL
            | &header::EXPIRES
            | &header::CONTENT_DISPOSITION
            | &header::CONTENT_RANGE
            | &header::ACCEPT_RANGES
    )
}

impl ExtendResponse for NtexResponse {
    type ResponseOptions = ResponseOptions;

    fn from_stream(stream: impl Stream<Item = String> + Send + 'static) -> Self {
        let pinned = Box::pin(stream.map(|chunk| Ok::<NBytes, io::Error>(NBytes::from(chunk))));
        NtexResponse(
            HttpResponse::Ok()
                .content_type("text/html; charset=utf-8")
                .streaming(pinned),
        )
    }

    fn extend_response(&mut self, res_options: &Self::ResponseOptions) {
        let taken = std::mem::take(&mut *res_options.0.write().or_poisoned());
        self.extend_response_parts(taken);
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

    // ntex stopgap override of the default `from_app`.
    //
    // This is a near-verbatim copy of `ExtendResponse::from_app` from
    // `leptos_integration_utils` 0.8.8, with exactly one behavioural change:
    // the reactive `Owner` is cleaned up through `OwnerCleanupStream`'s `Drop`
    // instead of a trailing stream item, so cleanup also runs when a client
    // disconnects mid-response (the trailing-item approach only runs on a full
    // drain, leaking the owner on early disconnect — leptos-rs/leptos#4739).
    //
    // TODO: delete this override and `OwnerCleanupStream`, and bump
    // `leptos_integration_utils`, once the upstream crate ships the
    // `Drop`-based cleanup.
    //
    // The explicit `-> impl Future + Send` form (rather than `async fn`)
    // mirrors the upstream trait method's signature verbatim and keeps the
    // `Send` bound visible; clippy's `manual_async_fn` would rewrite it.
    #[allow(clippy::manual_async_fn)]
    fn from_app<IV>(
        app_fn: impl FnOnce() -> IV + Send + 'static,
        meta_context: ServerMetaContextOutput,
        additional_context: impl FnOnce() + Send + 'static,
        res_options: Self::ResponseOptions,
        stream_builder: fn(
            IV,
            BoxedFnOnce<PinnedStream<String>>,
            bool,
        ) -> PinnedFuture<PinnedStream<String>>,
        supports_ooo: bool,
    ) -> impl Future<Output = Self> + Send
    where
        IV: IntoView + 'static,
    {
        async move {
            let prefetches = PrefetchLazyFn::default();

            let (owner, stream) =
                build_response(app_fn, additional_context, stream_builder, supports_ooo);

            owner.with(|| provide_context(prefetches.clone()));

            let sc = owner.shared_context().unwrap();

            let stream = stream.await.ready_chunks(32).map(|n| n.join(""));

            while let Some(pending) = sc.await_deferred() {
                pending.await;
            }

            if !prefetches.0.read_value().is_empty() {
                use leptos::prelude::*;

                let nonce = use_nonce().map(|n| n.to_string()).unwrap_or_default();
                if let Some(manifest) = use_context::<WasmSplitManifest>() {
                    let (pkg_path, manifest, wasm_split_file) = &*manifest.0.read_value();
                    let prefetches = prefetches.0.read_value();

                    let all_prefetches = prefetches
                        .iter()
                        .flat_map(|key| manifest.get(*key).into_iter().flatten());

                    for module in all_prefetches {
                        // to_html() on leptos_meta components registers them with the meta
                        // context, rather than returning HTML directly
                        _ = view! {
                            <Link
                                rel="preload"
                                href=format!("{pkg_path}/{module}.wasm")
                                as_="fetch"
                                type_="application/wasm"
                                crossorigin=nonce.clone()
                            />
                        }
                        .to_html();
                    }
                    _ = view! {
                        <Link rel="modulepreload" href=format!("{pkg_path}/{wasm_split_file}") crossorigin=nonce/>
                    }
                    .to_html();
                }
            }

            let mut stream = Box::pin(meta_context.inject_meta_context(stream).await.then({
                let sc = Arc::clone(&sc);
                move |chunk| {
                    let sc = Arc::clone(&sc);
                    async move {
                        while let Some(pending) = sc.await_deferred() {
                            pending.await;
                        }
                        chunk
                    }
                }
            }));

            // wait for the first chunk of the stream, then set the status and headers
            let first_chunk = stream.next().await.unwrap_or_default();

            // ntex divergence from upstream: tie owner cleanup to the body's
            // `Drop` (via `OwnerCleanupStream`) instead of a trailing stream
            // item, so it also runs on early client disconnect.
            let mut res = Self::from_stream(Sandboxed::new(OwnerCleanupStream::new(
                once(async move { first_chunk }).chain(stream),
                owner,
            )));

            res.extend_response(&res_options);

            // Set the Content Type headers on all responses. This makes Firefox show the page
            // source without complaining
            res.set_default_content_type("text/html; charset=utf-8");

            res
        }
    }
}

/// Wraps a response body stream and ties cleanup of the reactive [`Owner`] to
/// the stream's `Drop`, rather than to a chained trailing future.
///
/// Upstream `leptos_integration_utils` (0.8.8) appends
/// `owner.unset_with_forced_cleanup()` as the last item of the stream, so it
/// only runs when the stream is polled to completion. A client disconnecting
/// mid-response (slow client, browser cancel, proxy timeout) drops the body
/// before that terminal item runs, leaking the `Owner` and everything it
/// transitively keeps alive. Cleaning up on `Drop` runs whether the stream
/// finishes or is cancelled.
///
/// **ntex divergence from the upstream/axum fix.** The reactive cleanup is not
/// safe to run from an arbitrary thread (the same cross-thread-drop hazard the
/// [`Request`](crate::request::Request) wrapper documents). The axum
/// integration never hits this — every body is `Send` on a uniform tokio
/// runtime — but ntex workers are single-threaded, so in the normal path the
/// body is created, polled, and dropped on the same worker thread. As a
/// belt-and-suspenders guard we record the origin thread and only force
/// cleanup when dropped there; an unexpected off-thread drop falls back to the
/// pre-stopgap behaviour (skip the forced cleanup — no worse than today's
/// leak) rather than risking a panic during response teardown.
struct OwnerCleanupStream {
    inner: Pin<Box<dyn Stream<Item = String> + Send>>,
    owner: Option<Owner>,
    origin_thread: ThreadId,
}

impl OwnerCleanupStream {
    fn new(inner: impl Stream<Item = String> + Send + 'static, owner: Owner) -> Self {
        Self {
            inner: Box::pin(inner),
            owner: Some(owner),
            origin_thread: std::thread::current().id(),
        }
    }
}

impl Stream for OwnerCleanupStream {
    type Item = String;

    fn poll_next(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        self.get_mut().inner.as_mut().poll_next(cx)
    }
}

impl Drop for OwnerCleanupStream {
    fn drop(&mut self) {
        if let Some(owner) = self.owner.take() {
            if std::thread::current().id() == self.origin_thread {
                owner.unset_with_forced_cleanup();
            } else {
                // Off-origin-thread drop: do not run the reactive teardown
                // here. Letting `owner` drop normally would not help — dropping
                // the last `Owner` handle itself runs thread-affine cleanup
                // (child disposal, cleanup callbacks, arena removal), the very
                // cross-thread work we must avoid. `mem::forget` the handle so
                // none of it runs off-thread; the reactive graph leaks, which
                // is strictly no worse than the pre-stopgap behaviour. Mirrors
                // the `SendWrapper` invalid-thread guard in `Request`'s `Drop`
                // (see `crate::request`).
                let msg = "OwnerCleanupStream dropped off its origin thread; \
                           leaking the reactive Owner to avoid a cross-thread \
                           cleanup panic";
                #[cfg(feature = "tracing")]
                tracing::warn!("{msg}");
                #[cfg(not(feature = "tracing"))]
                eprintln!("{msg}");
                std::mem::forget(owner);
            }
        }
    }
}

/// Returns whether an `Accept` header value indicates the client will accept
/// an HTML response — an ordinary browser navigation or a plain `<form>`
/// submission, as opposed to a programmatic client expecting structured data.
///
/// Unlike a naive `contains("text/html")` check, each comma-separated media
/// range is parsed with the `mime` crate and an explicit `q=0` refusal is
/// honoured, so `text/html;q=0` (the client refusing HTML) and
/// `application/x-text/html-fake` (an unrelated, unparseable range) are both
/// correctly treated as *not* accepting HTML. Mirrors the `accepts_html`
/// parsing in `leptos_axum` / `leptos_actix`.
pub(crate) fn accept_header_includes_html(accept: &str) -> bool {
    accept.split(',').any(|range| {
        let Ok(media) = range.trim().parse::<mime::Mime>() else {
            return false;
        };
        if media.type_() != mime::TEXT || media.subtype() != mime::HTML {
            return false;
        }
        // honour an explicit `q=0`, which means the client refuses HTML
        match media.get_param("q") {
            Some(q) => q.as_str().parse::<f32>().map(|w| w > 0.0).unwrap_or(true),
            None => true,
        }
    })
}

/// Redirects the browser from within a server function.
///
/// Depending on the `Accept` header on the current request the function
/// either sets a `302 Found` (for plain `<form>` submissions) or emits a
/// custom [`REDIRECT_HEADER`] that the Leptos client picks up to perform a
/// client-side navigation while still letting the server fn return its
/// payload. The `Location` header is set whenever `path` is a valid HTTP
/// header value; a `path` carrying bytes that are illegal in a header value
/// (CR, LF, NUL, other control bytes) is logged and ignored — no `Location`,
/// no status change — rather than panicking.
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
        // The path is app-controlled (e.g. `<Redirect>` or a `?next=`
        // parameter) and may contain bytes that are not valid in an HTTP
        // header value (CR, LF, NUL, other control bytes). `from_str`
        // rejecting those is what prevents header injection — but turning
        // that rejection into a panic would let a crafted redirect target
        // abort the request handler. Degrade gracefully instead, matching
        // the conservative `NtexServerResponse::redirect` sibling.
        let Ok(location) = HeaderValue::from_str(path) else {
            let msg =
                "redirect() called with a path that is not a valid header value; Location not set.";
            #[cfg(feature = "tracing")]
            tracing::warn!("{msg}");
            #[cfg(not(feature = "tracing"))]
            eprintln!("{msg}");
            return;
        };
        res.insert_header(header::LOCATION, location);

        let accepts_html = req
            .headers()
            .get(header::ACCEPT)
            .and_then(|v| v.to_str().ok())
            .map(accept_header_includes_html)
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
        #[cfg(not(feature = "tracing"))]
        eprintln!("Couldn't retrieve either Parts or ResponseOptions while trying to redirect().");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn accept_header_plain_navigation_is_html() {
        // typical browser navigation
        assert!(accept_header_includes_html(
            "text/html,application/xhtml+xml,application/xml;q=0.9,*/*;q=0.8"
        ));
        assert!(accept_header_includes_html("text/html"));
        assert!(accept_header_includes_html("text/html; charset=utf-8"));
        assert!(accept_header_includes_html("text/html;q=0.1"));
    }

    #[test]
    fn accept_header_explicit_refusal_is_not_html() {
        // `q=0` means the client explicitly does not want HTML
        assert!(!accept_header_includes_html(
            "text/html;q=0, application/json"
        ));
        assert!(!accept_header_includes_html("text/html;q=0.0"));
    }

    #[test]
    fn accept_header_substring_is_not_html() {
        // these contain the literal substring "text/html" but are not it
        assert!(!accept_header_includes_html("application/x-text/html-fake"));
        assert!(!accept_header_includes_html("application/json"));
        assert!(!accept_header_includes_html("*/*"));
    }

    // Models a client disconnecting mid-response: the body stream is dropped
    // before it is drained. `OwnerCleanupStream::drop` must force the reactive
    // cleanup (on its origin thread) without panicking.
    #[test]
    fn owner_cleanup_stream_forces_cleanup_on_early_drop() {
        use std::sync::atomic::{AtomicBool, Ordering};

        // A context value whose own `Drop` records that it ran. Forcing the
        // owner's cleanup disposes its stored context, so this flag flips iff
        // `OwnerCleanupStream::drop` ran the reactive teardown.
        struct DropProbe(Arc<AtomicBool>);
        impl Drop for DropProbe {
            fn drop(&mut self) {
                self.0.store(true, Ordering::SeqCst);
            }
        }

        let cleaned = Arc::new(AtomicBool::new(false));
        let owner = Owner::new();
        owner.with(|| provide_context(DropProbe(cleaned.clone())));

        // Build the body but drop it WITHOUT draining the stream.
        let stream =
            OwnerCleanupStream::new(futures::stream::iter(vec!["partial".to_string()]), owner);
        drop(stream);

        assert!(
            cleaned.load(Ordering::SeqCst),
            "OwnerCleanupStream::drop should force reactive cleanup on early drop"
        );
    }

    // The thread-affinity guard: dropping the body on a thread other than the
    // one that built it must NOT run the reactive teardown (which is
    // thread-affine and would otherwise risk a panic). It must `mem::forget`
    // the owner instead — so cleanup is skipped and no panic occurs.
    #[test]
    fn owner_cleanup_stream_skips_cleanup_on_off_thread_drop() {
        use std::sync::atomic::{AtomicBool, Ordering};

        struct DropProbe(Arc<AtomicBool>);
        impl Drop for DropProbe {
            fn drop(&mut self) {
                self.0.store(true, Ordering::SeqCst);
            }
        }

        let cleaned = Arc::new(AtomicBool::new(false));
        let owner = Owner::new();
        owner.with(|| provide_context(DropProbe(cleaned.clone())));

        let stream =
            OwnerCleanupStream::new(futures::stream::iter(vec!["partial".to_string()]), owner);

        // Drop the body on a *different* thread than the one that built it.
        std::thread::spawn(move || drop(stream))
            .join()
            .expect("off-thread drop must not panic");

        // The guard must have skipped the forced cleanup (the owner is leaked
        // via `mem::forget`), so the probe is still alive.
        assert!(
            !cleaned.load(Ordering::SeqCst),
            "off-thread drop must skip reactive cleanup (leak, not cross-thread teardown)"
        );
    }
}
