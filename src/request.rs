//! Managed, thread-affine access to ntex requests stored in Leptos context.

use futures::task::AtomicWaker;
use ntex::web::HttpRequest;
use std::{
    cell::{Cell, RefCell},
    collections::HashMap,
    fmt,
    future::Future,
    pin::Pin,
    rc::{Rc, Weak},
    sync::{
        Arc, Weak as SyncWeak,
        atomic::{AtomicBool, AtomicUsize, Ordering},
    },
    task::{Context, Poll},
    thread::{self, ThreadId},
};

thread_local! {
    // Weak entries never own native requests during TLS destruction.
    static SCOPES: RefCell<Vec<Weak<Registry>>> = const { RefCell::new(Vec::new()) };
}
static NEXT_SCOPE: AtomicUsize = AtomicUsize::new(1);

/// A request cannot be created or accessed in the current execution scope.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub enum RequestAccessError {
    /// No live [`RequestScope`] exists on this thread.
    MissingScope,
    /// Native access was attempted away from the request's origin thread.
    WrongThread,
    /// The native request's scope has closed.
    Closed,
}

impl fmt::Display for RequestAccessError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::MissingScope => "no active RequestScope on this thread",
            Self::WrongThread => "request access requires its origin thread",
            Self::Closed => "request scope is closed",
        })
    }
}
impl std::error::Error for RequestAccessError {}

/// A transferable handle to an ntex request owned by a local [`RequestScope`].
///
/// Cloning or dropping this handle is safe on any thread. Native access through
/// [`Self::with`] and [`Self::try_into_inner`] requires the origin thread and a
/// live scope. Closing the scope invalidates handles that remain elsewhere.
/// No native request is stored in the transferable handle.
///
/// Unlike a `Deref` implementation, the callback confines borrowed data to one
/// synchronous operation. Return owned values when data must outlive it.
///
/// ```compile_fail
/// # use leptos_ntex_unofficial::Request;
/// fn escaped(request: &Request) -> &str {
///     request.with(|native| native.path()).unwrap()
/// }
/// ```
///
/// The callback may return an owned `HttpRequest::clone()`. That native handle
/// belongs to the application: it remains thread-affine and can keep its graph
/// alive after this scope closes. The same applies to [`Self::into_inner`].
#[derive(Clone)]
pub struct Request(Arc<Token>);

impl Request {
    /// Registers a clone of a native request in the current local scope.
    ///
    /// # Panics
    /// Panics if this thread has no active [`RequestScope`]. Use
    /// [`Self::try_new`] when absence of a scope is an expected outcome.
    pub fn new(req: &HttpRequest) -> Self {
        Self::try_new(req).expect("Request::new requires an active RequestScope")
    }

    /// Registers a clone of a native request in the current local scope.
    pub fn try_new(req: &HttpRequest) -> Result<Self, RequestAccessError> {
        let registry = current_registry().ok_or(RequestAccessError::MissingScope)?;
        let id = registry.next.get();
        registry
            .next
            .set(id.checked_add(1).expect("request identifiers exhausted"));
        let token = Arc::new(Token {
            scope: registry.id,
            id,
            origin: thread::current().id(),
            wake: registry.wake.clone(),
        });
        registry.entries.borrow_mut().insert(
            id,
            Entry {
                native: Rc::new(req.clone()),
                token: Arc::downgrade(&token),
            },
        );
        Ok(Self(token))
    }

    /// Reads the native request synchronously on its origin thread.
    ///
    /// No registry borrow is held while the callback runs. Nested reads are
    /// allowed. If it closes the scope, this invocation pins its native handle
    /// until the callback returns or unwinds; subsequent access reports Closed.
    pub fn with<R>(
        &self,
        work: impl for<'a> FnOnce(&'a HttpRequest) -> R,
    ) -> Result<R, RequestAccessError> {
        if self.0.origin != thread::current().id() {
            return Err(RequestAccessError::WrongThread);
        }
        let registry = registry(self.0.scope).ok_or(RequestAccessError::Closed)?;
        let native = registry
            .entries
            .borrow()
            .get(&self.0.id)
            .map(|entry| entry.native.clone())
            .ok_or(RequestAccessError::Closed)?;
        Ok(work(&native))
    }

    /// Consumes this handle and returns an owned native clone.
    /// Other Request clones keep their own access until the scope closes.
    /// The returned native request must be used and destroyed on its origin.
    pub fn try_into_inner(self) -> Result<HttpRequest, RequestAccessError> {
        self.with(HttpRequest::clone)
    }

    /// Consumes this handle and returns an owned native clone.
    ///
    /// # Panics
    /// Panics on another thread or after the scope has closed. Use
    /// [`Self::try_into_inner`] for fallible access.
    pub fn into_inner(self) -> HttpRequest {
        self.try_into_inner()
            .expect("Request::into_inner requires its live origin scope")
    }
}

