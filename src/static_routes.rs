//! Static (SSG) route generation and the internal catch-all route used by
//! [`LeptosRoutes`](crate::LeptosRoutes).
//!
//! Hosts [`StaticRouteGenerator`] (which writes every
//! [`SsrMode::Static`](leptos_router::SsrMode) route to disk), the
//! durable response snapshots, regeneration coordination and capability-scoped
//! filesystem operations.

mod policy;
mod storage;

use policy::{Permit, PolicyState};
pub use policy::{
    StaticPolicyError, StaticRoutePolicy, StaticStorageError, StaticStorageLimits, StaticWorkLimits,
};

use futures::StreamExt;
use leptos::{
    IntoView, config::LeptosOptions, context::use_context, prelude::expect_context,
    reactive::owner::Owner,
};
use leptos_integration_utils::{PinnedFuture, build_response};
use leptos_meta::ServerMetaContext;
use leptos_router::{RouteList, static_routes::RegenerationFn};
use ntex::http::{StatusCode, header};
use ntex::web::error::StateExtractorError;
use ntex::web::{self, ErrorRenderer, HttpRequest, HttpResponse, Route};
use or_poisoned::OrPoisoned;
use std::{
    fs,
    future::Future,
    io,
    path::{Path, PathBuf},
    sync::{
        Arc, Mutex,
        atomic::{AtomicU64, Ordering},
    },
};

use crate::owner::ScopedWork;
use crate::render::{async_stream_builder, provide_contexts};
use crate::request::Request;
use crate::response::{NtexResponse, ResponseOptions, ResponseParts};
use crate::routes::ensure_executor_initialized;

/// Coordination belongs to one generated application route list. It stores
/// only active work; response metadata is always read from durable artifacts.
#[derive(Default, Debug)]
pub(crate) struct StaticRuntime {
    policy: Mutex<PolicyState>,
    #[cfg(test)]
    registration_pause: Mutex<Option<RegistrationPause>>,
    work: Mutex<std::collections::HashMap<PathBuf, std::sync::Weak<StaticWork>>>,
}

impl StaticRuntime {
    /// The runtime behind every listing and generator that is composed by hand
    /// rather than produced by one `generate_route_list*` call. Sharing it is
    /// what lets a policy bound through either side govern both, and what
    /// keeps startup and request renders of one path in one generation.
    pub(crate) fn shared() -> Arc<Self> {
        static SHARED: std::sync::LazyLock<Arc<StaticRuntime>> =
            std::sync::LazyLock::new(Arc::default);
        SHARED.clone()
    }
}

#[cfg(test)]
#[derive(Debug)]
struct RegistrationPause {
    arrived: futures::channel::oneshot::Sender<()>,
    resume: std::sync::mpsc::Receiver<()>,
}

/// The result of one completed static render.
#[derive(Clone)]
enum StaticOutcome {
    /// The representation was published to disk and is served from there.
    Published,
    /// An error response is returned as rendered and never published.
    Inline {
        html: String,
        parts: Option<ResponseParts>,
    },
}

type StaticResult = Result<StaticOutcome, StaticPolicyError>;
type RenderedRoute = Result<(Owner, String), StaticPolicyError>;

// A render request is distinct from the subscription that may trigger later
// generations. Its factory can move to the worker that owns that subscription;
// the future it creates is polled locally with its own rendering Owner.
struct RenderTemplate {
    options: LeptosOptions,
    path: String,
    render: Box<dyn Fn() -> futures::future::LocalBoxFuture<'static, RenderedRoute> + Send>,
}

#[derive(Default)]
struct WorkState {
    waiters: Vec<futures::channel::oneshot::Sender<StaticResult>>,
    requested: Option<RenderTemplate>,
    running: bool,
    blocked_render: bool,
    terminal: Option<StaticResult>,
}

struct StaticWork {
    #[cfg(test)]
    abandonment_observer: Mutex<Option<futures::channel::oneshot::Sender<()>>>,
    state: Mutex<WorkState>,
    wake: futures::task::AtomicWaker,
}

impl std::fmt::Debug for StaticWork {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("StaticWork").finish_non_exhaustive()
    }
}

struct StaticWaiter {
    receiver: futures::channel::oneshot::Receiver<StaticResult>,
    work: Arc<StaticWork>,
}

impl Future for StaticWaiter {
    type Output = Result<StaticResult, futures::channel::oneshot::Canceled>;
    fn poll(
        mut self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<Self::Output> {
        std::pin::Pin::new(&mut self.receiver).poll(cx)
    }
}

impl Drop for StaticWaiter {
    fn drop(&mut self) {
        self.receiver.close();
        self.work.wake.wake();
    }
}

struct StaticTaskGuard {
    work: Arc<StaticWork>,
    runtime: std::sync::Weak<StaticRuntime>,
    key: StaticWorkKey,
}

impl Drop for StaticTaskGuard {
    fn drop(&mut self) {
        let (waiters, result) = {
            let mut state = self.work.state.lock().or_poisoned();
            let result = state
                .terminal
                .get_or_insert_with(|| Err("static render task ended before completion".into()))
                .clone();
            (std::mem::take(&mut state.waiters), result)
        };
        for waiter in waiters {
            let _ = waiter.send(result.clone());
        }
        if let Some(runtime) = self.runtime.upgrade() {
            let mut entries = runtime.work.lock().or_poisoned();
            for key in self.key.keys() {
                if entries
                    .get(key)
                    .is_some_and(|entry| entry.as_ptr() == Arc::as_ptr(&self.work))
                {
                    entries.remove(key);
                }
            }
        }
    }
}

// Generation work is keyed by the replaceable publication directory entry,
// never by a symlink target: publication replaces the final entry in its
// physical parent. An existing entry is identified by its stored spelling as
// listed by its parent directory, which the native lookup of the requested
// spelling selects through the entry inode. A missing entry keeps the
// requested spelling. Where the parent demonstrably folds ASCII case, both
// spellings fold so that aliases of one future or replaced entry agree.
// Key lookup is best-effort: filesystem errors here must not prevent the
// renderer from producing an inline error response.
#[derive(Clone, Debug, PartialEq, Eq)]
struct StaticWorkKey {
    entry: PathBuf,
    // The spelling this work publishes under when it differs from the
    // stored spelling after folding: a filesystem may adopt it on replacement.
    published: Option<PathBuf>,
}

impl StaticWorkKey {
    fn keys(&self) -> impl Iterator<Item = &PathBuf> {
        std::iter::once(&self.entry).chain(self.published.as_ref())
    }
}

fn static_work_key(path: &Path) -> StaticWorkKey {
    let absolute = std::path::absolute(path).unwrap_or_else(|_| path.to_path_buf());
    let (Some(name), Some(parent)) = (absolute.file_name(), absolute.parent()) else {
        return StaticWorkKey {
            entry: absolute,
            published: None,
        };
    };
    let namespace = static_namespace(parent);
    let rule = ascii_case_rule(&namespace);
    let stored = stored_entry_name(&namespace, name, rule).unwrap_or_else(|| name.to_os_string());
    let entry = namespace.join(rule.fold(&stored));
    let published = namespace.join(rule.fold(name));
    StaticWorkKey {
        published: (published != entry).then_some(published),
        entry,
    }
}

// A missing suffix must stay anchored to the same absolute parent before and
// after publication creates it.
fn static_namespace(parent: &Path) -> PathBuf {
    // Every failure, not only NotFound, walks back: an inaccessible suffix must
    // still anchor to its existing ancestor.
    let (mut namespace, suffix) =
        crate::fs_boundary::canonicalize_existing_prefix(parent, |_| true)
            .unwrap_or_else(|_| (parent.to_path_buf(), Vec::new()));
    for component in suffix {
        if component == ".." {
            namespace.pop();
        } else if component != "." {
            namespace.push(component);
        }
        // ParentDir can expose an existing alias followed by more missing
        // components. Resolve each exposed prefix before extending it.
        if let Ok(canonical) = namespace.canonicalize() {
            namespace = canonical;
        }
    }
    namespace
}

/// Whether one directory resolves ASCII letters of a name regardless of case.
///
/// The rule is observed by looking up an existing entry of the directory, or
/// of the nearest existing ancestor on the same device, under the opposite
/// ASCII case. Every case-insensitive filesystem folds all ASCII letters, so
/// one entry demonstrates the rule for every ASCII name in the directory.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum CaseRule {
    Folds,
    Distinct,
    Unknown,
}

impl CaseRule {
    fn fold(self, name: &std::ffi::OsStr) -> std::ffi::OsString {
        match self {
            CaseRule::Folds => map_ascii_case(name, |byte| byte.to_ascii_lowercase()),
            CaseRule::Distinct | CaseRule::Unknown => name.to_os_string(),
        }
    }
}

fn map_ascii_case(name: &std::ffi::OsStr, map: impl Fn(u8) -> u8) -> std::ffi::OsString {
    #[cfg(unix)]
    {
        use std::os::unix::ffi::{OsStrExt, OsStringExt};
        std::ffi::OsString::from_vec(name.as_bytes().iter().copied().map(map).collect())
    }
    #[cfg(not(unix))]
    {
        match name.to_str() {
            Some(name) => name
                .chars()
                .map(|char| {
                    u8::try_from(char)
                        .ok()
                        .filter(u8::is_ascii)
                        .map_or(char, |byte| char::from(map(byte)))
                })
                .collect::<String>()
                .into(),
            None => name.to_os_string(),
        }
    }
}

fn has_ascii_letter(name: &std::ffi::OsStr) -> bool {
    name.as_encoded_bytes().iter().any(u8::is_ascii_alphabetic)
}

fn swap_ascii_case(name: &std::ffi::OsStr) -> Option<std::ffi::OsString> {
    has_ascii_letter(name).then(|| {
        map_ascii_case(name, |byte| {
            if byte.is_ascii_uppercase() {
                byte.to_ascii_lowercase()
            } else {
                byte.to_ascii_uppercase()
            }
        })
    })
}

// Temporary files of this publisher are renamed away while they are listed.
fn is_transient_publication_name(name: &std::ffi::OsStr) -> bool {
    let bytes = name.as_encoded_bytes();
    bytes.starts_with(b".leptos-") && bytes.windows(5).any(|window| window == b".tmp.")
}

#[cfg(unix)]
type EntryIdentity = (u64, u64);
#[cfg(not(unix))]
type EntryIdentity = (
    fs::FileType,
    u64,
    Option<std::time::SystemTime>,
    Option<std::time::SystemTime>,
);

fn entry_identity(metadata: &fs::Metadata) -> EntryIdentity {
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        (metadata.dev(), metadata.ino())
    }
    #[cfg(not(unix))]
    {
        (
            metadata.file_type(),
            metadata.len(),
            metadata.modified().ok(),
            metadata.created().ok(),
        )
    }
}

// Hard links share one identity while they remain separate entries.
fn is_single_entry(metadata: &fs::Metadata) -> bool {
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        metadata.is_dir() || metadata.nlink() == 1
    }
    #[cfg(not(unix))]
    {
        metadata.is_dir()
    }
}

fn ascii_case_rule(namespace: &Path) -> CaseRule {
    let mut dir = namespace.to_path_buf();
    #[cfg(unix)]
    let mut device = None;
    loop {
        match fs::symlink_metadata(&dir) {
            Ok(metadata) if metadata.is_dir() => {
                #[cfg(unix)]
                {
                    use std::os::unix::fs::MetadataExt;
                    // A mounted root follows the rule of its own volume, not
                    // of the directory that holds its name.
                    if device.is_some_and(|device| device != metadata.dev()) {
                        return CaseRule::Unknown;
                    }
                    device = Some(metadata.dev());
                }
                match observe_case_rule(&dir) {
                    CaseRule::Unknown => {}
                    rule => return rule,
                }
            }
            Ok(_) => return CaseRule::Unknown,
            Err(_) => {}
        }
        if !dir.pop() {
            return CaseRule::Unknown;
        }
    }
}

fn observe_case_rule(dir: &Path) -> CaseRule {
    let Ok(entries) = fs::read_dir(dir) else {
        return CaseRule::Unknown;
    };
    for entry in entries.flatten() {
        let name = entry.file_name();
        if is_transient_publication_name(&name) {
            continue;
        }
        let Some(swapped) = swap_ascii_case(&name) else {
            continue;
        };
        let Ok(original) = fs::symlink_metadata(dir.join(&name)) else {
            continue;
        };
        if !is_single_entry(&original) {
            continue;
        }
        let identity = entry_identity(&original);
        let swapped = fs::symlink_metadata(dir.join(swapped));
        // The entry must outlive both lookups: a removal in between would
        // make a missing swapped name look like a case-distinct namespace.
        let Ok(retained) = fs::symlink_metadata(dir.join(&name)) else {
            continue;
        };
        if entry_identity(&retained) != identity {
            continue;
        }
        match swapped {
            Ok(alias) if entry_identity(&alias) == identity => return CaseRule::Folds,
            Ok(_) => return CaseRule::Distinct,
            Err(error) if error.kind() == io::ErrorKind::NotFound => return CaseRule::Distinct,
            Err(_) => continue,
        }
    }
    CaseRule::Unknown
}

// The stored spelling of an existing entry, without following a symlink.
// `None` means the entry is missing or its spelling cannot be observed.
#[cfg(unix)]
fn stored_entry_name(
    dir: &Path,
    name: &std::ffi::OsStr,
    rule: CaseRule,
) -> Option<std::ffi::OsString> {
    use std::os::unix::fs::{DirEntryExt, MetadataExt};
    // A concurrent replacement between the lookup and the listing yields a
    // new identity; the second attempt observes the replaced entry.
    for _ in 0..2 {
        let metadata = fs::symlink_metadata(dir.join(name)).ok()?;
        let identity = entry_identity(&metadata);
        let mut candidates = fs::read_dir(dir)
            .ok()?
            .flatten()
            .filter(|entry| {
                entry.ino() == metadata.ino()
                    && fs::symlink_metadata(entry.path())
                        .is_ok_and(|listed| entry_identity(&listed) == identity)
            })
            .map(|entry| entry.file_name())
            .collect::<Vec<_>>();
        match candidates.len() {
            0 => continue,
            1 => return candidates.pop(),
            _ => {}
        }
        // Hard links: the requested spelling selects one of the entries.
        if let Some(exact) = candidates.iter().find(|candidate| *candidate == name) {
            return Some(exact.clone());
        }
        let folded = rule.fold(name);
        let mut matching = candidates
            .iter()
            .filter(|candidate| rule.fold(candidate) == folded);
        return match (matching.next(), matching.next()) {
            (Some(candidate), None) => Some(candidate.clone()),
            _ => Some(name.to_os_string()),
        };
    }
    Some(name.to_os_string())
}

#[cfg(not(unix))]
fn stored_entry_name(
    dir: &Path,
    name: &std::ffi::OsStr,
    _rule: CaseRule,
) -> Option<std::ffi::OsString> {
    let metadata = fs::symlink_metadata(dir.join(name)).ok()?;
    if metadata.is_symlink() {
        return Some(name.to_os_string());
    }
    Some(
        dir.join(name)
            .canonicalize()
            .ok()
            .and_then(|canonical| canonical.file_name().map(std::ffi::OsStr::to_os_string))
            .unwrap_or_else(|| name.to_os_string()),
    )
}

impl StaticRuntime {
    async fn render<IV: IntoView + 'static>(
        self: &Arc<Self>,
        options: LeptosOptions,
        path: String,
        app_fn: impl Fn() -> IV + Clone + Send + 'static,
        additional_context: impl Fn() + Clone + Send + 'static,
        regenerate: Vec<RegenerationFn>,
    ) -> StaticResult {
        let policy = self
            .policy
            .lock()
            .or_poisoned()
            .start(Path::new(&*options.site_root));
        let _waiter_permit = policy.as_ref().map(StaticRoutePolicy::waiter).transpose()?;
        let key =
            static_path(&options, &path).ok_or_else(|| "invalid static route path".to_owned())?;
        let key = ntex::rt::spawn_blocking(move || static_work_key(&key))
            .await
            .map_err(|err| err.to_string())?;
        let render_path = path.clone();
        let mut template = Some(RenderTemplate {
            options,
            path,
            render: Box::new(move || {
                Box::pin(StaticRouteGenerator::render_route(
                    render_path.clone(),
                    app_fn.clone(),
                    additional_context.clone(),
                ))
            }),
        });
        let (work, fresh, rx) = loop {
            let (work, fresh) = {
                let mut entries = self.work.lock().or_poisoned();
                // Every spelling of one entry must find its active work
                // before a new one is registered under any of them.
                if let Some(work) = key.keys().find_map(|key| {
                    entries
                        .get(key)
                        .and_then(std::sync::Weak::upgrade)
                        .filter(|work| work.state.lock().or_poisoned().terminal.is_none())
                }) {
                    (work, false)
                } else {
                    let work = Arc::new(StaticWork {
                        #[cfg(test)]
                        abandonment_observer: Mutex::new(None),
                        state: Mutex::new(WorkState::default()),
                        wake: futures::task::AtomicWaker::new(),
                    });
                    for key in key.keys() {
                        entries.insert(key.clone(), Arc::downgrade(&work));
                    }
                    (work, true)
                }
            };
            #[cfg(test)]
            if let Some(pause) = self.registration_pause.lock().unwrap().take() {
                let _ = pause.arrived.send(());
                let _ = pause.resume.recv();
            }
            let (tx, rx) = futures::channel::oneshot::channel();
            {
                let mut state = work.state.lock().or_poisoned();
                // Cancellation or completion can win after the registry lookup.
                // Such a result belongs to that generation's waiters, never to
                // a new request. No SSR or I/O operation is retried here.
                if state.terminal.is_some() {
                    continue;
                }
                if state.blocked_render {
                    return Err(policy
                        .as_ref()
                        .expect("only managed work waits for capacity")
                        .render_capacity_error());
                }
                state.waiters.retain(|waiter| !waiter.is_canceled());
                state.waiters.push(tx);
                if !state.running && state.requested.is_none() {
                    state.requested = template.take();
                }
            }
            break (work, fresh, rx);
        };
        if fresh {
            let guard = StaticTaskGuard {
                work: work.clone(),
                runtime: Arc::downgrade(self),
                key,
            };
            ntex::rt::spawn(ScopedWork::new(
                Generation {
                    guard,
                    policy,
                    regenerate,
                }
                .run(),
            ));
        }
        // A caller joining a running generation shares its current template.
        // Drop unused callback captures outside the lock and only after the
        // task guard owns every accepted waiter, including during unwinding.
        let waiter = StaticWaiter { receiver: rx, work };
        drop(template);
        waiter.work.wake.wake();
        waiter
            .await
            .unwrap_or_else(|_| Err("static render was cancelled".into()))
    }
}

impl WorkState {
    /// Closes admission for callers refused a render slot: their captures and
    /// waiters leave while the last accepted template stays.
    fn refuse(
        &mut self,
    ) -> (
        Option<RenderTemplate>,
        Vec<futures::channel::oneshot::Sender<StaticResult>>,
    ) {
        self.blocked_render = true;
        self.running = false;
        (self.requested.take(), std::mem::take(&mut self.waiters))
    }
    fn reopen(&mut self) {
        self.blocked_render = false;
    }
    /// Reopens admission and takes the template a refreshed request supplied.
    fn begin_render(&mut self) -> Option<RenderTemplate> {
        self.blocked_render = false;
        self.running = true;
        self.requested.take()
    }
}

