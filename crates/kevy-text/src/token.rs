//! Script-aware, dictionary-free tokenization.
//!
//! - Latin/ASCII alphanumeric runs → lowercased word tokens (length
//!   ≥ 2; single characters are noise).
//! - CJK (unified ideographs, kana, hangul syllables) → adjacent
//!   BIGRAMS; a lone CJK character emits a unigram so single-char
//!   texts remain findable.
//! - Tokens never cross a script boundary.

/// Pluggable tokenizer ([`tokenize`] is the only shipped impl).
pub trait Tokenizer {
    /// Produce tokens for `text` (UTF-8; invalid bytes are skipped).
    fn tokens(&self, text: &[u8]) -> Vec<Vec<u8>>;
}

/// The default dictionary-free tokenizer.
#[derive(Debug, Clone, Copy, Default)]
pub struct KevyTokenizer;

impl Tokenizer for KevyTokenizer {
    fn tokens(&self, text: &[u8]) -> Vec<Vec<u8>> {
        tokenize(text)
    }
}

fn is_cjk(c: char) -> bool {
    matches!(c,
        '\u{4E00}'..='\u{9FFF}'   // CJK unified ideographs
        | '\u{3400}'..='\u{4DBF}' // extension A
        | '\u{3040}'..='\u{309F}' // hiragana
        | '\u{30A0}'..='\u{30FF}' // katakana
        | '\u{AC00}'..='\u{D7AF}' // hangul syllables
    )
}

/// Tokenize per the default rules (see module doc).
pub fn tokenize(text: &[u8]) -> Vec<Vec<u8>> {
    let s = String::from_utf8_lossy(text);
    let mut out = Vec::new();
    let mut word = String::new();
    let mut prev_cjk: Option<char> = None;
    let mut cjk_run = 0usize;

    let flush_word = |word: &mut String, out: &mut Vec<Vec<u8>>| {
        if word.chars().count() >= 2 {
            out.push(word.to_lowercase().into_bytes());
        }
        word.clear();
    };

    for c in s.chars() {
        if is_cjk(c) {
            flush_word(&mut word, &mut out);
            if let Some(p) = prev_cjk {
                let mut bi = String::with_capacity(8);
                bi.push(p);
                bi.push(c);
                out.push(bi.into_bytes());
            }
            prev_cjk = Some(c);
            cjk_run += 1;
        } else {
            // end of a CJK run: a lone character still emits itself
            if cjk_run == 1
                && let Some(p) = prev_cjk
            {
                out.push(p.to_string().into_bytes());
            }
            prev_cjk = None;
            cjk_run = 0;
            if c.is_alphanumeric() {
                word.push(c);
            } else {
                flush_word(&mut word, &mut out);
            }
        }
    }
    if cjk_run == 1
        && let Some(p) = prev_cjk
    {
        out.push(p.to_string().into_bytes());
    }
    flush_word(&mut word, &mut out);
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn toks(s: &str) -> Vec<String> {
        tokenize(s.as_bytes())
            .into_iter()
            .map(|t| String::from_utf8(t).unwrap())
            .collect()
    }

    #[test]
    fn latin_words_lowercased_min_two() {
        assert_eq!(toks("Hello, Rust world! a I"), vec!["hello", "rust", "world"]);
        assert_eq!(toks("v2.7-alpha"), vec!["v2", "alpha"]);
    }

    #[test]
    fn cjk_bigrams_no_dictionary() {
        assert_eq!(toks("全文检索"), vec!["全文", "文检", "检索"]);
        assert_eq!(toks("猫"), vec!["猫"], "lone CJK char = unigram");
        assert_eq!(toks("ひらがな"), vec!["ひら", "らが", "がな"]);
        assert_eq!(toks("한국어"), vec!["한국", "국어"]);
    }

    #[test]
    fn mixed_scripts_never_cross() {
        assert_eq!(
            toks("Rust搜索engine引擎x"),
            vec!["rust", "搜索", "engine", "引擎"],
            "boundaries split; trailing single latin char dropped"
        );
        assert_eq!(toks("日本語abc漢字"), vec!["日本", "本語", "abc", "漢字"]);
    }

    #[test]
    fn junk_and_empty() {
        assert!(toks("").is_empty());
        assert!(toks("!!! ...").is_empty());
        let bad = tokenize(&[0xFF, 0xFE, b'o', b'k', b'a', b'y']);
        assert_eq!(bad, vec![b"okay".to_vec()]);
    }
}
