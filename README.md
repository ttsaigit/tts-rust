# tts-rust

Official **Rust SDK** for the [TTS.ai](https://tts.ai) REST API. Async, mirrors the existing Python (`ttsai`), JS (`@ttsainpm/ttsai`), PHP (`ttsai/ttsai`), Go (`github.com/ttsaigit/tts-go`), and Ruby (`ttsai`) SDKs so cross-language porting is mechanical.

```rust
use ttsai::{Client, GenerateOptions};

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let tts = Client::new(std::env::var("TTSAI_API_KEY")?);
    let audio = tts.generate(GenerateOptions {
        text: "Hello world.".into(),
        voice: Some("af_bella".into()),
        model: Some("kokoro".into()),
        ..Default::default()
    }).await?;
    tokio::fs::write("out.mp3", audio).await?;
    Ok(())
}
```

## Install

```toml
[dependencies]
ttsai = "0.1"
tokio = { version = "1", features = ["full"] }
```

Requires Rust 1.86+. Uses `reqwest` with `rustls-tls` (no system OpenSSL needed).

## Methods

```rust
// Synchronous-feel async — submits + polls + downloads, returns audio bytes
let audio: Vec<u8> = tts.generate(GenerateOptions {
    text: "...".into(),
    voice: Some("af_bella".into()),
    model: Some("kokoro".into()),
    format: Some("mp3".into()),       // mp3 | wav | flac | ogg
    language: Some("en".into()),       // ISO; auto-detect if None
    speed: Some(1.0),                  // 0.5..2.0
    instructions: Some("say it sarcastically".into()), // qwen3-tts only
    pronunciations: Some(HashMap::from([("GIF".into(), "jiff".into())])),
    extra: serde_json::Map::new(),     // any backend-specific knobs
}).await?;

// Async — get back a queued Job; poll_result() yourself
let job = tts.generate_async(opts).await?;
let result = tts.poll_result(&job.uuid).await?;

// Transcription
let job = tts.transcribe("audio.mp3", Some("faster-whisper")).await?;
let result = tts.poll_result(&job.uuid).await?;

// Voice cloning — server smart-trims long reference audio automatically
let job = tts.clone_voice(CloneVoiceOptions {
    audio_path: "reference.wav".into(),
    text: "Read this in their voice.".into(),
    model: Some("chatterbox".into()),
    boost: Some(0.7),                  // similarity slider (chatterbox only)
    ..Default::default()
}).await?;

// Catalog
let voices = tts.list_voices(Some("kokoro"), Some("en")).await?;
let models = tts.list_models().await?;

// Subtitles for a completed job
let srt = tts.subtitles(&job.uuid, "srt").await?;  // or "vtt"
```

## Errors

```rust
use ttsai::Error;

match tts.generate(opts).await {
    Ok(audio) => write_audio(audio),
    Err(e) if e.is_insufficient_credits() => {
        eprintln!("Need {:?} chars; have {:?}", e.credits_needed(), e.credits_remaining());
    }
    Err(e) if e.is_rate_limit() => /* back off and retry */,
    Err(e) if e.is_authentication() => /* bad API key */,
    Err(e) if e.is_validation() => /* 400; inspect Error::Api { body, .. } for upgrade hints */,
    Err(e) if e.is_server() => /* 5xx after retries */,
    Err(e) => eprintln!("other error: {e}"),
}
```

`Error::Api` carries the parsed JSON `body` so callers can read structured fields (`max_length`, `upgrade.cta_url`, etc.).

## Configuration

```rust
use std::time::Duration;

let tts = Client::builder("sk-tts-...")
    .base_url("https://api.tts.ai")              // override for self-hosted
    .request_timeout(Duration::from_secs(300))
    .max_retries(3)                              // retries on 5xx + connection errors
    .poll_interval(Duration::from_secs(2))
    .poll_deadline(Duration::from_secs(900))
    .user_agent("my-app/1.0")
    .build();
```

`Client` is cheap to clone — internally `Arc<reqwest::Client>`.

## Sister SDKs

- Python: [`pip install ttsai`](https://pypi.org/project/ttsai/)
- JavaScript / Node: [`npm install @ttsainpm/ttsai`](https://www.npmjs.com/package/@ttsainpm/ttsai)
- PHP: [`composer require ttsai/ttsai`](https://github.com/ttsaigit/tts-php)
- Go: [`go get github.com/ttsaigit/tts-go`](https://github.com/ttsaigit/tts-go)
- Ruby: [`gem install ttsai`](https://github.com/ttsaigit/tts-ruby)
- Browser embeds: [`narrator.js`](https://github.com/ttsaigit/narrator-js), [`tts-widget`](https://github.com/ttsaigit/tts-widget)

All six SDKs expose the same methods + return shapes so porting between languages is mechanical.

## License

Apache-2.0. See `LICENSE`.
