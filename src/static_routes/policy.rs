//! Opt-in storage and process-local static work admission.

use super::{StaticRuntime, storage};
use or_poisoned::OrPoisoned;
use std::{
    fmt,
    future::Future,
    path::Path,
    pin::Pin,
    sync::{Arc, Mutex, Weak},
    task::{Context, Poll},
};

pub use storage::{StaticStorageError, StaticStorageLimits};

/// Optional process-local limits. Clones of one [`StaticRoutePolicy`] share
/// these counters; independently opened policies have independent counters.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct StaticWorkLimits {
    renders: Option<u64>,
    subscriptions: Option<u64>,
    waiters: Option<u64>,
}

impl StaticWorkLimits {
    /// Creates limits with no numerical defaults.
    pub const fn new() -> Self {
        Self {
            renders: None,
            subscriptions: None,
            waiters: None,
        }
    }
    /// Bounds simultaneous static renders, including their disk publication.
    pub const fn with_renders(mut self, maximum: u64) -> Self {
        self.renders = Some(maximum);
        self
    }
    /// Bounds live regeneration streams, rather than just route subscriptions.
    pub const fn with_subscriptions(mut self, maximum: u64) -> Self {
        self.subscriptions = Some(maximum);
        self
    }
    /// Bounds callers awaiting a static render. Cached reads need no permit.
    pub const fn with_waiters(mut self, maximum: u64) -> Self {
        self.waiters = Some(maximum);
        self
    }
    /// Returns the active render limit.
    pub const fn renders(self) -> Option<u64> {
        self.renders
    }
    /// Returns the regeneration stream limit.
    pub const fn subscriptions(self) -> Option<u64> {
        self.subscriptions
    }
    /// Returns the pending caller limit.
    pub const fn waiters(self) -> Option<u64> {
        self.waiters
    }
}

/// A static policy could not be installed or a static operation was refused.
#[derive(Clone, Debug)]
#[non_exhaustive]
pub enum StaticPolicyError {
    /// Storage coordination, policy validation, or capacity failed.
    Storage(StaticStorageError),
    /// A process-local resource has no remaining capacity.
    WorkCapacity {
        /// Resource whose permits are exhausted.
        resource: &'static str,
        /// Configured maximum for this resource.
        limit: u64,
    },
    /// A runtime has already registered handlers or started generation.
    AlreadyStarted,
    /// A runtime is already bound to a different handle.
    ConflictingPolicy,
    /// Rendering or runtime execution failed.
    Render(String),
}

impl StaticPolicyError {
    /// Whether the refusal is a capacity condition that clears on its own
    /// (HTTP 503), as opposed to a configuration or runtime fault (HTTP 500)
    /// that no retry can resolve.
    pub(super) fn is_admission(&self) -> bool {
        match self {
            Self::Storage(error) => error.is_capacity(),
            Self::WorkCapacity { .. } => true,
            Self::AlreadyStarted | Self::ConflictingPolicy | Self::Render(_) => false,
        }
    }
}
impl fmt::Display for StaticPolicyError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Storage(error) => error.fmt(f),
            Self::WorkCapacity { resource, limit } => {
                write!(f, "static {resource} capacity exhausted (limit {limit})")
            }
            Self::AlreadyStarted => f.write_str("static runtime has already started"),
            Self::ConflictingPolicy => f.write_str("static runtime already has a different policy"),
            Self::Render(error) => f.write_str(error),
        }
    }
}
impl std::error::Error for StaticPolicyError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Storage(error) => Some(error),
            _ => None,
        }
    }
}
impl From<String> for StaticPolicyError {
    fn from(value: String) -> Self {
        Self::Render(value)
    }
}
impl From<&str> for StaticPolicyError {
    fn from(value: &str) -> Self {
        Self::Render(value.to_owned())
    }
}
impl From<StaticStorageError> for StaticPolicyError {
    fn from(value: StaticStorageError) -> Self {
        Self::Storage(value)
    }
}

#[derive(Debug)]
struct PolicyInner {
    storage: storage::Storage,
    renders: Arc<PermitPool>,
    subscriptions: Arc<PermitPool>,
    waiters: Arc<PermitPool>,
}

