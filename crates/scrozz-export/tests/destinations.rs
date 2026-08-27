//! Destinations (decision D18): folders, the clipboard, and S3-compatible
//! storage.

mod common;

use std::{
    path::{Path, PathBuf},
    sync::{Arc, Mutex},
};

use common::{embedded_profile, solid};
use scrozz_core::{ColorSpace, Error, Frame, PixelFormat, ScaleFactor};
use scrozz_export::{
    Clipboard, ClipboardPlatform, ClipboardReport, ContentKind, Destination,
    DestinationCapabilities, DestinationColorSpace, DestinationProfile, Encoder, ExportOutcome,
    FileExporter, FrameEncoder, ImageFormat, NamePolicy, NameTemplate, NamingContext, S3Object,
    S3Uploader, Timestamp, UnimplementedS3Uploader,
};

// ---------------------------------------------------------------------------
// Doubles
// ---------------------------------------------------------------------------

/// A clipboard that records instead of writing, so the tests never go near the
/// real one.
///
/// The log lives behind an `Arc` because the exporter takes ownership of the
/// clipboard it is given; sharing the log is the only way to look at what was
/// written afterwards.
#[derive(Debug, Clone, Default)]
struct RecordingClipboard {
    written: Arc<Mutex<Vec<Frame>>>,
}

impl RecordingClipboard {
    fn last(&self) -> Option<Frame> {
        self.written.lock().unwrap().last().cloned()
    }

    fn count(&self) -> usize {
        self.written.lock().unwrap().len()
    }
}

impl Clipboard for RecordingClipboard {
    fn write_image(&self, frame: &Frame) -> scrozz_core::Result<()> {
        self.written.lock().unwrap().push(frame.clone());
        Ok(())
    }

    fn write_image_with_report(
        &self,
        frame: &Frame,
    ) -> scrozz_core::Result<Option<ClipboardReport>> {
        self.write_image(frame)?;
        Ok(Some(ClipboardReport {
            platform: ClipboardPlatform::MacOs,
            delivered: &["public.png"],
            missing: &[],
        }))
    }
}

/// A clipboard that always fails, standing in for a headless machine.
#[derive(Debug)]
struct BrokenClipboard;

impl Clipboard for BrokenClipboard {
    fn write_image(&self, _frame: &Frame) -> scrozz_core::Result<()> {
        Err(Error::Platform("no display server".into()))
    }
}

/// One recorded upload request.
type Request = (String, String, String, usize);

/// An uploader that records the request and hands back a plausible URL.
#[derive(Debug, Clone, Default)]
struct RecordingUploader {
    seen: Arc<Mutex<Vec<Request>>>,
}

impl RecordingUploader {
    fn requests(&self) -> Vec<Request> {
        self.seen.lock().unwrap().clone()
    }
}

impl S3Uploader for RecordingUploader {
    fn upload(&self, object: &S3Object<'_>) -> scrozz_core::Result<String> {
        self.seen.lock().unwrap().push((
            object.bucket.to_owned(),
            object.key.to_owned(),
            object.content_type.to_owned(),
            object.bytes.len(),
        ));
        Ok(format!("https://cdn.example.com/{}", object.key))
    }
}

/// An uploader that fails, so the queue behaviour D18 describes has something
/// to react to.
#[derive(Debug)]
struct FailingUploader;

impl S3Uploader for FailingUploader {
    fn upload(&self, _object: &S3Object<'_>) -> scrozz_core::Result<String> {
        Err(Error::Storage("503 from the bucket".into()))
    }
}

// ---------------------------------------------------------------------------
// Fixtures
// ---------------------------------------------------------------------------

fn scratch(name: &str) -> PathBuf {
    let dir = Path::new(env!("CARGO_TARGET_TMPDIR")).join(name);
    let _ = std::fs::remove_dir_all(&dir);
    dir
}

fn context() -> NamingContext {
    NamingContext {
        timestamp: Some(Timestamp {
            year: 2025,
            month: 3,
            day: 9,
            hour: 14,
            minute: 5,
            second: 7,
        }),
        app: Some("Safari".into()),
        title: Some("Inbox".into()),
        sequence: 1,
        width: 0,
        height: 0,
    }
}

fn png_bytes() -> Vec<u8> {
    FrameEncoder::new()
        .encode(&solid(4, 4, [1, 2, 3]), ImageFormat::Png)
        .unwrap()
}

