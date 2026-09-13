//! Request context across asynchronous rendering and streamed output lifetimes.
use futures::{Stream, future::poll_fn};
use leptos::prelude::*;
use leptos_integration_utils::{BoxedFnOnce, PinnedFuture, PinnedStream};
use leptos_ntex_unofficial::{handle_response_inner, handle_server_fns_with_context};
use lets_expect::*;
use ntex::web::{App, test};
use std::{
    pin::Pin,
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, AtomicUsize, Ordering},
    },
    task::{Context, Poll},
};

#[derive(Clone, Copy)]
struct Marker(u8);
fn marker() -> Option<u8> {
    use_context::<Marker>().map(|v| v.0)
}

#[derive(Clone, Copy)]
enum Stage {
    Setup,
    Body,
    CancelSetup,
    CancelBody,
    Deferred,
}
#[derive(Default)]
struct Observations {
    initial: Mutex<Option<Option<u8>>>,
    resumed: Mutex<Option<Option<u8>>>,
    dropped: Mutex<Option<Option<u8>>>,
    stored_on_drop: Mutex<Option<i32>>,
    cleaned: AtomicUsize,
    release_body: AtomicBool,
    body_waker: futures::task::AtomicWaker,
}
#[derive(Clone)]
struct ProbeState(Arc<Observations>, Stage);
struct DropProbe {
    state: Arc<Observations>,
    stored: StoredValue<i32>,
}
impl DropProbe {
    fn new(state: Arc<Observations>) -> Self {
        Self {
            state,
            stored: StoredValue::new(314),
        }
    }
}
impl Drop for DropProbe {
    fn drop(&mut self) {
        *self.state.dropped.lock().unwrap() = Some(marker());
        *self.state.stored_on_drop.lock().unwrap() = self.stored.try_get_value();
    }
}
struct LazyBody {
    probe: DropProbe,
    emitted_shell: bool,
    emitted_tail: bool,
}
impl Stream for LazyBody {
    type Item = String;
    fn poll_next(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<String>> {
        if !self.emitted_shell {
            self.emitted_shell = true;
            return Poll::Ready(Some(
                "<!DOCTYPE html><html><head></head><body>shell".to_owned(),
            ));
        }
        self.probe.state.body_waker.register(cx.waker());
        if !self.probe.state.release_body.load(Ordering::SeqCst) {
            return Poll::Pending;
        }
        if self.emitted_tail {
            return Poll::Ready(None);
        }
        self.emitted_tail = true;
        let value = marker();
        *self.probe.state.resumed.lock().unwrap() = Some(value);
        Poll::Ready(Some(format!("context:{value:?}</body></html>")))
    }
}
async fn yield_once() {
    let mut yielded = false;
    poll_fn(move |cx| {
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
fn builder(
    _: &'static str,
    _: BoxedFnOnce<PinnedStream<String>>,
    _: bool,
) -> PinnedFuture<PinnedStream<String>> {
    let ProbeState(state, stage) = expect_context();
    *state.initial.lock().unwrap() = Some(marker());
    let probe = DropProbe::new(state.clone());
    Box::pin(async move {
        if matches!(stage, Stage::CancelSetup) {
            let _probe = probe;
            futures::future::pending::<()>().await;
            unreachable!()
        }
        if matches!(stage, Stage::Setup) {
            yield_once().await;
            let value = marker();
            *state.resumed.lock().unwrap() = Some(value);
            // Keep the destructor after the suspension, including cancellation.
            drop(probe);
            return Box::pin(futures::stream::once(async move {
                format!("<!DOCTYPE html><html><head></head><body>context:{value:?}</body></html>")
            })) as PinnedStream<String>;
        }
        if matches!(stage, Stage::Deferred) {
            drop(probe);
            return Box::pin(futures::stream::once(async {
                "<!DOCTYPE html><html><head></head><body>deferred</body></html>".to_owned()
            })) as PinnedStream<String>;
        }
        Box::pin(LazyBody {
            probe,
            emitted_shell: false,
            emitted_tail: false,
        }) as PinnedStream<String>
    })
}

#[derive(Debug)]
struct ResultState {
    initial: Option<Option<u8>>,
    resumed: Option<Option<u8>>,
    dropped: Option<Option<u8>>,
    stored_on_drop: Option<i32>,
    caller_after_poll: Option<u8>,
    caller_after_drop: Option<u8>,
    cleaned: usize,
    body: String,
}
fn collect(
    state: &Observations,
    caller_after_poll: Option<u8>,
    caller_after_drop: Option<u8>,
    body: String,
) -> ResultState {
    ResultState {
        initial: *state.initial.lock().unwrap(),
        resumed: *state.resumed.lock().unwrap(),
        dropped: *state.dropped.lock().unwrap(),
        stored_on_drop: *state.stored_on_drop.lock().unwrap(),
        caller_after_poll,
        caller_after_drop,
        cleaned: state.cleaned.load(Ordering::SeqCst),
        body,
    }
}
fn run_ntex<T: 'static>(future: impl std::future::Future<Output = T> + 'static) -> T {
    ntex::rt::System::build()
        .testing()
        .build(leptos_ntex_unofficial::RequestRuntime::new(
            ntex::rt::DefaultRuntime,
        ))
        .block_on(future)
}
fn caller_scope(present: bool) -> Option<Owner> {
    present.then(|| {
        let owner = Owner::new_root(None);
        provide_context(Marker(7));
        owner
    })
}
fn ssr(stage: Stage, caller_present: bool, request_value: Option<u8>) -> ResultState {
    run_ntex(async move {
        let caller = caller_scope(caller_present);
        let state = Arc::new(Observations::default());
        let context_state = state.clone();
        let mut future = handle_response_inner(
            move || {
                if let Some(value) = request_value {
                    provide_context(Marker(value));
                }
                provide_context(ProbeState(context_state.clone(), stage));
                let clean = context_state.clone();
                on_cleanup(move || {
                    clean.cleaned.fetch_add(1, Ordering::SeqCst);
                });
                if matches!(stage, Stage::Deferred) {
                    Owner::current_shared_context()
                        .unwrap()
                        .defer_stream(Box::pin(async move {
                            yield_once().await;
                            *context_state.resumed.lock().unwrap() = Some(marker());
                        }));
                }
            },
            || "shell",
            test::TestRequest::get().to_http_request(),
            builder,
        );
        let (after_poll, body) = if matches!(stage, Stage::CancelSetup) {
            assert!(
                futures::poll!(future.as_mut()).is_pending(),
                "setup must reach Pending"
            );
            (marker(), String::new())
        } else {
            let mut response = future.as_mut().await;
            let after = marker();
            let body = if matches!(stage, Stage::CancelBody) {
                drop(response);
                String::new()
            } else {
                state.release_body.store(true, Ordering::SeqCst);
                state.body_waker.wake();
                String::from_utf8(
                    test::load_stream(response.take_body())
                        .await
                        .unwrap()
                        .to_vec(),
                )
                .unwrap()
            };
            (after, body)
        };
        drop(future);
        let result = collect(&state, after_poll, marker(), body);
        if let Some(caller) = caller {
            caller.unset_with_forced_cleanup();
        }
        result
    })
}
fn preserve_scope(
    stage: Stage,
    caller_present: bool,
    request_value: Option<u8>,
) -> impl Fn(&ResultState) -> AssertionResult {
    move |actual| {
        let caller = caller_present.then_some(7);
        let mut errors = Vec::new();
        if actual.initial != Some(request_value) {
            errors.push(format!("initial context: {:?}", actual.initial));
        }
        if actual.caller_after_poll != caller || actual.caller_after_drop != caller {
            errors.push(format!(
                "caller after poll/drop: {:?}/{:?}; expected {caller:?}",
                actual.caller_after_poll, actual.caller_after_drop
            ));
        }
        if actual.cleaned != 1 {
            errors.push(format!("cleanup count: {}", actual.cleaned));
        }
        if matches!(stage, Stage::Setup | Stage::Body | Stage::Deferred) {
            if actual.resumed != Some(request_value) {
                errors.push(format!(
                    "resumed request context: {:?}; expected {request_value:?}",
                    actual.resumed
                ));
            }
            if matches!(stage, Stage::Setup | Stage::Body)
                && !actual.body.contains(&format!("context:{request_value:?}"))
            {
                errors.push(format!("body: {:?}", actual.body));
            }
        } else if actual.dropped != Some(request_value) || actual.stored_on_drop != Some(314) {
            errors.push(format!(
                "destructor context/arena: {:?}/{:?}; expected {request_value:?}/314",
                actual.dropped, actual.stored_on_drop
            ));
        }
        if errors.is_empty() {
            Ok(())
        } else {
            Err(AssertionError::new(errors))
        }
    }
}
lets_expect! {
    expect(ssr(stage, caller_present, request_value)) as ssr_request_scope {
        let stage = Stage::Setup;
        let caller_present = true;
        let request_value = Some(42);
        to keeps_request_context_after_pending_setup { preserve_scope(stage, caller_present, request_value) }
        when lazy_body_is_polled { let stage = Stage::Body; to keeps_request_context_after_headers { preserve_scope(stage, caller_present, request_value) } }
        when pending_setup_is_cancelled { let stage = Stage::CancelSetup; to keeps_request_scope_in_destructor { preserve_scope(stage, caller_present, request_value) } }
        when body_is_cancelled { let stage = Stage::CancelBody; to keeps_request_scope_in_destructor { preserve_scope(stage, caller_present, request_value) } }
        when deferred_work_resumes { let stage = Stage::Deferred; to keeps_request_context_in_deferred_poll { preserve_scope(stage, caller_present, request_value) } }
        when caller_has_no_owner {
            let caller_present = false;
            to keeps_request_context_after_pending_setup { preserve_scope(stage, caller_present, request_value) }
            when lazy_body_is_polled { let stage = Stage::Body; to keeps_request_context_after_headers { preserve_scope(stage, caller_present, request_value) } }
            when pending_setup_is_cancelled { let stage = Stage::CancelSetup; to keeps_request_scope_in_destructor { preserve_scope(stage, caller_present, request_value) } }
            when body_is_cancelled { let stage = Stage::CancelBody; to keeps_request_scope_in_destructor { preserve_scope(stage, caller_present, request_value) } }
            when deferred_work_resumes { let stage = Stage::Deferred; to keeps_request_context_in_deferred_poll { preserve_scope(stage, caller_present, request_value) } }
        }
        when request_context_is_absent {
            let request_value = None;
            to does_not_inherit_caller_context_after_pending_setup { preserve_scope(stage, caller_present, request_value) }
            when lazy_body_is_polled { let stage = Stage::Body; to does_not_inherit_caller_context_after_headers { preserve_scope(stage, caller_present, request_value) } }
        }
    }
}

#[server(name = ContextOutput, prefix = "/scope", endpoint = "output", output = server_fn::codec::StreamingText, server = leptos_ntex_unofficial::NtexServerFnBackend)]
async fn context_output() -> Result<server_fn::codec::TextStream, ServerFnError> {
    let ProbeState(state, _) = expect_context();
    *state.initial.lock().unwrap() = Some(marker());
    let probe = DropProbe::new(state.clone());
    Ok(server_fn::codec::TextStream::new(futures::stream::once(
        async move {
            let value = marker();
            *state.resumed.lock().unwrap() = Some(value);
            drop(probe);
            Ok(format!("context:{value:?}"))
        },
    )))
}
fn server_output(stage: Stage, caller_present: bool) -> ResultState {
    run_ntex(async move {
        let caller = caller_scope(caller_present);
        let state = Arc::new(Observations::default());
        let context_state = state.clone();
        let app = test::init_service(App::new().service(
            ntex::web::resource("/scope/output").route(handle_server_fns_with_context(move || {
                provide_context(Marker(42));
                provide_context(ProbeState(context_state.clone(), Stage::Body));
                let clean = context_state.clone();
                on_cleanup(move || {
                    clean.cleaned.fetch_add(1, Ordering::SeqCst);
                });
            })),
        ))
        .await;
        let response = test::call_service(
            &app,
            test::TestRequest::post().uri("/scope/output").to_request(),
        )
        .await;
        assert_eq!(response.status(), ntex::http::StatusCode::OK);
        let after = marker();
        let body = if matches!(stage, Stage::CancelBody) {
            drop(response);
            String::new()
        } else {
            String::from_utf8(test::read_body(response).await.to_vec()).unwrap()
        };
        let result = collect(&state, after, marker(), body);
        if let Some(caller) = caller {
            caller.unset_with_forced_cleanup();
        }
        result
    })
}
lets_expect! {
    expect(server_output(stage, caller_present)) as server_output_scope {
        let caller_present = true;
        let stage = Stage::Body;
        to keeps_request_scope_in_stream_poll { preserve_scope(stage, caller_present, Some(42)) }
        when output_is_cancelled { let stage = Stage::CancelBody; to keeps_request_scope_in_stream_destructor { preserve_scope(stage, caller_present, Some(42)) } }
        when caller_has_no_owner {
            let caller_present = false;
            to keeps_request_scope_in_stream_poll { preserve_scope(stage, caller_present, Some(42)) }
            when output_is_cancelled { let stage = Stage::CancelBody; to keeps_request_scope_in_stream_destructor { preserve_scope(stage, caller_present, Some(42)) } }
        }
    }
}

#[server(name = ContextTask, prefix = "/scope", endpoint = "task", server = leptos_ntex_unofficial::NtexServerFnBackend)]
async fn context_task() -> Result<String, ServerFnError> {
    use server_fn::server::Server;
    let ProbeState(state, _) = expect_context();
    *state.initial.lock().unwrap() = Some(marker());
    let probe = DropProbe::new(state.clone());
    let (sender, receiver) = futures::channel::oneshot::channel();
    <leptos_ntex_unofficial::NtexServerFnBackend as Server<ServerFnError>>::spawn(async move {
        yield_once().await;
        let value = marker();
        *state.resumed.lock().unwrap() = Some(value);
        drop(probe);
        let _ = sender.send(format!("context:{value:?}"));
    })?;
    receiver
        .await
        .map_err(|error| ServerFnError::new(error.to_string()))
}
fn server_task(caller_present: bool) -> ResultState {
    run_ntex(async move {
        let caller = caller_scope(caller_present);
        let state = Arc::new(Observations::default());
        let context_state = state.clone();
        let app = test::init_service(App::new().service(ntex::web::resource("/scope/task").route(
            handle_server_fns_with_context(move || {
                provide_context(Marker(42));
                provide_context(ProbeState(context_state.clone(), Stage::Body));
                let clean = context_state.clone();
                on_cleanup(move || {
                    clean.cleaned.fetch_add(1, Ordering::SeqCst);
                });
            }),
        )))
        .await;
        let response = test::call_service(
            &app,
            test::TestRequest::post().uri("/scope/task").to_request(),
        )
        .await;
        assert_eq!(response.status(), ntex::http::StatusCode::OK);
        let after = marker();
        let body = String::from_utf8(test::read_body(response).await.to_vec()).unwrap();
        let result = collect(&state, after, marker(), body);
        if let Some(caller) = caller {
            caller.unset_with_forced_cleanup();
        }
        result
    })
}
lets_expect! {
    expect(server_task(caller_present)) as spawned_server_work {
        let caller_present = true;
        to preserves_request_context_in_task { preserve_scope(Stage::Body, caller_present, Some(42)) }
        when caller_has_no_owner {
            let caller_present = false;
            to preserves_request_context_in_task { preserve_scope(Stage::Body, caller_present, Some(42)) }
        }
    }
}

fn unscoped_task(change_caller: bool) -> (bool, Option<u8>) {
    run_ntex(async move {
        use server_fn::server::Server;
        let original = caller_scope(change_caller);
        let (sender, receiver) = futures::channel::oneshot::channel();
        <leptos_ntex_unofficial::NtexServerFnBackend as Server<ServerFnError>>::spawn(async move {
            let _ = sender.send((Owner::current().is_some(), marker()));
        })
        .unwrap();
        let caller = change_caller.then(|| {
            let owner = Owner::new_root(None);
            provide_context(Marker(9));
            owner
        });
        let observation = receiver.await.unwrap();
        if let Some(caller) = caller {
            caller.unset_with_forced_cleanup();
        }
        if let Some(original) = original {
            original.unset_with_forced_cleanup();
        }
        observation
    })
}
lets_expect! {
    expect(unscoped_task(change_caller)) as unscoped_backend_task {
        let change_caller = false;
        to does_not_manufacture_an_owner { equal((false, None)) }
        when the_caller_owner_changes_before_poll {
            let change_caller = true;
            to preserves_unscoped_executor_behavior { equal((true, Some(9))) }
        }
    }
}

struct TaskChannels {
    release: Mutex<Option<futures::channel::oneshot::Receiver<()>>>,
    completed: Mutex<Option<futures::channel::oneshot::Sender<String>>>,
}
#[server(name = DetachedContextTask, prefix = "/scope", endpoint = "detached", server = leptos_ntex_unofficial::NtexServerFnBackend)]
async fn detached_context_task() -> Result<(), ServerFnError> {
    use server_fn::server::Server;
    let ProbeState(state, _) = expect_context();
    *state.initial.lock().unwrap() = Some(marker());
    let channels = expect_context::<Arc<TaskChannels>>();
    let release = channels.release.lock().unwrap().take().unwrap();
    let completed = channels.completed.lock().unwrap().take().unwrap();
    let probe = DropProbe::new(state.clone());
    <leptos_ntex_unofficial::NtexServerFnBackend as Server<ServerFnError>>::spawn(async move {
        release.await.unwrap();
        let value = marker();
        *state.resumed.lock().unwrap() = Some(value);
        drop(probe);
        let _ = completed.send(format!("context:{value:?}"));
    })
}
#[derive(Debug)]
struct DetachedResult {
    task: ResultState,
    cleanup_before_release: usize,
}
fn detached_task(caller_present: bool) -> DetachedResult {
    run_ntex(async move {
        let caller = caller_scope(caller_present);
        let state = Arc::new(Observations::default());
        let context_state = state.clone();
        let (release, released) = futures::channel::oneshot::channel();
        let (complete, completed) = futures::channel::oneshot::channel();
        let channels = Arc::new(TaskChannels {
            release: Mutex::new(Some(released)),
            completed: Mutex::new(Some(complete)),
        });
        let app = test::init_service(App::new().service(
            ntex::web::resource("/scope/detached").route(handle_server_fns_with_context(
                move || {
                    provide_context(Marker(42));
                    provide_context(ProbeState(context_state.clone(), Stage::Body));
                    provide_context(channels.clone());
                    let clean = context_state.clone();
                    on_cleanup(move || {
                        clean.cleaned.fetch_add(1, Ordering::SeqCst);
                    });
                },
            )),
        ))
        .await;
        let response = test::call_service(
            &app,
            test::TestRequest::post()
                .uri("/scope/detached")
                .to_request(),
        )
        .await;
        assert_eq!(response.status(), ntex::http::StatusCode::OK);
        assert_eq!(test::read_body(response).await.as_ref(), b"null");
        let before = state.cleaned.load(Ordering::SeqCst);
        let after_poll = marker();
        release.send(()).unwrap();
        let output = completed.await.unwrap();
        let task = collect(&state, after_poll, marker(), output);
        if let Some(caller) = caller {
            caller.unset_with_forced_cleanup();
        }
        DetachedResult {
            task,
            cleanup_before_release: before,
        }
    })
}
fn retain_detached_scope(caller_present: bool) -> impl Fn(&DetachedResult) -> AssertionResult {
    move |actual| {
        preserve_scope(Stage::Body, caller_present, Some(42))(&actual.task)?;
        if actual.cleanup_before_release == 0
            && actual.task.dropped == Some(Some(42))
            && actual.task.stored_on_drop == Some(314)
        {
            Ok(())
        } else {
            Err(AssertionError::new(vec![format!(
                "detached task lease/destructor: {actual:?}"
            )]))
        }
    }
}
lets_expect! {
    expect(detached_task(caller_present)) as detached_server_work {
        let caller_present = true;
        to retains_request_scope_after_response_completion { retain_detached_scope(caller_present) }
        when caller_has_no_owner {
            let caller_present = false;
            to retains_request_scope_after_response_completion { retain_detached_scope(caller_present) }
        }
    }
}

#[server(name = PendingContext, prefix = "/scope", endpoint = "pending", server = leptos_ntex_unofficial::NtexServerFnBackend)]
async fn pending_context() -> Result<(), ServerFnError> {
    let ProbeState(state, _) = expect_context();
    *state.initial.lock().unwrap() = Some(marker());
    let _probe = DropProbe::new(state);
    futures::future::pending().await
}
fn pending_dispatch(caller_present: bool) -> ResultState {
    run_ntex(async move {
        let caller = caller_scope(caller_present);
        let state = Arc::new(Observations::default());
        let context_state = state.clone();
        let app = test::init_service(App::new().service(
            ntex::web::resource("/scope/pending").route(handle_server_fns_with_context(
                move || {
                    provide_context(Marker(42));
                    provide_context(ProbeState(context_state.clone(), Stage::CancelSetup));
                    let clean = context_state.clone();
                    on_cleanup(move || {
                        clean.cleaned.fetch_add(1, Ordering::SeqCst);
                    });
                },
            )),
        ))
        .await;
        let mut request = Box::pin(test::call_service(
            &app,
            test::TestRequest::post().uri("/scope/pending").to_request(),
        ));
        assert!(futures::poll!(request.as_mut()).is_pending());
        assert_eq!(
            *state.initial.lock().unwrap(),
            Some(Some(42)),
            "fixture must reach the pending server function"
        );
        let after_poll = marker();
        drop(request);
        let result = collect(&state, after_poll, marker(), String::new());
        if let Some(caller) = caller {
            caller.unset_with_forced_cleanup();
        }
        result
    })
}
lets_expect! {
    expect(pending_dispatch(caller_present)) as pending_server_dispatch {
        let caller_present = true;
        to destroys_pending_work_in_request_scope { preserve_scope(Stage::CancelSetup, caller_present, Some(42)) }
        when caller_has_no_owner {
            let caller_present = false;
            to destroys_pending_work_in_request_scope { preserve_scope(Stage::CancelSetup, caller_present, Some(42)) }
        }
    }
}

struct StaticSite(std::path::PathBuf);
impl StaticSite {
    fn new() -> Self {
        static NEXT: AtomicUsize = AtomicUsize::new(0);
        loop {
            let path = std::env::temp_dir().join(format!(
                "leptos_request_scope_{}_{}",
                std::process::id(),
                NEXT.fetch_add(1, Ordering::Relaxed)
            ));
            match std::fs::create_dir(&path) {
                Ok(()) => return Self(path),
                Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {}
                Err(error) => panic!("create static scope fixture: {error}"),
            }
        }
    }
}
impl Drop for StaticSite {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}
fn static_generation(caller_present: bool) -> ResultState {
    run_ntex(async move {
        use leptos_ntex_unofficial::{LeptosRoutes, NtexRouteListing, ResponseOptions};
        use leptos_router::{Method, SsrMode, static_routes::StaticRoute};
        use ntex::http::header::{HeaderName, HeaderValue};
        let caller = caller_scope(caller_present);
        let site = StaticSite::new();
        let state = Arc::new(Observations::default());
        let context_state = state.clone();
        let options = LeptosOptions::builder()
            .output_name("static_scope")
            .site_root(site.0.to_string_lossy().into_owned())
            .build();
        let routes = vec![NtexRouteListing::new(
            "/scope-static".into(),
            SsrMode::Static(StaticRoute::new()),
            [Method::Get],
            vec![],
        )];
        let app = test::init_service(App::new().state(options).leptos_routes_with_context(
            routes,
            move || {
                provide_context(Marker(42));
                *context_state.initial.lock().unwrap() = Some(marker());
                let clean = context_state.clone();
                on_cleanup(move || {
                    clean.cleaned.fetch_add(1, Ordering::SeqCst);
                });
                let state = context_state.clone();
                let probe = DropProbe::new(state.clone());
                let response = expect_context::<ResponseOptions>();
                Owner::current_shared_context()
                    .unwrap()
                    .defer_stream(Box::pin(async move {
                        yield_once().await;
                        let value = marker();
                        *state.resumed.lock().unwrap() = Some(value);
                        response.insert_header(
                            HeaderName::from_static("x-context"),
                            HeaderValue::from_str(&format!("context:{value:?}")).unwrap(),
                        );
                        drop(probe);
                    }));
            },
            || view! { <html><head></head><body>"static scope"</body></html> },
        ))
        .await;
        let response = test::call_service(
            &app,
            test::TestRequest::get().uri("/scope-static").to_request(),
        )
        .await;
        assert_eq!(response.status(), ntex::http::StatusCode::OK);
        let context_header = response
            .headers()
            .get("x-context")
            .unwrap()
            .to_str()
            .unwrap()
            .to_owned();
        let after_poll = marker();
        let html = test::read_body(response).await;
        assert!(std::str::from_utf8(&html).unwrap().contains("static scope"));
        let result = collect(&state, after_poll, marker(), context_header);
        if let Some(caller) = caller {
            caller.unset_with_forced_cleanup();
        }
        result
    })
}
lets_expect! {
    expect(static_generation(caller_present)) as static_generation_scope {
        let caller_present = true;
        to keeps_generation_scope_after_deferred_pending { preserve_scope(Stage::Body, caller_present, Some(42)) }
        when caller_has_no_owner {
            let caller_present = false;
            to keeps_generation_scope_after_deferred_pending { preserve_scope(Stage::Body, caller_present, Some(42)) }
        }
    }
}

fn pending_static_parameters(caller_present: bool) -> ResultState {
    run_ntex(async move {
        use leptos_ntex_unofficial::StaticRouteGenerator;
        use leptos_router::{
            Method, RouteList, RouteListing, SsrMode,
            static_routes::{StaticParamsMap, StaticRoute},
        };
        let caller = caller_scope(caller_present);
        let site = StaticSite::new();
        let state = Arc::new(Observations::default());
        let context_state = state.clone();
        let route = StaticRoute::new().prerender_params(|| {
            let state = expect_context::<Arc<Observations>>();
            *state.initial.lock().unwrap() = Some(marker());
            let probe = DropProbe::new(state);
            async move {
                let _probe = probe;
                futures::future::pending::<StaticParamsMap>().await
            }
        });
        let routes = RouteList::from(vec![RouteListing::new(
            [],
            SsrMode::Static(route),
            [Method::Get],
            [],
        )]);
        let generator = StaticRouteGenerator::new(
            &routes,
            || "unused static shell",
            move || {
                provide_context(Marker(42));
                provide_context(context_state.clone());
                let clean = context_state.clone();
                on_cleanup(move || {
                    clean.cleaned.fetch_add(1, Ordering::SeqCst);
                });
            },
        );
        let options = LeptosOptions::builder()
            .output_name("pending_static_parameters")
            .site_root(site.0.to_string_lossy().into_owned())
            .build();
        let mut future = Box::pin(generator.generate(&options));
        assert!(futures::poll!(future.as_mut()).is_pending());
        assert_eq!(
            *state.initial.lock().unwrap(),
            Some(Some(42)),
            "fixture must reach parameter generator"
        );
        let after_poll = marker();
        drop(future);
        let result = collect(&state, after_poll, marker(), String::new());
        if let Some(caller) = caller {
            caller.unset_with_forced_cleanup();
        }
        result
    })
}
lets_expect! {
    expect(pending_static_parameters(caller_present)) as pending_static_parameters {
        let caller_present = true;
        to destroys_parameter_work_in_generation_scope { preserve_scope(Stage::CancelSetup, caller_present, Some(42)) }
        when caller_has_no_owner {
            let caller_present = false;
            to destroys_parameter_work_in_generation_scope { preserve_scope(Stage::CancelSetup, caller_present, Some(42)) }
        }
    }
}
