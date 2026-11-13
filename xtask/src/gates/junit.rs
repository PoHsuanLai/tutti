//! A reader for the JUnit XML nextest writes (`[profile.<name>.junit]`): the
//! `testsuite` and `testcase` elements, and a `testcase`'s `failure`, `error`
//! and `skipped` children. Everything else (attributes the join does not use,
//! `system-out`, `flakyFailure`, `rerunFailure`, comments, CDATA) is read past.

/// How one test ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Outcome {
    Passed,
    Failed,
    Skipped,
}

/// One `testcase`: nextest's binary id (the `classname`), the test's name
/// within it, and its outcome. A test that failed and then passed on a retry
/// carries `flakyFailure` and counts as passed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct Case {
    pub(super) binary: String,
    pub(super) name: String,
    pub(super) outcome: Outcome,
}

/// The test cases of one JUnit document, or what makes it unreadable.
pub(super) fn parse(xml: &str) -> Result<Vec<Case>, String> {
    let mut cases = Vec::new();
    let mut suite = String::new();
    let mut open: Option<Case> = None;
    let mut at = 0;
    while let Some(lt) = xml[at..].find('<') {
        let start = at + lt;
        let rest = &xml[start..];
        if let Some(body) = rest.strip_prefix("<!--") {
            at = start + 4 + skip_to(body, "-->")?;
            continue;
        }
        if let Some(body) = rest.strip_prefix("<![CDATA[") {
            at = start + 9 + skip_to(body, "]]>")?;
            continue;
        }
        if let Some(body) = rest.strip_prefix("<?") {
            at = start + 2 + skip_to(body, "?>")?;
            continue;
        }
        let end =
            tag_end(rest).ok_or_else(|| format!("a tag opened at byte {start} never closes"))?;
        let tag = &rest[1..end];
        at = start + end + 1;
        if let Some(name) = tag.strip_prefix('/') {
            if name.trim() == "testcase" {
                cases.extend(open.take());
            }
            continue;
        }
        let self_closing = tag.ends_with('/');
        let tag = tag.strip_suffix('/').unwrap_or(tag);
        let (name, attrs) = tag.split_once(char::is_whitespace).unwrap_or((tag, ""));
        match name {
            "testsuite" => suite = attr(attrs, "name").unwrap_or_default(),
            "testcase" => {
                let case = Case {
                    binary: attr(attrs, "classname").unwrap_or_else(|| suite.clone()),
                    name: attr(attrs, "name").ok_or("a testcase has no name")?,
                    outcome: Outcome::Passed,
                };
                if self_closing {
                    cases.push(case);
                } else {
                    open = Some(case);
                }
            }
            "failure" | "error" => {
                if let Some(c) = open.as_mut() {
                    c.outcome = Outcome::Failed;
                }
            }
            "skipped" => {
                if let Some(c) = open.as_mut().filter(|c| c.outcome == Outcome::Passed) {
                    c.outcome = Outcome::Skipped;
                }
            }
            _ => {}
        }
    }
    Ok(cases)
}

/// The offset just past `end` in `s`.
fn skip_to(s: &str, end: &str) -> Result<usize, String> {
    s.find(end)
        .map(|i| i + end.len())
        .ok_or_else(|| format!("`{end}` is missing"))
}

/// The offset of the `>` that closes the tag `s` starts with, past any quoted
/// attribute value.
fn tag_end(s: &str) -> Option<usize> {
    let mut quote = None;
    for (i, c) in s.char_indices() {
        match (quote, c) {
            (None, '>') => return Some(i),
            (None, '"' | '\'') => quote = Some(c),
            (Some(q), c) if c == q => quote = None,
            _ => {}
        }
    }
    None
}

/// The value of attribute `key` in `attrs`, with its entities decoded.
fn attr(attrs: &str, key: &str) -> Option<String> {
    let mut rest = attrs.trim_start();
    while let Some((name, after)) = rest.split_once('=') {
        let after = after.trim_start();
        let q = after.chars().next().filter(|c| matches!(c, '"' | '\''))?;
        let body = &after[1..];
        let close = body.find(q)?;
        if name.trim() == key {
            return Some(unescape(&body[..close]));
        }
        rest = body[close + 1..].trim_start();
    }
    None
}