// ---------------------------------------------------------------------------
// Folders
// ---------------------------------------------------------------------------

#[test]
fn a_capture_lands_in_the_folder_under_its_templated_name() {
    let dir = scratch("folder-basic");
    let outcome = FileExporter::new()
        .export_bytes(&png_bytes(), &Destination::Folder(dir.clone()), &context())
        .expect("exports");

    let path = outcome.path.expect("a folder export reports where it went");
    assert_eq!(
        path.file_name().unwrap(),
        "Screenshot 2025-03-09 at 14-05-07.png"
    );
    assert!(path.exists());
    assert_eq!(std::fs::read(&path).unwrap(), png_bytes());

    std::fs::remove_dir_all(&dir).ok();
}

#[test]
fn the_folder_is_created_if_it_does_not_exist_yet() {
    // A Dropbox subfolder the user typed into settings will not exist until
    // something writes to it, and refusing at that point would be pointless.
    let dir = scratch("folder-created").join("nested").join("deeper");
    assert!(!dir.exists());

    FileExporter::new()
        .export_bytes(&png_bytes(), &Destination::Folder(dir.clone()), &context())
        .expect("exports");
    assert!(dir.is_dir());

    std::fs::remove_dir_all(dir.parent().unwrap().parent().unwrap()).ok();
}

#[test]
fn a_burst_of_captures_never_overwrites_an_earlier_one() {
    // Two captures a few milliseconds apart render identical names, so the
    // disambiguation has to happen at write time.
    let dir = scratch("folder-burst");
    let exporter = FileExporter::new();
    let mut paths = Vec::new();

    for _ in 0..4 {
        let outcome = exporter
            .export_bytes(&png_bytes(), &Destination::Folder(dir.clone()), &context())
            .expect("exports");
        paths.push(outcome.path.expect("path"));
    }

    let names: Vec<_> = paths
        .iter()
        .map(|p| p.file_name().unwrap().to_string_lossy().into_owned())
        .collect();
    assert_eq!(
        names,
        [
            "Screenshot 2025-03-09 at 14-05-07.png",
            "Screenshot 2025-03-09 at 14-05-07 2.png",
            "Screenshot 2025-03-09 at 14-05-07 3.png",
            "Screenshot 2025-03-09 at 14-05-07 4.png",
        ]
    );
    assert_eq!(std::fs::read_dir(&dir).unwrap().count(), 4);

    std::fs::remove_dir_all(&dir).ok();
}

#[test]
fn the_extension_follows_the_bytes_rather_than_a_guess() {
    let dir = scratch("folder-extensions");
    let frame = solid(4, 4, [9, 9, 9]);
    let exporter = FileExporter::new();

    for (format, extension) in [
        (ImageFormat::Png, "png"),
        (ImageFormat::Jpeg, "jpg"),
        (ImageFormat::WebP, "webp"),
    ] {
        let bytes = FrameEncoder::new().encode(&frame, format).unwrap();
        let outcome = exporter
            .export_bytes(&bytes, &Destination::Folder(dir.clone()), &context())
            .expect("exports");
        let path = outcome.path.unwrap();
        assert_eq!(
            path.extension().unwrap(),
            extension,
            "{format:?} should be saved as .{extension}"
        );
    }

    std::fs::remove_dir_all(&dir).ok();
}

#[test]
fn a_folder_export_reports_a_usable_file_url() {
    let dir = scratch("folder url");
    let outcome = FileExporter::new()
        .export_bytes(&png_bytes(), &Destination::Folder(dir.clone()), &context())
        .expect("exports");

    let url = outcome.url.expect("a URL");
    assert!(url.starts_with("file:///"), "got {url}");
    assert!(
        url.contains("folder%20url"),
        "spaces must be escaped: {url}"
    );
    assert!(
        !url.contains(' '),
        "a URL with a raw space is not a URL: {url}"
    );

    std::fs::remove_dir_all(&dir).ok();
}

