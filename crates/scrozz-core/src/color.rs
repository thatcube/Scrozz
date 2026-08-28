//! Converting colours between the [`ColorSpace`]s a capture can arrive in.
//!
//! # Why this exists
//!
//! A Display P3 screenshot is not sRGB bytes. The same triple `(255, 0, 0)`
//! means a redder red on a P3 display than on an sRGB one, and the difference is
//! large enough to see. That matters here because annotations are authored in
//! sRGB — a colour picker offers sRGB, and the swatch the user chose is an sRGB
//! value — so drawing that triple straight into a P3 buffer paints the wrong
//! colour. The annotation comes out more saturated than the swatch, and it does
//! not match the same annotation on an sRGB capture.
//!
//! So one space is chosen as the *working* space — the capture's own — and
//! everything entering it is converted first. The source pixels are already in
//! it and are never touched, which matters more than it might seem: resampling
//! a screenshot to a different space is a lossy operation the user did not ask
//! for, and it would make the exported file differ from what was captured.
//!
//! # What is deliberately not handled
//!
//! * [`ColorSpace::Unknown`] converts to and from nothing. Converting *from* an
//!   undefined space is not a defined operation, and guessing sRGB would be a
//!   lie that silently shifts colours. Unknown is passed through unchanged.
//! * Conversion is colorimetric with simple clipping, not gamut *mapping*. A
//!   saturated P3 colour shown in an sRGB preview is clipped to the sRGB
//!   boundary rather than compressed towards it, so out-of-gamut regions can
//!   flatten. Export is unaffected — it keeps the working space and its profile
//!   — so this is a preview-fidelity limit, not a data loss.
//! * Everything is done with the D65 primaries below and no chromatic
//!   adaptation, because all three supported spaces are already D65.

use crate::ColorSpace;

/// A 3×3 matrix, row-major.
type Matrix = [[f32; 3]; 3];

/// CIE xy chromaticities for a space's three primaries and its white point.
#[derive(Debug, Clone, Copy)]
struct Primaries {
    red: (f32, f32),
    green: (f32, f32),
    blue: (f32, f32),
    white: (f32, f32),
}

/// D65, the white point of all three supported spaces.
const D65: (f32, f32) = (0.312_7, 0.329);

const SRGB: Primaries = Primaries {
    red: (0.64, 0.33),
    green: (0.30, 0.60),
    blue: (0.15, 0.06),
    white: D65,
};

const DISPLAY_P3: Primaries = Primaries {
    red: (0.680, 0.320),
    green: (0.265, 0.690),
    blue: (0.150, 0.060),
    white: D65,
};

const REC2020: Primaries = Primaries {
    red: (0.708, 0.292),
    green: (0.170, 0.797),
    blue: (0.131, 0.046),
    white: D65,
};

/// The primaries of `space`, or `None` if it has none defined.
const fn primaries(space: ColorSpace) -> Option<Primaries> {
    match space {
        ColorSpace::Srgb => Some(SRGB),
        ColorSpace::DisplayP3 => Some(DISPLAY_P3),
        ColorSpace::Rec2020 => Some(REC2020),
        ColorSpace::Unknown => None,
    }
}

/// Linearises one channel.
///
/// sRGB and Display P3 share the sRGB transfer function — P3 differs only in
/// its primaries, which is exactly why a P3 capture cannot be spotted from its
/// bytes. Rec. 2020 is specified with the Rec. 709 curve.
fn to_linear(value: f32, space: ColorSpace) -> f32 {
    match space {
        ColorSpace::Rec2020 => {
            // Rec. 709 OETF, inverted.
            const ALPHA: f32 = 1.099_296_8;
            const BETA: f32 = 0.018_053_97;
            if value < BETA * 4.5 {
                value / 4.5
            } else {
                ((value + ALPHA - 1.0) / ALPHA).powf(1.0 / 0.45)
            }
        }
        _ => {
            if value <= 0.040_45 {
                value / 12.92
            } else {
                ((value + 0.055) / 1.055).powf(2.4)
            }
        }
    }
}

