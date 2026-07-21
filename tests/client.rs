mod common;

use common::{http_chunked_response, http_response, run, start_test_server, MemoryReader};
use rusty_request::{Backoff, Body, Client, Error, Json, Multipart, RetryPolicy};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::Duration;

#[test]
fn get_returns_status_headers_and_text() {
    run(async {
        let server = start_test_server(|req| {
            assert_eq!(req.method, "GET");
            http_response(200, "OK", &[("X-Test", "yes")], b"hello world")
        });

        let resp = rusty_request::get(&server.url("/hello")).await.unwrap();
        assert_eq!(resp.status().as_u16(), 200);
        assert!(resp.status().is_success());
        assert_eq!(resp.headers().get("x-test"), Some("yes"));
        assert_eq!(resp.text().unwrap(), "hello world");
    });
}

#[test]
fn post_json_body_round_trips() {
    run(async {
        let server = start_test_server(|req| {
            assert_eq!(req.method, "POST");
            assert_eq!(req.header("content-type"), Some("application/json"));
            let received = Json::parse(std::str::from_utf8(&req.body).unwrap()).unwrap();
            let mut echoed = Json::object();
            echoed.insert("you_sent", received);
            http_response(
                201,
                "Created",
                &[("Content-Type", "application/json")],
                echoed.to_json_string().as_bytes(),
            )
        });

        let mut body = Json::object();
        body.insert("name", "Ada");
        body.insert("age", 36);

        let resp = Client::new()
            .post(&server.url("/things"))
            .unwrap()
            .json(&body)
            .unwrap()
            .send()
            .await
            .unwrap();

        assert_eq!(resp.status().as_u16(), 201);
        let json = resp.json().unwrap();
        assert_eq!(
            json.get("you_sent").unwrap().get("name").unwrap().as_str(),
            Some("Ada")
        );
        assert_eq!(
            json.get("you_sent").unwrap().get("age").unwrap().as_f64(),
            Some(36.0)
        );
    });
}

#[test]
fn multipart_upload_reaches_the_server_with_a_matching_boundary_and_parts() {
    run(async {
        let server = start_test_server(|req| {
            assert_eq!(req.method, "POST");
            let content_type = req.header("content-type").unwrap().to_string();
            assert!(content_type.starts_with("multipart/form-data; boundary="));
            let boundary = content_type
                .strip_prefix("multipart/form-data; boundary=")
                .unwrap();

            let body = String::from_utf8_lossy(&req.body);
            assert!(body.contains(&format!("--{boundary}\r\n")));
            assert!(
                body.contains("Content-Disposition: form-data; name=\"title\"\r\n\r\nMy Upload")
            );
            assert!(body.contains(
                "Content-Disposition: form-data; name=\"file\"; filename=\"a.txt\"\r\n\
                 Content-Type: text/plain\r\n\r\nhello file"
            ));
            assert!(body.ends_with(&format!("--{boundary}--\r\n")));

            http_response(200, "OK", &[], b"ok")
        });

        let form = Multipart::new()
            .text("title", "My Upload")
            .file_with_content_type("file", "a.txt", "text/plain", b"hello file".to_vec());

        let resp = Client::new()
            .post(&server.url("/upload"))
            .unwrap()
            .multipart(form)
            .unwrap()
            .send()
            .await
            .unwrap();

        assert_eq!(resp.status().as_u16(), 200);
    });
}

#[test]
fn streaming_request_body_with_known_length_uses_content_length() {
    run(async {
        let payload = vec![b'x'; 20_000];
        let expected = payload.clone();
        let server = start_test_server(move |req| {
            assert_eq!(req.method, "POST");
            assert_eq!(req.header("content-length"), Some("20000"));
            assert!(req.header("transfer-encoding").is_none());
            assert_eq!(req.body, expected);
            http_response(200, "OK", &[], b"ok")
        });

        let data = payload.clone();
        let body = Body::streaming(Some(data.len() as u64), move || {
            MemoryReader::new(data.clone(), 777)
        });

        let resp = Client::new()
            .post(&server.url("/"))
            .unwrap()
            .body(body)
            .send()
            .await
            .unwrap();

        assert_eq!(resp.status().as_u16(), 200);
    });
}

#[test]
fn streaming_request_body_with_unknown_length_uses_chunked_encoding() {
    run(async {
        let payload = vec![b'y'; 20_000];
        let expected = payload.clone();
        let server = start_test_server(move |req| {
            assert_eq!(req.method, "POST");
            assert_eq!(req.header("transfer-encoding"), Some("chunked"));
            assert!(req.header("content-length").is_none());
            assert_eq!(req.body, expected);
            http_response(200, "OK", &[], b"ok")
        });

        let data = payload.clone();
        let body = Body::streaming(None, move || MemoryReader::new(data.clone(), 777));

        let resp = Client::new()
            .post(&server.url("/"))
            .unwrap()
            .body(body)
            .send()
            .await
            .unwrap();

        assert_eq!(resp.status().as_u16(), 200);
    });
}

