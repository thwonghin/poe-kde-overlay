//! Hand-set corrections (`overrides.json`) for the stats where upstream's
//! data departs from every rule the generator derives: a line it drops, a
//! value it sets by hand, an order or a wording no rule explains.
//!
//! Keyed by the description block's stat ids (space-separated), so an entry
//! survives a league, also applies to the block's Eldritch presence twins
//! (upstream edits those alike), and applies to every language: the translations of a
//! block have the same lines, with the same conditions, in the same order.
//! Lines are named by their index in the block (file order, from 0).
//! Where upstream treats a language apart (a value only English has),
//! `<ids>@<lang code>` replaces the entry for that language.
//!
//! `derive-overrides` writes the entries from upstream's data and checks
//! each reproduces it; an entry whose block no longer exists is reported.

use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::path::Path;

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Override {
    /// Why no rule covers it.
    pub why: String,
    /// Use the wording of this description file's block.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub file: Option<String>,
    /// Only these lines.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub keep: Option<Vec<usize>>,
    /// The value of these lines (null: none).
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub value: BTreeMap<usize, Option<f64>>,
    /// Matchers in this order of lines.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub order: Option<Vec<usize>>,
    /// Whether a higher (1) or lower (-1) value is better, where it isn't
    /// what the trade site's wording implies ("#% increased Damage taken").
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub better: Option<i8>,
}

pub type Overrides = BTreeMap<String, Override>;

pub fn key(ids: &[String]) -> String {
    ids.join(" ")
}

/// The entry for `ids` in language `code` (renderer code: "en", "ko").
pub fn get<'a>(overrides: &'a Overrides, ids: &[String], code: &str) -> Option<&'a Override> {
    let key = key(ids);
    overrides.get(&format!("{key}@{code}")).or_else(|| overrides.get(&key))
}

/// The block key of an entry's key ("a b@ko" -> "a b").
pub fn block_of(key: &str) -> &str {
    key.split_once('@').map_or(key, |(k, _)| k)
}

pub fn load(path: &Path) -> Result<Overrides, String> {
    match std::fs::read_to_string(path) {
        Ok(text) => serde_json::from_str(&text).map_err(|e| format!("{}: {e}", path.display())),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Overrides::new()),
        Err(e) => Err(format!("{}: {e}", path.display())),
    }
}

pub fn save(path: &Path, overrides: &Overrides) -> Result<(), String> {
    let text = serde_json::to_string_pretty(overrides).unwrap() + "\n";
    std::fs::write(path, text).map_err(|e| format!("{}: {e}", path.display()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn language_entry_replaces_the_shared_one() {
        let ov = |why: &str| Override { why: why.into(), ..Default::default() };
        let ovs = Overrides::from([("a b".into(), ov("all")), ("a b@ko".into(), ov("ko"))]);
        let ids = ["a".to_string(), "b".to_string()];
        assert_eq!(get(&ovs, &ids, "ko").unwrap().why, "ko");
        assert_eq!(get(&ovs, &ids, "en").unwrap().why, "all");
        assert_eq!(get(&ovs, &ids[..1], "ko"), None);
        assert_eq!(block_of("a b@ko"), "a b");
    }
}
