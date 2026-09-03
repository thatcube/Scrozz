#!/usr/bin/env python3
"""Mount and verify a completed DMG with signal-safe owned-device cleanup."""

import importlib.util
from pathlib import Path
import subprocess
import sys


def load_lifecycle_wrapper():
    path = Path(__file__).with_name("run-dmgbuild.py")
    spec = importlib.util.spec_from_file_location("scrozz_run_dmgbuild_verify", path)
    module = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(module)
    return module


def main() -> int:
    if len(sys.argv) != 4:
        print("usage: verify-dmg.py IMAGE MOUNT_POINT VOLUME_NAME", file=sys.stderr)
        return 2

    image = str(Path(sys.argv[1]).resolve())
    mount = str(Path(sys.argv[2]).resolve())
    volume_name = sys.argv[3]
    lifecycle = load_lifecycle_wrapper()
    attached = lifecycle.hdiutil_with_forced_detach(
        "attach",
        image,
        "-readonly",
        "-nobrowse",
        "-mountpoint",
        mount,
    )
    if attached[0] != 0:
        raise RuntimeError(f"could not mount completed DMG: {attached[1]}")
    device = next(
        (
            entity["dev-entry"]
            for entity in attached[1].get("system-entities", ())
            if entity.get("mount-point") == mount and entity.get("dev-entry")
        ),
        None,
    )
    if device is None:
        lifecycle.cleanup_attached_devices()
        raise RuntimeError("completed DMG mounted without a tracked device")

    try:
        applications = Path(mount, "Applications")
        if not applications.is_symlink():
            raise RuntimeError("Applications shortcut is missing")
        subprocess.run(
            ["codesign", "--verify", "--strict", "--verbose=2", f"{mount}/Scrozz.app"],
            check=True,
        )
        subprocess.run(
            [sys.executable, "tools/verify-dmg-layout.py", mount, volume_name],
            check=True,
        )
    finally:
        lifecycle.hdiutil_with_forced_detach("detach", device, plist=False)
        lifecycle.cleanup_attached_devices()

    if lifecycle._attached_devices:
        raise RuntimeError(
            "completed DMG verification left an owned device mounted: "
            + ", ".join(sorted(lifecycle._attached_devices))
        )
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
