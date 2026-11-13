//! `conformance`: the spec inventories joined to the tests' results, per OS.
//!
//! A row of `testing/conformance/<format>*.txt` is claimed by the tests whose
//! last doc line is `/// Spec: id, id` ([`super::conformance`]). Each OS's
//! JUnit file ([`super::junit`]) says how those tests ended there, and the
//! join gives every (row, OS) one [`Status`]:
//!
//! - `Na` when the row carries an `na-kind` and its citation;
//! - else `Failing` when a test claiming the row failed;
//! - else `Covered` when one passed (a skipped test did not run, so it covers
//!   nothing);
//! - else `Missing`, or `NoResults` when no test claiming any row of the area
//!   ran on that OS at all (the column has no data, so its rows are not
//!   missing coverage; the area-OS pair gets no badge).
//!
//! A plugin-contract case runs once per format and its tag lists every format's
//! ids, so a result of the binary `tutti-plugin-contract::<format>` covers only
//! that format's ids, and a case the format declares a `Gap` for (it passes by
//! failing the way the gap says) covers nothing. Nothing here is declined: a
//! row is covered, failing, missing, or not applicable by the spec's own text.
//!
//! Everything is a function of the inventory text, the sources and the JUnit
//! text, and returns the dashboard and the badges as strings.

use super::Tree;
use super::conformance::{self, Row};
use super::junit::{self, Case, Outcome};
use super::lex::blank_keeping_docs;
use std::collections::{BTreeMap, BTreeSet};

/// The operating systems a dashboard column is for, in column order.
pub(super) const OSES: [&str; 3] = ["linux", "macos", "windows"];

const CONTRACT: &str = "tutti-plugin-contract";
const TAG: &str = "/// Spec:";

/// One (row, OS) cell.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Status {
    Covered,
    Failing,
    Missing,
    /// Nothing of the area ran on the OS.
    NoResults,
    Na,
}

/// One row with its status and the tests behind it on each OS.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct RowReport {
    pub(super) row: Row,
    pub(super) status: [Status; 3],
    /// `binary: test` of every claiming test that ran on the OS, with its outcome.
    pub(super) tests: [Vec<(String, Outcome)>; 3],
}

/// The rows of one format, in inventory order.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct FormatReport {
    pub(super) name: String,
    pub(super) rows: Vec<RowReport>,
}

impl FormatReport {
    /// Whether any test claiming a row of the area ran on OS column `os`.
    pub(super) fn has_results(&self, os: usize) -> bool {
        self.rows.iter().any(|r| !r.tests[os].is_empty())
    }

    /// `(covered, failing, applicable)` on OS column `os`; a not-applicable
    /// row is out of `applicable`.
    pub(super) fn counts(&self, os: usize) -> (usize, usize, usize) {
        let count = |s: Status| self.rows.iter().filter(|r| r.status[os] == s).count();
        let (covered, failing, na) = (
            count(Status::Covered),
            count(Status::Failing),
            count(Status::Na),
        );
        (covered, failing, self.rows.len() - na)
    }
}

/// The file stem of an inventory path up to its first `-`: `midi-m2-104.txt`
/// is the `midi` format.
fn format_of(path: &str) -> &str {
    let file = path.rsplit('/').next().unwrap_or(path);
    let stem = file.strip_suffix(".txt").unwrap_or(file);
    stem.split('-').next().unwrap_or(stem)
}

/// `(package, fn name)` to the ids its `/// Spec:` tag claims. The package is
/// the directory under `crates/` or `testing/` the file sits in.
fn claims(sources: &[(String, String)]) -> BTreeMap<(String, String), BTreeSet<String>> {
    let mut out: BTreeMap<(String, String), BTreeSet<String>> = BTreeMap::new();
    for (path, text) in sources {
        let mut parts = path.split('/');
        let (Some(_), Some(package)) = (parts.next(), parts.next()) else {
            continue;
        };
        let code = blank_keeping_docs(text);
        let lines: Vec<&str> = code.lines().collect();
        for (i, l) in lines.iter().enumerate() {
            let Some(rest) = l.trim_start().strip_prefix(TAG) else {
                continue;
            };
            let Some(name) = lines[i + 1..].iter().take(12).find_map(|n| fn_name(n)) else {
                continue;
            };
            out.entry((package.to_owned(), name))
                .or_default()
                .extend(rest.split(',').map(|s| s.trim().to_owned()));
        }
    }
    out
}

