//! `check-conformance`: the spec inventories are well formed, every test tag
//! names a row, and the CLAP inventory is the set `clap-sys` declares.
//!
//! An inventory is one or more files `testing/conformance/<area>[-<part>].txt`,
//! one spec item per line, nine columns separated by `|`:
//!
//! ```text
//! id | side | maturity | kind | section | paraphrase | facts | na-kind | cite
//! ```
//!
//! Blank lines and lines starting with `#` are skipped. `id` is non-empty and
//! made of `a-z 0-9 . _ / # -`, unique across every file. `side` is a non-empty
//! lowercase token (`host`, `plugin`, `core`, `initiator`, ..). `maturity` is
//! `stable`, `draft` or empty; `kind` is `required`, `recommended`, `permitted`
//! or empty; `section` is free text or empty; `paraphrase` is the item in our
//! own words and is not empty in a committed row; `facts` is empty or space-separated `name=int`
//! pairs (`name` a lowercase identifier, `int` decimal or `0x` hex, no name
//! twice). `na-kind` (`deprecated`, `forbidden-for-side`, `other-side`) and
//! `cite` (the spec location) are both empty or both set. A row with the wrong
//! column count or any invalid column is an error naming the file and line;
//! nothing is half-read.
//!
//! A test claims rows with a last rustdoc line `/// Spec: id, id` on its `fn`
//! (or above a case in the plugin contract's list). A tag naming an id no
//! inventory holds, an empty tag, or a tag that is not the last doc line is a
//! finding. Rows no test claims are Missing, which is the work list and never
//! a finding.
//!
//! The rows whose id starts `clap.` are held to the `clap-sys` source the lock
//! file pins ([`clap_sys`](super::clap_sys)): the committed `(id, side,
//! maturity)` triples equal the extracted ones. With the source absent the
//! comparison is skipped and said so, unless `TUTTI_CONFORMANCE=require`.

use super::clap_sys::{self, Item};
use super::lex::blank_keeping_docs;
use super::{Report, Tree};
use std::collections::{BTreeMap, BTreeSet};

const DIR: &str = "testing/conformance";
const NA_KINDS: [&str; 3] = ["deprecated", "forbidden-for-side", "other-side"];
const KINDS: [&str; 3] = ["required", "recommended", "permitted"];
const MATURITIES: [&str; 2] = ["stable", "draft"];
const TAG: &str = "/// Spec:";

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
            .all(|b| matches!(b, b'a'..=b'z' | b'0'..=b'9' | b'.' | b'_' | b'/' | b'#' | b'-'))
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
            "id `{id}` is empty or has a character outside a-z 0-9 . _ / # -"
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

pub(super) fn check(tree: &dyn Tree, clap: Result<&dyn Tree, String>, require: bool) -> Report {
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
        Err(why) if require => r.fail(format!("clap inventory not checked: {why}")),
        Err(why) => r.say(format!("clap inventory not checked: {why}")),
    }
    r.say(format!(
        "check-conformance: {} rows, {} ids tagged",
        rows.len(),
        tagged.len()
    ));
    r
}

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
    let mut out = String::from(
        "# CLAP spec inventory. `cargo xtask conformance-inventory clap` regenerates the\n\
         # id, side and maturity columns from clap-sys; the others are edited by hand.\n\
         # id | side | maturity | kind | section | paraphrase | facts | na-kind | cite\n",
    );
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::gates::fake::Mem;

    const ROW: &str = "clap.ext.params.flush | plugin | stable | | | flush parameter changes without processing | | |";
    const LOCK: &str = "[[package]]\nname = \"clap-sys\"\nversion = \"0.5.0\"\n";
    const PARAMS_SRC: &str =
        "pub struct clap_plugin_params { pub flush: Option<unsafe extern \"C\" fn()>, }";

    fn inv(rows: &[&str]) -> String {
        rows.join("\n")
    }

    fn run(files: &[(&str, &str)]) -> Report {
        let mem = Mem::with(files);
        check(&mem, Err("absent".to_owned()), false)
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
            (0, "A.b", "id"),
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
            check(&mem, Ok(&sys), false)
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
        let skipped = check(&mem, Err("clap-sys 0.5.0 not found".to_owned()), false);
        assert!(skipped.passed());
        assert!(
            skipped
                .out
                .iter()
                .any(|l| l.contains("clap-sys 0.5.0 not found"))
        );
        let required = check(&mem, Err("clap-sys 0.5.0 not found".to_owned()), true);
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
}
