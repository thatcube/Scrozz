//! Subprocess OCR for Linux and portable, unpackaged Windows builds.

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

#[derive(Debug)]
struct CommandOutput {
    status: ExitStatus,
    stdout: Vec<u8>,
    stderr: Vec<u8>,
}

/// Lists the language data installed for Tesseract.
pub fn available_languages() -> Result<Vec<String>> {
    let output = run_tesseract(&[OsString::from("--list-langs")], None, COMMAND_TIMEOUT)?;
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
                    .all(|ch| ch.is_ascii_alphanumeric() || ch == '_' || ch == '-')
        })
        .map(str::to_string)
        .collect();
    languages.sort_unstable();
    languages.dedup();
    Ok(languages)
}

/// Recognizes text by streaming a PGM image to Tesseract and parsing TSV output.
pub fn recognize(frame: &Frame, options: &Options) -> Result<Vec<TextBlock>> {
    if options.automatic_language_detection {
        return Err(Error::Unsupported {
            what: "automatic OCR language detection".to_string(),
            why: "Tesseract requires an installed language model selected by name; it does not \
                  infer the language of arbitrary image content"
                .to_string(),
        });
    }

    let prepared = prepare::prepare(frame, options.upscale, None)?;
    let installed_languages = available_languages()?;
    let selected_languages = if options.languages.is_empty() {
        resolve_system_languages(&installed_languages)?
    } else {
        resolve_languages(&options.languages, &installed_languages)?
    };

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
    arguments.push(OsString::from("tsv"));

    let output = run_tesseract(&arguments, Some(pgm(&prepared.image)), COMMAND_TIMEOUT)?;
    if !output.status.success() {
        return Err(command_error("recognize text", &output.stderr));
    }

    let tsv = String::from_utf8(output.stdout)
        .map_err(|error| Error::Codec(format!("Tesseract returned non-UTF-8 TSV: {error}")))?;
    parse_tsv(&tsv, prepared.upscale, prepared.source_size, frame)
}

fn run_tesseract(
    arguments: &[OsString],
    input: Option<Vec<u8>>,
    timeout: Duration,
) -> Result<CommandOutput> {
    let installation =
        tesseract_installation(std::env::var_os(TESSERACT_DIRECTORY_ENV).as_deref())?;
    let mut configured_arguments = Vec::with_capacity(arguments.len() + 2);
    if let Some(tessdata) = installation.tessdata {
        configured_arguments.extend([OsString::from("--tessdata-dir"), tessdata.into_os_string()]);
    }
    configured_arguments.extend_from_slice(arguments);
    run_command(
        installation.program.as_os_str(),
        &configured_arguments,
        input,
        timeout,
    )
}

#[derive(Debug)]
struct TesseractInstallation {
    program: PathBuf,
    tessdata: Option<PathBuf>,
}

fn tesseract_installation(directory: Option<&OsStr>) -> Result<TesseractInstallation> {
    let Some(directory) = directory else {
        return Ok(TesseractInstallation {
            program: PathBuf::from("tesseract"),
            tessdata: None,
        });
    };
    let directory = Path::new(directory);
    if !directory.is_absolute() {
        return Err(Error::InvalidRequest(format!(
            "{TESSERACT_DIRECTORY_ENV} must be an absolute directory"
        )));
    }

    #[cfg(target_os = "windows")]
    let executable = "tesseract.exe";
    #[cfg(not(target_os = "windows"))]
    let executable = "tesseract";

    let program = directory.join(executable);
    let tessdata = directory.join("tessdata");
    if !program.is_file() || !tessdata.is_dir() {
        return Err(Error::Unsupported {
            what: "text recognition with the configured Tesseract installation".to_string(),
            why: format!(
                "{TESSERACT_DIRECTORY_ENV} must contain `{executable}` and a `tessdata` directory; \
                 configured path: {}",
                directory.display()
            ),
        });
    }
    Ok(TesseractInstallation {
        program,
        tessdata: Some(tessdata),
    })
}

