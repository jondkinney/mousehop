//! Click-to-record chord-capture widget for the daemon's
//! release-bind, plus the Pango markup that renders a stored chord
//! as ⌃ ⇧ ⌥ + Omarchy-logo glyphs (mirroring hyprcorrect/vernier's
//! egui chord chip in GTK).
//!
//! The widget wraps a `GtkButton` that displays the chord and, when
//! clicked, flips the preferences window into capture mode. While
//! capturing, an `EventControllerKey` installed on the window
//! intercepts keyboard input at the capture phase so the next
//! modifier + trigger-key combination becomes the new chord. Escape
//! aborts; clicking the button a second time also aborts.

use std::cell::{Cell, RefCell};
use std::rc::Rc;

use gtk::glib::Propagation;
use gtk::{
    EventControllerKey, PropagationPhase, gdk,
    glib::{self, signal::SignalHandlerId},
    prelude::*,
};

use input_event::scancode::{
    self,
    Linux::{KeyLeftAlt, KeyLeftCtrl, KeyLeftMeta, KeyLeftShift},
};

use crate::glyph_font::{OMARCHY_LOGO, omarchy_available};

/// Canonical default chord used when no override is set —
/// Ctrl+Shift+Alt+Meta. Mirrors `DEFAULT_RELEASE_KEYS` in
/// `mousehop/src/config.rs`; the daemon's `Config::release_bind`
/// returns the same set when the TOML key is absent, but the GUI
/// also wants to display it before the first `FrontendEvent::
/// ReleaseBind` arrives.
pub const DEFAULT_RELEASE_CHORD: [scancode::Linux; 4] =
    [KeyLeftCtrl, KeyLeftShift, KeyLeftMeta, KeyLeftAlt];

/// Chip-glyph point sizes (in Pango units; 1024 = 1pt). Mirrors
/// hyprcorrect/vernier's `CHIP_LETTER_PT = 17.0` + Omarchy
/// `FontTweak {scale: 0.75, y_offset_factor: 0.09}` exactly:
/// `OMARCHY_PT_K` is `CHIP_PT_K * 0.75` and `OMARCHY_RISE` is
/// `-CHIP_PT_K * 0.09` (Pango rise is upward-positive; egui's
/// `y_offset_factor` is downward-positive, so the sign flips).
/// 17pt matches the on-screen height of hyprcorrect's chip on the
/// same display.
const CHIP_PT_K: u32 = 14_000;
/// Tuned a hair below hyprcorrect's 0.75 ratio (`CHIP_PT_K * 67 /
/// 100`) so the rendered Omarchy logo's visual height matches the
/// surrounding letter cap-height. Pango's font metrics differ from
/// egui's enough that the literal 0.75 scale leaves the logo
/// slightly taller than the M / F / R / S that follow it.
const OMARCHY_PT_K: u32 = (CHIP_PT_K * 72) / 100; // 10_080 = 14 × 0.72

/// Vertical baseline offset for the Omarchy logo within the chord
/// chip line. Hyprcorrect computes the equivalent number via egui's
/// `FontTweak { y_offset_factor: 0.09 }`, which shifts the glyph
/// DOWN by 9% of the base font size — but egui's FontTweak applies
/// relative to the font's ascender, whereas Pango's `rise` applies
/// relative to the text baseline. Pango also respects the Omarchy
/// face's own `OS/2.sTypoDescender` (which puts a chunk of the
/// glyph below the baseline natively), so the equivalent Pango rise
/// is much smaller than the literal `-0.09 × size`. Tuned visually
/// against hyprcorrect's chip: the logo's bottom edge should sit at
/// the letter baseline (where the bottom of M / F sits), not below
/// it.
const OMARCHY_RISE: i32 = -250;

/// Family name of the bundled AdwaitaSans-Regular face (registered
/// by [`crate::glyph_font::install`]). Match hyprcorrect/vernier's
/// chord chip — they use Adwaita Sans for ⌃ ⇧ ⌥ and the trigger
/// letter so the chip reads identically across the three apps,
/// regardless of what the host's default UI font happens to be
/// (Cascadia Code on Omarchy, Cantarell on stock GNOME, etc.).
const CHIP_FAMILY: &str = "Adwaita Sans";

