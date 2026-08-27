//! Synthesising ICC profiles for the colour spaces Scrozz captures.
//!
//! # Why generate profiles instead of shipping them
//!
//! Screenshots are one of the few places colour management is immediately
//! visible. A Display P3 capture written out untagged is interpreted as sRGB by
//! any colour-managed viewer, and every saturated colour shifts: saturated reds
//! and greens are the worst affected because that is exactly where P3 extends
//! beyond sRGB. Users do not report this as "the colour profile was dropped";
//! they report it as "this app produces bad screenshots".
//!
//! So a profile must be embedded. The alternative to generating one is shipping
//! binary `.icc` blobs in the repository, which are opaque to review, awkward to
//! licence, and impossible to test beyond a checksum. The profiles needed here
//! are all *matrix-shaper* profiles — three primaries, a white point, and one
//! transfer curve — which is a few hundred bytes of well-specified structure.
//! Generating them means the primaries are visible as source, the maths is
//! testable against published values, and output is byte-for-byte deterministic.
//!
//! # What is generated
//!
//! An ICC v4.3 `mntr`/RGB/XYZ display profile with the tags a matrix-shaper
//! profile requires: `desc`, `cprt`, `wtpt`, `chad`, `rXYZ`/`gXYZ`/`bXYZ` and
//! `rTRC`/`gTRC`/`bTRC`.
//!
//! Two details are easy to get wrong and are worth stating. The profile
//! connection space is fixed at the D50 illuminant, but every space here is
//! defined at D65, so the primaries are Bradford-adapted to D50 and the
//! adaptation matrix is recorded in `chad` — omitting it makes the profile
//! self-inconsistent and some CMMs will refuse it. And the transfer curves are
//! `para` (parametric) rather than sampled `curv` tables, so they are exact
//! rather than interpolated, and 32 bytes instead of two kilobytes.
//!
//! [`ColorSpace::Unknown`] deliberately produces no profile. Embedding sRGB
//! because the backend could not determine the space is a lie that a viewer will
//! act on; leaving it untagged at least lets the viewer apply its own default.

use scrozz_core::ColorSpace;

/// CIE *xy* chromaticities describing an RGB colour space.
#[derive(Debug, Clone, Copy, PartialEq)]
struct Primaries {
    red: [f64; 2],
    green: [f64; 2],
    blue: [f64; 2],
    white: [f64; 2],
}

/// D65, the white point of sRGB, Display P3 and Rec. 2020 alike.
const D65: [f64; 2] = [0.312_7, 0.329_0];

/// D50, the ICC profile connection space illuminant. Not negotiable: the
/// specification fixes it, which is why `chad` exists at all.
const D50_XYZ: [f64; 3] = [0.964_202_880, 1.0, 0.824_905_395];

const SRGB: Primaries = Primaries {
    red: [0.640, 0.330],
    green: [0.300, 0.600],
    blue: [0.150, 0.060],
    white: D65,
};

const DISPLAY_P3: Primaries = Primaries {
    red: [0.680, 0.320],
    green: [0.265, 0.690],
    blue: [0.150, 0.060],
    white: D65,
};

const REC2020: Primaries = Primaries {
    red: [0.708, 0.292],
    green: [0.170, 0.797],
    blue: [0.131, 0.046],
    white: D65,
};

/// A parametric tone curve, ICC `para` function type 3.
///
/// `Y = (a·X + b)^g` for `X >= d`, and `Y = c·X` below it. This is the shape
/// both the sRGB and the Rec. 709 transfer functions take; only the constants
/// differ.
#[derive(Debug, Clone, Copy, PartialEq)]
struct ToneCurve {
    g: f64,
    a: f64,
    b: f64,
    c: f64,
    d: f64,
}

/// The sRGB transfer function, shared by sRGB and Display P3.
///
/// Display P3 is a wider gamut with *the same* curve — a frequent source of
/// confusion, and the reason P3 is not simply "sRGB with more saturation".
const SRGB_CURVE: ToneCurve = ToneCurve {
    g: 2.4,
    a: 1.0 / 1.055,
    b: 0.055 / 1.055,
    c: 1.0 / 12.92,
    d: 0.040_45,
};

/// The Rec. 709 transfer function, which Rec. 2020 inherits for SDR signals.
const REC709_CURVE: ToneCurve = ToneCurve {
    g: 1.0 / 0.45,
    a: 1.0 / 1.099,
    b: 0.099 / 1.099,
    c: 1.0 / 4.5,
    d: 0.081,
};

