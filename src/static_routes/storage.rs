//! Cooperating-process admission for the static publication transaction.

use crate::fs_boundary::{Anchor, FileLock, LockEntryError, LockError, LockMode, open_lock_entry};
use cap_std::{ambient_authority, fs::Dir};
use std::{
    cell::{Cell, RefCell},
    ffi::OsStr,
    fmt, fs,
    io::{self, Read, Seek, SeekFrom},
    path::{Path, PathBuf},
    sync::{Arc, Mutex},
};

#[cfg(unix)]
use std::io::Write;

const CONTROL: &str = ".leptos-static-policy.lock";
const RECORD_LEN: usize = 40;
const GENERATION_OFFSET: u64 = 32;
const MAGIC: &[u8; 8] = b"LNTXST02";

/// Optional bounds for named files and entries below one static site root.
///
/// Logical bytes count each regular file entry, including assets, hard links,
/// old artifacts and crash remnants. They do not measure physical disk blocks
/// or open files that have been unlinked. Entries include files, directories,
/// symlinks and special files; the root directory itself is excluded.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct StaticStorageLimits {
    logical_file_bytes: Option<u64>,
    namespace_entries: Option<u64>,
}
impl StaticStorageLimits {
    /// Creates a policy with no numerical limits.
    pub const fn new() -> Self {
        Self {
            logical_file_bytes: None,
            namespace_entries: None,
        }
    }
    /// Bounds logical named regular-file bytes, including publication staging.
    pub const fn with_logical_file_bytes(mut self, maximum: u64) -> Self {
        self.logical_file_bytes = Some(maximum);
        self
    }
    /// Bounds descendant namespace entries, including locks and staging files.
    pub const fn with_namespace_entries(mut self, maximum: u64) -> Self {
        self.namespace_entries = Some(maximum);
        self
    }
    /// Returns the logical byte limit, if configured.
    pub const fn logical_file_bytes(self) -> Option<u64> {
        self.logical_file_bytes
    }
    /// Returns the namespace entry limit, if configured.
    pub const fn namespace_entries(self) -> Option<u64> {
        self.namespace_entries
    }
}

