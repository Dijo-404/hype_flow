"""Native GTK3 layer-shell overlay, following Handy's Linux surface setup."""

import math
from pathlib import Path
import time

import gi

gi.require_version("Gtk", "3.0")
gi.require_version("Gdk", "3.0")
gi.require_version("GtkLayerShell", "0.1")
gi.require_version("PangoCairo", "1.0")
gi.require_version("GdkPixbuf", "2.0")
from gi.repository import Gdk, GdkPixbuf, Gio, GLib, Gtk, GtkLayerShell, Pango, PangoCairo
import cairo

from .audio import HOLD_SECONDS, sources
from .cleanup import DEFAULT_FILTERS
from .config import UNLOAD_CHOICES
from .hook import hook_path
from .setup import MODEL_FILES


# Geometry, timing and colours from OpenWhispr's VoicePill, PillWaveform, waveformMath,
# voicePillPresentation and dictation-panel.css (dark theme).
BARS, BAR_MIN, BAR_MAX = 11, 4, 22
INSET, HEIGHT = 12, 72  # dock inset keeps the glow inside the fixed surface
SAMPLE_SECONDS = 0.08
CIRCLE_STATES = ("starting", "transcribing", "delivering")
THINKING_STATES = ("transcribing", "delivering")
PULSE_STATES = ("starting", "loading")
TEXT_STATES = ("idle", "loading", "done", "error")
FOREGROUND, MUTED = (0.91, 0.91, 0.91), (0.604, 0.612, 0.631)
SURFACE_1, SURFACE_3 = (0.125, 0.133, 0.145), (0.169, 0.180, 0.196)
BORDER_HOVER, BORDER_60, BORDER_50 = (0.290, 0.302, 0.325), (0.184, 0.192, 0.204), (0.173, 0.180, 0.192)
SIGNAL_BRIGHT, SIGNAL_CORE = (0.220, 0.741, 0.973), (0.145, 0.388, 0.922)
SIGNAL_DEEP, SIGNAL_HEAD = (0.388, 0.400, 0.945), (0.918, 0.965, 1.0)
APPEARANCE = {  # (border, icon colour, icon alpha)
    "idle": (BORDER_50, MUTED, 1.0), "loading": (BORDER_60, MUTED, 1.0), "starting": (BORDER_60, FOREGROUND, 0.7),
    "recording": (BORDER_HOVER, FOREGROUND, 1.0), "transcribing": (BORDER_60, FOREGROUND, 1.0),
    "delivering": (BORDER_60, FOREGROUND, 1.0), "done": (BORDER_HOVER, FOREGROUND, 1.0),
    "error": ((0.541, 0.227, 0.239), FOREGROUND, 1.0),
}


def bezier(x1, y1, x2, y2):
    """CSS cubic-bezier timing function."""
    def curve(progress):
        low, high = 0.0, 1.0
        for _ in range(20):
            t = (low + high) / 2
            if 3 * (1 - t) ** 2 * t * x1 + 3 * (1 - t) * t * t * x2 + t ** 3 < progress:
                low = t
            else:
                high = t
        t = (low + high) / 2
        return 3 * (1 - t) ** 2 * t * y1 + 3 * (1 - t) * t * t * y2 + t ** 3
    return curve


GROW = bezier(0.2, 0, 0, 1)            # VOICE_PILL_GROW_EASING
EASE_OUT = bezier(0, 0, 0.2, 1)        # Tailwind v4 ease-out utility
CSS_EASE_OUT = bezier(0, 0, 0.58, 1)   # plain CSS ease-out
CSS_EASE_IN_OUT = bezier(0.42, 0, 0.58, 1)
PULSE = bezier(0.4, 0, 0.6, 1)         # Tailwind animate-pulse


def clock():
    return GLib.get_monotonic_time() / 1e6


def animations():
    return Gtk.Settings.get_default().get_property("gtk-enable-animations")


class Tween:
    """One CSS transition over a tuple of floats; retargeting starts from the current value."""

    def __init__(self, value):
        self.start = self.end = tuple(value)
        self.began, self.duration, self.curve = 0.0, 0.0, None

    def to(self, value, now, duration, curve):
        value = tuple(value)
        if value != self.end:
            self.start, self.end = self.value(now), value
            self.began, self.duration, self.curve = now, duration if animations() else 0.0, curve

    def value(self, now):
        if not self.busy(now):
            return self.end
        k = self.curve(max(0.0, (now - self.began) / self.duration))
        return tuple(a + (b - a) * k for a, b in zip(self.start, self.end))

    def busy(self, now):
        return self.duration > 0 and now < self.began + self.duration


def pulse_opacity(now):
    """animate-pulse: 2s, 50% { opacity: .5 }."""
    phase = (now % 2.0) / 2.0
    if phase < 0.5:
        return 1 - 0.5 * PULSE(phase * 2)
    return 0.5 + 0.5 * PULSE((phase - 0.5) * 2)


def capsule(context, x, y, width, height, grow=0.0):
    x, y, width, height = x - grow, y - grow, width + 2 * grow, height + 2 * grow
    radius = min(width, height) / 2
    context.new_sub_path()
    context.arc(x + width - radius, y + radius, radius, -math.pi / 2, math.pi / 2)
    context.arc(x + radius, y + radius, radius, math.pi / 2, 3 * math.pi / 2)
    context.close_path()


def conic(cx, cy, angle, stops):
    """CSS conic-gradient(from angle, ...) as a smooth Cairo mesh: 0deg points up, clockwise."""
    def color(position):
        for (p0, c0), (p1, c1) in zip(stops, stops[1:]):
            if position <= p1:
                k = 0 if p1 == p0 else (position - p0) / (p1 - p0)
                return tuple(a + (b - a) * k for a, b in zip(c0, c1))
        return stops[-1][1]

    mesh, segments, reach = cairo.MeshPattern(), 72, 400
    for index in range(segments):
        a0, a1 = index / segments, (index + 1) / segments
        t0, t1 = (math.radians(angle) + 2 * math.pi * a - math.pi / 2 for a in (a0, a1))
        mesh.begin_patch()
        mesh.move_to(cx, cy)
        mesh.line_to(cx + reach * math.cos(t0), cy + reach * math.sin(t0))
        mesh.line_to(cx + reach * math.cos(t1), cy + reach * math.sin(t1))
        mesh.line_to(cx, cy)
        for corner, position in enumerate((a0, a0, a1, a1)):
            mesh.set_corner_color_rgba(corner, *color(position))
        mesh.end_patch()
    return mesh