/// Inter-segment gap. Hyprcorrect's chip uses `CHIP_GAP: f32 = 6.0`
/// pixels between glyphs (see vernier-ui/src/prefs.rs); a U+2004
/// THREE-PER-EM SPACE lays out at 1/3 em (~4.7px at 14pt) which
/// reads as the tightly-but-evenly-distributed spacing hyprcorrect
/// renders. EN SPACE (1/2 em ~7px) is slightly too airy; plain
/// ASCII space collapses to the font's narrower word-space and
/// bunches the glyphs together.
const CHIP_SEPARATOR: &str = "\u{2004}";

/// Build Pango markup that renders `keys` as a row of modifier
/// glyphs followed (where present) by a non-modifier trigger key.
/// Super is drawn as the Omarchy logo when the bundled face was
/// registered, otherwise as ⌘ so the chip still reads on systems
/// where the font lookup failed.
pub fn chord_markup(keys: &[scancode::Linux]) -> String {
    if keys.is_empty() {
        return escape("Click to set").to_string();
    }

    let (mods, trigger) = split_chord(keys);
    let mut out = String::new();

    let mut first = true;
    for m in mods {
        if !first {
            out.push_str(CHIP_SEPARATOR);
        }
        first = false;
        out.push_str(&modifier_markup(m));
    }
    if let Some(t) = trigger {
        if !first {
            out.push_str(CHIP_SEPARATOR);
        }
        out.push_str(&format!(
            "<span face=\"{CHIP_FAMILY}\" size=\"{CHIP_PT_K}\" weight=\"normal\">{}</span>",
            escape(&trigger_label(t))
        ));
    }
    out
}

/// Build the Pango markup shown while we're waiting for the user to
/// press a chord.
pub fn capturing_markup() -> &'static str {
    "<span foreground=\"#9fbef0\" style=\"italic\">Press a shortcut…</span>"
}

/// Wire up a chord-capture button: clicking flips into capture mode,
/// the next chord (or Escape) ends it, and `on_apply` fires with the
/// captured `Vec<scancode::Linux>` (modifiers in canonical order
/// followed by the trigger key). The current chord is rendered on
/// the button label via [`chord_markup`].
///
/// Returns an opaque handle whose drop tears down the key
/// controller; callers usually keep it for the lifetime of the
/// surrounding row.
pub fn bind_button(
    window: &gtk::Window,
    label: &gtk::Label,
    button: &gtk::Button,
    on_apply: impl Fn(Vec<scancode::Linux>) + 'static,
) -> ShortcutCaptureHandle {
    let state = Rc::new(CaptureState {
        capturing: Cell::new(false),
        pressed_mods: RefCell::new(Vec::new()),
        last_chord: RefCell::new(DEFAULT_RELEASE_CHORD.to_vec()),
        on_apply: Box::new(on_apply),
        label: label.clone(),
        button: button.clone(),
    });

    // Render the initial label.
    state.refresh_label();

    let key_controller = EventControllerKey::new();
    key_controller.set_propagation_phase(PropagationPhase::Capture);

    let press_state = Rc::clone(&state);
    let press_id = key_controller.connect_key_pressed(move |_, keyval, _kc, _mods| {
        if !press_state.capturing.get() {
            return Propagation::Proceed;
        }
        if keyval == gdk::Key::Escape {
            press_state.cancel_capture();
            return Propagation::Stop;
        }
        let Some(scancode) = keyval_to_scancode(keyval) else {
            return Propagation::Stop;
        };
        if is_modifier(scancode) {
            // Accumulate held modifiers; the trigger key is what
            // ends the capture below.
            let mut held = press_state.pressed_mods.borrow_mut();
            if !held.contains(&scancode) {
                held.push(scancode);
            }
            return Propagation::Stop;
        }
        // Non-modifier press = trigger key. Snapshot modifiers + this
        // key, exit capture, emit.
        let held = press_state.pressed_mods.borrow().clone();
        if held.is_empty() {
            // A standalone non-modifier chord (e.g. just F12) is
            // legal — the daemon will simply release on that key. But
            // most users want a modifier + key, so we still accept
            // it. Ordering: modifiers (already canonical from
            // canonicalize_mods()) then the trigger.
        }
        let mut chord = canonicalize_mods(held);
        chord.push(scancode);
        press_state.finish_capture(chord);
        Propagation::Stop
    });

    let release_state = Rc::clone(&state);
    let _release_id = key_controller.connect_key_released(move |_, keyval, _kc, _mods| {
        if !release_state.capturing.get() {
            return;
        }
        if let Some(scancode) = keyval_to_scancode(keyval) {
            if is_modifier(scancode) {
                release_state
                    .pressed_mods
                    .borrow_mut()
                    .retain(|k| *k != scancode);
            }
        }
    });

    window.add_controller(key_controller.clone());

    // Clicking the button toggles capture mode.
    let click_state = Rc::clone(&state);
    let click_id = button.connect_clicked(move |_| {
        if click_state.capturing.get() {
            click_state.cancel_capture();
        } else {
            click_state.begin_capture();
        }
    });

    ShortcutCaptureHandle {
        state,
        _press_id: press_id,
        _click_id: click_id,
        _key_controller: key_controller,
        window: window.downgrade(),
    }
}

