//! The golden-image suite, and the tests that guard the harness itself.
//!
//! D25 says no Scrozz screenshot is ever taken by hand. That promise is only
//! worth anything if the machinery behind it is trustworthy, so this file tests
//! two different things and keeps them clearly apart:
//!
//! 1. **The harness works.** Determinism, the virtual clock, the diff reporter,
//!    the store manifest. These are unit tests that happen to render pictures.
//! 2. **The UI has not changed.** [`golden_corpus_matches_baselines`], the one
//!    test that will actually fail when somebody moves a rectangle.
//!
//! The first group exists because of how the second group dies. A flaky golden
//! test gets disabled within a week, and then the whole apparatus is worthless.
//! So determinism is asserted directly and loudly, rather than being left as an
//! emergent property that nobody notices decaying.
//!
//! # Regenerating baselines
//!
//! ```text
//! UPDATE_SNAPSHOTS=1 cargo test -p scrozz-ui --test golden
//! ```
//!
//! Then look at the diff. Reviewing a regenerated baseline is not optional; it
//! is the entire review.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::path::{Path, PathBuf};

use scrozz_ui::harness::{
    default_snapshot_dir, diff, docs_plan, golden_plan, store_plan, Background, GoldenOutcome,
    GoldenStore, Image, Profile, RenderSpec, Rng, Scenario, SceneRegistry, SequenceSpec,
    SoftwareRenderer, StoreManifest, Tolerance, VirtualClock, DEFAULT_SEED,
};

// ---------------------------------------------------------------------------
// Scratch space
// ---------------------------------------------------------------------------

/// A scratch directory beneath the crate, removed on the way in and on the way
/// out.
///
/// Not the system temp directory: these tests write PNGs that a human is
/// expected to open when something fails, and a path under the crate is one a
/// person can find. The failure artefacts from the real golden run land in
/// `snapshots/failures/`, which is the same idea.
struct Scratch {
    dir: PathBuf,
}

impl Scratch {
    fn new(name: &str) -> Self {
        let dir = default_snapshot_dir().join(".scratch").join(name);
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("create scratch dir");
        Self { dir }
    }