#[test]
fn a_custom_template_is_honoured() {
    let dir = scratch("folder-template");
    let exporter = FileExporter::new()
        .with_template(NameTemplate::parse("{app} {title} {width}x{height}").unwrap());

    let frame = solid(120, 80, [4, 5, 6]);
    let outcome = exporter
        .export_frame(
            &frame,
            ImageFormat::Png,
            &Destination::Folder(dir.clone()),
            &context(),
        )
        .expect("exports");

    assert_eq!(
        outcome.path.unwrap().file_name().unwrap(),
        "Safari Inbox 120x80.png",
        "export_frame should fill the size in from the frame itself"
    );

    std::fs::remove_dir_all(&dir).ok();
}

#[test]
fn frame_scale_adds_retina_suffix_without_changing_one_x_exports() {
    let dir = scratch("folder-retina");
    let exporter = FileExporter::new().with_template(NameTemplate::parse("Capture").unwrap());
    let one_x = solid(2, 2, [1, 2, 3]);
    let mut retina = solid(2, 2, [1, 2, 3]);
    retina.scale = ScaleFactor::new(2.0);

    let first = exporter
        .export_frame(
            &one_x,
            ImageFormat::Png,
            &Destination::Folder(dir.clone()),
            &context(),
        )
        .unwrap()
        .path
        .unwrap();
    let second = exporter
        .export_frame(
            &retina,
            ImageFormat::Png,
            &Destination::Folder(dir.clone()),
            &context(),
        )
        .unwrap()
        .path
        .unwrap();

    assert_eq!(first.file_name().unwrap(), "Capture.png");
    assert_eq!(second.file_name().unwrap(), "Capture@2x.png");
    std::fs::remove_dir_all(&dir).ok();
}

#[test]
fn automatic_export_uses_destination_format_and_colour_policy() {
    let dir = scratch("folder-auto");
    let source = common::frame(
        1,
        1,
        0,
        PixelFormat::Rgba8,
        ColorSpace::DisplayP3,
        |_, _| [128, 64, 32, 255],
    );
    let mut profile = DestinationProfile::folder();
    profile.capabilities = DestinationCapabilities::new([ImageFormat::Png]);
    profile.color_space = DestinationColorSpace::Srgb;

    let path = FileExporter::new()
        .export_frame_auto(
            &source,
            &Destination::Folder(dir.clone()),
            &profile,
            ContentKind::Screenshot,
            &context(),
        )
        .unwrap()
        .path
        .unwrap();
    let written = std::fs::read(path).unwrap();
    let (width, _, data) = common::decode(&written);

    assert_eq!(common::pixel_at(&data, width, 0, 0), [138, 59, 21, 255]);
    assert_eq!(
        embedded_profile(&written),
        scrozz_export::profile_for(ColorSpace::Srgb)
    );
    std::fs::remove_dir_all(&dir).ok();
}

