//! Contain ntex 3.9.6 HTTP/1's attempt to write a second response after a body error.
//!
//! Its `src/http/h1/dispatcher.rs::poll_send_payload` routes a body error to
//! `ctl_proto_err` even after the response has started. Keep this workaround
//! until the HTTP/1 body-error wire regression passes with a corrected ntex.

use ntex::{
    http::body::{Body, BodySize, MessageBody, ResponseBody},
    io::IoRef,
    util::Bytes,
    web::{HttpRequest, HttpResponse},
};
use std::{
    error::Error,
    rc::Rc,
    task::{Context, Poll},
};

/// Preserve the body error and size, but terminate an HTTP/1 connection before
/// ntex can serialize its protocol-error response into an already started body.
/// HEAD retains the body's size and ownership but never polls its producer:
/// ntex 3.9.6 suppresses its bytes while still polling a Sized/Stream body.
/// Keep this containment until ntex closes HTTP/1 IO on response-body errors
/// and stops evaluating content that HEAD does not send.
pub(crate) fn terminate_on_body_error(
    request: &HttpRequest,
    response: HttpResponse,
) -> HttpResponse {
    contain_for_protocol(
        request.version(),
        request.io().cloned(),
        response,
        request.method() == ntex::http::Method::HEAD,
    )
}

fn contain_for_protocol(
    version: ntex::http::Version,
    io: Option<IoRef>,
    response: HttpResponse,
    head_only: bool,
) -> HttpResponse {
    // The second-status-line defect belongs to ntex's HTTP/1 encoder. HTTP/2
    // must retain its native per-stream error handling on the shared socket.
    let io = if matches!(
        version,
        ntex::http::Version::HTTP_10 | ntex::http::Version::HTTP_11
    ) {
        io
    } else {
        None
    };
    response.map_body(|_, body| {
        ResponseBody::Body(Body::from_message(FallibleBody {
            body,
            io,
            head_only,
        }))
    })
}

struct FallibleBody {
    body: ResponseBody<Body>,
    io: Option<IoRef>,
    head_only: bool,
}

impl MessageBody for FallibleBody {
    fn size(&self) -> BodySize {
        self.body.size()
    }