/// The name a line declares with `fn`, when it does.
fn fn_name(line: &str) -> Option<String> {
    let t = line.trim_start();
    if t.starts_with("//") || t.starts_with('#') {
        return None;
    }
    let after = super::lex::find_word(t, "fn").map(|at| &t[at + 2..])?;
    let name: String = after
        .trim_start()
        .chars()
        .take_while(|c| c.is_alphanumeric() || *c == '_')
        .collect();
    (!name.is_empty()).then_some(name)
}

/// `(format name, case)` of every case a `Format` impl in `formats_rs` lists
/// in its `GAPS`: by a string literal that opens a `(case, Gap { .. })` pair,
/// or by the name of a `const NAME: (&str, Gap)` that does.
fn gapped(formats_rs: &str) -> BTreeSet<(String, String)> {
    let first_literal = |s: &str| {
        let open = s.find('"')? + 1;
        Some(s[open..open + s[open..].find('"')?].to_owned())
    };
    let mut named: BTreeMap<String, String> = BTreeMap::new();
    for chunk in formats_rs.split("const ").skip(1) {
        let Some((name, rest)) = chunk.split_once(':') else {
            continue;
        };
        if let Some(pair) = rest.trim_start().strip_prefix("(&str, Gap)")
            && let Some(case) = first_literal(pair)
        {
            named.insert(name.trim().to_owned(), case);
        }
    }
    let mut out = BTreeSet::new();
    for chunk in formats_rs.split("impl Format for ").skip(1) {
        let Some(name) = chunk
            .split_once("const NAME")
            .and_then(|(_, r)| first_literal(r))
        else {
            continue;
        };
        let Some(list) = chunk
            .split_once("const GAPS")
            .and_then(|(_, r)| r.split_once("= &[")?.1.split_once("];"))
            .map(|(l, _)| l)
        else {
            continue;
        };
        for (konst, case) in &named {
            if list
                .split(|c: char| !(c.is_alphanumeric() || c == '_'))
                .any(|w| w == konst)
            {
                out.insert((name.clone(), case.clone()));
            }
        }
        let mut rest = list;
        while let Some(q) = rest.find('"') {
            let Some(len) = rest[q + 1..].find('"') else {
                break;
            };
            let lit = &rest[q + 1..q + 1 + len];
            rest = &rest[q + 2 + len..];
            if rest
                .trim_start()
                .strip_prefix(',')
                .is_some_and(|r| r.trim_start().starts_with("Gap"))
            {
                out.insert((name.clone(), lit.to_owned()));
            }
        }
    }
    out
}

/// The ids a result of `case` covers: the tag's ids, narrowed to the
/// result's own format for a plugin-contract case, and none for a gapped one.
fn covers<'a>(
    case: &Case,
    ids: &'a BTreeSet<String>,
    gaps: &BTreeSet<(String, String)>,
) -> Vec<&'a str> {
    let test = case.name.rsplit("::").next().unwrap_or(&case.name);
    let (package, suffix) = case.binary.split_once("::").unwrap_or((&case.binary, ""));
    if package != CONTRACT {
        return ids.iter().map(String::as_str).collect();
    }
    let format = suffix.replace('_', "-");
    if gaps.contains(&(format.clone(), test.to_owned())) {
        return Vec::new();
    }
    let prefix = format!("{}.", format.split('-').next().unwrap_or(""));
    ids.iter()
        .map(String::as_str)
        .filter(|id| suffix.is_empty() || id.starts_with(&prefix))
        .collect()
}