/// Storage policy validation or publication admission failed.
#[derive(Clone, Debug)]
#[non_exhaustive]
pub enum StaticStorageError {
    /// Another cooperating operation holds an incompatible root lock while a
    /// policy is being installed. Publication waits for such holders instead.
    Busy,
    /// The complete publication peak would exceed a configured bound.
    LimitExceeded {
        /// The bounded resource that would be exceeded.
        resource: &'static str,
        /// The configured maximum.
        limit: u64,
        /// The accounted publication peak.
        required: u64,
    },
    /// This root already has a different immutable policy.
    PolicyMismatch,
    /// A publisher must bind the policy already installed for its destination.
    BindingRequired,
    /// The persistent control record is malformed or has an unknown version.
    InvalidPolicy,
    /// The root identity or overlapping scope cannot be supported.
    UnsupportedRoot,
    /// Bounded storage is not implemented on this platform.
    UnsupportedPlatform,
    /// A filesystem operation failed; the original kind and message are kept.
    Io(Arc<io::Error>),
    /// Resource accounting exceeded the range of a u64.
    Overflow,
}
impl StaticStorageError {
    /// Whether the failure is a capacity condition that later work can clear,
    /// rather than a configuration or filesystem fault.
    pub(super) fn is_capacity(&self) -> bool {
        matches!(self, Self::Busy | Self::LimitExceeded { .. })
    }
}
impl fmt::Display for StaticStorageError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Busy => f.write_str("static storage publication is busy"),
            Self::LimitExceeded {
                resource,
                limit,
                required,
            } => write!(
                f,
                "static storage {resource} limit {limit} would require {required}"
            ),
            Self::PolicyMismatch => {
                f.write_str("static storage policy does not match the installed policy")
            }
            Self::BindingRequired => {
                f.write_str("static publication requires the installed storage policy")
            }
            Self::InvalidPolicy => f.write_str("invalid static storage policy record"),
            Self::UnsupportedRoot => f.write_str("unsupported or overlapping static storage root"),
            Self::UnsupportedPlatform => {
                f.write_str("bounded static storage is not implemented on this platform")
            }
            Self::Io(error) => error.fmt(f),
            Self::Overflow => f.write_str("static storage accounting overflow"),
        }
    }
}
impl std::error::Error for StaticStorageError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Io(error) => Some(&**error),
            _ => None,
        }
    }
}
impl PartialEq for StaticStorageError {
    fn eq(&self, other: &Self) -> bool {
        match (self, other) {
            (Self::Io(a), Self::Io(b)) => a.kind() == b.kind() && a.to_string() == b.to_string(),
            (
                Self::LimitExceeded {
                    resource: a,
                    limit: b,
                    required: c,
                },
                Self::LimitExceeded {
                    resource: x,
                    limit: y,
                    required: z,
                },
            ) => (a, b, c) == (x, y, z),
            (Self::Busy, Self::Busy)
            | (Self::PolicyMismatch, Self::PolicyMismatch)
            | (Self::BindingRequired, Self::BindingRequired)
            | (Self::InvalidPolicy, Self::InvalidPolicy)
            | (Self::UnsupportedRoot, Self::UnsupportedRoot)
            | (Self::UnsupportedPlatform, Self::UnsupportedPlatform)
            | (Self::Overflow, Self::Overflow) => true,
            _ => false,
        }
    }
}
impl Eq for StaticStorageError {}
impl From<io::Error> for StaticStorageError {
    fn from(error: io::Error) -> Self {
        Self::Io(Arc::new(error))
    }
}
impl From<LockEntryError> for StaticStorageError {
    fn from(error: LockEntryError) -> Self {
        match error {
            // A record must live in a private regular file: writing it through
            // a link would grow every alias at once and could leave this root.
            LockEntryError::NotRegular | LockEntryError::Aliased => Self::InvalidPolicy,
            LockEntryError::Io(error) => error.into(),
        }
    }
}
impl From<LockError> for StaticStorageError {
    fn from(error: LockError) -> Self {
        match error {
            LockError::WouldBlock => Self::Busy,
            LockError::Io(error) => error.into(),
        }
    }
}

/// The persistent control record: immutable limits plus a publication
/// generation that cooperating processes bump around every managed
/// publication, so that a cached inventory is valid exactly while nobody else
/// has published.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Record {
    limits: StaticStorageLimits,
    generation: u64,
}

#[cfg(any(unix, test))]
fn encode(limits: StaticStorageLimits) -> [u8; RECORD_LEN] {
    let mut bytes = [0; RECORD_LEN];
    bytes[..8].copy_from_slice(MAGIC);
    bytes[8] = u8::from(limits.logical_file_bytes.is_some());
    bytes[9] = u8::from(limits.namespace_entries.is_some());
    bytes[16..24].copy_from_slice(&limits.logical_file_bytes.unwrap_or(0).to_le_bytes());
    bytes[24..32].copy_from_slice(&limits.namespace_entries.unwrap_or(0).to_le_bytes());
    bytes
}
fn decode(bytes: &[u8]) -> Result<Record, StaticStorageError> {
    if bytes.len() != RECORD_LEN
        || &bytes[..8] != MAGIC
        || bytes[8] > 1
        || bytes[9] > 1
        || bytes[10..16] != [0; 6]
    {
        return Err(StaticStorageError::InvalidPolicy);
    }
    let field = |range: std::ops::Range<usize>| {
        bytes[range]
            .try_into()
            .map(u64::from_le_bytes)
            .map_err(|_| StaticStorageError::InvalidPolicy)
    };
    let bytes_value = field(16..24)?;
    let entries_value = field(24..32)?;
    let generation = field(32..40)?;
    if (bytes[8] == 0 && bytes_value != 0) || (bytes[9] == 0 && entries_value != 0) {
        return Err(StaticStorageError::InvalidPolicy);
    }
    Ok(Record {
        limits: StaticStorageLimits {
            logical_file_bytes: (bytes[8] == 1).then_some(bytes_value),
            namespace_entries: (bytes[9] == 1).then_some(entries_value),
        },
        generation,
    })
}
fn read_record(file: &mut fs::File) -> Result<Option<Record>, StaticStorageError> {
    match file.metadata()?.len() {
        0 => Ok(None),
        length if length == RECORD_LEN as u64 => {
            file.seek(SeekFrom::Start(0))?;
            let mut bytes = [0; RECORD_LEN];
            file.read_exact(&mut bytes)?;
            decode(&bytes).map(Some)
        }
        _ => Err(StaticStorageError::InvalidPolicy),
    }
}
#[cfg(unix)]
fn write_generation(file: &mut fs::File, generation: u64) -> io::Result<()> {
    // Visibility to cooperating processes goes through the shared page cache;
    // durability is irrelevant, since every process restarts with an empty cache.
    file.seek(SeekFrom::Start(GENERATION_OFFSET))?;
    file.write_all(&generation.to_le_bytes())
}

