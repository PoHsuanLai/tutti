//! `check-conformance`: the spec inventories are well formed, every test tag
//! names a row, and the CLAP and VST3 inventories are the sets `clap-sys` and
//! the vendored VST3 SDK declare.
//!
//! An inventory is one or more files `testing/conformance/<area>[-<part>].txt`,
//! one spec item per line, nine columns separated by `|`:
//!
//! ```text
//! id | side | maturity | kind | section | paraphrase | facts | na-kind | cite
//! ```
//!
//! Blank lines and lines starting with `#` are skipped. `id` is non-empty and
//! made of `a-z A-Z 0-9 . _ / # -` (a spec's own capitals, `vst3.IComponent.setActive`, stay), unique across every file. `side` is a non-empty
//! lowercase token (`host`, `plugin`, `core`, `initiator`, ..). `maturity` is
//! `stable`, `draft` or empty; `kind` is `required`, `recommended`, `permitted`
//! or empty; `section` is free text or empty; `paraphrase` is the item in our
//! own words and is not empty in a committed row; `facts` is empty or
//! space-separated `name=int` pairs (`name` a lowercase identifier, `int`
//! decimal or `0x` hex, no name
//! twice). `na-kind` (`deprecated`, `forbidden-for-side`, `other-side`) and
//! `cite` (the spec location) are both empty or both set. A row with the wrong
//! column count or any invalid column is an error naming the file and line;
//! nothing is half-read.
//!
//! A test claims rows with a last rustdoc line `/// Spec: id, id` on its `fn`
//! (or above a case in the plugin contract's list; a contract case runs on
//! every format, so its tag lists every format's ids). A tag naming an id no
//! inventory holds, an empty tag, or a tag that is not the last doc line is a
//! finding. Rows no test claims are Missing, which is the work list and never
//! a finding.
//!
//! The rows whose id starts `clap.` are held to the `clap-sys` source the lock
//! file pins ([`super::clap_sys`]): the committed `(id, side,
//! maturity)` triples equal the extracted ones. With the source absent the
//! comparison is skipped and said so, unless `TUTTI_CONFORMANCE=require`.
//!
//! The rows whose id starts `vst3.` are held to the SDK headers under
//! `vendor/vst3-sdk/pluginterfaces` ([`super::vst3_sdk`]): the committed `(id,
//! side, maturity)` triples equal the extracted ones. The SDK is a submodule,
//! so a checkout without it skips the comparison and says so, unless
//! `TUTTI_CONFORMANCE=require` (CI sets it).
//!
//! `vst2.txt` (the VST 2.4 surface) and `au.txt` (the AudioUnit v2 surface) are
//! written by hand and checked by review: no VST 2.4 header is in the repo
//! (Steinberg no longer distributes the SDK) and the AudioToolbox headers ship
//! with macOS only, so nothing compares them to a source. They are held to the
//! same row format and tag rules as the others.
//!
//! The rows whose id starts with a MIDI document's prefix (`m2-104/`,
//! `m2-116/`) are held to that document's spec text in the directory
//! `TUTTI_MIDI_SPECS` points at ([`super::midi_spec`]): the committed `(id,
//! kind, section, facts)` quadruples equal the extracted ones. The MIDI specs
//! are not in the repo and not in CI, so with the text absent the comparison
//! is skipped and said so, and only `TUTTI_CONFORMANCE=require-all` (which
//! also requires the `clap-sys` source) makes the absence a finding. CI sets
//! `require`, which never demands the MIDI text; `require-all` is for a local
//! run on a machine that holds the specs.

use super::clap_sys::{self, Item};
use super::lex::blank_keeping_docs;
use super::midi_spec;
use super::vst3_sdk;
use super::{Report, Tree};
use std::collections::{BTreeMap, BTreeSet};

const DIR: &str = "testing/conformance";
const NA_KINDS: [&str; 3] = ["deprecated", "forbidden-for-side", "other-side"];
const KINDS: [&str; 3] = ["required", "recommended", "permitted"];
const MATURITIES: [&str; 2] = ["stable", "draft"];
const TAG: &str = "/// Spec:";
/// The ids of the VST3 inventory.
const VST3_PREFIX: &str = "vst3.";

fn skipped(dir: &str) -> bool {
    matches!(dir, "target" | ".git")
}

/// One inventory row, every column as written (trimmed).
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct Row {
    pub(super) id: String,
    pub(super) side: String,
    pub(super) maturity: String,
    pub(super) kind: String,
    pub(super) section: String,
    pub(super) paraphrase: String,
    pub(super) facts: String,
    pub(super) na_kind: String,
    pub(super) cite: String,
}

impl Row {
    fn render(&self) -> String {
        [
            &self.id,
            &self.side,
            &self.maturity,
            &self.kind,
            &self.section,
            &self.paraphrase,
            &self.facts,
            &self.na_kind,
            &self.cite,
        ]
        .iter()
        .map(|c| c.as_str())
        .collect::<Vec<_>>()
        .join(" | ")
        .trim_end()
        .to_owned()
    }
}

fn is_id(s: &str) -> bool {
    !s.is_empty()
        && s.bytes()
            .all(|b| matches!(b, b'a'..=b'z' | b'A'..=b'Z' | b'0'..=b'9' | b'.' | b'_' | b'/' | b'#' | b'-'))
}

fn is_token(s: &str, allow_digit_first: bool) -> bool {
    let mut b = s.bytes();
    let first = b.next();
    first.is_some_and(|f| f.is_ascii_lowercase() || (allow_digit_first && f.is_ascii_digit()))
        && b.all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == b'_' || c == b'-')
}

