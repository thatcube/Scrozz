//! The bookkeeping half of an OLE data object: which format answers which
//! request, and what happens to the old value when one is replaced.
//!
//! # Why this is a module and not four lines inside the Windows backend
//!
//! A drag source on Windows is not allowed to be a read-only bag of the
//! flavours it meant to offer. The shell's drag-image helper — the thing that
//! makes a thumbnail follow the pointer instead of a generic file cursor —
//! works by *writing into the source's data object*:
//!
//! > To support the drag-and-drop helper object, the data object's `SetData`
//! > and `GetData` implementations must be able to accept and return arbitrary
//! > private formats.
//!
//! `IDragSourceHelper::InitializeFromBitmap` stores `CFSTR_DRAGIMAGEBITS` and
//! its companions through `SetData` and expects to read them back through
//! `GetData`. A data object whose `SetData` returns `E_NOTIMPL` does not get a
//! degraded thumbnail; it gets **no** thumbnail, deterministically, because the
//! helper's very first write fails.
//!
//! So the object has to be a store, and a store has ownership rules: setting a
//! format twice must release what was displaced, and dropping the object must
//! release everything left. Get that wrong on Windows and it leaks an `HGLOBAL`
//! per drag, or — worse — double-frees one.
//!
//! None of that reasoning is Windows-specific, and none of it is checkable by a
//! type system. It is `Vec` bookkeeping with a lifetime rule, so it lives here,
//! free of a single `windows` type, and its tests run on every platform this
//! crate builds for. The Windows backend supplies the one thing that genuinely
//! cannot be portable — an owned `STGMEDIUM` that releases itself — and gets
//! the matching and the replacement semantics already tested.

use std::sync::Arc;

/// The parts of a `FORMATETC` that decide whether an entry answers a request.
///
/// # The target device is part of the identity
///
/// `FORMATETC` carries `ptd`, a `DVTARGETDEVICE*` describing a specific
/// rendering device, so a document can be asked for "this content, as it would
/// print on *that* printer". It is tempting to drop it — a dragged screenshot
/// has exactly one rendering — but dropping it from the *key* is a different
/// claim from ignoring it in the *answer*, and a much worse one: two entries
/// stored under formats that differ only by device would collide, the second
/// silently overwriting the first, and a request naming a device would be
/// handed whichever survived.
///
/// So the device is held here as its raw bytes, and two devices that differ are
/// two entries. But *matching* is not the same question as identity, and the
/// documentation is explicit that a null `ptd` is not simply a fifth device:
///
/// > A **NULL** value is used whenever the specified data format is independent
/// > of the target device or when the caller doesn't care what device is used.
/// > In the latter case, if the data requires a target device, the object should
/// > pick an appropriate default device (often the display for visual
/// > components). Data obtained from an object with a **NULL** target device,
/// > such as most metafiles, is independent of the target device.
///
/// So null means one thing in a stored entry — *this data does not depend on a
/// device* — and a different thing in a request — *any device will do*. Exact
/// equality gets both wrong: it makes device-independent data refuse a request
/// naming a printer, though by definition that data is valid for it, and it
/// makes an indifferent caller refuse the only representation there is, though
/// the object is told to pick a default rather than fail. [`DeviceFit`] is how
/// the two are told apart without ever letting one printer's rendering pass as
/// another's.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct FormatKey {
    /// The clipboard format id — `CF_HDROP`, or whatever `RegisterClipboardFormatW` returned.
    pub format: u16,
    /// `DVASPECT_*`: which view of the data. Almost always `DVASPECT_CONTENT`.
    pub aspect: u32,
    /// Which page of a multi-page format, or `-1` for the whole thing.
    ///
    /// Meaningless for two of the four aspects — see [`Self::answers`].
    pub index: i32,
    /// A *set* of `TYMED_*` bits in a request; a single one in a stored key.
    pub tymed: u32,
    /// The `DVTARGETDEVICE` blob, verbatim, or `None` for a null `ptd`.
    pub device: Option<Arc<[u8]>>,
}

impl FormatKey {
    /// A key for one storage medium of one format, in its ordinary aspect.
    ///
    /// The shape every flavour this crate offers itself takes: whole content,
    /// no paging, no target device.
    #[must_use]
    pub const fn content(format: u16, tymed: u32) -> Self {
        Self {
            format,
            aspect: DVASPECT_CONTENT,
            index: -1,
            tymed,
            device: None,
        }
    }

    /// How well an entry stored under this key answers `request`, if at all.
    ///
    /// Format and aspect must be *equal*; the medium need only *intersect*; the
    /// index is compared only for the aspects it means anything for; the device
    /// is matched by [`DeviceFit`]. The asymmetry is the whole subtlety of
    /// `FORMATETC` and none of it is arbitrary.
    ///
    /// `tymed` is a bitmask of the media the caller will accept, so a caller
    /// asking for `TYMED_HGLOBAL | TYMED_ISTREAM` is saying "either will do",
    /// and an entry supplying one of them answers.
    ///
    /// `lindex` is not always significant. The documentation is explicit:
    ///
    /// > For the aspects DVASPECT_THUMBNAIL and DVASPECT_ICON, lindex is
    /// > ignored.
    ///
    /// Comparing it anyway is a real bug rather than harmless strictness: a
    /// caller asking for an icon with `lindex` left at `0` would be told the
    /// format does not exist, because the icon was stored with the `-1` that
    /// means "all of it". For `DVASPECT_CONTENT` and `DVASPECT_DOCPRINT` the
    /// index *is* significant — `CFSTR_FILECONTENTS` uses it as the zero-based
    /// index of the file wanted — so it is compared there.
    ///
    /// Being lenient about aspect or a significant index would return the wrong
    /// data rather than an error, which is the failure that is hard to notice.
    /// Two *different* devices are equally never interchangeable — a page
    /// composed for one printer is not the other's — so that stays a mismatch.
    #[must_use]
    pub fn fit(&self, request: &Self) -> Option<DeviceFit> {
        if self.format != request.format
            || self.aspect != request.aspect
            || (!aspect_ignores_index(self.aspect) && self.index != request.index)
            || self.tymed & request.tymed == 0
        {
            return None;
        }
        if self.device == request.device {
            return Some(DeviceFit::Exact);
        }
        match (&self.device, &request.device) {
            (None, Some(_)) => Some(DeviceFit::Independent),
            (Some(_), None) => Some(DeviceFit::Default),
            // Two different devices, or the equality above would have fired.
            _ => None,
        }
    }

