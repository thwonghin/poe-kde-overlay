//! Generates the poe1 renderer's data (`renderer/public/data/<lang>/*.ndjson`)
//! from game files exported by `export.mjs` and the trade site's lists.
//!
//!   cargo run --release -- compare <cache/patch dir> [lang]
//!       generated stats vs upstream's committed ones (the oracle)
//!   cargo run --release -- derive-overrides <cache/patch dir>
//!       add overrides.json entries for stats no rule reproduces, each
//!       checked to reproduce upstream; lists what can't be expressed

mod descriptions;
mod murmur;
mod overrides;
mod stats;
mod trade;

use serde_json::Value;

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::process::ExitCode;

/// Languages the poe1 renderer ships (renderer/src/assets/make-index-files.mjs);
/// export.mjs's tables.mjs names the same set.
const LANGUAGES: &[(&str, &str)] = &[("en", "English"), ("ru", "Russian"), ("cmn-Hant", "Traditional Chinese"), ("ko", "Korean")];

fn read_json<T: serde::de::DeserializeOwned>(path: &Path) -> Result<T, String> {
    let text = std::fs::read_to_string(path).map_err(|e| format!("{}: {e}", path.display()))?;
    serde_json::from_str(&text).map_err(|e| format!("{}: {e}", path.display()))
}

/// Upstream's committed data; UPSTREAM_DATA points at another copy (e.g. a
/// past league's, `git -C vendor/awakened-poe-trade archive <rev>`).
fn upstream_dir() -> PathBuf {
    std::env::var_os("UPSTREAM_DATA").map(PathBuf::from)
        .unwrap_or_else(|| Path::new(env!("CARGO_MANIFEST_DIR")).join("../../vendor/awakened-poe-trade/renderer/public/data"))
}

/// The numeric hash of a `<type>.stat_<n>` trade id.
fn stat_hash(trade_id: &str) -> Option<u32> {
    trade_id.rsplit_once(".stat_")?.1.parse().ok()
}

fn overrides_path() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("overrides.json")
}

/// Everything a comparison with upstream needs.
struct Oracle {
    lang: &'static str,
    mods: Vec<stats::Mod>,
    desc: stats::Descriptions,
    collected: std::collections::BTreeMap<u32, stats::Collected>,
    /// Upstream's stats by trade stat hash, in its order (see `theirs`).
    upstream: HashMap<u32, Vec<Value>>,
    /// The language's ClientStrings, by id, and English ones.
    client_strings: HashMap<String, String>,
    en_client_strings: HashMap<String, String>,
    trade: trade::Trade,
}

fn load_client_strings(cache: &Path, lang: &str) -> Result<HashMap<String, String>, String> {
    let rows: Vec<Value> = read_json(&cache.join("tables").join(lang).join("ClientStrings.json"))?;
    Ok(rows.iter().filter_map(|r| Some((r["Id"].as_str()?.to_string(), r["Text"].as_str()?.to_string()))).collect())
}

impl Oracle {
    fn load(cache: &Path, code: &str) -> Result<Self, String> {
        let lang = LANGUAGES.iter().find(|(c, _)| *c == code).ok_or(format!("unknown language {code}"))?.1;
        let mods: Vec<stats::Mod> = read_json(&cache.join("tables/English/Mods.json"))?;
        let game_stats: Vec<stats::Stat> = read_json(&cache.join("tables/English/Stats.json"))?;
        let desc = stats::Descriptions::load(&cache.join("files"))?;
        let trade = trade::Trade::load(&cache.join("trade/stats.json"))?;
        // Only what the trade site searches.
        let mut collected = stats::collect(&mods, &game_stats, &desc);
        collected.retain(|h, _| trade.has(*h));
        let client_strings = load_client_strings(cache, lang)?;
        let en_client_strings = load_client_strings(cache, "English")?;

        let path = upstream_dir().join(code).join("stats.ndjson");
        let text = std::fs::read_to_string(&path).map_err(|e| format!("{}: {e}", path.display()))?;
        let mut upstream: HashMap<u32, Vec<Value>> = HashMap::new();
        for line in text.lines().filter(|l| !l.is_empty()) {
            let v: Value = serde_json::from_str(line).map_err(|e| e.to_string())?;
            let group = v.get("stats").and_then(Value::as_array).cloned().unwrap_or_else(|| vec![v]);
            for s in group {
                let ids: HashSet<u32> = s["trade"]["ids"].as_object().into_iter().flatten().flat_map(|(_, ids)| ids.as_array().into_iter().flatten()).filter_map(|id| id.as_str().and_then(stat_hash)).collect();
                for h in ids {
                    upstream.entry(h).or_default().push(s.clone());
                }
            }
        }
        Ok(Oracle { lang, mods, desc, collected, upstream, client_strings, en_client_strings, trade })
    }

