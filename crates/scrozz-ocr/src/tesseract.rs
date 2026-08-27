//! Artifact-local subprocess OCR for portable, unpackaged Windows builds.

use std::{
    collections::BTreeMap,
    ffi::{OsStr, OsString},
    io::{Error as IoError, ErrorKind, Read, Write},
    path::{Path, PathBuf},
    process::{Command, ExitStatus, Stdio},
    thread,
    time::{Duration, Instant},
};

use isolang::Language;
use scrozz_core::{Error, Frame, PhysicalRect, PhysicalSize, Result};

use crate::{Accuracy, Options, TESSERACT_DIRECTORY_ENV, TextBlock, layout, prepare};

const TSV_HEADER: &str =
    "level\tpage_num\tblock_num\tpar_num\tline_num\tword_num\tleft\ttop\twidth\theight\tconf\ttext";
const COMMAND_TIMEOUT: Duration = Duration::from_secs(20);
const WAIT_INTERVAL: Duration = Duration::from_millis(10);
const TESSERACT_EXECUTABLE: &str = "tesseract.exe";
const REQUIRED_ENGLISH_MODEL: &str = "eng.traineddata";

#[derive(Debug)]
struct CommandOutput {
    status: ExitStatus,
    stdout: Vec<u8>,
    stderr: Vec<u8>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct TesseractInstallation {
    program: PathBuf,
    tessdata: PathBuf,
}

/// Recognizes text with the portable artifact's local Tesseract payload.
pub(crate) fn recognize(frame: &Frame, options: &Options) -> Result<Vec<TextBlock>> {
    let prepared = prepare::prepare(frame, options.upscale, None)?;
    let installation = tesseract_installation()?;
    let installed_languages = available_languages(&installation)?;
    let selected_languages = if options.languages.is_empty() {
        resolve_system_languages(&installed_languages)?
    } else {
        resolve_languages(&options.languages, &installed_languages)?
    };
    let arguments = recognition_arguments(&selected_languages, options);

    let output = run_tesseract(
        &installation,
        &arguments,
        Some(pgm(&prepared.image)),
        COMMAND_TIMEOUT,
    )?;
    if !output.status.success() {
        return Err(command_error("recognize text", &output.stderr));
    }

    let tsv = String::from_utf8(output.stdout)
        .map_err(|error| Error::Codec(format!("Tesseract returned non-UTF-8 TSV: {error}")))?;
    parse_tsv(&tsv, prepared.upscale, prepared.source_size, frame)
}

fn recognition_arguments(selected_languages: &[String], options: &Options) -> Vec<OsString> {
    let mut arguments = vec![
        OsString::from("stdin"),
        OsString::from("stdout"),
        OsString::from("-l"),
        OsString::from(selected_languages.join("+")),
    ];
    if options.accuracy == Accuracy::Fast {
        arguments.extend([OsString::from("--psm"), OsString::from("11")]);
    }
    if !options.language_correction {
        arguments.extend([
            OsString::from("-c"),
            OsString::from("load_system_dawg=0"),
            OsString::from("-c"),
            OsString::from("load_freq_dawg=0"),
        ]);
    }
    // Do not use the bare `tsv` config name: that would add an undeclared
    // dependency on `tessdata/configs/tsv` to the portable artifact contract.
    arguments.extend([
        OsString::from("-c"),
        OsString::from("tessedit_create_tsv=1"),
    ]);
    arguments
}

fn available_languages(installation: &TesseractInstallation) -> Result<Vec<String>> {
    let output = run_tesseract(
        installation,
        &[OsString::from("--list-langs")],
        None,
        COMMAND_TIMEOUT,
    )?;
    if !output.status.success() {
        return Err(command_error("list OCR languages", &output.stderr));
    }

    let mut languages: Vec<String> = String::from_utf8_lossy(&output.stdout)
        .lines()
        .map(str::trim)
        .filter(|line| {
            !line.is_empty()
                && !line.starts_with("List of available languages")
                && line
                    .chars()
                    .all(|character| character.is_ascii_alphanumeric() || "_-".contains(character))
        })
        .map(str::to_owned)
        .collect();
    languages.sort_unstable();
    languages.dedup();
    Ok(languages)
}

fn run_tesseract(
    installation: &TesseractInstallation,
    arguments: &[OsString],
    input: Option<Vec<u8>>,
    timeout: Duration,
) -> Result<CommandOutput> {
    let mut configured_arguments = Vec::with_capacity(arguments.len() + 2);
    configured_arguments.extend([
        OsString::from("--tessdata-dir"),
        installation.tessdata.clone().into_os_string(),
    ]);
    configured_arguments.extend_from_slice(arguments);
    run_command(
        installation.program.as_os_str(),
        &configured_arguments,
        input,
        timeout,
    )
}

fn tesseract_installation() -> Result<TesseractInstallation> {
    let executable = std::env::current_exe().map_err(Error::Io)?;
    resolve_tesseract_installation(
        std::env::var_os(TESSERACT_DIRECTORY_ENV).as_deref(),
        &executable,
    )
}

fn resolve_tesseract_installation(
    configured_directory: Option<&OsStr>,
    scrozz_executable: &Path,
) -> Result<TesseractInstallation> {
    let (directory, configured) = match configured_directory {
        Some(directory) => (PathBuf::from(directory), true),
        None => (sibling_tesseract_directory(scrozz_executable)?, false),
    };

    if !directory.is_absolute() {
        return Err(Error::InvalidRequest(format!(
            "{TESSERACT_DIRECTORY_ENV} must be an absolute directory"
        )));
    }

    validate_tesseract_directory(&directory, configured)
}

fn sibling_tesseract_directory(executable: &Path) -> Result<PathBuf> {
    executable
        .parent()
        .map(|parent| parent.join("tesseract"))
        .ok_or_else(|| {
            Error::Platform(format!(
                "cannot resolve the Tesseract payload beside {}",
                executable.display()
            ))
        })
}

fn validate_tesseract_directory(
    directory: &Path,
    explicitly_configured: bool,
) -> Result<TesseractInstallation> {
    let program = directory.join(TESSERACT_EXECUTABLE);
    let tessdata = directory.join("tessdata");
    let english_model = tessdata.join(REQUIRED_ENGLISH_MODEL);
    if !program.is_file() || !tessdata.is_dir() || !english_model.is_file() {
        let source = if explicitly_configured {
            format!("{TESSERACT_DIRECTORY_ENV} must contain")
        } else {
            "the portable artifact must contain".to_string()
        };
        return Err(Error::Unsupported {
            what: if explicitly_configured {
                "text recognition with the configured Tesseract installation".to_string()
            } else {
                "text recognition from the portable Windows artifact".to_string()
            },
            why: format!(
                "{source} `{TESSERACT_EXECUTABLE}`, its dependent DLLs, and \
                 `tessdata/{REQUIRED_ENGLISH_MODEL}`; checked path: {}",
                directory.display()
            ),
        });
    }

    Ok(TesseractInstallation { program, tessdata })
}

fn run_command(
    program: &OsStr,
    arguments: &[OsString],
    input: Option<Vec<u8>>,
    timeout: Duration,
) -> Result<CommandOutput> {
    if !Path::new(program).is_absolute() {
        return Err(Error::InvalidRequest(format!(
            "refusing to resolve Tesseract through PATH; executable must be absolute: {}",
            Path::new(program).display()
        )));
    }

    let mut child = Command::new(program)
        .args(arguments)
        .stdin(if input.is_some() {
            Stdio::piped()
        } else {
            Stdio::null()
        })
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|error| spawn_error(program, error))?;

