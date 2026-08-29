//! Reusable GIF encoder tests.

use std::{
    io::Cursor,
    sync::atomic::{AtomicU64, Ordering},
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use image::{AnimationDecoder, codecs::gif::GifDecoder};
use scrozz_export::{
    AnimationFormat, AnimationRepeat, GifAnimationEncoder, GifDither, RgbaImage, TimedRgbaFrame,
    inspect_gif_file,
};

static NEXT_PATH: AtomicU64 = AtomicU64::new(0);

fn solid(width: u32, height: u32, color: [u8; 4], delay_ms: u64) -> TimedRgbaFrame {
    TimedRgbaFrame::new(
        RgbaImage {
            width,
            height,
            data: color.repeat((width * height) as usize),
        },
        Duration::from_millis(delay_ms),
    )
}

#[test]
fn gif_has_the_signature_and_preserves_animation_frames() {
    let frames = [
        solid(3, 2, [255, 0, 0, 255], 40),
        solid(3, 2, [0, 0, 255, 255], 90),
    ];
    let bytes = GifAnimationEncoder::new().encode(&frames).unwrap();

    assert!(bytes.starts_with(b"GIF89a") || bytes.starts_with(b"GIF87a"));
    let decoded = GifDecoder::new(Cursor::new(bytes))
        .unwrap()
        .into_frames()
        .collect_frames()
        .unwrap();
    assert_eq!(decoded.len(), 2);
    assert_eq!(decoded[0].buffer().dimensions(), (3, 2));
    assert_eq!(decoded[0].buffer().get_pixel(0, 0).0, [255, 0, 0, 255]);
    assert_eq!(decoded[1].buffer().get_pixel(0, 0).0, [0, 0, 255, 255]);
    assert_eq!(decoded[0].delay().numer_denom_ms(), (40, 1));
    assert_eq!(decoded[1].delay().numer_denom_ms(), (90, 1));
}

#[test]
fn encoding_is_deterministic_and_repeat_is_explicit() {
    let frames = [solid(2, 2, [1, 2, 3, 255], 50)];
    let encoder = GifAnimationEncoder::with_repeat(AnimationRepeat::Once);
    let once = encoder.encode(&frames).unwrap();
    assert_eq!(once, encoder.encode(&frames).unwrap());
    assert!(
        !once.windows(11).any(|window| window == b"NETSCAPE2.0"),
        "play-once GIF must omit the looping extension"
    );
    let looping = GifAnimationEncoder::new().encode(&frames).unwrap();
    assert!(
        looping.windows(11).any(|window| window == b"NETSCAPE2.0"),
        "infinite GIF must include the looping extension"
    );
    assert_eq!(encoder.repeat(), AnimationRepeat::Once);
    assert_eq!(AnimationFormat::Gif.extension(), "gif");
    assert_eq!(AnimationFormat::Gif.media_type(), "image/gif");

    let nonce = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let path = std::env::temp_dir().join(format!(
        "scrozz-gif-repeat-{}-{nonce}-{}.gif",
        std::process::id(),
        NEXT_PATH.fetch_add(1, Ordering::Relaxed)
    ));
    std::fs::write(&path, looping).unwrap();
    let inspection = inspect_gif_file(&path).unwrap();
    assert_eq!(inspection.frames, 1);
    assert_eq!(inspection.duration, Duration::from_millis(50));
    assert_eq!(inspection.repeat, AnimationRepeat::Infinite);
    std::fs::remove_file(path).unwrap();
}

#[test]
fn streaming_encoder_uses_cumulative_centisecond_quantization() {
    let encoder = GifAnimationEncoder::with_repeat(AnimationRepeat::Once);
    let mut stream = encoder.stream(Vec::new());
    stream
        .write_frame(solid(2, 2, [255, 0, 0, 255], 15))
        .unwrap();
    stream
        .write_frame(solid(2, 2, [0, 0, 255, 255], 15))
        .unwrap();
    let bytes = stream.finish().unwrap();

    let decoded = GifDecoder::new(Cursor::new(bytes))
        .unwrap()
        .into_frames()
        .collect_frames()
        .unwrap();
    let delays: Vec<_> = decoded
        .iter()
        .map(|frame| frame.delay().numer_denom_ms().0)
        .collect();
    assert_eq!(delays, [20, 10]);
    assert_eq!(delays.iter().sum::<u32>(), 30);
}

#[test]
fn malformed_animation_inputs_are_errors_not_panics() {
    let encoder = GifAnimationEncoder::new();
    assert!(encoder.encode(&[]).is_err());
    assert!(
        encoder
            .encode(&[TimedRgbaFrame::new(
                RgbaImage {
                    width: 2,
                    height: 2,
                    data: vec![0; 3],
                },
                Duration::from_millis(20),
            )])
            .is_err()
    );
    assert!(
        encoder
            .encode(&[
                solid(2, 2, [0, 0, 0, 255], 20),
                solid(3, 2, [0, 0, 0, 255], 20),
            ])
            .is_err()
    );
    assert!(encoder.encode(&[solid(2, 2, [0, 0, 0, 255], 0)]).is_err());
    assert!(encoder.encode(&[solid(2, 2, [0, 0, 0, 255], 1)]).is_err());
    assert!(
        encoder
            .encode(&[solid(2, 2, [0, 0, 0, 255], 655_351)])
            .is_err()
    );
}

#[test]
fn palette_dithering_is_deterministic_and_selectable() {
    let mut data = Vec::with_capacity(32 * 16 * 4);
    for y in 0..16_u8 {
        for x in 0..32_u8 {
            data.extend_from_slice(&[
                x.saturating_mul(8),
                y.saturating_mul(16),
                x.wrapping_mul(13).wrapping_add(y.wrapping_mul(7)),
                255,
            ]);
        }
    }
    let frame = TimedRgbaFrame::new(
        RgbaImage {
            width: 32,
            height: 16,
            data,
        },
        Duration::from_millis(100),
    );
    let plain = GifAnimationEncoder::with_options(AnimationRepeat::Once, 10, GifDither::None)
        .unwrap()
        .encode(std::slice::from_ref(&frame))
        .unwrap();
    let dithered =
        GifAnimationEncoder::with_options(AnimationRepeat::Once, 10, GifDither::FloydSteinberg)
            .unwrap()
            .encode(std::slice::from_ref(&frame))
            .unwrap();

    assert_ne!(plain, dithered);
    assert!(dithered.len() < 64 * 1024);
    assert_eq!(
        dithered,
        GifAnimationEncoder::with_options(AnimationRepeat::Once, 10, GifDither::FloydSteinberg,)
            .unwrap()
            .encode(&[frame])
            .unwrap()
    );
    assert!(
        GifDecoder::new(Cursor::new(dithered.clone()))
            .unwrap()
            .into_frames()
            .collect_frames()
            .is_ok()
    );
    let options = gif::DecodeOptions::new();
    let mut decoder = options.read_info(Cursor::new(dithered)).unwrap();
    let frame = decoder.read_next_frame().unwrap().unwrap();
    assert!(
        frame
            .palette
            .as_ref()
            .is_some_and(|palette| !palette.is_empty() && palette.len() <= 256 * 3)
    );
}

#[test]
fn dithering_preserves_transparent_pixels() {
    let frame = TimedRgbaFrame::new(
        RgbaImage {
            width: 2,
            height: 1,
            data: vec![255, 0, 0, 0, 0, 255, 0, 255],
        },
        Duration::from_millis(100),
    );
    let bytes =
        GifAnimationEncoder::with_options(AnimationRepeat::Once, 10, GifDither::FloydSteinberg)
            .unwrap()
            .encode(&[frame])
            .unwrap();
    let decoded = GifDecoder::new(Cursor::new(bytes))
        .unwrap()
        .into_frames()
        .collect_frames()
        .unwrap();
    assert_eq!(decoded[0].buffer().get_pixel(0, 0).0[3], 0);
    assert_eq!(decoded[0].buffer().get_pixel(1, 0).0[3], 255);
}
