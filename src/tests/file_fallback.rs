use super::*;
use lets_expect::lets_expect;
use ntex::{
    http::{Method as HttpMethod, StatusCode, header},
    web::{App as NtexApp, test},
};

#[derive(Debug)]
struct FileObservation {
    status: StatusCode,
    body: String,
    content_type: Option<String>,
    encoding: Option<String>,
    vary: Vec<String>,
    context: Option<String>,
    cookies: Vec<String>,
    content_range: Option<String>,
}

#[derive(Clone)]
struct FileRequest {
    uri: String,
    method: HttpMethod,
    headers: Vec<(&'static str, &'static str)>,
    context: bool,
    package: bool,
}

impl Default for FileRequest {
    fn default() -> Self {
        Self {
            uri: "/hello.txt".into(),
            method: HttpMethod::GET,
            headers: Vec::new(),
            context: false,
            package: false,
        }
    }
}

async fn file_request(request: FileRequest) -> FileObservation {
    let parent = temp_site_root("file_contract");
    let root = parent.join("public");
    std::fs::create_dir_all(root.join("assets/css")).unwrap();
    std::fs::create_dir_all(root.join(".well-known/acme-challenge")).unwrap();
    std::fs::create_dir_all(root.join("subdir")).unwrap();
    std::fs::write(root.join("hello.txt"), "world!").unwrap();
    std::fs::write(root.join("assets/css/app.css"), "body{color:red}").unwrap();
    std::fs::write(root.join(".well-known/acme-challenge/token"), "acme-proof").unwrap();
    std::fs::write(root.join(".env"), "SECRET").unwrap();
    std::fs::write(root.join("subdir/.env"), "SECRET").unwrap();
    std::fs::write(parent.join("secret.txt"), "SECRET").unwrap();
    std::fs::write(root.join("app.js"), "console.log('plain');").unwrap();
    std::fs::write(root.join("app.js.br"), "br-bytes").unwrap();
    std::fs::write(root.join("app.js.gz"), "gzip-bytes").unwrap();
    #[cfg(unix)]
    std::os::unix::fs::symlink(parent.join("secret.txt"), root.join("escape.txt")).unwrap();
    let options = LeptosOptions::builder()
        .output_name("file_contract")
        .site_root(root.to_string_lossy().into_owned())
        .site_pkg_dir("")
        .build();
    let add_context = request.context;
    let app = test::init_service(
        NtexApp::new()
            .state(options.clone())
            .service(crate::site_pkg_dir_service::<ntex::web::DefaultError>(
                &LeptosOptions::builder()
                    .output_name("file_contract_pkg")
                    .site_root(root.to_string_lossy().into_owned())
                    .site_pkg_dir("assets")
                    .build(),
            ))
            .route(
                "/{tail}*",
                crate::file_and_error_handler_with_context(
                    move || {
                        if add_context {
                            let response_options = use_context::<crate::ResponseOptions>().unwrap();
                            response_options.append_header(
                                header::SET_COOKIE,
                                header::HeaderValue::from_static("session=one; HttpOnly"),
                            );
                            response_options.append_header(
                                header::SET_COOKIE,
                                header::HeaderValue::from_static("csrf=two; SameSite=Lax"),
                            );
                            use_context::<crate::ResponseOptions>()
                                .unwrap()
                                .insert_header(
                                    header::HeaderName::from_static("x-context"),
                                    header::HeaderValue::from_static("yes"),
                                );
                        }
                    },
                    |_: LeptosOptions| view! { <h1>"Not Found Shell"</h1> },
                ),
            ),
    )
    .await;
    // The package scope fixture has the same bytes, so only the public
    // serving boundary changes in its context.
    if request.package {
        std::fs::write(root.join("assets/app.js"), "console.log('plain');").unwrap();
        std::fs::write(root.join("assets/app.js.br"), "br-bytes").unwrap();
        std::fs::write(root.join("assets/app.js.gz"), "gzip-bytes").unwrap();
    }
    let uri = if request.package {
        "/assets/app.js"
    } else {
        &request.uri
    };
    let mut req = test::TestRequest::with_uri(uri).method(request.method);
    for (name, value) in request.headers {
        req = req.header(name, value);
    }
    let response = test::call_service(&app, req.to_request()).await;
    let value = |name| {
        response
            .headers()
            .get(name)
            .and_then(|value| value.to_str().ok())
            .map(str::to_owned)
    };
    let cookies = response
        .headers()
        .get_all(header::SET_COOKIE)
        .map(|v| v.to_str().unwrap().to_owned())
        .collect();
    let status = response.status();
    let content_type = value(header::CONTENT_TYPE);
    let encoding = value(header::CONTENT_ENCODING);
    let content_range = value(header::CONTENT_RANGE);
    let context = value(header::HeaderName::from_static("x-context"));
    let vary = response
        .headers()
        .get_all(header::VARY)
        .filter_map(|value| value.to_str().ok())
        .flat_map(|value| value.split(','))
        .map(|value| value.trim().to_ascii_lowercase())
        .collect();
    let body = String::from_utf8(test::read_body(response).await.to_vec()).unwrap();
    FileObservation {
        status,
        body,
        content_type,
        encoding,
        vary,
        context,
        cookies,
        content_range,
    }
}

