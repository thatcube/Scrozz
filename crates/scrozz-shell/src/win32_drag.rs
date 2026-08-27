//! The byte layout Explorer expects for a promised file, built without COM.
//!
//! # Why this module exists before the drag does
//!
//! Windows promised-file drag needs an `IDataObject` offering
//! `CFSTR_FILEDESCRIPTORW` and `CFSTR_FILECONTENTS`, and the vtable plumbing
//! for that — `IDataObject`, `IStream`, `IDropSource`, `DoDragDrop` — is a
//! large piece of `unsafe` that cannot be exercised anywhere but Windows. It is
//! not in this slice, and [`crate::drag`] says so plainly rather than
//! pretending otherwise.
//!
//! But the *hardest* part of that recipe is not the vtables. It is this: a
//! `FILEGROUPDESCRIPTORW` is a fixed binary structure, `#[repr(C, packed(1))]`,
//! whose fields must land on exactly the right byte offsets and whose file name
//! is a fixed 260-`u16` array that must be NUL-terminated. Get one offset wrong
//! and Explorer reports a file with a garbled name, a nonsense size, or nothing
//! at all — a failure that looks like a COM bug and is not one.
//!
//! That part is pure arithmetic over bytes. It needs no Windows, so it is
//! written and tested here, today, on the machine this project is developed on.
//! When the FFI is written it will consume [`encode_file_group_descriptor`] and
//! the layout will already be right.
//!
//! # What is deliberately *not* here
//!
//! No `IStream`, no `IDataObject`, no `DoDragDrop`. Those are honestly reported
//! as unimplemented by [`crate::drag`]. This module makes the remaining work
//! smaller; it does not make it done.

/// `MAX_PATH` — the fixed length of `FILEDESCRIPTORW::cFileName`, in `u16`s.
///
/// Not a suggestion: the field is an array, so the structure's size depends on
/// it and a shorter name still occupies all 260 slots.
pub const MAX_PATH: usize = 260;

/// `sizeof(FILEDESCRIPTORW)`, which is `packed(1)`, so this is the plain sum of
/// its fields with no padding anywhere.
///
/// `4 (dwFlags) + 16 (clsid) + 8 (sizel) + 8 (pointl) + 4 (dwFileAttributes) +
/// 8 * 3 (three FILETIMEs) + 4 + 4 (file size) + 520 (cFileName)`.
pub const FILE_DESCRIPTOR_SIZE: usize = 4 + 16 + 8 + 8 + 4 + 24 + 4 + 4 + MAX_PATH * 2;

/// Byte offset of `cFileName` within a `FILEDESCRIPTORW`.
pub const FILE_NAME_OFFSET: usize = FILE_DESCRIPTOR_SIZE - MAX_PATH * 2;

/// Byte offset of `nFileSizeHigh` within a `FILEDESCRIPTORW`.
pub const FILE_SIZE_OFFSET: usize = FILE_NAME_OFFSET - 8;

/// Byte offset of `dwFileAttributes` within a `FILEDESCRIPTORW`.
pub const FILE_ATTRIBUTES_OFFSET: usize = 4 + 16 + 8 + 8;

/// `FD_ATTRIBUTES` — `dwFileAttributes` is meaningful.
pub const FD_ATTRIBUTES: u32 = 0x0000_0004;
/// `FD_FILESIZE` — `nFileSize{High,Low}` are meaningful.
pub const FD_FILESIZE: u32 = 0x0000_0040;
/// `FD_PROGRESSUI` — show a progress dialog while the contents stream.
pub const FD_PROGRESSUI: u32 = 0x0000_4000;
/// `FD_UNICODE` — the name is UTF-16. Always set for the `…W` descriptor.
pub const FD_UNICODE: u32 = 0x8000_0000;

/// `FILE_ATTRIBUTE_NORMAL`.
pub const FILE_ATTRIBUTE_NORMAL: u32 = 0x0000_0080;

/// One promised file, in the terms the descriptor needs.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PromisedFileDescriptor {
    /// The name Explorer will give the dropped file. Not a path.
    pub file_name: String,
    /// The length in bytes, when it is known before the drop.
    ///
    /// `None` is the interesting case and the usual one for Scrozz: a PNG that
    /// has not been encoded yet has no length, and claiming one would be a lie
    /// Explorer would later catch. Omitting it clears `FD_FILESIZE`, which is
    /// exactly what "I will tell you when I know" means in this protocol.
    pub size: Option<u64>,
    /// Whether to ask Explorer for a progress dialog.
    ///
    /// Worth setting whenever the contents are produced lazily, because the
    /// alternative is a copy that appears to hang.
    pub progress_ui: bool,
}

