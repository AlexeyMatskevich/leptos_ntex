//! Handlers built on a System without `RequestRuntime` answer 500, not panic.

use leptos::prelude::*;
use leptos_ntex_unofficial::{NtexRouteListing, file_and_error_handler, register_leptos_routes};
use leptos_router::{Method, SsrMode};
use lets_expect::*;
use ntex::{
    http::StatusCode,
    web::{App, test},
};

#[derive(Clone, Copy)]
enum Handler {
    Rendered,
    FileFallback,
    ServerFunction,
}

fn without_scope(handler: Handler) -> (StatusCode, StatusCode) {
    ntex::rt::System::new("plain", ntex::rt::DefaultRuntime).block_on(async move {
        let root = std::env::temp_dir().join(format!(
            "leptos_ntex_no_scope_{}_{}",
            std::process::id(),
            std::thread::current().name().unwrap_or("main")
        ));
        let _ = std::fs::create_dir_all(&root);
        let options = LeptosOptions::builder()
            .output_name("no_scope")
            .site_root(root.to_string_lossy().into_owned())
            .build();
        let routes = vec![NtexRouteListing::new(
            "/".to_owned(),
            SsrMode::Async,
            [Method::Get],
            vec![],
        )];
        let app = test::init_service(
            App::new()
                .state(options)
                .configure(|cfg| register_leptos_routes(cfg, routes, || view! { <p>"page"</p> }))
                .route(
                    "/api/{tail}*",
                    leptos_ntex_unofficial::handle_server_fns::<ntex::web::DefaultError>(),
                )
                .route(
                    "/{tail}*",
                    file_and_error_handler(|_: LeptosOptions| view! { <p>"missing"</p> }),
                ),
        )
        .await;
        let uri = match handler {
            Handler::Rendered => "/",
            Handler::FileFallback => "/missing.txt",
            Handler::ServerFunction => "/api/unknown",
        };
        let first = test::call_service(&app, test::TestRequest::with_uri(uri).to_request()).await;
        let first_status = first.status();
        let _ = test::read_body(first).await;
        // The worker survives: the next request is answered the same way.
        let second = test::call_service(&app, test::TestRequest::with_uri(uri).to_request()).await;
        let second_status = second.status();
        let _ = test::read_body(second).await;
        let _ = std::fs::remove_dir_all(&root);
        (first_status, second_status)
    })
}

lets_expect! {
    expect(without_scope(handler)) as a_handler_without_a_request_scope {
        let handler = Handler::Rendered;
        to answers_every_request_with_500 { equal((StatusCode::INTERNAL_SERVER_ERROR, StatusCode::INTERNAL_SERVER_ERROR)) }
        when the_file_fallback_renders_the_shell {
            let handler = Handler::FileFallback;
            to answers_every_request_with_500 { equal((StatusCode::INTERNAL_SERVER_ERROR, StatusCode::INTERNAL_SERVER_ERROR)) }
        }
        when a_server_function_is_dispatched {
            let handler = Handler::ServerFunction;
            to answers_the_lookup_before_needing_a_scope { equal((StatusCode::BAD_REQUEST, StatusCode::BAD_REQUEST)) }
        }
    }
}