fn negotiated_request(encodings: &'static str, package: bool) -> FileRequest {
    FileRequest {
        uri: "/app.js".into(),
        headers: vec![("accept-encoding", encodings)],
        package,
        ..Default::default()
    }
}

fn contains_text(expected: &'static str) -> impl Fn(&String) -> lets_expect::AssertionResult {
    move |actual| {
        if actual.contains(expected) {
            Ok(())
        } else {
            Err(lets_expect::AssertionError {
                message: vec![format!(
                    "Expected body to contain {expected:?}, received {actual:?}"
                )],
            })
        }
    }
}

fn excludes_text(expected: &'static str) -> impl Fn(&String) -> lets_expect::AssertionResult {
    move |actual| {
        if !actual.contains(expected) {
            Ok(())
        } else {
            Err(lets_expect::AssertionError {
                message: vec![format!(
                    "Expected body without {expected:?}, received {actual:?}"
                )],
            })
        }
    }
}

fn contains_token(expected: String) -> impl Fn(&Vec<String>) -> lets_expect::AssertionResult {
    move |actual| {
        if actual.contains(&expected) {
            Ok(())
        } else {
            Err(lets_expect::AssertionError {
                message: vec![format!("Expected token {expected:?}, received {actual:?}")],
            })
        }
    }
}

lets_expect! {
    expect(run_ntex(file_request(request))) as the_fallback_response {
        let request = FileRequest::default();
        to serves_the_file { have(status) equal(StatusCode::OK), have(body) equal("world!".to_owned()) }
        to reports_the_file_mime { have(content_type) equal(Some("text/plain".to_owned())) }
        when the_method_is_head {
            let request = FileRequest { method: HttpMethod::HEAD, ..Default::default() };
            to preserves_get_status_and_mime { have(status) equal(StatusCode::OK), have(content_type) equal(Some("text/plain".to_owned())) }
        }
        when the_method_is_post {
            let request = FileRequest { method: HttpMethod::POST, ..Default::default() };
            to does_not_dispatch_the_handler { have(status) equal(StatusCode::NOT_FOUND), have(body) equal("".to_owned()) }
        }
        when additional_context_sets_a_header {
            let request = FileRequest { context: true, ..Default::default() };
            to applies_the_header_on_a_file_hit { have(context) equal(Some("yes".to_owned())) }
        }
        when the_target_is_missing {
            let request = FileRequest { uri: "/missing".into(), ..Default::default() };
            to renders_the_404_shell { have(status) equal(StatusCode::NOT_FOUND), have(body.clone()) contains_text("Not Found Shell") }
            when the_method_is_head {
                let request = FileRequest { uri: "/missing".into(), method: HttpMethod::HEAD, ..Default::default() };
                to preserves_the_404_mime { have(status) equal(StatusCode::NOT_FOUND), have(content_type) equal(Some("text/html; charset=utf-8".to_owned())) }
            }
            when additional_context_sets_a_header {
                let request = FileRequest { uri: "/missing".into(), context: true, ..Default::default() };
                to applies_the_header_on_the_shell { have(status) equal(StatusCode::NOT_FOUND), have(context) equal(Some("yes".to_owned())) }
            }
        }
        when the_missing_target_is_nested {
            let request = FileRequest { uri: "/deep/missing/page".into(), ..Default::default() };
            to reaches_the_shell { have(status) equal(StatusCode::NOT_FOUND), have(body.clone()) contains_text("Not Found Shell") }
        }
        when the_target_is_nested_css {
            let request = FileRequest { uri: "/assets/css/app.css".into(), ..Default::default() };
            to serves_the_nested_file { have(status) equal(StatusCode::OK), have(body) equal("body{color:red}".to_owned()) }
        }
        when the_target_is_a_well_known_resource {
            let request = FileRequest { uri: "/.well-known/acme-challenge/token".into(), ..Default::default() };
            to serves_the_proof { have(status) equal(StatusCode::OK), have(body) equal("acme-proof".to_owned()) }
        }
    }
}

