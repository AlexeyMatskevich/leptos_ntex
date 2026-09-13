use super::run_ntex;
use crate::{ResponseOptions, handle_response_inner};
use futures::{FutureExt, Stream};
use leptos::prelude::*;
use leptos_integration_utils::{BoxedFnOnce, PinnedFuture, PinnedStream};
use lets_expect::lets_expect;
use ntex::{http::header, web::test};
use std::{
    pin::Pin,
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, AtomicUsize, Ordering},
    },
    task::{Context, Poll},
};

fn pending_builder(
    _: &'static str,
    _: BoxedFnOnce<PinnedStream<String>>,
    _: bool,
) -> PinnedFuture<PinnedStream<String>> {
    Box::pin(futures::future::pending())
}
fn pending_chunk(
    _: &'static str,
    _: BoxedFnOnce<PinnedStream<String>>,
    _: bool,
) -> PinnedFuture<PinnedStream<String>> {
    let entered = expect_context::<StepEntered>();
    Box::pin(async move {
        Box::pin(futures::stream::poll_fn(move |_| {
            entered.0.store(true, Ordering::SeqCst);
            Poll::Pending
        })) as PinnedStream<String>
    })
}
fn ready_body(
    _: &'static str,
    _: BoxedFnOnce<PinnedStream<String>>,
    _: bool,
) -> PinnedFuture<PinnedStream<String>> {
    Box::pin(async {
        Box::pin(futures::stream::iter([
            "<!DOCTYPE html><html><head></head><body>ready</body></html>".to_owned(),
        ])) as PinnedStream<String>
    })
}

#[derive(Clone)]
struct StepEntered(Arc<AtomicBool>);

#[derive(Clone, Copy)]
enum EndStage {
    Complete,
    EarlyBody,
    BuilderPending,
    DeferredPending,
    MetadataPending,
    FirstChunkPending,
    AppPanic,
    ContextPanic,
}

fn cleanup_at(stage: EndStage) -> (usize, bool) {
    run_ntex(async move {
        let cleaned = Arc::new(AtomicUsize::new(0));
        let retained = Arc::new(Mutex::new(None));
        let retain_context = retained.clone();
        let counter = cleaned.clone();
        let step_entered = StepEntered(Arc::new(AtomicBool::new(false)));
        let entered_context = step_entered.clone();
        let mut response = handle_response_inner(
            move || {
                *retain_context.lock().unwrap() = Owner::current();
                provide_context(entered_context.clone());
                on_cleanup(move || {
                    counter.fetch_add(1, Ordering::SeqCst);
                });
                if matches!(stage, EndStage::DeferredPending) {
                    Owner::current_shared_context()
                        .unwrap()
                        .defer_stream(Box::pin(futures::future::poll_fn(move |_| {
                            entered_context.0.store(true, Ordering::SeqCst);
                            Poll::Pending
                        })));
                }
                if matches!(stage, EndStage::ContextPanic) {
                    panic!("context probe");
                }
            },
            move || {
                if matches!(stage, EndStage::AppPanic) {
                    panic!("app probe");
                }
                "shell"
            },
            test::TestRequest::get().uri("/").to_http_request(),
            match stage {
                EndStage::BuilderPending => pending_builder,
                EndStage::FirstChunkPending => pending_chunk,
                _ => ready_body,
            },
        );
        let panicked = if matches!(stage, EndStage::AppPanic | EndStage::ContextPanic) {
            std::panic::AssertUnwindSafe(response.as_mut())
                .catch_unwind()
                .await
                .is_err()
        } else if matches!(stage, EndStage::Complete | EndStage::EarlyBody) {
            let mut response = response.as_mut().await;
            if matches!(stage, EndStage::Complete) {
                let bytes = test::load_stream(response.take_body()).await.unwrap();
                assert!(
                    !bytes.is_empty(),
                    "complete-body fixture must produce content"
                );
            } else {
                drop(response);
            }
            false
        } else {
            assert!(
                futures::poll!(response.as_mut()).is_pending(),
                "fixture must stop before a response exists"
            );
            if matches!(stage, EndStage::FirstChunkPending) {
                // Explicitly progress the metadata scheduler tick before
                // cancelling at the first pending body chunk.
                leptos::task::tick().await;
                assert!(futures::poll!(response.as_mut()).is_pending());
            }
            false
        };
        if matches!(
            stage,
            EndStage::FirstChunkPending | EndStage::DeferredPending
        ) {
            assert!(
                step_entered.0.load(Ordering::SeqCst),
                "the selected pending operation must actually be polled"
            );
        }
        assert!(
            retained.lock().unwrap().is_some(),
            "the owned scope must have been entered"
        );
        drop(response);
        let observed = cleaned.load(Ordering::SeqCst);
        retained
            .lock()
            .unwrap()
            .take()
            .unwrap()
            .unset_with_forced_cleanup();
        (observed, panicked)
    })
}

