# Dragging a card out — what is verified, and what a human must check

Dragging a screenshot out of a Scrozz card and into another application is the
one feature whose success is **a property of the destination process**. Our side
of the transaction is observable and tested; theirs is not. This document draws
that line exactly, so nobody mistakes a green test suite for a working drop.

---

## What the tests prove without a human

These run on every push and need no display server.

| Claim | Where it is proved |
| --- | --- |
| The payload names one real PNG on disk, with a sane filename | `apps/scrozz` — `drag::payload_for` tests |
| The file exists before the drag starts and is not deleted while it is in flight | `scrozz-shell` — `tests/drag.rs`, `mod artifact_lifetime` |
| A cancelled drag frees the file, and an accepted one keeps it until the retention window elapses | same |
| Orphans from a crashed run are swept, and live files are never swept | same |
| The card arms mid-gesture rather than at mouse-up | `scrozz-ui` — `tests/stack.rs`, "Mid-gesture hand-off" |
| An uncommitted press never arms a drag | same |
| The card is dismissed only on `Accepted` | `apps/scrozz` — `app.rs` drag tests |
| The pasteboard advertises `public.file-url` first, then `public.png`, then `public.tiff`, on one item | `scrozz-shell` — `tests/drag.rs`, `#[ignore]`d AppKit tests |
| The advertised URL round-trips back to the file we wrote | same |
| The Windows `CF_HDROP` payload has the right header offset, `fWide` flag and double NUL | `scrozz-shell` — `drag::hdrop` tests (run on **all** platforms) |

The AppKit tests are `#[ignore]`d because they need a real `NSPasteboard`. Run
them deliberately:

```bash
cargo test -p scrozz-shell --test drag -- --ignored --test-threads=1
```

Their limit is exact: they prove **we published the right thing**. They cannot
prove anyone reads it.

---

## The macOS matrix a human must walk

Drop acceptance depends on which flavour the destination asks for, and that is a
decision made inside code we do not own. The table below is the list of
behaviours that differ enough to be worth checking individually.

Drag one card from the overlay onto each target and record the result.

| # | Target | What it should do | Flavour it is expected to read |
| --- | --- | --- | --- |
| 1 | Finder window | Copies a `.png` file into that folder | `public.file-url` |
| 2 | Finder Desktop | Same, onto the desktop | `public.file-url` |
| 3 | Dock icon of an app | App opens the image | `public.file-url` |
| 4 | Slack message box | Attaches as an image, with a thumbnail preview | `public.file-url` (Chromium) |
| 5 | Discord message box | Same | `public.file-url` (Chromium) |
| 6 | Safari — a web upload zone | Uploads as a file | `public.file-url` |
| 7 | Chrome — a web upload zone | Uploads as a file | `public.file-url` |
| 8 | Mail compose window | Attaches, inline | `public.file-url` |
| 9 | Messages compose field | Attaches as an image | `public.file-url` |
| 10 | Notes body | Embeds the image inline | `public.png` or `public.tiff` |
| 11 | Preview — an open document | Adds a page or opens the file | `public.file-url` |
| 12 | Figma canvas | Places the image on the canvas | `public.png` |
| 13 | VS Code editor | Opens the file (or inserts a path) | `public.file-url` |
| 14 | Terminal window | Types the escaped path | `public.file-url` → text bridge |
| 15 | Drop on empty space, then release | Card springs back, nothing is written | — |
| 16 | Press Escape mid-drag | Card springs back, temp file is removed | — |

Rows 1–9 and 13–14 exercise the file-URL path, which is the one that matters
most; rows 10 and 12 exercise the image flavours; rows 15–16 exercise
cancellation.

### While walking the matrix, also confirm

- **The drag image is the screenshot thumbnail**, not a generic document icon and
  not an empty rectangle.
- **An ordinary click still works** — the card's own buttons, hover states, and
  tap-to-expand are unchanged by the drag work.
- **A short drag that does not pass the threshold springs back** rather than
  arming.
- **The full-resolution image arrives**, not the preview. Check the pixel
  dimensions of what landed against the capture's real size.

---

## Two things this work could not verify

**1. Whether a non-activating panel can originate a drag.** The overlay is a
borderless, non-activating always-on-top window so it never steals focus from
what you are screenshotting. `beginDraggingSessionWithItems:event:source:` is
documented on `NSView` without qualification, and nothing in AppKit's contract
says the window must be key — but "not documented as forbidden" is not the same
as "observed to work", and no test on this machine can close that gap. **If any
drag fails to start at all, this is the first thing to suspect**, and the fix is
to activate the panel for the duration of the drag.

**2. Whether any specific application accepts the drop.** That is precisely what
the matrix above is for.

---

## Windows

The `IDataObject` / `DoDragDrop` backend in `crates/scrozz-shell/src/windows/`
is **type-checked against the real `windows` crate API surface but has never
been run.** That is layer 1 of [the platform strategy](platforms.md) and no
more.

Its **byte layouts are an exception, and are genuinely verified.** `CF_HDROP`
and `CF_UNICODETEXT` are pure data, so they are built in
`crates/scrozz-shell/src/drag/hdrop.rs` with no `windows` types and no `cfg`,
and their tests run on every platform on every push — header offset, `fWide`,
the double NUL that ends the path list, little-endian units, and a
non-ASCII path round-trip. A single Windows-only test asserts that the
hand-rolled 20-byte header really is `size_of::<DROPFILES>()`.

That matters because those are exactly the mistakes the type checker cannot
see and a drop fails silently on.

It offers three formats, in this preference order:

| Format | Why |
| --- | --- |
| `CF_HDROP` | The universal one. Explorer, Office, browsers, Slack, Discord, Teams |
| Registered `"PNG"` | What Chromium reads when it wants pixels rather than a path |
| `CF_UNICODETEXT` | The path as text — the cheap last resort |

Delayed rendering (`CFSTR_FILEDESCRIPTORW` + `CFSTR_FILECONTENTS`) is
deliberately not used. The file already exists on disk before the drag begins, so
a promise would buy nothing and cost compatibility with targets that only
understand `CF_HDROP`.

What a human on Windows must check, beyond the macOS matrix translated to
Explorer / Edge / Slack / Discord / Office:

- The drag image appears at all — `IDragSourceHelper` is best-effort, and every
  failure inside it is logged and swallowed rather than aborting the drag.
- The `DoDragDrop` modal loop does not visibly stall the overlay.
- Right-click and Escape both cancel cleanly, leaving no temp file behind.

## Linux

Linux keeps the planned-but-unimplemented source, which reports its own
incapability rather than pretending. XDND is not a payload format but a
**protocol conversation** — a sequence of client messages exchanged with the
destination window across many event-loop turns — and the event loop belongs to
winit, not to `scrozz-shell`. Implementing it honestly means threading XDND
through winit's dispatch, which is a larger piece of work than the drag payload
itself and is deliberately not attempted here.

The consequence is contained and visible: `DragSource::capability()` reports
`None` on Linux, the UI can ask before offering the affordance, and nothing
silently no-ops.
