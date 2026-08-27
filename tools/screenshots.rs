//! `screenshots` — the command a human or an agent runs to regenerate every
//! picture Scrozz ships.
//!
//! D25: no Scrozz screenshot is ever taken by hand. This is the hand.
//!
//! ```text
//! cargo run --bin screenshots -- --profile store
//! cargo run --bin screenshots -- --profile docs   --out assets/docs
//! cargo run --bin screenshots -- --profile golden --update
//! cargo run --bin screenshots -- --list
//! ```
//!
//! # Why the arguments are parsed by hand
//!
//! `scrozz-ui` does not depend on `clap`, and pulling a 250 kLOC argument parser
//! into a shipping GUI crate so that a maintenance binary can have `--help` is a
//! poor trade. The grammar here is a dozen flags with no subcommands, no
//! abbreviations and no clustering, and an unknown flag is a hard error rather
//! than a shrug — which is the part that actually matters, because a typo'd
//! `--profile stroe` that silently regenerated the golden corpus would be worse
//! than no CLI at all.
//!
//! # This binary never opens a window
//!
//! Everything renders through the software rasteriser in
//! [`scrozz_ui::harness`]. There is no event loop, no surface, no GPU and no
//! display server. That is not an implementation detail — it is the reason the
//! harness exists, and it is what lets this run in CI and under an agent.

#![forbid(unsafe_code)]
#![allow(clippy::print_stdout, clippy::print_stderr, clippy::exit)]

use std::path::{Path, PathBuf};
use std::process::ExitCode;

use scrozz_ui::harness::{
    default_snapshot_dir, docs_plan, golden_plan, store_plan, Background, GeneratedBatch,
    GoldenStore, Profile, RenderSpec, Scenario, SceneRegistry, SequenceSpec, SoftwareRenderer,
    StoreManifest, Tolerance, VirtualClock,
};

const USAGE: &str = "\
screenshots — generate every Scrozz screenshot (D25)

USAGE:
    screenshots [OPTIONS]

PROFILES:
    --profile golden          Check the committed baselines (default).
    --profile store           Store listing assets, at exact per-store sizes.
    --profile docs            README and documentation stills and frame runs.

SELECTING WHAT TO RENDER:
    --scenario <slug|all>     One scenario, or all of them. Default: all.
    --store <id>              One store target from the manifest.
    --required-only           Only targets marked `required` in the manifest.
    --at <ms>                 Render one named instant, in virtual milliseconds.
    --sequence <a>..<b>@<s>   Render a frame run instead of a still.

OUTPUT:
    --out <dir>               Where to write. Defaults per profile.
    --stores <file>           Store manifest. Defaults to the embedded one.
    --locale <tag>            BCP-47 locale. Default: the target's first.
    --scale <n>               Override pixels-per-point.
    --theme <light|dark>      Override the theme.
    --reduce-motion           Render as if the OS reduce-motion switch is on.
    --seed <n>                Override the fixture seed.

BEHAVIOUR:
    --update                  Rewrite golden baselines instead of failing.
    --tolerance <exact|grudging>
                              Comparison strictness. Default: exact.
    --allow-placeholders      Permit watermarked stand-ins. Refused for stores.
    --list                    List scenarios, instants and store targets.
    --dry-run                 Say what would be written; write nothing.
    -q, --quiet               Only report failures.
    -h, --help                This.

EXAMPLES:
    screenshots --list
    screenshots --profile store --required-only
    screenshots --profile docs --scenario stack-entering --sequence 0..480@33
    UPDATE_SNAPSHOTS=1 cargo test -p scrozz-ui --test golden
";

fn main() -> ExitCode {
    match Args::parse(std::env::args().skip(1)) {
        Ok(None) => ExitCode::SUCCESS,
        Ok(Some(args)) => match run(&args) {
            Ok(code) => code,
            Err(e) => {
                eprintln!("error: {e}");
                ExitCode::FAILURE
            }
        },
        Err(e) => {
            eprintln!("error: {e}\n\nTry `--help`.");
            ExitCode::from(2)
        }
    }
}

// ---------------------------------------------------------------------------
// Arguments
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ProfileKind {
    Golden,
    Store,
    Docs,
}

