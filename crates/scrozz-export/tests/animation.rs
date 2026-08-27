//! Reusable GIF encoder tests.

use std::{io::Cursor, time::Duration};

use image::{AnimationDecoder, codecs::gif::GifDecoder};
use scrozz_export::{
    AnimationFormat, AnimationRepeat, GifAnimationEncoder, RgbaImage, TimedRgbaFrame,
};

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
