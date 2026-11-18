//! items.ndjson entries for base types upstream's data doesn't have yet
//! (3.30's Viscera), so the renderer recognises them: without an entry a
//! price check fails with "unknown item". `build-renderer.sh` appends them to
//! the built renderer's data.

use serde::Deserialize;
use serde_json::{Value, json};
use std::collections::HashSet;

/// Item classes (ItemClasses.Id) whose base types are added. The renderer has
/// no category for them, so it searches by base type and stats.
pub const CLASSES: &[&str] = &["Viscera"];

#[derive(Deserialize)]
#[serde(rename_all = "PascalCase")]
pub struct BaseItem {
    pub id: String,
    pub item_classes_key: Option<usize>,
    pub name: String,
}

#[derive(Deserialize)]
#[serde(rename_all = "PascalCase")]
pub struct ItemClass {
    pub id: String,
}

/// Entries for the bases of `CLASSES` not in `existing` (ITEM refNames),
/// named in the language of `names` (BaseItemTypes, same rows as `english`).
/// Icons are signed CDN URLs no game file gives; upstream leaves 242 empty.
pub fn extra_items(english: &[BaseItem], names: &[BaseItem], classes: &[ItemClass], existing: &HashSet<String>) -> Result<Vec<Value>, String> {
    let mut seen = HashSet::new();
    let mut out = Vec::new();
    for (en, local) in english.iter().zip(names) {
        if en.id != local.id {
            return Err(format!("BaseItemTypes rows differ: {} vs {}", en.id, local.id));
        }
        let class = en.item_classes_key.map(|k| classes[k].id.as_str());
        if !class.is_some_and(|c| CLASSES.contains(&c)) || existing.contains(&en.name) || !seen.insert(en.name.clone()) {
            continue;
        }
        out.push(json!({ "name": local.name, "refName": en.name, "namespace": "ITEM", "icon": "" }));
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn adds_missing_bases_of_the_classes() {
        let b = |id: &str, class, name: &str| BaseItem { id: id.into(), item_classes_key: Some(class), name: name.into() };
        let classes = [ItemClass { id: "Ring".into() }, ItemClass { id: "Viscera".into() }];
        let en = [b("a", 1, "Bear Head"), b("b", 1, "Bear Head"), b("c", 0, "Iron Ring"), b("d", 1, "Boar Head")];
        let ko = [b("a", 1, "곰 머리"), b("b", 1, "곰 머리"), b("c", 0, "철 반지"), b("d", 1, "멧돼지 머리")];
        let existing = HashSet::from(["Boar Head".to_string()]);
        let got = extra_items(&en, &ko, &classes, &existing).unwrap();
        // Another class, a base upstream has, and a second row of the same
        // name are left out.
        assert_eq!(got, [json!({ "name": "곰 머리", "refName": "Bear Head", "namespace": "ITEM", "icon": "" })]);
        assert!(extra_items(&en, &ko[1..], &classes, &existing).is_err());
    }
}
