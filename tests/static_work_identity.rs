//! Identity of long-lived static work across filesystem publication.

use futures::{
    Stream,
    channel::{mpsc, oneshot},
};
use leptos::prelude::*;
use leptos_ntex_unofficial::{
    LeptosRoutes, NtexRouteListing, Request, RequestRuntime, generate_route_list,
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
use std::{
    path::PathBuf,
    pin::Pin,
    sync::{
        Arc, Mutex, OnceLock,
        atomic::{AtomicUsize, Ordering},
    },
    task::{Context, Poll},
};

struct Root(PathBuf);
impl Root {
    fn new(relative: bool, existing: bool) -> Self {
        static NEXT: AtomicUsize = AtomicUsize::new(0);
        let name = format!(
            "static_work_identity_{}_{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        );
        let path = if relative {
            PathBuf::from(name)
        } else {
            std::env::temp_dir().join(name)
        };
        assert!(!path.exists(), "fixture root must be unique");
        if existing {
            std::fs::create_dir(&path).unwrap();
        }
        Self(path)
    }
}
impl Drop for Root {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

#[derive(Default)]
struct Signals {
    revision: AtomicUsize,
    senders: Mutex<Vec<(String, mpsc::Sender<()>)>>,
    labels: Mutex<Vec<String>>,
    settled: Mutex<Option<oneshot::Sender<()>>>,
    active: AtomicUsize,
    closed: futures::task::AtomicWaker,
}
impl Signals {
    fn subscribe(self: &Arc<Self>, name: String) -> Subscription {
        let (sender, receiver) = mpsc::channel(1);
        self.senders.lock().unwrap().push((name.clone(), sender));
        self.labels.lock().unwrap().push(name);
        self.active.fetch_add(1, Ordering::SeqCst);
        Subscription {
            receiver,
            signals: self.clone(),
            consumed: false,
        }
    }
    async fn close(&self) {
        self.senders.lock().unwrap().clear();
        futures::future::poll_fn(|cx| {
            self.closed.register(cx.waker());
            if self.active.load(Ordering::SeqCst) == 0 {
                Poll::Ready(())
            } else {
                Poll::Pending
            }
        })
        .await;
    }
}
struct Subscription {
    receiver: mpsc::Receiver<()>,
    signals: Arc<Signals>,
    consumed: bool,
}
impl Stream for Subscription {
    type Item = ();
    fn poll_next(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<()>> {
        // The adapter next polls this stream after awaiting publication of its
        // consumed event. This acknowledgment cannot run before that event.
        if self.consumed {
            self.consumed = false;
            if let Some(sender) = self.signals.settled.lock().unwrap().take() {
                let _ = sender.send(());
            }
        }
        let result = Pin::new(&mut self.receiver).poll_next(cx);
        if matches!(result, Poll::Ready(Some(()))) {
            self.consumed = true;
        }
        result
    }
}
impl Drop for Subscription {
    fn drop(&mut self) {
        self.signals.active.fetch_sub(1, Ordering::SeqCst);
        self.signals.closed.wake();
    }
}
struct CloseSubscriptions(Arc<Signals>);
impl Drop for CloseSubscriptions {
    fn drop(&mut self) {
        self.0.senders.lock().unwrap().clear();
    }
}

fn shell() -> impl IntoView {
    view! {
        <Router><Routes fallback=|| "missing">
            <Route path=path!("/:page")
                ssr=SsrMode::Static(StaticRoute::new().regenerate(|params| {
                    expect_context::<Arc<Signals>>().subscribe(params.get("page").unwrap())
                }))
                view=|| {
                    let signals = expect_context::<Arc<Signals>>();
                    let path = expect_context::<Request>().with(|request| request.path().to_owned()).unwrap();
                    format!("path:{path};revision:{}", signals.revision.load(Ordering::SeqCst))
                }/>
        </Routes></Router>
    }
}
fn listings() -> Vec<NtexRouteListing> {
    // Generation completes before any subject starts SSR; only this fixture
    // construction is shared. Requests and regeneration remain concurrent.
    static LISTINGS: OnceLock<Vec<NtexRouteListing>> = OnceLock::new();
    LISTINGS.get_or_init(|| generate_route_list(shell)).clone()
}
fn options(root: &Root) -> LeptosOptions {
    options_path(&root.0)
}
fn options_path(root: &std::path::Path) -> LeptosOptions {
    LeptosOptions::builder()
        .output_name("static_work_identity")
        .site_root(root.to_string_lossy().into_owned())
        .build()
}
fn run<T: 'static>(future: impl std::future::Future<Output = T> + 'static) -> T {
    ntex::rt::System::build()
        .testing()
        .build(RequestRuntime::new(ntex::rt::DefaultRuntime))
        .block_on(async move {
            ntex::time::timeout(ntex::time::Millis(10_000), future)
                .await
                .expect("TECHNICAL DEADLINE: static identity scenario did not progress")
        })
}

#[derive(Debug, PartialEq)]
struct RootObservation {
    statuses: [StatusCode; 2],
    bodies: [bool; 2],
    subscriptions: Vec<String>,
}
#[derive(Clone, Copy)]
enum RootSuffix {
    Plain,
    ParentDir,
    #[cfg(unix)]
    ParentSymlink,
}
fn root_identity(relative: bool, existing: bool, suffix: RootSuffix) -> RootObservation {
    run(async move {
        let routes = listings();
        let root = Root::new(relative, existing);
        let site = match suffix {
            RootSuffix::Plain => root.0.clone(),
            RootSuffix::ParentDir => root.0.join("missing/../site"),
            #[cfg(unix)]
            RootSuffix::ParentSymlink => {
                std::fs::create_dir_all(root.0.join("actual")).unwrap();
                std::os::unix::fs::symlink("actual", root.0.join("parent_link")).unwrap();
                root.0.join("missing/../parent_link/new_missing_dir")
            }
        };
        let signals = Arc::new(Signals::default());
        let _close = CloseSubscriptions(signals.clone());
        signals.revision.store(1, Ordering::SeqCst);
        let context = signals.clone();
        let app = test::init_service(
            App::new()
                .state(options_path(&site))
                .leptos_routes_with_context(
                    routes,
                    move || provide_context(context.clone()),
                    shell,
                ),
        )
        .await;
        let first =
            test::call_service(&app, test::TestRequest::with_uri("/first").to_request()).await;
        let first_status = first.status();
        let first_body = String::from_utf8(test::read_body(first).await.to_vec()).unwrap();
        std::fs::remove_file(site.join("first.html")).unwrap();
        signals.revision.store(2, Ordering::SeqCst);
        let second =
            test::call_service(&app, test::TestRequest::with_uri("/first").to_request()).await;
        let second_status = second.status();
        let second_body = String::from_utf8(test::read_body(second).await.to_vec()).unwrap();
        let subscriptions = signals.labels.lock().unwrap().clone();
        signals.close().await;
        RootObservation {
            statuses: [first_status, second_status],
            bodies: [
                first_body.contains("path:/first;revision:1"),
                second_body.contains("path:/first;revision:2"),
            ],
            subscriptions,
        }
    })
}
fn one_subscription() -> RootObservation {
    RootObservation {
        statuses: [StatusCode::OK; 2],
        bodies: [true; 2],
        subscriptions: vec!["first".into()],
    }
}

#[derive(Debug, PartialEq)]
struct NamespaceObservation {
    statuses: [StatusCode; 2],
    initial: [bool; 2],
    subscriptions: Vec<String>,
    regenerated: [bool; 2],
    regular_entries: [bool; 2],
    seed_unchanged: bool,
}
/// A metadata entry that exists but cannot be parsed: the HTML is present, yet
/// the paired artifact is damaged and must be rendered again.
fn damaged_metadata(root: &std::path::Path, html_name: &str) {
    let directory = root.join(".leptos-static-metadata");
    let _ = std::fs::create_dir(&directory);
    std::fs::write(directory.join(html_name), "invalid-metadata").unwrap();
}
fn namespace_identity(symlinks: bool) -> NamespaceObservation {
    run(async move {
        let routes = listings();
        let root = Root::new(false, true);
        std::fs::write(root.0.join("seed.html"), "seed-original").unwrap();
        // The target exists but cannot be served as a valid paired artifact.
        // Each requested namespace entry therefore needs public SSR publication.
        damaged_metadata(&root.0, "seed.html");
        if symlinks {
            #[cfg(unix)]
            for name in ["first.html", "second.html"] {
                std::os::unix::fs::symlink("seed.html", root.0.join(name)).unwrap();
            }
        }
        let signals = Arc::new(Signals::default());
        let _close = CloseSubscriptions(signals.clone());
        signals.revision.store(1, Ordering::SeqCst);
        let context = signals.clone();
        let app = test::init_service(App::new().state(options(&root)).leptos_routes_with_context(
            routes,
            move || provide_context(context.clone()),
            shell,
        ))
        .await;
        let first =
            test::call_service(&app, test::TestRequest::with_uri("/first").to_request()).await;
        let first_status = first.status();
        let first_body = String::from_utf8(test::read_body(first).await.to_vec()).unwrap();
        let second =
            test::call_service(&app, test::TestRequest::with_uri("/second").to_request()).await;
        let second_status = second.status();
        let second_body = String::from_utf8(test::read_body(second).await.to_vec()).unwrap();
        let subscriptions = signals.labels.lock().unwrap().clone();
        signals.revision.store(2, Ordering::SeqCst);
        let (sender, settled) = oneshot::channel();
        *signals.settled.lock().unwrap() = Some(sender);
        signals
            .senders
            .lock()
            .unwrap()
            .iter_mut()
            .find(|(name, _)| name == "first")
            .expect("first route must have installed an ISR stream")
            .1
            .try_send(())
            .unwrap();
        settled
            .await
            .expect("first stream must poll after publishing its event");
        let first_saved = std::fs::read_to_string(root.0.join("first.html")).unwrap();
        let second_saved = std::fs::read_to_string(root.0.join("second.html")).unwrap();
        let regular_entries = ["first.html", "second.html"].map(|name| {
            std::fs::symlink_metadata(root.0.join(name))
                .unwrap()
                .is_file()
        });
        let seed_unchanged =
            std::fs::read_to_string(root.0.join("seed.html")).unwrap() == "seed-original";
        signals.close().await;
        NamespaceObservation {
            statuses: [first_status, second_status],
            initial: [
                first_body.contains("path:/first;revision:1"),
                second_body.contains("path:/second;revision:1"),
            ],
            subscriptions,
            regenerated: [
                first_saved.contains("path:/first;revision:2"),
                second_saved.contains("path:/second;revision:1"),
            ],
            regular_entries,
            seed_unchanged,
        }
    })
}
fn independent_entries() -> NamespaceObservation {
    NamespaceObservation {
        statuses: [StatusCode::OK; 2],
        initial: [true; 2],
        subscriptions: vec!["first".into(), "second".into()],
        regenerated: [true; 2],
        regular_entries: [true; 2],
        seed_unchanged: true,
    }
}

lets_expect! {
    expect(root_identity(relative, existing, suffix)) as static_work_root_anchor {
        let suffix = RootSuffix::Plain;
        let relative = true;
        let existing = true;
        to reuses_subscription_after_cache_miss { equal(one_subscription()) }
        when site_root_does_not_exist {
            let existing = false;
            to reuses_subscription_after_publication_creates_root { equal(one_subscription()) }
            when path_crosses_a_missing_parent {
                let suffix = RootSuffix::ParentDir;
                to preserves_parent_directory_resolution { equal(one_subscription()) }
            }
        }
        when site_root_is_absolute {
            let relative = false;
            to reuses_subscription_after_cache_miss { equal(one_subscription()) }
            when site_root_does_not_exist {
                let existing = false;
                to reuses_subscription_after_publication_creates_root { equal(one_subscription()) }
            }
        }
    }
}
#[cfg(unix)]
lets_expect! {
    expect(namespace_identity(symlinks)) as static_work_namespace {
        let symlinks = false;
        to regenerates_only_the_requested_entry { equal(independent_entries()) }
        when final_symlinks_share_one_target {
            let symlinks = true;
            to keeps_publication_and_regeneration_independent { equal(independent_entries()) }
        }
    }
}

#[cfg(unix)]
mod file_alias {
    use super::*;

    #[derive(Debug, PartialEq)]
    enum NativeNames {
        Equivalent,
        Distinct,
    }
    #[derive(Debug, PartialEq)]
    struct Observation {
        names: NativeNames,
        first_params: String,
        second_params: String,
        statuses: [StatusCode; 2],
        bodies: [bool; 2],
        subscriptions: Vec<String>,
        final_is_regular: bool,
        seed_unchanged: bool,
    }
    fn observe(symlink: bool, unicode: bool) -> Observation {
        run(async move {
            let routes = listings();
            let root = Root::new(false, true);
            let (stored, alias) = if unicode {
                ("éclair", "e\u{301}clair")
            } else {
                ("First", "first")
            };
            std::fs::write(root.0.join("seed.html"), "seed-original").unwrap();
            if symlink {
                std::os::unix::fs::symlink("seed.html", root.0.join(format!("{stored}.html")))
                    .unwrap();
                damaged_metadata(&root.0, "seed.html");
            } else {
                std::fs::write(
                    root.0.join(format!("{stored}.html")),
                    "initial-invalid-artifact",
                )
                .unwrap();
                damaged_metadata(&root.0, &format!("{stored}.html"));
            }
            // The only named fixture entry is `stored`; an existing alias
            // therefore witnesses filesystem equivalence independently of
            // the adapter's key algorithm. Distinct names remain two routes.
            let names = if root.0.join(format!("{alias}.html")).exists() {
                NativeNames::Equivalent
            } else {
                NativeNames::Distinct
            };
            let first_segment = if symlink && names == NativeNames::Equivalent {
                alias
            } else {
                stored
            };
            let uri = |segment: &str| {
                format!(
                    "/{}",
                    percent_encoding::utf8_percent_encode(
                        segment,
                        percent_encoding::NON_ALPHANUMERIC
                    )
                )
            };
            let first_uri = uri(first_segment);
            let second_uri = uri(alias);
            let signals = Arc::new(Signals::default());
            let _close = CloseSubscriptions(signals.clone());
            signals.revision.store(1, Ordering::SeqCst);
            let context = signals.clone();
            let app =
                test::init_service(App::new().state(options(&root)).leptos_routes_with_context(
                    routes,
                    move || provide_context(context.clone()),
                    shell,
                ))
                .await;
            let first =
                test::call_service(&app, test::TestRequest::with_uri(&first_uri).to_request())
                    .await;
            let first_status = first.status();
            let first_body = String::from_utf8(test::read_body(first).await.to_vec()).unwrap();
            // Keep the regular HTML present, so the second key can use its
            // canonical spelling. A damaged snapshot alone requires rendering.
            damaged_metadata(&root.0, &format!("{stored}.html"));
            signals.revision.store(2, Ordering::SeqCst);
            let second =
                test::call_service(&app, test::TestRequest::with_uri(&second_uri).to_request())
                    .await;
            let second_status = second.status();
            let second_body = String::from_utf8(test::read_body(second).await.to_vec()).unwrap();
            let subscriptions = signals.labels.lock().unwrap().clone();
            let final_is_regular = [stored, alias].iter().all(|name| {
                std::fs::symlink_metadata(root.0.join(format!("{name}.html")))
                    .unwrap()
                    .is_file()
            });
            let seed_unchanged =
                std::fs::read_to_string(root.0.join("seed.html")).unwrap() == "seed-original";
            signals.close().await;
            Observation {
                names,
                first_params: first_segment.to_owned(),
                second_params: alias.to_owned(),
                statuses: [first_status, second_status],
                bodies: [
                    first_body.contains(&format!("path:{first_uri};revision:1")),
                    second_body.contains(&format!("path:{second_uri};revision:2")),
                ],
                subscriptions,
                final_is_regular,
                seed_unchanged,
            }
        })
    }
    fn respects_native_names(actual: &Observation) -> AssertionResult {
        let expected_subscriptions = match actual.names {
            NativeNames::Equivalent => vec![actual.first_params.clone()],
            NativeNames::Distinct => {
                vec![actual.first_params.clone(), actual.second_params.clone()]
            }
        };
        if actual.statuses == [StatusCode::OK; 2]
            && actual.bodies == [true; 2]
            && actual.subscriptions == expected_subscriptions
            && actual.final_is_regular
            && actual.seed_unchanged
        {
            Ok(())
        } else {
            Err(AssertionError::new(vec![format!(
                "expected correct published bodies, regular entries, preserved seed and subscriptions {expected_subscriptions:?} for {:?}; received {actual:?}",
                actual.names
            )]))
        }
    }
    lets_expect! {
        expect(observe(symlink, unicode)) as static_work_file_alias {
            let symlink = false;
            let unicode = false;
            to shares_the_existing_regular_entry { respects_native_names }
            when alias_uses_decomposed_unicode {
                let unicode = true;
                to shares_the_existing_regular_entry { respects_native_names }
            }
            when entry_starts_as_symlink {
                let symlink = true;
                to retains_the_subscription_after_replacement { respects_native_names }
                when alias_uses_decomposed_unicode {
                    let unicode = true;
                    to retains_the_subscription_after_replacement { respects_native_names }
                }
            }
        }
    }
}

#[cfg(unix)]
mod parent_alias {
    use super::*;
    lets_expect! {
        expect(root_identity(true, false, RootSuffix::ParentSymlink)) as static_work_parent_alias {
            to reuses_subscription_after_missing_parent_creation { equal(one_subscription()) }
        }
    }
}

#[cfg(unix)]
mod lookup_error {
    use super::*;
    fn observe() -> (StatusCode, bool, usize) {
        run(async move {
            let root = Root::new(false, true);
            let storage = root.0.join("not-a-directory");
            std::fs::write(&storage, "ordinary-file-site-root").unwrap();
            assert_eq!(
                std::fs::metadata(storage.join("denied.html"))
                    .expect_err("fixture requires lookup through a non-directory")
                    .kind(),
                std::io::ErrorKind::NotADirectory,
                "file-root fixture must distinguish invalid lookup from another I/O error"
            );
            let renders = Arc::new(AtomicUsize::new(0));
            let observed = renders.clone();
            let route = NtexRouteListing::new(
                "/denied".into(),
                SsrMode::Static(StaticRoute::new()),
                [leptos_router::Method::Get],
                Vec::new(),
            );
            let app = test::init_service(App::new().state(options_path(&storage)).leptos_routes(
                vec![route],
                move || {
                    renders.fetch_add(1, Ordering::SeqCst);
                    expect_context::<leptos_ntex_unofficial::ResponseOptions>()
                        .set_status(StatusCode::NOT_FOUND);
                    "inline-error-with-denied-storage"
                },
            ))
            .await;
            let response =
                test::call_service(&app, test::TestRequest::with_uri("/denied").to_request()).await;
            let status = response.status();
            let body = String::from_utf8(test::read_body(response).await.to_vec()).unwrap();
            (
                status,
                body.contains("inline-error-with-denied-storage"),
                observed.load(Ordering::SeqCst),
            )
        })
    }
    lets_expect! {
        expect(observe()) as static_key_lookup_error {
            to preserves_inline_error_rendering { equal((StatusCode::NOT_FOUND, true, 1)) }
        }
    }
}
