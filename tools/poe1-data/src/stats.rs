//! Builds stats.ndjson entries from game data, following SnosMe's recipe
//! (awakened-poe-trade#1641): mods that can spawn (or are unique) -> their
//! stats' description blocks (file chosen by mod domain) -> one entry per
//! block, with the trade id derived from the block's stat ids.

use crate::descriptions::{self, Block, Variant, matcher_text};
use crate::murmur::trade_stat_hash;
use crate::overrides::Override;
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, HashMap};
use std::path::Path;

/// ModDomains (dat-schema enum) of the mods upstream reads -> the description
/// file the game uses for them. Files include their parent (map ->
/// stat_descriptions), see `Descriptions`.
///
/// Left out because upstream's data has no stat that only they produce:
/// MONSTER 3, CHEST 4, ATLAS 11 (passive tree), LEAGUESTONE 12, MAP_DEVICE 14
/// (scarabs), DELVE_AREA 17, WATCHSTONE 25, EXPEDITION_RELIC 27,
/// ELDRITCH_ALTAR 29, SENTINEL 30, SANCTUM_SPECIAL 32, CRUCIBLE_MAP 33,
/// GRAFT 38. Their wordings must not win ties either (see `pick_block`).
fn domain_file(domain: u32) -> Option<&'static str> {
    Some(match domain {
        // ITEM, FLASK, CRAFTED, BASE_JEWEL, ABYSS_JEWEL, DELVE, AFFLICTION_JEWEL,
        // HEIST_TRINKET, VEILED, UNVEILED, ANIMAL_CHARM
        1 | 2 | 9 | 10 | 13 | 16 | 21 | 24 | 26 | 28 | 35 => "stat_descriptions",
        5 | 22 => "map_stat_descriptions", // AREA, HEIST_AREA
        7 => "sanctum_relic_stat_descriptions",
        23 => "heist_equipment_stat_descriptions", // HEIST_NPC
        34 => "tincture_stat_descriptions",
        37 => "atlas_relic_stat_descriptions", // IDOL (MapRelic mods): includes atlas, "in your Maps"
        _ => return None,
    })
}
const GEN_UNIQUE: u32 = 3;

#[derive(Deserialize)]
#[serde(rename_all = "PascalCase")]
pub struct Mod {
    pub id: String,
    pub domain: Option<u32>,
    pub generation_type: Option<u32>,
    #[serde(rename = "SpawnWeight_Values")]
    pub spawn_weights: Vec<i32>,
    pub stats_key1: Option<usize>,
    pub stats_key2: Option<usize>,
    pub stats_key3: Option<usize>,
    pub stats_key4: Option<usize>,
    pub stats_key5: Option<usize>,
    pub stats_key6: Option<usize>,
}

impl Mod {
    fn stat_keys(&self) -> impl Iterator<Item = usize> {
        [self.stats_key1, self.stats_key2, self.stats_key3, self.stats_key4, self.stats_key5, self.stats_key6]
            .into_iter()
            .flatten()
    }
}

#[derive(Deserialize)]
#[serde(rename_all = "PascalCase")]
pub struct Stat {
    pub id: String,
}

/// Every description file of a patch, with `include`s resolved.
pub struct Descriptions {
    pub blocks: Vec<Block>,
    /// File each block is from, by block index.
    pub block_file: Vec<String>,
    files: HashMap<String, (Option<String>, HashMap<String, usize>)>,
}

impl Descriptions {
    pub fn load(dir: &Path) -> Result<Self, String> {
        let mut out = Descriptions { blocks: Vec::new(), block_file: Vec::new(), files: HashMap::new() };
        for entry in std::fs::read_dir(dir).map_err(|e| format!("{}: {e}", dir.display()))? {
            let path = entry.map_err(|e| e.to_string())?.path();
            let text = std::fs::read_to_string(&path).map_err(|e| format!("{}: {e}", path.display()))?;
            let name = path.file_stem().unwrap().to_string_lossy().into_owned();
            out.add(&name, &text).map_err(|e| format!("{}: {e}", path.display()))?;
        }
        Ok(out)
    }

