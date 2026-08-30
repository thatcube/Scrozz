# Smart Frame compatibility

Smart Frame is the legacy name for [Scene](scene.md). Stored document fields,
CLI flags, settings keys, and host intents keep their existing names during the
migration so older documents and automation continue to work.

New product and editor behavior is defined by the Scene contract. In
particular, Scene preserves the immutable source, may hold back a conservative
automatic inner inset without discarding those pixels, and grows undersized
output canvases instead of shrinking the rendered subject.
