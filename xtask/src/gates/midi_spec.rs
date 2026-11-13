//! The MIDI conformance surface of each document in [`DOCS`], read from the
//! spec's `pdftotext -layout` text. The text is never committed; the extraction is a pure function from
//! that text to [`Item`]s, so a test feeds it small in-memory texts.
//!
//! Each document has its own prefix (`m2-104`, `m2-116`), its own table of
//! rows beyond the keyword sentences ([`Tables`]) and its own heading quirks
//! ([`Doc`]). Three kinds of item:
//!
//! - `<doc>/<section>#<n>`: the `n`th sentence (1-based, in document order)
//!   under the deepest numbered heading `<section>` (lowercase: `7.4.2`,
//!   `d.2.1`) that carries a conformance word. Each spec's own table of
//!   conformance words (Table 2 in both M2-104 and M2-116, the same three
//!   rows) maps `shall` to `required`, `should` to `recommended`
//!   and `may` to `permitted`; the table has no negated forms, so `shall
//!   not`, `should not` and `may not` keep the kind of the bare word and
//!   carry the fact `neg=1`. A sentence with several words takes the
//!   strongest. A word in quotation marks names the word and is not a use of
//!   it, and neither is the first cell of a table row (the table that
//!   defines the words). Notes and examples are not set apart: the spec says
//!   the words are used solely for conformance.
//! - `m2-104/mt/<mt>`: one per Message Type of the allocation table
//!   ([`Tables::Messages`]), facts `mt` and `bytes`, the UMP size.
//! - `m2-104/msg/<mt>/<status>`: one per defined message of the message
//!   tables ([`Tables::Messages`]). `<mt>` is the Message Type in lowercase hex,
//!   `<status>` the status in lowercase hex without `0x` and without leading
//!   zeros; for Message Type 0xD it is `<bank>.<status>`, each two hex
//!   digits. Facts: `mt`, `status` (and `bank` for 0xD), `bytes`.
//! - `m2-116/hdr/<n>`: one per byte of the File Header table
//!   ([`Tables::FileHeader`]), facts `byte` (1-based) and `value`.
//!
//! The text is cleaned first: lines that repeat on at least half the pages
//! (running headers and footers) and a page's last line when it is only
//! digits are dropped, and pages are joined with no paragraph break, so a
//! sentence that crosses a page is one sentence. Everything before the first
//! numbered heading `1` that has no dot leader is the cover and the table of
//! contents, and is skipped. A heading is a line that is the next sibling or
//! first child of a heading already seen (`7.4.2` after `7.4.1`, `D.1` after
//! `Appendix D:`), so a wrapped line that starts with a number is not one. A
//! document with [`Doc::implicit_levels`] also takes a heading whose missing
//! ancestors would each be the first of their level (`6.2.1.1` straight after
//! `6.2`); the missing levels count as seen.
//!
//! The extraction never returns half a set: a message row whose Message Type
//! has no size, two items with one id, or a configured table that is not
//! found is an error.

use std::collections::{BTreeMap, BTreeSet};

/// One extracted spec item. `text` is the sentence behind a keyword row (for
/// the person writing its paraphrase) and is empty for a table row; it is
/// never committed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct Item {
    pub(super) id: String,
    pub(super) kind: &'static str,
    pub(super) section: String,
    pub(super) facts: String,
    pub(super) text: String,
}

/// What a document has besides its keyword sentences.
#[derive(Clone, Copy)]
pub(super) enum Tables {
    /// The Message Type allocation table and the tables of defined messages.
    Messages {
        /// The allocation table's number.
        mt_table: u32,
        /// `(table number, row shape)` of each table that defines messages.
        messages: &'static [(u32, Rows)],
    },
    /// The table that spells the file header, one byte per column.
    FileHeader { table: u32 },
}

/// One spec document the extractor reads.
pub(super) struct Doc {
    /// The id prefix: `m2-104`.
    pub(super) prefix: &'static str,
    /// The document's name in `section` columns: `M2-104`.
    pub(super) label: &'static str,
    /// The title and version in the inventory header.
    pub(super) title: &'static str,
    /// The inventory file under `testing/conformance/`.
    pub(super) file: &'static str,
    /// The spec's file name starts with this (a `.txt` or a `.pdf`).
    pub(super) source: &'static str,
    /// The side column's words in the inventory header.
    pub(super) sides: &'static str,
    /// Lines the inventory header adds before the side line.
    pub(super) note: &'static str,
    /// Take a heading whose missing ancestors are each the first of their
    /// level.
    implicit_levels: bool,
    pub(super) tables: Tables,
}

/// The documents, one inventory file each.
pub(super) const DOCS: &[Doc] = &[M2_104, M2_116];

