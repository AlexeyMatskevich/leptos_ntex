//! Filesystem operations anchored to an opened site directory.

use cap_std::{
    ambient_authority,
    fs::{Dir, OpenOptions},
};
use std::{
    ffi::OsString,
    fs, io,
    path::{Component, Path, PathBuf},
};

pub(crate) struct SiteRoot {
    dir: Dir,
    canonical: PathBuf,
}

impl SiteRoot {
    pub(crate) fn from_capability(dir: Dir, canonical: PathBuf) -> Self {
        Self { dir, canonical }
    }

    pub(crate) fn open(root: &Path) -> io::Result<Self> {
        let canonical = root.canonicalize()?;
        Self::open_canonical(&canonical)
    }

    // The caller supplies a canonical path captured by its root cache. Open a
    // fresh capability without resolving that cached root a second time.
    pub(crate) fn open_canonical(canonical: &Path) -> io::Result<Self> {
        let dir = Dir::open_ambient_dir(canonical, ambient_authority())?;
        Ok(Self {
            dir,
            canonical: canonical.to_path_buf(),
        })
    }

    pub(crate) fn canonical(&self) -> &Path {
        &self.canonical
    }

    // Ambient resolution preserves supported absolute symlinks inside the
    // root. It only proposes a relative candidate: the final operation always
    // uses the capability, so namespace changes cannot grant ambient access.
    fn relative(&self, path: &Path, missing: bool) -> io::Result<PathBuf> {
        let (canonical, suffix) = canonicalize_existing_prefix(path, |error| {
            missing && error.kind() == io::ErrorKind::NotFound
        })?;
        let mut relative = canonical
            .strip_prefix(&self.canonical)
            .map_err(|_| io::Error::new(io::ErrorKind::PermissionDenied, "file escapes site_root"))?
            .to_path_buf();
        for name in suffix {
            if name == "." || name == ".." {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidInput,
                    "missing path components must be plain names",
                ));
            }
            relative.push(name);
        }
        Ok(relative)
    }

    /// Opens a regular file below the root. A lexical descendant of the
    /// canonical root is opened through the capability directly; only paths
    /// that cannot be resolved that way (absolute in-root symlinks, aliases)
    /// pay for ambient canonicalization before the capability open.
    pub(crate) fn open_file(&self, path: &Path) -> io::Result<fs::File> {
        if let Ok(relative) = path.strip_prefix(&self.canonical)
            && !relative.as_os_str().is_empty()
            && relative
                .components()
                .all(|component| matches!(component, Component::Normal(_)))
            && let Ok(file) = open_regular(&self.dir, relative)
        {
            return Ok(file);
        }
        let relative = self.relative(path, false)?;
        open_regular(&self.dir, &relative)
    }

    pub(crate) fn parent(&self, path: &Path, create: bool) -> io::Result<(Dir, PathBuf)> {
        let parent = path
            .parent()
            .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "file has no parent"))?;
        let relative = self.relative(parent, create)?;
        let relative = if relative.as_os_str().is_empty() {
            Path::new(".")
        } else {
            relative.as_path()
        };
        if create {
            self.dir.create_dir_all(relative)?;
        }
        let dir = self.dir.open_dir(relative)?;
        let name = path
            .file_name()
            .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "file has no name"))?;
        Ok((dir, PathBuf::from(name)))
    }

    pub(crate) fn canonical_file(&self, path: &Path) -> io::Result<PathBuf> {
        Ok(self.canonical.join(self.relative(path, false)?))
    }
}

/// Canonicalizes the longest prefix of `path` that resolves, popping trailing
/// components while `retry` accepts the failure. Returns the canonical prefix
/// and the popped components in path order. A prefix that empties out
/// resolves the current directory.
pub(crate) fn canonicalize_existing_prefix(
    path: &Path,
    retry: impl Fn(&io::Error) -> bool,
) -> io::Result<(PathBuf, Vec<OsString>)> {
    let mut candidate = path.to_path_buf();
    let mut suffix = Vec::new();
    let canonical = loop {
        let probe = if candidate.as_os_str().is_empty() {
            Path::new(".")
        } else {
            candidate.as_path()
        };
        match probe.canonicalize() {
            Ok(canonical) => break canonical,
            Err(error) if retry(&error) => {
                // file_name() omits `.` and `..`; keep them so that callers can
                // resolve or refuse them against the existing prefix.
                let Some(Component::Normal(_) | Component::CurDir | Component::ParentDir) =
                    candidate.components().next_back()
                else {
                    return Err(error);
                };
                let component = candidate
                    .components()
                    .next_back()
                    .map(|component| component.as_os_str().to_os_string())
                    .ok_or(error)?;
                candidate.pop();
                suffix.push(component);
            }
            Err(error) => return Err(error),
        }
    };
    suffix.reverse();
    Ok((canonical, suffix))
}

