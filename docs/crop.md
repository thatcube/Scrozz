# Crop

Crop is a focused, non-destructive document transaction. Its rectangle and
orientation remain draft state until **Apply**; **Cancel** discards both. Source
pixels and annotations stay in source coordinates. Rendering composites the
source and annotations, applies the source crop, then applies the persisted
90-degree rotation or reflection before Scene framing.

Resolved Scene focus is tied to the geometry that was analyzed. Applying or
reverting Crop therefore invalidates that focus across the current Scene and its
undo/redo lane; the Scene background remains intact and placement falls back to
geometric center until a new analysis resolves the transformed image.

Snap to Edges preprocesses each source image asynchronously into an immutable
index of long, axis-aligned structural boundaries. Pointer movement only queries
that index. A boundary attracts within 6 screen points and remains locked until
12 screen points, independent of zoom. Command on macOS and Ctrl elsewhere
temporarily bypass snapping. Source bounds remain available as exact synthetic
boundaries.

## Scene expansion contract

Crop does not own a background or fill. `Document::resolve_crop` validates a
requested rectangle and returns:

- `source_crop`, the intersected rectangle in original source coordinates; and
- `expansion`, the requested left, top, right, and bottom margins outside the
  source.

The current crop UI commits only `source_crop`. When Scene supports outward crop
materialization, it must consume `expansion` through Scene's existing
canvas/background model, call `expansion.apply_orientation(document.orientation())`
before assigning displayed sides, and apply `source_crop` in the same document
history transaction. It must not create crop-specific fill state. Export
continues to use the shared document renderer, so preview and output retain
identical crop/orientation/framing order.