type Subscriptions = futures::stream::SelectAll<ScopedWork<dyn futures::Stream<Item = ()>>>;

enum Trigger {
    Request,
    Subscription,
}

/// The generation task behind one static work entry: it renders the first
/// template, publishes it or returns it inline, installs the regeneration
/// subscriptions, and renders again for refreshed requests or subscription
/// events until every subscription has ended.
struct Generation {
    guard: StaticTaskGuard,
    policy: Option<StaticRoutePolicy>,
    regenerate: Vec<RegenerationFn>,
}

impl Generation {
    async fn run(self) {
        let Generation {
            guard,
            policy,
            regenerate,
        } = self;
        // Local variables drop in reverse declaration order on unwind:
        // subscription streams may read their Owner in Drop.
        let mut subscription_owner = None;
        let mut subscription_permit = match policy
            .as_ref()
            .map(|policy| policy.subscribe(regenerate.len()))
            .transpose()
        {
            Ok(permit) => permit,
            Err(error) => {
                finish_static_work(&guard.work, Err(error), true);
                return;
            }
        };
        let mut streams = Subscriptions::new();
        let mut first = true;
        let mut template = {
            let mut state = guard.work.state.lock().or_poisoned();
            state.running = true;
            state
                .requested
                .take()
                .expect("first waiter supplied a render template")
        };
        loop {
            let trigger = if first {
                Trigger::Request
            } else {
                match Self::next_trigger(&guard, &mut streams).await {
                    Some(trigger) => trigger,
                    None => break,
                }
            };
            let render_permit =
                match Self::render_permit(&guard, policy.as_ref(), first, trigger).await {
                    Ok(permit) => permit,
                    Err(RefusedRender::Ended) => break,
                    Err(RefusedRender::Retry) => continue,
                };
            if let Some(next) = guard.work.state.lock().or_poisoned().begin_render() {
                template = next;
            }
            let Some(rendered) =
                Self::render_or_abandon(&guard, &template, regenerate.is_empty()).await
            else {
                break;
            };
            let result = match rendered {
                Err(error) => Err(error),
                Ok((owner, html)) => {
                    let owner = crate::owner::OwnerCleanup::new(owner);
                    let result =
                        Self::publish(&template, &owner, html, policy.clone(), render_permit).await;
                    if first && (policy.is_none() || result.is_ok()) && !regenerate.is_empty() {
                        // Transfer cleanup authority before invoking factories:
                        // if one panics, previously created streams must still
                        // drop before their reactive scope is cleaned up.
                        let owner = subscription_owner.insert(owner);
                        Self::subscribe(owner, &regenerate, &mut subscription_permit, &mut streams);
                    }
                    result
                }
            };
            finish_static_work(&guard.work, result, streams.is_empty());
            first = false;
        }
        drop(streams);
        drop(subscription_owner);
        drop(guard);
    }

    /// Waits for a refreshed request or a regeneration event; `None` ends
    /// the generation because no subscription remains or one has closed.
    async fn next_trigger(guard: &StaticTaskGuard, streams: &mut Subscriptions) -> Option<Trigger> {
        let requested = futures::future::poll_fn(|cx| {
            guard.work.wake.register(cx.waker());
            if guard.work.state.lock().or_poisoned().requested.is_some() {
                std::task::Poll::Ready(())
            } else {
                std::task::Poll::Pending
            }
        });
        if streams.is_empty() {
            return None;
        }
        match futures::future::select(Box::pin(requested), Box::pin(streams.next())).await {
            futures::future::Either::Left(_) => Some(Trigger::Request),
            futures::future::Either::Right((Some(()), _)) => Some(Trigger::Subscription),
            futures::future::Either::Right((None, _)) => {
                let mut state = guard.work.state.lock().or_poisoned();
                // A request can register while next() polls the subscription
                // to EOF. Complete accepted work; otherwise close admission
                // before dropping it.
                if state.requested.is_none() {
                    state.terminal = Some(Err("static regeneration subscription ended".into()));
                    return None;
                }
                Some(Trigger::Request)
            }
        }
    }

    /// Takes a render slot. A refusal before the first render ends the
    /// generation; later refusals reject the waiting callers, and a consumed
    /// subscription event retains one refresh with the accepted template
    /// until a slot becomes available.
    async fn render_permit(
        guard: &StaticTaskGuard,
        policy: Option<&StaticRoutePolicy>,
        first: bool,
        trigger: Trigger,
    ) -> Result<Option<Permit>, RefusedRender> {
        let Some(policy) = policy else {
            return Ok(None);
        };
        match policy.render() {
            Ok(permit) => Ok(Some(permit)),
            Err(error) if first => {
                finish_static_work(&guard.work, Err(error), true);
                Err(RefusedRender::Ended)
            }
            Err(error) => {
                let (rejected, waiters) = guard.work.state.lock().or_poisoned().refuse();
                for waiter in waiters {
                    let _ = waiter.send(Err(error.clone()));
                }
                drop(rejected);
                match trigger {
                    Trigger::Request => {
                        guard.work.state.lock().or_poisoned().reopen();
                        Err(RefusedRender::Retry)
                    }
                    Trigger::Subscription => Ok(Some(policy.wait_for_render().await)),
                }
            }
        }
    }

    /// Renders the template. Without subscriptions the render is abandoned,
    /// and the generation ends, once every waiting caller has gone.
    async fn render_or_abandon(
        guard: &StaticTaskGuard,
        template: &RenderTemplate,
        abandonable: bool,
    ) -> Option<RenderedRoute> {
        let render = (template.render)();
        if !abandonable {
            return Some(render.await);
        }
        let abandoned = futures::future::poll_fn(|cx| {
            guard.work.wake.register(cx.waker());
            let mut state = guard.work.state.lock().or_poisoned();
            state.waiters.retain(|waiter| !waiter.is_canceled());
            #[cfg(test)]
            if let Some(observed) = guard.work.abandonment_observer.lock().unwrap().take() {
                let _ = observed.send(());
            }
            if state.waiters.is_empty() {
                state.terminal = Some(Err("static render has no remaining waiters".into()));
                std::task::Poll::Ready(())
            } else {
                std::task::Poll::Pending
            }
        });
        match futures::future::select(Box::pin(render), Box::pin(abandoned)).await {
            futures::future::Either::Left((rendered, _)) => Some(rendered),
            futures::future::Either::Right(_) => None,
        }
    }

    /// Publishes a successful render, or returns an error render inline.
    async fn publish(
        template: &RenderTemplate,
        owner: &crate::owner::OwnerCleanup,
        html: String,
        policy: Option<StaticRoutePolicy>,
        render_permit: Option<Permit>,
    ) -> StaticResult {
        let parts = owner.owner().with(use_context::<ResponseOptions>);
        if was_error_status(owner.owner()) {
            return Ok(StaticOutcome::Inline {
                html,
                parts: parts.map(|parts| parts.0.read().or_poisoned().clone()),
            });
        }
        write_static_route_managed(
            &template.options,
            parts,
            &template.path,
            html,
            policy,
            render_permit,
        )
        .await
        .map(|()| StaticOutcome::Published)
    }

    /// Installs the regeneration streams in the generation's reactive scope.
    fn subscribe(
        owner: &crate::owner::OwnerCleanup,
        regenerate: &[RegenerationFn],
        subscription_permit: &mut Option<Permit>,
        streams: &mut Subscriptions,
    ) {
        use leptos::prelude::GetUntracked;
        // Leptos 0.8.13 RawParamsMap is an alias of this public ArcMemo type.
        // Keep this integration lookup aligned with ResolvedStaticPath::build
        // when updating Leptos.
        let Some(params) = owner
            .owner()
            .use_context_bidirectional::<leptos::prelude::ArcMemo<leptos_router::params::ParamsMap>>()
        else {
            return;
        };
        let params = params.get_untracked();
        streams.extend(regenerate.iter().map(|regenerate| {
            let permit = subscription_permit.as_mut().map(Permit::split_one);
            // The factory itself runs in the generation's scope: it may read
            // context provided during the render.
            owner.owner().with(|| {
                let stream = regenerate(&params).map(move |event| {
                    let _permit = &permit;
                    event
                });
                ScopedWork::from_pin(
                    Box::pin(stream) as std::pin::Pin<Box<dyn futures::Stream<Item = ()>>>
                )
            })
        }));
    }
}

enum RefusedRender {
    Ended,
    Retry,
}

fn finish_static_work(work: &StaticWork, result: StaticResult, terminal: bool) {
    let waiters = {
        let mut state = work.state.lock().or_poisoned();
        state.running = false;
        if terminal {
            state.terminal = Some(result.clone());
        }
        std::mem::take(&mut state.waiters)
    };
    for waiter in waiters {
        let _ = waiter.send(result.clone());
    }
}

/// Allows generating prerendered static HTML for every [`SsrMode::Static`](leptos_router::SsrMode)
/// route in the application.
///
/// Produced by [`generate_route_list_with_ssg`](crate::generate_route_list_with_ssg).
/// For live regeneration, call [`StaticRouteGenerator::generate`] in each
/// serving process at startup, before accepting requests. Keep that ntex runtime
/// running while serving: subscriptions and their reactive contexts live there.
///
/// A separate pre-build process can write static artifacts, but its callbacks
/// do not survive process exit. A read-only SSG deployment serves those complete
/// artifacts without calling `generate` or installing live subscriptions.
/// Skipping this call does not disable on-demand rendering by serving helpers
/// when an artifact is missing or inconsistent; enforce filesystem policy separately.
#[allow(clippy::type_complexity)]
pub struct StaticRouteGenerator(
    // Kept alive so that any context values provided during generation stay
    // valid for the duration of the static rendering pipeline.
    #[allow(dead_code)] Owner,
    Box<
        dyn FnOnce(
                &LeptosOptions,
            )
                -> PinnedFuture<Result<StaticGenerationReport, StaticGenerationError>>
            + Send,
    >,
    Arc<StaticRuntime>,
);

/// Successful initial static renders. Application error responses count as
/// completed renders but are returned inline and are not published.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct StaticGenerationReport {
    /// Number of initial paths whose render completed successfully.
    pub completed: usize,
}

/// Initial generation finished with one or more failures. Earlier artifacts
/// and admitted regeneration subscriptions remain available; no rollback or
/// eviction occurs.
#[derive(Clone, Debug)]
pub struct StaticGenerationError {
    completed: usize,
    failures: Vec<(String, StaticPolicyError)>,
}
impl StaticGenerationError {
    /// Number of paths completed before or alongside the failures.
    pub fn completed(&self) -> usize {
        self.completed
    }
    /// Typed failures paired with their resolved static path.
    pub fn failures(&self) -> &[(String, StaticPolicyError)] {
        &self.failures
    }
}
impl std::fmt::Display for StaticGenerationError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "{} static generation failures after {} completed paths",
            self.failures.len(),
            self.completed
        )
    }
}
impl std::error::Error for StaticGenerationError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        self.failures.first().map(|(_, error)| error as _)
    }
}

impl StaticRouteGenerator {
    pub(crate) async fn render_route<IV: IntoView + 'static>(
        path: String,
        app_fn: impl Fn() -> IV + Clone + Send + 'static,
        additional_context: impl Fn() + Clone + Send + 'static,
    ) -> RenderedRoute {
        let (meta_context, meta_output) = ServerMetaContext::new();
        let request_path = if path.is_empty() { "/" } else { path.as_str() };
        let mock_req = ntex::web::test::TestRequest::with_uri(request_path)
            .header("Accept", "text/html")
            .to_http_request();
        // The synthetic request joins the worker's scope like a served one; a
        // missing scope is the same runtime misconfiguration as for a handler.
        let request = Request::try_new(&mock_req).map_err(|error| {
            crate::request::report_missing_scope(&error);
            StaticPolicyError::Render(error.to_string())
        })?;
        drop(mock_req);
        let additional_context = {
            let add_context = additional_context.clone();
            move || {
                let res_options = ResponseOptions::default();
                provide_contexts(request.clone(), &meta_context, &res_options);
                add_context();
            }
        };

        let (owner, stream) = {
            let _restore = crate::owner::RestoreOwner::capture();
            build_response(
                app_fn.clone(),
                additional_context,
                async_stream_builder,
                false,
            )
        };
        let owner = crate::owner::OwnerCleanup::new(owner);
        ScopedWork::with_owner(owner.owner().clone(), async move {
            let (owner, stream) = (owner, stream);
            let sc = owner.owner().shared_context().unwrap();
            let stream = stream.await;
            while let Some(pending) = sc.await_deferred() {
                pending.await;
            }

            let html = meta_output
                .inject_meta_context(stream)
                .await
                .collect::<String>()
                .await;
            Ok((owner.into_owner(), html))
        })
        .await
    }

    /// Creates a new static route generator from the given list of route
    /// definitions.
    pub fn new<IV>(
        routes: &RouteList,
        app_fn: impl Fn() -> IV + Clone + Send + 'static,
        additional_context: impl Fn() + Clone + Send + 'static,
    ) -> Self
    where
        IV: IntoView + 'static,
    {
        Self::with_runtime(routes, app_fn, additional_context, StaticRuntime::shared())
    }

    pub(crate) fn with_runtime<IV: IntoView + 'static>(
        routes: &RouteList,
        app_fn: impl Fn() -> IV + Clone + Send + 'static,
        additional_context: impl Fn() + Clone + Send + 'static,
        runtime: Arc<StaticRuntime>,
    ) -> Self {
        let owner = Owner::new();
        Self(
            owner.clone(),
            {
                let runtime = runtime.clone();
                let routes = routes.clone();
                Box::new(move |options| {
                    let options = options.clone();
                    let _restore = crate::owner::RestoreOwner::capture();
                    let managed = runtime
                        .policy
                        .lock()
                        .or_poisoned()
                        .start(Path::new(&*options.site_root))
                        .is_some();
                    owner.with(|| {
                        additional_context();
                        Box::pin(ScopedWork::new(async move {
                            let mut initial = Vec::new();
                            let mut report = StaticGenerationReport::default();
                            let mut failures = Vec::new();
                            for route in routes.into_inner() {
                                let regenerate = route.regenerate().to_vec();
                                let paths = match route.into_static_paths().await {
                                    Some(paths) => paths,
                                    None => continue,
                                };
                                for path in paths {
                                    let path = path.to_string();
                                    let render = runtime.render(
                                        options.clone(),
                                        path.clone(),
                                        app_fn.clone(),
                                        additional_context.clone(),
                                        regenerate.clone(),
                                    );
                                    if managed {
                                        match render.await {
                                            Ok(_) => report.completed += 1,
                                            Err(error) => failures.push((path, error)),
                                        }
                                    } else {
                                        initial.push(async move { (path, render.await) });
                                    }
                                }
                            }
                            for (path, result) in futures::future::join_all(initial).await {
                                match result {
                                    Ok(_) => report.completed += 1,
                                    Err(error) => failures.push((path, error)),
                                }
                            }
                            if failures.is_empty() {
                                Ok(report)
                            } else {
                                Err(StaticGenerationError {
                                    completed: report.completed,
                                    failures,
                                })
                            }
                        }))
                    })
                })
            },
            runtime,
        )
    }

    /// Binds this generator to a shared policy before its runtime starts.
    pub fn with_static_policy(self, policy: StaticRoutePolicy) -> Result<Self, StaticPolicyError> {
        policy.bind(&self.2)?;
        Ok(self)
    }

    /// Renders resolved static paths into the site root and installs their
    /// regeneration subscriptions on the current ntex runtime.
    ///
    /// Existing artifacts are refreshed too. Call this in every serving process
    /// after restart to resume ISR; reading saved HTML alone cannot restore Rust
    /// callbacks or their reactive owners. Skip this step for read-only SSG.
    pub async fn generate(self, options: &LeptosOptions) {
        if let Err(error) = self.try_generate(options).await {
            for (_, failure) in error.failures() {
                report_static_error(&failure.to_string());
            }
        }
    }

    /// Generates initial paths and reports typed partial failures. With an
    /// opt-in policy, initial renders are admitted one at a time; successful
    /// artifacts and subscriptions survive later failures. This is not an
    /// atomic deployment, and application parameter maps can allocate memory.
    pub async fn try_generate(
        self,
        options: &LeptosOptions,
    ) -> Result<StaticGenerationReport, StaticGenerationError> {
        crate::owner::OwnerContextFuture::new(async move { (self.1)(options).await }).await
    }
}

/// Whether a static render produced an ERROR status that must NOT be cached
/// to disk. leptos takes this as its `was_404` / `was_error` hook: when it
/// returns true, leptos sends the rendered HTML back instead of invoking the
/// writer, so the dynamic handler can re-render the live error on demand.
///
/// leptos's own contract is "404, 500, etc." (see the `was_error` comment in
/// leptos_router's static generation), so this skips EVERY 4xx/5xx render —
/// not only 404. A 500 (or any error) render cached as a bare static file
/// would otherwise be served from disk indefinitely, even after the cause
/// cleared. This is a deliberate divergence from `leptos_axum` /
/// `leptos_actix`, whose `was_404` callbacks check only `== NOT_FOUND`.
fn was_error_status(owner: &Owner) -> bool {
    let resp = owner.with(|| expect_context::<ResponseOptions>());
    let status = resp.0.read().or_poisoned().status;
    status.is_some_and(|status| status.is_client_error() || status.is_server_error())
}

fn static_path(options: &LeptosOptions, path: &str) -> Option<PathBuf> {
    let mut normalized = path.to_string();
    if normalized != "/" && normalized.ends_with('/') {
        normalized.push_str("index");
    }

    let trimmed = normalized.trim_start_matches('/');
    let logical = if trimmed.is_empty() { "index" } else { trimmed };
    let mut parts = Vec::new();
    for segment in logical.split('/') {
        if segment.is_empty() {
            continue;
        }
        let decoded = percent_encoding::percent_decode_str(segment)
            .decode_utf8()
            .ok()?;
        let segment = decoded.as_ref();
        // Blocks `.`, `..` and every dotfile, but lets the exact `.well-known`
        // segment through (RFC 8615) — see `crate::files::is_blocked_dot_segment`.
        if crate::files::is_blocked_dot_segment(segment)
            || segment.contains('\0')
            || segment.contains('/')
            || segment.contains('\\')
        {
            return None;
        }
        parts.push(segment.to_string());
    }

    let last = parts.last_mut()?;
    last.push_str(".html");

    let mut rel = PathBuf::new();
    for part in parts {
        rel.push(part);
    }
    if !rel.components().all(|component| {
        matches!(
            component,
            std::path::Component::Normal(part)
                if !crate::files::is_blocked_dot_segment(&part.to_string_lossy())
        )
    }) {
        return None;
    }
    Some(Path::new(&*options.site_root).join(rel))
}

#[cfg(test)]
fn validate_static_parent(root: &Path, file_path: &Path) -> io::Result<()> {
    fs::create_dir_all(root)?;
    crate::fs_boundary::SiteRoot::open(root)?.parent(file_path, true)?;
    Ok(())
}