fn check_facts(facts: &str) -> Result<(), String> {
    let mut seen = BTreeSet::new();
    for pair in facts.split_whitespace() {
        let (name, value) = pair
            .split_once('=')
            .ok_or_else(|| format!("fact `{pair}` is not name=int"))?;
        let ident = name
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'_')
            && name.bytes().next().is_some_and(|b| b.is_ascii_lowercase());
        if !ident {
            return Err(format!("fact name `{name}` is not a lowercase identifier"));
        }
        let ok = match value.strip_prefix("0x") {
            Some(h) => !h.is_empty() && u64::from_str_radix(h, 16).is_ok(),
            None => {
                !value.is_empty()
                    && value.bytes().all(|b| b.is_ascii_digit())
                    && value.parse::<u64>().is_ok()
            }
        };
        if !ok {
            return Err(format!("fact `{pair}` has a value that is not an integer"));
        }
        if !seen.insert(name) {
            return Err(format!("fact `{name}` is given twice"));
        }
    }
    Ok(())
}

/// One non-blank, non-comment line as a row, or what is wrong with it. The
/// paraphrase may be empty here: a freshly generated row has none, and
/// [`load`] refuses it.
pub(super) fn parse_row(line: &str) -> Result<Row, String> {
    let cols: Vec<&str> = line.split('|').map(str::trim).collect();
    let [
        id,
        side,
        maturity,
        kind,
        section,
        paraphrase,
        facts,
        na_kind,
        cite,
    ] = cols[..]
    else {
        return Err(format!("expected 9 columns, found {}", cols.len()));
    };
    if !is_id(id) {
        return Err(format!(
            "id `{id}` is empty or has a character outside a-z A-Z 0-9 . _ / # -"
        ));
    }
    if !is_token(side, false) {
        return Err(format!("side `{side}` is not a non-empty lowercase token"));
    }
    if !maturity.is_empty() && !MATURITIES.contains(&maturity) {
        return Err(format!(
            "maturity `{maturity}` is not stable, draft or empty"
        ));
    }
    if !kind.is_empty() && !KINDS.contains(&kind) {
        return Err(format!(
            "kind `{kind}` is not required, recommended, permitted or empty"
        ));
    }
    check_facts(facts)?;
    if na_kind.is_empty() != cite.is_empty() {
        return Err("na-kind and cite must be both set or both empty".to_owned());
    }
    if !na_kind.is_empty() && !NA_KINDS.contains(&na_kind) {
        return Err(format!(
            "na-kind `{na_kind}` is not deprecated, forbidden-for-side or other-side"
        ));
    }
    Ok(Row {
        id: id.to_owned(),
        side: side.to_owned(),
        maturity: maturity.to_owned(),
        kind: kind.to_owned(),
        section: section.to_owned(),
        paraphrase: paraphrase.to_owned(),
        facts: facts.to_owned(),
        na_kind: na_kind.to_owned(),
        cite: cite.to_owned(),
    })
}

/// A file's rows; the `Err`s are `file:line: problem`.
fn parse_file(path: &str, text: &str) -> (Vec<Row>, Vec<String>) {
    let (mut rows, mut errs) = (Vec::new(), Vec::new());
    for (i, line) in text.lines().enumerate() {
        let t = line.trim();
        if t.is_empty() || t.starts_with('#') {
            continue;
        }
        match parse_row(t) {
            Ok(r) => rows.push(r),
            Err(e) => errs.push(format!("{path}:{}: {e}", i + 1)),
        }
    }
    (rows, errs)
}

/// The version `Cargo.lock` pins for `clap-sys`.
pub(super) fn pinned_clap_sys(lock: &str) -> Option<String> {
    let mut lines = lock.lines();
    while let Some(l) = lines.next() {
        if l.trim() == "name = \"clap-sys\"" {
            let v = lines.next()?.trim().strip_prefix("version = \"")?;
            return Some(v.strip_suffix('"')?.to_owned());
        }
    }
    None
}

/// Every inventory file's rows (all files read, every error collected).
fn load(tree: &dyn Tree) -> (Vec<(String, Row)>, Vec<String>) {
    let (mut rows, mut errs) = (Vec::new(), Vec::new());
    let mut owner: BTreeMap<String, String> = BTreeMap::new();
    for path in tree.files(DIR, &skipped) {
        if !path.ends_with(".txt") {
            continue;
        }
        let Some(text) = tree.read(&path) else {
            continue;
        };
        let (file_rows, file_errs) = parse_file(&path, &text);
        errs.extend(file_errs);
        for r in file_rows {
            if r.paraphrase.is_empty() {
                errs.push(format!("{path}: row `{}` has an empty paraphrase", r.id));
            }
            if let Some(first) = owner.get(&r.id) {
                errs.push(format!("{path}: id `{}` is already a row of {first}", r.id));
            } else {
                owner.insert(r.id.clone(), path.clone());
                rows.push((path.clone(), r));
            }
        }
    }
    (rows, errs)
}

/// The tags of one file: `(line, ids)`, and the problems with them.
fn tags(path: &str, text: &str) -> (Vec<(usize, Vec<String>)>, Vec<String>) {
    let code = blank_keeping_docs(text);
    let lines: Vec<&str> = code.lines().collect();
    let (mut out, mut errs) = (Vec::new(), Vec::new());
    for (i, l) in lines.iter().enumerate() {
        let Some(rest) = l.trim_start().strip_prefix(TAG) else {
            continue;
        };
        let at = i + 1;
        if lines
            .get(i + 1)
            .is_some_and(|n| n.trim_start().starts_with("///"))
        {
            errs.push(format!(
                "{path}:{at}: the Spec tag is not the last doc line"
            ));
        }
        let ids: Vec<String> = rest.split(',').map(|s| s.trim().to_owned()).collect();
        if ids.iter().any(String::is_empty) {
            errs.push(format!("{path}:{at}: the Spec tag has an empty id"));
            continue;
        }
        out.push((at, ids));
    }
    (out, errs)
}

