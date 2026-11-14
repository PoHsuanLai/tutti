//! The VST3 spec surface, read from the SDK headers in `vendor/vst3-sdk`.
//!
//! The SDK is the spec: its `pluginterfaces/{base,vst,gui}/*.h` declare every
//! interface. One inventory id is a pure virtual method of an interface:
//! `vst3.<Interface>.<method>`. An interface is a `class` the header gives a
//! `DECLARE_CLASS_IID`; a method is a `virtual ... name (...) = 0;` in its body.
//!
//! The side is the one the interface's doc comment marks: `[plug imp]` is
//! `plugin`, `[host imp]` is `host`, both marks or none is `core`, except that
//! an unmarked interface takes the side of the interface it derives from when
//! that one is declared in the SDK (`IPluginFactory2` is the plug-in's, as
//! `IPluginFactory` is). The maturity is `stable`. The section is the header's
//! path under `pluginterfaces/`, with the `[released: x.y.z]` mark when the doc
//! comment has one.
//!
//! The extraction is a pure function over header text ([`extract_header`]) and
//! over the SDK as a [`Tree`] ([`extract`]). A class it cannot close, or two
//! methods with one id, is an error: it never returns half a set.

use super::Tree;
use super::lex::matching;
use std::collections::{BTreeMap, BTreeSet};

/// Where the SDK headers sit in the repo.
pub(super) const ROOT: &str = "vendor/vst3-sdk/pluginterfaces";
/// The header directories that hold interfaces.
const DIRS: [&str; 3] = ["base", "vst", "gui"];

/// One extracted spec item.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub(super) struct Item {
    pub(super) id: String,
    pub(super) side: &'static str,
    pub(super) maturity: &'static str,
    pub(super) section: String,
}

/// One interface of a header: its marks and its pure virtual methods.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct Interface {
    pub(super) name: String,
    pub(super) base: Option<String>,
    /// `Some("plugin")`, `Some("host")`, or `None` when both or neither mark is there.
    pub(super) marked_side: Option<&'static str>,
    pub(super) released: Option<String>,
    pub(super) methods: Vec<String>,
}

/// `text` with its comments and its string and character literals replaced by
/// spaces; newlines and offsets are kept.
fn blank_cpp(text: &str) -> String {
    let b = text.as_bytes();
    let mut out = b.to_vec();
    let mut i = 0;
    let wipe = |out: &mut [u8], from: usize, to: usize| {
        for c in &mut out[from..to] {
            if *c != b'\n' {
                *c = b' ';
            }
        }
    };
    while i < b.len() {
        match b[i] {
            b'/' if b.get(i + 1) == Some(&b'/') => {
                let end = text[i..].find('\n').map_or(b.len(), |n| i + n);
                wipe(&mut out, i, end);
                i = end;
            }
            b'/' if b.get(i + 1) == Some(&b'*') => {
                let end = text[i + 2..].find("*/").map_or(b.len(), |n| i + 2 + n + 2);
                wipe(&mut out, i, end);
                i = end;
            }
            q @ (b'"' | b'\'') => {
                let mut j = i + 1;
                while j < b.len() && b[j] != q && b[j] != b'\n' {
                    j += if b[j] == b'\\' { 2 } else { 1 };
                }
                let end = (j + 1).min(b.len());
                wipe(&mut out, i + 1, end.saturating_sub(1).max(i + 1));
                i = end;
            }
            _ => i += 1,
        }
    }
    String::from_utf8_lossy(&out).into_owned()
}

fn word_byte(c: u8) -> bool {
    c.is_ascii_alphanumeric() || c == b'_'
}

/// The identifier that starts at `from` after skipping whitespace.
fn ident_at(code: &str, from: usize) -> Option<(&str, usize)> {
    let b = code.as_bytes();
    let mut s = from;
    while s < b.len() && b[s].is_ascii_whitespace() {
        s += 1;
    }
    let mut e = s;
    while e < b.len() && word_byte(b[e]) {
        e += 1;
    }
    (e > s).then(|| (&code[s..e], e))
}

