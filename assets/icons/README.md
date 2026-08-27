# Scrozz icons

Everything here is generated from **`scrozz-icon.svg`**, which is the source of
truth. Do not edit the PNGs, the `.icns` or the `.ico` by hand — regenerate them.

| File | Used by |
|---|---|
| `scrozz-icon.svg` | Source of truth: the full app icon, plate and all |
| `scrozz-mark.svg` | The bare 32px mark on a flat plate, for in-app use and favicons |
| `Scrozz.icns` | macOS app bundle |
| `Scrozz.ico` | Windows executable and installer |
| `icon-*.png` | Linux (`hicolor` theme), Flathub, README, store listings |

## The design

The mark is **four crop-mark corners around the `ozz` family face** — the same
pixel grid, `zz` eyes and smile as Mozz, Plozz, Hozz and Twozz. The corners are
what make it specifically a *screenshot* app: they read as a selection region
before you have consciously identified them.

The plate is near-black with a faint purple cast (`#191622` → `#0D0B14`) and the
mark is lit from within by a wide, soft radial halo. **That glow is the family
signature** — Mozz's ghost emits light the same way — so it is not decoration and
should not be flattened away.

One thing to preserve if the mark is ever redrawn: the crop corners are thin
strokes, so as they spread toward the edges the centre of the icon empties out.
The face is scaled up slightly (×1.06) about the centre to compensate. Push the
corners further without also growing the face and the icon reads hollow.

Purple was tried as a plate colour and rejected — purple-on-purple loses the mark,
and the contrast collapses at 16px.

## Regenerating

```bash
python3 - <<'PY'
import cairosvg
for s in [16, 32, 48, 64, 128, 256, 512, 1024]:
    cairosvg.svg2png(url="scrozz-icon.svg", write_to=f"icon-{s}.png",
                     output_width=s, output_height=s)
PY

# macOS — iconutil requires the @2x naming convention exactly
mkdir -p Scrozz.iconset
for s in 16 32 128 256 512; do
  cp icon-$s.png       "Scrozz.iconset/icon_${s}x${s}.png"
  cp icon-$((s*2)).png "Scrozz.iconset/icon_${s}x${s}@2x.png"
done
iconutil -c icns Scrozz.iconset -o Scrozz.icns

# Windows — multi-resolution ico
magick icon-16.png icon-32.png icon-48.png icon-64.png icon-128.png icon-256.png Scrozz.ico
```

**Always check 32px and 16px after any change.** The icon lives in the Dock, the
menu bar and the taskbar far more than it lives at 512px, and a change that looks
better large can lose the face entirely small.
