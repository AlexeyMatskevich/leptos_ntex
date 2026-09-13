//! Reactive cleanup tied to an integration lifetime rather than Owner handles.

use leptos::reactive::owner::Owner;
use ntex::http::body::{Body, BodySize, MessageBody, ResponseBody};
use ntex::util::Bytes;
use ntex::web::HttpResponse;
use std::{
    error::Error,
    future::Future,
    rc::Rc,
    sync::Arc,
    task::{Context, Poll},
    thread::ThreadId,
};

pub(crate) struct OwnerCleanup {
    owner: Option<Owner>,
    origin_thread: ThreadId,
}

impl OwnerCleanup {
    pub(crate) fn new(owner: Owner) -> Self {
        Self {
            owner: Some(owner),
            origin_thread: std::thread::current().id(),
        }
    }

    pub(crate) fn owner(&self) -> &Owner {
        self.owner
            .as_ref()
            .expect("armed cleanup always owns its reactive Owner")
    }

    /// Runs synchronous work in the retained request Owner and its arena.
    /// Each caller wraps one poll or destructor, never an await. Off-thread
    /// destruction retains the existing leak safeguard instead of activating
    /// thread-affine request state on a foreign thread.
    pub(crate) fn with_context<R>(&self, work: impl FnOnce() -> R) -> R {
        let _restore = RestoreOwner::capture();
        if std::thread::current().id() == self.origin_thread {
            self.owner().set();
        }
        work()
    }

    pub(crate) fn into_owner(mut self) -> Owner {
        self.owner
            .take()
            .expect("armed cleanup always owns its reactive Owner")
    }
}

impl Drop for OwnerCleanup {
    fn drop(&mut self) {
        if let Some(owner) = self.owner.take() {
            if std::thread::current().id() == self.origin_thread {
                let _restore = RestoreOwner::capture();
                owner.unset_with_forced_cleanup();
            } else {
                let msg = "reactive response owner dropped off its origin thread; leaking the Owner to avoid thread-affine cleanup";
                #[cfg(feature = "tracing")]
                tracing::warn!("{msg}");
                #[cfg(not(feature = "tracing"))]
                eprintln!("{msg}");
                std::mem::forget(owner);
            }
        }
    }
}

pub(crate) fn with_owner_cleanup(response: HttpResponse, owner: Arc<OwnerCleanup>) -> HttpResponse {
    response
        .map_body(|_, inner| ResponseBody::Body(Body::from_message(OwnerLease::new(inner, owner))))
}

/// Keeps reactive values alive until the wrapped producer (an HTML chunk
/// stream or a response body) and its destructor finish. Every poll runs in
/// the retained Owner; Drop destroys the producer before forcing reactive
/// cleanup, then restores the caller's Owner (and its arena when present) even
/// if either destructor unwinds. An ownerless caller remains without an Owner.
pub(crate) struct OwnerLease<T: ?Sized> {
    inner: Option<std::pin::Pin<Box<T>>>,
    owner: Option<Arc<OwnerCleanup>>,
}

impl<T> OwnerLease<T> {
    pub(crate) fn new(inner: T, owner: Arc<OwnerCleanup>) -> Self {
        Self {
            inner: Some(Box::pin(inner)),
            owner: Some(owner),
        }
    }
}

impl<T: ?Sized> OwnerLease<T> {
    fn in_owner<R>(&mut self, work: impl FnOnce(std::pin::Pin<&mut T>) -> R) -> R {
        let inner = self.inner.as_mut().expect("producer present until drop");
        self.owner
            .as_ref()
            .expect("owner present until drop")
            .with_context(|| work(inner.as_mut()))
    }
}

impl<T: futures::Stream + ?Sized> futures::Stream for OwnerLease<T> {
    type Item = T::Item;

    fn poll_next(self: std::pin::Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        self.get_mut().in_owner(|inner| inner.poll_next(cx))
    }
}

