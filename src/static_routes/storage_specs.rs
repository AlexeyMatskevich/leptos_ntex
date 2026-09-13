use super::*;
#[cfg(unix)]
use crate::tests::temp_site_root;
use lets_expect::*;

#[cfg(unix)]
fn have_io_kind<T: fmt::Debug>(
    kind: io::ErrorKind,
) -> impl Fn(&Result<T, StaticStorageError>) -> AssertionResult {
    move |result| match result {
        Err(StaticStorageError::Io(error)) if error.kind() == kind => Ok(()),
        other => Err(AssertionError::new(vec![format!(
            "expected I/O {kind:?}, received {other:?}"
        )])),
    }
}

#[cfg(unix)]
fn exceeded(resource: &'static str, limit: u64, required: u64) -> StaticStorageError {
    StaticStorageError::LimitExceeded {
        resource,
        limit,
        required,
    }
}

#[derive(Clone, Copy)]
enum Damage {
    None,
    Partial,
    Magic,
    Flag,
    Reserved,
    Trailing,
    AbsentValue,
}
fn record(
    limits: StaticStorageLimits,
    damage: Damage,
) -> Result<StaticStorageLimits, StaticStorageError> {
    // A fresh record carries generation zero; decoding keeps the limits.
    let mut bytes = encode(limits).to_vec();
    match damage {
        Damage::None => {}
        Damage::Partial => {
            bytes.pop();
        }
        Damage::Magic => bytes[0] = b'X',
        Damage::Flag => bytes[8] = 2,
        Damage::Reserved => bytes[10] = 1,
        Damage::Trailing => bytes.push(0),
        Damage::AbsentValue => bytes[8] = 0,
    }
    decode(&bytes).map(|record| record.limits)
}

#[cfg(unix)]
mod filesystem {
    use super::*;
    use std::os::unix::fs::{MetadataExt, PermissionsExt, symlink};

    fn installed() -> (StaticStorageLimits, u64, usize) {
        let root = temp_site_root("static_storage_install");
        let limits = StaticStorageLimits::new()
            .with_logical_file_bytes(RECORD_LEN as u64)
            .with_namespace_entries(1);
        Storage::open(&root, limits).unwrap();
        (
            decode(&fs::read(root.join(CONTROL)).unwrap())
                .unwrap()
                .limits,
            fs::metadata(root.join(CONTROL)).unwrap().len(),
            fs::read_dir(&*root).unwrap().count(),
        )
    }
    fn reopen(changed: bool) -> (Result<(), StaticStorageError>, bool, Vec<u8>) {
        let root = temp_site_root("static_storage_reopen");
        let limits = StaticStorageLimits::new().with_logical_file_bytes(100);
        Storage::open(&root, limits).unwrap();
        let before = fs::metadata(root.join(CONTROL)).unwrap();
        let next = if changed {
            limits.with_logical_file_bytes(101)
        } else {
            limits
        };
        let result = Storage::open(&root, next).map(|_| ());
        let after = fs::metadata(root.join(CONTROL)).unwrap();
        (
            result,
            (before.dev(), before.ino()) == (after.dev(), after.ino()),
            fs::read(root.join(CONTROL)).unwrap(),
        )
    }
    fn invalid_existing() -> (Result<(), StaticStorageError>, Vec<u8>) {
        let root = temp_site_root("static_storage_partial");
        fs::write(root.join(CONTROL), b"LNTX").unwrap();
        let result = Storage::open(&root, StaticStorageLimits::new()).map(|_| ());
        (result, fs::read(root.join(CONTROL)).unwrap())
    }
    fn bootstrap(
        limits: StaticStorageLimits,
        asset: bool,
    ) -> (Result<(), StaticStorageError>, u64, Option<Vec<u8>>) {
        let root = temp_site_root("static_storage_bootstrap");
        if asset {
            fs::write(root.join("asset"), b"ASSET").unwrap();
        }
        let result = Storage::open(&root, limits).map(|_| ());
        (
            result,
            fs::metadata(root.join(CONTROL)).unwrap().len(),
            fs::read(root.join("asset")).ok(),
        )
    }