lets_expect! {
    expect(run_ntex(file_request(request))) as the_rejected_file_path_response {
        let uri = "/../secret.txt";
        let request = FileRequest { uri: uri.into(), ..Default::default() };
        to rejects_parent_traversal { have(status) equal(StatusCode::NOT_FOUND), have(body.clone()) contains_text("Not Found Shell"), have(body) excludes_text("SECRET") }
        when parent_is_percent_encoded {
            let uri = "/%2e%2e/secret.txt";
            to rejects_encoded_parent { have(status) equal(StatusCode::NOT_FOUND), have(body.clone()) contains_text("Not Found Shell"), have(body) excludes_text("SECRET") }
        }
        when the_path_has_an_absolute_root_shape {
            let uri = "/etc/passwd";
            to stays_in_the_site_root { have(status) equal(StatusCode::NOT_FOUND), have(body.clone()) contains_text("Not Found Shell"), have(body) excludes_text("root:") }
        }
        when the_target_is_a_dotfile {
            let uri = "/.env";
            to hides_the_file { have(status) equal(StatusCode::NOT_FOUND), have(body.clone()) contains_text("Not Found Shell"), have(body) excludes_text("SECRET") }
        }
        when the_dotfile_is_nested {
            let uri = "/subdir/.env";
            to hides_the_file { have(status) equal(StatusCode::NOT_FOUND), have(body.clone()) contains_text("Not Found Shell"), have(body) excludes_text("SECRET") }
        }
        when the_separator_is_encoded {
            let uri = "/subdir%2F.env";
            to hides_the_file { have(status) equal(StatusCode::NOT_FOUND), have(body.clone()) contains_text("Not Found Shell"), have(body) excludes_text("SECRET") }
        }
        when the_path_contains_a_nul {
            let uri = "/hello%00hidden.txt";
            to rejects_the_path { have(status) equal(StatusCode::NOT_FOUND), have(body.clone()) contains_text("Not Found Shell"), have(body) excludes_text("world!") }
        }
        when the_target_is_the_root_directory {
            let uri = "/";
            to renders_the_shell { have(status) equal(StatusCode::NOT_FOUND), have(body.clone()) contains_text("Not Found Shell") }
        }
    }
}

#[cfg(unix)]
lets_expect! {
    expect(run_ntex(file_request(FileRequest { uri: "/escape.txt".into(), ..Default::default() }))) as the_external_symlink_response {
        to keeps_the_external_file_private { have(status) equal(StatusCode::NOT_FOUND), have(body.clone()) contains_text("Not Found Shell"), have(body) excludes_text("SECRET") }
    }
}

