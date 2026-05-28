//! Register the bundled Omarchy + Adwaita Sans TrueType faces with
//! fontconfig at startup so the release-shortcut chip can render the
//! Hyprland/Omarchy logo (U+E900) for the Super key alongside the
//! standard ⌃ ⇧ ⌥ modifier glyphs.
//!
//! On Linux, Pango resolves font families through fontconfig, so the
//! bytes embedded via `include_bytes!` are dropped into the user cache
//! dir on first launch and registered with the process's fontconfig
//! configuration via `FcConfigAppFontAddFile`. This runs before the
//! first text layout (from `connect_startup`) so Pango's font map picks
//! the faces up when it is first built. Both faces ride in the binary
//! so the chip renders identically regardless of whether the host has
//! omarchy.ttf installed in `~/.local/share/fonts/`.
//!
//! `FcConfigAppFontAddFile` works on every fontconfig/Pango version we
//! ship against — unlike Pango 1.56's `add_font_file`, which the Ubuntu
//! release runners (Pango 1.50–1.52) don't have.
//!
//! On macOS/Windows the Omarchy logo is irrelevant (those platforms
//! draw the Super key as ⌘) and Pango uses a non-fontconfig backend, so
//! registration is a no-op there: `omarchy_available()` stays false and
//! drives the ⌘ fallback in `release_shortcut::super_markup`.

use std::fs;
use std::path::{Path, PathBuf};
use std::sync::OnceLock;

/// Hyprland/Omarchy logo glyph in the bundled `omarchy.ttf`. Lives in
/// the Private Use Area, so the only way to render it is to explicitly
/// ask for the Omarchy face — see `release_shortcut::super_markup`.
pub const OMARCHY_LOGO: char = '\u{e900}';

/// Whether the Omarchy face was successfully registered. Read by the
/// chord-chip markup builder: when false we fall back to the
/// platform-native Super glyph (⌘) instead of asking Pango for a face
/// it doesn't have.
static OMARCHY_FONT_AVAILABLE: OnceLock<bool> = OnceLock::new();

const OMARCHY_TTF: &[u8] = include_bytes!("../assets/omarchy.ttf");
const ADWAITA_SANS_TTF: &[u8] = include_bytes!("../assets/AdwaitaSans-Regular.ttf");

/// Materialize the bundled fonts in the per-user cache dir and register
/// them with fontconfig so Pango can resolve the "Adwaita Sans" and
/// "omarchy" families. Call once, early in startup, before any text is
/// laid out. Safe to call multiple times: the cache writes are
/// idempotent (the byte slice is fixed at compile time, so "exists at
/// same length" implies "matches") and re-adding a known file is a
/// no-op for fontconfig.
pub fn install() {
    let Some(dir) = cache_dir() else {
        log::warn!("glyph_font: no cache dir, skipping bundled-font registration");
        return;
    };
    if let Err(e) = fs::create_dir_all(&dir) {
        log::warn!("glyph_font: create_dir_all({}): {e}", dir.display());
        return;
    }

    let omarchy_path = dir.join("mousehop-omarchy.ttf");
    let adwaita_path = dir.join("mousehop-AdwaitaSans-Regular.ttf");

    let omarchy_ok = stage_and_register(&omarchy_path, OMARCHY_TTF, "Omarchy");
    let _adwaita_ok = stage_and_register(&adwaita_path, ADWAITA_SANS_TTF, "AdwaitaSans");

    let _ = OMARCHY_FONT_AVAILABLE.set(omarchy_ok);
}

/// True iff the Omarchy face is registered and the U+E900 logo glyph
/// can be rendered. Callers use this to pick between the Omarchy logo
/// and the ⌘ fallback for the Super-modifier chip.
pub fn omarchy_available() -> bool {
    *OMARCHY_FONT_AVAILABLE.get().unwrap_or(&false)
}

fn cache_dir() -> Option<PathBuf> {
    let base = std::env::var_os("XDG_CACHE_HOME")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".cache")))?;
    Some(base.join("mousehop").join("fonts"))
}

fn stage_and_register(path: &Path, bytes: &[u8], name: &str) -> bool {
    if let Err(e) = ensure_file(path, bytes) {
        log::warn!("glyph_font: writing {name} to {}: {e}", path.display());
        return false;
    }
    if register_app_font(path) {
        true
    } else {
        log::warn!(
            "glyph_font: fontconfig did not accept {name} at {}",
            path.display()
        );
        false
    }
}

fn ensure_file(path: &Path, bytes: &[u8]) -> std::io::Result<()> {
    if let Ok(existing) = fs::metadata(path) {
        if existing.len() == bytes.len() as u64 {
            return Ok(());
        }
    }
    fs::write(path, bytes)
}

// fontconfig's `FcConfigAppFontAddFile`, declared directly rather than
// via a `-sys` crate. A new crate dependency (and its dlopen helpers)
// isn't in the Nix binary cache, and the Nix build sandbox can't fetch
// it from crates.io (HTTP 403), so pulling one in breaks the flake
// build. libfontconfig is already linked transitively through
// GTK/Pango on Linux and is the same instance Pango resolves families
// against, so faces added here are visible to the chip's markup.
#[cfg(target_os = "linux")]
#[link(name = "fontconfig")]
extern "C" {
    #[link_name = "FcConfigAppFontAddFile"]
    fn fc_config_app_font_add_file(
        config: *mut std::os::raw::c_void,
        file: *const std::os::raw::c_uchar,
    ) -> std::os::raw::c_int;
}

#[cfg(target_os = "linux")]
fn register_app_font(path: &Path) -> bool {
    use std::ffi::CString;
    use std::os::unix::ffi::OsStrExt;

    let Ok(c_path) = CString::new(path.as_os_str().as_bytes()) else {
        return false;
    };
    // SAFETY: a null config selects fontconfig's current configuration,
    // initializing it on first use. fontconfig copies the path string,
    // so `c_path` only needs to outlive the call.
    unsafe { fc_config_app_font_add_file(std::ptr::null_mut(), c_path.as_ptr().cast()) != 0 }
}

#[cfg(not(target_os = "linux"))]
fn register_app_font(_path: &Path) -> bool {
    // macOS/Windows draw the Super key as ⌘ and Pango uses a
    // non-fontconfig backend there, so there is nothing to register.
    false
}