pub(super) const M2_104: Doc = Doc {
    prefix: "m2-104",
    label: "M2-104",
    title: "MIDI 2.0 UMP and Protocol (M2-104 v1.1.2)",
    file: "midi-m2-104.txt",
    source: "M2-104-UM",
    sides: "sender, receiver or both",
    note: "",
    implicit_levels: false,
    tables: Tables::Messages {
        mt_table: 4,
        messages: &[(34, Rows::Formats), (16, Rows::FlexStatus)],
    },
};

pub(super) const M2_116: Doc = Doc {
    prefix: "m2-116",
    label: "M2-116",
    title: "MIDI Clip File (M2-116 v1.0)",
    file: "midi-m2-116.txt",
    source: "M2-116-U",
    sides: "writer, reader or both",
    note: "\
# M2-116 has its own conformance-word table (its Table 2), the same three
# rows as M2-104's, and no negated forms either. Section 6.2.1.1 follows 6.2
# with no 6.2.1 between; it keeps its own number as the section.
#   m2-116/hdr/<n>                the nth byte of the file header: facts
#                                 byte (1-based) and value
",
    implicit_levels: true,
    tables: Tables::FileHeader { table: 5 },
};

/// How a message table's lines read as rows.
#[derive(Clone, Copy)]
pub(super) enum Rows {
    /// A line with a Message Type, a group cell and a status.
    Formats,
    /// A line with a Flex Data status bank, a status and a name.
    FlexStatus,
}

/// The Message Type of the Flex Data messages, whose status is a bank and a
/// status.
const FLEX_MT: u32 = 0xD;

const WORDS: [(&str, &str); 3] = [
    ("shall", "required"),
    ("should", "recommended"),
    ("may", "permitted"),
];

/// One cleaned line, classified.
#[derive(Debug, PartialEq)]
enum Line {
    Blank,
    /// A heading and its label (`7.4.2`, `D`, `D.1`, as written).
    Heading(String),
    /// A table or figure caption: `(is_table, number)`.
    Caption(bool, u32),
    Text(String),
}

/// The pages of `text` with running furniture removed, as one line list.
fn clean(text: &str) -> Vec<String> {
    let pages: Vec<Vec<&str>> = text
        .split('\x0c')
        .map(|p| p.lines().collect::<Vec<_>>())
        .collect();
    let mut seen: BTreeMap<&str, usize> = BTreeMap::new();
    for page in &pages {
        let mut here = BTreeSet::new();
        for l in page {
            let t = l.trim();
            if !t.is_empty() {
                here.insert(t);
            }
        }
        for t in here {
            *seen.entry(t).or_default() += 1;
        }
    }
    let floor = (pages.len() / 2).max(3);
    let mut out = Vec::new();
    for page in &pages {
        let last = page.iter().rposition(|l| !l.trim().is_empty());
        let mut kept: Vec<String> = Vec::new();
        for (i, l) in page.iter().enumerate() {
            let t = l.trim();
            let page_number =
                Some(i) == last && !t.is_empty() && t.bytes().all(|b| b.is_ascii_digit());
            let furniture = seen.get(t).is_some_and(|&n| !t.is_empty() && n >= floor);
            if !page_number && !furniture {
                kept.push((*l).to_owned());
            }
        }
        while kept.first().is_some_and(|l| l.trim().is_empty()) {
            kept.remove(0);
        }
        while kept.last().is_some_and(|l| l.trim().is_empty()) {
            kept.pop();
        }
        out.extend(kept);
    }
    out
}

/// A heading's label parts: `(appendix, numbers)`.
type Label = (bool, Vec<u32>);

fn render(label: &Label) -> String {
    let (app, nums) = label;
    if !app {
        return nums
            .iter()
            .map(u32::to_string)
            .collect::<Vec<_>>()
            .join(".");
    }
    let letter = char::from(b'A' + (nums[0] - 1) as u8);
    let rest: Vec<String> = nums[1..].iter().map(u32::to_string).collect();
    if rest.is_empty() {
        letter.to_string()
    } else {
        format!("{letter}.{}", rest.join("."))
    }
}

