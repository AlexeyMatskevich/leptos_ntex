//! Behavior when the reserved metadata directory or its entries are damaged.

use leptos::prelude::*;
use leptos_ntex_unofficial::{NtexRouteListing, ResponseOptions, register_leptos_routes};
use leptos_router::{Method, SsrMode, static_routes::StaticRoute};
use lets_expect::*;
use ntex::{
    http::{StatusCode, header},
    web::{App, test},
};
use std::{
    path::PathBuf,
    sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    },
};

const METADATA_DIRECTORY: &str = ".leptos-static-metadata";

struct Root(PathBuf);
impl Root {
    fn new() -> Self {
        static NEXT: AtomicUsize = AtomicUsize::new(0);
        loop {
            let path = std::env::temp_dir().join(format!(
                "static_namespace_{}_{}",
                std::process::id(),
                NEXT.fetch_add(1, Ordering::Relaxed)
            ));
            match std::fs::create_dir(&path) {
                Ok(()) => return Self(path),
                Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {}
                Err(error) => panic!("create fixture: {error}"),
            }
        }
    }
}
impl Drop for Root {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}
#[derive(Clone, Copy)]
enum Obstruction {
    RegularParent,
    #[cfg(unix)]
    BrokenPrimarySymlink,
}
#[derive(Debug)]
struct Observation {
    status: StatusCode,
    body: Vec<u8>,
    saved_body: Option<Vec<u8>>,
    selected_header: Option<String>,
    render_count: usize,
    sentinel: String,
    html: Option<Vec<u8>>,
    temporary_count: usize,
    primary_is_regular: bool,
}
fn run(existing: bool, obstruction: Obstruction) -> Observation {
    ntex::rt::System::new(
        "static-namespace",
        leptos_ntex_unofficial::RequestRuntime::new(ntex::rt::DefaultRuntime),
    )
    .block_on(async move {
        let root = Root::new();
        let calls = Arc::new(AtomicUsize::new(0));
        let render_calls = calls.clone();
        let segment = "page";
        let uri = format!("/{segment}");
        let routes = vec![NtexRouteListing::new(
            uri.clone(),
            SsrMode::Static(StaticRoute::new()),
            [Method::Get],
            vec![],
        )];
        let options = LeptosOptions::builder()
            .output_name("static_namespace")
            .site_root(root.0.to_string_lossy().into_owned())
            .build();
        let app = test::init_service(App::new().state(options).configure(|cfg| {
            register_leptos_routes(cfg, routes, move || {
                render_calls.fetch_add(1, Ordering::SeqCst);
                let response = expect_context::<ResponseOptions>();
                response.set_status(StatusCode::CREATED);
                response.insert_header(
                    header::HeaderName::from_static("x-saved-representation"),
                    header::HeaderValue::from_static("captured"),
                );
                "public-namespace-body"
            });
        }))
        .await;
        let metadata_directory = root.0.join(METADATA_DIRECTORY);
        let saved_body = if existing {
            let response =
                test::call_service(&app, test::TestRequest::with_uri(&uri).to_request()).await;
            assert_eq!(
                response.status(),
                StatusCode::CREATED,
                "fixture must publish initial artifact"
            );
            Some(test::read_body(response).await.to_vec())
        } else {
            None
        };
        let sentinel_path = match obstruction {
            Obstruction::RegularParent => {
                let _ = std::fs::remove_dir_all(&metadata_directory);
                metadata_directory.clone()
            }
            #[cfg(unix)]
            Obstruction::BrokenPrimarySymlink => {
                let _ = std::fs::create_dir(&metadata_directory);
                let entry = metadata_directory.join(format!("{segment}.html"));
                let _ = std::fs::remove_file(&entry);
                std::os::unix::fs::symlink("obstruction/child", entry).unwrap();
                metadata_directory.join("obstruction")
            }
        };
        std::fs::write(&sentinel_path, "unrelated-sentinel").unwrap();
        calls.store(0, Ordering::SeqCst);
        let response =
            test::call_service(&app, test::TestRequest::with_uri(&uri).to_request()).await;
        let status = response.status();
        let selected_header = response
            .headers()
            .get("x-saved-representation")
            .map(|value| value.to_str().unwrap().to_owned());
        let body = test::read_body(response).await.to_vec();
        let html = match std::fs::read(root.0.join(format!("{segment}.html"))) {
            Ok(contents) => Some(contents),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => None,
            Err(error) => panic!("read published HTML: {error}"),
        };
        let sentinel = std::fs::read_to_string(&sentinel_path).unwrap();
        let primary_is_regular =
            std::fs::symlink_metadata(metadata_directory.join(format!("{segment}.html")))
                .is_ok_and(|metadata| metadata.is_file());
        let temporary_count = std::fs::read_dir(&root.0)
            .unwrap()
            .map(|entry| entry.unwrap().file_name())
            .filter(|name| name.to_string_lossy().contains(".tmp."))
            .count();
        Observation {
            status,
            body,
            saved_body,
            selected_header,
            render_count: calls.load(Ordering::SeqCst),
            sentinel,
            html,
            temporary_count,
            primary_is_regular,
        }
    })
}
fn preserves_resources(actual: &Observation) -> bool {
    actual.sentinel == "unrelated-sentinel" && actual.temporary_count == 0
}
/// A regular file at the reserved directory name is a damaged deployment:
/// the saved HTML is not served with guessed metadata, regeneration cannot
/// publish, and the unrelated file is left untouched.
fn refuses_publication(actual: &Observation) -> AssertionResult {
    if actual.status == StatusCode::INTERNAL_SERVER_ERROR
        && actual.body.is_empty()
        && actual.selected_header.is_none()
        && actual.html == actual.saved_body
        && actual.render_count == 1
        && preserves_resources(actual)
    {
        Ok(())
    } else {
        Err(AssertionError::new(vec![format!(
            "publication must fail without serving saved HTML, changing the unrelated file or retaining temps; {actual:?}"
        )]))
    }
}
#[cfg(unix)]
fn repairs_primary(actual: &Observation) -> AssertionResult {
    if actual.status == StatusCode::CREATED
        && actual.selected_header.as_deref() == Some("captured")
        && actual.body.starts_with(b"public-namespace-body")
        && actual.html.as_deref() == Some(actual.body.as_slice())
        && actual.render_count == 1
        && actual.primary_is_regular
        && preserves_resources(actual)
    {
        Ok(())
    } else {
        Err(AssertionError::new(vec![format!(
            "a broken metadata entry must trigger regeneration that replaces it; {actual:?}"
        )]))
    }
}
lets_expect! {
    expect(run(existing, Obstruction::RegularParent)) as regular_file_at_metadata_directory_name {
        let existing = true;
        to refuses_without_serving_the_saved_html { refuses_publication }
        when the_artifact_is_missing {
            let existing = false;
            to refuses_publication_without_changing_the_unrelated_file { refuses_publication }
        }
    }
}