fn triples(items: &[Item]) -> BTreeSet<(String, String, String)> {
    items
        .iter()
        .map(|i| (i.id.clone(), i.side.to_owned(), i.maturity.to_owned()))
        .collect()
}

/// Which absent spec sources are findings (`TUTTI_CONFORMANCE`).
#[derive(Debug, Clone, Copy, Default)]
pub(super) struct Require {
    pub(super) clap: bool,
    pub(super) vst3: bool,
    pub(super) midi: bool,
}

impl Require {
    /// `require` demands the `clap-sys` source and the VST3 SDK; `require-all`
    /// also the MIDI spec text.
    pub(super) fn parse(value: Option<&str>) -> Require {
        match value {
            Some("require") => Require {
                clap: true,
                vst3: true,
                midi: false,
            },
            Some("require-all") => Require {
                clap: true,
                vst3: true,
                midi: true,
            },
            _ => Require::default(),
        }
    }
}

fn vst3_triples(items: &[vst3_sdk::Item]) -> BTreeSet<(String, String, String)> {
    items
        .iter()
        .map(|i| (i.id.clone(), i.side.to_owned(), i.maturity.to_owned()))
        .collect()
}

/// The `(id, kind, section, facts)` of every row whose id starts `prefix`.
fn generated(rows: &[(String, Row)], prefix: &str) -> BTreeSet<[String; 4]> {
    rows.iter()
        .map(|(_, r)| r)
        .filter(|r| r.id.starts_with(prefix))
        .map(|r| {
            [
                r.id.clone(),
                r.kind.clone(),
                r.section.clone(),
                r.facts.clone(),
            ]
        })
        .collect()
}

fn midi_quads(items: &[midi_spec::Item]) -> BTreeSet<[String; 4]> {
    items
        .iter()
        .map(|i| {
            [
                i.id.clone(),
                i.kind.to_owned(),
                i.section.clone(),
                i.facts.clone(),
            ]
        })
        .collect()
}

pub(super) fn check(
    tree: &dyn Tree,
    clap: Result<&dyn Tree, String>,
    midi: &dyn Fn(&midi_spec::Doc) -> Result<String, String>,
    require: Require,
) -> Report {
    let mut r = Report::default();
    let (rows, errs) = load(tree);
    for e in errs {
        r.fail(e);
    }
    let known: BTreeSet<&str> = rows.iter().map(|(_, row)| row.id.as_str()).collect();

    let mut tagged = BTreeSet::new();
    for root in ["crates", "testing"] {
        for path in tree.files(root, &skipped) {
            if !path.ends_with(".rs") {
                continue;
            }
            let Some(text) = tree.read(&path) else {
                continue;
            };
            let (found, errs) = tags(&path, &text);
            for e in errs {
                r.fail(e);
            }
            for (line, ids) in found {
                for id in ids {
                    if known.contains(id.as_str()) {
                        tagged.insert(id);
                    } else {
                        r.fail(format!(
                            "{path}:{line}: the Spec tag names unknown id `{id}`"
                        ));
                    }
                }
            }
        }
    }

    let committed: BTreeSet<(String, String, String)> = rows
        .iter()
        .filter(|(_, row)| row.id.starts_with("clap."))
        .map(|(_, row)| (row.id.clone(), row.side.clone(), row.maturity.clone()))
        .collect();
    let spec = tree
        .read("Cargo.lock")
        .and_then(|l| pinned_clap_sys(&l))
        .map_or_else(|| Err("Cargo.lock pins no clap-sys".to_owned()), |_| clap)
        .and_then(clap_sys::extract);
    match spec {
        Ok(items) => {
            let want = triples(&items);
            for t in want.difference(&committed) {
                r.fail(format!(
                    "{DIR}: clap-sys declares `{}` ({}, {}) that no clap row matches; run `cargo xtask conformance-inventory clap`",
                    t.0, t.1, t.2
                ));
            }
            for t in committed.difference(&want) {
                r.fail(format!(
                    "{DIR}: the clap row `{}` ({}, {}) is not what clap-sys declares; run `cargo xtask conformance-inventory clap`",
                    t.0, t.1, t.2
                ));
            }
        }
        Err(why) if require.clap => r.fail(format!("clap inventory not checked: {why}")),
        Err(why) => r.say(format!("clap inventory not checked: {why}")),
    }
    let committed_vst3: BTreeSet<(String, String, String)> = rows
        .iter()
        .filter(|(_, row)| row.id.starts_with(VST3_PREFIX))
        .map(|(_, row)| (row.id.clone(), row.side.clone(), row.maturity.clone()))
        .collect();
    match vst3_sdk::extract(tree) {
        Ok(items) => {
            let want = vst3_triples(&items);
            for t in want.difference(&committed_vst3) {
                r.fail(format!(
                    "{DIR}: the VST3 SDK declares `{}` ({}, {}) that no vst3 row matches; run `cargo xtask conformance-inventory vst3`",
                    t.0, t.1, t.2
                ));
            }
            for t in committed_vst3.difference(&want) {
                r.fail(format!(
                    "{DIR}: the vst3 row `{}` ({}, {}) is not what the VST3 SDK declares; run `cargo xtask conformance-inventory vst3`",
                    t.0, t.1, t.2
                ));
            }
        }
        Err(why) if require.vst3 => r.fail(format!("vst3 inventory not checked: {why}")),
        Err(why) => r.say(format!("vst3 inventory not checked: {why}")),
    }
    for doc in midi_spec::DOCS {
        let label = doc.label;
        match midi(doc).and_then(|text| midi_spec::extract(doc, &text)) {
            Ok(items) => {
                let want = midi_quads(&items);
                let have = generated(&rows, &format!("{}/", doc.prefix));
                for q in want.difference(&have) {
                    r.fail(format!(
                        "{DIR}: the {label} text gives `{}` ({}, {}, {}) that no row matches; run `cargo xtask conformance-inventory midi`",
                        q[0], q[1], q[2], q[3]
                    ));
                }
                for q in have.difference(&want) {
                    r.fail(format!(
                        "{DIR}: the {label} row `{}` ({}, {}, {}) is not what the text gives; run `cargo xtask conformance-inventory midi`",
                        q[0], q[1], q[2], q[3]
                    ));
                }
            }
            Err(why) if require.midi => r.fail(format!("{label} inventory not checked: {why}")),
            Err(why) => r.say(format!("{label} inventory not checked: {why}")),
        }
    }
    r.say(format!(
        "check-conformance: {} rows, {} ids tagged",
        rows.len(),
        tagged.len()
    ));
    r
}