/// A heading candidate: the label and nothing about whether it is expected.
fn parse_heading(line: &str) -> Option<Label> {
    let indent = line.len() - line.trim_start().len();
    let t = line.trim();
    if indent > 6 || t.contains(".....") || t.len() > 160 {
        return None;
    }
    if let Some(rest) = t.strip_prefix("Appendix ") {
        let mut c = rest.chars();
        let letter = c.next().filter(char::is_ascii_uppercase)?;
        let after = c.as_str().strip_prefix(':')?;
        after.trim().chars().next()?;
        return Some((true, vec![u32::from(letter as u8 - b'A') + 1]));
    }
    let (label, title) = t.split_once(char::is_whitespace)?;
    let title = title.trim_start();
    // A table row (`8   Balance   -`) has a gap inside what would be the title.
    if title.contains("   ")
        || !title
            .chars()
            .next()
            .is_some_and(|c| c.is_ascii_alphanumeric())
    {
        return None;
    }
    let mut parts = label.split('.');
    let first = parts.next()?;
    let (app, head) = match first.as_bytes() {
        [l] if l.is_ascii_uppercase() => (true, u32::from(l - b'A') + 1),
        b if !b.is_empty() && b.iter().all(u8::is_ascii_digit) => (false, first.parse().ok()?),
        _ => return None,
    };
    let mut nums = vec![head];
    for p in parts {
        if p.is_empty() || !p.bytes().all(|b| b.is_ascii_digit()) {
            return None;
        }
        nums.push(p.parse().ok()?);
    }
    // An appendix letter alone is no heading (`Appendix` carries it), and an
    // appendix sub-heading needs a number.
    if app && nums.len() < 2 {
        return None;
    }
    Some((app, nums))
}

/// Tracks the headings seen, accepting only the next sibling or first child
/// of one already seen.
#[derive(Default, Clone)]
struct Outline {
    implicit: bool,
    seen: BTreeSet<String>,
    last_child: BTreeMap<String, u32>,
}

impl Outline {
    /// Count the missing ancestors of `label` as seen when each is the first
    /// of its level and some ancestor is seen.
    fn fill_implicit(&mut self, label: &Label) {
        let (app, nums) = label;
        let up = |n: usize| -> Label { (*app, nums[..n].to_vec()) };
        let mut depth = nums.len().saturating_sub(1);
        while depth >= 1 && !self.seen.contains(&render(&up(depth))) {
            depth -= 1;
        }
        // The ancestors of lengths `depth + 1 ..= len - 1` are missing.
        let missing = depth + 1..nums.len();
        if depth == 0 || missing.is_empty() || missing.clone().any(|n| nums[n - 1] != 1) {
            return;
        }
        let mut parent = render(&up(depth));
        for n in missing {
            let this = render(&up(n));
            self.last_child.insert(parent, 1);
            self.seen.insert(this.clone());
            parent = this;
        }
    }

    fn accept(&mut self, label: &Label) -> bool {
        if !self.implicit {
            return self.accept_next(label);
        }
        let mut trial = self.clone();
        trial.fill_implicit(label);
        let ok = trial.accept_next(label);
        if ok {
            *self = trial;
        }
        ok
    }

    fn accept_next(&mut self, label: &Label) -> bool {
        let (app, nums) = label;
        let (parent, number) = match (app, nums.len()) {
            (true, 1) => ("#".to_owned(), nums[0]),
            (false, 1) => (String::new(), nums[0]),
            _ => {
                let up: Label = (*app, nums[..nums.len() - 1].to_vec());
                let parent = render(&up);
                if !self.seen.contains(&parent) {
                    return false;
                }
                (parent, nums[nums.len() - 1])
            }
        };
        if self.last_child.get(&parent).copied().unwrap_or(0) + 1 != number {
            return false;
        }
        self.last_child.insert(parent, number);
        self.seen.insert(render(label));
        true
    }
}

fn caption(t: &str) -> Option<(bool, u32)> {
    let (is_table, rest) = match (t.strip_prefix("Table "), t.strip_prefix("Figure ")) {
        (Some(r), _) => (true, r),
        (_, Some(r)) => (false, r),
        _ => return None,
    };
    let digits: String = rest.chars().take_while(char::is_ascii_digit).collect();
    let after = rest[digits.len()..].strip_prefix(' ')?;
    if digits.is_empty() || t.contains(".....") {
        return None;
    }
    after
        .chars()
        .next()
        .filter(|c| c.is_ascii_uppercase() || c.is_ascii_digit())?;
    Some((is_table, digits.parse().ok()?))
}

/// The classified lines of the body (from the first real heading `1`).
fn classify(lines: &[String], doc: &Doc) -> Result<Vec<Line>, String> {
    let mut outline = Outline {
        implicit: doc.implicit_levels,
        ..Outline::default()
    };
    let mut out = Vec::new();
    let mut started = false;
    for l in lines {
        let t = l.trim();
        if t.is_empty() {
            if started {
                out.push(Line::Blank);
            }
            continue;
        }
        if let Some(label) = parse_heading(l) {
            let first = !started && label == (false, vec![1]);
            if first || (started && outline.accept(&label)) {
                if first {
                    outline.accept(&label);
                    started = true;
                }
                out.push(Line::Heading(render(&label)));
                continue;
            }
        }
        if !started {
            continue;
        }
        match caption(t) {
            Some((table, n)) => out.push(Line::Caption(table, n)),
            None => out.push(Line::Text(l.clone())),
        }
    }
    if !started {
        return Err("no numbered heading `1` found: is this the spec text?".to_owned());
    }
    Ok(out)
}

