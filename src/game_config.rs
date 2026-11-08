//! poe2's GameConfig.ts: the show_advanced_item_descriptions key from
//! poe2_production_Config.ini, which EE2 holds with Ctrl+C so the copy
//! carries mod tiers and ranges (default Alt). poe1 needs none: APT 3.29
//! forces plain Ctrl+C, which now always copies the advanced format.
//!
//! Unlike upstream, read on every copy (a ~15KB local file), so rebinding
//! the key in-game applies without touching the overlay's settings. An
//! empty gameConfig is found from the focused game's own Wine prefix first
//! (a non-Steam shortcut or a custom prefix), then upstream's fixed
//! guesses.

use std::path::PathBuf;
use std::sync::Mutex;

use crate::logger::Logger;

const RELATIVE: &str = "My Games/Path of Exile 2/poe2_production_Config.ini";

/// The key to hold with Ctrl+C. Falls back to EE2's default Alt, logging
/// (once per outcome) why.
pub fn show_mods_key(configured: &str, pid: Option<i64>, logger: &Logger) -> String {
    let (key, msg) = match find(configured, pid) {
        Err(err) => ("Alt".to_string(), format!("error [GameConfig poe2] {err}; assuming Alt")),
        Ok(path) => match read(&path) {
            Ok(key) => {
                let msg = format!("info [GameConfig poe2] show_advanced_item_descriptions = {key} ({})", path.display());
                (key, msg)
            }
            Err(err) => ("Alt".into(), format!("error [GameConfig poe2] {}: {err}; assuming Alt", path.display())),
        },
    };
    static LAST: Mutex<String> = Mutex::new(String::new());
    let mut last = LAST.lock().unwrap();
    if *last != msg {
        println!("{msg}");
        logger.write(&msg);
        *last = msg;
    }
    key
}

fn find(configured: &str, pid: Option<i64>) -> Result<PathBuf, String> {
    if !configured.is_empty() {
        return Ok(PathBuf::from(configured));
    }
    if let Some(path) = pid.and_then(from_prefix) {
        return Ok(path);
    }
    let found: Vec<PathBuf> = candidates().into_iter().filter(|p| p.is_file()).collect();
    match found.as_slice() {
        [one] => Ok(one.clone()),
        // guessFileLocation: two matches is no match.
        [] | [_, _, ..] => Err("Failed to find game configuration file in the default location".into()),
    }
}

/// The config inside the Wine prefix process `pid` runs in.
fn from_prefix(pid: i64) -> Option<PathBuf> {
    let environ = std::fs::read(format!("/proc/{pid}/environ")).ok()?;
    let var = |name: &str| {
        environ.split(|&b| b == 0).find_map(|entry| {
            let value = entry.strip_prefix(name.as_bytes())?.strip_prefix(b"=")?;
            Some(PathBuf::from(String::from_utf8_lossy(value).into_owned()))
        })
    };
    let prefix = var("WINEPREFIX").or_else(|| var("STEAM_COMPAT_DATA_PATH").map(|p| p.join("pfx")))?;
    std::fs::read_dir(prefix.join("drive_c/users"))
        .ok()?
        .flatten()
        .map(|user| user.path().join("Documents").join(RELATIVE))
        .find(|path| path.is_file())
}

/// GameConfig.ts's Linux POSSIBLE_PATH.
fn candidates() -> Vec<PathBuf> {
    let Some(home) = std::env::var_os("HOME").map(PathBuf::from) else { return Vec::new() };
    // ponytail: $HOME/Documents, not XDG_DOCUMENTS_DIR (Electron's
    // getPath("documents") reads that). Switch if someone relocates it.
    vec![home.join("Documents").join(RELATIVE), home.join(STEAM_RELATIVE)]
}

const STEAM_RELATIVE: &str = ".local/share/Steam/steamapps/compatdata/2694490/pfx/drive_c/users/steamuser/Documents/My Games/Path of Exile 2/poe2_production_Config.ini";

fn read(path: &std::path::Path) -> Result<String, String> {
    let contents = std::fs::read_to_string(path).map_err(|e| e.to_string())?;
    let value = action_key(&contents).ok_or("no show_advanced_item_descriptions in [ACTION_KEYS]")?;
    parse_config_hotkey(value).ok_or_else(|| format!("Failed to read key: {value}"))
}

