//! The trade site's stat list (`/api/trade/data/stats`, saved by export.mjs):
//! which of our stats it searches, and as what types. SnosMe's recipe takes
//! only this from the site; the id itself is derived (`murmur`).

use serde::Deserialize;
use std::collections::HashMap;
use std::path::Path;

#[derive(Deserialize)]
struct List {
    result: Vec<Group>,
}

#[derive(Deserialize)]
struct Group {
    entries: Vec<Entry>,
}

#[derive(Deserialize)]
struct Entry {
    id: String,
    #[serde(default)]
    text: String,
}

/// Types the site lists a `<type>.stat_<hash>` under, in its group order
/// (explicit, implicit, fractured, ...).
pub struct Trade {
    types: HashMap<u32, Vec<String>>,
    /// The site's text of each stat (its first entry, in group order).
    texts: HashMap<u32, String>,
    /// Pseudo stat ids by text.
    pseudo: HashMap<String, Vec<String>>,
}

/// Tags the site appends that upstream's refs drop (it keeps "(Tier 14+)").
const REF_TAGS: &[&str] = &[" (Local)", " (Maps)", " (Shields)", " (Staves)"];

/// Trade site group -> the renderer's modifier type it's filed under (as in
/// upstream's data); None for groups upstream leaves out (retired leagues).
fn renderer_type(group: &str) -> Option<&str> {
    match group {
        "sanctum" => Some("explicit"),
        "mercenary" => Some("pseudo"),
        "scourge" | "delve" | "ultimatum" | "crucible" => None,
        g => Some(g),
    }
}

impl Trade {
    pub fn load(path: &Path) -> Result<Self, String> {
        let text = std::fs::read_to_string(path).map_err(|e| format!("{}: {e}", path.display()))?;
        let list: List = serde_json::from_str(&text).map_err(|e| format!("{}: {e}", path.display()))?;
        Ok(Self::from_entries(list.result.iter().flat_map(|g| g.entries.iter().map(|e| (e.id.as_str(), e.text.as_str())))))
    }

    pub fn from_entries<'a>(entries: impl IntoIterator<Item = (&'a str, &'a str)>) -> Self {
        let mut types: HashMap<u32, Vec<String>> = HashMap::new();
        let mut texts = HashMap::new();
        let mut pseudo: HashMap<String, Vec<String>> = HashMap::new();
        for (id, text) in entries {
            if id.starts_with("pseudo.pseudo_") {
                pseudo.entry(text.to_string()).or_default().push(id.to_string());
            }
            // "explicit.stat_123" (options, "|N" suffixed, and pseudo ids are
            // not derived from game stats)
            let Some((kind, hash)) = id.split_once(".stat_") else { continue };
            let Ok(hash) = hash.parse() else { continue };
            let kinds = types.entry(hash).or_default();
            if renderer_type(kind).is_some() && !kinds.iter().any(|k| k == kind) {
                kinds.push(kind.to_string());
                texts.entry(hash).or_insert_with(|| text.to_string());
            }
        }
        Trade { types, texts, pseudo }
    }

    /// The stat's ref: the site's text, minus a tag upstream drops.
    pub fn ref_text(&self, hash: u32) -> Option<String> {
        let text = self.texts.get(&hash)?;
        Some(REF_TAGS.iter().find_map(|t| text.strip_suffix(t)).unwrap_or(text).to_string())
    }

    /// The stat's `trade.ids` (`{ "explicit": ["explicit.stat_123"], ... }`),
    /// or None if the site doesn't search it (under a type the renderer uses).
    /// A pseudo stat with the same text as `ref` ("Adds # to # Chaos Damage")
    /// is added as `pseudo`, as upstream does (all 38 such in 3.29).
    pub fn ids(&self, hash: u32, ref_: &str) -> Option<serde_json::Map<String, serde_json::Value>> {
        let kinds = self.types.get(&hash).filter(|k| !k.is_empty())?;
        let mut ids: serde_json::Map<_, _> = kinds.iter().map(|k| (renderer_type(k).unwrap().to_string(), serde_json::json!([format!("{k}.stat_{hash}")]))).collect();
        if let Some(p) = self.pseudo.get(ref_) {
            ids.insert("pseudo".into(), serde_json::json!(p));
        }
        Some(ids)
    }

    pub fn has(&self, hash: u32) -> bool {
        self.types.get(&hash).is_some_and(|k| !k.is_empty())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn types_by_hash_and_skips_non_stat_ids() {
        let t = Trade::from_entries([("explicit.stat_1", "a"), ("implicit.stat_1", "a"), ("explicit.stat_2|3", "b"), ("pseudo.pseudo_total_life", "c"), ("explicit.indexable_skill_5", "d")]);
        assert_eq!(serde_json::Value::Object(t.ids(1, "a").unwrap()), serde_json::json!({ "explicit": ["explicit.stat_1"], "implicit": ["implicit.stat_1"] }));
        assert!(t.ids(2, "b").is_none());
        // Filed as upstream does; retired-league types dropped.
        let t = Trade::from_entries([("sanctum.stat_7", "x"), ("scourge.stat_7", "x"), ("scourge.stat_8", "y")]);
        assert_eq!(serde_json::Value::Object(t.ids(7, "x").unwrap()), serde_json::json!({ "explicit": ["sanctum.stat_7"] }));
        assert!(t.ids(8, "y").is_none());
    }

    #[test]
    fn ref_and_pseudo_by_text() {
        let t = Trade::from_entries([
            ("pseudo.pseudo_adds_chaos_damage", "Adds # to # Chaos Damage"),
            ("explicit.stat_1", "Adds # to # Chaos Damage"),
            ("explicit.stat_2", "#% increased Armour (Local)"),
            ("explicit.stat_3", "#% increased Quantity (Tier 14+)"),
        ]);
        assert_eq!(t.ref_text(2).unwrap(), "#% increased Armour");
        assert_eq!(t.ref_text(3).unwrap(), "#% increased Quantity (Tier 14+)");
        let ids = t.ids(1, &t.ref_text(1).unwrap()).unwrap();
        assert_eq!(ids["pseudo"], serde_json::json!(["pseudo.pseudo_adds_chaos_damage"]));
    }
}