/// The inverse of [`to_linear`].
fn from_linear(value: f32, space: ColorSpace) -> f32 {
    match space {
        ColorSpace::Rec2020 => {
            const ALPHA: f32 = 1.099_296_8;
            const BETA: f32 = 0.018_053_97;
            if value < BETA {
                value * 4.5
            } else {
                ALPHA * value.powf(0.45) - (ALPHA - 1.0)
            }
        }
        _ => {
            if value <= 0.003_130_8 {
                value * 12.92
            } else {
                1.055 * value.powf(1.0 / 2.4) - 0.055
            }
        }
    }
}

/// The linear-RGB-to-XYZ matrix for `p`, by the standard construction.
fn to_xyz(p: Primaries) -> Matrix {
    let xyz = |(x, y): (f32, f32)| [x / y, 1.0, (1.0 - x - y) / y];
    let (r, g, b) = (xyz(p.red), xyz(p.green), xyz(p.blue));
    let white = xyz(p.white);

    // Scale each primary so the three together sum to the white point.
    let m = [[r[0], g[0], b[0]], [r[1], g[1], b[1]], [r[2], g[2], b[2]]];
    let s = apply(invert(m), white);
    [
        [r[0] * s[0], g[0] * s[1], b[0] * s[2]],
        [r[1] * s[0], g[1] * s[1], b[1] * s[2]],
        [r[2] * s[0], g[2] * s[1], b[2] * s[2]],
    ]
}

fn apply(m: Matrix, v: [f32; 3]) -> [f32; 3] {
    [
        m[0][0] * v[0] + m[0][1] * v[1] + m[0][2] * v[2],
        m[1][0] * v[0] + m[1][1] * v[1] + m[1][2] * v[2],
        m[2][0] * v[0] + m[2][1] * v[1] + m[2][2] * v[2],
    ]
}

fn multiply(a: Matrix, b: Matrix) -> Matrix {
    let mut out = [[0.0; 3]; 3];
    for (i, row) in out.iter_mut().enumerate() {
        for (j, cell) in row.iter_mut().enumerate() {
            *cell = (0..3).map(|k| a[i][k] * b[k][j]).sum();
        }
    }
    out
}

fn invert(m: Matrix) -> Matrix {
    let det = m[0][0] * (m[1][1] * m[2][2] - m[1][2] * m[2][1])
        - m[0][1] * (m[1][0] * m[2][2] - m[1][2] * m[2][0])
        + m[0][2] * (m[1][0] * m[2][1] - m[1][1] * m[2][0]);
    // The primaries above are all linearly independent, so this cannot be zero;
    // guarding rather than dividing by it keeps a future primary set from
    // producing infinities instead of an obvious identity.
    if det.abs() < f32::EPSILON {
        return [[1.0, 0.0, 0.0], [0.0, 1.0, 0.0], [0.0, 0.0, 1.0]];
    }
    let inv = 1.0 / det;
    [
        [
            (m[1][1] * m[2][2] - m[1][2] * m[2][1]) * inv,
            (m[0][2] * m[2][1] - m[0][1] * m[2][2]) * inv,
            (m[0][1] * m[1][2] - m[0][2] * m[1][1]) * inv,
        ],
        [
            (m[1][2] * m[2][0] - m[1][0] * m[2][2]) * inv,
            (m[0][0] * m[2][2] - m[0][2] * m[2][0]) * inv,
            (m[0][2] * m[1][0] - m[0][0] * m[1][2]) * inv,
        ],
        [
            (m[1][0] * m[2][1] - m[1][1] * m[2][0]) * inv,
            (m[0][1] * m[2][0] - m[0][0] * m[2][1]) * inv,
            (m[0][0] * m[1][1] - m[0][1] * m[1][0]) * inv,
        ],
    ]
}