#[test]
fn streaming_request_body_is_reopened_for_a_preserved_redirect_hop() {
    run(async {
        let calls = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let calls_for_handler = calls.clone();
        let server = start_test_server(move |req| {
            assert_eq!(req.body, b"reopen me");
            if calls_for_handler.fetch_add(1, std::sync::atomic::Ordering::SeqCst) == 0 {
                http_response(307, "Temporary Redirect", &[("Location", "/next")], b"")
            } else {
                http_response(200, "OK", &[], b"ok")
            }
        });

        let body = Body::streaming(Some(9), || MemoryReader::new(b"reopen me".to_vec(), 3));

        let resp = Client::new()
            .post(&server.url("/first"))
            .unwrap()
            .body(body)
            .send()
            .await
            .unwrap();

        assert_eq!(resp.status().as_u16(), 200);
        assert_eq!(calls.load(std::sync::atomic::Ordering::SeqCst), 2);
    });
}

#[test]
fn send_streaming_reassembles_a_content_length_response() {
    run(async {
        let server =
            start_test_server(|_req| http_response(200, "OK", &[], b"hello streamed world"));

        let mut resp = Client::new()
            .get(&server.url("/"))
            .unwrap()
            .send_streaming()
            .await
            .unwrap();

        assert_eq!(resp.status().as_u16(), 200);
        let mut collected = Vec::new();
        while let Some(chunk) = resp.chunk().await.unwrap() {
            collected.extend_from_slice(&chunk);
        }
        assert_eq!(collected, b"hello streamed world");
    });
}

#[test]
fn send_streaming_reassembles_a_chunked_response() {
    run(async {
        let server = start_test_server(|_req| {
            http_chunked_response(200, "OK", &[], &[b"hello, ", b"chunked ", b"world"])
        });

        let mut resp = Client::new()
            .get(&server.url("/"))
            .unwrap()
            .send_streaming()
            .await
            .unwrap();

        let mut collected = Vec::new();
        while let Some(chunk) = resp.chunk().await.unwrap() {
            collected.extend_from_slice(&chunk);
        }
        assert_eq!(collected, b"hello, chunked world");
    });
}

#[test]
fn send_streaming_reassembles_a_close_delimited_response() {
    run(async {
        let server = start_test_server(|_req| {
            b"HTTP/1.1 200 OK\r\nX-No-Length: true\r\n\r\nno content-length, no chunking".to_vec()
        });

        let mut resp = Client::new()
            .get(&server.url("/"))
            .unwrap()
            .send_streaming()
            .await
            .unwrap();

        let mut collected = Vec::new();
        while let Some(chunk) = resp.chunk().await.unwrap() {
            collected.extend_from_slice(&chunk);
        }
        assert_eq!(collected, b"no content-length, no chunking");
    });
}

#[test]
fn send_streaming_follows_redirects_to_the_final_hop() {
    run(async {
        let server = start_test_server(|req| {
            if req.target == "/start" {
                http_response(302, "Found", &[("Location", "/end")], b"")
            } else {
                http_response(200, "OK", &[], b"streamed after redirect")
            }
        });

        let mut resp = Client::new()
            .get(&server.url("/start"))
            .unwrap()
            .send_streaming()
            .await
            .unwrap();

        assert_eq!(resp.status().as_u16(), 200);
        assert_eq!(resp.url().path, "/end");
        let mut collected = Vec::new();
        while let Some(chunk) = resp.chunk().await.unwrap() {
            collected.extend_from_slice(&chunk);
        }
        assert_eq!(collected, b"streamed after redirect");
    });
}

#[test]
fn query_params_are_appended_and_percent_encoded() {
    run(async {
        let server = start_test_server(|req| {
            assert_eq!(req.target, "/search?q=hello%20world&page=2");
            http_response(200, "OK", &[], b"ok")
        });

        let resp = Client::new()
            .get(&server.url("/search"))
            .unwrap()
            .query([("q", "hello world"), ("page", "2")])
            .send()
            .await
            .unwrap();
        assert_eq!(resp.status().as_u16(), 200);
    });
}

#[test]
fn custom_and_default_headers_are_sent() {
    run(async {
        let server = start_test_server(|req| {
            assert_eq!(req.header("x-api-key"), Some("secret"));
            assert!(req
                .header("user-agent")
                .unwrap()
                .starts_with("rusty_request/"));
            http_response(200, "OK", &[], b"")
        });

        let client = Client::builder()
            .default_header("X-Api-Key", "secret")
            .unwrap()
            .build();

        client.get(&server.url("/")).unwrap().send().await.unwrap();
    });
}

async fn assert_method_round_trips(method: &str, builder: rusty_request::RequestBuilder) {
    let resp = builder.body("payload").send().await.unwrap();
    assert_eq!(resp.status().as_u16(), 200);
    assert_eq!(resp.text().unwrap(), method);
}

