//! Filename templating, sanitisation and collision handling.

use std::{
    collections::HashSet,
    path::{Path, PathBuf},
};

use scrozz_core::ScaleFactor;
use scrozz_export::{FilenameRules, NamePolicy, NameTemplate, NamingContext, Timestamp};

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
        title: Some("Inbox (3)".into()),
        sequence: 42,
        width: 2560,
        height: 1440,
    }
}

fn render(source: &str) -> String {
    NameTemplate::parse(source)
        .expect("parses")
        .render(&context(), &NamePolicy::default())
}

// ---------------------------------------------------------------------------
// Templates
// ---------------------------------------------------------------------------

#[test]
fn the_default_template_reads_like_a_screenshot_filename() {
    let name = NamePolicy::default()
        .file_name(&NameTemplate::default(), &context(), "png", None)
        .expect("names");
    assert_eq!(name, "Screenshot 2025-03-09 at 14-05-07.png");
}

#[test]
fn the_time_field_avoids_colons() {
    // A colon is illegal on Windows and the macOS Finder draws it as a path
    // separator, so the one field guaranteed to contain times must not use it.
    assert_eq!(render("{time}"), "14-05-07");
    assert!(!render("{date} {time}").contains(':'));
}

#[test]
fn every_documented_field_renders() {
    assert_eq!(render("{year}-{month}-{day}"), "2025-03-09");
    assert_eq!(render("{hour}{minute}{second}"), "140507");
    assert_eq!(render("{date}"), "2025-03-09");
    assert_eq!(render("{app}"), "Safari");
    assert_eq!(render("{title}"), "Inbox (3)");
    assert_eq!(render("{seq}"), "42");
    assert_eq!(render("{width}x{height}"), "2560x1440");
}

#[test]
fn braces_can_be_written_literally() {
    assert_eq!(render("{{{app}}}"), "{Safari}");
    assert_eq!(render("100{{"), "100{");
}

#[test]
fn a_misspelt_field_is_rejected_at_parse_time_rather_than_silently_dropped() {
    // A settings dialog needs to be able to say "that is not a field"; a
    // template that quietly renders nothing produces a mystery later.
    let err = NameTemplate::parse("{date} {tilte}")
        .unwrap_err()
        .to_string();
    assert!(
        err.contains("tilte"),
        "the error should name the offending field: {err}"
    );
    assert!(
        err.contains("title"),
        "and should list what is available: {err}"
    );
}

#[test]
fn an_unclosed_brace_is_rejected() {
    for bad in ["{date", "{", "{date} {date", "prefix {app"] {
        assert!(
            NameTemplate::parse(bad).is_err(),
            "{bad:?} should not parse"
        );
    }
}

#[test]
fn missing_optional_values_render_as_nothing_and_still_produce_a_name() {
    // Wayland cannot supply a window title at all, so a title-based template
    // must degrade to something usable rather than to ".png".
    let empty = NamingContext::default();
    let policy = NamePolicy::default();
    let template = NameTemplate::parse("{title}").expect("parses");

    assert_eq!(template.render(&empty, &policy), "");
    assert_eq!(
        policy.file_name(&template, &empty, "png", None).unwrap(),
        "Screenshot.png"
    );
}

#[test]
fn window_supplied_fields_are_capped_but_clock_fields_are_not() {
    let policy = NamePolicy {
        max_field_chars: Some(10),
        ..NamePolicy::default()
    };
    let ctx = NamingContext::default()
        .with_title("a".repeat(400))
        .with_app("b".repeat(400));
    let rendered = NameTemplate::parse("{title}-{app}")
        .unwrap()
        .render(&ctx, &policy);
    assert_eq!(rendered, format!("{}-{}", "a".repeat(10), "b".repeat(10)));
}

// ---------------------------------------------------------------------------
// Sanitisation
// ---------------------------------------------------------------------------