#[cfg(test)]
fn validate_static_file(root: &Path, file_path: &Path) -> Option<PathBuf> {
    let root = crate::fs_boundary::SiteRoot::open(root).ok()?;
    root.open_file(file_path).ok()?;
    root.canonical_file(file_path).ok()
}

static TEMP_SEQ: AtomicU64 = AtomicU64::new(0);

struct TempFileGuard<'a> {
    dir: &'a cap_std::fs::Dir,
    name: Option<PathBuf>,
}

impl Drop for TempFileGuard<'_> {
    fn drop(&mut self) {
        if let Some(name) = self.name.take() {
            let _ = self.dir.remove_file(name);
        }
    }
}

fn prepare_file<'a>(
    dir: &'a cap_std::fs::Dir,
    name: &Path,
    contents: &[u8],
) -> io::Result<TempFileGuard<'a>> {
    use sha2::Digest;
    let seq = TEMP_SEQ.fetch_add(1, Ordering::Relaxed);
    // A full destination name can fit while its temporary suffix does not.
    // The digest keeps destinations distinct without extending their length.
    let digest = sha2::Sha256::digest(name.as_os_str().as_encoded_bytes());
    let temp = PathBuf::from(format!(
        ".leptos-{digest:x}.tmp.{}.{seq}",
        std::process::id()
    ));
    create_temporary(dir, temp, contents)
}

fn create_temporary<'a>(
    dir: &'a cap_std::fs::Dir,
    temp: PathBuf,
    contents: &[u8],
) -> io::Result<TempFileGuard<'a>> {
    use std::io::Write;
    let mut options = cap_std::fs::OpenOptions::new();
    options.write(true).create_new(true);
    let mut file = dir.open_with(&temp, &options)?;
    let guard = TempFileGuard {
        dir,
        name: Some(temp),
    };
    file.write_all(contents)?;
    file.sync_all()?;
    Ok(guard)
}

fn publish_file(
    dir: &cap_std::fs::Dir,
    mut temp: TempFileGuard<'_>,
    name: &Path,
) -> io::Result<()> {
    dir.rename(
        temp.name
            .as_ref()
            .expect("owned unpublished temporary file"),
        dir,
        name,
    )?;
    temp.name = None;
    Ok(())
}

#[cfg(test)]
fn write_file_atomic(file_path: &Path, contents: &[u8]) -> io::Result<()> {
    let parent = file_path
        .parent()
        .ok_or_else(|| io::Error::other("missing parent"))?;
    let root = crate::fs_boundary::SiteRoot::open(parent)?;
    let (dir, name) = root.parent(file_path, false)?;
    let temp = prepare_file(&dir, &name, contents)?;
    publish_file(&dir, temp, &name)
}

const METADATA_DIRECTORY: &str = ".leptos-static-metadata";
const PUBLICATION_LOCK: &str = ".leptos-static-publish.lock";

/// Response metadata lives beside the HTML in a reserved directory, under the
/// HTML entry's own name: it shares the filesystem's name limits and its case
/// and Unicode equivalence with the HTML entry.
fn metadata_name(name: &Path) -> PathBuf {
    Path::new(METADATA_DIRECTORY).join(name)
}

fn damaged_metadata_error(error: io::Error) -> io::Error {
    if error.kind() == io::ErrorKind::NotFound {
        io::Error::new(io::ErrorKind::InvalidData, error)
    } else {
        error
    }
}

/// Opens the metadata paired with `name`. `None` means the HTML was published
/// without metadata (an artifact of an older publisher); a metadata entry or
/// metadata directory that exists but cannot be read is a damaged artifact,
/// never a metadata-free one.
fn open_static_metadata(dir: &cap_std::fs::Dir, name: &Path) -> io::Result<Option<fs::File>> {
    let entry = metadata_name(name);
    let error = match crate::fs_boundary::open_regular(dir, &entry) {
        Ok(file) => return Ok(Some(file)),
        Err(error)
            if matches!(
                error.kind(),
                io::ErrorKind::NotFound | io::ErrorKind::NotADirectory
            ) =>
        {
            error
        }
        Err(error) => return Err(error),
    };
    // A failed lookup through a symlink does not prove that its namespace
    // entry is absent. Inspect the entries without following links.
    if dir.symlink_metadata(&entry).is_ok() {
        return Err(damaged_metadata_error(error));
    }
    match dir.symlink_metadata(METADATA_DIRECTORY) {
        Err(lookup) if lookup.kind() == io::ErrorKind::NotFound => Ok(None),
        Err(lookup) => Err(lookup),
        Ok(parent) if parent.is_dir() => Ok(None),
        Ok(parent) if parent.is_symlink() => match dir.metadata(METADATA_DIRECTORY) {
            Ok(resolved) if resolved.is_dir() => Ok(None),
            Ok(_) => Err(damaged_metadata_error(error)),
            Err(resolve) => Err(damaged_metadata_error(resolve)),
        },
        Ok(_) => Err(damaged_metadata_error(error)),
    }
}

fn publication_lock(
    dir: &cap_std::fs::Dir,
    write: bool,
) -> io::Result<crate::fs_boundary::FileLock> {
    crate::fs_boundary::lock_entry(dir, Path::new(PUBLICATION_LOCK), write)
}

fn lock_name(name: &Path) -> PathBuf {
    use sha2::Digest;
    let digest = sha2::Sha256::digest(name.as_os_str().as_encoded_bytes());
    PathBuf::from(format!(".leptos-static-{:02}.lock", digest[0] % 32))
}

fn lock_file(
    dir: &cap_std::fs::Dir,
    name: &Path,
    write: bool,
) -> io::Result<crate::fs_boundary::FileLock> {
    crate::fs_boundary::lock_entry(dir, &lock_name(name), write)
}

fn body_digest(file: &mut fs::File) -> io::Result<String> {
    use sha2::Digest;
    use std::io::{Read, Seek};
    let mut digest = sha2::Sha256::new();
    let mut bytes = [0_u8; 16384];
    loop {
        let size = file.read(&mut bytes)?;
        if size == 0 {
            break;
        }
        digest.update(&bytes[..size]);
    }
    file.rewind()?;
    Ok(format!("{:x}", digest.finalize()))
}

type StoredResponse = (u8, String, Option<u16>, Vec<(String, Vec<u8>)>);

/// Process-local memory of verified HTML/metadata pairs.
///
/// The digest check is a pure function of the two files' bytes, and a file's
/// identity (device, inode, length, change and modification times) pins those
/// bytes: publication replaces inodes, and any in-place edit moves the
/// change time. A repeated hit on unchanged identities therefore reuses the
/// verified parts instead of hashing the body and parsing the metadata again.
mod verified {
    use super::*;
    use std::collections::HashMap;

    #[cfg(test)]
    static DIGESTS: std::sync::LazyLock<Mutex<HashMap<PathBuf, u64>>> =
        std::sync::LazyLock::new(Mutex::default);
    #[cfg(test)]
    pub(super) fn record_digest(canonical: &Path) {
        *DIGESTS
            .lock()
            .or_poisoned()
            .entry(canonical.to_path_buf())
            .or_default() += 1;
    }
    #[cfg(test)]
    pub(super) fn digests(canonical: &Path) -> u64 {
        DIGESTS
            .lock()
            .or_poisoned()
            .get(canonical)
            .copied()
            .unwrap_or(0)
    }

    #[derive(Clone, Copy, PartialEq, Eq)]
    pub(super) struct Identity {
        device: u64,
        inode: u64,
        length: u64,
        changed: (i64, i64),
        modified: (i64, i64),
    }

    pub(super) fn identity(file: &fs::File) -> io::Result<Option<Identity>> {
        #[cfg(unix)]
        {
            use std::os::unix::fs::MetadataExt;
            let metadata = file.metadata()?;
            Ok(Some(Identity {
                device: metadata.dev(),
                inode: metadata.ino(),
                length: metadata.len(),
                changed: (metadata.ctime(), metadata.ctime_nsec()),
                modified: (metadata.mtime(), metadata.mtime_nsec()),
            }))
        }
        #[cfg(not(unix))]
        {
            let _ = file;
            Ok(None)
        }
    }

    struct Entry {
        html: Identity,
        metadata: Identity,
        parts: ResponseParts,
    }

    const CAPACITY: usize = 4096;
    static ENTRIES: std::sync::LazyLock<Mutex<HashMap<PathBuf, Entry>>> =
        std::sync::LazyLock::new(Mutex::default);

    pub(super) fn lookup(
        canonical: &Path,
        html: Identity,
        metadata: Identity,
    ) -> Option<ResponseParts> {
        ENTRIES
            .lock()
            .or_poisoned()
            .get(canonical)
            .filter(|entry| entry.html == html && entry.metadata == metadata)
            .map(|entry| entry.parts.clone())
    }

    pub(super) fn remember(
        canonical: &Path,
        html: Identity,
        metadata: Identity,
        parts: &ResponseParts,
    ) {
        let mut entries = ENTRIES.lock().or_poisoned();
        if entries.len() >= CAPACITY && !entries.contains_key(canonical) {
            entries.clear();
        }
        entries.insert(
            canonical.to_path_buf(),
            Entry {
                html,
                metadata,
                parts: parts.clone(),
            },
        );
    }
}

fn read_paired_static_file(
    root: &Path,
    file_path: &Path,
) -> io::Result<(ntex_files::NamedFile, Option<ResponseParts>)> {
    let root = crate::fs_boundary::SiteRoot::open(root)?;
    let canonical = root.canonical_file(file_path)?;
    let (dir, name) = root.parent(&canonical, false)?;
    let lock = match publication_lock(&dir, false) {
        Ok(lock) => Some(lock),
        Err(error) if error.kind() == io::ErrorKind::NotFound => None,
        Err(error) => return Err(error),
    };
    let lockless = lock.is_none();
    let pin_snapshot = |lockless: bool| -> io::Result<(fs::File, Option<fs::File>)> {
        // Open HTML first: absence of metadata then identifies a pinned legacy
        // generation, since publishers always commit metadata before HTML.
        let file = crate::fs_boundary::open_regular(&dir, &name)?;
        #[cfg(test)]
        if lockless {
            test_hooks::after_lockless_pin(file_path)?;
        }
        #[cfg(not(test))]
        let _ = lockless;
        let metadata = open_static_metadata(&dir, &name)?;
        Ok((file, metadata))
    };
    let pinned = pin_snapshot(lockless);
    // Publishers replace both inodes. Once their handles are pinned together,
    // neither JSON parsing nor hashing needs to hold the directory read lock.
    drop(lock);
    let legacy = pinned
        .as_ref()
        .is_ok_and(|(_, metadata)| metadata.is_none());
    #[cfg(test)]
    if legacy {
        test_hooks::after_legacy_classification(file_path)?;
    }
    let mut snapshot = pinned.and_then(|pinned| parse_static_snapshot(&canonical, pinned));
    if lockless && !legacy {
        #[cfg(test)]
        test_hooks::after_lockless_validation(file_path)?;
        // Even a failed snapshot can overlap the first publisher. Recheck and
        // reopen both formats under its lock before returning that failure.
        match publication_lock(&dir, false) {
            Ok(lock) => {
                let pinned = pin_snapshot(false);
                drop(lock);
                snapshot = pinned.and_then(|pinned| parse_static_snapshot(&canonical, pinned));
            }
            Err(error) if error.kind() == io::ErrorKind::NotFound => {}
            Err(error) => return Err(error),
        }
    }
    let (file, parts) = snapshot?;
    Ok((ntex_files::NamedFile::from_file(file, canonical)?, parts))
}

fn parse_static_snapshot(
    canonical: &Path,
    (mut file, metadata): (fs::File, Option<fs::File>),
) -> io::Result<(fs::File, Option<ResponseParts>)> {
    let Some(metadata) = metadata else {
        return Ok((file, None));
    };
    let identities = match (verified::identity(&file)?, verified::identity(&metadata)?) {
        (Some(html), Some(metadata)) => Some((html, metadata)),
        _ => None,
    };
    if let Some((html, metadata)) = identities
        && let Some(parts) = verified::lookup(canonical, html, metadata)
    {
        return Ok((file, Some(parts)));
    }
    let (version, digest, status, headers): StoredResponse =
        serde_json::from_reader(std::io::BufReader::new(metadata))?;
    #[cfg(test)]
    verified::record_digest(canonical);
    if version != 1 || digest != body_digest(&mut file)? {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "inconsistent static representation",
        ));
    }
    let mut parts = ResponseParts {
        status: status
            .map(StatusCode::from_u16)
            .transpose()
            .map_err(io::Error::other)?,
        ..Default::default()
    };
    for (name, value) in headers {
        parts.headers.append(
            header::HeaderName::from_bytes(name.as_bytes()).map_err(io::Error::other)?,
            header::HeaderValue::from_bytes(&value).map_err(io::Error::other)?,
        );
    }
    if let Some((html, metadata)) = identities {
        verified::remember(canonical, html, metadata, &parts);
    }
    Ok((file, Some(parts)))
}

#[cfg(test)]
async fn write_static_route(
    options: &LeptosOptions,
    response_options: Option<ResponseOptions>,
    path: &str,
    html: String,
) -> io::Result<()> {
    write_static_route_managed(options, response_options, path, html, None, None)
        .await
        .map_err(|error| match error {
            StaticPolicyError::Storage(StaticStorageError::Io(error)) => {
                io::Error::new(error.kind(), error.to_string())
            }
            other => io::Error::other(other),
        })
}

async fn write_static_route_managed(
    options: &LeptosOptions,
    response_options: Option<ResponseOptions>,
    path: &str,
    html: String,
    policy: Option<StaticRoutePolicy>,
    render_permit: Option<Permit>,
) -> Result<(), StaticPolicyError> {
    use sha2::Digest;
    let snapshot = response_options
        .map(|options| options.0.read().or_poisoned().clone())
        .unwrap_or_default();
    let file_path = static_path(options, path)
        .ok_or_else(|| StaticPolicyError::Render("invalid static route path".into()))?;
    let root_path = PathBuf::from(&*options.site_root);
    ntex::rt::spawn_blocking(move || {
        // The blocking operation owns its render permit even if its async
        // caller is canceled. Root coordination covers all staging and cleanup.
        let _render_permit = render_permit;
        let coordination =
            storage::coordinate(&root_path, policy.as_ref().map(StaticRoutePolicy::storage))?;
        let root = coordination.site_root()?;
        #[cfg(test)]
        test_hooks::before_digest(&file_path)?;
        let digest = format!("{:x}", sha2::Sha256::digest(html.as_bytes()));
        let headers = snapshot
            .headers
            .iter()
            .map(|(name, value)| (name.as_str().to_owned(), value.as_bytes().to_vec()))
            .collect::<Vec<_>>();
        let metadata: StoredResponse = (
            1,
            digest,
            snapshot.status.map(|status| status.as_u16()),
            headers,
        );
        let metadata_bytes = serde_json::to_vec(&metadata).map_err(io::Error::other)?;
        coordination.admit(&file_path, html.len() as u64, metadata_bytes.len() as u64)?;
        let (dir, name) = root.parent(&file_path, true)?;
        let _lock = lock_file(&dir, &name, true)?;
        let metadata_path = metadata_name(&name);
        let html_temp = prepare_file(&dir, &name, html.as_bytes())?;
        let metadata_temp = prepare_file(&dir, &metadata_path, &metadata_bytes)?;
        let _publication = publication_lock(&dir, true)?;
        dir.create_dir_all(METADATA_DIRECTORY)?;
        publish_file(&dir, metadata_temp, &metadata_path)?;
        #[cfg(unix)]
        dir.open_dir(METADATA_DIRECTORY)?
            .into_std_file()
            .sync_all()?;
        #[cfg(test)]
        test_hooks::after_metadata(&file_path)?;
        publish_file(&dir, html_temp, &name)?;
        #[cfg(unix)]
        dir.try_clone()?.into_std_file().sync_all()?;
        coordination.commit();
        Ok::<_, StaticStorageError>(())
    })
    .await
    .map_err(|error| StaticPolicyError::Render(error.to_string()))??;
    Ok(())
}