    let mut writer = input.map(|bytes| {
        let mut stdin = child.stdin.take().expect("piped stdin must be present");
        thread::spawn(move || stdin.write_all(&bytes))
    });
    let mut stdout = child.stdout.take().expect("piped stdout must be present");
    let mut stderr = child.stderr.take().expect("piped stderr must be present");
    let mut stdout_reader = Some(thread::spawn(move || {
        let mut bytes = Vec::new();
        stdout.read_to_end(&mut bytes).map(|_| bytes)
    }));
    let mut stderr_reader = Some(thread::spawn(move || {
        let mut bytes = Vec::new();
        stderr.read_to_end(&mut bytes).map(|_| bytes)
    }));

    let deadline = Instant::now() + timeout;
    let status = loop {
        if let Some(status) = child.try_wait().map_err(|error| {
            Error::Io(IoError::new(
                error.kind(),
                format!("waiting for Tesseract failed: {error}"),
            ))
        })? {
            break status;
        }
        if Instant::now() >= deadline {
            terminate_and_reap(&mut child)?;
            if let Err(error) = join_writer(writer.take()) {
                tracing::debug!(%error, "Tesseract stdin closed during timeout cleanup");
            }
            join_reader(
                stdout_reader.take().expect("stdout reader must be present"),
                "stdout",
            )?;
            join_reader(
                stderr_reader.take().expect("stderr reader must be present"),
                "stderr",
            )?;
            return Err(Error::Platform(format!(
                "Tesseract exceeded its {timeout:?} deadline and was terminated"
            )));
        }
        thread::sleep(WAIT_INTERVAL);
    };

