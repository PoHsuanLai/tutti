//! Readings of spec prose shared by the extractors that read spec text
//! (`midi_spec`, `sf2_spec`).

/// A spec's conformance words and the `kind` each gives, strongest first.
pub(super) type Words = [(&'static str, &'static str)];

/// The strongest of `words` in `sentence`, as `(kind, negated)`: earlier in
/// `words` is stronger. A word in quotes is named, not used, and does not
/// count; a word followed by `not` is negated.
pub(super) fn keyword(sentence: &str, words: &Words) -> Option<(&'static str, bool)> {
    let chars: Vec<(usize, char)> = sentence.char_indices().collect();
    let mut best: Option<(usize, &'static str, bool)> = None;
    let mut i = 0;
    while i < chars.len() {
        if !chars[i].1.is_ascii_alphabetic() {
            i += 1;
            continue;
        }
        let mut j = i;
        while j < chars.len() && chars[j].1.is_ascii_alphabetic() {
            j += 1;
        }
        let from = chars[i].0;
        let to = chars.get(j).map_or(sentence.len(), |c| c.0);
        let word = sentence[from..to].to_ascii_lowercase();
        if let Some(rank) = words.iter().position(|(w, _)| *w == word) {
            let quoted = i > 0
                && "'\"\u{2018}\u{201c}".contains(chars[i - 1].1)
                && chars
                    .get(j)
                    .is_some_and(|c| "'\"\u{2019}\u{201d}".contains(c.1));
            if !quoted {
                let tail = sentence[to..].trim_start();
                let negated = tail.len() >= 3
                    && tail.is_char_boundary(3)
                    && tail[..3].eq_ignore_ascii_case("not")
                    && !tail[3..].starts_with(|c: char| c.is_ascii_alphabetic());
                if best.is_none_or(|(r, ..)| rank < r) {
                    best = Some((rank, words[rank].1, negated));
                }
            }
        }
        i = j;
    }
    best.map(|(_, k, n)| (k, n))
}

/// A `0x`-prefixed hex token of at most `max_digits` digits.
pub(super) fn hex(token: &str, max_digits: usize) -> Option<u32> {
    let digits = token
        .strip_prefix("0x")
        .or_else(|| token.strip_prefix("0X"))?;
    if digits.is_empty()
        || digits.len() > max_digits
        || !digits.bytes().all(|b| b.is_ascii_hexdigit())
    {
        return None;
    }
    u32::from_str_radix(digits, 16).ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    const WORDS: [(&str, &str); 3] = [
        ("shall", "required"),
        ("should", "recommended"),
        ("may", "permitted"),
    ];

    #[test]
    fn the_strongest_word_decides_and_a_quoted_word_does_not_count() {
        assert_eq!(
            keyword("A host may stop, and it shall drain first.", &WORDS),
            Some(("required", false))
        );
        assert_eq!(keyword("The word \"shall\" is defined here.", &WORDS), None);
        assert_eq!(keyword("Nothing to do.", &WORDS), None);
    }

    #[test]
    fn a_word_followed_by_not_is_negated() {
        assert_eq!(
            keyword("A reader shall not repair it.", &WORDS),
            Some(("required", true))
        );
        assert_eq!(
            keyword("A reader shall nothing.", &WORDS),
            Some(("required", false))
        );
    }
}