lets_expect! {
    expect(cleanup_at(stage)) as ssr_owner_lifetime {
        let stage = EndStage::Complete;
        to cleans_once_after_body_completion { equal((1_usize, false)) }
        when the_body_is_dropped_early {
            let stage = EndStage::EarlyBody;
            to cleans_once_on_cancellation { equal((1_usize, false)) }
        }
        when the_stream_builder_is_pending {
            let stage = EndStage::BuilderPending;
            to cleans_once_on_cancellation { equal((1_usize, false)) }
        }
        when deferred_work_is_pending {
            let stage = EndStage::DeferredPending;
            to cleans_once_on_cancellation { equal((1_usize, false)) }
        }
        when the_metadata_tick_is_pending {
            let stage = EndStage::MetadataPending;
            to cleans_once_on_cancellation { equal((1_usize, false)) }
        }
        when the_first_body_chunk_is_pending {
            let stage = EndStage::FirstChunkPending;
            to cleans_once_on_cancellation { equal((1_usize, false)) }
        }
        when the_application_panics {
            let stage = EndStage::AppPanic;
            to cleans_once_and_preserves_the_panic { equal((1_usize, true)) }
        }
        when additional_context_panics {
            let stage = EndStage::ContextPanic;
            to cleans_once_and_preserves_the_panic { equal((1_usize, true)) }
        }
    }
}

struct DestructorProbe {
    value: StoredValue<i32>,
    seen: Arc<Mutex<Option<i32>>>,
    emitted: bool,
}
impl Drop for DestructorProbe {
    fn drop(&mut self) {
        *self.seen.lock().unwrap() = Some(self.value.get_value());
    }
}
impl Stream for DestructorProbe {
    type Item = String;
    fn poll_next(mut self: Pin<&mut Self>, _: &mut Context<'_>) -> Poll<Option<String>> {
        if self.emitted {
            Poll::Pending
        } else {
            self.emitted = true;
            Poll::Ready(Some(
                "<!DOCTYPE html><html><head></head><body>ready</body></html>".to_owned(),
            ))
        }
    }
}
impl std::future::Future for DestructorProbe {
    type Output = PinnedStream<String>;
    fn poll(self: Pin<&mut Self>, _: &mut Context<'_>) -> Poll<Self::Output> {
        Poll::Pending
    }
}
fn destructible_builder(
    _: &'static str,
    _: BoxedFnOnce<PinnedStream<String>>,
    _: bool,
) -> PinnedFuture<PinnedStream<String>> {
    let probe = DestructorProbe {
        value: StoredValue::new(42),
        seen: expect_context(),
        emitted: false,
    };
    if expect_context::<bool>() {
        Box::pin(probe)
    } else {
        Box::pin(async { Box::pin(probe) as PinnedStream<String> })
    }
}
fn destructor_observation(pending_builder: bool) -> (bool, Option<i32>) {
    run_ntex(async move {
        let seen = Arc::new(Mutex::new(None));
        let seen_context = seen.clone();
        let mut response = handle_response_inner(
            move || {
                provide_context(seen_context);
                provide_context(pending_builder);
            },
            || "shell",
            test::TestRequest::get().uri("/").to_http_request(),
            destructible_builder,
        );
        let dropped_without_panic = if pending_builder {
            assert!(futures::poll!(response.as_mut()).is_pending());
            std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| drop(response))).is_ok()
        } else {
            let body = response.as_mut().await;
            std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| drop(body))).is_ok()
        };
        let value = *seen.lock().unwrap();
        (dropped_without_panic, value)
    })
}
lets_expect! {
    expect(destructor_observation(pending_builder)) as reactive_destructor_lifetime {
        let pending_builder = false;
        to keeps_values_live_until_the_stream_destructor_finishes { equal((true, Some(42))) }
        when the_stream_builder_is_still_pending {
            let pending_builder = true;
            to keeps_values_live_until_the_future_destructor_finishes { equal((true, Some(42))) }
        }
    }
}