/// A prepared conversion between two spaces.
///
/// Worth building once and reusing: converting a preview means running this over
/// a few million pixels, and rebuilding and inverting the matrices per pixel
/// would dominate the cost.
#[derive(Debug, Clone, Copy)]
pub struct Transform {
    from: ColorSpace,
    to: ColorSpace,
    matrix: Option<Matrix>,
}

impl Transform {
    /// Prepares the conversion from `from` to `to`.
    #[must_use]
    pub fn new(from: ColorSpace, to: ColorSpace) -> Self {
        // Either end being undefined, or both ends being the same, means there
        // is nothing to do — and `is_identity` lets callers skip the work
        // entirely, which is the common case.
        let matrix = match (primaries(from), primaries(to)) {
            (Some(source), Some(target)) if from != to => {
                Some(multiply(invert(to_xyz(target)), to_xyz(source)))
            }
            _ => None,
        };
        Self { from, to, matrix }
    }

    /// A conversion that leaves every colour unchanged.
    ///
    /// For colours that are the same in every space — pure black, pure white,
    /// and any other neutral, since all three spaces share a white point.
    #[must_use]
    pub const fn identity() -> Self {
        Self {
            from: ColorSpace::Unknown,
            to: ColorSpace::Unknown,
            matrix: None,
        }
    }

    /// Whether this conversion leaves every colour unchanged.
    #[must_use]
    pub const fn is_identity(&self) -> bool {
        self.matrix.is_none()
    }

    /// Converts one non-linear RGB triple, each channel in `0.0..=1.0`.
    ///
    /// Values outside the destination gamut are clipped, not mapped. See the
    /// module docs.
    #[must_use]
    pub fn convert(&self, rgb: [f32; 3]) -> [f32; 3] {
        let Some(matrix) = self.matrix else {
            return rgb;
        };
        let linear = [
            to_linear(rgb[0], self.from),
            to_linear(rgb[1], self.from),
            to_linear(rgb[2], self.from),
        ];
        let converted = apply(matrix, linear);
        [
            from_linear(converted[0].clamp(0.0, 1.0), self.to),
            from_linear(converted[1].clamp(0.0, 1.0), self.to),
            from_linear(converted[2].clamp(0.0, 1.0), self.to),
        ]
    }

    /// Converts one non-linear RGB triple of 8-bit channels.
    #[must_use]
    pub fn convert_u8(&self, rgb: [u8; 3]) -> [u8; 3] {
        if self.matrix.is_none() {
            return rgb;
        }
        let out = self.convert([
            f32::from(rgb[0]) / 255.0,
            f32::from(rgb[1]) / 255.0,
            f32::from(rgb[2]) / 255.0,
        ]);
        [
            (out[0] * 255.0).round().clamp(0.0, 255.0) as u8,
            (out[1] * 255.0).round().clamp(0.0, 255.0) as u8,
            (out[2] * 255.0).round().clamp(0.0, 255.0) as u8,
        ]
    }

    /// A 256-entry-per-channel table is not enough for this transform, because
    /// the matrix mixes channels. This builds the cheap part — linearisation —
    /// so a caller converting many pixels can hoist it out of the loop.
    #[must_use]
    pub fn source_linear_table(&self) -> [f32; 256] {
        let mut table = [0.0; 256];
        for (i, slot) in table.iter_mut().enumerate() {
            *slot = to_linear(i as f32 / 255.0, self.from);
        }
        table
    }

