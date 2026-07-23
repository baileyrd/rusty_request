//! `https://` end-to-end tests: this is the acceptance test for the
//! whole `rusty_tls` seam this crate was the first named consumer of.
//!
//! Exercises the system trust store (`TrustPolicy::System`, the default)
//! hermetically (no real network, no real CA) against a local test
//! server's self-signed certificate, by pointing `SSL_CERT_FILE` at that
//! certificate's CA -- `rustls-native-certs` (which `TrustPolicy::System`
//! uses) honors that variable in place of the platform's real trust
//! store. Also covers the configurable-trust-policy API
//! (`ClientBuilder`/`RequestBuilder::trust_policy`): pinning a private CA
//! via `rusty_request::pinned_anchors` and `TrustPolicy::DangerNoVerification`.
//!
//! `SSL_CERT_FILE` is process-global, so every scenario that depends on
//! its value lives in *one* `#[test]` function, run strictly in
//! sequence, rather than several tests `cargo test` might schedule onto
//! different threads concurrently -- two tests racing to set it to
//! different paths would be flaky by construction, not just unlucky. The
//! `trust_policy`-based tests below don't consult `SSL_CERT_FILE` at all
//! (pinning/no-verification never touch the system store), so they're
//! free to run as ordinary, independent, parallel `#[test]` functions.

mod common;

use common::{http_response, run, start_connect_proxy, start_tls_test_server};
use rusty_request::{Client, Error, TrustPolicy};

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

#[test]
fn pinned_anchors_trusts_a_cert_signed_by_the_pinned_ca() {
    run(async {
        let server = start_tls_test_server(|_req| http_response(200, "OK", &[], b"pinned"));
        let url = format!("https://localhost:{}/hello", server.addr.port());

        let client = Client::builder()
            .trust_policy(rusty_request::pinned_anchors([server.ca_cert_der.clone()]))
            .build();
        let resp = client
            .get(&url)
            .unwrap()
            .send()
            .await
            .expect("https:// request should succeed against its own pinned CA");
        assert_eq!(resp.status().as_u16(), 200);
        assert_eq!(resp.text().unwrap(), "pinned");
    });
}

#[test]
fn pinned_anchors_rejects_a_cert_signed_by_a_different_ca() {
    run(async {
        let server = start_tls_test_server(|_req| http_response(200, "OK", &[], b"pinned"));
        let other_server = start_tls_test_server(|_req| http_response(200, "OK", &[], b"other"));
        let url = format!("https://localhost:{}/hello", server.addr.port());

        // Pinned to `other_server`'s CA, not `server`'s -- the connection
        // should fail verification even though both are otherwise
        // well-formed, valid-for-`localhost` certificates.
        let client = Client::builder()
            .trust_policy(rusty_request::pinned_anchors([other_server
                .ca_cert_der
                .clone()]))
            .build();
        let result = client.get(&url).unwrap().send().await;
        assert!(
            matches!(result, Err(Error::Io(_))),
            "expected Error::Io for a cert signed by an unpinned CA, got {result:?}"
        );
    });
}

#[test]
fn danger_no_verification_accepts_an_untrusted_self_signed_cert() {
    run(async {
        let server = start_tls_test_server(|_req| http_response(200, "OK", &[], b"insecure"));
        let url = format!("https://localhost:{}/hello", server.addr.port());

        // No `SSL_CERT_FILE`, no pinned CA -- `TrustPolicy::System` would
        // reject this exactly like `https_end_to_end`'s step 2. With
        // verification disabled entirely, it succeeds instead.
        let client = Client::builder()
            .trust_policy(TrustPolicy::DangerNoVerification)
            .build();
        let resp = client
            .get(&url)
            .unwrap()
            .send()
            .await
            .expect("DangerNoVerification should accept any server certificate");
        assert_eq!(resp.status().as_u16(), 200);
        assert_eq!(resp.text().unwrap(), "insecure");
    });
}

#[test]
fn request_level_trust_policy_overrides_the_client_default() {
    run(async {
        let server = start_tls_test_server(|_req| http_response(200, "OK", &[], b"override"));
        let url = format!("https://localhost:{}/hello", server.addr.port());

        // `Client` default is `TrustPolicy::System`, which would reject
        // this untrusted cert -- the per-request override should still
        // let it through.
        let client = Client::builder().build();
        let resp = client
            .get(&url)
            .unwrap()
            .trust_policy(TrustPolicy::DangerNoVerification)
            .send()
            .await
            .expect("a request-level DangerNoVerification override should accept the cert");
        assert_eq!(resp.status().as_u16(), 200);
        assert_eq!(resp.text().unwrap(), "override");
    });
}
