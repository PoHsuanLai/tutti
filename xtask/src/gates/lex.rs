//! Rust source with its comments and the contents of its literals blanked.
//!
//! Every gate that looks for a token in code (`unsafe`, `extern "C" fn`, a
//! `pub use`) reads the blanked text, so prose and strings that say the word are
//! not code. Byte offsets and newlines are kept, so an offset in the blanked
//! text is the same offset in the source and a line number holds.

fn word(b: u8) -> bool {
    b.is_ascii_alphanumeric() || b == b'_'
}

/// `src` with `//` and nested `/* */` comments, and what sits between the
/// quotes of a string, raw string, byte string or char literal, replaced by
/// spaces (newlines kept). The quotes stay, so `""` still marks a literal.
pub(super) fn blank(src: &str) -> String {
    let b = src.as_bytes();
    let mut out = b.to_vec();
    let n = b.len();
    let wipe = |out: &mut [u8], from: usize, to: usize| {
        for c in &mut out[from..to] {
            if *c != b'\n' {
                *c = b' ';
            }
        }
    };
    let mut i = 0;
    while i < n {
        let c = b[i];
        if c == b'/' && b.get(i + 1) == Some(&b'/') {
            let end = src[i..].find('\n').map_or(n, |j| i + j);
            wipe(&mut out, i, end);
            i = end;
        } else if c == b'/' && b.get(i + 1) == Some(&b'*') {
            let mut depth = 1;
            let mut j = i + 2;
            while j < n && depth > 0 {
                if b[j] == b'/' && b.get(j + 1) == Some(&b'*') {
                    depth += 1;
                    j += 2;
                } else if b[j] == b'*' && b.get(j + 1) == Some(&b'/') {
                    depth -= 1;
                    j += 2;
                } else {
                    j += 1;
                }
            }
            wipe(&mut out, i, j);
            i = j;
        } else if word(c) {
            // A whole identifier, unless it opens a raw or byte literal.
            let start = i;
            let mut j = i;
            while j < n && word(b[j]) {
                j += 1;
            }
            let ident = &b[start..j];
            if (ident == b"r" || ident == b"br") && matches!(b.get(j), Some(b'"' | b'#')) {
                let hashes = b[j..].iter().take_while(|&&h| h == b'#').count();
                if b.get(j + hashes) == Some(&b'"') {
                    let body = j + hashes + 1;
                    let close: Vec<u8> = std::iter::once(b'"')
                        .chain(std::iter::repeat_n(b'#', hashes))
                        .collect();
                    let end = b[body..]
                        .windows(close.len())
                        .position(|w| w == close.as_slice())
                        .map_or(n, |p| body + p);
                    wipe(&mut out, body, end);
                    i = (end + close.len()).min(n);
                    continue;
                }
            }
            if ident == b"b" && b.get(j) == Some(&b'"') {
                i = j;
                continue;
            }
            if ident == b"b" && b.get(j) == Some(&b'\'') {
                i = j;
                continue;
            }
            i = j;
        } else if c == b'"' {
            let mut j = i + 1;
            while j < n && b[j] != b'"' {
                j += if b[j] == b'\\' { 2 } else { 1 };
            }
            let end = j.min(n);
            wipe(&mut out, i + 1, end);
            i = (end + 1).min(n);
        } else if c == b'\'' {
            match char_literal_end(b, i) {
                Some(end) => {
                    wipe(&mut out, i + 1, end - 1);
                    i = end;
                }
                None => i += 1,
            }
        } else {
            i += 1;
        }
    }
    String::from_utf8(out).expect("blanking keeps UTF-8: it only writes spaces over whole spans")
}

/// The index after a char literal opening at `at` (`'x'`, `'\n'`, `'\x41'`,
/// `'\u{1F600}'`), or `None` when the quote is a lifetime's.
fn char_literal_end(b: &[u8], at: usize) -> Option<usize> {
    let rest = &b[at + 1..];
    let first = *rest.first()?;
    if first == b'\\' {
        let esc = *rest.get(1)?;
        let len = match esc {
            b'x' => 4,
            b'u' => {
                let close = rest.iter().position(|&c| c == b'}')?;
                close + 1
            }
            _ => 2,
        };
        (rest.get(len) == Some(&b'\'')).then_some(at + 1 + len + 1)
    } else if first == b'\'' || first == b'\n' {
        None
    } else {
        // One UTF-8 scalar.
        let len = match first {
            0..=0x7f => 1,
            0xc0..=0xdf => 2,
            0xe0..=0xef => 3,
            _ => 4,
        };
        (rest.get(len) == Some(&b'\'')).then_some(at + 1 + len + 1)
    }
}

/// The line (1-based) of a byte offset.
pub(super) fn line_of(text: &str, at: usize) -> usize {
    text.as_bytes()[..at]
        .iter()
        .filter(|&&c| c == b'\n')
        .count()
        + 1
}

/// The offset of the first whole-word `needle` in `hay`.
pub(super) fn find_word(hay: &str, needle: &str) -> Option<usize> {
    find_word_from(hay, needle, 0)
}

pub(super) fn find_word_from(hay: &str, needle: &str, from: usize) -> Option<usize> {
    let b = hay.as_bytes();
    let mut at = from;
    while let Some(p) = hay.get(at..)?.find(needle) {
        let s = at + p;
        let e = s + needle.len();
        let before = s == 0 || !word(b[s - 1]);
        let after = e >= b.len() || !word(b[e]);
        if before && after {
            return Some(s);
        }
        at = s + 1;
    }
    None
}

/// The index of the closer matching the opener at `open` (in blanked text, so
/// no literal can hold a bracket), or `None` when it never closes.
pub(super) fn matching(blanked: &str, open: usize, o: u8, c: u8) -> Option<usize> {
    let mut depth = 0;
    for (i, &x) in blanked.as_bytes().iter().enumerate().skip(open) {
        if x == o {
            depth += 1;
        } else if x == c {
            depth -= 1;
            if depth == 0 {
                return Some(i);
            }
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn comments_and_literals_are_blanked_with_offsets_kept() {
        let src =
            "let a = 1; // unsafe\n/* unsafe /* x */ unsafe */ let s = \"unsafe\"; let c = 'u';\n";
        let b = blank(src);
        assert_eq!(b.len(), src.len());
        assert!(!b.contains("unsafe"));
        assert_eq!(b.matches('\n').count(), 2);
        assert!(b.contains("let s = \"      \";"));
        assert!(b.contains("let c = ' ';"));
    }

    #[test]
    fn raw_strings_byte_strings_and_lifetimes() {
        let b =
            blank("r#\"unsafe \" x\"# b\"unsafe\" br\"unsafe\" fn f<'a>(x: &'a u8) { unsafe {} }");
        assert_eq!(b.matches("unsafe").count(), 1);
        assert!(b.contains("<'a>(x: &'a u8)"));
    }

    #[test]
    fn escapes_do_not_end_a_literal_early() {
        let b = blank(r#"let s = "a\"unsafe"; let c = '\''; unsafe {}"#);
        assert_eq!(b.matches("unsafe").count(), 1);
    }

    #[test]
    fn whole_words_only() {
        assert_eq!(find_word("not_unsafe unsafe", "unsafe"), Some(11));
        assert_eq!(find_word("unsafe_code", "unsafe"), None);
        assert_eq!(line_of("a\nb\nc", 4), 3);
    }
}