    fn poll_next_chunk(
        &mut self,
        cx: &mut Context<'_>,
    ) -> Poll<Option<Result<Bytes, Rc<dyn Error>>>> {
        if self.head_only {
            return Poll::Ready(None);
        }
        let result = self.body.poll_next_chunk(cx);
        if matches!(result, Poll::Ready(Some(Err(_))))
            && let Some(io) = &self.io
        {
            io.terminate();
        }
        result
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use futures::future::poll_fn;
    use lets_expect::*;
    use ntex::{
        http::Version,
        io::{Io, testing::IoTest},
    };

    struct OneBody(Option<Result<Bytes, Rc<dyn Error>>>);
    impl MessageBody for OneBody {
        fn size(&self) -> BodySize {
            BodySize::Stream
        }
        fn poll_next_chunk(
            &mut self,
            _: &mut Context<'_>,
        ) -> Poll<Option<Result<Bytes, Rc<dyn Error>>>> {
            Poll::Ready(self.0.take())
        }
    }
    #[derive(Debug, PartialEq)]
    struct Observation {
        terminated: bool,
        original_result: bool,
        original_size: bool,
    }

    async fn scope(version: Version, fails: bool) -> Observation {
        let (_peer, transport) = IoTest::create();
        let io = Io::new(transport, ntex::SharedCfg::default());
        let error: Rc<dyn Error> = Rc::new(std::io::Error::other("original-body-error"));
        let item = if fails {
            Err(error.clone())
        } else {
            Ok(Bytes::from_static(b"body"))
        };
        let response = HttpResponse::Ok().body(Body::from_message(OneBody(Some(item))));
        let mut response =
            contain_for_protocol(version, Some(io.get_ref().clone()), response, false);
        let mut body = response.take_body();
        let original_size = body.size() == BodySize::Stream;
        let result = poll_fn(|cx| body.poll_next_chunk(cx)).await;
        let original_result = match result {
            Some(Err(actual)) => fails && Rc::ptr_eq(&actual, &error),
            Some(Ok(bytes)) => !fails && bytes == b"body"[..],
            None => false,
        };
        Observation {
            terminated: io.is_closed(),
            original_result,
            original_size,
        }
    }
    fn preserved(terminated: bool) -> impl Fn(&Observation) -> AssertionResult {
        equal(Observation {
            terminated,
            original_result: true,
            original_size: true,
        })
    }
    lets_expect! {
        expect(crate::tests::run_ntex(scope(version, fails))) as response_error_scope {
            let version = Version::HTTP_11;
            let fails = false;
            to preserve_success { preserved(false) }
            when body_fails { let fails = true; to contain_http1_error { preserved(true) } }
            when protocol_is_http10 {
                let version = Version::HTTP_10;
                to preserve_success { preserved(false) }
                when body_fails { let fails = true; to contain_http1_error { preserved(true) } }
            }
            when protocol_is_http2 {
                let version = Version::HTTP_2;
                to preserve_success { preserved(false) }
                when body_fails { let fails = true; to leave_stream_error_to_http2 { preserved(false) } }
            }
        }
    }
}

#[cfg(test)]
mod file_head_after_truncation {
    use leptos::prelude::*;
    use lets_expect::*;
    use ntex::{
        http::HttpService,
        io::{Io, testing::IoTest},
        service::{Pipeline, ServiceFactory},
        time::{Millis, timeout},
        web::{App, HttpResponse, resource},
    };
    #[derive(Debug)]
    struct Observed {
        wire: String,
    }
    async fn observe() -> Observed {
        let directory = crate::tests::temp_site_root("head_file_truncation");
        let path = directory.join("file.txt");
        std::fs::write(&path, "body").unwrap();
        let options = LeptosOptions::builder()
            .output_name("head_file")
            .site_root(directory.to_string_lossy().into_owned())
            .site_pkg_dir("")
            .build();
        let factory = HttpService::h1(
            App::new()
                .state(options)
                .service(resource("/next").to(async || HttpResponse::Ok().body("next")))
                .route(
                    "/{tail}*",
                    crate::file_and_error_handler_with_context(
                        move || std::fs::write(&path, "").unwrap(),
                        |_: LeptosOptions| view! { <p>"missing"</p> },
                    ),
                ),
        );
        let (peer, transport) = IoTest::create();
        peer.remote_buffer_cap(4096);
        let io = Io::new(transport, ntex::SharedCfg::default());
        let service = Pipeline::new(factory.create(ntex::SharedCfg::default()).await.unwrap());
        ntex::rt::spawn(async move {
            let _ = service.call(io).await;
        });
        peer.write("HEAD /file.txt HTTP/1.1\r\nHost: example.test\r\n\r\nGET /next HTTP/1.1\r\nHost: example.test\r\nConnection: close\r\n\r\n");
        let mut bytes = Vec::new();
        timeout(Millis(1000), async {
            loop {
                let chunk = peer.read().await.unwrap();
                if chunk.is_empty() {
                    break;
                }
                bytes.extend_from_slice(&chunk);
                // The assertion concerns two complete HTTP responses. IoTest's
                // later graceful transport shutdown is a separate contract.
                if bytes.ends_with(b"\r\n\r\nnext") {
                    break;
                }
            }
        })
        .await
        .unwrap_or_else(|_| {
            panic!(
                "technical experiment timeout; bytes={:?}",
                String::from_utf8_lossy(&bytes)
            )
        });
        Observed {
            wire: String::from_utf8(bytes).unwrap(),
        }
    }
    fn completes_both_responses(result: &Observed) -> AssertionResult {
        let complete = result
            .wire
            .split_once("\r\n\r\n")
            .is_some_and(|(head, next)| {
                head.starts_with("HTTP/1.1 200 OK\r\n")
                    && head.contains("\r\ncontent-length: 4\r\n")
                    && next.split_once("\r\n\r\n").is_some_and(|(head, body)| {
                        head.starts_with("HTTP/1.1 200 OK\r\n")
                            && head.contains("\r\ncontent-length: 4\r\n")
                            && body == "next"
                    })
            });
        if complete {
            Ok(())
        } else {
            Err(AssertionError::new(vec![format!(
                "expected HEAD headers then completed GET, observed {:?}",
                result.wire
            )]))
        }
    }
    lets_expect! {
        expect(crate::tests::run_ntex(observe())) as the_head_file_response_after_truncation {
            to preserves_the_next_request { completes_both_responses }
        }
    }
}

#[cfg(test)]
mod head_body_lifetime {
    use super::*;
    use leptos::prelude::*;
    use lets_expect::*;
    use std::{
        cell::{Cell, RefCell},
        error::Error,
        sync::{
            Arc,
            atomic::{AtomicUsize, Ordering},
        },
    };

