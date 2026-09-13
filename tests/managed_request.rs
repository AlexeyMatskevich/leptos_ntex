use leptos_ntex_unofficial::{
    Request, RequestAccessError as AccessError, RequestRuntime, RequestScope,
};
use lets_expect::*;
use ntex::web::test::TestRequest;
use std::{
    future::{Future, poll_fn},
    pin::Pin,
    sync::{Arc, Mutex},
    task::{Context, Poll},
    thread::{self, ThreadId},
};

#[derive(Clone, Default)]
struct Seen(Arc<Mutex<Vec<ThreadId>>>);
impl Seen {
    fn count(&self) -> usize {
        self.0.lock().unwrap().len()
    }
    fn all_on(&self, origin: ThreadId) -> bool {
        self.0.lock().unwrap().iter().all(|id| *id == origin)
    }
}
struct NativeDrop {
    seen: Seen,
    panic: bool,
    read: Option<Request>,
}
impl Drop for NativeDrop {
    fn drop(&mut self) {
        if let Some(request) = &self.read {
            assert_eq!(
                request.with(|native| native.path().to_owned()),
                Ok("/managed".to_owned())
            );
        }
        self.seen.0.lock().unwrap().push(thread::current().id());
        assert!(!self.panic, "controlled native destructor panic");
    }
}
fn native(seen: &Seen) -> ntex::web::HttpRequest {
    let native = TestRequest::with_uri("/managed")
        .header("authorization", "secret-session-value")
        .to_http_request();
    native.extensions_mut().insert(NativeDrop {
        seen: seen.clone(),
        panic: false,
        read: None,
    });
    native
}
fn request(seen: &Seen) -> Request {
    let native = native(seen);
    Request::new(&native)
}
fn creation(active: bool) -> Result<String, AccessError> {
    let _scope = active.then(RequestScope::new);
    let native = TestRequest::with_uri("/managed").to_http_request();
    Request::try_new(&native).and_then(|request| request.with(|native| native.path().to_owned()))
}
fn missing_convenience() {
    let native = TestRequest::default().to_http_request();
    let _ = Request::new(&native);
}
fn access(closed: bool, foreign: bool) -> Result<String, AccessError> {
    let scope = RequestScope::new();
    let request = request(&Seen::default());
    let _scope = (!closed).then_some(scope);
    if foreign {
        thread::spawn(move || request.with(|native| native.path().to_owned()))
            .join()
            .unwrap()
    } else {
        request.with(|native| native.path().to_owned())
    }
}
fn clone_ownership(foreign: bool) -> (usize, String, usize, bool) {
    let _scope = RequestScope::new();
    let seen = Seen::default();
    let origin = thread::current().id();
    let first = request(&seen);
    let last = if foreign {
        thread::spawn(move || first.clone()).join().unwrap()
    } else {
        let last = first.clone();
        drop(first);
        last
    };
    let before = seen.count();
    let value = last.with(|native| native.path().to_owned()).unwrap();
    drop(last);
    (before, value, seen.count(), seen.all_on(origin))
}
#[derive(Clone, Copy)]
enum Retirement {
    Origin,
    Foreign,
    Closed,
    Repeated,
}
fn retirement(mode: Retirement) -> (usize, usize, usize, bool) {
    let scope = RequestScope::new();
    let seen = Seen::default();
    let origin = thread::current().id();
    match mode {
        Retirement::Origin => {
            drop(request(&seen));
            (
                seen.count(),
                scope.collect(),
                seen.count(),
                seen.all_on(origin),
            )
        }
        Retirement::Foreign => {
            let request = request(&seen);
            thread::spawn(move || drop(request)).join().unwrap();
            let before = seen.count();
            (before, scope.collect(), seen.count(), seen.all_on(origin))
        }
        Retirement::Closed => {
            let request = request(&seen);
            drop(scope);
            let before = seen.count();
            thread::spawn(move || drop(request)).join().unwrap();
            (before, 0, seen.count(), seen.all_on(origin))
        }
        Retirement::Repeated => {
            for _ in 0..64 {
                drop(request(&seen));
            }
            (
                seen.count(),
                scope.collect(),
                seen.count(),
                seen.all_on(origin),
            )
        }
    }
}
#[derive(Clone, Copy)]
enum Transfer {
    Fallible,
    Convenience,
    Callback,
}
fn transfer(mode: Transfer) -> (usize, String, usize, bool) {
    let scope = RequestScope::new();
    let seen = Seen::default();
    let origin = thread::current().id();
    let request = request(&seen);
    let native = match mode {
        Transfer::Fallible => request.try_into_inner().unwrap(),
        Transfer::Convenience => request.into_inner(),
        Transfer::Callback => request.with(ntex::web::HttpRequest::clone).unwrap(),
    };
    drop(scope);
    let before = seen.count();
    let path = native.path().to_owned();
    drop(native);
    (before, path, seen.count(), seen.all_on(origin))
}
fn failed_transfer(closed: bool) -> Result<(), AccessError> {
    let scope = RequestScope::new();
    let request = request(&Seen::default());
    if closed {
        drop(scope);
        request.try_into_inner().map(drop)
    } else {
        thread::spawn(move || request.try_into_inner().map(drop))
            .join()
            .unwrap()
    }
}
fn nesting(parent_first: bool) -> (usize, usize, bool, bool) {
    let parent = RequestScope::new();
    let parent_seen = Seen::default();
    let parent_request = request(&parent_seen);
    let child = RequestScope::new();
    let child_seen = Seen::default();
    let child_request = request(&child_seen);
    if parent_first {
        drop(parent);
        let later = request(&child_seen);
        let parent_closed = parent_request.with(|_| ()).is_err();
        let child_open = child_request.with(|_| ()).is_ok();
        drop(child);
        drop(later);
        (
            parent_seen.count(),
            child_seen.count(),
            parent_closed,
            child_open,
        )
    } else {
        drop(child);
        let later = request(&parent_seen);
        let child_closed = child_request.with(|_| ()).is_err();
        let parent_open = parent_request.with(|_| ()).is_ok();
        drop(parent);
        drop(later);
        (
            child_seen.count(),
            parent_seen.count(),
            child_closed,
            parent_open,
        )
    }
}
fn recursive_read() -> Result<Result<String, AccessError>, AccessError> {
    let _scope = RequestScope::new();
    let request = request(&Seen::default());
    request.with(|_| request.with(|native| native.path().to_owned()))
}
fn close_in_callback() -> (String, usize, usize, Result<(), AccessError>) {
    let scope = RequestScope::new();
    let seen = Seen::default();
    let request = request(&seen);
    let (path, during) = request
        .with(|native| {
            drop(scope);
            (native.path().to_owned(), seen.count())
        })
        .unwrap();
    (path, during, seen.count(), request.with(|_| ()))
}
fn callback_panic() -> (bool, String, usize) {
    let _scope = RequestScope::new();
    let seen = Seen::default();
    let request = request(&seen);
    let caught = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        request.with(|_| panic!("controlled callback panic"))
    }))
    .is_err();
    let path = request.with(|native| native.path().to_owned()).unwrap();
    drop(request);
    (caught, path, seen.count())
}
fn reentrant_destructor() -> (usize, String) {
    let _scope = RequestScope::new();
    let keeper = request(&Seen::default());
    let seen = Seen::default();
    let native = native(&seen);
    native
        .extensions_mut()
        .get_mut::<NativeDrop>()
        .unwrap()
        .read = Some(keeper.clone());
    let request = Request::new(&native);
    drop(native);
    drop(request);
    (
        seen.count(),
        keeper.with(|native| native.path().to_owned()).unwrap(),
    )
}
fn destructor_panic(unwinding: bool) -> (bool, usize, bool) {
    let seen = Seen::default();
    let mut handles = Vec::new();
    let caught = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        let scope = RequestScope::new();
        let first = native(&seen);
        first
            .extensions_mut()
            .get_mut::<NativeDrop>()
            .unwrap()
            .panic = true;
        handles.push(Request::new(&first));
        drop(first);
        handles.push(request(&seen));
        if unwinding {
            panic!("original unwind");
        }
        drop(scope);
    }))
    .is_err();
    (
        caught,
        seen.count(),
        handles
            .iter()
            .all(|request| request.with(|_| ()) == Err(AccessError::Closed)),
    )
}
async fn yield_once() {
    let mut yielded = false;
    poll_fn(|cx| {
        if yielded {
            Poll::Ready(())
        } else {
            yielded = true;
            cx.waker().wake_by_ref();
            Poll::Pending
        }
    })
    .await
}
fn runtime<T: 'static>(future: impl Future<Output = T> + 'static) -> T {
    ntex::rt::System::build()
        .testing()
        .build(RequestRuntime::new(ntex::rt::DefaultRuntime))
        .block_on(future)
}
fn runtime_retirement() -> (usize, bool) {
    let seen = Seen::default();
    let observed = seen.clone();
    let origin = thread::current().id();
    runtime(async move {
        let request = request(&observed);
        let (sent, ready) = futures::channel::oneshot::channel();
        let foreign = thread::spawn(move || {
            drop(request);
            sent.send(()).unwrap();
        });
        ready.await.unwrap();
        yield_once().await;
        foreign.join().unwrap();
        (observed.count(), observed.all_on(origin))
    })
}
fn runtime_close(panic: bool) -> (bool, usize, bool) {
    let seen = Seen::default();
    let observed = seen.clone();
    let retained = Arc::new(Mutex::new(None));
    let retained_in_future = retained.clone();
    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        runtime(async move {
            let request = request(&observed);
            *retained_in_future.lock().unwrap() = Some(request.clone());
            if panic {
                panic!("controlled root panic");
            }
            request
        })
    }));
    let retained = retained.lock().unwrap().take().unwrap();
    let closed = retained.with(|_| ()) == Err(AccessError::Closed);
    (result.is_err(), seen.count(), closed)
}

