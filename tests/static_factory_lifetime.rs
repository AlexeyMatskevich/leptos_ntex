//! Generation scope must outlive every stream created before a factory fails.
use futures::{Stream, channel::oneshot};
use leptos::prelude::*;
use leptos_ntex_unofficial::{
    LeptosRoutes, RequestRuntime, generate_route_list_with_exclusions_and_ssg_and_context,
};
use leptos_router::{
    SsrMode,
    components::{Outlet, ParentRoute, Route, Router, Routes},
    path,
    static_routes::StaticRoute,
};
use lets_expect::*;
use ntex::web::{App, test};
use std::{
    path::PathBuf,
    pin::Pin,
    sync::{
        Arc, Mutex,
        atomic::{AtomicUsize, Ordering},
    },
    task::{Context, Poll},
};

#[derive(Clone, Copy, PartialEq)]
enum FactoryOutcome {
    Returns,
    FirstPanics,
    LaterPanics,
}
#[derive(Default)]
struct Events {
    drops: Mutex<Vec<Option<usize>>>,
    cleanups: AtomicUsize,
    unwound: AtomicUsize,
    done: Mutex<Option<oneshot::Sender<()>>>,
}
struct PanicWitness(Arc<Events>);
impl Drop for PanicWitness {
    fn drop(&mut self) {
        if std::thread::panicking() {
            self.0.unwound.fetch_add(1, Ordering::SeqCst);
        }
    }
}
struct Subscription {
    value: StoredValue<usize>,
    events: Arc<Events>,
}
impl Stream for Subscription {
    type Item = ();
    fn poll_next(self: Pin<&mut Self>, _: &mut Context<'_>) -> Poll<Option<()>> {
        Poll::Ready(None)
    }
}
impl Drop for Subscription {
    fn drop(&mut self) {
        self.events
            .drops
            .lock()
            .unwrap()
            .push(self.value.try_get_value());
    }
}
fn factory(events: Arc<Events>, outcome: FactoryOutcome, first: bool) -> Subscription {
    if first {
        let events = events.clone();
        on_cleanup(move || {
            events.cleanups.fetch_add(1, Ordering::SeqCst);
            if let Some(done) = events.done.lock().unwrap().take() {
                let _ = done.send(());
            }
        });
    }
    if (first && outcome == FactoryOutcome::FirstPanics)
        || (!first && outcome == FactoryOutcome::LaterPanics)
    {
        let _witness = PanicWitness(events);
        panic!("controlled regeneration factory panic");
    }
    Subscription {
        value: StoredValue::new(7),
        events,
    }
}
fn shell(events: Arc<Events>, outcome: FactoryOutcome) -> impl IntoView {
    let parent = events.clone();
    view! { <Router><Routes fallback=|| "missing">
        <ParentRoute path=path!("/parent") view=Outlet
            ssr=SsrMode::Static(StaticRoute::new().regenerate(move |_| factory(parent.clone(),outcome,true)))>
            <Route path=path!("/child") view=|| "nested static page"
                ssr=SsrMode::Static(StaticRoute::new().regenerate(move |_| factory(events.clone(),outcome,false)))/>
        </ParentRoute>
    </Routes></Router> }
}
struct Root(PathBuf);
impl Root {
    fn new() -> Self {
        static NEXT: AtomicUsize = AtomicUsize::new(0);
        let path = std::env::temp_dir().join(format!(
            "static_factory_scope_{}_{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        std::fs::create_dir(&path).unwrap();
        Self(path)
    }
}
impl Drop for Root {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}
fn observe(startup: bool, outcome: FactoryOutcome) -> (Vec<Option<usize>>, usize, usize) {
    let root = Root::new();
    let events = Arc::new(Events::default());
    let observed = events.clone();
    let (done, complete) = oneshot::channel();
    *events.done.lock().unwrap() = Some(done);
    let options = LeptosOptions::builder()
        .output_name("factory_lifetime")
        .site_root(root.0.to_string_lossy().into_owned())
        .build();
    let run = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        ntex::rt::System::build()
            .testing()
            .build(RequestRuntime::new(ntex::rt::DefaultRuntime))
            .block_on(async move {
                ntex::time::timeout(ntex::time::Millis(5000), async move {
                    let app_fn = move || shell(events.clone(), outcome);
                    let (routes, generator) =
                        generate_route_list_with_exclusions_and_ssg_and_context(
                            app_fn.clone(),
                            None,
                            || {},
                        );
                    if startup {
                        generator.generate(&options).await;
                    } else {
                        drop(generator);
                        let app = test::init_service(
                            App::new().state(options).leptos_routes(routes, app_fn),
                        )
                        .await;
                        let response = test::call_service(
                            &app,
                            test::TestRequest::with_uri("/parent/child").to_request(),
                        )
                        .await;
                        let _ = test::read_body(response).await;
                    }
                    complete.await.expect("generation owner completes cleanup");
                })
                .await
                .expect("factory scenario must make progress");
            });
    }));
    if let Err(payload) = run {
        assert_eq!(
            payload.downcast_ref::<&str>().copied(),
            Some("controlled regeneration factory panic"),
            "timeout or unrelated panic is not the expected failure"
        );
    }
    let drops = observed.drops.lock().unwrap().clone();
    (
        drops,
        observed.cleanups.load(Ordering::SeqCst),
        observed.unwound.load(Ordering::SeqCst),
    )
}
lets_expect! {
    expect(observe(startup,outcome)) as regeneration_factory_lifetime {
        let startup=true; let outcome=FactoryOutcome::Returns;
        to drops_both_streams_before_cleanup { equal((vec![Some(7),Some(7)],1,0)) }
        when first_factory_panics { let outcome=FactoryOutcome::FirstPanics;
            to cleans_without_creating_a_stream { equal((vec![],1,1)) }
        }
        when later_factory_panics { let outcome=FactoryOutcome::LaterPanics;
            to preserves_the_created_stream_until_its_drop { equal((vec![Some(7)],1,1)) }
        }
        when generation_is_on_demand {
            let startup=false;
            to drops_both_streams_before_cleanup { equal((vec![Some(7),Some(7)],1,0)) }
            when first_factory_panics { let outcome=FactoryOutcome::FirstPanics;
                to cleans_without_creating_a_stream { equal((vec![],1,1)) }
            }
            when later_factory_panics { let outcome=FactoryOutcome::LaterPanics;
                to preserves_the_created_stream_until_its_drop { equal((vec![Some(7)],1,1)) }
            }
        }
    }
}
