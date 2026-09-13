//! Static artifact publication and representation metadata at the public HTTP boundary.

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
    sync::atomic::{AtomicUsize, Ordering},
};

struct Root(PathBuf);
impl Root {
    fn new() -> Self {
        static N: AtomicUsize = AtomicUsize::new(0);
        let path = std::env::temp_dir().join(format!(
            "static_artifacts_{}_{}",
            std::process::id(),
            N.fetch_add(1, Ordering::Relaxed)
        ));
        std::fs::create_dir(&path).unwrap();
        Self(path)
    }
}
impl Drop for Root {
    fn drop(&mut self) {
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let _ = std::fs::set_permissions(&self.0, std::fs::Permissions::from_mode(0o755));
        }
        let _ = std::fs::remove_dir_all(&self.0);
    }
}
#[derive(Debug)]
struct Observation {
    status: StatusCode,
    body: Vec<u8>,
    second_status: StatusCode,
    second_body: Vec<u8>,
    language: Option<String>,
    location: Option<String>,
}
#[derive(Clone, Copy)]
enum Condition {
    None,
    Range,
    IfRange,
    NotModified,
}
fn run(segment: String, legacy: bool, condition: Condition, read_only: bool) -> Observation {
    ntex::rt::System::new(
        "static-artifacts",
        leptos_ntex_unofficial::RequestRuntime::new(ntex::rt::DefaultRuntime),
    )
    .block_on(async move {
        let root = Root::new();
        let uri = format!(
            "/{}",
            percent_encoding::utf8_percent_encode(&segment, percent_encoding::NON_ALPHANUMERIC)
        );
        if legacy {
            std::fs::write(
                root.0.join(format!("{segment}.html")),
                "existing-public-body",
            )
            .unwrap();
        }
        let routes = vec![NtexRouteListing::new(
            format!("/{segment}"),
            SsrMode::Static(StaticRoute::new()),
            [Method::Get],
            vec![],
        )];
        let options = LeptosOptions::builder()
            .output_name("static_artifacts")
            .site_root(root.0.to_string_lossy().into_owned())
            .build();
        let app = test::init_service(App::new().state(options).configure(|cfg| {
            register_leptos_routes(cfg, routes, || {
                let response = expect_context::<ResponseOptions>();
                response.insert_header(
                    header::CONTENT_LANGUAGE,
                    header::HeaderValue::from_static("ru"),
                );
                response.insert_header(
                    header::CONTENT_LOCATION,
                    header::HeaderValue::from_static("/canonical"),
                );
                "existing-public-body"
            })
        }))
        .await;
        let first = test::call_service(&app, test::TestRequest::with_uri(&uri).to_request()).await;
        let status = first.status();
        let etag = first.headers().get(header::ETAG).cloned();
        let body = test::read_body(first).await.to_vec();
        #[cfg(unix)]
        if read_only {
            use std::os::unix::fs::PermissionsExt;
            for entry in std::fs::read_dir(&root.0).unwrap() {
                let path = entry.unwrap().path();
                if path
                    .extension()
                    .is_some_and(|extension| extension == "lock")
                {
                    std::fs::remove_file(path).unwrap();
                }
            }
            std::fs::set_permissions(&root.0, std::fs::Permissions::from_mode(0o555)).unwrap();
            assert!(
                std::fs::write(root.0.join("write-probe"), "denied")
                    .is_err_and(|error| error.kind() == std::io::ErrorKind::PermissionDenied)
            );
        }
        #[cfg(not(unix))]
        let _ = read_only;
        let mut request = test::TestRequest::with_uri(&uri);
        match condition {
            Condition::None => {}
            Condition::Range => {
                request = request.header(header::RANGE, "bytes=0-3");
            }
            Condition::IfRange => {
                request = request
                    .header(header::RANGE, "bytes=0-3")
                    .header(header::IF_RANGE, etag.unwrap());
            }
            Condition::NotModified => {
                request = request.header(header::IF_NONE_MATCH, etag.unwrap());
            }
        }
        let second = test::call_service(&app, request.to_request()).await;
        let second_status = second.status();
        let language = second
            .headers()
            .get(header::CONTENT_LANGUAGE)
            .map(|v| v.to_str().unwrap().to_owned());
        let location = second
            .headers()
            .get(header::CONTENT_LOCATION)
            .map(|v| v.to_str().unwrap().to_owned());
        let second_body = test::read_body(second).await.to_vec();
        Observation {
            status,
            body,
            second_status,
            second_body,
            language,
            location,
        }
    })
}
fn complete(actual: &Observation) -> AssertionResult {
    if actual.status == StatusCode::OK
        && actual.second_status == StatusCode::OK
        && actual.body.starts_with(b"existing-public-body")
        && actual.second_body == actual.body
    {
        Ok(())
    } else {
        Err(AssertionError::new(vec![format!(
            "expected complete public representation, actual {actual:?}"
        )]))
    }
}
lets_expect! {
    expect(run(segment, legacy, Condition::None, read_only)) as bounded_static_filename {
        let segment = "a".repeat(230);
        let legacy = false;
        let read_only = false;
        to publishes_and_reads_a_complete_response { complete }
        when legacy_metadata_reaches_name_max { let segment = "a".repeat(237); to publishes_and_reads_a_complete_response { complete } }
        when legacy_metadata_exceeds_name_max { let segment = "a".repeat(238); to publishes_and_reads_a_complete_response { complete } }
        when html_is_one_below_name_max { let segment = "a".repeat(249); to publishes_and_reads_a_complete_response { complete } }
        when html_reaches_name_max { let segment = "a".repeat(250); to publishes_and_reads_a_complete_response { complete }
        }
        when the_long_route_segment_uses_multibyte_utf8 { let segment = format!("{}a", "界".repeat(83)); to publishes_and_reads_a_complete_response { complete } }
        when html_exceeds_name_max { let segment = "a".repeat(251); to rejects_the_unrepresentable_filename { have(status) equal(StatusCode::INTERNAL_SERVER_ERROR) } }
        when existing_html_reaches_name_max { let segment = "a".repeat(250); let legacy = true; to reads_the_existing_representation { complete } }
    }
    expect(run("page".into(), false, condition, false)) as static_representation_metadata {
        let condition = Condition::None;
        to retains_language_and_location { have(language) equal(Some("ru".into())), have(location) equal(Some("/canonical".into())) }
        when the_request_has_a_range { let condition = Condition::Range; to retains_required_representation_fields { have(second_status) equal(StatusCode::PARTIAL_CONTENT), have(language) equal(Some("ru".into())), have(location) equal(Some("/canonical".into())) } }
        when the_range_has_a_matching_if_range { let condition = Condition::IfRange; to omits_redundant_language { have(second_status) equal(StatusCode::PARTIAL_CONTENT), have(language) be_none, have(location) equal(Some("/canonical".into())) } }
        when the_request_revalidates_the_representation { let condition = Condition::NotModified; to omits_redundant_language { have(second_status) equal(StatusCode::NOT_MODIFIED), have(language) be_none, have(location) equal(Some("/canonical".into())) } }
    }
}

#[cfg(unix)]
lets_expect! {
    expect(run("a".repeat(250), legacy, Condition::None, true)) as the_read_only_long_artifact {
        let legacy = false;
        to reads_the_complete_saved_representation { complete }
        when the_artifact_has_no_metadata { let legacy = true; to reads_the_legacy_representation { complete } }
    }
}