fn open_control(anchor: Anchor<'_>, create: bool) -> Result<fs::File, StaticStorageError> {
    Ok(open_lock_entry(anchor, Path::new(CONTROL), create)?)
}
fn lock_control(
    file: fs::File,
    mode: LockMode,
    wait: bool,
) -> Result<FileLock, StaticStorageError> {
    Ok(FileLock::acquire(file, mode, wait)?)
}

/// Whether the directory at `anchor` carries an installed policy record.
/// Only search permission on an ambient ancestor is required.
fn active_policy(anchor: Anchor<'_>) -> Result<bool, StaticStorageError> {
    let file = match open_lock_entry(anchor, Path::new(CONTROL), false) {
        Err(error) if error.is_not_found() => return Ok(false),
        result => result?,
    };
    let mut control = lock_control(file, LockMode::Shared, true)?;
    Ok(read_record(control.file_mut())?.is_some())
}
/// Whether any directory strictly between `stop` (exclusive) and `path`
/// (exclusive) carries a policy. `None` walks to the filesystem root.
fn ancestor_policy(path: &Path, stop: Option<&Path>) -> Result<bool, StaticStorageError> {
    for ancestor in path.ancestors().skip(1) {
        if stop == Some(ancestor) {
            break;
        }
        if active_policy(Anchor::Ambient(ancestor))? {
            return Ok(true);
        }
    }
    Ok(false)
}
fn open_root(
    path: &Path,
    overlap: StaticStorageError,
) -> Result<(Dir, PathBuf), StaticStorageError> {
    // Refuse overlapping scopes before mkdir: otherwise even a rejected writer
    // could consume entries inside a managed ancestor before admission.
    let (ancestor, missing) = crate::fs_boundary::canonicalize_existing_prefix(path, |error| {
        error.kind() == io::ErrorKind::NotFound
    })?;
    if (!missing.is_empty() && active_policy(Anchor::Ambient(&ancestor))?)
        || ancestor_policy(&ancestor, None)?
    {
        return Err(overlap);
    }
    fs::create_dir_all(path)?;
    let canonical = path.canonicalize()?;
    let dir = Dir::open_ambient_dir(&canonical, ambient_authority())?;
    Ok((dir, canonical))
}