/// A line that is a table row: it has a gap of three spaces between cells.
fn cells(text: &str) -> Vec<&str> {
    text.split("   ")
        .map(str::trim)
        .filter(|c| !c.is_empty())
        .collect()
}

fn is_abbreviation(word: &str) -> bool {
    matches!(
        word.to_ascii_lowercase().as_str(),
        "e.g" | "i.e" | "vs" | "cf" | "approx" | "fig" | "sec" | "no"
    )
}

/// The sentences of one paragraph unit: split after `. ? !` that is followed
/// by a space and a capital, digit-free of abbreviations.
fn sentences(unit: &str) -> Vec<&str> {
    let b = unit.as_bytes();
    let mut out = Vec::new();
    let mut start = 0;
    for i in 0..b.len() {
        if !matches!(b[i], b'.' | b'?' | b'!') {
            continue;
        }
        // Skip closing quotes and brackets after the mark.
        let mut j = i + 1;
        while unit[j..].starts_with(['"', '\'', ')', '\u{201d}', '\u{2019}']) {
            j += unit[j..].chars().next().map_or(1, char::len_utf8);
        }
        let rest = &unit[j..];
        let Some(next) = rest.strip_prefix(' ').map(str::trim_start) else {
            continue;
        };
        if !next
            .chars()
            .next()
            .is_some_and(|c| c.is_uppercase() || matches!(c, '\u{2018}' | '\u{201c}' | '"' | '\''))
        {
            continue;
        }
        let word_start = unit[..i]
            .rfind(|c: char| c.is_whitespace())
            .map_or(0, |p| p + 1);
        if b[i] == b'.'
            && is_abbreviation(unit[word_start..i].trim_start_matches(['(', '\u{201c}', '"']))
        {
            continue;
        }
        out.push(unit[start..j].trim());
        start = j;
    }
    out.push(unit[start..].trim());
    out.retain(|s| !s.is_empty());
    out
}

/// The strongest conformance word of a sentence: `(kind, negated)`.
fn keyword(sentence: &str) -> Option<(&'static str, bool)> {
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
        if let Some(rank) = WORDS.iter().position(|(w, _)| *w == word) {
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
                    best = Some((rank, WORDS[rank].1, negated));
                }
            }
        }
        i = j;
    }
    best.map(|(_, k, n)| (k, n))
}

struct Unit {
    section: String,
    text: String,
}

/// The paragraph units of the body, each under its section.
fn units(lines: &[Line]) -> Vec<Unit> {
    let mut out = Vec::new();
    let mut section = String::new();
    let mut cur: Option<String> = None;
    let mut last_item = 0;
    let flush = |cur: &mut Option<String>, section: &str, out: &mut Vec<Unit>| {
        if let Some(text) = cur.take() {
            out.push(Unit {
                section: section.to_owned(),
                text,
            });
        }
    };
    for l in lines {
        let raw = match l {
            Line::Heading(label) => {
                flush(&mut cur, &section, &mut out);
                section = label.to_ascii_lowercase();
                last_item = 0;
                continue;
            }
            Line::Blank | Line::Caption(..) => {
                flush(&mut cur, &section, &mut out);
                continue;
            }
            Line::Text(t) => t,
        };
        let t = raw.trim();
        let (bullet, body) = match t.strip_prefix('\u{2022}') {
            Some(r) => (true, r.trim_start()),
            None => (false, t),
        };
        // A numbered item is the next of a list (`1.` starts one); a wrapped
        // line that starts `32. Instead` is not.
        let item = body
            .split_once(". ")
            .filter(|(n, r)| !r.is_empty() && n.bytes().all(|b| b.is_ascii_digit()))
            .and_then(|(n, _)| n.parse::<u32>().ok());
        let numbered = item.is_some_and(|n| n == 1 || n == last_item + 1);
        if let Some(n) = item.filter(|_| numbered) {
            last_item = n;
        }
        let row = cells(body);
        if row.len() > 1 {
            flush(&mut cur, &section, &mut out);
            // The row that defines a conformance word is not a use of it.
            let defines = WORDS.iter().any(|(w, _)| row[0].eq_ignore_ascii_case(w));
            if !defines {
                out.push(Unit {
                    section: section.clone(),
                    text: row.join(" "),
                });
            }
            continue;
        }
        let starts_note = body.starts_with("Note:") || body.starts_with("Example");
        if bullet || numbered || starts_note {
            flush(&mut cur, &section, &mut out);
        }
        match cur.as_mut() {
            Some(c) => {
                if c.ends_with('-') && body.starts_with(|ch: char| ch.is_lowercase()) {
                    c.push_str(body);
                } else {
                    c.push(' ');
                    c.push_str(body);
                }
            }
            None => cur = Some(body.to_owned()),
        }
    }
    flush(&mut cur, &section, &mut out);
    out
}

