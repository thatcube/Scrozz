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

/// A bounded undo/redo stack over a document's editable state.
///
/// The history does not own the document; it is handed one to snapshot and one
/// to restore into. That keeps the editor free to hold the document wherever it
/// likes, and makes the history itself trivially testable.
#[derive(Debug, Clone)]
pub struct History {
    past: Vec<DocumentData>,
    present: DocumentData,
    future: Vec<DocumentData>,
    /// The tag of the step in progress, if the last commit was coalescible.
    tag: Option<String>,
    limit: usize,
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
            present: document.data(),
            future: Vec::new(),
            tag: None,
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
        if data == self.present {
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
        if data == self.present {
            return;
        }
        if self.tag.as_deref() == Some(tag) {
            // Extend the step already in progress: the past keeps the state
            // from before the gesture began, so one undo still reverts it all.
            self.present = data;
            self.future.clear();
            return;
        }
        self.push(data);
        self.tag = Some(tag.to_owned());
    }

    /// Discards the open coalescing group, restoring `document` to before it.
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
    /// Only the open group is discarded. Anything sealed before it is untouched,
    /// so this cannot swallow an edit the user did finish.
    ///
    /// Returns `false` when there is no open group to abandon — the caller
    /// still has to clean up the document itself in that case.
    /// # Errors
    ///
    /// Returns an error only if the recorded state cannot be applied to this
    /// document, for the same reason [`undo`](Self::undo) can.
    pub fn abandon(&mut self, document: &mut Document) -> Result<bool> {
        if self.tag.is_none() {
            return Ok(false);
        }
        let Some(previous) = self.past.last().cloned() else {
            // A zero-limit history keeps no past, so there is nothing to go back
            // to and the caller has to undo the change by hand.
            self.tag = None;
            return Ok(false);
        };
        document.restore(previous.clone())?;
        self.past.pop();
        self.present = previous;
        self.tag = None;
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
        match document.restore(previous.clone()) {
            Ok(()) => {
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
        match document.restore(next.clone()) {
            Ok(()) => {
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

    /// Forgets every step, keeping the document's current state as the origin.
    pub fn reset(&mut self, document: &Document) {
        self.past.clear();
        self.future.clear();
        self.present = document.data();
        self.tag = None;
    }

    /// Pushes `data` as the new present, discarding any redo branch.
    fn push(&mut self, data: DocumentData) {
        let previous = std::mem::replace(&mut self.present, data);
        self.past.push(previous);
        self.future.clear();
        if self.past.len() > self.limit {
            let excess = self.past.len() - self.limit;
            self.past.drain(..excess);
        }
    }
}