fn run_command(
    program: &OsStr,
    arguments: &[OsString],
    input: Option<Vec<u8>>,
    timeout: Duration,
) -> Result<CommandOutput> {
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
        .map_err(spawn_error)?;

    let mut writer = input.map(|bytes| {
        let mut stdin = child.stdin.take().expect("piped stdin must be present");
        thread::spawn(move || stdin.write_all(&bytes))
    });
    let mut stdout = child.stdout.take().ok_or_else(|| {
        Error::Io(IoError::new(
            ErrorKind::BrokenPipe,
            "Tesseract stdout was not available",
        ))
    })?;
    let mut stderr = child.stderr.take().ok_or_else(|| {
        Error::Io(IoError::new(
            ErrorKind::BrokenPipe,
            "Tesseract stderr was not available",
        ))
    })?;
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
                // Closing a killed child's input commonly races the writer and
                // produces BrokenPipe. The timeout remains the useful failure.
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
    let luma = prepare::rec601_luma_on_white(image);
    let header = format!("P5\n{} {}\n255\n", image.width, image.height);
    let mut bytes = Vec::with_capacity(header.len() + luma.len());
    bytes.extend_from_slice(header.as_bytes());
    bytes.extend_from_slice(&luma);
    bytes
}

fn resolve_languages(requested: &[String], installed: &[String]) -> Result<Vec<String>> {
    let mut selected = Vec::new();
    for requested_tag in requested {
        let installed_tag = installed_language(requested_tag, installed);
        if let Some(installed_tag) = installed_tag
            && !selected.contains(installed_tag)
        {
            selected.push(installed_tag.clone());
        }
    }
    if selected.is_empty() {
        return Err(Error::Unsupported {
            what: format!("text recognition in {}", requested.join(", ")),
            why: format!(
                "none of the requested Tesseract language models is installed. Installed \
                 models: {}. {}",
                installed_description(installed),
                install_guidance()
            ),
        });
    }
    Ok(selected)
}

fn installed_language<'a>(requested: &str, installed: &'a [String]) -> Option<&'a String> {
    installed
        .iter()
        .find(|tag| tag.eq_ignore_ascii_case(requested))
        .or_else(|| {
            tesseract_language_aliases(requested)
                .iter()
                .find_map(|alias| installed.iter().find(|tag| tag.eq_ignore_ascii_case(alias)))
        })
}

