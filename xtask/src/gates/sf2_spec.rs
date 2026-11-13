//! The SoundFont 2.04 facts read from the spec's `pdftotext -layout` text. The
//! text is never committed; the extraction is a pure function from that text
//! to [`Spec`], so a test feeds it small in-memory texts.
//!
//! The text is cleaned first (form feeds and the page footer lines are
//! dropped), then cut into sections at its numbered headings. A heading is a
//! line at column 0 whose number is the first child of the heading before it,
//! or a later sibling of it or of one of its ancestors (`2.01 NRPN` inside the
//! text of 9.6.2 is therefore no heading); a chapter title is an indented
//! `<n> <Title>` line. Lines with dot leaders (the table of contents) are
//! skipped. Every section number found is in [`Spec::headings`]; the gate
//! requires each hand-written row's section to be one of them.
//!
//! Nine families of rows are extracted. Every `section` is `SF2 §<n>`. `kind`
//! is read from the row's own text when it carries a conformance word
//! (`must` or `shall` is `required`, `should` is `recommended`, `may` is
//! `permitted`; the strongest wins) and is empty otherwise, except for the
//! INFO sub-chunks, where the spec's own `mandatory` is `required` and
//! `optional` is `permitted`.
//!
//! - `sf2.gen.<name>` (section 8.1.2): one per generator enumerator, facts
//!   `num`; from the summary table of 8.1.3, when the generator has a row
//!   there: `min`, `max` and `def` (each only when the cell is an integer),
//!   `inst_only=1` for the `+` mark, `nonrt=1` for the `@` mark.
//! - `sf2.mod.default.<n>` (section 8.4.`<n>`): one per default modulator,
//!   facts `src` (hex), `type`, `p`, `d`, `cc`, `index` (the source's bit
//!   fields), `amount`, `amtsrc` (hex), `transform`.
//! - `sf2.mod.src.<name>` (8.2.1, facts `index`), `sf2.mod.srctype.<name>`
//!   (8.2.4, facts `value`), `sf2.mod.transform.<name>` (8.3, facts `value`):
//!   the entries of the enumeration tables; `<name>` is the entry's name in
//!   camel case.
//! - `sf2.chunk.pdta.<ck>` (7.2 to 7.10): one per pdta sub-chunk, facts
//!   `recsize` (the record size in bytes) and, when the text gives it,
//!   `minrecs`.
//! - `sf2.chunk.info.<ck>` (5.1 to 5.11): one per INFO sub-chunk, facts
//!   `size` (a fixed byte size) or `maxlen` (the string's byte limit) when
//!   the text gives one.
//! - `sf2.sampletype.<name>` (4.5): one per `sfSampleType` enumerator, fact
//!   `value`.
//!
//! The extraction never returns half a set: generator numbers that are not
//! 0, 1, 2, .. in order, a summary row with no matching generator, a default
//! modulator or INFO or pdta sub-chunk section with a field missing, a
//! missing family, or two items with one id are errors.

use std::collections::{BTreeMap, BTreeSet};

/// The directory variable that points at the spec text.
pub(super) const ENV: &str = "TUTTI_SF2_SPEC";
/// The spec's file name starts with this (a `.txt` or a `.pdf`).
pub(super) const SOURCE: &str = "sfspec24";
/// The id prefixes the extraction owns; every other `sf2.` row is hand written.
const OWNED: [&str; 6] = [
    "sf2.gen.",
    "sf2.mod.default.",
    "sf2.mod.src.",
    "sf2.mod.srctype.",
    "sf2.mod.transform.",
    "sf2.sampletype.",
];
const OWNED_CHUNK: [&str; 2] = ["sf2.chunk.pdta.", "sf2.chunk.info."];

/// Whether the extraction owns `id` (its row is compared to the text).
pub(super) fn owns(id: &str) -> bool {
    OWNED.iter().chain(&OWNED_CHUNK).any(|p| id.starts_with(p))
}

/// The `section` column for the section number `n`.
pub(super) fn section_label(n: &str) -> String {
    format!("SF2 §{n}")
}

