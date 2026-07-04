use std::process::Command;

#[derive(serde::Serialize)]
pub struct ThemeColors {
    pub dark: bool,
}

/// Dark/light detection, most-canonical signal first:
///
/// 1. `org.gnome.desktop.interface color-scheme` — the freedesktop-standard
///    key since GNOME 42 ('prefer-dark' / 'prefer-light' / 'default').
///    This is the signal desktops are converging on; theme NAMES are the
///    legacy channel and are slowly being drained of meaning.
/// 2. `gtk-theme` name containing "dark" — legacy fallback, still what many
///    GTK3 desktops (and the Lean Linux themes) set.
/// 3. Undetectable → default dark (matches the app's pre-React default).
pub fn get_theme_colors() -> ThemeColors {
    let dark = match read_color_scheme().as_str() {
        s if s.contains("prefer-dark") => true,
        s if s.contains("prefer-light") => false,
        // 'default', empty, or gsettings unavailable → legacy fallback
        _ => {
            let name = read_gtk_theme_name();
            name.is_empty() || name.to_lowercase().contains("dark")
        }
    };
    ThemeColors { dark }
}

fn read_color_scheme() -> String {
    read_gsetting("color-scheme")
}

fn read_gtk_theme_name() -> String {
    read_gsetting("gtk-theme")
}

fn read_gsetting(key: &str) -> String {
    let output = Command::new("gsettings")
        .args(["get", "org.gnome.desktop.interface", key])
        .output();

    match output {
        Ok(out) if out.status.success() => {
            let raw = String::from_utf8_lossy(&out.stdout);
            raw.trim().trim_matches('\'').to_string()
        }
        _ => String::new(),
    }
}
