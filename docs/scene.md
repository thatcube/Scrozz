# Scene

Scene is Scrozz's singular, nondestructive presentation tool. It wraps an
editable document around untouched capture pixels. Preview and export use the
same renderer; export flattens a copy and never consumes the retained document.

## Document contract

- The source capture remains immutable. **Remove Scene** restores the exact
  source composition.
- **Clear canvas** is different: Scene remains editable, but its canvas is
  transparent.
- Scene owns background, asymmetric canvas padding, placement, subject corners
  and shadow, aspect ratio, and minimum output size.
- Aspect ratio starts at **Original** and is always an explicit choice.
- Ratio and output size only grow the canvas. They never crop, stretch, or
  force the source into a smaller frame.
- An exact output size supersedes the aspect-ratio control and retains its own
  ratio if the canvas must grow to contain the source and padding.
- Asymmetric canvas padding is the shared model for scrolling captures and
  future outward Crop expansion.

## Automatic properties

Background, padding, placement, corners, shadow, and output size can each remain
Automatic or become fixed. The editor shows the resolved value. Editing that
value fixes only that property. **Reset to Automatic** restores the immutable
built-in Automatic Scene.

User presets are authored from the editor and preserve the Automatic/fixed
choice for every property. They never contain capture pixels.

Automatic padding is proportional to the shorter source edge and bounded to an
empirical range. Automatic placement uses only a sufficiently confident stored
focus and applies a subtle optical shift; lower confidence resolves to exact
center.

## Backgrounds

Curated backgrounds remain distinct from capture-derived suggestions. Generated
suggestions are local, deterministic, and limited to four art directions:
Balanced, Soft, Vibrant, and Neutral. A generated background persists its
algorithm version, style, template, seed, palette, edge reference, and source
color space, so reopening a document reproduces the same pixels.

Reliable local treatments are smooth gradients, soft meshes, tonal studio
fields, and blurred source. Image and Desktop backgrounds use explicit pixels
supplied by the host. No generated treatment uses network access, ML, or
unconstrained randomness.

## Native window appearance

Window captures preserve the platform result as native appearance. Scene may
change only background, padding, placement, ratio, and output size. Inset,
synthetic corners, border, and shadow are unavailable, and Scene never stacks a
second shadow. The subject rectangle is copied byte-stably at native export
scale.

## Compatibility

Stored document fields, command-line flags, settings keys, and host intents may
retain the legacy `beautification` and `smart-frame` names during migration.
Those names are compatibility surfaces; the editor and product language use
Scene.