/// Lifetime handle returned by [`bind_button`]. Holds the wired-up
/// `EventControllerKey` so dropping the handle removes it from the
/// window.
pub struct ShortcutCaptureHandle {
    state: Rc<CaptureState>,
    _press_id: SignalHandlerId,
    _click_id: SignalHandlerId,
    _key_controller: EventControllerKey,
    window: glib::WeakRef<gtk::Window>,
}

impl ShortcutCaptureHandle {
    /// Update the displayed chord (called when the daemon pushes a
    /// new `FrontendEvent::ReleaseBind`).
    pub fn set_chord(&self, chord: Vec<scancode::Linux>) {
        *self.state.last_chord.borrow_mut() = chord;
        if !self.state.capturing.get() {
            self.state.refresh_label();
        }
    }
}

impl Drop for ShortcutCaptureHandle {
    fn drop(&mut self) {
        if let Some(w) = self.window.upgrade() {
            w.remove_controller(&self._key_controller);
        }
    }
}

struct CaptureState {
    capturing: Cell<bool>,
    pressed_mods: RefCell<Vec<scancode::Linux>>,
    last_chord: RefCell<Vec<scancode::Linux>>,
    on_apply: Box<dyn Fn(Vec<scancode::Linux>)>,
    label: gtk::Label,
    button: gtk::Button,
}

impl CaptureState {
    fn begin_capture(self: &Rc<Self>) {
        self.capturing.set(true);
        self.pressed_mods.borrow_mut().clear();
        self.label.set_markup(capturing_markup());
        self.button.add_css_class("suggested-action");
        // Take keyboard focus so platforms that route key events to
        // focused widgets still funnel them to the window-level
        // controller via the capture-phase grab.
        let _ = self.button.grab_focus();
    }

    fn cancel_capture(self: &Rc<Self>) {
        self.capturing.set(false);
        self.pressed_mods.borrow_mut().clear();
        self.button.remove_css_class("suggested-action");
        self.refresh_label();
    }

    fn finish_capture(self: &Rc<Self>, chord: Vec<scancode::Linux>) {
        self.capturing.set(false);
        self.pressed_mods.borrow_mut().clear();
        self.button.remove_css_class("suggested-action");
        *self.last_chord.borrow_mut() = chord.clone();
        self.refresh_label();
        (self.on_apply)(chord);
    }

    fn refresh_label(&self) {
        let chord = self.last_chord.borrow();
        self.label.set_markup(&chord_markup(&chord));
    }
}

