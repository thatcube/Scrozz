//! Undo and redo, as snapshots of the editable state.
//!
//! # Why snapshots rather than a command log
//!
//! A command log is the textbook answer and the wrong one here. Inverting a
//! command is only correct if it is inverted against exactly the state it was
//! applied to, and this document renumbers counters as a side effect of almost
//! every edit — deleting marker 2 renumbers 3, 4 and 5, so the inverse of
//! "delete" is not "insert". Every such coupling is another chance for undo to
//! reconstruct a document that never existed, and a wrong undo is worse than no
//! undo because the user cannot tell it happened.
//!
//! A snapshot cannot be wrong. [`DocumentData`] is already the crate's
//! serialisable state — the same type that persists to disk — so a snapshot is
//! a clone of a small vector of vector objects, not of the image. The source
//! [`Capture`](scrozz_core::Capture), which is the only large thing in a
//! document, is never part of a snapshot and is never restored.
//!
//! # Gestures are one step
//!
//! A drag that moves an arrow across the screen mutates the document on every
//! frame so the preview stays honest, but it must cost exactly one undo. Two
//! mechanisms make that true: [`History::commit`] ignores a commit that would
//! record no change, and [`History::commit_coalesced`] folds consecutive
//! same-tagged commits into the step already in progress.

use scrozz_core::Result;

use crate::document::{Document, DocumentData};

/// How many undo steps are kept before the oldest is dropped.
///
/// Deep enough that no realistic annotation session reaches the bottom, and
/// bounded so a long-lived editor window cannot grow without limit.
pub const DEFAULT_LIMIT: usize = 200;

/// A caller-supplied marker recorded alongside a state.
///
/// The history has no idea what it means. The editor uses it for the text
/// caret, which belongs to the moment an edit was made rather than to the
/// drawing: putting it in [`DocumentData`] would serialise it into the sidecar
/// and make two identical annotations compare unequal because someone's cursor
/// happened to be somewhere else.
pub type Mark = u64;

/// A recorded state and the caller's marker for it.
#[derive(Debug, Clone)]
struct Step {
    data: DocumentData,
    mark: Mark,
}

/// A bounded undo/redo stack over a document's editable state.
///
/// The history does not own the document; it is handed one to snapshot and one
/// to restore into. That keeps the editor free to hold the document wherever it
/// likes, and makes the history itself trivially testable.
#[derive(Debug, Clone)]
pub struct History {
    past: Vec<Step>,
    present: Step,
    future: Vec<Step>,
    /// The tag of the step in progress, if the last commit was coalescible.
    tag: Option<String>,
    /// The rollback point of the edit in progress, if one was opened.
    open: Option<Open>,
    /// The marker the next recorded state will carry.
    cursor: Mark,
    /// How many steps have been dropped off the front of `past` to stay within
    /// the limit, so a rollback point can be expressed as a depth that a later
    /// eviction cannot silently move.
    evicted: usize,
    limit: usize,
}

/// Where an edit in progress began, so it can be undone as though it never was.
///
/// This is deliberately not "the last step" or "steps sharing a tag". An edit
/// the user might abandon is not one commit: placing a label and then changing
/// its colour before typing anything is two, under two different tags. What
/// makes it one thing is that the user started it and has not finished it, and
/// only the caller who started it knows that.
#[derive(Debug, Clone)]
struct Open {
    /// Undo depth at the moment the edit began, counted from the start of the
    /// session so eviction cannot invalidate it.
    depth: usize,
    present: Step,
    /// The redo branch as it stood before the edit began.
    ///
    /// It is moved here rather than copied: `push` would have cleared it
    /// anyway, so while the edit is open there is nothing to redo, and if the
    /// edit is abandoned it comes back untouched.
    future: Vec<Step>,
    tag: Option<String>,
}

impl History {
    /// Starts a history whose only state is the document as it stands.
    #[must_use]
    pub fn new(document: &Document) -> Self {
        Self::with_limit(document, DEFAULT_LIMIT)
    }

    /// Starts a history that keeps at most `limit` undo steps.
    ///
    /// A `limit` of zero still allows redo of the step just undone; it simply
    /// keeps no undo depth.
    #[must_use]
    pub fn with_limit(document: &Document, limit: usize) -> Self {
        Self {
            past: Vec::new(),
            present: Step {
                data: document.data(),
                mark: 0,
            },
            future: Vec::new(),
            tag: None,
            open: None,
            cursor: 0,
            evicted: 0,
            limit,
        }
    }

