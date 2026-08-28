//! The `CF_HDROP` and `CF_UNICODETEXT` byte layouts, built portably.
//!
//! These two formats are Windows clipboard formats, but their *contents* are
//! just bytes with a documented shape, and getting that shape wrong is the
//! classic failure here: an off-by-one in the header offset, a missing second
//! NUL terminator, the wrong endianness. None of those are visible to the type
//! checker, and all of them make a drop silently do nothing.
//!
//! So the layout lives here, with no `windows` types and no `cfg`, and its
//! tests run on every platform on every push. The Windows backend supplies the
//! UTF-16 encoding of the path (which must be `OsStrExt::encode_wide`, because
//! Windows paths are WTF-16 and may contain unpaired surrogates that a lossy
//! `String` conversion would destroy) and this module lays out the bytes.
//!
//! That moves the part most likely to be wrong from "compiled on Windows" to
//! "executed on three platforms", which is the whole point of the layering in
//! `docs/platforms.md`.

/// Size of the `DROPFILES` header that precedes the file list.
///
/// `DROPFILES` is `DWORD pFiles; POINT pt; BOOL fNC; BOOL fWide;` — that is
/// 4 + (4 + 4) + 4 + 4 bytes, with no padding, on both 32- and 64-bit Windows.
/// The struct is not `#[repr(C)]`-transmuted here precisely so that this number
/// is asserted rather than assumed.
pub const DROPFILES_LEN: usize = 20;

/// The `CF_HDROP` payload naming exactly one file.
///
/// The shape is a `DROPFILES` header followed by a double-NUL-terminated list
/// of NUL-terminated UTF-16 paths. With one path that means the units, then a
/// NUL to end the path, then a second NUL to end the list. Dropping the second
/// NUL is the single most common way to make Explorer ignore a drop.
///
/// `units` is the path already encoded as UTF-16 code units, **without** any
/// terminator.
#[must_use]
pub fn hdrop(units: &[u16]) -> Vec<u8> {
    let mut out = Vec::with_capacity(DROPFILES_LEN + (units.len() + 2) * 2);

    out.extend_from_slice(&u32::try_from(DROPFILES_LEN).unwrap_or(20).to_le_bytes());
    out.extend_from_slice(&0i32.to_le_bytes()); // pt.x
    out.extend_from_slice(&0i32.to_le_bytes()); // pt.y
    out.extend_from_slice(&0u32.to_le_bytes()); // fNC — not a non-client drop
    out.extend_from_slice(&1u32.to_le_bytes()); // fWide — the list is UTF-16

    for unit in units {
        out.extend_from_slice(&unit.to_le_bytes());
    }
    out.extend_from_slice(&0u16.to_le_bytes()); // ends this path
    out.extend_from_slice(&0u16.to_le_bytes()); // ends the list

    out
}

/// The `CF_UNICODETEXT` payload: the units, NUL-terminated.
///
/// One terminator, not two — this is a string, not a list.
#[must_use]
pub fn unicode_text(units: &[u16]) -> Vec<u8> {
    let mut out = Vec::with_capacity((units.len() + 1) * 2);
    for unit in units {
        out.extend_from_slice(&unit.to_le_bytes());
    }
    out.extend_from_slice(&0u16.to_le_bytes());
    out
}

/// Read a UTF-16 run back out of a little-endian byte slice, stopping at the
/// first NUL. Used by the tests, and by nothing else.
#[must_use]
pub fn read_units(bytes: &[u8]) -> Vec<u16> {
    bytes
        .as_chunks::<2>()
        .0
        .iter()
        .map(|c| u16::from_le_bytes(*c))
        .take_while(|&u| u != 0)
        .collect()
}

#[cfg(test)]
mod tests {
    use super::{DROPFILES_LEN, hdrop, read_units, unicode_text};

