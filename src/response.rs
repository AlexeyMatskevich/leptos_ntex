//! HTTP response types and the [`redirect`] helper.
//!
//! Hosts the response side of the integration: the [`ResponseOptions`]
//! override used from inside components/server-fns, the boxed streaming
//! HTML body type, the `ExtendResponse` adapter for ntex, and the
//! public [`redirect`] helper.

use futures::{Stream, StreamExt};
use ntex::http::{
    StatusCode,
    header::{self, HeaderName, HeaderValue},
};
use ntex::util::Bytes as NBytes;
use ntex::web::HttpResponse;
use or_poisoned::OrPoisoned;
use server_fn::redirect::REDIRECT_HEADER;
use std::{
    io,
    sync::{Arc, RwLock},
};

use leptos::context::use_context;
use leptos_integration_utils::ExtendResponse;

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
        #[cfg(not(feature = "tracing"))]
        eprintln!("Couldn't retrieve either Parts or ResponseOptions while trying to redirect().");
    }
}