impl ProfileKind {
    fn parse(s: &str) -> Result<Self, String> {
        match s {
            "golden" => Ok(Self::Golden),
            "store" => Ok(Self::Store),
            "docs" | "readme" => Ok(Self::Docs),
            other => Err(format!(
                "unknown profile `{other}`; expected `golden`, `store` or `docs`"
            )),
        }
    }
}

#[derive(Debug)]
struct Args {
    profile: ProfileKind,
    scenario: Option<Scenario>,
    store_id: Option<String>,
    required_only: bool,
    at_ms: Option<u64>,
    sequence: Option<SequenceSpec>,
    out: Option<PathBuf>,
    stores: Option<PathBuf>,
    locale: Option<String>,
    scale: Option<f32>,
    theme: Option<egui::Theme>,
    reduce_motion: bool,
    seed: Option<u64>,
    update: bool,
    tolerance: Tolerance,
    allow_placeholders: bool,
    list: bool,
    dry_run: bool,
    quiet: bool,
}

impl Args {
    /// `Ok(None)` means the work is done — `--help` printed and nothing to run.
    fn parse(argv: impl Iterator<Item = String>) -> Result<Option<Self>, String> {
        let mut a = Self {
            profile: ProfileKind::Golden,
            scenario: None,
            store_id: None,
            required_only: false,
            at_ms: None,
            sequence: None,
            out: None,
            stores: None,
            locale: None,
            scale: None,
            theme: None,
            reduce_motion: false,
            seed: None,
            update: false,
            tolerance: Tolerance::EXACT,
            allow_placeholders: false,
            list: false,
            dry_run: false,
            quiet: false,
        };

        let argv: Vec<String> = argv.collect();
        let mut i = 0;
        // A flag's value, or a diagnosis of why there isn't one. `--out` with no
        // path silently defaulting is how a generator writes a hundred files into
        // the wrong directory.
        let value = |i: &mut usize, flag: &str| -> Result<String, String> {
            *i += 1;
            argv.get(*i)
                .cloned()
                .ok_or_else(|| format!("`{flag}` needs a value"))
        };

        while i < argv.len() {
            let arg = argv[i].clone();
            match arg.as_str() {
                "-h" | "--help" => {
                    print!("{USAGE}");
                    return Ok(None);
                }
                "--profile" => a.profile = ProfileKind::parse(&value(&mut i, "--profile")?)?,
                "--scenario" => {
                    let v = value(&mut i, "--scenario")?;
                    a.scenario = if v == "all" {
                        None
                    } else {
                        Some(Scenario::from_slug(&v).map_err(|e| e.to_string())?)
                    };
                }
                "--store" => a.store_id = Some(value(&mut i, "--store")?),
                "--required-only" => a.required_only = true,
                "--at" => {
                    let v = value(&mut i, "--at")?;
                    a.at_ms = Some(v.parse().map_err(|_| format!("`--at {v}` is not a whole number of milliseconds"))?);
                }
                "--sequence" => a.sequence = Some(parse_sequence(&value(&mut i, "--sequence")?)?),
                "--out" => a.out = Some(PathBuf::from(value(&mut i, "--out")?)),
                "--stores" => a.stores = Some(PathBuf::from(value(&mut i, "--stores")?)),
                "--locale" => a.locale = Some(value(&mut i, "--locale")?),
                "--scale" => {
                    let v = value(&mut i, "--scale")?;
                    let n: f32 = v.parse().map_err(|_| format!("`--scale {v}` is not a number"))?;
                    if !(n.is_finite() && n > 0.0) {
                        return Err(format!("`--scale {v}` must be positive and finite"));
                    }
                    a.scale = Some(n);
                }
                "--theme" => {
                    let v = value(&mut i, "--theme")?;
                    a.theme = Some(match v.as_str() {
                        "light" => egui::Theme::Light,
                        "dark" => egui::Theme::Dark,
                        other => return Err(format!("unknown theme `{other}`")),
                    });
                }
                "--reduce-motion" => a.reduce_motion = true,
                "--seed" => {
                    let v = value(&mut i, "--seed")?;
                    a.seed = Some(v.parse().map_err(|_| format!("`--seed {v}` is not a number"))?);
                }
                "--update" => a.update = true,
                "--tolerance" => {
                    let v = value(&mut i, "--tolerance")?;
                    a.tolerance = match v.as_str() {
                        "exact" => Tolerance::EXACT,
                        "grudging" => Tolerance::GRUDGING,
                        other => {
                            return Err(format!(
                                "unknown tolerance `{other}`; expected `exact` or `grudging`"
                            ))
                        }
                    };
                }
                "--allow-placeholders" => a.allow_placeholders = true,
                "--list" => a.list = true,
                "--dry-run" => a.dry_run = true,
                "-q" | "--quiet" => a.quiet = true,
                other => {
                    return Err(format!("unrecognised argument `{other}`"));
                }
            }
            i += 1;
        }

        if a.profile == ProfileKind::Store && a.allow_placeholders {
            return Err(
                "`--allow-placeholders` cannot be used with `--profile store`: a \
                 watermarked stand-in must never reach a store listing"
                    .to_owned(),
            );
        }

        Ok(Some(a))
    }