    #[derive(Clone, Copy)]
    enum Seed {
        Empty,
        Replaced,
        Orphan,
    }
    fn byte_admission(
        limit: Option<u64>,
        seed: Seed,
        html: u64,
    ) -> (Result<(), StaticStorageError>, bool) {
        let root = temp_site_root("static_storage_bytes");
        match seed {
            Seed::Empty => {}
            Seed::Replaced => {
                fs::write(root.join("page.html"), [0; 64]).unwrap();
                fs::create_dir(root.join(crate::static_routes::METADATA_DIRECTORY)).unwrap();
                fs::write(
                    root.join(crate::static_routes::metadata_name(Path::new("page.html"))),
                    [0; 16],
                )
                .unwrap();
            }
            Seed::Orphan => {
                fs::write(root.join(".leptos-temp-abandoned"), [0; 17]).unwrap();
            }
        }
        let limits = StaticStorageLimits {
            logical_file_bytes: limit,
            namespace_entries: None,
        };
        let storage = Storage::open(&root, limits).unwrap();
        let guard = coordinate(&root, Some(&storage)).unwrap();
        let before = inventory(guard.dir()).unwrap();
        let result = guard.admit(&root.join("page.html"), html, 16);
        (result, before == inventory(guard.dir()).unwrap())
    }
    #[derive(Clone, Copy)]
    enum Layout {
        Short,
        Nested,
        Long,
        ExistingLocks,
        Replacement,
    }
    fn entry_admission(limit: u64, layout: Layout) -> (Result<(), StaticStorageError>, bool) {
        let root = temp_site_root("static_storage_entries");
        let name = match layout {
            Layout::Long => format!("{}.html", "a".repeat(250)),
            _ => "page.html".to_owned(),
        };
        let relative = match layout {
            Layout::Nested => PathBuf::from("a/b").join(&name),
            _ => PathBuf::from(&name),
        };
        if matches!(layout, Layout::ExistingLocks) {
            fs::write(
                root.join(crate::static_routes::lock_name(Path::new(&name))),
                b"",
            )
            .unwrap();
            fs::write(root.join(crate::static_routes::PUBLICATION_LOCK), b"").unwrap();
        }
        if matches!(layout, Layout::Replacement) {
            fs::write(root.join(&name), b"OLD").unwrap();
            fs::create_dir(root.join(crate::static_routes::METADATA_DIRECTORY)).unwrap();
            fs::write(
                root.join(crate::static_routes::metadata_name(Path::new(&name))),
                b"{}",
            )
            .unwrap();
        }
        let storage = Storage::open(
            &root,
            StaticStorageLimits::new().with_namespace_entries(limit),
        )
        .unwrap();
        let guard = coordinate(&root, Some(&storage)).unwrap();
        let before = inventory(guard.dir()).unwrap();
        let result = guard.admit(&root.join(relative), 4, 2);
        (result, before == inventory(guard.dir()).unwrap())
    }

    #[derive(Clone, Copy)]
    enum Contents {
        Ordinary,
        HardLink,
        SymlinkDirectory,
        EmptyDirectory,
        Fifo,
        Unreadable,
    }
    struct RestoreMode(PathBuf);
    impl Drop for RestoreMode {
        fn drop(&mut self) {
            let _ = fs::set_permissions(&self.0, fs::Permissions::from_mode(0o700));
        }
    }
    fn scanned(contents: Contents) -> Result<Usage, StaticStorageError> {
        let root = temp_site_root("static_storage_inventory");
        let outside = temp_site_root("static_storage_outside");
        fs::write(root.join("asset"), b"ASSET").unwrap();
        let _restore;
        match contents {
            Contents::Ordinary => {
                fs::write(root.join(".orphan"), b"AB").unwrap();
                fs::write(root.join("metadata.json"), b"INVALID").unwrap();
            }
            Contents::HardLink => {
                fs::hard_link(root.join("asset"), root.join("alias")).unwrap();
            }
            Contents::SymlinkDirectory => {
                fs::write(outside.join("large"), [0; 200]).unwrap();
                symlink(&*outside, root.join("link")).unwrap();
            }
            Contents::EmptyDirectory => {
                fs::create_dir(root.join("empty")).unwrap();
            }
            Contents::Fifo => {
                assert!(
                    std::process::Command::new("mkfifo")
                        .arg(root.join("pipe"))
                        .status()
                        .unwrap()
                        .success(),
                    "FIFO fixture must be created"
                );
            }
            Contents::Unreadable => {
                let path = root.join("private");
                fs::create_dir(&path).unwrap();
                _restore = RestoreMode(path.clone());
                fs::set_permissions(&path, fs::Permissions::from_mode(0o0)).unwrap();
            }
        }
        let dir = Dir::open_ambient_dir(&*root, ambient_authority()).unwrap();
        inventory(&dir)
    }

