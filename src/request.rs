//! The [`Request`] wrapper that makes an ntex [`HttpRequest`] storable in
//! Leptos's `Send`/`Sync` reactive context.

use ntex::web::HttpRequest;
use send_wrapper::SendWrapper;

/// A wrapper for an ntex [`HttpRequest`] that can be placed in Leptos's
/// `Send`/`Sync` context API.
///
/// ntex's [`HttpRequest`] is not [`Send`], so it is wrapped in a
/// [`SendWrapper`]. The wrapper panics if dropped on another thread — which
/// can happen during static-route prerendering, where Leptos owners may be
/// moved across thread boundaries.
///
/// ### Cross-thread drop: leak vs. panic
///
/// The [`Drop`] implementation checks [`SendWrapper::valid`] and, when the
/// drop is happening off-thread, calls [`std::mem::forget`] on the inner
/// wrapper instead of panicking. ntex's `HttpRequest` is internally
/// reference-counted, so forgetting the wrapper leaks one `Rc` increment
/// worth of state. This is deliberately preferred over panicking (which
/// would tear down the arbiter).
///
/// The leak size is bounded by the number of `Request` instances whose
/// owning reactive context gets moved across threads before drop — this
/// includes static-route prerendering at SSG time and any user-introduced
/// `spawn_blocking` / cross-thread `Owner` pattern. For in-request SSR on
/// a single arbiter the wrapper always drops on its creator thread and no
/// leak occurs.
///
/// ### Cross-thread clone, deref, into_inner
///
/// The [`Clone`], [`Deref`](std::ops::Deref), [`DerefMut`](std::ops::DerefMut)
/// and [`Request::into_inner`] operations dereference the inner
/// [`SendWrapper`]. If called from a thread other than the one on which
/// the `Request` was constructed they will panic (same invariant as any
/// [`SendWrapper`]). These paths do **not** have the leak-instead-of-panic
/// safety net — only [`Drop`] does. In practice you only hit this when you
/// explicitly move a `Request` onto a different worker and try to read it
/// there, which should be avoided.
#[derive(Debug)]
pub struct Request(Option<SendWrapper<HttpRequest>>);

impl Request {
    /// Wraps an existing ntex request.
    pub fn new(req: &HttpRequest) -> Self {
        Self(Some(SendWrapper::new(req.clone())))
    }

    /// Consumes the wrapper and returns the inner ntex request.
    pub fn into_inner(mut self) -> HttpRequest {
        self.0
            .take()
            .expect("Request should always contain an HttpRequest")
            .take()
    }
}

impl Clone for Request {
    fn clone(&self) -> Self {
        Self::new(self)
    }
}

impl std::ops::Deref for Request {
    type Target = HttpRequest;

    fn deref(&self) -> &Self::Target {
        self.0
            .as_ref()
            .expect("Request should always contain an HttpRequest")
    }
}

impl std::ops::DerefMut for Request {
    fn deref_mut(&mut self) -> &mut Self::Target {
        self.0
            .as_mut()
            .expect("Request should always contain an HttpRequest")
    }
}

impl Drop for Request {
    fn drop(&mut self) {
        if let Some(req) = self.0.take() {
            if req.valid() {
                drop(req);
            } else {
                std::mem::forget(req);
            }
        }
    }
}