fn same_root(first: &Dir, second: &Dir) -> Result<bool, StaticStorageError> {
    #[cfg(unix)]
    {
        use cap_std::fs::MetadataExt;
        let a = first.dir_metadata()?;
        let b = second.dir_metadata()?;
        Ok(a.dev() == b.dev() && a.ino() == b.ino())
    }
    #[cfg(not(unix))]
    {
        let _ = (first, second);
        Err(StaticStorageError::UnsupportedPlatform)
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Cached {
    generation: u64,
    usage: Usage,
}
#[derive(Debug)]
struct StorageInner {
    dir: Dir,
    canonical: PathBuf,
    limits: StaticStorageLimits,
    // The site inventory as of `generation`. Valid while the record still
    // carries that generation, that is while no cooperating process has begun
    // or finished another managed publication.
    cached: Mutex<Option<Cached>>,
}
#[derive(Clone, Debug)]
pub(super) struct Storage(Arc<StorageInner>);
impl Storage {
    pub(super) fn open(
        root: &Path,
        limits: StaticStorageLimits,
    ) -> Result<Self, StaticStorageError> {
        #[cfg(not(unix))]
        {
            let _ = (root, limits);
            Err(StaticStorageError::UnsupportedPlatform)
        }
        #[cfg(unix)]
        {
            let (dir, canonical) = open_root(root, StaticStorageError::UnsupportedRoot)?;
            // Installation must not wait behind a publisher: a stalled lease
            // (for example a descriptor inherited by a fork) would otherwise
            // hang startup silently. Publication itself waits, see coordinate.
            let mut control = lock_control(
                open_control(Anchor::Capability(&dir), true)?,
                LockMode::Exclusive,
                false,
            )?;
            let previous = read_record(control.file_mut())?;
            if previous.is_some_and(|previous| previous.limits != limits) {
                return Err(StaticStorageError::PolicyMismatch);
            }
            let mut usage = inventory(&dir)?;
            if previous.is_none() {
                usage.bytes = add(usage.bytes, RECORD_LEN as u64)?;
            }
            check_limits(usage, limits)?;
            let generation = match previous {
                Some(previous) => previous.generation,
                None => {
                    control.file_mut().write_all(&encode(limits))?;
                    control.file_mut().sync_all()?;
                    crate::fs_boundary::sync_dir(&dir)?;
                    0
                }
            };
            Ok(Self(Arc::new(StorageInner {
                dir,
                canonical,
                limits,
                cached: Mutex::new(Some(Cached { generation, usage })),
            })))
        }
    }
    /// The canonical path of the managed root, which identifies the policy's
    /// scope inside a process.
    pub(super) fn canonical(&self) -> &Path {
        &self.0.canonical
    }
    #[cfg(unix)]
    fn cached(&self, generation: u64) -> Option<Usage> {
        (*self
            .0
            .cached
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner))
        .filter(|cached| cached.generation == generation)
        .map(|cached| cached.usage)
    }
    #[cfg(unix)]
    fn remember(&self, cached: Option<Cached>) {
        *self
            .0
            .cached
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = cached;
    }
}

// Accounting state between admission and commit of one publication.
struct Pending {
    total: Usage,
    before: Usage,
    existing: Dir,
    created: Option<std::ffi::OsString>,
}

pub(super) struct PublicationGuard {
    // Logical ownership ends explicitly, even if another process inherited an fd.
    control: RefCell<FileLock>,
    dir: Dir,
    canonical: PathBuf,
    storage: Option<Storage>,
    generation: Cell<u64>,
    pending: RefCell<Option<Pending>>,
}
/// Takes the root publication lease: exclusive for a bound publisher, whose
/// admission accounts the whole site, shared for an unlimited one. Publication
/// runs on the blocking pool, so incompatible holders are awaited rather than
/// refused; concurrent renders serialize only at their publication step.
pub(super) fn coordinate(
    root: &Path,
    storage: Option<&Storage>,
) -> Result<PublicationGuard, StaticStorageError> {
    let (dir, canonical) = if let Some(storage) = storage {
        // A bound publisher must never create a different root while checking
        // its identity. The pinned capability remains the publication anchor.
        // Its ancestors were checked at installation and cannot become managed
        // afterwards: an ancestor installation finds this root's record.
        let canonical = root.canonicalize()?;
        let opened = Dir::open_ambient_dir(&canonical, ambient_authority())?;
        if !same_root(&opened, &storage.0.dir)? {
            return Err(StaticStorageError::UnsupportedRoot);
        }
        (storage.0.dir.try_clone()?, canonical)
    } else {
        open_root(root, StaticStorageError::BindingRequired)?
    };
    let mode = if storage.is_some() {
        LockMode::Exclusive
    } else {
        LockMode::Shared
    };
    let mut control = lock_control(open_control(Anchor::Capability(&dir), true)?, mode, true)?;
    let record = read_record(control.file_mut())?;
    let generation = match (storage, record) {
        (Some(storage), Some(record)) if record.limits == storage.0.limits => record.generation,
        (Some(_), _) => return Err(StaticStorageError::PolicyMismatch),
        (None, Some(_)) => return Err(StaticStorageError::BindingRequired),
        (None, None) => 0,
    };
    Ok(PublicationGuard {
        control: RefCell::new(control),
        dir,
        canonical,
        storage: storage.cloned(),
        generation: Cell::new(generation),
        pending: RefCell::new(None),
    })
}
impl PublicationGuard {
    pub(super) fn site_root(&self) -> io::Result<crate::fs_boundary::SiteRoot> {
        Ok(crate::fs_boundary::SiteRoot::from_capability(
            self.dir.try_clone()?,
            self.canonical.clone(),
        ))
    }
    #[cfg(test)]
    pub(super) fn dir(&self) -> &Dir {
        &self.dir
    }
    #[cfg(test)]
    pub(super) fn control_file(&self) -> fs::File {
        self.control
            .borrow_mut()
            .file_mut()
            .try_clone()
            .expect("control descriptor clone")
    }
    /// Accounts the complete publication peak of `file_path` against the bound
    /// limits, or checks that an unlimited destination is not managed.
    pub(super) fn admit(
        &self,
        file_path: &Path,
        html_bytes: u64,
        metadata_bytes: u64,
    ) -> Result<(), StaticStorageError> {
        let site = self.site_root()?;
        let mut parent = file_path
            .parent()
            .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "file has no parent"))?
            .to_path_buf();
        let name = file_path
            .file_name()
            .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "file has no name"))?;
        let mut missing_parents = 0;
        let mut created = None;
        let dir = loop {
            match site.parent(&parent.join(name), false) {
                Ok((dir, _)) => break dir,
                Err(error) if error.kind() == io::ErrorKind::NotFound => {
                    missing_parents = add(missing_parents, 1)?;
                    created = parent.file_name().map(OsStr::to_os_string);
                    parent = parent.parent().ok_or(error)?.to_path_buf();
                }
                Err(error) => return Err(error.into()),
            }
        };
        // Unlimited publishers need only inspect their destination ancestry
        // inside this root; the root's own ancestors were checked when it was
        // opened. Managed scans detect an installed descendant policy instead.
        let Some(storage) = &self.storage else {
            let canonical_parent = parent.canonicalize()?;
            if active_policy(Anchor::Ambient(&canonical_parent))?
                || ancestor_policy(&canonical_parent, Some(&self.canonical))?
            {
                return Err(StaticStorageError::BindingRequired);
            }
            return Ok(());
        };
        let generation = self.generation.get();
        let total = match storage.cached(generation) {
            Some(usage) => usage,
            None => {
                let usage = inventory(&self.dir)?;
                storage.remember(Some(Cached { generation, usage }));
                usage
            }
        };
        let mut usage = total;
        usage.bytes = add(add(usage.bytes, html_bytes)?, metadata_bytes)?;
        let name = Path::new(name);
        let mut entries = add(missing_parents, 2)?;
        if missing_parents != 0 {
            // Scratch lock, publication lock and the metadata directory.
            entries = add(entries, 3)?;
        } else {
            entries = add(entries, missing_entry(&dir, &super::lock_name(name))?)?;
            entries = add(
                entries,
                missing_entry(&dir, Path::new(super::PUBLICATION_LOCK))?,
            )?;
            entries = add(
                entries,
                missing_entry(&dir, Path::new(super::METADATA_DIRECTORY))?,
            )?;
        }
        usage.entries = add(usage.entries, entries)?;
        check_limits(usage, storage.0.limits)?;
        let before = local_usage(&dir, created.as_deref())?;
        // Announce the publication before creating files: a process that
        // crashes here leaves a generation no cache matches, so every
        // cooperating publisher re-scans the remnants.
        self.bump()?;
        *self.pending.borrow_mut() = Some(Pending {
            total,
            before,
            existing: dir,
            created,
        });
        Ok(())
    }
    #[cfg(unix)]
    fn bump(&self) -> Result<(), StaticStorageError> {
        let next = add(self.generation.get(), 1)?;
        write_generation(self.control.borrow_mut().file_mut(), next)?;
        self.generation.set(next);
        Ok(())
    }
    #[cfg(not(unix))]
    fn bump(&self) -> Result<(), StaticStorageError> {
        Err(StaticStorageError::UnsupportedPlatform)
    }
    /// Records a completed publication: the site inventory changes exactly by
    /// the difference observed in the touched directory, so the cached total
    /// stays exact without another full scan. Any failure here only drops the
    /// cache; the publication itself has already been committed to disk.
    pub(super) fn commit(&self) {
        let Some(storage) = &self.storage else {
            return;
        };
        let Some(pending) = self.pending.borrow_mut().take() else {
            return;
        };
        let updated = local_usage(&pending.existing, pending.created.as_deref())
            .ok()
            .and_then(|after| {
                Some(Usage {
                    bytes: pending
                        .total
                        .bytes
                        .checked_sub(pending.before.bytes)?
                        .checked_add(after.bytes)?,
                    entries: pending
                        .total
                        .entries
                        .checked_sub(pending.before.entries)?
                        .checked_add(after.entries)?,
                })
            });
        let cached = match (updated, self.bump()) {
            (Some(usage), Ok(())) => Some(Cached {
                generation: self.generation.get(),
                usage,
            }),
            _ => None,
        };
        storage.remember(cached);
    }
}
fn missing_entry(dir: &Dir, path: &Path) -> Result<u64, StaticStorageError> {
    match dir.symlink_metadata(path) {
        Ok(_) => Ok(0),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(1),
        Err(error) => Err(error.into()),
    }
}
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(super) struct Usage {
    pub(super) bytes: u64,
    pub(super) entries: u64,
}
fn add(a: u64, b: u64) -> Result<u64, StaticStorageError> {
    a.checked_add(b).ok_or(StaticStorageError::Overflow)
}
fn check_limits(usage: Usage, limits: StaticStorageLimits) -> Result<(), StaticStorageError> {
    for (resource, required, limit) in [
        ("logical file bytes", usage.bytes, limits.logical_file_bytes),
        ("namespace entries", usage.entries, limits.namespace_entries),
    ] {
        if let Some(limit) = limit
            && required > limit
        {
            return Err(StaticStorageError::LimitExceeded {
                resource,
                limit,
                required,
            });
        }
    }
    Ok(())
}
fn charge(usage: &mut Usage, metadata: &cap_std::fs::Metadata) -> Result<(), StaticStorageError> {
    usage.entries = add(usage.entries, 1)?;
    if metadata.is_file() {
        usage.bytes = add(usage.bytes, metadata.len())?;
    }
    Ok(())
}
/// Every entry below `root`, refusing a managed descendant.
pub(super) fn inventory(root: &Dir) -> Result<Usage, StaticStorageError> {
    let mut usage = Usage::default();
    let mut directories = vec![root.entries()?];
    while let Some(entries) = directories.last_mut() {
        let Some(entry) = entries.next() else {
            directories.pop();
            continue;
        };
        let entry = entry?;
        let metadata = entry.metadata()?;
        charge(&mut usage, &metadata)?;
        if metadata.is_dir() {
            let dir = entry.open_dir()?;
            if active_policy(Anchor::Capability(&dir))? {
                return Err(StaticStorageError::UnsupportedRoot);
            }
            directories.push(dir.entries()?);
        }
    }
    Ok(usage)
}
/// The entries a publication into `dir` can touch: its direct children, the
/// metadata directory below it and, when publication creates a new
/// subdirectory chain, that chain. Taken before and after publication, the
/// difference is the exact change of the site inventory.
fn local_usage(dir: &Dir, created: Option<&OsStr>) -> Result<Usage, StaticStorageError> {
    let mut usage = Usage::default();
    for entry in dir.entries()? {
        let entry = entry?;
        let metadata = entry.metadata()?;
        charge(&mut usage, &metadata)?;
        let name = entry.file_name();
        if metadata.is_dir() && (name == super::METADATA_DIRECTORY || Some(&*name) == created) {
            let nested = inventory(&entry.open_dir()?)?;
            usage.bytes = add(usage.bytes, nested.bytes)?;
            usage.entries = add(usage.entries, nested.entries)?;
        }
    }
    Ok(usage)
}

#[cfg(test)]
#[path = "storage_specs.rs"]
mod specs;
