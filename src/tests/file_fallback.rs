use super::*;
use leptos::config::LeptosOptions;
use ntex::http::StatusCode;
use ntex::web::{App as NtexApp, test};

#[ntex::test]
async fn file_and_error_handler_serves_file_then_falls_back() {
    use crate::file_and_error_handler;

    let site_root = temp_site_root("file_handler");
    std::fs::create_dir_all(&site_root).unwrap();
    std::fs::write(site_root.join("hello.txt"), "world!").unwrap();

    let options = LeptosOptions::builder()
        .output_name("leptos_ntex_file_handler")
        .site_root(site_root.to_string_lossy().to_string())
        .site_pkg_dir("pkg")
        .build();

    let app = test::init_service(NtexApp::new().state(options.clone()).route(
        "/{tail}*",
        file_and_error_handler(|_opts: LeptosOptions| {
            view! { <h1>"Not Found Shell"</h1> }
        }),
    ))
    .await;

    let req = test::TestRequest::with_uri("/hello.txt").to_request();
    let resp = test::call_service(&app, req).await;
    assert_eq!(resp.status(), StatusCode::OK);
    let body = test::read_body(resp).await;
    assert_eq!(String::from_utf8(body.to_vec()).unwrap(), "world!");

    let req = test::TestRequest::with_uri("/missing").to_request();
    let resp = test::call_service(&app, req).await;
    assert_eq!(resp.status(), StatusCode::NOT_FOUND);
    let body = test::read_body(resp).await;
    let html = String::from_utf8(body.to_vec()).unwrap();
    assert!(html.contains("Not Found Shell"));

    let _ = std::fs::remove_dir_all(&site_root);
}

/// The catch-all must reach the handler for *nested* paths (multi-segment),
/// which the actix `/{tail:.*}` idiom did not in ntex — only `/{tail}*`
/// does. Pins: a nested asset and an RFC 8615 `.well-known/*` file are
/// served, while a top-level dotfile (`/.env`) stays hidden and a deep miss
/// renders the 404 shell. (A genuinely *nested* dotfile is covered by
/// `traversal_dotfile_in_subdirectory_rejected`.)
#[ntex::test]
async fn file_and_error_handler_serves_nested_paths_and_well_known() {
    use crate::file_and_error_handler;

    let site_root = temp_site_root("nested_paths");
    std::fs::create_dir_all(site_root.join("assets/css")).unwrap();
    std::fs::create_dir_all(site_root.join(".well-known/acme-challenge")).unwrap();
    std::fs::write(site_root.join("assets/css/app.css"), "body{color:red}").unwrap();
    std::fs::write(
        site_root.join(".well-known/acme-challenge/token"),
        "acme-proof",
    )
    .unwrap();
    std::fs::write(site_root.join(".env"), "API_KEY=secret").unwrap();

    let options = LeptosOptions::builder()
        .output_name("leptos_ntex_nested_paths")
        .site_root(site_root.to_string_lossy().to_string())
        .site_pkg_dir("pkg")
        .build();

    let app = test::init_service(NtexApp::new().state(options.clone()).route(
        "/{tail}*",
        file_and_error_handler(|_opts: LeptosOptions| view! { <h1>"Not Found Shell"</h1> }),
    ))
    .await;

    // Nested static asset (2 segments) is served.
    let resp = test::call_service(
        &app,
        test::TestRequest::with_uri("/assets/css/app.css").to_request(),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::OK);
    let body = test::read_body(resp).await;
    assert_eq!(String::from_utf8(body.to_vec()).unwrap(), "body{color:red}");

    // RFC 8615 well-known asset (3 segments, leading dot) is served.
    let resp = test::call_service(
        &app,
        test::TestRequest::with_uri("/.well-known/acme-challenge/token").to_request(),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::OK);
    let body = test::read_body(resp).await;
    assert_eq!(String::from_utf8(body.to_vec()).unwrap(), "acme-proof");

    // An ordinary dotfile is still hidden (renders the 404 shell).
    let resp = test::call_service(&app, test::TestRequest::with_uri("/.env").to_request()).await;
    assert_ne!(resp.status(), StatusCode::OK);
    let body = test::read_body(resp).await;
    let html = String::from_utf8(body.to_vec()).unwrap();
    assert!(!html.contains("API_KEY"), "dotfile leaked: {html}");

    // A nested miss falls back to the shell, not a bare router 404.
    let resp = test::call_service(
        &app,
        test::TestRequest::with_uri("/deep/missing/page").to_request(),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::NOT_FOUND);
    let body = test::read_body(resp).await;
    assert!(
        String::from_utf8(body.to_vec())
            .unwrap()
            .contains("Not Found Shell"),
        "nested miss must reach the handler and render the shell"
    );

    let _ = std::fs::remove_dir_all(&site_root);
}

