//! Integration tests against the fake llama-server harness (Continuous §6).

mod support;

use futures_util::StreamExt;
use nexus::client::{ChatCompletionRequest, ChatMessage, NexusClient};
use support::fake_llama::{spawn_fake_llama, FakeLlamaConfig};

#[tokio::test]
async fn fake_llama_health_and_models() {
    let (base, handle) = spawn_fake_llama(FakeLlamaConfig {
        model_id: "stub-7b".into(),
        ..Default::default()
    })
    .await
    .expect("bind fake llama");

    let client = NexusClient::new(&base);
    assert!(
        client.health().await.expect("health"),
        "health must be true"
    );
    let models = client.models().await.expect("models");
    assert_eq!(models, vec!["stub-7b".to_string()]);

    handle.abort();
}

#[tokio::test]
async fn fake_llama_canned_sse_stream() {
    let (base, handle) = spawn_fake_llama(FakeLlamaConfig {
        model_id: "stub-7b".into(),
        sse_tokens: vec!["Hi".into(), " there".into()],
    })
    .await
    .expect("bind fake llama");

    let client = NexusClient::new(&base);
    let mut stream = client
        .stream_chat(ChatCompletionRequest {
            model: "stub-7b".into(),
            messages: vec![ChatMessage::user("ping")],
            temperature: None,
            top_p: None,
            max_tokens: Some(16),
            stream: true,
        })
        .await
        .expect("stream_chat");

    let mut collected = String::new();
    while let Some(item) = stream.next().await {
        collected.push_str(&item.expect("token ok"));
    }
    assert_eq!(collected, "Hi there");

    handle.abort();
}

#[tokio::test]
async fn fake_llama_sse_error_chunk_surfaced() {
    use bytes::Bytes;
    use http_body_util::Full;
    use hyper::server::conn::http1;
    use hyper::service::service_fn;
    use hyper::{Response, StatusCode};
    use hyper_util::rt::TokioIo;
    use nexus::client::ClientError;
    use std::convert::Infallible;
    use std::net::SocketAddr;

    let listener = tokio::net::TcpListener::bind(SocketAddr::from(([127, 0, 0, 1], 0)))
        .await
        .expect("bind listener");
    let addr = listener.local_addr().expect("local addr");

    let server_handle = tokio::spawn(async move {
        if let Ok((stream, _)) = listener.accept().await {
            let io = TokioIo::new(stream);
            let _ = http1::Builder::new()
                .serve_connection(
                    io,
                    service_fn(|_req| async {
                        let sse = "data: {\"error\":{\"code\":500,\"message\":\"decode() failed: vk::Device::createComputePipeline: ErrorUnknown\",\"type\":\"server_error\"}}\n\n";
                        let resp = Response::builder()
                            .status(StatusCode::OK)
                            .header("content-type", "text/event-stream")
                            .header("cache-control", "no-cache")
                            .body(Full::new(Bytes::from(sse)))
                            .unwrap();
                        Ok::<_, Infallible>(resp)
                    }),
                )
                .await;
        }
    });

    let client = NexusClient::new(format!("http://{}", addr));
    let mut stream = client
        .stream_chat(ChatCompletionRequest {
            model: "stub-7b".into(),
            messages: vec![ChatMessage::user("hi")],
            temperature: None,
            top_p: None,
            max_tokens: Some(4),
            stream: true,
        })
        .await
        .expect("stream_chat");

    let first = stream.next().await;
    assert!(first.is_some(), "expected error item in stream");
    match first.unwrap() {
        Err(ClientError::ApiError { message, .. }) => {
            assert!(
                message.contains("decode() failed"),
                "expected decode error in message: {message}"
            );
        }
        other => panic!("expected ApiError, got {other:?}"),
    }

    server_handle.abort();
}