    /// The offset written into the header must be where the list actually
    /// starts, or Explorer reads the header as a filename.
    #[test]
    fn the_header_offset_points_at_the_first_path_unit() {
        let units: Vec<u16> = "C:\\x.png".encode_utf16().collect();
        let bytes = hdrop(&units);

        let declared = u32::from_le_bytes(bytes[0..4].try_into().unwrap()) as usize;

        assert_eq!(declared, DROPFILES_LEN);
        assert_eq!(read_units(&bytes[declared..]), units);
    }

    /// `fWide` must be non-zero or the list is read as ANSI, which turns a
    /// UTF-16 path into mojibake that names no file.
    #[test]
    fn the_list_is_declared_wide() {
        let bytes = hdrop(&"C:\\x.png".encode_utf16().collect::<Vec<_>>());

        let f_nc = u32::from_le_bytes(bytes[12..16].try_into().unwrap());
        let f_wide = u32::from_le_bytes(bytes[16..20].try_into().unwrap());

        assert_eq!(f_nc, 0, "this is not a non-client drop");
        assert_ne!(f_wide, 0, "the path list is UTF-16");
    }

    /// The drop point is the destination's business, not ours.
    #[test]
    fn the_header_point_is_zero() {
        let bytes = hdrop(&"C:\\x.png".encode_utf16().collect::<Vec<_>>());

        assert_eq!(i32::from_le_bytes(bytes[4..8].try_into().unwrap()), 0);
        assert_eq!(i32::from_le_bytes(bytes[8..12].try_into().unwrap()), 0);
    }

    /// The bug that silently breaks every drop: one NUL instead of two.
    #[test]
    fn the_path_list_ends_with_two_nuls() {
        let bytes = hdrop(&"C:\\x.png".encode_utf16().collect::<Vec<_>>());

        assert_eq!(
            &bytes[bytes.len() - 4..],
            &[0, 0, 0, 0],
            "one NUL ends the path, a second ends the list"
        );
    }

    /// Nothing may be lost or reordered on the way through.
    #[test]
    fn the_path_survives_the_round_trip() {
        for path in [
            "C:\\Users\\a\\Pictures\\Scrozz 2024-01-01 at 12.00.00.png",
            "\\\\server\\share\\shot.png",
            "C:\\naïve\\日本語\\🎉.png",
        ] {
            let units: Vec<u16> = path.encode_utf16().collect();
            let bytes = hdrop(&units);

            let back = read_units(&bytes[DROPFILES_LEN..]);

            assert_eq!(
                String::from_utf16(&back).unwrap(),
                path,
                "{path} did not survive"
            );
        }
    }

    /// An empty list is still a well-formed one — it must not underflow.
    #[test]
    fn an_empty_path_still_produces_a_well_formed_header() {
        let bytes = hdrop(&[]);

        assert_eq!(bytes.len(), DROPFILES_LEN + 4);
        assert_eq!(&bytes[DROPFILES_LEN..], &[0, 0, 0, 0]);
    }

    /// A string ends once, not twice — a second NUL would be read as content
    /// by targets that measure to the terminator.
    #[test]
    fn unicode_text_ends_with_exactly_one_nul() {
        let units: Vec<u16> = "C:\\x.png".encode_utf16().collect();
        let bytes = unicode_text(&units);

        assert_eq!(bytes.len(), (units.len() + 1) * 2);
        assert_eq!(&bytes[bytes.len() - 2..], &[0, 0]);
        assert_ne!(
            &bytes[bytes.len() - 4..bytes.len() - 2],
            &[0, 0],
            "only one terminator"
        );
        assert_eq!(read_units(&bytes), units);
    }

    /// Little-endian, explicitly — the byte order is part of the format, not a
    /// property of the machine that happens to be running.
    #[test]
    fn units_are_written_little_endian() {
        // U+1234 is 0x34 0x12 on the wire.
        let bytes = unicode_text(&[0x1234]);

        assert_eq!(&bytes[0..2], &[0x34, 0x12]);
    }
}