#[cfg(unix)]
lets_expect! {
    expect(run(true, Obstruction::BrokenPrimarySymlink)) as metadata_entry_with_non_directory_symlink_target {
        to regenerates_and_replaces_the_entry { repairs_primary }
    }
}

#[cfg(unix)]
mod metadata_namespace {
    use super::*;
    use std::{
        fs,
        os::unix::fs::{PermissionsExt, symlink},
        path::Path,
    };

    #[derive(Clone, Copy, Debug)]
    enum Namespace {
        Present,
        FileLink,
        MissingEntry,
        MissingDirectory,
        DirectoryLink,
        EmptyDirectoryLink,
        DanglingEntry,
        DanglingDirectory,
        InvalidEntryTarget,
        InvalidDirectoryTarget,
    }

    #[derive(Clone, Copy)]
    enum RecoveryEnvironment {
        Writable,
        ReadOnly,
    }

    struct FixtureRoot(Root);
    impl Drop for FixtureRoot {
        fn drop(&mut self) {
            let _ = fs::set_permissions(&self.0.0, fs::Permissions::from_mode(0o755));
            let metadata = self.0.0.join(METADATA_DIRECTORY);
            if metadata.is_dir() {
                let _ = fs::set_permissions(metadata, fs::Permissions::from_mode(0o755));
            }
        }
    }

    struct ResponseObservation {
        namespace: Namespace,
        status: StatusCode,
        revision: Option<String>,
        body: Vec<u8>,
        saved_body: Vec<u8>,
        disk_body: Vec<u8>,
        renders: usize,
        selected_metadata_regular: bool,
        parent_is_link: bool,
        read_only: bool,
        write_denied: bool,
    }