    fn out_dir(&self) -> PathBuf {
        self.out.clone().unwrap_or_else(|| match self.profile {
            ProfileKind::Golden => default_snapshot_dir().join("golden"),
            ProfileKind::Store => repo_root().join("assets/store"),
            ProfileKind::Docs => repo_root().join("assets/docs"),
        })
    }
}

/// `0..480@33`
fn parse_sequence(s: &str) -> Result<SequenceSpec, String> {
    let bad = || {
        format!(
            "`--sequence {s}` is malformed; expected `<start>..<end>@<step>`, \
             for example `0..480@33`"
        )
    };
    let (range, step) = s.split_once('@').ok_or_else(bad)?;
    let (start, end) = range.split_once("..").ok_or_else(bad)?;
    let start: u64 = start.trim().parse().map_err(|_| bad())?;
    let end: u64 = end.trim().parse().map_err(|_| bad())?;
    let step: u64 = step.trim().parse().map_err(|_| bad())?;
    if step == 0 {
        return Err("`--sequence` step must be greater than zero".to_owned());
    }
    if end < start {
        return Err("`--sequence` end must not precede start".to_owned());
    }
    Ok(SequenceSpec {
        start_ms: start,
        end_ms: end,
        step_ms: step,
    })
}

/// The repository root, derived from the crate rather than the shell's working
/// directory, so `cargo run` from anywhere writes to the same place.
fn repo_root() -> PathBuf {
    // <repo>/crates/scrozz-ui
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .ancestors()
        .nth(2)
        .map_or_else(|| PathBuf::from("."), Path::to_path_buf)
}

// ---------------------------------------------------------------------------
// Running
// ---------------------------------------------------------------------------

fn run(args: &Args) -> Result<ExitCode, Box<dyn std::error::Error>> {
    let manifest = match &args.stores {
        Some(p) => StoreManifest::load(p)?,
        None => StoreManifest::embedded()?,
    };

    if args.list {
        list(&manifest);
        return Ok(ExitCode::SUCCESS);
    }

    let renderer = if args.allow_placeholders {
        SoftwareRenderer::new(SceneRegistry::placeholders())
    } else {
        SoftwareRenderer::production()
    };

    let placeholders = renderer.registry().placeholder_scenarios();
    if !placeholders.is_empty() && !args.quiet {
        eprintln!(
            "note: {} scenario(s) are still watermarked placeholders: {}",
            placeholders.len(),
            placeholders
                .iter()
                .map(|s| s.slug())
                .collect::<Vec<_>>()
                .join(", ")
        );
    }

    match args.profile {
        ProfileKind::Golden => run_golden(args, &renderer),
        ProfileKind::Store => run_store(args, &renderer, &manifest),
        ProfileKind::Docs => run_docs(args, &renderer),
    }
}