#[test]
fn characters_that_are_illegal_anywhere_are_replaced_everywhere() {
    // Portable rules by default, because D18 says captures land in folders that
    // sync to other operating systems.
    let policy = NamePolicy::default();
    assert_eq!(
        policy.sanitise(r#"a<b>c:d"e/f\g|h?i*j"#),
        "a-b-c-d-e-f-g-h-i-j"
    );
}

#[test]
fn runs_of_illegal_characters_collapse_to_one_separator() {
    assert_eq!(NamePolicy::default().sanitise("docs///notes"), "docs-notes");
}

#[test]
fn control_characters_are_removed_rather_than_made_visible() {
    // A title with a stray bell or null should not gain a dash where the
    // invisible character was; nothing was ever shown to the user there. A tab
    // or newline is different — the user saw the gap, so the gap survives.
    let policy = NamePolicy::default();
    assert_eq!(policy.sanitise("Hello\u{7}\tWorld\n"), "Hello World");
    assert_eq!(policy.sanitise("Two\u{0}Words"), "TwoWords");
    assert_eq!(policy.sanitise("Line\r\n\r\nBreak"), "Line Break");
}

#[test]
fn trailing_dots_and_spaces_are_trimmed() {
    // Windows strips them silently, so a name ending in one is not the name
    // that ends up on disk.
    assert_eq!(NamePolicy::default().sanitise("report..."), "report");
    assert_eq!(NamePolicy::default().sanitise("report   "), "report");
    assert_eq!(NamePolicy::default().sanitise(".hidden"), "hidden");
}

#[test]
fn reserved_windows_device_names_are_defused() {
    let policy = NamePolicy::default();
    for name in ["CON", "con", "NUL", "com1", "LPT9", "aux"] {
        let out = policy.sanitise(name);
        assert!(
            out.ends_with('_'),
            "{name} should not remain a device name, got {out}"
        );
    }
    // The reservation applies to the part before the first dot, so this one is
    // reserved even though the whole string is not.
    assert_eq!(policy.sanitise("CON.backup"), "CON.backup_");
    assert_eq!(
        policy.sanitise("CONTACT"),
        "CONTACT",
        "only exact device names are reserved"
    );
}

#[test]
fn a_name_that_sanitises_away_entirely_falls_back() {
    let policy = NamePolicy::default();
    assert_eq!(policy.sanitise("???"), "-");
    assert_eq!(policy.sanitise(""), "Screenshot");
    assert_eq!(policy.sanitise("..."), "Screenshot");

    let ctx = NamingContext::default().with_title("...");
    let name = policy
        .file_name(&NameTemplate::parse("{title}").unwrap(), &ctx, "png", None)
        .unwrap();
    assert_eq!(name, "Screenshot.png", "must never produce a bare '.png'");
}

#[test]
fn native_rules_keep_more_of_the_users_title_than_portable_rules() {
    let native = NamePolicy {
        rules: FilenameRules::Native,
        ..NamePolicy::default()
    };
    let portable = NamePolicy::default();
    let title = r#"Q1 <report> "final""#;

    assert_eq!(portable.sanitise(title), "Q1 -report- -final-");
    if cfg!(windows) {
        assert_eq!(native.sanitise(title), "Q1 -report- -final-");
    } else {
        assert_eq!(native.sanitise(title), title);
    }
    // A colon is excluded even under native Unix rules, because the Finder
    // renders it as a slash.
    assert_eq!(native.sanitise("10:30"), "10-30");
}

// ---------------------------------------------------------------------------
// Lengths
// ---------------------------------------------------------------------------

#[test]
fn a_very_long_window_title_is_truncated_without_splitting_a_character() {
    // Every character here is three UTF-8 bytes, so a byte-wise cut at 255 lands
    // mid-character and produces invalid UTF-8.
    let title = "設定".repeat(300);
    let ctx = NamingContext::default().with_title(&title);
    let policy = NamePolicy {
        max_field_chars: None,
        ..NamePolicy::default()
    };

    let name = policy
        .file_name(&NameTemplate::parse("{title}").unwrap(), &ctx, "png", None)
        .expect("names");

    assert!(
        name.len() <= policy.max_component_bytes,
        "{} bytes is too long",
        name.len()
    );
    assert!(name.ends_with(".png"));
    assert!(
        name.chars().all(|c| c != '\u{FFFD}'),
        "truncation produced invalid UTF-8"
    );
}

#[test]
fn the_whole_path_stays_inside_the_windows_limit() {
    let deep = PathBuf::from(format!("C:\\Users\\somebody\\{}", "folder\\".repeat(20)));
    let ctx = NamingContext::default().with_title("t".repeat(300));
    let policy = NamePolicy::default();

    let name = policy
        .file_name(
            &NameTemplate::parse("{title}").unwrap(),
            &ctx,
            "png",
            Some(&deep),
        )
        .expect("names");

    let full = deep.join(&name);
    assert!(
        full.as_os_str().len() <= policy.max_path_bytes,
        "{} is {} bytes, over the {} limit",
        full.display(),
        full.as_os_str().len(),
        policy.max_path_bytes
    );
}

#[test]
fn a_directory_with_no_room_left_is_an_error_the_user_can_act_on() {
    // Better to say "choose a shorter folder" than to write a file called
    // ".png" that nothing will open.
    let absurd = PathBuf::from("/".to_owned() + &"x".repeat(400));
    let err = NamePolicy::default()
        .file_name(&NameTemplate::default(), &context(), "png", Some(&absurd))
        .unwrap_err()
        .to_string();
    assert!(
        err.contains("shorter"),
        "the error should suggest the remedy: {err}"
    );
}

// ---------------------------------------------------------------------------
// Collisions
// ---------------------------------------------------------------------------

#[test]
fn the_first_capture_gets_the_plain_name_and_the_rest_are_numbered() {
    let policy = NamePolicy::default();
    let dir = Path::new("/shots");
    let mut taken: HashSet<PathBuf> = HashSet::new();

    let mut names = Vec::new();
    for _ in 0..3 {
        let path = policy
            .unique_path(dir, &NameTemplate::default(), &context(), "png", &mut |p| {
                taken.contains(p)
            })
            .expect("finds a free name");
        taken.insert(path.clone());
        names.push(path.file_name().unwrap().to_string_lossy().into_owned());
    }

    assert_eq!(
        names,
        [
            "Screenshot 2025-03-09 at 14-05-07.png",
            "Screenshot 2025-03-09 at 14-05-07 2.png",
            "Screenshot 2025-03-09 at 14-05-07 3.png",
        ]
    );
}

#[test]
fn retina_suffix_precedes_extension_and_composes_with_collisions() {
    let policy = NamePolicy::default();
    let template = NameTemplate::parse("Capture").unwrap();
    let dir = Path::new("/shots");
    let mut taken = HashSet::new();

    let first = policy
        .unique_path_for_scale(
            dir,
            &template,
            &context(),
            "png",
            ScaleFactor::new(1.999),
            &mut |path| taken.contains(path),
        )
        .unwrap();
    taken.insert(first.clone());
    let second = policy
        .unique_path_for_scale(
            dir,
            &template,
            &context(),
            "png",
            ScaleFactor::new(2.0),
            &mut |path| taken.contains(path),
        )
        .unwrap();

    assert_eq!(first.file_name().unwrap(), "Capture@2x.png");
    assert_eq!(second.file_name().unwrap(), "Capture@2x 2.png");
}

#[test]
fn one_x_names_are_unchanged_and_existing_retina_suffix_is_not_duplicated() {
    let policy = NamePolicy::default();
    assert_eq!(
        policy
            .file_name_for_scale(
                &NameTemplate::parse("Capture").unwrap(),
                &context(),
                "png",
                None,
                ScaleFactor::IDENTITY,
            )
            .unwrap(),
        "Capture.png"
    );
    assert_eq!(
        policy
            .file_name_for_scale(
                &NameTemplate::parse("Capture@2x").unwrap(),
                &context(),
                "png",
                None,
                ScaleFactor::new(2.0),
            )
            .unwrap(),
        "Capture@2x.png"
    );
}

#[test]
fn the_disambiguating_suffix_is_reserved_inside_the_length_budget() {
    // The classic bug: truncate to exactly the limit, then append " 2" and go
    // over it. The suffix has to be accounted for before the cut, not after.
    let ctx = NamingContext::default().with_title("t".repeat(600));
    let policy = NamePolicy {
        max_field_chars: None,
        ..NamePolicy::default()
    };
    let dir = Path::new("/s");
    let template = NameTemplate::parse("{title}").unwrap();
    let mut seen = HashSet::new();

    for _ in 0..12 {
        let path = policy
            .unique_path(dir, &template, &ctx, "png", &mut |p| seen.contains(p))
            .expect("finds a free name");
        let name = path.file_name().unwrap().to_string_lossy().into_owned();
        assert!(
            name.len() <= policy.max_component_bytes,
            "{} bytes exceeds the component limit",
            name.len()
        );
        assert!(
            path.as_os_str().len() <= policy.max_path_bytes,
            "{} bytes exceeds the path limit",
            path.as_os_str().len()
        );
        assert!(
            seen.insert(path),
            "unique_path handed out a name it had already used"
        );
    }
}

#[test]
fn collisions_are_resolved_against_the_real_filesystem_too() {
    let dir = Path::new(env!("CARGO_TARGET_TMPDIR")).join("naming-collisions");
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("creates the directory");

    let policy = NamePolicy::default();
    let first = policy
        .unique_path(
            &dir,
            &NameTemplate::default(),
            &context(),
            "png",
            &mut |p| p.exists(),
        )
        .unwrap();
    std::fs::write(&first, b"x").unwrap();

    let second = policy
        .unique_path(
            &dir,
            &NameTemplate::default(),
            &context(),
            "png",
            &mut |p| p.exists(),
        )
        .unwrap();
    assert_ne!(first, second);
    assert!(second.to_string_lossy().ends_with(" 2.png"));

    std::fs::remove_dir_all(&dir).ok();
}

#[test]
fn a_sequence_number_is_an_alternative_to_timestamps() {
    let policy = NamePolicy::default();
    let template = NameTemplate::parse("Shot {seq}").unwrap();
    let names: Vec<_> = (1..=3)
        .map(|n| {
            let ctx = NamingContext::default().with_sequence(n);
            policy.file_name(&template, &ctx, "webp", None).unwrap()
        })
        .collect();
    assert_eq!(names, ["Shot 1.webp", "Shot 2.webp", "Shot 3.webp"]);
}

// ---------------------------------------------------------------------------
// Timestamps
// ---------------------------------------------------------------------------

#[test]
fn timestamps_convert_from_unix_seconds() {
    let t = Timestamp::from_unix_seconds(1_741_530_307);
    assert_eq!((t.year, t.month, t.day), (2025, 3, 9));
    assert_eq!((t.hour, t.minute, t.second), (14, 25, 7));
}

#[test]
fn a_pre_epoch_timestamp_does_not_wrap_into_the_future() {
    let t = Timestamp::from_unix_seconds(-1);
    assert_eq!((t.year, t.month, t.day), (1969, 12, 31));
    assert_eq!((t.hour, t.minute, t.second), (23, 59, 59));
}

#[test]
fn the_current_time_is_plausible() {
    let now = Timestamp::now_utc();
    assert!((2024..2100).contains(&now.year), "got year {}", now.year);
    assert!((1..=12).contains(&now.month));
    assert!((1..=31).contains(&now.day));
}
