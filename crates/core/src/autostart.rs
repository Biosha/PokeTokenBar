//! Launch-at-login via an XDG autostart entry (the port of the macOS `LoginItem`).
//!
//! Platform scope: on macOS that one toggle also carried the crash-restart watchdog
//! (a KeepAlive LaunchAgent, because Apple's login-item API does not restart crashes).
//! Linux has no equivalent single primitive — the XDG autostart entry only starts the
//! app at session login; crash restart is out of scope for this port.

use std::fs;
use std::path::{Path, PathBuf};

/// Well-known app id (matches the D-Bus name in `sni.rs` / the GApplication id).
const APP_ID: &str = "io.github.poketoken.app";
/// Marker line proving the autostart file is ours (never adopt a foreign file).
const MARKER: &str = "# PokeTokenBar autostart entry";
/// Seconds after login before the app starts — the session bus and tray host need a
/// moment to come up, and a tray app started too early registers no icon.
const AUTOSTART_DELAY: u32 = 5;

/// The XDG autostart file for this app: `$XDG_CONFIG_HOME/autostart/<id>.desktop`
/// (else `~/.config/autostart/…`).
pub fn desktop_file() -> Option<PathBuf> {
    dirs::config_dir().map(|d| d.join("autostart").join(format!("{APP_ID}.desktop")))
}

/// Render the autostart entry for `exec` (pure — the file write is a thin wrapper).
pub fn render_desktop_entry(exec: &str) -> String {
    format!(
        "{MARKER}\n\
         [Desktop Entry]\n\
         Type=Application\n\
         Name=PokeTokenBar\n\
         Comment=Token-usage companion Pokémon\n\
         Exec={exec}\n\
         Terminal=false\n\
         X-GNOME-Autostart-enabled=true\n\
         X-GNOME-Autostart-Delay={AUTOSTART_DELAY}\n"
    )
}

/// Whether `contents` is our autostart entry (the marker, not mere file existence —
/// a hand-edited or stale file without it must not read as "enabled").
pub fn is_our_entry(contents: &str) -> bool {
    contents.lines().any(|line| line.trim() == MARKER)
}

pub fn is_enabled() -> bool {
    desktop_file()
        .and_then(|p| fs::read_to_string(p).ok())
        .map(|c| is_our_entry(&c))
        .unwrap_or(false)
}

/// Enable or disable launch-at-login. `exec` is the running binary path (quoted if it
/// contains spaces, per the desktop-entry spec).
pub fn set_enabled(on: bool) -> anyhow::Result<()> {
    let path = desktop_file().ok_or_else(|| anyhow::anyhow!("no config directory"))?;
    set_enabled_at(&path, on)
}

/// The file-level operation, path-injected so tests never touch the real autostart dir.
pub fn set_enabled_at(path: &Path, on: bool) -> anyhow::Result<()> {
    if on {
        let exec = std::env::current_exe().unwrap_or_else(|_| PathBuf::from("poketoken-app"));
        let exec = exec.to_string_lossy();
        let exec = if exec.contains(' ') {
            format!("\"{exec}\"")
        } else {
            exec.to_string()
        };
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)?;
        }
        let tmp = path.with_extension("desktop.tmp");
        fs::write(&tmp, render_desktop_entry(&exec))?;
        fs::rename(&tmp, path)?;
    } else {
        match fs::remove_file(path) {
            Ok(()) => {}
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => return Err(e.into()),
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn renders_required_desktop_keys() {
        let entry = render_desktop_entry("/usr/local/bin/poketoken-app");
        assert!(entry.contains("[Desktop Entry]"));
        assert!(entry.contains("Type=Application"));
        assert!(entry.contains("Exec=/usr/local/bin/poketoken-app"));
        assert!(entry.contains("X-GNOME-Autostart-enabled=true"));
        assert!(is_our_entry(&entry));
    }

    #[test]
    fn marker_detection_is_line_anchored() {
        assert!(is_our_entry("junk\n# PokeTokenBar autostart entry\n[Desktop Entry]"));
        assert!(!is_our_entry("# PokeTokenBar autostart entry: do not edit"));
        assert!(!is_our_entry("[Desktop Entry]"));
    }

    #[test]
    fn enable_disable_roundtrip() {
        let dir = std::env::temp_dir().join(format!(
            "ptb-autostart-test-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let path = dir.join("autostart").join(format!("{APP_ID}.desktop"));
        set_enabled_at(&path, true).unwrap();
        assert!(path.exists());
        assert!(is_our_entry(&fs::read_to_string(&path).unwrap()));

        set_enabled_at(&path, false).unwrap();
        assert!(!path.exists());
        // Disabling twice is a no-op, not an error.
        set_enabled_at(&path, false).unwrap();
        let _ = fs::remove_dir_all(&dir);
    }
}
