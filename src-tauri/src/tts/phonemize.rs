//! Text → phoneme tokens for Kokoro v1.0.
//!
//! Pipeline: input English string → `misaki_rs::G2P` (pure Rust, no espeak)
//! → IPA character string → per-char vocab lookup → token id sequence
//! padded with `0` at start and end. Returns both the padded sequence (for
//! ONNX `input_ids`) and the unpadded count (for indexing the voicepack).
//!
//! The vocabulary is hardcoded from upstream `hexgrad/Kokoro-82M/config.json`.
//! Kokoro defines a 178-slot space with gaps; only 94 IPA characters and
//! prosody markers actually map to ids.

use std::collections::HashMap;

use misaki_rs::{language::Language, G2P};

use crate::error::{AppError, AppResult};

/// Maximum unpadded token count Kokoro accepts. The model was trained with
/// `(1, 512)` input and pads with a 0 at each end, leaving 510 real tokens.
pub const MAX_TOKENS: usize = 510;

/// Token id of the space character in the Kokoro vocab (see [`build_vocab`]);
/// used as the word-boundary marker when truncating over-long sequences.
const SPACE_ID: i64 = 16;

#[derive(Debug)]
pub struct PhonemeTokens {
    /// `[0, ...ids..., 0]` — the tensor that feeds `input_ids`.
    pub padded_ids: Vec<i64>,
    /// Unpadded token count. Index into the voicepack with this — Kokoro's
    /// style embeddings vary by utterance length.
    pub raw_count: usize,
    /// The raw IPA string for diagnostics — the *untruncated* G2P output, so
    /// it can describe more than `padded_ids` encodes when the 510-token cap
    /// hit. Read by the espeak OOV-fallback test to assert real G2P (not
    /// letter-spelling); unused in normal builds.
    #[cfg_attr(not(test), allow(dead_code))]
    pub phonemes: String,
}

pub struct Phonemizer {
    g2p: G2P,
    vocab: HashMap<char, i64>,
}

impl Phonemizer {
    pub fn new() -> Self {
        Self {
            g2p: G2P::new(Language::EnglishUS),
            vocab: build_vocab(),
        }
    }

    pub fn phonemize(&self, text: &str) -> AppResult<PhonemeTokens> {
        let (phonemes, _tokens) = match self.g2p.g2p(text) {
            Ok(res) => res,
            Err(first) => {
                // One unphonemizable token fails the whole sentence: espeak
                // returns no phonemes for pure symbol runs ("###", "->"),
                // misaki propagates that as Err, and the sentence's audio
                // would be dropped entirely. Retry without symbol-only
                // tokens before giving up.
                let cleaned: String = text
                    .split_whitespace()
                    .filter(|w| w.chars().any(char::is_alphanumeric))
                    .collect::<Vec<_>>()
                    .join(" ");
                if cleaned.is_empty() {
                    return Err(AppError::Tts(format!("g2p: {first}")));
                }
                tracing::debug!(
                    error = %first,
                    "g2p failed; retrying without symbol-only tokens"
                );
                self.g2p
                    .g2p(&cleaned)
                    .map_err(|e| AppError::Tts(format!("g2p: {e}")))?
            }
        };

        let mut ids: Vec<i64> = Vec::with_capacity(phonemes.chars().count() + 2);
        let mut unknown_count = 0usize;
        for c in phonemes.chars() {
            if let Some(&id) = self.vocab.get(&c) {
                ids.push(id);
            } else {
                unknown_count += 1;
            }
        }
        if unknown_count > 0 {
            tracing::debug!(
                unknown = unknown_count,
                "skipped phoneme chars not in Kokoro vocab"
            );
        }

        if ids.len() > MAX_TOKENS {
            // Cut at the last word boundary (space token) inside the limit so
            // the audio doesn't clip mid-word; a single boundary-free run
            // still gets the hard cut.
            let cut = ids[..MAX_TOKENS]
                .iter()
                .rposition(|&id| id == SPACE_ID)
                .filter(|&p| p > 0)
                .unwrap_or(MAX_TOKENS);
            tracing::warn!(
                len = ids.len(),
                cut,
                limit = MAX_TOKENS,
                "phoneme sequence exceeds Kokoro's 510-token limit; truncating"
            );
            ids.truncate(cut);
        }

        let raw_count = ids.len();
        let mut padded_ids = Vec::with_capacity(raw_count + 2);
        padded_ids.push(0);
        padded_ids.extend(ids);
        padded_ids.push(0);

        Ok(PhonemeTokens {
            padded_ids,
            raw_count,
            phonemes,
        })
    }
}