/// Durably records directory entry changes (renames, creations) below `dir`.
///
/// cap-std keeps directories as `O_PATH` handles where the platform offers
/// them (Linux, Android, FreeBSD), and `fsync` refuses such a handle with
/// `EBADF`; the directory is therefore reopened for reading through the same
/// capability before it is synced.
pub(crate) fn sync_dir(dir: &Dir) -> io::Result<()> {
    dir.open(".")?.sync_all()
}

pub(crate) fn open_regular(dir: &Dir, path: &Path) -> io::Result<fs::File> {
    let mut options = OpenOptions::new();
    options.read(true);
    #[cfg(unix)]
    {
        use cap_std::fs::OpenOptionsExt;
        options.custom_flags(rustix::fs::OFlags::NONBLOCK.bits() as i32);
    }
    let file = dir.open_with(path, &options)?.into_std();
    if !file.metadata()?.is_file() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "not a regular file",
        ));
    }
    Ok(file)
}

/// Where a lock or control entry is looked up: through an opened directory
/// capability, or by ambient path when only search permission on the
/// directory is available (for example an ancestor of the site root).
#[derive(Clone, Copy)]
pub(crate) enum Anchor<'a> {
    Capability(&'a Dir),
    Ambient(&'a Path),
}

/// A lock entry could not be opened as a private regular file.
#[derive(Debug)]
pub(crate) enum LockEntryError {
    /// The entry is a symlink, directory, FIFO or other non-regular file.
    NotRegular,
    /// The inode has other names; a lock or record through it would alias them.
    Aliased,
    /// Any other filesystem failure, including a missing entry.
    Io(io::Error),
}
impl From<io::Error> for LockEntryError {
    fn from(error: io::Error) -> Self {
        Self::Io(error)
    }
}
impl From<LockEntryError> for io::Error {
    fn from(error: LockEntryError) -> Self {
        match error {
            LockEntryError::NotRegular => io::Error::new(
                io::ErrorKind::InvalidData,
                "lock entry is not a regular file",
            ),
            LockEntryError::Aliased => {
                io::Error::new(io::ErrorKind::InvalidData, "lock entry has another name")
            }
            LockEntryError::Io(error) => error,
        }
    }
}
impl LockEntryError {
    pub(crate) fn is_not_found(&self) -> bool {
        matches!(self, Self::Io(error) if error.kind() == io::ErrorKind::NotFound)
    }
}

/// Opens the private regular file `name` used as a lock or control record,
/// creating it when `create` is set. Symlinks are never followed; directories,
/// FIFOs and hard-linked inodes are refused; the descriptor is nonblocking so
/// that a planted special file cannot stall the opener.
pub(crate) fn open_lock_entry(
    anchor: Anchor<'_>,
    name: &Path,
    create: bool,
) -> Result<fs::File, LockEntryError> {
    if create {
        match anchor.open(name, true, true) {
            Ok(file) => return checked_lock_file(file),
            Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {}
            Err(error) => return Err(error.into()),
        }
    }
    if !anchor.is_regular_entry(name)? {
        return Err(LockEntryError::NotRegular);
    }
    checked_lock_file(anchor.open(name, create, false)?)
}

fn checked_lock_file(file: fs::File) -> Result<fs::File, LockEntryError> {
    let metadata = file.metadata()?;
    if !metadata.is_file() {
        return Err(LockEntryError::NotRegular);
    }
    #[cfg(unix)]
    if std::os::unix::fs::MetadataExt::nlink(&metadata) != 1 {
        return Err(LockEntryError::Aliased);
    }
    Ok(file)
}

impl Anchor<'_> {
    fn is_regular_entry(self, name: &Path) -> io::Result<bool> {
        Ok(match self {
            Self::Capability(dir) => dir.symlink_metadata(name)?.is_file(),
            Self::Ambient(path) => fs::symlink_metadata(path.join(name))?.is_file(),
        })
    }

    fn open(self, name: &Path, write: bool, create_new: bool) -> io::Result<fs::File> {
        match self {
            Self::Capability(dir) => {
                let mut options = OpenOptions::new();
                options.read(true).write(write).create_new(create_new);
                #[cfg(unix)]
                {
                    use cap_std::fs::OpenOptionsExt;
                    options.custom_flags(lock_entry_flags());
                }
                Ok(dir.open_with(name, &options)?.into_std())
            }
            Self::Ambient(path) => {
                let mut options = fs::OpenOptions::new();
                options.read(true).write(write).create_new(create_new);
                #[cfg(unix)]
                {
                    use std::os::unix::fs::OpenOptionsExt;
                    options.custom_flags(lock_entry_flags());
                }
                options.open(path.join(name))
            }
        }
    }
}