/// The `<doc>/<section>#<n>` rows.
fn keyword_items(lines: &[Line], doc: &Doc) -> Vec<Item> {
    let mut count: BTreeMap<String, u32> = BTreeMap::new();
    let mut out = Vec::new();
    for unit in units(lines) {
        for s in sentences(&unit.text) {
            let Some((kind, negated)) = keyword(s) else {
                continue;
            };
            let n = count.entry(unit.section.clone()).or_default();
            *n += 1;
            out.push(Item {
                id: format!("{}/{}#{n}", doc.prefix, unit.section),
                kind,
                section: format!("{} \u{a7}{}", doc.label, unit.section.to_ascii_uppercase()),
                facts: if negated {
                    "neg=1".to_owned()
                } else {
                    String::new()
                },
                text: s.to_owned(),
            });
        }
    }
    out
}

/// The lines of table `n`: from its caption to the next caption or heading.
fn table_lines(lines: &[Line], n: u32) -> Result<Vec<&str>, String> {
    let at = lines
        .iter()
        .position(|l| *l == Line::Caption(true, n))
        .ok_or_else(|| format!("Table {n} not found"))?;
    Ok(lines[at + 1..]
        .iter()
        .take_while(|l| matches!(l, Line::Blank | Line::Text(_)))
        .filter_map(|l| match l {
            Line::Text(t) => Some(t.as_str()),
            _ => None,
        })
        .collect())
}

