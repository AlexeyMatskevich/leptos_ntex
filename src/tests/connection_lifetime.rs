use super::*;
use futures::{StreamExt, channel::oneshot};
use leptos::{
    context::{provide_context, use_context},
    reactive::{
        owner::{Owner, StoredValue},
        traits::GetValue,
    },
};
use lets_expect::lets_expect;
use ntex::web::{App, test};
use server_fn::{ServerFn, error::ServerFnError};
use std::sync::{
    Arc, Mutex,
    atomic::{AtomicUsize, Ordering},
};

#[derive(Clone)]
struct LifetimeFixture {
    body_pending: bool,
    started: Arc<Mutex<Option<oneshot::Sender<()>>>>,
    dropped: Arc<Mutex<Option<oneshot::Sender<bool>>>>,
}

struct DropProbe {
    sender: Option<oneshot::Sender<bool>>,
    value: StoredValue<usize>,
}

impl Drop for DropProbe {
    fn drop(&mut self) {
        let readable = self.value.try_get_value() == Some(7);
        if let Some(sender) = self.sender.take() {
            let _ = sender.send(readable);
        }
    }
}

#[leptos::server(
    name = PendingConnection,
    prefix = "/connection-lifetime",
    endpoint = "pending",
    protocol = server_fn::Websocket<server_fn::codec::JsonEncoding, server_fn::codec::JsonEncoding>,
    server = crate::NtexServerFnBackend
)]
async fn pending_connection(
    _input: server_fn::BoxedStream<String, ServerFnError>,
) -> Result<server_fn::BoxedStream<String, ServerFnError>, ServerFnError> {
    let fixture = use_context::<LifetimeFixture>().expect("test fixture provided");
    let probe = DropProbe {
        sender: fixture.dropped.lock().unwrap().take(),
        value: StoredValue::new(7),
    };
    let _ = fixture.started.lock().unwrap().take().unwrap().send(());
    if fixture.body_pending {
        let _probe = probe;
        futures::future::pending::<()>().await;
        return Ok(futures::stream::empty().into());
    }
    Ok(futures::stream::once(async { Ok("ready".to_owned()) })
        .chain(futures::stream::pending())
        .map(move |item| {
            let _ = &probe;
            item
        })
        .into())
}

#[derive(Debug, PartialEq)]
struct LifetimeObservation {
    resource_dropped_with_context: bool,
    cleanups: usize,
}

async fn lifetime(body_pending: bool, transport_disconnect: bool) -> LifetimeObservation {
    crate::register_explicit::<PendingConnection>();
    let (started_tx, started_rx) = oneshot::channel();
    let (dropped_tx, dropped_rx) = oneshot::channel();
    let fixture = LifetimeFixture {
        body_pending,
        started: Arc::new(Mutex::new(Some(started_tx))),
        dropped: Arc::new(Mutex::new(Some(dropped_tx))),
    };
    let cleanups = Arc::new(AtomicUsize::new(0));
    let retained = Arc::new(Mutex::new(None));
    let app_cleanups = cleanups.clone();
    let app_retained = retained.clone();
    let (cleanup_tx, cleanup_rx) = oneshot::channel();
    let cleanup_tx = Arc::new(Mutex::new(Some(cleanup_tx)));
    let server = test::server(move || {
        let fixture = fixture.clone();
        let cleanups = app_cleanups.clone();
        let retained = app_retained.clone();
        let cleanup_tx = cleanup_tx.clone();
        async move {
            App::new().route(
                "/connection-lifetime/{tail}*",
                crate::handle_server_fns_with_context(move || {
                    provide_context(fixture.clone());
                    *retained.lock().unwrap() = Owner::current();
                    let cleanups = cleanups.clone();
                    let cleanup_tx = cleanup_tx.clone();
                    leptos::prelude::on_cleanup(move || {
                        cleanups.fetch_add(1, Ordering::SeqCst);
                        if let Some(sender) = cleanup_tx.lock().unwrap().take() {
                            let _ = sender.send(());
                        }
                    });
                }),
            )
        }
    })
    .await;
    let connection = server.ws_at(PendingConnection::PATH).await.unwrap();
    let sink = connection.sink();
    let receiver = connection.receiver();
    ntex::time::timeout(ntex::time::Millis(1000), started_rx)
        .await
        .expect("function started")
        .unwrap();
    if !body_pending {
        let ready = ntex::time::timeout(ntex::time::Millis(1000), receiver.recv())
            .await
            .unwrap();
        assert!(
            matches!(ready, Some(Ok(ntex::ws::Frame::Binary(_)))),
            "forwarder ready: {ready:?}"
        );
    }
    if transport_disconnect {
        sink.io().terminate();
    } else {
        sink.send(ntex::ws::Message::Close(None)).await.unwrap();
    }
    // The result is an acknowledgment from Drop, not inference from elapsed time.
    let resource_dropped_with_context = ntex::time::timeout(ntex::time::Millis(1000), dropped_rx)
        .await
        .expect("connection future must be dropped")
        .unwrap();
    ntex::time::timeout(ntex::time::Millis(1000), cleanup_rx)
        .await
        .expect("owner must be cleaned")
        .unwrap();
    let cleanups = cleanups.load(Ordering::SeqCst);
    drop(retained);
    LifetimeObservation {
        resource_dropped_with_context,
        cleanups,
    }
}

lets_expect! {
    expect(run_ntex(lifetime(body_pending, transport_disconnect))) as connection_lifetime {
        let body_pending = false;
        let transport_disconnect = false;
        to drops_pending_output_before_cleaning_its_context {
            equal(LifetimeObservation { resource_dropped_with_context: true, cleanups: 1 })
        }
        when the_function_body_is_pending {
            let body_pending = true;
            to drops_the_function_future_before_cleaning_its_context {
                equal(LifetimeObservation { resource_dropped_with_context: true, cleanups: 1 })
            }
        }
        when the_transport_disconnects {
            let transport_disconnect = true;
            to drops_pending_output_before_cleaning_its_context {
                equal(LifetimeObservation { resource_dropped_with_context: true, cleanups: 1 })
            }
            when the_function_body_is_pending {
                let body_pending = true;
                to drops_the_function_future_before_cleaning_its_context {
                    equal(LifetimeObservation { resource_dropped_with_context: true, cleanups: 1 })
                }
            }
        }
    }
}
