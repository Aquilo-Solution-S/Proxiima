use std::net::SocketAddr;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use proxiima_core::PlaintextHosts;
use proxiima_core::embedding::{EmbeddingClient, LlmError};
use proxiima_core::models::EmbedCaps;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};

use super::{OpenAiCompatConfig, OpenAiCompatEmbeddingClient};

enum Target {
    RemoteHttp,
    Loop,
}

struct RedirectServer {
    addr: SocketAddr,
    requests: Arc<AtomicUsize>,
    target_requests: Arc<AtomicUsize>,
    task: tokio::task::JoinHandle<()>,
}

impl Drop for RedirectServer {
    fn drop(&mut self) {
        self.task.abort();
    }
}

async fn read_request(stream: &mut TcpStream) -> String {
    let mut request = Vec::new();
    loop {
        let mut buffer = [0; 4096];
        let count = stream.read(&mut buffer).await.expect("read request");
        assert!(count > 0, "connection ended before a complete request");
        request.extend_from_slice(&buffer[..count]);
        let Some(end) = request.windows(4).position(|bytes| bytes == b"\r\n\r\n") else {
            continue;
        };
        let headers = std::str::from_utf8(&request[..end]).expect("UTF-8 headers");
        let content_length: usize = headers
            .lines()
            .filter_map(|line| line.split_once(':'))
            .find(|(name, _)| name.eq_ignore_ascii_case("content-length"))
            .map_or(0, |(_, value)| {
                value.trim().parse().expect("content length")
            });
        if request.len() >= end + 4 + content_length {
            return String::from_utf8(request).expect("UTF-8 test request");
        }
    }
}

const PATIENCE: Duration = Duration::from_secs(30);

struct ResponseTask(tokio::task::JoinHandle<()>);

impl ResponseTask {
    async fn complete(mut self) {
        tokio::time::timeout(PATIENCE, &mut self.0)
            .await
            .expect("server completes before deadline")
            .expect("server task completes");
    }
}

impl Drop for ResponseTask {
    fn drop(&mut self) {
        self.0.abort();
    }
}

async fn spawn_response_server(response: String) -> (SocketAddr, ResponseTask) {
    let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind server");
    let addr = listener.local_addr().expect("server address");
    let task = tokio::spawn(async move {
        tokio::time::timeout(PATIENCE, async {
            let (mut stream, _) = listener.accept().await.expect("accept request");
            let _ = read_request(&mut stream).await;
            let _ = stream.write_all(response.as_bytes()).await;
        })
        .await
        .expect("response finishes before deadline");
    });
    (addr, ResponseTask(task))
}

