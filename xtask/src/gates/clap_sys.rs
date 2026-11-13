//! The CLAP spec surface, read from the `clap-sys` crate's source.
//!
//! `clap-sys` mirrors the C headers, so its modules are the spec's own
//! layout: `ext/<name>.rs` is a stable extension, `ext/draft/<name>.rs` a
//! draft one, `factory/` and `factory/draft/` the factories, and the top level
//! the entry, host, plugin, stream and event headers. One inventory id is a
//! function pointer of a vtable struct, or an event type or flag:
//!
//! - `clap.ext.<module>[.<part>].<field>` for an extension's vtables, side
//!   `plugin` for `clap_plugin_<module>[_<part>]`; `clap.ext.<module>.host.<field>`
//!   with side `host` for `clap_host_<module>`; and the helper tables
//!   [`EXT_HELPERS`] names (`clap.ext.<module>.<part>.<field>`);
//! - `clap.factory.<name>[.<part>].<field>` for a factory and what it hands out;
//! - `clap.entry.<field>`, `clap.plugin.<field>`, `clap.host.<field>`,
//!   `clap.stream.<istream|ostream>.<field>` and
//!   `clap.events.<input|output>.<field>`, side `core`;
//! - `clap.event.<type>` and `clap.event.flag.<flag>`, side `core`.
//!
//! The extraction is a pure function over the crate's files, read through
//! [`Tree`] rooted at the crate directory. A vtable struct it cannot place, a
//! constant of an unknown type, or two items with one id is an error: it never
//! returns half a set.

use super::Tree;
use super::lex::{blank, matching};
use std::collections::BTreeSet;

/// One extracted spec item.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub(super) struct Item {
    pub(super) id: String,
    pub(super) side: &'static str,
    pub(super) maturity: &'static str,
}

/// Vtable structs of an extension module that carry neither `clap_plugin_` nor
/// `clap_host_`, with the side that implements them.
const EXT_HELPERS: &[(&str, &str)] = &[("clap_context_menu_builder", "host")];

/// Factory-side structs: `(struct, id prefix, side)`.
const FACTORY: &[(&str, &str, &str)] = &[
    ("clap_plugin_factory", "clap.factory.plugin", "core"),
    (
        "clap_preset_discovery_factory",
        "clap.factory.preset_discovery",
        "core",
    ),
    (
        "clap_preset_discovery_provider",
        "clap.factory.preset_discovery.provider",
        "plugin",
    ),
    (
        "clap_preset_discovery_indexer",
        "clap.factory.preset_discovery.indexer",
        "host",
    ),
    (
        "clap_preset_discovery_metadata_receiver",
        "clap.factory.preset_discovery.metadata_receiver",
        "host",
    ),
    (
        "clap_plugin_invalidation_factory",
        "clap.factory.plugin_invalidation",
        "core",
    ),
    (
        "clap_plugin_state_converter_factory",
        "clap.factory.plugin_state_converter",
        "core",
    ),
    (
        "clap_plugin_state_converter",
        "clap.factory.plugin_state_converter.converter",
        "plugin",
    ),
];

/// Core headers' structs: `(file, struct, id prefix)`, all side `core`.
const CORE: &[(&str, &str, &str)] = &[
    ("src/entry.rs", "clap_plugin_entry", "clap.entry"),
    ("src/host.rs", "clap_host", "clap.host"),
    ("src/plugin.rs", "clap_plugin", "clap.plugin"),
    ("src/stream.rs", "clap_istream", "clap.stream.istream"),
    ("src/stream.rs", "clap_ostream", "clap.stream.ostream"),
    ("src/events.rs", "clap_input_events", "clap.events.input"),
    ("src/events.rs", "clap_output_events", "clap.events.output"),
];

/// Every item the crate's source declares.
pub(super) fn extract(src: &dyn Tree) -> Result<Vec<Item>, String> {
    let mut out: Vec<Item> = Vec::new();
    let mut seen = BTreeSet::new();
    let mut push = |item: Item| -> Result<(), String> {
        if seen.insert(item.id.clone()) {
            out.push(item);
            Ok(())
        } else {
            Err(format!("clap-sys: two items share the id `{}`", item.id))
        }
    };
    let mut any = false;
    for path in src.files("src", &|_| false) {
        if !path.ends_with(".rs") {
            continue;
        }
        let Some(text) = src.read(&path) else {
            return Err(format!("clap-sys: cannot read {path}"));
        };
        let code = blank(&text);
        any = true;
        for (name, fields) in fn_structs(&code) {
            let (prefix, side, maturity) = classify(&path, &name).ok_or_else(|| {
                format!(
                    "clap-sys: {path}: vtable struct `{name}` has no place in the inventory scheme"
                )
            })?;
            for f in fields {
                push(Item {
                    id: format!("{prefix}.{f}"),
                    side,
                    maturity,
                })?;
            }
        }
        if path == "src/events.rs" {
            for item in event_consts(&code)? {
                push(item)?;
            }
        }
    }
    if !any {
        return Err("clap-sys: no source files under src/".to_owned());
    }
    out.sort();
    Ok(out)
}