#[test]
fn put_patch_delete_reach_the_server_with_the_right_method() {
    run(async {
        let client = Client::new();

        let server = start_test_server(|req| {
            assert_eq!(req.method, "PUT");
            http_response(200, "OK", &[], b"PUT")
        });
        assert_method_round_trips("PUT", client.put(&server.url("/")).unwrap()).await;

        let server = start_test_server(|req| {
            assert_eq!(req.method, "PATCH");
            http_response(200, "OK", &[], b"PATCH")
        });
        assert_method_round_trips("PATCH", client.patch(&server.url("/")).unwrap()).await;

        let server = start_test_server(|req| {
            assert_eq!(req.method, "DELETE");
            http_response(200, "OK", &[], b"DELETE")
        });
        assert_method_round_trips("DELETE", client.delete(&server.url("/")).unwrap()).await;
    });
}

#[test]
fn head_request_never_reads_a_body_even_if_headers_claim_one() {
    run(async {
        let server = start_test_server(|req| {
            assert_eq!(req.method, "HEAD");
            // A real server sends the Content-Length a GET would have,
            // but zero actual body bytes, for HEAD. If the client
            // didn't special-case HEAD it would hang waiting for bytes
            // that never arrive.
            // No actual body bytes follow, even though Content-Length
            // claims 12345 -- exactly what a real server does for HEAD.
            b"HTTP/1.1 200 OK\r\nContent-Length: 12345\r\n\r\n".to_vec()
        });

        let resp = Client::new()
            .head(&server.url("/"))
            .unwrap()
            .send()
            .await
            .unwrap();
        assert_eq!(resp.status().as_u16(), 200);
        assert!(resp.bytes().is_empty());
    });
}

#[test]
fn chunked_response_is_decoded() {
    run(async {
        let server = start_test_server(|_req| {
            http_chunked_response(200, "OK", &[], &[b"hello, ", b"chunked ", b"world"])
        });

        let resp = rusty_request::get(&server.url("/")).await.unwrap();
        assert_eq!(resp.text().unwrap(), "hello, chunked world");
    });
}

#[test]
fn close_delimited_body_without_content_length_is_read_to_eof() {
    run(async {
        let server = start_test_server(|_req| {
            b"HTTP/1.1 200 OK\r\nX-No-Length: true\r\n\r\nno content-length, no chunking".to_vec()
        });

        let resp = rusty_request::get(&server.url("/")).await.unwrap();
        assert_eq!(resp.text().unwrap(), "no content-length, no chunking");
    });
}

#[test]
fn error_for_status_rejects_4xx_and_5xx() {
    run(async {
        let server = start_test_server(|_req| http_response(404, "Not Found", &[], b"nope"));
        let resp = rusty_request::get(&server.url("/missing")).await.unwrap();
        assert!(resp.status().is_client_error());
        match resp.error_for_status() {
            Err(Error::Status(s)) => assert_eq!(s.as_u16(), 404),
            other => panic!("expected Error::Status(404), got {other:?}"),
        }
    });
}

#[test]
fn error_for_status_passes_through_2xx() {
    run(async {
        let server = start_test_server(|_req| http_response(200, "OK", &[], b"fine"));
        let resp = rusty_request::get(&server.url("/")).await.unwrap();
        let resp = resp.error_for_status().unwrap();
        assert_eq!(resp.text().unwrap(), "fine");
    });
}

#[test]
fn request_times_out_when_the_server_never_responds() {
    run(async {
        let listener = rusty_tokio::io::TcpListener::bind("127.0.0.1:0".parse().unwrap()).unwrap();
        let addr = listener.local_addr().unwrap();
        rusty_tokio::spawn(async move {
            // Accept and then simply never write a response. Keeping
            // `stream` alive (not dropping it immediately) matters:
            // dropping a socket with the client's already-written
            // request bytes still sitting unread in the receive buffer
            // makes the kernel send RST instead of a clean close, which
            // would surface as a spurious `ConnectionReset` on the
            // client instead of the timeout this test means to exercise.
            if let Ok((stream, _peer)) = listener.accept().await {
                rusty_tokio::time::sleep(Duration::from_secs(5)).await;
                drop(stream);
            }
        });

        let result = Client::new()
            .get(&format!("http://{addr}/"))
            .unwrap()
            .timeout(Duration::from_millis(100))
            .send()
            .await;

        match result {
            Err(Error::Timeout) => {}
            other => panic!("expected Error::Timeout, got {other:?}"),
        }
    });
}

#[test]
fn unsupported_scheme_is_rejected_before_connecting() {
    run(async {
        let result = rusty_request::get("https://example.com/").await;
        match result {
            Err(Error::UnsupportedScheme(scheme)) => assert_eq!(scheme, "https"),
            other => panic!("expected Error::UnsupportedScheme, got {other:?}"),
        }
    });
}

