//! Process boundary contract for live ISR startup and read-only SSG deployment.

use futures::{StreamExt, channel::mpsc};
use leptos::prelude::*;
use leptos_meta::{MetaTags, provide_meta_context};
use leptos_ntex_unofficial::{
    ResponseOptions, generate_route_list_with_exclusions_and_ssg_and_context,
    register_leptos_routes,
};
use leptos_router::{
    SsrMode,
    components::{Route, Router, Routes},
    path,
    static_routes::StaticRoute,
};
use lets_expect::*;
use ntex::{
    http::{StatusCode, header},
    web::{App, test},
};
use serde_json::{Value, json};
use std::{
    io::{BufRead, BufReader, Write},
    path::PathBuf,
    process::{Child, ChildStdin, Command, Stdio},
    sync::{
        Arc, Mutex,
        atomic::{AtomicUsize, Ordering},
    },
    time::Duration,
};

const EVENT_PREFIX: &str = "ISR_EVENT ";
const CHILD_ROLE: &str = "LEPTOS_NTEX_ISR_CHILD";
const CHILD_ROOT: &str = "LEPTOS_NTEX_ISR_ROOT";

fn emit(event: Value) {
    let mut output = std::io::stdout().lock();
    writeln!(output, "{EVENT_PREFIX}{event}").expect("write child event");
    output.flush().expect("flush child event");
}

#[derive(Default)]
struct PageState {
    revision: AtomicUsize,
    subscriptions: AtomicUsize,
    triggers: Mutex<Vec<mpsc::Sender<()>>>,
}

impl PageState {
    fn subscribe(self: &Arc<Self>) -> impl futures::Stream<Item = ()> + Send + use<> {
        self.subscriptions.fetch_add(1, Ordering::SeqCst);
        let (sender, receiver) = mpsc::channel(1);
        self.triggers.lock().unwrap().push(sender);
        let state = self.clone();
        futures::stream::unfold(receiver, move |mut receiver| {
            let state = state.clone();
            async move {
                // The adapter polls for the next event only after publishing
                // the render caused by the previous event. No timer is used.
                emit(json!({"event":"settled", "revision":state.revision.load(Ordering::SeqCst)}));
                receiver.next().await.map(|()| ((), receiver))
            }
        })
    }

    fn trigger(&self) -> bool {
        let mut senders = self.triggers.lock().unwrap();
        for sender in senders.iter_mut() {
            sender
                .try_send(())
                .expect("one outstanding fixture trigger");
        }
        !senders.is_empty()
    }
}

fn shell(state: Arc<PageState>) -> impl IntoView {
    provide_meta_context();
    let subscribed = state.clone();
    view! {
        <!DOCTYPE html>
        <html><head><MetaTags/></head><body>
            <Router><Routes fallback=|| "missing">
                <Route path=path!("/isr")
                    ssr=SsrMode::Static(StaticRoute::new().regenerate(move |_| subscribed.subscribe()))
                    view=move || {
                        let revision = state.revision.load(Ordering::SeqCst);
                        let response = expect_context::<ResponseOptions>();
                        response.set_status(StatusCode::from_u16(201 + revision as u16).unwrap());
                        response.append_header(header::HeaderName::from_static("x-isr-value"), header::HeaderValue::from_str(&format!("revision-{revision}")).unwrap());
                        response.append_header(header::HeaderName::from_static("x-isr-value"), header::HeaderValue::from_static("public"));
                        view! { <p>{format!("revision:{revision}")}</p> }
                    }
                />
            </Routes></Router>
        </body></html>
    }
}

