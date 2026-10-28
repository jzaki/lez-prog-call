//! Actors as `wallet program call` takes them, with one addition: a PDA's seed may be named by a
//! `PdaSeed` constant in the guest's source.

use crate::{hex, iface::Seed};

/// `actor` with a seed constant's name, in `pda:[<owner>/]<NAME>[@program][:sign]`, replaced by
/// its 0x hex. Every other actor, and a seed that isn't a constant's name, is passed through.
pub fn with_seed_hex(actor: &str, seeds: &[Seed]) -> String {
    let actor = actor.trim();
    let Some(body) = actor.strip_prefix("pda:") else {
        return actor.to_owned();
    };
    let (body, sign) = body
        .strip_suffix(":sign")
        .map_or((body, ""), |body| (body, ":sign"));
    let (body, at) = body
        .rsplit_once('@')
        .map_or((body, String::new()), |(body, program)| {
            (body, format!("@{program}"))
        });
    let (owner, seed) = body
        .rsplit_once('/')
        .map_or((String::new(), body), |(owner, seed)| {
            (format!("{owner}/"), seed)
        });
    let seed = seeds
        .iter()
        .find(|constant| constant.name == seed)
        .map_or_else(
            || seed.to_owned(),
            |constant| format!("0x{}", hex::encode(constant.bytes)),
        );
    format!("pda:{owner}{seed}{at}{sign}")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn seed_names_become_hex() {
        let seeds = [Seed {
            name: "PDA_SEED".to_owned(),
            bytes: [37; 32],
        }];
        let hex = format!("0x{}", "25".repeat(32));
        assert_eq!(with_seed_hex("pda:PDA_SEED", &seeds), format!("pda:{hex}"));
        assert_eq!(
            with_seed_hex("pda:Public/Abc/PDA_SEED@Def:sign", &seeds),
            format!("pda:Public/Abc/{hex}@Def:sign")
        );
        assert_eq!(with_seed_hex("pda:0x01", &seeds), "pda:0x01");
        assert_eq!(with_seed_hex("Public/Abc:sign", &seeds), "Public/Abc:sign");
    }
}
