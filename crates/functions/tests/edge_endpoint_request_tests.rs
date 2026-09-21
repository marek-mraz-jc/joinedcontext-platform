//! Edge cases of `endpoint::request`, the sandbox's one host call (T-2498, SDK-22, GW10).
//!
//! **The contract.** One HTTP call to the function's own endpoint through the gateway: GET, POST,
//! PATCH or DELETE only, with the caller's token or none, `Accept: application/json` always, and a
//! body only on POST and PATCH. It answers `{status, body}`, `status: 0` when the gateway did not
//! answer or cut its answer off, and never panics on an answer that is not JSON or too large.
//!
//! **Inputs.** The method (a string the function's code chose), the path (checked by `allowed`,
//! whose cases are the module's own tests), an optional body, and the gateway's answer.

use functions::endpoint::{request, Endpoint, ANSWER_LIMIT};
use serde_json::{json, Value};
use tokio::io::AsyncWriteExt;
use wiremock::matchers::any;
use wiremock::{Mock, MockServer, ResponseTemplate};

const SLUG: &str = "k7m2qz4tv6xh3n5jb2ryd3wcfa";

fn path() -> String {
    format!("/api/endpoint/{SLUG}/ngsi-ld/v1/entities?type=A")
}

fn endpoint(gateway: &str, token: Option<&str>) -> Endpoint {
    Endpoint {
        http: reqwest::Client::new(),
        gateway: gateway.to_owned(),
        slug: SLUG.to_owned(),
        token: token.map(str::to_owned),
    }
}

async fn answering(template: ResponseTemplate) -> MockServer {
    let server = MockServer::start().await;
    Mock::given(any())
        .respond_with(template)
        .mount(&server)
        .await;
    server
}

/// SDK-22: a method the runtime does not send is refused before anything leaves the sandbox.
#[tokio::test]
async fn a_method_outside_get_post_patch_delete_is_403_before_any_request_is_sent() {
    let server = answering(ResponseTemplate::new(200)).await;
    for method in [
        "PUT", "HEAD", "OPTIONS", "CONNECT", "TRACE", "get", "Post", "", "GET ",
    ] {
        let answer = request(&endpoint(&server.uri(), Some("t")), method, &path(), None).await;
        assert_eq!(answer["status"], 403, "{method:?}: {answer}");
    }
    assert!(server
        .received_requests()
        .await
        .unwrap_or_default()
        .is_empty());
}

/// SDK-22: an answer past `ANSWER_LIMIT` is a 502 of the runtime's own, not a buffered body.
#[tokio::test]
async fn an_answer_over_8_mib_is_502_and_the_stream_is_abandoned() {
    let server =
        answering(ResponseTemplate::new(200).set_body_bytes(vec![b'a'; ANSWER_LIMIT + 1])).await;
    let answer = request(&endpoint(&server.uri(), None), "GET", &path(), None).await;
    assert_eq!(answer["status"], 502, "{}", answer["body"]);
    assert!(answer["body"]["title"]
        .as_str()
        .is_some_and(|t| t.contains("8 MiB")));

    let server =
        answering(ResponseTemplate::new(200).set_body_bytes(vec![b'a'; ANSWER_LIMIT])).await;
    let answer = request(&endpoint(&server.uri(), None), "GET", &path(), None).await;
    assert_eq!(
        answer["status"], 200,
        "exactly the limit is still an answer"
    );
}

/// SDK-22: a gateway that promises more than it sends is an answer cut off, status 0.
#[tokio::test]
async fn a_chunk_read_error_mid_stream_answers_status_zero_not_a_panic() {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("a port");
    let address = listener.local_addr().expect("an address");
    tokio::spawn(async move {
        if let Ok((mut socket, _)) = listener.accept().await {
            let mut buffer = [0u8; 4096];
            let _ = tokio::io::AsyncReadExt::read(&mut socket, &mut buffer).await;
            let _ = socket
                .write_all(b"HTTP/1.1 200 OK\r\ncontent-length: 1000\r\n\r\n{\"partial\":")
                .await;
            let _ = socket.shutdown().await;
        }
    });
    let answer = request(
        &endpoint(&format!("http://{address}"), None),
        "GET",
        &path(),
        None,
    )
    .await;
    assert_eq!(answer["status"], 0, "{answer}");
    assert!(answer["body"]["title"].is_string(), "{answer}");
}