fn run_golden(
    args: &Args,
    renderer: &SoftwareRenderer,
) -> Result<ExitCode, Box<dyn std::error::Error>> {
    let dir = args.out_dir();
    let store = GoldenStore::new(&dir)
        .with_failures_dir(default_snapshot_dir().join("failures"))
        .with_tolerance(args.tolerance)
        .with_update(args.update);

    let cases: Vec<_> = golden_plan()
        .into_iter()
        .filter(|c| args.scenario.is_none_or(|s| c.spec.scenario == s))
        .collect();

    if cases.is_empty() {
        return Err("no golden cases matched".into());
    }

    if args.dry_run {
        for case in &cases {
            println!("{}", store.path_for(&case.name).display());
        }
        return Ok(ExitCode::SUCCESS);
    }

    if args.update {
        let batch = renderer.generate_goldens(&store)?;
        report(args, &batch, &dir)?;
        if !args.quiet {
            println!(
                "\nRegenerated {} baseline(s). Review the diff before committing: a \
                 baseline accepted without being looked at is a regression that has \
                 been promoted to a specification.",
                batch.assets.len()
            );
        }
        return Ok(ExitCode::SUCCESS);
    }

    let mut failures = 0usize;
    let mut created = 0usize;
    for case in &cases {
        let image = renderer.render_case(case)?;
        match store.compare(&case.name, &image)? {
            scrozz_ui::harness::GoldenOutcome::Matched => {}
            scrozz_ui::harness::GoldenOutcome::Created(p) => {
                created += 1;
                if !args.quiet {
                    println!("created  {}", p.display());
                }
            }
            scrozz_ui::harness::GoldenOutcome::Updated(p) => {
                if !args.quiet {
                    println!("updated  {}", p.display());
                }
            }
            scrozz_ui::harness::GoldenOutcome::Mismatched(f) => {
                failures += 1;
                eprintln!("{f}");
            }
        }
    }

    if !args.quiet {
        println!(
            "{} checked, {created} created, {failures} changed",
            cases.len()
        );
    }

    if failures > 0 {
        Ok(ExitCode::FAILURE)
    } else {
        Ok(ExitCode::SUCCESS)
    }
}

fn run_store(
    args: &Args,
    renderer: &SoftwareRenderer,
    manifest: &StoreManifest,
) -> Result<ExitCode, Box<dyn std::error::Error>> {
    let dir = args.out_dir();
    let cases = store_plan(manifest, args.store_id.as_deref(), args.required_only)?;
    if cases.is_empty() {
        return Err(format!(
            "no store targets matched{}",
            args.store_id
                .as_deref()
                .map_or(String::new(), |id| format!(" `{id}`"))
        )
        .into());
    }

    if args.dry_run {
        for case in &cases {
            let (w, h) = case.spec.resolved_size_px()?;
            println!("{}\t{w}x{h}", dir.join(&case.relative_path).display());
        }
        return Ok(ExitCode::SUCCESS);
    }

    let batch = renderer.generate_store(
        manifest,
        &dir,
        args.store_id.as_deref(),
        args.required_only,
    )?;
    report(args, &batch, &dir)?;
    Ok(ExitCode::SUCCESS)
}

fn run_docs(
    args: &Args,
    renderer: &SoftwareRenderer,
) -> Result<ExitCode, Box<dyn std::error::Error>> {
    let dir = args.out_dir();

    // A single explicitly requested still or frame run, rather than the plan.
    if let (Some(scenario), true) = (args.scenario, args.at_ms.is_some() || args.sequence.is_some())
    {
        let mut spec = RenderSpec::docs(
            scenario,
            VirtualClock::from_millis(args.at_ms.unwrap_or(0)),
        )
        .with_reduce_motion(args.reduce_motion);
        if let Some(scale) = args.scale {
            spec = spec.with_scale(scale);
        }
        if let Some(theme) = args.theme {
            spec = spec.with_theme(theme);
        }
        if let Some(seed) = args.seed {
            spec = spec.with_seed(seed);
        }

        std::fs::create_dir_all(&dir)?;
        if let Some(seq) = args.sequence {
            let frames_dir = dir.join(format!("{}-frames", scenario.slug()));
            std::fs::create_dir_all(&frames_dir)?;
            let frames = renderer.render_sequence(&spec, seq)?;
            for (i, (clock, image)) in frames.iter().enumerate() {
                let path = frames_dir.join(format!("frame-{i:04}.png"));
                if args.dry_run {
                    println!("{}\t t={}ms", path.display(), clock.as_millis());
                } else {
                    image.write_png(&path)?;
                    // No `index.txt` here: this is an ad-hoc request, and an
                    // index naming one asset would clobber the complete one a
                    // full `--profile docs` run writes. Fingerprints still go
                    // to stdout so the frames are reviewable without opening
                    // thirty pictures.
                    if !args.quiet {
                        println!(
                            "{}\tt={}ms\t{}",
                            path.display(),
                            clock.as_millis(),
                            image.fingerprint()
                        );
                    }
                }
            }
            if !args.quiet {
                println!(
                    "{} frame(s) in {}\n\nEncode with:\n  {}",
                    frames.len(),
                    frames_dir.display(),
                    seq.ffmpeg_command(&frames_dir)
                );
            }
        } else {
            let image = renderer.render(&spec)?;
            let path = dir.join(format!("{}.png", spec.output_name()));
            if args.dry_run {
                println!("{}", path.display());
            } else {
                image.write_png(&path)?;
                if !args.quiet {
                    println!(
                        "{}\t{}x{}\t{}",
                        path.display(),
                        image.width(),
                        image.height(),
                        image.fingerprint()
                    );
                }
            }
        }
        return Ok(ExitCode::SUCCESS);
    }

    if args.dry_run {
        for case in docs_plan() {
            println!("{}", dir.join(&case.relative_path).display());
        }
        return Ok(ExitCode::SUCCESS);
    }

    let batch = renderer.generate_docs(&dir)?;
    report(args, &batch, &dir)?;
    Ok(ExitCode::SUCCESS)
}