fn cache_control_directives() -> Vec<String> {
    run_ntex(async {
        let response = handle_response_inner(
            || {},
            || {
                let options = expect_context::<ResponseOptions>();
                options.append_header(
                    header::CACHE_CONTROL,
                    header::HeaderValue::from_static("no-store"),
                );
                options.append_header(
                    header::CACHE_CONTROL,
                    header::HeaderValue::from_static("max-age=60"),
                );
                "private response"
            },
            test::TestRequest::get().uri("/").to_http_request(),
            ready_body,
        )
        .await;
        response
            .headers()
            .get_all(header::CACHE_CONTROL)
            .map(|v| v.to_str().unwrap().to_string())
            .collect()
    })
}
lets_expect! {
    expect(cache_control_directives()) as rendered_cache_control {
        to preserves_every_appended_directive { equal(vec!["no-store".to_string(), "max-age=60".to_string()]) }
    }
}

#[derive(Clone)]
struct CallerMarker(u8);
#[derive(Clone, Copy)]
enum FirstPoll {
    Pending,
    AppPanic,
    ContextPanic,
}

type ContextObservation = (Option<u8>, Option<u8>, Option<u8>, bool, bool, bool, usize);
fn caller_context(caller_present: bool, end: FirstPoll) -> ContextObservation {
    run_ntex(async move {
        let observe = || {
            let before = use_context::<CallerMarker>().map(|marker| marker.0);
            let cleaned = Arc::new(AtomicUsize::new(0));
            let counter = cleaned.clone();
            let mut response = handle_response_inner(
                move || {
                    provide_context(CallerMarker(9));
                    on_cleanup(move || {
                        counter.fetch_add(1, Ordering::SeqCst);
                    });
                    if matches!(end, FirstPoll::ContextPanic) {
                        panic!("nested context probe");
                    }
                },
                move || {
                    if matches!(end, FirstPoll::AppPanic) {
                        panic!("nested app probe");
                    }
                    "shell"
                },
                test::TestRequest::get().to_http_request(),
                pending_builder,
            );
            let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                response
                    .as_mut()
                    .poll(&mut Context::from_waker(futures::task::noop_waker_ref()))
            }));
            let panicked = result.is_err();
            let after_poll = use_context::<CallerMarker>().map(|marker| marker.0);
            let owner_after_poll = Owner::current().is_some();
            drop(result);
            drop(response);
            let after_drop = use_context::<CallerMarker>().map(|marker| marker.0);
            (
                before,
                after_poll,
                after_drop,
                owner_after_poll,
                Owner::current().is_some(),
                panicked,
                cleaned.load(Ordering::SeqCst),
            )
        };
        if caller_present {
            Owner::new().with(|| {
                provide_context(CallerMarker(7));
                observe()
            })
        } else {
            observe()
        }
    })
}