/// The section number of a `section` column, when it is one.
pub(super) fn section_number(column: &str) -> Option<&str> {
    column.strip_prefix("SF2 §")
}

/// One extracted spec item. `text` is the passage behind it (for the person
/// writing its paraphrase); it is never committed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct Item {
    pub(super) id: String,
    pub(super) kind: &'static str,
    pub(super) section: String,
    pub(super) facts: String,
    pub(super) text: String,
}

/// What the text gives.
#[derive(Debug, Default)]
pub(super) struct Spec {
    pub(super) items: Vec<Item>,
    /// Every section number the text has (`7.2`, `8.4.10`, and chapters).
    pub(super) headings: BTreeSet<String>,
}

/// One numbered section: its number, title and the lines up to the next.
struct Section {
    label: String,
    title: String,
    lines: Vec<String>,
}

fn clean(text: &str) -> Vec<String> {
    text.replace('\x0c', "")
        .lines()
        .filter(|l| !l.contains("Technical Specification - Page"))
        .map(|l| l.trim_end().to_owned())
        .collect()
}

fn number(s: &str) -> Option<Vec<u32>> {
    s.split('.')
        .map(|p| {
            if !p.is_empty() && p.bytes().all(|b| b.is_ascii_digit()) {
                p.parse().ok()
            } else {
                None
            }
        })
        .collect()
}

/// Whether `n` may follow the heading `last`: its first child, a later
/// sibling of it or of an ancestor, or the first child of the next chapter.
fn follows(last: &[u32], n: &[u32]) -> bool {
    if last.is_empty() {
        return true;
    }
    if n.len() == last.len() + 1 && n[..last.len()] == *last && n[last.len()] == 1 {
        return true;
    }
    if n.len() == 2 && n[0] == last[0] + 1 && n[1] == 1 {
        return true;
    }
    (1..=last.len()).any(|k| n.len() == k && n[..k - 1] == last[..k - 1] && n[k - 1] > last[k - 1])
}

fn title_ok(title: &str) -> bool {
    title.chars().next().is_some_and(|c| c.is_ascii_uppercase())
        && !title.contains("   ")
        && title.len() <= 120
}

fn sections(lines: &[String]) -> Vec<Section> {
    let mut out: Vec<Section> = Vec::new();
    let mut last: Vec<u32> = Vec::new();
    for l in lines {
        let t = l.trim();
        if t.contains(".....") {
            continue;
        }
        let indent = l.len() - l.trim_start().len();
        let heading = t.split_once(' ').and_then(|(num, title)| {
            let n = number(num)?;
            let title = title.trim();
            let ok = title_ok(title)
                && if n.len() == 1 {
                    indent >= 20 && (last.is_empty() || n[0] > last[0])
                } else {
                    indent == 0 && follows(&last, &n)
                };
            ok.then(|| (n, title.to_owned()))
        });
        match heading {
            Some((n, title)) => {
                out.push(Section {
                    label: n.iter().map(u32::to_string).collect::<Vec<_>>().join("."),
                    title,
                    lines: Vec::new(),
                });
                last = n;
            }
            None => {
                if let Some(s) = out.last_mut() {
                    s.lines.push(l.clone());
                }
            }
        }
    }
    out
}

/// The strongest conformance word in `text`.
fn kind_of(text: &str) -> &'static str {
    let words: BTreeSet<String> = text
        .split(|c: char| !c.is_ascii_alphabetic())
        .map(str::to_ascii_lowercase)
        .collect();
    if words.contains("must") || words.contains("shall") {
        "required"
    } else if words.contains("should") {
        "recommended"
    } else if words.contains("may") {
        "permitted"
    } else {
        ""
    }
}

fn int(s: &str) -> Option<i64> {
    let s = s.trim();
    let digits = s.strip_prefix(['+', '-']).unwrap_or(s);
    if digits.is_empty() || !digits.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    s.strip_prefix('+').unwrap_or(s).parse().ok()
}

fn hex(s: &str) -> Option<i64> {
    i64::from_str_radix(s.trim().strip_prefix("0x")?, 16).ok()
}