class Overlay:
    def __init__(self, action):
        if not GtkLayerShell.is_supported():
            raise RuntimeError("Wayland layer-shell is unavailable. Run inside niri.")
        self.action = action
        self.state = "idle"
        self.volume = 0.0
        self.levels = [0.0] * BARS
        self.bars_from = self.bars_to = [float(BAR_MIN)] * BARS
        self.sampled = 0.0
        self.generation = 0
        self.shown = False
        self.title, self.detail = "Hype Flow", "Ready"
        self.region = None
        self.window = Gtk.Window(title="Hype Flow")
        self.window.set_name("hype-flow")
        self.window.set_decorated(False)
        self.window.set_resizable(False)
        self.window.set_app_paintable(True)
        self.window.set_visual(self.window.get_screen().get_rgba_visual())
        GtkLayerShell.init_for_window(self.window)
        GtkLayerShell.set_namespace(self.window, "hype-flow")
        GtkLayerShell.set_layer(self.window, GtkLayerShell.Layer.OVERLAY)
        GtkLayerShell.set_keyboard_mode(self.window, GtkLayerShell.KeyboardMode.NONE)
        GtkLayerShell.set_exclusive_zone(self.window, 0)
        GtkLayerShell.set_anchor(self.window, GtkLayerShell.Edge.BOTTOM, True)
        GtkLayerShell.set_margin(self.window, GtkLayerShell.Edge.BOTTOM, 20)
        self.area = Gtk.DrawingArea()
        self.area.add_events(Gdk.EventMask.BUTTON_PRESS_MASK | Gdk.EventMask.ENTER_NOTIFY_MASK
                             | Gdk.EventMask.LEAVE_NOTIFY_MASK)
        self.area.connect("draw", self.draw)
        self.area.connect("button-press-event", self.clicked)
        self.area.connect("enter-notify-event", lambda *_: self.hovered(True))
        self.area.connect("leave-notify-event", lambda *_: self.hovered(False))
        self.area.add_tick_callback(self.tick)
        self.area.get_accessible().set_name("Hype Flow")
        self.window.add(self.area)
        self.title_font = Pango.FontDescription.from_string("Noto Sans Medium 13px")
        self.detail_font = Pango.FontDescription.from_string("Noto Sans 11px")
        now = clock()
        self.geometry = Tween(self.shape("text"))
        self.colors = Tween(self.appearance("idle"))
        self.wave = Tween((0.0,))
        self.glow = Tween((0.0,))
        self.hover = Tween((0.0,))
        self.alpha = Tween((0.0,))
        self.morph("text", now)

    def clicked(self, _, event):
        if event.button == 3:
            self.action("cancel")
        elif event.button == 1:
            self.action("toggle" if self.state in ("recording", "starting") else "show")
        return True

    def hovered(self, inside):
        self.hover.to((1.0 if inside else 0.0,), clock(), 0.2, EASE_OUT)
        self.area.queue_draw()
        return False

    def layout(self, text, font):
        layout = self.area.create_pango_layout(text)
        layout.set_font_description(font)
        layout.set_width(300 * Pango.SCALE)
        layout.set_ellipsize(Pango.EllipsizeMode.END)
        layout.set_single_paragraph_mode(True)
        return layout

    def shape(self, kind):
        """(width, height, icon inset, waveform width, text alpha) of each footprint."""
        if kind == "wave":
            return (98.0, 36.0, 6.0, 52.0, 0.0)
        if kind == "circle":
            return (40.0, 40.0, 9.0, 0.0, 0.0)
        text = max(self.layout(self.title, self.title_font).get_pixel_size()[0],
                   self.layout(self.detail, self.detail_font).get_pixel_size()[0])
        return (float(43 + text + 16), 44.0, 11.0, 0.0, 1.0)

    def appearance(self, state):
        border, icon, icon_alpha = APPEARANCE[state]
        return (*border, *icon, icon_alpha)

    def morph(self, kind, now=None):
        now = now or clock()
        target = self.shape(kind)
        current = self.geometry.value(now)
        self.kind = kind
        self.geometry.to(target, now, 0.3, GROW)
        self.area.set_size_request(math.ceil(max(current[0], target[0])) + 2 * INSET, HEIGHT)

    def later(self, milliseconds, callback):
        generation = self.generation

        def run():
            if generation == self.generation:
                callback()
            return False

        GLib.timeout_add(milliseconds, run)

    def tick(self, area, frame_clock):
        now = clock()
        if self.state == "recording" and now - self.sampled >= SAMPLE_SECONDS:
            # PillWaveform: newest level enters on the right every 80ms.
            self.sampled = now
            self.levels = self.levels[1:] + [self.volume]
            self.bars_from = self.bars_to
            self.bars_to = [BAR_MIN + level ** 0.75 * (BAR_MAX - BAR_MIN) for level in self.levels]
        if not self.geometry.busy(now):
            width = math.ceil(self.geometry.end[0]) + 2 * INSET
            if area.get_allocated_width() > width:
                area.set_size_request(width, HEIGHT)
                self.window.resize(1, 1)
        if not self.shown and not self.alpha.busy(now):
            self.window.hide()
            return True
        tweens = (self.geometry, self.colors, self.wave, self.glow, self.hover, self.alpha)
        if (self.state == "recording" or self.state in PULSE_STATES or self.glow.end[0] > 0
                or any(tween.busy(now) for tween in tweens)):
            area.queue_draw()
        return True

    def draw(self, area, context):
        now = clock()
        width, height = area.get_allocated_width(), area.get_allocated_height()
        pill_w, pill_h, icon_inset, wave_w, text_alpha = self.geometry.value(now)
        x, y = (width - pill_w) / 2, (height - pill_h) / 2
        region = cairo.RectangleInt(int(x), int(y), math.ceil(pill_w), math.ceil(pill_h))
        if self.region != (region.x, region.y, region.width, region.height):
            self.region = (region.x, region.y, region.width, region.height)
            self.window.input_shape_combine_region(cairo.Region(region))
        context.push_group()
        self.draw_glow(context, x, y, pill_w, pill_h, now)
        for spread, alpha in ((3, 0.05), (2, 0.07), (1, 0.08)):  # shadow-card
            capsule(context, x, y + 1, pill_w, pill_h, spread)
            context.set_source_rgba(0, 0, 0, alpha)
            context.fill()
        colors = self.colors.value(now)
        hover = self.hover.value(now)[0] if self.state in TEXT_STATES else 0.0
        background = tuple(a + (b - a) * hover for a, b in zip(SURFACE_1, SURFACE_3))
        border = tuple(a + (b - a) * hover for a, b in zip(colors[:3], BORDER_HOVER))
        capsule(context, x, y, pill_w, pill_h)
        context.set_source_rgb(*background)
        context.fill_preserve()
        if hover:  # bg-gradient-to-br from-foreground/10 at opacity .72
            gradient = cairo.LinearGradient(x, y, x + pill_w, y + pill_h)
            gradient.add_color_stop_rgba(0, *FOREGROUND, 0.1 * 0.72 * hover)
            gradient.add_color_stop_rgba(1, *FOREGROUND, 0)
            context.set_source(gradient)
            context.fill_preserve()
        context.save()
        context.clip_preserve()
        context.set_source_rgba(1, 1, 1, 0.03)  # inset top highlight
        capsule(context, x, y + 1, pill_w, pill_h)
        context.set_line_width(1)
        context.stroke()
        context.restore()
        capsule(context, x + 0.5, y + 0.5, pill_w - 1, pill_h - 1)
        context.set_source_rgb(*border)
        context.set_line_width(1)
        context.stroke()
        # Content is centred like the flex row: icon, 6px gap and waveform grow together.
        gap = 6 * wave_w / 52
        padding_right = 6 * wave_w / 52
        content = 22 + gap + wave_w
        if text_alpha < 1:
            icon_x = x + 1 + (pill_w - 2 - padding_right - content) / 2
            icon_x += (x + icon_inset - icon_x) * text_alpha
        else:
            icon_x = x + icon_inset
        icon_alpha = colors[6] * (pulse_opacity(now) if self.state in PULSE_STATES and animations() else 1)
        self.draw_icon(context, icon_x + 11, height / 2, colors[3:6], icon_alpha)
        wave_alpha = self.wave.value(now)[0]
        if wave_w > 0.5 and wave_alpha > 0:
            self.draw_wave(context, icon_x + 22 + gap, height / 2, wave_w, wave_alpha, now)
        if text_alpha > 0:
            self.draw_text(context, x, y, pill_w, pill_h, icon_x + 32, height / 2, text_alpha)
        context.pop_group_to_source()
        context.paint_with_alpha(self.alpha.value(now)[0])

    @staticmethod
    def draw_icon(context, cx, cy, color, alpha):
        # OpenWhispr's voice identity: a listening ring holding three bars.
        context.set_source_rgba(*color, alpha)
        context.set_line_cap(cairo.LINE_CAP_ROUND)
        context.set_line_width(1.75)
        context.new_sub_path()
        context.arc(cx, cy, 9.5, 0, 2 * math.pi)
        context.stroke()
        for offset, length in ((-3.5, 5), (0, 9), (3.5, 6)):
            context.move_to(cx + offset, cy - length / 2)
            context.line_to(cx + offset, cy + length / 2)
        context.stroke()

    def draw_wave(self, context, left, cy, width, alpha, now):
        context.save()
        context.rectangle(left, cy - 16, width, 32)
        context.clip()
        # Bars transition height over 75ms (duration-75 ease-out) after each 80ms sample.
        k = EASE_OUT(min(1.0, (now - self.sampled) / 0.075)) if animations() else 1.0
        first = left + width / 2 - 26 + 1
        context.set_source_rgba(*FOREGROUND, alpha)
        context.set_line_cap(cairo.LINE_CAP_ROUND)
        context.set_line_width(2)
        for index, (start, end) in enumerate(zip(self.bars_from, self.bars_to)):
            length = start + (end - start) * k - 2
            context.move_to(first + index * 5, cy - length / 2)
            context.line_to(first + index * 5, cy + length / 2)
        context.stroke()
        context.restore()

    def draw_text(self, context, x, y, width, height, left, cy, alpha):
        context.save()
        capsule(context, x, y, width, height)
        context.clip()
        title = self.layout(self.title, self.title_font)
        detail = self.layout(self.detail, self.detail_font)
        title_h, detail_h = title.get_pixel_size()[1], detail.get_pixel_size()[1]
        top = cy - (title_h + 1 + detail_h) / 2
        context.set_source_rgba(*FOREGROUND, alpha)
        context.move_to(left, top)
        PangoCairo.show_layout(context, title)
        context.set_source_rgba(*MUTED, alpha)
        context.move_to(left, top + title_h + 1)
        PangoCairo.show_layout(context, detail)
        context.restore()

    def draw_glow(self, context, x, y, width, height, now):
        """The Signal glow: a comet orbiting a breathing halo, lit only while transcribing."""
        strength = self.glow.value(now)[0]
        if strength <= 0:
            return
        moving = animations()
        cx, cy = x + width / 2, y + height / 2
        angle = 360 * ((now % 1.6) / 1.6) if moving else 0
        phase = (now % 1.9) / 1.9
        breath = CSS_EASE_IN_OUT(phase * 2 if phase < 0.5 else (1 - phase) * 2) if moving else 1
        context.push_group()
        # Breathing halo: radial core-to-deep wash, inset -4px, 5px blur, scale .96 to 1.05.
        radius = (min(width, height) / 2 + 4) * (0.96 + 0.09 * breath)
        blurred = radius + 5
        halo = cairo.RadialGradient(cx, cy, 0, cx, cy, blurred)
        halo.add_color_stop_rgba(0.45 * radius / blurred, *SIGNAL_CORE, 0.9)
        halo.add_color_stop_rgba(0.75 * radius / blurred, *SIGNAL_DEEP, 0.7)
        halo.add_color_stop_rgba(1, *SIGNAL_DEEP, 0)
        context.save()
        context.translate(cx, cy)
        context.scale((width / 2 + 4) / (height / 2 + 4), 1)  # closest-side ellipse of the -4px box
        context.translate(-cx, -cy)
        context.set_source(halo)
        context.arc(cx, cy, blurred, 0, 2 * math.pi)
        context.fill()
        context.restore()
        context.pop_group_to_source()
        context.paint_with_alpha(0.8 * (0.55 + 0.45 * breath) * strength)
        # Comet bloom: conic tail, inset -4px with a 4px blur approximated by fading strokes.
        bloom = conic(cx, cy, angle, ((0, (*SIGNAL_CORE, 0)), (0.55, (*SIGNAL_CORE, 0)),
                                       (0.82, (*SIGNAL_CORE, 0.7)), (1, (*SIGNAL_BRIGHT, 1))))
        for offset in range(9):
            context.push_group()
            capsule(context, x, y, width, height, offset)
            context.set_line_width(1.2)
            context.set_source(bloom)
            context.stroke()
            context.pop_group_to_source()
            context.paint_with_alpha(0.95 * strength * (1 if offset <= 3 else max(0.0, (8 - offset) / 5)))
        # Ring: 2.5px gradient band at inset -2px, white-hot head cooling into the deep tail.
        ring = conic(cx, cy, angle, ((0, (*SIGNAL_CORE, 0.25)), (0.45, (*SIGNAL_DEEP, 0.55)),
                                      (0.75, (*SIGNAL_CORE, 1)), (0.94, (*SIGNAL_BRIGHT, 1)), (1, (*SIGNAL_HEAD, 1))))
        context.save()
        context.set_fill_rule(cairo.FILL_RULE_EVEN_ODD)
        capsule(context, x, y, width, height, 2)
        capsule(context, x, y, width, height, -0.5)
        context.clip()
        context.set_source(ring)
        context.paint_with_alpha(strength)
        context.restore()

    def show(self, visible, now=None):
        now = now or clock()
        if visible and not self.shown:
            self.shown = True
            self.window.show_all()
        elif not visible and self.shown:
            self.shown = False
        # Wrapper fades with transition-opacity duration-150 ease-out.
        self.alpha.to((1.0 if visible else 0.0,), now, 0.15, EASE_OUT)
        self.area.queue_draw()

    def update(self, state, message, visible=True, volume=0.0, duration=0.0):
        now = clock()
        changed = state != self.state
        self.state, self.volume = state, volume
        titles = {"starting": "Opening microphone", "recording": "Listening", "loading": "Loading speech model",
                  "transcribing": "Transcribing", "delivering": "Copying transcript", "error": "Hype Flow needs attention",
                  "done": message, "idle": "Hype Flow"}
        if state == "recording":
            seconds = int(duration)
            detail = f"{seconds // 60}:{seconds % 60:02d}   ·   Press your shortcut to finish"
        elif state == "error":
            detail = message
        elif state in ("loading", "transcribing", "delivering"):
            detail = "Working locally"
        elif state == "done":
            detail = "Transcript stays on this device"
        else:
            detail = "Press your shortcut to speak"
        self.title, self.detail = titles[state], detail
        self.area.get_accessible().set_name(self.title)
        self.area.get_accessible().set_description(detail)
        self.window.set_tooltip_text(message if state != "recording" else "Click to finish dictation. Right-click to cancel.")
        if changed:
            self.generation += 1
            self.colors.to(self.appearance(state), now, 0.22, CSS_EASE_OUT)
            self.glow.to((1.0 if state in THINKING_STATES else 0.0,), now, 0.22, CSS_EASE_OUT)
            if state == "recording":
                # Listening entrance: 260ms as the identity, 300ms grow, 100ms hold, then the waveform.
                self.levels = [0.0] * BARS
                self.bars_from = self.bars_to = [float(BAR_MIN)] * BARS
                self.sampled = now
                self.morph("circle", now)
                self.later(260, lambda: self.morph("wave"))
                self.later(660, lambda: self.wave.to((1.0,), clock(), 0.2, EASE_OUT))
            else:
                self.wave.to((0.0,), now, 0.2, EASE_OUT)
                self.morph("circle" if state in CIRCLE_STATES else "text", now)
            if state == "done":
                self.later(1800, lambda: self.show(False))
        elif self.kind == "text":
            self.morph("text", now)  # message text can change within a state
        self.show(visible, now)