#[test]
fn invalid_url_is_rejected() {
    run(async {
        let result = rusty_request::get("not a url").await;
        assert!(matches!(result, Err(Error::InvalidUrl(_))));
    });
}

#[test]
fn request_level_basic_auth_sets_authorization_header() {
    run(async {
        let server = start_test_server(|req| {
            // "Aladdin:open sesame" -- the RFC 7617 example.
            assert_eq!(
                req.header("authorization"),
                Some("Basic QWxhZGRpbjpvcGVuIHNlc2FtZQ==")
            );
            http_response(200, "OK", &[], b"ok")
        });

        let resp = Client::new()
            .get(&server.url("/"))
            .unwrap()
            .basic_auth("Aladdin", "open sesame")
            .unwrap()
            .send()
            .await
            .unwrap();
        assert_eq!(resp.status().as_u16(), 200);
    });
}

#[test]
fn request_level_bearer_auth_sets_authorization_header() {
    run(async {
        let server = start_test_server(|req| {
            assert_eq!(req.header("authorization"), Some("Bearer secret-token"));
            http_response(200, "OK", &[], b"ok")
        });

        let resp = Client::new()
            .get(&server.url("/"))
            .unwrap()
            .bearer_auth("secret-token")
            .unwrap()
            .send()
            .await
            .unwrap();
        assert_eq!(resp.status().as_u16(), 200);
    });
}

#[test]
fn client_level_basic_auth_applies_to_every_request_but_request_level_overrides() {
    run(async {
        let server = start_test_server(|req| {
            let expected = if req.target == "/override" {
                "Basic Zm9vOmJhcg=="
            } else {
                "Basic QWxhZGRpbjpvcGVuIHNlc2FtZQ=="
            };
            assert_eq!(req.header("authorization"), Some(expected));
            http_response(200, "OK", &[], b"ok")
        });

        let client = Client::builder()
            .basic_auth("Aladdin", "open sesame")
            .unwrap()
            .build();

        // Uses the client-level default.
        client.get(&server.url("/")).unwrap().send().await.unwrap();

        // A request-level basic_auth() overrides the client default.
        client
            .get(&server.url("/override"))
            .unwrap()
            .basic_auth("foo", "bar")
            .unwrap()
            .send()
            .await
            .unwrap();
    });
}

#[test]
fn client_level_bearer_auth_applies_to_every_request() {
    run(async {
        let server = start_test_server(|req| {
            assert_eq!(req.header("authorization"), Some("Bearer client-token"));
            http_response(200, "OK", &[], b"ok")
        });

        let client = Client::builder()
            .bearer_auth("client-token")
            .unwrap()
            .build();

        client.get(&server.url("/")).unwrap().send().await.unwrap();
        client
            .get(&server.url("/again"))
            .unwrap()
            .send()
            .await
            .unwrap();
    });
}

#[test]
fn redirect_301_get_is_followed() {
    run(async {
        let server = start_test_server(|req| match req.target.as_str() {
            "/start" => http_response(301, "Moved Permanently", &[("Location", "/end")], b""),
            "/end" => http_response(200, "OK", &[], b"final"),
            other => panic!("unexpected request to {other}"),
        });

        let resp = rusty_request::get(&server.url("/start")).await.unwrap();
        assert_eq!(resp.status().as_u16(), 200);
        assert_eq!(resp.text().unwrap(), "final");
        assert_eq!(resp.url().path, "/end");
    });
}

#[test]
fn redirect_303_downgrades_post_to_bodyless_get() {
    run(async {
        let server = start_test_server(|req| match req.target.as_str() {
            "/start" => {
                assert_eq!(req.method, "POST");
                http_response(303, "See Other", &[("Location", "/end")], b"")
            }
            "/end" => {
                assert_eq!(req.method, "GET");
                assert!(req.body.is_empty());
                http_response(200, "OK", &[], b"ok")
            }
            other => panic!("unexpected request to {other}"),
        });

        let resp = Client::new()
            .post(&server.url("/start"))
            .unwrap()
            .body("original payload")
            .send()
            .await
            .unwrap();
        assert_eq!(resp.status().as_u16(), 200);
    });
}

#[test]
fn redirect_301_downgrades_non_get_head_to_bodyless_get() {
    run(async {
        let server = start_test_server(|req| match req.target.as_str() {
            "/start" => {
                assert_eq!(req.method, "POST");
                http_response(301, "Moved Permanently", &[("Location", "/end")], b"")
            }
            "/end" => {
                assert_eq!(req.method, "GET");
                assert!(req.body.is_empty());
                http_response(200, "OK", &[], b"ok")
            }
            other => panic!("unexpected request to {other}"),
        });

        Client::new()
            .post(&server.url("/start"))
            .unwrap()
            .body("payload")
            .send()
            .await
            .unwrap();
    });
}