fn unescape(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut rest = s;
    while let Some(amp) = rest.find('&') {
        out.push_str(&rest[..amp]);
        rest = &rest[amp..];
        let decoded = rest.find(';').and_then(|semi| {
            let ch = match &rest[1..semi] {
                "amp" => '&',
                "lt" => '<',
                "gt" => '>',
                "quot" => '"',
                "apos" => '\'',
                n => {
                    let code = match n.strip_prefix("#x") {
                        Some(h) => u32::from_str_radix(h, 16).ok()?,
                        None => n.strip_prefix('#')?.parse().ok()?,
                    };
                    char::from_u32(code)?
                }
            };
            Some((ch, semi + 1))
        });
        match decoded {
            Some((ch, used)) => {
                out.push(ch);
                rest = &rest[used..];
            }
            None => {
                out.push('&');
                rest = &rest[1..];
            }
        }
    }
    out.push_str(rest);
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn case(binary: &str, name: &str, outcome: Outcome) -> Case {
        Case {
            binary: binary.to_owned(),
            name: name.to_owned(),
            outcome,
        }
    }

    const DOC: &str = r#"<?xml version="1.0" encoding="UTF-8"?>
<testsuites name="nextest-run" tests="5">
  <!-- <testcase name="in_a_comment" classname="x"/> -->
  <testsuite name="a::b" tests="5">
    <testcase name="passes" classname="a::b" time="0.1"/>
    <testcase name="fails" classname="a::b">
      <failure message="assertion `left == right` failed">
        <![CDATA[<testcase name="in_cdata" classname="x"/>]]>
      </failure>
      <system-out><![CDATA[ 1 < 2 ]]></system-out>
    </testcase>
    <testcase name="skips" classname="a::b"><skipped/></testcase>
    <testcase name="flaky" classname="a::b"><flakyFailure message="m"/><system-err/></testcase>
    <testcase name="mod::q&amp;&lt;x&gt;" classname='a::c'/>
  </testsuite>
</testsuites>"#;

    #[test]
    fn each_outcome_is_read_and_comments_and_cdata_are_not_tests() {
        assert_eq!(
            parse(DOC).unwrap(),
            [
                case("a::b", "passes", Outcome::Passed),
                case("a::b", "fails", Outcome::Failed),
                case("a::b", "skips", Outcome::Skipped),
                case("a::b", "flaky", Outcome::Passed),
                case("a::c", "mod::q&<x>", Outcome::Passed),
            ]
        );
    }

    #[test]
    fn a_failure_wins_over_a_skip_and_an_error_is_a_failure() {
        let xml = r#"<testsuite name="s"><testcase name="t"><failure/><skipped/></testcase><testcase name="u"><error/></testcase></testsuite>"#;
        let got = parse(xml).unwrap();
        assert_eq!(got[0].outcome, Outcome::Failed);
        assert_eq!(got[1].outcome, Outcome::Failed);
    }

    #[test]
    fn the_class_falls_back_to_the_suite_and_a_gt_in_a_value_does_not_end_the_tag() {
        let xml = r#"<testsuite name="s"><testcase name="a&gt;b" note="x>y"/></testsuite>"#;
        assert_eq!(parse(xml).unwrap(), [case("s", "a>b", Outcome::Passed)]);
    }

    #[test]
    fn an_unterminated_tag_or_comment_is_an_error() {
        assert!(parse("<testsuite name=\"s\"").is_err());
        assert!(parse("<!-- never").is_err());
        assert!(parse("<testcase classname=\"x\"/>").is_err());
    }

    #[test]
    fn numeric_references_decode() {
        assert_eq!(unescape("a&#65;&#x42;&bogus;&"), "aAB&bogus;&");
    }
}
