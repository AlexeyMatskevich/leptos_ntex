//! Adapter context consumers when a managed request becomes unavailable.

use leptos::prelude::*;
use leptos_ntex_unofficial::{Request, RequestScope, ResponseOptions, extract, redirect};
use lets_expect::*;
use ntex::{
    http::{Payload, StatusCode, header},
    web::{DefaultError, FromRequest, HttpRequest, test},
};
use std::{
    convert::Infallible,
    sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    },
};

#[derive(Clone, Copy)]
enum Access {
    Active,
    Closed,
    Foreign,
}

struct ExtractedPath(String);

impl FromRequest<DefaultError> for ExtractedPath {
    type Error = Infallible;

    async fn from_request(req: &HttpRequest, _: &mut Payload) -> Result<Self, Self::Error> {
        req.extensions()
            .get::<Arc<AtomicUsize>>()
            .expect("fixture supplies an extraction counter")
            .fetch_add(1, Ordering::SeqCst);
        Ok(Self(req.path().to_owned()))
    }
}

struct ContextFixture {
    owner: Owner,
    scope: Option<RequestScope>,
    response: ResponseOptions,
    extractions: Arc<AtomicUsize>,
}

impl ContextFixture {
    fn new(access: Access) -> Self {
        let scope = RequestScope::new();
        let native = test::TestRequest::with_uri("/context")
            .header(header::ACCEPT, "text/html")
            .to_http_request();
        let extractions = Arc::new(AtomicUsize::new(0));
        native.extensions_mut().insert(extractions.clone());
        let request = Request::new(&native);
        let response = ResponseOptions::default();
        response.set_status(StatusCode::ACCEPTED);
        response.insert_header(
            header::LOCATION,
            header::HeaderValue::from_static("/initial"),
        );
        let owner = Owner::new();
        owner.with(|| {
            provide_context(request);
            provide_context(response.clone());
        });
        let mut fixture = Self {
            owner,
            scope: Some(scope),
            response,
            extractions,
        };
        if matches!(access, Access::Closed) {
            drop(fixture.scope.take());
        }
        fixture
    }

    fn run<T: Send + 'static>(
        &self,
        access: Access,
        work: impl FnOnce() -> T + Send + 'static,
    ) -> T {
        if matches!(access, Access::Foreign) {
            let owner = self.owner.clone();
            std::thread::spawn(move || owner.with(work))
                .join()
                .expect("foreign context operation must complete")
        } else {
            self.owner.with(work)
        }
    }
}

impl Drop for ContextFixture {
    fn drop(&mut self) {
        self.owner.clone().unset_with_forced_cleanup();
    }
}

fn contextual_redirect(access: Access) -> (Option<StatusCode>, String, bool) {
    let fixture = ContextFixture::new(access);
    fixture.run(access, || redirect("/target"));
    let parts = fixture.response.0.read().unwrap();
    (
        parts.status,
        parts
            .headers
            .get(header::LOCATION)
            .unwrap()
            .to_str()
            .unwrap()
            .to_owned(),
        parts.headers.contains_key("serverfnredirect"),
    )
}

fn contextual_extraction(access: Access) -> (Result<String, String>, usize) {
    let fixture = ContextFixture::new(access);
    let result = fixture.run(access, || {
        futures::executor::block_on(extract::<ExtractedPath>())
            .map(|path| path.0)
            .map_err(|error| error.to_string())
    });
    (result, fixture.extractions.load(Ordering::SeqCst))
}

fn rejects_before_extraction(
    reason: &'static str,
) -> impl Fn(&(Result<String, String>, usize)) -> AssertionResult {
    move |actual| match &actual.0 {
        Err(error) if error.contains(reason) && actual.1 == 0 => Ok(()),
        _ => Err(AssertionError::new(vec![format!(
            "Expected request access error containing {reason:?} before any extraction; received {actual:?}"
        )])),
    }
}

lets_expect! {
    expect(contextual_redirect(access)) as managed_request_redirect {
        let access = Access::Active;
        to preserves_the_html_redirect_contract {
            equal((Some(StatusCode::FOUND), "/target".to_owned(), false))
        }
        when the_request_scope_is_closed {
            let access = Access::Closed;
            to leaves_the_response_unchanged {
                equal((Some(StatusCode::ACCEPTED), "/initial".to_owned(), false))
            }
        }
        when called_on_a_foreign_thread {
            let access = Access::Foreign;
            to leaves_the_response_unchanged {
                equal((Some(StatusCode::ACCEPTED), "/initial".to_owned(), false))
            }
        }
    }
    expect(contextual_extraction(access)) as managed_request_extraction {
        let access = Access::Active;
        to passes_the_request_head_to_the_extractor {
            equal((Ok("/context".to_owned()), 1_usize))
        }
        when the_request_scope_is_closed {
            let access = Access::Closed;
            to reports_scope_closure_before_extraction { rejects_before_extraction("closed") }
        }
        when called_on_a_foreign_thread {
            let access = Access::Foreign;
            to reports_the_thread_error_before_extraction { rejects_before_extraction("thread") }
        }
    }
}
