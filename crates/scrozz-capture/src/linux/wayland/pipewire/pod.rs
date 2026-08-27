//! SPA POD encoding and decoding, by hand.
//!
//! # Why this exists rather than a binding
//!
//! PipeWire's parameter language is SPA POD: a small self-describing binary
//! format. The C library ships a builder for it — and that builder is
//! `static inline` in `spa/pod/builder.h`, every function of it. There are no
//! `spa_pod_builder_*` symbols in `libspa-0.2.so` to call, because they were
//! never compiled into it. A binding generator can reach them only by compiling
//! the headers, which is precisely the `bindgen`/`libclang`/`pkg-config` chain
//! this crate declines to take on (see [`super`]).
//!
//! So the format is implemented here, in safe Rust, from the header
//! definitions. That is less alarming than it sounds: POD is a fixed, versioned
//! wire format with four rules, and getting it wrong is *visible* — the server
//! rejects the parameter rather than silently misbehaving. It is also the same
//! bargain `x11/wire.rs` already takes for RandR, and for the same reason: byte
//! layout is testable on any machine, and this file is tested on all three.
//!
//! # The format, in full
//!
//! Every POD is a header followed by a body:
//!
//! ```text
//! struct spa_pod { uint32_t size;   /* of the BODY only */
//!                  uint32_t type; } /* one of the ids in `kind` */
//! ```
//!
//! Four rules govern everything else:
//!
//! 1. A POD's total footprint is `8 + size`, **rounded up to a multiple of 8**.
//!    The padding is not counted in `size`.
//! 2. Inside an `Object`, each property is `{ u32 key; u32 flags; }` followed by
//!    a complete padded POD. Since the pair is 8 bytes and the value is padded
//!    to 8, properties stay 8-aligned without extra work.
//! 3. Inside a `Choice` or an `Array`, the alternatives are written **body-only
//!    and unpadded**, packed at exactly the child's `size`. This is the rule
//!    that is easy to get wrong; it comes from `spa_pod_builder_pad` returning
//!    early while the builder is in `SPA_POD_BUILDER_FLAG_BODY`.
//! 4. All integers are native-endian.
//!
//! # What is decoded
//!
//! Only enough to read a fixated `Format` object back out of `param_changed`:
//! find a property by key, and read it as an id, a rectangle or a fraction. The
//! server may answer with the value wrapped in a single-alternative `Choice`
//! rather than bare, so the readers unwrap that transparently — a real
//! difference between compositors, and one that would otherwise look like a
//! missing property.

/// POD type ids, from `enum spa_type` in `spa/utils/type.h`.
pub mod kind {
    /// `SPA_TYPE_None`.
    pub const NONE: u32 = 1;
    /// `SPA_TYPE_Bool`.
    pub const BOOL: u32 = 2;
    /// `SPA_TYPE_Id` — a 32-bit enumeration member.
    pub const ID: u32 = 3;
    /// `SPA_TYPE_Int`.
    pub const INT: u32 = 4;
    /// `SPA_TYPE_Long`.
    pub const LONG: u32 = 5;
    /// `SPA_TYPE_Rectangle` — `{ u32 width; u32 height; }`.
    pub const RECTANGLE: u32 = 10;
    /// `SPA_TYPE_Fraction` — `{ u32 num; u32 denom; }`.
    pub const FRACTION: u32 = 11;
    /// `SPA_TYPE_Array`.
    pub const ARRAY: u32 = 13;
    /// `SPA_TYPE_Struct`.
    pub const STRUCT: u32 = 14;
    /// `SPA_TYPE_Object`.
    pub const OBJECT: u32 = 15;
    /// `SPA_TYPE_Choice`.
    pub const CHOICE: u32 = 19;
}