/// The report over the checkout `tree` and one JUnit text per OS (`results`:
/// `(os, xml)`): one [`FormatReport`] per area the inventory files name, in
/// path order, the parts of an area (`midi-m2-104.txt`, `midi-m2-116.txt`)
/// joined.
pub(super) fn report(
    tree: &dyn Tree,
    results: &[(String, String)],
) -> Result<Vec<FormatReport>, String> {
    let (rows, errs) = conformance::load(tree);
    if !errs.is_empty() {
        return Err(format!(
            "the inventories are malformed:\n{}",
            errs.join("\n")
        ));
    }
    let mut sources = Vec::new();
    for root in ["crates", "testing"] {
        for path in tree.files(root, &|d| matches!(d, "target" | ".git")) {
            if path.ends_with(".rs")
                && let Some(text) = tree.read(&path)
            {
                sources.push((path, text));
            }
        }
    }
    let claims = claims(&sources);
    let gaps = tree
        .read("testing/tutti-plugin-contract/src/formats.rs")
        .map(|t| gapped(&t))
        .unwrap_or_default();

    // id -> per OS -> the results of the tests that claim it.
    let mut seen: BTreeMap<&str, [Vec<(String, Outcome)>; 3]> = BTreeMap::new();
    for (os, xml) in results {
        let col = OSES
            .iter()
            .position(|o| o == os)
            .ok_or_else(|| format!("`{os}` is not an OS column ({})", OSES.join(", ")))?;
        for case in junit::parse(xml).map_err(|e| format!("{os}: {e}"))? {
            let package = case.binary.split("::").next().unwrap_or(&case.binary);
            let test = case.name.rsplit("::").next().unwrap_or(&case.name);
            let Some(ids) = claims.get(&(package.to_owned(), test.to_owned())) else {
                continue;
            };
            for id in covers(&case, ids, &gaps) {
                seen.entry(id).or_default()[col]
                    .push((format!("{}: {}", case.binary, case.name), case.outcome));
            }
        }
    }

    let mut areas: Vec<&str> = Vec::new();
    for (path, _) in &rows {
        let area = format_of(path);
        if !areas.contains(&area) {
            areas.push(area);
        }
    }
    let mut out = Vec::new();
    for area in areas {
        let reports = rows
            .iter()
            .filter(|(path, _)| format_of(path) == area)
            .map(|(_, row)| {
                let tests = seen.get(row.id.as_str()).cloned().unwrap_or_default();
                let status = std::array::from_fn(|os| {
                    let ran = &tests[os];
                    if !row.na_kind.is_empty() {
                        Status::Na
                    } else if ran.iter().any(|(_, o)| *o == Outcome::Failed) {
                        Status::Failing
                    } else if ran.iter().any(|(_, o)| *o == Outcome::Passed) {
                        Status::Covered
                    } else {
                        Status::Missing
                    }
                });
                RowReport {
                    row: row.clone(),
                    status,
                    tests,
                }
            })
            .collect();
        let mut report = FormatReport {
            name: area.to_owned(),
            rows: reports,
        };
        for os in 0..OSES.len() {
            if !report.has_results(os) {
                for r in &mut report.rows {
                    if r.status[os] == Status::Missing {
                        r.status[os] = Status::NoResults;
                    }
                }
            }
        }
        out.push(report);
    }
    Ok(out)
}

fn esc(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
}

fn json_str(s: &str) -> String {
    let mut out = String::from("\"");
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            c if (c as u32) < 0x20 => out.push_str(&format!("\\u{:04x}", c as u32)),
            c => out.push(c),
        }
    }
    out.push('"');
    out
}

/// The shields.io endpoint for one format on one OS.
pub(super) fn badge(format: &FormatReport, os: usize) -> String {
    let (covered, failing, applicable) = format.counts(os);
    let color = if applicable == 0 {
        "lightgrey"
    } else if failing > 0 {
        "red"
    } else {
        match covered * 100 / applicable {
            100 => "brightgreen",
            80..=99 => "green",
            50..=79 => "yellow",
            20..=49 => "orange",
            _ => "red",
        }
    };
    format!(
        "{{\"schemaVersion\":1,\"label\":{},\"message\":{},\"color\":{}}}\n",
        json_str(&format!("{} {}", format.name, OSES[os])),
        json_str(&format!("{covered}/{applicable}")),
        json_str(color),
    )
}

/// `(path under the site root, contents)` of every badge, `badges/<format>-<os>.json`.
pub(super) fn badges(formats: &[FormatReport]) -> Vec<(String, String)> {
    formats
        .iter()
        .flat_map(|f| {
            (0..OSES.len())
                .filter(|&os| f.has_results(os))
                .map(|os| (format!("{}-{}.json", f.name, OSES[os]), badge(f, os)))
        })
        .collect()
}