#[test]
fn redirect_307_preserves_method_and_body() {
    run(async {
        let server = start_test_server(|req| match req.target.as_str() {
            "/start" => {
                assert_eq!(req.method, "POST");
                http_response(307, "Temporary Redirect", &[("Location", "/end")], b"")
            }
            "/end" => {
                assert_eq!(req.method, "POST");
                assert_eq!(req.body, b"payload");
                http_response(200, "OK", &[], b"ok")
            }
            other => panic!("unexpected request to {other}"),
        });

        Client::new()
            .post(&server.url("/start"))
            .unwrap()
            .body("payload")
            .send()
            .await
            .unwrap();
    });
}

#[test]
fn too_many_redirects_returns_error() {
    run(async {
        let server =
            start_test_server(|_req| http_response(302, "Found", &[("Location", "/loop")], b""));

        let result = Client::new()
            .get(&server.url("/loop"))
            .unwrap()
            .max_redirects(3)
            .send()
            .await;

        match result {
            Err(Error::TooManyRedirects(3)) => {}
            other => panic!("expected Error::TooManyRedirects(3), got {other:?}"),
        }
    });
}

#[test]
fn no_redirects_returns_the_raw_3xx_response() {
    run(async {
        let server =
            start_test_server(|_req| http_response(302, "Found", &[("Location", "/end")], b""));

        let resp = Client::new()
            .get(&server.url("/start"))
            .unwrap()
            .no_redirects()
            .send()
            .await
            .unwrap();

        assert_eq!(resp.status().as_u16(), 302);
        assert_eq!(resp.headers().get("location"), Some("/end"));
    });
}

#[test]
fn cross_origin_redirect_strips_authorization_header() {
    run(async {
        let target = start_test_server(|req| {
            assert_eq!(req.header("authorization"), None);
            http_response(200, "OK", &[], b"ok")
        });
        let target_url = target.url("/end");

        let entry = start_test_server(move |req| {
            assert_eq!(
                req.header("authorization"),
                Some("Basic QWxhZGRpbjpvcGVuIHNlc2FtZQ==")
            );
            http_response(302, "Found", &[("Location", target_url.as_str())], b"")
        });

        let resp = Client::new()
            .get(&entry.url("/start"))
            .unwrap()
            .basic_auth("Aladdin", "open sesame")
            .unwrap()
            .send()
            .await
            .unwrap();
        assert_eq!(resp.status().as_u16(), 200);
    });
}

#[test]
fn cookie_set_on_one_request_is_sent_on_the_next_through_the_same_client() {
    run(async {
        let server = start_test_server(|req| match req.target.as_str() {
            "/login" => http_response(200, "OK", &[("Set-Cookie", "session=abc123")], b""),
            "/profile" => {
                assert_eq!(req.header("cookie"), Some("session=abc123"));
                http_response(200, "OK", &[], b"")
            }
            other => panic!("unexpected request to {other}"),
        });

        let client = Client::new();
        client
            .get(&server.url("/login"))
            .unwrap()
            .send()
            .await
            .unwrap();
        client
            .get(&server.url("/profile"))
            .unwrap()
            .send()
            .await
            .unwrap();
    });
}

#[test]
fn cookies_are_not_shared_across_different_clients() {
    run(async {
        let server = start_test_server(|req| match req.target.as_str() {
            "/login" => http_response(200, "OK", &[("Set-Cookie", "session=abc123")], b""),
            "/profile" => {
                assert_eq!(req.header("cookie"), None);
                http_response(200, "OK", &[], b"")
            }
            other => panic!("unexpected request to {other}"),
        });

        Client::new()
            .get(&server.url("/login"))
            .unwrap()
            .send()
            .await
            .unwrap();
        // A different `Client` -- a different session -- must not see
        // the first client's cookie.
        Client::new()
            .get(&server.url("/profile"))
            .unwrap()
            .send()
            .await
            .unwrap();
    });
}

#[test]
fn cookie_set_on_a_redirect_hop_is_sent_on_the_next_hop() {
    run(async {
        let server = start_test_server(|req| match req.target.as_str() {
            "/start" => http_response(
                302,
                "Found",
                &[("Set-Cookie", "session=abc123"), ("Location", "/end")],
                b"",
            ),
            "/end" => {
                assert_eq!(req.header("cookie"), Some("session=abc123"));
                http_response(200, "OK", &[], b"ok")
            }
            other => panic!("unexpected request to {other}"),
        });

        let resp = rusty_request::get(&server.url("/start")).await.unwrap();
        assert_eq!(resp.status().as_u16(), 200);
    });
}

#[test]
fn no_cookie_store_disables_cookie_persistence() {
    run(async {
        let server = start_test_server(|req| match req.target.as_str() {
            "/login" => http_response(200, "OK", &[("Set-Cookie", "session=abc123")], b""),
            "/profile" => {
                assert_eq!(req.header("cookie"), None);
                http_response(200, "OK", &[], b"")
            }
            other => panic!("unexpected request to {other}"),
        });

        let client = Client::builder().no_cookie_store().build();
        client
            .get(&server.url("/login"))
            .unwrap()
            .send()
            .await
            .unwrap();
        client
            .get(&server.url("/profile"))
            .unwrap()
            .send()
            .await
            .unwrap();
    });
}

