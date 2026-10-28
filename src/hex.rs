//! Hex, without a crate for it.

use std::fmt::Write as _;

use anyhow::{Context as _, ensure};

pub fn encode(bytes: impl AsRef<[u8]>) -> String {
    bytes.as_ref().iter().fold(String::new(), |mut text, byte| {
        let _ = write!(text, "{byte:02x}");
        text
    })
}

pub fn decode(text: &str) -> anyhow::Result<Vec<u8>> {
    ensure!(text.len().is_multiple_of(2), "odd number of hex digits");
    (0..text.len())
        .step_by(2)
        .map(|at| {
            let pair = text.get(at..at + 2).context("not ASCII")?;
            u8::from_str_radix(pair, 16).with_context(|| format!("{pair:?} isn't hex"))
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trips() {
        assert_eq!(encode([0x0a, 0xff]), "0aff");
        assert_eq!(decode("0aFF").unwrap(), [0x0a, 0xff]);
        assert!(decode("abc").is_err());
        assert!(decode("zz").is_err());
    }
}