async fn child_worker(role: String, root: String) {
    let state = Arc::new(PageState::default());
    state
        .revision
        .store(usize::from(role == "reader"), Ordering::SeqCst);
    let options = LeptosOptions::builder()
        .output_name("isr_restart")
        .site_root(root)
        .build();
    let app_fn = {
        let state = state.clone();
        move || shell(state.clone())
    };
    let (routes, generator) =
        generate_route_list_with_exclusions_and_ssg_and_context(app_fn.clone(), None, || {});
    let mut generator = Some(generator);
    let server_options = options.clone();
    let server = test::server(move || {
        let options = server_options.clone();
        let routes = routes.clone();
        let app_fn = app_fn.clone();
        async move {
            App::new()
                .state(options)
                .configure(move |cfg| register_leptos_routes(cfg, routes.clone(), app_fn.clone()))
        }
    })
    .await;
    emit(
        json!({"event":"ready", "pid":std::process::id(), "subscriptions":state.subscriptions.load(Ordering::SeqCst)}),
    );
    loop {
        let command = ntex::rt::spawn_blocking(|| {
            let mut command = String::new();
            std::io::stdin()
                .read_line(&mut command)
                .expect("read parent command");
            command.trim().to_owned()
        })
        .await
        .expect("join command reader");
        match command.as_str() {
            "generate" => {
                generator
                    .take()
                    .expect("startup runs once")
                    .generate(&options)
                    .await;
                emit(
                    json!({"event":"generated", "subscriptions":state.subscriptions.load(Ordering::SeqCst)}),
                );
            }
            "read" => {
                let response = server
                    .get("/isr")
                    .send()
                    .await
                    .expect("request persisted route over TCP");
                let status = response.status().as_u16();
                let values = response
                    .headers()
                    .iter()
                    .filter(|(name, _)| name.as_str() == "x-isr-value")
                    .map(|(_, value)| value.to_str().unwrap().to_owned())
                    .collect::<Vec<_>>();
                let body =
                    String::from_utf8(response.body().await.expect("read HTTP body").to_vec())
                        .unwrap();
                emit(
                    json!({"event":"page", "status":status, "values":values, "body":body, "subscriptions":state.subscriptions.load(Ordering::SeqCst)}),
                );
            }
            "trigger" => {
                state.revision.store(2, Ordering::SeqCst);
                let notified = state.trigger();
                emit(json!({"event":"triggered", "notified":notified}));
            }
            "stop" => {
                server.stop().await;
                return;
            }
            other => panic!("unexpected parent command {other:?}"),
        }
    }
}

