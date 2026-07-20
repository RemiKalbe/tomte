//! Word-level changed ranges inside a merge region (highlighting support).

use std::ops::Range;

use imara_diff::{Algorithm, Diff, InternedInput};

use super::SliceTokens;

const MAX_TOKENS: usize = 5000;

#[derive(Debug, Default)]
pub struct WordDiff {
    pub changed_a: Vec<Range<usize>>,
    pub changed_b: Vec<Range<usize>>,
}

fn is_word(c: char) -> bool {
    c.is_alphanumeric() || c == '_' || c == '.' || c == '-'
}

/// Maximal runs of word / non-word characters, with their byte offsets.
fn tokens(s: &str) -> (Vec<&str>, Vec<usize>) {
    let mut toks = Vec::new();
    let mut offs = Vec::new();
    let mut start = 0usize;
    let mut prev_word: Option<bool> = None;
    for (i, c) in s.char_indices() {
        let w = is_word(c);
        if let Some(pw) = prev_word
            && pw != w
        {
            toks.push(&s[start..i]);
            offs.push(start);
            start = i;
        }
        prev_word = Some(w);
    }
    if prev_word.is_some() {
        toks.push(&s[start..]);
        offs.push(start);
    }
    (toks, offs)
}

pub fn word_diff(a: &str, b: &str) -> WordDiff {
    let (ta, oa) = tokens(a);
    let (tb, ob) = tokens(b);
    if ta.len() > MAX_TOKENS || tb.len() > MAX_TOKENS {
        let mut whole = WordDiff::default();
        if !a.is_empty() {
            whole.changed_a.push(0..a.len());
        }
        if !b.is_empty() {
            whole.changed_b.push(0..b.len());
        }
        return whole;
    }
    let input = InternedInput::new(SliceTokens(&ta), SliceTokens(&tb));
    let diff = Diff::compute(Algorithm::Histogram, &input);
    let mut out = WordDiff::default();
    let byte_range =
        |toks: &[&str], offs: &[usize], r: std::ops::Range<u32>| -> Option<Range<usize>> {
            if r.is_empty() {
                return None;
            }
            let (s, e) = (r.start as usize, r.end as usize - 1);
            Some(offs[s]..offs[e] + toks[e].len())
        };
    for h in diff.hunks() {
        if let Some(r) = byte_range(&ta, &oa, h.before.clone()) {
            out.changed_a.push(r);
        }
        if let Some(r) = byte_range(&tb, &ob, h.after.clone()) {
            out.changed_b.push(r);
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn equal_strings_have_no_changes() {
        let d = word_diff("alias g=git\n", "alias g=git\n");
        assert!(d.changed_a.is_empty() && d.changed_b.is_empty());
    }

    #[test]
    fn single_word_change_is_isolated() {
        let a = "profile = \"work\"\n";
        let b = "profile = \"personal\"\n";
        let d = word_diff(a, b);
        assert_eq!(d.changed_a.len(), 1);
        assert_eq!(&a[d.changed_a[0].clone()], "work");
        assert_eq!(&b[d.changed_b[0].clone()], "personal");
    }

    #[test]
    fn insertion_only_marks_b() {
        let a = "a c\n";
        let b = "a b c\n";
        let d = word_diff(a, b);
        assert!(d.changed_a.is_empty());
        assert_eq!(d.changed_b.len(), 1);
        // exact alignment (" b" vs "b ") is the diff algorithm's choice;
        // assert the inserted word is captured either way
        assert_eq!(b[d.changed_b[0].clone()].trim(), "b");
    }
}