fn resolve_system_languages(installed: &[String]) -> Result<Vec<String>> {
    let requested = system_language_tags()?;
    if requested.is_empty() {
        return Err(Error::Unsupported {
            what: "text recognition in the system language".to_string(),
            why: format!(
                "the system locale does not name a language Tesseract can select. Installed \
                 models: {}. Configure a language locale or pass `--language <tag>` explicitly",
                installed_description(installed)
            ),
        });
    }
    resolve_languages(&requested, installed).map_err(|_| Error::Unsupported {
        what: format!(
            "text recognition in the system locale ({})",
            requested.join(", ")
        ),
        why: format!(
            "no installed Tesseract model matches the system locale. Installed models: {}. {}",
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

#[cfg(any(target_os = "linux", all(test, target_os = "macos")))]
fn system_language_tags() -> Result<Vec<String>> {
    let values = ["LC_ALL", "LC_MESSAGES", "LANGUAGE", "LANG"]
        .map(|key| std::env::var_os(key).map(|value| value.to_string_lossy().into_owned()));
    Ok(first_configured_language_list(
        values.iter().map(Option::as_deref),
    ))
}

#[cfg(target_os = "windows")]
fn system_language_tags() -> Result<Vec<String>> {
    use windows::Win32::{Foundation::GetLastError, Globalization::GetUserDefaultLocaleName};

    let mut buffer = [0u16; 85];
    let length = unsafe { GetUserDefaultLocaleName(&mut buffer) };
    if length == 0 {
        return Err(Error::Platform(format!(
            "GetUserDefaultLocaleName failed with Win32 error {}",
            unsafe { GetLastError() }.0
        )));
    }
    let value = String::from_utf16(&buffer[..length.saturating_sub(1) as usize])
        .map_err(|error| Error::Platform(format!("Windows returned an invalid locale: {error}")))?;
    Ok(normalize_locale(&value).into_iter().collect())
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

fn first_configured_language_list<'a>(
    values: impl IntoIterator<Item = Option<&'a str>>,
) -> Vec<String> {
    for value in values.into_iter().flatten() {
        if value.trim().is_empty() {
            continue;
        }
        let mut tags = Vec::new();
        for raw in value.split(':') {
            if let Some(tag) = normalize_locale(raw)
                && !tags.contains(&tag)
            {
                tags.push(tag);
            }
        }
        return tags;
    }
    Vec::new()
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
    // ISO 639-2 defines 20 legacy bibliographic spellings. BCP-47 registries
    // normally prefer the terminology spelling, but accepting the complete
    // equivalence set keeps model resolution correct for older locale sources.
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

fn spawn_error(error: std::io::Error) -> Error {
    if error.kind() == std::io::ErrorKind::NotFound {
        Error::Unsupported {
            what: "text recognition with Tesseract".to_string(),
            why: format!(
                "the `tesseract` executable was not found on PATH. {}",
                install_guidance()
            ),
        }
    } else {
        Error::Io(IoError::new(
            error.kind(),
            format!("starting Tesseract failed: {error}"),
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
                "a required Tesseract language model is missing. {} Tesseract reported: {detail}",
                install_guidance()
            ),
        }
    } else {
        Error::Platform(format!("Tesseract could not {action}: {detail}"))
    }
}

#[cfg(target_os = "linux")]
fn install_guidance() -> &'static str {
    "Install `tesseract-ocr` plus `tesseract-ocr-<lang>` on Debian/Ubuntu, \
     `tesseract` plus `tesseract-langpack-<lang>` on Fedora, or `tesseract` plus \
     `tesseract-data-<lang>` on Arch Linux."
}

#[cfg(target_os = "windows")]
fn install_guidance() -> &'static str {
    "Install the local Tesseract OCR distribution and its language data, then add its install \
     directory to PATH; the UB Mannheim Windows build is the commonly maintained distribution."
}

#[cfg(all(test, not(any(target_os = "linux", target_os = "windows"))))]
fn install_guidance() -> &'static str {
    "Install Tesseract and its language data."
}

#[cfg(test)]
mod tests {
    use scrozz_core::{ColorSpace, PixelFormat, ScaleFactor};

    use super::*;

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
    fn resolves_bcp47_aliases_without_native_libraries() {
        assert_eq!(
            resolve_languages(&["en-US".to_string()], &["eng".to_string()]).unwrap(),
            vec!["eng"]
        );
        assert_eq!(
            resolve_languages(&["zh-Hant".to_string()], &["chi_tra".to_string()]).unwrap(),
            vec!["chi_tra"]
        );
        assert_eq!(
            resolve_languages(&["ro-RO".to_string()], &["ron".to_string()]).unwrap(),
            vec!["ron"]
        );
        assert_eq!(
            resolve_languages(
                &["zh-HK".to_string()],
                &["chi_sim".to_string(), "chi_tra".to_string()]
            )
            .unwrap(),
            vec!["chi_tra"]
        );
        assert_eq!(
            resolve_languages(&["cy-GB".to_string()], &["cym".to_string()]).unwrap(),
            vec!["cym"]
        );
        assert_eq!(
            resolve_languages(&["wel-GB".to_string()], &["cym".to_string()]).unwrap(),
            vec!["cym"]
        );
    }

    #[test]
    fn configured_tesseract_requires_the_portable_payload_layout() {
        let root =
            std::env::temp_dir().join(format!("scrozz-tesseract-layout-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(root.join("tessdata")).expect("tessdata directory");
        #[cfg(target_os = "windows")]
        let executable = "tesseract.exe";
        #[cfg(not(target_os = "windows"))]
        let executable = "tesseract";
        std::fs::write(root.join(executable), []).expect("fake executable");

        let installation =
            tesseract_installation(Some(root.as_os_str())).expect("valid portable layout");

        assert_eq!(installation.program, root.join(executable));
        assert_eq!(installation.tessdata, Some(root.join("tessdata")));
        std::fs::remove_dir_all(root).expect("remove fixture");
    }

    #[test]
    fn configured_tesseract_directory_must_be_absolute() {
        let error = tesseract_installation(Some(OsStr::new("relative/tesseract"))).unwrap_err();
        assert!(matches!(error, Error::InvalidRequest(message) if message.contains("absolute")));
    }

    #[test]
    fn normalizes_host_locales_before_model_resolution() {
        assert_eq!(normalize_locale("en_US.UTF-8"), Some("en-US".to_string()));
        assert_eq!(
            normalize_locale("sr_RS@latin.UTF-8"),
            Some("sr-RS-Latn".to_string())
        );
        assert_eq!(
            normalize_locale("az_AZ.UTF-8@cyrillic"),
            Some("az-AZ-Cyrl".to_string())
        );
        assert_eq!(normalize_locale("C.UTF-8"), None);
        assert_eq!(normalize_locale("POSIX"), None);
    }

    #[test]
    fn effective_locale_does_not_fall_through_to_lower_priority_variables() {
        let tags =
            first_configured_language_list([Some("fr_FR.UTF-8"), Some("en_US.UTF-8"), None, None]);
        assert_eq!(tags, ["fr-FR"]);

        let c_locale =
            first_configured_language_list([Some("C.UTF-8"), Some("en_US.UTF-8"), None, None]);
        assert!(c_locale.is_empty());
    }

    #[test]
    fn unavailable_host_language_is_a_typed_error() {
        let error = resolve_languages(&["fr-FR".to_string()], &["eng".to_string()]).unwrap_err();
        assert!(matches!(error, Error::Unsupported { .. }));
    }

    #[test]
    fn script_specific_models_are_preferred_with_base_fallbacks() {
        assert_eq!(
            resolve_languages(
                &["sr-Latn".to_string()],
                &["srp".to_string(), "srp_latn".to_string()]
            )
            .unwrap(),
            vec!["srp_latn"]
        );
        assert_eq!(
            resolve_languages(
                &["az-Cyrl".to_string()],
                &["aze".to_string(), "aze_cyrl".to_string()]
            )
            .unwrap(),
            vec!["aze_cyrl"]
        );
        assert_eq!(
            resolve_languages(
                &["uz-Cyrl".to_string()],
                &["uzb".to_string(), "uzb_cyrl".to_string()]
            )
            .unwrap(),
            vec!["uzb_cyrl"]
        );
        assert_eq!(
            resolve_languages(&["sr-Latn".to_string()], &["srp".to_string()]).unwrap(),
            vec!["srp"]
        );
    }

    #[test]
    fn chinese_models_never_cross_fallback_between_scripts() {
        let traditional = resolve_languages(&["chi_tra".to_string()], &["chi_sim".to_string()]);
        assert!(matches!(traditional, Err(Error::Unsupported { .. })));

        let simplified = resolve_languages(&["chi_sim".to_string()], &["chi_tra".to_string()]);
        assert!(matches!(simplified, Err(Error::Unsupported { .. })));
    }

    #[test]
    fn timeout_terminates_and_reaps_the_child() {
        let executable = std::env::current_exe().expect("test executable");
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
            Duration::from_millis(50),
        )
        .unwrap_err();

        assert!(matches!(error, Error::Platform(message) if message.contains("deadline")));
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
    fn rejects_non_tsv_success_output() {
        let error =
            parse_tsv("not tsv\n", 1.0, PhysicalSize::new(1.0, 1.0), &frame(1, 1)).unwrap_err();
        assert!(matches!(error, Error::Codec(_)));
    }

    #[test]
    fn pgm_composites_transparency_onto_white() {
        let image = prepare::Rgba8Image::new(1, 1, vec![0, 0, 0, 0]).unwrap();
        assert_eq!(pgm(&image).last(), Some(&255));
    }
}