/// `enum spa_choice_type`, from `spa/pod/pod.h`.
pub mod choice {
    /// A single fixed value; the alternatives list holds only the default.
    pub const NONE: u32 = 0;
    /// `default, min, max`.
    pub const RANGE: u32 = 1;
    /// `default, min, max, step`.
    pub const STEP: u32 = 2;
    /// `default` followed by every other acceptable value.
    pub const ENUM: u32 = 3;
    /// A bit mask of acceptable flags.
    pub const FLAGS: u32 = 4;
}

/// Rounds a length up to the next multiple of eight.
#[must_use]
pub const fn pad_to_8(len: usize) -> usize {
    len.div_ceil(8) * 8
}

fn checked_pad_to_8(len: usize) -> Option<usize> {
    len.checked_add(7).map(|padded| padded / 8 * 8)
}

/// A POD value's type together with its unpadded body bytes.
///
/// Kept separate from the encoded form because a [`Choice`] needs bodies
/// *without* headers or padding, while everywhere else needs the full padded
/// POD. Carrying both in one type is what makes rule 3 above hard to violate by
/// accident.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Scalar {
    /// The POD type id; one of the constants in [`kind`].
    pub kind: u32,
    /// The body, exactly `size` bytes, native-endian, unpadded.
    pub body: Vec<u8>,
}

impl Scalar {
    /// An `Id`: a member of a SPA enumeration.
    #[must_use]
    pub fn id(value: u32) -> Self {
        Self {
            kind: kind::ID,
            body: value.to_ne_bytes().to_vec(),
        }
    }

    /// An `Int`.
    #[must_use]
    pub fn int(value: i32) -> Self {
        Self {
            kind: kind::INT,
            body: value.to_ne_bytes().to_vec(),
        }
    }

    /// A `Long`.
    #[must_use]
    pub fn long(value: i64) -> Self {
        Self {
            kind: kind::LONG,
            body: value.to_ne_bytes().to_vec(),
        }
    }

    /// A `Rectangle`, which SPA stores as two unsigned 32-bit fields.
    #[must_use]
    pub fn rectangle(width: u32, height: u32) -> Self {
        let mut body = Vec::with_capacity(8);
        body.extend_from_slice(&width.to_ne_bytes());
        body.extend_from_slice(&height.to_ne_bytes());
        Self {
            kind: kind::RECTANGLE,
            body,
        }
    }

    /// A `Fraction`, numerator then denominator.
    #[must_use]
    pub fn fraction(numerator: u32, denominator: u32) -> Self {
        let mut body = Vec::with_capacity(8);
        body.extend_from_slice(&numerator.to_ne_bytes());
        body.extend_from_slice(&denominator.to_ne_bytes());
        Self {
            kind: kind::FRACTION,
            body,
        }
    }

    /// The complete POD: header, body, and padding to an 8-byte boundary.
    #[must_use]
    pub fn encode(&self) -> Vec<u8> {
        let mut out = Vec::with_capacity(pad_to_8(8 + self.body.len()));
        push_header(&mut out, self.body.len(), self.kind);
        out.extend_from_slice(&self.body);
        pad(&mut out);
        out
    }
}

/// A set of acceptable values for one property.
///
/// The server picks from these during format negotiation and reports its choice
/// back as a fixated value.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Choice {
    /// Which flavour of choice; one of the constants in [`choice`].
    pub flavour: u32,
    /// The default first, then the remaining values in the flavour's order.
    ///
    /// Every entry must share a `kind` and a body length, because the child
    /// header that describes them is written once for the whole list.
    pub values: Vec<Scalar>,
}

impl Choice {
    /// One value that negotiation has already fixated.
    #[must_use]
    pub fn fixed(value: Scalar) -> Self {
        Self {
            flavour: choice::NONE,
            values: vec![value],
        }
    }

    /// An enumeration of acceptable values, most-preferred first.
    ///
    /// The first entry doubles as the default, which is what a server that does
    /// not care picks — so preference order is expressed simply by ordering.
    #[must_use]
    pub fn enumerated(values: Vec<Scalar>) -> Self {
        Self {
            flavour: choice::ENUM,
            values,
        }
    }