#[test]
fn secure_cookie_is_never_sent_over_plain_http_end_to_end() {
    run(async {
        let server = start_test_server(|req| match req.target.as_str() {
            "/login" => http_response(200, "OK", &[("Set-Cookie", "session=abc123; Secure")], b""),
            "/profile" => {
                assert_eq!(req.header("cookie"), None);
                http_response(200, "OK", &[], b"")
            }
            other => panic!("unexpected request to {other}"),
        });

        let client = Client::new();
        client
            .get(&server.url("/login"))
            .unwrap()
            .send()
            .await
            .unwrap();
        client
            .get(&server.url("/profile"))
            .unwrap()
            .send()
            .await
            .unwrap();
    });
}

#[test]
fn connection_is_reused_for_a_second_request_to_the_same_origin() {
    run(async {
        let server = start_test_server(|_req| http_response(200, "OK", &[], b"ok"));

        let client = Client::new();
        client.get(&server.url("/a")).unwrap().send().await.unwrap();
        client.get(&server.url("/b")).unwrap().send().await.unwrap();

        assert_eq!(server.connections_accepted(), 1);
    });
}

#[test]
fn connection_is_not_reused_when_response_says_close() {
    run(async {
        let server =
            start_test_server(|_req| http_response(200, "OK", &[("Connection", "close")], b"ok"));

        let client = Client::new();
        client.get(&server.url("/a")).unwrap().send().await.unwrap();
        client.get(&server.url("/b")).unwrap().send().await.unwrap();

        assert_eq!(server.connections_accepted(), 2);
    });
}

#[test]
fn no_pool_never_reuses_a_connection() {
    run(async {
        let server = start_test_server(|_req| http_response(200, "OK", &[], b"ok"));

        let client = Client::builder().no_pool().build();
        client.get(&server.url("/a")).unwrap().send().await.unwrap();
        client.get(&server.url("/b")).unwrap().send().await.unwrap();

        assert_eq!(server.connections_accepted(), 2);
    });
}

#[test]
fn stale_pooled_connection_is_retried_on_a_fresh_connection() {
    run(async {
        let listener = rusty_tokio::io::TcpListener::bind("127.0.0.1:0".parse().unwrap()).unwrap();
        let addr = listener.local_addr().unwrap();

        rusty_tokio::spawn(async move {
            loop {
                let (stream, _peer) = match listener.accept().await {
                    Ok(pair) => pair,
                    Err(_) => break,
                };
                rusty_tokio::spawn(async move {
                    // Serve exactly one request per accepted connection,
                    // then let `stream` drop -- every connection looks,
                    // from the client's point of view, like a server
                    // that hung up right after its own keep-alive idle
                    // timeout, in between the client's two requests.
                    if let Ok(_req) = common::read_request(&stream).await {
                        let _ = stream
                            .write_all(&common::http_response(200, "OK", &[], b"ok"))
                            .await;
                    }
                });
            }
        });

        let client = Client::new();
        let first = client
            .get(&format!("http://{addr}/a"))
            .unwrap()
            .send()
            .await
            .unwrap();
        assert_eq!(first.status().as_u16(), 200);

        // Give the per-connection task above time to finish and drop
        // its stream, so the pooled connection is genuinely dead by the
        // time the client tries to reuse it below -- otherwise this
        // test wouldn't reliably exercise the retry path at all.
        rusty_tokio::time::sleep(Duration::from_millis(50)).await;

        let second = client
            .get(&format!("http://{addr}/b"))
            .unwrap()
            .send()
            .await
            .unwrap();
        assert_eq!(second.status().as_u16(), 200);
    });
}

#[test]
fn retry_recovers_from_a_retryable_status_code() {
    run(async {
        let calls = Arc::new(AtomicUsize::new(0));
        let calls_for_handler = calls.clone();
        let server = start_test_server(move |_req| {
            if calls_for_handler.fetch_add(1, Ordering::SeqCst) == 0 {
                http_response(503, "Service Unavailable", &[], b"")
            } else {
                http_response(200, "OK", &[], b"ok")
            }
        });

        let policy = RetryPolicy::new(2).backoff(Backoff::fixed(Duration::from_millis(1)));
        let resp = Client::new()
            .get(&server.url("/"))
            .unwrap()
            .retry(policy)
            .send()
            .await
            .unwrap();

        assert_eq!(resp.status().as_u16(), 200);
        assert_eq!(calls.load(Ordering::SeqCst), 2);
    });
}