impl Default for Phonemizer {
    fn default() -> Self {
        Self::new()
    }
}

/// Kokoro v1.0 phoneme → token id mapping. Hardcoded from upstream
/// `hexgrad/Kokoro-82M/config.json`. ID 0 is reserved for padding.
fn build_vocab() -> HashMap<char, i64> {
    let pairs: &[(char, i64)] = &[
        (';', 1),
        (':', 2),
        (',', 3),
        ('.', 4),
        ('!', 5),
        ('?', 6),
        ('\u{2014}', 9),  // em dash —
        ('\u{2026}', 10), // ellipsis …
        ('"', 11),
        ('(', 12),
        (')', 13),
        ('\u{201C}', 14), // left double quote “
        ('\u{201D}', 15), // right double quote ”
        (' ', 16),
        ('\u{0303}', 17), // combining tilde
        ('ʣ', 18),
        ('ʥ', 19),
        ('ʦ', 20),
        ('ʨ', 21),
        ('ᵝ', 22),
        ('\u{AB67}', 23),
        ('A', 24),
        ('I', 25),
        ('O', 31),
        ('Q', 33),
        ('S', 35),
        ('T', 36),
        ('W', 39),
        ('Y', 41),
        ('ᵊ', 42),
        ('a', 43),
        ('b', 44),
        ('c', 45),
        ('d', 46),
        ('e', 47),
        ('f', 48),
        ('h', 50),
        ('i', 51),
        ('j', 52),
        ('k', 53),
        ('l', 54),
        ('m', 55),
        ('n', 56),
        ('o', 57),
        ('p', 58),
        ('q', 59),
        ('r', 60),
        ('s', 61),
        ('t', 62),
        ('u', 63),
        ('v', 64),
        ('w', 65),
        ('x', 66),
        ('y', 67),
        ('z', 68),
        ('ɑ', 69),
        ('ɐ', 70),
        ('ɒ', 71),
        ('æ', 72),
        ('β', 75),
        ('ɔ', 76),
        ('ɕ', 77),
        ('ç', 78),
        ('ɖ', 80),
        ('ð', 81),
        ('ʤ', 82),
        ('ə', 83),
        ('ɚ', 85),
        ('ɛ', 86),
        ('ɜ', 87),
        ('ɟ', 90),
        ('ɡ', 92),
        ('ɥ', 99),
        ('ɨ', 101),
        ('ɪ', 102),
        ('ʝ', 103),
        ('ɯ', 110),
        ('ɰ', 111),
        ('ŋ', 112),
        ('ɳ', 113),
        ('ɲ', 114),
        ('ɴ', 115),
        ('ø', 116),
        ('ɸ', 118),
        ('θ', 119),
        ('œ', 120),
        ('ɹ', 123),
        ('ɾ', 125),
        ('ɻ', 126),
        ('ʁ', 128),
        ('ɽ', 129),
        ('ʂ', 130),
        ('ʃ', 131),
        ('ʈ', 132),
        ('ʧ', 133),
        ('ʊ', 135),
        ('ʋ', 136),
        ('ʌ', 138),
        ('ɣ', 139),
        ('ɤ', 140),
        ('χ', 142),
        ('ʎ', 143),
        ('ʒ', 147),
        ('ʔ', 148),
        ('ˈ', 156),
        ('ˌ', 157),
        ('ː', 158),
        ('ʰ', 162),
        ('ʲ', 164),
        ('\u{2193}', 169), // ↓
        ('\u{2192}', 171), // →
        ('\u{2197}', 172), // ↗
        ('\u{2198}', 173), // ↘
        ('ᵻ', 177),
    ];
    let mut m = HashMap::with_capacity(pairs.len());
    for &(c, id) in pairs {
        m.insert(c, id);
    }
    m
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn vocab_has_no_zero_id() {
        // 0 is reserved for padding; no real phoneme should map to it.
        let v = build_vocab();
        assert!(v.values().all(|&id| id != 0));
    }

    #[test]
    fn vocab_contains_basic_letters() {
        let v = build_vocab();
        assert_eq!(v.get(&'a'), Some(&43));
        assert_eq!(v.get(&'h'), Some(&50));
        assert_eq!(v.get(&' '), Some(&16));
    }

    #[test]
    fn symbol_run_does_not_kill_the_sentence() {
        // A pure symbol token ("###") can make the espeak fallback return no
        // phonemes, failing the whole g2p call; the retry without symbol-only
        // tokens must still voice the real words.
        let p = Phonemizer::new();
        let toks = p
            .phonemize("see ### here")
            .expect("sentence should survive");
        assert!(toks.raw_count > 0);
    }

    #[test]
    fn truncation_cuts_at_word_boundary() {
        let p = Phonemizer::new();
        let long = "hello world ".repeat(80);
        let toks = p.phonemize(&long).unwrap();
        // For spaced text the boundary cut lands strictly below the cap
        // (a hard cut would sit exactly at MAX_TOKENS).
        assert!(toks.raw_count < MAX_TOKENS);
        assert!(toks.raw_count > 0);
        assert_eq!(toks.padded_ids.len(), toks.raw_count + 2);
        assert_ne!(toks.padded_ids[toks.raw_count], SPACE_ID);
    }

    #[test]
    fn phonemize_pads_with_zeros() {
        let p = Phonemizer::new();
        let toks = p.phonemize("hi").unwrap();
        assert_eq!(toks.padded_ids.first(), Some(&0));
        assert_eq!(toks.padded_ids.last(), Some(&0));
        assert_eq!(toks.padded_ids.len(), toks.raw_count + 2);
    }

    /// Run with: `cargo test --bin cimp -- --ignored --nocapture espeak_fallback`.
    /// espeak-ng (statically linked via misaki-rs's default features) handles
    /// out-of-vocabulary words. The check is that "eBook" doesn't degrade to
    /// misaki's letter-by-letter pattern (stress-marked individual letters).
    #[test]
    #[ignore]
    fn espeak_fallback_engages_on_oov() {
        // Point espeak-rs at the data dir copied by build.rs.
        let manifest = std::path::Path::new(env!("CARGO_MANIFEST_DIR"));
        let profile = if cfg!(debug_assertions) {
            "debug"
        } else {
            "release"
        };
        let data = manifest.join("target").join(profile).join("espeak-ng-data");
        assert!(
            data.is_dir(),
            "espeak-ng-data not at {} — run `cargo build --features espeak` first",
            data.display()
        );
        std::env::set_var("PIPER_ESPEAKNG_DATA_DIRECTORY", &data);

        let p = Phonemizer::new();
        let toks = p.phonemize("eBook").unwrap();
        eprintln!("eBook → {:?}", toks.phonemes);

        // Letter-by-letter fallback emits each letter's name with primary stress
        // (e.g. "i bˈi oˈʊ ʊ kˈeɪ"). Real espeak G2P produces a single compact
        // pronunciation. Count primary-stress markers as a proxy: ≥3 means
        // letter-spelling, <3 means a real word.
        let stresses = toks.phonemes.matches('ˈ').count();
        assert!(
            stresses < 3,
            "eBook still letter-spelled: {:?} ({} stress marks)",
            toks.phonemes,
            stresses
        );
    }
}