    /// Our matchers for a collected stat, as JSON.
    fn matchers(&self, c: &stats::Collected, ov: Option<&overrides::Override>) -> Value {
        let block = &self.desc.blocks[c.chosen_block(&self.desc, ov)];
        let lines = block.lang(self.lang).unwrap_or_default();
        let mut matchers = stats::block_matchers_indexed(lines, ov);
        // Untranslated, or mistranslated ("恢復{1}生命" for Mana): the English
        // lines, as upstream has them.
        let (lang, client_strings) = if lines.is_empty() || stats::mistranslated(lines) {
            matchers = stats::block_matchers_indexed(block.lang("English").unwrap_or_default(), ov);
            ("English", &self.en_client_strings)
        } else {
            (self.lang, &self.client_strings)
        };
        stats::add_advanced(&mut matchers, &self.desc, block, lang);
        serde_json::to_value(c.conditioned(matchers.into_iter().map(|(_, m)| m).collect(), client_strings)).unwrap()
    }

    /// A stats.ndjson entry (the fields generated so far) named `ref_`.
    fn stat(&self, hash: u32, c: &stats::Collected, ov: Option<&overrides::Override>, ref_: &str) -> Value {
        let block = &self.desc.blocks[c.chosen_block(&self.desc, ov)];
        let english = block.lang("English").unwrap_or_default();
        // The site counts the stat in its ref's direction: a ref that is a
        // negated line ("Golems have #% less Life") means inverted values.
        let en = c.conditioned(stats::block_matchers(english, ov), &self.en_client_strings);
        let wording = ref_.replace("+#", "#").replace("-#", "#");
        let inverted = en.iter().any(|m| m.negate && m.string == wording);
        let better = ov.and_then(|o| o.better).unwrap_or(if inverted { -1 } else { 1 });
        let mut trade = serde_json::json!({ "ids": self.trade.ids(hash, ref_).unwrap() });
        if inverted {
            trade["inverted"] = true.into();
        }
        let mut stat = serde_json::json!({ "ref": ref_, "better": better, "matchers": self.matchers(c, ov), "trade": trade });
        if stats::shows_decimals(english) {
            stat["dp"] = true.into();
        }
        stat
    }
}

/// Upstream's entry for a hash to compare with. 27 hashes have two (an
/// item's wording and a map mod's, `fromAreaMods`; or a "Global" twin) and
/// we generate one: the one whose matchers are `ours`, else the last. Not
/// by position alone: upstream re-sorts the file between versions.
fn theirs<'a>(entries: &'a [Value], ours: &Value) -> &'a Value {
    entries.iter().find(|s| s["matchers"] == *ours).unwrap_or_else(|| entries.last().unwrap())
}

/// The fields of an upstream stat the generator makes so far.
fn generated_fields(s: &Value) -> Value {
    let mut out = serde_json::Map::new();
    for k in ["ref", "dp", "better", "matchers"] {
        if let Some(v) = s.get(k) {
            out.insert(k.into(), v.clone());
        }
    }
    let mut trade = serde_json::Map::new();
    for k in ["ids", "inverted"] {
        if let Some(v) = s["trade"].get(k) {
            trade.insert(k.into(), v.clone());
        }
    }
    out.insert("trade".into(), Value::Object(trade));
    Value::Object(out)
}

