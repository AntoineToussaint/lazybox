# dmgbuild settings for the Lazybox disk image (scripts/release/build.mjs).
#
# dmgbuild writes the window's .DS_Store itself, so the layout (background,
# icon positions, window size) needs no Finder scripting and builds the same
# on a CI runner as on a desk. The build passes:
#   -D app=<path to Lazybox.app>  -D background=<background.tiff>  -D icon=<icon.icns>
#   -D notices=<THIRD_PARTY_NOTICES.txt> (optional)
import os.path

application = defines["app"]  # noqa: F821 (provided by dmgbuild)
appname = os.path.basename(application)

format = "UDZO"
compression_level = 9
filesystem = "HFS+"
size = None

# The third-party notices travel with the app (also inside it, in
# Contents/Resources), placed under the drag-to-Applications layout: in the
# image, out of the way of the two slots, a scroll away in the window.
notices = defines.get("notices")  # noqa: F821
files = [application] + ([notices] if notices else [])
symlinks = {"Applications": "/Applications"}
icon = defines.get("icon")  # noqa: F821

background = defines["background"]  # noqa: F821
window_rect = ((200, 120), (660, 400))
default_view = "icon-view"
show_status_bar = False
show_tab_view = False
show_toolbar = False
show_pathbar = False
show_sidebar = False
show_icon_preview = False
include_icon_view_settings = True
include_list_view_settings = False

arrange_by = None
grid_offset = (0, 0)
grid_spacing = 100
scroll_position = (0, 0)
label_pos = "bottom"
text_size = 13
icon_size = 112

# The centres of the two slots drawn in background.html.
icon_locations = {
    appname: (165, 190),
    "Applications": (495, 190),
}
if notices:
    icon_locations[os.path.basename(notices)] = (330, 520)