#[ntex::test]
async fn file_and_error_handler_file_hit_applies_context_response_options() {
    use crate::file_and_error_handler_with_context;

    let site_root = temp_site_root("file_handler_context");
    std::fs::create_dir_all(&site_root).unwrap();
    std::fs::write(site_root.join("hello.txt"), "world!").unwrap();

    let options = LeptosOptions::builder()
        .output_name("leptos_ntex_file_handler_context")
        .site_root(site_root.to_string_lossy().to_string())
        .site_pkg_dir("pkg")
        .build();

    let app = test::init_service(NtexApp::new().state(options.clone()).route(
        "/{tail}*",
        file_and_error_handler_with_context(
            || {
                let res = use_context::<crate::ResponseOptions>()
                    .expect("ResponseOptions should be provided on file hits");
                res.insert_header(
                    ntex::http::header::HeaderName::from_static("x-file-hit"),
                    ntex::http::header::HeaderValue::from_static("yes"),
                );
            },
            |_opts: LeptosOptions| view! { <h1>"Not Found Shell"</h1> },
        ),
    ))
    .await;

    let resp =
        test::call_service(&app, test::TestRequest::with_uri("/hello.txt").to_request()).await;
    assert_eq!(resp.status(), StatusCode::OK);
    assert_eq!(
        resp.headers()
            .get("x-file-hit")
            .and_then(|v| v.to_str().ok()),
        Some("yes")
    );

    let _ = std::fs::remove_dir_all(&site_root);
}

/// `additional_context` must also run on the 404/miss branch (after
/// `provide_contexts` has already provided `ResponseOptions`), not only on a
/// file hit — the doc comment on `file_and_error_handler_with_context`
/// states the miss path as its primary documented purpose. A regression
/// that dropped or reordered the call on that branch would silently stop
/// setting the header on 404s while this exact closure kept working on
/// hits.
#[ntex::test]
async fn file_and_error_handler_miss_applies_context_response_options() {
    use crate::file_and_error_handler_with_context;

    let site_root = temp_site_root("file_handler_context_miss");
    std::fs::create_dir_all(&site_root).unwrap();

    let options = LeptosOptions::builder()
        .output_name("leptos_ntex_file_handler_context_miss")
        .site_root(site_root.to_string_lossy().to_string())
        .site_pkg_dir("pkg")
        .build();

    let app = test::init_service(NtexApp::new().state(options.clone()).route(
        "/{tail}*",
        file_and_error_handler_with_context(
            || {
                let res = use_context::<crate::ResponseOptions>()
                    .expect("ResponseOptions should be provided on a miss too");
                res.insert_header(
                    ntex::http::header::HeaderName::from_static("x-miss-hit"),
                    ntex::http::header::HeaderValue::from_static("yes"),
                );
            },
            |_opts: LeptosOptions| view! { <h1>"Not Found Shell"</h1> },
        ),
    ))
    .await;

    let resp = test::call_service(&app, test::TestRequest::with_uri("/missing").to_request()).await;
    assert_eq!(resp.status(), StatusCode::NOT_FOUND);
    assert_eq!(
        resp.headers()
            .get("x-miss-hit")
            .and_then(|v| v.to_str().ok()),
        Some("yes"),
        "additional_context must also fire on the 404 shell branch"
    );
    let body = test::read_body(resp).await;
    assert!(
        String::from_utf8(body.to_vec())
            .unwrap()
            .contains("Not Found Shell")
    );

    let _ = std::fs::remove_dir_all(&site_root);
}

/// Builds the shared `app.js` + `.br` + `.gz` fixture used by the three
/// encoding-negotiation tests below, under a fresh `site_root`.
fn precompressed_site_root(name: &str) -> std::path::PathBuf {
    let site_root = temp_site_root(name);
    std::fs::create_dir_all(&site_root).unwrap();
    std::fs::write(site_root.join("app.js"), "console.log('plain');").unwrap();
    std::fs::write(site_root.join("app.js.br"), "br-bytes").unwrap();
    std::fs::write(site_root.join("app.js.gz"), "gzip-bytes").unwrap();
    site_root
}