/// The patch upstream's vendored data was generated from: the latest
/// (github.com/poe-tool-dev/latest-patch-version) when it was committed.
/// Bump it with the vendor: `node export.mjs <patch>`. A patch the CDN has
/// lost bundles of needs oracle.mjs instead (as 3.29.3.3 did).
#[cfg(test)]
const ORACLE_PATCH: &str = "3.30.0.3";

/// Stats the oracle can't reproduce for a reason outside the generator.
const ORACLE_NOISE: &[(&str, &str)] = &[
    // The trade site's list is live: it gained these types after upstream
    // fetched it (matchers equal; ours only has the extra type).
    ("# Maximum Void Charges", "site added implicit"),
    ("#% increased Unarmed Attack Speed with Melee Skills", "site added fractured"),
    ("Attacks with Axes or Swords grant # Rage on Hit", "site added fractured"),
    ("Consumes a Void Charge to Trigger Level # Void Shot when you fire Arrows with a Non-Triggered Skill", "site added implicit"),
    ("Gain Added Spell Damage equal to #% of Damage of Equipped Dagger\nIf two Daggers are Equipped, each contributes 60% as much Added Damage", "site added fractured"),
    ("Gain Added Spell Damage equal to #% of Damage of Equipped Wand\nIf two Wands are Equipped, each contributes half as much Added Damage", "site added fractured"),
    ("Gain a Void Charge every # seconds", "site added implicit"),
    ("Minions convert #% of Physical Damage to Cold Damage", "site added fractured"),
    ("Minions convert #% of Physical Damage to Fire Damage", "site added fractured"),
];

/// Returns the refs of covered stats that differ from upstream.
fn compare(cache: &Path, code: &str) -> Result<Vec<String>, String> {
    let o = Oracle::load(cache, code)?;
    let ovs = overrides::load(&overrides_path())?;
    let (mut covered, mut exact, mut stat_exact, mut ref_rule) = (0, 0, 0, 0);
    let (mut miss, mut diff, mut stat_diff, mut differ) = (Vec::new(), Vec::new(), Vec::new(), Vec::new());
    for (h, entries) in &o.upstream {
        let Some(c) = o.collected.get(h) else {
            miss.push(entries.last().unwrap()["ref"].clone());
            continue;
        };
        covered += 1;
        // Keyed by block: upstream's hand edits also apply to its presence twins.
        let ov = overrides::get(&ovs, &o.desc.blocks[c.block()].ids, code);
        let ours = o.matchers(c, ov);
        let s = theirs(entries, &ours);
        // Refs stay as previously named (renderer code and saved filters
        // look them up); a new stat's is the site's text.
        let rule_ref = o.trade.ref_text(*h).unwrap();
        if rule_ref == s["ref"] {
            ref_rule += 1;
        }
        let ids = c.trade_ids(&o.desc);
        let stat =o.stat(*h, c, ov, s["ref"].as_str().unwrap());
        let theirs = generated_fields(s);
        if stat == theirs {
            stat_exact += 1;
        } else {
            differ.push(s["ref"].as_str().unwrap().to_string());
            stat_diff.push(serde_json::json!({ "theirs": theirs, "ours": stat }));
        }
        if ours == s["matchers"] {
            exact += 1;
        } else {
            let candidates: Vec<Value> = c
                .blocks
                .iter()
                .map(|&b| {
                    let by: Vec<String> = c.mods.iter().filter(|(_, mb)| *mb == b).take(4).map(|&(m, _)| format!("{} d{} g{}", o.mods[m].id, o.mods[m].domain.unwrap_or(0), o.mods[m].generation_type.unwrap_or(0))).collect();
                    serde_json::json!({ "file": o.desc.block_file[b], "text": o.desc.blocks[b].langs[0].1.first().map(|v| v.text.clone()), "mods": by })
                })
                .collect();
            diff.push(serde_json::json!({ "ref": s["ref"], "theirs": s["matchers"], "ours": ours, "ids": ids, "candidates": candidates }));
        }
    }
    let up_hashes: HashSet<&u32> = o.upstream.keys().collect();
    let extra: Vec<Value> = o
        .collected
        .iter()
        .filter(|(h, _)| !up_hashes.contains(h))
        .map(|(h, c)| {
            let mods: Vec<String> = c.mods.iter().map(|&(m, _)| format!("{} d{} g{}", o.mods[m].id, o.mods[m].domain.unwrap_or(0), o.mods[m].generation_type.unwrap_or(0))).collect();
            serde_json::json!({ "ids": o.desc.blocks[c.blocks[0]].ids, "text": o.trade.ref_text(*h), "mods": mods })
        })
        .collect();
    let keys: HashSet<String> = o.collected.values().map(|c| overrides::key(&o.desc.blocks[c.block()].ids)).collect();
    let stale: Vec<&String> = ovs.keys().filter(|k| !keys.contains(overrides::block_of(k))).collect();
    println!(
        "upstream hash stats {}, generated {}, covered {covered}, matchers exact {exact} ({:.1}%), stats exact {stat_exact}, ref = site text {ref_rule}, extra {}, overrides {} ({} stale)",
        o.upstream.len(),
        o.collected.len(),
        100.0 * exact as f64 / covered.max(1) as f64,
        extra.len(),
        ovs.len(),
        stale.len()
    );
    let out = cache.join(format!("compare-{code}.json"));
    let report = serde_json::json!({ "miss": miss, "diff": diff, "stat_diff": stat_diff, "extra": extra, "stale_overrides": stale });
    std::fs::write(&out, serde_json::to_string_pretty(&report).unwrap()).map_err(|e| e.to_string())?;
    let noise = differ.iter().filter(|r| ORACLE_NOISE.iter().any(|(n, _)| n == *r)).count();
    println!("{} differ, {noise} of them known oracle noise (ORACLE_NOISE); details: {}", differ.len(), out.display());
    Ok(differ)
}