impl fmt::Debug for Request {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Request")
            .field("scope", &self.0.scope)
            .field("origin", &self.0.origin)
            .finish_non_exhaustive()
    }
}

struct Token {
    scope: usize,
    id: usize,
    origin: ThreadId,
    wake: Arc<RetirementWake>,
}
impl Drop for Token {
    fn drop(&mut self) {
        if self.origin == thread::current().id() {
            if let Some(registry) = registry(self.scope) {
                let entry = registry.entries.borrow_mut().remove(&self.id);
                drop_entries(entry);
            }
        } else {
            // AtomicWaker consumes its registered waker, coalescing concurrent
            // retirements until the next root poll registers again.
            self.wake.dirty.store(true, Ordering::Release);
            self.wake.waker.wake();
        }
    }
}
struct RetirementWake {
    waker: AtomicWaker,
    dirty: AtomicBool,
}
struct Entry {
    native: Rc<HttpRequest>,
    token: SyncWeak<Token>,
}
struct Registry {
    id: usize,
    next: Cell<usize>,
    closed: Cell<bool>,
    entries: RefCell<HashMap<usize, Entry>>,
    wake: Arc<RetirementWake>,
}
impl Registry {
    fn collect(&self) -> usize {
        let entries = {
            let mut live = self.entries.borrow_mut();
            let retired: Vec<_> = live
                .iter()
                .filter_map(|(id, entry)| (entry.token.strong_count() == 0).then_some(*id))
                .collect();
            retired
                .into_iter()
                .filter_map(|id| live.remove(&id))
                .collect::<Vec<_>>()
        };
        let count = entries.len();
        drop_entries(entries);
        count
    }
}

fn current_registry() -> Option<Rc<Registry>> {
    SCOPES
        .try_with(|scopes| {
            scopes
                .borrow()
                .iter()
                .rev()
                .filter_map(Weak::upgrade)
                .find(|scope| !scope.closed.get())
        })
        .ok()
        .flatten()
}
fn registry(id: usize) -> Option<Rc<Registry>> {
    SCOPES
        .try_with(|scopes| {
            scopes
                .borrow()
                .iter()
                .filter_map(Weak::upgrade)
                .find(|scope| scope.id == id && !scope.closed.get())
        })
        .ok()
        .flatten()
}

// Native destructors run after all our borrows have ended. Complete the selected
// cleanup batch before propagating its first panic; avoid a second panic while
// unwinding. Arbitrarily panicking panic-payload destructors remain user code.
fn drop_entries(entries: impl IntoIterator<Item = Entry>) {
    let unwinding = thread::panicking();
    let mut first_panic = None;
    for entry in entries {
        if let Err(payload) = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| drop(entry)))
            && first_panic.is_none()
        {
            first_panic = Some(payload);
        }
    }
    if !unwinding && let Some(payload) = first_panic {
        std::panic::resume_unwind(payload);
    }
}

/// Registers the request for an adapter handler, or produces the `500` that
/// every handler returns in place of a panic when no [`RequestScope`] is
/// active on this worker. The cause is reported once per process.
pub(crate) fn scoped_request(req: &HttpRequest) -> Result<Request, ntex::web::HttpResponse> {
    Request::try_new(req).map_err(|error| {
        report_missing_scope(&error);
        ntex::web::HttpResponse::InternalServerError().finish()
    })
}

/// Reports the runtime misconfiguration behind a [`RequestAccessError`] once.
pub(crate) fn report_missing_scope(error: &RequestAccessError) {
    static REPORTED: std::sync::Once = std::sync::Once::new();
    REPORTED.call_once(|| {
        let message = format!(
            "leptos_ntex cannot serve requests: {error}. Build the ntex System with \
             `RequestRuntime::new(DefaultRuntime)` (see the README quick start) or open a \
             `RequestScope` around manual embedding; affected requests are answered with 500"
        );
        #[cfg(feature = "tracing")]
        tracing::error!("{message}");
        #[cfg(not(feature = "tracing"))]
        eprintln!("{message}");
    });
}

/// Startup diagnostic for handler registration, which ntex performs on the
/// same worker threads that later serve requests: a missing scope here means
/// every request would fail, so say so before the first one arrives.
pub(crate) fn check_registration_scope() {
    if current_registry().is_none() {
        report_missing_scope(&RequestAccessError::MissingScope);
    }
}