#[cfg(test)]
pub(crate) mod test_hooks {
    use super::*;
    use std::{
        collections::HashMap,
        sync::{LazyLock, atomic::AtomicUsize},
        task::{Poll, Waker},
    };
    struct MissBarrier {
        arrived: AtomicUsize,
        waiters: Mutex<Vec<Waker>>,
    }
    static MISSES: LazyLock<Mutex<HashMap<PathBuf, Arc<MissBarrier>>>> =
        LazyLock::new(Mutex::default);
    pub(crate) struct MissRegistration(PathBuf);
    impl Drop for MissRegistration {
        fn drop(&mut self) {
            MISSES.lock().or_poisoned().remove(&self.0);
        }
    }
    pub(crate) fn two_misses(root: PathBuf) -> MissRegistration {
        MISSES.lock().or_poisoned().insert(
            root.clone(),
            Arc::new(MissBarrier {
                arrived: AtomicUsize::new(0),
                waiters: Mutex::new(Vec::new()),
            }),
        );
        MissRegistration(root)
    }
    pub(super) async fn after_miss(root: &Path) {
        let barrier = MISSES.lock().or_poisoned().get(root).cloned();
        if let Some(barrier) = barrier {
            if barrier.arrived.fetch_add(1, Ordering::SeqCst) + 1 >= 2 {
                let waiters = std::mem::take(&mut *barrier.waiters.lock().or_poisoned());
                for waiter in waiters {
                    waiter.wake();
                }
            }
            futures::future::poll_fn(|cx| {
                let mut waiters = barrier.waiters.lock().or_poisoned();
                if barrier.arrived.load(Ordering::SeqCst) >= 2 {
                    Poll::Ready(())
                } else {
                    waiters.push(cx.waker().clone());
                    Poll::Pending
                }
            })
            .await;
        }
    }
    type PublicationHook = Box<dyn FnOnce() -> io::Result<()> + Send>;
    static LOCKLESS_PIN: LazyLock<Mutex<HashMap<PathBuf, PublicationHook>>> =
        LazyLock::new(Mutex::default);
    static LOCKLESS_VALIDATION: LazyLock<Mutex<HashMap<PathBuf, PublicationHook>>> =
        LazyLock::new(Mutex::default);
    pub(super) fn on_lockless_pin(path: PathBuf, hook: PublicationHook) {
        LOCKLESS_PIN.lock().or_poisoned().insert(path, hook);
    }
    pub(super) fn after_lockless_pin(path: &Path) -> io::Result<()> {
        let hook = LOCKLESS_PIN.lock().or_poisoned().remove(path);
        hook.map_or(Ok(()), |hook| hook())
    }
    pub(super) fn on_lockless_validation(path: PathBuf, hook: PublicationHook) {
        LOCKLESS_VALIDATION.lock().or_poisoned().insert(path, hook);
    }
    pub(super) fn after_lockless_validation(path: &Path) -> io::Result<()> {
        let hook = LOCKLESS_VALIDATION.lock().or_poisoned().remove(path);
        hook.map_or(Ok(()), |hook| hook())
    }
    pub(super) struct LocklessHookRegistration(pub PathBuf);
    impl Drop for LocklessHookRegistration {
        fn drop(&mut self) {
            LOCKLESS_PIN.lock().or_poisoned().remove(&self.0);
            LOCKLESS_VALIDATION.lock().or_poisoned().remove(&self.0);
            PUBLICATION.lock().or_poisoned().remove(&self.0);
        }
    }
    static DIGEST: LazyLock<Mutex<HashMap<PathBuf, PublicationHook>>> =
        LazyLock::new(Mutex::default);
    pub(super) struct DigestRegistration(PathBuf);
    impl Drop for DigestRegistration {
        fn drop(&mut self) {
            DIGEST.lock().or_poisoned().remove(&self.0);
        }
    }
    pub(super) fn on_digest(path: PathBuf, hook: PublicationHook) -> DigestRegistration {
        DIGEST.lock().or_poisoned().insert(path.clone(), hook);
        DigestRegistration(path)
    }
    pub(super) fn before_digest(path: &Path) -> io::Result<()> {
        let hook = DIGEST.lock().or_poisoned().remove(path);
        if let Some(hook) = hook {
            hook()?;
        }
        Ok(())
    }
    static PUBLICATION: LazyLock<Mutex<HashMap<PathBuf, PublicationHook>>> =
        LazyLock::new(Mutex::default);
    static LEGACY_READ: LazyLock<Mutex<HashMap<PathBuf, PublicationHook>>> =
        LazyLock::new(Mutex::default);
    #[must_use]
    pub(crate) struct HookRegistration {
        path: PathBuf,
        hooks: &'static Mutex<HashMap<PathBuf, PublicationHook>>,
    }
    impl Drop for HookRegistration {
        fn drop(&mut self) {
            let hook = self.hooks.lock().or_poisoned().remove(&self.path);
            drop(hook);
        }
    }
    fn register(
        hooks: &'static Mutex<HashMap<PathBuf, PublicationHook>>,
        path: PathBuf,
        hook: PublicationHook,
    ) -> HookRegistration {
        hooks.lock().or_poisoned().insert(path.clone(), hook);
        HookRegistration { path, hooks }
    }
    pub(super) fn on_legacy_read(path: PathBuf, hook: PublicationHook) -> HookRegistration {
        register(&LEGACY_READ, path, hook)
    }
    pub(super) fn after_legacy_classification(path: &Path) -> io::Result<()> {
        let hook = LEGACY_READ.lock().or_poisoned().remove(path);
        if let Some(hook) = hook {
            hook()?;
        }
        Ok(())
    }
    pub(super) fn on_publication(path: PathBuf, hook: PublicationHook) -> HookRegistration {
        register(&PUBLICATION, path, hook)
    }
    static REOPEN: LazyLock<Mutex<HashMap<PathBuf, PublicationHook>>> =
        LazyLock::new(Mutex::default);
    pub(crate) fn on_reopen(path: PathBuf, hook: PublicationHook) -> HookRegistration {
        register(&REOPEN, path, hook)
    }
    pub(super) fn before_reopen(path: &Path) -> io::Result<()> {
        let hook = REOPEN.lock().or_poisoned().remove(path);
        hook.map_or(Ok(()), |hook| hook())
    }
    pub(super) fn after_metadata(path: &Path) -> io::Result<()> {
        let hook = PUBLICATION.lock().or_poisoned().remove(path);
        if let Some(hook) = hook {
            hook()?;
        }
        Ok(())
    }
}

pub(crate) fn handle_static_route<IV, Err>(
    method: leptos_router::Method,
    additional_context: impl Fn() + 'static + Clone + Send,
    app_fn: impl Fn() -> IV + Clone + Send + 'static,
    regenerate: Vec<RegenerationFn>,
    runtime: Arc<StaticRuntime>,
) -> Route<Err>
where
    Err: ErrorRenderer,
    Err::Container: From<StateExtractorError>,
    IV: IntoView + 'static,
{
    ensure_executor_initialized();
    crate::request::check_registration_scope();
    let handler = move |req: HttpRequest, state: web::types::State<LeptosOptions>| {
        let app_fn = app_fn.clone();
        let additional_context = additional_context.clone();
        let regenerate = regenerate.clone();
        let runtime = runtime.clone();
        async move {
            let options = state.get_ref().clone();
            let path = req.uri().path().to_owned();
            let Some(file_path) = static_path(&options, &path) else {
                return HttpResponse::NotFound().finish();
            };
            let root = PathBuf::from(&*options.site_root);
            let (mut opened, mut parts) =
                open_static_response(root.clone(), file_path.clone()).await;
            let mut inline = None;
            if opened.is_none() {
                #[cfg(test)]
                test_hooks::after_miss(&root).await;
                let outcome = match runtime
                    .render(options, path, app_fn, additional_context, regenerate)
                    .await
                {
                    Ok(outcome) => outcome,
                    Err(err) => {
                        report_static_error(&err.to_string());
                        return if err.is_admission() {
                            HttpResponse::ServiceUnavailable().finish()
                        } else {
                            HttpResponse::InternalServerError().finish()
                        };
                    }
                };
                match outcome {
                    StaticOutcome::Inline {
                        html,
                        parts: rendered,
                    } => {
                        inline = Some(html);
                        parts = rendered;
                    }
                    StaticOutcome::Published => {
                        #[cfg(test)]
                        test_hooks::before_reopen(&root).expect("reopen hook failed");
                        (opened, parts) = open_static_response(root, file_path).await;
                    }
                }
            }
            let served_from_file = inline.is_none();
            let response = match inline {
                // An inline render is an error response by construction; its
                // captured status replaces this default below.
                Some(html) => HttpResponse::NotFound()
                    .content_type("text/html")
                    .body(html),
                None => match opened {
                    Some(mut file) => {
                        // MIME is application-selected representation metadata.
                        // Apply it before NamedFile evaluates HTTP conditions and
                        // ranges; the final header merge must not overwrite those
                        // derived responses. Malformed selected values retain the
                        // filename-derived default, like an absent override.
                        let media_type = parts.as_ref().and_then(|parts| {
                            parts
                                .headers
                                .get_all(header::CONTENT_TYPE)
                                .last()
                                .and_then(|value| value.to_str().ok())
                                .and_then(|value| value.parse().ok())
                        });
                        if let Some(media_type) = media_type {
                            file = file.set_content_type(media_type);
                        }
                        crate::files::file_response(
                            file,
                            &req,
                            parts
                                .as_ref()
                                .and_then(|parts| parts.status)
                                .unwrap_or(StatusCode::OK),
                        )
                    }
                    None => {
                        report_static_error("static representation unavailable after publication");
                        HttpResponse::InternalServerError().finish()
                    }
                },
            };
            let mut response = NtexResponse(response);
            if let Some(mut parts) = parts {
                if served_from_file {
                    // The file builder has already applied base status,
                    // preconditions and ranges to the selected representation.
                    parts.status = None;
                    // RFC 9110 §§15.3.7/15.4.5: Content-Location remains
                    // required, while redundant language metadata is omitted
                    // for revalidation and a range resumed with If-Range.
                    if response.0.status() == StatusCode::NOT_MODIFIED
                        || (response.0.status() == StatusCode::PARTIAL_CONTENT
                            && req.headers().contains_key(header::IF_RANGE))
                    {
                        parts.headers.remove(header::CONTENT_LANGUAGE);
                    }
                    for name in crate::response::REPRESENTATION_HEADERS.iter() {
                        parts.headers.remove(name);
                    }
                }
                response.extend_response_parts(parts);
            }
            crate::stream::terminate_on_body_error(&req, response.take())
        }
    };
    let route = Route::<Err>::new().to(handler);
    match method {
        leptos_router::Method::Get => {
            route.guard(ntex::web::guard::Any(ntex::web::guard::Get()).or(ntex::web::guard::Head()))
        }
        leptos_router::Method::Post => route.guard(ntex::web::guard::Post()),
        leptos_router::Method::Put => route.guard(ntex::web::guard::Put()),
        leptos_router::Method::Delete => route.guard(ntex::web::guard::Delete()),
        leptos_router::Method::Patch => route.guard(ntex::web::guard::Patch()),
    }
}

async fn open_static_response(
    root: PathBuf,
    path: PathBuf,
) -> (Option<ntex_files::NamedFile>, Option<ResponseParts>) {
    match ntex::rt::spawn_blocking(move || read_paired_static_file(&root, &path)).await {
        Ok(Ok((file, parts))) => (Some(file), parts),
        Ok(Err(err)) => {
            if err.kind() != io::ErrorKind::NotFound {
                report_static_error(&err);
            }
            (None, None)
        }
        Err(err) => {
            crate::files::warn_blocking_join_failed("static file open", &err);
            (None, None)
        }
    }
}

fn report_static_error(error: &(impl std::fmt::Display + ?Sized)) {
    #[cfg(feature = "tracing")]
    tracing::warn!("static rendering or storage failed: {error}");
    #[cfg(not(feature = "tracing"))]
    eprintln!("static rendering or storage failed: {error}");
}

#[cfg(test)]
mod tests {
    use super::*;
    use lets_expect::lets_expect;

    async fn cancelled_initial_render(survivor: bool) -> (usize, usize, bool) {
        use crate::tests::temp_site_root;
        use leptos::prelude::{Suspend, on_cleanup};
        use std::sync::atomic::AtomicUsize;
        ensure_executor_initialized();
        let root = temp_site_root("static_cancellation");
        let options = LeptosOptions::builder()
            .output_name("static_cancellation")
            .site_root(root.to_string_lossy().into_owned())
            .build();
        let runtime = Arc::new(StaticRuntime::default());
        let cleanup = Arc::new(AtomicUsize::new(0));
        let retained = Arc::new(Mutex::new(None));
        let (started_tx, started_rx) = futures::channel::oneshot::channel();
        let started_tx = Arc::new(Mutex::new(Some(started_tx)));
        let (release_tx, release_rx) = futures::channel::oneshot::channel::<()>();
        let release_rx = Arc::new(Mutex::new(Some(release_rx)));
        let app_fn = {
            let started_tx = started_tx.clone();
            let release_rx = release_rx.clone();
            move || {
                let started = started_tx.lock().unwrap().take().unwrap();
                let released = release_rx.lock().unwrap().take().unwrap();
                Suspend::new(async move {
                    let _ = started.send(());
                    let _ = released.await;
                    "released"
                })
            }
        };
        let context = {
            let cleanup = cleanup.clone();
            let retained = retained.clone();
            move || {
                *retained.lock().unwrap() = Owner::current();
                let cleanup = cleanup.clone();
                on_cleanup(move || {
                    cleanup.fetch_add(1, Ordering::SeqCst);
                });
            }
        };
        let mut first = Box::pin(runtime.render(
            options.clone(),
            "/pending".into(),
            app_fn.clone(),
            context.clone(),
            Vec::new(),
        ));
        match futures::future::select(first.as_mut(), started_rx).await {
            futures::future::Either::Right((Ok(()), _)) => {}
            _ => panic!("renderer must suspend before cancellation"),
        }
        let mut second =
            Box::pin(runtime.render(options, "/pending".into(), app_fn, context, Vec::new()));
        if survivor {
            // Poll until the second receiver is actually registered; the
            // renderer stays suspended on the independent release channel.
            futures::future::poll_fn(|cx| {
                assert!(second.as_mut().poll(cx).is_pending());
                let count = runtime
                    .work
                    .lock()
                    .unwrap()
                    .values()
                    .filter_map(std::sync::Weak::upgrade)
                    .map(|work| work.state.lock().unwrap().waiters.len())
                    .sum::<usize>();
                if count == 2 {
                    std::task::Poll::Ready(())
                } else {
                    std::task::Poll::Pending
                }
            })
            .await;
        }
        drop(first);
        let before = cleanup.load(Ordering::SeqCst);
        let completed = if survivor {
            release_tx.send(()).unwrap();
            second.await.is_ok()
        } else {
            drop(second);
            true
        };
        // A scheduler turn lets the task finish its destructor sequence.
        // The condition is observed directly, not inferred from a sleep.
        ntex::time::timeout(
            ntex::time::Millis(1000),
            futures::future::poll_fn(|cx| {
                if runtime.work.lock().unwrap().is_empty() {
                    std::task::Poll::Ready(())
                } else {
                    cx.waker().wake_by_ref();
                    std::task::Poll::Pending
                }
            }),
        )
        .await
        .expect("static task failed to terminate after cancellation/completion");
        let after = cleanup.load(Ordering::SeqCst);
        drop(retained);
        (before, after, completed)
    }

    lets_expect! {
        expect(crate::tests::run_ntex(cancelled_initial_render(survivor))) as the_pending_static_render {
            let survivor = false;
            to releases_the_owner_when_its_last_waiter_leaves { equal((0_usize, 1_usize, true)) }
            when another_waiter_remains {
                let survivor = true;
                to completes_for_the_survivor_before_cleanup { equal((0_usize, 1_usize, true)) }
            }
        }
    }

    const TEST_SITE_ROOT: &str = "/tmp/leptos_ntex_static_path_test";

    fn options() -> LeptosOptions {
        LeptosOptions::builder()
            .output_name("leptos_ntex_static_path_test")
            .site_root(TEST_SITE_ROOT)
            .site_pkg_dir("pkg")
            .build()
    }

    /// The on-disk path `static_path` is expected to resolve `rel` to, under
    /// the test `site_root`. `static_path` is pure (no filesystem access), so
    /// the directory need not exist for these assertions.
    fn under_site_root(rel: &str) -> Option<PathBuf> {
        Some(Path::new(TEST_SITE_ROOT).join(rel))
    }

    // ----- static_path: exhaustive spec --------------------------------
    // A pure URL-path -> on-disk-path resolver with two jobs: resolve the
    // happy path (append `.html`; map `/`, the empty path and trailing
    // slashes to `index.html`) and REJECT every traversal / dotfile /
    // smuggling shape. The old tests asserted only rejection (`None`); all
    // nine `Some` leaves below — the `.html` suffixing and the index
    // resolution — were previously unpinned, so a regression dropping
    // `.html` or mangling the join would not have been caught.
    lets_expect! {
        expect(static_path(&options(), path)) as the_resolved_static_path {
            let path = "/about";

            to resolves_to_the_html_file { equal(under_site_root("about.html")) }

            when the_path_is_the_site_root {
                let path = "/";
                to resolves_to_the_index_file { equal(under_site_root("index.html")) }
            }

            when the_path_is_empty {
                let path = "";
                to resolves_to_the_index_file { equal(under_site_root("index.html")) }
            }

            when the_path_has_a_trailing_slash {
                let path = "/blog/";
                to resolves_to_a_nested_index_file { equal(under_site_root("blog/index.html")) }
            }

            when the_path_has_redundant_leading_slashes {
                let path = "//about";
                to collapses_them_and_resolves_the_html_file {
                    equal(under_site_root("about.html"))
                }
            }

            // The segment loop skips every empty split (`if segment.is_empty()
            // { continue; }`), so a redundant slash is NOT special-cased to the
            // leading position — an interior doubled "/" collapses exactly like
            // a leading one, rather than being rejected as malformed input.
            when the_path_has_a_redundant_interior_slash {
                let path = "/blog//post-1";
                to collapses_it_and_resolves_the_same_file_as_a_single_slash {
                    equal(under_site_root("blog/post-1.html"))
                }
            }

            when the_path_is_nested {
                let path = "/blog/post-1";
                to resolves_to_the_nested_html_file {
                    equal(under_site_root("blog/post-1.html"))
                }
            }

            when a_segment_is_percent_encoded {
                let path = "/foo%20bar";
                to decodes_the_segment_before_resolving { equal(under_site_root("foo bar.html")) }
            }

            when a_segment_decodes_to_non_ascii_utf8 {
                let path = "/r%C3%A9sum%C3%A9";
                to decodes_the_utf8_segment { equal(under_site_root("résumé.html")) }
            }

            when a_segment_is_the_current_directory {
                let path = "/sub/./x";
                to is_rejected { be_none }
            }

            when a_segment_is_a_parent_traversal {
                let path = "/static/../outside";
                to is_rejected { be_none }
            }

            when a_segment_is_an_encoded_parent_traversal {
                let path = "/static/%2e%2e/outside";
                to is_rejected { be_none }
            }

            when an_encoded_parent_traversal_is_uppercase {
                let path = "/a/%2E%2E/b";
                to is_rejected { be_none }
            }

            when a_segment_is_a_dotfile {
                let path = "/static/.env";
                to is_rejected { be_none }
            }

            // RFC 8615: the exact `.well-known` segment is exempted from the
            // dotfile guard (in both this resolver and the file fallback's
            // `safe_subpath`), so ACME challenges / security.txt resolve. The
            // exemption is narrow — see the two rejection leaves below.
            when the_path_is_a_well_known_uri {
                let path = "/.well-known/acme-challenge/token";
                to resolves_under_well_known {
                    equal(under_site_root(".well-known/acme-challenge/token.html"))
                }
            }

            when a_dotfile_is_nested_inside_well_known {
                let path = "/.well-known/.secret";
                to is_still_rejected { be_none }
            }

            when a_traversal_hides_behind_well_known {
                let path = "/.well-known/../etc/passwd";
                to is_still_rejected { be_none }
            }

            when a_segment_contains_a_nul_byte {
                let path = "/a\0b";
                to is_rejected { be_none }
            }

            when a_segment_encodes_a_path_separator {
                let path = "/static/subdir%2F.env";
                to is_rejected { be_none }
            }

            when an_encoded_separator_is_leading {
                let path = "/%2Fetc";
                to is_rejected { be_none }
            }

            when a_segment_contains_a_backslash {
                let path = "/a\\b";
                to is_rejected { be_none }
            }

            when a_segment_is_not_valid_utf8_after_decoding {
                let path = "/file%FFname";
                to is_rejected { be_none }
            }
        }
    }

    use crate::tests::{run_ntex, temp_site_root};

    type FilenamePublication = Result<(String, Option<StatusCode>, Vec<Vec<u8>>, usize), String>;

    async fn publish_fitting_filename(segment: String) -> FilenamePublication {
        use std::io::Read;
        let root = temp_site_root("fitting_static_filename");
        let options = LeptosOptions::builder()
            .output_name("fitting_static_filename")
            .site_root(root.to_string_lossy().into_owned())
            .build();
        let file_name = format!("{segment}.html");
        let response = ResponseOptions::default();
        response.set_status(StatusCode::CREATED);
        response.append_header(header::SET_COOKIE, header::HeaderValue::from_static("a=1"));
        response.append_header(header::SET_COOKIE, header::HeaderValue::from_static("b=2"));
        write_static_route(
            &options,
            Some(response),
            &format!("/{segment}"),
            "complete-long-name-body".into(),
        )
        .await
        .map_err(|error| format!("publication failed: {error}"))?;
        let (mut file, parts) = read_paired_static_file(&root, &root.join(file_name))
            .map_err(|error| format!("durable read failed: {error}"))?;
        let parts = parts.ok_or("published response has no durable metadata")?;
        let mut body = String::new();
        file.read_to_string(&mut body)
            .map_err(|error| format!("body read failed: {error}"))?;
        let cookies = parts
            .headers
            .get_all(header::SET_COOKIE)
            .map(|value| value.as_bytes().to_vec())
            .collect();
        let leftovers = fs::read_dir(&root)
            .unwrap()
            .map(Result::unwrap)
            .filter(|entry| entry.file_name().to_string_lossy().contains(".tmp."))
            .count();
        Ok((body, parts.status, cookies, leftovers))
    }