    /// Converts a linearised triple, for callers using
    /// [`source_linear_table`](Self::source_linear_table).
    #[must_use]
    pub fn convert_linear(&self, linear: [f32; 3]) -> [f32; 3] {
        let Some(matrix) = self.matrix else {
            return [
                from_linear(linear[0], self.to),
                from_linear(linear[1], self.to),
                from_linear(linear[2], self.to),
            ];
        };
        let converted = apply(matrix, linear);
        [
            from_linear(converted[0].clamp(0.0, 1.0), self.to),
            from_linear(converted[1].clamp(0.0, 1.0), self.to),
            from_linear(converted[2].clamp(0.0, 1.0), self.to),
        ]
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// How far apart two 8-bit channels may be and still count as equal.
    ///
    /// One step covers rounding through a float round trip; the tests that care
    /// about a *difference* assert far larger gaps than this.
    const TOLERANCE: u8 = 1;

    fn close(a: [u8; 3], b: [u8; 3]) -> bool {
        a.iter()
            .zip(b.iter())
            .all(|(x, y)| x.abs_diff(*y) <= TOLERANCE)
    }

    #[test]
    fn the_same_space_is_the_identity() {
        let t = Transform::new(ColorSpace::DisplayP3, ColorSpace::DisplayP3);
        assert!(t.is_identity());
        assert_eq!(t.convert_u8([12, 200, 77]), [12, 200, 77]);
    }

    #[test]
    fn an_unknown_source_is_passed_through_rather_than_guessed() {
        let t = Transform::new(ColorSpace::Unknown, ColorSpace::Srgb);
        assert!(t.is_identity());
        assert_eq!(t.convert_u8([255, 0, 0]), [255, 0, 0]);
    }

    #[test]
    fn an_unknown_destination_is_passed_through_too() {
        let t = Transform::new(ColorSpace::DisplayP3, ColorSpace::Unknown);
        assert!(t.is_identity());
        assert_eq!(t.convert_u8([255, 0, 0]), [255, 0, 0]);
    }

    #[test]
    fn neutrals_are_unchanged_because_the_white_points_match() {
        let t = Transform::new(ColorSpace::Srgb, ColorSpace::DisplayP3);
        for grey in [0u8, 1, 64, 128, 200, 255] {
            assert!(
                close(t.convert_u8([grey, grey, grey]), [grey, grey, grey]),
                "grey {grey} shifted"
            );
        }
    }

    #[test]
    fn srgb_red_is_a_smaller_number_in_display_p3() {
        // P3's red primary is more saturated, so reproducing sRGB's less
        // saturated red needs less of it — and some of the other two.
        let out = Transform::new(ColorSpace::Srgb, ColorSpace::DisplayP3).convert_u8([255, 0, 0]);
        assert!(out[0] < 255, "P3 red stayed at full scale: {out:?}");
        assert!(out[1] > 0, "P3 red gained no green: {out:?}");
        // Published value for sRGB red in Display P3 is about (234, 51, 35).
        assert!((225..=245).contains(&out[0]), "{out:?}");
        assert!((40..=62).contains(&out[1]), "{out:?}");
        assert!((25..=48).contains(&out[2]), "{out:?}");
    }

    #[test]
    fn display_p3_red_clips_at_the_srgb_boundary() {
        // The other direction is out of gamut, so it clips to full red.
        let out = Transform::new(ColorSpace::DisplayP3, ColorSpace::Srgb).convert_u8([255, 0, 0]);
        assert_eq!(out[0], 255, "{out:?}");
        assert_eq!(out[1], 0, "{out:?}");
    }

    #[test]
    fn interior_colours_survive_a_round_trip_through_display_p3() {
        let out = Transform::new(ColorSpace::Srgb, ColorSpace::DisplayP3);
        let back = Transform::new(ColorSpace::DisplayP3, ColorSpace::Srgb);
        // Well inside both gamuts, so nothing clips and the only error is
        // 8-bit quantisation.
        for color in [[17u8, 133, 91], [200, 180, 160], [64, 64, 200]] {
            let round = back.convert_u8(out.convert_u8(color));
            assert!(close(round, color), "{color:?} came back as {round:?}");
        }
    }

    #[test]
    fn a_saturated_primary_round_trips_only_approximately() {
        // The documented limit, pinned rather than papered over. sRGB's green
        // needs a slightly negative red in P3; clipping that to zero is a real
        // change, so the return trip lands a few steps away. Small enough to be
        // invisible, and confined to fully saturated primaries — but it is not
        // lossless, and a future gamut-mapping change should have to update this
        // test deliberately.
        let out = Transform::new(ColorSpace::Srgb, ColorSpace::DisplayP3);
        let back = Transform::new(ColorSpace::DisplayP3, ColorSpace::Srgb);
        for color in [[255u8, 0, 0], [0, 255, 0], [0, 0, 255]] {
            let round = back.convert_u8(out.convert_u8(color));
            let drift = color
                .iter()
                .zip(round.iter())
                .map(|(a, b)| a.abs_diff(*b))
                .max()
                .unwrap_or(0);
            assert!(
                drift <= 4,
                "{color:?} came back as {round:?}, drift {drift}"
            );
        }
    }

    #[test]
    fn rec2020_is_wider_still_than_display_p3() {
        let to_p3 = Transform::new(ColorSpace::Srgb, ColorSpace::DisplayP3).convert_u8([0, 255, 0]);
        let to_2020 = Transform::new(ColorSpace::Srgb, ColorSpace::Rec2020).convert_u8([0, 255, 0]);
        assert!(
            to_2020[1] < to_p3[1],
            "Rec.2020 should need less of its own green than P3 does: {to_2020:?} vs {to_p3:?}"
        );
    }

    #[test]
    fn rec2020_uses_its_own_transfer_curve() {
        // The Rec.709 curve differs from sRGB's, so a neutral survives a
        // round trip only if both directions use the right one.
        let out = Transform::new(ColorSpace::Srgb, ColorSpace::Rec2020);
        let back = Transform::new(ColorSpace::Rec2020, ColorSpace::Srgb);
        assert!(close(
            back.convert_u8(out.convert_u8([128, 128, 128])),
            [128, 128, 128]
        ));
    }

    #[test]
    fn black_and_white_are_exact() {
        let t = Transform::new(ColorSpace::Srgb, ColorSpace::Rec2020);
        assert_eq!(t.convert_u8([0, 0, 0]), [0, 0, 0]);
        assert_eq!(t.convert_u8([255, 255, 255]), [255, 255, 255]);
    }

    #[test]
    fn the_linear_table_agrees_with_the_direct_path() {
        let t = Transform::new(ColorSpace::DisplayP3, ColorSpace::Srgb);
        let table = t.source_linear_table();
        for color in [[10u8, 200, 30], [255, 255, 0], [7, 7, 200]] {
            let direct = t.convert([
                f32::from(color[0]) / 255.0,
                f32::from(color[1]) / 255.0,
                f32::from(color[2]) / 255.0,
            ]);
            let tabled = t.convert_linear([
                table[color[0] as usize],
                table[color[1] as usize],
                table[color[2] as usize],
            ]);
            for (a, b) in direct.iter().zip(tabled.iter()) {
                assert!((a - b).abs() < 1e-4, "{direct:?} vs {tabled:?}");
            }
        }
    }

    #[test]
    fn identity_still_re_encodes_through_convert_linear() {
        // The identity transform has no matrix, but a caller using the table has
        // already linearised — so it still has to encode on the way out.
        let t = Transform::new(ColorSpace::Srgb, ColorSpace::Srgb);
        let table = t.source_linear_table();
        let out = t.convert_linear([table[128], table[128], table[128]]);
        assert!((out[0] - 128.0 / 255.0).abs() < 1e-3, "{out:?}");
    }

    #[test]
    fn the_explicit_identity_changes_nothing() {
        assert!(Transform::identity().is_identity());
        assert_eq!(Transform::identity().convert_u8([1, 2, 3]), [1, 2, 3]);
    }
}