async fn spawn_body_server(status: u16, body: String) -> (SocketAddr, ResponseTask) {
    spawn_response_server(format!(
        "HTTP/1.1 {status} Response\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    ))
    .await
}

struct HeldResponse {
    addr: SocketAddr,
    headers_sent: tokio::sync::oneshot::Receiver<()>,
    release_body: tokio::sync::oneshot::Sender<()>,
    task: ResponseTask,
}

async fn spawn_held_response(status: u16) -> HeldResponse {
    let listener = TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind held server");
    let addr = listener.local_addr().expect("server address");
    let (headers_tx, headers_sent) = tokio::sync::oneshot::channel();
    let (release_body, body_released) = tokio::sync::oneshot::channel();
    let task = tokio::spawn(async move {
        tokio::time::timeout(PATIENCE * 2, async {
            let (mut stream, _) = listener.accept().await.expect("accept request");
            let _ = read_request(&mut stream).await;
            let headers = format!(
                "HTTP/1.1 {status} Response\r\nRetry-After: 120\r\nContent-Length: 4\r\nConnection: close\r\n\r\n"
            );
            stream.write_all(headers.as_bytes()).await.expect("write headers");
            stream.flush().await.expect("flush headers");
            headers_tx.send(()).expect("announce headers");
            if body_released.await.is_ok() {
                let _ = stream.write_all(b"body").await;
            }
        }).await.expect("held response deadline");
    });
    HeldResponse {
        addr,
        headers_sent,
        release_body,
        task: ResponseTask(task),
    }
}

async fn spawn_redirect_server(status: u16, target: Target) -> RedirectServer {
    let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind server");
    let addr = listener.local_addr().expect("server address");
    let requests = Arc::new(AtomicUsize::new(0));
    let target_requests = Arc::new(AtomicUsize::new(0));
    let all_count = requests.clone();
    let target_count = target_requests.clone();
    let task = tokio::spawn(async move {
        loop {
            let (mut stream, _) = listener.accept().await.expect("accept request");
            let request = read_request(&mut stream).await;
            all_count.fetch_add(1, Ordering::SeqCst);
            let response = if request.starts_with("POST /target ") {
                target_count.fetch_add(1, Ordering::SeqCst);
                let (_, body) = request.split_once("\r\n\r\n").expect("request body");
                let body: serde_json::Value = serde_json::from_str(body).expect("JSON input");
                assert_eq!(body["input"], serde_json::json!(["private memory text"]));
                let body = r#"{"data":[{"index":0,"embedding":[1.0,0.0]}]}"#;
                format!(
                    "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                    body.len()
                )
            } else {
                let destination = if request.starts_with("POST /v1/embeddings ") {
                    "/intermediate".to_string()
                } else {
                    match target {
                        Target::RemoteHttp => {
                            format!("http://redirect.example:{}/target", addr.port())
                        }
                        Target::Loop => "/intermediate".to_string(),
                    }
                };
                format!(
                    "HTTP/1.1 {status} Redirect\r\nLocation: {destination}\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
                )
            };
            stream
                .write_all(response.as_bytes())
                .await
                .expect("respond");
        }
    });
    RedirectServer {
        addr,
        requests,
        target_requests,
        task,
    }
}

fn client_for(server: &RedirectServer) -> OpenAiCompatEmbeddingClient {
    client_for_hosts(server, &PlaintextHosts::none())
}

fn client_for_hosts(
    server: &RedirectServer,
    hosts: &PlaintextHosts,
) -> OpenAiCompatEmbeddingClient {
    let mut client = OpenAiCompatEmbeddingClient::new(
        "test-embed",
        EmbedCaps::new(2, false),
        OpenAiCompatConfig::new(format!("http://{}/v1", server.addr), None)
            .with_plaintext_hosts(hosts.clone()),
    )
    .expect("loopback embedding endpoint");
    client.client = crate::build_http_client(
        reqwest::Client::builder()
            .timeout(Duration::from_secs(5))
            .no_proxy()
            .resolve("redirect.example", server.addr),
    )
    .expect("build test client with production redirect policy");
    client
}

#[tokio::test]
async fn only_explicit_query_operations_send_the_search_purpose_header() {
    use proxiima_core::{EMBEDDING_PURPOSE_HEADER, EMBEDDING_SEARCH_PURPOSE};
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let (captured, mut requests) = tokio::sync::mpsc::unbounded_channel();
    let server = ResponseTask(tokio::spawn(async move {
        for _ in 0..3 {
            let (mut stream, _) = listener.accept().await.unwrap();
            captured.send(read_request(&mut stream).await).unwrap();
            let body = r#"{"data":[{"index":0,"embedding":[1.0,0.0]}]}"#;
            stream
                .write_all(
                    format!(
                        "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                        body.len()
                    )
                    .as_bytes(),
                )
                .await
                .unwrap();
        }
    }));
    let client = OpenAiCompatEmbeddingClient::new(
        "test-embed",
        EmbedCaps::new(2, false),
        OpenAiCompatConfig::new(format!("http://{addr}/v1"), None),
    )
    .unwrap();
    tokio::time::timeout(PATIENCE, async {
        client.embed("text").await.unwrap();
        client.embed_many(&["text".to_owned()]).await.unwrap();
        client.embed_query("text").await.unwrap();
    })
    .await
    .unwrap();
    server.complete().await;
    for query in [false, false, true] {
        let request = requests.try_recv().unwrap();
        let (headers, body) = request.split_once("\r\n\r\n").unwrap();
        let purpose = headers
            .lines()
            .filter_map(|line| line.split_once(':'))
            .find(|(name, _)| name.eq_ignore_ascii_case(EMBEDDING_PURPOSE_HEADER))
            .map(|(_, value)| value.trim());
        assert_eq!(purpose, query.then_some(EMBEDDING_SEARCH_PURPOSE));
        let body: serde_json::Value = serde_json::from_str(body).unwrap();
        assert_eq!(body["input"], serde_json::json!(["text"]));
    }
}

#[tokio::test]
async fn loopback_query_latency_while_a_writing_response_is_held() {
    use proxiima_core::{EMBEDDING_PURPOSE_HEADER, EMBEDDING_SEARCH_PURPOSE};
    const SAMPLES: usize = 32;
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let (writer_entered, entered) = tokio::sync::oneshot::channel();
    let (release, writer_release) = tokio::sync::oneshot::channel();
    let server =
        ResponseTask(tokio::spawn(async move {
            let body = r#"{"data":[{"index":0,"embedding":[1.0,0.0]}]}"#;
            let response = format!(
                "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                body.len()
            );
            let (mut writer, _) = listener.accept().await.unwrap();
            let _ = read_request(&mut writer).await;
            writer_entered.send(()).unwrap();
            for _ in 0..SAMPLES {
                let (mut query, _) = listener.accept().await.unwrap();
                let request = read_request(&mut query).await;
                assert!(request.lines().any(|line| line
                    == format!("{EMBEDDING_PURPOSE_HEADER}: {EMBEDDING_SEARCH_PURPOSE}")));
                query.write_all(response.as_bytes()).await.unwrap();
            }
            writer_release.await.unwrap();
            writer.write_all(response.as_bytes()).await.unwrap();
        }));
    let client = OpenAiCompatEmbeddingClient::new(
        "test-embed",
        EmbedCaps::new(2, false),
        OpenAiCompatConfig::new(format!("http://{addr}/v1"), None),
    )
    .unwrap();
    let mut writer = Box::pin(client.embed("held writing request"));
    tokio::time::timeout(PATIENCE, async {
        tokio::select! {
            result = &mut writer => panic!("writer answered before release: {result:?}"),
            result = entered => result.unwrap(),
        }
    })
    .await
    .unwrap();
    let mut latencies = Vec::with_capacity(SAMPLES);
    for _ in 0..SAMPLES {
        let started = std::time::Instant::now();
        tokio::time::timeout(PATIENCE, client.embed_query("query"))
            .await
            .unwrap()
            .unwrap();
        latencies.push(started.elapsed().as_micros());
    }
    release.send(()).unwrap();
    tokio::time::timeout(PATIENCE, writer)
        .await
        .unwrap()
        .unwrap();
    server.complete().await;
    latencies.sort_unstable();
    eprintln!(
        "loopback HTTP queries while writing held: n={SAMPLES} median_us={} p95_us={} max_us={}",
        latencies[SAMPLES / 2],
        latencies[(SAMPLES * 95).div_ceil(100) - 1],
        latencies[SAMPLES - 1]
    );
}

#[tokio::test]
async fn embedding_base_query_preserves_request_path_and_parameters() {
    for (base_path, expected_target) in [
        (
            "/v1?api-version=2026-09&tenant=a%2Fb&route=/",
            "/v1/embeddings?api-version=2026-09&tenant=a%2Fb&route=/",
        ),
        ("/v1/?mode=x&mode=y", "/v1/embeddings?mode=x&mode=y"),
        ("/v1%2Ftenant?mode=x", "/v1%2Ftenant/embeddings?mode=x"),
    ] {
        let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind server");
        let addr = listener.local_addr().expect("server address");
        let request_task = tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.expect("accept request");
            let request = read_request(&mut stream).await;
            let body = r#"{"data":[{"index":0,"embedding":[1.0,0.0]}]}"#;
            let response = format!(
                "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                body.len()
            );
            stream
                .write_all(response.as_bytes())
                .await
                .expect("respond");
            request
        });
        let client = OpenAiCompatEmbeddingClient::new(
            "test-embed",
            EmbedCaps::new(2, false),
            OpenAiCompatConfig::new(format!("http://{addr}{base_path}"), None)
                .with_timeout(Duration::from_secs(5)),
        )
        .expect("query-bearing base is valid");
        assert_eq!(
            client.embed("test text").await.expect("embed"),
            vec![1.0, 0.0]
        );
        let request = request_task.await.expect("request captured");
        assert_eq!(
            request.lines().next().expect("request line"),
            format!("POST {expected_target} HTTP/1.1"),
        );
    }
}