/// Owns native requests for one explicitly bounded origin-thread lifetime.
///
/// A scope is thread-wide, not task-local. Keep a manual guard around an entire
/// execution boundary (such as `run_local`), not an individual request future
/// that can yield while other tasks use the same thread.
///
/// Create this guard outside the work that may retain Request handles, and
/// destroy it on that thread before thread-local teardown. Nested scopes select
/// the most recently created live scope; closing a parent first is supported.
/// Closing releases this scope's native handles even if transferable Request
/// handles remain elsewhere. An executing callback or application-owned native
/// clone can keep its own handle alive longer.
///
/// Origin-thread final token drops retire immediately. In a manual scope,
/// foreign-thread retirements are reclaimed by [`Self::collect`] or scope Drop.
/// [`RequestRuntime`] also collects them when the runtime's root future is polled.
/// Cleanup invokes native destructors outside this crate's locks/borrows; it
/// does not change reentrancy restrictions inside ntex or user extensions.
/// It completes the cleanup batch before resuming its first destructor panic,
/// and suppresses a second destructor panic during an existing unwind.
///
/// ```compile_fail
/// # use leptos_ntex_unofficial::RequestScope;
/// fn requires_send<T: Send>() {}
/// requires_send::<RequestScope>();
/// ```
#[must_use = "keep the scope alive while its Request handles need native access"]
pub struct RequestScope {
    registry: Rc<Registry>,
}
impl RequestScope {
    /// Opens a local scope, making it current until it is closed.
    pub fn new() -> Self {
        let id = NEXT_SCOPE
            .fetch_update(Ordering::Relaxed, Ordering::Relaxed, |id| id.checked_add(1))
            .expect("request scope identifiers exhausted");
        let registry = Rc::new(Registry {
            id,
            next: Cell::new(0),
            closed: Cell::new(false),
            entries: RefCell::new(HashMap::new()),
            wake: Arc::new(RetirementWake {
                waker: AtomicWaker::new(),
                dirty: AtomicBool::new(false),
            }),
        });
        SCOPES.with(|scopes| {
            let mut scopes = scopes.borrow_mut();
            scopes.retain(|scope| scope.strong_count() != 0);
            scopes.push(Rc::downgrade(&registry));
        });
        Self { registry }
    }

    /// Reclaims entries whose final Request token was dropped on another thread.
    /// Returns the number of registry entries removed. Native clones owned by
    /// callers or callbacks may delay destruction of the underlying graph.
    pub fn collect(&self) -> usize {
        self.registry.collect()
    }
}
impl Default for RequestScope {
    fn default() -> Self {
        Self::new()
    }
}
impl fmt::Debug for RequestScope {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("RequestScope")
            .field("id", &self.registry.id)
            .finish_non_exhaustive()
    }
}
impl Drop for RequestScope {
    fn drop(&mut self) {
        self.registry.closed.set(true);
        let _ = SCOPES.try_with(|scopes| {
            scopes
                .borrow_mut()
                .retain(|scope| scope.as_ptr() != Rc::as_ptr(&self.registry))
        });
        self.registry.wake.waker.take();
        let entries = std::mem::take(&mut *self.registry.entries.borrow_mut());
        drop_entries(entries.into_values());
    }
}

/// Adds an outer [`RequestScope`] to an existing ntex runtime backend.
///
/// Pass this Runner to `ntex::rt::System::build().build(...)`. ntex uses the
/// configured Runner on the main runtime and on newly created arbiters/workers.
/// Wrapping `ntex::rt::DefaultRuntime` preserves its selected Neon/Tokio backend;
/// this wrapper does not install or replace the Leptos executor.
///
/// The scope closes after the inner Runner returns or unwinds, before normal
/// thread-local teardown. This guarantees origin-thread cleanup, not an active
/// inner runtime during arbitrary native destructors. `SystemRunner::run_local`
/// bypasses its Runner: use a manual RequestScope around that execution instead.
#[derive(Clone, Copy, Debug)]
pub struct RequestRuntime<R> {
    inner: R,
}
impl<R> RequestRuntime<R> {
    /// Wraps an existing Runner without changing its backend.
    pub const fn new(inner: R) -> Self {
        Self { inner }
    }
}
// The scope boundary is identical for both ntex-rt Runner return shapes.
macro_rules! impl_request_runner {
    ($($output:tt)*) => {
        impl<R: ntex::rt::Runner> ntex::rt::Runner for RequestRuntime<R> {
            fn block_on(&self, future: ntex::rt::BlockFuture) $($output)* {
                let scope = RequestScope::new();
                self.inner.block_on(Box::pin(CollectingFuture {
                    future,
                    registry: Rc::downgrade(&scope.registry),
                }))
            }
        }
    };
}
#[cfg(not(ntex_runner_returns_result))]
impl_request_runner!();
#[cfg(ntex_runner_returns_result)]
impl_request_runner!(-> std::thread::Result<()>);

struct CollectingFuture {
    future: ntex::rt::BlockFuture,
    registry: Weak<Registry>,
}
impl Future for CollectingFuture {
    type Output = ();
    fn poll(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<()> {
        if let Some(registry) = self.registry.upgrade() {
            // Register before inspecting retirement. A drop after collection
            // wakes this root; Ready instead closes the outer scope immediately.
            registry.wake.waker.register(cx.waker());
            if registry.wake.dirty.swap(false, Ordering::AcqRel) {
                registry.collect();
            }
        }
        self.future.as_mut().poll(cx)
    }
}