    let write_result = join_writer(writer.take());
    let stdout = join_reader(
        stdout_reader.take().expect("stdout reader must be present"),
        "stdout",
    )?;
    let stderr = join_reader(
        stderr_reader.take().expect("stderr reader must be present"),
        "stderr",
    )?;
    if status.success() {
        write_result?;
    }

    Ok(CommandOutput {
        status,
        stdout,
        stderr,
    })
}

fn terminate_and_reap(child: &mut std::process::Child) -> Result<()> {
    match child.kill() {
        Ok(()) => child.wait().map(|_| ()).map_err(|error| {
            Error::Io(IoError::new(
                error.kind(),
                format!("reaping timed-out Tesseract failed: {error}"),
            ))
        }),
        Err(kill_error) => match child.try_wait() {
            Ok(Some(_)) => Ok(()),
            Ok(None) => Err(Error::Io(IoError::new(
                kill_error.kind(),
                format!("terminating timed-out Tesseract failed: {kill_error}"),
            ))),
            Err(wait_error) => Err(Error::Io(IoError::new(
                wait_error.kind(),
                format!(
                    "terminating timed-out Tesseract failed ({kill_error}); checking its status \
                     also failed: {wait_error}"
                ),
            ))),
        },
    }
}

fn join_writer(writer: Option<thread::JoinHandle<std::io::Result<()>>>) -> Result<()> {
    let Some(writer) = writer else {
        return Ok(());
    };
    writer
        .join()
        .map_err(|_| Error::Platform("the Tesseract stdin worker panicked".to_string()))?
        .map_err(|error| {
            Error::Io(IoError::new(
                error.kind(),
                format!("writing image to Tesseract failed: {error}"),
            ))
        })
}

fn join_reader(
    reader: thread::JoinHandle<std::io::Result<Vec<u8>>>,
    stream: &str,
) -> Result<Vec<u8>> {
    reader
        .join()
        .map_err(|_| Error::Platform(format!("the Tesseract {stream} worker panicked")))?
        .map_err(|error| {
            Error::Io(IoError::new(
                error.kind(),
                format!("reading Tesseract {stream} failed: {error}"),
            ))
        })
}

fn pgm(image: &prepare::Rgba8Image) -> Vec<u8> {
    let luma = rec601_luma_on_white(image);
    let header = format!("P5\n{} {}\n255\n", image.width, image.height);
    let mut bytes = Vec::with_capacity(header.len() + luma.len());
    bytes.extend_from_slice(header.as_bytes());
    bytes.extend_from_slice(&luma);
    bytes
}

