//! Ntex-backed [`server_fn::response`] types.

use bytes::Bytes as SfBytes;
use futures::{Stream, StreamExt};
use ntex::http::{
    StatusCode,
    header::{self, HeaderValue},
};
use ntex::util::Bytes as NBytes;
use ntex::web::HttpResponse;
use send_wrapper::SendWrapper;
use server_fn::{
    error::FromServerFnError,
    response::{Res, TryRes},
};
use std::io;

/// Wraps an ntex [`HttpResponse`] for use as a server function output.
///
/// Implements [`server_fn::response::TryRes`] and [`server_fn::response::Res`]
/// so the server-function runtime can build responses of the expected
/// content-type. ntex's [`HttpResponse`] is not [`Send`], hence the
/// [`SendWrapper`].
pub struct NtexServerResponse(pub SendWrapper<HttpResponse>);

impl NtexServerResponse {
    /// Consumes the wrapper and returns the inner ntex response.
    pub fn take(self) -> HttpResponse {
        self.0.take()
    }
}

impl From<HttpResponse> for NtexServerResponse {
    fn from(value: HttpResponse) -> Self {
        Self(SendWrapper::new(value))
    }
}

impl<E> TryRes<E> for NtexServerResponse
where
    E: FromServerFnError,
{
    fn try_from_string(content_type: &str, data: String) -> Result<Self, E> {
        Ok(Self(SendWrapper::new(
            HttpResponse::Ok().content_type(content_type).body(data),
        )))
    }

    fn try_from_bytes(content_type: &str, data: SfBytes) -> Result<Self, E> {
        Ok(Self(SendWrapper::new(
            HttpResponse::Ok()
                .content_type(content_type)
                .body(NBytes::copy_from_slice(&data)),
        )))
    }

    fn try_from_stream(
        content_type: &str,
        data: impl Stream<Item = Result<SfBytes, SfBytes>> + Send + 'static,
    ) -> Result<Self, E> {
        let pinned = Box::pin(data.map(|data| {
            data.map(|b| NBytes::copy_from_slice(&b))
                .map_err(|e| io::Error::other(String::from_utf8_lossy(&e).to_string()))
        }));
        Ok(Self(SendWrapper::new(
            HttpResponse::Ok()
                .content_type(content_type)
                .streaming(pinned),
        )))
    }
}

impl Res for NtexServerResponse {
    fn error_response(path: &str, err: SfBytes) -> Self {
        Self(SendWrapper::new(
            HttpResponse::InternalServerError()
                .header(server_fn::error::SERVER_FN_ERROR_HEADER, path)
                .body(NBytes::copy_from_slice(&err)),
        ))
    }

    fn content_type(&mut self, content_type: &str) {
        if let Ok(content_type) = HeaderValue::from_str(content_type) {
            self.0
                .headers_mut()
                .insert(header::CONTENT_TYPE, content_type);
        }
    }

    fn redirect(&mut self, path: &str) {
        if let Ok(path) = HeaderValue::from_str(path) {
            *self.0.status_mut() = StatusCode::FOUND;
            self.0.headers_mut().insert(header::LOCATION, path);
        }
    }
}