/// The ICC profile for a colour space, or `None` when none should be embedded.
///
/// Profiles are a few hundred bytes and are rebuilt per call; if that ever shows
/// up in a profile, the three possible results are trivially cacheable.
#[must_use]
pub fn profile_for(space: ColorSpace) -> Option<Vec<u8>> {
    let (primaries, curve, name) = match space {
        ColorSpace::Srgb => (SRGB, SRGB_CURVE, "sRGB"),
        ColorSpace::DisplayP3 => (DISPLAY_P3, SRGB_CURVE, "Display P3"),
        ColorSpace::Rec2020 => (REC2020, REC709_CURVE, "Rec. 2020"),
        // Not an oversight. See the module documentation: an unknown space
        // tagged sRGB is worse than an untagged one.
        ColorSpace::Unknown => return None,
    };
    Some(build(&primaries, curve, name))
}

// ---------------------------------------------------------------------------
// Colour maths
// ---------------------------------------------------------------------------

/// Row-major 3×3.
type Matrix = [[f64; 3]; 3];

/// The Bradford cone response matrix, the standard basis for chromatic
/// adaptation and what every real display profile's `chad` tag is built from.
const BRADFORD: Matrix = [
    [0.895_1, 0.266_4, -0.161_4],
    [-0.750_2, 1.713_5, 0.036_7],
    [0.038_9, -0.068_5, 1.029_6],
];

fn multiply(a: &Matrix, b: &Matrix) -> Matrix {
    let mut out = [[0.0; 3]; 3];
    for (i, row) in out.iter_mut().enumerate() {
        for (j, cell) in row.iter_mut().enumerate() {
            *cell = (0..3).map(|k| a[i][k] * b[k][j]).sum();
        }
    }
    out
}

fn apply(m: &Matrix, v: [f64; 3]) -> [f64; 3] {
    let mut out = [0.0; 3];
    for (i, o) in out.iter_mut().enumerate() {
        *o = (0..3).map(|k| m[i][k] * v[k]).sum();
    }
    out
}

/// Inverts a 3×3 by the adjugate.
///
/// # Panics
///
/// Panics on a singular matrix. Every matrix inverted here is built from
/// non-degenerate primaries defined as constants above, so a singular one would
/// mean a typo in a constant — which should stop the build's tests, not silently
/// produce a wrong profile.
fn invert(m: &Matrix) -> Matrix {
    let det = m[0][0] * (m[1][1] * m[2][2] - m[1][2] * m[2][1])
        - m[0][1] * (m[1][0] * m[2][2] - m[1][2] * m[2][0])
        + m[0][2] * (m[1][0] * m[2][1] - m[1][1] * m[2][0]);
    assert!(det.abs() > 1e-12, "singular matrix: {m:?}");

    let mut out = [[0.0; 3]; 3];
    for (i, row) in out.iter_mut().enumerate() {
        for (j, cell) in row.iter_mut().enumerate() {
            // Cofactor of (j, i) — transposed, giving the adjugate.
            let (r0, r1) = ((j + 1) % 3, (j + 2) % 3);
            let (c0, c1) = ((i + 1) % 3, (i + 2) % 3);
            *cell = (m[r0][c0] * m[r1][c1] - m[r0][c1] * m[r1][c0]) / det;
        }
    }
    out
}

/// CIE *xy* to XYZ at unit luminance.
fn xy_to_xyz(xy: [f64; 2]) -> [f64; 3] {
    let [x, y] = xy;
    [x / y, 1.0, (1.0 - x - y) / y]
}

/// The RGB-to-XYZ matrix for a set of primaries, at their own white point.
///
/// Each primary's chromaticity fixes only its *direction* in XYZ; the scale
/// factors are whatever makes `RGB = (1,1,1)` land exactly on the white point.
fn rgb_to_xyz(p: &Primaries) -> Matrix {
    let r = xy_to_xyz(p.red);
    let g = xy_to_xyz(p.green);
    let b = xy_to_xyz(p.blue);
    let directions: Matrix = [[r[0], g[0], b[0]], [r[1], g[1], b[1]], [r[2], g[2], b[2]]];

    let scales = apply(&invert(&directions), xy_to_xyz(p.white));
    let mut out = directions;
    for row in &mut out {
        for (j, cell) in row.iter_mut().enumerate() {
            *cell *= scales[j];
        }
    }
    out
}

