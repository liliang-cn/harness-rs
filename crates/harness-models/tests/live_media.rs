//! Live end-to-end checks for the image and speech adapters.
//!
//! `#[ignore]` because they cost money and depend on a third party being up.
//! Run by hand:
//!
//! ```bash
//! HARNESS_IMAGE_BASE_URL=https://cpa.superleo.app/v1 \
//! HARNESS_IMAGE_MODEL=gemini-3.1-flash-image \
//! HARNESS_IMAGE_KEY=sk-… \
//!   cargo test -p harness-rs-models --test live_media -- --ignored --nocapture
//! ```
//!
//! Each test skips (rather than fails) when its keys are absent, so a partial
//! configuration still exercises what it can.

use harness_core::{ImageModel, ImageRef, ImageRequest, SpeechModel, SpeechRequest};
use harness_models::{ChatImageModel, DashScopeSpeech};

fn env3(base: &str, model: &str, key: &str) -> Option<(String, String, String)> {
    Some((
        std::env::var(base).ok()?,
        std::env::var(model).ok()?,
        std::env::var(key).ok()?,
    ))
}

#[tokio::test]
#[ignore = "network + costs money: live image generation"]
async fn live_chat_image_generates_a_real_image() {
    let Some((base, model, key)) = env3(
        "HARNESS_IMAGE_BASE_URL",
        "HARNESS_IMAGE_MODEL",
        "HARNESS_IMAGE_KEY",
    ) else {
        eprintln!("skipped: set HARNESS_IMAGE_{{BASE_URL,MODEL,KEY}}");
        return;
    };

    let m = ChatImageModel::with_key(base, model, key);
    let imgs = m
        .generate(&ImageRequest::new(
            "A friendly cartoon brown bear cub riding a red bicycle down a grassy \
             hill, soft watercolor children's picture book illustration",
        ))
        .await
        .expect("image generation");

    assert_eq!(imgs.len(), 1);
    let img = &imgs[0];
    eprintln!(
        "got {} bytes of {} from {}",
        img.bytes.len(),
        img.media_type,
        m.handle()
    );
    assert!(img.media_type.starts_with("image/"));
    assert!(img.bytes.len() > 10_000, "a real illustration is not tiny");
    // Magic bytes: the contract is decoded bytes, not a base64 string that
    // merely looks right.
    let magic = &img.bytes[..4];
    assert!(
        magic == [0xff, 0xd8, 0xff, 0xe0]      // JPEG/JFIF
            || magic == [0xff, 0xd8, 0xff, 0xe1] // JPEG/Exif
            || magic == [0x89, 0x50, 0x4e, 0x47], // PNG
        "unexpected magic bytes {magic:02x?}"
    );
}

#[tokio::test]
#[ignore = "network + costs money: live image generation with a reference"]
async fn live_chat_image_accepts_a_reference_image() {
    let Some((base, model, key)) = env3(
        "HARNESS_IMAGE_BASE_URL",
        "HARNESS_IMAGE_MODEL",
        "HARNESS_IMAGE_KEY",
    ) else {
        eprintln!("skipped: set HARNESS_IMAGE_{{BASE_URL,MODEL,KEY}}");
        return;
    };

    let m = ChatImageModel::with_key(base, model, key);
    let first = m
        .generate(&ImageRequest::new(
            "A cartoon brown bear cub in yellow overalls, watercolor children's book style",
        ))
        .await
        .expect("cover image");

    // The character-consistency path: page one comes back as a reference for
    // page two. This is the mechanism the picture-book pipeline depends on.
    let second = m
        .generate(
            &ImageRequest::new("The same bear cub waving goodbye at sunset").with_references(vec![
                ImageRef::bytes(&first[0].media_type, first[0].bytes.clone()),
            ]),
        )
        .await
        .expect("referenced image");

    assert_eq!(second.len(), 1);
    assert!(second[0].bytes.len() > 10_000);
    assert_ne!(
        first[0].bytes, second[0].bytes,
        "a reference must steer generation, not echo the reference back"
    );
}

#[tokio::test]
#[ignore = "network + costs money: live TTS"]
async fn live_dashscope_speech_returns_playable_wav() {
    let Some((base, model, key)) = env3(
        "HARNESS_SPEECH_BASE_URL",
        "HARNESS_SPEECH_MODEL",
        "HARNESS_SPEECH_KEY",
    ) else {
        eprintln!("skipped: set HARNESS_SPEECH_{{BASE_URL,MODEL,KEY}}");
        return;
    };

    let m = DashScopeSpeech::with_key(base, model, key);
    let audio = m
        .synthesize(
            &SpeechRequest::new(
                "Little Bear wobbled down the grassy hill, and for the very first \
                 time, he did not fall.",
            )
            .voice("Cherry")
            .language("English"),
        )
        .await
        .expect("speech synthesis");

    eprintln!(
        "got {} bytes of {} from {}",
        audio.bytes.len(),
        audio.media_type,
        m.handle()
    );
    assert_eq!(audio.media_type, "audio/wav");
    assert!(audio.bytes.len() > 10_000);
    // RIFF/WAVE header — proves the adapter fetched and returned real bytes
    // rather than the expiring URL the provider actually answered with.
    assert_eq!(&audio.bytes[..4], b"RIFF");
    assert_eq!(&audio.bytes[8..12], b"WAVE");
}

/// Live check of the OpenAI-compatible embeddings route.
///
/// ```bash
/// HARNESS_EMBED_URL=https://… HARNESS_EMBED_KEY=… \
///   cargo test -p harness-rs-models --test live_media embeddings -- --ignored --nocapture
/// ```
///
/// The unit tests only prove the adapter refuses bad input. What they cannot
/// prove is that the vectors mean anything — so this asserts the property the
/// whole feature rests on: similar sentences must score closer than unrelated
/// ones. An adapter that returned the right *shape* of garbage would pass
/// every offline test and rank a corpus at random.
#[tokio::test]
#[ignore = "calls a paid third-party service"]
async fn embeddings_place_similar_sentences_near_each_other() {
    use harness_core::{Embedder, l2_normalize};
    use harness_models::OpenAiEmbed;

    let (Ok(url), Ok(key)) = (
        std::env::var("HARNESS_EMBED_URL"),
        std::env::var("HARNESS_EMBED_KEY"),
    ) else {
        eprintln!("skipped: set HARNESS_EMBED_URL and HARNESS_EMBED_KEY");
        return;
    };
    let model = std::env::var("HARNESS_EMBED_MODEL").unwrap_or_else(|_| "embeddinggemma:latest".into());
    let dim: usize = std::env::var("HARNESS_EMBED_DIM")
        .ok()
        .and_then(|d| d.parse().ok())
        .unwrap_or(768);

    let e = OpenAiEmbed::with_key(url, model, key, dim);
    let inputs = [
        "a lost kitten finds her way home in the snow",
        "a little cat is lost in the winter and walks back home",
        "a spreadsheet of quarterly revenue projections",
    ];
    let mut vs = e.embed(&inputs).await.expect("embedding failed");
    assert_eq!(vs.len(), 3);
    for v in &mut vs {
        assert_eq!(v.len(), dim);
        l2_normalize(v);
    }

    let dot = |a: &[f32], b: &[f32]| a.iter().zip(b).map(|(x, y)| x * y).sum::<f32>();
    let near = dot(&vs[0], &vs[1]);
    let far = dot(&vs[0], &vs[2]);
    eprintln!("similar={near:.3} unrelated={far:.3}");
    assert!(
        near > far,
        "the two kitten sentences must be closer than the spreadsheet: {near} vs {far}"
    );
}