#[ntex::test]
async fn file_and_error_handler_serves_br_with_original_mime() {
    use crate::file_and_error_handler;

    let site_root = precompressed_site_root("file_handler_br");

    let options = LeptosOptions::builder()
        .output_name("leptos_ntex_file_handler_br")
        .site_root(site_root.to_string_lossy().to_string())
        .site_pkg_dir("pkg")
        .build();

    let app = test::init_service(NtexApp::new().state(options.clone()).route(
        "/{tail}*",
        file_and_error_handler(|_opts: LeptosOptions| {
            view! { <h1>"Not Found Shell"</h1> }
        }),
    ))
    .await;

    let resp = test::call_service(
        &app,
        test::TestRequest::with_uri("/app.js")
            .header(ntex::http::header::ACCEPT_ENCODING, "br")
            .to_request(),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::OK);
    assert_eq!(
        resp.headers()
            .get(ntex::http::header::CONTENT_ENCODING)
            .and_then(|v| v.to_str().ok()),
        Some("br")
    );
    assert_eq!(
        resp.headers()
            .get(ntex::http::header::CONTENT_TYPE)
            .and_then(|v| v.to_str().ok()),
        Some("text/javascript")
    );
    let vary = resp
        .headers()
        .get(ntex::http::header::VARY)
        .and_then(|v| v.to_str().ok())
        .unwrap_or_default();
    // Token-match, not substring: a malformed value like `X-Accept-Encoding`
    // contains the substring but is not the `Accept-Encoding` token.
    assert!(
        vary.split(',')
            .any(|v| v.trim().eq_ignore_ascii_case("Accept-Encoding")),
        "Vary must list the Accept-Encoding token, got {vary:?}"
    );
    let body = test::read_body(resp).await;
    assert_eq!(String::from_utf8(body.to_vec()).unwrap(), "br-bytes");

    let _ = std::fs::remove_dir_all(&site_root);
}

/// All three encodings explicitly refused (`br;q=0, gzip;q=0`) with a
/// wildcard also offered (`*;q=1`) must NOT backfill either, since an
/// explicit refusal outranks the wildcard — so the plain, uncompressed file
/// is served, with its original Content-Type intact.
#[ntex::test]
async fn file_and_error_handler_falls_back_to_plain_file_when_all_encodings_refused() {
    use crate::file_and_error_handler;

    let site_root = precompressed_site_root("file_handler_plain_fallback");

    let options = LeptosOptions::builder()
        .output_name("leptos_ntex_file_handler_plain_fallback")
        .site_root(site_root.to_string_lossy().to_string())
        .site_pkg_dir("pkg")
        .build();

    let app = test::init_service(NtexApp::new().state(options.clone()).route(
        "/{tail}*",
        file_and_error_handler(|_opts: LeptosOptions| {
            view! { <h1>"Not Found Shell"</h1> }
        }),
    ))
    .await;

    let resp = test::call_service(
        &app,
        test::TestRequest::with_uri("/app.js")
            .header(
                ntex::http::header::ACCEPT_ENCODING,
                "br;q=0, gzip;q=0, *;q=1",
            )
            .to_request(),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::OK);
    assert!(
        resp.headers()
            .get(ntex::http::header::CONTENT_ENCODING)
            .is_none()
    );
    assert_eq!(
        resp.headers()
            .get(ntex::http::header::CONTENT_TYPE)
            .and_then(|v| v.to_str().ok()),
        Some("text/javascript"),
        "the plain-file fallback must still report the original MIME"
    );
    let body = test::read_body(resp).await;
    assert_eq!(
        String::from_utf8(body.to_vec()).unwrap(),
        "console.log('plain');"
    );

    let _ = std::fs::remove_dir_all(&site_root);
}