/// The Bradford adaptation matrix taking `source` white to `destination` white.
fn bradford_adaptation(source: [f64; 3], destination: [f64; 3]) -> Matrix {
    let src_cone = apply(&BRADFORD, source);
    let dst_cone = apply(&BRADFORD, destination);
    let ratio: Matrix = [
        [dst_cone[0] / src_cone[0], 0.0, 0.0],
        [0.0, dst_cone[1] / src_cone[1], 0.0],
        [0.0, 0.0, dst_cone[2] / src_cone[2]],
    ];
    multiply(&invert(&BRADFORD), &multiply(&ratio, &BRADFORD))
}

// ---------------------------------------------------------------------------
// ICC serialisation
// ---------------------------------------------------------------------------

/// ICC's `s15Fixed16Number`: signed, 16 fractional bits.
///
/// Signed matters. Display P3's red primary has a *negative* Z component once
/// adapted to D50 — real profiles contain it — and an unsigned encoding would
/// mangle it.
fn s15_fixed16(v: f64) -> i32 {
    (v * 65536.0)
        .round()
        .clamp(f64::from(i32::MIN), f64::from(i32::MAX)) as i32
}

fn push_s15(out: &mut Vec<u8>, v: f64) {
    out.extend_from_slice(&s15_fixed16(v).to_be_bytes());
}

fn xyz_tag(v: [f64; 3]) -> Vec<u8> {
    let mut t = Vec::with_capacity(20);
    t.extend_from_slice(b"XYZ ");
    t.extend_from_slice(&0u32.to_be_bytes());
    for c in v {
        push_s15(&mut t, c);
    }
    t
}

fn curve_tag(c: ToneCurve) -> Vec<u8> {
    let mut t = Vec::with_capacity(32);
    t.extend_from_slice(b"para");
    t.extend_from_slice(&0u32.to_be_bytes());
    t.extend_from_slice(&3u16.to_be_bytes()); // function type 3
    t.extend_from_slice(&0u16.to_be_bytes()); // reserved
    for v in [c.g, c.a, c.b, c.c, c.d] {
        push_s15(&mut t, v);
    }
    t
}

fn matrix_tag(m: &Matrix) -> Vec<u8> {
    let mut t = Vec::with_capacity(8 + 36);
    t.extend_from_slice(b"sf32");
    t.extend_from_slice(&0u32.to_be_bytes());
    for row in m {
        for v in row {
            push_s15(&mut t, *v);
        }
    }
    t
}

/// A `mluc` multi-localised Unicode string, the v4 type for `desc` and `cprt`.
fn text_tag(s: &str) -> Vec<u8> {
    let utf16: Vec<u8> = s.encode_utf16().flat_map(u16::to_be_bytes).collect();
    let mut t = Vec::with_capacity(28 + utf16.len());
    t.extend_from_slice(b"mluc");
    t.extend_from_slice(&0u32.to_be_bytes());
    t.extend_from_slice(&1u32.to_be_bytes()); // one record
    t.extend_from_slice(&12u32.to_be_bytes()); // record size
    t.extend_from_slice(b"enUS");
    t.extend_from_slice(&(utf16.len() as u32).to_be_bytes());
    t.extend_from_slice(&28u32.to_be_bytes()); // header + table = 16 + 12
    t.extend_from_slice(&utf16);
    t
}