fn hex(token: &str, max_digits: usize) -> Option<u32> {
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

/// The size in bytes of each Message Type, from the allocation table.
fn mt_sizes(lines: &[Line], table: u32) -> Result<BTreeMap<u32, u32>, String> {
    let mut out = BTreeMap::new();
    for l in table_lines(lines, table)? {
        let t: Vec<&str> = l.split_whitespace().collect();
        if let [mt, bits, "bits", ..] = t[..]
            && let (Some(mt), Ok(bits)) = (hex(mt, 1), bits.parse::<u32>())
        {
            out.insert(mt, bits / 8);
        }
    }
    if out.len() != 16 {
        return Err(format!(
            "Table {table} gives {} Message Type sizes, not 16",
            out.len()
        ));
    }
    Ok(out)
}

/// `mt` and `status` of a `Rows::Formats` line.
fn format_row(line: &str) -> Option<(u32, u32, Option<u32>)> {
    let t: Vec<&str> = line.split_whitespace().collect();
    let i = t.iter().position(|x| hex(x, 1).is_some())?;
    let mt = hex(t[i], 1)?;
    if mt == FLEX_MT {
        let bank = hex(t.get(i + 5)?, 2)?;
        let status = hex(t.get(i + 6)?, 2)?;
        return Some((mt, status, Some(bank)));
    }
    let mut k = i + 2;
    if t.get(k) == Some(&"status") {
        k += 1;
    }
    Some((mt, hex(t.get(k)?, 2)?, None))
}

/// `(bank, status)` of a `Rows::FlexStatus` line.
fn flex_status_row(line: &str) -> Option<(u32, u32)> {
    let t: Vec<&str> = line.split_whitespace().collect();
    Some((hex(t.first()?, 2)?, hex(t.get(1)?, 2)?))
}

fn message_item(
    doc: &Doc,
    mt: u32,
    bank: Option<u32>,
    status: u32,
    bytes: u32,
    table: u32,
) -> Item {
    let (id_status, facts) = match bank {
        Some(b) => (
            format!("{b:02x}.{status:02x}"),
            format!("mt=0x{mt:x} bank=0x{b:x} status=0x{status:x} bytes={bytes}"),
        ),
        None => (
            format!("{status:x}"),
            format!("mt=0x{mt:x} status=0x{status:x} bytes={bytes}"),
        ),
    };
    Item {
        id: format!("{}/msg/{mt:x}/{id_status}", doc.prefix),
        kind: "",
        section: format!("{} Table {table}", doc.label),
        facts,
        text: String::new(),
    }
}

/// The Message Type rows and the defined-message rows.
fn message_items(
    lines: &[Line],
    doc: &Doc,
    mt_table: u32,
    tables: &[(u32, Rows)],
) -> Result<Vec<Item>, String> {
    let sizes = mt_sizes(lines, mt_table)?;
    let mut out: Vec<Item> = sizes
        .iter()
        .map(|(mt, bytes)| Item {
            id: format!("{}/mt/{mt:x}", doc.prefix),
            kind: "",
            section: format!("{} Table {mt_table}", doc.label),
            facts: format!("mt=0x{mt:x} bytes={bytes}"),
            text: String::new(),
        })
        .collect();
    for &(table, shape) in tables {
        let mut found = 0;
        for l in table_lines(lines, table)? {
            let row = match shape {
                Rows::Formats => format_row(l),
                Rows::FlexStatus => flex_status_row(l).map(|(b, s)| (FLEX_MT, s, Some(b))),
            };
            let Some((mt, status, bank)) = row else {
                continue;
            };
            let bytes = *sizes
                .get(&mt)
                .ok_or_else(|| format!("Table {table}: Message Type 0x{mt:x} has no size"))?;
            out.push(message_item(doc, mt, bank, status, bytes, table));
            found += 1;
        }
        if found == 0 {
            return Err(format!("Table {table} has no message rows"));
        }
    }
    Ok(out)
}

/// The `<doc>/hdr/<n>` rows: the `Value` line of the header table, eight
/// hex bytes.
fn header_items(lines: &[Line], doc: &Doc, table: u32) -> Result<Vec<Item>, String> {
    let values: Vec<u32> = table_lines(lines, table)?
        .into_iter()
        .find_map(|l| {
            let t: Vec<&str> = l.split_whitespace().collect();
            (t.first() == Some(&"Value")).then(|| {
                t[1..]
                    .iter()
                    .map(|x| hex(x, 2))
                    .collect::<Option<Vec<u32>>>()
            })
        })
        .flatten()
        .ok_or_else(|| format!("Table {table} has no Value line of hex bytes"))?;
    if values.len() != 8 {
        return Err(format!(
            "Table {table} gives {} header bytes, not 8",
            values.len()
        ));
    }
    Ok(values
        .iter()
        .enumerate()
        .map(|(i, v)| Item {
            id: format!("{}/hdr/{}", doc.prefix, i + 1),
            kind: "",
            section: format!("{} Table {table}", doc.label),
            facts: format!("byte={} value=0x{v:x}", i + 1),
            text: String::new(),
        })
        .collect())
}

/// Every item of the spec text: the keyword rows in document order, then the
/// document's table rows.
pub(super) fn extract(doc: &Doc, text: &str) -> Result<Vec<Item>, String> {
    let lines = classify(&clean(text), doc)?;
    let mut out = keyword_items(&lines, doc);
    out.extend(match doc.tables {
        Tables::Messages { mt_table, messages } => message_items(&lines, doc, mt_table, messages)?,
        Tables::FileHeader { table } => header_items(&lines, doc, table)?,
    });
    let mut seen = BTreeSet::new();
    for it in &out {
        if !seen.insert(it.id.as_str()) {
            return Err(format!("two items share the id `{}`", it.id));
        }
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn page(body: &str) -> String {
        format!("RUNNING HEADER\n\n{body}\n\nFOOTER\n 7\n")
    }

    /// A made-up text: four pages with a running header and footer.
    fn doc(pages: &[&str]) -> String {
        pages
            .iter()
            .map(|p| page(p))
            .collect::<Vec<_>>()
            .join("\x0c")
    }

    const TOC: &str = "Contents\n1 Alpha ........ 3\n 1.1 Beta .......... 3\n";

    fn ids(items: &[Item]) -> Vec<(&str, &str)> {
        items.iter().map(|i| (i.id.as_str(), i.kind)).collect()
    }

    fn sentences_of(body: &str) -> Vec<Item> {
        let text = doc(&[TOC, body, "", ""]);
        keyword_items(&classify(&clean(&text), &M2_104).unwrap(), &M2_104)
    }

    #[test]
    fn keywords_map_to_kinds_and_count_per_section() {
        let items = sentences_of(
            "1 Alpha\nA frob shall spin. A frob should hum. A frob may idle.\n\n1.1 Beta\nA bar shall not ring. Plain.\n",
        );
        assert_eq!(
            ids(&items),
            [
                ("m2-104/1#1", "required"),
                ("m2-104/1#2", "recommended"),
                ("m2-104/1#3", "permitted"),
                ("m2-104/1.1#1", "required"),
            ]
        );
        assert_eq!(items[3].facts, "neg=1");
        assert_eq!(items[0].facts, "");
        assert_eq!(items[0].section, "M2-104 \u{a7}1");
    }

    #[test]
    fn the_strongest_word_of_a_sentence_decides_and_case_is_ignored() {
        let items = sentences_of("1 Alpha\nA may idle but a frob SHALL NOT spin.\n");
        assert_eq!(items.len(), 1);
        assert_eq!(items[0].kind, "required");
        assert_eq!(items[0].facts, "neg=1");
    }

    #[test]
    fn a_quoted_word_and_a_defining_table_row_are_not_uses() {
        let items = sentences_of(
            "1 Alpha\nshall      Statements of requirement     All \u{2018}shall\u{2019} statements.\nA rule about \u{2018}should\u{2019} words.\nA frob shall spin.\n",
        );
        assert_eq!(ids(&items), [("m2-104/1#1", "required")]);
    }

    #[test]
    fn a_sentence_that_wraps_hyphenates_and_crosses_a_page_is_one() {
        let text = doc(&[
            TOC,
            "1 Alpha\nA frob with a long-\nrunning spin shall turn when the\n",
            "wheel moves.\n\nAnother frob shall stop.\n",
            "",
        ]);
        let items = keyword_items(&classify(&clean(&text), &M2_104).unwrap(), &M2_104);
        assert_eq!(items.len(), 2);
        assert_eq!(
            items[0].text,
            "A frob with a long-running spin shall turn when the wheel moves."
        );
    }

    #[test]
    fn bullets_and_numbered_items_are_sentences_of_their_own() {
        let items = sentences_of(
            "1 Alpha\nIn particular:\n  \u{2022} a frob shall spin\n  \u{2022} a frob shall hum\n1. The sender should wait\n2. The sender may go\n",
        );
        assert_eq!(items.len(), 4);
        assert_eq!(items[0].text, "a frob shall spin");
        assert_eq!(items[2].kind, "recommended");
    }

    #[test]
    fn an_abbreviation_does_not_end_a_sentence() {
        let items = sentences_of("1 Alpha\nA frob (e.g. A big one) shall spin. B.\n");
        assert_eq!(items.len(), 1);
        assert_eq!(items[0].text, "A frob (e.g. A big one) shall spin.");
    }

    #[test]
    fn only_the_next_sibling_or_first_child_is_a_heading() {
        let items = sentences_of(
            "1 Alpha\n1.1 Beta\nA frob shall spin.\n16 Groups want it. The frob should hum.\n1.2 Gamma\nA frob may idle.\nAppendix A: Delta\nA.1 Epsilon\nA frob shall stop.\nB.1 Stray\nA frob shall go.\n",
        );
        assert_eq!(
            ids(&items),
            [
                ("m2-104/1.1#1", "required"),
                ("m2-104/1.1#2", "recommended"),
                ("m2-104/1.2#1", "permitted"),
                ("m2-104/a.1#1", "required"),
                ("m2-104/a.1#2", "required"),
            ]
        );
    }

    #[test]
    fn implicit_levels_take_a_heading_whose_ancestors_are_each_the_first() {
        let text = doc(&[
            TOC,
            "1 Alpha\n1.1 Beta\nA frob shall spin.\n1.1.1.1 Deep\nA frob should hum.\n1.2 Gamma\nA frob may idle.\n\n1.2.2.1 Stray\nA frob shall go.\n",
            "",
            "",
        ]);
        let lines = classify(&clean(&text), &M2_116).unwrap();
        assert_eq!(
            ids(&keyword_items(&lines, &M2_116)),
            [
                ("m2-116/1.1#1", "required"),
                ("m2-116/1.1.1.1#1", "recommended"),
                ("m2-116/1.2#1", "permitted"),
                ("m2-116/1.2#2", "required"),
            ]
        );
        // Without the flag a heading with a missing parent is text.
        let lines = classify(&clean(&text), &M2_104).unwrap();
        let plain = keyword_items(&lines, &M2_104);
        assert!(plain.iter().all(|i| !i.id.contains("1.1.1.1")));
        assert_eq!(plain[1].id, "m2-104/1.2#1");
    }

    #[test]
    fn the_file_header_rows_come_from_the_value_line() {
        let text = doc(&[
            TOC,
            "1 Alpha\nTable 5 Header\nByte 1  Byte 2\nValue  0x53 0x4D 0x46 0x32 0x43 0x4C 0x49 0x50\nText  S M F 2 C L I P\nFigure 1 End\n",
            "",
            "",
        ]);
        let items = extract(&M2_116, &text).unwrap();
        let by = |id: &str| items.iter().find(|i| i.id == id).map(|i| i.facts.as_str());
        assert_eq!(by("m2-116/hdr/1"), Some("byte=1 value=0x53"));
        assert_eq!(by("m2-116/hdr/8"), Some("byte=8 value=0x50"));
        let short = text.replace(" 0x50", "");
        assert!(extract(&M2_116, &short).unwrap_err().contains("not 8"));
    }

    #[test]
    fn the_cover_and_contents_are_skipped() {
        let text = doc(&[
            "Cover. A frob shall spin.\n1 Alpha ........ 3\n",
            "1 Alpha\nA frob shall spin.\n",
            "",
            "",
        ]);
        let items = keyword_items(&classify(&clean(&text), &M2_104).unwrap(), &M2_104);
        assert_eq!(ids(&items), [("m2-104/1#1", "required")]);
        assert!(classify(&clean("no headings here"), &M2_104).is_err());
    }

    const TABLES: &str = "Table 4 Allocation\n  0x0   32 bits   Utility\n  0x1   32 bits   Real Time\n  0x2   32 bits\n  0x3   64 bits\n  0x4   64 bits\n  0x5   128 bits\n  0x6   32 bits   Reserved\n  0x7   32 bits\n  0x8   64 bits\n  0x9   64 bits\n  0xA   64 bits\n  0xB   96 bits\n  0xC   96 bits\n  0xD   128 bits   Flex\n  0xE   128 bits\n  0xF   128 bits   Stream\nFigure 4 Example\n";

    const MESSAGES: &str = "Table 34 All Messages\nUTILITY   NOOP   0x0   res.   0x0   20 bit\nSYSTEM COMMON   MIDI Time Code   0x1   0xg   0xF1   7 bit\nMIDI 2.0 CHANNEL VOICE   Note On   0x4   0xg   0x9   0xn   7 bit\nUMP Stream   Endpoint Discovery   0xF   0   status 0x0   UMP ver.\nUMP Stream   Function Block Discovery   0xF   1   status 0x10   fb\nFlex Data   Set Tempo   0xD   0xg   0   1   0   0x00   0x00   Number\nFlex Data   Text Message Common   0xD   0xg   0-3   0xa   0xn   0x01-0x02   0xvv   data\nColor Key   Reserved for future use\nTable 16 Text\nBank   Status   Message\n0x01   0x00   Unknown\n0x01   0x0A   Accompanying\n0x02   0x01   Lyrics\nFigure 5 End\n";

    fn spec(extra: &str) -> String {
        doc(&[
            TOC,
            "1 Alpha\nA frob shall spin.\n",
            &format!("{TABLES}\n{MESSAGES}"),
            extra,
        ])
    }

    #[test]
    fn message_type_and_message_rows_come_from_the_configured_tables() {
        let items = extract(&M2_104, &spec("")).unwrap();
        let by = |id: &str| items.iter().find(|i| i.id == id).map(|i| i.facts.as_str());
        assert_eq!(by("m2-104/mt/b"), Some("mt=0xb bytes=12"));
        assert_eq!(by("m2-104/msg/0/0"), Some("mt=0x0 status=0x0 bytes=4"));
        assert_eq!(by("m2-104/msg/1/f1"), Some("mt=0x1 status=0xf1 bytes=4"));
        assert_eq!(by("m2-104/msg/4/9"), Some("mt=0x4 status=0x9 bytes=8"));
        assert_eq!(by("m2-104/msg/f/10"), Some("mt=0xf status=0x10 bytes=16"));
        assert_eq!(
            by("m2-104/msg/d/00.00"),
            Some("mt=0xd bank=0x0 status=0x0 bytes=16")
        );
        assert_eq!(
            by("m2-104/msg/d/01.0a"),
            Some("mt=0xd bank=0x1 status=0xa bytes=16")
        );
        assert!(by("m2-104/msg/d/02.01").is_some());
        // The common-format row has no single status; the status table
        // supplies the messages.
        assert_eq!(
            items
                .iter()
                .filter(|i| i.id.starts_with("m2-104/msg/d/"))
                .count(),
            4
        );
        assert_eq!(
            items
                .iter()
                .filter(|i| i.id.starts_with("m2-104/mt/"))
                .count(),
            16
        );
        assert_eq!(items[0].id, "m2-104/1#1");
    }

    #[test]
    fn a_missing_table_or_size_is_an_error() {
        let no_msgs = doc(&[TOC, &format!("1 Alpha\n{TABLES}"), "", ""]);
        assert!(
            extract(&M2_104, &no_msgs)
                .unwrap_err()
                .contains("Table 34 not found")
        );
        let short = spec("").replace("  0xE   128 bits\n", "");
        assert!(extract(&M2_104, &short).unwrap_err().contains("not 16"));
    }

    #[test]
    fn extraction_is_deterministic() {
        let text = spec("1.1 Beta\nA bar should ring.\n");
        assert_eq!(
            extract(&M2_104, &text).unwrap(),
            extract(&M2_104, &text).unwrap()
        );
    }
}