#[tokio::test]
async fn successful_http_response_with_overflow_is_retryable() {
    let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind server");
    let addr = listener.local_addr().expect("server address");
    let request_task = tokio::spawn(async move {
        tokio::time::timeout(Duration::from_secs(5), async move {
            let (mut stream, _) = listener.accept().await.expect("accept request");
            let request = read_request(&mut stream).await;
            let body = r#"{"data":[{"index":0,"embedding":[0.0,1e39]}]}"#;
            let response = format!(
                "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                body.len()
            );
            stream
                .write_all(response.as_bytes())
                .await
                .expect("respond");
            request
        })
        .await
        .expect("local response completes")
    });
    let mut client = OpenAiCompatEmbeddingClient::new(
        "test-embed",
        EmbedCaps::new(2, false),
        OpenAiCompatConfig::new(format!("http://{addr}/v1"), None),
    )
    .expect("loopback embedding endpoint");
    client.client = crate::build_http_client(
        reqwest::Client::builder()
            .timeout(Duration::from_secs(5))
            .no_proxy(),
    )
    .expect("test client with production response handling");
    let result = client.embed("test text").await;
    let request = request_task.await.expect("request captured");
    assert!(request.starts_with("POST /v1/embeddings HTTP/1.1\r\n"));
    assert!(
        matches!(result, Err(LlmError::Retryable { ref message, .. })
            if message == "embedding 0 has non-finite component at position 1"),
        "HTTP success must not accept an invalid vector: {result:?}",
    );
}