/// A cardinal in digits (commas allowed) or in words up to ninety-nine.
fn number_word(w: &str) -> Option<i64> {
    const ONES: [&str; 20] = [
        "zero",
        "one",
        "two",
        "three",
        "four",
        "five",
        "six",
        "seven",
        "eight",
        "nine",
        "ten",
        "eleven",
        "twelve",
        "thirteen",
        "fourteen",
        "fifteen",
        "sixteen",
        "seventeen",
        "eighteen",
        "nineteen",
    ];
    const TENS: [&str; 8] = [
        "twenty", "thirty", "forty", "fifty", "sixty", "seventy", "eighty", "ninety",
    ];
    let w = w.to_ascii_lowercase();
    if let Some(n) = int(&w.replace(',', "")) {
        return Some(n);
    }
    let one = |s: &str| ONES.iter().position(|o| *o == s).map(|p| p as i64);
    match w.split_once('-') {
        Some((t, o)) => {
            let t = TENS.iter().position(|x| *x == t)? as i64 * 10 + 20;
            Some(t + one(o).filter(|n| (1..10).contains(n))?)
        }
        None => one(&w).or_else(|| {
            TENS.iter()
                .position(|x| *x == w)
                .map(|p| p as i64 * 10 + 20)
        }),
    }
}

/// The text of `lines` as one line of single-spaced words.
fn flat(lines: &[String]) -> String {
    lines
        .join(" ")
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
}

/// The number the words after `before` and up to the next word `after`
/// spell: `("multiple of ", "bytes")` in `a multiple of four bytes long`.
fn number_between(text: &str, before: &str, after: &str) -> Option<i64> {
    let rest = &text[text.find(before)? + before.len()..];
    let span = rest[..rest.find(after)?].trim();
    if span.contains(' ') {
        return None;
    }
    number_word(span)
}

fn facts(pairs: &[(&str, Option<i64>)]) -> String {
    pairs
        .iter()
        .filter_map(|(n, v)| v.map(|v| format!("{n}={v}")))
        .collect::<Vec<_>>()
        .join(" ")
}

/// One row of an enumeration table: `<n> <Name>   <description>`, with the
/// description's wrapped lines (indented) after it.
struct Entry {
    num: i64,
    name: String,
    text: String,
}

fn entries(lines: &[String]) -> Vec<Entry> {
    let mut out: Vec<Entry> = Vec::new();
    for l in lines {
        let start = l
            .bytes()
            .next()
            .is_some_and(|b| b.is_ascii_digit())
            .then(|| {
                let (num, rest) = l.split_once(' ')?;
                let num = int(num)?;
                let rest = rest.trim_start();
                let at = rest.find("  ")?;
                let name = &rest[..at];
                let name_ok = name.chars().next().is_some_and(|c| c.is_ascii_alphabetic())
                    && name
                        .chars()
                        .all(|c| c.is_ascii_alphanumeric() || c == ' ' || c == '-');
                name_ok.then(|| Entry {
                    num,
                    name: name.to_owned(),
                    text: rest[at..].trim().to_owned(),
                })
            })
            .flatten();
        match start {
            Some(e) => out.push(e),
            None if l.starts_with(' ') => {
                if let Some(e) = out.last_mut() {
                    e.text.push(' ');
                    e.text.push_str(l.trim());
                }
            }
            None => {}
        }
    }
    out
}

fn camel(name: &str) -> String {
    let mut out = String::new();
    for (i, w) in name
        .split(|c: char| !c.is_ascii_alphanumeric())
        .filter(|w| !w.is_empty())
        .enumerate()
    {
        let lower = w.to_ascii_lowercase();
        if i == 0 {
            out.push_str(&lower);
        } else {
            out.push_str(&lower[..1].to_ascii_uppercase());
            out.push_str(&lower[1..]);
        }
    }
    out
}

fn find<'a>(secs: &'a [Section], label: &str) -> Result<&'a Section, String> {
    secs.iter()
        .find(|s| s.label == label)
        .ok_or_else(|| format!("the SF2 text has no section {label}"))
}