    /// Whether an entry stored under this key may answer `request` at all.
    ///
    /// The predicate behind [`Self::fit`], for callers that do not need to rank
    /// one candidate against another.
    #[must_use]
    pub fn answers(&self, request: &Self) -> bool {
        self.fit(request).is_some()
    }
}

/// How closely a stored entry's target device answers a request's.
///
/// Ordered best first, so ranking candidates is `min`. The order is not a
/// preference this crate invented; it falls out of what a null `ptd` means on
/// each side, quoted in full on [`FormatKey`].
///
/// * [`Exact`](Self::Exact) — the same device, or both device-free. Nothing
///   beats a representation composed for precisely the device asked about.
/// * [`Independent`](Self::Independent) — a device-free entry answering a
///   request that names a device. Device-independent data "is independent of
///   the target device", so it is genuinely valid for that device; it is second
///   only because an entry made for the device itself is more specific.
/// * [`Default`](Self::Default) — a device-specific entry answering a request
///   that names none. The caller "doesn't care what device is used" and the
///   object "should pick an appropriate default device", so answering is right;
///   it ranks last so that a device-free entry is the default when one exists.
///
/// Two entries reaching the same rank are broken by insertion order, which is
/// the order the offering code wrote them in, so repeated requests never
/// oscillate between representations.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum DeviceFit {
    /// Same device, or neither names one.
    Exact,
    /// Device-independent data answering a device-specific request.
    Independent,
    /// Device-specific data answering a caller that is indifferent.
    Default,
}

/// Whether `lindex` carries no meaning for this aspect.
///
/// See the quotation in [`FormatKey::fit`].
#[must_use]
pub const fn aspect_ignores_index(aspect: u32) -> bool {
    aspect == DVASPECT_THUMBNAIL || aspect == DVASPECT_ICON
}

/// `DVASPECT_CONTENT`, spelled out so this module needs no Windows import.
///
/// Asserted against the real constant in the Windows backend's tests.
pub const DVASPECT_CONTENT: u32 = 1;

/// `DVASPECT_THUMBNAIL`, likewise.
pub const DVASPECT_THUMBNAIL: u32 = 2;

/// `DVASPECT_ICON`, likewise.
pub const DVASPECT_ICON: u32 = 4;

/// `DVASPECT_DOCPRINT`, likewise.
pub const DVASPECT_DOCPRINT: u32 = 8;

/// `TYMED_HGLOBAL`, likewise.
pub const TYMED_HGLOBAL: u32 = 1;

/// Every documented medium, or'd together.
///
/// `TYMED_NULL` is absent because it is zero: the absence of a medium rather
/// than one of them.
pub const TYMED_ANY: u32 = 1 | 2 | 4 | 8 | 16 | 32 | 64;

/// Whether `tymed` names exactly one documented medium.
///
/// A stored medium is one thing. `ReleaseStgMedium` frees it by switching on
/// this field to choose both which arm of the union is live and which of seven
/// quite different release actions to take, so a medium naming two media at
/// once cannot be freed correctly by anybody, and one naming none of them
/// cannot be freed at all.
#[must_use]
pub const fn is_single_medium(tymed: u32) -> bool {
    tymed & TYMED_ANY == tymed && tymed.count_ones() == 1
}

/// The `tymed` an entry should be keyed by, or `None` if it must be refused.
///
/// `offered` is a `FORMATETC::tymed`: a *set*, the media the caller is willing
/// to use. `actual` is a `STGMEDIUM::tymed`: the one thing the medium in hand
/// really is. Keying an entry by the set would make it claim media it cannot
/// supply, so that a query promising a stream is answered by a global handle —
/// a type confusion the receiver has no way to detect before it dereferences.
///
/// So the answer is `actual`, and only when `actual` names a single documented
/// medium that the format offered. `IDataObject::SetData` states the rule
/// without qualification:
///
/// > The type of medium specified in the *pformatetc* and *pmedium* parameters
/// > must be the same. For example, one cannot be a global handle and the other
/// > a stream.
///
/// A medium of `TYMED_NULL` is refused rather than stored. It is documented as
/// "No data is being passed", so a format offering a real medium alongside it
/// is exactly the disagreement the sentence above forbids, and a format
/// offering `TYMED_NULL` too describes data that nothing can ever ask for. No
/// Microsoft documentation gives `SetData` a null medium any other meaning —
/// in particular none establishes it as a request to delete a stored format —
/// so it is reported as `DV_E_TYMED`, "The tymed value is not valid", instead
/// of being accepted into a store where it would silently do nothing.
#[must_use]
pub const fn stored_medium(offered: u32, actual: u32) -> Option<u32> {
    if !is_single_medium(actual) || offered & actual == 0 {
        return None;
    }
    Some(actual)
}