    /// Records the document's current state as a new undo step.
    ///
    /// Does nothing if the document is identical to the last recorded state, so
    /// a click that selects without editing — or a drag that ends where it
    /// started — never costs an undo the user has to press twice.
    pub fn commit(&mut self, document: &Document) {
        let data = document.data();
        if data == self.present.data {
            return;
        }
        self.tag = None;
        self.push(data);
    }

    /// Records the document, folding into the previous step if it shares `tag`.
    ///
    /// For continuous edits whose end the editor cannot observe — dragging a
    /// stroke-width slider, nudging a selection with the arrow keys — where
    /// every intermediate value would otherwise become its own undo step.
    ///
    /// Any [`Self::commit`], [`Self::undo`] or [`Self::redo`] closes the open
    /// step, so a nudge, an unrelated edit, and another nudge are three steps
    /// rather than a merged two.
    pub fn commit_coalesced(&mut self, document: &Document, tag: &str) {
        let data = document.data();
        if data == self.present.data {
            return;
        }
        if self.tag.as_deref() == Some(tag) {
            // Extend the step already in progress: the past keeps the state
            // from before the gesture began, so one undo still reverts it all.
            self.present = Step {
                data,
                mark: self.cursor,
            };
            self.future.clear();
            return;
        }
        self.push(data);
        self.tag = Some(tag.to_owned());
    }

    /// Marks the start of an edit the caller may decide never happened.
    ///
    /// Call it *before* making the change — before the annotation is added, not
    /// after. Everything committed from here until [`Self::finish`] or
    /// [`Self::abandon`] belongs to one reversible unit, however many commits
    /// or tags it turns out to span.
    ///
    /// Opening a second edit finishes the first, which is the only sane reading
    /// of starting something new while something is unfinished.
    ///
    /// # Why [`undo`](Self::undo) and [`redo`](Self::redo) do not close it
    ///
    /// They used to, on the reasoning that navigating the history made the edit
    /// part of it. That reasoning is wrong for the case this exists to serve.
    /// Place a label, change its colour, press ⌘Z, press Escape: the label is
    /// still unfinished and still empty, and the click that made it still has
    /// to be taken back. Closing the edit on the ⌘Z left `open` gone, so the
    /// cancellation fell through to deleting the annotation and committing —
    /// which puts an invisible empty label one ⌘Z away, exactly the bug this
    /// was built to prevent — and threw away the redo branch `begin` had put
    /// into safekeeping.
    ///
    /// The rollback point survives navigation because it does not describe a
    /// position in the stack, it *is* a snapshot plus an absolute depth, and
    /// [`abandon`](Self::abandon) already refuses when navigation has taken the
    /// document somewhere that depth no longer reaches.
    pub fn begin(&mut self) {
        self.finish();
        self.open = Some(Open {
            depth: self.evicted + self.past.len(),
            present: self.present.clone(),
            // Taking it leaves the redo stack empty, which is exactly what the
            // first commit of this edit would have done. If the edit is
            // abandoned it goes back, and nothing the user could redo was lost
            // to a click they took back.
            future: std::mem::take(&mut self.future),
            tag: self.tag.clone(),
        });
    }

    /// Accepts the edit in progress, making it permanent.
    ///
    /// Harmless when no edit is open.
    pub fn finish(&mut self) {
        self.open = None;
    }

    /// Discards the edit in progress, restoring `document` to before it began.
    ///
    /// # Why this is not [`undo`](Self::undo)
    ///
    /// Undo is a step the user took back, so it goes on the redo stack. This is
    /// for a step the user never finished: placing a text label and typing
    /// nothing into it. Committing that would leave an invisible empty
    /// annotation one ⌘Z away — the user sees nothing happen, presses undo, and
    /// something they never made comes back. There is nothing to redo either,
    /// because nothing happened.
    ///
    /// Everything sealed before the edit began is untouched, including the redo
    /// branch: abandoning something the user never did must not cost them
    /// something they did.
    ///
    /// Returns `false` when there is no edit to abandon, or when the point it
    /// began has since been evicted from a full history — the caller still has
    /// to clean up the document itself in those cases.
    ///
    /// # Errors
    ///
    /// Returns an error only if the recorded state cannot be applied to this
    /// document, for the same reason [`undo`](Self::undo) can.
    pub fn abandon(&mut self, document: &mut Document) -> Result<bool> {
        let Some(open) = self.open.take() else {
            return Ok(false);
        };
        // A history so short that the beginning of this edit has already fallen
        // off the back of it. Nothing sensible to roll back to.
        let Some(target) = open.depth.checked_sub(self.evicted) else {
            return Ok(false);
        };
        if target > self.past.len() {
            return Ok(false);
        }
        document.restore(open.present.data.clone())?;
        self.past.truncate(target);
        self.cursor = open.present.mark;
        self.present = open.present;
        self.future = open.future;
        self.tag = open.tag;
        Ok(true)
    }