#[tokio::test]
async fn successful_response_larger_than_derived_limit_is_refused() {
    let body = format!(
        "{{\"data\":[{{\"embedding\":[1.0,0.0]}}]}}{}",
        " ".repeat(70_000)
    );
    let (addr, task) = spawn_body_server(200, body).await;
    let client = OpenAiCompatEmbeddingClient::new(
        "test-embed",
        EmbedCaps::new(2, false),
        OpenAiCompatConfig::new(format!("http://{addr}/v1"), None),
    )
    .expect("loopback embedding endpoint");
    let result = client.embed("test text").await;
    task.complete().await;
    assert!(
        matches!(result, Err(LlmError::Retryable { ref message, .. }) if message.contains("exceeded the 65600-byte limit")),
        "{result:?}"
    );
}

#[tokio::test]
async fn malformed_success_body_is_limited_to_a_utf8_safe_prefix() {
    for (body, expected) in [
        (
            format!("{}€{}", "x".repeat(510), "y".repeat(1024 * 1024 - 513)),
            "x".repeat(510),
        ),
        (
            format!(r#"{{"data":"{}"}}"#, "x".repeat(1024 * 1024 - 11)),
            format!("{{\"data\":\"{}", "x".repeat(503)),
        ),
    ] {
        let (addr, task) = spawn_body_server(200, body).await;
        let client = OpenAiCompatEmbeddingClient::new(
            "test-embed",
            EmbedCaps::new(32_768, false),
            OpenAiCompatConfig::new(format!("http://{addr}/v1"), None),
        )
        .expect("loopback embedding endpoint");
        let result = client.embed("test text").await;
        task.complete().await;
        let Err(LlmError::Retryable { message, .. }) = result else {
            panic!("expected bounded decode error: {result:?}");
        };
        let (_, detail) = message.split_once("; body: ").expect("JSON decode error");
        assert!(detail.len() <= super::ERROR_BODY_PREFIX_BYTES);
        assert_eq!(detail, expected);
        assert!(message.len() < 700);
    }
}

#[tokio::test]
async fn error_body_reader_is_limited_to_a_utf8_safe_prefix() {
    let body = format!("{}€{}", "a".repeat(510), "b".repeat(1024 * 1024 - 513));
    let (addr, task) = spawn_body_server(500, body).await;
    let client = OpenAiCompatEmbeddingClient::new(
        "test-embed",
        EmbedCaps::new(2, false),
        OpenAiCompatConfig::new(format!("http://{addr}/v1"), None),
    )
    .expect("loopback embedding endpoint");
    let mut response = client
        .client
        .post(client.endpoint.clone())
        .send()
        .await
        .expect("HTTP response");
    let prefix = tokio::time::timeout(PATIENCE, super::read_error_prefix(&mut response))
        .await
        .expect("bounded reader finishes")
        .expect("complete body");
    assert_eq!(prefix.len(), super::ERROR_BODY_PREFIX_BYTES);
    let status = response.status();
    drop(response);
    task.complete().await;
    let error = super::embed_http_error(status, None, super::utf8_prefix(&prefix));
    let LlmError::Retryable { message, .. } = error else {
        panic!("expected bounded HTTP error: {error:?}");
    };
    let detail = message
        .strip_prefix("openai-compatible /embeddings returned 500 Internal Server Error: ")
        .expect("HTTP error prefix");
    assert!(detail.len() <= super::ERROR_BODY_PREFIX_BYTES);
    assert_eq!(detail, "a".repeat(510));
}

#[tokio::test]
async fn http_failures_keep_status_and_retry_after_when_diagnostics_fail() {
    for status in [400, 401, 403, 404, 413, 422, 429, 451, 503] {
        for content_length in [4, 100] {
            let response = format!(
                "HTTP/1.1 {status} Response\r\nRetry-After: 120\r\nContent-Length: {content_length}\r\nConnection: close\r\n\r\nbody"
            );
            let (addr, task) = spawn_response_server(response).await;
            let client = OpenAiCompatEmbeddingClient::new(
                "test-embed",
                EmbedCaps::new(2, false),
                OpenAiCompatConfig::new(format!("http://{addr}/v1"), None),
            )
            .expect("loopback embedding endpoint");
            let error = client.embed("test text").await.expect_err("HTTP refusal");
            task.complete().await;
            super::tests::assert_http_failure(&error, status, Some(Duration::from_mins(2)));
            if let LlmError::Retryable { message, .. } = error {
                assert!(message.len() < 700, "diagnostic remains bounded: {message}");
            }
        }
    }
}

#[tokio::test]
async fn error_body_reader_distinguishes_complete_and_truncated_bodies() {
    for content_length in [4, 100] {
        let response = format!(
            "HTTP/1.1 503 Response\r\nContent-Length: {content_length}\r\nConnection: close\r\n\r\nbody"
        );
        let (addr, task) = spawn_response_server(response).await;
        let client = OpenAiCompatEmbeddingClient::new(
            "test-embed",
            EmbedCaps::new(2, false),
            OpenAiCompatConfig::new(format!("http://{addr}/v1"), None),
        )
        .expect("loopback embedding endpoint");
        let mut response = client
            .client
            .post(client.endpoint.clone())
            .send()
            .await
            .expect("HTTP headers");
        let prefix = tokio::time::timeout(PATIENCE, super::read_error_prefix(&mut response))
            .await
            .expect("reader finishes within deadline");
        task.complete().await;
        if content_length == 4 {
            assert_eq!(prefix.expect("complete body"), b"body");
        } else {
            assert!(
                prefix.is_err(),
                "truncated diagnostic must surface a body-read error"
            );
        }
    }
}

#[tokio::test]
async fn stalled_error_bodies_cannot_overwrite_captured_status_or_retry_after() {
    for status in [401, 413, 429] {
        let HeldResponse {
            addr,
            headers_sent,
            release_body,
            task,
        } = spawn_held_response(status).await;
        let client = OpenAiCompatEmbeddingClient::new(
            "test-embed",
            EmbedCaps::new(2, false),
            OpenAiCompatConfig::new(format!("http://{addr}/v1"), None).with_timeout(PATIENCE * 2),
        )
        .expect("loopback embedding endpoint");
        let (result, marker) = tokio::join!(
            proxiima_core::embedding::embed_with_timeout(&client, "test text", PATIENCE),
            tokio::time::timeout(PATIENCE, headers_sent),
        );
        marker
            .expect("headers sent before deadline")
            .expect("positive headers marker");
        let error = result.expect_err("HTTP refusal before diagnostic body is released");
        super::tests::assert_http_failure(&error, status, Some(Duration::from_mins(2)));
        release_body
            .send(())
            .expect("body was held until classification finished");
        task.complete().await;
    }
}

#[tokio::test]
async fn redirect_failure_keeps_headers_without_consuming_the_body() {
    let response = "HTTP/1.1 307 Temporary Redirect\r\nRetry-After: 90\r\nLocation: http://redirect.example/target\r\nContent-Length: 100\r\nConnection: close\r\n\r\nx".to_owned();
    let (addr, task) = spawn_response_server(response).await;
    let client = OpenAiCompatEmbeddingClient::new(
        "test-embed",
        EmbedCaps::new(2, false),
        OpenAiCompatConfig::new(format!("http://{addr}/v1"), None),
    )
    .expect("loopback embedding endpoint");
    let error = client
        .embed("test text")
        .await
        .expect_err("redirect refusal");
    task.complete().await;
    super::tests::assert_http_failure(&error, 307, Some(Duration::from_secs(90)));
    assert!(error.to_string().contains("redirect refused"), "{error}");
    assert!(!error.to_string().contains("HTTP body read:"), "{error}");
}

#[tokio::test]
async fn every_redirect_status_is_refused_without_reading_its_body() {
    for status in [300, 301, 302, 303, 304, 305, 306, 307, 308, 399] {
        let (addr, task) = spawn_body_server(status, "x".repeat(1_048_576)).await;
        let client = OpenAiCompatEmbeddingClient::new(
            "test-embed",
            EmbedCaps::new(2, false),
            OpenAiCompatConfig::new(format!("http://{addr}/v1"), None),
        )
        .expect("loopback embedding endpoint");
        let result = client.embed("test text").await;
        task.complete().await;
        assert!(
            matches!(result, Err(LlmError::Retryable { ref message, .. }) if message.contains("redirect refused")),
            "status {status}: {result:?}"
        );
    }
}

#[tokio::test]
async fn nonloopback_plaintext_redirect_cannot_receive_embedding_input() {
    for status in [307, 308] {
        let server = spawn_redirect_server(status, Target::RemoteHttp).await;
        let result = client_for(&server).embed("private memory text").await;
        assert_eq!(server.target_requests.load(Ordering::SeqCst), 0);
        assert_eq!(server.requests.load(Ordering::SeqCst), 1);
        assert!(
            matches!(result, Err(LlmError::Retryable { ref message, .. }) if message.contains("redirect refused")),
            "{result:?}"
        );
    }
}

#[tokio::test]
async fn redirects_are_refused_even_for_named_plaintext_hosts() {
    for status in [307, 308] {
        let server = spawn_redirect_server(status, Target::RemoteHttp).await;
        let named = PlaintextHosts::parse("redirect.example").unwrap();
        let result = client_for_hosts(&server, &named)
            .embed("private memory text")
            .await;
        assert_eq!(server.target_requests.load(Ordering::SeqCst), 0);
        assert_eq!(server.requests.load(Ordering::SeqCst), 1);
        assert!(
            matches!(result, Err(LlmError::Retryable { ref message, .. }) if message.contains("redirect refused")),
            "{result:?}"
        );

        let server = spawn_redirect_server(status, Target::RemoteHttp).await;
        let other = PlaintextHosts::parse("gateway").unwrap();
        let result = client_for_hosts(&server, &other)
            .embed("private memory text")
            .await;
        assert_eq!(server.target_requests.load(Ordering::SeqCst), 0);
        assert_eq!(server.requests.load(Ordering::SeqCst), 1);
        assert!(
            matches!(result, Err(LlmError::Retryable { ref message, .. }) if message.contains("redirect refused")),
            "{result:?}"
        );
    }
}

#[tokio::test]
async fn loopback_redirects_are_refused() {
    for status in [307, 308] {
        let target = TcpListener::bind("127.0.0.1:0")
            .await
            .expect("loopback target");
        let target_addr = target.local_addr().expect("target address");
        let source = TcpListener::bind("127.0.0.1:0")
            .await
            .expect("provider stub");
        let source_addr = source.local_addr().expect("source address");
        let task = tokio::spawn(async move {
            let (mut stream, _) = source.accept().await.expect("provider request");
            let _ = read_request(&mut stream).await;
            let response = format!(
                "HTTP/1.1 {status} Redirect\r\nLocation: http://{target_addr}/target\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
            );
            stream
                .write_all(response.as_bytes())
                .await
                .expect("redirect");
        });
        let mut client = OpenAiCompatEmbeddingClient::new(
            "test-embed",
            EmbedCaps::new(2, false),
            OpenAiCompatConfig::new(format!("http://{source_addr}/v1"), None),
        )
        .expect("provider stub endpoint");
        client.client = crate::build_http_client(
            reqwest::Client::builder()
                .timeout(Duration::from_millis(500))
                .no_proxy(),
        )
        .expect("production redirect policy");
        let result = client.embed("private memory text").await;
        task.await.expect("provider responded");
        assert!(
            tokio::time::timeout(Duration::from_millis(20), target.accept())
                .await
                .is_err(),
            "redirect target must receive no connection"
        );
        assert!(
            matches!(result, Err(LlmError::Retryable { ref message, .. }) if message.contains("redirect refused")),
            "{result:?}"
        );
    }
}

#[tokio::test]
async fn redirect_loops_are_refused_without_hops() {
    let server = spawn_redirect_server(307, Target::Loop).await;
    let result = client_for(&server).embed("private memory text").await;
    assert!(
        matches!(result, Err(LlmError::Retryable { ref message, .. }) if message.contains("redirect refused")),
        "{result:?}"
    );
    assert_eq!(server.requests.load(Ordering::SeqCst), 1);
    assert_eq!(server.target_requests.load(Ordering::SeqCst), 0);
}
