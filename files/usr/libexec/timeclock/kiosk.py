#!/usr/bin/python3
"""The clock's kiosk browser: one WebKitGTK window with no browser UI, under its own Wayland app id.

sway and the watcher tell the clock window from the standby's by that id. sway places and shows the window; this only
loads pages. A failed load goes back to the start page after a second, so no error page ever shows; a crashed web
process reloads it. Until the first page has loaded, the window shows the boot splash (Splash) instead of its own white
background.

Usage: kiosk.py <app-id> <start-url>
"""

# Standard library imports
import configparser
import sys
from pathlib import Path

# Third party imports
import gi

gi.require_version("Gdk", "4.0")
gi.require_version("Graphene", "1.0")
gi.require_version("Gtk", "4.0")
gi.require_version("WebKit", "6.0")
from gi.repository import Gdk, Gio, GLib, Graphene, Gtk, WebKit  # noqa: E402

RETRY_MS = 1000
THEME = Path("/usr/share/plymouth/themes/timeclock/timeclock.plymouth")
SPIN_SECONDS = 2.0  # plymouth's THROBBER_DURATION: one turn through the throbber frames
# No pinch zoom (owner): a touch with two or more fingers has its default cancelled in every page, and WebKit only runs
# its zoom gesture on touches the page didn't cancel. One-finger touches go through untouched.
NO_PINCH = """
for (const type of ["touchstart", "touchmove"]) {
  document.addEventListener(type, (e) => { if (e.touches.length > 1) e.preventDefault(); }, { passive: false, capture: true });
}
"""


class Splash(Gtk.Widget):
  """The boot splash, from the same theme file and images, laid out and timed as plymouth's two-step does it.

  The watermark at alignment x (screen - image), the throbber centred at alignment x screen, its frame from the time
  since start. Without the theme (a dev machine), just its black.
  """

  def __init__(self) -> None:  # noqa: D107
    super().__init__()
    self.bg, self.mark, self.frames, self.frame = Gdk.RGBA(), None, [], 0
    self.bg.parse("black")
    try:
      theme = configparser.ConfigParser(interpolation=None)
      theme.read(THEME, encoding="utf-8")
      t = theme["two-step"]
      images = Path(t["ImageDir"])
      self.bg.parse("#" + t.get("BackgroundStartColor", "0x000000").removeprefix("0x"))
      self.mark_at = float(t["WatermarkHorizontalAlignment"]), float(t["WatermarkVerticalAlignment"])
      self.spin_at = float(t["HorizontalAlignment"]), float(t["VerticalAlignment"])
      self.mark = Gdk.Texture.new_from_filename(str(images / "watermark.png"))
      self.frames = [Gdk.Texture.new_from_filename(str(p)) for p in sorted(images.glob("throbber-*.png"))]
    except (KeyError, ValueError, GLib.Error) as e:
      print(f"splash: {THEME}: {e}; black only", flush=True)
    self.start = GLib.get_monotonic_time()
    self.ticking = self.add_tick_callback(self.tick)

  def tick(self, _widget: Gtk.Widget, _clock: Gdk.FrameClock) -> bool:
    """Every display frame: redraw once the throbber's next frame is due."""
    if self.frames:
      turn = (GLib.get_monotonic_time() - self.start) / 1e6 % SPIN_SECONDS / SPIN_SECONDS
      if (frame := int(len(self.frames) * turn)) != self.frame:
        self.frame = frame
        self.queue_draw()
    return GLib.SOURCE_CONTINUE

  def do_snapshot(self, snapshot: Gtk.Snapshot) -> None:  # noqa: D102 - GTK's draw
    w, h = self.get_width(), self.get_height()
    snapshot.append_color(self.bg, Graphene.Rect().init(0, 0, w, h))
    if self.mark:
      tw, th = self.mark.get_width(), self.mark.get_height()
      x, y = int(w * self.mark_at[0] - tw * self.mark_at[0]), int(h * self.mark_at[1] - th * self.mark_at[1])
      snapshot.append_texture(self.mark, Graphene.Rect().init(x, y, tw, th))
    if self.frames:
      f = self.frames[self.frame]
      tw, th = f.get_width(), f.get_height()
      x, y = int(w * self.spin_at[0] - tw / 2), int(h * self.spin_at[1] - th / 2)
      snapshot.append_texture(f, Graphene.Rect().init(x, y, tw, th))


def main() -> int:  # noqa: D103
  app_id, start = sys.argv[1], sys.argv[2]
  # NON_UNIQUE: the clock and the standby are separate processes, each under its own systemd unit (the watcher counts
  # the clock unit's connections), never one merged instance.
  app = Gtk.Application(application_id=app_id, flags=Gio.ApplicationFlags.NON_UNIQUE)

  def activate(app: Gtk.Application) -> None:
    content = WebKit.UserContentManager()
    content.add_script(
      WebKit.UserScript.new(NO_PINCH, WebKit.UserContentInjectedFrames.ALL_FRAMES, WebKit.UserScriptInjectionTime.START, None, None)
    )
    view = WebKit.WebView(user_content_manager=content)
    black = Gdk.RGBA()
    black.parse("black")
    view.set_background_color(black)
    view.connect("context-menu", lambda *_: True)  # no right-click or long-press menu
    # The splash over the page until its first load: WebKit takes seconds to draw anything (more on a fresh install).
    page, splash = Gtk.Overlay(child=view), Splash()
    page.add_overlay(splash)

    def back_to_start() -> bool:
      view.load_uri(start)
      return False

    load_failed = False  # a failed load ends with FINISHED too

    def changed(view: WebKit.WebView, event: WebKit.LoadEvent) -> None:
      nonlocal load_failed
      if event == WebKit.LoadEvent.STARTED:
        load_failed = False
      elif event == WebKit.LoadEvent.FINISHED and not load_failed:
        print(f"<{view.get_uri()}> loaded", flush=True)
        if splash.get_parent():
          splash.remove_tick_callback(splash.ticking)
          page.remove_overlay(splash)

    def failed(_view: WebKit.WebView, _event: WebKit.LoadEvent, uri: str, error: GLib.Error) -> bool:
      nonlocal load_failed
      load_failed = True
      print(f"<{uri}> failed ({error.message}); back to the start page", flush=True)
      GLib.timeout_add(RETRY_MS, back_to_start)
      return True  # handled: no error page

    def terminated(_view: WebKit.WebView, reason: WebKit.WebProcessTerminationReason) -> None:
      print(f"web process ended ({reason.value_nick}); reloading the start page", flush=True)
      back_to_start()

    view.connect("load-changed", changed)
    view.connect("load-failed", failed)
    view.connect("web-process-terminated", terminated)
    window = Gtk.ApplicationWindow(application=app)
    window.set_decorated(False)
    window.set_child(page)
    window.present()
    view.load_uri(start)

  app.connect("activate", activate)
  return app.run([sys.argv[0]])


if __name__ == "__main__":
  sys.exit(main())