/// Only `gzip` is accepted (no `br` token at all) — the gzip sibling is
/// served, and its Content-Type is still the original file's MIME, not the
/// compressed sibling's.
#[ntex::test]
async fn file_and_error_handler_serves_gzip_when_only_gzip_is_accepted() {
    use crate::file_and_error_handler;

    let site_root = precompressed_site_root("file_handler_gzip_only");

    let options = LeptosOptions::builder()
        .output_name("leptos_ntex_file_handler_gzip_only")
        .site_root(site_root.to_string_lossy().to_string())
        .site_pkg_dir("pkg")
        .build();

    let app = test::init_service(NtexApp::new().state(options.clone()).route(
        "/{tail}*",
        file_and_error_handler(|_opts: LeptosOptions| {
            view! { <h1>"Not Found Shell"</h1> }
        }),
    ))
    .await;

    let resp = test::call_service(
        &app,
        test::TestRequest::with_uri("/app.js")
            .header(ntex::http::header::ACCEPT_ENCODING, "gzip;q=0.1")
            .to_request(),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::OK);
    assert_eq!(
        resp.headers()
            .get(ntex::http::header::CONTENT_ENCODING)
            .and_then(|v| v.to_str().ok()),
        Some("gzip")
    );
    assert_eq!(
        resp.headers()
            .get(ntex::http::header::CONTENT_TYPE)
            .and_then(|v| v.to_str().ok()),
        Some("text/javascript"),
        "the gzip sibling must still report the original MIME"
    );
    let body = test::read_body(resp).await;
    assert_eq!(String::from_utf8(body.to_vec()).unwrap(), "gzip-bytes");

    let _ = std::fs::remove_dir_all(&site_root);
}

/// A client whose q-weights rank gzip ABOVE brotli must receive the gzip
/// sibling even though a `.br` sibling exists — the fixed br-then-gz probe
/// order must not override the client's stated preference. (The q-ordering
/// matrix itself is pinned by the `precompressed_preference` spec in
/// `files.rs`; this leaf pins the end-to-end wiring through the handler.)
#[ntex::test]
async fn file_and_error_handler_honours_gzip_preference_over_br() {
    use crate::file_and_error_handler;

    let site_root = temp_site_root("file_handler_gzip_pref");
    std::fs::create_dir_all(&site_root).unwrap();
    std::fs::write(site_root.join("app.js"), "console.log('plain');").unwrap();
    std::fs::write(site_root.join("app.js.br"), "br-bytes").unwrap();
    std::fs::write(site_root.join("app.js.gz"), "gzip-bytes").unwrap();

    let options = LeptosOptions::builder()
        .output_name("leptos_ntex_file_handler_gzip_pref")
        .site_root(site_root.to_string_lossy().to_string())
        .site_pkg_dir("pkg")
        .build();

    let app = test::init_service(NtexApp::new().state(options.clone()).route(
        "/{tail}*",
        file_and_error_handler(|_opts: LeptosOptions| {
            view! { <h1>"Not Found Shell"</h1> }
        }),
    ))
    .await;

    let resp = test::call_service(
        &app,
        test::TestRequest::with_uri("/app.js")
            .header(ntex::http::header::ACCEPT_ENCODING, "gzip;q=1, br;q=0.1")
            .to_request(),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::OK);
    assert_eq!(
        resp.headers()
            .get(ntex::http::header::CONTENT_ENCODING)
            .and_then(|v| v.to_str().ok()),
        Some("gzip"),
        "gzip;q=1 must outrank br;q=0.1"
    );
    let body = test::read_body(resp).await;
    assert_eq!(String::from_utf8(body.to_vec()).unwrap(), "gzip-bytes");

    let _ = std::fs::remove_dir_all(&site_root);
}

/// Builds a shell-only app with `file_and_error_handler` rooted at
/// `site_root`, suitable for traversal assertions.
macro_rules! traversal_app {
    ($site_root:expr) => {{
        use crate::file_and_error_handler;
        let options = LeptosOptions::builder()
            .output_name("leptos_ntex_traversal")
            .site_root($site_root.to_string_lossy().to_string())
            .site_pkg_dir("pkg")
            .build();
        test::init_service(NtexApp::new().state(options).route(
            "/{tail}*",
            file_and_error_handler(|_opts: LeptosOptions| {
                view! { <h1>"Shell"</h1> }
            }),
        ))
        .await
    }};
}

