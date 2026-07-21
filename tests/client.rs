mod common;

use common::{http_chunked_response, http_response, run, start_test_server};
use rusty_request::{Client, Error, Json};
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