lets_expect! {
    expect(caller_context(caller_present, end)) as caller_scope_restoration {
        let caller_present = true;
        let end = FirstPoll::Pending;
        to preserves_the_caller_during_cancellation { equal((Some(7), Some(7), Some(7), true, true, false, 1_usize)) }
        when the_application_panics {
            let end = FirstPoll::AppPanic;
            to preserves_the_caller_while_cleaning_the_request { equal((Some(7), Some(7), Some(7), true, true, true, 1_usize)) }
        }
        when additional_context_panics {
            let end = FirstPoll::ContextPanic;
            to preserves_the_caller_while_cleaning_the_request { equal((Some(7), Some(7), Some(7), true, true, true, 1_usize)) }
        }
        when there_is_no_caller_owner {
            let caller_present = false;
            to leaves_no_request_owner_current { equal((None, None, None, false, false, false, 1_usize)) }
            when the_application_panics {
                let end = FirstPoll::AppPanic;
                to leaves_no_request_owner_current { equal((None, None, None, false, false, true, 1_usize)) }
            }
            when additional_context_panics {
                let end = FirstPoll::ContextPanic;
                to leaves_no_request_owner_current { equal((None, None, None, false, false, true, 1_usize)) }
            }
        }
    }
}
#[derive(Clone, Copy)]
enum BodyFailure {
    Poll,
    Drop,
    Cleanup,
}
struct BodyContextProbe {
    failure: BodyFailure,
    armed: Arc<AtomicBool>,
    emitted: bool,
}
fn nested_owner_panic() {
    Owner::new().with(|| panic!("body context probe"));
}
impl Stream for BodyContextProbe {
    type Item = String;
    fn poll_next(mut self: Pin<&mut Self>, _: &mut Context<'_>) -> Poll<Option<String>> {
        if !self.emitted {
            self.emitted = true;
            Poll::Ready(Some(
                "<!DOCTYPE html><html><head></head><body>ready</body></html>".to_owned(),
            ))
        } else if self.armed.load(Ordering::SeqCst) && matches!(self.failure, BodyFailure::Poll) {
            nested_owner_panic();
            unreachable!()
        } else {
            Poll::Pending
        }
    }
}
impl Drop for BodyContextProbe {
    fn drop(&mut self) {
        if self.armed.load(Ordering::SeqCst) && matches!(self.failure, BodyFailure::Drop) {
            nested_owner_panic();
        }
    }
}
fn body_context_builder(
    _: &'static str,
    _: BoxedFnOnce<PinnedStream<String>>,
    _: bool,
) -> PinnedFuture<PinnedStream<String>> {
    let (failure, armed) = expect_context::<(BodyFailure, Arc<AtomicBool>)>();
    Box::pin(async move {
        Box::pin(BodyContextProbe {
            failure,
            armed,
            emitted: false,
        }) as PinnedStream<String>
    })
}
fn body_caller_context(failure: BodyFailure) -> (bool, Option<u8>, usize) {
    run_ntex(async move {
        let armed = Arc::new(AtomicBool::new(false));
        let arm_context = armed.clone();
        let cleaned = Arc::new(AtomicUsize::new(0));
        let counter = cleaned.clone();
        let mut response = handle_response_inner(
            move || {
                provide_context((failure, arm_context));
                on_cleanup(move || {
                    counter.fetch_add(1, Ordering::SeqCst);
                    if matches!(failure, BodyFailure::Cleanup) {
                        nested_owner_panic();
                    }
                });
            },
            || "shell",
            test::TestRequest::get().to_http_request(),
            body_context_builder,
        )
        .await;
        armed.store(true, Ordering::SeqCst);
        Owner::new().with(|| {
            provide_context(CallerMarker(7));
            let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                if matches!(failure, BodyFailure::Poll) {
                    use ntex::http::body::MessageBody;
                    let mut body = response.take_body();
                    // The prefetched first chunk is followed by the armed stream.
                    let mut cx = Context::from_waker(futures::task::noop_waker_ref());
                    assert!(matches!(
                        body.poll_next_chunk(&mut cx),
                        Poll::Ready(Some(Ok(_)))
                    ));
                    let _ = body.poll_next_chunk(&mut cx);
                }
                drop(response);
            }));
            (
                result.is_err(),
                use_context::<CallerMarker>().map(|v| v.0),
                cleaned.load(Ordering::SeqCst),
            )
        })
    })
}
lets_expect! {
    expect(body_caller_context(failure)) as response_body_caller_scope {
        let failure = BodyFailure::Poll;
        to restores_the_caller_after_a_body_poll_panic { equal((true, Some(7), 1_usize)) }
        when the_stream_destructor_panics {
            let failure = BodyFailure::Drop;
            to restores_the_caller_after_body_drop { equal((true, Some(7), 1_usize)) }
        }
        when the_cleanup_hook_panics {
            let failure = BodyFailure::Cleanup;
            to restores_the_caller_after_reactive_cleanup { equal((true, Some(7), 1_usize)) }
        }
    }
}