    impl std::fmt::Debug for ResponseObservation {
        fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            f.debug_struct("ResponseObservation")
                .field("namespace", &self.namespace)
                .field("status", &self.status)
                .field("revision", &self.revision)
                .field("body", &String::from_utf8_lossy(&self.body))
                .field("body_matches_saved", &(self.body == self.saved_body))
                .field("disk_matches_saved", &(self.disk_body == self.saved_body))
                .field("disk_matches_response", &(self.disk_body == self.body))
                .field("renders", &self.renders)
                .field("selected_metadata_regular", &self.selected_metadata_regular)
                .field("parent_is_link", &self.parent_is_link)
                .field("read_only", &self.read_only)
                .field("write_denied", &self.write_denied)
                .finish()
        }
    }

    fn invalid_target_link(link: &Path) {
        let target = "t".repeat(256);
        symlink(&target, link).expect("fixture must create the metadata link");
        assert!(fs::symlink_metadata(link).unwrap().is_symlink());
        assert_eq!(fs::read_link(link).unwrap(), PathBuf::from(target));
        let parent = cap_std::fs::Dir::open_ambient_dir(
            link.parent().unwrap(),
            cap_std::ambient_authority(),
        )
        .unwrap();
        let error = parent
            .open(link.file_name().unwrap())
            .expect_err("target component must exceed this filesystem's limit");
        assert_eq!(
            error.kind(),
            std::io::ErrorKind::InvalidFilename,
            "fixture must reach the invalid-name outcome through cap_std: {error}"
        );
    }

    fn observe(namespace: Namespace, environment: RecoveryEnvironment) -> ResponseObservation {
        let read_only = matches!(environment, RecoveryEnvironment::ReadOnly);
        ntex::rt::System::new(
            "static-metadata-namespace",
            leptos_ntex_unofficial::RequestRuntime::new(ntex::rt::DefaultRuntime),
        )
        .block_on(async move {
            let root = FixtureRoot(Root::new());
            let path = &root.0.0;
            let segment = "page";
            let uri = format!("/{segment}");
            let filename = format!("{segment}.html");
            let calls = Arc::new(AtomicUsize::new(0));
            let render_calls = calls.clone();
            let routes = vec![NtexRouteListing::new(
                uri.clone(),
                SsrMode::Static(StaticRoute::new()),
                [Method::Get],
                vec![],
            )];
            let options = LeptosOptions::builder()
                .output_name("static_namespace")
                .site_root(path.to_string_lossy().into_owned())
                .build();
            let app = test::init_service(App::new().state(options).configure(|cfg| {
                register_leptos_routes(cfg, routes, move || {
                    let first = render_calls.fetch_add(1, Ordering::SeqCst) == 0;
                    let response = expect_context::<ResponseOptions>();
                    response.set_status(if first {
                        StatusCode::CREATED
                    } else {
                        StatusCode::NON_AUTHORITATIVE_INFORMATION
                    });
                    response.insert_header(
                        header::HeaderName::from_static("x-namespace-revision"),
                        header::HeaderValue::from_static(if first {
                            "saved"
                        } else {
                            "regenerated"
                        }),
                    );
                    if first {
                        "saved-namespace-body"
                    } else {
                        "regenerated-namespace-body"
                    }
                });
            }))
            .await;
            let initial =
                test::call_service(&app, test::TestRequest::with_uri(&uri).to_request()).await;
            assert_eq!(
                initial.status(),
                StatusCode::CREATED,
                "initial public fixture must publish"
            );
            let saved_body = test::read_body(initial).await.to_vec();
            let directory = path.join(METADATA_DIRECTORY);
            let entry = directory.join(&filename);
            assert!(entry.is_file(), "publication writes the metadata entry");
            match namespace {
                Namespace::Present => {}
                Namespace::FileLink => {
                    fs::rename(&entry, directory.join("saved-record")).unwrap();
                    symlink("saved-record", &entry).unwrap();
                }
                Namespace::MissingEntry => fs::remove_file(&entry).unwrap(),
                Namespace::MissingDirectory => fs::remove_dir_all(&directory).unwrap(),
                Namespace::DirectoryLink | Namespace::EmptyDirectoryLink => {
                    let actual = path.join("actual-metadata");
                    fs::rename(&directory, &actual).unwrap();
                    if matches!(namespace, Namespace::EmptyDirectoryLink) {
                        fs::remove_file(actual.join(&filename)).unwrap();
                    }
                    symlink("actual-metadata", &directory).unwrap();
                }
                Namespace::DanglingEntry => {
                    fs::remove_file(&entry).unwrap();
                    symlink("missing-record", &entry).unwrap();
                }
                Namespace::DanglingDirectory => {
                    fs::remove_dir_all(&directory).unwrap();
                    symlink("missing-directory", &directory).unwrap();
                }
                Namespace::InvalidEntryTarget => {
                    fs::remove_file(&entry).unwrap();
                    invalid_target_link(&entry);
                }
                Namespace::InvalidDirectoryTarget => {
                    fs::remove_dir_all(&directory).unwrap();
                    invalid_target_link(&directory);
                }
            }
            let write_denied = if read_only {
                if directory.is_dir() {
                    fs::set_permissions(&directory, fs::Permissions::from_mode(0o555)).unwrap();
                }
                fs::set_permissions(path, fs::Permissions::from_mode(0o555)).unwrap();
                let denied = fs::write(path.join("write-probe"), b"denied")
                    .is_err_and(|error| error.kind() == std::io::ErrorKind::PermissionDenied);
                assert!(denied, "fixture must reject writes before the subject");
                denied
            } else {
                false
            };
            let response =
                test::call_service(&app, test::TestRequest::with_uri(&uri).to_request()).await;
            let status = response.status();
            let revision = response
                .headers()
                .get("x-namespace-revision")
                .map(|value| value.to_str().unwrap().to_owned());
            let body = test::read_body(response).await.to_vec();
            ResponseObservation {
                namespace,
                status,
                revision,
                body,
                saved_body,
                disk_body: fs::read(path.join(&filename)).unwrap(),
                renders: calls.load(Ordering::SeqCst) - 1,
                selected_metadata_regular: fs::symlink_metadata(&entry)
                    .is_ok_and(|metadata| metadata.is_file()),
                parent_is_link: fs::symlink_metadata(&directory)
                    .is_ok_and(|metadata| metadata.is_symlink()),
                read_only,
                write_denied,
            }
        })
    }

    fn reads_metadata(
        status: StatusCode,
        revision: Option<&'static str>,
    ) -> impl Fn(&ResponseObservation) -> AssertionResult {
        move |actual| {
            if actual.status == status
                && actual.revision.as_deref() == revision
                && actual.body == actual.saved_body
                && actual.disk_body == actual.saved_body
                && actual.renders == 0
            {
                Ok(())
            } else {
                Err(AssertionError::new(vec![format!(
                    "expected unchanged saved representation status={status}, revision={revision:?}, renders=0; actual {actual:?}"
                )]))
            }
        }
    }
    fn regenerates(actual: &ResponseObservation) -> AssertionResult {
        if actual.status == StatusCode::NON_AUTHORITATIVE_INFORMATION
            && actual.revision.as_deref() == Some("regenerated")
            && actual.body.starts_with(b"regenerated-namespace-body")
            && actual.body != actual.saved_body
            && actual.disk_body == actual.body
            && actual.renders == 1
            && actual.selected_metadata_regular
        {
            Ok(())
        } else {
            Err(AssertionError::new(vec![format!(
                "present damaged metadata must regenerate status, headers and HTML exactly once; actual {actual:?}"
            )]))
        }
    }
    fn refuses_saved(actual: &ResponseObservation) -> AssertionResult {
        if actual.status == StatusCode::INTERNAL_SERVER_ERROR
            && actual.body.is_empty()
            && actual.revision.is_none()
            && actual.disk_body == actual.saved_body
            && actual.renders == 1
            && (!actual.read_only || actual.write_denied)
            && (!matches!(
                actual.namespace,
                Namespace::DanglingDirectory | Namespace::InvalidDirectoryTarget
            ) || actual.parent_is_link)
        {
            Ok(())
        } else {
            Err(AssertionError::new(vec![format!(
                "unrepairable existing metadata must fail without serving saved HTML or losing its namespace; actual {actual:?}"
            )]))
        }
    }

    lets_expect! {
        expect(observe(namespace, environment)) as metadata_namespace_representation {
            let namespace = Namespace::Present;
            let environment = RecoveryEnvironment::Writable;
            to reads_the_saved_metadata { reads_metadata(StatusCode::CREATED, Some("saved")) }
            when the_entry_is_a_resolvable_file_link {
                let namespace = Namespace::FileLink;
                to reads_the_saved_metadata { reads_metadata(StatusCode::CREATED, Some("saved")) }
            }
            when only_the_entry_is_absent {
                let namespace = Namespace::MissingEntry;
                to reads_the_plain_html_of_an_older_publisher { reads_metadata(StatusCode::OK, None) }
            }
            when the_directory_is_absent {
                let namespace = Namespace::MissingDirectory;
                to reads_the_plain_html_of_an_older_publisher { reads_metadata(StatusCode::OK, None) }
            }
            when the_directory_is_a_resolvable_link {
                let namespace = Namespace::DirectoryLink;
                to reads_the_saved_metadata { reads_metadata(StatusCode::CREATED, Some("saved")) }
            }
            when the_directory_link_has_no_selected_entry {
                let namespace = Namespace::EmptyDirectoryLink;
                to reads_the_plain_html_of_an_older_publisher { reads_metadata(StatusCode::OK, None) }
            }
            when the_entry_is_a_dangling_link {
                let namespace = Namespace::DanglingEntry;
                to regenerates_the_representation { regenerates }
                when publication_is_read_only {
                    let environment = RecoveryEnvironment::ReadOnly;
                    to rejects_without_serving_saved_html { refuses_saved }
                }
            }
            when the_directory_is_a_dangling_link {
                let namespace = Namespace::DanglingDirectory;
                to rejects_without_serving_saved_html { refuses_saved }
            }
            when the_entry_has_an_invalid_target {
                let namespace = Namespace::InvalidEntryTarget;
                to regenerates_the_representation { regenerates }
            }
            when the_directory_has_an_invalid_target {
                let namespace = Namespace::InvalidDirectoryTarget;
                to rejects_without_serving_saved_html { refuses_saved }
            }
        }
    }
}