    #[derive(Clone, Copy)]
    enum Scope {
        Same,
        Alias,
        Other,
        Escaped,
        ParentManaged,
        ChildManaged,
        ChildDestination,
        SiblingDestination,
    }
    fn scope(which: Scope) -> Result<(), StaticStorageError> {
        let root = temp_site_root("static_storage_scope");
        let child = root.join("child");
        fs::create_dir(&child).unwrap();
        let limits = StaticStorageLimits::new();
        match which {
            Scope::Same => {
                let storage = Storage::open(&root, limits)?;
                coordinate(&root, Some(&storage))?.admit(&root.join("page.html"), 1, 1)
            }
            Scope::Alias => {
                let storage = Storage::open(&child, limits)?;
                symlink(&child, root.join("alias")).unwrap();
                coordinate(&root.join("alias"), Some(&storage))?.admit(
                    &root.join("alias/page.html"),
                    1,
                    1,
                )
            }
            Scope::Other => {
                let storage = Storage::open(&child, limits)?;
                coordinate(&root, Some(&storage)).map(|_| ())
            }
            Scope::Escaped => {
                let storage = Storage::open(&child, limits)?;
                coordinate(&child, Some(&storage))?.admit(&root.join("page.html"), 1, 1)
            }
            Scope::ParentManaged => {
                Storage::open(&root, limits)?;
                Storage::open(&child, limits).map(|_| ())
            }
            Scope::ChildManaged => {
                Storage::open(&child, limits)?;
                Storage::open(&root, limits).map(|_| ())
            }
            Scope::ChildDestination => {
                Storage::open(&child, limits)?;
                coordinate(&root, None)?.admit(&child.join("new/page.html"), 1, 1)
            }
            Scope::SiblingDestination => {
                Storage::open(&child, limits)?;
                coordinate(&root, None)?.admit(&root.join("sibling/page.html"), 1, 1)
            }
        }
    }
    #[derive(Clone, Copy)]
    enum LockState {
        Released,
        Held,
        DefaultHolder,
        DefaultAfterPolicy,
    }
    /// Runs `attempt` on another thread while `held` is alive and reports
    /// whether it waited for the release rather than completing or refusing.
    fn attempt_while_held<T: Send + 'static>(
        held: impl Send + 'static,
        attempt: impl FnOnce() -> T + Send + 'static,
    ) -> (Attempt, T) {
        let (started, wait) = std::sync::mpsc::channel();
        let worker = std::thread::spawn(move || {
            started.send(()).unwrap();
            attempt()
        });
        wait.recv().unwrap();
        let waited = !worker.is_finished() && {
            std::thread::sleep(std::time::Duration::from_millis(300));
            !worker.is_finished()
        };
        drop(held);
        let result = worker.join().unwrap();
        (
            if waited {
                Attempt::WaitedForRelease
            } else {
                Attempt::Completed
            },
            result,
        )
    }
    /// The filesystem steps of one publication after admission, as the
    /// publisher performs them: locks, metadata directory and entry, HTML.
    fn publish_like_the_publisher(root: &Path, relative: &Path, html: &[u8], metadata: &[u8]) {
        let file = root.join(relative);
        let dir = file.parent().unwrap();
        let name = Path::new(file.file_name().unwrap());
        fs::create_dir_all(dir).unwrap();
        fs::write(dir.join(crate::static_routes::lock_name(name)), b"").unwrap();
        fs::write(dir.join(crate::static_routes::PUBLICATION_LOCK), b"").unwrap();
        fs::create_dir_all(dir.join(crate::static_routes::METADATA_DIRECTORY)).unwrap();
        fs::write(
            dir.join(crate::static_routes::metadata_name(name)),
            metadata,
        )
        .unwrap();
        fs::write(&file, html).unwrap();
    }
    #[derive(Clone, Copy, Debug)]
    enum Publication {
        Flat,
        Nested,
        Replacement,
        Remnant,
    }
    /// After commit the cached total must equal a fresh inventory, whatever the
    /// publication touched, and the next admission must use it.
    fn cached_inventory(publication: Publication) -> (bool, bool) {
        let root = temp_site_root("static_storage_cache");
        let relative = match publication {
            Publication::Nested => Path::new("a/b/page.html"),
            _ => Path::new("page.html"),
        };
        if matches!(publication, Publication::Replacement) {
            publish_like_the_publisher(&root, relative, &[0; 64], &[0; 16]);
        }
        let storage = Storage::open(&root, StaticStorageLimits::new()).unwrap();
        let guard = coordinate(&root, Some(&storage)).unwrap();
        guard.admit(&root.join(relative), 32, 8).unwrap();
        publish_like_the_publisher(&root, relative, &[1; 32], &[2; 8]);
        if matches!(publication, Publication::Remnant) {
            fs::write(
                root.join(relative)
                    .with_file_name(".leptos-remnant.tmp.1.1"),
                [0; 17],
            )
            .unwrap();
        }
        guard.commit();
        let generation = read_record(&mut guard.control_file())
            .unwrap()
            .unwrap()
            .generation;
        let cached = storage.cached(generation);
        let actual = inventory(guard.dir()).unwrap();
        (cached == Some(actual), cached.is_some())
    }
    /// Another cooperating process publishes between our admissions: its
    /// generation bump must retire our cached total before the next admission.
    fn foreign_publication() -> Result<(), StaticStorageError> {
        let root = temp_site_root("static_storage_foreign");
        let limits = StaticStorageLimits::new().with_logical_file_bytes(RECORD_LEN as u64 + 100);
        let ours = Storage::open(&root, limits).unwrap();
        let theirs = Storage::open(&root, limits).unwrap();
        {
            let guard = coordinate(&root, Some(&theirs)).unwrap();
            guard.admit(&root.join("page.html"), 60, 10).unwrap();
            publish_like_the_publisher(&root, Path::new("page.html"), &[1; 60], &[2; 10]);
            guard.commit();
        }
        coordinate(&root, Some(&ours))?.admit(&root.join("other.html"), 40, 0)
    }
    /// The site root sits below a directory that grants search but not read
    /// permission, as deployment trees owned by another account often do.
    fn unreadable_ancestor(bound: bool) -> Result<(), StaticStorageError> {
        let parent = temp_site_root("static_storage_ancestor");
        let deploy = parent.join("deploy");
        let site = deploy.join("site");
        fs::create_dir_all(&site).unwrap();
        let _restore = RestoreMode(deploy.clone());
        fs::set_permissions(&deploy, fs::Permissions::from_mode(0o311)).unwrap();
        let limits = StaticStorageLimits::new();
        if bound {
            let storage = Storage::open(&site, limits)?;
            coordinate(&site, Some(&storage))?.admit(&site.join("page.html"), 1, 1)
        } else {
            coordinate(&site, None)?.admit(&site.join("page.html"), 1, 1)
        }
    }

    #[derive(Debug, PartialEq)]
    enum Attempt {
        WaitedForRelease,
        Completed,
    }
    fn coordinated(state: LockState) -> (Attempt, Result<(), StaticStorageError>) {
        let root = temp_site_root("static_storage_coordination");
        let limits = StaticStorageLimits::new();
        if matches!(state, LockState::DefaultHolder) {
            let held = coordinate(&root, None).unwrap();
            let owned = root.clone();
            return attempt_while_held(held, move || Storage::open(&owned, limits).map(|_| ()));
        }
        let storage = Storage::open(&root, limits).unwrap();
        if matches!(state, LockState::DefaultAfterPolicy) {
            return (Attempt::Completed, coordinate(&root, None).map(|_| ()));
        }
        let held = coordinate(&root, Some(&storage)).unwrap();
        if matches!(state, LockState::Released) {
            drop(held);
            return (
                Attempt::Completed,
                coordinate(&root, Some(&storage)).map(|_| ()),
            );
        }
        let owned = root.clone();
        attempt_while_held(held, move || coordinate(&owned, Some(&storage)).map(|_| ()))
    }

    fn missing_managed_root(default: bool) -> (Result<(), StaticStorageError>, bool) {
        let root = temp_site_root("static_storage_missing_root");
        Storage::open(&root, StaticStorageLimits::new().with_namespace_entries(1)).unwrap();
        let missing = root.join("new/child");
        let result = if default {
            coordinate(&missing, None).map(|_| ())
        } else {
            Storage::open(&missing, StaticStorageLimits::new()).map(|_| ())
        };
        (result, !root.join("new").exists())
    }

    fn duplicated_control(managed: bool) -> (Attempt, Result<(), StaticStorageError>) {
        let root = temp_site_root("static_storage_control_duplicate");
        let limits = StaticStorageLimits::new();
        let storage = managed.then(|| Storage::open(&root, limits).unwrap());
        let guard = coordinate(&root, storage.as_ref()).unwrap();
        // A dup and an inherited descriptor refer to the same open file
        // description. Keep that extra reference beyond the logical lease.
        let duplicate = guard.control_file();
        let owned = root.clone();
        let (attempt, result) = attempt_while_held(guard, move || match &storage {
            Some(storage) => coordinate(&owned, Some(storage)).map(|_| ()),
            None => Storage::open(&owned, limits).map(|_| ()),
        });
        drop(duplicate);
        (attempt, result)
    }

    fn hardlinked_control() -> (Result<(), StaticStorageError>, u64, u64) {
        let root = temp_site_root("static_storage_control_hardlink");
        fs::write(root.join(CONTROL), "").unwrap();
        fs::hard_link(root.join(CONTROL), root.join("alias")).unwrap();
        let result = Storage::open(
            &root,
            StaticStorageLimits::new().with_logical_file_bytes(32),
        )
        .map(|_| ());
        (
            result,
            fs::metadata(root.join(CONTROL)).unwrap().len(),
            fs::metadata(root.join("alias")).unwrap().len(),
        )
    }

    #[derive(Clone, Copy)]
    enum ControlEntry {
        Symlink,
        Directory,
        Fifo,
    }
    fn unsafe_control(entry: ControlEntry) -> (Result<(), StaticStorageError>, Vec<u8>) {
        let root = temp_site_root("static_storage_control");
        let outside = temp_site_root("static_storage_control_outside");
        fs::write(outside.join("marker"), "MARKER").unwrap();
        match entry {
            ControlEntry::Symlink => symlink(outside.join("marker"), root.join(CONTROL)).unwrap(),
            ControlEntry::Directory => fs::create_dir(root.join(CONTROL)).unwrap(),
            ControlEntry::Fifo => assert!(
                std::process::Command::new("mkfifo")
                    .arg(root.join(CONTROL))
                    .status()
                    .unwrap()
                    .success()
            ),
        }
        (
            Storage::open(&root, StaticStorageLimits::new()).map(|_| ()),
            fs::read(outside.join("marker")).unwrap(),
        )
    }

    struct ChildProcess(std::process::Child);
    impl Drop for ChildProcess {
        fn drop(&mut self) {
            let _ = self.0.kill();
            let _ = self.0.wait();
        }
    }
    fn process_lock(crash: bool) -> (Attempt, Result<(), StaticStorageError>, Usage) {
        use std::io::BufRead;
        const ROOT_ENV: &str = "LEPTOS_STORAGE_PROCESS_ROOT";
        const CRASH_ENV: &str = "LEPTOS_STORAGE_PROCESS_CRASH";
        let limits =
            StaticStorageLimits::new().with_logical_file_bytes(RECORD_LEN as u64 + 64 + 16);
        if let Some(root) = std::env::var_os(ROOT_ENV) {
            let root = PathBuf::from(root);
            let storage = Storage::open(&root, limits).unwrap();
            let guard = coordinate(&root, Some(&storage)).unwrap();
            if std::env::var_os(CRASH_ENV).is_some() {
                // A cooperating publisher stages only after admission; the
                // remnant therefore appears behind an announced publication.
                guard.admit(&root.join("page.html"), 64, 16).unwrap();
                fs::write(root.join(".abandoned-publication"), [0; 17]).unwrap();
            }
            println!("STORAGE_HELD");
            io::stdout().flush().unwrap();
            let mut release = [0];
            io::stdin().read_exact(&mut release).unwrap();
            drop(guard);
            std::process::exit(0);
        }
        let root = temp_site_root("static_storage_process");
        let storage = Storage::open(&root, limits).unwrap();
        let mut command = std::process::Command::new(std::env::current_exe().unwrap());
        command
            .arg(std::thread::current().name().unwrap())
            .args(["--exact", "--nocapture"])
            .env(ROOT_ENV, &*root)
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::inherit());
        if crash {
            command.env(CRASH_ENV, "1");
        }
        let mut child = ChildProcess(command.spawn().unwrap());
        let output = child.0.stdout.take().unwrap();
        let (sender, receiver) = std::sync::mpsc::sync_channel(1);
        let reader = std::thread::spawn(move || {
            let mut held = false;
            for line in io::BufReader::new(output).lines() {
                match line {
                    Ok(line) if line.ends_with("STORAGE_HELD") => {
                        held = true;
                        break;
                    }
                    Ok(_) => {}
                    Err(_) => break,
                }
            }
            let _ = sender.send(held);
        });
        let held = receiver.recv_timeout(std::time::Duration::from_secs(20));
        if !matches!(held, Ok(true)) {
            let _ = child.0.kill();
            let _ = child.0.wait();
            reader.join().unwrap();
            panic!("child did not reach the held-lock event: {held:?}");
        }
        reader.join().unwrap();
        struct Release {
            child: Option<ChildProcess>,
            crash: bool,
        }
        impl Drop for Release {
            fn drop(&mut self) {
                let mut child = self.child.take().unwrap();
                if self.crash {
                    child.0.kill().unwrap();
                } else {
                    child.0.stdin.take().unwrap().write_all(&[1]).unwrap();
                }
                let status = child.0.wait().unwrap();
                assert!(
                    self.crash || status.success(),
                    "released worker failed: {status}"
                );
            }
        }
        let release = Release {
            child: Some(child),
            crash,
        };
        let owned = root.clone();
        let competing = storage.clone();
        let (during, result) = attempt_while_held(release, move || {
            coordinate(&owned, Some(&competing)).map(|_| ())
        });
        let guard = coordinate(&root, Some(&storage)).unwrap();
        let after = result.and_then(|()| guard.admit(&root.join("page.html"), 64, 16));
        (during, after, inventory(guard.dir()).unwrap())
    }

    lets_expect! {
        expect(duplicated_control(managed)) as duplicated_storage_control {
            let managed = false;
            to releases_the_shared_lease_before_the_duplicate_closes { equal((Attempt::Completed, Err(StaticStorageError::Busy))) }
            when the_lease_is_exclusive { let managed = true; to releases_the_exclusive_lease_before_the_duplicate_closes { equal((Attempt::WaitedForRelease, Ok(()))) } }
        }
        expect(hardlinked_control()) as hardlinked_policy_control {
            to refuses_before_expanding_each_named_file { equal((Err(StaticStorageError::InvalidPolicy), 0, 0)) }
        }
        expect(unsafe_control(entry)) as nonregular_policy_control {
            let entry = ControlEntry::Symlink;
            to refuses_the_symlink_and_preserves_its_target { equal((Err(StaticStorageError::InvalidPolicy), "MARKER".as_bytes().to_vec())) }
            when the_control_name_is_a_directory { let entry = ControlEntry::Directory; to refuses_a_directory { equal((Err(StaticStorageError::InvalidPolicy), "MARKER".as_bytes().to_vec())) } }
            when the_control_name_is_a_fifo { let entry = ControlEntry::Fifo; to refuses_without_waiting_for_a_writer { equal((Err(StaticStorageError::InvalidPolicy), "MARKER".as_bytes().to_vec())) } }
        }
        expect(process_lock(crash)) as cooperating_storage_process {
            let crash = false;
            to waits_for_the_competing_publication_before_admitting { equal((Attempt::WaitedForRelease, Ok(()), Usage { bytes: RECORD_LEN as u64, entries: 1 })) }
            when the_publisher_is_killed_after_staging {
                let crash = true;
                to releases_the_lock_and_charges_the_remnant { equal((Attempt::WaitedForRelease, Err(exceeded("logical file bytes", RECORD_LEN as u64 + 80, RECORD_LEN as u64 + 97)), Usage { bytes: RECORD_LEN as u64 + 17, entries: 2 })) }
            }
        }
        expect(installed()) as opening_static_storage {
            to persists_the_policy_and_counts_its_record { equal((StaticStorageLimits::new().with_logical_file_bytes(RECORD_LEN as u64).with_namespace_entries(1), RECORD_LEN as u64, 1)) }
        }
        expect(reopen(changed)) as reopening_static_storage {
            let changed = false;
            to preserves_the_control_inode_and_record { equal((Ok(()), true, encode(StaticStorageLimits::new().with_logical_file_bytes(100)).to_vec())) }
            when the_policy_differs {
                let changed = true;
                to refuses_without_rewriting { equal((Err(StaticStorageError::PolicyMismatch), true, encode(StaticStorageLimits::new().with_logical_file_bytes(100)).to_vec())) }
            }
        }
        expect(invalid_existing()) as partial_storage_policy {
            to refuses_without_repairing_the_record { equal((Err(StaticStorageError::InvalidPolicy), "LNTX".as_bytes().to_vec())) }
        }
        expect(bootstrap(limits, asset)) as storage_policy_bootstrap {
            let limits = StaticStorageLimits::new().with_logical_file_bytes(RECORD_LEN as u64 - 1);
            let asset = false;
            to refuses_a_record_larger_than_the_byte_budget { equal((Err(exceeded("logical file bytes", RECORD_LEN as u64 - 1, RECORD_LEN as u64)), 0, None)) }
            when the_entry_budget_is_zero {
                let limits = StaticStorageLimits::new().with_namespace_entries(0);
                to charges_the_control_entry { equal((Err(exceeded("namespace entries", 0, 1)), 0, None)) }
            }
            when existing_assets_exceed_the_budget { let limits = StaticStorageLimits::new().with_logical_file_bytes(RECORD_LEN as u64 + 4); let asset = true; to preserves_assets_and_refuses_installation { equal((Err(exceeded("logical file bytes", RECORD_LEN as u64 + 4, RECORD_LEN as u64 + 5)), 0, Some("ASSET".as_bytes().to_vec()))) } }
        }
        expect(byte_admission(limit, seed, html)) as static_storage_byte_admission {
            let limit = Some(RECORD_LEN as u64 + 80);
            let seed = Seed::Empty;
            let html = 64;
            to accepts_the_exact_peak_without_writing { equal((Ok(()), true)) }
            when one_byte_is_missing {
                let limit = Some(RECORD_LEN as u64 + 79);
                to refuses_before_staging { equal((Err(exceeded("logical file bytes", RECORD_LEN as u64 + 79, RECORD_LEN as u64 + 80)), true)) }
            }
            when the_budget_is_unlimited {
                let limit = None;
                to accepts_the_peak { equal((Ok(()), true)) }
            }
            when an_old_pair_will_be_replaced {
                let seed = Seed::Replaced;
                to keeps_old_bytes_in_the_peak { equal((Err(exceeded("logical file bytes", RECORD_LEN as u64 + 80, RECORD_LEN as u64 + 160)), true)) }
            }
            when a_crash_left_a_temporary_file {
                let seed = Seed::Orphan;
                to charges_the_remnant { equal((Err(exceeded("logical file bytes", RECORD_LEN as u64 + 80, RECORD_LEN as u64 + 97)), true)) }
            }
            when accounting_would_overflow {
                let html = u64::MAX;
                to refuses_overflow_without_writing { equal((Err(StaticStorageError::Overflow), true)) }
            }
        }
        expect(entry_admission(limit, layout)) as static_storage_entry_admission {
            let limit = 6;
            let layout = Layout::Short;
            to accepts_two_locks_the_metadata_directory_and_two_staging_entries { equal((Ok(()), true)) }
            when one_entry_is_missing {
                let limit = 5;
                to refuses_before_creating_entries { equal((Err(exceeded("namespace entries", 5, 6)), true)) }
            }
            when two_parent_directories_are_missing {
                let layout = Layout::Nested;
                to charges_both_directories { equal((Err(exceeded("namespace entries", 6, 8)), true)) }
            }
            when the_name_reaches_the_filename_limit { let layout = Layout::Long; to charges_no_extra_entry { equal((Ok(()), true)) } }
            when locks_already_exist {
                let layout = Layout::ExistingLocks;
                to counts_only_new_staging_entries { equal((Ok(()), true)) }
            }
            when an_old_pair_will_be_replaced {
                let layout = Layout::Replacement;
                to keeps_two_new_staging_entries_in_the_peak { equal((Err(exceeded("namespace entries", 6, 8)), true)) }
            }
        }
        expect(scanned(contents)) as storage_inventory {
            let contents = Contents::Ordinary;
            to counts_assets_invalid_metadata_and_remnants { equal(Ok(Usage { bytes: 14, entries: 3 })) }
            when a_file_has_another_hard_link {
                let contents = Contents::HardLink;
                to charges_each_named_file { equal(Ok(Usage { bytes: 10, entries: 2 })) }
            }
            when a_symlink_targets_a_directory {
                let contents = Contents::SymlinkDirectory;
                to charges_the_link_without_traversing_it { equal(Ok(Usage { bytes: 5, entries: 2 })) }
            }
            when a_directory_is_empty {
                let contents = Contents::EmptyDirectory;
                to charges_the_directory_entry { equal(Ok(Usage { bytes: 5, entries: 2 })) }
            }
            when a_fifo_is_present {
                let contents = Contents::Fifo;
                to counts_without_opening_the_fifo { equal(Ok(Usage { bytes: 5, entries: 2 })) }
            }
            when a_directory_cannot_be_read {
                let contents = Contents::Unreadable;
                to propagates_the_inventory_error { have_io_kind(io::ErrorKind::PermissionDenied) }
            }
        }
        expect(scope(which)) as static_storage_scope {
            let which = Scope::Same;
            to admits_the_bound_root { equal(Ok(())) }
            when a_symlink_alias_names_the_root { let which = Scope::Alias; to admits_the_same_physical_root { equal(Ok(())) } }
            when a_different_root_is_supplied {
                let which = Scope::Other;
                to refuses_the_other_identity { equal(Err(StaticStorageError::UnsupportedRoot)) }
            }
            when the_destination_escapes_the_root {
                let which = Scope::Escaped;
                to preserves_the_capability_boundary { have_io_kind(io::ErrorKind::PermissionDenied) }
            }
            when an_ancestor_is_managed {
                let which = Scope::ParentManaged;
                to refuses_a_nested_scope { equal(Err(StaticStorageError::UnsupportedRoot)) }
            }
            when a_descendant_is_managed {
                let which = Scope::ChildManaged;
                to refuses_an_overlapping_scope { equal(Err(StaticStorageError::UnsupportedRoot)) }
            }
            when an_unlimited_writer_enters_a_managed_child {
                let which = Scope::ChildDestination;
                to requires_the_installed_binding { equal(Err(StaticStorageError::BindingRequired)) }
            }
            when an_unlimited_writer_uses_an_unmanaged_sibling {
                let which = Scope::SiblingDestination;
                to admits_the_disjoint_destination { equal(Ok(())) }
            }
        }
        expect(cached_inventory(publication)) as cached_storage_inventory {
            let publication = Publication::Flat;
            to equals_a_fresh_scan_after_commit { equal((true, true)) }
            when publication_creates_parent_directories { let publication = Publication::Nested; to equals_a_fresh_scan_after_commit { equal((true, true)) } }
            when publication_replaces_an_old_pair { let publication = Publication::Replacement; to equals_a_fresh_scan_after_commit { equal((true, true)) } }
            when a_remnant_is_left_beside_the_artifact { let publication = Publication::Remnant; to equals_a_fresh_scan_after_commit { equal((true, true)) } }
        }
        expect(foreign_publication()) as admission_after_a_foreign_publication {
            to charges_the_bytes_the_other_process_published { equal(Err(exceeded("logical file bytes", RECORD_LEN as u64 + 100, RECORD_LEN as u64 + 110))) }
        }
        expect(unreadable_ancestor(bound)) as site_root_below_an_unreadable_ancestor {
            let bound = true;
            to installs_and_admits { equal(Ok(())) }
            when the_publisher_is_unlimited { let bound = false; to admits { equal(Ok(())) } }
        }
        expect(coordinated(state)) as storage_coordination {
            let state = LockState::Released;
            to admits_after_guard_release { equal((Attempt::Completed, Ok(()))) }
            when a_managed_guard_is_alive { let state = LockState::Held; to waits_for_the_competing_publication { equal((Attempt::WaitedForRelease, Ok(()))) } }
            when an_unlimited_guard_predates_installation {
                let state = LockState::DefaultHolder;
                to refuses_installation_without_waiting { equal((Attempt::Completed, Err(StaticStorageError::Busy))) }
            }
            when an_unlimited_publisher_follows_installation {
                let state = LockState::DefaultAfterPolicy;
                to requires_binding { equal((Attempt::Completed, Err(StaticStorageError::BindingRequired))) }
            }
        }
        expect(missing_managed_root(default)) as missing_root_inside_managed_storage {
            let default = false;
            to refuses_installation_before_creating_directories { equal((Err(StaticStorageError::UnsupportedRoot), true)) }
            when the_publisher_has_no_binding { let default = true; to refuses_publication_before_creating_directories { equal((Err(StaticStorageError::BindingRequired), true)) } }
        }
    }
}