    fn path(&self) -> &Path {
        &self.dir
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        // Left behind on panic would be tidier for debugging, but these are
        // reproducible in one command and stale directories under a committed
        // tree are how junk gets committed.
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

/// Asserts two images are pixel-identical, and says something useful when they
/// are not.
///
/// `assert_eq!(a.as_rgba(), b.as_rgba())` would be shorter, and on failure it
/// prints tens of megabytes of integers. That is the "bytes differ" report this
/// whole module exists to abolish, so the tests do not get to use it either.
#[track_caller]
fn assert_same_pixels(a: &Image, b: &Image, what: &str) {
    if a.as_rgba() == b.as_rgba() {
        return;
    }
    let report = diff(a, b);
    panic!(
        "{what}\n{}\n  left  fingerprint {}\n  right fingerprint {}",
        report.summary(),
        a.fingerprint(),
        b.fingerprint()
    );
}

/// Asserts two images differ, and says so without dumping either.
#[track_caller]
fn assert_different_pixels(a: &Image, b: &Image, what: &str) {
    assert!(
        a.as_rgba() != b.as_rgba(),
        "{what}\n  both rendered to fingerprint {}",
        a.fingerprint()
    );
}

/// The renderer under test.
///
/// Placeholders are permitted here: until the real scenes land, the corpus is
/// exercising the harness rather than the UI, and a corpus that refuses to run
/// until the UI is finished is a corpus that gets written after the UI, which
/// is the wrong order.
fn renderer() -> SoftwareRenderer {
    SoftwareRenderer::production()
}

// ---------------------------------------------------------------------------
// 1. Determinism
// ---------------------------------------------------------------------------

/// The load-bearing test. Same scenario, same instant, byte-identical output.
#[test]
fn same_instant_renders_byte_identically() {
    let r = renderer();
    let spec = RenderSpec::golden(Scenario::StackFull, VirtualClock::from_millis(0));

    let a = r.render(&spec).expect("first render");
    let b = r.render(&spec).expect("second render");

    assert_same_pixels(&a, &b, "two renders of the same instant differed");
}

/// Determinism must survive a fresh renderer, not just a warm one.
///
/// A cached font atlas or a memoised layout that happened to make the second
/// render match would hide exactly the class of bug this suite exists to catch,
/// so the second render here starts from nothing.
#[test]
fn a_fresh_renderer_produces_the_same_pixels() {
    let spec = RenderSpec::golden(Scenario::StackEntering, VirtualClock::from_millis(180));

    let a = renderer().render(&spec).expect("first renderer");
    let b = renderer().render(&spec).expect("second renderer");

    assert_same_pixels(
        &a,
        &b,
        "a fresh renderer disagreed with a warm one; something is leaking state \
         between renders",
    );
}

/// Every scenario, twice. Slower, but the one above only proves it for one.
#[test]
fn every_scenario_is_deterministic() {
    let r = renderer();
    for &scenario in Scenario::all() {
        let spec = RenderSpec::golden(scenario, VirtualClock::from_millis(120));
        let a = r.render(&spec).expect("render a");
        let b = r.render(&spec).expect("render b");
        assert_eq!(
            a.fingerprint(),
            b.fingerprint(),
            "scenario `{}` is not deterministic",
            scenario.slug()
        );
    }
}

/// The seed has to actually reach the scene, or "seeded" is decoration.
#[test]
fn the_seed_is_wired_through() {
    let r = renderer();
    let base = RenderSpec::golden(Scenario::StackFull, VirtualClock::ZERO);
    let other = base.clone().with_seed(DEFAULT_SEED ^ 0x9E37_79B9);

    let a = r.render(&base).expect("default seed");
    let b = r.render(&other).expect("other seed");

    assert_ne!(
        a.fingerprint(),
        b.fingerprint(),
        "changing the seed changed nothing, so scene content is not actually seeded"
    );

    // ...and the same seed still lands in the same place.
    let c = r.render(&other).expect("other seed again");
    assert_same_pixels(&b, &c, "a seeded render is not reproducible");
}

/// The RNG itself, independent of rendering.
#[test]
fn the_rng_is_a_pure_function_of_its_seed() {
    let mut a = Rng::new(1234);
    let mut b = Rng::new(1234);
    let mut c = Rng::new(1235);

    let va: Vec<u64> = (0..16).map(|_| a.next_u64()).collect();
    let vb: Vec<u64> = (0..16).map(|_| b.next_u64()).collect();
    let vc: Vec<u64> = (0..16).map(|_| c.next_u64()).collect();

    assert_eq!(va, vb, "same seed produced a different stream");
    assert_ne!(va, vc, "different seeds produced the same stream");

    let mut d = Rng::new(7);
    for _ in 0..4096 {
        let f = d.next_f32();
        assert!((0.0..1.0).contains(&f), "next_f32 escaped [0, 1): {f}");
    }
}

// ---------------------------------------------------------------------------
// 2. The virtual clock
// ---------------------------------------------------------------------------

/// Two instants, two pictures. Without this, the clock is a parameter that
/// nothing reads and every "at t=180ms" golden is a lie.
#[test]
fn different_instants_render_differently() {
    let r = renderer();
    let early = RenderSpec::golden(Scenario::StackEntering, VirtualClock::from_millis(0));
    let late = RenderSpec::golden(Scenario::StackEntering, VirtualClock::from_millis(180));

    let a = r.render(&early).expect("t=0");
    let b = r.render(&late).expect("t=180");

    assert_ne!(
        a.fingerprint(),
        b.fingerprint(),
        "t=0 and t=180ms produced the same picture; the virtual clock is not \
         reaching the scene"
    );
}

/// An animation that has finished should stop changing.
///
/// This is what makes a "settled" golden meaningful: if the picture still drifts
/// after the animation's stated duration, then the duration is wrong or the
/// motion is unbounded, and either way the baseline is arbitrary.
#[test]
fn motion_settles_and_then_holds_still() {
    let r = renderer();
    let a = r
        .render(&RenderSpec::golden(
            Scenario::StackEntering,
            VirtualClock::from_millis(2_000),
        ))
        .expect("t=2s");
    let b = r
        .render(&RenderSpec::golden(
            Scenario::StackEntering,
            VirtualClock::from_millis(5_000),
        ))
        .expect("t=5s");

    assert_same_pixels(
        &a,
        &b,
        "the scene was still moving long after its animation should have ended",
    );
}

/// D13: reduce-motion collapses every duration to zero, so the first frame is
/// already the settled frame.
#[test]
fn reduce_motion_starts_settled() {
    let r = renderer();
    let first = r
        .render(
            &RenderSpec::golden(Scenario::StackEntering, VirtualClock::ZERO)
                .with_reduce_motion(true),
        )
        .expect("reduce-motion t=0");
    let later = r
        .render(
            &RenderSpec::golden(Scenario::StackEntering, VirtualClock::from_millis(4_000))
                .with_reduce_motion(true),
        )
        .expect("reduce-motion t=4s");

    assert_same_pixels(
        &first,
        &later,
        "with reduce-motion on, the first frame differed from a much later one, \
         so something is still animating",
    );
}

/// Stepping the clock is how animated store and README assets get produced, so
/// the frame run has to be reproducible and actually contain motion.
#[test]
fn a_frame_sequence_is_reproducible_and_moves() {
    let r = renderer();
    let spec = RenderSpec::docs(Scenario::StackEntering, VirtualClock::ZERO);
    let seq = SequenceSpec {
        start_ms: 0,
        end_ms: 240,
        step_ms: 60,
    };

    let a = r.render_sequence(&spec, seq).expect("sequence a");
    let b = r.render_sequence(&spec, seq).expect("sequence b");

    assert_eq!(a.len(), seq.clocks().len(), "wrong number of frames");
    assert!(a.len() >= 4, "sequence too short to prove anything");

    for (i, (fa, fb)) in a.iter().zip(b.iter()).enumerate() {
        assert_same_pixels(
            &fa.1,
            &fb.1,
            &format!("frame {i} differed between two runs of the same sequence"),
        );
    }

    assert_different_pixels(
        &a.first().unwrap().1,
        &a.last().unwrap().1,
        "the first and last frames of an animated sequence are identical",
    );

    let cmd = seq.ffmpeg_command(Path::new("assets/docs/entry"));
    assert!(cmd.contains("ffmpeg"), "no encode command was offered");
    assert!(
        cmd.contains(&format!("{}", 1000 / seq.step_ms.max(1))),
        "the encode command does not carry the sequence frame rate: {cmd}"
    );
}

/// Every key instant a fixture names must be renderable, and the names must be
/// unique within a scenario, because they become file names.
#[test]
fn key_instants_are_renderable_and_uniquely_named() {
    let r = renderer();
    for &scenario in Scenario::all() {
        let frames = r
            .render_key_instants(&RenderSpec::golden(scenario, VirtualClock::ZERO))
            .unwrap_or_else(|e| panic!("key instants for `{}`: {e}", scenario.slug()));
        assert!(
            !frames.is_empty(),
            "scenario `{}` names no key instants, so it has no golden",
            scenario.slug()
        );

        let mut names: Vec<&str> = frames.iter().map(|(name, _, _)| *name).collect();
        let before = names.len();
        names.sort_unstable();
        names.dedup();
        assert_eq!(
            names.len(),
            before,
            "scenario `{}` has duplicate key-instant names",
            scenario.slug()
        );
    }
}

// ---------------------------------------------------------------------------
// 3. The diff reporter
// ---------------------------------------------------------------------------

/// Identical images must report identical. If the reporter has a false positive
/// the corpus is noise; if it has a false negative the corpus is decoration.
#[test]
fn the_reporter_agrees_with_itself() {
    let r = renderer();
    let spec = RenderSpec::golden(Scenario::DockCollapsed, VirtualClock::ZERO);
    let a = r.render(&spec).expect("a");
    let b = r.render(&spec).expect("b");

    let report = diff(&a, &b);
    assert!(report.is_identical(), "identical images reported as different");
    assert!(report.passes(Tolerance::EXACT), "identical images failed EXACT");
    assert_eq!(report.changed_pixels, 0);
}

/// One changed pixel has to be caught. Sub-pixel drift is the failure mode that
/// gets waved through, so the default tolerance is exact and this proves it.
#[test]
fn a_single_changed_pixel_is_caught() {
    let r = renderer();
    let a = r
        .render(&RenderSpec::golden(Scenario::StackFull, VirtualClock::ZERO))
        .expect("render");
    let mut b = a.clone();

    let (x, y) = (a.width() / 2, a.height() / 2);
    let mut px = b.pixel(x, y);
    px[0] = px[0].wrapping_add(1);
    b.set_pixel(x, y, px);

    let report = diff(&a, &b);
    assert_eq!(report.changed_pixels, 1, "one changed pixel was miscounted");
    assert!(!report.passes(Tolerance::EXACT), "EXACT let a changed pixel through");
    assert_eq!(
        report.bounding_box,
        Some((x, y, 1, 1)),
        "the bounding box did not point at the changed pixel"
    );
    assert!(
        report.summary().contains("1"),
        "the summary does not mention the change: {}",
        report.summary()
    );
}

/// The whole point of D25's tooling clause: a failure has to produce something
/// a person can look at, not the string "bytes differ".
#[test]
fn a_mismatch_writes_a_legible_triptych() {
    let scratch = Scratch::new("triptych");
    let r = renderer();

    // `with_update(false)` explicitly: `GoldenStore::new` picks up
    // `UPDATE_SNAPSHOTS` from the environment, and this test's whole job is to
    // observe a mismatch. Without the override, anyone re-baselining the real
    // corpus turns this test into a no-op that still reports "ok" — a test
    // whose result depends on an unrelated ambient variable.
    let store = GoldenStore::new(scratch.path().join("golden"))
        .with_failures_dir(scratch.path().join("failures"))
        .with_update(false);

    let early = r
        .render(&RenderSpec::golden(
            Scenario::StackEntering,
            VirtualClock::from_millis(0),
        ))
        .expect("early");
    let late = r
        .render(&RenderSpec::golden(
            Scenario::StackEntering,
            VirtualClock::from_millis(180),
        ))
        .expect("late");

    // Seed a baseline, then hand it a genuinely different picture.
    match store.compare("triptych-case", &early).expect("create") {
        GoldenOutcome::Created(p) => assert!(p.exists(), "baseline was not written"),
        other => panic!("expected a created baseline, got {other:?}"),
    }

    let outcome = store.compare("triptych-case", &late).expect("compare");
    let GoldenOutcome::Mismatched(failure) = outcome else {
        panic!("a different picture did not fail the comparison");
    };

    for path in [
        &failure.expected_path,
        &failure.actual_path,
        &failure.diff_path,
        &failure.triptych_path,
    ] {
        assert!(path.exists(), "failure artefact missing: {}", path.display());
    }

    let triptych = Image::read_png(&failure.triptych_path).expect("read triptych");
    assert!(
        triptych.width() >= early.width() * 3,
        "the comparison image is not three panels wide ({} vs 3 x {})",
        triptych.width(),
        early.width()
    );
    assert!(
        triptych.height() > early.height(),
        "the comparison image has no room for labels"
    );

    let text = failure.to_string();
    for needle in ["golden mismatch", "open this", "UPDATE_SNAPSHOTS"] {
        assert!(
            text.contains(needle),
            "the failure message omits `{needle}`:\n{text}"
        );
    }
}

/// Differently sized images must not panic, and must not silently pass.
#[test]
fn mismatched_sizes_fail_rather_than_panic() {
    let a = Image::transparent(10, 10);
    let b = Image::transparent(12, 10);
    let report = diff(&a, &b);
    assert!(!report.is_identical(), "different sizes reported as identical");
    assert!(!report.passes(Tolerance::EXACT));
}

// ---------------------------------------------------------------------------
// 4. Store targets as data
// ---------------------------------------------------------------------------

/// The manifest is the only place a store dimension appears, so it had better
/// parse and had better be sane.
#[test]
fn the_store_manifest_is_sane() {
    let manifest = StoreManifest::embedded().expect("parse embedded manifest");
    assert!(!manifest.targets.is_empty(), "no store targets");

    let mut ids: Vec<&str> = manifest.targets.iter().map(|t| t.id.as_str()).collect();
    let before = ids.len();
    ids.sort_unstable();
    ids.dedup();
    assert_eq!(ids.len(), before, "duplicate store target ids");

    for t in &manifest.targets {
        assert!(t.width > 0 && t.height > 0, "{}: zero-sized", t.id);
        assert!(t.scale > 0.0, "{}: non-positive scale", t.id);
        assert!(
            t.is_whole(),
            "{}: {}x{} at {}x is not a whole number of points, which means the \
             surface cannot be laid out exactly and the asset will be resampled",
            t.id,
            t.width,
            t.height,
            t.scale
        );
        assert!(!t.locales.is_empty(), "{}: no locales", t.id);
    }

    let stores = manifest.by_store();
    // D25 names the four consumers this harness must serve. Renaming one here
    // is a deliberate act; failing this test is how it stays deliberate.
    for expected in [
        "Mac App Store",
        "Microsoft Store",
        "Flathub",
        "GitHub README",
    ] {
        assert!(
            stores.contains_key(expected),
            "the manifest does not cover `{expected}`; known: {:?}",
            stores.keys().collect::<Vec<_>>()
        );
    }

    assert!(
        !manifest.required().is_empty(),
        "no target is marked required, so `--required-only` would produce nothing"
    );
}

/// Adding a store must not need a code change: a target loaded from a file has
/// to behave exactly like one compiled in.
#[test]
fn a_store_can_be_added_without_touching_code() {
    let scratch = Scratch::new("manifest");
    let path = scratch.path().join("extra.toml");
    std::fs::write(
        &path,
        r#"
[[target]]
id = "invented-store-1024"
store = "invented-store"
label = "Invented Store"
width = 1024
height = 768
scale = 1.0
locales = ["en-US"]
required = true
notes = "Added by a test, purely to prove that TOML is enough."
"#,
    )
    .expect("write manifest");

    let manifest = StoreManifest::load(&path).expect("load manifest");
    let target = manifest.target("invented-store-1024").expect("find target");
    assert_eq!(target.size_pt(), (1024.0, 768.0));

    let cases = store_plan(&manifest, None, true).expect("plan");
    assert!(
        !cases.is_empty(),
        "a required target produced no store assets"
    );
    for case in &cases {
        assert_eq!(case.target_id, "invented-store-1024");
    }
}

/// Every store target must resolve to exactly the declared pixel size, and must
/// actually render at it. A store rejects an upload for being one pixel off, and
/// finding that out at upload time is finding it out too late.
#[test]
fn store_renders_hit_the_declared_pixel_size() {
    let manifest = StoreManifest::embedded().expect("manifest");
    let r = SoftwareRenderer::new(SceneRegistry::placeholders());

    for target in manifest.targets.iter() {
        let store_spec =
            RenderSpec::store(Scenario::StackFull, VirtualClock::ZERO, target, "en-US");
        let resolved = store_spec
            .resolved_size_px()
            .unwrap_or_else(|e| panic!("{}: {e}", target.id));
        assert_eq!(
            resolved,
            (target.width, target.height),
            "{}: the resolved pixel size does not match the manifest",
            target.id
        );

        // The store profile refuses placeholders by design, so the geometry is
        // exercised through the docs profile at the same size and scale.
        let image = r
            .render(
                &RenderSpec::docs(Scenario::StackFull, VirtualClock::ZERO)
                    .with_scale(target.scale)
                    .with_background(Background::Studio)
                    .with_size_pt(store_spec.resolved_size_pt()),
            )
            .unwrap_or_else(|e| panic!("{}: {e}", target.id));

        assert_eq!(
            (image.width(), image.height()),
            (target.width, target.height),
            "{}: rendered {}x{}, manifest says {}x{}",
            target.id,
            image.width(),
            image.height(),
            target.width,
            target.height
        );
    }
}

/// A watermarked stand-in reaching a store listing is the single failure this
/// module exists to prevent, so it is enforced in code.
#[test]
fn the_store_profile_refuses_a_placeholder() {
    let manifest = StoreManifest::embedded().expect("manifest");
    let target = manifest.required().first().copied().expect("a target");
    let r = SoftwareRenderer::new(SceneRegistry::placeholders());

    let spec = RenderSpec::store(Scenario::StackFull, VirtualClock::ZERO, target, "en-US");
    assert!(!spec.profile.allows_placeholders());

    let err = r
        .render(&spec)
        .expect_err("a placeholder was rendered into a store asset");
    let text = err.to_string();
    assert!(
        text.contains("placeholder") || text.contains("Placeholder"),
        "the refusal does not say why: {text}"
    );
}

// ---------------------------------------------------------------------------
// 5. The corpus itself
// ---------------------------------------------------------------------------

/// One list, used by the tests and by the asset generator, so they cannot
/// disagree about what a scenario looks like.
#[test]
fn the_plans_are_well_formed() {
    let golden = golden_plan();
    assert!(!golden.is_empty(), "the golden corpus is empty");

    let mut names: Vec<&str> = golden.iter().map(|c| c.name.as_str()).collect();
    let before = names.len();
    names.sort_unstable();
    names.dedup();
    assert_eq!(names.len(), before, "duplicate golden baseline names");

    for case in &golden {
        assert!(
            !case.expectation.trim().is_empty(),
            "`{}` has no stated expectation, so a failure would tell nobody what \
             to look for",
            case.name
        );
        assert!(
            matches!(case.spec.profile, Profile::Golden),
            "`{}` is not a golden-profile render",
            case.name
        );
        assert!(
            !case.name.contains(std::path::MAIN_SEPARATOR),
            "`{}` would escape the snapshot directory",
            case.name
        );
    }

    // Every scenario must appear, or a scenario exists that nothing checks.
    for &scenario in Scenario::all() {
        assert!(
            golden.iter().any(|c| c.name.starts_with(scenario.slug())),
            "scenario `{}` has no golden baseline",
            scenario.slug()
        );
        assert_eq!(
            Scenario::from_slug(scenario.slug()).expect("round trip"),
            scenario,
            "slug round-trip failed for `{}`",
            scenario.slug()
        );
    }

    let docs = docs_plan();
    assert!(!docs.is_empty(), "no documentation assets are planned");
    for case in &docs {
        assert!(
            !case.alt.trim().is_empty(),
            "`{}` has no alt text",
            case.relative_path.display()
        );
    }
    assert!(
        docs.iter().any(|c| c.sequence.is_some()),
        "no animated documentation asset is planned, but the app is motion-heavy"
    );
}

/// The one that fails when somebody moves a rectangle.
///
/// On a first run this writes every baseline and passes; that is deliberate, so
/// a fresh clone is not blocked by files it has no way to produce. The baselines
/// are committed, so on every subsequent run this is a real comparison.
#[test]
fn golden_corpus_matches_baselines() {
    let r = renderer();
    let store = GoldenStore::new(default_snapshot_dir().join("golden"))
        .with_failures_dir(default_snapshot_dir().join("failures"));

    let failures = r.check_goldens(&store).expect("run the corpus");
    if failures.is_empty() {
        return;
    }

    let mut message = format!(
        "{} golden baseline(s) changed.\n\n\
         Open the `.compare.png` for each: left is the committed baseline, \
         middle is what was rendered now, right is the difference.\n\n",
        failures.len()
    );
    for f in &failures {
        message.push_str(&f.to_string());
        message.push('\n');
    }
    panic!("{message}");
}