impl PromisedFileDescriptor {
    /// A lazily-produced file of unknown length.
    #[must_use]
    pub fn promised(file_name: impl Into<String>) -> Self {
        Self {
            file_name: file_name.into(),
            size: None,
            progress_ui: true,
        }
    }

    /// The `dwFlags` word this descriptor implies.
    #[must_use]
    pub const fn flags(&self) -> u32 {
        let mut flags = FD_UNICODE | FD_ATTRIBUTES;
        if self.size.is_some() {
            flags |= FD_FILESIZE;
        }
        if self.progress_ui {
            flags |= FD_PROGRESSUI;
        }
        flags
    }
}

/// Why a descriptor could not be encoded.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DescriptorError {
    /// The name is empty. Explorer would show an unnamed file.
    EmptyName,
    /// The name does not fit in `cFileName`, counted in UTF-16 code units with
    /// room for the terminating NUL.
    ///
    /// Carries what it measured, because "too long" without a number is the
    /// kind of error that gets reported as "it just doesn't work".
    NameTooLong {
        /// UTF-16 code units the name needs.
        units: usize,
        /// UTF-16 code units available, NUL included.
        limit: usize,
    },
    /// The name contains a character Windows cannot put in a file name.
    ///
    /// Rejected here rather than left to Explorer, which silently substitutes
    /// or truncates and produces a file the user did not ask for.
    IllegalCharacter(char),
}

impl core::fmt::Display for DescriptorError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Self::EmptyName => write!(f, "a promised file needs a name"),
            Self::NameTooLong { units, limit } => write!(
                f,
                "the file name needs {units} UTF-16 code units but Windows allows {limit} \
                 including the terminator"
            ),
            Self::IllegalCharacter(ch) => {
                write!(f, "{ch:?} cannot appear in a Windows file name")
            }
        }
    }
}

impl core::error::Error for DescriptorError {}

/// Characters Windows forbids in a file name.
const ILLEGAL: [char; 9] = ['<', '>', ':', '"', '/', '\\', '|', '?', '*'];

/// Encodes `file_name` into a `cFileName` array: UTF-16, NUL-terminated, and
/// zero-filled to the end.
///
/// # Errors
///
/// [`DescriptorError`] if the name is empty, too long for `MAX_PATH`, or holds
/// a character Windows forbids.
pub fn encode_file_name(file_name: &str) -> Result<[u16; MAX_PATH], DescriptorError> {
    if file_name.is_empty() {
        return Err(DescriptorError::EmptyName);
    }
    if let Some(ch) = file_name
        .chars()
        .find(|ch| ILLEGAL.contains(ch) || (*ch as u32) < 0x20)
    {
        return Err(DescriptorError::IllegalCharacter(ch));
    }

    let units: Vec<u16> = file_name.encode_utf16().collect();
    // `<` not `<=`: the last slot belongs to the terminator, and a name that
    // filled the array would be handed to Explorer unterminated.
    if units.len() >= MAX_PATH {
        return Err(DescriptorError::NameTooLong {
            units: units.len(),
            limit: MAX_PATH,
        });
    }

    let mut out = [0u16; MAX_PATH];
    out[..units.len()].copy_from_slice(&units);
    Ok(out)
}

/// Encodes a one-file `FILEGROUPDESCRIPTORW` as the bytes to place in an
/// `HGLOBAL`.
///
/// The structure is `#[repr(C, packed(1))]`, so this is a straight little-endian
/// concatenation with no padding: a `u32` count followed by one
/// `FILEDESCRIPTORW`. Everything not named by [`PromisedFileDescriptor`] —
/// `clsid`, `sizel`, `pointl`, the three `FILETIME`s — is left zero, which is
/// what their absent flags mean.
///
/// # Errors
///
/// [`DescriptorError`] from [`encode_file_name`].
pub fn encode_file_group_descriptor(
    file: &PromisedFileDescriptor,
) -> Result<Vec<u8>, DescriptorError> {
    let name = encode_file_name(&file.file_name)?;

    let mut out = Vec::with_capacity(4 + FILE_DESCRIPTOR_SIZE);
    // FILEGROUPDESCRIPTORW::cItems
    out.extend_from_slice(&1u32.to_le_bytes());
    // FILEDESCRIPTORW::dwFlags
    out.extend_from_slice(&file.flags().to_le_bytes());
    // clsid, sizel, pointl — unset, and their flags say so.
    out.extend_from_slice(&[0u8; 16 + 8 + 8]);
    // dwFileAttributes
    out.extend_from_slice(&FILE_ATTRIBUTE_NORMAL.to_le_bytes());
    // ftCreationTime, ftLastAccessTime, ftLastWriteTime
    out.extend_from_slice(&[0u8; 24]);
    // nFileSizeHigh, nFileSizeLow — in that order, which is not the order the
    // names suggest when read as one number.
    let size = file.size.unwrap_or(0);
    #[allow(clippy::cast_possible_truncation)]
    let (high, low) = ((size >> 32) as u32, (size & 0xFFFF_FFFF) as u32);
    out.extend_from_slice(&high.to_le_bytes());
    out.extend_from_slice(&low.to_le_bytes());
    // cFileName
    for unit in name {
        out.extend_from_slice(&unit.to_le_bytes());
    }

    debug_assert_eq!(out.len(), 4 + FILE_DESCRIPTOR_SIZE);
    Ok(out)
}

