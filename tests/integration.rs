use std::time::Duration;
use ttsai::{Client, GenerateOptions};
use wiremock::matchers::{method, path, query_param};
use wiremock::{Mock, MockServer, ResponseTemplate};

#[tokio::test]
async fn generate_async_posts_expected_body_and_returns_uuid() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/tts/"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "uuid": "abc",
            "status": "queued",
            "credits_used": 11,
            "credits_remaining": 1000
        })))
        .mount(&server)
        .await;

    let client = Client::builder("sk-tts-test")
        .base_url(server.uri())
        .build();

    let job = client
        .generate_async(GenerateOptions {
            text: "hi".into(),
            ..Default::default()
        })
        .await
        .expect("ok");
    assert_eq!(job.uuid, "abc");
    assert_eq!(job.credits_used, Some(11));
}

#[tokio::test]
async fn poll_result_returns_completed() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/v1/speech/results/"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "uuid": "abc",
            "status": "completed",
            "result_url": "https://cdn/test.wav"
        })))
        .mount(&server)
        .await;

    let client = Client::builder("sk")
        .base_url(server.uri())
        .poll_interval(Duration::from_millis(10))
        .poll_deadline(Duration::from_secs(2))
        .build();

    let res = client.poll_result("abc").await.expect("ok");
    assert_eq!(res.status, "completed");
    assert_eq!(res.result_url.as_deref(), Some("https://cdn/test.wav"));
}

#[tokio::test]
async fn poll_result_raises_on_failed() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/v1/speech/results/"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "uuid": "abc",
            "status": "failed",
            "message": "boom"
        })))
        .mount(&server)
        .await;

    let client = Client::builder("sk")
        .base_url(server.uri())
        .poll_interval(Duration::from_millis(10))
        .poll_deadline(Duration::from_secs(2))
        .build();

    let err = client.poll_result("abc").await.unwrap_err();
    assert!(matches!(err, ttsai::Error::JobFailed(_)));
    assert!(err.to_string().contains("boom"));
}

#[tokio::test]
async fn http_402_maps_to_insufficient_credits() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/tts/"))
        .respond_with(ResponseTemplate::new(402).set_body_json(serde_json::json!({
            "error": "insufficient_credits",
            "message": "need 5000",
            "credits_needed": 5000,
            "credits_remaining": 100
        })))
        .mount(&server)
        .await;

    let client = Client::builder("sk")
        .base_url(server.uri())
        .max_retries(0)
        .build();
    let err = client
        .generate_async(GenerateOptions {
            text: "hi".into(),
            ..Default::default()
        })
        .await
        .unwrap_err();
    assert!(err.is_insufficient_credits());
    assert_eq!(err.credits_needed(), Some(5000));
    assert_eq!(err.credits_remaining(), Some(100));
}

#[tokio::test]
async fn http_401_maps_to_authentication() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/tts/"))
        .respond_with(ResponseTemplate::new(401).set_body_json(serde_json::json!({
            "error": "unauthorized",
            "message": "bad key"
        })))
        .mount(&server)
        .await;
    let client = Client::builder("bad")
        .base_url(server.uri())
        .max_retries(0)
        .build();
    let err = client
        .generate_async(GenerateOptions {
            text: "hi".into(),
            ..Default::default()
        })
        .await
        .unwrap_err();
    assert!(err.is_authentication());
}

#[tokio::test]
async fn list_voices_filters_via_query_string() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/v1/voices/"))
        .and(query_param("model", "kokoro"))
        .and(query_param("language", "en"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "voices": [{"id": "af_bella", "language": "en"}],
            "total": 1
        })))
        .mount(&server)
        .await;
    let client = Client::builder("sk").base_url(server.uri()).build();
    let voices = client.list_voices(Some("kokoro"), Some("en")).await.unwrap();
    assert_eq!(voices.len(), 1);
}

#[tokio::test]
async fn subtitles_returns_srt() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/v1/speech/subtitles/"))
        .and(query_param("uuid", "u"))
        .and(query_param("format", "srt"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "format": "srt",
            "content": "1\n00:00:00,000 --> 00:00:01,000\nhi\n",
            "cached": false
        })))
        .mount(&server)
        .await;
    let client = Client::builder("sk").base_url(server.uri()).build();
    let res = client.subtitles("u", "srt").await.unwrap();
    assert_eq!(res.format, "srt");
    assert!(res.content.contains("00:00:00,000 --> 00:00:01,000"));
}

#[tokio::test]
async fn server_error_retries_then_succeeds() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/tts/"))
        .respond_with(ResponseTemplate::new(503))
        .up_to_n_times(1)
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(path("/v1/tts/"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "uuid": "abc",
            "status": "queued"
        })))
        .mount(&server)
        .await;
    let client = Client::builder("sk").base_url(server.uri()).max_retries(2).build();
    let job = client
        .generate_async(GenerateOptions {
            text: "hi".into(),
            ..Default::default()
        })
        .await
        .unwrap();
    assert_eq!(job.uuid, "abc");
}
