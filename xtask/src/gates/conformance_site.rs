//! `conformance`: the spec inventories joined to the tests' results, per OS.
//!
//! A row of `testing/conformance/<area>[-<part>].txt` is claimed by the tests whose
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
//! that format's ids. A case the format declares a `Gap` for passes by failing
//! the way the gap says, and the contract prints [`GAP_MARKER`] and the case's
//! name before it runs: a case whose output carries the line covers nothing.
//! Nothing here is declined: a row is covered, failing, missing, or not
//! applicable by the spec's own text.
//!
//! Everything is a function of the inventory text, the sources and the JUnit
//! text, and returns the dashboard and the badges as strings.

use super::Tree;
use super::conformance::{self, Row};
use super::junit::{self, Case, Outcome};
use std::collections::{BTreeMap, BTreeSet};
use std::ops::{Index, IndexMut};

/// The line prefix the plugin contract prints for a declared gap, followed by
/// the case's name (`testing/tutti-plugin-contract`'s `gap_marker`).
const GAP_MARKER: &str = "tutti-contract: gap ";

const CONTRACT: &str = "tutti-plugin-contract";

/// An operating system a dashboard column is for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Os {
    Linux,
    Macos,
    Windows,
}

impl Os {
    /// Every OS, in column order.
    pub(super) const ALL: [Os; 3] = [Os::Linux, Os::Macos, Os::Windows];

    /// The name that labels a column, a badge and a results file.
    pub(super) fn name(self) -> &'static str {
        match self {
            Os::Linux => "linux",
            Os::Macos => "macos",
            Os::Windows => "windows",
        }
    }

    /// The OS a results file stem names.
    pub(super) fn from_name(name: &str) -> Result<Os, String> {
        Os::ALL
            .into_iter()
            .find(|os| os.name() == name)
            .ok_or_else(|| {
                let names: Vec<&str> = Os::ALL.iter().map(|os| os.name()).collect();
                format!("`{name}` is not an OS ({})", names.join(", "))
            })
    }
}

/// One value per [`Os`].
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(super) struct PerOs<T>([T; 3]);

impl<T> PerOs<T> {
    pub(super) fn from_fn(mut f: impl FnMut(Os) -> T) -> Self {
        PerOs(Os::ALL.map(&mut f))
    }
}

impl<T> Index<Os> for PerOs<T> {
    type Output = T;
    fn index(&self, os: Os) -> &T {
        &self.0[os as usize]
    }
}

impl<T> IndexMut<Os> for PerOs<T> {
    fn index_mut(&mut self, os: Os) -> &mut T {
        &mut self.0[os as usize]
    }
}

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
    pub(super) status: PerOs<Status>,
    /// `binary: test` of every claiming test that ran on the OS, with its outcome.
    pub(super) tests: PerOs<Vec<(String, Outcome)>>,
}

/// The rows of one area, in inventory order.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct AreaReport {
    pub(super) name: String,
    pub(super) rows: Vec<RowReport>,
}

impl AreaReport {
    /// Whether any test claiming a row of the area ran on `os`.
    pub(super) fn has_results(&self, os: Os) -> bool {
        self.rows.iter().any(|r| !r.tests[os].is_empty())
    }

    /// `(covered, failing, applicable)` on `os`; a not-applicable
    /// row is out of `applicable`.
    pub(super) fn counts(&self, os: Os) -> (usize, usize, usize) {
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
/// is the `midi` area.
fn area_of(path: &str) -> &str {
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
        for tag in conformance::tags(path, text).0 {
            if let Some(item) = tag.item {
                out.entry((package.to_owned(), item))
                    .or_default()
                    .extend(tag.ids);
            }
        }
    }
    out
}

/// The ids a result of `case` covers: the tag's ids, narrowed to the result's
/// own format for a plugin-contract case, and none for a case that declared
/// its gap.
fn covers<'a>(case: &Case, ids: &'a BTreeSet<String>) -> Vec<&'a str> {
    let (package, suffix) = case.binary.split_once("::").unwrap_or((&case.binary, ""));
    if package != CONTRACT {
        return ids.iter().map(String::as_str).collect();
    }
    if case.output.lines().any(|l| l.starts_with(GAP_MARKER)) {
        return Vec::new();
    }
    let prefix = format!("{}.", suffix.split('_').next().unwrap_or(""));
    ids.iter()
        .map(String::as_str)
        .filter(|id| suffix.is_empty() || id.starts_with(&prefix))
        .collect()
}