/// The cells of a summary-table row: the number, the name with its marks,
/// then the eight columns after it.
fn table_row(line: &str) -> Option<Vec<String>> {
    let cells: Vec<String> = line
        .trim()
        .split("  ")
        .map(str::trim)
        .filter(|c| !c.is_empty())
        .map(str::to_owned)
        .collect();
    (cells.len() == 10 && int(&cells[0]).is_some()).then_some(cells)
}

fn generators(secs: &[Section]) -> Result<Vec<Item>, String> {
    let defs = find(secs, "8.1.2")?;
    let gens = entries(&defs.lines);
    for (i, g) in gens.iter().enumerate() {
        if g.num != i as i64 {
            return Err(format!(
                "the SF2 generator list has number {} where {i} is next",
                g.num
            ));
        }
    }
    if gens.is_empty() {
        return Err("the SF2 text has no generator list in 8.1.2".to_owned());
    }
    let summary = find(secs, "8.1.3")?;
    let mut rows: BTreeMap<i64, (String, Vec<String>)> = BTreeMap::new();
    for cells in summary.lines.iter().filter_map(|l| table_row(l)) {
        let num = int(&cells[0]).unwrap_or(-1);
        rows.insert(num, (cells[1].clone(), cells));
    }
    let mut items = Vec::new();
    for g in &gens {
        let mut pairs: Vec<(&str, Option<i64>)> = vec![("num", Some(g.num))];
        if let Some((marked, cells)) = rows.remove(&g.num) {
            let bare: String = marked
                .chars()
                .take_while(|c| c.is_ascii_alphanumeric())
                .collect();
            if bare != g.name {
                return Err(format!(
                    "the SF2 summary row {} is `{bare}`, the list's is `{}`",
                    g.num, g.name
                ));
            }
            let marks = &marked[bare.len()..];
            pairs.push(("min", int(&cells[4])));
            pairs.push(("max", int(&cells[6])));
            pairs.push(("def", int(&cells[8])));
            pairs.push(("inst_only", marks.contains('+').then_some(1)));
            pairs.push(("nonrt", marks.contains('@').then_some(1)));
        }
        items.push(Item {
            id: format!("sf2.gen.{}", g.name),
            kind: kind_of(&g.text),
            section: section_label("8.1.2"),
            facts: facts(&pairs),
            text: g.text.clone(),
        });
    }
    if let Some((n, (name, _))) = rows.iter().next() {
        return Err(format!(
            "the SF2 summary row {n} `{name}` has no generator in the list"
        ));
    }
    Ok(items)
}

fn enumeration(
    secs: &[Section],
    label: &str,
    prefix: &str,
    fact: &str,
) -> Result<Vec<Item>, String> {
    let sec = find(secs, label)?;
    let found = entries(&sec.lines);
    if found.is_empty() {
        return Err(format!("the SF2 section {label} has no table rows"));
    }
    Ok(found
        .into_iter()
        .map(|e| Item {
            id: format!("{prefix}{}", camel(&e.name)),
            kind: kind_of(&e.text),
            section: section_label(label),
            facts: facts(&[(fact, Some(e.num))]),
            text: e.text,
        })
        .collect())
}