    /// An inclusive range: `default`, then `min`, then `max`.
    #[must_use]
    pub fn range(default: Scalar, min: Scalar, max: Scalar) -> Self {
        Self {
            flavour: choice::RANGE,
            values: vec![default, min, max],
        }
    }

    /// A bit mask of acceptable flags.
    ///
    /// Unlike an enum or range, SPA encodes a flags choice as one value whose
    /// set bits are the alternatives.
    #[must_use]
    pub fn flags(value: Scalar) -> Self {
        Self {
            flavour: choice::FLAGS,
            values: vec![value],
        }
    }

    /// The complete POD, or `None` if the values disagree about type or width.
    ///
    /// A mismatched list cannot be encoded at all — the child header describes
    /// every entry — so this is a genuine failure rather than something to
    /// paper over. In practice the constructors above make it unreachable.
    #[must_use]
    pub fn encode(&self) -> Option<Vec<u8>> {
        let first = self.values.first()?;
        let child_kind = first.kind;
        let child_size = first.body.len();
        if self
            .values
            .iter()
            .any(|value| value.kind != child_kind || value.body.len() != child_size)
        {
            return None;
        }

        let values_len = child_size.checked_mul(self.values.len())?;
        let body_len = 16usize.checked_add(values_len)?;
        let total_len = 8usize.checked_add(body_len)?;
        let mut out = Vec::with_capacity(checked_pad_to_8(total_len)?);
        push_header(&mut out, body_len, kind::CHOICE);
        out.extend_from_slice(&self.flavour.to_ne_bytes());
        out.extend_from_slice(&0u32.to_ne_bytes()); // flags
        push_header(&mut out, child_size, child_kind);
        for value in &self.values {
            // Rule 3: bodies only, packed, no padding.
            out.extend_from_slice(&value.body);
        }
        pad(&mut out);
        Some(out)
    }
}

/// One property of an object: a key and an already-encoded value.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Property {
    /// The key, whose meaning depends on the enclosing object's type.
    pub key: u32,
    /// `spa_pod_prop` flags; zero for everything this crate sends.
    pub flags: u32,
    /// The value as a complete, padded POD.
    pub value: Vec<u8>,
}

impl Property {
    /// A property holding a single fixed value.
    #[must_use]
    pub fn scalar(key: u32, value: &Scalar) -> Self {
        Self {
            key,
            flags: 0,
            value: value.encode(),
        }
    }

    /// A property offering the server a choice.
    ///
    /// Returns `None` for a choice that cannot be encoded; see
    /// [`Choice::encode`].
    #[must_use]
    pub fn choice(key: u32, value: &Choice) -> Option<Self> {
        Some(Self {
            key,
            flags: 0,
            value: value.encode()?,
        })
    }

    /// Key, flags, then the value POD.
    #[must_use]
    pub fn encode(&self) -> Vec<u8> {
        let mut out = Vec::with_capacity(8 + self.value.len());
        out.extend_from_slice(&self.key.to_ne_bytes());
        out.extend_from_slice(&self.flags.to_ne_bytes());
        out.extend_from_slice(&self.value);
        out
    }
}

/// A POD object: a typed, keyed bag of properties.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Object {
    /// `SPA_TYPE_OBJECT_*`; [`super::format::OBJECT_FORMAT`] is the only one
    /// this crate builds.
    pub object_type: u32,
    /// The parameter id this object answers, from `enum spa_param_type`.
    pub id: u32,
    /// The properties, in the order they should appear on the wire.
    pub properties: Vec<Property>,
}

