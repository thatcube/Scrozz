#!/usr/bin/env python3
"""Verify Finder metadata in a mounted Scrozz installer image."""

from pathlib import Path
import sys

from ds_store import DSStore
from mac_alias import Alias


def require(condition: bool, message: str) -> None:
    if not condition:
        raise ValueError(message)


def main() -> int:
    if len(sys.argv) not in (2, 3):
        print("usage: verify-dmg-layout.py MOUNT_POINT [VOLUME_NAME]", file=sys.stderr)
        return 2

    mount = Path(sys.argv[1]).resolve()
    volume_name = sys.argv[2] if len(sys.argv) == 3 else "Scrozz"
    background = mount / ".background.tiff"
    require(background.is_file(), f"missing DMG background: {background}")

    with DSStore.open(str(mount / ".DS_Store"), "r") as store:
        records = list(store)
        browser = store["."]["bwsp"]
        icons = store["."]["icvp"]
        view = store["."]["icvl"]

        require(
            browser["WindowBounds"] == "{{160, 120}, {720, 460}}",
            "unexpected Finder window bounds",
        )
        for key in (
            "ShowStatusBar",
            "ShowTabView",
            "ShowToolbar",
            "ShowPathbar",
            "ShowSidebar",
            "ContainerShowSidebar",
        ):
            require(browser[key] is False, f"{key} is not disabled")

        require(view == (b"type", b"icnv"), "Finder default view is not icon view")
        require(icons["backgroundType"] == 2, "Finder background is not image-backed")
        require(icons["iconSize"] == 112.0, "unexpected Finder icon size")
        for key in (
            "backgroundColorRed",
            "backgroundColorGreen",
            "backgroundColorBlue",
        ):
            require(icons[key] == 1.0, f"missing Finder fallback color: {key}")
        require(
            store["Scrozz.app"]["Iloc"] == (180, 250),
            "Scrozz app icon moved",
        )
        require(
            store["Applications"]["Iloc"] == (540, 250),
            "Applications icon moved",
        )
        require(
            not any(record.code == b"pBBk" for record in records),
            "Tahoe-incompatible background bookmark is present",
        )

        alias = Alias.from_bytes(icons["backgroundImageAlias"])
        require(
            alias.target.filename == ".background.tiff",
            "Finder background alias has the wrong target",
        )
        require(
            alias.target.posix_path == "/.background.tiff",
            "Finder background alias is not volume-relative",
        )
        require(
            alias.volume.posix_path == f"/Volumes/{volume_name}",
            "Finder background alias was not created on the final mount",
        )
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
