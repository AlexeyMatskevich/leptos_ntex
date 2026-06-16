//! Diagnostic-only ntex readiness-barrier stress probe.
//!
//! This intentionally avoids leptos_ntex_unofficial code and does not propose a
//! workaround. It checks whether first-request failures still reproduce when a
//! manually started ntex server waits for `ServerStatus::Ready` before the
//! client sends the first request.

use std::net;
#[cfg(unix)]
use std::os::fd::AsRawFd;
use std::sync::{Arc, Mutex, mpsc};
use std::time::Duration;

use ntex::client::Client;
use ntex::http::{HttpService, HttpServiceConfig, StatusCode};
use ntex::io::IoConfig;
use ntex::server::{self, ServerStatus};
use ntex::web::{self, App, HttpResponse, WebAppConfig};
use ntex::{SharedCfg, rt};

async fn wait_for_ready(name: &'static str, rx: mpsc::Receiver<()>) {
    let result = rt::spawn_blocking(move || rx.recv_timeout(Duration::from_secs(5))).await;

    match result {
        Ok(Ok(())) => eprintln!("{name}: observed ServerStatus::Ready"),
        Ok(Err(err)) => panic!("{name}: timed out waiting for ServerStatus::Ready: {err:?}"),
        Err(err) => panic!("{name}: spawn_blocking failed while waiting for Ready: {err}"),
    }
}

async fn one_round(name: &'static str) {
    let listener = net::TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap();
    let (ready_tx, ready_rx) = mpsc::channel::<()>();
    let ready_tx = Arc::new(Mutex::new(Some(ready_tx)));
    let app_name = format!("{name}-ready-barrier");

    #[cfg(unix)]
    eprintln!(
        "{name}: listener bound at {addr} fd={}",
        listener.as_raw_fd()
    );
    #[cfg(not(unix))]
    eprintln!("{name}: listener bound at {addr}");

    let server = server::build()
        .workers(1)
        .disable_signals()
        .status_handler({
            let ready_tx = Arc::clone(&ready_tx);
            move |status| {
                eprintln!("{name}: server status {status:?}");
                if status == ServerStatus::Ready
                    && let Some(tx) = ready_tx.lock().unwrap().take()
                {
                    let _ = tx.send(());
                }
            }
        })
        .listen("ready-barrier", listener, async |_| {
            HttpService::new(
                App::new()
                    .service(web::resource("/").to(|| async { HttpResponse::Ok().body("ok") })),
            )
        })
        .unwrap()
        .config(
            "ready-barrier",
            SharedCfg::new("READY-BARRIER-SRV")
                .add(IoConfig::new())
                .add(HttpServiceConfig::new())
                .add(WebAppConfig::with(
                    &app_name,
                    false,
                    addr,
                    format!("{addr}"),
                )),
        )
        .run();

    wait_for_ready(name, ready_rx).await;

    let url = format!("http://{addr}/");
    eprintln!("{name}: sending first request to {url}");

    let client = Client::new().await;
    let response = client.get(url).send().await.unwrap();
    assert_eq!(response.status(), StatusCode::OK);

    let body = response.body().await.unwrap();
    assert_eq!(body.as_ref(), b"ok");

    server.stop(false).await;
}

macro_rules! define_ready_barrier_probe {
    ($($name:ident),+ $(,)?) => {
        $(
            #[ntex::test]
            async fn $name() {
                one_round(stringify!($name)).await;
            }
        )+
    };
}

define_ready_barrier_probe!(
    ntex_ready_barrier_parallel_00,
    ntex_ready_barrier_parallel_01,
    ntex_ready_barrier_parallel_02,
    ntex_ready_barrier_parallel_03,
    ntex_ready_barrier_parallel_04,
    ntex_ready_barrier_parallel_05,
    ntex_ready_barrier_parallel_06,
    ntex_ready_barrier_parallel_07,
    ntex_ready_barrier_parallel_08,
    ntex_ready_barrier_parallel_09,
    ntex_ready_barrier_parallel_10,
    ntex_ready_barrier_parallel_11,
    ntex_ready_barrier_parallel_12,
    ntex_ready_barrier_parallel_13,
    ntex_ready_barrier_parallel_14,
    ntex_ready_barrier_parallel_15,
);
