//! HEAD requests never evaluate the body the response would carry.

use leptos::prelude::*;
use lets_expect::*;
use ntex::{
    http::Method,
    web::{App, test},
};
use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
};

/// A streamed SSR route with a deferred fragment: GET must render it, HEAD
/// must return the same status without ever running the deferred work.
/// The fragment waits for an external release, like a slow resource, so that
/// neither the synchronous render nor the first-chunk collection can finish
/// it; only the body stream can.
async fn deferred_fragment(method: Method) -> (u16, bool, usize) {
    let rendered = Arc::new(AtomicBool::new(false));
    let (release_tx, release_rx) = futures::channel::oneshot::channel::<()>();
    let release_rx = Arc::new(std::sync::Mutex::new(Some(release_rx)));
    let flag = rendered.clone();
    let app_fn = move || {
        let flag = flag.clone();
        let release_rx = release_rx.lock().unwrap().take();
        view! {
            <Suspense fallback=|| "loading">
                {Suspend::new(async move {
                    if let Some(release) = release_rx {
                        let _ = release.await;
                    }
                    flag.store(true, Ordering::SeqCst);
                    "deferred"
                })}
            </Suspense>
        }
    };
    let app = test::init_service(
        App::new()
            .state(LeptosOptions::builder().output_name("head").build())
            .route(
                "/",
                crate::render_app_to_stream::<_, ntex::web::DefaultError>(
                    app_fn,
                    leptos_router::Method::Get,
                ),
            ),
    )
    .await;
    let request = test::TestRequest::with_uri("/").method(method).to_request();
    let response = test::call_service(&app, request).await;
    let status = response.status().as_u16();
    // Headers are out; from here only the body stream can drive the fragment.
    let _ = release_tx.send(());
    let body = test::read_body(response).await;
    (status, rendered.load(Ordering::SeqCst), body.len())
}

fn head_skips_the_body((status, rendered, length): &(u16, bool, usize)) -> AssertionResult {
    if *status == 200 && !*rendered && *length == 0 {
        Ok(())
    } else {
        Err(AssertionError::new(vec![format!(
            "expected 200 with no deferred work and an empty body, observed status {status}, rendered {rendered}, {length} body bytes"
        )]))
    }
}
fn get_renders_the_fragment((status, rendered, length): &(u16, bool, usize)) -> AssertionResult {
    if *status == 200 && *rendered && *length > 0 {
        Ok(())
    } else {
        Err(AssertionError::new(vec![format!(
            "expected 200 with the deferred fragment rendered, observed status {status}, rendered {rendered}, {length} body bytes"
        )]))
    }
}

lets_expect! {
    expect(crate::tests::run_ntex(deferred_fragment(method))) as the_streamed_route {
        let method = Method::HEAD;
        to answers_head_without_running_deferred_work { head_skips_the_body }
        when the_method_is_get {
            let method = Method::GET;
            to renders_the_deferred_fragment { get_renders_the_fragment }
        }
    }
}