fn worker_scope() -> (usize, bool, bool) {
    let seen = Seen::default();
    let observed = seen.clone();
    let parent = thread::current().id();
    runtime(async move {
        let mut arbiter = ntex::rt::Arbiter::new();
        let (sent, ready) = futures::channel::oneshot::channel();
        arbiter.handle().spawn(async move {
            let origin = thread::current().id();
            let request = request(&observed);
            sent.send((request, origin)).unwrap();
            ntex::rt::Arbiter::current().stop();
        });
        let (request, origin) = ready.await.unwrap();
        arbiter.join().unwrap();
        let result = (seen.count(), seen.all_on(origin), origin != parent);
        drop(request);
        result
    })
}
struct PendingToken {
    request: Request,
    started: Option<futures::channel::oneshot::Sender<()>>,
    external: futures::channel::oneshot::Receiver<()>,
}
impl Future for PendingToken {
    type Output = ();
    fn poll(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<()> {
        assert_eq!(
            self.request.with(|native| native.path().to_owned()),
            Ok("/managed".to_owned())
        );
        if let Some(started) = self.started.take() {
            started.send(()).unwrap();
        }
        Pin::new(&mut self.external).poll(cx).map(|_| ())
    }
}
fn pending_shutdown() -> (usize, bool) {
    let seen = Seen::default();
    let observed = seen.clone();
    let (external, receiver) = futures::channel::oneshot::channel();
    let origin = thread::current().id();
    runtime(async move {
        let request = request(&observed);
        let (started, ready) = futures::channel::oneshot::channel();
        ntex::rt::spawn(PendingToken {
            request,
            started: Some(started),
            external: receiver,
        });
        ready.await.unwrap();
    });
    drop(external);
    (seen.count(), seen.all_on(origin))
}
#[derive(Clone, Copy)]
enum DropWindow {
    AfterPending,
    DuringInnerPoll,
}
struct WakeCount(std::sync::atomic::AtomicUsize);
impl futures::task::ArcWake for WakeCount {
    fn wake_by_ref(arc: &Arc<Self>) {
        arc.0.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
    }
}
struct WakeFixture {
    request: Mutex<Option<Request>>,
    seen: Seen,
    result: Mutex<Option<(usize, usize, usize, bool)>>,
}
fn drop_foreign(fixture: &WakeFixture) {
    let request = fixture.request.lock().unwrap().take().unwrap();
    thread::spawn(move || drop(request)).join().unwrap();
}
struct ControlledRunner {
    fixture: Arc<WakeFixture>,
    window: DropWindow,
}
impl ControlledRunner {
    fn run(&self, mut future: ntex::rt::BlockFuture) {
        let origin = thread::current().id();
        let wake = Arc::new(WakeCount(std::sync::atomic::AtomicUsize::new(0)));
        let waker = futures::task::waker(wake.clone());
        let mut cx = Context::from_waker(&waker);
        assert!(future.as_mut().poll(&mut cx).is_pending());
        match self.window {
            DropWindow::AfterPending => drop_foreign(&self.fixture),
            DropWindow::DuringInnerPoll => assert!(future.as_mut().poll(&mut cx).is_pending()),
        }
        let before = self.fixture.seen.count();
        let wakes = wake.0.load(std::sync::atomic::Ordering::SeqCst);
        assert!(future.as_mut().poll(&mut cx).is_ready());
        *self.fixture.result.lock().unwrap() = Some((
            before,
            wakes,
            self.fixture.seen.count(),
            self.fixture.seen.all_on(origin),
        ));
    }
}
#[cfg(not(ntex_runner_returns_result))]
impl ntex::rt::Runner for ControlledRunner {
    fn block_on(&self, future: ntex::rt::BlockFuture) {
        self.run(future)
    }
}
#[cfg(ntex_runner_returns_result)]
impl ntex::rt::Runner for ControlledRunner {
    fn block_on(&self, future: ntex::rt::BlockFuture) -> std::thread::Result<()> {
        self.run(future);
        Ok(())
    }
}
struct ControlledRoot {
    fixture: Arc<WakeFixture>,
    window: DropWindow,
    polls: u8,
}
impl Future for ControlledRoot {
    type Output = ();
    fn poll(mut self: Pin<&mut Self>, _: &mut Context<'_>) -> Poll<()> {
        self.polls += 1;
        if self.polls == 1 {
            *self.fixture.request.lock().unwrap() = Some(request(&self.fixture.seen));
            Poll::Pending
        } else if self.polls == 2 && matches!(self.window, DropWindow::DuringInnerPoll) {
            drop_foreign(&self.fixture);
            Poll::Pending
        } else {
            Poll::Ready(())
        }
    }
}
fn wakeup(window: DropWindow) -> (usize, usize, usize, bool) {
    let fixture = Arc::new(WakeFixture {
        request: Mutex::new(None),
        seen: Seen::default(),
        result: Mutex::new(None),
    });
    let runner = RequestRuntime::new(ControlledRunner {
        fixture: fixture.clone(),
        window,
    });
    let future = Box::pin(ControlledRoot {
        fixture: fixture.clone(),
        window,
        polls: 0,
    });
    #[cfg(ntex_runner_returns_result)]
    ntex::rt::Runner::block_on(&runner, future).expect("controlled runtime completes successfully");
    #[cfg(not(ntex_runner_returns_result))]
    ntex::rt::Runner::block_on(&runner, future);
    fixture.result.lock().unwrap().take().unwrap()
}
fn debug_is_private() -> bool {
    let _scope = RequestScope::new();
    let request = request(&Seen::default());
    let text = format!("{request:?}");
    !text.contains("secret-session-value") && !text.contains("authorization")
}

lets_expect! {
    expect(creation(active)) as creation {
        let active = true;
        to reads_owned_value { equal(Ok("/managed".to_owned())) }
        when no_scope { let active = false; to reports_missing_scope { equal(Err(AccessError::MissingScope)) } }
    }
    expect(missing_convenience()) as creation_without_scope {
        when convenience_constructor_has_no_scope { to panic }
    }
    expect(access(closed, foreign)) as request_access {
        let closed = false; let foreign = false;
        to reads_on_origin { equal(Ok("/managed".to_owned())) }
        when access_is_foreign { let foreign = true; to reports_wrong_thread { equal(Err(AccessError::WrongThread)) } }
        when scope_is_closed {
            let closed = true;
            to reports_closed { equal(Err(AccessError::Closed)) }
            when access_is_foreign { let foreign = true; to reports_wrong_thread { equal(Err(AccessError::WrongThread)) } }
        }
    }
    expect(clone_ownership(foreign)) as token_ownership {
        let foreign = false;
        to releases_after_last_clone { equal((0, "/managed".to_owned(), 1, true)) }
        when clone_is_made_on_foreign_thread { let foreign = true; to preserves_origin_access { equal((0, "/managed".to_owned(), 1, true)) } }
    }
    expect(retirement(mode)) as request_retirement {
        let mode = Retirement::Origin;
        to releases_on_origin { equal((1, 0, 1, true)) }
        when final_drop_is_foreign { let mode = Retirement::Foreign; to releases_after_explicit_collection { equal((0, 1, 1, true)) } }
        when scope_closes_first { let mode = Retirement::Closed; to releases_before_foreign_drop { equal((1, 0, 1, true)) } }
        when requests_complete_repeatedly { let mode = Retirement::Repeated; to retains_no_completed_graphs { equal((64, 0, 64, true)) } }
    }
    expect(transfer(mode)) as native_transfer {
        let mode = Transfer::Fallible;
        to transfers_with_try_into_inner { equal((0, "/managed".to_owned(), 1, true)) }
        when convenience_method_is_used { let mode = Transfer::Convenience; to transfers_with_into_inner { equal((0, "/managed".to_owned(), 1, true)) } }
        when callback_clones_native { let mode = Transfer::Callback; to preserves_external_native_owner { equal((0, "/managed".to_owned(), 1, true)) } }
    }
    expect(failed_transfer(closed)) as native_transfer_failure {
        let closed = false;
        when access_is_foreign { to reports_wrong_thread { equal(Err(AccessError::WrongThread)) } }
        when scope_is_closed { let closed = true; to reports_closed { equal(Err(AccessError::Closed)) } }
    }
    expect(nesting(parent_first)) as nested_scopes {
        let parent_first = false;
        to restores_parent_after_inner_close { equal((1, 2, true, true)) }
        when parent_closes_first { let parent_first = true; to preserves_child_current_scope { equal((1, 2, true, true)) } }
    }
    expect(recursive_read()) as callback_scope { to allows_reentrant_access { equal(Ok(Ok("/managed".to_owned()))) } }
    expect(close_in_callback()) as callback_closes_scope { to pins_native_until_callback_returns { equal(("/managed".to_owned(), 0, 1, Err(AccessError::Closed))) } }
    expect(callback_panic()) as callback_panics { to preserves_registry { equal((true, "/managed".to_owned(), 1)) } }
    expect(reentrant_destructor()) as destructor_scope { to permits_reentrant_native_drop { equal((1, "/managed".to_owned())) } }
    expect(destructor_panic(unwinding)) as destructor_panic {
        let unwinding = false;
        to closes_all_entries_before_resuming { equal((true, 2, true)) }
        when scope_unwinds { let unwinding = true; to avoids_a_second_panic { equal((true, 2, true)) } }
    }
    expect(runtime_retirement()) as runtime_retirement { to collects_foreign_drop_before_next_poll { equal((1, true)) } }
    expect(runtime_close(panic)) as runtime_close {
        let panic = false;
        to closes_native_before_return { equal((false, 1, true)) }
        when root_panics { let panic = true; to closes_native_during_unwind { equal((true, 1, true)) } }
    }
    expect(worker_scope()) as worker_scope { to installs_scope_on_worker { equal((1, true, true)) } }
    expect(pending_shutdown()) as pending_shutdown { to closes_native_without_task_drop { equal((1, true)) } }
    expect(wakeup(window)) as retirement_wakeup {
        let window = DropWindow::AfterPending;
        to wakes_the_root_after_pending { equal((0, 1, 1, true)) }
        when drop_occurs_during_inner_poll { let window = DropWindow::DuringInnerPoll; to wakes_the_next_collection { equal((0, 1, 1, true)) } }
    }
    expect(debug_is_private()) as request_debug { to omits_sensitive_headers { be_true } }
}

#[cfg(ntex_runner_returns_result)]
mod fallible_runner {
    use super::*;