struct SiteRoot(PathBuf);
impl SiteRoot {
    fn new() -> Self {
        static NEXT: AtomicUsize = AtomicUsize::new(0);
        loop {
            let path = std::env::temp_dir().join(format!(
                "leptos_isr_restart_{}_{}",
                std::process::id(),
                NEXT.fetch_add(1, Ordering::Relaxed)
            ));
            match std::fs::create_dir(&path) {
                Ok(()) => return Self(path),
                Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {}
                Err(error) => panic!("create isolated site root: {error}"),
            }
        }
    }
    /// Every regular file below the root with its relative path, so that the
    /// metadata directory's entries take part in the comparison.
    fn artifacts(&self) -> Vec<(String, Vec<u8>)> {
        fn collect(
            root: &std::path::Path,
            dir: &std::path::Path,
            files: &mut Vec<(String, Vec<u8>)>,
        ) {
            for entry in std::fs::read_dir(dir).unwrap() {
                let path = entry.unwrap().path();
                if path.is_dir() {
                    collect(root, &path, files);
                } else {
                    let relative = path
                        .strip_prefix(root)
                        .unwrap()
                        .to_string_lossy()
                        .into_owned();
                    files.push((relative, std::fs::read(&path).unwrap()));
                }
            }
        }
        let mut files = Vec::new();
        collect(&self.0, &self.0, &mut files);
        files.sort();
        files
    }
    /// A read-only deployment: files lose write permission and directories,
    /// including the metadata directory, stay traversable but immutable.
    fn make_read_only(&self) {
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            fn restrict(dir: &std::path::Path) {
                for entry in std::fs::read_dir(dir).unwrap() {
                    let path = entry.unwrap().path();
                    if path.is_dir() {
                        restrict(&path);
                    } else {
                        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o444))
                            .unwrap();
                    }
                }
                std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o555)).unwrap();
            }
            restrict(&self.0);
        }
    }
}
impl Drop for SiteRoot {
    fn drop(&mut self) {
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let _ = std::fs::set_permissions(&self.0, std::fs::Permissions::from_mode(0o755));
            let _ = std::fs::set_permissions(
                self.0.join(".leptos-static-metadata"),
                std::fs::Permissions::from_mode(0o755),
            );
        }
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

struct Worker {
    child: Child,
    input: ChildStdin,
    events: std::sync::mpsc::Receiver<Value>,
    pending: Vec<Value>,
    reader: Option<std::thread::JoinHandle<()>>,
}
impl Worker {
    fn start(role: &str, root: &SiteRoot) -> Self {
        let name = std::thread::current()
            .name()
            .expect("libtest names the selected leaf")
            .to_owned();
        let mut child = Command::new(std::env::current_exe().unwrap())
            .args(["--exact", &name, "--nocapture"])
            .env(CHILD_ROLE, role)
            .env(CHILD_ROOT, &root.0)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit())
            .spawn()
            .expect("spawn isolated ISR process");
        let input = child.stdin.take().unwrap();
        let output = child.stdout.take().unwrap();
        let (sender, events) = std::sync::mpsc::channel();
        let reader = std::thread::spawn(move || {
            for line in BufReader::new(output).lines() {
                let line = line.expect("read child output");
                if let Some(event) = line.strip_prefix(EVENT_PREFIX)
                    && sender
                        .send(serde_json::from_str(event).expect("parse child event"))
                        .is_err()
                {
                    break;
                }
            }
        });
        Self {
            child,
            input,
            events,
            pending: Vec::new(),
            reader: Some(reader),
        }
    }
    fn send(&mut self, command: &str) {
        writeln!(self.input, "{command}").expect("send worker command");
        self.input.flush().unwrap();
    }
    fn receive(&mut self, event: &str) -> Value {
        if let Some(index) = self
            .pending
            .iter()
            .position(|value| value["event"] == event)
        {
            return self.pending.remove(index);
        }
        loop {
            let value = self
                .events
                .recv_timeout(Duration::from_secs(20))
                .unwrap_or_else(|error| {
                    panic!(
                        "ISR worker {} did not report {event}: {error}; buffered {:?}",
                        self.child.id(),
                        self.pending
                    )
                });
            if value["event"] == event {
                return value;
            }
            self.pending.push(value);
        }
    }
    fn read_page(&mut self) -> Value {
        self.send("read");
        self.receive("page")
    }
    fn stop(&mut self) -> bool {
        self.send("stop");
        self.child
            .wait()
            .expect("wait for worker termination")
            .success()
    }
}
impl Drop for Worker {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
        if let Some(reader) = self.reader.take() {
            let _ = reader.join();
        }
    }
}

#[derive(Clone, Copy)]
enum ServingMode {
    Live,
    ReadOnly,
}
#[derive(Debug)]
struct RestartObservation {
    writer: Value,
    before_startup: Value,
    after_startup: Value,
    final_page: Value,
    writer_exited: bool,
    reader_exited: bool,
    different_processes: bool,
    writer_pid: u64,
    reader_pid: u64,
    subscriptions: usize,
    artifacts_unchanged: bool,
}