/// The override that makes our matchers for `c` equal upstream's `theirs`,
/// if one exists: some wording's lines, kept/valued/ordered as upstream has.
fn derive(o: &Oracle, c: &stats::Collected, theirs: &Value) -> Option<overrides::Override> {
    let wanted: Vec<(String, Option<f64>)> = theirs.as_array()?.iter().map(|m| (m["string"].as_str().unwrap_or_default().to_string(), m["value"].as_f64())).collect();
    let default = c.block();
    let blocks = std::iter::once(default).chain(c.blocks.iter().copied().filter(|&b| b != default));
    for b in blocks {
        let lines = o.desc.blocks[b].lang(o.lang)?;
        // Upstream's matchers as line indexes (first line with that text).
        let wrap = |t: String| c.conditioned(vec![stats::Matcher { string: t, negate: false, value: None, advanced: None }], &o.client_strings).remove(0).string;
        let Some(idx) = wanted.iter().map(|(s, _)| lines.iter().position(|v| wrap(stats::line_string(v)) == *s)).collect::<Option<Vec<usize>>>() else { continue };
        let mut ov = overrides::Override { file: (b != default).then(|| o.desc.block_file[b].clone()), ..Default::default() };
        let all = stats::block_matchers_indexed(lines, None);
        if all.len() != idx.len() || !all.iter().all(|(i, _)| idx.contains(i)) {
            ov.keep = Some({
                let mut k = idx.clone();
                k.sort();
                k
            });
        }
        let ours = stats::block_matchers_indexed(lines, Some(&ov));
        for ((s, value), &i) in wanted.iter().zip(&idx) {
            if let Some((_, m)) = ours.iter().find(|(j, _)| *j == i) {
                if wrap(m.string.clone()) == *s && m.value != *value {
                    ov.value.insert(i, *value);
                }
            }
        }
        let ours: Vec<usize> = stats::block_matchers_indexed(lines, Some(&ov)).into_iter().map(|(i, _)| i).collect();
        if ours != idx {
            ov.order = Some(idx.clone());
        }
        let mut why = Vec::new();
        if ov.file.is_some() { why.push("upstream uses another mod's wording") }
        if ov.keep.is_some() { why.push("upstream drops lines no rule explains") }
        if !ov.value.is_empty() { why.push("upstream sets the value by hand") }
        if ov.order.is_some() { why.push("upstream's order follows no rule") }
        ov.why = why.join("; ");
        let got = serde_json::to_value(c.conditioned(stats::block_matchers(lines, Some(&ov)), &o.client_strings)).unwrap();
        if got == *theirs {
            return Some(ov);
        }
    }
    None
}

