#!/usr/bin/env python3
"""Hermetic lifecycle tests for the dmgbuild detach wrapper."""

import importlib.util
import io
import tempfile
from contextlib import redirect_stderr
from pathlib import Path


path = Path(__file__).with_name("run-dmgbuild.py")
spec = importlib.util.spec_from_file_location("scrozz_run_dmgbuild", path)
wrapper = importlib.util.module_from_spec(spec)
spec.loader.exec_module(wrapper)


def assert_failed_detach_remains_tracked():
    wrapper._attached_devices.clear()
    wrapper._attached_devices.add("/dev/disk-test")
    wrapper._hdiutil = lambda *_args, **_kwargs: (1, "busy")
    wrapper.time.sleep = lambda _delay: None

    with redirect_stderr(io.StringIO()):
        wrapper.cleanup_attached_devices()

    assert wrapper._attached_devices == {"/dev/disk-test"}
    wrapper._attached_devices.clear()


def assert_cleanup_retries_until_detached():
    wrapper._attached_devices.clear()
    wrapper._attached_devices.add("/dev/disk-test")
    attempts = 0

    def detach(*_args, **_kwargs):
        nonlocal attempts
        attempts += 1
        return (0, "detached") if attempts == 2 else (1, "busy")

    wrapper._hdiutil = detach
    wrapper.time.sleep = lambda _delay: None
    wrapper.cleanup_attached_devices()

    assert attempts == 2
    assert not wrapper._attached_devices


def assert_attach_registration_is_signal_atomic():
    wrapper._attached_devices.clear()
    masks = []

    def mask(how, signals):
        masks.append((how, tuple(signals)))
        return set()

    wrapper.signal.pthread_sigmask = mask
    wrapper._hdiutil = lambda *_args, **_kwargs: (
        0,
        {
            "system-entities": [
                {
                    "mount-point": "/Volumes/Scrozz",
                    "dev-entry": "/dev/disk-test",
                }
            ]
        },
    )

    wrapper.hdiutil_with_forced_detach("attach", "owned.dmg")

    assert wrapper._attached_devices == {"/dev/disk-test"}
    assert masks[0][0] == wrapper.signal.SIG_BLOCK
    assert masks[-1][0] == wrapper.signal.SIG_SETMASK
    wrapper._attached_devices.clear()


def assert_hdiutil_child_clears_signal_mask():
    popen = wrapper.subprocess.Popen
    masks = []
    captured = {}

    class Process:
        def communicate(self):
            return wrapper.plistlib.dumps({"system-entities": []}), None

        def wait(self):
            return 0

    def spawn(command, **kwargs):
        captured["command"] = command
        captured["kwargs"] = kwargs
        return Process()

    wrapper.subprocess.Popen = spawn
    wrapper.signal.pthread_sigmask = lambda how, signals: masks.append(
        (how, tuple(signals))
    )
    try:
        result = wrapper.system_hdiutil("attach", "owned.dmg")
        captured["kwargs"]["preexec_fn"]()
    finally:
        wrapper.subprocess.Popen = popen

    assert result == (0, {"system-entities": []})
    assert captured["command"][-1] == "-plist"
    assert masks == [(wrapper.signal.SIG_SETMASK, ())]


def assert_interrupted_attach_reconciles_and_detaches():
    wrapper._attached_devices.clear()
    calls = []
    attached = False

    with tempfile.NamedTemporaryFile(suffix=".dmg") as image:
        image_path = str(Path(image.name).resolve())

        def hdiutil(command, *args, **_kwargs):
            nonlocal attached
            calls.append((command, args))
            if command == "info":
                images = []
                if attached:
                    images.append(
                        {
                            "image-path": image_path,
                            "system-entities": [
                                {
                                    "dev-entry": "/dev/disk-test",
                                }
                            ],
                        }
                    )
                return 0, {"images": images}
            if command == "attach":
                attached = True
                return -2, {}
            if command == "detach":
                attached = False
                return 0, "detached"
            raise AssertionError(command)

        wrapper._hdiutil = hdiutil
        result = wrapper.hdiutil_with_forced_detach("attach", image_path)

    assert result == (-2, {})
    assert not attached
    assert not wrapper._attached_devices
    assert [call[0] for call in calls] == ["info", "attach", "info", "detach"]


assert_failed_detach_remains_tracked()
assert_cleanup_retries_until_detached()
assert_attach_registration_is_signal_atomic()
assert_hdiutil_child_clears_signal_mask()
assert_interrupted_attach_reconciles_and_detaches()
print("dmgbuild wrapper lifecycle checks passed")