lets_expect! {
    expect((limits.logical_file_bytes(), limits.namespace_entries())) as static_storage_limits {
        let limits = StaticStorageLimits::new().with_logical_file_bytes(64).with_namespace_entries(8);
        to exposes_the_independent_finite_bounds { equal((Some(64), Some(8))) }
        when no_bounds_are_configured { let limits = StaticStorageLimits::default(); to exposes_unlimited_defaults { equal((None, None)) } }
        when the_bounds_are_zero { let limits = StaticStorageLimits::new().with_logical_file_bytes(0).with_namespace_entries(0); to exposes_zero_capacity { equal((Some(0), Some(0))) } }
        when the_bounds_are_maximal { let limits = StaticStorageLimits::new().with_logical_file_bytes(u64::MAX).with_namespace_entries(u64::MAX); to exposes_the_integer_boundary { equal((Some(u64::MAX), Some(u64::MAX))) } }
    }

    expect(record(limits, damage)) as stored_policy_record {
        let limits = StaticStorageLimits::new().with_logical_file_bytes(64).with_namespace_entries(8);
        let damage = Damage::None;
        to preserves_finite_limits { equal(Ok(limits)) }
        when limits_are_absent {
            let limits = StaticStorageLimits::new();
            to preserves_unlimited_fields { equal(Ok(limits)) }
        }
        when limits_are_zero {
            let limits = StaticStorageLimits::new().with_logical_file_bytes(0).with_namespace_entries(0);
            to preserves_zero_as_a_real_bound { equal(Ok(limits)) }
        }
        when limits_are_maximal {
            let limits = StaticStorageLimits::new().with_logical_file_bytes(u64::MAX).with_namespace_entries(u64::MAX);
            to preserves_the_full_integer_range { equal(Ok(limits)) }
        }
        when the_record_is_partial { let damage = Damage::Partial; to rejects_partial_publication { equal(Err(StaticStorageError::InvalidPolicy)) } }
        when the_version_is_unknown { let damage = Damage::Magic; to rejects_unknown_protocol { equal(Err(StaticStorageError::InvalidPolicy)) } }
        when a_presence_flag_is_invalid { let damage = Damage::Flag; to rejects_noncanonical_flags { equal(Err(StaticStorageError::InvalidPolicy)) } }
        when reserved_bytes_are_nonzero { let damage = Damage::Reserved; to rejects_unknown_record_fields { equal(Err(StaticStorageError::InvalidPolicy)) } }
        when the_record_has_trailing_bytes { let damage = Damage::Trailing; to rejects_trailing_data { equal(Err(StaticStorageError::InvalidPolicy)) } }
        when an_absent_limit_carries_a_value { let damage = Damage::AbsentValue; to rejects_ambiguous_limits { equal(Err(StaticStorageError::InvalidPolicy)) } }
    }
}
