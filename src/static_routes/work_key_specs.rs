use super::*;
use lets_expect::*;
use std::os::unix::fs::{MetadataExt, symlink};

#[derive(Clone, Copy, Debug)]
enum Entry {
    Regular,
    Symlink,
    Hardlinked,
    Missing,
}

#[derive(Clone, Copy, Debug)]
enum Spelling {
    Exact,
    AsciiCase,
    DecomposedUnicode,
}

/// Whether the filesystem under test resolves the alias to the stored entry.
/// Measured on the fixture itself, never assumed from the platform.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum NativeNames {
    Equivalent,
    Distinct,
}

#[derive(Debug)]
struct Observation {
    names: NativeNames,
    canonical_dir: PathBuf,
    stored: &'static str,
    stored_key: StaticWorkKey,
    alias_key: StaticWorkKey,
    other_key: Option<StaticWorkKey>,
    target_key: StaticWorkKey,
}

struct Site {
    _root: crate::tests::TempSiteRoot,
    dir: PathBuf,
}

fn site(seeded: bool) -> Site {
    let root = crate::tests::temp_site_root("static_work_key");
    let dir = root.join("site");
    fs::create_dir(&dir).unwrap();
    if seeded {
        fs::write(dir.join("seed.html"), "seed").unwrap();
    }
    Site { _root: root, dir }
}

fn spellings(spelling: Spelling) -> (&'static str, &'static str) {
    match spelling {
        Spelling::Exact => ("Stored.html", "Stored.html"),
        Spelling::AsciiCase => ("Stored.html", "stored.html"),
        Spelling::DecomposedUnicode => ("caf\u{e9}.html", "cafe\u{301}.html"),
    }
}

fn same_entry(dir: &Path, stored: &str, alias: &str) -> NativeNames {
    let first = fs::symlink_metadata(dir.join(stored)).unwrap();
    match fs::symlink_metadata(dir.join(alias)) {
        Ok(second) if (second.dev(), second.ino()) == (first.dev(), first.ino()) => {
            NativeNames::Equivalent
        }
        Ok(_) => NativeNames::Distinct,
        Err(error) if error.kind() == io::ErrorKind::NotFound => NativeNames::Distinct,
        Err(error) => panic!("native lookup fixture failed: {error}"),
    }
}

fn observe(entry: Entry, spelling: Spelling, seeded: bool) -> Observation {
    let site = site(seeded);
    let dir = &site.dir;
    let (stored, alias) = spellings(spelling);
    if !seeded {
        assert!(
            matches!(entry, Entry::Missing),
            "only missing entries observe an empty directory"
        );
    }
    let names = match entry {
        Entry::Regular => {
            fs::write(dir.join(stored), "stored").unwrap();
            same_entry(dir, stored, alias)
        }
        Entry::Symlink => {
            symlink("seed.html", dir.join(stored)).unwrap();
            symlink("seed.html", dir.join("Other.html")).unwrap();
            same_entry(dir, stored, alias)
        }
        Entry::Hardlinked => {
            symlink("seed.html", dir.join(stored)).unwrap();
            fs::hard_link(dir.join(stored), dir.join("Other.html")).unwrap();
            assert!(
                fs::symlink_metadata(dir.join("Other.html"))
                    .unwrap()
                    .is_symlink()
            );
            same_entry(dir, stored, alias)
        }
        Entry::Missing => {
            // The rule is measured on a short-lived probe and every probe
            // entry is removed before the subject observes the directory.
            fs::write(dir.join(stored), "probe").unwrap();
            let names = same_entry(dir, stored, alias);
            fs::remove_file(dir.join(stored)).unwrap();
            for name in [stored, alias] {
                assert_eq!(
                    fs::symlink_metadata(dir.join(name)).unwrap_err().kind(),
                    io::ErrorKind::NotFound
                );
            }
            names
        }
    };
    Observation {
        names,
        canonical_dir: dir.canonicalize().unwrap(),
        stored,
        stored_key: static_work_key(&dir.join(stored)),
        alias_key: static_work_key(&dir.join(alias)),
        other_key: matches!(entry, Entry::Symlink | Entry::Hardlinked)
            .then(|| static_work_key(&dir.join("Other.html"))),
        target_key: static_work_key(&dir.join("seed.html")),
    }
}