    pub fn add(&mut self, name: &str, text: &str) -> Result<(), String> {
        let mut by_stat = HashMap::new();
        for block in descriptions::parse(text)? {
            let idx = self.blocks.len();
            for id in &block.ids {
                by_stat.insert(id.clone(), idx); // a later definition wins (10 stats are defined twice)
            }
            self.blocks.push(block);
            self.block_file.push(name.to_string());
        }
        self.files.insert(name.to_string(), (descriptions::include_of(text), by_stat));
        Ok(())
    }

    /// The block the game would use for `stat_id` in `file`: the file's own,
    /// else the one of the file it includes, and so on.
    pub fn lookup(&self, file: &str, stat_id: &str) -> Option<usize> {
        let mut file = self.files.get(file);
        while let Some((include, by_stat)) = file {
            if let Some(&idx) = by_stat.get(stat_id) {
                return Some(idx);
            }
            file = include.as_ref().and_then(|f| self.files.get(f));
        }
        None
    }
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Matcher {
    pub string: String,
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    pub negate: bool,
    #[serde(skip_serializing_if = "Option::is_none", serialize_with = "number")]
    pub value: Option<f64>,
}

// 1.0 -> 1, as upstream's JSON has it.
fn number<S: serde::Serializer>(v: &Option<f64>, s: S) -> Result<S::Ok, S::Error> {
    match v {
        Some(v) if v.fract() == 0.0 => s.serialize_i64(*v as i64),
        Some(v) => s.serialize_f64(*v),
        None => s.serialize_none(),
    }
}

/// Unit conversion the game applies before showing a number (flag names
/// carry a precision suffix: `_2dp_if_required`, ...); None if `flag` isn't one.
fn conversion(flag: &str) -> Option<f64> {
    const DIVISORS: &[(&str, f64)] = &[
        ("milliseconds_to_seconds", 1000.0),
        ("deciseconds_to_seconds", 10.0),
        ("divide_by_one_thousand", 1000.0),
        ("divide_by_one_hundred", 100.0),
        ("divide_by_ten", 10.0),
        ("per_minute_to_per_second", 60.0),
        ("locations_to_metres", 10.0),
    ];
    if flag.ends_with("negate") {
        return None; // divide_by_one_hundred_and_negate: not a plain unit
    }
    DIVISORS.iter().find(|(p, _)| flag.starts_with(p)).map(|&(_, d)| d)
}

/// The value a line without a `#` stands for ("Gain Onslaught on Kill" is a
/// 100% chance, "Unlucky" is -1): the bound of the first stat it fixes to a
/// non-zero range ("Projectiles Return to you" is `0 100|#` -> 100). With
/// none, a `!0` stat means no single value ("Your Energy Shield starts at
/// zero": `0 !0`), else a stat fixed to 0 gives 0. Shown in display units:
/// the stat's conversion flag from this line, else from another line of the
/// block ("every second" is `1000` -> 1 by its sibling's
/// milliseconds_to_seconds).
fn fixed_value(v: &Variant, block: &[Variant]) -> Option<f64> {
    let bound = |c: &descriptions::Cond| c.min.or(c.max);
    let zero = |c: &descriptions::Cond| c.min == Some(0) && c.max == Some(0);
    let stat = match v.conds.iter().position(|c| bound(c).is_some() && !zero(c)) {
        Some(i) => i,
        None if v.conds.iter().any(|c| c.not.is_some()) => return None,
        None => v.conds.iter().position(zero)?,
    };
    let index = (stat + 1).to_string();
    let divisor = |flags: &[descriptions::Flag]| flags.iter().find(|f| f.args.first() == Some(&index)).and_then(|f| conversion(&f.name));
    let divisor = divisor(&v.flags).or_else(|| block.iter().find_map(|o| divisor(&o.flags))).unwrap_or(1.0);
    Some(bound(&v.conds[stat])? as f64 / divisor)
}

/// Matchers for one language's variants of a block: all of them, even
/// lines no mod can roll ("Adds # minimum Cold Damage" when every mod has a
/// maximum too) — upstream keeps those. Filtering by mod rolls, by the last
/// mod, by real uniques (wiki) or by trade texts each matched upstream worse
/// than keeping all; where upstream drops a line by hand, overrides.json
/// says so.
///
/// Order, as upstream sorts (1818 of 1823 multi-line stats in 3.29): fixed
/// lines with a negative value ("Lose an Endurance Charge"), plain lines,
/// fixed lines with a positive value, negated lines; by value, then by text
/// (code points: "#%" < "30%", "Gain" < "Lose", "less" < "more").
///
/// `ov` applies a hand-set correction (see `overrides`).
pub fn block_matchers(variants: &[Variant], ov: Option<&Override>) -> Vec<Matcher> {
    block_matchers_indexed(variants, ov).into_iter().map(|(_, m)| m).collect()
}

/// The text a line is matched by. The renderer reads a value with its sign
/// ("-5 to X" is # = -5), so "+#"/"-#" lines are just "#" (and "-{0} to
/// Evasion" is a duplicate of "+{0} to Evasion").
pub fn line_string(v: &Variant) -> String {
    matcher_text(&v.text).replace("+#", "#").replace("-#", "#")
}

/// `block_matchers`, each with the index of the line it comes from (the
/// first, of lines with the same text).
pub fn block_matchers_indexed(variants: &[Variant], ov: Option<&Override>) -> Vec<(usize, Matcher)> {
    let mut out: Vec<(usize, Matcher)> = Vec::new();
    for (i, v) in variants.iter().enumerate() {
        let string = line_string(v);
        if out.iter().any(|(_, m)| m.string == string) || ov.and_then(|o| o.keep.as_ref()).is_some_and(|keep| !keep.contains(&i)) {
            continue;
        }
        let mut m = Matcher { string, negate: false, value: None };
        if let Some(value) = Some(()).filter(|_| !m.string.contains('#')).and_then(|_| fixed_value(v, variants)) {
            m.value = Some(value);
        } else if v.has_flag("negate") {
            m.negate = true;
        }
        out.push((i, m));
    }
    // A lone flag line ("Can't use Mana Flasks") has nothing to tell apart.
    if let [(_, only)] = out.as_mut_slice() {
        only.value = None;
    }
    for (i, m) in &mut out {
        if let Some(&value) = ov.and_then(|o| o.value.get(i)) {
            m.value = value;
            m.negate = value.is_none() && variants[*i].has_flag("negate");
        }
    }
    let class = |m: &Matcher| match m.value {
        Some(v) if v < 0.0 => 0,
        Some(_) => 2,
        None if m.negate => 3,
        None => 1,
    };
    out.sort_by(|(_, a), (_, b)| {
        class(a).cmp(&class(b)).then(a.value.unwrap_or(0.0).total_cmp(&b.value.unwrap_or(0.0))).then_with(|| a.string.cmp(&b.string))
    });
    if let Some(order) = ov.and_then(|o| o.order.as_ref()) {
        out.sort_by_key(|(i, _)| order.iter().position(|o| o == i).unwrap_or(usize::MAX));
    }
    out
}

/// A trade stat: every wording (block) the game has for it, one per mod
/// domain that uses it, and the mods.
#[derive(Default)]
pub struct Collected {
    pub blocks: Vec<usize>,
    /// (mod, block it was described with), in Mods table order.
    pub mods: Vec<(usize, usize)>,
}

impl Collected {
    /// The wording upstream uses: the last mod's (in table order) — as if
    /// each mod overwrote the stat (Idol "in your Maps" over a map mod's "in
    /// Area"). The 3.29 exceptions are in overrides.json.
    pub fn block(&self) -> usize {
        self.mods.last().expect("collected with a mod").1
    }