/// Shared opt-in limits for one static site root.
///
/// Currently, installing an opt-in storage policy requires Unix.
///
/// Open this once per process and clone it for every application serving the
/// root. Storage limits are persisted and must match in every cooperating
/// process. Work counters are shared only by clones of this handle. Independent
/// and overlapping managed roots are unsupported. Install a policy only while
/// no publisher uses an overlapping, different root. Publishers using exactly
/// the same root are coordinated during installation. Existing artifacts are counted and never evicted.
///
/// Bind listings or their generator before the root starts generating: a
/// binding is scoped to its canonical site root, so one runtime can serve
/// several managed roots. Read hits do not take permits or the root
/// publication lock. Publication uses logical named file bytes, not physical
/// disk usage or application/SSR memory; noncooperating writers are outside the
/// limit contract.
#[derive(Clone, Debug)]
pub struct StaticRoutePolicy(Arc<PolicyInner>);

impl StaticRoutePolicy {
    /// Opens a writable site root and validates its immutable storage policy.
    /// Missing directories and the stable control file may be created even if
    /// installation subsequently fails. Existing data is not removed. Inside a
    /// running ntex System the blocking work runs on its pool; elsewhere it
    /// runs inline, so the future may be awaited on any executor.
    pub async fn open(
        root: impl AsRef<Path>,
        storage: StaticStorageLimits,
        work: StaticWorkLimits,
    ) -> Result<Self, StaticPolicyError> {
        let root = root.as_ref().to_path_buf();
        // Blocking filesystem work leaves the reactor only when there is one;
        // outside an ntex System (a build step, another executor) it runs inline.
        let storage = if ntex_rt::System::try_current().is_some() {
            ntex::rt::spawn_blocking(move || storage::Storage::open(&root, storage))
                .await
                .map_err(|error| StaticPolicyError::Render(error.to_string()))??
        } else {
            storage::Storage::open(&root, storage)?
        };
        Ok(Self(Arc::new(PolicyInner {
            storage,
            renders: PermitPool::new("renders", work.renders),
            subscriptions: PermitPool::new("subscriptions", work.subscriptions),
            waiters: PermitPool::new("waiters", work.waiters),
        })))
    }

    /// Binds this policy for its site root on every runtime behind `routes`.
    /// Repeating the same handle succeeds; binding after that root has started
    /// generating, or while another handle governs it, fails without changing
    /// any runtime.
    pub fn configure_routes(
        &self,
        routes: &[crate::routes::NtexRouteListing],
    ) -> Result<(), StaticPolicyError> {
        let mut runtimes = routes
            .iter()
            .map(|route| &route.runtime)
            .collect::<Vec<_>>();
        runtimes.sort_unstable_by_key(|runtime| Arc::as_ptr(runtime));
        runtimes.dedup_by_key(|runtime| Arc::as_ptr(runtime));
        let mut guards = runtimes
            .iter()
            .map(|runtime| runtime.policy.lock().or_poisoned())
            .collect::<Vec<_>>();
        for guard in &guards {
            guard.check(self)?;
        }
        for guard in &mut guards {
            guard.bind(self);
        }
        Ok(())
    }

    pub(super) fn bind(&self, runtime: &StaticRuntime) -> Result<(), StaticPolicyError> {
        let mut state = runtime.policy.lock().or_poisoned();
        state.check(self)?;
        state.bind(self);
        Ok(())
    }
    /// The canonical site root this policy governs.
    pub(super) fn root(&self) -> &Path {
        self.0.storage.canonical()
    }
    pub(super) fn storage(&self) -> &storage::Storage {
        &self.0.storage
    }
    pub(super) fn render(&self) -> Result<Permit, StaticPolicyError> {
        self.0.renders.acquire(1)
    }
    pub(super) fn wait_for_render(&self) -> PermitFuture {
        PermitFuture::new(self.0.renders.clone())
    }
    pub(super) fn subscribe(&self, amount: usize) -> Result<Permit, StaticPolicyError> {
        self.0
            .subscriptions
            .acquire(u64::try_from(amount).unwrap_or(u64::MAX))
    }
    pub(super) fn waiter(&self) -> Result<Permit, StaticPolicyError> {
        self.0.waiters.acquire(1)
    }
    pub(super) fn render_capacity_error(&self) -> StaticPolicyError {
        self.0.renders.error()
    }
}

