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

/// The parts of a `FORMATETC` that decide whether an entry answers a request.
///
/// # What is deliberately not here
///
/// `FORMATETC` also carries `ptd`, a `DVTARGETDEVICE*` describing a specific
/// rendering device. It exists so a document can be asked for "this content, as
/// it would print on *that* printer", and it is not meaningful for a dragged
/// screenshot: there is one rendering, and it is the file. Every reference
/// implementation of a shell data object matches device-independently, and so
/// does this. A caller that passes a target device gets the same bytes it would
/// have got with a null one.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct FormatKey {
    /// The clipboard format id — `CF_HDROP`, or whatever `RegisterClipboardFormatW` returned.
    pub format: u16,
    /// `DVASPECT_*`: which view of the data. Almost always `DVASPECT_CONTENT`.
    pub aspect: u32,
    /// Which page of a multi-page format, or `-1` for the whole thing.
    pub index: i32,
    /// A *set* of `TYMED_*` bits, not a single one. See [`Self::answers`].
    pub tymed: u32,
}

impl FormatKey {
    /// A key for one storage medium of one format, in its ordinary aspect.
    ///
    /// The shape every flavour this crate offers itself takes: whole content,
    /// no paging.
    #[must_use]
    pub const fn content(format: u16, tymed: u32) -> Self {
        Self {
            format,
            aspect: DVASPECT_CONTENT,
            index: -1,
            tymed,
        }
    }

    /// Whether an entry stored under this key may answer `request`.
    ///
    /// Format, aspect and index must be *equal*; the medium need only
    /// *intersect*. That asymmetry is the whole subtlety of `FORMATETC` and it
    /// is not arbitrary: `tymed` is a bitmask of the media the caller is
    /// willing to accept, so a caller asking for `TYMED_HGLOBAL | TYMED_ISTREAM`
    /// is saying "either will do", and an entry that can supply one of them
    /// answers. `dwAspect` and `lindex` are single values naming *which data* is
    /// wanted, and a mismatch there means a different thing was asked for, not
    /// a different way of carrying the same thing.
    ///
    /// Being lenient about aspect or index would return the wrong data rather
    /// than an error, which is the failure that is hard to notice.
    #[must_use]
    pub const fn answers(&self, request: &Self) -> bool {
        self.format == request.format
            && self.aspect == request.aspect
            && self.index == request.index
            && self.tymed & request.tymed != 0
    }
}

/// `DVASPECT_CONTENT`, spelled out so this module needs no Windows import.
///
/// Asserted against the real constant in the Windows backend's tests.
pub const DVASPECT_CONTENT: u32 = 1;

/// `TYMED_HGLOBAL`, likewise.
pub const TYMED_HGLOBAL: u32 = 1;

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
    /// First match in insertion order, so an earlier entry is preferred exactly
    /// as [`Self::keys`] would suggest.
    #[must_use]
    pub fn get(&self, request: &FormatKey) -> Option<&T> {
        self.entries
            .iter()
            .find(|(key, _)| key.answers(request))
            .map(|(_, value)| value)
    }

    /// The key an entry answering `request` is stored under.
    ///
    /// Needed because the stored key, not the request, describes the medium the
    /// value actually is — a request for `HGLOBAL | ISTREAM` is answered by an
    /// entry that is one or the other, and a caller about to duplicate it has
    /// to know which.
    #[must_use]
    pub fn key_for(&self, request: &FormatKey) -> Option<FormatKey> {
        self.entries
            .iter()
            .find(|(key, _)| key.answers(request))
            .map(|(key, _)| *key)
    }

    /// Every key, in insertion order.
    pub fn keys(&self) -> impl Iterator<Item = FormatKey> + '_ {
        self.entries.iter().map(|(key, _)| *key)
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
    /// `DVASPECT_THUMBNAIL`.
    const DVASPECT_THUMBNAIL: u32 = 2;

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
}