lets_expect! {
    expect(run_ntex(file_request(request))) as the_negotiated_file_response {
        let encodings = "";
        let package = false;
        let request = negotiated_request(encodings, package);
        to serves_identity { have(status) equal(StatusCode::OK), have(body) equal("console.log('plain');".to_owned()), have(encoding) be_none }
        to keeps_the_original_mime { have(content_type) equal(Some("text/javascript".to_owned())) }
        to varies_even_when_identity_is_selected { have(vary) contains_token("accept-encoding".to_owned()) }
        when brotli_is_accepted {
            let encodings = "br";
            to serves_brotli_bytes { have(status) equal(StatusCode::OK), have(body) equal("br-bytes".to_owned()), have(encoding) equal(Some("br".to_owned())) }
            to preserves_mime_and_vary { have(content_type) equal(Some("text/javascript".to_owned())), have(vary) contains_token("accept-encoding".to_owned()) }
            when the_service_is_the_package_scope {
                let package = true;
                to serves_the_same_representation { have(status) equal(StatusCode::OK), have(body) equal("br-bytes".to_owned()), have(encoding) equal(Some("br".to_owned())) }
            }
        }
        when only_gzip_is_named {
            let encodings = "gzip;q=0.1";
            to serves_gzip { have(body) equal("gzip-bytes".to_owned()), have(encoding) equal(Some("gzip".to_owned())), have(content_type) equal(Some("text/javascript".to_owned())) }
        }
        when gzip_has_a_higher_weight {
            let encodings = "gzip;q=1,br;q=0.1";
            to prefers_gzip { have(body) equal("gzip-bytes".to_owned()), have(encoding) equal(Some("gzip".to_owned())) }
        }
        when the_wildcard_has_a_higher_weight_than_named_codings {
            let encodings = "*;q=1,br;q=0.1,gzip;q=0.1";
            to serves_brotli_without_an_explicit_identity_preference { have(status) equal(StatusCode::OK), have(body) equal("br-bytes".to_owned()), have(encoding) equal(Some("br".to_owned())) }
        }
        when gzip_inherits_the_wildcard_weight {
            let encodings = "*;q=1,br;q=0.5";
            to prefers_gzip { have(status) equal(StatusCode::OK), have(body) equal("gzip-bytes".to_owned()), have(encoding) equal(Some("gzip".to_owned())) }
        }
        when both_codings_are_refused_despite_the_wildcard {
            let encodings = "br;q=0,gzip;q=0,*;q=1";
            to serves_identity { have(status) equal(StatusCode::OK), have(body) equal("console.log('plain');".to_owned()), have(encoding) be_none }
        }
        when identity_is_also_refused {
            let encodings = "identity;q=0,*;q=0";
            to reports_no_acceptable_representation { have(status) equal(StatusCode::NOT_ACCEPTABLE), have(body) equal("".to_owned()) }
            when the_service_is_the_package_scope {
                let package = true;
                to reports_the_same_refusal { have(status) equal(StatusCode::NOT_ACCEPTABLE), have(body) equal("".to_owned()) }
            }
        }
        when identity_has_an_explicit_higher_weight {
            let encodings = "identity;q=1,br;q=0.1,gzip;q=0.2";
            to prefers_identity { have(body) equal("console.log('plain');".to_owned()), have(encoding) be_none }
        }
    }
}

lets_expect! {
    expect(run_ntex(file_request(request))) as the_file_conditional_response {
        let headers = Vec::new();
        let method = HttpMethod::GET;
        let request = FileRequest { headers, method, ..Default::default() };
        to serves_the_full_file { have(status) equal(StatusCode::OK), have(body) equal("world!".to_owned()) }
        when a_partial_range_is_requested {
            let headers = vec![("range", "bytes=1-2")];
            to serves_the_slice { have(status) equal(StatusCode::PARTIAL_CONTENT), have(body) equal("or".to_owned()), have(content_range) equal(Some("bytes 1-2/6".to_owned())) }
            when the_method_is_head {
                let method = HttpMethod::HEAD;
                to ignores_range { have(status) equal(StatusCode::OK), have(content_range) be_none }
            }
            when if_range_does_not_match {
                let headers = vec![("range", "bytes=1-2"), ("if-range", "\"not-current\"")];
                to serves_the_full_representation { have(status) equal(StatusCode::OK), have(body) equal("world!".to_owned()), have(content_range) be_none }
            }
        }
        when range_has_no_members {
            let headers = vec![("range", "bytes=")];
            to rejects_without_panicking { have(status) equal(StatusCode::RANGE_NOT_SATISFIABLE), have(body) equal("".to_owned()) }
        }
        when if_match_fails_and_range_is_unsatisfiable {
            let headers = vec![("if-match", "\"not-current\""), ("range", "bytes=99-100")];
            to evaluates_the_precondition_first { have(status) equal(StatusCode::PRECONDITION_FAILED), have(content_range) be_none }
        }
        when if_none_match_matches_and_range_is_unsatisfiable {
            let headers = vec![("if-none-match", "*"), ("range", "bytes=99-100")];
            to evaluates_the_precondition_first { have(status) equal(StatusCode::NOT_MODIFIED), have(content_range) be_none }
        }
    }
}