    fn be_a_complete_filename_publication(
        actual: &FilenamePublication,
    ) -> lets_expect::AssertionResult {
        lets_expect::equal(Ok((
            "complete-long-name-body".to_owned(),
            Some(StatusCode::CREATED),
            vec![b"a=1".to_vec(), b"b=2".to_vec()],
            0,
        )))(actual)
    }

    lets_expect! {
        expect(run_ntex(publish_fitting_filename(segment))) as static_publication_with_fitting_names {
            let segment = "a".repeat(230);
            to publishes_the_complete_representation { be_a_complete_filename_publication }
            when the_html_name_reaches_the_portable_limit {
                let segment = "a".repeat(250);
                to publishes_the_complete_representation { be_a_complete_filename_publication }
            }
            when the_html_name_reaches_the_portable_limit_in_utf8_bytes {
                let segment = "界".repeat(83);
                to publishes_the_complete_representation { be_a_complete_filename_publication }
            }
        }
    }

    #[derive(Debug)]
    struct PublicationObservation {
        body: String,
        status: Option<StatusCode>,
        cookies: Vec<Vec<u8>>,
        leftovers: usize,
    }

    async fn published_response(captured: bool) -> PublicationObservation {
        use std::io::Read;
        let root = temp_site_root("durable_publication");
        let options = LeptosOptions::builder()
            .output_name("durable_publication")
            .site_root(root.to_string_lossy().into_owned())
            .build();
        let response = captured.then(|| {
            let parts = ResponseOptions::default();
            parts.set_status(StatusCode::CREATED);
            parts.append_header(header::SET_COOKIE, header::HeaderValue::from_static("a=1"));
            parts.append_header(header::SET_COOKIE, header::HeaderValue::from_static("b=2"));
            parts
        });
        write_static_route(&options, response, "/page", "published-body".into())
            .await
            .unwrap();
        let (mut file, parts) = read_paired_static_file(&root, &root.join("page.html")).unwrap();
        let mut body = String::new();
        file.read_to_string(&mut body).unwrap();
        let parts = parts.unwrap();
        let cookies = parts
            .headers
            .get_all(header::SET_COOKIE)
            .map(|value| value.as_bytes().to_vec())
            .collect();
        let leftovers = fs::read_dir(&root)
            .unwrap()
            .map(Result::unwrap)
            .filter(|entry| entry.file_name().to_string_lossy().contains(".tmp."))
            .count();
        PublicationObservation {
            body,
            status: parts.status,
            cookies,
            leftovers,
        }
    }

    lets_expect! {
        expect(run_ntex(published_response(captured))) as the_durable_static_response {
            let captured = true;
            to keeps_body_and_status_together { have(body) equal("published-body".to_owned()), have(status) equal(Some(StatusCode::CREATED)) }
            to preserves_repeated_cookie_values { have(cookies) equal(vec![b"a=1".to_vec(), b"b=2".to_vec()]) }
            to removes_owned_temporary_files { have(leftovers) equal(0_usize) }
            when no_response_options_were_provided {
                let captured = false;
                to persists_the_default_representation { have(body) equal("published-body".to_owned()), have(status) be_none, have(cookies) equal(Vec::<Vec<u8>>::new()) }
            }
        }
    }

    #[derive(Debug)]
    struct AtomicObservation {
        result: Result<(), io::ErrorKind>,
        body: Option<String>,
        directory: bool,
        leftovers: usize,
    }
    fn atomic_publication(directory: bool) -> AtomicObservation {
        let root = temp_site_root("atomic_contract");
        let target = root.join("page.html");
        if directory {
            fs::create_dir(&target).unwrap();
        } else {
            fs::write(&target, "previous body").unwrap();
        }
        let result = write_file_atomic(&target, b"new complete body").map_err(|err| err.kind());
        let body = fs::read_to_string(&target).ok();
        let leftovers = fs::read_dir(&root)
            .unwrap()
            .map(Result::unwrap)
            .filter(|entry| entry.file_name().to_string_lossy().contains(".tmp."))
            .count();
        AtomicObservation {
            result,
            body,
            directory: target.is_dir(),
            leftovers,
        }
    }
    lets_expect! {
        expect(atomic_publication(directory)) as atomic_replacement {
            let directory = false;
            to replaces_the_whole_body { have(result) be_ok, have(body) equal(Some("new complete body".to_owned())) }
            to removes_the_temporary_file { have(leftovers) equal(0_usize) }
            when the_destination_is_a_directory {
                let directory = true;
                to preserves_the_directory_on_failure { have(result) be_err, have(directory) be_true, have(leftovers) equal(0_usize) }
            }
        }
    }

    fn parent_validation(no_parent: bool) -> Result<(), io::ErrorKind> {
        let root = temp_site_root("parent_validation");
        let path = if no_parent {
            PathBuf::from("/")
        } else {
            root.join("nested/new/page.html")
        };
        validate_static_parent(&root, &path).map_err(|err| err.kind())
    }
    lets_expect! {
        expect(parent_validation(no_parent)) as the_static_parent_validation {
            let no_parent = false;
            to creates_parents_inside_the_root { be_ok }
            when the_path_has_no_parent {
                let no_parent = true;
                to returns_invalid_input { be_err_and equal(io::ErrorKind::InvalidInput) }
            }
        }
    }

    #[derive(Clone, Copy)]
    enum MissingPart {
        None,
        File,
        Root,
        Directory,
    }
    fn validated_file(kind: MissingPart) -> bool {
        let parent = temp_site_root("file_validation");
        let root = parent.join("site");
        let file = root.join("page.html");
        if !matches!(kind, MissingPart::Root) {
            fs::create_dir(&root).unwrap();
        }
        match kind {
            MissingPart::None => fs::write(&file, "hello").unwrap(),
            MissingPart::Directory => fs::create_dir(&file).unwrap(),
            _ => {}
        }
        validate_static_file(&root, &file).is_some()
    }
    lets_expect! {
        expect(validated_file(kind)) as the_openable_static_file {
            let kind = MissingPart::None;
            to accepts_a_regular_file_inside_root { be_true }
            when the_file_is_missing { let kind = MissingPart::File; to is_a_miss { be_false } }
            when the_root_is_missing { let kind = MissingPart::Root; to is_a_miss { be_false } }
            when the_target_is_a_directory { let kind = MissingPart::Directory; to is_a_miss { be_false } }
        }
    }

    #[cfg(unix)]
    mod unix {
        use super::*;
        use std::io::Read;

        #[derive(Debug)]
        struct EscapeObservation {
            error: io::ErrorKind,
            created_outside: bool,
            opened_outside: bool,
        }
        fn escaped_parent() -> EscapeObservation {
            let parent = temp_site_root("parent_escape_contract");
            let root = parent.join("site");
            let outside = parent.join("outside");
            fs::create_dir(&root).unwrap();
            fs::create_dir(&outside).unwrap();
            fs::write(outside.join("secret.html"), "SECRET").unwrap();
            std::os::unix::fs::symlink(&outside, root.join("escape")).unwrap();
            let error = validate_static_parent(&root, &root.join("escape/new/deep/page.html"))
                .unwrap_err()
                .kind();
            EscapeObservation {
                error,
                created_outside: outside.join("new").exists(),
                opened_outside: validate_static_file(&root, &root.join("escape/secret.html"))
                    .is_some(),
            }
        }
        lets_expect! {
            expect(escaped_parent()) as the_external_symlink_boundary {
                to returns_a_permission_error { have(error) equal(io::ErrorKind::PermissionDenied) }
                to performs_no_outside_directory_creation { have(created_outside) be_false }
                to does_not_open_the_outside_file { have(opened_outside) be_false }
            }
        }

        async fn alias_pair(nested: bool) -> (String, Option<StatusCode>) {
            let parent = temp_site_root("paired_alias_contract");
            let root = parent.join("site");
            fs::create_dir(&root).unwrap();
            let alias = parent.join("alias");
            std::os::unix::fs::symlink(&root, &alias).unwrap();
            let first = LeptosOptions::builder()
                .output_name("alias")
                .site_root(root.to_string_lossy().into_owned())
                .build();
            let second = LeptosOptions::builder()
                .output_name("alias")
                .site_root(
                    (if nested { root.join("nested") } else { alias })
                        .to_string_lossy()
                        .into_owned(),
                )
                .build();
            let first_path = if nested { "/nested/page" } else { "/page" };
            let response = ResponseOptions::default();
            response.set_status(StatusCode::CREATED);
            write_static_route(&first, Some(response), first_path, "epoch-one".into())
                .await
                .unwrap();
            let response = ResponseOptions::default();
            response.set_status(StatusCode::ACCEPTED);
            write_static_route(&second, Some(response), "/page", "epoch-two".into())
                .await
                .unwrap();
            let (mut file, parts) =
                read_paired_static_file(&root, &static_path(&first, first_path).unwrap()).unwrap();
            let mut html = String::new();
            file.read_to_string(&mut html).unwrap();
            (html, parts.and_then(|parts| parts.status))
        }
        lets_expect! {
            expect(run_ntex(alias_pair(nested))) as the_physical_artifact_identity {
                let nested = false;
                to reads_the_latest_body_and_status_through_a_root_alias { equal(("epoch-two".to_owned(), Some(StatusCode::ACCEPTED))) }
                when the_roots_are_nested {
                    let nested = true;
                    to reads_the_same_physical_pair { equal(("epoch-two".to_owned(), Some(StatusCode::ACCEPTED))) }
                }
            }
        }
    }
    #[derive(Debug)]
    struct CommitObservation {
        lock_blocked: bool,
        incomplete_rejected: bool,
        body: String,
        status: Option<StatusCode>,
    }
    async fn publication_interleaving(interrupt: bool) -> CommitObservation {
        use std::io::Read;
        let root = temp_site_root("controlled_publication");
        let options = LeptosOptions::builder()
            .output_name("controlled_publication")
            .site_root(root.to_string_lossy().into_owned())
            .build();
        let response = ResponseOptions::default();
        response.set_status(StatusCode::CREATED);
        write_static_route(&options, Some(response), "/page", "old-body".into())
            .await
            .unwrap();
        let path = root.join("page.html");
        let mut lock_blocked = false;
        let mut incomplete_rejected = false;
        if interrupt {
            let _publication = test_hooks::on_publication(
                path.clone(),
                Box::new(|| {
                    Err(io::Error::other(
                        "controlled interruption after metadata publication",
                    ))
                }),
            );
            let response = ResponseOptions::default();
            response.set_status(StatusCode::ACCEPTED);
            let result =
                write_static_route(&options, Some(response), "/page", "new-body".into()).await;
            assert!(
                result.is_err(),
                "the injected publication interruption must be reached"
            );
            incomplete_rejected = read_paired_static_file(&root, &path)
                .is_err_and(|error| error.kind() == io::ErrorKind::InvalidData);
        } else {
            let (entered_tx, entered_rx) = futures::channel::oneshot::channel();
            let (release_tx, release_rx) = std::sync::mpsc::channel();
            let _publication = test_hooks::on_publication(
                path.clone(),
                Box::new(move || {
                    let _ = entered_tx.send(());
                    release_rx.recv().map_err(io::Error::other)
                }),
            );
            let options = options.clone();
            let writer = ntex::rt::spawn(async move {
                let response = ResponseOptions::default();
                response.set_status(StatusCode::ACCEPTED);
                write_static_route(&options, Some(response), "/page", "new-body".into()).await
            });
            let writer = match futures::future::select(Box::pin(entered_rx), Box::pin(writer)).await
            {
                futures::future::Either::Left((Ok(()), writer)) => writer,
                futures::future::Either::Left((Err(error), _)) => {
                    panic!("publication hook ended: {error}")
                }
                futures::future::Either::Right((result, _)) => {
                    panic!("writer ended before publication hook: {result:?}")
                }
            };
            let site = crate::fs_boundary::SiteRoot::open(&root).unwrap();
            let (parent, _) = site.parent(&path, false).unwrap();
            let lock =
                crate::fs_boundary::open_regular(&parent, Path::new(PUBLICATION_LOCK)).unwrap();
            lock_blocked = matches!(
                fs4::FileExt::try_lock_shared(&lock),
                Err(fs4::TryLockError::WouldBlock)
            );
            release_tx.send(()).unwrap();
            writer.await.unwrap().unwrap();
        }
        if interrupt {
            let response = ResponseOptions::default();
            response.set_status(StatusCode::ACCEPTED);
            write_static_route(&options, Some(response), "/page", "new-body".into())
                .await
                .unwrap();
        }
        let (mut file, parts) = read_paired_static_file(&root, &path).unwrap();
        let mut body = String::new();
        file.read_to_string(&mut body).unwrap();
        CommitObservation {
            lock_blocked,
            incomplete_rejected,
            body,
            status: parts.and_then(|parts| parts.status),
        }
    }
    lets_expect! {
        expect(run_ntex(publication_interleaving(interrupt))) as the_controlled_static_commit {
            let interrupt = false;
            to excludes_a_reader_between_metadata_and_html { have(lock_blocked) be_true }
            to publishes_one_complete_response { have(body) equal("new-body".to_owned()), have(status) equal(Some(StatusCode::ACCEPTED)) }
            when publication_is_interrupted_after_metadata {
                let interrupt = true;
                to rejects_the_incomplete_representation { have(incomplete_rejected) be_true }
                to recovers_with_a_complete_representation { have(body) equal("new-body".to_owned()), have(status) equal(Some(StatusCode::ACCEPTED)) }
            }
        }
    }

    #[cfg(unix)]
    fn occupied_temporary() -> (Result<(), io::ErrorKind>, String, bool) {
        let root = temp_site_root("occupied_temp_contract");
        let outside = root.join("outside.txt");
        fs::write(&outside, "UNRELATED").unwrap();
        let name = PathBuf::from(".occupied.tmp");
        std::os::unix::fs::symlink(&outside, root.join(&name)).unwrap();
        let dir = cap_std::fs::Dir::open_ambient_dir(root.as_path(), cap_std::ambient_authority())
            .unwrap();
        let result = create_temporary(&dir, name.clone(), b"RENDER")
            .map(drop)
            .map_err(|error| error.kind());
        (
            result,
            fs::read_to_string(&outside).unwrap(),
            fs::symlink_metadata(root.join(name)).is_ok_and(|meta| meta.file_type().is_symlink()),
        )
    }
    #[cfg(unix)]
    lets_expect! {
        expect(occupied_temporary()) as the_preexisting_temporary_entry {
            to does_not_modify_or_remove_a_foreign_entry { equal((Err(io::ErrorKind::AlreadyExists), "UNRELATED".to_owned(), true)) }
        }
    }
    async fn legacy_transition() -> (String, Option<StatusCode>) {
        use std::io::Read;
        let root = temp_site_root("legacy_transition");
        let options = LeptosOptions::builder()
            .output_name("legacy_transition")
            .site_root(root.to_string_lossy().into_owned())
            .build();
        let path = root.join("page.html");
        fs::write(&path, "legacy-body").unwrap();
        let (entered_tx, entered_rx) = futures::channel::oneshot::channel();
        let (release_tx, release_rx) = std::sync::mpsc::channel();
        let _legacy = test_hooks::on_legacy_read(
            path.clone(),
            Box::new(move || {
                let _ = entered_tx.send(());
                release_rx.recv().map_err(io::Error::other)
            }),
        );
        let reading_root = root.clone();
        let reader = ntex::rt::spawn_blocking(move || {
            let (mut file, parts) = read_paired_static_file(&reading_root, &path).unwrap();
            let mut body = String::new();
            file.read_to_string(&mut body).unwrap();
            (body, parts.and_then(|parts| parts.status))
        });
        let reader = match futures::future::select(Box::pin(entered_rx), Box::pin(reader)).await {
            futures::future::Either::Left((Ok(()), reader)) => reader,
            futures::future::Either::Left((Err(error), _)) => panic!("legacy hook ended: {error}"),
            futures::future::Either::Right((result, _)) => {
                panic!("reader ended before legacy hook: {result:?}")
            }
        };
        let response = ResponseOptions::default();
        response.set_status(StatusCode::CREATED);
        write_static_route(&options, Some(response), "/page", "new-body".into())
            .await
            .unwrap();
        release_tx.send(()).unwrap();
        reader.await.unwrap()
    }
    lets_expect! {
        expect(run_ntex(legacy_transition())) as the_legacy_to_generated_transition {
            to keeps_the_opened_legacy_generation { equal(("legacy-body".to_owned(), None)) }
        }
    }
}

#[cfg(test)]
mod scoped_work_specs {
    use super::ScopedWork;
    use leptos::prelude::*;
    use lets_expect::*;
    use std::sync::{Arc, Mutex};

