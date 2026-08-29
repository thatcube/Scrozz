# Smart Frame

Smart Frame is Scrozz's one-action, non-destructive presentation treatment for
the current editor revision. It was extended from reviewed commit
`da486c6e35f764c731304e43d40352642827054c`; the branch was verified at that
exact commit before implementation.

The private Xnapper and CleanShot captures listed in
`~/.copilot/scrozz-ui-reference/INDEX.md` were used only to identify behavior and
hierarchy: one obvious action, Auto Balance as the default value, named starting
points before advanced controls, and an inspector that can disclose the complete
model. Scrozz's palette, copy, tokens, controls, and layout remain original.

## Contract

- **Smart Frame** creates a live draft immediately. Pixel analysis runs on a
  cancellable worker and is accepted only for the immutable revision that
  requested it.
- **Apply** commits the entire framing draft as one undo step. **Cancel** restores
  the exact prior framing without a revision or persistence write. **Revert**
  removes applied framing and is itself undoable.
- Analysis stores its algorithm version, source colour space, sampled dimensions,
  visual focus, content class, inset decision, confidence, and resolved
  background colours. Reopening a document does not re-style it under a newer
  algorithm.
- Automatic inset scans every pixel in the candidate outer bands and trims only
  transparent or tightly uniform margins. Low confidence resolves to zero with
  an explanation.
- Adaptive padding, corners, shadow, and border use quantised bounded values.
  Near-identical inputs therefore change gradually rather than crossing large
  preset thresholds.
- Exact output sizes fit the capture without changing its aspect ratio. The
  presentation canvas may expand; a smaller target scales ordinary captures
  down. A window target that cannot contain the native pixels plus padding is
  rejected.
- Source colour profiles are retained. Analysis converts Display P3 and Rec. 2020
  samples to sRGB for measurement, while rendering converts authored colours
  into the source space instead of retagging source pixels.

## Decision D9

Window pixels are immutable. Smart Frame may add a background and padding around
the captured rectangle and may position that rectangle in a larger canvas. It
cannot inset, round, border, re-shadow, or scale a native window during final
export. The renderer copies a native-size window rectangle with source blending
and tests the inner bytes exactly.

## Persistence and migration

Annotation document format version 3 adds source-space inset, exact output size,
resolved Smart Frame metadata, automatic backgrounds, optional watermark data,
and forward-preserved unknown fields. Versions 1 and 2 continue to load through
Serde defaults and are rewritten as version 3 on the next save.

User presets live in the per-user Scrozz settings file. They have their own
version, preserve unknown fields, are bounded by the settings-file limit, and
store only portable values. Custom background pixels, capture pixels, tokens,
and credentials are rejected or absent from the preset representation.

`after-capture.apply-smart-frame` is persisted and defaults to `false`. GUI
capture resolves one derived revision before the enabled Copy, Save, Upload,
Quick Access, Open Editor, and Pin consumers. Consumer failures are isolated.
The original source and editable document remain in history. Direct CLI and JSON
captures do not read this ambient policy; `--smart-frame` opts in explicitly.

## Sensitive-region seam

`SensitiveRegionReview` accepts revision-bound, provider-owned suggestions.
Smart Frame displays only the count and review requirement. It never converts a
suggestion into a redaction and never changes or uploads pixels based on an
unconfirmed detection.

## Aggregate reconciliation

This branch deliberately changes only the existing shared seams:

- `scrozz-annotate`: versioned model, analysis, rendering, and migrations;
- `scrozz-ui::editor`: draft transaction, progressive inspector, preset events,
  and deterministic scenes;
- `apps/scrozz::gui::pipeline`: asynchronous analysis and one derived
  After Capture revision;
- `apps/scrozz::settings`: atomic preferences and preset persistence;
- CLI: explicit `--smart-frame` and `--size`, never ambient GUI policy.

When reconciling with newer app/editor lineages, keep those public types and
events, then connect them to the newer shell. Do not replace newer capture,
history, upload, pin, or settings surfaces with this branch's older files.

## Manual matrix

1. Open a region capture, choose **Smart Frame**, wait for the analysis note,
   then verify Apply, Cancel, Revert, Undo, and Redo.
2. Exercise sparse text, one-sided UI, a photograph, transparency, very wide and
   very tall captures, and disconnected objects. Confirm low-confidence inset
   remains zero.
3. Open a window capture. Verify Padding, Background, Alignment, and a large
   output size work while Inset, Corners, Shadow, and Border stay disabled.
4. Save, update, duplicate, and delete a custom preset; reopen another capture
   and confirm it is present.
5. Toggle `after-capture.apply-smart-frame`, Copy, Save, Overlay, and Open Editor.
   Confirm every successful destination represents the same framed revision and
   one failed destination does not suppress later actions.
6. Export sRGB, Display P3, Rec. 2020, transparent, annotated, and destructively
   redacted captures. Confirm profile tags and visible pixels match the preview.
