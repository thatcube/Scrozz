//! Linux OCR through the distro-provided `tesseract` executable.

use std::{
    collections::BTreeMap,
    io::{Error as IoError, ErrorKind, Write},
    process::{Command, Stdio},
};

use scrozz_core::{Error, Frame, PhysicalRect, PhysicalSize, Result};

use crate::{Accuracy, Options, TextBlock, layout, prepare};

const TSV_HEADER: &str =
    "level\tpage_num\tblock_num\tpar_num\tline_num\tword_num\tleft\ttop\twidth\theight\tconf\ttext";

/// Lists the language data installed for Tesseract.
pub fn available_languages() -> Result<Vec<String>> {
    let output = Command::new("tesseract")
        .arg("--list-langs")
        .output()
        .map_err(spawn_error)?;
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
    let selected_languages = if options.languages.is_empty() {
        Vec::new()
    } else {
        resolve_languages(&options.languages, &available_languages()?)?
    };

    let mut command = Command::new("tesseract");
    command
        .arg("stdin")
        .arg("stdout")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    if !selected_languages.is_empty() {
        command.arg("-l").arg(selected_languages.join("+"));
    }
    if options.accuracy == Accuracy::Fast {
        command.arg("--psm").arg("11");
    }
    if !options.language_correction {
        command
            .arg("-c")
            .arg("load_system_dawg=0")
            .arg("-c")
            .arg("load_freq_dawg=0");
    }
    command.arg("tsv");

    let mut child = command.spawn().map_err(spawn_error)?;
    child
        .stdin
        .take()
        .ok_or_else(|| {
            Error::Io(IoError::new(
                ErrorKind::BrokenPipe,
                "Tesseract stdin was not available",
            ))
        })?
        .write_all(&pgm(&prepared.image))
        .map_err(|error| {
            Error::Io(IoError::new(
                error.kind(),
                format!("writing image to Tesseract failed: {error}"),
            ))
        })?;
    let output = child.wait_with_output().map_err(|error| {
        Error::Io(IoError::new(
            error.kind(),
            format!("waiting for Tesseract failed: {error}"),
        ))
    })?;
    if !output.status.success() {
        return Err(command_error("recognize text", &output.stderr));
    }
    let tsv = String::from_utf8(output.stdout)
        .map_err(|error| Error::Codec(format!("Tesseract returned non-UTF-8 TSV: {error}")))?;
    parse_tsv(&tsv, prepared.upscale, prepared.source_size, frame)
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
        let installed_tag = installed
            .iter()
            .find(|tag| tag.eq_ignore_ascii_case(requested_tag))
            .or_else(|| {
                tesseract_language_alias(requested_tag)
                    .and_then(|alias| installed.iter().find(|tag| tag.eq_ignore_ascii_case(alias)))
            });
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
                 models: {}. Install the matching `tesseract-ocr-<lang>`, \
                 `tesseract-langpack-<lang>`, or `tesseract-data-<lang>` package",
                if installed.is_empty() {
                    "none".to_string()
                } else {
                    installed.join(", ")
                }
            ),
        });
    }
    Ok(selected)
}

fn tesseract_language_alias(tag: &str) -> Option<&'static str> {
    let normalized = tag.replace('_', "-").to_ascii_lowercase();
    let base = normalized.split('-').next().unwrap_or(normalized.as_str());
    match (base, normalized.as_str()) {
        ("zh", value) if value.contains("hant") || value.ends_with("-tw") => Some("chi_tra"),
        ("zh", _) => Some("chi_sim"),
        ("en", _) => Some("eng"),
        ("de", _) => Some("deu"),
        ("fr", _) => Some("fra"),
        ("es", _) => Some("spa"),
        ("it", _) => Some("ita"),
        ("pt", _) => Some("por"),
        ("nl", _) => Some("nld"),
        ("pl", _) => Some("pol"),
        ("ru", _) => Some("rus"),
        ("uk", _) => Some("ukr"),
        ("ja", _) => Some("jpn"),
        ("ko", _) => Some("kor"),
        ("ar", _) => Some("ara"),
        ("hi", _) => Some("hin"),
        ("tr", _) => Some("tur"),
        ("sv", _) => Some("swe"),
        ("no", _) => Some("nor"),
        ("da", _) => Some("dan"),
        ("fi", _) => Some("fin"),
        ("cs", _) => Some("ces"),
        ("el", _) => Some("ell"),
        ("he", _) => Some("heb"),
        ("vi", _) => Some("vie"),
        ("th", _) => Some("tha"),
        _ => None,
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
            why: "the `tesseract` executable was not found. Install `tesseract-ocr` on \
                  Debian/Ubuntu, `tesseract` plus a `tesseract-langpack-*` on Fedora, or \
                  `tesseract` plus `tesseract-data-*` on Arch Linux"
                .to_string(),
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
                "a required Tesseract language model is missing. Install the matching distro \
                 language-data package. Tesseract reported: {detail}"
            ),
        }
    } else {
        Error::Platform(format!("Tesseract could not {action}: {detail}"))
    }
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