    #[derive(Clone)]
    struct Scope(u8);
    struct PendingChildStream {
        value: StoredValue<usize>,
        panic_poll: bool,
        events: Arc<Mutex<Vec<String>>>,
    }
    impl futures::Stream for PendingChildStream {
        type Item = ();
        fn poll_next(
            self: std::pin::Pin<&mut Self>,
            _: &mut std::task::Context<'_>,
        ) -> std::task::Poll<Option<()>> {
            self.events.lock().unwrap().push(format!(
                "poll:{:?}",
                use_context::<Scope>().map(|value| value.0)
            ));
            if self.panic_poll {
                panic!("controlled scoped task panic");
            }
            std::task::Poll::Pending
        }
    }
    impl Drop for PendingChildStream {
        fn drop(&mut self) {
            self.events.lock().unwrap().push(format!(
                "drop:{:?}:{:?}",
                use_context::<Scope>().map(|value| value.0),
                self.value.try_get_value(),
            ));
        }
    }
    #[derive(Clone, Copy, Debug, PartialEq)]
    enum TaskPollResult {
        Pending,
        Panicked,
        UnexpectedReady,
    }
    #[derive(Debug, PartialEq)]
    struct CancellationObservation {
        events: Vec<String>,
        caller_after_poll: Option<u8>,
        caller_after_drop: Option<u8>,
        result: TaskPollResult,
    }
    fn cancelled_task(caller_context: Option<u8>, panic_poll: bool) -> CancellationObservation {
        use futures::{Future, StreamExt};
        let _restore = crate::owner::RestoreOwner::capture();
        let events = Arc::new(Mutex::new(Vec::new()));
        let parent = Owner::new_root(None);
        parent.with(|| {
            provide_context(Scope(53));
            let events = events.clone();
            on_cleanup(move || events.lock().unwrap().push("parent_cleanup".into()));
        });
        let observed = events.clone();
        let mut task = parent.with(|| {
            ScopedWork::new(async move {
                observed.lock().unwrap().push(format!(
                    "task:{:?}",
                    use_context::<Scope>().map(|value| value.0)
                ));
                let child = Owner::new();
                let mut stream = child.with(|| {
                    provide_context(Scope(79));
                    ScopedWork::new(PendingChildStream {
                        value: StoredValue::new(7),
                        panic_poll,
                        events: observed,
                    })
                });
                stream.next().await;
            })
        });
        // Only the task's captured scope retains the parent now. Its Drop must
        // follow the nested stream's Drop, including when poll unwinds.
        drop(parent);
        let caller = caller_context.map(|value| {
            let owner = Owner::new_root(None);
            owner.with(|| provide_context(Scope(value)));
            owner.set();
            owner
        });
        if caller.is_none()
            && let Some(current) = Owner::current()
        {
            current.unset();
        }
        let waker = futures::task::noop_waker();
        let mut cx = std::task::Context::from_waker(&waker);
        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            std::pin::Pin::new(&mut task).poll(&mut cx)
        }));
        let result = match result {
            Ok(std::task::Poll::Pending) => TaskPollResult::Pending,
            Ok(std::task::Poll::Ready(())) => TaskPollResult::UnexpectedReady,
            Err(payload) => {
                assert_eq!(
                    payload.downcast_ref::<&str>().copied(),
                    Some("controlled scoped task panic"),
                    "only the fixture stream may produce the expected panic"
                );
                TaskPollResult::Panicked
            }
        };
        let caller_after_poll = use_context::<Scope>().map(|value| value.0);
        drop(task);
        let caller_after_drop = use_context::<Scope>().map(|value| value.0);
        drop(caller);
        let events = events.lock().unwrap().clone();
        CancellationObservation {
            events,
            caller_after_poll,
            caller_after_drop,
            result,
        }
    }
    fn preserve_scope_on_drop(
        caller: Option<u8>,
        result: TaskPollResult,
    ) -> impl Fn(&CancellationObservation) -> AssertionResult {
        move |actual| {
            equal(CancellationObservation {
                events: vec![
                    "task:Some(53)".into(),
                    "poll:Some(79)".into(),
                    "drop:Some(79):Some(7)".into(),
                    "parent_cleanup".into(),
                ],
                caller_after_poll: caller,
                caller_after_drop: caller,
                result,
            })(actual)
        }
    }
    lets_expect! {
        expect(cancelled_task(caller_context, panic_poll)) as scoped_static_task_cancellation {
            let caller_context = Some(99);
            let panic_poll = false;
            let result = TaskPollResult::Pending;
            to preserves_child_scope_until_drop_and_restores_caller { preserve_scope_on_drop(caller_context, result) }
            when polling_panics {
                let panic_poll = true;
                let result = TaskPollResult::Panicked;
                to preserves_child_scope_during_unwind_and_restores_caller { preserve_scope_on_drop(caller_context, result) }
            }
            when caller_has_no_owner {
                let caller_context = None;
                to preserves_child_scope_until_drop_and_restores_caller { preserve_scope_on_drop(caller_context, result) }
                when polling_panics {
                    let panic_poll = true;
                    let result = TaskPollResult::Panicked;
                    to preserves_child_scope_during_unwind_and_restores_caller { preserve_scope_on_drop(caller_context, result) }
                }
            }
        }
    }
}

#[cfg(all(test, unix))]
mod lockless_read_tests {
    use super::*;
    use lets_expect::*;
    use std::{io::Read, os::unix::fs::MetadataExt, time::Duration};

    #[derive(Debug)]
    struct Observation {
        old_inode: u64,
        returned_inode: u64,
        published_inode: u64,
        status: Option<StatusCode>,
        body: String,
        writer_holds_exclusive_lock: bool,
        expected_body: String,
    }

    struct ReleaseWriter(Option<std::sync::mpsc::Sender<()>>);
    impl Drop for ReleaseWriter {
        fn drop(&mut self) {
            if let Some(sender) = self.0.take() {
                let _ = sender.send(());
            }
        }
    }

    async fn first_writer_during_read(changed_body: bool) -> Result<Observation, String> {
        let new_body = if changed_body {
            "new-body"
        } else {
            "same-body"
        };
        let root = crate::tests::temp_site_root("static_first_writer");
        let segment = "page";
        let filename = format!("{segment}.html");
        let route = format!("/{segment}");
        let path = root.join(&filename);
        let options = LeptosOptions::builder()
            .output_name("static_first_writer")
            .site_root(root.to_string_lossy().into_owned())
            .build();
        let response = ResponseOptions::default();
        response.set_status(StatusCode::CREATED);
        write_static_route(&options, Some(response), &route, "same-body".into())
            .await
            .unwrap();
        let old_inode = fs::metadata(&path).unwrap().ino();
        fs::remove_file(root.join(PUBLICATION_LOCK)).unwrap();

        let (pinned_tx, pinned_rx) = futures::channel::oneshot::channel();
        let (continue_read_tx, continue_read_rx) = std::sync::mpsc::channel();
        test_hooks::on_lockless_pin(
            path.clone(),
            Box::new(move || {
                pinned_tx
                    .send(())
                    .map_err(|_| io::Error::other("pin receiver ended"))?;
                continue_read_rx
                    .recv_timeout(Duration::from_secs(5))
                    .map_err(io::Error::other)
            }),
        );
        let (validated_tx, validated_rx) = futures::channel::oneshot::channel();
        test_hooks::on_lockless_validation(
            path.clone(),
            Box::new(move || {
                validated_tx
                    .send(())
                    .map_err(|_| io::Error::other("validation receiver ended"))
            }),
        );
        let (metadata_tx, metadata_rx) = futures::channel::oneshot::channel();
        let (release_tx, release_rx) = std::sync::mpsc::channel();
        let release = ReleaseWriter(Some(release_tx));
        let _publication = test_hooks::on_publication(
            path.clone(),
            Box::new(move || {
                metadata_tx
                    .send(())
                    .map_err(|_| io::Error::other("metadata receiver ended"))?;
                release_rx
                    .recv_timeout(Duration::from_secs(5))
                    .map_err(io::Error::other)
            }),
        );
        let reader = ntex::rt::spawn_blocking({
            let root = root.clone();
            let path = path.clone();
            move || read_paired_static_file(&root, &path)
        });
        let _hooks = test_hooks::LocklessHookRegistration(path.clone());
        let reader = match futures::future::select(Box::pin(pinned_rx), Box::pin(reader)).await {
            futures::future::Either::Left((Ok(()), reader)) => reader,
            futures::future::Either::Left((Err(error), _)) => {
                return Err(format!("reader pin notification ended: {error}"));
            }
            futures::future::Either::Right((result, _)) => {
                return Err(match result {
                    Ok(Err(error)) => {
                        format!("initial read completed before first writer: {error}")
                    }
                    Ok(Ok(_)) => "initial read completed before first-writer gate".to_owned(),
                    Err(error) => format!("initial read task failed: {error}"),
                });
            }
        };
        let writer_root = root.clone();
        let writer = ntex::rt::spawn(async move {
            let _root = writer_root;
            let response = ResponseOptions::default();
            response.set_status(StatusCode::ACCEPTED);
            write_static_route(&options, Some(response), &route, new_body.into()).await
        });
        let writer = match futures::future::select(Box::pin(metadata_rx), Box::pin(writer)).await {
            futures::future::Either::Left((Ok(()), writer)) => writer,
            futures::future::Either::Left((Err(error), _)) => {
                return Err(format!("metadata notification ended: {error}"));
            }
            futures::future::Either::Right((result, _)) => {
                return Err(match result {
                    Ok(Err(error)) => format!("writer ended before metadata notification: {error}"),
                    Ok(Ok(())) => "writer ended before metadata gate".to_owned(),
                    Err(error) => {
                        format!("writer task ended before metadata notification: {error}")
                    }
                });
            }
        };
        let site = crate::fs_boundary::SiteRoot::open(&root).unwrap();
        let (dir, _) = site.parent(&path, false).unwrap();
        let lock = crate::fs_boundary::open_regular(&dir, Path::new(PUBLICATION_LOCK)).unwrap();
        let writer_holds_exclusive_lock = matches!(
            fs4::FileExt::try_lock_shared(&lock),
            Err(fs4::TryLockError::WouldBlock)
        );
        drop(lock);
        continue_read_tx.send(()).unwrap();
        let reader = match futures::future::select(Box::pin(validated_rx), reader).await {
            futures::future::Either::Left((Ok(()), reader)) => reader,
            futures::future::Either::Left((Err(error), _)) => {
                return Err(format!("validation notification ended: {error}"));
            }
            futures::future::Either::Right((result, _)) => {
                return Err(match result {
                    Ok(Err(error)) => {
                        format!("reader ended before validation notification: {error}")
                    }
                    Ok(Ok(_)) => "reader ended before validation gate".to_owned(),
                    Err(error) => {
                        format!("reader task ended before validation notification: {error}")
                    }
                });
            }
        };
        drop(release);
        writer.await.unwrap().unwrap();
        let (mut file, parts) = reader
            .await
            .map_err(|error| error.to_string())?
            .map_err(|error| error.to_string())?;
        let returned_inode = file.file().metadata().unwrap().ino();
        let published_inode = fs::metadata(&path).unwrap().ino();
        let mut body = String::new();
        file.read_to_string(&mut body).unwrap();
        Ok(Observation {
            old_inode,
            returned_inode,
            published_inode,
            status: parts.and_then(|parts| parts.status),
            body,
            writer_holds_exclusive_lock,
            expected_body: new_body.to_owned(),
        })
    }

    fn first_writer_result(changed_body: bool) -> Result<Observation, String> {
        crate::tests::run_ntex(async move {
            ntex::time::timeout(
                ntex::time::Millis(10_000),
                first_writer_during_read(changed_body),
            )
            .await
            .unwrap_or_else(|_| {
                Err("TECHNICAL DEADLINE: first-writer fixture did not finish".to_owned())
            })
        })
    }

    fn be_one_published_generation(result: &Result<Observation, String>) -> AssertionResult {
        let actual = result.as_ref().map_err(|error| {
            AssertionError::new(vec![format!(
                "expected a complete snapshot after first writer, got {error}"
            )])
        })?;
        if actual.returned_inode == actual.published_inode
            && actual.returned_inode != actual.old_inode
            && actual.status == Some(StatusCode::ACCEPTED)
            && actual.body == actual.expected_body
            && actual.writer_holds_exclusive_lock
        {
            Ok(())
        } else {
            Err(AssertionError::new(vec![format!(
                "expected final inode/status202/body under observed writer EX; actual {actual:?}"
            )]))
        }
    }

    lets_expect! {
        expect(first_writer_result(changed_body)) as the_first_writer_during_a_lockless_read {
            let changed_body = false;
            to returns_one_complete_published_generation { be_one_published_generation }
            when the_writer_changes_the_html_bytes {
                let changed_body = true;
                to retries_the_failed_snapshot_under_the_new_lock { be_one_published_generation }
            }
        }
    }
}

#[cfg(test)]
mod verified_snapshot_specs {
    use super::*;
    use crate::tests::{run_ntex, temp_site_root};
    use lets_expect::*;

    #[derive(Clone, Copy)]
    enum Change {
        None,
        RewriteInPlace,
        Republish,
    }
    type Observation = (u64, u64, Result<Option<StatusCode>, io::ErrorKind>);
    async fn repeated_reads(change: Change) -> Observation {
        let root = temp_site_root("verified_snapshot");
        let options = LeptosOptions::builder()
            .output_name("verified_snapshot")
            .site_root(root.to_string_lossy().into_owned())
            .build();
        let response = ResponseOptions::default();
        response.set_status(StatusCode::CREATED);
        write_static_route(&options, Some(response), "/page", "same-length-body".into())
            .await
            .unwrap();
        let path = root.join("page.html");
        let canonical = path.canonicalize().unwrap();
        read_paired_static_file(&root, &path).unwrap();
        let first = verified::digests(&canonical);
        match change {
            Change::None => {}
            Change::RewriteInPlace => fs::write(&path, "SAME-LENGTH-BODY").unwrap(),
            Change::Republish => {
                let response = ResponseOptions::default();
                response.set_status(StatusCode::ACCEPTED);
                write_static_route(&options, Some(response), "/page", "another-body".into())
                    .await
                    .unwrap();
            }
        }
        let second = read_paired_static_file(&root, &path)
            .map(|(_, parts)| parts.and_then(|parts| parts.status))
            .map_err(|error| error.kind());
        (first, verified::digests(&canonical) - first, second)
    }
    lets_expect! {
        expect(run_ntex(repeated_reads(change))) as the_repeated_static_read {
            let change = Change::None;
            to reuses_the_verified_snapshot_without_hashing_again { equal((1, 0, Ok(Some(StatusCode::CREATED)))) }
            when the_html_bytes_change_in_place {
                let change = Change::RewriteInPlace;
                to verifies_again_and_rejects_the_torn_pair { equal((1, 1, Err(io::ErrorKind::InvalidData))) }
            }
            when the_pair_is_republished {
                let change = Change::Republish;
                to verifies_the_new_pair_once { equal((1, 1, Ok(Some(StatusCode::ACCEPTED)))) }
            }
        }
    }
}

#[cfg(test)]
mod digest_specs {
    use super::*;
    use crate::tests::{run_ntex, temp_site_root};
    use lets_expect::*;
    use std::io::Read;

    struct ReleaseDigest(Option<std::sync::mpsc::Sender<()>>);
    impl Drop for ReleaseDigest {
        fn drop(&mut self) {
            if let Some(sender) = self.0.take() {
                let _ = sender.send(());
            }
        }
    }
    type Snapshot = (String, Option<StatusCode>, Option<String>);

    async fn read_during_digest(another_route: bool) -> Option<Snapshot> {
        let root = temp_site_root("static_digest_reader");
        let options = LeptosOptions::builder()
            .output_name("static_digest_reader")
            .site_root(root.to_string_lossy().into_owned())
            .build();
        let old = ResponseOptions::default();
        old.set_status(StatusCode::CREATED);
        old.insert_header(
            header::HeaderName::from_static("x-generation"),
            header::HeaderValue::from_static("old"),
        );
        write_static_route(&options, Some(old.clone()), "/a", "old-body".into())
            .await
            .unwrap();
        let reader_route = if another_route {
            let writer_bucket = lock_name(Path::new("a.html"));
            let name = (0..1024)
                .map(|index| PathBuf::from(format!("page-{index}.html")))
                .find(|name| lock_name(name) == writer_bucket)
                .expect("fixture must find a different route sharing the scratch bucket");
            let route = format!("/{}", name.file_stem().unwrap().to_string_lossy());
            write_static_route(&options, Some(old), &route, "old-body".into())
                .await
                .unwrap();
            route
        } else {
            "/a".to_owned()
        };
        let reader_path = static_path(&options, &reader_route).unwrap();
        let (entered_tx, entered_rx) = futures::channel::oneshot::channel();
        let (release_tx, release_rx) = std::sync::mpsc::channel();
        let release = ReleaseDigest(Some(release_tx));
        let _registration = test_hooks::on_digest(
            root.join("a.html"),
            Box::new(move || {
                entered_tx
                    .send(())
                    .map_err(|_| io::Error::other("digest observer ended"))?;
                release_rx.recv().map_err(io::Error::other)
            }),
        );
        let writer_root = root.clone();
        let writer = ntex::rt::spawn(async move {
            let _root = writer_root;
            let response = ResponseOptions::default();
            response.set_status(StatusCode::ACCEPTED);
            write_static_route(&options, Some(response), "/a", "new-body".into()).await
        });
        ntex::time::timeout(ntex::time::Millis(5000), entered_rx)
            .await
            .expect("writer must reach the digest step")
            .expect("writer must notify the digest observer");
        let reader_root = root.clone();
        let observed = ntex::rt::spawn_blocking(move || -> io::Result<Option<Snapshot>> {
            let site = crate::fs_boundary::SiteRoot::open(&reader_root)?;
            let (dir, _) = site.parent(&reader_path, false)?;
            let shared = crate::fs_boundary::open_regular(&dir, Path::new(PUBLICATION_LOCK))?;
            match fs4::FileExt::try_lock_shared(&shared) {
                Ok(()) => {}
                Err(fs4::TryLockError::WouldBlock) => return Ok(None),
                Err(error) => return Err(io::Error::other(error)),
            }
            // The writer stays at the channel barrier until this real read
            // finishes; checking availability alone would not prove the pair.
            let (mut file, parts) = read_paired_static_file(&reader_root, &reader_path)?;
            let mut body = String::new();
            file.read_to_string(&mut body)?;
            let parts = parts.ok_or_else(|| io::Error::other("expected generated metadata"))?;
            let generation = parts
                .headers
                .get("x-generation")
                .and_then(|value| value.to_str().ok())
                .map(str::to_owned);
            Ok(Some((body, parts.status, generation)))
        })
        .await
        .expect("reader blocking task must complete")
        .expect("reader I/O must succeed");
        drop(release);
        writer
            .await
            .expect("writer task must complete")
            .expect("publication must succeed");
        observed
    }

    fn read_previous_static_representation(observed: &Option<Snapshot>) -> AssertionResult {
        let expected = Some((
            "old-body".to_owned(),
            Some(StatusCode::CREATED),
            Some("old".to_owned()),
        ));
        if *observed == expected {
            Ok(())
        } else {
            Err(AssertionError::new(vec![format!(
                "reader must finish with the complete previous representation during digest computation; expected {expected:?}, got {observed:?} (None means its shared lock was blocked)"
            )]))
        }
    }

    lets_expect! {
        expect(run_ntex(read_during_digest(another_route))) as reader_during_static_digest {
            let another_route = false;
            to reads_the_complete_previous_representation { read_previous_static_representation }
            when another_route_shares_the_lock_bucket {
                let another_route = true;
                to reads_the_complete_previous_representation { read_previous_static_representation }
            }
        }
    }
}

#[cfg(all(test, unix))]
mod hook_cleanup_specs {
    use super::*;
    use crate::tests::{run_ntex, temp_site_root};
    use lets_expect::*;
    use std::os::unix::fs::PermissionsExt;
    #[derive(Debug, PartialEq)]
    struct Retention {
        operation_failed: bool,
        receiver_pending: bool,
        root_retained: bool,
        cleared_root_removed: bool,
    }
    struct RestorePermissions(PathBuf);
    impl Drop for RestorePermissions {
        fn drop(&mut self) {
            let _ = fs::set_permissions(&self.0, fs::Permissions::from_mode(0o755));
        }
    }
    async fn failed_before_hook(writer: bool) -> Retention {
        let root = temp_site_root("hook_failure_retention");
        let path = root.join("page.html");
        let root_path = root.to_path_buf();
        let (sender, mut receiver) = futures::channel::oneshot::channel::<()>();
        let retained = root.clone();
        let hook: Box<dyn FnOnce() -> io::Result<()> + Send> = Box::new(move || {
            drop(sender);
            drop(retained);
            Ok(())
        });
        let registration = if writer {
            test_hooks::on_publication(path.clone(), hook)
        } else {
            test_hooks::on_legacy_read(path.clone(), hook)
        };
        let operation_failed = if writer {
            let restore = RestorePermissions(root_path.clone());
            fs::set_permissions(&root_path, fs::Permissions::from_mode(0o555)).unwrap();
            let options = LeptosOptions::builder()
                .output_name("hooks")
                .site_root(root_path.to_string_lossy().into_owned())
                .build();
            let result = write_static_route(&options, None, "/page", "public body".into()).await;
            drop(restore);
            result.is_err_and(|error| error.kind() == io::ErrorKind::PermissionDenied)
        } else {
            read_paired_static_file(&root_path, &path)
                .is_err_and(|error| error.kind() == io::ErrorKind::NotFound)
        };
        drop(registration);
        let receiver_pending = matches!(receiver.try_recv(), Ok(None));
        drop(root);
        let root_retained = root_path.exists();
        if writer {
            test_hooks::after_metadata(&path).unwrap();
        } else {
            test_hooks::after_legacy_classification(&path).unwrap();
        }
        Retention {
            operation_failed,
            receiver_pending,
            root_retained,
            cleared_root_removed: !root_path.exists(),
        }
    }
    lets_expect! {
        expect(run_ntex(failed_before_hook(writer))) as the_unreached_hook_resources {
            let writer = true;
            to releases_unused_resources_after_the_writer_has_failed { equal(Retention { operation_failed: true, receiver_pending: false, root_retained: false, cleared_root_removed: true }) }
            when the_reader_fails_before_legacy_classification { let writer = false; to releases_unused_resources_after_the_reader_has_failed { equal(Retention { operation_failed: true, receiver_pending: false, root_retained: false, cleared_root_removed: true }) } }
        }
    }
}