/// `show_advanced_item_descriptions` under `[ACTION_KEYS]` (the same key
/// under `[WASD_ACTION_KEYS]` is another control scheme's).
fn action_key(contents: &str) -> Option<&str> {
    let mut in_section = false;
    for line in contents.trim_start_matches('\u{feff}').lines() {
        let line = line.trim();
        if line.starts_with('[') {
            in_section = line == "[ACTION_KEYS]";
        } else if in_section && let Some((key, value)) = line.split_once('=') {
            if key.trim() == "show_advanced_item_descriptions" {
                return Some(value.trim());
            }
        }
    }
    None
}

/// GameConfig.ts's parseConfigHotkey: "<virtual key code>[ <1|2|3>]",
/// the digit a Shift/Ctrl/Alt modifier, as a renderer hotkey string.
fn parse_config_hotkey(value: &str) -> Option<String> {
    let mut parts = value.split(' ');
    let key = vk_to_key(parts.next()?.parse().ok()?)?;
    let modifier = match parts.next() {
        None => None,
        Some("1") => Some("Shift"),
        Some("2") => Some("Ctrl"),
        Some("3") => Some("Alt"),
        Some(_) => return None,
    };
    Some(match modifier {
        Some(m) if m != key => format!("{m} + {key}"),
        _ => key,
    })
}

/// KeyToCode.ts's CodeToKey (Windows virtual-key codes).
fn vk_to_key(code: u32) -> Option<String> {
    const NAMED: &[(u32, &str)] = &[
        (8, "Backspace"), (9, "Tab"), (13, "Enter"), (16, "Shift"), (17, "Ctrl"), (18, "Alt"),
        (20, "CapsLock"), (27, "Escape"), (32, "Space"), (33, "PageUp"), (34, "PageDown"),
        (35, "End"), (36, "Home"), (37, "ArrowLeft"), (38, "ArrowUp"), (39, "ArrowRight"),
        (40, "ArrowDown"), (45, "Insert"), (46, "Delete"), (106, "NumpadMultiply"),
        (107, "NumpadAdd"), (109, "NumpadSubtract"), (110, "NumpadDecimal"),
        (111, "NumpadDivide"), (186, "Semicolon"), (187, "Equal"), (188, "Comma"),
        (189, "Minus"), (190, "Period"), (191, "Slash"), (192, "Backquote"),
        (219, "BracketLeft"), (220, "Backslash"), (221, "BracketRight"), (222, "Quote"),
    ];
    Some(match code {
        48..=57 | 65..=90 => char::from(code as u8).to_string(),
        96..=105 => format!("Numpad{}", code - 96),
        112..=135 => format!("F{}", code - 111),
        _ => NAMED.iter().find(|(c, _)| *c == code)?.1.to_string(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_the_action_keys_binding() {
        let ini = "\u{feff}[UI]\nx=1\n[ACTION_KEYS]\nshow_advanced_item_descriptions=90 2\n[WASD_ACTION_KEYS]\nshow_advanced_item_descriptions=18\n";
        assert_eq!(action_key(ini), Some("90 2"));
        assert_eq!(parse_config_hotkey("18").as_deref(), Some("Alt"));
        assert_eq!(parse_config_hotkey("90 2").as_deref(), Some("Ctrl + Z"));
        assert_eq!(parse_config_hotkey("18 3").as_deref(), Some("Alt"));
        assert_eq!(parse_config_hotkey("18 4"), None);
        assert_eq!(parse_config_hotkey("999"), None);
    }

    /// Every key EE2 can read is one we can also press.
    #[test]
    fn virtual_keys_match_vendor_and_are_pressable() {
        let source = std::fs::read_to_string(
            std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("vendor/exiled-exchange-2/ipc/KeyToCode.ts"),
        )
        .unwrap();
        let table = &source[source.find("export const KeyToCode").unwrap()..];
        let table = &table[..table.find('}').unwrap()];
        let mut count = 0;
        for line in table.lines().map(str::trim).filter(|l| !l.starts_with("//")) {
            let Some((name, code)) = line.trim_end_matches(',').split_once(": ") else { continue };
            let code: u32 = code.parse().unwrap();
            assert_eq!(vk_to_key(code).as_deref(), Some(name), "{code}");
            assert!(crate::host_config::key_to_evdev(name).is_some(), "{name}");
            count += 1;
        }
        assert!(count > 100);
    }

    #[test]
    fn guessed_paths_match_vendor() {
        let source = crate::game::vendor_sources("main/src/host-files/GameConfig.ts").remove(1);
        assert!(source.contains(&format!("\"{RELATIVE}\"")));
        assert!(source.contains(&format!("\"{STEAM_RELATIVE}\"")));
    }
}