fn default_modulators(secs: &[Section]) -> Result<Vec<Item>, String> {
    let mut items = Vec::new();
    for s in secs {
        let parts: Vec<&str> = s.label.split('.').collect();
        if parts.len() != 3 || parts[..2] != ["8", "4"] {
            continue;
        }
        let missing = |what: &str| format!("the SF2 section {} has no {what}", s.label);
        let value = |key: &str| {
            s.lines.iter().find_map(|l| {
                let (k, v) = l.trim().split_once('=')?;
                (k.trim() == key).then(|| v.trim().to_owned())
            })
        };
        let src = value("Source Enumeration").ok_or_else(|| missing("source enumeration"))?;
        let (src_hex, fields) = src
            .split_once('(')
            .ok_or_else(|| missing("source bit fields"))?;
        let src_value = hex(src_hex.split_whitespace().next().unwrap_or(""))
            .ok_or_else(|| missing("hex source enumeration"))?;
        let squeezed: String = fields
            .trim_end_matches(')')
            .chars()
            .filter(|c| *c != ' ')
            .collect();
        let field = |name: &str| {
            squeezed.split(',').find_map(|kv| {
                let (k, v) = kv.split_once('=')?;
                if k.eq_ignore_ascii_case(name) {
                    int(v)
                } else {
                    None
                }
            })
        };
        let lead = |v: Option<String>| {
            v.as_deref()
                .and_then(|v| v.split_whitespace().next().map(str::to_owned))
        };
        let amount = lead(value("Amount")).and_then(|a| int(&a));
        let amt_src = lead(value("Amount Source Enumeration")).and_then(|a| hex(&a));
        let transform = lead(value("Transform Enumeration")).and_then(|a| int(&a));
        let (Some(amount), Some(amt_src), Some(transform)) = (amount, amt_src, transform) else {
            return Err(missing("amount, amount source or transform"));
        };
        let (ty, p, d, cc, index) = (
            field("type"),
            field("p"),
            field("d"),
            field("cc"),
            field("index"),
        );
        if [ty, p, d, cc, index].iter().any(Option::is_none) {
            return Err(missing("type, P, D, CC and index fields"));
        }
        let mut f = facts(&[
            ("type", ty),
            ("p", p),
            ("d", d),
            ("cc", cc),
            ("index", index),
            ("amount", Some(amount)),
            ("transform", Some(transform)),
        ]);
        f = format!("src=0x{src_value:04x} amtsrc=0x{amt_src:04x} {f}");
        items.push(Item {
            id: format!("sf2.mod.default.{}", parts[2]),
            kind: kind_of(&flat(&s.lines)),
            section: section_label(&s.label),
            facts: f,
            text: s.title.clone(),
        });
    }
    if items.is_empty() {
        return Err("the SF2 text has no default modulators in 8.4".to_owned());
    }
    Ok(items)
}

/// The sub-chunk a section titled `The <CK> Sub-chunk` describes.
fn chunk_name(title: &str) -> Option<String> {
    let name = title.strip_prefix("The ")?.strip_suffix(" Sub-chunk")?;
    (!name.contains(' ')).then(|| name.to_ascii_lowercase())
}

fn chunks(secs: &[Section]) -> Result<Vec<Item>, String> {
    let mut items = Vec::new();
    for s in secs {
        let Some(name) = chunk_name(&s.title) else {
            continue;
        };
        let text = flat(&s.lines);
        let missing = |what: &str| format!("the SF2 section {} has no {what}", s.label);
        let top = s.label.split('.').next().unwrap_or("");
        let (id, pairs, kind) = match (top, s.label.matches('.').count()) {
            ("7", 1) => {
                let size = number_between(&text, "multiple of ", " bytes")
                    .ok_or_else(|| missing("record size"))?;
                let min = number_between(&text, "minimum of ", " records");
                (
                    format!("sf2.chunk.pdta.{name}"),
                    facts(&[("recsize", Some(size)), ("minrecs", min)]),
                    "",
                )
            }
            ("5", 1) => {
                let kind = if text.contains("is a mandatory sub-chunk") {
                    "required"
                } else if text.contains("is an optional sub-chunk") {
                    "permitted"
                } else {
                    return Err(missing("mandatory or optional"));
                };
                let size = number_between(&text, "always ", " bytes in length");
                let max = number_between(&text, "string of ", " or fewer bytes");
                (
                    format!("sf2.chunk.info.{name}"),
                    facts(&[("size", size), ("maxlen", max)]),
                    kind,
                )
            }
            _ => continue,
        };
        items.push(Item {
            id,
            kind,
            section: section_label(&s.label),
            facts: pairs,
            text: s.title.clone(),
        });
    }
    if !items.iter().any(|i| i.id.starts_with("sf2.chunk.pdta.")) {
        return Err("the SF2 text has no pdta sub-chunk sections".to_owned());
    }
    if !items.iter().any(|i| i.id.starts_with("sf2.chunk.info.")) {
        return Err("the SF2 text has no INFO sub-chunk sections".to_owned());
    }
    Ok(items)
}

