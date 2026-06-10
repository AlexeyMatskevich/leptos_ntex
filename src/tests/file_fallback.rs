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
/// served, while a nested dotfile stays hidden and a deep miss renders the
/// 404 shell.
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

#[ntex::test]
async fn file_and_error_handler_serves_precompressed_br_with_original_mime() {
    use crate::file_and_error_handler;

    let site_root = temp_site_root("file_handler_br");
    std::fs::create_dir_all(&site_root).unwrap();
    std::fs::write(site_root.join("app.js"), "console.log('plain');").unwrap();
    std::fs::write(site_root.join("app.js.br"), "br-bytes").unwrap();
    std::fs::write(site_root.join("app.js.gz"), "gzip-bytes").unwrap();

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
    assert!(vary.contains("Accept-Encoding"));
    let body = test::read_body(resp).await;
    assert_eq!(String::from_utf8(body.to_vec()).unwrap(), "br-bytes");

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
    let body = test::read_body(resp).await;
    assert_eq!(
        String::from_utf8(body.to_vec()).unwrap(),
        "console.log('plain');"
    );

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
    assert_ne!(
        status,
        StatusCode::OK,
        "traversal must not return 200, got body = {text:?}"
    );
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
    let body = test::read_body(resp).await;
    let text = String::from_utf8(body.to_vec()).unwrap();
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
    assert_ne!(status, StatusCode::OK);
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
    let body = test::read_body(resp).await;
    let text = String::from_utf8(body.to_vec()).unwrap();
    assert!(!text.contains("API_KEY"));

    let _ = std::fs::remove_dir_all(&site_root);
}

/// A NUL byte in a path segment must be rejected outright — NUL is
/// illegal in POSIX paths and typically signals a smuggling attempt.
#[ntex::test]
async fn traversal_null_byte_rejected() {
    let site_root = temp_site_root("traversal_nul");
    std::fs::create_dir_all(&site_root).unwrap();
    std::fs::write(site_root.join("ok.txt"), "ok").unwrap();

    let app = traversal_app!(&site_root);
    let req = test::TestRequest::with_uri("/ok%00hidden.txt").to_request();
    let resp = test::call_service(&app, req).await;
    assert_eq!(resp.status(), StatusCode::NOT_FOUND);

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
    let _ = std::os::unix::fs::symlink(parent.join("outside.txt"), site_root.join("escape.txt"));

    let app = traversal_app!(&site_root);
    let req = test::TestRequest::with_uri("/escape.txt").to_request();
    let resp = test::call_service(&app, req).await;
    let body = test::read_body(resp).await;
    let text = String::from_utf8(body.to_vec()).unwrap();
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
    let body = test::read_body(resp).await;
    let text = String::from_utf8(body.to_vec()).unwrap();
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
    let body = test::read_body(resp).await;
    let text = String::from_utf8(body.to_vec()).unwrap();
    assert!(
        !text.contains("SECRET=encoded"),
        "encoded slash must not bypass dotfile filtering"
    );

    let _ = std::fs::remove_dir_all(&site_root);
}