#[test]
fn retry_exhausts_and_returns_the_last_retryable_response() {
    run(async {
        let calls = Arc::new(AtomicUsize::new(0));
        let calls_for_handler = calls.clone();
        let server = start_test_server(move |_req| {
            calls_for_handler.fetch_add(1, Ordering::SeqCst);
            http_response(503, "Service Unavailable", &[], b"")
        });

        let policy = RetryPolicy::new(2).backoff(Backoff::fixed(Duration::from_millis(1)));
        let resp = Client::new()
            .get(&server.url("/"))
            .unwrap()
            .retry(policy)
            .send()
            .await
            .unwrap();

        assert_eq!(resp.status().as_u16(), 503);
        // The first attempt plus 2 configured retries: 3 total.
        assert_eq!(calls.load(Ordering::SeqCst), 3);
    });
}

#[test]
fn non_idempotent_requests_are_not_retried_by_default() {
    run(async {
        let calls = Arc::new(AtomicUsize::new(0));
        let calls_for_handler = calls.clone();
        let server = start_test_server(move |_req| {
            calls_for_handler.fetch_add(1, Ordering::SeqCst);
            http_response(503, "Service Unavailable", &[], b"")
        });

        let policy = RetryPolicy::new(3).backoff(Backoff::fixed(Duration::from_millis(1)));
        let resp = Client::new()
            .post(&server.url("/"))
            .unwrap()
            .retry(policy)
            .send()
            .await
            .unwrap();

        assert_eq!(resp.status().as_u16(), 503);
        assert_eq!(calls.load(Ordering::SeqCst), 1);
    });
}

#[test]
fn retry_non_idempotent_opts_a_post_into_retrying() {
    run(async {
        let calls = Arc::new(AtomicUsize::new(0));
        let calls_for_handler = calls.clone();
        let server = start_test_server(move |_req| {
            if calls_for_handler.fetch_add(1, Ordering::SeqCst) == 0 {
                http_response(503, "Service Unavailable", &[], b"")
            } else {
                http_response(200, "OK", &[], b"ok")
            }
        });

        let policy = RetryPolicy::new(2)
            .backoff(Backoff::fixed(Duration::from_millis(1)))
            .retry_non_idempotent();
        let resp = Client::new()
            .post(&server.url("/"))
            .unwrap()
            .retry(policy)
            .send()
            .await
            .unwrap();

        assert_eq!(resp.status().as_u16(), 200);
        assert_eq!(calls.load(Ordering::SeqCst), 2);
    });
}

#[test]
fn no_retry_policy_never_retries() {
    run(async {
        let calls = Arc::new(AtomicUsize::new(0));
        let calls_for_handler = calls.clone();
        let server = start_test_server(move |_req| {
            calls_for_handler.fetch_add(1, Ordering::SeqCst);
            http_response(503, "Service Unavailable", &[], b"")
        });

        let resp = Client::new()
            .get(&server.url("/"))
            .unwrap()
            .send()
            .await
            .unwrap();

        assert_eq!(resp.status().as_u16(), 503);
        assert_eq!(calls.load(Ordering::SeqCst), 1);
    });
}

#[test]
fn connection_refused_is_retried_on_a_fresh_connection() {
    run(async {
        // Bind to grab an ephemeral port, then immediately drop the
        // listener -- the first connection attempt against it fails
        // with "connection refused". A delayed task rebinds the exact
        // same port a little later and serves one request, so the
        // retry (not the original attempt) is what actually succeeds.
        let listener = rusty_tokio::io::TcpListener::bind("127.0.0.1:0".parse().unwrap()).unwrap();
        let addr = listener.local_addr().unwrap();
        drop(listener);

        rusty_tokio::spawn(async move {
            rusty_tokio::time::sleep(Duration::from_millis(20)).await;
            let listener = rusty_tokio::io::TcpListener::bind(addr).unwrap();
            if let Ok((stream, _peer)) = listener.accept().await {
                if let Ok(_req) = common::read_request(&stream).await {
                    let _ = stream
                        .write_all(&http_response(200, "OK", &[], b"ok"))
                        .await;
                }
            }
        });

        let policy = RetryPolicy::new(1).backoff(Backoff::fixed(Duration::from_millis(60)));
        let resp = Client::new()
            .get(&format!("http://{addr}/"))
            .unwrap()
            .retry(policy)
            .send()
            .await
            .unwrap();

        assert_eq!(resp.status().as_u16(), 200);
    });
}

#[test]
fn retry_respects_the_retry_after_header() {
    run(async {
        let calls = Arc::new(AtomicUsize::new(0));
        let calls_for_handler = calls.clone();
        let server = start_test_server(move |_req| {
            if calls_for_handler.fetch_add(1, Ordering::SeqCst) == 0 {
                http_response(503, "Service Unavailable", &[("Retry-After", "1")], b"")
            } else {
                http_response(200, "OK", &[], b"ok")
            }
        });

        // The policy's own backoff would retry near-instantly; a
        // 1-second `Retry-After` should override it and make the whole
        // call take at least ~1s.
        let policy = RetryPolicy::new(1).backoff(Backoff::fixed(Duration::from_millis(1)));
        let start = std::time::Instant::now();
        let resp = Client::new()
            .get(&server.url("/"))
            .unwrap()
            .retry(policy)
            .send()
            .await
            .unwrap();

        assert_eq!(resp.status().as_u16(), 200);
        assert!(start.elapsed() >= Duration::from_millis(900));
    });
}