/// The fixed part of a `DVTARGETDEVICE`: one `u32` and four `u16`s.
pub const TARGET_DEVICE_HEADER: usize = 12;

/// The largest target device this code will copy, as a sanity bound.
///
/// A `DVTARGETDEVICE` is a printer driver's `DEVMODE` plus three names. Real
/// ones are a few kilobytes. The bound exists so a malformed `tdSize` asks for
/// a refusal rather than a 4 GiB allocation.
pub const TARGET_DEVICE_MAX: usize = 16 << 20;

/// The least a nonzero `DVTARGETDEVICE` name offset must leave behind it.
///
/// One byte: the terminator of the shortest possible NUL-terminated string.
pub const TARGET_DEVICE_NAME_MIN: usize = 1;

/// The least a nonzero `tdExtDevmodeOffset` must leave behind it.
///
/// Forty bytes: an ANSI `DEVMODE` up to and including `dmDriverExtra`
/// (`dmDeviceName[32]`, then `dmSpecVersion`, `dmDriverVersion`, `dmSize` and
/// `dmDriverExtra`, two bytes each). The wide layout's prefix is longer, at 72,
/// so the smaller of the two is used: the point is to catch an offset that
/// cannot possibly be a device mode, not to guess which build wrote it.
pub const TARGET_DEVICE_DEVMODE_MIN: usize = 40;

/// Validates a `DVTARGETDEVICE` header and returns the whole structure's size.
///
/// `header` must be the first [`TARGET_DEVICE_HEADER`] bytes, little-endian, as
/// they appear in memory. The returned size counts those bytes, so a caller
/// copies exactly that many from the start of the structure.
///
/// This exists because `ptd` arrives as a bare pointer from another process's
/// idea of what a device is, and the length used to copy it comes from inside
/// the very structure being copied. Reading `tdSize` and trusting it is how a
/// buffer overrun happens; every field it could be is checked first.
///
/// The four offsets are checked to be within the structure, with room for
/// something to actually be there. They do not affect the copy, which is
/// byte-for-byte, but an offset pointing outside — or exactly one past the end —
/// is proof the blob is malformed and worth refusing before it is passed on.
///
/// The minima are deliberately the smallest a conforming structure could use,
/// so nothing valid is turned away. A name offset must leave at least one byte,
/// because the fields are documented as pointing at "a NULL-terminated string in
/// the tdData buffer" and a string is at least its terminator; asserting two
/// would assume a character width the structure never states. The devmode
/// offset must leave [`TARGET_DEVICE_DEVMODE_MIN`] bytes, because a consumer has
/// to read the `dmSize` and `dmDriverExtra` fields to learn the real length:
///
/// > The number of bytes to be allocated should be the sum of **dmSize** +
/// > **dmDriverExtra**.
#[must_use]
pub fn target_device_size(header: &[u8]) -> Option<usize> {
    let header: &[u8; TARGET_DEVICE_HEADER] =
        header.get(..TARGET_DEVICE_HEADER)?.try_into().ok()?;

    let field = |at: usize| u16::from_le_bytes([header[at], header[at + 1]]) as usize;
    let size = u32::from_le_bytes([header[0], header[1], header[2], header[3]]) as usize;

    if !(TARGET_DEVICE_HEADER..=TARGET_DEVICE_MAX).contains(&size) {
        return None;
    }
    // Offset zero is the documented "this field is absent"; anything else must
    // land inside the blob, past the header it would otherwise overlap, with
    // room left for the smallest thing it could be pointing at.
    for (at, least) in [
        (4, TARGET_DEVICE_NAME_MIN),
        (6, TARGET_DEVICE_NAME_MIN),
        (8, TARGET_DEVICE_NAME_MIN),
        (10, TARGET_DEVICE_DEVMODE_MIN),
    ] {
        let offset = field(at);
        if offset == 0 {
            continue;
        }
        if offset < TARGET_DEVICE_HEADER || offset.checked_add(least)? > size {
            return None;
        }
    }
    Some(size)
}

/// An ordered, replace-on-set collection of `(key, value)` entries.
///
/// Insertion-ordered rather than a map, because enumeration order is part of
/// the contract a data object exposes: `EnumFormatEtc` is documented to yield
/// formats in the source's order of preference, and a receiver that walks the
/// enumeration takes the first thing it understands. A `HashMap` would make the
/// flavour a drop target picks vary between runs.
///
/// `T` is whatever owns the actual bytes. The Windows backend passes a wrapper
/// around `STGMEDIUM` whose `Drop` calls `ReleaseStgMedium`, which is what makes
/// the replacement and teardown rules below into real resource management
/// rather than bookkeeping.
#[derive(Debug)]
pub struct FormatStore<T> {
    entries: Vec<(FormatKey, T)>,
}

impl<T> Default for FormatStore<T> {
    fn default() -> Self {
        Self::new()
    }
}

impl<T> FormatStore<T> {
    /// An empty store.
    #[must_use]
    pub const fn new() -> Self {
        Self {
            entries: Vec::new(),
        }
    }