fn sample_types(secs: &[Section]) -> Result<Vec<Item>, String> {
    let sec = find(secs, "4.5")?;
    let start = sec
        .lines
        .iter()
        .position(|l| l.trim_start().starts_with("typedef enum"))
        .ok_or("the SF2 section 4.5 has no enumeration")?;
    let mut items = Vec::new();
    for l in &sec.lines[start + 1..] {
        let t = l.trim();
        if t.starts_with('}') {
            break;
        }
        let Some((name, value)) = t.trim_end_matches(',').split_once('=') else {
            continue;
        };
        let value = value.trim();
        let v = hex(value).or_else(|| int(value));
        let (name, Some(v)) = (name.trim(), v) else {
            return Err(format!("the SF2 enumeration line `{t}` has no value"));
        };
        items.push(Item {
            id: format!("sf2.sampletype.{name}"),
            kind: "",
            section: section_label("4.5"),
            facts: facts(&[("value", Some(v))]),
            text: String::new(),
        });
    }
    if items.is_empty() {
        return Err("the SF2 section 4.5 enumeration has no entries".to_owned());
    }
    Ok(items)
}

/// Everything the text gives, or why it cannot be read.
pub(super) fn extract(text: &str) -> Result<Spec, String> {
    let secs = sections(&clean(text));
    let mut items = generators(&secs)?;
    items.extend(default_modulators(&secs)?);
    items.extend(enumeration(&secs, "8.2.1", "sf2.mod.src.", "index")?);
    items.extend(enumeration(&secs, "8.2.4", "sf2.mod.srctype.", "value")?);
    items.extend(enumeration(&secs, "8.3", "sf2.mod.transform.", "value")?);
    items.extend(chunks(&secs)?);
    items.extend(sample_types(&secs)?);
    let mut seen = BTreeSet::new();
    for i in &items {
        if !seen.insert(i.id.as_str()) {
            return Err(format!("the SF2 text gives two items with id `{}`", i.id));
        }
    }
    let headings = secs.iter().map(|s| s.label.clone()).collect();
    Ok(Spec { items, headings })
}

/// A made-up text in the layout of the real one: a table of contents with
/// dot leaders, page footers, form feeds and indented chapter titles.
#[cfg(test)]
pub(super) const SAMPLE: &str = "\
Contents
1 Alpha ........ 3
4.5 Types ........ 4

                                 1 Alpha

1.1 First part
A frob must spin. Not a heading:
9.01 Something that wraps into a line.

                                 4 Layouts

4.1 Overview

4.5 Type definitions

typedef enum
  {
  oneKind = 1,
  hiKind = 0x8001,
  } Kinds;
\x0c  Gadget Technical Specification - Page 9 - Printed 1/1/1990

                                 5 Info

5.1 The abcd Sub-chunk

Chunk abcd is a mandatory sub-chunk; its size is always four bytes in length.

5.2 The efgh Sub-chunk

Chunk efgh is an optional sub-chunk holding a string of 256 or fewer bytes.
It may be skipped.

                                 7 Records

7.1 The tree

7.2 The AAAA Sub-chunk

Records here fill a size that is a multiple of thirty-eight bytes, with a minimum of two records.

7.3 The BBBB Sub-chunk

Records here are packed to a multiple of four bytes.

                                 8 Enums

8.1 Gens

8.1.1 Kinds

8.1.2 Generators Defined

0   alpha                    Takes the first value. It should be
                         small.

1   beta                     A second one.
\x0c  Gadget Technical Specification - Page 10 - Printed 1/1/1990
2   gamma                    The last one, which must come last.

8.1.3 Generator Summary

No.   Label                    Measure    Zero      Lo        Lo2       Hi        Hi2        Dflt      Note

0     alpha +                 smpls       0         0         None      *         *          0         None
1     beta                    cent        8.176     -12000    -10 oct   +500      5 oct      -1        Open
                                      Hz
2     gamma @                 cB          0         *         *         127       top        0-127     full

8.2 Sources

8.2.1 Palettes