/// The comment lines that open `clap.txt`: its columns and id grammar.
const HEADER: &str = "\
# CLAP spec inventory. `cargo xtask conformance-inventory clap` regenerates the
# id, side and maturity columns from clap-sys; the others are edited by hand.
#
# Columns: id | side | maturity | kind | section | paraphrase | facts | na-kind | cite
#
# Ids, one per function pointer of a clap-sys vtable struct, event type or flag:
#   clap.ext.<ext>.<fn>                 plugin side of an extension (stable or draft)
#   clap.ext.<ext>.host.<fn>            host side of an extension
#   clap.ext.<ext>.<part>.<fn>          a sub-table: audio_ports_config.info,
#                                       context_menu.builder, undo.context, undo.delta, tuning.t
#   clap.factory.<name>[.<part>].<fn>   factories and what they hand out (core;
#                                       provider is plugin, indexer and
#                                       metadata_receiver are host)
#   clap.entry.<fn>, clap.plugin.<fn>, clap.host.<fn>   the core tables (core)
#   clap.stream.<istream|ostream>.<fn>, clap.events.<input|output>.<fn>   (core)
#   clap.event.<type>                   an event type, e.g. clap.event.note_on
#   clap.event.flag.<flag>              an event header flag, e.g. clap.event.flag.is_live
";

/// The text of `testing/conformance/clap.txt` for the extracted `items`: their
/// id, side and maturity, with every other column kept from `existing` for an
/// id that is still there; the rows keep the order of `items`.
pub(super) fn regenerate(existing: Option<&str>, items: &[Item]) -> Result<String, String> {
    let mut old: BTreeMap<String, Row> = BTreeMap::new();
    if let Some(text) = existing {
        let (rows, errs) = parse_file("clap.txt", text);
        if !errs.is_empty() {
            return Err(errs.join("\n"));
        }
        old.extend(rows.into_iter().map(|r| (r.id.clone(), r)));
    }
    let mut out = String::from(HEADER);
    for it in items {
        let row = match old.remove(&it.id) {
            Some(mut r) => {
                r.side = it.side.to_owned();
                r.maturity = it.maturity.to_owned();
                r
            }
            None => Row {
                id: it.id.clone(),
                side: it.side.to_owned(),
                maturity: it.maturity.to_owned(),
                kind: String::new(),
                section: String::new(),
                paraphrase: String::new(),
                facts: String::new(),
                na_kind: String::new(),
                cite: String::new(),
            },
        };
        out.push_str(&row.render());
        out.push('\n');
    }
    Ok(out)
}

/// The comment lines that open `vst3.txt`: its columns and id grammar.
const VST3_HEADER: &str = "\
# VST3 spec inventory. The spec is the SDK under `vendor/vst3-sdk/pluginterfaces`
# (base, vst, gui headers). `cargo xtask conformance-inventory vst3` regenerates
# the id, side, maturity and section columns from the headers; the others are
# edited by hand. `cargo xtask check-conformance` holds (id, side, maturity) to
# the headers.
#
# Columns: id | side | maturity | kind | section | paraphrase | facts | na-kind | cite
#
# Ids, one per pure virtual method of an interface (a class the header gives a
# DECLARE_CLASS_IID): vst3.<Interface>.<method>, e.g. vst3.IComponent.setActive.
# side: plugin for an interface the doc comment marks [plug imp], host for
# [host imp], core for both marks or none (an unmarked interface takes the side
# of the SDK interface it derives from). maturity: stable. section: the header's
# path under pluginterfaces/, with the [released: x.y.z] mark when it has one.
";

/// The text of `testing/conformance/vst3.txt` for the extracted `items`: their
/// id, side, maturity and section, with the other columns kept from `existing`
/// for an id that is still there. Also the ids added and removed.
pub(super) fn regenerate_vst3(
    existing: Option<&str>,
    items: &[vst3_sdk::Item],
) -> Result<(String, Vec<String>, Vec<String>), String> {
    let mut old: BTreeMap<String, Row> = BTreeMap::new();
    if let Some(text) = existing {
        let (rows, errs) = parse_file("vst3.txt", text);
        if !errs.is_empty() {
            return Err(errs.join("\n"));
        }
        old.extend(rows.into_iter().map(|r| (r.id.clone(), r)));
    }
    let (mut out, mut added) = (String::from(VST3_HEADER), Vec::new());
    for it in items {
        let mut row = old.remove(&it.id).unwrap_or_else(|| {
            added.push(it.id.clone());
            Row {
                id: it.id.clone(),
                side: String::new(),
                maturity: String::new(),
                kind: String::new(),
                section: String::new(),
                paraphrase: String::new(),
                facts: String::new(),
                na_kind: String::new(),
                cite: String::new(),
            }
        });
        row.side = it.side.to_owned();
        row.maturity = it.maturity.to_owned();
        row.section = it.section.clone();
        out.push_str(&row.render());
        out.push('\n');
    }
    Ok((out, added, old.into_keys().collect()))
}

