//! Diagnostic-only ntex test-server stress probe.
//!
//! This intentionally avoids all leptos_ntex_unofficial code. The goal is to
//! check whether the flaky first-request failures reproduce with only
//! `ntex::web::test::server` instances running under the default parallel Rust
//! test harness on GitHub's io-uring runner.

use ntex::http::{Method, StatusCode};
use ntex::web::{self, App, HttpResponse, test};

fn server_url(srv: &test::TestServer, uri: &str) -> String {
    // Avoid TestServer::url(), which goes through localhost and can mix in
    // resolver / IPv6 noise. The integration tests use the same direct addr.
    if uri.starts_with('/') {
        format!("http://{}{}", srv.addr(), uri)
    } else {
        format!("http://{}/{}", srv.addr(), uri)
    }
}

async fn one_round(name: &'static str) {
    let srv = test::server(|| async {
        App::new().service(web::resource("/").to(|| async { HttpResponse::Ok().body("ok") }))
    })
    .await;

    let url = server_url(&srv, "/");
    eprintln!("{name}: server addr={}, url={url}", srv.addr());

    let mut resp = srv.request(Method::GET, url).send().await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK);

    let body = resp.body().await.unwrap();
    assert_eq!(body.as_ref(), b"ok");
}

macro_rules! define_parallel_probe {
    ($($name:ident),+ $(,)?) => {
        $(
            #[ntex::test]
            async fn $name() {
                one_round(stringify!($name)).await;
            }
        )+
    };
}

define_parallel_probe!(
    ntex_test_server_parallel_00,
    ntex_test_server_parallel_01,
    ntex_test_server_parallel_02,
    ntex_test_server_parallel_03,
    ntex_test_server_parallel_04,
    ntex_test_server_parallel_05,
    ntex_test_server_parallel_06,
    ntex_test_server_parallel_07,
    ntex_test_server_parallel_08,
    ntex_test_server_parallel_09,
    ntex_test_server_parallel_10,
    ntex_test_server_parallel_11,
    ntex_test_server_parallel_12,
    ntex_test_server_parallel_13,
    ntex_test_server_parallel_14,
    ntex_test_server_parallel_15,
);