    /// `block`, unless an override names the file whose wording to use.
    pub fn chosen_block(&self, desc: &Descriptions, ov: Option<&Override>) -> usize {
        let file = ov.and_then(|o| o.file.as_deref());
        file.and_then(|f| self.blocks.iter().copied().find(|&b| desc.block_file[b] == f)).unwrap_or_else(|| self.block())
    }
}

pub fn collect(mods: &[Mod], stats: &[Stat], desc: &Descriptions) -> BTreeMap<u32, Collected> {
    let mut by_hash: BTreeMap<u32, Collected> = BTreeMap::new();
    for (mod_idx, m) in mods.iter().enumerate() {
        let spawns = m.spawn_weights.iter().any(|&w| w > 0) || m.generation_type == Some(GEN_UNIQUE);
        let Some(file) = m.domain.and_then(domain_file).filter(|_| spawns) else { continue };
        let mut seen = Vec::new();
        for key in m.stat_keys() {
            let Some(block) = desc.lookup(file, &stats[key].id) else { continue };
            if seen.contains(&block) {
                continue; // a mod's second stat of the same block
            }
            seen.push(block);
            let entry = by_hash.entry(trade_stat_hash(&desc.blocks[block].ids)).or_default();
            if !entry.blocks.contains(&block) {
                entry.blocks.push(block);
            }
            entry.mods.push((mod_idx, block));
        }
    }
    by_hash
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::descriptions::parse;

    fn matchers(src: &str) -> Vec<(String, bool, Option<f64>)> {
        let text = format!("description\n1 s\n{src}");
        block_matchers(parse(&text).unwrap()[0].lang("English").unwrap(), None).into_iter().map(|m| (m.string, m.negate, m.value)).collect()
    }
    fn m(s: &str, negate: bool, value: Option<f64>) -> (String, bool, Option<f64>) {
        (s.into(), negate, value)
    }

    #[test]
    fn negated_variants_follow_plain_ones() {
        let got = matchers("2\n#|-1 \"Lose {0} Life\" negate 1\n1|# \"{0} Life gained\"");
        assert_eq!(got, [m("# Life gained", false, None), m("Lose # Life", true, None)]);
    }

    #[test]
    fn order_negative_fixed_plain_positive_fixed_negated_then_text() {
        // gain_X_endurance_charges_per_second, in file order.
        let got = matchers("4\n1 \"Gain an Endurance Charge each second\"\n1|# \"Gain {0} Endurance Charges each second\"\n-1 \"Lose an Endurance Charge each second\"\n#|-1 \"Lose {0} Endurance Charges each second\" negate 1");
        assert_eq!(got, [
            m("Lose an Endurance Charge each second", false, Some(-1.0)),
            m("Gain # Endurance Charges each second", false, None),
            m("Gain an Endurance Charge each second", false, Some(1.0)),
            m("Lose # Endurance Charges each second", true, None),
        ]);
        // Plain lines by text, not range: "Lose" before "Recover", "1" before "#".
        let got = matchers("2\n1|# \"Recover {0}% of Mana\"\n#|-1 \"Lose {0}% of Mana\"");
        assert_eq!(got, [m("Lose #% of Mana", false, None), m("Recover #% of Mana", false, None)]);
        let got = matchers("2\n# \"{0} seconds\"\n1 \"{0} second\"");
        assert_eq!(got, [m("# second", false, None), m("# seconds", false, None)]);
    }

    #[test]
    fn fixed_text_variant_carries_its_value() {
        // "100|#" chance -> 100; sign dropped from "+#".
        let got = matchers("2\n1|99 \"{0:+d}% chance to Blind\"\n100|# \"Blind\"");
        assert_eq!(got, [m("#% chance to Blind", false, None), m("Blind", false, Some(100.0))]);
        // Both fixed: sorted by value, a negative bound counts, no negate.
        let got = matchers("2\n1|# \"Lucky\"\n#|-1 \"Unlucky\" negate 1");
        assert_eq!(got, [m("Unlucky", false, Some(-1.0)), m("Lucky", false, Some(1.0))]);
    }

    #[test]
    fn fixed_value_is_converted_to_display_units() {
        let got = matchers("2\n1000 \"once every second\" milliseconds_to_seconds 1\n# \"once every {0} seconds\" milliseconds_to_seconds 1");
        assert_eq!(got, [m("once every # seconds", false, None), m("once every second", false, Some(1.0))]);
    }

    #[test]
    fn keeps_every_variant_but_sign_duplicates() {
        let strings = |src: &str| {
            let b = parse(&format!("description\n2 a b\n{src}")).unwrap().remove(0);
            let mut s = block_matchers(b.lang("English").unwrap(), None).into_iter().map(|m| m.string).collect::<Vec<_>>();
            s.sort();
            s
        };
        // Lines needing the other stat at 0 are kept (no mod check).
        assert_eq!(strings("3\n# 0 \"Adds {0} minimum\"\n0 # \"Adds {1} maximum\"\n# # \"Adds {0} to {1}\""), ["Adds # maximum", "Adds # minimum", "Adds # to #"]);
        // An explicit minus is just the value's sign: a duplicate.
        assert_eq!(strings("2\n1|# 0 \"+{0} to Evasion\"\n#|-1 0 \"-{0} to Evasion\" negate 1"), ["# to Evasion"]);
    }

    #[test]
    fn fixed_value_from_the_stat_it_fixes() {
        let values = |src: &str| {
            let b = parse(&format!("description\n2 a b\n{src}")).unwrap().remove(0);
            block_matchers(b.lang("English").unwrap(), None).into_iter().map(|m| (m.string, m.value)).collect::<Vec<_>>()
        };
        // First stat fixed to 0, the second's 100|# is the value.
        assert!(values("2\n0 1|99 \"{1}% chance to Return\"\n0 100|# \"Return\"").contains(&("Return".into(), Some(100.0))));
        // Only zero and !0: no single value.
        assert!(values("2\n!0 # \"Keystone\"\n0 !0 \"Starts at zero\"").contains(&("Starts at zero".into(), None)));
        // Only a zero: 0.
        assert!(values("2\n# 0 \"Splash\"\n# 1|# \"Splash, with {1}% AoE\"").contains(&("Splash".into(), Some(0.0))));
        // Converted by a sibling's flag for that stat.
        assert!(values("2\n1000 0 \"every second\"\n# 0 \"every {0} seconds\" milliseconds_to_seconds 1").contains(&("every second".into(), Some(1.0))));
        assert!(values("2\n1|999 0 \"{0}% chance\" divide_by_ten_1dp_if_required 1\n1000|# 0 \"always\"").contains(&("always".into(), Some(100.0))));
    }

    #[test]
    fn overrides_keep_value_and_order_lines() {
        let b = parse("description\n2 a b\n3\n1|# 0 \"{0}% increased\"\n#|-1 0 \"{0}% reduced\" negate 1\n1|# !0 \"{0}% increased. -1% when used\"").unwrap().remove(0);
        let lines = b.lang("English").unwrap();
        let got = |ov: &Override| block_matchers(lines, Some(ov)).into_iter().map(|m| (m.string, m.negate, m.value)).collect::<Vec<_>>();
        // keep: only lines 0 and 1.
        let ov = Override { keep: Some(vec![0, 1]), ..Default::default() };
        assert_eq!(got(&ov), [m("#% increased", false, None), m("#% reduced", true, None)]);
        // value: a hand-set value replaces negate; order: by line index.
        let ov = Override { keep: Some(vec![0, 1]), value: [(1, Some(-100.0))].into(), order: Some(vec![0, 1]), ..Default::default() };
        assert_eq!(got(&ov), [m("#% increased", false, None), m("#% reduced", false, Some(-100.0))]);
    }

    #[test]
    fn override_file_picks_that_wording() {
        let mut d = Descriptions { blocks: vec![], block_file: vec![], files: HashMap::new() };
        d.add("stat_descriptions", "description\n1 a\n1\n# \"#% Flask Charges gained\"").unwrap();
        d.add("heist_equipment_stat_descriptions", "description\n1 a\n1\n# \"Players have #% Flask Charges gained\"").unwrap();
        let c = Collected { blocks: vec![1, 0], mods: vec![(0, 1), (1, 0)] };
        assert_eq!(c.chosen_block(&d, None), 0); // last mod's
        let ov = Override { file: Some("heist_equipment_stat_descriptions".into()), ..Default::default() };
        assert_eq!(c.chosen_block(&d, Some(&ov)), 1);
    }

    #[test]
    fn lone_flag_line_has_no_value() {
        assert_eq!(matchers("1\n1|# \"Can't use Mana Flasks\""), [m("Can't use Mana Flasks", false, None)]);
    }

    #[test]
    fn matcher_json_matches_upstream_shape() {
        let json = |m: Matcher| serde_json::to_string(&m).unwrap();
        assert_eq!(json(Matcher { string: "a".into(), negate: false, value: Some(100.0) }), r#"{"string":"a","value":100}"#);
        assert_eq!(json(Matcher { string: "a".into(), negate: false, value: Some(1.5) }), r#"{"string":"a","value":1.5}"#);
        assert_eq!(json(Matcher { string: "a".into(), negate: true, value: None }), r#"{"string":"a","negate":true}"#);
    }

    #[test]
    fn lookup_follows_includes() {
        let mut d = Descriptions { blocks: vec![], block_file: vec![], files: HashMap::new() };
        d.add("stat_descriptions", "description\n1 a\n1\n# \"base {0}\"\ndescription\n1 b\n1\n# \"base b\"").unwrap();
        d.add("map_stat_descriptions", "include \"Metadata/StatDescriptions/stat_descriptions.csd\"\ndescription\n1 a\n1\n# \"map {0}\"").unwrap();
        let text = |i: usize| d.blocks[i].lang("English").unwrap()[0].text.clone();
        assert_eq!(text(d.lookup("map_stat_descriptions", "a").unwrap()), "map {0}");
        assert_eq!(text(d.lookup("map_stat_descriptions", "b").unwrap()), "base b");
        assert_eq!(d.lookup("map_stat_descriptions", "c"), None);
        // A stat defined twice in a file: the later block wins (3.29
        // offering_spells_effect_+% "Your Offerings have" vs "effect of Offerings").
        let mut d = Descriptions { blocks: vec![], block_file: vec![], files: HashMap::new() };
        d.add("graft_stat_descriptions", "description\n1 a\n1\n# \"first\"\ndescription\n1 a\n1\n# \"second\"").unwrap();
        let block = d.lookup("graft_stat_descriptions", "a").unwrap();
        assert_eq!(d.blocks[block].lang("English").unwrap()[0].text, "second");
    }

    #[test]
    fn last_mod_picks_the_wording() {
        let mut d = Descriptions { blocks: vec![], block_file: vec![], files: HashMap::new() };
        d.add("stat_descriptions", "description\n1 a\n1\n# \"{0}% more in Area\"").unwrap();
        d.add("map_stat_descriptions", "include \"stat_descriptions.csd\"").unwrap();
        d.add("atlas_stat_descriptions", "description\n1 a\n1\n# \"{0}% more in your Maps\"").unwrap();
        // Idols read atlas_relic, which includes atlas.
        d.add("atlas_relic_stat_descriptions", "include \"atlas_stat_descriptions.csd\"").unwrap();
        let m = |domain, weight| Mod {
            id: String::new(),
            domain: Some(domain),
            generation_type: Some(1),
            spawn_weights: vec![weight],
            stats_key1: Some(0),
            stats_key2: None, stats_key3: None, stats_key4: None, stats_key5: None, stats_key6: None,
        };
        let stats = [Stat { id: "a".into() }];
        let text = |mods: &[Mod]| {
            let c = collect(mods, &stats, &d);
            let c = c.values().next().unwrap();
            d.blocks[c.block()].lang("English").unwrap()[0].text.clone()
        };
        // AREA (map -> stat_descriptions) then IDOL (atlas): atlas wording.
        assert_eq!(text(&[m(5, 100), m(37, 100)]), "{0}% more in your Maps");
        assert_eq!(text(&[m(37, 100), m(5, 100)]), "{0}% more in Area");
        // A mod that can't spawn (and isn't unique) is not read, nor is a
        // domain upstream leaves out (ATLAS passives, 11).
        assert_eq!(text(&[m(5, 100), m(37, 0)]), "{0}% more in Area");
        assert_eq!(text(&[m(5, 100), m(11, 100)]), "{0}% more in Area");
    }
}