const STYLE: &str = "\
:root{color-scheme:light dark;--ok:#2da44e;--bad:#cf222e;--none:#8c959f;--na:#6e7781}\
body{font:14px/1.4 system-ui,sans-serif;margin:1rem auto;max-width:78rem;padding:0 1rem}\
h1,h2{margin:.6em 0 .3em}nav a{margin-right:1rem}\
table{border-collapse:collapse;width:100%;margin-bottom:2rem}\
th,td{border:1px solid #8884;padding:.25rem .5rem;text-align:left;vertical-align:top}\
td.id{font-family:ui-monospace,monospace;white-space:nowrap}\
td.s{text-align:center;color:#fff;font-weight:600;white-space:nowrap;width:5.5rem}\
td.covered{background:var(--ok)}td.failing{background:var(--bad)}\
td.missing{background:var(--none)}td.noresults{background:transparent;color:var(--na);font-weight:400}td.na{background:var(--na)}\
#filter{width:100%;max-width:30rem;padding:.4rem;margin:.5rem 0}\
.counts{margin:0 0 .5rem}.counts span{margin-right:1.2rem}\
tr.hide{display:none}";

const SCRIPT: &str = "\
var box=document.getElementById('filter');\
box.addEventListener('input',function(){\
var q=box.value.toLowerCase();\
document.querySelectorAll('tbody tr').forEach(function(tr){\
tr.classList.toggle('hide',q!==''&&tr.textContent.toLowerCase().indexOf(q)<0);});});";

fn cell(r: &RowReport, os: usize) -> String {
    let (class, text) = match r.status[os] {
        Status::Covered => ("covered", "pass"),
        Status::Failing => ("failing", "FAIL"),
        Status::Missing => ("missing", "missing"),
        Status::NoResults => ("noresults", "no results"),
        Status::Na => ("na", "n/a"),
    };
    let title = if r.status[os] == Status::Na {
        format!("{}: {}", r.row.na_kind, r.row.cite)
    } else {
        r.tests[os]
            .iter()
            .map(|(t, o)| {
                format!(
                    "{} {t}",
                    if *o == Outcome::Passed {
                        "passed"
                    } else {
                        "did not pass"
                    }
                )
            })
            .collect::<Vec<_>>()
            .join("\n")
    };
    format!(
        "<td class=\"s {class}\" title=\"{}\">{text}</td>",
        esc(&title)
    )
}

/// The site's landing page: relative links to the dashboard and the coverage
/// report, which sit beside it.
pub(super) fn landing() -> String {
    "<!doctype html>\n<html lang=\"en\"><head><meta charset=\"utf-8\">\
<meta name=\"viewport\" content=\"width=device-width,initial-scale=1\">\
<title>tutti</title></head><body>\n<h1>tutti</h1>\n<ul>\n\
<li><a href=\"conformance/index.html\">Spec conformance</a></li>\n\
<li><a href=\"coverage/html/index.html\">Line coverage</a></li>\n\
</ul>\n</body></html>\n"
        .to_owned()
}