fn split_chord(keys: &[scancode::Linux]) -> (Vec<scancode::Linux>, Option<scancode::Linux>) {
    let mut mods: Vec<scancode::Linux> = keys.iter().copied().filter(|k| is_modifier(*k)).collect();
    mods = canonicalize_mods(mods);
    let trigger = keys.iter().copied().find(|k| !is_modifier(*k));
    (mods, trigger)
}

fn canonicalize_mods(mut mods: Vec<scancode::Linux>) -> Vec<scancode::Linux> {
    // Stable display order: Ctrl, Shift, Alt, Meta/Super. Match the
    // egui chips so the keyboard's leftmost modifier reads leftmost.
    fn rank(k: scancode::Linux) -> u8 {
        match k {
            scancode::Linux::KeyLeftCtrl | scancode::Linux::KeyRightCtrl => 0,
            scancode::Linux::KeyLeftShift | scancode::Linux::KeyRightShift => 1,
            scancode::Linux::KeyLeftAlt | scancode::Linux::KeyRightalt => 2,
            scancode::Linux::KeyLeftMeta | scancode::Linux::KeyRightmeta => 3,
            _ => 99,
        }
    }
    mods.sort_by_key(|k| rank(*k));
    mods.dedup_by_key(|k| rank(*k));
    mods
}

fn modifier_markup(k: scancode::Linux) -> String {
    let glyph = match k {
        scancode::Linux::KeyLeftCtrl | scancode::Linux::KeyRightCtrl => "⌃",
        scancode::Linux::KeyLeftShift | scancode::Linux::KeyRightShift => "⇧",
        scancode::Linux::KeyLeftAlt | scancode::Linux::KeyRightalt => "⌥",
        scancode::Linux::KeyLeftMeta | scancode::Linux::KeyRightmeta => return super_markup(),
        _ => return escape(&trigger_label(k)),
    };
    format!("<span face=\"{CHIP_FAMILY}\" size=\"{CHIP_PT_K}\" weight=\"normal\">{glyph}</span>")
}

fn super_markup() -> String {
    if omarchy_available() {
        // Match vernier's chord chip exactly: omarchy face rendered at
        // 0.75× the surrounding modifier-glyph size (the logo fills
        // its full em square so without scaling it dwarfs the bold
        // modifier glyphs), with the baseline shifted down by ~9% of
        // the chip's base size to drop the logo's visual center onto
        // the letters' x-height. Vernier expresses this as `FontTweak
        // {scale: 0.75, y_offset_factor: 0.09}` baked into the font
        // data; Pango doesn't have a per-face tweak so the same
        // numbers are applied here via inline span attrs.
        format!(
            "<span face=\"omarchy\" size=\"{OMARCHY_PT_K}\" rise=\"{OMARCHY_RISE}\">{}</span>",
            OMARCHY_LOGO
        )
    } else {
        // No Omarchy face — fall back to the Mac-style Command glyph
        // so the chip still reads as a Super-modifier shortcut.
        format!("<span face=\"{CHIP_FAMILY}\" size=\"{CHIP_PT_K}\">⌘</span>")
    }
}