/// Verifies that relative-parent traversal does not escape `site_root`.
/// Writes a "secret" file *outside* the root but inside its parent, then
/// checks that `/../secret.txt` returns the shell rather than file
/// contents.
#[ntex::test]
async fn traversal_relative_parent_rejected() {
    let parent = temp_site_root("traversal_parent");
    std::fs::create_dir_all(&parent).unwrap();
    std::fs::write(parent.join("secret.txt"), "SECRET").unwrap();
    let site_root = parent.join("public");
    std::fs::create_dir_all(&site_root).unwrap();

    let app = traversal_app!(&site_root);
    let req = test::TestRequest::with_uri("/../secret.txt").to_request();
    let resp = test::call_service(&app, req).await;
    let status = resp.status();
    let body = test::read_body(resp).await;
    let text = String::from_utf8(body.to_vec()).unwrap();
    // Full safe-rejection contract (status + shell + no secret) — secret
    // absence alone would also pass on a 500 or an empty body.
    assert_eq!(
        status,
        StatusCode::NOT_FOUND,
        "traversal must fall back to the 404 shell, got body = {text:?}"
    );
    assert!(text.contains("Shell"), "must render the fallback shell");
    assert!(
        !text.contains("SECRET"),
        "traversal leaked: body = {text:?}"
    );

    let _ = std::fs::remove_dir_all(&parent);
}

/// Percent-encoded `..` (`%2e%2e`) must not bypass the traversal filter.
#[ntex::test]
async fn traversal_percent_encoded_parent_rejected() {
    let parent = temp_site_root("traversal_pct");
    std::fs::create_dir_all(&parent).unwrap();
    std::fs::write(parent.join("secret.txt"), "PCT_SECRET").unwrap();
    let site_root = parent.join("public");
    std::fs::create_dir_all(&site_root).unwrap();

    let app = traversal_app!(&site_root);
    let req = test::TestRequest::with_uri("/%2e%2e/secret.txt").to_request();
    let resp = test::call_service(&app, req).await;
    // The full safe-rejection contract, not just secret absence: a 500 or an
    // empty response must NOT pass for a correct rejection. The handler falls
    // back to the 404 shell on a rejected path.
    assert_eq!(resp.status(), StatusCode::NOT_FOUND);
    let body = test::read_body(resp).await;
    let text = String::from_utf8(body.to_vec()).unwrap();
    assert!(text.contains("Shell"), "must render the fallback shell");
    assert!(!text.contains("PCT_SECRET"));

    let _ = std::fs::remove_dir_all(&parent);
}

/// A root-style URI (leading `/etc/…`) must not pull files from the
/// real `/etc` — `Path::join` replacement of the root is the classic
/// exploit vector. Our `safe_subpath` reconstructs the path from split
/// segments so a bare `/etc/passwd` resolves under `<site_root>/etc/…`.
#[ntex::test]
async fn traversal_absolute_path_rejected() {
    let site_root = temp_site_root("traversal_abs");
    std::fs::create_dir_all(&site_root).unwrap();

    let app = traversal_app!(&site_root);
    let req = test::TestRequest::with_uri("/etc/passwd").to_request();
    let resp = test::call_service(&app, req).await;
    let status = resp.status();
    let body = test::read_body(resp).await;
    let text = String::from_utf8(body.to_vec()).unwrap();
    // Full safe-rejection contract (status + shell + no secret) — secret
    // absence alone would also pass on a 500 or an empty body.
    assert_eq!(status, StatusCode::NOT_FOUND);
    assert!(text.contains("Shell"), "must render the fallback shell");
    assert!(!text.contains("root:"), "leaked /etc/passwd: {text:?}");

    let _ = std::fs::remove_dir_all(&site_root);
}

/// Dotfiles (`.env`, `.htaccess`) must not be served by the fallback
/// handler — matches the convention established by `ntex_files::Files`.
#[ntex::test]
async fn traversal_dotfile_rejected() {
    let site_root = temp_site_root("traversal_dot");
    std::fs::create_dir_all(&site_root).unwrap();
    std::fs::write(site_root.join(".env"), "API_KEY=secret").unwrap();

    let app = traversal_app!(&site_root);
    let req = test::TestRequest::with_uri("/.env").to_request();
    let resp = test::call_service(&app, req).await;
    // Full safe-rejection contract (status + shell + no secret) — secret
    // absence alone would also pass on a 500 or an empty body.
    assert_eq!(resp.status(), StatusCode::NOT_FOUND);
    let body = test::read_body(resp).await;
    let text = String::from_utf8(body.to_vec()).unwrap();
    assert!(text.contains("Shell"), "must render the fallback shell");
    assert!(!text.contains("API_KEY"));

    let _ = std::fs::remove_dir_all(&site_root);
}

