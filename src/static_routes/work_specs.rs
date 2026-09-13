use super::*;
use crate::LeptosRoutes;
use futures::{
    Future, Stream,
    channel::{mpsc, oneshot},
};
use leptos::prelude::*;
use leptos_router::{
    SsrMode,
    components::{Route as LeptosRoute, Router, Routes},
    path,
    static_routes::StaticRoute,
};
use lets_expect::*;
use ntex::web::{App, test};
use std::{
    pin::Pin,
    sync::atomic::AtomicUsize,
    task::{Context, Poll},
};

fn run<T: 'static>(future: impl Future<Output = T> + 'static) -> T {
    crate::tests::run_ntex(async move {
        match futures::future::select(
            Box::pin(future),
            Box::pin(ntex::time::sleep(std::time::Duration::from_secs(10))),
        )
        .await
        {
            futures::future::Either::Left((value, _)) => value,
            futures::future::Either::Right(_) => {
                panic!("TECHNICAL DEADLINE: static work fixture did not finish")
            }
        }
    })
}

fn options(root: &Path) -> LeptosOptions {
    LeptosOptions::builder()
        .output_name("static_work_policy")
        .site_root(root.to_string_lossy().into_owned())
        .build()
}

#[derive(Default)]
struct Signals {
    created: AtomicUsize,
    active: AtomicUsize,
    senders: Mutex<Vec<mpsc::UnboundedSender<()>>>,
    dropped: Mutex<Option<oneshot::Sender<()>>>,
}
impl Signals {
    fn subscribe(self: &Arc<Self>) -> Subscription {
        let (sender, receiver) = mpsc::unbounded();
        self.senders.lock().unwrap().push(sender);
        self.created.fetch_add(1, Ordering::SeqCst);
        self.active.fetch_add(1, Ordering::SeqCst);
        Subscription {
            receiver,
            signals: self.clone(),
        }
    }
    fn close(&self) {
        self.senders.lock().unwrap().clear();
    }
}
struct Subscription {
    receiver: mpsc::UnboundedReceiver<()>,
    signals: Arc<Signals>,
}
impl Stream for Subscription {
    type Item = ();
    fn poll_next(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<()>> {
        Pin::new(&mut self.receiver).poll_next(cx)
    }
}
impl Drop for Subscription {
    fn drop(&mut self) {
        self.signals.active.fetch_sub(1, Ordering::SeqCst);
        if let Some(sender) = self.signals.dropped.lock().unwrap().take() {
            let _ = sender.send(());
        }
    }
}
struct CloseSubscriptions(Arc<Signals>);
impl Drop for CloseSubscriptions {
    fn drop(&mut self) {
        self.0.close();
    }
}

fn shell(
    renders: Arc<AtomicUsize>,
    signals: Arc<Signals>,
    subscribed: bool,
    large: bool,
) -> impl IntoView {
    let route = if subscribed {
        StaticRoute::new().regenerate(move |_| signals.subscribe())
    } else {
        StaticRoute::new()
    };
    view! {
        <Router><Routes fallback=|| "missing">
            <LeptosRoute path=path!("/pages/*tail") ssr=SsrMode::Static(route)
                view=move || { renders.fetch_add(1, Ordering::SeqCst); if large { "x".repeat(8192) } else { "managed static page".to_owned() } }/>
        </Routes></Router>
    }
}
#[derive(Clone, Copy)]
enum RequestCase {
    Admitted,
    RenderFull,
    WaiterFull,
    SubscriptionFull,
    Cached,
    StorageFull,
    Unbound,
}
async fn requests(case: RequestCase) -> (Vec<StatusCode>, usize, bool, usize) {
    let root = crate::tests::temp_site_root("static_policy_requests");
    let renders = Arc::new(AtomicUsize::new(0));
    let signals = Arc::new(Signals::default());
    let _close = CloseSubscriptions(signals.clone());
    fs::write(root.join("asset.txt"), "existing site asset").unwrap();
    if matches!(case, RequestCase::Cached) {
        fs::create_dir(root.join("pages")).unwrap();
        fs::write(root.join("pages/one.html"), "cached one").unwrap();
        fs::write(root.join("pages/two.html"), "cached two").unwrap();
    }
    let work = match case {
        RequestCase::RenderFull => StaticWorkLimits::new().with_renders(0),
        RequestCase::WaiterFull => StaticWorkLimits::new().with_waiters(0),
        RequestCase::SubscriptionFull => StaticWorkLimits::new().with_subscriptions(1),
        RequestCase::Cached => StaticWorkLimits::new()
            .with_renders(0)
            .with_waiters(0)
            .with_subscriptions(0),
        _ => StaticWorkLimits::new().with_renders(1).with_waiters(1),
    };
    let storage = if matches!(case, RequestCase::StorageFull) {
        StaticStorageLimits::new().with_logical_file_bytes(1024)
    } else {
        StaticStorageLimits::new()
    };
    let policy = StaticRoutePolicy::open(&*root, storage, work)
        .await
        .unwrap();
    let app_fn = {
        let renders = renders.clone();
        let signals = signals.clone();
        move || {
            shell(
                renders.clone(),
                signals.clone(),
                matches!(case, RequestCase::SubscriptionFull),
                matches!(case, RequestCase::StorageFull),
            )
        }
    };
    let mut routes = crate::tests::gen_route_list(app_fn.clone());
    if matches!(case, RequestCase::SubscriptionFull) {
        let regeneration = routes[0].regenerate[0].clone();
        routes[0].regenerate.push(regeneration);
    }
    if !matches!(case, RequestCase::Unbound) {
        policy.configure_routes(&routes).unwrap();
    }
    renders.store(0, Ordering::SeqCst);
    let app = test::init_service(
        App::new()
            .state(options(&root))
            .leptos_routes(routes, app_fn),
    )
    .await;
    let mut statuses = Vec::new();
    for path in ["/pages/one", "/pages/two"] {
        let response =
            test::call_service(&app, test::TestRequest::with_uri(path).to_request()).await;
        statuses.push(response.status());
        let _ = test::read_body(response).await;
    }
    let preserved = fs::read_to_string(root.join("asset.txt")).unwrap() == "existing site asset";
    let created = signals.created.load(Ordering::SeqCst);
    signals.close();
    futures::future::poll_fn(|cx| {
        if signals.active.load(Ordering::SeqCst) == 0 {
            Poll::Ready(())
        } else {
            cx.waker().wake_by_ref();
            Poll::Pending
        }
    })
    .await;
    (statuses, renders.load(Ordering::SeqCst), preserved, created)
}

fn startup_shell(signals: Arc<Signals>, subscribed: bool, dynamic: bool) -> impl IntoView {
    let route = if subscribed {
        StaticRoute::new().regenerate(move |_| signals.subscribe())
    } else {
        StaticRoute::new()
    };
    view! {
        <Router><Routes fallback=|| "missing">
            <LeptosRoute path=path!("/one") ssr=SsrMode::Static(route.clone()) view=|| "one"/>
            <LeptosRoute path=path!("/two") ssr=SsrMode::Static(route) view=|| "two"/>
            <LeptosRoute path=path!("/dynamic") ssr=if dynamic { SsrMode::Async } else { SsrMode::Static(StaticRoute::new()) } view=|| "dynamic"/>
        </Routes></Router>
    }
}
#[derive(Clone, Copy)]
enum StartupCase {
    Admitted,
    Partial,
    Nonstatic,
}
async fn startup(case: StartupCase) -> (usize, usize, bool) {
    let root = crate::tests::temp_site_root("static_policy_startup");
    let signals = Arc::new(Signals::default());
    let close = CloseSubscriptions(signals.clone());
    let policy = StaticRoutePolicy::open(
        &*root,
        StaticStorageLimits::new(),
        StaticWorkLimits::new()
            .with_renders(1)
            .with_subscriptions(1)
            .with_waiters(1),
    )
    .await
    .unwrap();
    let app_fn = {
        let signals = signals.clone();
        move || {
            startup_shell(
                signals.clone(),
                matches!(case, StartupCase::Partial),
                !matches!(case, StartupCase::Admitted),
            )
        }
    };
    let (_, generator) = crate::tests::gen_route_list_with_ssg(app_fn);
    let result = generator
        .with_static_policy(policy)
        .unwrap()
        .try_generate(&options(&root))
        .await;
    let (completed, failures) = match result {
        Ok(report) => (report.completed, 0),
        Err(error) => {
            assert!(error.failures().iter().all(|(_, error)| matches!(
                error,
                StaticPolicyError::WorkCapacity {
                    resource: "subscriptions",
                    ..
                }
            )));
            (error.completed(), error.failures().len())
        }
    };
    let retained = root.join("one.html").is_file()
        && (matches!(case, StartupCase::Partial) || root.join("two.html").is_file())
        && (!matches!(case, StartupCase::Nonstatic) || !root.join("dynamic.html").exists());
    if signals.active.load(Ordering::SeqCst) != 0 {
        let (sender, receiver) = oneshot::channel();
        *signals.dropped.lock().unwrap() = Some(sender);
        drop(close);
        receiver.await.unwrap();
    }
    (completed, failures, retained)
}

/// A listing composed by hand and a generator built separately share the
/// process runtime, so binding the policy on either side governs both.
async fn hand_composed(bind_generator: bool) -> (Vec<StatusCode>, usize) {
    let root = crate::tests::temp_site_root("static_policy_hand_composed");
    let renders = Arc::new(AtomicUsize::new(0));
    let signals = Arc::new(Signals::default());
    let _close = CloseSubscriptions(signals.clone());
    let policy = StaticRoutePolicy::open(
        &*root,
        StaticStorageLimits::new(),
        StaticWorkLimits::new().with_renders(1),
    )
    .await
    .unwrap();
    let app_fn = {
        let renders = renders.clone();
        let signals = signals.clone();
        move || shell(renders.clone(), signals.clone(), false, false)
    };
    let routes = vec![crate::NtexRouteListing::new(
        "/pages/{tail}*".to_owned(),
        SsrMode::Static(StaticRoute::new()),
        [leptos_router::Method::Get],
        vec![],
    )];
    let generator = StaticRouteGenerator::new(&RouteList::new(), app_fn.clone(), || {});
    if bind_generator {
        generator.with_static_policy(policy).unwrap();
    } else {
        policy.configure_routes(&routes).unwrap();
    }
    let app = test::init_service(
        App::new()
            .state(options(&root))
            .leptos_routes(routes, app_fn),
    )
    .await;
    let mut statuses = Vec::new();
    for path in ["/pages/one", "/pages/one"] {
        let response =
            test::call_service(&app, test::TestRequest::with_uri(path).to_request()).await;
        statuses.push(response.status());
        let _ = test::read_body(response).await;
    }
    (statuses, renders.load(Ordering::SeqCst))
}

lets_expect! {
    expect(run(hand_composed(bind_generator))) as hand_composed_managed_listing {
        let bind_generator = true;
        to publishes_under_the_policy_bound_through_the_generator { equal((vec![StatusCode::OK; 2], 1)) }
        when the_policy_is_bound_through_the_listing {
            let bind_generator = false;
            to publishes_under_the_policy { equal((vec![StatusCode::OK; 2], 1)) }
        }
    }
}

lets_expect! {
    expect(run(requests(case))) as managed_static_request {
        let case = RequestCase::Admitted;
        to publishes_successive_misses_with_reusable_capacity { equal((vec![StatusCode::OK; 2], 2, true, 0)) }
        when render_capacity_is_zero {
            let case = RequestCase::RenderFull;
            to refuses_without_invoking_the_factory { equal((vec![StatusCode::SERVICE_UNAVAILABLE; 2], 0, true, 0)) }
        }
        when waiter_capacity_is_zero {
            let case = RequestCase::WaiterFull;
            to refuses_without_invoking_the_factory { equal((vec![StatusCode::SERVICE_UNAVAILABLE; 2], 0, true, 0)) }
        }
        when subscription_demand_exceeds_capacity {
            let case = RequestCase::SubscriptionFull;
            to refuses_without_constructing_subscriptions { equal((vec![StatusCode::SERVICE_UNAVAILABLE; 2], 0, true, 0)) }
        }
        when cached_artifacts_exist_with_zero_work_capacity {
            let case = RequestCase::Cached;
            to serves_the_cached_artifacts { equal((vec![StatusCode::OK; 2], 0, true, 0)) }
        }
        when publication_exceeds_storage_capacity {
            let case = RequestCase::StorageFull;
            to refuses_and_preserves_existing_data { equal((vec![StatusCode::SERVICE_UNAVAILABLE; 2], 2, true, 0)) }
        }
        when the_publisher_is_not_bound_to_the_stored_policy {
            let case = RequestCase::Unbound;
            to reports_the_configuration_fault { equal((vec![StatusCode::INTERNAL_SERVER_ERROR; 2], 2, true, 0)) }
        }
    }
    expect(run(startup(case))) as managed_static_startup {
        let case = StartupCase::Admitted;
        to reports_completed_paths { equal((3, 0, true)) }
        when a_later_path_exhausts_subscription_capacity {
            let case = StartupCase::Partial;
            to reports_typed_partial_failure_and_retains_completed_artifacts { equal((1, 1, true)) }
        }
        when a_listing_is_not_static {
            let case = StartupCase::Nonstatic;
            to skips_it_without_failure { equal((2, 0, true)) }
        }
    }
}

async fn cancelled_render(waiter_only: bool) -> (bool, bool) {
    ensure_executor_initialized();
    let root = crate::tests::temp_site_root("static_policy_cancellation");
    let policy = StaticRoutePolicy::open(
        &*root,
        StaticStorageLimits::new(),
        StaticWorkLimits::new().with_renders(1).with_waiters(1),
    )
    .await
    .unwrap();
    let runtime = Arc::new(StaticRuntime::default());
    policy.bind(&runtime).unwrap();
    let (started_tx, started_rx) = oneshot::channel();
    let started_tx = Arc::new(Mutex::new(Some(started_tx)));
    let (release_tx, release_rx) = oneshot::channel::<()>();
    let release_rx = Arc::new(Mutex::new(Some(release_rx)));
    let (cleaned_tx, cleaned_rx) = oneshot::channel();
    let cleaned_tx = Arc::new(Mutex::new(Some(cleaned_tx)));
    let app_fn = move || {
        let started = started_tx.lock().unwrap().take().unwrap();
        let release = release_rx.lock().unwrap().take().unwrap();
        Suspend::new(async move {
            let _ = started.send(());
            let _ = release.await;
            "released"
        })
    };
    let context = move || {
        let cleaned = cleaned_tx.lock().unwrap().take().unwrap();
        on_cleanup(move || {
            let _ = cleaned.send(());
        });
    };
    let mut pending = Box::pin(runtime.render(
        options(&root),
        "/pending".into(),
        app_fn,
        context,
        Vec::new(),
    ));
    match futures::future::select(pending.as_mut(), started_rx).await {
        futures::future::Either::Right((Ok(()), _)) => {}
        _ => panic!("render must reach its controlled suspension"),
    }
    let occupied = if waiter_only {
        policy.waiter().is_err()
    } else {
        policy.render().is_err()
    };
    drop(pending);
    let waiter_reusable = policy.waiter().is_ok();
    cleaned_rx.await.unwrap();
    policy.wait_for_render().await;
    let reusable = if waiter_only {
        waiter_reusable
    } else {
        policy.render().is_ok()
    };
    drop(release_tx);
    (occupied, reusable)
}

struct ResumeOnDrop(Option<std::sync::mpsc::Sender<()>>);
impl ResumeOnDrop {
    fn resume(&mut self) {
        if let Some(sender) = self.0.take() {
            let _ = sender.send(());
        }
    }
}
impl Drop for ResumeOnDrop {
    fn drop(&mut self) {
        self.resume();
    }
}
async fn canceled_publication() -> (bool, bool) {
    let root = crate::tests::temp_site_root("static_policy_blocking");
    let options = options(&root);
    let path = root.join("page.html");
    let policy = StaticRoutePolicy::open(
        &*root,
        StaticStorageLimits::new(),
        StaticWorkLimits::new().with_renders(1),
    )
    .await
    .unwrap();
    let (started_tx, started_rx) = oneshot::channel();
    let (resume_tx, resume_rx) = std::sync::mpsc::channel();
    let mut resume = ResumeOnDrop(Some(resume_tx));
    let keep_root = root.clone();
    let _digest = test_hooks::on_digest(
        path.clone(),
        Box::new(move || {
            let _root = keep_root;
            let _ = started_tx.send(());
            resume_rx.recv().map_err(io::Error::other)
        }),
    );
    let (committed_tx, committed_rx) = oneshot::channel();
    let _published = test_hooks::on_publication(
        path.clone(),
        Box::new(move || {
            let _ = committed_tx.send(());
            Ok(())
        }),
    );
    let mut pending = Box::pin(write_static_route_managed(
        &options,
        None,
        "/page",
        "written".into(),
        Some(policy.clone()),
        Some(policy.render().unwrap()),
    ));
    match futures::future::select(pending.as_mut(), started_rx).await {
        futures::future::Either::Right((Ok(()), _)) => {}
        _ => panic!("writer must reach its controlled suspension"),
    }
    drop(pending);
    let occupied = policy.render().is_err();
    resume.resume();
    committed_rx.await.unwrap();
    let read_root = root.to_path_buf();
    let valid = ntex::rt::spawn_blocking(move || {
        // The first HTML inode does not exist at the metadata hook yet. Wait
        // for that publication lock before asking the reader to canonicalize it.
        let root = crate::fs_boundary::SiteRoot::open(&read_root).unwrap();
        let (dir, _) = root.parent(&path, false).unwrap();
        drop(publication_lock(&dir, false).unwrap());
        read_paired_static_file(&read_root, &path).is_ok()
    })
    .await
    .unwrap();
    policy.wait_for_render().await;
    (occupied, valid && policy.render().is_ok())
}

struct Live {
    root: crate::tests::TempSiteRoot,
    policy: StaticRoutePolicy,
    runtime: Arc<StaticRuntime>,
    signals: Arc<Signals>,
    renders: Arc<AtomicUsize>,
    _close: CloseSubscriptions,
}
async fn live() -> Live {
    let root = crate::tests::temp_site_root("static_policy_live");
    let signals = Arc::new(Signals::default());
    let close = CloseSubscriptions(signals.clone());
    let renders = Arc::new(AtomicUsize::new(0));
    let policy = StaticRoutePolicy::open(
        &*root,
        StaticStorageLimits::new(),
        StaticWorkLimits::new()
            .with_renders(1)
            .with_subscriptions(1),
    )
    .await
    .unwrap();
    let app_fn = {
        let renders = renders.clone();
        let signals = signals.clone();
        move || shell(renders.clone(), signals.clone(), true, false)
    };
    let routes = crate::tests::gen_route_list(app_fn.clone());
    policy.configure_routes(&routes).unwrap();
    let runtime = routes[0].runtime.clone();
    renders.store(0, Ordering::SeqCst);
    runtime
        .render(
            options(&root),
            "/pages/live".into(),
            app_fn,
            || {},
            routes[0].regenerate.clone(),
        )
        .await
        .unwrap();
    Live {
        root,
        policy,
        runtime,
        signals,
        renders,
        _close: close,
    }
}
async fn subscription_eof() -> (bool, bool) {
    let live = live().await;
    let occupied = live.policy.subscribe(1).is_err();
    let (sender, receiver) = oneshot::channel();
    *live.signals.dropped.lock().unwrap() = Some(sender);
    live.signals.close();
    receiver.await.unwrap();
    (occupied, live.policy.subscribe(1).is_ok())
}
async fn pending_regeneration() -> (bool, usize, bool) {
    let live = live().await;
    let held = live.policy.render().unwrap();
    live.signals.senders.lock().unwrap()[0]
        .unbounded_send(())
        .unwrap();
    futures::future::poll_fn(|cx| {
        let blocked = live
            .runtime
            .work
            .lock()
            .unwrap()
            .values()
            .filter_map(std::sync::Weak::upgrade)
            .any(|work| work.state.lock().unwrap().blocked_render);
        if blocked {
            Poll::Ready(())
        } else {
            cx.waker().wake_by_ref();
            Poll::Pending
        }
    })
    .await;
    let mut new_waiter = Box::pin(live.runtime.render(
        options(&live.root),
        "/pages/live".into(),
        || "unexpected",
        || {},
        Vec::new(),
    ));
    let refused = futures::future::poll_fn(|cx| {
        if let Poll::Ready(result) = new_waiter.as_mut().poll(cx) {
            return Poll::Ready(matches!(
                result,
                Err(StaticPolicyError::WorkCapacity {
                    resource: "renders",
                    ..
                })
            ));
        }
        let accepted = live
            .runtime
            .work
            .lock()
            .unwrap()
            .values()
            .filter_map(std::sync::Weak::upgrade)
            .any(|work| {
                work.state
                    .lock()
                    .unwrap()
                    .waiters
                    .iter()
                    .any(|waiter| !waiter.is_canceled())
            });
        if accepted {
            Poll::Ready(false)
        } else {
            cx.waker().wake_by_ref();
            Poll::Pending
        }
    })
    .await;
    drop(new_waiter);
    let (published_tx, published_rx) = oneshot::channel();
    let path = live.root.join("pages/live.html");
    let _hook = test_hooks::on_publication(
        path.clone(),
        Box::new(move || {
            let _ = published_tx.send(());
            Ok(())
        }),
    );
    drop(held);
    published_rx.await.unwrap();
    let root = live.root.to_path_buf();
    let valid = ntex::rt::spawn_blocking(move || read_paired_static_file(&root, &path).is_ok())
        .await
        .unwrap();
    let renders = live.renders.load(Ordering::SeqCst);
    let (sender, receiver) = oneshot::channel();
    *live.signals.dropped.lock().unwrap() = Some(sender);
    live.signals.close();
    receiver.await.unwrap();
    (refused, renders, valid)
}

lets_expect! {
    expect(run(cancelled_render(waiter_only))) as canceled_static_caller {
        let waiter_only = true;
        to returns_its_waiter_capacity { equal((true, true)) }
        when its_abandoned_render_is_cleaned_up {
            let waiter_only = false;
            to returns_its_render_capacity { equal((true, true)) }
        }
    }
    expect(run(canceled_publication())) as canceled_static_publication {
        to retains_render_capacity_until_the_blocking_writer_finishes { equal((true, true)) }
    }
    expect(run(subscription_eof())) as completed_static_subscription {
        to returns_its_stream_capacity { equal((true, true)) }
    }
    expect(run(pending_regeneration())) as static_refresh_waiting_for_capacity {
        to refuses_new_waiters_and_resumes_the_retained_refresh { equal((true, 2, true)) }
    }
}

async fn default_failed_publication() -> (StatusCode, usize, usize) {
    let root = crate::tests::temp_site_root("default_static_io_failure");
    let signals = Arc::new(Signals::default());
    let _close = CloseSubscriptions(signals.clone());
    let renders = Arc::new(AtomicUsize::new(0));
    let app_fn = {
        let signals = signals.clone();
        move || shell(renders.clone(), signals.clone(), true, false)
    };
    let routes = crate::tests::gen_route_list(app_fn.clone());
    let _failure = test_hooks::on_digest(
        root.join("pages/live.html"),
        Box::new(|| Err(io::Error::other("controlled initial publication failure"))),
    );
    let app = test::init_service(
        App::new()
            .state(options(&root))
            .leptos_routes(routes, app_fn),
    )
    .await;
    let response = test::call_service(
        &app,
        test::TestRequest::with_uri("/pages/live").to_request(),
    )
    .await;
    let status = response.status();
    let _ = test::read_body(response).await;
    let created = signals.created.load(Ordering::SeqCst);
    signals.close();
    futures::future::poll_fn(|cx| {
        if signals.active.load(Ordering::SeqCst) == 0 {
            Poll::Ready(())
        } else {
            cx.waker().wake_by_ref();
            Poll::Pending
        }
    })
    .await;
    (status, created, signals.active.load(Ordering::SeqCst))
}
lets_expect! {
    expect(run(default_failed_publication())) as default_static_publication {
        when initial_disk_write_fails {
            to returns_500_and_retains_regeneration_until_eof { equal((StatusCode::INTERNAL_SERVER_ERROR, 1, 0)) }
        }
    }
}

async fn rejected_refresh_factory() -> (bool, usize, usize, bool) {
    let live = live().await;
    let held = live.policy.render().unwrap();
    let rejected = Arc::new(AtomicUsize::new(0));
    let rejected_factory = {
        let rejected = rejected.clone();
        move || {
            rejected.fetch_add(1, Ordering::SeqCst);
            "refused factory"
        }
    };
    let refused = matches!(
        live.runtime
            .render(
                options(&live.root),
                "/pages/live".into(),
                rejected_factory,
                || {},
                Vec::new()
            )
            .await,
        Err(StaticPolicyError::WorkCapacity {
            resource: "renders",
            ..
        })
    );
    let (published_tx, published_rx) = oneshot::channel();
    let path = live.root.join("pages/live.html");
    let _publication = test_hooks::on_publication(
        path.clone(),
        Box::new(move || {
            let _ = published_tx.send(());
            Ok(())
        }),
    );
    drop(held);
    live.signals.senders.lock().unwrap()[0]
        .unbounded_send(())
        .unwrap();
    published_rx.await.unwrap();
    let root = live.root.to_path_buf();
    let valid = ntex::rt::spawn_blocking(move || read_paired_static_file(&root, &path).is_ok())
        .await
        .unwrap();
    live.signals.close();
    futures::future::poll_fn(|cx| {
        if live.signals.active.load(Ordering::SeqCst) == 0 {
            Poll::Ready(())
        } else {
            cx.waker().wake_by_ref();
            Poll::Pending
        }
    })
    .await;
    (
        refused,
        live.renders.load(Ordering::SeqCst),
        rejected.load(Ordering::SeqCst),
        valid,
    )
}
lets_expect! {
    expect(run(rejected_refresh_factory())) as rejected_static_refresh_request {
        when render_capacity_is_exhausted {
            to uses_the_previous_factory_for_the_next_isr_event { equal((true, 2, 0, true)) }
        }
    }
}