/// The comment lines that open a MIDI inventory file: its columns and id
/// grammar.
fn midi_header(doc: &midi_spec::Doc) -> String {
    let (p, label, title) = (doc.prefix, doc.label, doc.title);
    let id = |shape: String, what: &str| format!("#   {shape:<30}{what}\n");
    let mut out = format!(
        "\
# {title} inventory. The spec text is not in
# the repo: `TUTTI_MIDI_SPECS=<dir> cargo xtask conformance-inventory midi`
# regenerates the id, kind, section and facts columns from the text that
# `pdftotext -layout` (26.01.0) makes of the {label} PDF in that directory (or a
# .txt of it beside the PDF); the other columns are written by hand, in our own
# words. `cargo xtask check-conformance` holds the generated columns to the
# text when `TUTTI_MIDI_SPECS` is set. Nothing of the spec is quoted here: ids,
# section numbers, keyword kinds and functional facts only.
#
# Columns: id | side | maturity | kind | section | paraphrase | facts | na-kind | cite
#
# Ids:
"
    );
    out.push_str(&id(
        format!("{p}/<section>#<n>"),
        "the nth sentence under the numbered section",
    ));
    out.push_str(
        "\
#                                 (appendix letters lowercase: d.2.1) that carries
#                                 a conformance word. kind: shall is required,
#                                 should recommended, may permitted; the negated
#                                 forms keep the kind and carry the fact neg=1.
",
    );
    if let midi_spec::Tables::Messages { .. } = doc.tables {
        out.push_str(&id(
            format!("{p}/mt/<mt>"),
            "a Message Type: facts mt, bytes (the UMP size)",
        ));
        out.push_str(&id(
            format!("{p}/msg/<mt>/<status>"),
            "a defined message: <mt> and <status> are",
        ));
        out.push_str(
            "\
#                                 lowercase hex without 0x; Flex Data (mt d) is
#                                 <bank>.<status>, two hex digits each. Facts:
#                                 mt, status, bank (Flex Data), bytes
",
        );
    }
    out.push_str(doc.note);
    out.push_str(&format!(
        "# side: {} (todo marks a row not yet read).\n",
        doc.sides
    ));
    out
}

