# Smart Frame compatibility

Smart Frame is the legacy name for [Scene](scene.md). Stored document fields,
CLI flags, settings keys, and host intents keep their existing names during the
migration so older documents and automation continue to work.

New product and editor behavior is defined by the Scene contract. In
particular, Scene preserves the complete immutable source and grows a
background-filled canvas around it. The legacy source-inset field remains
readable for compatibility but is no longer created or presented as padding.
