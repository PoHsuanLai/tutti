//! The per-file ratchet the counting gates share.
//!
//! A counting gate finds sites in each file. A baseline file holds one
//! `<count> <path>` line per file that held sites when the gate began (a file
//! not listed has 0). A file fails when it holds more sites than its
//! baseline; a file below its baseline passes and the report says how far the
//! baseline can be lowered.

use super::{Report, Tree};
use std::collections::BTreeMap;

/// A file's baseline: path to count.
pub(super) type Baseline = BTreeMap<String, usize>;

/// The sites one file holds, each a printable line.
pub(super) struct Found {
    pub(super) path: String,
    pub(super) sites: Vec<String>,
}

/// What one gate prints around its ratchet.
pub(super) struct Wording<'a> {
    /// The gate's recipe name, the prefix of every line.
    pub(super) gate: &'a str,
    /// The baseline file's repo path.
    pub(super) baseline: &'a str,
    /// What a site is, plural, in the per-file line (`unresolved names`).
    pub(super) noun: &'a str,
    /// The failure headline's remainder, after `FAIL (`.
    pub(super) headline: &'a str,
}

/// `<count> <path>` lines; blank lines are skipped.
pub(super) fn parse(path: &str, text: &str) -> Result<Baseline, String> {
    let mut map = Baseline::new();
    for (n, line) in text.lines().enumerate() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        let (count, file) = line
            .split_once(' ')
            .and_then(|(c, p)| Some((c.parse::<usize>().ok()?, p.trim())))
            .ok_or_else(|| format!("{path}:{}: not `<count> <path>`: {line}", n + 1))?;
        map.insert(file.to_owned(), count);
    }
    Ok(map)
}

/// The baseline `w.baseline` holds, or a failing report that says why not.
pub(super) fn load(tree: &dyn Tree, w: &Wording) -> Result<Baseline, Report> {
    let fail = |why: String| {
        let mut r = Report::default();
        r.fail(format!("{}: FAIL: {why}", w.gate));
        r
    };
    match tree.read(w.baseline).map(|t| parse(w.baseline, &t)) {
        Some(Ok(b)) => Ok(b),
        Some(Err(e)) => Err(fail(e)),
        None => Err(fail(format!("{} is missing", w.baseline))),
    }
}

/// Settles `found` (one entry per file, in path order) against `baseline`.
pub(super) fn settle(w: &Wording, baseline: &Baseline, found: Vec<Found>) -> Report {
    let mut r = Report::default();
    let mut over = Vec::new();
    let mut slack = 0;
    let mut seen = std::collections::BTreeSet::new();
    let mut total = 0;
    for f in found {
        let allowed = baseline.get(&f.path).copied().unwrap_or(0);
        total += f.sites.len();
        seen.insert(f.path.clone());
        if f.sites.len() > allowed {
            over.push(format!(
                "  {}: {} {}, baseline {allowed}",
                f.path,
                f.sites.len(),
                w.noun
            ));
            over.extend(f.sites);
        } else {
            slack += allowed - f.sites.len();
        }
    }
    slack += baseline
        .iter()
        .filter(|(p, _)| !seen.contains(*p))
        .map(|(_, n)| n)
        .sum::<usize>();
    if over.is_empty() {
        r.say(format!("{}: OK", w.gate));
    } else {
        r.fail(format!("{}: FAIL ({}):", w.gate, w.headline));
        for line in over {
            r.fail(line);
        }
    }
    r.say(format!(
        "{}: {total} {} in {} baseline files ({} total in {})",
        w.gate,
        w.noun,
        baseline.len(),
        baseline.values().sum::<usize>(),
        w.baseline
    ));
    if slack > 0 {
        r.say(format!(
            "{}: {} can drop by {slack}; lower it to the current counts",
            w.gate, w.baseline
        ));
    }
    r
}

#[cfg(test)]
mod tests {
    use super::*;

    const W: Wording = Wording {
        gate: "check-x",
        baseline: "xtask/x.txt",
        noun: "sites",
        headline: "a file gained a site",
    };

    fn found(path: &str, n: usize) -> Found {
        Found {
            path: path.to_owned(),
            sites: (1..=n).map(|i| format!("    {path}:{i}: site")).collect(),
        }
    }

    #[test]
    fn a_file_over_its_baseline_fails_and_lists_its_sites() {
        let base = parse("b", "1 a.rs\n").unwrap();
        let r = settle(&W, &base, vec![found("a.rs", 2), found("b.rs", 0)]);
        assert_eq!(
            r.findings,
            [
                "check-x: FAIL (a file gained a site):",
                "  a.rs: 2 sites, baseline 1",
                "    a.rs:1: site",
                "    a.rs:2: site",
            ]
        );
    }

    #[test]
    fn a_file_under_its_baseline_passes_and_the_slack_is_reported() {
        let base = parse("b", "3 a.rs\n2 gone.rs\n").unwrap();
        let r = settle(&W, &base, vec![found("a.rs", 1)]);
        assert!(r.passed());
        assert_eq!(
            r.out,
            [
                "check-x: OK",
                "check-x: 1 sites in 2 baseline files (5 total in xtask/x.txt)",
                "check-x: xtask/x.txt can drop by 4; lower it to the current counts",
            ]
        );
    }

    #[test]
    fn a_file_missing_from_the_baseline_has_a_baseline_of_zero() {
        let r = settle(&W, &Baseline::new(), vec![found("new.rs", 1)]);
        assert!(!r.passed());
        assert_eq!(r.findings[1], "  new.rs: 1 sites, baseline 0");
    }

    #[test]
    fn a_malformed_baseline_line_is_an_error() {
        assert!(parse("b", "many a.rs\n").is_err());
        assert!(parse("b", "3\n").is_err());
        assert_eq!(parse("b", "\n3 a b.rs\n").unwrap()["a b.rs"], 3);
    }
}