#[derive(Debug)]
struct Replacement {
    names: NativeNames,
    before: StaticWorkKey,
    alias_after: StaticWorkKey,
    stored_after: StaticWorkKey,
}

fn observe_replacement(spelling: Spelling, adopts_published_spelling: bool) -> Replacement {
    let site = site(true);
    let dir = &site.dir;
    let (stored, alias) = spellings(spelling);
    symlink("seed.html", dir.join(stored)).unwrap();
    let names = same_entry(dir, stored, alias);
    let before = static_work_key(&dir.join(alias));
    // Publication prepares a temporary file and renames it over the entry
    // under the requested spelling.
    fs::write(dir.join(".leptos-temp.tmp.1.1"), "published").unwrap();
    fs::rename(dir.join(".leptos-temp.tmp.1.1"), dir.join(alias)).unwrap();
    assert!(fs::symlink_metadata(dir.join(alias)).unwrap().is_file());
    if adopts_published_spelling {
        // A filesystem may store the replacing name instead of keeping the
        // replaced one. Give the regular entry the published spelling.
        fs::rename(dir.join(alias), dir.join(".leptos-temp.tmp.1.2")).unwrap();
        fs::rename(dir.join(".leptos-temp.tmp.1.2"), dir.join(alias)).unwrap();
        let listed = fs::read_dir(dir)
            .unwrap()
            .filter_map(Result::ok)
            .map(|entry| entry.file_name())
            .find(|name| name == alias);
        assert_eq!(listed.as_deref(), Some(std::ffi::OsStr::new(alias)));
    }
    Replacement {
        names,
        before,
        alias_after: static_work_key(&dir.join(alias)),
        stored_after: static_work_key(&dir.join(stored)),
    }
}

fn observe_non_directory_parent() -> (PathBuf, StaticWorkKey) {
    let site = site(true);
    let parent = site.dir.join("seed.html");
    let requested = parent.join("child.html");
    (parent.canonicalize().unwrap(), static_work_key(&requested))
}

fn be_anchored_in_the_directory(actual: &Observation) -> AssertionResult {
    let key = &actual.stored_key;
    let anchored = key.entry.parent() == Some(actual.canonical_dir.as_path())
        && key
            .entry
            .file_name()
            .is_some_and(|name| name.eq_ignore_ascii_case(actual.stored))
        && key.published.is_none();
    if anchored {
        Ok(())
    } else {
        Err(AssertionError::new(vec![format!(
            "expected the key of {} anchored in {}; actual {actual:?}",
            actual.stored,
            actual.canonical_dir.display()
        )]))
    }
}

fn respect_native_names(actual: &Observation) -> AssertionResult {
    let shared = actual.alias_key.entry == actual.stored_key.entry;
    let expected = match actual.names {
        NativeNames::Equivalent => shared,
        NativeNames::Distinct => !shared,
    };
    if expected {
        Ok(())
    } else {
        Err(AssertionError::new(vec![format!(
            "expected {:?} native names to give {} work key; actual {actual:?}",
            actual.names,
            if shared { "distinct" } else { "one" }
        )]))
    }
}

fn not_follow_the_link(actual: &Observation) -> AssertionResult {
    if actual.stored_key.entry != actual.target_key.entry {
        Ok(())
    } else {
        Err(AssertionError::new(vec![format!(
            "expected the symlink entry to be keyed apart from its target; actual {actual:?}"
        )]))
    }
}

fn keep_other_entry_distinct(actual: &Observation) -> AssertionResult {
    let other = actual
        .other_key
        .as_ref()
        .expect("fixture creates Other.html");
    if other.entry != actual.stored_key.entry && other.entry != actual.alias_key.entry {
        Ok(())
    } else {
        Err(AssertionError::new(vec![format!(
            "expected Other.html keyed apart from the requested entry; actual {actual:?}"
        )]))
    }
}