#[cfg(test)]
mod conditional_fields {
    use super::*;
    use crate::file_and_error_handler;
    use lets_expect::lets_expect;
    async fn response(kind: u8) -> (StatusCode, String, bool) {
        let root = crate::tests::temp_site_root("http_extra");
        std::fs::create_dir_all(&*root).unwrap();
        std::fs::write(root.join("plain.txt"), "world!").unwrap();
        let opts = LeptosOptions::builder()
            .output_name("http_extra")
            .site_root(root.to_string_lossy().into_owned())
            .build();
        let app = ntex::web::test::init_service(
            ntex::web::App::new()
                .state(opts)
                .route("/{tail}*", file_and_error_handler(|_| "missing")),
        )
        .await;
        let initial = ntex::web::test::call_service(
            &app,
            ntex::web::test::TestRequest::with_uri("/plain.txt").to_request(),
        )
        .await;
        let etag = initial.headers().get(header::ETAG).unwrap().clone();
        let mut req = ntex::web::test::TestRequest::with_uri("/plain.txt");
        if kind == 0 {
            req = req.header(header::RANGE, "pages=0-1");
        } else {
            let name = if kind == 1 {
                header::IF_MATCH
            } else {
                header::IF_NONE_MATCH
            };
            req = req.header(name.clone(), "\"unmatched\"").header(name, etag);
        }
        let response = ntex::web::test::call_service(&app, req.to_request()).await;
        let status = response.status();
        let range = response.headers().contains_key(header::CONTENT_RANGE);
        let body = String::from_utf8(ntex::web::test::read_body(response).await.to_vec()).unwrap();
        (status, body, range)
    }
    fn run(kind: u8) -> (StatusCode, String, bool) {
        crate::tests::run_ntex(response(kind))
    }
    lets_expect! {
        expect(run(0)) as unknown_range_unit { to ignore_it { equal((StatusCode::OK,"world!".to_owned(),false)) } }
        expect(run(kind)) as repeated_validator {
            let kind = 1;
            to accepts_a_matching_if_match_member_on_the_second_line { equal((StatusCode::OK,"world!".to_owned(),false)) }
            when the_condition_is_if_none_match {
                let kind = 2;
                to recognizes_a_matching_member_on_the_second_line { equal((StatusCode::NOT_MODIFIED,"".to_owned(),false)) }
            }
        }
    }
}

async fn boundary_file_response(pre_epoch: bool) -> (StatusCode, String, bool, bool, bool) {
    let root = temp_site_root("file_boundary");
    std::fs::create_dir_all(&*root).unwrap();
    let path = root.join("boundary.txt");
    std::fs::write(&path, "world!").unwrap();
    if pre_epoch {
        let modified = std::time::UNIX_EPOCH - std::time::Duration::from_secs(10);
        std::fs::File::open(&path)
            .unwrap()
            .set_times(std::fs::FileTimes::new().set_modified(modified))
            .unwrap();
    }
    let options = LeptosOptions::builder()
        .output_name("file_boundary")
        .site_root(root.to_string_lossy().into_owned())
        .build();
    let app = test::init_service(
        NtexApp::new()
            .state(options)
            .route("/{tail}*", crate::file_and_error_handler(|_| "missing")),
    )
    .await;
    let mut request = test::TestRequest::with_uri("/boundary.txt");
    if !pre_epoch {
        request = request.header(ntex::http::header::RANGE, "bytes=0-5");
    }
    let response = test::call_service(&app, request.to_request()).await;
    let status = response.status();
    let range = response
        .headers()
        .contains_key(ntex::http::header::CONTENT_RANGE);
    let etag = response.headers().contains_key(ntex::http::header::ETAG);
    let modified = response
        .headers()
        .contains_key(ntex::http::header::LAST_MODIFIED);
    let body = String::from_utf8(test::read_body(response).await.to_vec()).unwrap();
    (status, body, range, etag, modified)
}
lets_expect::lets_expect! {
    expect(run_ntex(boundary_file_response(false))) as the_full_cover_file_range {
        to keeps_partial_response_semantics { equal((StatusCode::PARTIAL_CONTENT,"world!".to_owned(),true,true,true)) }
    }
}
#[cfg(unix)]
lets_expect::lets_expect! {
    expect(run_ntex(boundary_file_response(true))) as the_pre_epoch_file_timestamp {
        to serves_without_panicking_in_etag_generation { equal((StatusCode::OK,"world!".to_owned(),false,false,false)) }
    }
}