    #[derive(Clone, Copy)]
    struct Marker(u8);
    struct Producer {
        polls: Rc<Cell<usize>>,
        dropped: Rc<RefCell<Option<u8>>>,
    }
    impl MessageBody for Producer {
        fn size(&self) -> BodySize {
            BodySize::Sized(4)
        }
        fn poll_next_chunk(
            &mut self,
            _: &mut Context<'_>,
        ) -> Poll<Option<Result<Bytes, Rc<dyn Error>>>> {
            self.polls.set(self.polls.get() + 1);
            Poll::Ready(Some(Err(Rc::new(std::io::Error::other(
                "body must not run for HEAD",
            )))))
        }
    }
    impl Drop for Producer {
        fn drop(&mut self) {
            *self.dropped.borrow_mut() = use_context::<Marker>().map(|v| v.0);
        }
    }
    #[derive(Debug, PartialEq)]
    struct Observation {
        eof: bool,
        size: BodySize,
        polls: usize,
        retained_until_drop: bool,
        drop_context: Option<u8>,
        cleaned: usize,
        caller: Option<u8>,
    }
    async fn observe(poll: bool) -> Observation {
        let caller = Owner::new();
        caller.set();
        provide_context(Marker(7));
        let owner = Arc::new(crate::owner::OwnerCleanup::new(Owner::new()));
        let cleaned = Arc::new(AtomicUsize::new(0));
        let cleanup_count = cleaned.clone();
        owner.with_context(|| {
            provide_context(Marker(42));
            on_cleanup(move || {
                cleanup_count.fetch_add(1, Ordering::SeqCst);
            });
        });
        let polls = Rc::new(Cell::new(0));
        let dropped = Rc::new(RefCell::new(None));
        let response = HttpResponse::Ok().body(Body::from_message(Producer {
            polls: polls.clone(),
            dropped: dropped.clone(),
        }));
        let request = ntex::web::test::TestRequest::default()
            .method(ntex::http::Method::HEAD)
            .to_http_request();
        // Same ownership order as the server-function dispatcher: the owner
        // wrapper encloses the body completion wrapper and its retained producer.
        let mut response =
            crate::owner::with_owner_cleanup(terminate_on_body_error(&request, response), owner);
        let mut body = response.take_body();
        let size = body.size();
        let eof = if poll {
            futures::future::poll_fn(|cx| body.poll_next_chunk(cx))
                .await
                .is_none()
        } else {
            true
        };
        let retained_until_drop = dropped.borrow().is_none();
        drop(body);
        drop(response);
        let result = Observation {
            eof,
            size,
            polls: polls.get(),
            retained_until_drop,
            drop_context: *dropped.borrow(),
            cleaned: cleaned.load(Ordering::SeqCst),
            caller: use_context::<Marker>().map(|v| v.0),
        };
        caller.unset_with_forced_cleanup();
        result
    }
    fn preserves_lifetime(observed: &Observation) -> AssertionResult {
        equal(Observation {
            eof: true,
            size: BodySize::Sized(4),
            polls: 0,
            retained_until_drop: true,
            drop_context: Some(42),
            cleaned: 1,
            caller: Some(7),
        })(observed)
    }
    lets_expect! {
        expect(crate::tests::run_ntex(observe(poll))) as the_head_body_lifetime {
            let poll = true;
            to skips_producer_poll_and_preserves_its_lifetime { preserves_lifetime }
            when dropped_without_poll {
                let poll = false;
                to preserves_the_unpolled_producer_lifetime { preserves_lifetime }
            }
        }
    }
}