/// Reads the file name back out of encoded descriptor bytes.
///
/// Exists for tests and for a future `IDataObject::GetData` round-trip check;
/// `None` if the buffer is too short or the name is not terminated.
#[must_use]
pub fn decode_file_name(bytes: &[u8]) -> Option<String> {
    let start = 4 + FILE_NAME_OFFSET;
    let end = start + MAX_PATH * 2;
    let field = bytes.get(start..end)?;
    let units: Vec<u16> = field
        .as_chunks::<2>()
        .0
        .iter()
        .map(|pair| u16::from_le_bytes(*pair))
        .take_while(|unit| *unit != 0)
        .collect();
    // A name that used every slot has no terminator, which is the bug
    // `encode_file_name` refuses to create.
    if units.len() >= MAX_PATH {
        return None;
    }
    String::from_utf16(&units).ok()
}

#[cfg(test)]
mod tests {
    use super::{
        DescriptorError, FD_ATTRIBUTES, FD_FILESIZE, FD_PROGRESSUI, FD_UNICODE,
        FILE_ATTRIBUTE_NORMAL, FILE_ATTRIBUTES_OFFSET, FILE_DESCRIPTOR_SIZE, FILE_NAME_OFFSET,
        FILE_SIZE_OFFSET, MAX_PATH, PromisedFileDescriptor, decode_file_name, encode_file_name,
        encode_file_group_descriptor,
    };

    /// The count field that precedes the single descriptor.
    const HEADER: usize = 4;

    #[test]
    fn the_structure_is_the_size_windows_says_it_is() {
        // `packed(1)` means no padding, so this is checkable by hand and worth
        // checking: everything else in the module is an offset into it.
        assert_eq!(FILE_DESCRIPTOR_SIZE, 592);
        assert_eq!(FILE_NAME_OFFSET, 72);
        assert_eq!(FILE_SIZE_OFFSET, 64);
        assert_eq!(FILE_ATTRIBUTES_OFFSET, 36);
    }

    #[test]
    fn a_descriptor_is_exactly_one_header_and_one_entry() {
        let bytes = encode_file_group_descriptor(&PromisedFileDescriptor::promised("Shot.png"))
            .expect("a plain name encodes");
        assert_eq!(bytes.len(), HEADER + FILE_DESCRIPTOR_SIZE);
        assert_eq!(&bytes[..4], &1u32.to_le_bytes(), "cItems must be 1");
    }

    #[test]
    fn the_name_lands_on_the_offset_windows_will_read() {
        // The failure this catches is the module's whole reason for existing:
        // a name one field early reads as garbage, and looks like a COM fault.
        let bytes = encode_file_group_descriptor(&PromisedFileDescriptor::promised("Shot.png"))
            .expect("encodes");
        assert_eq!(decode_file_name(&bytes).as_deref(), Some("Shot.png"));
    }

    #[test]
    fn an_unknown_length_is_omitted_rather_than_guessed() {
        // A promised PNG has not been encoded yet, so its length is genuinely
        // unknown. Claiming zero *with* FD_FILESIZE would tell Explorer the
        // file is empty; clearing the flag tells it to wait and see.
        let file = PromisedFileDescriptor::promised("Shot.png");
        assert_eq!(file.size, None);
        assert_eq!(file.flags() & FD_FILESIZE, 0, "must not claim a length");
        assert_ne!(file.flags() & FD_PROGRESSUI, 0, "streaming needs progress");
        assert_ne!(file.flags() & FD_UNICODE, 0, "the W descriptor is UTF-16");
        assert_ne!(file.flags() & FD_ATTRIBUTES, 0);
    }