impl Object {
    /// The complete POD: header, `{ type, id }` body, then every property.
    #[must_use]
    pub fn encode(&self) -> Vec<u8> {
        let mut props = Vec::new();
        for property in &self.properties {
            props.extend_from_slice(&property.encode());
        }

        // The body is `spa_pod_object_body` (type and id) plus the properties.
        let body_len = 8 + props.len();
        let mut out = Vec::with_capacity(8 + body_len);
        push_header(&mut out, body_len, kind::OBJECT);
        out.extend_from_slice(&self.object_type.to_ne_bytes());
        out.extend_from_slice(&self.id.to_ne_bytes());
        out.extend_from_slice(&props);
        out
    }
}

fn push_header(out: &mut Vec<u8>, size: usize, kind: u32) {
    let size = u32::try_from(size).unwrap_or(u32::MAX);
    out.extend_from_slice(&size.to_ne_bytes());
    out.extend_from_slice(&kind.to_ne_bytes());
}

fn pad(out: &mut Vec<u8>) {
    out.resize(pad_to_8(out.len()), 0);
}

// ---------------------------------------------------------------------------
// Reading
// ---------------------------------------------------------------------------

/// A borrowed view of one property inside a parsed object.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PropertyRef<'a> {
    /// The property key.
    pub key: u32,
    /// The property flags.
    pub flags: u32,
    /// The value's POD type id.
    pub kind: u32,
    /// The value's body, unpadded.
    pub body: &'a [u8],
}

impl PropertyRef<'_> {
    /// The flavour when this property is a `Choice`.
    #[must_use]
    pub fn choice_flavour(&self) -> Option<u32> {
        (self.kind == kind::CHOICE && self.body.len() >= 4).then(|| read_u32(self.body, 0))
    }

    /// Whether this is a scalar or a choice explicitly marked as fixated.
    ///
    /// A one-entry enum is still an offer, not an agreed value. Callers parsing
    /// a negotiated parameter must check this before accepting the default.
    #[must_use]
    pub fn is_fixated(&self) -> bool {
        self.kind != kind::CHOICE || self.choice_flavour() == Some(choice::NONE)
    }

    /// Reads the value as an `Id`, unwrapping a single-value choice.
    #[must_use]
    pub fn as_id(&self) -> Option<u32> {
        let (ty, body) = self.unwrap_choice()?;
        (ty == kind::ID && body.len() == 4).then(|| read_u32(body, 0))
    }

    /// Reads the value as an `Int`.
    #[must_use]
    pub fn as_int(&self) -> Option<i32> {
        let (ty, body) = self.unwrap_choice()?;
        (ty == kind::INT && body.len() == 4).then(|| read_u32(body, 0).cast_signed())
    }

    /// Reads the value as a `Rectangle`.
    #[must_use]
    pub fn as_rectangle(&self) -> Option<(u32, u32)> {
        let (ty, body) = self.unwrap_choice()?;
        (ty == kind::RECTANGLE && body.len() == 8).then(|| (read_u32(body, 0), read_u32(body, 4)))
    }

    /// Reads the value as a `Fraction`.
    #[must_use]
    pub fn as_fraction(&self) -> Option<(u32, u32)> {
        let (ty, body) = self.unwrap_choice()?;
        (ty == kind::FRACTION && body.len() == 8).then(|| (read_u32(body, 0), read_u32(body, 4)))
    }

    /// The value's type and body, following one level of `Choice` to its
    /// current/default value.
    ///
    /// A fixated format normally arrives with bare values, but several
    /// compositors send `Choice(None)` wrappers instead. Treating those as
    /// "wrong type" would look exactly like a compositor that omitted the
    /// property, which is a diagnosis this code should never have to make.
    fn unwrap_choice(&self) -> Option<(u32, &[u8])> {
        if self.kind != kind::CHOICE {
            return Some((self.kind, self.body));
        }
        if self.body.len() < 16 {
            return None;
        }
        let flavour = read_u32(self.body, 0);
        let child_size = read_u32(self.body, 8) as usize;
        let child_kind = read_u32(self.body, 12);
        if child_size == 0 {
            return None;
        }
        let values_len = self.body.len().checked_sub(16)?;
        let value_count = values_len.checked_div(child_size)?;
        if value_count == 0 || values_len % child_size != 0 {
            return None;
        }
        if value_count != 1 {
            // Generic callers may inspect a one-option offer. Negotiated-format
            // callers additionally require `is_fixated`, so its default cannot
            // be mistaken for an agreement.
            return None;
        }
        let child_end = 16usize.checked_add(child_size)?;
        let default = self.body.get(16..child_end)?;
        Some((child_kind, default))
    }
}

