//! Render templates for successive generations of one static representation.
use futures::{
    StreamExt,
    channel::{mpsc, oneshot},
};
use leptos::prelude::*;
use leptos_ntex_unofficial::{
    LeptosRoutes, Request, generate_route_list_with_exclusions_and_ssg_and_context,
};
use leptos_router::{
    SsrMode,
    components::{Route, Router, Routes},
    path,
    static_routes::StaticRoute,
};
use lets_expect::*;
use ntex::{
    http::StatusCode,
    web::{App, test},
};
use std::sync::{
    Arc, Mutex,
    atomic::{AtomicUsize, Ordering},
};

#[derive(Clone, Copy)]
struct Marker(u8);
#[derive(Default)]
struct Signals {
    subscriptions: AtomicUsize,
    senders: Mutex<Vec<mpsc::Sender<()>>>,
    settled: Mutex<Option<oneshot::Sender<()>>>,
}
impl Signals {
    fn subscribe(self: &Arc<Self>) -> impl futures::Stream<Item = ()> + Send + use<> {
        self.subscriptions.fetch_add(1, Ordering::SeqCst);
        let (sender, receiver) = mpsc::channel(1);
        self.senders.lock().unwrap().push(sender);
        let signals = self.clone();
        futures::stream::unfold(receiver, move |mut receiver| {
            let signals = signals.clone();
            async move {
                if let Some(settled) = signals.settled.lock().unwrap().take() {
                    let _ = settled.send(());
                }
                receiver.next().await.map(|()| ((), receiver))
            }
        })
    }
}
fn shell(source: &'static str, signals: Arc<Signals>) -> impl IntoView {
    view! { <Router><Routes fallback=|| "missing">
        <Route path=path!("/template") ssr=SsrMode::Static(StaticRoute::new().regenerate(move |_| signals.subscribe()))
            view=move || format!("marker:{:?};method:{:?};source:{source};path:{}", use_context::<Marker>().map(|m| m.0), use_context::<leptos_router::Method>(), expect_context::<Request>().with(|request| request.path().to_owned()).unwrap())/>
    </Routes></Router> }
}
struct Root(std::path::PathBuf);
impl Root {
    fn new() -> Self {
        static NEXT: AtomicUsize = AtomicUsize::new(0);
        let path = std::env::temp_dir().join(format!(
            "static_template_{}_{}",
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
#[derive(Debug)]
struct Observation {
    requested: String,
    regenerated: String,
    subscriptions: usize,
}
async fn observe(startup: bool) -> Observation {
    let root = Root::new();
    let options = LeptosOptions::builder()
        .output_name("template")
        .site_root(root.0.to_string_lossy().into_owned())
        .build();
    let signals = Arc::new(Signals::default());
    let initial = {
        let signals = signals.clone();
        move || shell("initial", signals.clone())
    };
    let (routes, generator) =
        generate_route_list_with_exclusions_and_ssg_and_context(initial.clone(), None, || {
            provide_context(Marker(10))
        });
    if startup {
        generator.generate(&options).await;
    } else {
        drop(generator);
        let app = test::init_service(
            App::new()
                .state(options.clone())
                .leptos_routes_with_context(
                    routes.clone(),
                    || provide_context(Marker(10)),
                    initial,
                ),
        )
        .await;
        let response =
            test::call_service(&app, test::TestRequest::with_uri("/template").to_request()).await;
        assert_eq!(response.status(), StatusCode::OK, "initial render");
        let _ = test::read_body(response).await;
    }
    std::fs::remove_file(root.0.join("template.html")).unwrap();
    let updated = {
        let signals = signals.clone();
        move || shell("updated", signals.clone())
    };
    let app = test::init_service(App::new().state(options).leptos_routes_with_context(
        routes,
        || provide_context(Marker(20)),
        updated,
    ))
    .await;
    let response =
        test::call_service(&app, test::TestRequest::with_uri("/template").to_request()).await;
    assert_eq!(response.status(), StatusCode::OK, "subsequent render");
    let requested = String::from_utf8(test::read_body(response).await.to_vec()).unwrap();
    let (settled_tx, settled) = oneshot::channel();
    *signals.settled.lock().unwrap() = Some(settled_tx);
    for sender in signals.senders.lock().unwrap().iter_mut() {
        sender.try_send(()).unwrap();
    }
    settled
        .await
        .expect("subscription polls again after publishing its trigger");
    let regenerated = std::fs::read_to_string(root.0.join("template.html")).unwrap();
    Observation {
        requested,
        regenerated,
        subscriptions: signals.subscriptions.load(Ordering::SeqCst),
    }
}
fn run(startup: bool) -> Observation {
    ntex::rt::System::build()
        .testing()
        .build(leptos_ntex_unofficial::RequestRuntime::new(
            ntex::rt::DefaultRuntime,
        ))
        .block_on(async move {
            ntex::time::timeout(ntex::time::Millis(5000), observe(startup))
                .await
                .expect("generation scenario must progress")
        })
}
fn uses_updated_template(body: &String) -> AssertionResult {
    let expected = "marker:Some(20);method:Some(Get);source:updated;path:/template";
    if body.contains(expected) {
        Ok(())
    } else {
        Err(AssertionError::new(vec![format!(
            "expected rendered template {expected:?}, got {body:?}"
        )]))
    }
}
lets_expect! {
    expect(run(startup)) as static_render_template {
        let startup=true;
        to uses_the_new_template_for_a_cache_miss { have(requested) uses_updated_template }
        to uses_the_new_template_for_regeneration { have(regenerated) uses_updated_template }
        to keeps_one_subscription { have(subscriptions) equal(1usize) }
        when first_generation_is_on_demand {
            let startup=false;
            to uses_the_new_template_for_a_cache_miss { have(requested) uses_updated_template }
            to uses_the_new_template_for_regeneration { have(regenerated) uses_updated_template }
            to keeps_one_subscription { have(subscriptions) equal(1usize) }
        }
    }
}