/// Policy bindings of one runtime, keyed by the canonical site root they
/// govern. Roots are the natural identity of static work: two applications or
/// tests that share a runtime but publish into different roots never observe
/// each other's bindings or start state.
#[derive(Debug, Default)]
pub(super) struct PolicyState {
    bindings: std::collections::HashMap<std::path::PathBuf, Binding>,
}
#[derive(Debug, Default)]
struct Binding {
    policy: Option<StaticRoutePolicy>,
    started: bool,
}
impl PolicyState {
    fn check(&self, policy: &StaticRoutePolicy) -> Result<(), StaticPolicyError> {
        let Some(binding) = self.bindings.get(policy.root()) else {
            return Ok(());
        };
        if binding.started {
            return Err(StaticPolicyError::AlreadyStarted);
        }
        if binding
            .policy
            .as_ref()
            .is_some_and(|current| !Arc::ptr_eq(&current.0, &policy.0))
        {
            return Err(StaticPolicyError::ConflictingPolicy);
        }
        Ok(())
    }
    fn bind(&mut self, policy: &StaticRoutePolicy) {
        self.bindings
            .entry(policy.root().to_path_buf())
            .or_default()
            .policy = Some(policy.clone());
    }
    /// Marks generation into `site_root` as started and returns its policy.
    /// A root that cannot be resolved has no installed policy: opening a policy
    /// creates its root.
    pub(super) fn start(&mut self, site_root: &Path) -> Option<StaticRoutePolicy> {
        let canonical = site_root.canonicalize().ok()?;
        let binding = self.bindings.entry(canonical).or_default();
        binding.started = true;
        binding.policy.clone()
    }
    #[cfg(test)]
    pub(super) fn bound(&self, site_root: &Path) -> Option<StaticRoutePolicy> {
        let canonical = site_root.canonicalize().ok()?;
        self.bindings.get(&canonical)?.policy.clone()
    }
}

#[derive(Debug, Default)]
struct PoolState {
    used: u64,
    pending: Vec<Weak<futures::task::AtomicWaker>>,
}
#[derive(Debug)]
pub(super) struct PermitPool {
    resource: &'static str,
    limit: Option<u64>,
    state: Mutex<PoolState>,
}
impl PermitPool {
    fn new(resource: &'static str, limit: Option<u64>) -> Arc<Self> {
        Arc::new(Self {
            resource,
            limit,
            state: Mutex::new(PoolState::default()),
        })
    }
    fn error(&self) -> StaticPolicyError {
        StaticPolicyError::WorkCapacity {
            resource: self.resource,
            limit: self.limit.unwrap_or(u64::MAX),
        }
    }
    fn acquire(self: &Arc<Self>, amount: u64) -> Result<Permit, StaticPolicyError> {
        let mut state = self.state.lock().or_poisoned();
        let next = state.used.checked_add(amount).ok_or_else(|| self.error())?;
        if self.limit.is_some_and(|limit| next > limit) {
            return Err(self.error());
        }
        state.used = next;
        Ok(Permit {
            pool: self.clone(),
            amount,
        })
    }
}

#[derive(Debug)]
pub(super) struct Permit {
    pool: Arc<PermitPool>,
    amount: u64,
}
impl Permit {
    pub(super) fn split_one(&mut self) -> Self {
        assert!(self.amount > 0, "reserved subscription count");
        self.amount -= 1;
        Self {
            pool: self.pool.clone(),
            amount: 1,
        }
    }
}
impl Drop for Permit {
    fn drop(&mut self) {
        let wakes = {
            let mut state = self.pool.state.lock().or_poisoned();
            state.used -= self.amount;
            state.pending.retain(|entry| entry.strong_count() != 0);
            state
                .pending
                .iter()
                .filter_map(Weak::upgrade)
                .collect::<Vec<_>>()
        };
        for wake in wakes {
            wake.wake();
        }
    }
}

pub(super) struct PermitFuture {
    pool: Arc<PermitPool>,
    wake: Arc<futures::task::AtomicWaker>,
    registered: bool,
}
impl PermitFuture {
    fn new(pool: Arc<PermitPool>) -> Self {
        Self {
            pool,
            wake: Arc::new(futures::task::AtomicWaker::new()),
            registered: false,
        }
    }
}
impl Future for PermitFuture {
    type Output = Permit;
    fn poll(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Permit> {
        self.wake.register(cx.waker());
        if !self.registered {
            {
                let mut state = self.pool.state.lock().or_poisoned();
                state.pending.retain(|entry| entry.strong_count() != 0);
                state.pending.push(Arc::downgrade(&self.wake));
            }
            self.registered = true;
        }
        match self.pool.acquire(1) {
            Ok(permit) => Poll::Ready(permit),
            Err(_) => Poll::Pending,
        }
    }
}
impl Drop for PermitFuture {
    fn drop(&mut self) {
        self.pool
            .state
            .lock()
            .or_poisoned()
            .pending
            .retain(|entry| entry.as_ptr() != Arc::as_ptr(&self.wake));
    }
}

#[cfg(test)]
#[path = "policy_specs.rs"]
mod specs;
