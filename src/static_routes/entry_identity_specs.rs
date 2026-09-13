use super::*;
use crate::{LeptosRoutes, NtexRouteListing};
use futures::{
    FutureExt, Stream,
    channel::{mpsc, oneshot},
    future::{Shared, poll_fn},
};
use leptos::prelude::*;
use leptos_router::{
    SsrMode,
    components::{Route, Router, Routes},
    path,
    static_routes::StaticRoute,
};
use lets_expect::*;
use ntex::web::{App, test};
use std::{
    os::unix::fs::{MetadataExt, symlink},
    pin::Pin,
    sync::{OnceLock, atomic::AtomicUsize},
    task::{Context, Poll},
};

#[derive(Clone, Copy, Debug)]
enum EntryKind {
    NativeSpellings,
    AdoptedPublishedSpelling,
    SeparateLinks,
    HardlinkedLinks,
}
impl EntryKind {
    // The stored entry name and the alias spelling a second request uses.
    fn names(self) -> (&'static str, &'static str) {
        match self {
            EntryKind::AdoptedPublishedSpelling => ("caf\u{e9}", "cafe\u{301}"),
            _ => ("Link", "link"),
        }
    }
}
fn uri(name: &str) -> String {
    format!(
        "/{}",
        percent_encoding::utf8_percent_encode(name, percent_encoding::NON_ALPHANUMERIC)
    )
}
#[derive(Clone, Copy, Debug)]
enum InitialEntry {
    Symlink,
    Missing,
}
#[derive(Clone, Copy, Debug, PartialEq)]
enum NamespaceIdentity {
    SameEntry,
    SeparateEntries,
}
struct Signals {
    release: Shared<oneshot::Receiver<()>>,
    started: Mutex<Option<oneshot::Sender<()>>>,
    renders: AtomicUsize,
    factories: Mutex<Vec<String>>,
    senders: Mutex<Vec<mpsc::Sender<()>>>,
    active: AtomicUsize,
    cleanups: AtomicUsize,
    closed: futures::task::AtomicWaker,
}
impl Signals {
    fn subscribe(self: &Arc<Self>, name: String) -> Subscription {
        self.factories.lock().unwrap().push(name);
        let (sender, receiver) = mpsc::channel(1);
        self.senders.lock().unwrap().push(sender);
        self.active.fetch_add(1, Ordering::SeqCst);
        Subscription {
            receiver,
            signals: self.clone(),
        }
    }
    async fn close(&self) {
        self.senders.lock().unwrap().clear();
        poll_fn(|cx| {
            self.closed.register(cx.waker());
            if self.active.load(Ordering::SeqCst) == 0
                && self.cleanups.load(Ordering::SeqCst) == self.renders.load(Ordering::SeqCst)
            {
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
        self.signals.closed.wake();
    }
}
struct CloseSubscriptions(Arc<Signals>);
impl Drop for CloseSubscriptions {
    fn drop(&mut self) {
        self.0.senders.lock().unwrap().clear();
    }
}
struct Release(Option<oneshot::Sender<()>>);
impl Release {
    fn now(&mut self) {
        if let Some(sender) = self.0.take() {
            let _ = sender.send(());
        }
    }
}
impl Drop for Release {
    fn drop(&mut self) {
        self.now();
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
                    signals.renders.fetch_add(1, Ordering::SeqCst);
                    let cleanup = signals.clone();
                    on_cleanup(move || {
                        cleanup.cleanups.fetch_add(1, Ordering::SeqCst);
                        cleanup.closed.wake();
                    });
                    view! {
                        <Suspense fallback=|| "pending">
                            {Suspend::new(async move {
                                if let Some(started) = signals.started.lock().unwrap().take() {
                                    let _ = started.send(());
                                }
                                let _ = signals.release.clone().await;
                                format!("accepted-route:{path}")
                            })}
                        </Suspense>
                    }
                }/>
        </Routes></Router>
    }
}
fn listings() -> Vec<NtexRouteListing> {
    static LISTINGS: OnceLock<Vec<NtexRouteListing>> = OnceLock::new();
    LISTINGS
        .get_or_init(|| crate::tests::gen_route_list(shell))
        .clone()
}
#[derive(Debug)]
struct Observation {
    first_name: &'static str,
    second_name: &'static str,
    identity: NamespaceIdentity,
    registered_waiters: usize,
    pending_work: usize,
    initial: InitialEntry,
    active_before_close: usize,
    metadata_publications: usize,
    named_artifacts: usize,
    statuses: [StatusCode; 2],
    response_has_expected_route: [bool; 2],
    saved_has_expected_route: [bool; 2],
    responses_equal: bool,
    renders: usize,
    factories: Vec<String>,
    active: usize,
    cleanups: usize,
    entries_regular: bool,
    seed_unchanged: bool,
}
fn observe(kind: EntryKind, initial: InitialEntry) -> Observation {
    let routes = listings();
    let _render_guard = crate::tests::ROUTE_GEN_VS_RENDER.read().unwrap();
    crate::tests::run_ntex(async move {
        ntex::time::timeout(ntex::time::Millis(10_000), async move {
            let root = crate::tests::temp_site_root("pending_static_entry");
            fs::create_dir_all(&*root).unwrap();
            fs::write(root.join("seed.html"), "seed-original").unwrap();
            fs::create_dir(root.join(METADATA_DIRECTORY)).unwrap();
            fs::write(
                root.join(metadata_name(Path::new("seed.html"))),
                "invalid-metadata",
            )
            .unwrap();
            let (stored, alias_name) = kind.names();
            let stored_entry = root.join(format!("{stored}.html"));
            match initial {
                InitialEntry::Symlink => symlink("seed.html", &stored_entry).unwrap(),
                InitialEntry::Missing => fs::write(&stored_entry, "native-first-probe").unwrap(),
            }
            let (second_name, identity) = match kind {
                EntryKind::NativeSpellings | EntryKind::AdoptedPublishedSpelling => {
                    let alias = root.join(format!("{alias_name}.html"));
                    match fs::symlink_metadata(&alias) {
                        Ok(metadata) => {
                            let first = fs::symlink_metadata(&stored_entry).unwrap();
                            assert_eq!(
                                (metadata.dev(), metadata.ino()),
                                (first.dev(), first.ino()),
                                "single created entry must resolve through alias"
                            );
                            (alias_name, NamespaceIdentity::SameEntry)
                        }
                        Err(error) if error.kind() == io::ErrorKind::NotFound => {
                            match initial {
                                InitialEntry::Symlink => symlink("seed.html", alias).unwrap(),
                                InitialEntry::Missing => {
                                    fs::write(&alias, "native-second-probe").unwrap();
                                    assert_eq!(
                                        fs::read_to_string(&stored_entry).unwrap(),
                                        "native-first-probe"
                                    );
                                    assert_eq!(
                                        fs::read_to_string(&alias).unwrap(),
                                        "native-second-probe"
                                    );
                                    let first = fs::metadata(&stored_entry).unwrap();
                                    let second = fs::metadata(&alias).unwrap();
                                    assert_ne!(
                                        (first.dev(), first.ino()),
                                        (second.dev(), second.ino())
                                    );
                                }
                            }
                            (alias_name, NamespaceIdentity::SeparateEntries)
                        }
                        Err(error) => panic!("native lookup fixture failed: {error}"),
                    }
                }
                EntryKind::SeparateLinks => {
                    symlink("seed.html", root.join("Other.html")).unwrap();
                    ("Other", NamespaceIdentity::SeparateEntries)
                }
                EntryKind::HardlinkedLinks => {
                    fs::hard_link(&stored_entry, root.join("Other.html")).unwrap();
                    let first = fs::symlink_metadata(&stored_entry).unwrap();
                    let second = fs::symlink_metadata(root.join("Other.html")).unwrap();
                    assert!(
                        first.is_symlink() && second.is_symlink(),
                        "hardlink fixture must preserve symlink kind"
                    );
                    assert_eq!(
                        (first.dev(), first.ino()),
                        (second.dev(), second.ino()),
                        "hardlinked symlinks must share an inode"
                    );
                    assert_eq!(
                        fs::read_dir(&*root)
                            .unwrap()
                            .filter_map(Result::ok)
                            .filter(|entry| matches!(
                                entry.file_name().to_str(),
                                Some("Link.html" | "Other.html")
                            ))
                            .count(),
                        2,
                        "hardlinks remain two separately named entries"
                    );
                    ("Other", NamespaceIdentity::SeparateEntries)
                }
            };
            if matches!(initial, InitialEntry::Missing) {
                assert!(matches!(kind, EntryKind::NativeSpellings));
                fs::remove_file(&stored_entry).unwrap();
                if identity == NamespaceIdentity::SeparateEntries {
                    fs::remove_file(root.join(format!("{alias_name}.html"))).unwrap();
                }
                for name in [stored, alias_name] {
                    assert_eq!(
                        fs::symlink_metadata(root.join(format!("{name}.html")))
                            .unwrap_err()
                            .kind(),
                        io::ErrorKind::NotFound
                    );
                }
            }
            // The adopted-spelling scenario publishes under the alias first:
            // the filesystem then stores that spelling for the entry.
            let (first_name, second_name) = match kind {
                EntryKind::AdoptedPublishedSpelling => (alias_name, stored),
                _ => (stored, second_name),
            };
            let publications = Arc::new(AtomicUsize::new(0));
            let first_publication = publications.clone();
            let _first_publication = test_hooks::on_publication(
                root.join(format!("{first_name}.html")),
                Box::new(move || {
                    first_publication.fetch_add(1, Ordering::SeqCst);
                    Ok(())
                }),
            );
            let second_publication = publications.clone();
            let _second_publication = test_hooks::on_publication(
                root.join(format!("{second_name}.html")),
                Box::new(move || {
                    second_publication.fetch_add(1, Ordering::SeqCst);
                    Ok(())
                }),
            );
            let (release, released) = oneshot::channel();
            let mut release = Release(Some(release));
            let (started_tx, started) = oneshot::channel();
            let signals = Arc::new(Signals {
                release: released.shared(),
                started: Mutex::new(Some(started_tx)),
                renders: AtomicUsize::new(0),
                factories: Mutex::new(Vec::new()),
                senders: Mutex::new(Vec::new()),
                active: AtomicUsize::new(0),
                cleanups: AtomicUsize::new(0),
                closed: Default::default(),
            });
            let _close = CloseSubscriptions(signals.clone());
            let context = signals.clone();
            let runtime = routes[0].runtime.clone();
            let options = LeptosOptions::builder()
                .output_name("pending_static_entry")
                .site_root(root.to_string_lossy().into_owned())
                .build();
            let app = test::init_service(App::new().state(options).leptos_routes_with_context(
                routes,
                move || provide_context(context.clone()),
                shell,
            ))
            .await;
            let first_uri = uri(first_name);
            let first_response = async {
                let response =
                    test::call_service(&app, test::TestRequest::with_uri(&first_uri).to_request())
                        .await;
                (response.status(), test::read_body(response).await.to_vec())
            };
            let mut first_response = Box::pin(first_response);
            match futures::future::select(first_response.as_mut(), started).await {
                futures::future::Either::Right((Ok(()), _)) => {}
                _ => panic!("first SSR must reach its pending gate before publication"),
            }
            if matches!(kind, EntryKind::AdoptedPublishedSpelling)
                && identity == NamespaceIdentity::SameEntry
            {
                // While the first generation is pending, the entry takes the
                // spelling that generation will publish under.
                let adopted = root.join(format!("{first_name}.html"));
                fs::rename(&stored_entry, root.join(".leptos-adopt.tmp.1.1")).unwrap();
                fs::rename(root.join(".leptos-adopt.tmp.1.1"), &adopted).unwrap();
                let listed = fs::read_dir(&*root)
                    .unwrap()
                    .filter_map(Result::ok)
                    .map(|entry| entry.file_name())
                    .find(|name| name.to_str() == Some(&format!("{first_name}.html")));
                assert!(listed.is_some(), "entry must list the adopted spelling");
            }
            let second_uri = uri(second_name);
            let second_response = async {
                let response =
                    test::call_service(&app, test::TestRequest::with_uri(&second_uri).to_request())
                        .await;
                (response.status(), test::read_body(response).await.to_vec())
            };
            let mut responses = Box::pin(futures::future::join(first_response, second_response));
            let canonical_root = root.canonicalize().unwrap();
            let (registered_waiters, pending_work) = poll_fn(|cx| {
                assert!(
                    responses.as_mut().poll(cx).is_pending(),
                    "SSR cannot finish before gate release"
                );
                let entries = runtime.work.lock().unwrap();
                let mut waiters = 0;
                // One work may be registered under more than one spelling.
                let mut works = std::collections::HashSet::new();
                for (key, weak) in entries
                    .iter()
                    .filter(|(key, _)| key.starts_with(&canonical_root))
                {
                    let _ = key;
                    if let Some(work) = weak.upgrade() {
                        if !works.insert(Arc::as_ptr(&work)) {
                            continue;
                        }
                        waiters += work
                            .state
                            .lock()
                            .unwrap()
                            .waiters
                            .iter()
                            .filter(|sender| !sender.is_canceled())
                            .count();
                    }
                }
                if waiters == 2 {
                    Poll::Ready((waiters, works.len()))
                } else {
                    cx.waker().wake_by_ref();
                    Poll::Pending
                }
            })
            .await;
            match initial {
                InitialEntry::Symlink => assert!(
                    fs::symlink_metadata(root.join(format!("{first_name}.html")))
                        .unwrap()
                        .is_symlink(),
                    "first publication must remain gated through second admission"
                ),
                InitialEntry::Missing => {
                    for name in [stored, alias_name] {
                        assert_eq!(
                            fs::symlink_metadata(root.join(format!("{name}.html")))
                                .unwrap_err()
                                .kind(),
                            io::ErrorKind::NotFound,
                            "publication must remain gated through second admission"
                        );
                    }
                }
            }
            release.now();
            let ((first_status, first_body), (second_status, second_body)) = responses.await;
            let active_before_close = signals.active.load(Ordering::SeqCst);
            signals.close().await;
            let expected_second = if identity == NamespaceIdentity::SameEntry {
                first_uri.clone()
            } else {
                second_uri
            };
            let first_saved = fs::read_to_string(root.join(format!("{first_name}.html"))).unwrap();
            let second_saved =
                fs::read_to_string(root.join(format!("{second_name}.html"))).unwrap();
            let mut factories = signals.factories.lock().unwrap().clone();
            factories.sort();
            Observation {
                first_name,
                second_name,
                identity,
                registered_waiters,
                pending_work,
                initial,
                active_before_close,
                metadata_publications: publications.load(Ordering::SeqCst),
                named_artifacts: fs::read_dir(&*root)
                    .unwrap()
                    .filter_map(Result::ok)
                    .filter(|entry| {
                        [stored, alias_name, "Other"]
                            .iter()
                            .any(|name| entry.file_name().to_str() == Some(&format!("{name}.html")))
                    })
                    .count(),
                statuses: [first_status, second_status],
                response_has_expected_route: [
                    String::from_utf8_lossy(&first_body)
                        .contains(&format!("accepted-route:{first_uri}")),
                    String::from_utf8_lossy(&second_body)
                        .contains(&format!("accepted-route:{expected_second}")),
                ],
                saved_has_expected_route: [
                    first_saved.contains(&format!("accepted-route:{first_uri}")),
                    second_saved.contains(&format!("accepted-route:{expected_second}")),
                ],
                responses_equal: first_body == second_body,
                renders: signals.renders.load(Ordering::SeqCst),
                factories,
                active: signals.active.load(Ordering::SeqCst),
                cleanups: signals.cleanups.load(Ordering::SeqCst),
                entries_regular: [first_name, second_name].iter().all(|name| {
                    fs::symlink_metadata(root.join(format!("{name}.html")))
                        .unwrap()
                        .is_file()
                }),
                seed_unchanged: fs::read_to_string(root.join("seed.html")).unwrap()
                    == "seed-original",
            }
        })
        .await
        .expect("TECHNICAL DEADLINE: pending identity scenario did not progress")
    })
}
fn respects_entry_identity(actual: &Observation) -> AssertionResult {
    let pending_work = actual.pending_work;
    let _initial = actual.initial;
    let expected_count = if actual.identity == NamespaceIdentity::SameEntry {
        1
    } else {
        2
    };
    let mut expected_factories = if expected_count == 1 {
        vec![actual.first_name]
    } else {
        vec![actual.first_name, actual.second_name]
    };
    expected_factories.sort();
    if actual.registered_waiters == 2
        && actual.pending_work == expected_count
        && actual.active_before_close == expected_count
        && actual.metadata_publications == expected_count
        && actual.named_artifacts == expected_count
        && actual.statuses == [StatusCode::OK; 2]
        && actual.response_has_expected_route == [true; 2]
        && actual.saved_has_expected_route == [true; 2]
        && (expected_count != 1 || actual.responses_equal)
        && actual.renders == expected_count
        && actual.factories == expected_factories
        && actual.active == 0
        && actual.cleanups == expected_count
        && actual.entries_regular
        && actual.seed_unchanged
    {
        Ok(())
    } else {
        Err(AssertionError::new(vec![format!(
            "expected {expected_count} independent generation(s) matching real namespace identity, correct public bodies and complete cleanup; pending_work={pending_work}; actual {actual:?}"
        )]))
    }
}
lets_expect! {
    expect(observe(kind, initial)) as pending_static_entry_identity {
        let kind = EntryKind::NativeSpellings;
        let initial = InitialEntry::Symlink;
        to shares_generation_before_publication { respects_entry_identity }
        when final_entry_does_not_exist {
            let initial = InitialEntry::Missing;
            to shares_work_and_publication_for_native_aliases { respects_entry_identity }
        }
        when the_entry_adopts_the_published_spelling {
            let kind = EntryKind::AdoptedPublishedSpelling;
            to keeps_the_pending_work_reachable_under_both_spellings { respects_entry_identity }
        }
        when separate_symlinks_have_one_target {
            let kind = EntryKind::SeparateLinks;
            to preserves_independent_generation { respects_entry_identity }
        }
        when symlink_entries_are_hardlinked {
            let kind = EntryKind::HardlinkedLinks;
            to preserves_independent_generation { respects_entry_identity }
        }
    }
}