// ---------------------------------------------------------------------------
// Output
// ---------------------------------------------------------------------------

/// Writes the index and prints the summary.
///
/// The index carries a fingerprint per asset, so `git diff` on that one file
/// says which pictures changed without opening any of them — which is the
/// difference between a reviewable asset commit and an unreviewable one.
fn report(
    args: &Args,
    batch: &GeneratedBatch,
    dir: &Path,
) -> Result<(), Box<dyn std::error::Error>> {
    let index_path = dir.join("index.txt");
    std::fs::create_dir_all(dir)?;
    std::fs::write(&index_path, batch.index())?;

    if args.quiet {
        return Ok(());
    }

    for asset in &batch.assets {
        println!(
            "{}\t{}x{}\t{}",
            asset.path.display(),
            asset.size.0,
            asset.size.1,
            asset.fingerprint
        );
    }
    println!("\n{} asset(s) -> {}", batch.assets.len(), dir.display());
    println!("index: {}", index_path.display());

    if !batch.encode_commands.is_empty() {
        println!(
            "\n{} animated asset(s) were written as numbered frames. Encode them:",
            batch.encode_commands.len()
        );
        for cmd in &batch.encode_commands {
            println!("  {cmd}");
        }
    }
    Ok(())
}

fn list(manifest: &StoreManifest) {
    println!("SCENARIOS");
    for &scenario in Scenario::all() {
        let f = scenario.fixture();
        println!("  {:<26} {}", scenario.slug(), f.title);
        println!("  {:<26} {}", "", f.intent);
        for instant in f.key_instants {
            println!(
                "  {:<26}   t={:>5}ms  {}",
                "", instant.at_ms, instant.expectation
            );
        }
        if let Some(seq) = f.sequence {
            println!(
                "  {:<26}   animated {}..{}ms every {}ms",
                "", seq.start_ms, seq.end_ms, seq.step_ms
            );
        }
        println!();
    }

    println!("STORE TARGETS");
    for (store, targets) in manifest.by_store() {
        println!("  {store}");
        for t in targets {
            println!(
                "    {:<24} {:>5}x{:<5} @{}x  {:<8} {}",
                t.id,
                t.width,
                t.height,
                t.scale,
                if t.required { "required" } else { "optional" },
                t.locales.join(", ")
            );
        }
    }

    println!("\nPROFILES");
    for p in [
        Profile::Golden,
        Profile::Store {
            width: 0,
            height: 0,
        },
        Profile::Docs,
    ] {
        println!(
            "  {:<10} placeholders {}",
            p.slug(),
            if p.allows_placeholders() {
                "allowed"
            } else {
                "refused"
            }
        );
    }

    println!("\nBACKGROUNDS");
    for b in [
        Background::Transparent,
        Background::Solid([0x18, 0x18, 0x1B, 0xFF]),
        Background::Studio,
        Background::Checkerboard,
    ] {
        println!("  {b:?}");
    }
}
