# Smart Frame

Smart Frame is Scrozz's one-action, non-destructive presentation treatment for
the current editor revision. External UI references informed behavior and
hierarchy only; Scrozz's pixels, layout, controls, and visual system are
original.

## Contract

- **Smart Frame** creates a live draft immediately. Pixel analysis runs on a
  cancellable worker and is accepted only for the editor lifetime and immutable
  revision that requested it.
- **Apply** commits the framing draft as one undo step. **Cancel** restores the
  exact prior framing without a persistence write. **Revert** removes applied
  framing and is itself undoable.
- Analysis stores its algorithm version, source color space, sampled
  dimensions, visual focus, content class, inset decision, confidence, and
  resolved background colors. Reopening a document does not restyle it under a
  newer algorithm.
- Automatic inset scans every pixel in candidate outer bands and trims only
  transparent or tightly uniform margins. Low confidence resolves to zero with
  an explanation.
- Adaptive padding, corners, shadow, and border use quantized bounded values.
- Exact output sizes fit the capture without changing its aspect ratio. The
  presentation canvas may expand; a smaller target scales ordinary captures
  down. A window target that cannot contain the native pixels plus padding is
  rejected.
- Source color profiles are retained. Analysis converts Display P3 and Rec.
  2020 samples to sRGB for measurement, while rendering converts authored
  colors into the source space instead of retagging source pixels.

## Decision D9

Window pixels are immutable. Smart Frame may add a background and padding
around the captured rectangle and may position that rectangle in a larger
canvas. It cannot inset, round, border, re-shadow, or scale a native window
during final export. The renderer copies the native-size window rectangle with
source blending and tests the inner bytes exactly.

## Persistence and migration

Annotation document schema version 5 includes source-space inset, exact output
size, resolved Smart Frame metadata, automatic backgrounds, optional watermark
data, secure Redact settings, and forward-preserved unknown fields. Older
documents continue to load through defaults and are upgraded only when saved.

User presets live in the existing versioned per-user Scrozz settings document.
They preserve unknown fields, are bounded, and contain portable settings only.
Custom background pixels, capture pixels, tokens, and credentials are not part
of the preset representation.

`after-capture.apply-smart-frame` is persisted and defaults to `false`. A GUI
capture resolves one derived revision before enabled Copy, Save, Upload, Recent
Captures Overlay, Open Editor, and Pin consumers. Consumer failures remain
isolated. The original source and editable document remain in history. Direct
CLI captures do not read this ambient policy; `--smart-frame` opts in
explicitly.

## Sensitive-region seam

`SensitiveRegionReview` accepts revision-bound, provider-owned suggestions.
Smart Frame displays only reviewed suggestions. It never converts one into a
redaction and never changes or uploads pixels based on an unconfirmed
detection.

## Manual matrix

1. Open a region capture, choose **Smart Frame**, wait for analysis, then verify
   Apply, Cancel, Revert, Undo, and Redo.
2. Exercise sparse text, one-sided UI, a photograph, transparency, very wide
   and very tall captures, and disconnected objects. Confirm low-confidence
   inset remains zero.
3. Open a window capture. Verify Padding, Background, Alignment, and a large
   output size work while Inset, Corners, Shadow, and Border stay disabled.
4. Save, update, duplicate, and delete a custom preset; reopen another capture
   and confirm it is present.
5. Toggle `after-capture.apply-smart-frame`, Copy, Save, Recent Captures
   Overlay, and Open Editor. Confirm every successful destination represents
   the same framed revision and one failed destination does not suppress later
   actions.
6. Export sRGB, Display P3, Rec. 2020, transparent, annotated, and destructively
   redacted captures. Confirm profile tags and visible pixels match the preview.