fn rec601_luma_on_white(image: &prepare::Rgba8Image) -> Vec<u8> {
    image
        .data
        .as_chunks::<4>()
        .0
        .iter()
        .map(|pixel| {
            let red = u32::from(pixel[0]);
            let green = u32::from(pixel[1]);
            let blue = u32::from(pixel[2]);
            let alpha = u32::from(pixel[3]);
            let luma = (299 * red + 587 * green + 114 * blue + 500) / 1_000;
            ((luma * alpha + 255 * (255 - alpha) + 127) / 255) as u8
        })
        .collect()
}

fn resolve_languages(requested: &[String], installed: &[String]) -> Result<Vec<String>> {
    let mut selected = Vec::new();
    for requested_tag in requested {
        if let Some(installed_tag) = installed_language(requested_tag, installed)
            && !selected
                .iter()
                .any(|selected: &String| selected.eq_ignore_ascii_case(installed_tag))
        {
            selected.push(installed_tag.to_string());
        }
    }
    if selected.is_empty() {
        return Err(Error::Unsupported {
            what: format!("text recognition in {}", requested.join(", ")),
            why: format!(
                "none of the requested Tesseract traineddata is installed. Installed models: {}. \
                 {}",
                installed_description(installed),
                install_guidance()
            ),
        });
    }
    Ok(selected)
}

fn installed_language<'a>(requested: &str, installed: &'a [String]) -> Option<&'a str> {
    installed
        .iter()
        .find(|tag| tag.eq_ignore_ascii_case(requested))
        .map(String::as_str)
        .or_else(|| {
            tesseract_language_aliases(requested)
                .iter()
                .find_map(|alias| {
                    installed
                        .iter()
                        .find(|tag| tag.eq_ignore_ascii_case(alias))
                        .map(String::as_str)
                })
        })
}

fn resolve_system_languages(installed: &[String]) -> Result<Vec<String>> {
    let requested = system_language_tags()?;
    if requested.is_empty() {
        return Err(Error::Unsupported {
            what: "text recognition in the system language".to_string(),
            why: format!(
                "the Windows locale does not name a language Tesseract can select. Installed \
                 models: {}. Pass an explicit BCP-47 language tag",
                installed_description(installed)
            ),
        });
    }
    resolve_languages(&requested, installed).map_err(|_| Error::Unsupported {
        what: format!(
            "text recognition in the Windows locale ({})",
            requested.join(", ")
        ),
        why: format!(
            "no installed Tesseract traineddata matches the Windows locale. Installed models: {}. \
             {}",
            installed_description(installed),
            install_guidance()
        ),
    })
}

fn installed_description(installed: &[String]) -> String {
    if installed.is_empty() {
        "none".to_string()
    } else {
        installed.join(", ")
    }
}

#[cfg(target_os = "windows")]
fn system_language_tags() -> Result<Vec<String>> {
    use windows::Win32::{Foundation::GetLastError, Globalization::GetUserDefaultLocaleName};

    let mut buffer = [0u16; 85];
    // SAFETY: the buffer contains 85 writable UTF-16 code units as required by
    // `GetUserDefaultLocaleName`.
    let length = unsafe { GetUserDefaultLocaleName(&mut buffer) };
    if length == 0 {
        return Err(Error::Platform(format!(
            "GetUserDefaultLocaleName failed with Win32 error {}",
            // SAFETY: this immediately follows the failed Win32 call.
            unsafe { GetLastError() }.0
        )));
    }
    let value = String::from_utf16(&buffer[..length.saturating_sub(1) as usize])
        .map_err(|error| Error::Platform(format!("Windows returned an invalid locale: {error}")))?;
    Ok(normalize_locale(&value).into_iter().collect())
}

#[cfg(all(test, not(target_os = "windows")))]
fn system_language_tags() -> Result<Vec<String>> {
    Ok(vec!["en-US".to_string()])
}