impl<T: MessageBody + Unpin + ?Sized> MessageBody for OwnerLease<T> {
    fn size(&self) -> BodySize {
        self.inner
            .as_ref()
            .expect("producer present until drop")
            .size()
    }

    fn poll_next_chunk(
        &mut self,
        cx: &mut Context<'_>,
    ) -> Poll<Option<Result<Bytes, Rc<dyn Error>>>> {
        self.in_owner(|inner| inner.get_mut().poll_next_chunk(cx))
    }
}

impl<T: ?Sized> Drop for OwnerLease<T> {
    fn drop(&mut self) {
        let _restore = RestoreOwner::capture();
        // Keep the lease local: even a panicking producer destructor must
        // finish reactive cleanup before the caller's context is restored.
        let owner = self.owner.take().expect("owner present until drop");
        owner.with_context(|| drop(self.inner.take()));
        drop(owner);
    }
}

/// Restores the caller's Owner even when upstream `Owner::with` unwinds.
/// A present caller Owner also restores its arena. Without a caller Owner,
/// this restores absence of an Owner, not an independent ownerless arena.
/// This guard must never be held across an await.
pub(crate) struct RestoreOwner(Option<Owner>);

impl RestoreOwner {
    pub(crate) fn capture() -> Self {
        Self(Owner::current())
    }
}

impl Drop for RestoreOwner {
    fn drop(&mut self) {
        if let Some(previous) = self.0.take() {
            previous.set();
        } else if let Some(current) = Owner::current() {
            current.unset();
        }
    }
}

/// Keeps the context boundary around every poll and around cancellation.
/// The inner allocation permits safe pinning of arbitrary !Unpin futures.
pub(crate) struct OwnerContextFuture<F: ?Sized> {
    inner: Option<std::pin::Pin<Box<F>>>,
}

impl<F> OwnerContextFuture<F> {
    pub(crate) fn new(future: F) -> Self {
        Self {
            inner: Some(Box::pin(future)),
        }
    }
}

impl<F: ?Sized> OwnerContextFuture<F> {
    pub(crate) fn from_pin(future: std::pin::Pin<Box<F>>) -> Self {
        Self {
            inner: Some(future),
        }
    }
}

impl<F: std::future::Future + ?Sized> std::future::Future for OwnerContextFuture<F> {
    type Output = F::Output;
    fn poll(self: std::pin::Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        let _restore = RestoreOwner::capture();
        self.get_mut()
            .inner
            .as_mut()
            .expect("future present until drop")
            .as_mut()
            .poll(cx)
    }
}

impl<F: ?Sized> Drop for OwnerContextFuture<F> {
    fn drop(&mut self) {
        let _restore = RestoreOwner::capture();
        drop(self.inner.take());
    }
}

/// Keeps a task or subscription in its creation scope. The scope outlives the
/// inner value: dropping a parent Owner can clean child arenas even while child
/// Owner handles remain alive, so ScopedFuture's field Drop order is insufficient.
pub(crate) struct ScopedWork<T: ?Sized> {
    inner: Option<std::pin::Pin<Box<T>>>,
    owner: Option<Owner>,
    observer: Option<leptos::reactive::graph::AnySubscriber>,
}

impl<T> ScopedWork<T> {
    pub(crate) fn new(inner: T) -> Self {
        Self::from_pin(Box::pin(inner))
    }

    pub(crate) fn with_owner(owner: Owner, inner: T) -> Self {
        Self::from_pin_with_owner(owner, Box::pin(inner))
    }
}

impl<T: ?Sized> ScopedWork<T> {
    pub(crate) fn from_pin(inner: std::pin::Pin<Box<T>>) -> Self {
        Self::from_pin_with_owner(Owner::current().unwrap_or_default(), inner)
    }

    fn from_pin_with_owner(owner: Owner, inner: std::pin::Pin<Box<T>>) -> Self {
        Self {
            inner: Some(inner),
            owner: Some(owner),
            observer: leptos::reactive::graph::Observer::get(),
        }
    }

