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

fn upstream_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../../vendor/awakened-poe-trade/renderer/public/data")
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
    /// Upstream's stats by trade stat hash.
    upstream: HashMap<u32, Value>,
}

impl Oracle {
    fn load(cache: &Path, code: &str) -> Result<Self, String> {
        let lang = LANGUAGES.iter().find(|(c, _)| *c == code).ok_or(format!("unknown language {code}"))?.1;
        let mods: Vec<stats::Mod> = read_json(&cache.join("tables/English/Mods.json"))?;
        let game_stats: Vec<stats::Stat> = read_json(&cache.join("tables/English/Stats.json"))?;
        let desc = stats::Descriptions::load(&cache.join("files"))?;
        let collected = stats::collect(&mods, &game_stats, &desc);

        let path = upstream_dir().join(code).join("stats.ndjson");
        let text = std::fs::read_to_string(&path).map_err(|e| format!("{}: {e}", path.display()))?;
        let mut upstream: HashMap<u32, Value> = HashMap::new();
        for line in text.lines().filter(|l| !l.is_empty()) {
            let v: Value = serde_json::from_str(line).map_err(|e| e.to_string())?;
            let group = v.get("stats").and_then(Value::as_array).cloned().unwrap_or_else(|| vec![v]);
            for s in group {
                let ids: Vec<u32> = s["trade"]["ids"].as_object().into_iter().flatten().flat_map(|(_, ids)| ids.as_array().into_iter().flatten()).filter_map(|id| id.as_str().and_then(stat_hash)).collect();
                for h in ids {
                    upstream.insert(h, s.clone());
                }
            }
        }
        Ok(Oracle { lang, mods, desc, collected, upstream })
    }

    /// Our matchers for a collected stat, as JSON.
    fn matchers(&self, c: &stats::Collected, ov: Option<&overrides::Override>) -> Value {
        let block = &self.desc.blocks[c.chosen_block(&self.desc, ov)];
        serde_json::to_value(stats::block_matchers(block.lang(self.lang).unwrap_or_default(), ov)).unwrap()
    }
}

/// Returns (covered, exact).
fn compare(cache: &Path, code: &str) -> Result<(usize, usize), String> {
    let o = Oracle::load(cache, code)?;
    let ovs = overrides::load(&overrides_path())?;
    let (mut covered, mut exact) = (0, 0);
    let (mut miss, mut diff) = (Vec::new(), Vec::new());
    for (h, s) in &o.upstream {
        let Some(c) = o.collected.get(h) else {
            miss.push(s["ref"].clone());
            continue;
        };
        covered += 1;
        let ids = &o.desc.blocks[c.block()].ids;
        let ours = o.matchers(c, ovs.get(&overrides::key(ids)));
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
    let extra: Vec<String> = o.collected.iter().filter(|(h, _)| !up_hashes.contains(h)).map(|(_, c)| o.desc.blocks[c.blocks[0]].ids.join(" ")).collect();
    let keys: HashSet<String> = o.collected.values().map(|c| overrides::key(&o.desc.blocks[c.block()].ids)).collect();
    let stale: Vec<&String> = ovs.keys().filter(|k| !keys.contains(*k)).collect();
    println!(
        "upstream hash stats {}, generated {}, covered {covered}, exact {exact} ({:.1}%), extra {}, overrides {} ({} stale)",
        o.upstream.len(),
        o.collected.len(),
        100.0 * exact as f64 / covered.max(1) as f64,
        extra.len(),
        ovs.len(),
        stale.len()
    );
    let out = cache.join(format!("compare-{code}.json"));
    let report = serde_json::json!({ "miss": miss, "diff": diff, "extra": extra, "stale_overrides": stale });
    std::fs::write(&out, serde_json::to_string_pretty(&report).unwrap()).map_err(|e| e.to_string())?;
    println!("details: {}", out.display());
    Ok((covered, exact))
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
        let Some(idx) = wanted.iter().map(|(s, _)| lines.iter().position(|v| stats::line_string(v) == *s)).collect::<Option<Vec<usize>>>() else { continue };
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
                if m.string == *s && m.value != *value {
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
        let got = serde_json::to_value(stats::block_matchers(lines, Some(&ov))).unwrap();
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
    for (h, s) in &o.upstream {
        let Some(c) = o.collected.get(h) else { continue };
        let key = overrides::key(&o.desc.blocks[c.block()].ids);
        if o.matchers(c, ovs.get(&key)) == s["matchers"] {
            continue;
        }
        match derive(&o, c, &s["matchers"]) {
            Some(ov) => {
                ovs.insert(key, ov);
                added += 1;
            }
            None => inexpressible.push(format!("{key}: {}", s["ref"])),
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

    /// Every stat we generate that upstream has must match it exactly. Needs
    /// cache/oracle (export.mjs both patches, then oracle.mjs); skipped
    /// without it.
    #[test]
    fn oracle_matches_upstream() {
        let cache = Path::new(env!("CARGO_MANIFEST_DIR")).join("cache/oracle");
        if !cache.join("files").is_dir() {
            eprintln!("skipped: no {} (see oracle.mjs)", cache.display());
            return;
        }
        let (covered, exact) = compare(&cache, "en").unwrap();
        assert_eq!(exact, covered, "see cache/oracle/compare-en.json");
    }
}