fn normalize_locale(raw: &str) -> Option<String> {
    let raw = raw.trim();
    let (locale, modifier) = raw
        .split_once('@')
        .map_or((raw, None), |(locale, modifier)| (locale, Some(modifier)));
    let locale = locale.split('.').next().unwrap_or_default();
    if locale.is_empty() || locale.eq_ignore_ascii_case("c") || locale.eq_ignore_ascii_case("posix")
    {
        return None;
    }
    let mut normalized = locale.replace('_', "-");
    if let Some(script) = modifier
        .and_then(|modifier| modifier.split('.').next())
        .and_then(posix_script)
        && !normalized
            .split('-')
            .any(|subtag| subtag.eq_ignore_ascii_case(script))
    {
        normalized.push('-');
        normalized.push_str(script);
    }
    Some(normalized)
}

fn posix_script(modifier: &str) -> Option<&'static str> {
    if modifier.eq_ignore_ascii_case("latin") || modifier.eq_ignore_ascii_case("latn") {
        Some("Latn")
    } else if modifier.eq_ignore_ascii_case("cyrillic") || modifier.eq_ignore_ascii_case("cyrl") {
        Some("Cyrl")
    } else {
        None
    }
}

fn tesseract_language_aliases(tag: &str) -> Vec<String> {
    let normalized = tag.replace('_', "-").to_ascii_lowercase();
    let base = normalized.split('-').next().unwrap_or(normalized.as_str());

    if matches!(base, "zh" | "zho" | "chi") {
        let subtags = normalized.split('-').skip(1).collect::<Vec<_>>();
        let traditional = if normalized == "chi-tra" || subtags.contains(&"hant") {
            true
        } else if normalized == "chi-sim" || subtags.contains(&"hans") {
            false
        } else {
            subtags
                .iter()
                .any(|subtag| matches!(*subtag, "hk" | "mo" | "tw"))
        };
        return vec![if traditional { "chi_tra" } else { "chi_sim" }.to_string()];
    }

    // Tesseract ships one Norwegian model, while BCP-47 distinguishes Bokmal
    // (`nb`) and Nynorsk (`nn`).
    if matches!(base, "nb" | "nob" | "nn" | "nno") {
        return vec!["nor".to_string()];
    }

    let base = iso_639_2b_terminology(base);
    let Some(language) = Language::from_639_1(base).or_else(|| Language::from_639_3(base)) else {
        return Vec::new();
    };
    let model = language.to_639_3();
    let script = normalized
        .split('-')
        .skip(1)
        .find(|subtag| subtag.len() == 4);
    let script_model = match (model, script) {
        ("aze", Some("cyrl")) => Some("aze_cyrl"),
        ("srp", Some("latn")) => Some("srp_latn"),
        ("uzb", Some("cyrl")) => Some("uzb_cyrl"),
        _ => None,
    };
    script_model.map_or_else(
        || vec![model.to_string()],
        |script_model| vec![script_model.to_string(), model.to_string()],
    )
}

fn iso_639_2b_terminology(code: &str) -> &str {
    match code {
        "alb" => "sqi",
        "arm" => "hye",
        "baq" => "eus",
        "bur" => "mya",
        "chi" => "zho",
        "cze" => "ces",
        "dut" => "nld",
        "fre" => "fra",
        "geo" => "kat",
        "ger" => "deu",
        "gre" => "ell",
        "ice" => "isl",
        "mac" => "mkd",
        "mao" => "mri",
        "may" => "msa",
        "per" => "fas",
        "rum" => "ron",
        "slo" => "slk",
        "tib" => "bod",
        "wel" => "cym",
        _ => code,
    }
}

#[derive(Default)]
struct TsvLine {
    words: Vec<String>,
    bounds: PhysicalRect,
    weighted_confidence: f32,
    confidence_weight: usize,
}