fn keep_the_key_stable(actual: &Replacement) -> AssertionResult {
    let shares = |after: &StaticWorkKey| {
        actual
            .before
            .keys()
            .any(|key| after.keys().any(|after| after == key))
    };
    let expected = match actual.names {
        NativeNames::Equivalent => shares(&actual.alias_after) && shares(&actual.stored_after),
        NativeNames::Distinct => shares(&actual.alias_after) && !shares(&actual.stored_after),
    };
    if expected {
        Ok(())
    } else {
        Err(AssertionError::new(vec![format!(
            "expected {:?} names to keep the pending work reachable exactly through its own entry after replacement; actual {actual:?}",
            actual.names
        )]))
    }
}

fn keep_the_requested_name_under_the_parent(actual: &(PathBuf, StaticWorkKey)) -> AssertionResult {
    let (parent, key) = actual;
    if key.entry == parent.join("child.html") && key.published.is_none() {
        Ok(())
    } else {
        Err(AssertionError::new(vec![format!(
            "expected the requested name under its resolved parent as best-effort key; actual {actual:?}"
        )]))
    }
}

lets_expect! {
    expect(observe(entry, spelling, seeded)) as static_work_entry_key {
        let entry = Entry::Regular;
        let spelling = Spelling::Exact;
        let seeded = true;
        to identifies_the_entry_by_its_stored_spelling { be_anchored_in_the_directory }
        when request_uses_ascii_case_alias {
            let spelling = Spelling::AsciiCase;
            to keys_one_entry_only_where_the_directory_folds_case { respect_native_names }
        }
        when request_uses_decomposed_unicode {
            let spelling = Spelling::DecomposedUnicode;
            to keys_one_entry_only_where_the_directory_normalizes { respect_native_names }
        }
        when entry_is_a_symlink {
            let entry = Entry::Symlink;
            to keys_the_link_entry_apart_from_its_target {
                not_follow_the_link,
                keep_other_entry_distinct
            }
            when request_uses_ascii_case_alias {
                let spelling = Spelling::AsciiCase;
                to keys_one_entry_only_where_the_directory_folds_case {
                    respect_native_names,
                    keep_other_entry_distinct
                }
            }
            when request_uses_decomposed_unicode {
                let spelling = Spelling::DecomposedUnicode;
                to keys_one_entry_only_where_the_directory_normalizes {
                    respect_native_names,
                    keep_other_entry_distinct
                }
            }
        }
        when entries_are_hardlinked {
            let entry = Entry::Hardlinked;
            to keys_each_entry_by_its_exact_name { keep_other_entry_distinct }
            when request_uses_ascii_case_alias {
                let spelling = Spelling::AsciiCase;
                to keys_one_entry_only_where_the_directory_folds_case {
                    respect_native_names,
                    keep_other_entry_distinct
                }
            }
        }
        when entry_is_missing {
            let entry = Entry::Missing;
            let spelling = Spelling::AsciiCase;
            to keys_one_future_entry_only_where_the_directory_folds_case { respect_native_names }
            when the_directory_is_empty {
                let seeded = false;
                to observes_the_rule_from_the_parent_directory { respect_native_names }
            }
        }
    }
    expect(observe_replacement(spelling, adopts_published_spelling)) as static_work_entry_key_after_replacement {
        let spelling = Spelling::AsciiCase;
        let adopts_published_spelling = false;
        to keeps_the_key_of_the_pending_work { keep_the_key_stable }
        when the_filesystem_adopts_the_published_spelling {
            let adopts_published_spelling = true;
            to keeps_the_key_of_the_pending_work { keep_the_key_stable }
        }
        when request_uses_decomposed_unicode {
            let spelling = Spelling::DecomposedUnicode;
            to keeps_the_key_of_the_pending_work { keep_the_key_stable }
            when the_filesystem_adopts_the_published_spelling {
                let adopts_published_spelling = true;
                to keeps_the_key_of_the_pending_work { keep_the_key_stable }
            }
        }
    }
    expect(observe_non_directory_parent()) as static_work_entry_key_lookup_error {
        to keeps_the_requested_name_under_the_resolved_parent { keep_the_requested_name_under_the_parent }
    }
}