#[cfg(test)]
mod metadata_directory_specs {
    use super::*;
    use crate::tests::{run_ntex, temp_site_root};
    use lets_expect::*;
    use std::io::Read;

    #[derive(Clone, Copy)]
    enum Alias {
        Exact,
        Encoded,
        DifferentName,
    }
    async fn publication_identity(
        alias: Alias,
    ) -> Result<(String, Option<StatusCode>, usize), String> {
        let root = temp_site_root("long_artifact_identity");
        let options = LeptosOptions::builder()
            .output_name("long_identity")
            .site_root(root.to_string_lossy().into_owned())
            .build();
        let segment = format!("P{}", "a".repeat(249));
        let first = format!("/{segment}");
        let response = ResponseOptions::default();
        response.set_status(StatusCode::CREATED);
        write_static_route(&options, Some(response), &first, "first-body".into())
            .await
            .map_err(|error| error.to_string())?;
        let read_path = match alias {
            Alias::Exact => root.join(format!("{segment}.html")),
            Alias::Encoded => static_path(&options, &format!("/%50{}", "a".repeat(249))).unwrap(),
            Alias::DifferentName => {
                let second = format!("/Q{}", "a".repeat(249));
                let response = ResponseOptions::default();
                response.set_status(StatusCode::ACCEPTED);
                write_static_route(&options, Some(response), &second, "second-body".into())
                    .await
                    .map_err(|error| error.to_string())?;
                static_path(&options, &second).unwrap()
            }
        };
        let (mut file, parts) =
            read_paired_static_file(&root, &read_path).map_err(|error| error.to_string())?;
        let mut body = String::new();
        file.read_to_string(&mut body).unwrap();
        let metadata_count = fs::read_dir(root.join(METADATA_DIRECTORY))
            .map_err(|error| error.to_string())?
            .count();
        Ok((body, parts.and_then(|p| p.status), metadata_count))
    }
    lets_expect! {
        expect(run_ntex(publication_identity(alias))) as the_long_artifact_identity {
            let alias = Alias::Exact;
            to reads_the_published_snapshot { equal(Ok(("first-body".into(), Some(StatusCode::CREATED), 1))) }
            when the_url_uses_percent_encoding { let alias = Alias::Encoded; to reads_the_same_snapshot { equal(Ok(("first-body".into(), Some(StatusCode::CREATED), 1))) } }
            when another_long_filename_is_published { let alias = Alias::DifferentName; to keeps_the_snapshots_distinct { equal(Ok(("second-body".into(), Some(StatusCode::ACCEPTED), 2))) } }
        }
    }
}

#[cfg(test)]
mod unused_hook_scope_specs {
    use super::*;
    use crate::tests::temp_site_root;
    use lets_expect::*;
    #[derive(Clone, Copy)]
    enum Stage {
        Publication,
        Legacy,
        Reopen,
    }
    fn released_on_exit(stage: Stage, unwind: bool) -> (bool, bool) {
        let root = temp_site_root("unused_hook_scope");
        let path = root.to_path_buf();
        let callback_root = root.clone();
        let hook: Box<dyn FnOnce() -> io::Result<()> + Send> = Box::new(move || {
            drop(callback_root);
            Ok(())
        });
        let outcome = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let _registration = match stage {
                Stage::Publication => test_hooks::on_publication(path.clone(), hook),
                Stage::Legacy => test_hooks::on_legacy_read(path.clone(), hook),
                Stage::Reopen => test_hooks::on_reopen(path.clone(), hook),
            };
            if unwind {
                panic!("controlled fixture unwind before callback");
            }
        }));
        drop(root);
        let released = !path.exists();
        // Ensure a failed mutation cannot leave resources in the process registry.
        match stage {
            Stage::Publication => test_hooks::after_metadata(&path).unwrap(),
            Stage::Legacy => test_hooks::after_legacy_classification(&path).unwrap(),
            Stage::Reopen => test_hooks::before_reopen(&path).unwrap(),
        }
        (outcome.is_err(), released)
    }
    lets_expect! {
        expect(released_on_exit(stage, unwind)) as unused_static_hook_scope {
            let stage = Stage::Publication;
            let unwind = false;
            to releases_callback_captures { equal((false, true)) }
            when the_scope_unwinds { let unwind = true; to releases_callback_captures { equal((true, true)) } }
            when the_hook_precedes_a_legacy_read {
                let stage = Stage::Legacy;
                to releases_callback_captures { equal((false, true)) }
                when the_scope_unwinds { let unwind = true; to releases_callback_captures { equal((true, true)) } }
            }
            when the_hook_precedes_regenerated_file_reopen {
                let stage = Stage::Reopen;
                to releases_callback_captures { equal((false, true)) }
                when the_scope_unwinds { let unwind = true; to releases_callback_captures { equal((true, true)) } }
            }
        }
    }
}

#[cfg(all(test, unix))]
mod publication_namespace_specs {
    use super::*;
    use crate::tests::{run_ntex, temp_site_root};
    use lets_expect::*;
    use std::io::Read;
    use std::os::unix::fs::{MetadataExt, symlink};

    #[derive(Clone, Copy)]
    enum Names {
        ShortCase,
        LongCase,
        #[cfg(target_os = "macos")]
        Unicode,
        #[cfg(target_os = "macos")]
        ShortThenLong,
        #[cfg(target_os = "macos")]
        LongThenShort,
    }
    impl Names {
        fn pair(self) -> (String, String) {
            match self {
                Self::ShortCase => ("Page".into(), "page".into()),
                Self::LongCase => (
                    format!("P{}", "a".repeat(249)),
                    format!("p{}", "a".repeat(249)),
                ),
                #[cfg(target_os = "macos")]
                Self::Unicode => ("é".repeat(125), "e\u{301}".repeat(125)),
                #[cfg(target_os = "macos")]
                Self::ShortThenLong => ("é".repeat(110), "e\u{301}".repeat(110)),
                #[cfg(target_os = "macos")]
                Self::LongThenShort => ("e\u{301}".repeat(110), "é".repeat(110)),
            }
        }
    }
    #[derive(Debug)]
    enum NativeIdentity {
        Alias,
        DistinctEntries,
    }
    fn same_entry(root: &Path, first: &str, second: &str) -> io::Result<NativeIdentity> {
        let probe = root.join("identity-probe");
        fs::create_dir(&probe)?;
        let first_path = probe.join(format!("{first}.html"));
        let second_path = probe.join(format!("{second}.html"));
        fs::write(&first_path, b"identity")?;
        let first = fs::symlink_metadata(&first_path)?;
        match fs::symlink_metadata(&second_path) {
            Ok(second) if first.dev() == second.dev() && first.ino() == second.ino() => {
                Ok(NativeIdentity::Alias)
            }
            Ok(_) => Err(io::Error::other("unexpected existing control entry")),
            Err(error) if error.kind() == io::ErrorKind::NotFound => {
                fs::write(&second_path, b"second-identity")?;
                let second = fs::symlink_metadata(&second_path)?;
                if (first.dev(), first.ino()) == (second.dev(), second.ino())
                    || fs::read(&first_path)? != b"identity"
                    || fs::read(&second_path)? != b"second-identity"
                {
                    return Err(io::Error::other(
                        "filesystem did not preserve distinct control entries",
                    ));
                }
                Ok(NativeIdentity::DistinctEntries)
            }
            Err(error) => Err(error),
        }
    }
    fn snapshot(root: &Path, segment: &str) -> io::Result<(String, Option<StatusCode>)> {
        let (mut file, parts) =
            read_paired_static_file(root, &root.join(format!("{segment}.html")))?;
        let mut body = String::new();
        file.read_to_string(&mut body)?;
        Ok((body, parts.and_then(|parts| parts.status)))
    }
    async fn publish(options: &LeptosOptions, segment: &str, first: bool) -> io::Result<()> {
        let response = ResponseOptions::default();
        response.set_status(if first {
            StatusCode::CREATED
        } else {
            StatusCode::ACCEPTED
        });
        write_static_route(
            options,
            Some(response),
            &format!("/{segment}"),
            if first { "first-body" } else { "second-body" }.into(),
        )
        .await
    }
    #[derive(Debug)]
    struct Identity {
        identity: NativeIdentity,
        first: (String, Option<StatusCode>),
        second: (String, Option<StatusCode>),
    }
    impl Identity {
        fn check(&self) -> Result<(), String> {
            let old = ("first-body".into(), Some(StatusCode::CREATED));
            let new = ("second-body".into(), Some(StatusCode::ACCEPTED));
            let expected_first = match self.identity {
                NativeIdentity::Alias => &new,
                NativeIdentity::DistinctEntries => &old,
            };
            if &self.first == expected_first && self.second == new {
                Ok(())
            } else {
                Err(format!(
                    "filesystem identity={:?}, expected first={expected_first:?}, second={new:?}; observed {self:?}",
                    self.identity
                ))
            }
        }
    }
    async fn successive_publications(names: Names) -> Result<(), String> {
        let root = temp_site_root("publication_namespace");
        let (first, second) = names.pair();
        let identity = same_entry(&root, &first, &second).map_err(|error| error.to_string())?;
        let options = LeptosOptions::builder()
            .output_name("publication_namespace")
            .site_root(root.to_string_lossy().into_owned())
            .build();
        publish(&options, &first, true)
            .await
            .map_err(|error| error.to_string())?;
        publish(&options, &second, false)
            .await
            .map_err(|error| error.to_string())?;
        Identity {
            identity,
            first: snapshot(&root, &first).map_err(|error| error.to_string())?,
            second: snapshot(&root, &second).map_err(|error| error.to_string())?,
        }
        .check()
    }
    lets_expect! {
        expect(run_ntex(successive_publications(names))) as successive_static_publications {
            let names = Names::ShortCase;
            to preserves_the_filesystems_name_identity { equal(Ok(())) }
            when the_names_need_bounded_metadata {
                let names = Names::LongCase;
                to preserves_the_filesystems_name_identity { equal(Ok(())) }
            }
        }
    }
    #[cfg(target_os = "macos")]
    lets_expect! {
        expect(run_ntex(successive_publications(names))) as unicode_static_publications {
            let names = Names::Unicode;
            to preserves_the_filesystems_normalization_identity { equal(Ok(())) }
            when the_second_spelling_is_longer {
                let names = Names::ShortThenLong;
                to reads_the_latest_snapshot_through_both_spellings { equal(Ok(())) }
            }
            when the_first_spelling_is_longer {
                let names = Names::LongThenShort;
                to reads_the_latest_snapshot_through_both_spellings { equal(Ok(())) }
            }
        }
    }

    async fn replace_symlink(
        hard_link: bool,
    ) -> Result<(bool, bool, String, (String, Option<StatusCode>)), String> {
        let root = temp_site_root("static_symlink_replacement");
        let segment = format!("P{}", "a".repeat(249));
        let selected = root.join(format!("{segment}.html"));
        let target = root.join("target.html");
        fs::write(&target, "target-body").map_err(|error| error.to_string())?;
        symlink("target.html", &selected).map_err(|error| error.to_string())?;
        let sibling = root.join("other-link.html");
        if hard_link {
            fs::hard_link(&selected, &sibling).map_err(|error| error.to_string())?;
        }
        let options = LeptosOptions::builder()
            .output_name("symlink_replacement")
            .site_root(root.to_string_lossy().into_owned())
            .build();
        publish(&options, &segment, false)
            .await
            .map_err(|error| error.to_string())?;
        let selected_is_file = fs::symlink_metadata(&selected)
            .map_err(|error| error.to_string())?
            .is_file();
        let sibling_preserved = !hard_link
            || (fs::symlink_metadata(&sibling)
                .map_err(|error| error.to_string())?
                .file_type()
                .is_symlink()
                && fs::read_to_string(&sibling).map_err(|error| error.to_string())?
                    == "target-body");
        Ok((
            selected_is_file,
            sibling_preserved,
            fs::read_to_string(target).map_err(|error| error.to_string())?,
            snapshot(&root, &segment).map_err(|error| error.to_string())?,
        ))
    }
    lets_expect! {
        expect(run_ntex(replace_symlink(hard_link))) as publishing_over_a_final_symlink {
            let hard_link = false;
            to replaces_the_entry_and_preserves_the_target { equal(Ok((true, true, "target-body".into(), ("second-body".into(), Some(StatusCode::ACCEPTED))))) }
            when another_entry_links_the_same_symlink_inode {
                let hard_link = true;
                to replaces_only_the_selected_entry { equal(Ok((true, true, "target-body".into(), ("second-body".into(), Some(StatusCode::ACCEPTED))))) }
            }
        }
    }
}

#[cfg(all(test, unix))]
mod publication_process_specs {
    use super::*;
    use crate::tests::{run_ntex, temp_site_root};
    use lets_expect::*;
    use std::{
        io::{BufRead, BufReader, Read, Write},
        process::{Child, ChildStdin, Command, Stdio},
        sync::mpsc::{self, Receiver},
        time::Duration,
    };

    const CHILD_ROOT: &str = "LEPTOS_NTEX_PUBLICATION_ROOT";
    const CHILD_SEGMENT: &str = "LEPTOS_NTEX_PUBLICATION_SEGMENT";
    const CHILD_PAUSE: &str = "LEPTOS_NTEX_PUBLICATION_PAUSE";
    const EVENT: &str = "STATIC_PUBLICATION ";
    fn emit(event: &str) {
        let mut output = std::io::stdout().lock();
        writeln!(output, "{EVENT}{event}").unwrap();
        output.flush().unwrap();
    }
    fn child(root: String) -> io::Result<()> {
        let segment = std::env::var(CHILD_SEGMENT).unwrap();
        let paused = std::env::var_os(CHILD_PAUSE).is_some();
        let path = Path::new(&root).join(format!("{segment}.html"));
        let _registration = paused.then(|| {
            test_hooks::on_publication(
                path.clone(),
                Box::new(|| {
                    emit("metadata");
                    let mut line = String::new();
                    let read = std::io::stdin().read_line(&mut line)?;
                    if read == 0 {
                        return Err(io::Error::other(
                            "parent ended before releasing publication",
                        ));
                    }
                    Ok(())
                }),
            )
        });
        let probe_root = root.clone();
        let _digest = (!paused).then(|| {
            test_hooks::on_digest(
                path,
                Box::new(move || {
                    let lock = fs::File::open(Path::new(&probe_root).join(PUBLICATION_LOCK))?;
                    let blocked = matches!(
                        fs4::FileExt::try_lock(&lock),
                        Err(fs4::TryLockError::WouldBlock)
                    );
                    drop(lock);
                    emit(if blocked {
                        "blocked=true"
                    } else {
                        "blocked=false"
                    });
                    Ok(())
                }),
            )
        });
        let options = LeptosOptions::builder()
            .output_name("publication_process")
            .site_root(root)
            .build();
        emit("ready");
        run_ntex(async move {
            let response = ResponseOptions::default();
            response.set_status(if paused {
                StatusCode::CREATED
            } else {
                StatusCode::ACCEPTED
            });
            write_static_route(
                &options,
                Some(response),
                &format!("/{segment}"),
                if paused { "first-body" } else { "second-body" }.into(),
            )
            .await
        })?;
        emit("done");
        Ok(())
    }
    struct Worker {
        process: Child,
        input: ChildStdin,
        events: Receiver<String>,
        reader: Option<std::thread::JoinHandle<()>>,
    }
    impl Worker {
        fn start(root: &Path, segment: &str, paused: bool) -> Self {
            let name = std::thread::current().name().unwrap().to_owned();
            let mut command = Command::new(std::env::current_exe().unwrap());
            command
                .args(["--exact", &name, "--nocapture"])
                .env(CHILD_ROOT, root)
                .env(CHILD_SEGMENT, segment)
                .env_remove(CHILD_PAUSE)
                .stdin(Stdio::piped())
                .stdout(Stdio::piped())
                .stderr(Stdio::inherit());
            if paused {
                command.env(CHILD_PAUSE, "1");
            }
            let mut process = command.spawn().unwrap();
            let input = process.stdin.take().unwrap();
            let output = process.stdout.take().unwrap();
            let (sender, events) = mpsc::channel();
            let reader = std::thread::spawn(move || {
                for line in BufReader::new(output).lines() {
                    let Ok(line) = line else { break };
                    if let Some(event) = line.strip_prefix(EVENT)
                        && sender.send(event.to_owned()).is_err()
                    {
                        break;
                    }
                }
            });
            Self {
                process,
                input,
                events,
                reader: Some(reader),
            }
        }
        fn next_event(&self) -> String {
            // This deadline only bounds a failed experiment. The lock result,
            // not elapsed time or missing output, proves exclusion.
            self.events
                .recv_timeout(Duration::from_secs(10))
                .expect("child exited or failed to reach the requested publication boundary")
        }
        fn event(&self, expected: &str) {
            assert_eq!(self.next_event(), expected);
        }
        fn release(&mut self) {
            writeln!(self.input, "continue").unwrap();
            self.input.flush().unwrap();
        }
        fn finish(&mut self) -> bool {
            self.event("done");
            self.process.wait().unwrap().success()
        }
    }
    impl Drop for Worker {
        fn drop(&mut self) {
            let _ = self.process.kill();
            let _ = self.process.wait();
            if let Some(reader) = self.reader.take() {
                let _ = reader.join();
            }
        }
    }
    fn observation(long: bool, existing: bool, crash: bool) -> Result<(bool, bool, bool), String> {
        if let Ok(root) = std::env::var(CHILD_ROOT) {
            child(root).map_err(|error| error.to_string())?;
            return Ok((true, true, true));
        }
        let root = temp_site_root("publication_process");
        let (first, second) = if long {
            (
                format!("P{}", "a".repeat(249)),
                format!("p{}", "a".repeat(249)),
            )
        } else {
            ("Page".into(), "page".into())
        };
        // The crash outcome isolates one exact artifact. Normal cases use
        // distinct scratch buckets to exercise the historical alias race.
        let second = if crash { first.clone() } else { second };
        if !crash {
            assert_ne!(
                lock_name(Path::new(&format!("{first}.html"))),
                lock_name(Path::new(&format!("{second}.html")))
            );
        }
        if existing {
            let options = LeptosOptions::builder()
                .output_name("publication_process")
                .site_root(root.to_string_lossy().into_owned())
                .build();
            let route = format!("/{first}");
            run_ntex(
                async move { write_static_route(&options, None, &route, "old-body".into()).await },
            )
            .map_err(|error| error.to_string())?;
        }
        let mut first_worker = Worker::start(&root, &first, true);
        first_worker.event("ready");
        first_worker.event("metadata");
        let lock =
            fs::File::open(root.join(PUBLICATION_LOCK)).map_err(|error| error.to_string())?;
        let excluded = matches!(
            fs4::FileExt::try_lock_shared(&lock),
            Err(fs4::TryLockError::WouldBlock)
        );
        // If a mutation incorrectly lets this probe acquire SH, release it
        // before proceeding so the assertion fails without creating a hang.
        drop(lock);
        let mut second_worker = Worker::start(&root, &second, false);
        second_worker.event("ready");
        let other_process_excluded = second_worker.next_event() == "blocked=true";
        let completed = if crash {
            first_worker
                .process
                .kill()
                .map_err(|error| error.to_string())?;
            let killed = !first_worker
                .process
                .wait()
                .map_err(|error| error.to_string())?
                .success();
            killed && second_worker.finish()
        } else {
            first_worker.release();
            first_worker.finish() && second_worker.finish()
        };
        let read = |segment: &str| -> io::Result<(String, Option<StatusCode>)> {
            let (mut file, parts) =
                read_paired_static_file(&root, &root.join(format!("{segment}.html")))?;
            let mut body = String::new();
            file.read_to_string(&mut body)?;
            Ok((body, parts.and_then(|parts| parts.status)))
        };
        let second_pair = read(&second).map_err(|error| error.to_string())?;
        let first_pair = read(&first).map_err(|error| error.to_string())?;
        let first_metadata =
            fs::metadata(root.join(format!("{first}.html"))).map_err(|error| error.to_string())?;
        let second_metadata =
            fs::metadata(root.join(format!("{second}.html"))).map_err(|error| error.to_string())?;
        use std::os::unix::fs::MetadataExt;
        let equivalent = first_metadata.dev() == second_metadata.dev()
            && first_metadata.ino() == second_metadata.ino();
        let new = ("second-body".into(), Some(StatusCode::ACCEPTED));
        let expected_first = if equivalent {
            new.clone()
        } else {
            ("first-body".into(), Some(StatusCode::CREATED))
        };
        Ok((
            excluded && other_process_excluded,
            completed,
            first_pair == expected_first && second_pair == new,
        ))
    }
    lets_expect! {
        expect(observation(long, existing, crash)) as cooperating_static_publisher_processes {
            let long = false;
            let existing = false;
            let crash = false;
            to commits_complete_pairs_under_one_parent_lock { equal(Ok((true, true, true))) }
            when the_html_already_exists {
                let existing = true;
                to commits_complete_pairs_under_one_parent_lock { equal(Ok((true, true, true))) }
            }
            when the_first_process_exits_before_publishing_html {
                let crash = true;
                to releases_the_lock_and_allows_the_other_process_to_publish_a_complete_pair { equal(Ok((true, true, true))) }
            }
            when the_names_need_bounded_metadata {
                let long = true;
                to commits_complete_pairs_under_one_parent_lock { equal(Ok((true, true, true))) }
                when the_html_already_exists {
                    let existing = true;
                    to commits_complete_pairs_under_one_parent_lock { equal(Ok((true, true, true))) }
                }
            }
        }
    }
}