#[test]
fn a_title_full_of_illegal_characters_still_produces_a_file() {
    let dir = scratch("folder-illegal");
    let ctx = context().with_title(r#"Re: <draft> "notes" 50/50?"#);
    let exporter = FileExporter::new().with_template(NameTemplate::parse("{title}").unwrap());

    let path = exporter
        .export_bytes(&png_bytes(), &Destination::Folder(dir.clone()), &ctx)
        .expect("exports")
        .path
        .unwrap();

    assert_eq!(path.file_name().unwrap(), "Re- -draft- -notes- 50-50-.png");
    assert!(path.exists());

    std::fs::remove_dir_all(&dir).ok();
}

#[test]
fn bytes_that_are_not_an_image_are_refused_with_a_reason() {
    let dir = scratch("folder-garbage");
    let err = FileExporter::new()
        .export_bytes(
            b"this is not a picture",
            &Destination::Folder(dir),
            &context(),
        )
        .unwrap_err();

    assert!(matches!(err, Error::Codec(_)), "got {err:?}");
    assert!(
        err.to_string().contains("PNG"),
        "the message should say what was expected"
    );
}

#[test]
fn the_written_file_keeps_the_captures_colour_profile() {
    // The end-to-end version of the colour-management requirement: a Display P3
    // capture must still be a Display P3 file once it is on disk.
    let dir = scratch("folder-profile");
    let frame = common::frame(
        4,
        4,
        0,
        PixelFormat::Rgba8,
        ColorSpace::DisplayP3,
        |_, _| [255, 0, 0, 255],
    );

    let path = FileExporter::new()
        .export_frame(
            &frame,
            ImageFormat::Png,
            &Destination::Folder(dir.clone()),
            &context(),
        )
        .expect("exports")
        .path
        .unwrap();

    let written = std::fs::read(&path).unwrap();
    assert_eq!(
        embedded_profile(&written),
        scrozz_export::profile_for(ColorSpace::DisplayP3),
        "the saved file lost its Display P3 profile"
    );

    std::fs::remove_dir_all(&dir).ok();
}

// ---------------------------------------------------------------------------
// Clipboard
// ---------------------------------------------------------------------------

#[test]
fn a_clipboard_export_hands_the_frame_over_and_writes_no_file() {
    let recorder = RecordingClipboard::default();
    let exporter = FileExporter::new().with_clipboard(Box::new(recorder.clone()));

    let frame = solid(6, 6, [3, 4, 5]);
    let outcome = exporter
        .export_frame(
            &frame,
            ImageFormat::Png,
            &Destination::Clipboard,
            &context(),
        )
        .expect("exports");

    assert_eq!(recorder.count(), 1);
    assert_eq!(outcome.path, None, "nothing should be written to disk");
    assert_eq!(outcome.url, None, "the clipboard produces no link");
    assert_eq!(outcome.clipboard.unwrap().delivered, ["public.png"]);
}

#[test]
fn exporting_a_frame_to_the_clipboard_keeps_its_colour_space() {
    // The reason export_frame exists: the byte-oriented Exporter contract has
    // to decode the image again, and a decoded PNG cannot say what space it was
    // in without an ICC parser.
    let recorder = RecordingClipboard::default();
    let exporter = FileExporter::new().with_clipboard(Box::new(recorder.clone()));

    let frame = common::frame(
        3,
        3,
        4,
        PixelFormat::Rgba8,
        ColorSpace::DisplayP3,
        |_, _| [1, 2, 3, 255],
    );
    exporter
        .export_frame(
            &frame,
            ImageFormat::Png,
            &Destination::Clipboard,
            &context(),
        )
        .expect("exports");

    let seen = recorder.last().expect("a frame");
    assert_eq!(seen.color_space, ColorSpace::DisplayP3);
    assert_eq!(
        seen.stride, frame.stride,
        "the frame should be passed through untouched"
    );
}

#[test]
fn the_byte_oriented_path_still_reaches_the_clipboard() {
    let recorder = RecordingClipboard::default();
    let exporter = FileExporter::new().with_clipboard(Box::new(recorder.clone()));

    exporter
        .export_bytes(&png_bytes(), &Destination::Clipboard, &context())
        .expect("exports");

    let seen = recorder.last().expect("a frame");
    assert_eq!((seen.width(), seen.height()), (4, 4));
    assert_eq!(
        seen.color_space,
        ColorSpace::Unknown,
        "a decoded image must admit it does not know its colour space rather than \
         claim sRGB"
    );
}

#[test]
fn a_clipboard_failure_is_reported_rather_than_swallowed() {
    let exporter = FileExporter::new().with_clipboard(Box::new(BrokenClipboard));
    let err = exporter
        .export_frame(
            &solid(2, 2, [0, 0, 0]),
            ImageFormat::Png,
            &Destination::Clipboard,
            &context(),
        )
        .unwrap_err();
    assert!(matches!(err, Error::Platform(_)), "got {err:?}");
}

// ---------------------------------------------------------------------------
// S3
// ---------------------------------------------------------------------------

#[test]
fn without_a_bucket_configured_s3_is_unsupported_and_says_what_to_do_instead() {
    // Never a panic on the default path: `UnimplementedS3Uploader` is only
    // reached if somebody installs it deliberately.
    let destination = Destination::S3 {
        bucket: "shots".into(),
        prefix: "2025/".into(),
    };
    let err = FileExporter::new()
        .export_bytes(&png_bytes(), &destination, &context())
        .unwrap_err();

    assert!(matches!(err, Error::Unsupported { .. }), "got {err:?}");
    let text = err.to_string();
    assert!(
        text.contains("folder"),
        "the message should offer the alternative: {text}"
    );
}

#[test]
fn an_upload_gets_the_bytes_the_key_and_the_media_type() {
    let uploader = RecordingUploader::default();
    let exporter =
        FileExporter::new().with_uploader(Box::new(uploader.clone()), "https://cdn.example.com");

    let bytes = FrameEncoder::new()
        .encode(&solid(4, 4, [1, 1, 1]), ImageFormat::Jpeg)
        .unwrap();
    let destination = Destination::S3 {
        bucket: "shots".into(),
        prefix: "captures".into(),
    };
    let outcome = exporter
        .export_bytes(&bytes, &destination, &context())
        .expect("uploads");

    let seen = uploader.requests();
    assert_eq!(seen.len(), 1);
    let (bucket, key, content_type, length) = seen[0].clone();
    assert_eq!(bucket, "shots");
    assert_eq!(key, "captures/Screenshot 2025-03-09 at 14-05-07.jpg");
    assert_eq!(
        content_type, "image/jpeg",
        "without this the link downloads instead of showing"
    );
    assert_eq!(length, bytes.len());
    assert_eq!(
        outcome.url.unwrap(),
        "https://cdn.example.com/captures/Screenshot 2025-03-09 at 14-05-07.jpg"
    );
    assert_eq!(outcome.path, None);
}

#[test]
fn key_prefixes_are_normalised_to_exactly_one_separator() {
    // A leading slash produces a bucket with an unnamed top-level folder, which
    // most browsers will not show at all.
    let cases = [
        ("", ""),
        ("shots", "shots/"),
        ("/shots/", "shots/"),
        ("a/b", "a/b/"),
    ];

    for (prefix, expected) in cases {
        let uploader = RecordingUploader::default();
        let exporter = FileExporter::new()
            .with_uploader(Box::new(uploader.clone()), "https://x")
            .with_template(NameTemplate::parse("shot").unwrap());

        let destination = Destination::S3 {
            bucket: "b".into(),
            prefix: prefix.into(),
        };
        exporter
            .export_bytes(&png_bytes(), &destination, &context())
            .expect("uploads");

        assert_eq!(uploader.requests()[0].1, format!("{expected}shot.png"));
    }
}

#[test]
fn an_upload_failure_surfaces_so_the_queue_can_retry() {
    let exporter = FileExporter::new().with_uploader(Box::new(FailingUploader), "https://x");
    let destination = Destination::S3 {
        bucket: "b".into(),
        prefix: String::new(),
    };
    let err = exporter
        .export_bytes(&png_bytes(), &destination, &context())
        .unwrap_err();
    assert!(matches!(err, Error::Storage(_)), "got {err:?}");
}

#[test]
#[should_panic(expected = "Signature Version 4")]
fn the_stub_uploader_is_explicitly_unfinished() {
    // Documents the deliberate stub: the interface is defined and wired, the
    // protocol work is not done, and nothing reaches this unless it is
    // installed on purpose.
    let _ = UnimplementedS3Uploader.upload(&S3Object {
        bucket: "b",
        key: "k",
        bytes: &[],
        content_type: "image/png",
    });
}

// ---------------------------------------------------------------------------
// The trait
// ---------------------------------------------------------------------------

#[test]
fn the_exporter_trait_works_through_a_trait_object() {
    use scrozz_export::Exporter;

    let dir = scratch("trait-object");
    let exporter: Box<dyn Exporter> = Box::new(FileExporter::new());
    let url = exporter
        .export(&png_bytes(), &Destination::Folder(dir.clone()))
        .expect("exports")
        .expect("a URL");

    assert!(url.starts_with("file://"));
    assert_eq!(std::fs::read_dir(&dir).unwrap().count(), 1);

    std::fs::remove_dir_all(&dir).ok();
}

#[test]
fn the_default_outcome_is_empty() {
    let outcome = ExportOutcome::default();
    assert!(outcome.path.is_none() && outcome.url.is_none() && outcome.clipboard.is_none());
}

#[test]
fn the_policy_can_be_swapped_for_one_with_different_limits() {
    let dir = scratch("policy");
    let exporter = FileExporter::new()
        .with_policy(NamePolicy {
            max_component_bytes: 24,
            ..NamePolicy::default()
        })
        .with_template(NameTemplate::parse("{title}").unwrap());

    let ctx = context().with_title("x".repeat(300));
    let path = exporter
        .export_bytes(&png_bytes(), &Destination::Folder(dir.clone()), &ctx)
        .expect("exports")
        .path
        .unwrap();

    assert!(path.file_name().unwrap().len() <= 24);
    assert!(path.exists());

    std::fs::remove_dir_all(&dir).ok();
}
