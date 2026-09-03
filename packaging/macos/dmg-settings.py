"""Finder presentation for the Scrozz drag-to-Applications disk image."""

from pathlib import Path
import subprocess

application = defines["app"]
background = defines["background"]
icon = defines["volume_icon"]

files = [
    (defines["no_index"], ".metadata_never_index"),
    (application, "Scrozz.app"),
    (defines["legal"], ".background"),
]
if preview := defines.get("preview"):
    files.append((preview, "PREVIEW.txt"))

symlinks = {"Applications": "/Applications"}

filesystem = "HFS+"
format = "UDZO"
compression_level = 9

window_rect = ((160, 120), (720, 460))
default_view = "icon-view"
include_icon_view_settings = True

show_status_bar = False
show_tab_view = False
show_toolbar = False
show_pathbar = False
show_sidebar = False

arrange_by = None
grid_spacing = 100.0
show_icon_preview = True
show_item_info = False
label_pos = "bottom"
text_size = 13.0
icon_size = 112.0

icon_locations = {
    "Scrozz.app": (180, 250),
    "Applications": (540, 250),
}


def create_hook(mount_point, _options):
    """Keep Spotlight from holding the writable image open during conversion."""
    Path(mount_point, ".metadata_never_index").touch()
    subprocess.run(
        ["/usr/bin/mdutil", "-i", "off", mount_point],
        check=True,
        stdout=subprocess.DEVNULL,
        stderr=subprocess.DEVNULL,
    )
