#!/usr/bin/env python3
"""Run dmgbuild with a safe fallback for scanners that veto clean detachment."""

import atexit
import os
import plistlib
import signal
import subprocess
import sys
import time

import dmgbuild.core


_attached_devices = set()
_handled_signals = (signal.SIGHUP, signal.SIGINT, signal.SIGTERM)


def unblock_child_signals():
    if hasattr(signal, "pthread_sigmask"):
        signal.pthread_sigmask(signal.SIG_SETMASK, ())


def system_hdiutil(cmd, *args, **kwargs):
    plist = kwargs.get("plist", True)
    command = ["/usr/bin/hdiutil", cmd, *args]
    if plist:
        command.append("-plist")
    process = subprocess.Popen(
        command,
        stdout=subprocess.PIPE,
        stderr=None if plist else subprocess.STDOUT,
        close_fds=True,
        preexec_fn=unblock_child_signals,
    )
    output, _ = process.communicate()
    result = plistlib.loads(output) if plist else output.decode()
    return process.wait(), result


_hdiutil = system_hdiutil


def detach_target(args):
    return next(
        (arg for arg in args if isinstance(arg, str) and arg.startswith("/dev/")),
        None,
    )


def attach_image(args):
    for arg in args:
        if isinstance(arg, str) and os.path.isfile(arg):
            return os.path.realpath(arg)
    return None


def mounted_devices_for_image(image):
    if image is None:
        return set()
    try:
        status, info = _hdiutil("info")
    except BaseException:
        return set()
    if status != 0 or not isinstance(info, dict):
        return set()
    devices = set()
    for attached in info.get("images", ()):
        attached_path = attached.get("image-path")
        if not attached_path or os.path.realpath(attached_path) != image:
            continue
        entries = [
            entity["dev-entry"]
            for entity in attached.get("system-entities", ())
            if entity.get("dev-entry")
        ]
        if entries:
            # The first entity is the image's root device. Detaching it also
            # retires child partitions and any synthesized APFS device.
            devices.add(entries[0])
    return devices


def cleanup_attached_devices():
    for attempt in range(3):
        for device in tuple(_attached_devices):
            result = _hdiutil("detach", "-force", device, plist=False)
            if result[0] == 0:
                _attached_devices.discard(device)
        if not _attached_devices:
            return
        time.sleep(0.1 * (attempt + 1))
    if _attached_devices:
        print(
            "dmgbuild: could not detach owned device(s): "
            + ", ".join(sorted(_attached_devices)),
            file=sys.stderr,
        )


def terminate(signum, _frame):
    cleanup_attached_devices()
    raise SystemExit(128 + signum)


def hdiutil_with_forced_detach(cmd, *args, **kwargs):
    previous_mask = None
    image = attach_image(args) if cmd == "attach" else None
    baseline_devices = set()
    if cmd == "attach" and hasattr(signal, "pthread_sigmask"):
        previous_mask = signal.pthread_sigmask(signal.SIG_BLOCK, _handled_signals)
    if image is not None:
        baseline_devices = mounted_devices_for_image(image)
    try:
        try:
            result = _hdiutil(cmd, *args, **kwargs)
        except BaseException:
            if image is not None:
                _attached_devices.update(
                    mounted_devices_for_image(image) - baseline_devices
                )
                cleanup_attached_devices()
            raise
        if cmd == "attach":
            if result[0] == 0:
                for entity in result[1].get("system-entities", ()):
                    if entity.get("mount-point") and (device := entity.get("dev-entry")):
                        if device not in baseline_devices:
                            _attached_devices.add(device)
            elif image is not None:
                _attached_devices.update(
                    mounted_devices_for_image(image) - baseline_devices
                )
                cleanup_attached_devices()
    finally:
        if previous_mask is not None:
            signal.pthread_sigmask(signal.SIG_SETMASK, previous_mask)
    device = detach_target(args) if cmd == "detach" else None
    if device and result[0] == 0:
        _attached_devices.discard(device)
    if cmd != "detach" or "-force" in args or result[0] == 0:
        return result

    forced = _hdiutil("detach", "-force", *args, **kwargs)
    if forced[0] == 0:
        if device:
            _attached_devices.discard(device)
        print(
            "dmgbuild: clean detach was vetoed after sync; forced owned image detach",
            file=sys.stderr,
        )
        return forced
    return result


dmgbuild.core.hdiutil = hdiutil_with_forced_detach
atexit.register(cleanup_attached_devices)
for handled_signal in _handled_signals:
    signal.signal(handled_signal, terminate)

from dmgbuild.__main__ import main  # noqa: E402


if __name__ == "__main__":
    main()