#[test]
fn proxy_routes_the_connection_and_sends_an_absolute_form_target() {
    run(async {
        // The target host is deliberately unresolvable via real DNS --
        // the request only succeeds at all if the client routed the
        // connection to the proxy instead of trying to resolve/connect
        // to the target directly.
        let proxy = start_test_server(|req| {
            assert_eq!(req.target, "http://internal.example.test/data?x=1");
            assert_eq!(req.header("host"), Some("internal.example.test"));
            http_response(200, "OK", &[], b"via proxy")
        });

        let client = Client::builder().proxy(&proxy.url("")).unwrap().build();
        let resp = client
            .get("http://internal.example.test/data?x=1")
            .unwrap()
            .send()
            .await
            .unwrap();

        assert_eq!(resp.status().as_u16(), 200);
        assert_eq!(resp.text().unwrap(), "via proxy");
    });
}

#[test]
fn proxy_bypass_connects_directly_for_matching_hosts() {
    run(async {
        let origin = start_test_server(|req| {
            // Origin-form, not absolute-form -- proves this request went
            // straight to the origin rather than through the proxy.
            assert_eq!(req.target, "/direct");
            http_response(200, "OK", &[], b"direct")
        });
        let proxy = start_test_server(|_req| http_response(200, "OK", &[], b"via proxy"));

        let client = Client::builder()
            .proxy(&proxy.url(""))
            .unwrap()
            .proxy_bypass(["127.0.0.1"])
            .build();

        let resp = client
            .get(&origin.url("/direct"))
            .unwrap()
            .send()
            .await
            .unwrap();

        assert_eq!(resp.text().unwrap(), "direct");
        assert_eq!(proxy.connections_accepted(), 0);
        assert_eq!(origin.connections_accepted(), 1);
    });
}

#[test]
fn proxy_bypass_star_disables_proxying_entirely() {
    run(async {
        let origin = start_test_server(|_req| http_response(200, "OK", &[], b"direct"));
        let proxy = start_test_server(|_req| http_response(200, "OK", &[], b"via proxy"));

        let client = Client::builder()
            .proxy(&proxy.url(""))
            .unwrap()
            .proxy_bypass(["*"])
            .build();

        client.get(&origin.url("/")).unwrap().send().await.unwrap();

        assert_eq!(proxy.connections_accepted(), 0);
        assert_eq!(origin.connections_accepted(), 1);
    });
}

#[test]
fn no_proxy_disables_a_previously_configured_proxy() {
    run(async {
        let origin = start_test_server(|_req| http_response(200, "OK", &[], b"direct"));
        let proxy = start_test_server(|_req| http_response(200, "OK", &[], b"via proxy"));

        let client = Client::builder()
            .proxy(&proxy.url(""))
            .unwrap()
            .no_proxy()
            .build();

        client.get(&origin.url("/")).unwrap().send().await.unwrap();

        assert_eq!(proxy.connections_accepted(), 0);
        assert_eq!(origin.connections_accepted(), 1);
    });
}

#[test]
fn request_level_proxy_overrides_the_client_default() {
    run(async {
        let client_proxy = start_test_server(|_req| http_response(200, "OK", &[], b"client proxy"));
        let request_proxy =
            start_test_server(|_req| http_response(200, "OK", &[], b"request proxy"));

        let client = Client::builder()
            .proxy(&client_proxy.url(""))
            .unwrap()
            .build();

        let resp = client
            .get("http://some.unresolvable.example/")
            .unwrap()
            .proxy(&request_proxy.url(""))
            .unwrap()
            .send()
            .await
            .unwrap();

        assert_eq!(resp.text().unwrap(), "request proxy");
        assert_eq!(client_proxy.connections_accepted(), 0);
        assert_eq!(request_proxy.connections_accepted(), 1);
    });
}

#[test]
fn proxy_connection_is_reused_across_different_origins() {
    run(async {
        let proxy = start_test_server(|_req| http_response(200, "OK", &[], b"ok"));
        let client = Client::builder().proxy(&proxy.url("")).unwrap().build();

        client
            .get("http://first.unresolvable.example/")
            .unwrap()
            .send()
            .await
            .unwrap();
        client
            .get("http://second.unresolvable.example/")
            .unwrap()
            .send()
            .await
            .unwrap();

        // One persistent connection to the proxy served both (different)
        // origins -- correct for plain forward-proxying, unlike a direct
        // connection pool which is keyed per-origin.
        assert_eq!(proxy.connections_accepted(), 1);
    });
}