/// A borrowed view of a parsed POD object.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ObjectRef<'a> {
    /// `SPA_TYPE_OBJECT_*`.
    pub object_type: u32,
    /// The parameter id.
    pub id: u32,
    properties: &'a [u8],
}

impl<'a> ObjectRef<'a> {
    /// Parses a complete object POD.
    ///
    /// Returns `None` for anything that is not a well-formed object, including
    /// a truncated one — this parses memory handed over by another process, so
    /// every length is treated as hostile.
    #[must_use]
    pub fn parse(bytes: &'a [u8]) -> Option<Self> {
        if bytes.len() < 16 {
            return None;
        }
        let size = read_u32(bytes, 0) as usize;
        if read_u32(bytes, 4) != kind::OBJECT || size < 8 {
            return None;
        }
        let body_end = 8usize.checked_add(size)?;
        let body = bytes.get(8..body_end)?;
        let properties = &body[8..];
        if !properties_well_formed(properties) {
            return None;
        }
        Some(Self {
            object_type: read_u32(body, 0),
            id: read_u32(body, 4),
            properties,
        })
    }

    /// Every property, in wire order.
    pub fn properties(&self) -> impl Iterator<Item = PropertyRef<'a>> {
        Properties {
            rest: self.properties,
        }
    }

    /// The first property with a given key.
    #[must_use]
    pub fn property(&self, key: u32) -> Option<PropertyRef<'a>> {
        self.properties().find(|property| property.key == key)
    }
}

fn properties_well_formed(mut rest: &[u8]) -> bool {
    while !rest.is_empty() {
        if rest.len() < 16 {
            return false;
        }
        let size = read_u32(rest, 8) as usize;
        let Some(advance) = size
            .checked_add(8)
            .and_then(checked_pad_to_8)
            .and_then(|len| len.checked_add(8))
        else {
            return false;
        };
        let Some(next) = rest.get(advance..) else {
            return false;
        };
        rest = next;
    }
    true
}

struct Properties<'a> {
    rest: &'a [u8],
}

impl<'a> Iterator for Properties<'a> {
    type Item = PropertyRef<'a>;

    fn next(&mut self) -> Option<Self::Item> {
        if self.rest.len() < 16 {
            self.rest = &[];
            return None;
        }
        let key = read_u32(self.rest, 0);
        let flags = read_u32(self.rest, 4);
        let size = read_u32(self.rest, 8) as usize;
        let kind = read_u32(self.rest, 12);
        let Some(body_end) = 16usize.checked_add(size) else {
            self.rest = &[];
            return None;
        };
        let Some(body) = self.rest.get(16..body_end) else {
            self.rest = &[];
            return None;
        };

        // Rule 1: the next property starts after this value's padding.
        let Some(value_len) = 8usize.checked_add(size) else {
            self.rest = &[];
            return None;
        };
        let Some(advance) = checked_pad_to_8(value_len).and_then(|len| len.checked_add(8)) else {
            self.rest = &[];
            return None;
        };
        self.rest = self.rest.get(advance..).unwrap_or(&[]);
        Some(PropertyRef {
            key,
            flags,
            kind,
            body,
        })
    }
}

fn read_u32(bytes: &[u8], offset: usize) -> u32 {
    let mut buf = [0u8; 4];
    buf.copy_from_slice(&bytes[offset..offset + 4]);
    u32::from_ne_bytes(buf)
}