/// The report over the checkout `tree` and one JUnit text per OS (`results`:
/// `(os, xml)`): one [`AreaReport`] per area the inventory files name, in
/// path order, the parts of an area (`midi-m2-104.txt`, `midi-m2-116.txt`)
/// joined.
pub(super) fn report(tree: &dyn Tree, results: &[(Os, String)]) -> Result<Vec<AreaReport>, String> {
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

    // id -> per OS -> the results of the tests that claim it.
    let mut seen: BTreeMap<&str, PerOs<Vec<(String, Outcome)>>> = BTreeMap::new();
    for (os, xml) in results {
        for case in junit::parse(xml).map_err(|e| format!("{}: {e}", os.name()))? {
            let package = case.binary.split("::").next().unwrap_or(&case.binary);
            let test = case.name.rsplit("::").next().unwrap_or(&case.name);
            let Some(ids) = claims.get(&(package.to_owned(), test.to_owned())) else {
                continue;
            };
            for id in covers(&case, ids) {
                seen.entry(id).or_default()[*os]
                    .push((format!("{}: {}", case.binary, case.name), case.outcome));
            }
        }
    }

    let mut areas: Vec<&str> = Vec::new();
    for (path, _) in &rows {
        let area = area_of(path);
        if !areas.contains(&area) {
            areas.push(area);
        }
    }
    let mut out = Vec::new();
    for area in areas {
        let reports = rows
            .iter()
            .filter(|(path, _)| area_of(path) == area)
            .map(|(_, row)| {
                let tests = seen.get(row.id.as_str()).cloned().unwrap_or_default();
                let status = PerOs::from_fn(|os| {
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
        let mut report = AreaReport {
            name: area.to_owned(),
            rows: reports,
        };
        for os in Os::ALL {
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

/// The shields.io endpoint for one area on one OS.
pub(super) fn badge(area: &AreaReport, os: Os) -> String {
    let (covered, failing, applicable) = area.counts(os);
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
        json_str(&format!("{} {}", area.name, os.name())),
        json_str(&format!("{covered}/{applicable}")),
        json_str(color),
    )
}

/// `(file name, contents)` of every badge, `<area>-<os>.json`, for each pair
/// that has results.
pub(super) fn badges(areas: &[AreaReport]) -> Vec<(String, String)> {
    areas
        .iter()
        .flat_map(|a| {
            Os::ALL
                .into_iter()
                .filter(|&os| a.has_results(os))
                .map(|os| (format!("{}-{}.json", a.name, os.name()), badge(a, os)))
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

fn cell(r: &RowReport, os: Os) -> String {
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

/// The dashboard page: one table per area, rows by OS, with the counts and
/// a filter box. Plain HTML, inline CSS and JS, no external reference.
pub(super) fn dashboard(areas: &[AreaReport]) -> String {
    let mut h = String::from(
        "<!doctype html>\n<html lang=\"en\"><head><meta charset=\"utf-8\">\
<meta name=\"viewport\" content=\"width=device-width,initial-scale=1\">\
<title>tutti plugin conformance</title>\n<style>",
    );
    h.push_str(STYLE);
    h.push_str("</style></head><body>\n<h1>Plugin spec conformance</h1>\n");
    h.push_str(
        "<p>Each row is one item of an area's spec. A cell is <b>pass</b> when a test \
claiming the row passed on that OS, <b>FAIL</b> when one failed, <b>missing</b> when none ran on an OS that has results for the area, \
<b>no results</b> when nothing of the area ran on that OS, \
and <b>n/a</b> when the spec itself excludes the row (hover for the citation). \
<a href=\"../coverage/html/index.html\">Line coverage</a>.</p>\n",
    );
    h.push_str("<nav>");
    for f in areas {
        h.push_str(&format!("<a href=\"#{0}\">{0}</a>", esc(&f.name)));
    }
    h.push_str("</nav>\n<input id=\"filter\" type=\"search\" placeholder=\"filter rows (id, text, status)\" autocomplete=\"off\">\n");
    for f in areas {
        h.push_str(&format!(
            "<h2 id=\"{0}\">{0}</h2>\n<p class=\"counts\">",
            esc(&f.name)
        ));
        for os in Os::ALL {
            let name = os.name();
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
        for os in Os::ALL {
            h.push_str(&format!("<th>{}</th>", os.name()));
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
            for os in Os::ALL {
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
    const HOST: &str = "/// Spec: clap.c\n#[test]\nfn c_works() {}\n";

    fn tree() -> Mem {
        Mem::with(&[
            ("testing/conformance/clap.txt", CLAP),
            ("testing/conformance/vst3.txt", VST3),
            ("testing/tutti-plugin-contract/src/cases.rs", CASES),
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

    const LINUX: Os = Os::Linux;
    const MACOS: Os = Os::Macos;
    const WINDOWS: Os = Os::Windows;

    /// The marker the contract prints for a declared gap, as nextest keeps it.
    const GAP_OUT: &str = "<system-out><![CDATA[tutti-contract: gap gapped_one\n]]></system-out>";

    fn run(results: &[(Os, String)]) -> Vec<AreaReport> {
        report(&tree(), results).unwrap()
    }

    fn status(f: &AreaReport, id: &str, os: Os) -> Status {
        f.rows.iter().find(|r| r.row.id == id).unwrap().status[os]
    }

    #[test]
    fn a_row_is_covered_failing_missing_or_not_applicable_by_what_ran() {
        let linux = xml(&[
            ("tutti-plugin-contract::clap", "shared", ""),
            ("tutti-clap-host", "tests::c_works", "<failure/>"),
        ]);
        let r = run(&[(LINUX, linux)]);
        let clap = &r[0];
        assert_eq!(status(clap, "clap.a", LINUX), Status::Covered);
        assert_eq!(
            status(clap, "clap.b", LINUX),
            Status::Missing,
            "no result for its case"
        );
        assert_eq!(status(clap, "clap.c", LINUX), Status::Failing);
        assert_eq!(status(clap, "clap.d", LINUX), Status::Na);
        assert_eq!(
            status(clap, "clap.a", MACOS),
            Status::NoResults,
            "no macos file"
        );
        assert_eq!(status(clap, "clap.d", WINDOWS), Status::Na);
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
        let clap = &run(&[(LINUX, linux)])[0];
        assert_eq!(status(clap, "clap.a", LINUX), Status::Missing);
        assert_eq!(status(clap, "clap.b", LINUX), Status::Covered);
    }

    #[test]
    fn a_contract_case_covers_only_the_format_it_ran_for() {
        let linux = xml(&[("tutti-plugin-contract::clap", "shared", "")]);
        let r = run(&[(LINUX, linux)]);
        assert_eq!(status(&r[0], "clap.a", LINUX), Status::Covered);
        assert_eq!(status(&r[1], "vst3.x", LINUX), Status::NoResults);
        let both = xml(&[
            ("tutti-plugin-contract::clap", "shared", ""),
            ("tutti-plugin-contract::vst3", "shared", ""),
        ]);
        assert_eq!(
            status(&run(&[(LINUX, both)])[1], "vst3.x", LINUX),
            Status::Covered
        );
    }

    #[test]
    fn a_case_that_printed_its_gap_covers_nothing_for_its_format() {
        let linux = xml(&[
            ("tutti-plugin-contract::vst3", "gapped_one", GAP_OUT),
            ("tutti-plugin-contract::clap", "gapped_one", ""),
        ]);
        let r = run(&[(LINUX, linux)]);
        assert_eq!(status(&r[1], "vst3.x", LINUX), Status::NoResults);
        assert_eq!(
            status(&r[0], "clap.b", LINUX),
            Status::Covered,
            "clap printed no marker"
        );
    }

    #[test]
    fn the_marker_must_open_a_line_of_a_contract_cases_output() {
        let quoted = "<system-out><![CDATA[note: tutti-contract: gap gapped_one\n]]></system-out>";
        let linux = xml(&[("tutti-plugin-contract::clap", "gapped_one", quoted)]);
        assert_eq!(
            status(&run(&[(LINUX, linux)])[0], "clap.b", LINUX),
            Status::Covered
        );
        let other = xml(&[("tutti-clap-host", "c_works", GAP_OUT)]);
        assert_eq!(
            status(&run(&[(LINUX, other)])[0], "clap.c", LINUX),
            Status::Covered,
            "only the contract's cases carry the marker's meaning"
        );
    }

    #[test]
    fn an_os_is_named_by_its_file_stem() {
        for os in Os::ALL {
            assert_eq!(Os::from_name(os.name()), Ok(os));
        }
        let err = Os::from_name("beos").unwrap_err();
        assert_eq!(err, "`beos` is not an OS (linux, macos, windows)");
    }

    #[test]
    fn bad_xml_is_an_error_naming_its_os() {
        let err = report(&tree(), &[(WINDOWS, "<testcase".into())]).unwrap_err();
        assert!(err.starts_with("windows: "), "{err}");
    }

    #[test]
    fn an_os_with_no_results_for_an_area_is_no_results_and_one_with_results_is_missing() {
        // clap ran on linux and macos; vst3 ran on macos only.
        let linux = xml(&[("tutti-plugin-contract::clap", "shared", "")]);
        let macos = xml(&[
            ("tutti-plugin-contract::clap", "shared", ""),
            ("tutti-plugin-contract::vst3", "shared", "<skipped/>"),
        ]);
        let r = run(&[(LINUX, linux), (MACOS, macos)]);
        let (clap, vst3) = (&r[0], &r[1]);
        assert_eq!(
            status(clap, "clap.b", LINUX),
            Status::Missing,
            "linux has clap results"
        );
        assert_eq!(
            status(clap, "clap.b", WINDOWS),
            Status::NoResults,
            "windows has none"
        );
        assert_eq!(
            status(clap, "clap.d", WINDOWS),
            Status::Na,
            "the spec's own N/A stays"
        );
        assert_eq!(status(vst3, "vst3.x", LINUX), Status::NoResults);
        assert_eq!(
            status(vst3, "vst3.x", MACOS),
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
        let clap = &run(&[(LINUX, linux)])[0];
        assert_eq!(clap.counts(LINUX), (1, 0, 3));
    }

    #[test]
    fn badges_carry_the_count_and_a_colour() {
        let linux = xml(&[
            ("tutti-plugin-contract::clap", "shared", ""),
            ("tutti-clap-host", "c_works", ""),
            ("tutti-plugin-contract::clap", "gapped_one", ""),
        ]);
        let clap = &run(&[(LINUX, linux)])[0];
        assert_eq!(
            badge(clap, LINUX),
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
        let r = run(&[(LINUX, linux)]);
        assert!(badge(&r[0], LINUX).contains("\"red\""));
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
        let results = [(MACOS, xml(&[("zed-core", "tests::two", "")]))];
        let r = report(&mem, &results).unwrap();
        let names: Vec<&str> = r.iter().map(|f| f.name.as_str()).collect();
        assert_eq!(names, ["clap", "vst3", "zed"]);
        let zed = &r[2];
        assert_eq!(zed.rows.len(), 2, "the parts are one area");
        assert_eq!(status(zed, "zed.two", MACOS), Status::Covered);
        assert_eq!(status(zed, "zed.one", MACOS), Status::Missing);
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
