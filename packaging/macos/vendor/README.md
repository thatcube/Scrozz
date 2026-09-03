# Finder disk-image dependencies

`tools/make-dmg.sh` imports these pure-Python wheels directly from this directory
so release packaging never downloads mutable build tooling:

| Wheel | Version | SHA-256 |
|---|---:|---|
| `dmgbuild-1.6.7-py3-none-any.whl` | 1.6.7 | `37ee5771c377beb3203d9164aae8046ffed8531c06edf9227f5788b3c599b1bf` |
| `ds_store-1.3.3-py3-none-any.whl` | 1.3.3 | `b92a371efbf1b4ccce2a04d1ed13fceacc4736c81ba09cf5aefb74c088160a35` |
| `mac_alias-2.2.3-py3-none-any.whl` | 2.2.3 | `7362b521d2132ef92f606a37abfed5fcd849ceb2f28b6f9743e014b02af92f0d` |

All three projects are distributed under the MIT license and include their license
files inside the wheels.