/// The names `DECLARE_CLASS_IID (Name, ...)` declares in `code`.
fn iid_names(code: &str) -> BTreeSet<String> {
    let mut out = BTreeSet::new();
    let mut at = 0;
    while let Some(p) = code[at..].find("DECLARE_CLASS_IID") {
        let after = at + p + "DECLARE_CLASS_IID".len();
        at = after;
        let rest = code[after..].trim_start();
        if let Some(args) = rest.strip_prefix('(')
            && let Some((name, end)) = ident_at(args, 0)
            && args[end..].trim_start().starts_with(',')
        {
            out.insert(name.to_owned());
        }
    }
    out
}

/// The doc comment that ends right before byte `at` of `text`, if any.
fn doc_before(text: &str, at: usize) -> &str {
    let head = text[..at].trim_end();
    if !head.ends_with("*/") {
        return "";
    }
    head.rfind("/*").map_or("", |s| &head[s..])
}

fn released(doc: &str) -> Option<String> {
    let p = doc.find("[released:")?;
    let rest = &doc[p + "[released:".len()..];
    let line = rest.lines().next().unwrap_or("");
    let v = line.split(']').next().unwrap_or("").trim();
    (!v.is_empty()).then(|| v.to_owned())
}

/// The pure virtual methods in a class body (blanked text).
fn pure_methods(body: &str) -> Vec<String> {
    let (mut out, mut depth, mut stmt) = (Vec::new(), 0usize, String::new());
    for ch in body.chars() {
        match ch {
            '{' => depth += 1,
            '}' => {
                depth = depth.saturating_sub(1);
                if depth == 0 {
                    stmt.clear();
                }
            }
            ';' if depth == 0 => {
                let s = stmt.trim();
                if let Some(head) = s.strip_suffix("= 0").map(str::trim_end)
                    && let Some(paren) = head.find('(')
                    && head.split_whitespace().next() == Some("virtual")
                    && let Some(name) = head[..paren].split_whitespace().last()
                {
                    out.push(name.to_owned());
                }
                stmt.clear();
            }
            ':' if depth == 0 && matches!(stmt.trim(), "public" | "protected" | "private") => {
                stmt.clear();
            }
            _ if depth == 0 => stmt.push(ch),
            _ => {}
        }
    }
    out
}

/// The interfaces one header declares: the classes it gives a
/// `DECLARE_CLASS_IID`, in order, with their pure virtual methods.
pub(super) fn extract_header(path: &str, text: &str) -> Result<Vec<Interface>, String> {
    let code = blank_cpp(text);
    let iids = iid_names(&code);
    let mut out = Vec::new();
    let mut at = 0;
    while let Some(p) = super::lex::find_word_from(&code, "class", at) {
        at = p + "class".len();
        let Some((name, after)) = ident_at(&code, at) else {
            continue;
        };
        if !iids.contains(name) {
            continue;
        }
        let rest = &code[after..];
        let (Some(open), semi) = (rest.find('{'), rest.find(';')) else {
            continue;
        };
        if semi.is_some_and(|s| s < open) {
            continue;
        }
        let open = after + open;
        let close = matching(&code, open, b'{', b'}')
            .ok_or_else(|| format!("{path}: class `{name}` never closes"))?;
        let head = &code[after..open];
        let base = head.split_once(':').and_then(|(_, b)| {
            b.split_whitespace()
                .find(|w| *w != "public")
                .map(|w| w.trim_end_matches(',').to_owned())
        });
        let doc = doc_before(text, p);
        let marked_side = match (doc.contains("[plug imp]"), doc.contains("[host imp]")) {
            (true, false) => Some("plugin"),
            (false, true) => Some("host"),
            _ => None,
        };
        out.push(Interface {
            name: name.to_owned(),
            base,
            marked_side,
            released: released(doc),
            methods: pure_methods(&code[open + 1..close]),
        });
        at = close;
    }
    Ok(out)
}

