#!/usr/bin/env python3
"""Regression checks for completed-DMG verification cleanup."""

import importlib.util
from pathlib import Path
import subprocess
import sys
import tempfile


SCRIPT = Path(__file__).with_name("verify-dmg.py")
SPEC = importlib.util.spec_from_file_location("scrozz_verify_dmg_test", SCRIPT)
VERIFY = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(VERIFY)


class FakeLifecycle:
    def __init__(self, *, applications=True, detach_succeeds=True):
        self.applications = applications
        self.detach_succeeds = detach_succeeds
        self._attached_devices = set()
        self.calls = []
        self.cleanup_calls = 0

    def hdiutil_with_forced_detach(self, command, *args, **kwargs):
        self.calls.append((command, args, kwargs))
        if command == "attach":
            mount = Path(args[args.index("-mountpoint") + 1])
            mount.mkdir(parents=True, exist_ok=True)
            if self.applications:
                (mount / "Applications").symlink_to("/Applications")
            self._attached_devices.add("/dev/disk-test")
            return (
                0,
                {
                    "system-entities": [
                        {
                            "dev-entry": "/dev/disk-test",
                            "mount-point": str(mount),
                        }
                    ]
                },
            )
        if self.detach_succeeds:
            self._attached_devices.discard("/dev/disk-test")
            return (0, "")
        return (1, "busy")

    def cleanup_attached_devices(self):
        self.cleanup_calls += 1
        if self.detach_succeeds:
            self._attached_devices.clear()


def run_case(lifecycle):
    with tempfile.TemporaryDirectory() as root:
        mount = Path(root, "mount")
        original_argv = sys.argv
        original_loader = VERIFY.load_lifecycle_wrapper
        original_run = VERIFY.subprocess.run
        commands = []
        VERIFY.load_lifecycle_wrapper = lambda: lifecycle

        def record(command, **kwargs):
            commands.append((command, kwargs))
            return subprocess.CompletedProcess(command, 0)

        VERIFY.subprocess.run = record
        sys.argv = [str(SCRIPT), str(Path(root, "Scrozz.dmg")), str(mount), "Scrozz"]
        try:
            result = VERIFY.main()
        finally:
            sys.argv = original_argv
            VERIFY.load_lifecycle_wrapper = original_loader
            VERIFY.subprocess.run = original_run
        return result, commands


def main():
    lifecycle = FakeLifecycle()
    result, commands = run_case(lifecycle)
    assert result == 0
    assert [call[0] for call in lifecycle.calls] == ["attach", "detach"]
    assert lifecycle.cleanup_calls == 1
    assert not lifecycle._attached_devices
    assert commands[0][0][0:3] == ["codesign", "--verify", "--strict"]
    assert commands[1][0][1] == "tools/verify-dmg-layout.py"

    lifecycle = FakeLifecycle(applications=False)
    try:
        run_case(lifecycle)
    except RuntimeError as error:
        assert str(error) == "Applications shortcut is missing"
    else:
        raise AssertionError("missing Applications shortcut passed verification")
    assert not lifecycle._attached_devices

    lifecycle = FakeLifecycle(detach_succeeds=False)
    try:
        run_case(lifecycle)
    except RuntimeError as error:
        assert "left an owned device mounted" in str(error)
    else:
        raise AssertionError("a leaked verification mount passed verification")
    assert lifecycle._attached_devices == {"/dev/disk-test"}


if __name__ == "__main__":
    main()