    struct ResultRunner(bool);
    impl ntex::rt::Runner for ResultRunner {
        fn block_on(&self, future: ntex::rt::BlockFuture) -> std::thread::Result<()> {
            futures::executor::block_on(future);
            if self.0 {
                Err(Box::new("controlled runner error"))
            } else {
                Ok(())
            }
        }
    }
    fn completion(
        fails: bool,
    ) -> (
        Result<(), &'static str>,
        usize,
        bool,
        Result<(), AccessError>,
    ) {
        let retained = Arc::new(Mutex::new(None));
        let inside = retained.clone();
        let seen = Seen::default();
        let inner_seen = seen.clone();
        let origin = thread::current().id();
        let result = ntex::rt::Runner::block_on(
            &RequestRuntime::new(ResultRunner(fails)),
            Box::pin(async move {
                *inside.lock().unwrap() = Some(request(&inner_seen));
            }),
        )
        .map_err(|payload| {
            *payload
                .downcast::<&'static str>()
                .expect("original runner payload type")
        });
        let request = retained.lock().unwrap().take().unwrap();
        (
            result,
            seen.count(),
            seen.all_on(origin),
            request.with(|_| ()),
        )
    }
    lets_expect! {
        expect(completion(fails)) as runner_result {
            let fails = false;
            to closes_scope_before_returning_success { equal((Ok(()), 1, true, Err(AccessError::Closed))) }
            when inner_runner_returns_error {
                let fails = true;
                to closes_scope_and_preserves_the_error { equal((Err("controlled runner error"), 1, true, Err(AccessError::Closed))) }
            }
        }
    }
}
