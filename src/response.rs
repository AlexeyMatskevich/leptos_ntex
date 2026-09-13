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
    sync::{Arc, RwLock},
};

use leptos::context::{provide_context, use_context};
use leptos::prelude::ReadValue;
use leptos::{IntoView, PrefetchLazyFn, WasmSplitManifest};
use leptos_integration_utils::{
    BoxedFnOnce, ExtendResponse, PinnedFuture, PinnedStream, build_response,
};
use leptos_meta::{Link, ServerMetaContextOutput};

use crate::owner::{OwnerCleanup, OwnerLease, RestoreOwner, ScopedWork};
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
    /// When set, overrides the response status, except for adapter-generated
    /// WebSocket handshake and payload-limit errors.
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
///
/// Adapter-generated WebSocket handshake and payload-limit errors retain their
/// own status, representation metadata and supported-version advertisement.
/// Other supplied headers (including repeated cookies) survive when context
/// setup preceded the rejection. Preflight rejection does not set up context.
#[derive(Debug, Clone, Default)]
pub struct ResponseOptions(pub Arc<RwLock<ResponseParts>>);

impl ResponseOptions {
    /// Replaces the inner [`ResponseParts`] wholesale.
    pub fn overwrite(&self, parts: ResponseParts) {
        let mut writable = self.0.write().or_poisoned();
        *writable = parts;
    }

    /// Sets the response status, subject to the adapter-generated error
    /// exceptions described on [`ResponseOptions`].
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
        for key in parts.headers.keys() {
            let values = parts.headers.get_all(key);
            match header_merge(key) {
                HeaderMerge::ReplaceOne => {
                    // Preserve last-wins compatibility for duplicate singleton fields.
                    if let Some(value) = values.last() {
                        headers.insert(key.clone(), value.clone());
                    }
                }
                HeaderMerge::ReplaceList => {
                    // Replace the old set once; every incoming list member survives.
                    headers.remove(key);
                    for value in values {
                        headers.append(key.clone(), value.clone());
                    }
                }
                HeaderMerge::Append => {
                    for value in values {
                        headers.append(key.clone(), value.clone());
                    }
                }
            }
        }
        if let Some(status) = parts.status {
            *self.0.status_mut() = status;
        }
    }
}

/// Fields that describe the entity bytes a response carries. When the adapter
/// produces those bytes itself (a served file, an error it generated), captured
/// application values for these fields are dropped: they describe a
/// representation the response does not carry.
pub(crate) static REPRESENTATION_HEADERS: std::sync::LazyLock<[HeaderName; 10]> =
    std::sync::LazyLock::new(|| {
        [
            header::CONTENT_LENGTH,
            header::CONTENT_TYPE,
            header::CONTENT_ENCODING,
            header::CONTENT_RANGE,
            header::TRANSFER_ENCODING,
            header::ACCEPT_RANGES,
            header::ETAG,
            header::LAST_MODIFIED,
            HeaderName::from_static("content-digest"),
            HeaderName::from_static("digest"),
        ]
    });

/// Fields that identify the selected representation as a whole, dropped in
/// addition to [`REPRESENTATION_HEADERS`] when an adapter error replaces the
/// intended response: neither its language, location, digest nor handshake
/// version fields can describe a response that failed to complete.
pub(crate) static FAILED_REPRESENTATION_HEADERS: std::sync::LazyLock<[HeaderName; 4]> =
    std::sync::LazyLock::new(|| {
        [
            header::CONTENT_LANGUAGE,
            header::CONTENT_LOCATION,
            HeaderName::from_static("repr-digest"),
            header::SEC_WEBSOCKET_VERSION,
        ]
    });

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum HeaderMerge {
    ReplaceOne,
    ReplaceList,
    Append,
}

