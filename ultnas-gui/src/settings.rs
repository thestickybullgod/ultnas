//! The app's own settings (`<config>/ultnas/gui.toml`) and its autostart
//! entry (`<config>/autostart/<app id>.desktop`).

use serde::{Deserialize, Serialize};
use std::{
    fs, io,
    path::{Path, PathBuf},
};

use crate::APP_ID;

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct Settings {
    /// Keep running with no window, and start at login, so notifications
    /// keep arriving.
    pub background: bool,
}

/// `$XDG_CONFIG_HOME`, else `~/.config`.
pub fn config_dir() -> Option<PathBuf> {
    let var = |k| {
        std::env::var_os(k)
            .filter(|v| !v.is_empty())
            .map(PathBuf::from)
    };
    var("XDG_CONFIG_HOME").or_else(|| var("HOME").map(|h| h.join(".config")))
}

impl Settings {
    /// The saved settings; defaults when there are none or they can't be read.
    pub fn load(config: &Path) -> Self {
        fs::read_to_string(settings_file(config))
            .ok()
            .and_then(|s| toml::from_str(&s).ok())
            .unwrap_or_default()
    }

    /// Save, and add or remove the autostart entry to match.
    pub fn save(&self, config: &Path) -> io::Result<()> {
        let file = settings_file(config);
        fs::create_dir_all(file.parent().expect("has a parent"))?;
        let text = toml::to_string(self).map_err(io::Error::other)?;
        fs::write(&file, text)?;
        set_autostart(config, self.background)
    }
}

fn settings_file(config: &Path) -> PathBuf {
    config.join("ultnas").join("gui.toml")
}

fn autostart_file(config: &Path) -> PathBuf {
    config.join("autostart").join(format!("{APP_ID}.desktop"))
}

fn set_autostart(config: &Path, enabled: bool) -> io::Result<()> {
    let file = autostart_file(config);
    if !enabled {
        return match fs::remove_file(&file) {
            Err(e) if e.kind() != io::ErrorKind::NotFound => Err(e),
            _ => Ok(()),
        };
    }
    fs::create_dir_all(file.parent().expect("has a parent"))?;
    fs::write(
        &file,
        format!(
            "[Desktop Entry]\n\
             Type=Application\n\
             Name=Ultnas\n\
             Comment=Notifications when Ultnas protects your files\n\
             Exec=ultnas-gui --background\n\
             Icon={APP_ID}\n\
             NoDisplay=true\n\
             X-GNOME-Autostart-enabled=true\n"
        ),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn missing_settings_are_the_defaults() {
        let dir = tempfile::TempDir::new().unwrap();
        assert_eq!(Settings::load(dir.path()), Settings::default());
    }

    #[test]
    fn background_round_trips_and_drives_autostart() {
        let dir = tempfile::TempDir::new().unwrap();
        let on = Settings { background: true };
        on.save(dir.path()).unwrap();
        assert_eq!(Settings::load(dir.path()), on);
        let entry = fs::read_to_string(autostart_file(dir.path())).unwrap();
        assert!(entry.contains("Exec=ultnas-gui --background"), "{entry}");

        Settings::default().save(dir.path()).unwrap();
        assert_eq!(Settings::load(dir.path()), Settings::default());
        assert!(!autostart_file(dir.path()).exists());
        // Turning it off twice is fine.
        Settings::default().save(dir.path()).unwrap();
    }

    #[test]
    fn unreadable_settings_fall_back_to_defaults() {
        let dir = tempfile::TempDir::new().unwrap();
        let file = settings_file(dir.path());
        fs::create_dir_all(file.parent().unwrap()).unwrap();
        fs::write(&file, "background = [not toml").unwrap();
        assert_eq!(Settings::load(dir.path()), Settings::default());
    }
}