/// Assembles the header, tag table and tag data.
fn build(primaries: &Primaries, curve: ToneCurve, name: &str) -> Vec<u8> {
    let adaptation = bradford_adaptation(xy_to_xyz(primaries.white), D50_XYZ);
    let native = rgb_to_xyz(primaries);
    let adapted = multiply(&adaptation, &native);
    let column = |j: usize| [adapted[0][j], adapted[1][j], adapted[2][j]];

    let curve_bytes = curve_tag(curve);
    let tags: Vec<(&[u8; 4], Vec<u8>)> = vec![
        (b"desc", text_tag(&format!("Scrozz {name}"))),
        (b"cprt", text_tag("Public Domain")),
        // The white point of a v4 display profile is the PCS illuminant, not the
        // medium's own white; `chad` below carries the difference.
        (b"wtpt", xyz_tag(D50_XYZ)),
        (b"chad", matrix_tag(&adaptation)),
        (b"rXYZ", xyz_tag(column(0))),
        (b"gXYZ", xyz_tag(column(1))),
        (b"bXYZ", xyz_tag(column(2))),
        (b"rTRC", curve_bytes.clone()),
        (b"gTRC", curve_bytes.clone()),
        (b"bTRC", curve_bytes),
    ];

    const HEADER: usize = 128;
    let table_len = 4 + tags.len() * 12;
    let mut table = Vec::with_capacity(table_len);
    let mut data = Vec::new();
    table.extend_from_slice(&(tags.len() as u32).to_be_bytes());

    for (signature, bytes) in &tags {
        let offset = HEADER + table_len + data.len();
        table.extend_from_slice(*signature);
        table.extend_from_slice(&(offset as u32).to_be_bytes());
        table.extend_from_slice(&(bytes.len() as u32).to_be_bytes());
        data.extend_from_slice(bytes);
        // Tag data elements start on a four-byte boundary. The declared size
        // excludes the padding, which is why it is added after the table entry.
        data.resize(data.len().next_multiple_of(4), 0);
    }

    let mut out = Vec::with_capacity(HEADER + table.len() + data.len());
    out.extend_from_slice(&((HEADER + table.len() + data.len()) as u32).to_be_bytes());
    out.extend_from_slice(&0u32.to_be_bytes()); // preferred CMM: none
    out.extend_from_slice(&0x0430_0000u32.to_be_bytes()); // version 4.3
    out.extend_from_slice(b"mntr"); // display device
    out.extend_from_slice(b"RGB ");
    out.extend_from_slice(b"XYZ ");
    // A fixed timestamp. Encoding the same frame twice must produce identical
    // bytes: it makes output diffable, caches sound, and tests exact.
    for field in [2024u16, 1, 1, 0, 0, 0] {
        out.extend_from_slice(&field.to_be_bytes());
    }
    out.extend_from_slice(b"acsp");
    out.extend_from_slice(&0u32.to_be_bytes()); // primary platform: none
    out.extend_from_slice(&0u32.to_be_bytes()); // flags
    out.extend_from_slice(&0u32.to_be_bytes()); // device manufacturer
    out.extend_from_slice(&0u32.to_be_bytes()); // device model
    out.extend_from_slice(&0u64.to_be_bytes()); // device attributes
    out.extend_from_slice(&0u32.to_be_bytes()); // rendering intent: perceptual
    for c in D50_XYZ {
        push_s15(&mut out, c);
    }
    out.extend_from_slice(&0u32.to_be_bytes()); // profile creator
    out.extend_from_slice(&[0u8; 16]); // profile ID: optional, left zero
    out.extend_from_slice(&[0u8; 28]); // reserved
    debug_assert_eq!(out.len(), HEADER);

    out.extend_from_slice(&table);
    out.extend_from_slice(&data);
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn close(a: f64, b: f64, tolerance: f64) -> bool {
        (a - b).abs() <= tolerance
    }

    #[test]
    fn bradford_inverse_is_an_inverse() {
        let identity = multiply(&BRADFORD, &invert(&BRADFORD));
        for i in 0..3 {
            for j in 0..3 {
                let expected = if i == j { 1.0 } else { 0.0 };
                assert!(close(identity[i][j], expected, 1e-12), "{identity:?}");
            }
        }
    }

    #[test]
    fn srgb_primaries_match_the_published_profile() {
        // The rXYZ/gXYZ/bXYZ of the canonical sRGB IEC61966-2.1 profile.
        let m = multiply(
            &bradford_adaptation(xy_to_xyz(D65), D50_XYZ),
            &rgb_to_xyz(&SRGB),
        );
        let expected = [
            [0.4360, 0.3851, 0.1431],
            [0.2225, 0.7169, 0.0606],
            [0.0139, 0.0971, 0.7141],
        ];
        for i in 0..3 {
            for j in 0..3 {
                assert!(
                    close(m[i][j], expected[i][j], 5e-4),
                    "row {i} col {j}: {m:?}"
                );
            }
        }
    }

    #[test]
    fn display_p3_red_keeps_its_negative_z() {
        // Apple's Display P3 profile really does store a negative bXYZ component
        // for red. If the fixed-point encoding were unsigned this would wrap to
        // an enormous positive value and every red would be wrong.
        let m = multiply(
            &bradford_adaptation(xy_to_xyz(D65), D50_XYZ),
            &rgb_to_xyz(&DISPLAY_P3),
        );
        assert!(close(m[2][0], -0.0011, 5e-4), "{:?}", m[2][0]);
        assert!(s15_fixed16(m[2][0]) < 0);
    }

    #[test]
    fn white_maps_to_the_pcs_illuminant() {
        // The whole point of the adaptation: RGB (1,1,1) must land on D50.
        for p in [SRGB, DISPLAY_P3, REC2020] {
            let m = multiply(
                &bradford_adaptation(xy_to_xyz(p.white), D50_XYZ),
                &rgb_to_xyz(&p),
            );
            let white = apply(&m, [1.0, 1.0, 1.0]);
            for c in 0..3 {
                assert!(close(white[c], D50_XYZ[c], 1e-6), "{white:?}");
            }
        }
    }
}