/// A NUL byte in a path segment must be rejected outright — NUL is
/// illegal in POSIX paths and typically signals a smuggling attempt. This is
/// defense-in-depth: the OS would also reject a NUL path, so the test cannot
/// fully ISOLATE `safe_subpath`'s `contains('\0')` guard from the OS rejection
/// — but it pins the full safe-rejection contract (404 + shell + no leak of
/// the sibling file), so a regression to a 500 or an empty body is still
/// caught.
#[ntex::test]
async fn traversal_null_byte_rejected() {
    let site_root = temp_site_root("traversal_nul");
    std::fs::create_dir_all(&site_root).unwrap();
    std::fs::write(site_root.join("ok.txt"), "NUL_SENTINEL_BODY").unwrap();

    let app = traversal_app!(&site_root);
    let req = test::TestRequest::with_uri("/ok%00hidden.txt").to_request();
    let resp = test::call_service(&app, req).await;
    assert_eq!(resp.status(), StatusCode::NOT_FOUND);
    let body = test::read_body(resp).await;
    let text = String::from_utf8(body.to_vec()).unwrap();
    assert!(text.contains("Shell"), "must render the fallback shell");
    assert!(
        !text.contains("NUL_SENTINEL_BODY"),
        "must not serve the sibling file body"
    );

    let _ = std::fs::remove_dir_all(&site_root);
}

/// Symlink escape: a symlink inside `site_root` pointing outside must
/// not leak external files. `canonicalize()` + `starts_with(canon_root)`
/// catches this.
#[cfg(unix)]
#[ntex::test]
async fn traversal_symlink_escape_rejected() {
    let parent = temp_site_root("traversal_symlink");
    std::fs::create_dir_all(&parent).unwrap();
    std::fs::write(parent.join("outside.txt"), "OUTSIDE").unwrap();
    let site_root = parent.join("public");
    std::fs::create_dir_all(&site_root).unwrap();
    // Must NOT be `let _ =`: if symlink creation fails, `/escape.txt` would be
    // a missing file and the 404-shell-no-leak assertions below would all pass
    // vacuously WITHOUT ever exercising the symlink-escape boundary. Fail loud
    // instead, and confirm the link really is a symlink before the request.
    std::os::unix::fs::symlink(parent.join("outside.txt"), site_root.join("escape.txt"))
        .expect("symlink fixture must be created for this test to be meaningful");
    assert!(
        std::fs::symlink_metadata(site_root.join("escape.txt"))
            .unwrap()
            .file_type()
            .is_symlink(),
        "escape.txt must be a symlink pointing outside the root before the request"
    );

    let app = traversal_app!(&site_root);
    let req = test::TestRequest::with_uri("/escape.txt").to_request();
    let resp = test::call_service(&app, req).await;
    // Full safe-rejection contract (status + shell + no secret) — secret
    // absence alone would also pass on a 500 or an empty body.
    assert_eq!(resp.status(), StatusCode::NOT_FOUND);
    let body = test::read_body(resp).await;
    let text = String::from_utf8(body.to_vec()).unwrap();
    assert!(text.contains("Shell"), "must render the fallback shell");
    assert!(!text.contains("OUTSIDE"), "symlink escape leaked: {text:?}");

    let _ = std::fs::remove_dir_all(&parent);
}

/// A dotfile nested inside a subdirectory must be rejected.
#[ntex::test]
async fn traversal_dotfile_in_subdirectory_rejected() {
    let site_root = temp_site_root("dotfile_subdir");
    std::fs::create_dir_all(site_root.join("subdir")).unwrap();
    std::fs::write(site_root.join("subdir/.env"), "SECRET=abc").unwrap();

    let app = traversal_app!(&site_root);

    let req = test::TestRequest::with_uri("/subdir/.env").to_request();
    let resp = test::call_service(&app, req).await;
    // Full safe-rejection contract (status + shell + no secret) — secret
    // absence alone would also pass on a 500 or an empty body.
    assert_eq!(resp.status(), StatusCode::NOT_FOUND);
    let body = test::read_body(resp).await;
    let text = String::from_utf8(body.to_vec()).unwrap();
    assert!(text.contains("Shell"), "must render the fallback shell");
    assert!(
        !text.contains("SECRET"),
        "dotfile in subdirectory must not be served"
    );

    let _ = std::fs::remove_dir_all(&site_root);
}

