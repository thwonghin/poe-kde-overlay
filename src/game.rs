//! The two games this host serves, as equal peers: poe1 runs the
//! awakened-poe-trade renderer, poe2 the exiled-exchange-2 one. Which one
//! the overlay shows is decided only by which game is focused (main.rs's
//! `switch_game`); neither is a default or a fallback for the other.

use std::path::PathBuf;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Game {
    Poe1,
    Poe2,
}

impl Game {
    pub const ALL: [Game; 2] = [Game::Poe1, Game::Poe2];

    /// Data subdir, install dir and log name.
    pub fn name(self) -> &'static str {
        match self {
            Game::Poe1 => "poe1",
            Game::Poe2 => "poe2",
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            Game::Poe1 => "PoE1",
            Game::Poe2 => "PoE2",
        }
    }

    /// The game's own window caption, lowercased (kwin_tracker matches
    /// exact captions; see `WindowEvent::is_path_of_exile`).
    pub fn caption(self) -> &'static str {
        match self {
            Game::Poe1 => "path of exile",
            Game::Poe2 => "path of exile 2",
        }
    }

    /// The renderer's built `dist/`. `cargo run` gets the env var from
    /// .cargo/config.toml's [env] table; anything else needs it exported,
    /// otherwise this falls back to where the PKGBUILD installs it.
    pub fn dist_dir(self) -> PathBuf {
        let var = match self {
            Game::Poe1 => "POE_KDE_OVERLAY_POE1_DIST",
            Game::Poe2 => "POE_KDE_OVERLAY_POE2_DIST",
        };
        resolve_dist_dir(self, std::env::var_os(var))
    }

    /// Whether this game's renderer is built at all.
    pub fn available(self) -> bool {
        self.dist_dir().join("index.html").is_file()
    }

    /// Per-game config.json and uploads/.
    pub fn data_dir(self) -> PathBuf {
        crate::xdg::data_dir().join("poe-kde-overlay").join(self.name())
    }

    /// PoE2 only copies advanced mod descriptions (tiers, ranges) with its
    /// show_advanced_item_descriptions key held: EE2's Shortcuts.ts presses
    /// Ctrl + showModsKey (default Alt) + C. APT 3.29 forces plain Ctrl+C,
    /// since PoE1's Ctrl+C now always copies the advanced format.
    /// ponytail: EE2 reads showModsKey from poe2_production_Config.ini; we
    /// assume the default Alt. Parse the ini if someone rebinds it.
    pub fn copy_with_alt(self) -> bool {
        self == Game::Poe2
    }
}

fn resolve_dist_dir(game: Game, env_override: Option<std::ffi::OsString>) -> PathBuf {
    env_override
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("/usr/share/poe-kde-overlay").join(game.name()))
}

// Test-only readers of the vendored upstream sources, for drift checks of
// lists we mirror from both renderers' `main/` (which the build never uses).

#[cfg(test)]
/// `rel` (a path inside each vendor checkout) from every vendor.
pub fn vendor_sources(rel: &str) -> Vec<String> {
    let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR"));
    ["awakened-poe-trade", "exiled-exchange-2"]
        .iter()
        .map(|vendor| {
            let path = root.join("vendor").join(vendor).join(rel);
            std::fs::read_to_string(&path).unwrap_or_else(|e| panic!("{}: {e}", path.display()))
        })
        .collect()
}

#[cfg(test)]
/// Every quoted value after `key:` in a TS source (either quote style,
/// backslash escapes resolved).
pub fn ts_string_values(source: &str, key: &str) -> Vec<String> {
    let mut values = Vec::new();
    for (at, _) in source.match_indices(&format!("{key}:")) {
        let mut chars = source[at + key.len() + 1..].trim_start().chars();
        let Some(quote @ ('\'' | '"')) = chars.next() else { continue };
        let mut value = String::new();
        while let Some(c) = chars.next() {
            match c {
                '\\' => value.extend(chars.next()),
                c if c == quote => break,
                c => value.push(c),
            }
        }
        values.push(value);
    }
    values
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dist_dir_falls_back_to_system_path_without_override() {
        assert_eq!(resolve_dist_dir(Game::Poe2, None), PathBuf::from("/usr/share/poe-kde-overlay/poe2"));
        assert_eq!(resolve_dist_dir(Game::Poe1, Some("/x".into())), PathBuf::from("/x"));
    }
}