/// Every method the SDK's headers declare.
pub(super) fn extract(repo: &dyn Tree) -> Result<Vec<Item>, String> {
    let mut found: Vec<(String, Interface)> = Vec::new();
    for dir in DIRS {
        for path in repo.files(&format!("{ROOT}/{dir}"), &|_| false) {
            if !path.ends_with(".h") {
                continue;
            }
            let Some(text) = repo.read(&path) else {
                return Err(format!("vst3 sdk: cannot read {path}"));
            };
            let rel = path
                .strip_prefix(&format!("{ROOT}/"))
                .unwrap_or(&path)
                .to_owned();
            for i in extract_header(&path, &text)? {
                found.push((rel.clone(), i));
            }
        }
    }
    if found.is_empty() {
        return Err(format!("no VST3 SDK headers under {ROOT}"));
    }
    items(&found)
}

/// The items of the interfaces, each with its side resolved through its bases.
pub(super) fn items(found: &[(String, Interface)]) -> Result<Vec<Item>, String> {
    let by_name: BTreeMap<&str, &Interface> =
        found.iter().map(|(_, i)| (i.name.as_str(), i)).collect();
    let side_of = |i: &Interface| -> &'static str {
        let mut cur = i;
        for _ in 0..found.len() {
            if let Some(s) = cur.marked_side {
                return s;
            }
            match cur.base.as_deref().and_then(|b| by_name.get(b)) {
                Some(next) => cur = next,
                None => break,
            }
        }
        "core"
    };
    let (mut out, mut seen) = (Vec::new(), BTreeSet::new());
    for (rel, i) in found {
        let side = side_of(i);
        let section = match &i.released {
            Some(v) => format!("{rel} [released {v}]"),
            None => rel.clone(),
        };
        for m in &i.methods {
            let id = format!("vst3.{}.{m}", i.name);
            if !seen.insert(id.clone()) {
                return Err(format!("vst3 sdk: two methods share the id `{id}`"));
            }
            out.push(Item {
                id,
                side,
                maturity: "stable",
                section: section.clone(),
            });
        }
    }
    out.sort();
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::gates::fake::Mem;

    const COMPONENT: &str = "\
/** Component base interface
- [plug imp]
- [released: 3.0.0]
*/
class IComponent : public IPluginBase
{
public:
	/** Called first. */
	virtual tresult PLUGIN_API setActive (TBool state /*in*/) = 0;
	virtual tresult PLUGIN_API getBusInfo (MediaType type,
	                                       BusInfo& bus) = 0;
	virtual int32 helper () { return 0; }
	enum Mode { kA = 0, kB };
	static const FUID iid;
};
DECLARE_CLASS_IID (IComponent, 0x1, 0x2, 0x3, 0x4)
";

    fn one(text: &str) -> Interface {
        let mut v = extract_header("x.h", text).unwrap();
        assert_eq!(v.len(), 1, "{v:?}");
        v.remove(0)
    }

    #[test]
    fn methods_are_the_pure_virtuals_of_the_body_only() {
        let i = one(COMPONENT);
        assert_eq!(i.name, "IComponent");
        assert_eq!(i.base.as_deref(), Some("IPluginBase"));
        assert_eq!(i.methods, ["setActive", "getBusInfo"]);
        assert_eq!(i.released.as_deref(), Some("3.0.0"));
        assert_eq!(i.marked_side, Some("plugin"));
    }

    #[test]
    fn an_unclosed_release_mark_ends_with_its_line() {
        let text = "/** [plug imp]\n- [released: 3.7.9\n- [optional]\n*/\nclass IO : public FUnknown { virtual void a () = 0; };\nDECLARE_CLASS_IID (IO, 1, 2, 3, 4)\n";
        assert_eq!(one(text).released.as_deref(), Some("3.7.9"));
    }

    #[test]
    fn a_class_without_an_iid_or_inside_a_comment_is_not_an_interface() {
        let text = "\
class NotAnInterface : public FUnknown { virtual void f () = 0; };
// class Commented : public FUnknown { virtual void g () = 0; };
/* class Block : public FUnknown { virtual void h () = 0; }; */
class IFwd;
class IReal : public FUnknown { public: virtual void k () = 0; };
DECLARE_CLASS_IID (IReal, 1, 2, 3, 4)
";
        let v = extract_header("x.h", text).unwrap();
        assert_eq!(v.len(), 1);
        assert_eq!(v[0].name, "IReal");
        assert_eq!(v[0].methods, ["k"]);
    }

    #[test]
    fn both_marks_or_none_leave_the_side_unmarked() {
        let both = "/** [host imp] or [plug imp] */\nclass IB : public FUnknown { virtual void a () = 0; };\nDECLARE_CLASS_IID (IB, 1, 2, 3, 4)\n";
        assert_eq!(one(both).marked_side, None);
        let host = "/** [host imp] */\nclass IH : public FUnknown { virtual void a () = 0; };\nDECLARE_CLASS_IID (IH, 1, 2, 3, 4)\n";
        assert_eq!(one(host).marked_side, Some("host"));
    }

    #[test]
    fn an_unmarked_interface_takes_its_base_s_side() {
        let a = "/** [plug imp] */\nclass IP : public FUnknown { virtual void a () = 0; };\nDECLARE_CLASS_IID (IP, 1, 2, 3, 4)\n\
                 class IP2 : public IP { virtual void b () = 0; };\nDECLARE_CLASS_IID (IP2, 1, 2, 3, 4)\n\
                 class IU : public FUnknown { virtual void c () = 0; };\nDECLARE_CLASS_IID (IU, 1, 2, 3, 4)\n";
        let found: Vec<_> = extract_header("a.h", a)
            .unwrap()
            .into_iter()
            .map(|i| ("a.h".to_owned(), i))
            .collect();
        let got: Vec<_> = items(&found)
            .unwrap()
            .into_iter()
            .map(|i| (i.id, i.side))
            .collect();
        assert_eq!(
            got,
            [
                ("vst3.IP.a".to_owned(), "plugin"),
                ("vst3.IP2.b".to_owned(), "plugin"),
                ("vst3.IU.c".to_owned(), "core"),
            ]
        );
    }

    #[test]
    fn the_sdk_tree_gives_sorted_items_with_section_and_release() {
        let mem = Mem::with(&[
            (
                "vendor/vst3-sdk/pluginterfaces/vst/ivstcomponent.h",
                COMPONENT,
            ),
            ("vendor/vst3-sdk/pluginterfaces/vst/note.txt", "ignored"),
        ]);
        let items = extract(&mem).unwrap();
        assert_eq!(items.len(), 2);
        assert_eq!(items[0].id, "vst3.IComponent.getBusInfo");
        assert_eq!(items[0].section, "vst/ivstcomponent.h [released 3.0.0]");
        assert_eq!((items[0].side, items[0].maturity), ("plugin", "stable"));
    }

    #[test]
    fn an_empty_sdk_and_a_duplicate_method_are_errors() {
        assert!(extract(&Mem::default()).is_err());
        let dup = "class IA : public FUnknown { virtual void a () = 0; virtual void a (int) = 0; };\nDECLARE_CLASS_IID (IA, 1, 2, 3, 4)\n";
        let found: Vec<_> = extract_header("a.h", dup)
            .unwrap()
            .into_iter()
            .map(|i| ("a.h".to_owned(), i))
            .collect();
        assert!(items(&found).unwrap_err().contains("vst3.IA.a"));
    }

    #[test]
    fn an_unclosed_class_is_an_error() {
        let text = "class IA : public FUnknown { virtual void a () = 0;\nDECLARE_CLASS_IID (IA, 1, 2, 3, 4)\n";
        assert!(
            extract_header("a.h", text)
                .unwrap_err()
                .contains("never closes")
        );
    }
}