/// SDK-22: GET and DELETE carry no body, whatever the function handed over.
#[tokio::test]
async fn a_get_or_delete_with_a_body_never_attaches_it_to_the_request() {
    let server = answering(ResponseTemplate::new(204)).await;
    for method in ["GET", "DELETE"] {
        request(
            &endpoint(&server.uri(), Some("t")),
            method,
            &path(),
            Some(r#"{"smuggled":true}"#.to_owned()),
        )
        .await;
    }
    for sent in server.received_requests().await.unwrap_or_default() {
        assert!(
            sent.body.is_empty(),
            "{} carried {:?}",
            sent.method,
            sent.body
        );
        assert!(
            sent.headers.get("content-type").is_none(),
            "{}",
            sent.method
        );
    }
}

/// SDK-22: POST and PATCH send their body as JSON.
#[tokio::test]
async fn a_post_or_patch_body_is_sent_with_the_json_content_type() {
    let server = answering(ResponseTemplate::new(204)).await;
    for method in ["POST", "PATCH"] {
        request(
            &endpoint(&server.uri(), Some("t")),
            method,
            &path(),
            Some(r#"{"pm10":{"value":1}}"#.to_owned()),
        )
        .await;
    }
    let sent = server.received_requests().await.unwrap_or_default();
    assert_eq!(sent.len(), 2);
    for request in sent {
        assert_eq!(
            request
                .headers
                .get("content-type")
                .and_then(|v| v.to_str().ok()),
            Some("application/json"),
            "{}",
            request.method
        );
        assert_eq!(request.body, br#"{"pm10":{"value":1}}"#);
    }
}

/// GW10: without the caller's token the endpoint is called as the anonymous public.
#[tokio::test]
async fn no_token_calls_the_endpoint_anonymously_without_an_authorization_header() {
    let server = answering(ResponseTemplate::new(200).set_body_json(json!([]))).await;
    request(&endpoint(&server.uri(), None), "GET", &path(), None).await;
    request(
        &endpoint(&server.uri(), Some("caller-token")),
        "GET",
        &path(),
        None,
    )
    .await;
    let sent = server.received_requests().await.unwrap_or_default();
    assert!(sent[0].headers.get("authorization").is_none());
    assert_eq!(
        sent[1]
            .headers
            .get("authorization")
            .and_then(|v| v.to_str().ok()),
        Some("Bearer caller-token")
    );
}

/// SDK-22: a gateway that does not answer is status 0 with a title that names nothing of it.
#[tokio::test]
async fn a_transport_error_answers_status_zero_and_a_generic_title() {
    let answer = request(
        &endpoint("http://127.0.0.1:1", Some("t")),
        "GET",
        &path(),
        None,
    )
    .await;
    assert_eq!(answer["status"], 0, "{answer}");
    let title = answer["body"]["title"].as_str().expect("a title");
    assert!(
        !title.contains("127.0.0.1") && !title.contains("refused"),
        "{title}"
    );
}

/// SDK-22: an answer that is not JSON reaches the function as a string.
#[tokio::test]
async fn a_non_json_answer_body_is_returned_as_a_string_not_an_error() {
    for body in ["<html>x</html>", "not json", "{", "\u{00e9}t\u{00e9}"] {
        let server = answering(ResponseTemplate::new(200).set_body_string(body)).await;
        let answer = request(&endpoint(&server.uri(), None), "GET", &path(), None).await;
        assert_eq!(answer["status"], 200);
        assert_eq!(answer["body"], Value::String(body.to_owned()), "{body}");
    }
    let server = answering(ResponseTemplate::new(200).set_body_bytes(vec![0xff, 0xfe, b'a'])).await;
    let answer = request(&endpoint(&server.uri(), None), "GET", &path(), None).await;
    assert!(
        answer["body"].is_string(),
        "bytes that are not UTF-8 are still a string: {answer}"
    );
}

/// SDK-22: no body is `null`, never `""`.
#[tokio::test]
async fn an_empty_body_answers_null_not_an_empty_string() {
    for status in [200, 204, 404] {
        let server = answering(ResponseTemplate::new(status)).await;
        let answer = request(&endpoint(&server.uri(), None), "GET", &path(), None).await;
        assert_eq!(answer, json!({ "status": status, "body": null }));
    }
}

/// SDK-22: every call asks for JSON, whatever method or body the function chose.
#[tokio::test]
async fn the_accept_header_is_always_application_json_regardless_of_the_functions_request() {
    let server = answering(ResponseTemplate::new(204)).await;
    for method in ["GET", "POST", "PATCH", "DELETE"] {
        request(
            &endpoint(&server.uri(), Some("t")),
            method,
            &path(),
            Some("{}".to_owned()),
        )
        .await;
    }
    let sent = server.received_requests().await.unwrap_or_default();
    assert_eq!(sent.len(), 4);
    for request in sent {
        assert_eq!(
            request.headers.get("accept").and_then(|v| v.to_str().ok()),
            Some("application/json"),
            "{}",
            request.method
        );
    }
}