    // One poll of the inner value inside reactive_graph's own scoping wrapper,
    // so Owner, Observer and diagnostics behave exactly as for ScopedFuture.
    fn poll_in_scope<R>(&mut self, poll: impl FnOnce(std::pin::Pin<&mut T>) -> R) -> R {
        let _restore = crate::owner::RestoreOwner::capture();
        let owner = self.owner.as_ref().expect("scope present until Drop");
        let inner = self.inner.as_mut().expect("inner present until Drop");
        let mut poll = Some(poll);
        let scoped = leptos::reactive::computed::ScopedFuture {
            owner: owner.clone(),
            observer: self.observer.clone(),
            fut: std::future::poll_fn(|_| {
                let poll = poll.take().expect("the scoped poll runs once");
                Poll::Ready(poll(inner.as_mut()))
            }),
        };
        let noop = std::task::Waker::noop();
        match std::pin::pin!(scoped).poll(&mut Context::from_waker(noop)) {
            Poll::Ready(result) => result,
            Poll::Pending => unreachable!("the scoped poll completes immediately"),
        }
    }
}

impl<T: Future + ?Sized> Future for ScopedWork<T> {
    type Output = T::Output;
    fn poll(
        self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<Self::Output> {
        self.get_mut().poll_in_scope(|inner| inner.poll(cx))
    }
}

impl<T: futures::Stream + ?Sized> futures::Stream for ScopedWork<T> {
    type Item = T::Item;
    fn poll_next(
        self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<Option<Self::Item>> {
        self.get_mut().poll_in_scope(|inner| inner.poll_next(cx))
    }
}

impl<T: ?Sized> Drop for ScopedWork<T> {
    fn drop(&mut self) {
        use leptos::reactive::graph::WithObserver;
        let _restore = crate::owner::RestoreOwner::capture();
        let owner = self.owner.take().expect("scope present until Drop");
        let inner = self.inner.take();
        owner.with(|| self.observer.with_observer(|| drop(inner)));
        drop(owner);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use leptos::prelude::on_cleanup;
    use lets_expect::lets_expect;
    use std::sync::atomic::{AtomicUsize, Ordering};

    fn tracked_owner() -> (Owner, Arc<AtomicUsize>) {
        let count = Arc::new(AtomicUsize::new(0));
        let counter = count.clone();
        let owner = Owner::new();
        owner.with(|| {
            on_cleanup(move || {
                counter.fetch_add(1, Ordering::SeqCst);
            })
        });
        (owner, count)
    }

    fn integration_leases() -> (usize, usize) {
        let (owner, count) = tracked_owner();
        let retained = owner.clone();
        let first = Arc::new(OwnerCleanup::new(owner));
        let second = first.clone();
        drop(first);
        let before = count.load(Ordering::SeqCst);
        drop(second);
        let after = count.load(Ordering::SeqCst);
        retained.unset_with_forced_cleanup();
        (before, after)
    }

    fn transfer_owner() -> (usize, usize) {
        let (owner, count) = tracked_owner();
        let owner = OwnerCleanup::new(owner).into_owner();
        let before = count.load(Ordering::SeqCst);
        owner.unset_with_forced_cleanup();
        (before, count.load(Ordering::SeqCst))
    }

    fn cleanup_hook_panics() {
        let owner = Owner::new();
        owner.with(|| on_cleanup(|| panic!("cleanup hook probe")));
        drop(OwnerCleanup::new(owner));
    }

    lets_expect! {
        expect(integration_leases()) as shared_integration_lifetime {
            to cleans_only_after_the_final_lease { equal((0_usize, 1_usize)) }
        }
        expect(transfer_owner()) as successful_owner_handoff {
            to leaves_cleanup_to_the_recipient { equal((0_usize, 1_usize)) }
        }
        expect(cleanup_hook_panics()) as cleanup_hook_failure {
            to panic
        }
    }
}