#[cfg(unix)]
fn lock_entry_flags() -> i32 {
    (rustix::fs::OFlags::NOFOLLOW | rustix::fs::OFlags::NONBLOCK).bits() as i32
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum LockMode {
    Shared,
    Exclusive,
}

/// A lock acquisition failed.
#[derive(Debug)]
pub(crate) enum LockError {
    /// The lock is held incompatibly and waiting was not requested.
    WouldBlock,
    Io(io::Error),
}

/// An advisory lock whose lease ends explicitly when the guard drops.
///
/// A concurrent fork can retain a reference to this open file description
/// until exec. Closing our descriptor alone would then prolong the lease
/// beyond its logical owner; explicit unlock ends it.
#[derive(Debug)]
pub(crate) struct FileLock {
    file: fs::File,
}
impl FileLock {
    pub(crate) fn acquire(file: fs::File, mode: LockMode, wait: bool) -> Result<Self, LockError> {
        let result = match (mode, wait) {
            (LockMode::Shared, true) => fs4::FileExt::lock_shared(&file),
            (LockMode::Exclusive, true) => fs4::FileExt::lock(&file),
            (LockMode::Shared, false) => try_lock_result(fs4::FileExt::try_lock_shared(&file)),
            (LockMode::Exclusive, false) => try_lock_result(fs4::FileExt::try_lock(&file)),
        };
        match result {
            Ok(()) => Ok(Self { file }),
            Err(error) if error.kind() == io::ErrorKind::WouldBlock => Err(LockError::WouldBlock),
            Err(error) => Err(LockError::Io(error)),
        }
    }
    pub(crate) fn file_mut(&mut self) -> &mut fs::File {
        &mut self.file
    }
}
fn try_lock_result(result: Result<(), fs4::TryLockError>) -> io::Result<()> {
    result.map_err(|error| match error {
        fs4::TryLockError::WouldBlock => io::Error::from(io::ErrorKind::WouldBlock),
        fs4::TryLockError::Error(error) => error,
    })
}
impl Drop for FileLock {
    fn drop(&mut self) {
        let _ = fs4::FileExt::unlock(&self.file);
    }
}

/// Opens (creating when `write` is set) and locks a lock entry, waiting for
/// incompatible holders. Writers take the exclusive lease, readers the shared one.
pub(crate) fn lock_entry(dir: &Dir, name: &Path, write: bool) -> io::Result<FileLock> {
    let file = open_lock_entry(Anchor::Capability(dir), name, write)?;
    let mode = if write {
        LockMode::Exclusive
    } else {
        LockMode::Shared
    };
    FileLock::acquire(file, mode, true).map_err(|error| match error {
        LockError::WouldBlock => io::Error::from(io::ErrorKind::WouldBlock),
        LockError::Io(error) => error,
    })
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use crate::tests::temp_site_root;
    use lets_expect::lets_expect;
    use std::io::Read;

    fn swapped_ancestor() -> (bool, String) {
        let parent = temp_site_root("capability_swap");
        let site = parent.join("site");
        let outside = parent.join("outside");
        fs::create_dir_all(site.join("folder")).unwrap();
        fs::create_dir(&outside).unwrap();
        fs::write(site.join("folder/file"), "INSIDE").unwrap();
        fs::write(outside.join("file"), "OUTSIDE").unwrap();
        let root = SiteRoot::open(&site).unwrap();
        // These are the two production steps in open_file, with the
        // namespace replacement deliberately placed between them.
        let relative = root.relative(&site.join("folder/file"), false).unwrap();
        fs::rename(site.join("folder"), site.join("old-folder")).unwrap();
        std::os::unix::fs::symlink(&outside, site.join("folder")).unwrap();
        let refused = open_regular(&root.dir, &relative).is_err();
        (refused, fs::read_to_string(outside.join("file")).unwrap())
    }

    fn internal_symlink(absolute: bool) -> String {
        let site = temp_site_root("capability_internal_symlink");
        fs::write(site.join("actual"), "INSIDE").unwrap();
        let target = if absolute {
            site.join("actual")
        } else {
            PathBuf::from("actual")
        };
        std::os::unix::fs::symlink(target, site.join("link")).unwrap();
        let mut file = SiteRoot::open(&site)
            .unwrap()
            .open_file(&site.join("link"))
            .unwrap();
        let mut body = String::new();
        file.read_to_string(&mut body).unwrap();
        body
    }

    fn fifo_open() -> bool {
        let site = temp_site_root("capability_fifo");
        let path = site.join("pipe");
        assert!(
            std::process::Command::new("mkfifo")
                .arg(&path)
                .status()
                .unwrap()
                .success(),
            "FIFO fixture must be created"
        );
        SiteRoot::open(&site)
            .unwrap()
            .open_file(&path)
            .is_err_and(|error| error.kind() == io::ErrorKind::InvalidInput)
    }

    #[derive(Clone, Copy)]
    enum Planted {
        Regular,
        Absent,
        Symlink,
        Directory,
        HardLink,
        Fifo,
    }
    fn lock_entry_outcome(planted: Planted, create: bool) -> Result<(), String> {
        let site = temp_site_root("lock_entry");
        let name = Path::new(".probe.lock");
        let path = site.join(name);
        match planted {
            Planted::Regular => fs::write(&path, "").unwrap(),
            Planted::Absent => {}
            Planted::Symlink => {
                fs::write(site.join("target"), "").unwrap();
                std::os::unix::fs::symlink("target", &path).unwrap();
            }
            Planted::Directory => fs::create_dir(&path).unwrap(),
            Planted::HardLink => {
                fs::write(&path, "").unwrap();
                fs::hard_link(&path, site.join("alias")).unwrap();
            }
            Planted::Fifo => assert!(
                std::process::Command::new("mkfifo")
                    .arg(&path)
                    .status()
                    .unwrap()
                    .success(),
                "FIFO fixture must be created"
            ),
        }
        let dir = Dir::open_ambient_dir(&*site, ambient_authority()).unwrap();
        open_lock_entry(Anchor::Capability(&dir), name, create)
            .map(|_| ())
            .map_err(|error| match error {
                LockEntryError::NotRegular => "not-regular".to_owned(),
                LockEntryError::Aliased => "aliased".to_owned(),
                LockEntryError::Io(error) => format!("{:?}", error.kind()),
            })
    }

    lets_expect! {
        expect(lock_entry_outcome(planted, create)) as the_lock_entry {
            let planted = Planted::Regular;
            let create = false;
            to opens_the_private_regular_file { equal(Ok(())) }
            when the_entry_is_absent {
                let planted = Planted::Absent;
                to reports_not_found { equal(Err("NotFound".to_owned())) }
                when creation_is_requested { let create = true; to creates_it { equal(Ok(())) } }
            }
            when a_symlink_occupies_the_name {
                let planted = Planted::Symlink;
                to refuses_to_follow_it { equal(Err("not-regular".to_owned())) }
                when creation_is_requested { let create = true; to refuses_to_follow_it { equal(Err("not-regular".to_owned())) } }
            }
            when a_directory_occupies_the_name { let planted = Planted::Directory; to refuses_it { equal(Err("not-regular".to_owned())) } }
            when the_inode_has_another_name { let planted = Planted::HardLink; to refuses_the_alias { equal(Err("aliased".to_owned())) } }
            when a_fifo_occupies_the_name { let planted = Planted::Fifo; to refuses_without_waiting_for_a_writer { equal(Err("not-regular".to_owned())) } }
        }
    }

    lets_expect! {
        expect(swapped_ancestor()) as the_replaced_path_ancestor {
            to does_not_follow_the_new_outside_target { equal((true, "OUTSIDE".to_owned())) }
        }
        expect(internal_symlink(absolute)) as the_internal_symlink {
            let absolute = false;
            to serves_the_internal_file { equal("INSIDE".to_owned()) }
            when the_link_target_is_absolute {
                let absolute = true;
                to preserves_internal_symlink_compatibility { equal("INSIDE".to_owned()) }
            }
        }
        expect(fifo_open()) as the_fifo_target {
            to rejects_without_waiting_for_a_writer { be_true }
        }
    }
}