fn restart_observation(mode: ServingMode) -> RestartObservation {
    if let Ok(role) = std::env::var(CHILD_ROLE) {
        let root = std::env::var(CHILD_ROOT).expect("child root");
        // A watchdog bounds even a broken shutdown. This is an experiment
        // deadline, not a condition used to prove regeneration has completed.
        let (finished, finished_rx) = std::sync::mpsc::channel();
        let watchdog = std::thread::spawn(move || {
            if finished_rx.recv_timeout(Duration::from_secs(30)).is_err() {
                eprintln!("ISR child exceeded its technical lifetime limit");
                std::process::exit(2);
            }
        });
        ntex::rt::System::new(
            "isr-restart-worker",
            leptos_ntex_unofficial::RequestRuntime::new(ntex::rt::DefaultRuntime),
        )
        .block_on(child_worker(role, root));
        let _ = finished.send(());
        watchdog.join().expect("join child watchdog");
        std::process::exit(0);
    }
    let root = SiteRoot::new();
    let mut writer = Worker::start("writer", &root);
    let writer_ready = writer.receive("ready");
    writer.send("generate");
    writer.receive("generated");
    writer.receive("settled");
    let writer_page = writer.read_page();
    let writer_exited = writer.stop();
    drop(writer);
    let artifacts = root.artifacts();
    if matches!(mode, ServingMode::ReadOnly) {
        root.make_read_only();
    }
    // wait() above precedes spawn: no callbacks/Owner survive from process A.
    let mut reader = Worker::start("reader", &root);
    let reader_ready = reader.receive("ready");
    let before_startup = reader.read_page();
    let (after_startup, final_page) = match mode {
        ServingMode::Live => {
            reader.send("generate");
            let generated = reader.receive("generated");
            if generated["subscriptions"].as_u64().unwrap() > 0 {
                reader.receive("settled");
            }
            let after_startup = reader.read_page();
            reader.send("trigger");
            if reader.receive("triggered")["notified"] == true {
                reader.receive("settled");
            }
            (after_startup, reader.read_page())
        }
        ServingMode::ReadOnly => (reader.read_page(), reader.read_page()),
    };
    let subscriptions = final_page["subscriptions"].as_u64().unwrap() as usize;
    let reader_exited = reader.stop();
    let observation = RestartObservation {
        writer: writer_page,
        before_startup,
        after_startup,
        final_page,
        writer_exited,
        reader_exited,
        different_processes: writer_ready["pid"] != reader_ready["pid"],
        writer_pid: writer_ready["pid"].as_u64().unwrap(),
        reader_pid: reader_ready["pid"].as_u64().unwrap(),
        subscriptions,
        artifacts_unchanged: artifacts == root.artifacts(),
    };
    eprintln!(
        "ISR process transition {} -> {}: {observation:?}",
        observation.writer_pid, observation.reader_pid
    );
    observation
}

fn is_revision(revision: usize, subscriptions: usize) -> impl Fn(&Value) -> AssertionResult {
    move |page| {
        let expected_values = json!([format!("revision-{revision}"), "public"]);
        if page["status"] == 201 + revision
            && page["values"] == expected_values
            && page["body"]
                .as_str()
                .is_some_and(|body| body.contains(&format!("<p>revision:{revision}</p>")))
            && page["subscriptions"] == subscriptions
        {
            Ok(())
        } else {
            Err(AssertionError::new(vec![format!(
                "Expected revision {revision}, status {}, repeated headers {expected_values}, subscriptions {subscriptions}; received {page}",
                201 + revision
            )]))
        }
    }
}

lets_expect! {
    expect(restart_observation(mode)) as static_service_after_process_restart {
        let mode = ServingMode::Live;
        to resumes_regeneration_with_one_new_subscription {
            have(writer) is_revision(0, 1),
            have(before_startup) is_revision(0, 0),
            have(after_startup) is_revision(1, 1),
            have(final_page) is_revision(2, 1),
            have(writer_exited) equal(true), have(reader_exited) equal(true),
            have(different_processes) equal(true), have(subscriptions) equal(1_usize),
            have(artifacts_unchanged) equal(false),
        }
        when deployed_as_read_only_ssg {
            let mode = ServingMode::ReadOnly;
            to preserves_the_snapshot_without_installing_a_subscription {
                have(writer) is_revision(0, 1),
                have(before_startup) is_revision(0, 0),
                have(after_startup) is_revision(0, 0),
                have(final_page) is_revision(0, 0),
                have(writer_exited) equal(true), have(reader_exited) equal(true),
                have(different_processes) equal(true), have(subscriptions) equal(0_usize),
                have(artifacts_unchanged) equal(true),
            }
        }
    }
}
