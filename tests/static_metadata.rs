//! Static representation metadata at the public HTTP boundary.

use leptos::prelude::*;
use leptos_ntex_unofficial::{NtexRouteListing, ResponseOptions, register_leptos_routes};
use leptos_router::{Method, SsrMode, static_routes::StaticRoute};
use lets_expect::*;
use ntex::{
    http::{Method as HttpMethod, StatusCode, header},
    web::{App, test},
};
use std::{
    path::PathBuf,
    sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    },
};

struct SiteRoot(PathBuf);
impl SiteRoot {
    fn new() -> Self {
        static NEXT: AtomicUsize = AtomicUsize::new(0);
        loop {
            let path = std::env::temp_dir().join(format!(
                "leptos_static_metadata_{}_{}",
                std::process::id(),
                NEXT.fetch_add(1, Ordering::Relaxed)
            ));
            match std::fs::create_dir(&path) {
                Ok(()) => return Self(path),
                Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {}
                Err(error) => panic!("create metadata fixture: {error}"),
            }
        }
    }
    /// A read-only deployment: files lose write permission and directories,
    /// including the metadata directory, stay traversable but immutable.
    #[cfg(unix)]
    fn make_read_only(&self) {
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

#[derive(Clone, Copy)]
enum Media {
    Absent,
    Utf8,
    Malformed,
    NonUtf8,
    RepeatedLastUtf8,
    RepeatedLastMalformed,
}
impl Media {
    fn values(self) -> Vec<header::HeaderValue> {
        let values: &[&'static str] = match self {
            Self::Absent => &[],
            Self::Utf8 => &["text/html; charset=utf-8"],
            Self::Malformed => &["not-a-media-type"],
            Self::NonUtf8 => return vec![header::HeaderValue::from_bytes(b"\xff").unwrap()],
            Self::RepeatedLastUtf8 => &["text/plain; charset=us-ascii", "text/html; charset=utf-8"],
            Self::RepeatedLastMalformed => &["text/plain; charset=us-ascii", "not-a-media-type"],
        };
        values
            .iter()
            .map(|value| header::HeaderValue::from_static(value))
            .collect()
    }
}

#[derive(Clone, Copy)]
enum Operation {
    Get,
    Head,
    Range,
    Conditional,
}

#[derive(Clone, Copy)]
enum Artifact {
    Complete,
    #[cfg(unix)]
    MissingLock,
    #[cfg(unix)]
    MismatchedBody,
    #[cfg(unix)]
    UnreadableLock,
}

#[derive(Clone, Copy)]
struct RequestCase {
    media: Media,
    operation: Operation,
    status: StatusCode,
    artifact: Artifact,
    read_only: bool,
}
impl Default for RequestCase {
    fn default() -> Self {
        Self {
            media: Media::Absent,
            operation: Operation::Get,
            status: StatusCode::OK,
            artifact: Artifact::Complete,
            read_only: false,
        }
    }
}

#[derive(Debug)]
struct ResponseData {
    status: StatusCode,
    media_type: Vec<String>,
    repeated: Vec<String>,
    body: Vec<u8>,
}
#[derive(Debug)]
struct Observation {
    first: ResponseData,
    second: ResponseData,
    disk_body: Option<Vec<u8>>,
    second_renders: usize,
    write_denied: bool,
}

fn run_case(case: RequestCase) -> Observation {
    ntex::rt::System::new(
        "static-metadata-contract",
        leptos_ntex_unofficial::RequestRuntime::new(ntex::rt::DefaultRuntime),
    )
    .block_on(observe(case))
}

async fn observe(case: RequestCase) -> Observation {
    let root = SiteRoot::new();
    let renders = Arc::new(AtomicUsize::new(0));
    let app_fn = {
        let renders = renders.clone();
        move || {
            let revision = renders.fetch_add(1, Ordering::Relaxed) + 1;
            let response = expect_context::<ResponseOptions>();
            response.set_status(case.status);
            for value in case.media.values() {
                response.append_header(header::CONTENT_TYPE, value);
            }
            response.append_header(
                header::HeaderName::from_static("x-static-value"),
                header::HeaderValue::from_static("first"),
            );
            response.append_header(
                header::HeaderName::from_static("x-static-value"),
                header::HeaderValue::from_static("second"),
            );
            view! { <main>{format!("revision:{revision}; Привет")}</main> }
        }
    };
    let routes = vec![NtexRouteListing::new(
        "/page".to_owned(),
        SsrMode::Static(StaticRoute::new()),
        [Method::Get],
        vec![],
    )];
    let options = LeptosOptions::builder()
        .output_name("static_metadata")
        .site_root(root.0.to_string_lossy().into_owned())
        .build();
    let app = test::init_service(
        App::new()
            .state(options)
            .configure(|cfg| register_leptos_routes(cfg, routes, app_fn)),
    )
    .await;
    let response =
        test::call_service(&app, test::TestRequest::with_uri("/page").to_request()).await;
    let validator = response.headers().get(header::ETAG).cloned();
    let first = ResponseData {
        status: response.status(),
        media_type: response
            .headers()
            .get_all(header::CONTENT_TYPE)
            .map(|v| v.to_str().unwrap().to_owned())
            .collect(),
        repeated: response
            .headers()
            .get_all("x-static-value")
            .map(|v| v.to_str().unwrap().to_owned())
            .collect(),
        body: test::read_body(response).await.to_vec(),
    };
    let file = root.0.join("page.html");
    let disk_body = std::fs::read(&file).ok();
    #[cfg(unix)]
    let lock_files = std::fs::read_dir(&root.0)
        .unwrap()
        .map(|entry| entry.unwrap().path())
        .filter(|path| path.extension().is_some_and(|ext| ext == "lock"))
        .collect::<Vec<_>>();
    match case.artifact {
        Artifact::Complete => {}
        #[cfg(unix)]
        Artifact::MissingLock | Artifact::MismatchedBody => {
            for lock in &lock_files {
                std::fs::remove_file(lock).unwrap();
            }
            if matches!(case.artifact, Artifact::MismatchedBody) {
                std::fs::write(&file, b"mismatched-body").unwrap();
            }
        }
        #[cfg(unix)]
        Artifact::UnreadableLock => {}
    }
    let mut write_denied = false;
    #[cfg(unix)]
    if case.read_only {
        root.make_read_only();
        if matches!(case.artifact, Artifact::UnreadableLock) {
            use std::os::unix::fs::PermissionsExt;
            for lock in &lock_files {
                std::fs::set_permissions(lock, std::fs::Permissions::from_mode(0o000)).unwrap();
            }
        }
        write_denied = std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(root.0.join("write-probe"))
            .is_err_and(|error| error.kind() == std::io::ErrorKind::PermissionDenied);
    }
    #[cfg(not(unix))]
    let _ = (&mut write_denied, case.read_only);
    let mut request = test::TestRequest::with_uri("/page");
    request = match case.operation {
        Operation::Get => request,
        Operation::Head => request.method(HttpMethod::HEAD),
        Operation::Range => request.header(header::RANGE, "bytes=0-8"),
        Operation::Conditional => request.header(
            header::IF_NONE_MATCH,
            validator.expect("the successful fixture has an ETag"),
        ),
    };
    let before = renders.load(Ordering::Relaxed);
    let response = test::call_service(&app, request.to_request()).await;
    let second = ResponseData {
        status: response.status(),
        media_type: response
            .headers()
            .get_all(header::CONTENT_TYPE)
            .map(|v| v.to_str().unwrap().to_owned())
            .collect(),
        repeated: response
            .headers()
            .get_all("x-static-value")
            .map(|v| v.to_str().unwrap().to_owned())
            .collect(),
        body: test::read_body(response).await.to_vec(),
    };
    Observation {
        first,
        second,
        disk_body,
        second_renders: renders.load(Ordering::Relaxed) - before,
        write_denied,
    }
}

#[derive(Clone, Copy)]
enum BodyOutcome {
    Complete,
    Head,
    Range,
    NotModified,
    InlineError,
}

fn have_media_response(
    expected: &'static str,
    outcome: BodyOutcome,
) -> impl Fn(&Observation) -> AssertionResult {
    move |actual| {
        let initial_status = if matches!(outcome, BodyOutcome::InlineError) {
            StatusCode::UNPROCESSABLE_ENTITY
        } else {
            StatusCode::OK
        };
        let final_status = match outcome {
            BodyOutcome::Range => StatusCode::PARTIAL_CONTENT,
            BodyOutcome::NotModified => StatusCode::NOT_MODIFIED,
            _ => initial_status,
        };
        let body_ok = match outcome {
            BodyOutcome::Complete => actual.second.body == actual.first.body,
            BodyOutcome::Head => true, // Internal response body does not prove wire HEAD behavior.
            BodyOutcome::Range => actual.first.body.get(..9) == Some(actual.second.body.as_slice()),
            BodyOutcome::NotModified => actual.second.body.is_empty(),
            BodyOutcome::InlineError => actual.disk_body.is_none() && actual.second_renders == 1,
        };
        let persisted_ok = matches!(outcome, BodyOutcome::InlineError)
            || actual.disk_body.as_ref() == Some(&actual.first.body);
        if actual.first.status == initial_status
            && actual.second.status == final_status
            && actual.first.media_type == [expected]
            && actual.second.media_type == [expected]
            && body_ok
            && persisted_ok
        {
            Ok(())
        } else {
            Err(AssertionError::new(vec![format!(
                "expected Content-Type {expected:?}, statuses {initial_status}/{final_status}, consistent selected body; actual {actual:?}"
            )]))
        }
    }
}

lets_expect! {
    expect(run_case(case)) as the_static_response_media_type {
        let media = Media::Absent;
        let operation = Operation::Get;
        let status = StatusCode::OK;
        let case = RequestCase { media, operation, status, ..Default::default() };
        to derives_the_file_media_type { have_media_response("text/html", BodyOutcome::Complete) }
        when the_media_type_is_explicit_utf8_html {
            let media = Media::Utf8;
            to preserves_the_explicit_media_type_on_both_requests { have_media_response("text/html; charset=utf-8", BodyOutcome::Complete) }
            when the_request_is_head {
                let operation = Operation::Head;
                to preserves_the_selected_representation_metadata { have_media_response("text/html; charset=utf-8", BodyOutcome::Head) }
            }
            when the_request_selects_a_byte_range {
                let operation = Operation::Range;
                to preserves_the_media_type_of_the_selected_bytes { have_media_response("text/html; charset=utf-8", BodyOutcome::Range) }
            }
            when the_request_validator_matches {
                let operation = Operation::Conditional;
                to preserves_the_selected_representation_media_type { have_media_response("text/html; charset=utf-8", BodyOutcome::NotModified) }
            }
            when the_render_is_an_inline_error {
                let status = StatusCode::UNPROCESSABLE_ENTITY;
                to preserves_the_application_media_type { have_media_response("text/html; charset=utf-8", BodyOutcome::InlineError) }
            }
        }
        when the_media_type_is_malformed {
            let media = Media::Malformed;
            to uses_the_filename_fallback { have_media_response("text/html", BodyOutcome::Complete) }
        }
        when the_media_type_is_not_utf8 {
            let media = Media::NonUtf8;
            to uses_the_filename_fallback { have_media_response("text/html", BodyOutcome::Complete) }
        }
        when the_media_type_has_repeated_values {
            let media = Media::RepeatedLastUtf8;
            to uses_the_last_selected_media_type { have_media_response("text/html; charset=utf-8", BodyOutcome::Complete) }
        }
        when the_last_repeated_media_type_is_malformed {
            let media = Media::RepeatedLastMalformed;
            to uses_the_filename_fallback { have_media_response("text/html", BodyOutcome::Complete) }
        }
    }
}

#[cfg(unix)]
mod read_only {
    use super::*;

    fn serve_snapshot(actual: &Observation) -> AssertionResult {
        if actual.second.status == StatusCode::CREATED
            && actual.second.media_type == ["text/html; charset=utf-8"]
            && actual.second.repeated == ["first", "second"]
            && actual.second.body == actual.first.body
            && actual.disk_body.as_ref() == Some(&actual.first.body)
            && actual.second_renders == 0
            && actual.write_denied
        {
            Ok(())
        } else {
            Err(AssertionError::new(vec![format!(
                "expected persisted status201/media/repeated headers/body without render under read-only permissions; actual {actual:?}"
            )]))
        }
    }

    fn reject_snapshot(actual: &Observation) -> AssertionResult {
        if actual.second.status == StatusCode::INTERNAL_SERVER_ERROR
            && actual.second.body.is_empty()
            && actual.write_denied
        {
            Ok(())
        } else {
            Err(AssertionError::new(vec![format!(
                "expected rejection without a shared snapshot body under read-only permissions; actual {actual:?}"
            )]))
        }
    }

    lets_expect! {
        expect(run_case(case)) as a_read_only_static_deployment {
            let artifact = Artifact::MissingLock;
            let case = RequestCase { media: Media::Utf8, status: StatusCode::CREATED, artifact, read_only: true, ..Default::default() };
            to serves_the_persisted_representation_without_rendering { serve_snapshot }
            when the_html_does_not_match_its_metadata {
                let artifact = Artifact::MismatchedBody;
                to rejects_the_inconsistent_representation { reject_snapshot }
            }
            when the_existing_lock_is_unreadable {
                let artifact = Artifact::UnreadableLock;
                to does_not_bypass_coordination_permissions { reject_snapshot }
            }
        }
    }
}