#[cfg(test)]
mod date_ranges {
    use super::*;

    async fn dated_range(condition: &'static str) -> (StatusCode, String, Option<String>) {
        let root = temp_site_root("dated_range");
        std::fs::create_dir_all(&*root).unwrap();
        let path = root.join("dated.txt");
        std::fs::write(&path, "world!").unwrap();
        let modified = std::time::UNIX_EPOCH + std::time::Duration::from_secs(784_111_777);
        std::fs::File::open(&path)
            .unwrap()
            .set_times(std::fs::FileTimes::new().set_modified(modified))
            .unwrap();
        let options = LeptosOptions::builder()
            .output_name("dated_range")
            .site_root(root.to_string_lossy().into_owned())
            .build();
        let app = test::init_service(
            NtexApp::new()
                .state(options)
                .route("/{tail}*", crate::file_and_error_handler(|_| "missing")),
        )
        .await;
        let request = test::TestRequest::with_uri("/dated.txt")
            .header(header::RANGE, "bytes=1-2")
            .header(header::IF_RANGE, condition)
            .to_request();
        let response = test::call_service(&app, request).await;
        assert_eq!(
            response.headers().get(header::LAST_MODIFIED).unwrap(),
            "Sun, 06 Nov 1994 08:49:37 GMT"
        );
        let status = response.status();
        let range = response
            .headers()
            .get(header::CONTENT_RANGE)
            .and_then(|v| v.to_str().ok())
            .map(str::to_owned);
        let body = String::from_utf8(test::read_body(response).await.to_vec()).unwrap();
        (status, body, range)
    }

    lets_expect! {
        expect(run_ntex(dated_range(condition))) as the_date_conditioned_file_range {
            let condition = "Sun, 06 Nov 1994 08:49:37 GMT";
            to serves_the_requested_bytes { equal((StatusCode::PARTIAL_CONTENT, "or".to_owned(), Some("bytes 1-2/6".to_owned()))) }
            when the_condition_date_is_earlier {
                let condition = "Sun, 06 Nov 1994 08:49:36 GMT";
                to serves_the_complete_representation { equal((StatusCode::OK, "world!".to_owned(), None)) }
            }
            when the_condition_date_is_later {
                let condition = "Sun, 06 Nov 1994 08:49:38 GMT";
                to serves_the_complete_representation { equal((StatusCode::OK, "world!".to_owned(), None)) }
            }
        }
    }
}

lets_expect! {
    expect(run_ntex(file_request(request))) as file_context_on_negotiation_failure {
        let method = HttpMethod::GET;
        let request = FileRequest { method, context: true, headers: vec![("accept-encoding", "identity;q=0,*;q=0")], ..Default::default() };
        to preserves_the_context_headers_on_get {
            have(status) equal(StatusCode::NOT_ACCEPTABLE), have(body) equal(String::new()),
            have(context) equal(Some("yes".to_owned())), have(vary) contains_token("accept-encoding".to_owned()),
            have(cookies) equal(vec!["session=one; HttpOnly".to_owned(), "csrf=two; SameSite=Lax".to_owned()])
        }
        when the_method_is_head {
            let method = HttpMethod::HEAD;
            to preserves_the_context_headers_on_head {
                have(status) equal(StatusCode::NOT_ACCEPTABLE), have(body) equal(String::new()),
                have(context) equal(Some("yes".to_owned())), have(vary) contains_token("accept-encoding".to_owned()),
                have(cookies) equal(vec!["session=one; HttpOnly".to_owned(), "csrf=two; SameSite=Lax".to_owned()])
            }
        }
    }
}