fn parse_tsv(
    tsv: &str,
    upscale: f64,
    source_size: PhysicalSize,
    frame: &Frame,
) -> Result<Vec<TextBlock>> {
    let mut rows = tsv.lines();
    let header = rows.next().unwrap_or_default().trim_end_matches('\r');
    if header != TSV_HEADER {
        return Err(Error::Codec(format!(
            "Tesseract returned an unexpected TSV header: {header:?}"
        )));
    }

    let mut lines: BTreeMap<(u32, u32, u32, u32), TsvLine> = BTreeMap::new();
    for (index, row) in rows.enumerate() {
        if row.trim().is_empty() {
            continue;
        }
        let columns: Vec<&str> = row.trim_end_matches('\r').splitn(12, '\t').collect();
        if columns.len() != 12 {
            return Err(Error::Codec(format!(
                "Tesseract TSV row {} has {} columns, expected 12",
                index + 2,
                columns.len()
            )));
        }
        if parse::<u32>(columns[0], index, "level")? != 5 || columns[11].trim().is_empty() {
            continue;
        }
        let key = (
            parse(columns[1], index, "page_num")?,
            parse(columns[2], index, "block_num")?,
            parse(columns[3], index, "par_num")?,
            parse(columns[4], index, "line_num")?,
        );
        let left = parse::<f64>(columns[6], index, "left")?;
        let top = parse::<f64>(columns[7], index, "top")?;
        let width = parse::<f64>(columns[8], index, "width")?;
        let height = parse::<f64>(columns[9], index, "height")?;
        let confidence = parse::<f32>(columns[10], index, "conf")?.clamp(0.0, 100.0) / 100.0;
        let word = columns[11].trim();
        let weight = word.chars().count().max(1);
        let row_bounds = layout::pixels_to_physical(left, top, width, height, upscale, source_size);
        let line = lines.entry(key).or_default();
        line.words.push(word.to_string());
        line.bounds = layout::union(line.bounds, row_bounds);
        line.weighted_confidence += confidence * weight as f32;
        line.confidence_weight += weight;
    }

    let blocks = lines
        .into_values()
        .filter(|line| !line.bounds.is_empty())
        .map(|line| TextBlock {
            text: line.words.join(" "),
            confidence: line.weighted_confidence / line.confidence_weight as f32,
            bounds: layout::to_logical(line.bounds, frame.scale),
        })
        .collect();
    Ok(layout::sort_reading_order(blocks))
}

fn parse<T>(value: &str, row_index: usize, name: &str) -> Result<T>
where
    T: std::str::FromStr,
    T::Err: std::fmt::Display,
{
    value.parse().map_err(|error| {
        Error::Codec(format!(
            "invalid {name} in Tesseract TSV row {}: {error}",
            row_index + 2
        ))
    })
}

fn spawn_error(program: &OsStr, error: std::io::Error) -> Error {
    if error.kind() == ErrorKind::NotFound {
        Error::Unsupported {
            what: "text recognition with Tesseract".to_string(),
            why: format!(
                "the artifact-local Tesseract executable was not found at {}. {}",
                Path::new(program).display(),
                install_guidance()
            ),
        }
    } else {
        Error::Io(IoError::new(
            error.kind(),
            format!(
                "starting artifact-local Tesseract at {} failed: {error}",
                Path::new(program).display()
            ),
        ))
    }
}

fn command_error(action: &str, stderr: &[u8]) -> Error {
    let detail = String::from_utf8_lossy(stderr).trim().to_string();
    if detail.contains("Failed loading language")
        || detail.contains("Error opening data file")
        || detail.contains("TESSDATA_PREFIX")
    {
        Error::Unsupported {
            what: format!("{action} with Tesseract"),
            why: format!(
                "a required Tesseract traineddata file is missing. {} Tesseract reported: {detail}",
                install_guidance()
            ),
        }
    } else {
        Error::Platform(format!("Tesseract could not {action}: {detail}"))
    }
}