/// `(id prefix, side, maturity)` of a vtable struct in `path`.
fn classify(path: &str, name: &str) -> Option<(String, &'static str, &'static str)> {
    if let Some((_, _, prefix)) = CORE.iter().find(|(f, n, _)| *f == path && *n == name) {
        return Some(((*prefix).to_owned(), "core", "stable"));
    }
    let maturity = if path.contains("/draft/") {
        "draft"
    } else {
        "stable"
    };
    if path.starts_with("src/factory/") {
        let (_, prefix, side) = FACTORY.iter().find(|(n, ..)| *n == name)?;
        return Some(((*prefix).to_owned(), side, maturity));
    }
    let stem = path
        .strip_prefix("src/ext/draft/")
        .or_else(|| path.strip_prefix("src/ext/"))?
        .strip_suffix(".rs")?;
    let (side, base, host) = if let Some(b) = name.strip_prefix("clap_plugin_") {
        ("plugin", b, "")
    } else if let Some(b) = name.strip_prefix("clap_host_") {
        // A host table shares its module's field names with the plugin's
        // (`populate`, `changed`), so its id carries `.host`.
        ("host", b, ".host")
    } else {
        let (_, side) = EXT_HELPERS.iter().find(|(n, _)| *n == name)?;
        (*side, name.strip_prefix("clap_")?, "")
    };
    let part = if base == stem {
        String::new()
    } else {
        format!(".{}", base.strip_prefix(stem)?.strip_prefix('_')?)
    };
    Some((format!("clap.ext.{stem}{host}{part}"), side, maturity))
}

/// `pub struct Name { .. }` of blanked `code` with the names of its function
/// pointer fields (`Option<.. fn(..) ..>`); structs without one are left out.
fn fn_structs(code: &str) -> Vec<(String, Vec<String>)> {
    let mut out = Vec::new();
    for (at, _) in code.match_indices("pub struct ") {
        let rest = &code[at + "pub struct ".len()..];
        let name: String = rest
            .chars()
            .take_while(|c| c.is_ascii_alphanumeric() || *c == '_')
            .collect();
        let after = at + "pub struct ".len() + name.len();
        let Some(open) = code[after..]
            .find(|c: char| !c.is_whitespace())
            .map(|p| after + p)
            .filter(|&p| code.as_bytes()[p] == b'{')
        else {
            continue;
        };
        let Some(close) = matching(code, open, b'{', b'}') else {
            continue;
        };
        let fields: Vec<String> = split_fields(&code[open + 1..close])
            .into_iter()
            .filter_map(|f| {
                let f = f.trim().strip_prefix("pub")?.trim_start();
                let (field, ty) = f.split_once(':')?;
                let ty = ty.trim_start();
                (ty.starts_with("Option<") && ty.contains("fn(")).then(|| field.trim().to_owned())
            })
            .collect();
        if !fields.is_empty() {
            out.push((name, fields));
        }
    }
    out
}

/// The top-level, comma-separated pieces of a struct body.
fn split_fields(body: &str) -> Vec<&str> {
    let b = body.as_bytes();
    let (mut depth, mut start) = (0i32, 0);
    let mut out = Vec::new();
    for (i, &c) in b.iter().enumerate() {
        match c {
            b'(' | b'[' | b'{' | b'<' => depth += 1,
            b')' | b']' | b'}' => depth -= 1,
            b'>' if i == 0 || b[i - 1] != b'-' => depth -= 1,
            b',' if depth == 0 => {
                out.push(&body[start..i]);
                start = i + 1;
            }
            _ => {}
        }
    }
    out.push(&body[start..]);
    out
}