/// How a captured application header joins the framework's own: single-valued
/// fields (RFC 9110 §5.3 non-list fields) and list fields that describe the
/// representation replace the framework value; every other field, including
/// unknown ones, appends so that repeated application members survive.
pub(crate) fn header_merge(key: &HeaderName) -> HeaderMerge {
    if REPRESENTATION_HEADERS.contains(key) || FAILED_REPRESENTATION_HEADERS.contains(key) {
        return match key {
            &header::CONTENT_ENCODING
            | &header::TRANSFER_ENCODING
            | &header::ACCEPT_RANGES
            | &header::CONTENT_LANGUAGE => HeaderMerge::ReplaceList,
            _ => HeaderMerge::ReplaceOne,
        };
    }
    match key {
        &header::LOCATION
        | &header::EXPIRES
        | &header::CONTENT_DISPOSITION
        | &header::RETRY_AFTER
        | &header::AGE
        | &header::DATE
        | &header::SERVER
        | &header::STRICT_TRANSPORT_SECURITY
        | &header::X_FRAME_OPTIONS
        | &header::X_CONTENT_TYPE_OPTIONS
        | &header::REFERRER_POLICY => HeaderMerge::ReplaceOne,
        &header::CACHE_CONTROL => HeaderMerge::ReplaceList,
        _ => HeaderMerge::Append,
    }
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
            // `content_type` is a `&str`, so it may not be a valid header value
            // (e.g. an embedded NUL byte). Skip the header rather than
            // unwrapping, which would panic and take down the worker — the same
            // degrade-gracefully posture as `redirect()` below, and the same
            // fix shipped for `leptos_actix` / `leptos_axum`
            // (leptos-rs/leptos#4755). The sole in-crate caller passes a
            // literal, so this is defensive, but it removes the foot-gun for
            // any future dynamic caller.
            match HeaderValue::from_str(content_type) {
                Ok(value) => {
                    headers.insert(header::CONTENT_TYPE, value);
                }
                Err(_) => {
                    #[cfg(feature = "tracing")]
                    tracing::warn!(
                        "skipped default Content-Type: {content_type:?} is not a valid header value"
                    );
                    #[cfg(not(feature = "tracing"))]
                    eprintln!(
                        "skipped default Content-Type: {content_type:?} is not a valid header value"
                    );
                }
            }
        }
    }

    // Local override of `ExtendResponse::from_app` from
    // `leptos_integration_utils` 0.8.8. Its trailing cleanup item misses
    // cancellation. An integration guard is armed before the first await,
    // survives response construction, and outlives the body's destructor.
    //
    // Remove this copy only when a released upstream implementation provides
    // these guarantees for cancellation before headers and during the body,
    // including retained Owner handles and thread-affine destruction.
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

            let (owner, stream) = {
                let _restore = RestoreOwner::capture();
                build_response(app_fn, additional_context, stream_builder, supports_ooo)
            };
            // Arm cleanup before constructing the scoped future. Its scope
            // restores the request Owner for setup, deferred work and metadata,
            // and keeps its arena active through cancellation destructors.
            let owner = OwnerCleanup::new(owner);
            ScopedWork::with_owner(owner.owner().clone(), async move {
                let (owner, stream) = (owner, stream);

                provide_context(prefetches.clone());

                let sc = owner.owner().shared_context().unwrap();

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
                // `Drop` (via `OwnerLease`) instead of a trailing stream
                // item, so it also runs on early client disconnect.
                let mut res = Self::from_stream(OwnerLease::new(
                    once(async move { first_chunk }).chain(stream),
                    Arc::new(owner),
                ));

                res.extend_response(&res_options);

                // Set the Content Type headers on all responses. This makes Firefox show the page
                // source without complaining
                res.set_default_content_type("text/html; charset=utf-8");

                res
            }).await
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
        // Honour an explicit `q=0`, which means the client refuses HTML. A
        // MALFORMED `q` (non-numeric, negative, > 1, `NaN`/`inf`) is not a
        // valid refusal, so it defaults to accepting — the same
        // "malformed → field default" policy the `Accept-Encoding` parser
        // applies, via the shared `parse_qvalue`.
        match media.get_param("q") {
            Some(q) => crate::config::parse_qvalue(q.as_str())
                .map(|w| w > 0.0)
                .unwrap_or(true),
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
        let accepts_html = req.with(|http| {
            http.headers()
                .get(header::ACCEPT)
                .and_then(|v| v.to_str().ok())
                .map(accept_header_includes_html)
                .unwrap_or(false)
        });
        let Ok(accepts_html) = accepts_html else {
            #[cfg(feature = "tracing")]
            tracing::warn!("redirect() could not access the current request scope");
            #[cfg(not(feature = "tracing"))]
            eprintln!("redirect() could not access the current request scope");
            return;
        };
        res.insert_header(header::LOCATION, location);

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
    use leptos::prelude::Owner;
    use lets_expect::lets_expect;

    // ----- HTML-navigation heuristic: selected characteristic states ----
    // Domain-walk of a type-poor `&str -> bool`: the media range parses or
    // not, its type/subtype is text/html or not, the `q` quality is
    // absent / positive / zero / unparseable, and HTML may appear among
    // several comma-separated ranges. Every leaf below is the behaviour
    // derived from this helper's navigation contract, not full content negotiation.
    lets_expect! {
        expect(accept_header_includes_html(accept)) as html_acceptance {
            let accept = "text/html";

            to accepts_a_plain_html_range { be_true }

            when the_html_range_carries_a_charset {
                let accept = "text/html; charset=utf-8";
                to accepts_html { be_true }
            }

            when the_html_range_has_a_positive_quality {
                let accept = "text/html;q=0.1";
                to accepts_html { be_true }
            }

            when the_html_quality_is_zero {
                let accept = "text/html;q=0";
                to does_not_accept_html { be_false }

                when the_zero_quality_is_written_as_a_decimal {
                    let accept = "text/html;q=0.0";
                    to does_not_accept_html { be_false }
                }
            }

            when the_quality_value_is_unparseable {
                let accept = "text/html;q=not-a-number";
                to treats_an_unparseable_quality_as_accepting { be_true }
            }

            when html_appears_among_several_ranges {
                let accept = "text/html,application/xhtml+xml,application/xml;q=0.9,*/*;q=0.8";
                to accepts_html { be_true }
            }

            when html_is_refused_and_only_non_html_remains {
                let accept = "text/html;q=0, application/json";
                to does_not_accept_html { be_false }
            }

            when the_range_is_a_different_media_type {
                let accept = "application/json";
                to does_not_accept_html { be_false }
            }

            // type == text, subtype != html: only the SUBTYPE guard fires.
            // The `application/json` case above has BOTH type and subtype
            // differ, so it cannot tell `type != text || subtype != html`
            // apart from `&&` — this text/non-html range is the one that can.
            when the_range_is_text_but_not_html {
                let accept = "text/plain";
                to does_not_accept_html { be_false }
            }

            when the_value_only_contains_an_html_substring {
                let accept = "application/x-text/html-fake";
                to does_not_accept_html { be_false }
            }

            when the_range_is_a_wildcard {
                let accept = "*/*";
                to does_not_accept_html { be_false }
            }

            when the_html_range_uses_uppercase_letters {
                let accept = "TEXT/HTML";
                to accepts_html_case_insensitively { be_true }
            }

            when html_is_not_the_first_range {
                let accept = "application/json, text/html";
                to scans_every_range_and_accepts_html { be_true }
            }

            when a_refused_html_range_precedes_an_accepted_one {
                let accept = "text/html;q=0, text/html";
                to keeps_scanning_past_the_refused_range { be_true }
            }

            when a_trailing_comma_leaves_an_empty_range {
                let accept = "text/html,";
                to ignores_the_empty_range_and_accepts_html { be_true }
            }

            // A negative `q` is outside the RFC 0..=1 range, i.e. MALFORMED —
            // it is not a valid `q=0` refusal, so it defaults to accepting
            // (the shared `parse_qvalue` policy), exactly like the unparseable
            // case above. The out-of-range-high, finite-max, and non-finite
            // siblings pin the rest of that boundary.
            when the_quality_is_negative {
                let accept = "text/html;q=-0.5";
                to treats_an_out_of_range_quality_as_accepting { be_true }
            }

            when the_quality_is_above_the_maximum {
                let accept = "text/html;q=2";
                to treats_an_out_of_range_quality_as_accepting { be_true }
            }

            when the_quality_is_exactly_the_maximum {
                let accept = "text/html;q=1";
                to accepts_html { be_true }
            }

            when the_quality_is_not_a_finite_number {
                let accept = "text/html;q=nan";
                to treats_a_non_finite_quality_as_accepting { be_true }
            }

            when the_accept_header_is_empty {
                let accept = "";
                to does_not_accept_html { be_false }
            }

            when the_accept_header_is_only_whitespace {
                let accept = "   ";
                to does_not_accept_html { be_false }
            }
        }
    }

    enum DropSite {
        OriginThread,
        OffOriginThread,
    }

    fn reactive_cleanup_runs_when_dropped_at(site: DropSite) -> usize {
        use leptos::prelude::{Owner, on_cleanup};
        use std::sync::atomic::{AtomicUsize, Ordering};
        let cleaned = Arc::new(AtomicUsize::new(0));
        let owner = Owner::new();
        let retained = owner.clone();
        let counter = cleaned.clone();
        owner.with(|| {
            on_cleanup(move || {
                counter.fetch_add(1, Ordering::SeqCst);
            })
        });
        let stream = OwnerLease::new(
            futures::stream::iter(vec!["partial".to_string()]),
            Arc::new(OwnerCleanup::new(owner)),
        );
        match site {
            DropSite::OriginThread => drop(stream),
            DropSite::OffOriginThread => std::thread::spawn(move || drop(stream)).join().unwrap(),
        }
        let observed = cleaned.load(Ordering::SeqCst);
        retained.unset_with_forced_cleanup();
        observed
    }

    lets_expect! {
        expect(reactive_cleanup_runs_when_dropped_at(drop_site)) as body_cleanup_thread_affinity {
            let drop_site = DropSite::OriginThread;
            to forces_cleanup_despite_a_retained_owner { equal(1_usize) }
            when the_body_is_dropped_off_its_origin_thread {
                let drop_site = DropSite::OffOriginThread;
                to preserves_the_documented_off_thread_fallback { equal(0_usize) }
            }
        }
    }

    // ----- set_default_content_type: set valid, skip invalid ------------
    // The default content type is applied only when none is set yet, from a
    // `&str` that may not be a valid header value. A valid value is inserted;
    // an invalid one (embedded NUL) must be skipped, NOT unwrapped — the
    // unwrap would panic the worker. Observe the resulting CONTENT_TYPE.
    // The invalid value's NUL is built at runtime here rather than written as
    // a `"\0"` literal in the `lets_expect!` body below: a NUL token in the
    // macro input trips rust-analyzer's proc-macro server (real rustc compiles
    // it fine), so keeping it out preserves the IDE experience.
    fn content_type_after_default(preset: Option<&str>, valid: bool) -> Option<String> {
        let mut builder = HttpResponse::Ok();
        if let Some(content_type) = preset {
            builder.content_type(content_type);
        }
        let mut res = NtexResponse(builder.finish());
        let value = if valid {
            "text/html; charset=utf-8".to_string()
        } else {
            format!("text/html{}bad", '\0')
        };
        res.set_default_content_type(&value);
        res.0
            .headers()
            .get(header::CONTENT_TYPE)
            .and_then(|v| v.to_str().ok())
            .map(str::to_owned)
    }

    lets_expect! {
        expect(content_type_after_default(preset, valid)) as default_content_type {
            // default: nothing set yet, a valid value -> it is applied
            let preset: Option<&str> = None;
            let valid = true;

            to sets_the_default_when_none_is_present {
                equal(Some("text/html; charset=utf-8".to_string()))
            }

            when the_value_is_not_a_valid_header_value {
                let valid = false;
                to skips_the_header_instead_of_panicking { be_none }
            }

            // The guard short-circuits before parsing, so an app-set content
            // type must survive regardless of the default's validity — the
            // method only fills in a *missing* one.
            when a_content_type_is_already_present {
                let preset = Some("application/json");
                to leaves_the_existing_content_type_unchanged {
                    equal(Some("application/json".to_string()))
                }
            }

            // Boundary of the `preset: Option<&str>` domain: an EMPTY-but-PRESENT
            // header value. `headers.contains_key` is true for `Some("")`, so the
            // guard must still treat it as "already present" and leave it alone —
            // pinning that the check is presence-based, not non-empty-based.
            when the_existing_content_type_is_an_empty_string {
                let preset = Some("");
                to leaves_the_empty_content_type_unchanged {
                    equal(Some("".to_string()))
                }
            }
        }
    }

    // ----- redirect(): reactive-context presence and Accept-header specs ---
    // `redirect()` has three characteristics of its own, independent of
    // `accept_header_includes_html`'s parser spec above (which covers
    // the STRING PARSING of an Accept value once one is in hand):
    //   1. reactive-context presence: both Request and ResponseOptions
    //      present (the only state that can do anything) vs. either/both
    //      missing (the `else` branch: warn and no-op);
    //   2. Accept-header presence: present-and-parseable vs. ABSENT
    //      (`headers().get(..)` is `None`) vs. present-but-NOT-valid-UTF-8
    //      (`to_str()` is `Err`) — both of the latter must fall through to
    //      `unwrap_or(false)`, i.e. "does not accept html", so a client
    //      redirect header is emitted rather than a raw 302;
    //   3. within "context present", whether the resolved accepts-html verdict
    //      is true or false, driving 302-vs-REDIRECT_HEADER.
    // The oracle is the full observable outcome (Location + status +
    // REDIRECT_HEADER presence), not just one field, so a wrong `Location`
    // that happens to pick the same status can't slip through.

    /// Which of the two reactive-context values (`Request`, `ResponseOptions`)
    /// are provided to the `Owner` running `redirect()`.
    enum ContextSetup {
        Both,
        OnlyResponseOptions,
    }

    /// The observable outcome of one `redirect()` call: the `Location` header
    /// value (if any), the overridden status (if any), and whether the
    /// client-redirect header was set. `None`/`false` for all three is the
    /// "no-op" outcome of the missing-context branch.
    #[derive(Debug, PartialEq)]
    struct RedirectOutcome {
        location: Option<String>,
        status: Option<StatusCode>,
        redirect_header_present: bool,
    }

    /// Executes redirect against the actual ResponseOptions supplied to context.
    /// Missing-ResponseOptions behavior is a separate completion contract below.
    fn redirect_outcome(ctx: ContextSetup, accept: Option<&HeaderValue>) -> RedirectOutcome {
        let _scope = crate::RequestScope::new();
        let mut req_builder = ntex::web::test::TestRequest::with_uri("/");
        if let Some(accept) = accept {
            req_builder = req_builder.header(header::ACCEPT, accept.clone());
        }
        let http_req = req_builder.to_http_request();

        let owner = Owner::new();
        let res_options = ResponseOptions::default();
        owner.with(|| {
            match ctx {
                ContextSetup::Both => {
                    provide_context(crate::request::Request::new(&http_req));
                    provide_context(res_options.clone());
                }
                ContextSetup::OnlyResponseOptions => {
                    provide_context(res_options.clone());
                }
            }

            redirect("/target");
        });

        let parts = res_options.0.read().or_poisoned();
        RedirectOutcome {
            location: parts
                .headers
                .get(header::LOCATION)
                .and_then(|v| v.to_str().ok())
                .map(str::to_owned),
            status: parts.status,
            redirect_header_present: parts
                .headers
                .contains_key(HeaderName::from_static(REDIRECT_HEADER)),
        }
    }

    lets_expect! {
        expect(redirect_outcome(ctx, accept.as_ref())) as contextual_redirect {
            // Default: both contexts present, Accept accepts html -> 302 + Location.
            let ctx = ContextSetup::Both;
            let accept: Option<HeaderValue> = Some(HeaderValue::from_static("text/html"));

            to sets_location_and_a_302_for_an_html_accepting_client {
                equal(RedirectOutcome {
                    location: Some("/target".to_string()),
                    status: Some(StatusCode::FOUND),
                    redirect_header_present: false,
                })
            }

            when the_client_does_not_accept_html {
                let accept: Option<HeaderValue> = Some(HeaderValue::from_static("application/json"));
                to sets_location_and_the_client_redirect_header_instead_of_a_302 {
                    equal(RedirectOutcome {
                        location: Some("/target".to_string()),
                        status: None,
                        redirect_header_present: true,
                    })
                }
            }

            // `headers().get(ACCEPT)` is `None` — the `Option` chain's absent
            // state, distinct from a present-but-unparseable value below.
            // Must fall through the SAME `unwrap_or(false)` as a rejected
            // Accept header, i.e. NOT accept html.
            when the_accept_header_is_absent {
                let accept: Option<HeaderValue> = None;
                to treats_a_missing_accept_header_as_not_accepting_html {
                    equal(RedirectOutcome {
                        location: Some("/target".to_string()),
                        status: None,
                        redirect_header_present: true,
                    })
                }
            }

            // Present, but its bytes are not valid UTF-8, so `to_str()` is
            // `Err` and `.ok()` collapses it to `None` — same fallthrough as
            // "absent" above, exercised via the OTHER branch of the `Option`
            // chain (`Some(header) -> to_str().ok() == None`, rather than
            // `headers().get(..) == None`). A byte `>= 0x80` is a VALID
            // header-value byte (only control bytes are rejected) but is
            // never valid UTF-8 on its own, so `HeaderValue::from_bytes`
            // succeeds while `to_str()` fails — this is the only way to
            // reach that branch without panicking.
            when the_accept_header_is_present_but_not_valid_utf8 {
                let accept: Option<HeaderValue> =
                    Some(HeaderValue::from_bytes(&[0xFF, 0xFE]).unwrap());
                to treats_a_non_utf8_accept_header_as_not_accepting_html {
                    equal(RedirectOutcome {
                        location: Some("/target".to_string()),
                        status: None,
                        redirect_header_present: true,
                    })
                }
            }

            // Reactive-context-presence axis: `redirect()` requires BOTH a
            // `Request` and a `ResponseOptions` in context; any state short of
            // "both" takes the `else` (warn-and-no-op) branch. Each state
            // below observes the actual supplied ResponseOptions.


            when the_reactive_context_is_missing_the_request {
                let ctx = ContextSetup::OnlyResponseOptions;
                to performs_no_side_effect {
                    equal(RedirectOutcome {
                        location: None,
                        status: None,
                        redirect_header_present: false,
                    })
                }
            }


        }
    }
    fn redirect_without_response_context(request_present: bool) {
        let _scope = crate::RequestScope::new();
        let owner = Owner::new();
        owner.with(|| {
            if request_present {
                let request = ntex::web::test::TestRequest::get().to_http_request();
                provide_context(crate::request::Request::new(&request));
            }
            redirect("/target");
        });
    }
    lets_expect! {
        expect(redirect_without_response_context(request_present)) as redirect_without_response_options {
            let request_present = true;
            to completes_without_a_response_context { equal(()) }
            when the_request_is_also_absent {
                let request_present = false;
                to completes_without_any_request_context { equal(()) }
            }
        }
    }
}

#[cfg(test)]
mod header_ownership_specs {
    use super::*;
    use lets_expect::*;

    /// Every field the adapter strips from its own representations is also a
    /// field whose captured value replaces the framework's; a header added to
    /// one list without the other would surface here.
    fn appended_owned_headers() -> Vec<String> {
        REPRESENTATION_HEADERS
            .iter()
            .chain(FAILED_REPRESENTATION_HEADERS.iter())
            .filter(|name| header_merge(name) == HeaderMerge::Append)
            .map(|name| name.as_str().to_owned())
            .collect()
    }

    lets_expect! {
        expect(appended_owned_headers()) as adapter_owned_headers {
            to never_append_to_the_frameworks_value { equal(Vec::<String>::new()) }
        }
    }
}
