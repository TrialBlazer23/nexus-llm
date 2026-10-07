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