def preview(seconds=8):
    overlay = Overlay(lambda _: Gtk.main_quit())
    start = time.monotonic()
    animations = Gtk.Settings.get_default().get_property("gtk-enable-animations")

    def tick():
        elapsed = time.monotonic() - start
        if elapsed >= seconds:
            Gtk.main_quit()
            return False
        # Preview only. Live dictation renders actual measured microphone amplitude.
        volume = 0.35 + 0.3 * math.sin(elapsed * 5) if animations else 0.5
        overlay.update("recording", "Listening", True, volume, elapsed)
        return True

    GLib.timeout_add(50, tick)
    tick()
    Gtk.main()


class Home(Gtk.ApplicationWindow):
    def __init__(self, app, values, download, save, action):
        GLib.set_prgname("dev.hype.Flow")
        super().__init__(application=app, title="Hype Flow")
        self.set_name("hype-settings")
        self.set_default_size(960, 680)
        self.set_size_request(620, 480)
        self.connect("delete-event", self.hide_window)
        self.connect("key-press-event", self.key_pressed)
        self.connect("map", lambda *_: self.reveal(self.stack.get_visible_child()))
        self.previous = None
        self.values = values
        self.rows = []
        self.compact = None
        provider = Gtk.CssProvider()
        provider.load_from_path(str(Path(__file__).with_name("settings.css")))
        Gtk.StyleContext.add_provider_for_screen(self.get_screen(), provider, Gtk.STYLE_PROVIDER_PRIORITY_APPLICATION)
        shell = Gtk.Box(orientation=Gtk.Orientation.HORIZONTAL)
        self.add(shell)
        self.sidebar = Gtk.Box(orientation=Gtk.Orientation.VERTICAL, spacing=14)
        self.sidebar.set_name("settings-sidebar")
        shell.pack_start(self.sidebar, False, False, 0)
        # macOS-style window controls; the empty strip around them drags the window.
        titlebar = Gtk.EventBox()
        titlebar.connect("button-press-event", self.drag_window)
        lights = Gtk.Box(orientation=Gtk.Orientation.HORIZONTAL, spacing=8)
        lights.get_style_context().add_class("traffic-lights")
        for name, glyph, tip, callback in (("close", "×", "Close window (Ctrl+W)", lambda _: self.hide_window()),
                                            ("zoom", "+", "Zoom", lambda _: self.unmaximize() if self.is_maximized() else self.maximize())):
            light = self.button(glyph, callback, "traffic")
            light.get_style_context().add_class(name)
            light.set_tooltip_text(tip)
            light.get_accessible().set_name(tip)
            light.set_valign(Gtk.Align.CENTER)
            light.set_can_focus(False)  # like macOS; Ctrl+W closes from the keyboard
            lights.pack_start(light, False, False, 0)
        titlebar.add(lights)
        self.sidebar.pack_start(titlebar, False, False, 0)
        brand = Gtk.Box(orientation=Gtk.Orientation.HORIZONTAL, spacing=10)
        brand.get_style_context().add_class("brand")
        # Rendered at 2x and shown at 32px so the logo stays sharp on HiDPI screens.
        logo = GdkPixbuf.Pixbuf.new_from_file_at_size(str(Path(__file__).with_name("logo.svg")), 64, 64)
        mark = Gtk.Image.new_from_surface(Gdk.cairo_surface_create_from_pixbuf(logo, 2, None))
        mark.get_accessible().set_name("Hype Flow logo")
        brand.pack_start(mark, False, False, 0)
        self.brand_name = self.text("Hype Flow", "brand-name")
        brand.pack_start(self.brand_name, False, False, 0)
        self.sidebar.pack_start(brand, False, False, 0)
        self.navigation = Gtk.ListBox()
        self.navigation.set_name("settings-navigation")
        self.navigation.set_selection_mode(Gtk.SelectionMode.SINGLE)
        self.sidebar.pack_start(self.navigation, False, False, 0)
        self.navigation_labels = []
        sections = (
            ("general", "General", "preferences-system-symbolic", "Your everyday dictation preferences."),
            ("model", "Speech model", "computer-symbolic", "Accurate speech recognition, entirely on your device."),
            ("vocabulary", "Vocabulary", "accessories-dictionary-symbolic", "Give names and specialist terms a little context."),
            ("cleanup", "Text cleanup", "edit-clear-symbolic", "Correct familiar mistakes and filter words before pasting."),
            ("shortcuts", "Shortcuts", "input-keyboard-symbolic", "Dictate from any app without leaving your keyboard."),
        )
        tiles = {"general": "gray", "model": "blue", "vocabulary": "orange", "cleanup": "green", "shortcuts": "indigo"}
        for name, title, icon, description in sections:
            row = Gtk.ListBoxRow()
            row.section_name = name
            row.set_tooltip_text(title)
            row.get_accessible().set_name(title)
            line = Gtk.Box(orientation=Gtk.Orientation.HORIZONTAL, spacing=10)
            tile = Gtk.Image.new_from_icon_name(icon, Gtk.IconSize.MENU)
            tile.set_pixel_size(14)
            tile.get_style_context().add_class("tile")
            tile.get_style_context().add_class(tiles[name])
            line.pack_start(tile, False, False, 0)
            label = self.text(title)
            line.pack_start(label, False, False, 0)
            self.navigation_labels.append(label)
            row.add(line)
            self.navigation.add(row)
        self.action = action

        content = Gtk.Box(orientation=Gtk.Orientation.VERTICAL)
        content.set_name("settings-content")
        shell.pack_start(content, True, True, 0)
        header = Gtk.Box(orientation=Gtk.Orientation.HORIZONTAL, spacing=12)
        header.get_style_context().add_class("page-header")
        self.page_title = self.text("General", "page-title")
        header.pack_start(self.page_title, True, True, 0)
        indicator = Gtk.Box(orientation=Gtk.Orientation.HORIZONTAL, spacing=7)
        indicator.get_style_context().add_class("status-indicator")
        self.status_dot = self.dot("ok")
        indicator.pack_start(self.status_dot, False, False, 0)
        self.badge = self.text("On-device", "status-badge")
        indicator.pack_start(self.badge, False, False, 0)
        indicator.set_valign(Gtk.Align.CENTER)
        header.pack_end(indicator, False, False, 0)
        content.pack_start(header, False, False, 0)
        self.stack = Gtk.Stack()
        self.stack.set_homogeneous(False)
        self.stack.set_transition_type(Gtk.StackTransitionType.CROSSFADE)
        self.stack.set_transition_duration(160)
        content.pack_start(self.stack, True, True, 0)
        pages = {}
        for name, title, icon, description in sections:
            page = Gtk.Box(orientation=Gtk.Orientation.VERTICAL, spacing=18)
            page.get_style_context().add_class("settings-page")
            page.pack_start(self.text(description, "page-description"), False, False, 0)
            scroll = Gtk.ScrolledWindow()
            scroll.set_policy(Gtk.PolicyType.NEVER, Gtk.PolicyType.AUTOMATIC)
            scroll.add(page)
            self.stack.add_named(scroll, name)
            pages[name] = page
        self.navigation.connect("row-selected", self.select_page)

        general = self.panel(pages["general"], "Dictation")
        self.paste = Gtk.Switch()
        self.paste.set_active(values["paste"])
        self.setting_row(general, "Paste after dictation", "Insert text into the app where you started speaking.", self.paste)
        self.trailing_space = self.switch(values, "trailing_space", False)
        self.setting_row(general, "Add a space after pasted text", "Keeps consecutive dictations from running together.", self.trailing_space)
        self.press_enter = self.switch(values, "press_enter", False)
        self.setting_row(general, "Press Enter after pasting", "Sends the message in chat apps. Skipped in terminals, so dictation never runs a command.", self.press_enter)
        microphone = self.panel(pages["general"], "Microphone")
        self.microphone = Gtk.ComboBoxText()
        self.microphone.append("auto", "System default")
        for name, description in sources():
            self.microphone.append(name, description)
        if not self.microphone.set_active_id(values["input_target"]):
            self.microphone.append(values["input_target"], values["input_target"] + " (unavailable)")
            self.microphone.set_active_id(values["input_target"])
        self.microphone.set_size_request(220, -1)
        self.setting_row(microphone, "Input device", "Used the next time you start dictating.", self.microphone)
        self.keep_mic_ready = self.switch(values, "keep_mic_ready", False)
        self.setting_row(microphone, "Keep microphone ready", f"Stays open {HOLD_SECONDS} seconds after dictating, so the next one starts instantly and keeps the half second before you press. Your microphone indicator stays on meanwhile.", self.keep_mic_ready)
        feedback = self.panel(pages["general"], "Feedback")
        self.sounds = self.switch(values, "sounds", True)
        self.setting_row(feedback, "Play start and stop sounds", "The start sound plays once the microphone is live, so you know when to speak.", self.sounds)
        self.pause_media = self.switch(values, "pause_media", True)
        self.setting_row(feedback, "Pause music while dictating", "Pauses only players that are playing, then resumes them.", self.pause_media)
        transcript = self.panel(pages["general"], "Last transcript")
        copy = self.button("Copy transcript", lambda _: action("copy-last"))
        self.setting_row(transcript, "Recover your last dictation", "Available for this session. Cleared when you quit.", copy)
        original = self.button("Copy original", lambda _: action("copy-raw"))
        self.setting_row(transcript, "Original transcript", "Recognizer output before corrections and word filtering.", original)
        hint = Gtk.Box(orientation=Gtk.Orientation.HORIZONTAL, spacing=8)
        hint.get_style_context().add_class("shortcut-hint")
        hint.pack_start(self.text("Press", "muted"), False, False, 0)
        hint.pack_start(self.keys("Super", "Ctrl", "Space"), False, False, 0)
        hint.pack_start(self.text("to start dictating, and again to finish.", "muted"), False, False, 0)
        pages["general"].pack_start(hint, False, False, 0)

        model = self.panel(pages["model"], "Transcription")
        model_header = Gtk.Box(orientation=Gtk.Orientation.HORIZONTAL, spacing=12)
        model_header.get_style_context().add_class("model-header")
        model_header.pack_start(self.text("Parakeet Unified English", "model-title"), True, True, 0)
        model_header.pack_end(self.text("INT8 · 663 MB", "model-badge"), False, False, 0)
        model.pack_start(model_header, False, False, 0)
        summary = self.text("English recognition with punctuation and capitalization.\n663 MB model files. Audio is processed on this device.", "muted")
        summary.get_style_context().add_class("model-description")
        model.pack_start(summary, False, False, 0)
        model_state = Gtk.Box(orientation=Gtk.Orientation.HORIZONTAL, spacing=8)
        model_state.get_style_context().add_class("model-description")
        self.model_dot = self.dot("busy")
        model_state.pack_start(self.model_dot, False, False, 0)
        self.model_status = self.text("Checking model…", "muted")
        model_state.pack_start(self.model_status, False, False, 0)
        model.pack_start(model_state, False, False, 0)
        self.download_button = self.button(self.download_label(), lambda _: download(), "primary")
        self.download_button.set_halign(Gtk.Align.START)
        self.download_button.set_margin_start(16)
        self.download_button.set_margin_end(16)
        self.download_button.set_margin_bottom(16)
        model.pack_start(self.download_button, False, False, 0)
        processing = self.panel(pages["model"], "Audio processing")
        self.cut_silence = self.switch(values, "cut_silence", True)
        self.setting_row(processing, "Cut silence before recognition", "Removes long pauses with on-device voice detection. Faster, and keeps half a second around speech so words are not clipped.", self.cut_silence)
        self.noise_suppression = self.switch(values, "noise_suppression", False)
        self.setting_row(processing, "Suppress background noise", "Cleans fans, hum and room noise before recognition. Adds about half a second; Parakeet already copes with moderate noise.", self.noise_suppression)
        memory = self.panel(pages["model"], "Memory")
        self.unload_minutes = Gtk.ComboBoxText()
        for minutes in UNLOAD_CHOICES:
            self.unload_minutes.append(str(minutes), "Never" if not minutes else f"After {minutes} minutes")
        self.unload_minutes.set_active_id(str(values.get("unload_minutes", 10)))
        self.unload_minutes.set_size_request(180, -1)
        self.setting_row(memory, "Unload model when idle", "Frees about 1.4 GB. It reloads while you speak, so dictation still starts at once.", self.unload_minutes)

        vocabulary = self.panel(pages["vocabulary"], "Custom vocabulary")
        vocabulary_body = Gtk.Box(orientation=Gtk.Orientation.VERTICAL, spacing=12)
        vocabulary_body.get_style_context().add_class("panel-body")
        vocabulary_body.pack_start(self.text("Names and specialist terms", "row-title"), False, False, 0)
        vocabulary_body.pack_start(self.text("Separate names and phrases with commas. Keep the list focused; at most 100 entries.", "muted"), False, False, 0)
        self.prompt = Gtk.Entry()
        self.prompt.set_text(values["prompt"])
        self.prompt.set_placeholder_text("niri, Hype Flow, project names…")
        self.prompt.get_accessible().set_name("Names and vocabulary")
        vocabulary_body.pack_start(self.prompt, False, False, 0)
        vocabulary.pack_start(vocabulary_body, False, False, 0)
        self.boosting = Gtk.Switch()
        self.boosting.set_active(values.get("vocabulary_boosting", False))
        self.setting_row(vocabulary, "Boost vocabulary during recognition", "Experimental. Parakeet Unified barely responds to boosting, and it slows decoding. Correction rules are more reliable.", self.boosting)
        self.boost_score = Gtk.SpinButton.new_with_range(0.5, 3.0, 0.5)
        self.boost_score.set_value(values.get("boost_score", 1.5))
        self.boost_score.set_digits(1)
        self.setting_row(vocabulary, "Vocabulary strength", "Start low. Strong bias can insert words you did not say.", self.boost_score)
        near = self.panel(pages["vocabulary"], "Near-miss correction")
        self.fuzzy_vocabulary = self.switch(values, "fuzzy_vocabulary", True)
        self.setting_row(near, "Fix near-miss spellings", "Snaps misheard words to your list when they look or sound alike, and joins split words: “Charge B” becomes “ChargeBee”.", self.fuzzy_vocabulary)
        self.fuzzy_tolerance = Gtk.SpinButton.new_with_range(0.05, 0.4, 0.01)
        self.fuzzy_tolerance.set_digits(2)
        self.fuzzy_tolerance.set_value(values.get("fuzzy_tolerance", 0.18))
        self.setting_row(near, "Tolerance", "Higher catches more mishearings but risks replacing real words. 0.18 is a good start.", self.fuzzy_tolerance)

        replacements = self.panel(pages["cleanup"], "Auto-correction")
        self.auto_correct = Gtk.Switch()
        self.auto_correct.set_active(values.get("auto_correct", True))
        self.setting_row(replacements, "Apply saved corrections", "Use your replacement rules and preferred vocabulary capitalization.", self.auto_correct)
        self.corrections_buffer = self.editor(replacements, "One rule per line: heard words => replacement. Example: open ai => OpenAI",
                                               values.get("corrections", ""), "Correction rules")
        filters = self.panel(pages["cleanup"], "Word filters")
        self.filter_words = Gtk.Switch()
        self.filter_words.set_active(values.get("filter_words", True))
        self.setting_row(filters, "Remove listed words and phrases", "Whole words only. URLs, email addresses, quoted text and inline code are preserved.", self.filter_words)
        self.blocked_buffer = self.editor(filters, "Separate words with commas or new lines. Only list words you want removed.",
                                          values.get("blocked_words", DEFAULT_FILTERS), "Words and phrases to remove")
        repetitions = self.panel(pages["cleanup"], "Repetitions")
        self.remove_repetitions = Gtk.Switch()
        self.remove_repetitions.set_active(values.get("remove_repetitions", False))
        self.setting_row(repetitions, "Remove adjacent repeated words", "Optional: “the the” becomes “the”. Leave off to preserve emphasis such as “very very”.", self.remove_repetitions)
        pages["cleanup"].pack_start(self.text("The original transcript stays recoverable for this session with Copy original in General.", "muted"), False, False, 0)
        pages["cleanup"].pack_start(self.text(f"Advanced: an executable you own at {str(hook_path()).replace(str(Path.home()), '~')} receives each transcript on standard input, and what it prints is pasted instead. It has 30 seconds; on failure the cleaned transcript is used.", "muted"), False, False, 0)

        shortcuts = self.panel(pages["shortcuts"], "Keyboard shortcuts")
        self.setting_row(shortcuts, "Start / finish dictation", "Press once to speak, then again to transcribe.", self.keys("Super", "Ctrl", "Space"))
        self.setting_row(shortcuts, "Cancel dictation", "Discard the current recording or transcription.", self.keys("Super", "Ctrl", "Esc"))
        integration = self.panel(pages["shortcuts"], "niri integration")
        self.shortcut_button = self.button("Configure niri", self.install_shortcuts)
        self.shortcut_button.set_tooltip_text("Add dictation bindings and autostart to your niri config. A backup is saved first.")
        self.setting_row(integration, "Available in every app", "Set up global shortcuts and start Hype Flow at login.", self.shortcut_button)
        pages["shortcuts"].pack_start(self.text("The notch shows your microphone level while you speak. Click its capsule to finish, or right-click to cancel.", "muted"), False, False, 0)

        footer = Gtk.Box(orientation=Gtk.Orientation.HORIZONTAL, spacing=20)
        footer.get_style_context().add_class("settings-footer")
        self.status = self.text("Getting ready", "muted")
        self.status.set_selectable(True)
        footer.pack_start(self.status, True, True, 0)
        self.save_button = self.button("Save changes", lambda _: self.saved(save(self.preferences())), "primary")
        footer.pack_end(self.save_button, False, False, 0)
        content.pack_end(footer, False, False, 0)
        shell.show_all()
        # Save stays dimmed until something changes, like a document's edited state.
        self.save_button.set_sensitive(False)
        dirty = lambda *_: self.save_button.set_sensitive(True)
        for switch in (self.paste, self.boosting, self.auto_correct, self.filter_words, self.remove_repetitions,
                       self.trailing_space, self.press_enter, self.keep_mic_ready, self.sounds, self.pause_media,
                       self.fuzzy_vocabulary, self.cut_silence, self.noise_suppression):
            switch.connect("notify::active", dirty)
        self.prompt.connect("changed", dirty)
        for spin in (self.boost_score, self.fuzzy_tolerance):
            spin.connect("value-changed", dirty)
        for combo in (self.microphone, self.unload_minutes):
            combo.connect("changed", dirty)
        for buffer in (self.corrections_buffer, self.blocked_buffer):
            buffer.connect("changed", dirty)
        self.download_button.set_no_show_all(True)
        self.navigation.select_row(self.navigation.get_row_at_index(0))
        self.connect("size-allocate", self.resize_layout)

    @staticmethod
    def text(value, style=None):
        label = Gtk.Label(label=value, xalign=0)
        label.set_line_wrap(True)
        label.set_line_wrap_mode(Pango.WrapMode.WORD_CHAR)
        label.set_max_width_chars(60)
        if style:
            label.get_style_context().add_class(style)
        return label

    def keys(self, *names):
        box = Gtk.Box(orientation=Gtk.Orientation.HORIZONTAL, spacing=4)
        box.set_valign(Gtk.Align.CENTER)
        for name in names:
            key = self.text(name, "keycap")
            key.set_line_wrap(False)
            key.set_xalign(0.5)
            box.pack_start(key, False, False, 0)
        return box

    @staticmethod
    def switch(values, key, default):
        switch = Gtk.Switch()
        switch.set_active(values.get(key, default))
        return switch

    @staticmethod
    def dot(tone):
        dot = Gtk.Box()
        dot.set_valign(Gtk.Align.CENTER)
        dot.get_style_context().add_class("status-dot")
        dot.get_style_context().add_class(tone)
        return dot

    @staticmethod
    def tone(dot, tone):
        context = dot.get_style_context()
        for name in ("ok", "busy", "error"):
            context.remove_class(name)
        context.add_class(tone)

    @staticmethod
    def button(label, callback, style=None):
        button = Gtk.Button(label=label)
        button.connect("clicked", callback)
        if style:
            button.get_style_context().add_class(style)
        return button

    def editor(self, panel, description, value, name):
        body = Gtk.Box(orientation=Gtk.Orientation.VERTICAL, spacing=10)
        body.get_style_context().add_class("panel-body")
        body.pack_start(self.text(description, "muted"), False, False, 0)
        view = Gtk.TextView()
        view.set_wrap_mode(Gtk.WrapMode.WORD_CHAR)
        view.set_left_margin(10)
        view.set_right_margin(10)
        view.set_top_margin(8)
        view.set_bottom_margin(8)
        view.get_accessible().set_name(name)
        buffer = view.get_buffer()
        buffer.set_text(value)
        scroll = Gtk.ScrolledWindow()
        scroll.set_policy(Gtk.PolicyType.NEVER, Gtk.PolicyType.AUTOMATIC)
        scroll.set_min_content_height(90)
        scroll.get_style_context().add_class("text-editor")
        scroll.add(view)
        body.pack_start(scroll, False, False, 0)
        panel.pack_start(body, False, False, 0)
        return buffer

    def preferences(self):
        return {"prompt": self.prompt.get_text(), "paste": self.paste.get_active(),
                "vocabulary_boosting": self.boosting.get_active(), "boost_score": self.boost_score.get_value(),
                "auto_correct": self.auto_correct.get_active(),
                "corrections": self.corrections_buffer.get_text(*self.corrections_buffer.get_bounds(), True),
                "filter_words": self.filter_words.get_active(),
                "blocked_words": self.blocked_buffer.get_text(*self.blocked_buffer.get_bounds(), True),
                "remove_repetitions": self.remove_repetitions.get_active(),
                "trailing_space": self.trailing_space.get_active(), "press_enter": self.press_enter.get_active(),
                "input_target": self.microphone.get_active_id() or "auto",
                "keep_mic_ready": self.keep_mic_ready.get_active(), "sounds": self.sounds.get_active(),
                "pause_media": self.pause_media.get_active(), "fuzzy_vocabulary": self.fuzzy_vocabulary.get_active(),
                "fuzzy_tolerance": round(self.fuzzy_tolerance.get_value(), 2),
                "unload_minutes": int(self.unload_minutes.get_active_id() or 0),
                "cut_silence": self.cut_silence.get_active(), "noise_suppression": self.noise_suppression.get_active()}

    def download_label(self):
        model = Path(self.values["model"])
        present = all((model / name).is_file() for name in MODEL_FILES)
        return "Update speech support" if present else "Download model · 663 MB"

    def panel(self, page, title):
        section = Gtk.Box(orientation=Gtk.Orientation.VERTICAL, spacing=7)
        section.pack_start(self.text(title, "section-title"), False, False, 0)
        panel = Gtk.Box(orientation=Gtk.Orientation.VERTICAL)
        panel.get_style_context().add_class("settings-panel")
        section.pack_start(panel, False, False, 0)
        page.pack_start(section, False, False, 0)
        return panel

    def setting_row(self, panel, title, description, control):
        row = Gtk.Box(orientation=Gtk.Orientation.HORIZONTAL, spacing=20)
        row.get_style_context().add_class("setting-row")
        labels = Gtk.Box(orientation=Gtk.Orientation.VERTICAL, spacing=3)
        labels.pack_start(self.text(title, "row-title"), False, False, 0)
        labels.pack_start(self.text(description, "muted"), False, False, 0)
        row.pack_start(labels, True, True, 0)
        control.set_valign(Gtk.Align.CENTER)
        control.set_halign(Gtk.Align.END)
        control.get_accessible().set_name(title)
        row.pack_end(control, False, False, 0)
        panel.pack_start(row, False, False, 0)
        self.rows.append((row, control))

    def select_page(self, _, row):
        if row is not None:
            self.stack.set_visible_child_name(row.section_name)
            self.page_title.set_text(row.get_tooltip_text())
            self.reveal(self.stack.get_visible_child())

    def reveal(self, page):
        """Fade the page in while it settles 8px upward (220ms, the pill's grow curve)."""
        tween, start = Tween((0.0,)), clock()
        tween.to((1.0,), start, 0.22, GROW)

        def step(widget, _clock):
            progress = tween.value(clock())[0]
            widget.set_opacity(progress)
            widget.set_margin_top(round(8 * (1 - progress)))
            return tween.busy(clock())

        step(page, None)
        page.add_tick_callback(step)

    def saved(self, ok):
        if not ok:
            return
        self.save_button.set_sensitive(False)
        self.save_button.set_label("✓  Saved")
        self.save_button.get_style_context().add_class("saved")

        def restore():
            self.save_button.set_label("Save changes")
            self.save_button.get_style_context().remove_class("saved")
            return False

        GLib.timeout_add(1600, restore)

    def key_pressed(self, _, event):
        if event.state & Gdk.ModifierType.CONTROL_MASK:
            key = Gdk.keyval_to_lower(event.keyval)
            if key == Gdk.KEY_w:
                return self.hide_window()
            if key == Gdk.KEY_q:
                self.action("quit")
                return True
        return False

    def drag_window(self, _, event):
        if event.button == 1:
            self.begin_move_drag(event.button, int(event.x_root), int(event.y_root), event.time)
        return False

    def resize_layout(self, _, allocation):
        compact = allocation.width < 760
        stacked = compact
        if self.compact == (compact, stacked):
            return
        self.compact = (compact, stacked)
        self.sidebar.set_size_request(64 if compact else 200, -1)
        for label in [self.brand_name, *self.navigation_labels]:
            label.set_visible(not compact)
        for row, control in self.rows:
            row.set_orientation(Gtk.Orientation.VERTICAL if stacked else Gtk.Orientation.HORIZONTAL)
            control.set_halign(Gtk.Align.START if stacked else Gtk.Align.END)

    def hide_window(self, *_):
        self.hide()
        return True

    def feedback(self, message):
        self.status.set_text(message)
        self.status.set_tooltip_text(message)
        return False

    def set_downloading(self, active):
        self.download_button.set_sensitive(not active)
        self.download_button.set_label("Installing…" if active else self.download_label())

    def update(self, state, message, assets_ready):
        if self.previous != (state, message, assets_ready):
            self.previous = (state, message, assets_ready)
            self.feedback(message)
            self.download_button.set_visible(not assets_ready)
            missing = "Model installed; update speech support to enable vocabulary." if self.download_label() == "Update speech support" else "Download the model to start dictating."
            self.model_status.set_text("Installed on this device" if assets_ready else missing)
            self.tone(self.model_dot, "ok" if assets_ready else "busy")
            self.tone(self.status_dot, "error" if state == "error" else
                      "ok" if assets_ready and state in ("idle", "done") else "busy")
            self.badge.set_text({"loading": "Loading", "recording": "Listening", "starting": "Starting",
                                 "transcribing": "Transcribing", "delivering": "Copying", "error": "Needs attention"}.get(state, "On-device"))
            context = self.status.get_style_context()
            if state == "error":
                context.add_class("error")
            else:
                context.remove_class("error")

    def install_shortcuts(self, _):
        from .shortcuts import install
        try:
            self.feedback(install())
        except (OSError, RuntimeError) as error:
            self.feedback(str(error))