fn trigger_label(k: scancode::Linux) -> String {
    use scancode::Linux::*;
    match k {
        KeyA => "A".into(),
        KeyB => "B".into(),
        KeyC => "C".into(),
        KeyD => "D".into(),
        KeyE => "E".into(),
        KeyF => "F".into(),
        KeyG => "G".into(),
        KeyH => "H".into(),
        KeyI => "I".into(),
        KeyJ => "J".into(),
        KeyK => "K".into(),
        KeyL => "L".into(),
        KeyM => "M".into(),
        KeyN => "N".into(),
        KeyO => "O".into(),
        KeyP => "P".into(),
        KeyQ => "Q".into(),
        KeyR => "R".into(),
        KeyS => "S".into(),
        KeyT => "T".into(),
        KeyU => "U".into(),
        KeyV => "V".into(),
        KeyW => "W".into(),
        KeyX => "X".into(),
        KeyY => "Y".into(),
        KeyZ => "Z".into(),
        Key0 => "0".into(),
        Key1 => "1".into(),
        Key2 => "2".into(),
        Key3 => "3".into(),
        Key4 => "4".into(),
        Key5 => "5".into(),
        Key6 => "6".into(),
        Key7 => "7".into(),
        Key8 => "8".into(),
        Key9 => "9".into(),
        KeyF1 => "F1".into(),
        KeyF2 => "F2".into(),
        KeyF3 => "F3".into(),
        KeyF4 => "F4".into(),
        KeyF5 => "F5".into(),
        KeyF6 => "F6".into(),
        KeyF7 => "F7".into(),
        KeyF8 => "F8".into(),
        KeyF9 => "F9".into(),
        KeyF10 => "F10".into(),
        KeyF11 => "F11".into(),
        KeyF12 => "F12".into(),
        KeyEnter => "↵".into(),
        KeyEsc => "⎋".into(),
        KeyTab => "⇥".into(),
        KeySpace => "␣".into(),
        KeyBackspace => "⌫".into(),
        KeyDelete => "⌦".into(),
        KeyUp => "↑".into(),
        KeyDown => "↓".into(),
        KeyLeft => "←".into(),
        KeyRight => "→".into(),
        KeyHome => "Home".into(),
        KeyEnd => "End".into(),
        KeyPageup => "PgUp".into(),
        KeyPagedown => "PgDn".into(),
        KeyMinus => "-".into(),
        KeyEqual => "=".into(),
        KeyComma => ",".into(),
        KeyDot => ".".into(),
        KeySlash => "/".into(),
        KeyBackslash => "\\".into(),
        KeyApostrophe => "'".into(),
        KeyGrave => "`".into(),
        KeySemicolon => ";".into(),
        KeyLeftbrace => "[".into(),
        KeyRightbrace => "]".into(),
        other => format!("{other:?}"),
    }
}

