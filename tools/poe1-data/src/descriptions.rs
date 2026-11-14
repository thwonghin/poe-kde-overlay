//! Parser for the game's stat description files (Metadata/StatDescriptions,
//! written out as UTF-8 by export.mjs).
//!
//! ```text
//! description [name]
//! 	<n> <stat id> ...              stats this block describes, in order
//! 	<count>                         English variants follow
//! 		<cond per stat> "<text>" [flag [args]]...
//! 	lang "Russian"
//! 	<count>
//! 		...
//! ```
//!
//! A condition is `#` (any), `N`, `N|M`, `#|M`, `N|#` (inclusive ranges) or
//! `!N` (anything but N). Text uses `{i}` / `{i:+d}` for the i-th stat's value.

#[derive(Debug, Clone, Copy, PartialEq, Default)]
pub struct Cond {
    pub min: Option<i64>,
    pub max: Option<i64>,
    pub not: Option<i64>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Flag {
    pub name: String,
    pub args: Vec<String>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Variant {
    pub conds: Vec<Cond>,
    /// Skill files put one (`gem_quality`) after the conditions.
    pub qualifier: Option<String>,
    pub text: String,
    pub flags: Vec<Flag>,
}

impl Variant {
    pub fn has_flag(&self, name: &str) -> bool {
        self.flags.iter().any(|f| f.name == name)
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct Block {
    pub ids: Vec<String>,
    /// In file order; English first.
    pub langs: Vec<(String, Vec<Variant>)>,
}

impl Block {
    pub fn lang(&self, name: &str) -> Option<&[Variant]> {
        self.langs.iter().find(|(l, _)| l == name).map(|(_, v)| v.as_slice())
    }
}

fn parse_bound(s: &str) -> Result<Option<i64>, String> {
    if s == "#" {
        return Ok(None);
    }
    s.parse().map(Some).map_err(|_| format!("bad bound: {s}"))
}

fn parse_cond(tok: &str) -> Result<Cond, String> {
    if let Some(n) = tok.strip_prefix('!') {
        return Ok(Cond { not: parse_bound(n)?, ..Cond::default() });
    }
    // The game has typos like `1|99#` and `1|1|#`; read the first range.
    let mut parts = tok.split('|').map(|p| {
        let p = p.strip_suffix('#').filter(|d| d.ends_with(|c: char| c.is_ascii_digit())).unwrap_or(p);
        parse_bound(p)
    });
    let min = parts.next().ok_or("empty condition")??;
    let max = match parts.next() {
        Some(max) => max?,
        None => min,
    };
    Ok(Cond { min, max, not: None })
}

fn parse_variant(line: &str, stat_count: usize) -> Result<Variant, String> {
    let (q1, q2) = match (line.find('"'), line.rfind('"')) {
        (Some(a), Some(b)) if a < b => (a, b),
        _ => return Err(format!("no quoted text: {line}")),
    };
    let pre: Vec<&str> = line[..q1].split_whitespace().collect();
    if pre.len() < stat_count || pre.len() > stat_count + 1 {
        return Err(format!("expected {stat_count} conditions: {line}"));
    }
    let conds = pre[..stat_count].iter().map(|t| parse_cond(t)).collect::<Result<_, _>>()?;
    let qualifier = pre.get(stat_count).map(|s| s.to_string());

    // Flags take stat indexes (1-based; `display_indexable_support 2 1`
    // takes two), reminderstring takes a name, canonical_line takes nothing.
    let rest: Vec<&str> = line[q2 + 1..].split_whitespace().collect();
    let mut flags = Vec::new();
    let mut i = 0;
    while i < rest.len() {
        let name = rest[i].to_string();
        i += 1;
        let mut args = Vec::new();
        if name == "reminderstring" && i < rest.len() {
            args.push(rest[i].to_string());
            i += 1;
        }
        while i < rest.len() && rest[i].bytes().all(|b| b.is_ascii_digit()) {
            args.push(rest[i].to_string());
            i += 1;
        }
        flags.push(Flag { name, args });
    }
    Ok(Variant { conds, qualifier, text: line[q1 + 1..q2].to_string(), flags })
}

/// Non-blank lines, trimmed, with their 1-based line numbers for errors.
/// The game has blank lines inside blocks (3.29: between `lang "French"`
/// and its count), so blanks carry no meaning.
struct Lines<'a> {
    lines: Vec<(usize, &'a str)>,
    pos: usize,
}

impl<'a> Lines<'a> {
    fn peek(&self) -> Option<&'a str> {
        self.lines.get(self.pos).map(|&(_, l)| l)
    }
    fn next(&mut self) -> Result<&'a str, String> {
        let l = self.peek().ok_or("unexpected end of file")?;
        self.pos += 1;
        Ok(l)
    }
    fn line_no(&self) -> usize {
        self.lines.get(self.pos.saturating_sub(1)).map_or(0, |&(n, _)| n)
    }
}

pub fn parse(text: &str) -> Result<Vec<Block>, String> {
    let mut lines = Lines {
        lines: text.lines().enumerate().map(|(n, l)| (n + 1, l.trim())).filter(|(_, l)| !l.is_empty()).collect(),
        pos: 0,
    };
    let mut blocks = Vec::new();
    while let Some(line) = lines.peek() {
        lines.pos += 1;
        if line.starts_with("description") {
            let start = lines.line_no();
            blocks.push(parse_block(&mut lines).map_err(|e| format!("block at line {start}: {e} (line {})", lines.line_no()))?);
        }
    }
    Ok(blocks)
}

fn parse_block(lines: &mut Lines) -> Result<Block, String> {
    let head: Vec<&str> = lines.next()?.split_whitespace().collect();
    let ids: Vec<String> = head.iter().skip(1).map(|s| s.to_string()).collect();
    if head.first().and_then(|n| n.parse::<usize>().ok()) != Some(ids.len()) {
        return Err("stat count mismatch".into());
    }
    let mut langs = Vec::new();
    let mut lang = "English".to_string();
    loop {
        let count: usize = lines.next()?.parse().map_err(|_| "bad variant count")?;
        let mut variants = Vec::with_capacity(count);
        for _ in 0..count {
            variants.push(parse_variant(lines.next()?, ids.len())?);
        }
        langs.push((lang, variants));
        match lines.peek().and_then(|l| l.strip_prefix("lang ")) {
            Some(name) => {
                lang = name.trim_matches('"').to_string();
                lines.pos += 1;
            }
            None => break,
        }
    }
    Ok(Block { ids, langs })
}

/// The file a description file `include`s, by name without extension.
pub fn include_of(text: &str) -> Option<String> {
    let line = text.lines().find(|l| l.starts_with("include \""))?;
    let path = line.trim_start_matches("include ").trim_matches('"');
    let file = path.rsplit('/').next()?;
    Some(file.rsplit_once('.').map_or(file, |(stem, _)| stem).to_string())
}

/// Text the renderer compares item lines to: markup `[Key|Shown]` / `[Shown]`
/// keeps the shown text, `{i}` / `{i:+d}` / `{}` become `#`, `\n` breaks lines.
pub fn matcher_text(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut rest = text;
    while let Some(c) = rest.chars().next() {
        let closer = match c {
            '[' => Some(']'),
            '{' => Some('}'),
            _ => None,
        };
        if let Some(end) = closer.and_then(|cl| rest.find(cl)) {
            let inner = &rest[1..end];
            if c == '[' && !inner.contains('[') {
                out.push_str(inner.split_once('|').map_or(inner, |(_, shown)| shown));
                rest = &rest[end + 1..];
                continue;
            }
            let index = inner.split_once(':').map_or(inner, |(i, _)| i);
            if c == '{' && index.bytes().all(|b| b.is_ascii_digit()) {
                out.push('#');
                rest = &rest[end + 1..];
                continue;
            }
        }
        if let Some(after) = rest.strip_prefix("\\n") {
            out.push('\n');
            rest = after;
            continue;
        }
        out.push(c);
        rest = &rest[c.len_utf8()..];
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    const SAMPLE: &str = "description
\t1 life_gained_on_block
\t2
\t\t1|# \"{0} Life gained when you Block\"
\t\t#|-1 \"Lose {0} Life when you Block\" negate 1
\tlang \"Traditional Chinese\"
\t1
\t\t# \"格擋時獲得 {0} 生命\"

no_description some_hidden_stat

description
\t2 attack_minimum_added_fire_damage attack_maximum_added_fire_damage
\t1
\t\t# # \"Adds {0} to {1} Fire Damage to Attacks\" reminderstring ReminderTextX
";

    #[test]
    fn parses_blocks_languages_and_flags() {
        let blocks = parse(SAMPLE).unwrap();
        assert_eq!(blocks.len(), 2);
        let b = &blocks[0];
        assert_eq!(b.ids, ["life_gained_on_block"]);
        let en = b.lang("English").unwrap();
        assert_eq!(en[0].conds, [Cond { min: Some(1), max: None, not: None }]);
        assert_eq!(en[1].conds, [Cond { min: None, max: Some(-1), not: None }]);
        assert_eq!(en[1].flags, [Flag { name: "negate".into(), args: vec!["1".into()] }]);
        assert_eq!(b.lang("Traditional Chinese").unwrap()[0].text, "格擋時獲得 {0} 生命");
        let fire = &blocks[1].lang("English").unwrap()[0];
        assert_eq!(fire.conds.len(), 2);
        assert_eq!(fire.flags[0].args, ["ReminderTextX"]);
    }

    #[test]
    fn blank_lines_inside_a_block() {
        // Also trailing blank lines at the end of the file.
        let blocks = parse("description\n1 a\n1\n# \"en\"\nlang \"French\"\n\n1\n# \"fr\"\n\r\n\n").unwrap();
        assert_eq!(blocks[0].lang("French").unwrap()[0].text, "fr");
    }

    #[test]
    fn conditions_including_game_typos() {
        let c = |t| parse_cond(t).unwrap();
        assert_eq!(c("#"), Cond { min: None, max: None, not: None });
        assert_eq!(c("100"), Cond { min: Some(100), max: Some(100), not: None });
        assert_eq!(c("!0"), Cond { min: None, max: None, not: Some(0) });
        assert_eq!(c("1|99#"), Cond { min: Some(1), max: Some(99), not: None });
        assert_eq!(c("1|1|#"), Cond { min: Some(1), max: Some(1), not: None });
    }

    #[test]
    fn variant_qualifier_and_multi_arg_flags() {
        let v = parse_variant(r#"1|# gem_quality "{0:+d}% more Damage" canonical_line"#, 1).unwrap();
        assert_eq!(v.qualifier.as_deref(), Some("gem_quality"));
        assert_eq!(v.flags, [Flag { name: "canonical_line".into(), args: vec![] }]);
        let v = parse_variant(r#"# # "Supported by Level {0} {1}" display_indexable_support 2 1"#, 2).unwrap();
        assert_eq!(v.flags[0].args, ["2", "1"]);
    }

    #[test]
    fn include_names_ignore_extension() {
        assert_eq!(include_of("include \"Metadata/StatDescriptions/stat_descriptions.csd\"\n"), Some("stat_descriptions".into()));
        assert_eq!(include_of("description\n"), None);
    }

    #[test]
    fn matcher_text_strips_markup_and_values() {
        assert_eq!(matcher_text("{0:+d}% to [Blind] Enemies"), "#% to Blind Enemies");
        assert_eq!(matcher_text("Triggers Level {0} [PhysicalAegis::#|Physical Aegis]"), "Triggers Level # Physical Aegis");
        assert_eq!(matcher_text("Duration {} seconds\\nsecond line"), "Duration # seconds\nsecond line");
        // Not markup or a placeholder: left alone.
        assert_eq!(matcher_text("{word} [unclosed"), "{word} [unclosed");
    }
}
