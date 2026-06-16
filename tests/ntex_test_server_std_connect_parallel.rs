//! Diagnostic-only ntex test-server probe using blocking std TCP.
//!
//! This keeps `ntex::web::test::server` as the server helper, but removes the
//! ntex async client from the first request path.

use std::io::{BufRead, BufReader, Read, Write};
use std::net::{SocketAddr, TcpStream};
use std::time::Duration;

use ntex::rt;
use ntex::web::{self, App, HttpResponse, test};

async fn std_http_get(name: &'static str, addr: SocketAddr) -> (String, Vec<u8>) {
    let result = rt::spawn_blocking(move || {
        let mut stream = TcpStream::connect_timeout(&addr, Duration::from_secs(5))?;
        stream.set_read_timeout(Some(Duration::from_secs(5)))?;
        stream.set_write_timeout(Some(Duration::from_secs(5)))?;

        let request = format!("GET / HTTP/1.1\r\nHost: {addr}\r\nConnection: close\r\n\r\n");
        stream.write_all(request.as_bytes())?;

        let mut reader = BufReader::new(stream);
        let mut status = String::new();
        reader.read_line(&mut status)?;

        let mut content_len = 0usize;
        loop {
            let mut header = String::new();
            reader.read_line(&mut header)?;
            let trimmed = header.trim_end();
            if trimmed.is_empty() {
                break;
            }
            if let Some((name, value)) = trimmed.split_once(':')
                && name.eq_ignore_ascii_case("content-length")
            {
                content_len = value.trim().parse().unwrap_or(0);
            }
        }

        let mut body = vec![0; content_len];
        reader.read_exact(&mut body)?;

        Ok::<_, std::io::Error>((status, body))
    })
    .await;

    match result {
        Ok(Ok(response)) => response,
        Ok(Err(err)) => panic!("{name}: std TcpStream request failed: {err:?}"),
        Err(err) => panic!("{name}: spawn_blocking failed during std request: {err}"),
    }
}

async fn one_round(name: &'static str) {
    let srv = test::server(|| async {
        App::new().service(web::resource("/").to(|| async { HttpResponse::Ok().body("ok") }))
    })
    .await;

    let addr = srv.addr();
    eprintln!("{name}: test::server addr={addr}");

    let (status, body) = std_http_get(name, addr).await;
    assert!(status.starts_with("HTTP/1.1 200 OK"), "{status:?}");
    assert_eq!(body.as_slice(), b"ok");
}

macro_rules! define_test_server_std_probe {
    ($($name:ident),+ $(,)?) => {
        $(
            #[ntex::test]
            async fn $name() {
                one_round(stringify!($name)).await;
            }
        )+
    };
}

define_test_server_std_probe!(
    ntex_test_server_std_connect_parallel_00,
    ntex_test_server_std_connect_parallel_01,
    ntex_test_server_std_connect_parallel_02,
    ntex_test_server_std_connect_parallel_03,
    ntex_test_server_std_connect_parallel_04,
    ntex_test_server_std_connect_parallel_05,
    ntex_test_server_std_connect_parallel_06,
    ntex_test_server_std_connect_parallel_07,
    ntex_test_server_std_connect_parallel_08,
    ntex_test_server_std_connect_parallel_09,
    ntex_test_server_std_connect_parallel_10,
    ntex_test_server_std_connect_parallel_11,
    ntex_test_server_std_connect_parallel_12,
    ntex_test_server_std_connect_parallel_13,
    ntex_test_server_std_connect_parallel_14,
    ntex_test_server_std_connect_parallel_15,
);