    #[test]
    fn a_known_length_is_split_high_word_first() {
        // The field order is nFileSizeHigh then nFileSizeLow, which is the
        // opposite of how the number reads, and is a classic transposition.
        let file = PromisedFileDescriptor {
            file_name: "Shot.png".to_owned(),
            size: Some(0x0000_00AB_1234_5678),
            progress_ui: false,
        };
        assert_ne!(file.flags() & FD_FILESIZE, 0);
        let bytes = encode_file_group_descriptor(&file).expect("encodes");
        let at = HEADER + FILE_SIZE_OFFSET;
        assert_eq!(&bytes[at..at + 4], &0x0000_00ABu32.to_le_bytes(), "high");
        assert_eq!(
            &bytes[at + 4..at + 8],
            &0x1234_5678u32.to_le_bytes(),
            "low"
        );
    }

    #[test]
    fn attributes_say_normal_file() {
        let bytes = encode_file_group_descriptor(&PromisedFileDescriptor::promised("Shot.png"))
            .expect("encodes");
        let at = HEADER + FILE_ATTRIBUTES_OFFSET;
        assert_eq!(&bytes[at..at + 4], &FILE_ATTRIBUTE_NORMAL.to_le_bytes());
    }

    #[test]
    fn unused_fields_are_zero_because_their_flags_are_clear() {
        let bytes = encode_file_group_descriptor(&PromisedFileDescriptor::promised("Shot.png"))
            .expect("encodes");
        // clsid + sizel + pointl
        assert!(bytes[HEADER + 4..HEADER + 36].iter().all(|b| *b == 0));
        // three FILETIMEs
        assert!(bytes[HEADER + 40..HEADER + 64].iter().all(|b| *b == 0));
    }

    #[test]
    fn the_name_is_nul_terminated_and_zero_filled() {
        let name = encode_file_name("Hi").expect("encodes");
        assert_eq!(&name[..2], &[u16::from(b'H'), u16::from(b'i')]);
        assert!(name[2..].iter().all(|unit| *unit == 0));
    }

    #[test]
    fn a_name_that_would_fill_the_array_is_refused_not_truncated() {
        // Explorer reads until NUL. A name occupying all 260 slots has no NUL,
        // so it would read off the end of the field into the next structure.
        let long = "a".repeat(MAX_PATH);
        match encode_file_name(&long) {
            Err(DescriptorError::NameTooLong { units, limit }) => {
                assert_eq!(units, MAX_PATH);
                assert_eq!(limit, MAX_PATH);
            }
            other => panic!("a name with no room for its terminator must be refused: {other:?}"),
        }
        // One shorter is exactly right, terminator included.
        let fits = "a".repeat(MAX_PATH - 1);
        assert!(encode_file_name(&fits).is_ok());
    }

    #[test]
    fn a_name_is_measured_in_utf16_units_not_characters() {
        // An emoji is one `char` and two UTF-16 units. Counting characters
        // would let a name overrun the array by up to a factor of two.
        let emoji = "\u{1F600}".repeat(130); // 130 chars, 260 units
        assert_eq!(emoji.chars().count(), 130);
        assert!(
            encode_file_name(&emoji).is_err(),
            "260 code units leaves no room for the terminator"
        );
    }

    #[test]
    fn path_separators_are_refused_because_this_is_a_name_not_a_path() {
        // `cFileName` names a file *inside* the drop target. A separator here
        // would let a drag write outside the folder the user dropped on.
        for bad in ["a/b.png", "a\\b.png", "..\\..\\evil.png", "C:evil.png"] {
            assert!(
                matches!(
                    encode_file_name(bad),
                    Err(DescriptorError::IllegalCharacter(_))
                ),
                "{bad} must be refused"
            );
        }
    }

    #[test]
    fn control_characters_are_refused() {
        assert!(matches!(
            encode_file_name("a\nb.png"),
            Err(DescriptorError::IllegalCharacter('\n'))
        ));
    }

    #[test]
    fn an_empty_name_is_refused() {
        assert_eq!(encode_file_name(""), Err(DescriptorError::EmptyName));
    }

    #[test]
    fn errors_say_what_is_wrong_in_words() {
        let too_long = encode_file_name(&"a".repeat(MAX_PATH)).unwrap_err();
        let text = too_long.to_string();
        assert!(text.contains("260"), "{text}");
        assert!(text.contains("UTF-16"), "{text}");
    }

    #[test]
    fn a_unicode_name_survives_the_round_trip() {
        let file = PromisedFileDescriptor::promised("Скриншот \u{1F600}.png");
        let bytes = encode_file_group_descriptor(&file).expect("encodes");
        assert_eq!(decode_file_name(&bytes).as_deref(), Some(&*file.file_name));
    }
}
