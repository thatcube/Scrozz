//! Pure decisions shared by the native Windows adapter and host-side tests.

use crate::prepare::Rgba8Image;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Backend {
    WindowsMediaOcr,
    Tesseract,
}

impl Backend {
    pub(crate) const fn engine_name(self) -> &'static str {
        match self {
            Self::WindowsMediaOcr => "windows-media-ocr",
            Self::Tesseract => "tesseract",
        }
    }
}

pub(crate) const fn backend_for_package_identity(has_identity: bool) -> Backend {
    if has_identity {
        Backend::WindowsMediaOcr
    } else {
        Backend::Tesseract
    }
}

/// Runs exactly one backend. In particular, an error from native OCR is final.
pub(crate) fn dispatch<T, E>(
    backend: Backend,
    native: impl FnOnce() -> std::result::Result<T, E>,
    portable: impl FnOnce() -> std::result::Result<T, E>,
) -> std::result::Result<T, E> {
    match backend {
        Backend::WindowsMediaOcr => native(),
        Backend::Tesseract => portable(),
    }
}

/// Converts straight-alpha RGBA into premultiplied BGRA over opaque white.
///
/// After compositing, alpha is 255, so straight and premultiplied channel values
/// are identical. Transparent capture padding therefore becomes white rather
/// than the black glyph-like pixels produced by premultiplying onto transparent
/// black.
pub(crate) fn bgra_premultiplied_on_white(image: &Rgba8Image) -> Vec<u8> {
    let mut out = vec![0u8; image.data.len()];
    for (source, destination) in image
        .data
        .as_chunks::<4>()
        .0
        .iter()
        .zip(out.as_chunks_mut::<4>().0.iter_mut())
    {
        let alpha = source[3];
        destination[0] = composite_on_white(source[2], alpha);
        destination[1] = composite_on_white(source[1], alpha);
        destination[2] = composite_on_white(source[0], alpha);
        destination[3] = 255;
    }
    out
}

fn composite_on_white(channel: u8, alpha: u8) -> u8 {
    let foreground = u32::from(channel) * u32::from(alpha);
    let background = 255 * u32::from(255 - alpha);
    ((foreground + background + 127) / 255) as u8
}

#[cfg(test)]
mod tests {
    use std::cell::Cell;

    use scrozz_core::Error;

    use super::*;

    #[test]
    fn package_identity_is_the_only_backend_selector() {
        let packaged = backend_for_package_identity(true);
        let portable = backend_for_package_identity(false);
        assert_eq!(packaged, Backend::WindowsMediaOcr);
        assert_eq!(packaged.engine_name(), "windows-media-ocr");
        assert_eq!(portable, Backend::Tesseract);
        assert_eq!(portable.engine_name(), "tesseract");
    }

    #[test]
    fn native_recognition_errors_never_fall_back() {
        let portable_called = Cell::new(false);
        let result: scrozz_core::Result<()> = dispatch(
            Backend::WindowsMediaOcr,
            || Err(Error::Platform("native recognition failed".to_string())),
            || {
                portable_called.set(true);
                Ok(())
            },
        );

        assert!(matches!(result, Err(Error::Platform(message)) if message.contains("native")));
        assert!(!portable_called.get());
    }

    #[test]
    fn portable_selection_never_activates_winrt() {
        let native_called = Cell::new(false);
        let result: scrozz_core::Result<&str> = dispatch(
            Backend::Tesseract,
            || {
                native_called.set(true);
                Ok("native")
            },
            || Ok("portable"),
        );

        assert_eq!(result.unwrap(), "portable");
        assert!(!native_called.get());
    }

    #[test]
    fn native_bitmap_composites_straight_alpha_before_premultiplication() {
        let image = Rgba8Image::new(
            3,
            1,
            vec![
                0, 0, 0, 0, // transparent black becomes the white background
                0, 0, 0, 128, // half-transparent black becomes mid-grey
                1, 2, 3, 255, // opaque RGB is only swizzled
            ],
        )
        .unwrap();

        assert_eq!(
            bgra_premultiplied_on_white(&image),
            [
                255, 255, 255, 255, //
                127, 127, 127, 255, //
                3, 2, 1, 255,
            ]
        );
    }
}