    /// Stores `value` under `key`, returning whatever it displaced.
    ///
    /// Replacement is by *exact* key, not by [`FormatKey::answers`]: storing
    /// the same format on a different medium adds an alternative rather than
    /// overwriting one, which is what a caller offering two media means.
    ///
    /// **The displaced value is returned, not dropped here.** The caller
    /// decides when it dies. On Windows that matters more than it looks: a
    /// medium must not be released while a lock is held on the store, because
    /// `ReleaseStgMedium` can re-enter (a medium carrying `pUnkForRelease` calls
    /// out to arbitrary code). Handing the old value back lets the caller drop
    /// it after the borrow ends.
    pub fn set(&mut self, key: FormatKey, value: T) -> Option<T> {
        if let Some(slot) = self.entries.iter_mut().find(|(k, _)| *k == key) {
            return Some(std::mem::replace(&mut slot.1, value));
        }
        self.entries.push((key, value));
        None
    }

    /// The value that answers `request`, if one does.
    ///
    /// Best [`DeviceFit`] wins, insertion order breaks ties.
    #[must_use]
    pub fn get(&self, request: &FormatKey) -> Option<&T> {
        self.best(request).map(|(_, value)| value)
    }

    /// The key an entry answering `request` is stored under.
    ///
    /// Needed because the stored key, not the request, describes the medium the
    /// value actually is — a request for `HGLOBAL | ISTREAM` is answered by an
    /// entry that is one or the other, and a caller about to duplicate it has
    /// to know which. Same for the device: a request naming none can be
    /// answered by an entry that names one.
    #[must_use]
    pub fn key_for(&self, request: &FormatKey) -> Option<FormatKey> {
        self.best(request).map(|(key, _)| key.clone())
    }

    /// The entry that best answers `request`.
    ///
    /// `min_by_key` keeps the first of several equal minima, so two entries of
    /// the same [`DeviceFit`] resolve to the one stored first — the same answer
    /// every time, which matters because a target may query a format and then
    /// fetch it as two separate calls.
    fn best(&self, request: &FormatKey) -> Option<&(FormatKey, T)> {
        self.entries
            .iter()
            .filter_map(|entry| entry.0.fit(request).map(|fit| (fit, entry)))
            .min_by_key(|(fit, _)| *fit)
            .map(|(_, entry)| entry)
    }