/// `CLAP_EVENT_*` constants of `events.rs`: event types and header flags.
fn event_consts(code: &str) -> Result<Vec<Item>, String> {
    let mut out = Vec::new();
    for line in code.lines() {
        let Some(rest) = line.trim().strip_prefix("pub const CLAP_EVENT_") else {
            continue;
        };
        let Some((name, tail)) = rest.split_once(':') else {
            continue;
        };
        let ty = tail.split('=').next().unwrap_or("").trim();
        let lower = name.trim().to_ascii_lowercase();
        let id = match ty {
            "clap_event_type" => format!("clap.event.{lower}"),
            "clap_event_flags" => format!("clap.event.flag.{lower}"),
            _ => {
                return Err(format!(
                    "clap-sys: events.rs: CLAP_EVENT_{name} has the unknown type `{ty}`"
                ));
            }
        };
        out.push(Item {
            id,
            side: "core",
            maturity: "stable",
        });
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::gates::fake::Mem;

    fn tree(files: &[(&str, &str)]) -> Mem {
        Mem::with(files)
    }

    fn ids(items: &[Item]) -> Vec<(&str, &str, &str)> {
        items
            .iter()
            .map(|i| (i.id.as_str(), i.side, i.maturity))
            .collect()
    }

    const PARAMS: &str = r#"
pub struct clap_plugin_params {
    pub count: Option<unsafe extern "C" fn(plugin: *const clap_plugin) -> u32>,
    pub flush: Option<
        unsafe extern "C" fn(plugin: *const clap_plugin, n: Option<u32>),
    >,
}
pub struct clap_host_params {
    pub rescan: Option<unsafe extern "C" fn(host: *const clap_host, flags: u32)>,
}
"#;

    #[test]
    fn extension_vtables_give_plugin_and_host_ids_by_maturity() {
        let t = tree(&[
            ("src/ext/params.rs", PARAMS),
            (
                "src/ext/draft/tuning.rs",
                "pub struct clap_plugin_tuning_t { pub changed: Option<unsafe extern \"C\" fn()>, }",
            ),
        ]);
        let got = extract(&t).unwrap();
        assert_eq!(
            ids(&got),
            [
                ("clap.ext.params.count", "plugin", "stable"),
                ("clap.ext.params.flush", "plugin", "stable"),
                ("clap.ext.params.host.rescan", "host", "stable"),
                ("clap.ext.tuning.t.changed", "plugin", "draft"),
            ]
        );
    }

    #[test]
    fn a_struct_with_a_function_pointer_inside_an_unrelated_name_is_not_taken() {
        // `clap_param_info` has an `Option<fn>` field but lives in params.rs:
        // it is no vtable of the module, so the extractor refuses to place it.
        let t = tree(&[(
            "src/ext/params.rs",
            "pub struct clap_param_info { pub cb: Option<unsafe extern \"C\" fn(a: u32) -> u32>, }",
        )]);
        assert!(extract(&t).unwrap_err().contains("clap_param_info"));
    }

    #[test]
    fn core_factory_and_event_items() {
        let t = tree(&[
            (
                "src/plugin.rs",
                "pub struct clap_plugin_descriptor { pub id: *const u8, }\npub struct clap_plugin { pub init: Option<unsafe extern \"C\" fn(p: *const clap_plugin) -> bool>, pub desc: u32, }",
            ),
            (
                "src/factory/draft/plugin_invalidation.rs",
                "pub struct clap_plugin_invalidation_factory { pub count: Option<unsafe extern \"C\" fn(f: u32) -> u32>, }",
            ),
            (
                "src/events.rs",
                "pub const CLAP_EVENT_IS_LIVE: clap_event_flags = 1;\npub const CLAP_EVENT_NOTE_ON: clap_event_type = 0;\npub const CLAP_CORE_EVENT_SPACE_ID: u16 = 0;\n",
            ),
        ]);
        assert_eq!(
            ids(&extract(&t).unwrap()),
            [
                ("clap.event.flag.is_live", "core", "stable"),
                ("clap.event.note_on", "core", "stable"),
                ("clap.factory.plugin_invalidation.count", "core", "draft"),
                ("clap.plugin.init", "core", "stable"),
            ]
        );
    }

    #[test]
    fn an_unplaceable_vtable_or_unknown_constant_fails_by_name() {
        let t = tree(&[(
            "src/process.rs",
            "pub struct clap_odd { pub f: Option<unsafe extern \"C\" fn()>, }",
        )]);
        assert!(extract(&t).unwrap_err().contains("clap_odd"));
        let t = tree(&[("src/events.rs", "pub const CLAP_EVENT_X: u8 = 1;\n")]);
        assert!(extract(&t).unwrap_err().contains("CLAP_EVENT_X"));
    }

    #[test]
    fn a_duplicate_id_is_an_error() {
        let t = tree(&[(
            "src/ext/log.rs",
            "pub struct clap_host_log { pub a: Option<unsafe extern \"C\" fn()>, }\npub struct clap_host_log { pub a: Option<unsafe extern \"C\" fn()>, }",
        )]);
        assert!(extract(&t).unwrap_err().contains("clap.ext.log.host.a"));
    }

    #[test]
    fn an_empty_crate_is_an_error() {
        assert!(extract(&tree(&[])).is_err());
    }

    #[test]
    fn helper_tables_and_sub_vtables_are_named_by_part() {
        let t = tree(&[
            (
                "src/ext/context_menu.rs",
                "pub struct clap_context_menu_builder { pub add_item: Option<unsafe extern \"C\" fn()>, }",
            ),
            (
                "src/ext/audio_ports_config.rs",
                "pub struct clap_plugin_audio_ports_config_info { pub get: Option<unsafe extern \"C\" fn()>, }",
            ),
        ]);
        assert_eq!(
            ids(&extract(&t).unwrap()),
            [
                ("clap.ext.audio_ports_config.info.get", "plugin", "stable"),
                ("clap.ext.context_menu.builder.add_item", "host", "stable"),
            ]
        );
    }
}