/// The text of a MIDI inventory file for the extracted
/// `items` of `doc`: their id, kind, section and facts, with the other columns kept
/// from `existing` for an id that is still there, and a new id as a `todo`
/// row with no paraphrase (the gate refuses it). Also the ids added and
/// removed.
pub(super) fn regenerate_midi(
    doc: &midi_spec::Doc,
    existing: Option<&str>,
    items: &[midi_spec::Item],
) -> Result<(String, Vec<String>, Vec<String>), String> {
    let mut old: BTreeMap<String, Row> = BTreeMap::new();
    if let Some(text) = existing {
        let (rows, errs) = parse_file(doc.file, text);
        if !errs.is_empty() {
            return Err(errs.join("\n"));
        }
        old.extend(rows.into_iter().map(|r| (r.id.clone(), r)));
    }
    let (mut out, mut added) = (midi_header(doc), Vec::new());
    for it in items {
        let mut row = old.remove(&it.id).unwrap_or_else(|| {
            added.push(it.id.clone());
            Row {
                id: it.id.clone(),
                side: "todo".to_owned(),
                maturity: String::new(),
                kind: String::new(),
                section: String::new(),
                paraphrase: String::new(),
                facts: String::new(),
                na_kind: String::new(),
                cite: String::new(),
            }
        });
        row.kind = it.kind.to_owned();
        row.section = it.section.clone();
        row.facts = it.facts.clone();
        out.push_str(&row.render());
        out.push('\n');
    }
    Ok((out, added, old.into_keys().collect()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::gates::fake::Mem;

    const ROW: &str = "clap.ext.params.flush | plugin | stable | | | flush parameter changes without processing | | |";
    const LOCK: &str = "[[package]]\nname = \"clap-sys\"\nversion = \"0.5.0\"\n";
    const PARAMS_SRC: &str =
        "pub struct clap_plugin_params { pub flush: Option<unsafe extern \"C\" fn()>, }";

    const VST3_ROW: &str = "vst3.IComponent.setActive | plugin | stable | | vst/ivstcomponent.h | host activates or deactivates the component | | |";
    const VST3_HEADER_PATH: &str = "vendor/vst3-sdk/pluginterfaces/vst/ivstcomponent.h";
    const VST3_SRC: &str = "/** [plug imp] */\nclass IComponent : public IPluginBase { public: virtual tresult PLUGIN_API setActive (TBool state) = 0; };\nDECLARE_CLASS_IID (IComponent, 1, 2, 3, 4)\n";

    fn inv(rows: &[&str]) -> String {
        rows.join("\n")
    }

    fn run(files: &[(&str, &str)]) -> Report {
        let mem = Mem::with(files);
        check(
            &mem,
            Err("absent".to_owned()),
            &|_| Err("no specs".to_owned()),
            Require::default(),
        )
    }

    fn has_out(r: &Report, needle: &str) -> bool {
        r.out.iter().any(|l| l.contains(needle))
    }

    fn has(r: &Report, needle: &str) -> bool {
        r.findings.iter().any(|f| f.contains(needle))
    }

    #[test]
    fn a_well_formed_inventory_with_a_matching_tag_passes() {
        let r = run(&[
            ("testing/conformance/clap.txt", &inv(&["# c", "", ROW])),
            (
                "crates/a/src/lib.rs",
                "/// Does a thing.\n/// Spec: clap.ext.params.flush\n#[test]\nfn t() {}\n",
            ),
        ]);
        assert!(r.passed(), "{:?}", r.findings);
        assert!(r.out.iter().any(|l| l.contains("1 rows, 1 ids tagged")));
    }

    #[test]
    fn midi_style_rows_parse_with_section_and_facts() {
        let row = parse_row(
            "m2-104/7.4.2#3 | receiver | | required | M2-104 §7.4.2 | note on carries velocity | mt=0x4 status=0x9 n=12 | |",
        )
        .unwrap();
        assert_eq!(row.facts, "mt=0x4 status=0x9 n=12");
        assert_eq!(row.id, "m2-104/7.4.2#3");
    }

    #[test]
    fn malformed_rows_name_file_and_line() {
        let r = run(&[("testing/conformance/clap.txt", "# c\nonly | three | cols\n")]);
        assert!(has(
            &r,
            "testing/conformance/clap.txt:2: expected 9 columns"
        ));
    }

    #[test]
    fn each_column_validation_can_fail() {
        let base = |i: usize, v: &str| {
            let mut c: Vec<&str> =
                "a.b | host | stable | required | s | para | x=1 | deprecated | cite"
                    .split(" | ")
                    .collect();
            c[i] = v;
            c.join(" | ")
        };
        assert!(parse_row(&base(0, "a.b")).is_ok());
        for (i, v, why) in [
            (0, "a b", "id"),
            (0, "a.b!", "id"),
            (0, "", "id"),
            (1, "Host", "side"),
            (1, "", "side"),
            (2, "beta", "maturity"),
            (3, "must", "kind"),
            (6, "x", "not name=int"),
            (6, "x=abc", "not an integer"),
            (6, "x=0x", "not an integer"),
            (6, "X=1", "lowercase identifier"),
            (6, "x=1 x=2", "twice"),
            (7, "gone", "na-kind"),
            (7, "", "both"),
            (8, "", "both"),
        ] {
            let e = parse_row(&base(i, v)).unwrap_err();
            assert!(e.contains(why), "{i} {v}: {e}");
        }
        let blank_maturity_and_kind = "a | host | | | | para | | |";
        assert!(parse_row(blank_maturity_and_kind).is_ok());
        assert!(parse_row("a | initiator | | | | p | | other-side | m2 s1").is_ok());
    }

    #[test]
    fn an_empty_paraphrase_fails_the_gate_but_still_parses_for_regeneration() {
        let row = "a.b | host | | | | | | |";
        assert!(parse_row(row).is_ok());
        let r = run(&[("testing/conformance/x.txt", row)]);
        assert!(has(&r, "row `a.b` has an empty paraphrase"));
        let items = [Item {
            id: "a.b".to_owned(),
            side: "host",
            maturity: "",
        }];
        assert!(regenerate(Some(row), &items).is_ok());
    }

    #[test]
    fn a_duplicate_id_across_files_is_an_error() {
        let r = run(&[
            ("testing/conformance/clap.txt", ROW),
            ("testing/conformance/midi-a.txt", ROW),
        ]);
        assert!(has(&r, "already a row of testing/conformance/clap.txt"));
    }

    #[test]
    fn a_tag_naming_an_unknown_id_fails() {
        let r = run(&[
            ("testing/conformance/clap.txt", ROW),
            (
                "testing/t/tests/x.rs",
                "/// Spec: clap.ext.params.flush, clap.nope\nfn t() {}\n",
            ),
        ]);
        assert!(has(
            &r,
            "testing/t/tests/x.rs:1: the Spec tag names unknown id `clap.nope`"
        ));
        assert_eq!(r.findings.len(), 1);
    }

    #[test]
    fn a_tag_in_a_string_or_plain_comment_is_not_a_tag() {
        let r = run(&[
            ("testing/conformance/clap.txt", ROW),
            (
                "crates/a/src/lib.rs",
                "// /// Spec: nope\nconst S: &str = \"/// Spec: nope\";\n",
            ),
        ]);
        assert!(r.passed(), "{:?}", r.findings);
    }

    #[test]
    fn an_empty_tag_or_one_that_is_not_last_fails() {
        let r = run(&[
            ("testing/conformance/clap.txt", ROW),
            (
                "crates/a/src/lib.rs",
                "/// Spec: clap.ext.params.flush,\nfn a() {}\n",
            ),
            (
                "crates/b/src/lib.rs",
                "/// Spec: clap.ext.params.flush\n/// More prose.\nfn b() {}\n",
            ),
        ]);
        assert!(has(
            &r,
            "crates/a/src/lib.rs:1: the Spec tag has an empty id"
        ));
        assert!(has(
            &r,
            "crates/b/src/lib.rs:1: the Spec tag is not the last doc line"
        ));
    }

    #[test]
    fn missing_rows_are_never_a_finding() {
        let r = run(&[
            ("testing/conformance/clap.txt", ROW),
            ("crates/a/src/lib.rs", "fn t() {}\n"),
        ]);
        assert!(r.passed());
    }

    #[test]
    fn the_clap_inventory_must_equal_what_clap_sys_declares() {
        let sys = Mem::with(&[("src/ext/params.rs", PARAMS_SRC)]);
        let go = |rows: &str| {
            let mem = Mem::with(&[("Cargo.lock", LOCK), ("testing/conformance/clap.txt", rows)]);
            check(
                &mem,
                Ok(&sys),
                &|_| Err("no specs".to_owned()),
                Require::default(),
            )
        };
        assert!(go(ROW).passed());
        assert!(has(
            &go("x.y | host | | | | p | | |"),
            "clap-sys declares `clap.ext.params.flush`"
        ));
        let extra = format!("{ROW}\nclap.ext.params.zap | plugin | stable | | | p | | |");
        assert!(has(
            &go(&extra),
            "`clap.ext.params.zap` (plugin, stable) is not what clap-sys declares"
        ));
        let wrong_side = ROW.replace("plugin | stable", "host | stable");
        assert!(has(
            &go(&wrong_side),
            "(host, stable) is not what clap-sys declares"
        ));
    }

    #[test]
    fn without_the_clap_sys_source_the_check_is_skipped_unless_required() {
        let mem = Mem::with(&[("Cargo.lock", LOCK), ("testing/conformance/clap.txt", ROW)]);
        let skipped = check(
            &mem,
            Err("clap-sys 0.5.0 not found".to_owned()),
            &|_| Err("no specs".to_owned()),
            Require::default(),
        );
        assert!(skipped.passed());
        assert!(
            skipped
                .out
                .iter()
                .any(|l| l.contains("clap-sys 0.5.0 not found"))
        );
        let required = check(
            &mem,
            Err("clap-sys 0.5.0 not found".to_owned()),
            &|_| Err("no specs".to_owned()),
            Require::parse(Some("require")),
        );
        assert!(has(&required, "clap-sys 0.5.0 not found"));
    }

    #[test]
    fn the_pinned_version_is_read_from_the_lock() {
        assert_eq!(pinned_clap_sys(LOCK).as_deref(), Some("0.5.0"));
        assert_eq!(pinned_clap_sys("name = \"other\"\n"), None);
    }

    #[test]
    fn regeneration_keeps_hand_columns_of_surviving_ids_and_drops_the_rest() {
        let kept = "clap.ext.params.flush | plugin | stable | required | §1 | my words | n=0x2 | deprecated | clap ext/params.h\n\
                    clap.gone | host | | | | old | | |\n";
        let items = [
            Item {
                id: "clap.ext.params.flush".to_owned(),
                side: "host",
                maturity: "draft",
            },
            Item {
                id: "clap.ext.params.count".to_owned(),
                side: "plugin",
                maturity: "stable",
            },
        ];
        let out = regenerate(Some(kept), &items).unwrap();
        let rows: Vec<&str> = out.lines().filter(|l| !l.starts_with('#')).collect();
        assert_eq!(
            rows,
            [
                "clap.ext.params.flush | host | draft | required | §1 | my words | n=0x2 | deprecated | clap ext/params.h",
                "clap.ext.params.count | plugin | stable |  |  |  |  |  |",
            ]
        );
        assert!(regenerate(Some("bad line\n"), &items).is_err());
    }

    const MIDI_TEXT: &str = "RUNNING\n 1\n\x0cRUNNING\n 2\n\x0cRUNNING\n 3\n\x0c1 Alpha\nA frob shall spin.\nTable 4 Allocation\n0x0 32 bits\n0x1 32 bits\n0x2 32 bits\n0x3 64 bits\n0x4 64 bits\n0x5 128 bits\n0x6 32 bits\n0x7 32 bits\n0x8 64 bits\n0x9 64 bits\n0xA 64 bits\n0xB 96 bits\n0xC 96 bits\n0xD 128 bits\n0xE 128 bits\n0xF 128 bits\nFigure 4 End\nTable 34 Messages\nUTILITY NOOP 0x0 res. 0x0 20\nFlex Data Set Tempo 0xD 0xg 0 1 0 0x00 0x00 n\nTable 16 Text\n0x01 0x00 Unknown\nFigure 5 End\n";

    fn midi_rows() -> String {
        let items = midi_spec::extract(&midi_spec::M2_104, MIDI_TEXT).unwrap();
        let (text, added, removed) = regenerate_midi(&midi_spec::M2_104, None, &items).unwrap();
        assert_eq!(added.len(), items.len());
        assert!(removed.is_empty());
        text
    }

    #[test]
    fn the_midi_rows_must_equal_what_the_text_gives() {
        // Rebuild each row with a paraphrase in column six.
        let with_words: Vec<String> = midi_rows()
            .lines()
            .filter(|l| !l.starts_with('#'))
            .map(|l| {
                let mut c: Vec<&str> = l.split(" | ").collect();
                c[1] = "both";
                c[5] = "words";
                c.join(" | ")
            })
            .collect();
        let good = with_words.join("\n");
        let go = |inv: &str, text: Result<&str, String>| {
            let mem = Mem::with(&[("testing/conformance/midi-m2-104.txt", inv)]);
            let only_104 = |doc: &midi_spec::Doc| match (&text, doc.prefix) {
                (Ok(t), "m2-104") => Ok((*t).to_owned()),
                (Err(e), _) => Err(e.clone()),
                _ => Err("no M2-116 text".to_owned()),
            };
            check(
                &mem,
                Err("absent".to_owned()),
                &only_104,
                Require::default(),
            )
        };
        assert!(
            go(&good, Ok(MIDI_TEXT)).passed(),
            "{:?}",
            go(&good, Ok(MIDI_TEXT)).findings
        );
        let wrong_kind = good.replacen("| required |", "| recommended |", 1);
        assert!(has(
            &go(&wrong_kind, Ok(MIDI_TEXT)),
            "the M2-104 row `m2-104/1#1` (recommended"
        ));
        assert!(has(
            &go(&wrong_kind, Ok(MIDI_TEXT)),
            "the M2-104 text gives `m2-104/1#1` (required"
        ));
        let dropped: String = good.lines().skip(1).collect::<Vec<_>>().join("\n");
        assert!(has(&go(&dropped, Ok(MIDI_TEXT)), "gives `m2-104/1#1`"));
    }

    #[test]
    fn without_the_midi_text_the_check_is_skipped_unless_require_all() {
        let lock = Mem::with(&[
            ("Cargo.lock", LOCK),
            ("testing/conformance/clap.txt", ROW),
            ("testing/conformance/vst3.txt", VST3_ROW),
            (VST3_HEADER_PATH, VST3_SRC),
        ]);
        let sys = Mem::with(&[("src/ext/params.rs", PARAMS_SRC)]);
        let run = |v: Option<&str>| {
            check(
                &lock,
                Ok(&sys),
                &|_| Err("TUTTI_MIDI_SPECS is not set".to_owned()),
                Require::parse(v),
            )
        };
        for v in [None, Some("require")] {
            let r = run(v);
            assert!(r.passed(), "{:?}", r.findings);
            assert!(
                r.out
                    .iter()
                    .any(|l| l.contains("M2-104 inventory not checked"))
            );
        }
        assert!(has(
            &run(Some("require-all")),
            "M2-104 inventory not checked: TUTTI_MIDI_SPECS"
        ));
    }

    #[test]
    fn the_vst3_rows_are_held_to_the_sdk_headers() {
        let go = |row: &str| {
            check(
                &Mem::with(&[
                    ("testing/conformance/vst3.txt", row),
                    (VST3_HEADER_PATH, VST3_SRC),
                ]),
                Err("absent".to_owned()),
                &|_| Err("no specs".to_owned()),
                Require::default(),
            )
        };
        assert!(go(VST3_ROW).passed());
        let wrong_side = VST3_ROW.replace("| plugin |", "| host |");
        assert!(has(&go(&wrong_side), "is not what the VST3 SDK declares"));
        assert!(has(&go(""), "declares `vst3.IComponent.setActive`"));
        let r = run(&[("testing/conformance/vst3.txt", VST3_ROW)]);
        assert!(r.passed() && has_out(&r, "vst3 inventory not checked"));
    }

    #[test]
    fn vst3_regeneration_keeps_hand_columns_and_reports_added_and_removed() {
        let items = vst3_sdk::extract(&Mem::with(&[(VST3_HEADER_PATH, VST3_SRC)])).unwrap();
        let kept = format!(
            "{} | host | draft | required | stale | my words | | |\nvst3.IGone.x | core | stable | | | old | | |\n",
            items[0].id
        );
        let (out, added, removed) = regenerate_vst3(Some(&kept), &items).unwrap();
        let row = out.lines().find(|l| l.starts_with(&items[0].id)).unwrap();
        assert_eq!(
            row,
            "vst3.IComponent.setActive | plugin | stable | required | vst/ivstcomponent.h | my words |  |  |"
        );
        assert!(added.is_empty());
        assert_eq!(removed, ["vst3.IGone.x"]);
        let (_, added, _) = regenerate_vst3(None, &items).unwrap();
        assert_eq!(added, ["vst3.IComponent.setActive"]);
    }

    #[test]
    fn midi_regeneration_keeps_hand_columns_flags_new_ids_and_reports_removed() {
        let items = midi_spec::extract(&midi_spec::M2_104, MIDI_TEXT).unwrap();
        let first = &items[0];
        let kept = format!(
            "{} | sender | | required | stale | my words | | deprecated | cite\nm2-104/gone#1 | both | | | | old | | |\n",
            first.id
        );
        let (out, added, removed) =
            regenerate_midi(&midi_spec::M2_104, Some(&kept), &items).unwrap();
        let row = out.lines().find(|l| l.starts_with(&first.id)).unwrap();
        assert_eq!(
            row,
            format!(
                "{} | sender |  | required | {} | my words |  | deprecated | cite",
                first.id, first.section
            )
        );
        assert_eq!(removed, ["m2-104/gone#1"]);
        assert_eq!(added.len(), items.len() - 1);
        let new = out.lines().find(|l| l.starts_with("m2-104/mt/0")).unwrap();
        assert!(new.contains("| todo |"));
        let r = run(&[("testing/conformance/midi-m2-104.txt", &out)]);
        assert!(has(&r, "has an empty paraphrase"));
    }

    #[test]
    fn each_document_is_held_to_its_own_text() {
        let text_116 = "RUNNING\n 1\n\x0cRUNNING\n 2\n\x0cRUNNING\n 3\n\x0c1 Alpha\nA frob shall spin.\nTable 5 Header\nByte 1  Byte 2\nValue  0x53 0x4D 0x46 0x32 0x43 0x4C 0x49 0x50\nFigure 1 End\n";
        let items = midi_spec::extract(&midi_spec::M2_116, text_116).unwrap();
        let (inv, _, _) = regenerate_midi(&midi_spec::M2_116, None, &items).unwrap();
        assert!(inv.starts_with("# MIDI Clip File (M2-116 v1.0) inventory."));
        assert!(inv.contains("m2-116/hdr/<n>"));
        assert!(!inv.contains("m2-116/msg"));
        let rows: String = inv
            .lines()
            .filter(|l| !l.starts_with('#'))
            .map(|l| {
                let mut c: Vec<&str> = l.split(" | ").collect();
                c[1] = "both";
                c[5] = "words";
                c.join(" | ") + "\n"
            })
            .collect();
        let mem = Mem::with(&[("testing/conformance/midi-m2-116.txt", &rows)]);
        let go = |t: &'static str| {
            check(
                &mem,
                Err("absent".to_owned()),
                &move |d: &midi_spec::Doc| {
                    if d.prefix == "m2-116" {
                        Ok(t.to_owned())
                    } else {
                        Err("no M2-104 text".to_owned())
                    }
                },
                Require::default(),
            )
        };
        assert!(go(text_116).passed(), "{:?}", go(text_116).findings);
        let other = text_116.replace("0x50", "0x51");
        let other: &'static str = Box::leak(other.into_boxed_str());
        assert!(has(&go(other), "the M2-116 text gives `m2-116/hdr/8`"));
    }
}