fn keyval_to_scancode(keyval: gdk::Key) -> Option<scancode::Linux> {
    use scancode::Linux::*;
    Some(match keyval {
        // -- Modifiers ---------------------------------------------------
        gdk::Key::Control_L => KeyLeftCtrl,
        gdk::Key::Control_R => KeyRightCtrl,
        gdk::Key::Shift_L => KeyLeftShift,
        gdk::Key::Shift_R => KeyRightShift,
        gdk::Key::Alt_L | gdk::Key::Meta_L => KeyLeftAlt,
        gdk::Key::Alt_R | gdk::Key::Meta_R | gdk::Key::ISO_Level3_Shift => KeyRightalt,
        gdk::Key::Super_L | gdk::Key::Hyper_L => KeyLeftMeta,
        gdk::Key::Super_R | gdk::Key::Hyper_R => KeyRightmeta,

        // -- Letters (case-insensitive) ----------------------------------
        gdk::Key::a | gdk::Key::A => KeyA,
        gdk::Key::b | gdk::Key::B => KeyB,
        gdk::Key::c | gdk::Key::C => KeyC,
        gdk::Key::d | gdk::Key::D => KeyD,
        gdk::Key::e | gdk::Key::E => KeyE,
        gdk::Key::f | gdk::Key::F => KeyF,
        gdk::Key::g | gdk::Key::G => KeyG,
        gdk::Key::h | gdk::Key::H => KeyH,
        gdk::Key::i | gdk::Key::I => KeyI,
        gdk::Key::j | gdk::Key::J => KeyJ,
        gdk::Key::k | gdk::Key::K => KeyK,
        gdk::Key::l | gdk::Key::L => KeyL,
        gdk::Key::m | gdk::Key::M => KeyM,
        gdk::Key::n | gdk::Key::N => KeyN,
        gdk::Key::o | gdk::Key::O => KeyO,
        gdk::Key::p | gdk::Key::P => KeyP,
        gdk::Key::q | gdk::Key::Q => KeyQ,
        gdk::Key::r | gdk::Key::R => KeyR,
        gdk::Key::s | gdk::Key::S => KeyS,
        gdk::Key::t | gdk::Key::T => KeyT,
        gdk::Key::u | gdk::Key::U => KeyU,
        gdk::Key::v | gdk::Key::V => KeyV,
        gdk::Key::w | gdk::Key::W => KeyW,
        gdk::Key::x | gdk::Key::X => KeyX,
        gdk::Key::y | gdk::Key::Y => KeyY,
        gdk::Key::z | gdk::Key::Z => KeyZ,

        // -- Top-row digits ----------------------------------------------
        gdk::Key::_0 => Key0,
        gdk::Key::_1 => Key1,
        gdk::Key::_2 => Key2,
        gdk::Key::_3 => Key3,
        gdk::Key::_4 => Key4,
        gdk::Key::_5 => Key5,
        gdk::Key::_6 => Key6,
        gdk::Key::_7 => Key7,
        gdk::Key::_8 => Key8,
        gdk::Key::_9 => Key9,

        // -- Function row ------------------------------------------------
        gdk::Key::F1 => KeyF1,
        gdk::Key::F2 => KeyF2,
        gdk::Key::F3 => KeyF3,
        gdk::Key::F4 => KeyF4,
        gdk::Key::F5 => KeyF5,
        gdk::Key::F6 => KeyF6,
        gdk::Key::F7 => KeyF7,
        gdk::Key::F8 => KeyF8,
        gdk::Key::F9 => KeyF9,
        gdk::Key::F10 => KeyF10,
        gdk::Key::F11 => KeyF11,
        gdk::Key::F12 => KeyF12,

        // -- Navigation / editing ----------------------------------------
        gdk::Key::Return | gdk::Key::KP_Enter => KeyEnter,
        gdk::Key::Tab | gdk::Key::ISO_Left_Tab => KeyTab,
        gdk::Key::space => KeySpace,
        gdk::Key::BackSpace => KeyBackspace,
        gdk::Key::Delete => KeyDelete,
        gdk::Key::Up => KeyUp,
        gdk::Key::Down => KeyDown,
        gdk::Key::Left => KeyLeft,
        gdk::Key::Right => KeyRight,
        gdk::Key::Home => KeyHome,
        gdk::Key::End => KeyEnd,
        gdk::Key::Page_Up => KeyPageup,
        gdk::Key::Page_Down => KeyPagedown,

        // -- Punctuation / symbols ---------------------------------------
        gdk::Key::minus | gdk::Key::underscore => KeyMinus,
        gdk::Key::equal | gdk::Key::plus => KeyEqual,
        gdk::Key::comma | gdk::Key::less => KeyComma,
        gdk::Key::period | gdk::Key::greater => KeyDot,
        gdk::Key::slash | gdk::Key::question => KeySlash,
        gdk::Key::backslash | gdk::Key::bar => KeyBackslash,
        gdk::Key::apostrophe | gdk::Key::quotedbl => KeyApostrophe,
        gdk::Key::grave | gdk::Key::asciitilde => KeyGrave,
        gdk::Key::semicolon | gdk::Key::colon => KeySemicolon,
        gdk::Key::bracketleft | gdk::Key::braceleft => KeyLeftbrace,
        gdk::Key::bracketright | gdk::Key::braceright => KeyRightbrace,

        _ => return None,
    })
}

fn is_modifier(k: scancode::Linux) -> bool {
    matches!(
        k,
        scancode::Linux::KeyLeftCtrl
            | scancode::Linux::KeyRightCtrl
            | scancode::Linux::KeyLeftShift
            | scancode::Linux::KeyRightShift
            | scancode::Linux::KeyLeftAlt
            | scancode::Linux::KeyRightalt
            | scancode::Linux::KeyLeftMeta
            | scancode::Linux::KeyRightmeta
    )
}

fn escape(s: &str) -> String {
    glib::markup_escape_text(s).to_string()
}