    /// Abandons any open coalescing group.
    ///
    /// Call it when a gesture ends, so the next edit of the same kind starts a
    /// fresh undo step instead of merging into the one that just finished.
    pub fn seal(&mut self) {
        self.tag = None;
    }

    /// Whether there is a state to go back to.
    #[must_use]
    pub fn can_undo(&self) -> bool {
        !self.past.is_empty()
    }

    /// Whether there is a state to go forward to.
    #[must_use]
    pub fn can_redo(&self) -> bool {
        !self.future.is_empty()
    }

    /// How many undo steps are available.
    #[must_use]
    pub fn undo_depth(&self) -> usize {
        self.past.len()
    }

    /// How many redo steps are available.
    #[must_use]
    pub fn redo_depth(&self) -> usize {
        self.future.len()
    }

    /// Restores the previous state into `document`.
    ///
    /// Returns `false` if there was nothing to undo, leaving the document
    /// untouched.
    ///
    /// # Errors
    ///
    /// Returns an error only if the recorded state cannot be applied to this
    /// document — which means the document's provenance changed underneath the
    /// history. The history is left unchanged so the caller can retry or
    /// discard it.
    pub fn undo(&mut self, document: &mut Document) -> Result<bool> {
        let Some(previous) = self.past.pop() else {
            return Ok(false);
        };
        match document.restore(previous.data.clone()) {
            Ok(()) => {
                self.cursor = previous.mark;
                let current = std::mem::replace(&mut self.present, previous);
                self.future.push(current);
                self.tag = None;
                Ok(true)
            }
            Err(error) => {
                self.past.push(previous);
                Err(error)
            }
        }
    }

    /// Restores the next state into `document`.
    ///
    /// Returns `false` if there was nothing to redo.
    ///
    /// # Errors
    ///
    /// As [`Self::undo`].
    pub fn redo(&mut self, document: &mut Document) -> Result<bool> {
        let Some(next) = self.future.pop() else {
            return Ok(false);
        };
        match document.restore(next.data.clone()) {
            Ok(()) => {
                self.cursor = next.mark;
                let current = std::mem::replace(&mut self.present, next);
                self.past.push(current);
                self.tag = None;
                Ok(true)
            }
            Err(error) => {
                self.future.push(next);
                Err(error)
            }
        }
    }

    /// Records the caller's marker for wherever the document now stands.
    ///
    /// The editor calls this as the caret moves, so the marker travels with the
    /// state rather than with the keystroke that happened to commit it. Undo and
    /// redo then put the caret back where it was when that state was on screen,
    /// instead of leaving it wherever the user last was and hoping it still
    /// points at something sensible.
    pub const fn mark(&mut self, mark: Mark) {
        self.cursor = mark;
        self.present.mark = mark;
    }

    /// The marker belonging to the state the document is at.
    #[must_use]
    pub const fn current_mark(&self) -> Mark {
        self.present.mark
    }

    /// Forgets every step, keeping the document's current state as the origin.
    pub fn reset(&mut self, document: &Document) {
        self.past.clear();
        self.future.clear();
        self.present = Step {
            data: document.data(),
            mark: self.cursor,
        };
        self.tag = None;
        self.open = None;
        self.evicted = 0;
    }

    /// Pushes `data` as the new present, discarding any redo branch.
    fn push(&mut self, data: DocumentData) {
        let step = Step {
            data,
            mark: self.cursor,
        };
        let previous = std::mem::replace(&mut self.present, step);
        self.past.push(previous);
        self.future.clear();
        if self.past.len() > self.limit {
            let excess = self.past.len() - self.limit;
            self.past.drain(..excess);
            self.evicted += excess;
        }
    }
}