#[ntex::test]
async fn traversal_encoded_slash_dotfile_rejected() {
    let site_root = temp_site_root("dotfile_encoded_slash");
    std::fs::create_dir_all(site_root.join("subdir")).unwrap();
    std::fs::write(site_root.join("subdir/.env"), "SECRET=encoded").unwrap();

    let app = traversal_app!(&site_root);

    let req = test::TestRequest::with_uri("/subdir%2F.env").to_request();
    let resp = test::call_service(&app, req).await;
    // Full safe-rejection contract (status + shell + no secret) — secret
    // absence alone would also pass on a 500 or an empty body.
    assert_eq!(resp.status(), StatusCode::NOT_FOUND);
    let body = test::read_body(resp).await;
    let text = String::from_utf8(body.to_vec()).unwrap();
    assert!(text.contains("Shell"), "must render the fallback shell");
    assert!(
        !text.contains("SECRET=encoded"),
        "encoded slash must not bypass dotfile filtering"
    );

    let _ = std::fs::remove_dir_all(&site_root);
}

/// The route is registered with a union guard
/// (`Any(Get()).or(Head())`) specifically so HEAD mirrors GET — the doc
/// comment on `file_and_error_handler_with_context` calls this out
/// explicitly because `.method()` AND-combines incompatibly across two
/// methods. A regression back to an AND-combined method guard would
/// silently stop matching HEAD for the entire fallback route. (Wire-level
/// body elision is ntex's h1 writer's job and is not observable through
/// `test::call_service`, which calls the service directly — so this test
/// pins status + Content-Type/Content-Length parity with GET, matching the
/// convention established by `head_request_on_static_route_mirrors_get` in
/// `static_routes.rs`.)
#[ntex::test]
async fn file_and_error_handler_head_mirrors_get_on_a_file_hit() {
    use crate::file_and_error_handler;

    let site_root = temp_site_root("head_file_hit");
    std::fs::create_dir_all(&site_root).unwrap();
    std::fs::write(site_root.join("hello.txt"), "world!").unwrap();

    let options = LeptosOptions::builder()
        .output_name("leptos_ntex_head_file_hit")
        .site_root(site_root.to_string_lossy().to_string())
        .site_pkg_dir("pkg")
        .build();

    let app = test::init_service(NtexApp::new().state(options.clone()).route(
        "/{tail}*",
        file_and_error_handler(|_opts: LeptosOptions| {
            view! { <h1>"Not Found Shell"</h1> }
        }),
    ))
    .await;

    let get_resp =
        test::call_service(&app, test::TestRequest::with_uri("/hello.txt").to_request()).await;
    let get_status = get_resp.status();
    let get_content_type = get_resp
        .headers()
        .get(ntex::http::header::CONTENT_TYPE)
        .cloned();
    let get_content_length = get_resp
        .headers()
        .get(ntex::http::header::CONTENT_LENGTH)
        .cloned();

    let head_resp = test::call_service(
        &app,
        test::TestRequest::with_uri("/hello.txt")
            .method(ntex::http::Method::HEAD)
            .to_request(),
    )
    .await;

    assert_eq!(head_resp.status(), StatusCode::OK);
    assert_eq!(head_resp.status(), get_status);
    assert_eq!(
        head_resp.headers().get(ntex::http::header::CONTENT_TYPE),
        get_content_type.as_ref(),
        "HEAD must report the same Content-Type as GET"
    );
    assert_eq!(
        head_resp.headers().get(ntex::http::header::CONTENT_LENGTH),
        get_content_length.as_ref(),
        "HEAD must report the same Content-Length as GET"
    );

    let _ = std::fs::remove_dir_all(&site_root);
}

/// HEAD must also mirror GET on the 404/miss branch — same status and
/// Content-Type as the equivalent GET request.
#[ntex::test]
async fn file_and_error_handler_head_mirrors_get_on_a_miss() {
    use crate::file_and_error_handler;

    let site_root = temp_site_root("head_file_miss");
    std::fs::create_dir_all(&site_root).unwrap();

    let options = LeptosOptions::builder()
        .output_name("leptos_ntex_head_file_miss")
        .site_root(site_root.to_string_lossy().to_string())
        .site_pkg_dir("pkg")
        .build();

    let app = test::init_service(NtexApp::new().state(options.clone()).route(
        "/{tail}*",
        file_and_error_handler(|_opts: LeptosOptions| {
            view! { <h1>"Not Found Shell"</h1> }
        }),
    ))
    .await;

    let get_resp =
        test::call_service(&app, test::TestRequest::with_uri("/missing").to_request()).await;
    let get_status = get_resp.status();
    let get_content_type = get_resp
        .headers()
        .get(ntex::http::header::CONTENT_TYPE)
        .cloned();

    let head_resp = test::call_service(
        &app,
        test::TestRequest::with_uri("/missing")
            .method(ntex::http::Method::HEAD)
            .to_request(),
    )
    .await;
    assert_eq!(head_resp.status(), StatusCode::NOT_FOUND);
    assert_eq!(head_resp.status(), get_status);
    assert_eq!(
        head_resp.headers().get(ntex::http::header::CONTENT_TYPE),
        get_content_type.as_ref(),
        "HEAD must report the same Content-Type as GET on a miss"
    );

    let _ = std::fs::remove_dir_all(&site_root);
}