fn derive_overrides(cache: &Path) -> Result<(), String> {
    let o = Oracle::load(cache, "en")?;
    let path = overrides_path();
    let mut ovs = overrides::load(&path)?;
    let mut added = 0;
    let mut inexpressible = Vec::new();
    for (h, entries) in &o.upstream {
        let Some(c) = o.collected.get(h) else { continue };
        let key = overrides::key(&o.desc.blocks[c.block()].ids);
        let ours = o.matchers(c, ovs.get(&key));
        let s = theirs(entries, &ours);
        let ref_ = s["ref"].as_str().unwrap();
        if ours != s["matchers"] {
            match derive(&o, c, &s["matchers"]) {
                Some(ov) => {
                    ovs.insert(key.clone(), ov);
                    added += 1;
                }
                None => {
                    inexpressible.push(format!("{key}: {}", s["ref"]));
                    continue;
                }
            }
        }
        let better = s["better"].as_i64().unwrap() as i8;
        if o.stat(*h, c, ovs.get(&key), ref_)["better"] != better {
            let ov = ovs.entry(key).or_default();
            ov.better = Some(better);
            if !ov.why.contains("better") {
                ov.why = [ov.why.as_str(), "which way is better is semantic"].iter().filter(|s| !s.is_empty()).copied().collect::<Vec<_>>().join("; ");
            }
            added += 1;
        }
    }
    overrides::save(&path, &ovs)?;
    println!("added {added} overrides ({} total) to {}", ovs.len(), path.display());
    inexpressible.sort();
    println!("{} not expressible from this patch's lines (text differs):", inexpressible.len());
    for line in &inexpressible {
        println!("  {line}");
    }
    Ok(())
}

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let result = match args.iter().map(String::as_str).collect::<Vec<_>>().as_slice() {
        ["compare", cache] => compare(Path::new(cache), "en").map(drop),
        ["compare", cache, lang] => compare(Path::new(cache), lang).map(drop),
        ["derive-overrides", cache] => derive_overrides(Path::new(cache)),
        _ => Err("usage: poe1-data compare <cache/patch dir> [lang] | derive-overrides <cache/patch dir>".into()),
    };
    match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("error: {e}");
            ExitCode::FAILURE
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Every stat we generate that upstream has must match it exactly, in
    /// every language. Needs
    /// cache/<ORACLE_PATCH> (export.mjs); skipped without it.
    #[test]
    fn oracle_matches_upstream() {
        let cache = Path::new(env!("CARGO_MANIFEST_DIR")).join("cache").join(ORACLE_PATCH);
        if !cache.join("files").is_dir() {
            eprintln!("skipped: no {} (node export.mjs {ORACLE_PATCH})", cache.display());
            return;
        }
        for (code, _) in LANGUAGES {
            let differ = compare(&cache, code).unwrap();
            let unexplained: Vec<&String> = differ.iter().filter(|r| !ORACLE_NOISE.iter().any(|(n, _)| n == r)).collect();
            assert!(unexplained.is_empty(), "{code} differs from upstream (see {}/compare-{code}.json): {unexplained:?}", cache.display());
        }
    }
}

