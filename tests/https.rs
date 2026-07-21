//! `https://` end-to-end tests: this is the acceptance test for the
//! whole `rusty_tls` seam this crate was the first named consumer of.
//!
//! `rusty_request` has no public API for a custom `rusty_tls::TrustPolicy`
//! -- every `https://` request is verified against the system trust
//! store (`TrustPolicy::System`). To exercise that hermetically (no real
//! network, no real CA) against a local test server's self-signed
//! certificate, these tests point `SSL_CERT_FILE` at that certificate's
//! CA -- `rustls-native-certs` (which `TrustPolicy::System` uses) honors
//! that variable in place of the platform's real trust store.
//!
//! `SSL_CERT_FILE` is process-global, so every scenario that depends on
//! its value lives in *one* `#[test]` function, run strictly in
//! sequence, rather than several tests `cargo test` might schedule onto
//! different threads concurrently -- two tests racing to set it to
//! different paths would be flaky by construction, not just unlucky.

mod common;

use common::{http_response, run, start_connect_proxy, start_tls_test_server};
use rusty_request::{Client, Error};

fn set_ssl_cert_file(pem: &str) -> std::path::PathBuf {
    let path =
        std::env::temp_dir().join(format!("rusty_request_test_ca_{}.pem", std::process::id()));
    std::fs::write(&path, pem).expect("failed to write test CA to a temp file");
    std::env::set_var("SSL_CERT_FILE", &path);
    path
}

#[test]
fn https_end_to_end() {
    run(async {
        let server = start_tls_test_server(|req| {
            assert_eq!(req.method, "GET");
            http_response(200, "OK", &[], b"hello over tls")
        });
        let url = format!("https://localhost:{}/hello", server.addr.port());

        // 1. Verified against a trusted anchor: the request succeeds and
        //    the response round-trips correctly through the TLS layer.
        let ca_path = set_ssl_cert_file(&server.ca_cert_pem);
        let resp = rusty_request::get(&url)
            .await
            .expect("https:// request with a trusted CA should succeed");
        assert_eq!(resp.status().as_u16(), 200);
        assert_eq!(resp.text().unwrap(), "hello over tls");

        // 2. Verify-by-default: without that trust anchor, the same
        //    self-signed certificate is *not* silently accepted. This
        //    surfaces as `Error::Io` rather than `Error::Tls` -- the
        //    rejection happens during the handshake, which runs inside
        //    `AsyncTlsStream`'s `AsyncRead`/`AsyncWrite` impls, and
        //    those are contractually bound to return `io::Result`.
        //    `Error::Tls` only fires for a failure in `AsyncTlsStream::new`
        //    itself (e.g. an invalid server name), before any I/O.
        std::env::remove_var("SSL_CERT_FILE");
        let result = rusty_request::get(&url).await;
        assert!(
            matches!(result, Err(Error::Io(_))),
            "expected Error::Io for an untrusted certificate, got {result:?}"
        );

        // 3. The same trusted request, tunneled through a CONNECT proxy
        //    instead of connecting directly -- the acceptance test for
        //    proxy.rs's CONNECT-tunnel support.
        std::env::set_var("SSL_CERT_FILE", &ca_path);
        let proxy_url = format!("http://{}", start_connect_proxy().addr);
        let client = Client::builder().proxy(&proxy_url).unwrap().build();
        let resp = client
            .get(&url)
            .unwrap()
            .send()
            .await
            .expect("https:// request through a CONNECT-tunneling proxy should succeed");
        assert_eq!(resp.status().as_u16(), 200);
        assert_eq!(resp.text().unwrap(), "hello over tls");

        std::env::remove_var("SSL_CERT_FILE");
        let _ = std::fs::remove_file(&ca_path);
    });
}
