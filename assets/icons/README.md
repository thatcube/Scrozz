# Scrozz icons

Everything here is generated from **`scrozz-icon.svg`**, which is the source of
truth. Do not edit the PNGs, the `.icns` or the `.ico` by hand — regenerate them.

| File | Used by |
|---|---|
| `scrozz-icon.svg` | Source of truth: the full app icon, plate and all |
| `scrozz-icon-32.svg` | Pixel-snapped Finder-list/Dock artwork |
| `scrozz-icon-16.svg` | 16px derivative of the exact 32px master |
| `Scrozz.icon/` | Native layered macOS 26 icon (light and dark appearances) |
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

### Small representations are separate artwork

The 16px and 32px PNGs are **not downscaled from `scrozz-icon.svg`**. Finder list
view exposed why that does not work: the large icon uses fractional transforms and
a Gaussian bloom, which look good in grid view but turn the one-pixel crop marks
muddy at list size.

`scrozz-icon-32.svg` removes the bloom and every fractional transform. The
original mark lands exactly on integer device pixels, while a wide radial wash in
the plate preserves the luminous family treatment without blurring the glyph.

`scrozz-icon-16.svg` keeps the exact original face and crop geometry. It is
reduced from Brandon's 32px master rather than substituted with a simplified
face: a first optical-size redraw made the `zz` eyes look like generic equals
signs, weakening the family identity more than any extra crispness helped.

### macOS 26 uses a native layered icon

Tahoe puts legacy `.icns` artwork with transparent corners inside a white/silver
compatibility container. The large icon looked fine in Finder grid view, while
list view exposed the icon shrunk inside that conspicuous outer tile. That tile
is added by macOS; changing pixels inside the `.icns` cannot remove it.

`Scrozz.icon/` is the Icon Composer source compiled by `actool` into `Assets.car`.
`CFBundleIconName` selects it on macOS 26, while `CFBundleIconFile` keeps
`Scrozz.icns` as the fallback for Sequoia and earlier.

The layered icon has real light and dark appearances. Light mode uses a pale
lavender plate with a dark-violet mark; dark mode keeps the near-black plate and
the original purple mark. The foreground halo peaks at 16.5% opacity—exactly 25%
below the first version—so it reads as ambient light rather than a visible disc.

## Regenerating

```bash
python3 - <<'PY'
import cairosvg
for s in [48, 64, 128, 256, 512, 1024]:
    cairosvg.svg2png(url="scrozz-icon.svg", write_to=f"icon-{s}.png",
                     output_width=s, output_height=s)
cairosvg.svg2png(url="scrozz-icon-16.svg", write_to="icon-16.png",
                 output_width=16, output_height=16)
cairosvg.svg2png(url="scrozz-icon-32.svg", write_to="icon-32.png",
                 output_width=32, output_height=32)
PY

# macOS legacy fallback — iconutil requires the @2x naming convention exactly
mkdir -p Scrozz.iconset
for s in 16 32 128 256 512; do
  cp icon-$s.png       "Scrozz.iconset/icon_${s}x${s}.png"
  cp icon-$((s*2)).png "Scrozz.iconset/icon_${s}x${s}@2x.png"
done
iconutil -c icns Scrozz.iconset -o Scrozz.icns

# macOS 26 — tools/make-app-bundle.sh runs this automatically
DEVELOPER_DIR=/Applications/Xcode.app/Contents/Developer \
  xcrun actool Scrozz.icon \
  --compile /path/to/Scrozz.app/Contents/Resources \
  --app-icon Scrozz.icon \
  --target-device mac --platform macosx \
  --minimum-deployment-target 12.3 \
  --enable-on-demand-resources NO \
  --enable-icon-stack-fallback-generation=disabled \
  --include-all-app-icons --output-partial-info-plist /dev/null

# Windows — multi-resolution ico
magick icon-16.png icon-32.png icon-48.png icon-64.png icon-128.png icon-256.png Scrozz.ico
```

**Always check 32px and 16px after any change.** The icon lives in the Dock, the
menu bar and the taskbar far more than it lives at 512px, and a change that looks
better large can lose the face entirely small.