/// A disallowed method (POST) must not be served by the file/shell logic at
/// all — it should fall through at the router level (a bare 405/404, no
/// shell body), not render the 404 shell as if it were a GET/HEAD miss.
#[ntex::test]
async fn file_and_error_handler_rejects_post() {
    use crate::file_and_error_handler;

    let site_root = temp_site_root("post_rejected");
    std::fs::create_dir_all(&site_root).unwrap();
    std::fs::write(site_root.join("hello.txt"), "world!").unwrap();

    let options = LeptosOptions::builder()
        .output_name("leptos_ntex_post_rejected")
        .site_root(site_root.to_string_lossy().to_string())
        .site_pkg_dir("pkg")
        .build();

    let app = test::init_service(NtexApp::new().state(options.clone()).route(
        "/{tail}*",
        file_and_error_handler(|_opts: LeptosOptions| {
            view! { <h1>"Not Found Shell"</h1> }
        }),
    ))
    .await;

    let resp = test::call_service(
        &app,
        test::TestRequest::with_uri("/hello.txt")
            .method(ntex::http::Method::POST)
            .to_request(),
    )
    .await;
    // The union guard (`Any(Get()).or(Head())`) simply doesn't match POST, so
    // the request falls through to the app's default "no route matched"
    // handling, which is a bare 404 (ntex only returns 405 if a route path
    // matched but every guard on it rejected the method AND a `Method Not
    // Allowed` responder were wired up — this app registers no such thing).
    // Pinning the exact code (not just `!= OK`) catches a regression that
    // swapped in some other non-200 status (e.g. a 500) and still "rejected"
    // the request without truly falling through to the router's default.
    assert_eq!(
        resp.status(),
        StatusCode::NOT_FOUND,
        "POST must not be served by the file/shell handler"
    );
    let body = test::read_body(resp).await;
    let text = String::from_utf8_lossy(&body);
    assert!(
        !text.contains("Not Found Shell"),
        "POST must not fall through to the shell render path, got body = {text:?}"
    );

    let _ = std::fs::remove_dir_all(&site_root);
}

/// Regression: `safe_subpath` used to skip every empty split segment of a
/// bare root, leaving `rel` empty so `candidate` resolved to `canon_root`
/// itself — a directory. `canonicalize()` and `starts_with` both accepted it
/// trivially, and `NamedFile::open` succeeds on a directory on Unix, so a
/// bare-root request would have been served as if it were an openable file
/// instead of falling through to the 404 shell. Fixed by rejecting any
/// `safe_subpath` target that is not a regular file.
#[ntex::test]
async fn file_and_error_handler_bare_root_falls_back_to_shell() {
    use crate::file_and_error_handler;

    let site_root = temp_site_root("bare_root");
    std::fs::create_dir_all(&site_root).unwrap();
    std::fs::write(site_root.join("hello.txt"), "world!").unwrap();

    let options = LeptosOptions::builder()
        .output_name("leptos_ntex_bare_root")
        .site_root(site_root.to_string_lossy().to_string())
        .site_pkg_dir("pkg")
        .build();

    let app = test::init_service(NtexApp::new().state(options.clone()).route(
        "/{tail}*",
        file_and_error_handler(|_opts: LeptosOptions| {
            view! { <h1>"Not Found Shell"</h1> }
        }),
    ))
    .await;

    let resp = test::call_service(&app, test::TestRequest::with_uri("/").to_request()).await;
    assert_eq!(
        resp.status(),
        StatusCode::NOT_FOUND,
        "a bare root request must render the 404 shell, not open the site_root directory"
    );
    let body = test::read_body(resp).await;
    let text = String::from_utf8(body.to_vec()).unwrap();
    assert!(
        text.contains("Not Found Shell"),
        "must render the fallback shell"
    );

    let _ = std::fs::remove_dir_all(&site_root);
}