fn install_guidance() -> &'static str {
    "Keep `tesseract/` beside `scrozz.exe` with `tesseract.exe`, its dependent DLLs, \
     and `tessdata/eng.traineddata`; source builds and tests may instead set \
     `SCROZZ_TESSERACT_DIR` to an absolute directory with that layout."
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicU64, Ordering};

    use scrozz_core::{ColorSpace, PixelFormat, ScaleFactor};

    use super::*;

    static FIXTURE_ID: AtomicU64 = AtomicU64::new(0);

    struct FixtureDirectory(PathBuf);

    impl FixtureDirectory {
        fn new(label: &str) -> Self {
            let id = FIXTURE_ID.fetch_add(1, Ordering::Relaxed);
            let path = std::env::temp_dir()
                .join(format!("scrozz-ocr-{label}-{}-{id}", std::process::id()));
            let _ = std::fs::remove_dir_all(&path);
            std::fs::create_dir_all(&path).unwrap();
            Self(path)
        }

        fn portable_payload(&self) -> PathBuf {
            let directory = self.0.join("tesseract");
            std::fs::create_dir_all(directory.join("tessdata")).unwrap();
            std::fs::write(directory.join(TESSERACT_EXECUTABLE), []).unwrap();
            std::fs::write(directory.join("tessdata").join(REQUIRED_ENGLISH_MODEL), []).unwrap();
            directory
        }
    }

    impl Drop for FixtureDirectory {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    fn frame(width: u32, height: u32) -> Frame {
        Frame {
            data: vec![255; width as usize * height as usize * 4],
            size: PhysicalSize::new(f64::from(width), f64::from(height)),
            stride: width as usize * 4,
            format: PixelFormat::Rgba8,
            color_space: ColorSpace::Srgb,
            scale: ScaleFactor::IDENTITY,
        }
    }

    #[test]
    fn portable_default_is_sibling_to_scrozz_and_never_path() {
        assert_eq!(
            sibling_tesseract_directory(Path::new("/artifact/scrozz.exe")).unwrap(),
            PathBuf::from("/artifact/tesseract")
        );
        let error =
            run_command(OsStr::new(TESSERACT_EXECUTABLE), &[], None, Duration::ZERO).unwrap_err();
        assert!(
            matches!(error, Error::InvalidRequest(message) if message.contains("PATH") && message.contains("absolute"))
        );
    }

    #[test]
    fn default_layout_resolves_an_absolute_artifact_local_program() {
        let fixture = FixtureDirectory::new("artifact-layout");
        let payload = fixture.portable_payload();
        let scrozz = fixture.0.join("scrozz.exe");
        std::fs::write(&scrozz, []).unwrap();

        let installation = resolve_tesseract_installation(None, &scrozz).unwrap();
        assert_eq!(installation.program, payload.join(TESSERACT_EXECUTABLE));
        assert_eq!(installation.tessdata, payload.join("tessdata"));
        assert!(installation.program.is_absolute());
    }

    #[test]
    fn configured_override_must_be_absolute_and_have_the_artifact_contract() {
        let relative = resolve_tesseract_installation(
            Some(OsStr::new("relative/tesseract")),
            Path::new("/artifact/scrozz.exe"),
        )
        .unwrap_err();
        assert!(matches!(relative, Error::InvalidRequest(message) if message.contains("absolute")));

        let fixture = FixtureDirectory::new("override-layout");
        let payload = fixture.portable_payload();
        let installation =
            resolve_tesseract_installation(Some(payload.as_os_str()), Path::new("/unused.exe"))
                .unwrap();
        assert_eq!(installation.program, payload.join(TESSERACT_EXECUTABLE));
    }

    #[test]
    fn english_traineddata_is_part_of_the_portable_contract() {
        let fixture = FixtureDirectory::new("missing-eng");
        let directory = fixture.0.join("tesseract");
        std::fs::create_dir_all(directory.join("tessdata")).unwrap();
        std::fs::write(directory.join(TESSERACT_EXECUTABLE), []).unwrap();

        let error = validate_tesseract_directory(&directory, false).unwrap_err();
        assert!(matches!(error, Error::Unsupported { why, .. } if why.contains("eng.traineddata")));
    }

    #[test]
    fn recognition_always_passes_an_explicit_language() {
        let arguments = recognition_arguments(&["eng".to_string()], &Options::default());
        let language_index = arguments
            .iter()
            .position(|argument| argument == "-l")
            .expect("-l");
        assert_eq!(arguments[language_index + 1], "eng");
        assert!(
            arguments
                .iter()
                .any(|argument| argument == "tessedit_create_tsv=1")
        );
        assert!(
            !arguments.iter().any(|argument| argument == "tsv"),
            "the bare config would require tessdata/configs/tsv"
        );
    }

    #[test]
    fn resolves_bcp47_tags_to_installed_traineddata() {
        assert_eq!(
            resolve_languages(&["en-US".to_string()], &["eng".to_string()]).unwrap(),
            ["eng"]
        );
        assert_eq!(
            resolve_languages(&["ro-RO".to_string()], &["ron".to_string()]).unwrap(),
            ["ron"]
        );
        assert_eq!(
            resolve_languages(&["nb-NO".to_string()], &["nor".to_string()]).unwrap(),
            ["nor"]
        );
        assert_eq!(
            resolve_languages(&["nn-NO".to_string()], &["nor".to_string()]).unwrap(),
            ["nor"]
        );
    }

    #[test]
    fn chinese_scripts_never_cross_resolve() {
        let installed = ["chi_sim".to_string(), "chi_tra".to_string()];
        assert_eq!(
            resolve_languages(&["zh-CN".to_string()], &installed).unwrap(),
            ["chi_sim"]
        );
        assert_eq!(
            resolve_languages(&["zh-TW".to_string()], &installed).unwrap(),
            ["chi_tra"]
        );
        assert!(matches!(
            resolve_languages(&["chi_tra".to_string()], &["chi_sim".to_string()]),
            Err(Error::Unsupported { .. })
        ));
        assert!(matches!(
            resolve_languages(&["chi_sim".to_string()], &["chi_tra".to_string()]),
            Err(Error::Unsupported { .. })
        ));
    }

    #[test]
    fn timeout_terminates_and_reaps_the_child() {
        let executable = std::env::current_exe().unwrap();
        let arguments = [
            OsString::from("--ignored"),
            OsString::from("--exact"),
            OsString::from("tesseract::tests::timeout_child"),
        ];
        let started = Instant::now();
        let error = run_command(
            executable.as_os_str(),
            &arguments,
            None,
            Duration::from_millis(100),
        )
        .unwrap_err();

        assert!(matches!(error, Error::Platform(message) if message.contains("terminated")));
        assert!(started.elapsed() < Duration::from_secs(2));
    }

    #[test]
    #[ignore = "launched by timeout_terminates_and_reaps_the_child"]
    fn timeout_child() {
        thread::sleep(Duration::from_secs(10));
    }

    #[test]
    fn parses_words_into_visual_lines_and_logical_bounds() {
        let tsv = concat!(
            "level\tpage_num\tblock_num\tpar_num\tline_num\tword_num\tleft\ttop\twidth\theight\tconf\ttext\n",
            "5\t1\t1\t1\t1\t1\t20\t10\t40\t20\t90\tHello\n",
            "5\t1\t1\t1\t1\t2\t70\t10\t50\t20\t80\tworld\n",
            "5\t1\t1\t1\t2\t1\t20\t50\t40\t20\t100\tAgain\n"
        );

        let blocks =
            parse_tsv(tsv, 2.0, PhysicalSize::new(100.0, 100.0), &frame(100, 100)).unwrap();
        assert_eq!(blocks.len(), 2);
        assert_eq!(blocks[0].text, "Hello world");
        assert_eq!(
            blocks[0].bounds.origin,
            scrozz_core::LogicalPoint::new(10.0, 5.0)
        );
        assert_eq!(blocks[0].bounds.size.width, 50.0);
        assert_eq!(blocks[1].text, "Again");
    }

    #[test]
    fn pgm_composites_transparency_onto_white() {
        let image = prepare::Rgba8Image::new(1, 1, vec![0, 0, 0, 0]).unwrap();
        assert_eq!(pgm(&image).last(), Some(&255));
    }
}