/// The dashboard page: one table per format, rows by OS, with the counts and
/// a filter box. Plain HTML, inline CSS and JS, no external reference.
pub(super) fn dashboard(formats: &[FormatReport]) -> String {
    let mut h = String::from(
        "<!doctype html>\n<html lang=\"en\"><head><meta charset=\"utf-8\">\
<meta name=\"viewport\" content=\"width=device-width,initial-scale=1\">\
<title>tutti plugin conformance</title>\n<style>",
    );
    h.push_str(STYLE);
    h.push_str("</style></head><body>\n<h1>Plugin spec conformance</h1>\n");
    h.push_str(
        "<p>Each row is one item of a format's spec. A cell is <b>pass</b> when a test \
claiming the row passed on that OS, <b>FAIL</b> when one failed, <b>missing</b> when none ran on an OS that has results for the area, \
<b>no results</b> when nothing of the area ran on that OS, \
and <b>n/a</b> when the spec itself excludes the row (hover for the citation). \
<a href=\"../coverage/html/index.html\">Line coverage</a>.</p>\n",
    );
    h.push_str("<nav>");
    for f in formats {
        h.push_str(&format!("<a href=\"#{0}\">{0}</a>", esc(&f.name)));
    }
    h.push_str("</nav>\n<input id=\"filter\" type=\"search\" placeholder=\"filter rows (id, text, status)\" autocomplete=\"off\">\n");
    for f in formats {
        h.push_str(&format!(
            "<h2 id=\"{0}\">{0}</h2>\n<p class=\"counts\">",
            esc(&f.name)
        ));
        for (os, name) in OSES.iter().enumerate() {
            if f.has_results(os) {
                let (covered, failing, applicable) = f.counts(os);
                h.push_str(&format!(
                    "<span>{name}: {covered}/{applicable} covered, {failing} failing</span>"
                ));
            } else {
                h.push_str(&format!("<span>{name}: no results</span>"));
            }
        }
        h.push_str("</p>\n<table><thead><tr><th>id</th><th>side</th><th>kind</th><th>item</th>");
        for os in OSES {
            h.push_str(&format!("<th>{os}</th>"));
        }
        h.push_str("</tr></thead><tbody>\n");
        for r in &f.rows {
            h.push_str(&format!(
                "<tr><td class=\"id\">{}</td><td>{}</td><td>{}</td><td>{}</td>",
                esc(&r.row.id),
                esc(&r.row.side),
                esc(&r.row.kind),
                esc(&r.row.paraphrase),
            ));
            for os in 0..OSES.len() {
                h.push_str(&cell(r, os));
            }
            h.push_str("</tr>\n");
        }
        h.push_str("</tbody></table>\n");
    }
    h.push_str("<script>");
    h.push_str(SCRIPT);
    h.push_str("</script></body></html>\n");
    h
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::gates::fake::Mem;

    const CLAP: &str = "\
clap.a | plugin | stable | required | | the a call | | |
clap.b | plugin | stable | | | the b call | | |
clap.c | plugin | stable | | | the c call | | |
clap.d | plugin | stable | | | the d call | | deprecated | clap 1.2 marks it deprecated
";
    const VST3: &str = "vst3.x | plugin | stable | | | the x call | | |\n";
    const CASES: &str = "\
/// Spec: clap.a, vst3.x
pub fn shared<F>() {}
/// Spec: clap.b, vst3.x
pub fn gapped_one<F>() {}
";
    const FORMATS: &str = "\
const NO_X: (&str, Gap) = (
    \"gapped_one\",
    Gap { why: \"w\", fails_with: \"f\" },
);
impl Format for Clap {
    const NAME: &'static str = \"clap\";
    const GAPS: &'static [(&'static str, Gap)] = &[];
}
impl Format for Vst3 {
    const NAME: &'static str = \"vst3\";
    const GAPS: &'static [(&'static str, Gap)] = &[NO_X];
}
";
    const HOST: &str = "/// Spec: clap.c\n#[test]\nfn c_works() {}\n";

    fn tree() -> Mem {
        Mem::with(&[
            ("testing/conformance/clap.txt", CLAP),
            ("testing/conformance/vst3.txt", VST3),
            ("testing/tutti-plugin-contract/src/cases.rs", CASES),
            ("testing/tutti-plugin-contract/src/formats.rs", FORMATS),
            ("crates/tutti-clap-host/src/lib.rs", HOST),
        ])
    }

    fn xml(cases: &[(&str, &str, &str)]) -> String {
        let mut s = String::from("<testsuites>");
        for (bin, name, kids) in cases {
            s.push_str(&format!(
                "<testcase name=\"{name}\" classname=\"{bin}\">{kids}</testcase>"
            ));
        }
        s + "</testsuites>"
    }

    fn run(results: &[(&str, String)]) -> Vec<FormatReport> {
        let results: Vec<(String, String)> = results
            .iter()
            .map(|(o, x)| ((*o).to_owned(), x.clone()))
            .collect();
        report(&tree(), &results).unwrap()
    }

    fn status(f: &FormatReport, id: &str, os: usize) -> Status {
        f.rows.iter().find(|r| r.row.id == id).unwrap().status[os]
    }

    #[test]
    fn a_row_is_covered_failing_missing_or_not_applicable_by_what_ran() {
        let linux = xml(&[
            ("tutti-plugin-contract::clap", "shared", ""),
            ("tutti-clap-host", "tests::c_works", "<failure/>"),
        ]);
        let r = run(&[("linux", linux)]);
        let clap = &r[0];
        assert_eq!(status(clap, "clap.a", 0), Status::Covered);
        assert_eq!(
            status(clap, "clap.b", 0),
            Status::Missing,
            "no result for its case"
        );
        assert_eq!(status(clap, "clap.c", 0), Status::Failing);
        assert_eq!(status(clap, "clap.d", 0), Status::Na);
        assert_eq!(
            status(clap, "clap.a", 1),
            Status::NoResults,
            "no macos file"
        );
        assert_eq!(status(clap, "clap.d", 2), Status::Na);
    }

    #[test]
    fn a_skipped_test_covers_nothing_and_a_pass_after_a_flake_covers() {
        let linux = xml(&[
            ("tutti-plugin-contract::clap", "shared", "<skipped/>"),
            (
                "tutti-plugin-contract::clap",
                "gapped_one",
                "<flakyFailure/>",
            ),
        ]);
        let clap = &run(&[("linux", linux)])[0];
        assert_eq!(status(clap, "clap.a", 0), Status::Missing);
        assert_eq!(status(clap, "clap.b", 0), Status::Covered);
    }

    #[test]
    fn a_contract_case_covers_only_the_format_it_ran_for() {
        let linux = xml(&[("tutti-plugin-contract::clap", "shared", "")]);
        let r = run(&[("linux", linux)]);
        assert_eq!(status(&r[0], "clap.a", 0), Status::Covered);
        assert_eq!(status(&r[1], "vst3.x", 0), Status::NoResults);
        let both = xml(&[
            ("tutti-plugin-contract::clap", "shared", ""),
            ("tutti-plugin-contract::vst3", "shared", ""),
        ]);
        assert_eq!(
            status(&run(&[("linux", both)])[1], "vst3.x", 0),
            Status::Covered
        );
    }

    #[test]
    fn a_gapped_case_that_passes_by_failing_covers_nothing_for_its_format() {
        let linux = xml(&[
            ("tutti-plugin-contract::vst3", "gapped_one", ""),
            ("tutti-plugin-contract::clap", "gapped_one", ""),
        ]);
        let r = run(&[("linux", linux)]);
        assert_eq!(status(&r[1], "vst3.x", 0), Status::NoResults);
        assert_eq!(
            status(&r[0], "clap.b", 0),
            Status::Covered,
            "clap declares no gap"
        );
    }

    #[test]
    fn the_gaps_are_read_from_consts_and_literals() {
        let src = format!(
            "{FORMATS}impl Format for Au {{\n const NAME: &'static str = \"au\";\n const GAPS: &'static [(&'static str, Gap)] = &[\n NO_X,\n (\n \"other\",\n Gap {{ why: \"w\", fails_with: \"f\" }},\n ),\n ];\n}}\n"
        );
        let want: BTreeSet<(String, String)> = [
            ("vst3", "gapped_one"),
            ("au", "gapped_one"),
            ("au", "other"),
        ]
        .iter()
        .map(|(a, b)| ((*a).to_owned(), (*b).to_owned()))
        .collect();
        assert_eq!(gapped(&src), want);
    }

    #[test]
    fn an_unknown_os_or_bad_xml_is_an_error() {
        let t = tree();
        assert!(report(&t, &[("beos".into(), "<a/>".into())]).is_err());
        assert!(report(&t, &[("linux".into(), "<testcase".into())]).is_err());
    }

    #[test]
    fn an_os_with_no_results_for_an_area_is_no_results_and_one_with_results_is_missing() {
        // clap ran on linux and macos; vst3 ran on macos only.
        let linux = xml(&[("tutti-plugin-contract::clap", "shared", "")]);
        let macos = xml(&[
            ("tutti-plugin-contract::clap", "shared", ""),
            ("tutti-plugin-contract::vst3", "shared", "<skipped/>"),
        ]);
        let r = run(&[("linux", linux), ("macos", macos)]);
        let (clap, vst3) = (&r[0], &r[1]);
        assert_eq!(
            status(clap, "clap.b", 0),
            Status::Missing,
            "linux has clap results"
        );
        assert_eq!(
            status(clap, "clap.b", 2),
            Status::NoResults,
            "windows has none"
        );
        assert_eq!(
            status(clap, "clap.d", 2),
            Status::Na,
            "the spec's own N/A stays"
        );
        assert_eq!(status(vst3, "vst3.x", 0), Status::NoResults);
        assert_eq!(
            status(vst3, "vst3.x", 1),
            Status::Missing,
            "a skipped test is a result"
        );
        let names: Vec<String> = badges(&r).into_iter().map(|b| b.0).collect();
        assert_eq!(
            names,
            ["clap-linux.json", "clap-macos.json", "vst3-macos.json"]
        );
        let page = dashboard(&r);
        assert!(page.contains("class=\"s noresults\""));
        assert!(page.contains("<span>windows: no results</span>"));
    }

    #[test]
    fn the_landing_page_links_the_dashboard_and_the_coverage_relatively() {
        let page = landing();
        assert!(page.contains("href=\"conformance/index.html\""));
        assert!(page.contains("href=\"coverage/html/index.html\""));
        assert!(!page.contains("http://") && !page.contains("https://"));
    }

    #[test]
    fn counts_leave_not_applicable_rows_out_of_the_total() {
        let linux = xml(&[("tutti-plugin-contract::clap", "shared", "")]);
        let clap = &run(&[("linux", linux)])[0];
        assert_eq!(clap.counts(0), (1, 0, 3));
    }

    #[test]
    fn badges_carry_the_count_and_a_colour() {
        let linux = xml(&[
            ("tutti-plugin-contract::clap", "shared", ""),
            ("tutti-clap-host", "c_works", ""),
            ("tutti-plugin-contract::clap", "gapped_one", ""),
        ]);
        let clap = &run(&[("linux", linux)])[0];
        assert_eq!(
            badge(clap, 0),
            "{\"schemaVersion\":1,\"label\":\"clap linux\",\"message\":\"3/3\",\"color\":\"brightgreen\"}\n"
        );
        let names: Vec<String> = badges(std::slice::from_ref(clap))
            .into_iter()
            .map(|b| b.0)
            .collect();
        assert_eq!(names, ["clap-linux.json"], "no badge without results");
    }

    #[test]
    fn a_failing_row_turns_the_badge_red_and_the_page_shows_it() {
        let linux = xml(&[("tutti-clap-host", "c_works", "<failure/>")]);
        let r = run(&[("linux", linux)]);
        assert!(badge(&r[0], 0).contains("\"red\""));
        let page = dashboard(&r);
        assert!(page.contains("class=\"s failing\""));
        assert!(page.contains("class=\"s na\" title=\"deprecated: clap 1.2 marks it deprecated\""));
        assert!(page.contains("id=\"filter\"") && page.contains("<h2 id=\"vst3\">"));
        assert!(
            !page.contains("http://") && !page.contains("https://"),
            "no external reference"
        );
    }

    #[test]
    fn a_new_area_needs_only_an_inventory_and_tags() {
        let mut mem = tree();
        mem.files.insert(
            "testing/conformance/zed-part1.txt".into(),
            "zed.one | host | | | | first | | |\n".into(),
        );
        mem.files.insert(
            "testing/conformance/zed-part2.txt".into(),
            "zed.two | host | | | | second | | |\n".into(),
        );
        mem.files.insert(
            "crates/zed-core/src/lib.rs".into(),
            "/// Spec: zed.two\n#[test]\nfn two() {}\n".into(),
        );
        let results = [("macos".to_owned(), xml(&[("zed-core", "tests::two", "")]))];
        let r = report(&mem, &results).unwrap();
        let names: Vec<&str> = r.iter().map(|f| f.name.as_str()).collect();
        assert_eq!(names, ["clap", "vst3", "zed"]);
        let zed = &r[2];
        assert_eq!(zed.rows.len(), 2, "the parts are one area");
        assert_eq!(status(zed, "zed.two", 1), Status::Covered);
        assert_eq!(status(zed, "zed.one", 1), Status::Missing);
        assert!(badges(&r).iter().any(|(n, _)| n == "zed-macos.json"));
        assert!(dashboard(&r).contains("<h2 id=\"zed\">"));
    }

    #[test]
    fn page_text_is_escaped() {
        let mem = Mem::with(&[(
            "testing/conformance/clap.txt",
            "clap.a | plugin | | | | a <b> & \"c\" | | |\n",
        )]);
        let r = report(&mem, &[]).unwrap();
        let page = dashboard(&r);
        assert!(page.contains("a &lt;b&gt; &amp; &quot;c&quot;"));
    }
}