    /// Every key, in insertion order.
    pub fn keys(&self) -> impl Iterator<Item = FormatKey> + '_ {
        self.entries.iter().map(|(key, _)| key.clone())
    }

    /// How many entries are held.
    #[must_use]
    pub fn len(&self) -> usize {
        self.entries.len()
    }

    /// Whether nothing is held.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// Removes everything, handing the values back for the caller to drop.
    ///
    /// Same reasoning as [`Self::set`]: teardown of a medium can re-enter, so
    /// the store gives up its contents rather than destroying them under a
    /// borrow it holds.
    pub fn take_all(&mut self) -> Vec<(FormatKey, T)> {
        std::mem::take(&mut self.entries)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use std::cell::Cell;
    use std::rc::Rc;

    /// `TYMED_ISTREAM`, for the intersection tests.
    const TYMED_ISTREAM: u32 = 4;

    /// A value that reports its own destruction.
    ///
    /// Stands in for the Windows `STGMEDIUM` wrapper, whose entire job is to
    /// release exactly once. Everything the real one does that this cannot —
    /// calling `ReleaseStgMedium` — is a single line; *when* it is called is the
    /// part with the bugs in it, and that is what this pins.
    struct Counted {
        deaths: Rc<Cell<usize>>,
    }

    impl Counted {
        fn new(deaths: &Rc<Cell<usize>>) -> Self {
            Self {
                deaths: Rc::clone(deaths),
            }
        }
    }

    impl Drop for Counted {
        fn drop(&mut self) {
            self.deaths.set(self.deaths.get() + 1);
        }
    }

    fn hglobal(format: u16) -> FormatKey {
        FormatKey::content(format, TYMED_HGLOBAL)
    }

    // -- matching -----------------------------------------------------------

    #[test]
    fn a_key_answers_a_request_for_the_same_format_and_medium() {
        assert!(hglobal(15).answers(&hglobal(15)));
    }

    #[test]
    fn a_different_format_is_a_different_request() {
        assert!(!hglobal(15).answers(&hglobal(13)));
    }

    #[test]
    fn a_medium_need_only_intersect() {
        // The caller will take an HGLOBAL or a stream; this entry is an
        // HGLOBAL, so it answers.
        let request = FormatKey::content(15, TYMED_HGLOBAL | TYMED_ISTREAM);
        assert!(hglobal(15).answers(&request));
    }

    #[test]
    fn a_disjoint_medium_does_not_answer() {
        let request = FormatKey::content(15, TYMED_ISTREAM);
        assert!(!hglobal(15).answers(&request));
    }

    #[test]
    fn an_empty_medium_set_matches_nothing() {
        // TYMED_NULL as a request is "no medium is acceptable", and an
        // intersection with the empty set is empty.
        let request = FormatKey::content(15, 0);
        assert!(!hglobal(15).answers(&request));
    }

    #[test]
    fn a_different_aspect_is_a_different_request() {
        let request = FormatKey {
            aspect: DVASPECT_THUMBNAIL,
            ..hglobal(15)
        };
        assert!(
            !hglobal(15).answers(&request),
            "a thumbnail aspect must not be answered with the full content"
        );
    }

    #[test]
    fn a_different_index_is_a_different_request() {
        let request = FormatKey {
            index: 2,
            ..hglobal(15)
        };
        assert!(!hglobal(15).answers(&request));
    }

    // -- storing ------------------------------------------------------------

    #[test]
    fn a_stored_value_answers_a_matching_request() {
        let mut store = FormatStore::new();
        assert!(store.set(hglobal(15), "hdrop").is_none());

        assert_eq!(store.get(&hglobal(15)), Some(&"hdrop"));
    }

    #[test]
    fn an_unmatched_request_gets_nothing() {
        let mut store = FormatStore::new();
        store.set(hglobal(15), "hdrop");

        assert_eq!(store.get(&hglobal(13)), None);
    }

    #[test]
    fn setting_the_same_key_twice_hands_back_the_old_value() {
        let mut store = FormatStore::new();
        store.set(hglobal(15), "first");

        let displaced = store.set(hglobal(15), "second");

        assert_eq!(
            displaced,
            Some("first"),
            "the caller must receive the displaced value so it can release it"
        );
        assert_eq!(store.get(&hglobal(15)), Some(&"second"));
        assert_eq!(store.len(), 1, "a replacement must not grow the store");
    }

    #[test]
    fn the_same_format_on_a_different_medium_is_an_addition_not_a_replacement() {
        let mut store = FormatStore::new();
        store.set(hglobal(15), "as memory");

        let displaced = store.set(FormatKey::content(15, TYMED_ISTREAM), "as a stream");

        assert!(displaced.is_none(), "a second medium displaces nothing");
        assert_eq!(store.len(), 2);
    }

    #[test]
    fn replacing_releases_exactly_the_displaced_value() {
        let deaths = Rc::new(Cell::new(0));
        let mut store = FormatStore::new();
        store.set(hglobal(15), Counted::new(&deaths));

        let displaced = store.set(hglobal(15), Counted::new(&deaths));
        assert_eq!(deaths.get(), 0, "the store must not drop it under a borrow");

        drop(displaced);
        assert_eq!(
            deaths.get(),
            1,
            "the displaced value dies once, when dropped"
        );

        drop(store);
        assert_eq!(deaths.get(), 2, "and the survivor dies with the store");
    }

    #[test]
    fn dropping_the_store_releases_everything_left_in_it() {
        let deaths = Rc::new(Cell::new(0));
        let mut store = FormatStore::new();
        for format in 1_u16..=5 {
            store.set(hglobal(format), Counted::new(&deaths));
        }

        drop(store);

        assert_eq!(
            deaths.get(),
            5,
            "one release per entry, no more and no less"
        );
    }

    #[test]
    fn taking_everything_empties_the_store_without_dropping() {
        let deaths = Rc::new(Cell::new(0));
        let mut store = FormatStore::new();
        store.set(hglobal(15), Counted::new(&deaths));
        store.set(hglobal(13), Counted::new(&deaths));

        let taken = store.take_all();

        assert_eq!(taken.len(), 2);
        assert!(store.is_empty());
        assert_eq!(
            deaths.get(),
            0,
            "take_all hands ownership on, it does not free"
        );

        drop(taken);
        assert_eq!(deaths.get(), 2);
    }

    // -- enumeration --------------------------------------------------------

    #[test]
    fn keys_come_back_in_the_order_they_were_stored() {
        let mut store = FormatStore::new();
        for format in [15_u16, 49_161, 13] {
            store.set(hglobal(format), format);
        }

        let order: Vec<u16> = store.keys().map(|key| key.format).collect();

        assert_eq!(
            order,
            vec![15, 49_161, 13],
            "EnumFormatEtc yields preference order; a reordering changes which \
             flavour a target picks"
        );
    }

    #[test]
    fn replacing_a_value_keeps_its_place_in_the_order() {
        let mut store = FormatStore::new();
        store.set(hglobal(15), "hdrop");
        store.set(hglobal(13), "text");
        store.set(hglobal(15), "hdrop again");

        let order: Vec<u16> = store.keys().map(|key| key.format).collect();

        assert_eq!(order, vec![15, 13], "a replacement must not reorder");
    }

    #[test]
    fn the_first_matching_entry_wins() {
        let mut store = FormatStore::new();
        store.set(FormatKey::content(15, TYMED_HGLOBAL), "preferred");
        store.set(FormatKey::content(15, TYMED_ISTREAM), "fallback");

        let request = FormatKey::content(15, TYMED_HGLOBAL | TYMED_ISTREAM);

        assert_eq!(store.get(&request), Some(&"preferred"));
    }

    #[test]
    fn the_stored_key_describes_the_medium_not_the_request() {
        let mut store = FormatStore::new();
        store.set(FormatKey::content(15, TYMED_HGLOBAL), "bytes");

        let request = FormatKey::content(15, TYMED_HGLOBAL | TYMED_ISTREAM);
        let found = store.key_for(&request).expect("the entry answers");

        assert_eq!(
            found.tymed, TYMED_HGLOBAL,
            "a caller about to duplicate the medium needs the one it really is"
        );
    }

    #[test]
    fn an_empty_store_is_empty() {
        let store: FormatStore<()> = FormatStore::new();

        assert!(store.is_empty());
        assert_eq!(store.len(), 0);
        assert_eq!(store.keys().count(), 0);
    }

    // -----------------------------------------------------------------------
    // The index is only part of the question for two of the four aspects
    // -----------------------------------------------------------------------

    /// A key in `aspect`, at `index`, on global memory.
    fn aspected(aspect: u32, index: i32) -> FormatKey {
        FormatKey {
            format: 42,
            aspect,
            index,
            tymed: TYMED_HGLOBAL,
            device: None,
        }
    }

    #[test]
    fn a_thumbnail_ignores_the_index_it_was_asked_for() {
        // "For the aspects DVASPECT_THUMBNAIL and DVASPECT_ICON, lindex is
        // ignored." A caller leaving it at zero must still find the thumbnail
        // stored under the -1 that means "all of it".
        let stored = aspected(DVASPECT_THUMBNAIL, -1);

        assert!(stored.answers(&aspected(DVASPECT_THUMBNAIL, 0)));
        assert!(stored.answers(&aspected(DVASPECT_THUMBNAIL, 7)));
    }

    #[test]
    fn an_icon_ignores_the_index_it_was_asked_for() {
        let stored = aspected(DVASPECT_ICON, -1);

        assert!(stored.answers(&aspected(DVASPECT_ICON, 0)));
    }

    #[test]
    fn content_still_cares_which_page_was_asked_for() {
        // The other half of the rule, and the reason this is not just
        // "ignore lindex": CFSTR_FILECONTENTS indexes the file wanted, so
        // being lenient here would hand back the wrong file.
        let stored = aspected(DVASPECT_CONTENT, 0);

        assert!(stored.answers(&aspected(DVASPECT_CONTENT, 0)));
        assert!(!stored.answers(&aspected(DVASPECT_CONTENT, 1)));
    }

    #[test]
    fn a_print_rendering_still_cares_which_page_was_asked_for() {
        let stored = aspected(DVASPECT_DOCPRINT, 2);

        assert!(!stored.answers(&aspected(DVASPECT_DOCPRINT, 3)));
    }

    #[test]
    fn ignoring_the_index_does_not_leak_across_aspects() {
        // The lenience is per-aspect, not global: a thumbnail must not answer
        // a request for content just because indices stopped mattering.
        let thumbnail = aspected(DVASPECT_THUMBNAIL, -1);

        assert!(!thumbnail.answers(&aspected(DVASPECT_CONTENT, -1)));
    }

    // -----------------------------------------------------------------------
    // -----------------------------------------------------------------------
    // A stored medium is one thing, not a set of possibilities
    // -----------------------------------------------------------------------

    #[test]
    fn a_medium_is_kept_under_what_it_is_not_what_was_offered() {
        // "Global memory or a stream, either will do" is answered by whichever
        // one actually arrived, and the entry may only promise that one.
        assert_eq!(stored_medium(1 | 4, 1), Some(1));
        assert_eq!(stored_medium(1 | 4, 4), Some(4));
    }

    #[test]
    fn a_medium_the_format_never_offered_is_refused() {
        assert_eq!(stored_medium(1, 4), None);
        assert_eq!(stored_medium(0, 1), None);
    }

    #[test]
    fn a_medium_claiming_two_kinds_at_once_is_refused() {
        // Release picks one arm of the union; a medium that is both a handle
        // and a stream cannot be freed correctly by anyone.
        assert_eq!(stored_medium(u32::MAX, 1 | 4), None);
        assert_eq!(stored_medium(u32::MAX, TYMED_ANY), None);
    }

    #[test]
    fn a_medium_naming_something_undocumented_is_refused() {
        // A bit outside the seven has no release action, so nothing could free
        // it even if it were stored.
        assert_eq!(stored_medium(u32::MAX, 128), None);
        assert_eq!(stored_medium(u32::MAX, 1 << 31), None);
    }

    #[test]
    fn each_documented_medium_is_a_single_medium() {
        for tymed in [1, 2, 4, 8, 16, 32, 64] {
            assert!(is_single_medium(tymed), "TYMED {tymed} is one of the seven");
            assert_eq!(stored_medium(u32::MAX, tymed), Some(tymed));
        }
        assert!(
            !is_single_medium(0),
            "TYMED_NULL is the absence of a medium"
        );
    }

    #[test]
    fn an_absent_medium_is_refused() {
        // "The type of medium specified in the pformatetc and pmedium
        // parameters must be the same" — a format naming global memory does not
        // agree with a medium naming nothing, and TYMED_NULL is "No data is
        // being passed", so there is nothing to store either way.
        assert_eq!(stored_medium(TYMED_HGLOBAL, 0), None);
        assert_eq!(stored_medium(u32::MAX, 0), None);
        assert_eq!(stored_medium(0, 0), None);
        assert_eq!(stored_medium(0, TYMED_HGLOBAL), None);
    }

    // -----------------------------------------------------------------------
    // The target device is part of the identity
    // -----------------------------------------------------------------------

    /// A key for `format` on global memory, rendered for `device`.
    fn for_device(format: u16, device: Option<&[u8]>) -> FormatKey {
        FormatKey {
            device: device.map(Arc::from),
            ..FormatKey::content(format, TYMED_HGLOBAL)
        }
    }

    #[test]
    fn two_devices_are_two_entries_not_one_overwritten() {
        // The bug this pins: with the device dropped from the key, the second
        // store would displace the first and both requests would get the
        // laser printer's rendering.
        let mut store = FormatStore::new();

        assert!(
            store
                .set(for_device(9, Some(b"inkjet")), "inkjet")
                .is_none()
        );
        assert!(store.set(for_device(9, Some(b"laser")), "laser").is_none());

        assert_eq!(store.len(), 2);
        assert_eq!(store.get(&for_device(9, Some(b"inkjet"))), Some(&"inkjet"));
        assert_eq!(store.get(&for_device(9, Some(b"laser"))), Some(&"laser"));
    }

    #[test]
    fn one_printer_never_answers_for_another() {
        // The leniency below is about null, never about two real devices.
        let mut store = FormatStore::new();
        store.set(for_device(9, Some(b"inkjet")), "inkjet");

        assert_eq!(store.get(&for_device(9, Some(b"laser"))), None);
        assert_eq!(
            for_device(9, Some(b"inkjet")).fit(&for_device(9, Some(b"laser"))),
            None
        );
    }

    #[test]
    fn a_device_specific_entry_answers_an_indifferent_request_as_a_default() {
        // "when the caller doesn't care what device is used ... the object
        // should pick an appropriate default device" — refusing would deny the
        // caller the only representation there is.
        let mut store = FormatStore::new();
        store.set(for_device(9, Some(b"inkjet")), "inkjet");

        assert_eq!(store.get(&for_device(9, None)), Some(&"inkjet"));
        assert_eq!(
            for_device(9, Some(b"inkjet")).fit(&for_device(9, None)),
            Some(DeviceFit::Default)
        );
    }

    #[test]
    fn a_device_independent_entry_answers_a_device_specific_request() {
        // "Data obtained from an object with a NULL target device ... is
        // independent of the target device", so it is valid for the printer the
        // caller named.
        let mut store = FormatStore::new();
        store.set(for_device(9, None), "any");

        assert_eq!(store.get(&for_device(9, Some(b"inkjet"))), Some(&"any"));
        assert_eq!(
            for_device(9, None).fit(&for_device(9, Some(b"inkjet"))),
            Some(DeviceFit::Independent)
        );
    }

    #[test]
    fn the_named_device_beats_the_device_independent_entry() {
        // Both answer; the one composed for that printer is more specific, and
        // wins however the entries were ordered.
        let mut store = FormatStore::new();
        store.set(for_device(9, None), "any");
        store.set(for_device(9, Some(b"inkjet")), "inkjet");

        assert_eq!(store.get(&for_device(9, Some(b"inkjet"))), Some(&"inkjet"));

        let mut reversed = FormatStore::new();
        reversed.set(for_device(9, Some(b"inkjet")), "inkjet");
        reversed.set(for_device(9, None), "any");

        assert_eq!(
            reversed.get(&for_device(9, Some(b"inkjet"))),
            Some(&"inkjet")
        );
    }

    #[test]
    fn an_indifferent_request_prefers_the_device_independent_entry() {
        // Ranking, not insertion order: the device-specific entry was stored
        // first and still loses, because a caller that named no device is best
        // served by data that depends on none.
        let mut store = FormatStore::new();
        store.set(for_device(9, Some(b"inkjet")), "inkjet");
        store.set(for_device(9, None), "any");

        assert_eq!(store.get(&for_device(9, None)), Some(&"any"));
    }

    #[test]
    fn an_indifferent_request_picks_the_same_default_every_time() {
        // Two printers, no device-free entry: something must be chosen, and it
        // has to be the same something each call, because a target queries a
        // format and fetches it as two separate trips.
        let mut store = FormatStore::new();
        store.set(for_device(9, Some(b"inkjet")), "inkjet");
        store.set(for_device(9, Some(b"laser")), "laser");

        assert_eq!(store.get(&for_device(9, None)), Some(&"inkjet"));
        assert_eq!(store.get(&for_device(9, None)), Some(&"inkjet"));
        assert_eq!(
            store.key_for(&for_device(9, None)),
            Some(for_device(9, Some(b"inkjet")))
        );
    }

    #[test]
    fn the_key_reported_for_an_indifferent_request_is_the_stored_one() {
        // The caller duplicates what it was handed, so it must be told the
        // device the entry really carries, not the null it asked with.
        let mut store = FormatStore::new();
        store.set(for_device(9, Some(b"inkjet")), "inkjet");

        let found = store.key_for(&for_device(9, None)).expect("it answers");

        assert_eq!(found.device.as_deref(), Some(&b"inkjet"[..]));
    }

    #[test]
    fn fit_ranks_exact_above_independent_above_default() {
        assert!(DeviceFit::Exact < DeviceFit::Independent);
        assert!(DeviceFit::Independent < DeviceFit::Default);
        assert_eq!(
            for_device(9, None).fit(&for_device(9, None)),
            Some(DeviceFit::Exact)
        );
        assert_eq!(
            for_device(9, Some(b"inkjet")).fit(&for_device(9, Some(b"inkjet"))),
            Some(DeviceFit::Exact)
        );
    }

    #[test]
    fn device_leniency_never_crosses_a_format_or_a_medium() {
        // Everything else is still compared exactly; only the device is ranked.
        let mut store = FormatStore::new();
        store.set(for_device(9, None), "any");

        assert_eq!(store.get(&for_device(10, Some(b"inkjet"))), None);
        assert_eq!(
            store.get(&FormatKey {
                device: Some(Arc::from(&b"inkjet"[..])),
                ..FormatKey::content(9, 4)
            }),
            None
        );
    }

    #[test]
    fn the_same_device_by_value_is_the_same_key() {
        // Equality is over the bytes, not the pointer — the blob is copied out
        // of the caller's memory, so two equal devices arrive at different
        // addresses and must still collide.
        let mut store = FormatStore::new();
        store.set(for_device(9, Some(b"inkjet")), "first");

        let displaced = store.set(for_device(9, Some(b"inkjet")), "second");

        assert_eq!(displaced, Some("first"));
        assert_eq!(store.len(), 1);
    }

    // -----------------------------------------------------------------------
    // Trusting a length that lives inside the buffer it measures
    // -----------------------------------------------------------------------

    /// A `DVTARGETDEVICE` header: `tdSize`, the three name offsets, then
    /// `tdExtDevmodeOffset`.
    fn device_header(size: u32, offsets: [u16; 4]) -> Vec<u8> {
        let mut header = size.to_le_bytes().to_vec();
        for offset in offsets {
            header.extend_from_slice(&offset.to_le_bytes());
        }
        header
    }

    #[test]
    fn an_ordinary_target_device_measures_itself() {
        // Three short names after the header, then a device mode with room for
        // the fields a consumer has to read to size it.
        let header = device_header(128, [12, 20, 28, 36]);

        assert_eq!(target_device_size(&header), Some(128));
    }

    #[test]
    fn a_device_with_no_names_is_still_a_device() {
        // Zero is the documented "absent", not an out-of-range offset.
        let header = device_header(TARGET_DEVICE_HEADER as u32, [0, 0, 0, 0]);

        assert_eq!(
            target_device_size(&header),
            Some(TARGET_DEVICE_HEADER),
            "a header and nothing else is a legitimate, if empty, device"
        );
    }

    #[test]
    fn a_device_smaller_than_its_own_header_is_refused() {
        let header = device_header(8, [0, 0, 0, 0]);

        assert_eq!(target_device_size(&header), None);
    }

    #[test]
    fn a_device_claiming_four_gigabytes_is_refused() {
        // The whole point of the check: `tdSize` is attacker-controlled and is
        // the length used to copy the very structure it lives in.
        let header = device_header(u32::MAX, [0, 0, 0, 0]);

        assert_eq!(target_device_size(&header), None);
    }

    #[test]
    fn a_name_pointing_past_the_end_is_refused() {
        let header = device_header(64, [65, 0, 0, 0]);

        assert_eq!(target_device_size(&header), None);
    }

    #[test]
    fn a_name_starting_one_past_the_end_is_refused() {
        // The boundary this pins: an offset equal to `tdSize` addresses the
        // byte after the structure, so the string it claims to point at is not
        // in the blob at all. An inclusive bound calls this well formed.
        let header = device_header(64, [64, 0, 0, 0]);

        assert_eq!(target_device_size(&header), None);
    }

    #[test]
    fn a_name_that_is_only_its_terminator_is_accepted() {
        // One byte left is enough for the empty string, and nothing in the
        // structure states a character width, so this is not ours to refuse.
        let header = device_header(64, [63, 0, 0, 0]);

        assert_eq!(target_device_size(&header), Some(64));
    }

    #[test]
    fn a_devmode_with_no_room_for_its_own_length_is_refused() {
        // A consumer reads `dmSize` and `dmDriverExtra` to learn how long the
        // device mode really is. An offset that leaves fewer bytes than those
        // fields need cannot be pointing at one.
        let short = TARGET_DEVICE_DEVMODE_MIN as u16 - 1;
        let header = device_header(64, [0, 0, 0, 64 - short]);

        assert_eq!(target_device_size(&header), None);
    }

    #[test]
    fn a_devmode_with_exactly_enough_room_is_accepted() {
        let least = TARGET_DEVICE_DEVMODE_MIN as u16;
        let header = device_header(64, [0, 0, 0, 64 - least]);

        assert_eq!(target_device_size(&header), Some(64));
    }

    #[test]
    fn a_devmode_starting_one_past_the_end_is_refused() {
        let header = device_header(64, [0, 0, 0, 64]);

        assert_eq!(target_device_size(&header), None);
    }

    #[test]
    fn the_devmode_floor_is_the_ansi_prefix_through_its_own_lengths() {
        // Spelled out rather than derived from the constant, so that the number
        // stays tied to the structure it came from: `dmDeviceName[32]` plus
        // `dmSpecVersion`, `dmDriverVersion`, `dmSize` and `dmDriverExtra` at
        // two bytes each. A consumer needs all of those to size the device mode
        // as `dmSize + dmDriverExtra`, so anything shorter cannot be one.
        let ansi_prefix = 32 + 2 + 2 + 2 + 2;
        assert_eq!(TARGET_DEVICE_DEVMODE_MIN, ansi_prefix);

        let just_short = device_header(64, [0, 0, 0, 64 - ansi_prefix as u16 + 1]);
        let exactly_enough = device_header(64, [0, 0, 0, 64 - ansi_prefix as u16]);

        assert_eq!(target_device_size(&just_short), None);
        assert_eq!(target_device_size(&exactly_enough), Some(64));
    }

    #[test]
    fn an_offset_that_would_overflow_when_bounded_is_refused() {
        // `tdSize` is capped well below u16::MAX, so a maximal offset is out of
        // range on its own; the arithmetic must not wrap on the way to saying so.
        let header = device_header(64, [u16::MAX, 0, 0, 0]);

        assert_eq!(target_device_size(&header), None);
    }

    #[test]
    fn a_name_pointing_into_the_header_is_refused() {
        let header = device_header(64, [11, 0, 0, 0]);

        assert_eq!(target_device_size(&header), None);
    }

    #[test]
    fn a_truncated_header_is_refused_rather_than_read_past() {
        let header = device_header(64, [12, 20, 28, 36]);

        assert_eq!(target_device_size(&header[..11]), None);
        assert_eq!(target_device_size(&[]), None);
    }
}