#[cfg(test)]
mod waiter_registration_contract {
    use super::*;
    use futures::future::poll_fn;
    use leptos::prelude::{Suspend, on_cleanup};
    use lets_expect::*;
    use std::sync::atomic::AtomicUsize;

    #[derive(Clone, Copy)]
    enum Arrival {
        Before,
        Overlap,
        After,
    }

    struct ResumeOnDrop(Option<std::sync::mpsc::Sender<()>>);

    impl ResumeOnDrop {
        fn release(&mut self) {
            if let Some(resume) = self.0.take() {
                let _ = resume.send(());
            }
        }
    }

    impl Drop for ResumeOnDrop {
        fn drop(&mut self) {
            self.release();
        }
    }

    struct WorkerGuard {
        cancel: Option<futures::channel::oneshot::Sender<()>>,
        resume: ResumeOnDrop,
        thread: Option<std::thread::JoinHandle<()>>,
    }

    impl WorkerGuard {
        fn spawn<F: Future<Output = bool> + 'static>(
            resume: Option<std::sync::mpsc::Sender<()>>,
            task: impl FnOnce() -> F + Send + 'static,
        ) -> (Self, futures::channel::oneshot::Receiver<bool>) {
            let (cancel, cancelled) = futures::channel::oneshot::channel();
            let (finished_tx, finished) = futures::channel::oneshot::channel();
            let worker_resume = ResumeOnDrop(resume.clone());
            let thread = std::thread::spawn(move || {
                // A worker panic must release a parent blocked in a controlled
                // subscription poll before the parent can join this thread.
                let _resume = worker_resume;
                let result = crate::tests::run_ntex(async move {
                    match futures::future::select(Box::pin(task()), cancelled).await {
                        futures::future::Either::Left((result, _)) => result,
                        futures::future::Either::Right(_) => false,
                    }
                });
                let _ = finished_tx.send(result);
            });
            (
                Self {
                    cancel: Some(cancel),
                    resume: ResumeOnDrop(resume),
                    thread: Some(thread),
                },
                finished,
            )
        }

        fn join(&mut self) {
            if let Some(thread) = self.thread.take() {
                thread
                    .join()
                    .expect("static generation worker must finish normally");
            }
        }
    }

    impl Drop for WorkerGuard {
        fn drop(&mut self) {
            // Release synchronous gates before cancellation and joining: the
            // worker cannot poll its cancellation while blocked at such a gate.
            self.resume.release();
            if let Some(cancel) = self.cancel.take() {
                let _ = cancel.send(());
            }
            if let Some(thread) = self.thread.take() {
                // During unwinding retain the original failure, but still join.
                let _ = thread.join();
            }
        }
    }

    async fn observe(arrival: Arrival) -> (bool, usize, bool) {
        ensure_executor_initialized();
        let root = crate::tests::temp_site_root("waiter_registration");
        let options = LeptosOptions::builder()
            .output_name("waiter_registration")
            .site_root(root.to_string_lossy().into_owned())
            .build();
        let runtime = Arc::new(StaticRuntime::default());
        let count = Arc::new(AtomicUsize::new(0));
        let (started_tx, started) = futures::channel::oneshot::channel();
        let (release, released) = futures::channel::oneshot::channel();
        let gates = Arc::new(Mutex::new(Some((started_tx, released))));
        let (cleaned_tx, cleaned) = futures::channel::oneshot::channel();
        let cleaned_tx = Arc::new(Mutex::new(Some(cleaned_tx)));
        let context = move || {
            if let Some(cleaned) = cleaned_tx.lock().unwrap().take() {
                on_cleanup(move || {
                    let _ = cleaned.send(());
                });
            }
        };
        let app = {
            let count = count.clone();
            move || {
                count.fetch_add(1, Ordering::SeqCst);
                let gate = gates.lock().unwrap().take();
                Suspend::new(async move {
                    if let Some((started, released)) = gate {
                        let _ = started.send(());
                        let _ = released.await;
                    }
                    "rendered successfully"
                })
            }
        };
        let mut first = Box::pin(runtime.render(
            options.clone(),
            "/waiter".into(),
            app.clone(),
            context.clone(),
            Vec::new(),
        ));
        match futures::future::select(first.as_mut(), started).await {
            futures::future::Either::Right((Ok(()), _)) => {}
            _ => panic!("first generation must wait for its release"),
        }
        let mut first = Some(first);
        let mut cleaned = Some(cleaned);
        if matches!(arrival, Arrival::After) {
            drop(first.take());
            cleaned.take().unwrap().await.unwrap();
        }
        let mut pause = None;
        let mut resume = None;
        if matches!(arrival, Arrival::Overlap) {
            let (arrived_tx, arrived) = futures::channel::oneshot::channel();
            let (resume_tx, resume_rx) = std::sync::mpsc::channel();
            *runtime.registration_pause.lock().unwrap() = Some(RegistrationPause {
                arrived: arrived_tx,
                resume: resume_rx,
            });
            pause = Some(arrived);
            resume = Some(resume_tx);
        }
        let second_runtime = runtime.clone();
        let (mut worker, finished) = WorkerGuard::spawn(resume, move || async move {
            second_runtime
                .render(options, "/waiter".into(), app, context, Vec::new())
                .await
                .is_ok()
        });
        if let Some(pause) = pause {
            pause.await.unwrap();
        }
        if matches!(arrival, Arrival::Before) {
            poll_fn(|cx| {
                let waiters = runtime
                    .work
                    .lock()
                    .unwrap()
                    .values()
                    .filter_map(std::sync::Weak::upgrade)
                    .map(|work| {
                        work.state
                            .lock()
                            .unwrap()
                            .waiters
                            .iter()
                            .filter(|waiter| !waiter.is_canceled())
                            .count()
                    })
                    .sum::<usize>();
                if waiters == 2 {
                    std::task::Poll::Ready(())
                } else {
                    cx.waker().wake_by_ref();
                    std::task::Poll::Pending
                }
            })
            .await;
            drop(first.take());
            let _ = release.send(());
        } else {
            drop(first.take());
            if let Some(cleaned) = cleaned.take() {
                cleaned.await.unwrap();
            }
            worker.resume.release();
            drop(release);
        }
        let result = finished.await.unwrap();
        worker.join();
        let body = fs::read_to_string(root.join("waiter.html")).unwrap_or_default();
        (
            result,
            count.load(Ordering::SeqCst),
            body.contains("rendered successfully"),
        )
    }
    fn bounded(arrival: Arrival) -> (bool, usize, bool) {
        crate::tests::run_ntex(async move {
            ntex::time::timeout(ntex::time::Millis(5000), observe(arrival))
                .await
                .expect("registration scenario must progress")
        })
    }
    struct EndingStream {
        entered: Option<futures::channel::oneshot::Sender<()>>,
        resume: std::sync::mpsc::Receiver<()>,
    }
    impl futures::Stream for EndingStream {
        type Item = ();
        fn poll_next(
            mut self: std::pin::Pin<&mut Self>,
            _: &mut std::task::Context<'_>,
        ) -> std::task::Poll<Option<()>> {
            if let Some(entered) = self.entered.take() {
                let _ = entered.send(());
            }
            self.resume
                .recv_timeout(std::time::Duration::from_secs(5))
                .expect("second worker must register before EOF");
            std::task::Poll::Ready(None)
        }
    }
    async fn arrive_during_eof() -> (bool, usize) {
        use leptos::prelude::*;
        use leptos_router::{
            SsrMode,
            components::{Route, Router, Routes},
            path,
            static_routes::StaticRoute,
        };
        let root = crate::tests::temp_site_root("subscription_completion");
        let options = LeptosOptions::builder()
            .output_name("subscription_completion")
            .site_root(root.to_string_lossy().into_owned())
            .build();
        let (entered_tx, entered) = futures::channel::oneshot::channel();
        let (resume, resume_rx) = std::sync::mpsc::channel();
        let controls = Arc::new(Mutex::new(Some((entered_tx, resume_rx))));
        let count = Arc::new(AtomicUsize::new(0));
        let app = {
            let count = count.clone();
            move || {
                let controls = controls.clone();
                let count = count.clone();
                view! { <Router><Routes fallback=|| "missing">
                    <Route path=path!("/eof") ssr=SsrMode::Static(StaticRoute::new().regenerate(move |_| {
                        let (entered,resume) = controls.lock().unwrap().take().unwrap();
                        EndingStream { entered:Some(entered),resume }
                    })) view=move || { count.fetch_add(1,Ordering::SeqCst); "rendered successfully" }/>
                </Routes></Router> }
            }
        };
        let routes = crate::tests::gen_route_list(app.clone());
        count.store(0, Ordering::SeqCst);
        let runtime = routes[0].runtime.clone();
        let regenerate = routes[0].regenerate.clone();
        let second_runtime = runtime.clone();
        let second_options = options.clone();
        let second_app = app.clone();
        let second_regenerate = regenerate.clone();
        let (mut worker, finished) = WorkerGuard::spawn(Some(resume.clone()), move || async move {
            entered.await.expect("subscription must reach EOF poll");
            let mut future = Box::pin(second_runtime.render(
                second_options,
                "/eof".into(),
                second_app,
                || {},
                second_regenerate,
            ));
            let mut resume = ResumeOnDrop(Some(resume));
            poll_fn(|cx| {
                let result = future.as_mut().poll(cx);
                let registered = second_runtime
                    .work
                    .lock()
                    .unwrap()
                    .values()
                    .filter_map(std::sync::Weak::upgrade)
                    .any(|work| !work.state.lock().unwrap().waiters.is_empty());
                if registered {
                    resume.release();
                }
                result
            })
            .await
            .is_ok()
        });
        assert!(
            runtime
                .render(options, "/eof".into(), app, || {}, regenerate)
                .await
                .is_ok(),
            "first render completes"
        );
        let result = finished.await.unwrap();
        worker.join();
        (result, count.load(Ordering::SeqCst))
    }

    #[derive(Clone)]
    struct PanickingCapture(Arc<CaptureDrop>);

    impl PanickingCapture {
        fn touch(&self) {
            let _ = &self.0;
        }
    }

    struct CaptureDrop {
        entered: Mutex<Option<futures::channel::oneshot::Sender<()>>>,
        resume: Mutex<std::sync::mpsc::Receiver<()>>,
    }

    impl Drop for CaptureDrop {
        fn drop(&mut self) {
            if let Some(entered) = self.entered.get_mut().unwrap().take() {
                let _ = entered.send(());
            }
            self.resume
                .get_mut()
                .unwrap()
                .recv_timeout(std::time::Duration::from_secs(5))
                .expect("parent must release the unused capture");
            panic!("one user capture destructor panic");
        }
    }

    async fn cancel_during_capture_drop() -> (bool, bool) {
        use futures::FutureExt;
        ensure_executor_initialized();
        let root = crate::tests::temp_site_root("unused_static_template");
        let options = LeptosOptions::builder()
            .output_name("unused_static_template")
            .site_root(root.to_string_lossy().into_owned())
            .build();
        let runtime = Arc::new(StaticRuntime::default());
        let (started_tx, started) = futures::channel::oneshot::channel();
        let (release, released) = futures::channel::oneshot::channel();
        let gate = Arc::new(Mutex::new(Some((started_tx, released))));
        let app = move || {
            let (started, released) = gate.lock().unwrap().take().unwrap();
            Suspend::new(async move {
                let _ = started.send(());
                let _ = released.await;
                "rendered successfully"
            })
        };
        let (cleaned_tx, mut cleaned) = futures::channel::oneshot::channel();
        let cleanup = Arc::new(Mutex::new(Some(cleaned_tx)));
        let context = move || {
            if let Some(cleaned) = cleanup.lock().unwrap().take() {
                on_cleanup(move || {
                    let _ = cleaned.send(());
                });
            }
        };
        let mut first =
            Box::pin(runtime.render(options.clone(), "/capture".into(), app, context, Vec::new()));
        match futures::future::select(first.as_mut(), started).await {
            futures::future::Either::Right((Ok(()), _)) => {}
            _ => panic!("first generation must remain suspended"),
        }
        let (entered_tx, entered) = futures::channel::oneshot::channel();
        let (resume, resume_rx) = std::sync::mpsc::channel();
        let capture = PanickingCapture(Arc::new(CaptureDrop {
            entered: Mutex::new(Some(entered_tx)),
            resume: Mutex::new(resume_rx),
        }));
        let second_runtime = runtime.clone();
        let (mut worker, finished) = WorkerGuard::spawn(Some(resume), move || async move {
            let app = move || {
                capture.touch();
                "unused template"
            };
            std::panic::AssertUnwindSafe(second_runtime.render(
                options,
                "/capture".into(),
                app,
                || {},
                Vec::new(),
            ))
            .catch_unwind()
            .await
            .is_err()
        });
        entered
            .await
            .expect("joined unused template reaches capture Drop");
        let (observed_tx, observed) = futures::channel::oneshot::channel();
        let work = runtime
            .work
            .lock()
            .unwrap()
            .values()
            .find_map(std::sync::Weak::upgrade)
            .expect("initial generation remains active");
        *work.abandonment_observer.lock().unwrap() = Some(observed_tx);
        drop(first);
        observed
            .await
            .expect("abandonment sees the joined waiter before its capture unwinds");
        worker.resume.release();
        let caught_panic = finished
            .await
            .expect("worker catches the single user panic");
        worker.join();
        let cleanup = ntex::time::timeout(ntex::time::Millis(250), &mut cleaned).await;
        let needs_release = cleanup.is_err();
        let cleaned_before_release = matches!(cleanup, Ok(Ok(())));
        // Even the broken candidate must release the pending render for teardown.
        let _ = release.send(());
        if needs_release {
            ntex::time::timeout(ntex::time::Millis(1000), cleaned)
                .await
                .expect("released fixture generation cleans up")
                .expect("cleanup sends its signal");
        }
        (caught_panic, cleaned_before_release)
    }

    fn bounded_capture_drop() -> (bool, bool) {
        crate::tests::run_ntex(async {
            ntex::time::timeout(ntex::time::Millis(5000), cancel_during_capture_drop())
                .await
                .expect("capture cancellation scenario must progress")
        })
    }

    fn bounded_eof() -> (bool, usize) {
        crate::tests::run_ntex(async {
            ntex::time::timeout(ntex::time::Millis(5000), arrive_during_eof())
                .await
                .expect("subscription completion must progress")
        })
    }

    lets_expect! {
        expect(bounded_capture_drop()) as unused_joined_template {
            when its_capture_panics_during_drop {
                to cancels_the_last_waiter_and_cleans_the_generation { equal((true, true)) }
            }
        }
        expect(bounded_eof()) as static_subscription_completion {
            when a_request_arrives_during_eof {
                to completes_the_registered_generation { equal((true, 2usize)) }
            }
        }
        expect(bounded(arrival)) as registration_of_static_waiter {
            let arrival = Arrival::Before;
            to serves_the_remaining_waiter { equal((true, 1usize, true)) }
            when arrival_overlaps_abandonment {
                let arrival = Arrival::Overlap;
                to starts_a_fresh_generation { equal((true, 2usize, true)) }
            }
            when abandoned_task_has_finished {
                let arrival = Arrival::After;
                to starts_a_fresh_generation { equal((true, 2usize, true)) }
            }
        }
    }
}

#[cfg(all(test, unix))]
#[path = "static_routes/work_specs.rs"]
mod work_specs;

#[cfg(all(test, unix))]
mod entry_identity_specs;
#[cfg(all(test, unix))]
mod work_key_specs;
