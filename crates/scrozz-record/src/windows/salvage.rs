//! Pure fragmented-MP4 validation and retention policy.

use std::{
    fs::{File, OpenOptions},
    io::{self, Read, Seek, SeekFrom},
    path::Path,
};

/// What finalisation means for the output file.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Outcome {
    /// Media Foundation finalised normally.
    Complete,
    /// Fragmented MP4 data exists and is reported as a partial recording.
    Salvaged(String),
    /// No useful media reached disk.
    Unusable(String),
}

/// Structurally complete top-level MP4 data.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct Inspection {
    /// Original file size.
    pub file_bytes: u64,
    /// Last safe byte after at least one complete fragment.
    pub truncate_to: u64,
    /// Number of complete `moof`/`mdat` pairs.
    pub complete_fragments: u64,
    /// Whether both initialization boxes are complete.
    pub has_initialization: bool,
}

impl Inspection {
    /// Whether the file contains initialization metadata and encoded media.
    #[must_use]
    pub const fn playable(self) -> bool {
        self.has_initialization && self.complete_fragments != 0 && self.truncate_to != 0
    }
}

/// Reads only top-level box headers, so even multi-gigabyte recordings can be
/// checked without loading media payloads into memory.
pub fn inspect<R: Read + Seek>(reader: &mut R) -> io::Result<Inspection> {
    let file_bytes = reader.seek(SeekFrom::End(0))?;
    let mut offset = 0u64;
    let mut safe_end = 0u64;
    let mut pending_fragment = None;
    let mut has_ftyp = false;
    let mut has_moov = false;
    let mut complete_fragments = 0u64;

    while file_bytes.saturating_sub(offset) >= 8 {
        reader.seek(SeekFrom::Start(offset))?;
        let mut header = [0u8; 16];
        reader.read_exact(&mut header[..8])?;
        let size32 = u32::from_be_bytes(header[..4].try_into().expect("four bytes"));
        let kind: [u8; 4] = header[4..8].try_into().expect("four bytes");
        let (header_bytes, box_bytes) = match size32 {
            0 => (8, file_bytes - offset),
            1 => {
                if file_bytes.saturating_sub(offset) < 16 {
                    break;
                }
                reader.read_exact(&mut header[8..16])?;
                (
                    16,
                    u64::from_be_bytes(header[8..16].try_into().expect("eight bytes")),
                )
            }
            size => (8, u64::from(size)),
        };
        let Some(end) = offset.checked_add(box_bytes) else {
            break;
        };
        if box_bytes < header_bytes || end > file_bytes {
            break;
        }

        match &kind {
            b"ftyp" => has_ftyp = true,
            b"moov" => has_moov = true,
            b"moof" => {
                pending_fragment.get_or_insert(offset);
            }
            b"mdat" if pending_fragment.is_some() => {
                complete_fragments = complete_fragments.saturating_add(1);
                pending_fragment = None;
                safe_end = end;
            }
            _ => {}
        }
        if pending_fragment.is_none() {
            safe_end = end;
        }
        offset = end;
    }

    Ok(Inspection {
        file_bytes,
        truncate_to: pending_fragment.unwrap_or(safe_end).min(file_bytes),
        complete_fragments,
        has_initialization: has_ftyp && has_moov,
    })
}

/// Inspects one recording on disk.
pub fn inspect_file(path: &Path) -> io::Result<Inspection> {
    inspect(&mut File::open(path)?)
}

/// Removes an incomplete top-level box tail while preserving complete media
/// fragments.
pub fn repair_file(path: &Path, inspection: Inspection) -> io::Result<()> {
    if inspection.truncate_to < inspection.file_bytes {
        OpenOptions::new()
            .write(true)
            .open(path)?
            .set_len(inspection.truncate_to)?;
    }
    Ok(())
}

/// Classifies finalisation without hiding a verified partial recording.
#[must_use]
pub fn classify(
    failure: Option<&str>,
    file_bytes: u64,
    samples_written: u64,
    inspection: Option<Inspection>,
) -> Outcome {
    let Some(error) = failure else {
        return Outcome::Complete;
    };

    if let Some(inspection) = inspection.filter(|inspection| inspection.playable())
        && samples_written != 0
    {
        Outcome::Salvaged(format!(
            "recording ended before clean completion ({error}); \
             retained {} bytes with {} complete media fragment(s) \
             and {samples_written} accepted samples",
            inspection.truncate_to, inspection.complete_fragments
        ))
    } else {
        Outcome::Unusable(format!(
            "recording failed ({error}) and no encoded media \
             fragment could be verified in the {file_bytes}-byte output"
        ))
    }
}