0 No Controller                 Nothing is wired in. It should not turn anything off.
                            More words here.

2 Note-On Velocity              The velocity value.

8.2.4 Types

0 Linear                        Straight.

8.3 Transforms

2 Absolute Value                The magnitude.

8.4 Defaults

8.4.1 Velocity to Gain

Source Enumeration = 0x0345 (type=0, P=1, D=1, CC=1, index = 69)
Destination Enumeration = Something
Amount = -960
Amount Source Enumeration = 0x0 (No controller)
Transform Enumeration = 0 (Linear)

The amount may be changed.

8.4.2 Wheel to Pitch

Source Enumeration = 0x0111 (type=1, P=0, D=0, CC=0, index = 17)
Destination Enumeration = Pitch
Amount = 12700 Cents
Amount Source Enumeration = 0x0013 (type=0, D=0, P=0, C=0, index=19)
Transform Enumeration = 0 (Linear)
";

#[cfg(test)]
mod tests {
    use super::*;

    fn get<'a>(spec: &'a Spec, id: &str) -> &'a Item {
        spec.items
            .iter()
            .find(|i| i.id == id)
            .unwrap_or_else(|| panic!("no item {id}"))
    }

    fn row(spec: &Spec, id: &str) -> (&'static str, String, String) {
        let i = get(spec, id);
        (i.kind, i.section.clone(), i.facts.clone())
    }

    #[test]
    fn generators_take_their_number_and_the_summary_row_when_there_is_one() {
        let s = extract(SAMPLE).unwrap();
        assert_eq!(
            row(&s, "sf2.gen.alpha"),
            (
                "recommended",
                "SF2 §8.1.2".to_owned(),
                "num=0 min=0 def=0 inst_only=1".to_owned()
            )
        );
        // `+500` is an integer, `*` and `0-127` are not, `@` marks non-real-time.
        assert_eq!(
            row(&s, "sf2.gen.beta"),
            (
                "",
                "SF2 §8.1.2".to_owned(),
                "num=1 min=-12000 max=500 def=-1".to_owned()
            )
        );
        assert_eq!(
            row(&s, "sf2.gen.gamma"),
            (
                "required",
                "SF2 §8.1.2".to_owned(),
                "num=2 max=127 nonrt=1".to_owned()
            )
        );
    }

    #[test]
    fn generator_numbers_must_run_from_zero_without_a_gap() {
        let gap = SAMPLE.replace("2   gamma  ", "3   gamma  ");
        assert!(extract(&gap).unwrap_err().contains("number 3 where 2"));
    }

    #[test]
    fn a_summary_row_must_name_a_generator_of_the_list() {
        let other = SAMPLE.replace("0     alpha +", "0     omega +");
        assert!(extract(&other).unwrap_err().contains("`omega`"));
        let extra = SAMPLE.replace(
            "2     gamma @",
            "7     delta                   cB          0         *         *         127       top        0         full\n2     gamma @",
        );
        assert!(extract(&extra).unwrap_err().contains("row 7 `delta`"));
    }

    #[test]
    fn default_modulators_take_the_source_bit_fields_and_the_amounts() {
        let s = extract(SAMPLE).unwrap();
        assert_eq!(
            row(&s, "sf2.mod.default.1"),
            (
                "permitted",
                "SF2 §8.4.1".to_owned(),
                "src=0x0345 amtsrc=0x0000 type=0 p=1 d=1 cc=1 index=69 amount=-960 transform=0"
                    .to_owned()
            )
        );
        assert_eq!(
            get(&s, "sf2.mod.default.2").facts,
            "src=0x0111 amtsrc=0x0013 type=1 p=0 d=0 cc=0 index=17 amount=12700 transform=0"
        );
    }

    #[test]
    fn a_default_modulator_without_a_field_is_an_error() {
        let cut = SAMPLE.replace(
            "Transform Enumeration = 0 (Linear)\n\nThe amount",
            "\nThe amount",
        );
        assert!(
            extract(&cut)
                .unwrap_err()
                .contains("section 8.4.1 has no amount")
        );
    }

    #[test]
    fn enumeration_tables_name_entries_in_camel_case() {
        let s = extract(SAMPLE).unwrap();
        assert_eq!(
            row(&s, "sf2.mod.src.noController"),
            ("recommended", "SF2 §8.2.1".to_owned(), "index=0".to_owned())
        );
        assert_eq!(get(&s, "sf2.mod.src.noteOnVelocity").facts, "index=2");
        assert_eq!(get(&s, "sf2.mod.srctype.linear").facts, "value=0");
        assert_eq!(get(&s, "sf2.mod.transform.absoluteValue").facts, "value=2");
    }

    #[test]
    fn chunk_sections_give_sizes_and_the_mandatory_or_optional_kind() {
        let s = extract(SAMPLE).unwrap();
        assert_eq!(
            row(&s, "sf2.chunk.pdta.aaaa"),
            ("", "SF2 §7.2".to_owned(), "recsize=38 minrecs=2".to_owned())
        );
        assert_eq!(get(&s, "sf2.chunk.pdta.bbbb").facts, "recsize=4");
        assert_eq!(
            row(&s, "sf2.chunk.info.abcd"),
            ("required", "SF2 §5.1".to_owned(), "size=4".to_owned())
        );
        assert_eq!(
            row(&s, "sf2.chunk.info.efgh"),
            ("permitted", "SF2 §5.2".to_owned(), "maxlen=256".to_owned())
        );
    }

    #[test]
    fn an_info_section_that_is_neither_mandatory_nor_optional_is_an_error() {
        let bad = SAMPLE.replace("is an optional sub-chunk", "is a sub-chunk");
        assert!(extract(&bad).unwrap_err().contains("mandatory or optional"));
    }

    #[test]
    fn sample_types_take_decimal_and_hex_values() {
        let s = extract(SAMPLE).unwrap();
        assert_eq!(get(&s, "sf2.sampletype.oneKind").facts, "value=1");
        assert_eq!(get(&s, "sf2.sampletype.hiKind").facts, "value=32769");
    }

    #[test]
    fn headings_follow_the_outline_and_skip_the_contents_and_wrapped_lines() {
        let s = extract(SAMPLE).unwrap();
        for n in ["1", "1.1", "4.5", "5.2", "7.3", "8.1.3", "8.4.2"] {
            assert!(s.headings.contains(n), "{n}");
        }
        // The contents line and the `9.01` that wraps into the text.
        assert!(!s.headings.contains("9.1"));
        assert_eq!(
            s.headings.iter().filter(|h| h.starts_with("4.5")).count(),
            1
        );
    }

    #[test]
    fn page_footers_inside_a_table_do_not_end_it() {
        let s = extract(SAMPLE).unwrap();
        assert!(s.items.iter().any(|i| i.id == "sf2.gen.gamma"));
        assert!(get(&s, "sf2.gen.alpha").text.contains("should be small"));
    }

    #[test]
    fn a_missing_family_is_an_error_not_a_short_set() {
        let no_defaults = SAMPLE.split("8.4 Defaults").next().unwrap();
        assert!(
            extract(no_defaults)
                .unwrap_err()
                .contains("default modulators")
        );
        assert!(
            extract("1.1 Nothing\n")
                .unwrap_err()
                .contains("section 8.1.2")
        );
    }

    #[test]
    fn number_words_cover_the_sizes_the_spec_uses() {
        for (w, n) in [
            ("four", 4),
            ("ten", 10),
            ("twenty-two", 22),
            ("thirty-eight", 38),
            ("forty-six", 46),
            ("256", 256),
            ("65,536", 65536),
        ] {
            assert_eq!(number_word(w), Some(n), "{w}");
        }
        assert_eq!(number_word("twenty-ten"), None);
        assert_eq!(number_word("lots"), None);
    }

    #[test]
    fn owned_ids_are_the_extracted_families() {
        assert!(owns("sf2.gen.pan") && owns("sf2.chunk.pdta.phdr"));
        assert!(!owns("sf2.chunk.pdta") && !owns("sf2.synth.envelope"));
    }
}
