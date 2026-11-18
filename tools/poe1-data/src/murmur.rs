//! The trade site's stat id: `stat_<n>` where n is murmur2 over the
//! concatenated murmur2 hashes of a description block's stat ids.
//! Ported from Exiled Exchange 2's dataParser (stores/helpers/hash_computer.py).

/// MurmurHash2, 32-bit.
pub fn murmur2(bytes: &[u8], seed: u32) -> u32 {
    const M: u32 = 0x5bd1_e995;
    let mut h = seed ^ bytes.len() as u32;
    let mut chunks = bytes.chunks_exact(4);
    for c in &mut chunks {
        let mut k = u32::from_le_bytes([c[0], c[1], c[2], c[3]]);
        k = k.wrapping_mul(M);
        k ^= k >> 24;
        k = k.wrapping_mul(M);
        h = h.wrapping_mul(M) ^ k;
    }
    let rest = chunks.remainder();
    if !rest.is_empty() {
        for (i, &b) in rest.iter().enumerate() {
            h ^= (b as u32) << (8 * i);
        }
        h = h.wrapping_mul(M);
    }
    h ^= h >> 13;
    h = h.wrapping_mul(M);
    h ^ (h >> 15)
}

pub fn trade_stat_hash<S: AsRef<str>>(stat_ids: &[S]) -> u32 {
    let bytes: Vec<u8> = stat_ids
        .iter()
        .flat_map(|id| murmur2(id.as_ref().as_bytes(), 0xC58F_1A7B).to_le_bytes())
        .collect();
    murmur2(&bytes, 0x0231_2233)
}

#[cfg(test)]
mod tests {
    use super::*;

    // Ids as the trade site lists them (explicit.stat_762600725 is
    // "# Life gained when you Block").
    #[test]
    fn matches_trade_site_ids() {
        assert_eq!(trade_stat_hash(&["life_gained_on_block"]), 762600725);
        // Two-stat block: "Adds # to # Fire Damage" (explicit.stat_1573130764).
        assert_eq!(
            trade_stat_hash(&["attack_minimum_added_fire_damage", "attack_maximum_added_fire_damage"]),
            1573130764
        );
    }

    #[test]
    fn presence_stat_appends_the_condition() {
        // implicit.stat_2960338345: "While a Pinnacle Atlas Boss is in your
        // Presence, Attacks have #% chance to cause Bleeding".
        let ids = ["bleed_on_hit_with_attacks_%", "global_bleed_on_hit", "cannot_cause_bleeding", "local_influence_mod_requires_celestial_boss_presence"];
        assert_eq!(trade_stat_hash(&ids), 2960338345);
    }

    #[test]
    fn murmur2_every_tail_length() {
        // 0..=3 trailing bytes after the 4-byte blocks (seed 0).
        let cases: [(&[u8], u32); 6] = [
            (b"", 0),
            (b"a", 2456313694),
            (b"ab", 446775395),
            (b"abc", 324500635),
            (b"abcd", 646393889),
            (b"abcde", 1594468574),
        ];
        for (input, want) in cases {
            assert_eq!(murmur2(input, 0), want, "{:?}", std::str::from_utf8(input));
        }
    }
}
