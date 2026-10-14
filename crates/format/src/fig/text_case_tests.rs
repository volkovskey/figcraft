//! Effective local case keeps standard shared typography and its actual style reference.

use super::kiwi::Kv;
use super::{export, import};
use figcraft_doc::{Document, StyleKind, TextCase};
use serde_json::Value;

fn wire_case(case: TextCase) -> &'static str {
    match case {
        TextCase::Original => "ORIGINAL",
        TextCase::Upper => "UPPER",
        TextCase::Lower => "LOWER",
        TextCase::Title => "TITLE",
        TextCase::SmallCaps => "SMALL_CAPS",
    }
}

fn raw(shared: Option<&'static str>, local: Option<&'static str>) -> Value {
    fn guid(id: u32) -> Kv<'static> {
        Kv::Object(vec![("sessionID", Kv::Uint(8)), ("localID", Kv::Uint(u64::from(id)))])
    }
    fn object(fields: Vec<(&'static str, Kv<'static>)>) -> Kv<'static> {
        Kv::Object(fields)
    }
    fn node(id: u32, ty: &'static str, name: &str, parent: Option<u32>, mut more: Vec<(&'static str, Kv<'static>)>) -> Kv<'static> {
        let mut fields = vec![("guid", guid(id)), ("type", Kv::Enum(ty)), ("name", Kv::Str(name.into()))];
        if let Some(parent) = parent {
            fields.push(("parentIndex", object(vec![("guid", guid(parent)), ("position", Kv::Str("a".into()))])));
        }
        fields.append(&mut more);
        object(fields)
    }
    fn font(family: &str) -> Kv<'static> {
        object(vec![("family", Kv::Str(family.into())), ("style", Kv::Str("Regular".into())), ("postscript", Kv::Str(String::new()))])
    }
    let mut definition = vec![("styleType", Kv::Enum("TEXT")), ("fontName", font("Inter")), ("fontSize", Kv::Float(20.0))];
    if let Some(case) = shared {
        definition.push(("textCase", Kv::Enum(case)));
    }
    let mut layer = vec![
        ("fontName", font("Stale copy")),
        ("fontSize", Kv::Float(40.0)),
        ("styleIdForText", object(vec![("guid", guid(10))])),
        ("textData", object(vec![("characters", Kv::Str("ab".into()))])),
    ];
    if let Some(case) = local {
        layer.push(("textCase", Kv::Enum(case)));
    }
    let message = object(vec![(
        "nodeChanges",
        Kv::Array(vec![
            node(1, "DOCUMENT", "Document", None, vec![]),
            node(2, "CANVAS", "Page", Some(1), vec![]),
            node(10, "TEXT", "Shared", Some(2), definition),
            node(11, "TEXT", "Label", Some(2), layer),
        ]),
    )]);
    super::map::to_rest(&message, "Synthetic case source").unwrap().0
}

fn assert_native(doc: &Document, local: TextCase, shared: TextCase) {
    let node = doc.nodes().find(|node| node.name == "Label").unwrap();
    let text = node.kind.text().unwrap();
    assert_eq!(text.characters, "ab");
    assert_eq!(text.style.case, local);
    assert_eq!(text.style.font_family, "Inter", "shared typography must still beat the stale local family");
    assert_eq!(text.style.font_size, 20.0, "shared 20px must still beat stale local 40px");
    let linked = doc.style(node.styles.text.as_deref().unwrap()).unwrap();
    assert_eq!(linked.name, "Shared");
    let StyleKind::Text { style } = &linked.kind else { panic!("the actual linked definition must remain text") };
    assert_eq!(style.case, shared, "a local override must not change the definition");
    assert_eq!(style.font_family, "Inter");
    assert_eq!(style.font_size, 20.0);
}

fn native(value: &Value, local: TextCase, shared: TextCase) -> Document {
    let (doc, warnings) = crate::rest::import_fig(value, &|_| None, Vec::new()).unwrap();
    assert!(warnings.is_empty(), "{warnings:?}");
    assert_native(&doc, local, shared);
    doc
}

fn two_rounds(mut doc: Document, local: TextCase, shared: TextCase) {
    for round in 1..=2 {
        let before = doc.clone();
        let (bytes, warnings) = export(&doc, None).unwrap();
        assert!(warnings.is_empty(), "round {round}: {warnings:?}");
        assert!(doc == before);
        let archive = super::Archive::open(&bytes).unwrap();
        let (schema, body) = super::decode_canvas(&archive.canvas).unwrap();
        let message = schema.decode_message(schema.find("Message").unwrap(), &body).unwrap();
        let message = super::debug_json(&message);
        let nodes = message["nodeChanges"].as_array().unwrap();
        let layer = nodes.iter().find(|node| node["name"] == "Label").unwrap();
        let definition = nodes.iter().find(|node| node["styleType"] == "TEXT").unwrap();
        assert_eq!(layer["textCase"].as_str(), Some(wire_case(local)));
        assert_eq!(definition["textCase"].as_str(), Some(wire_case(shared)));
        assert_eq!(layer["styleIdForText"]["guid"], definition["guid"], "actual standard style GUID reference");
        let (back, warnings) = import(&bytes, "Synthetic cases").unwrap();
        assert!(warnings.is_empty(), "round {round}: {warnings:?}");
        assert_native(&back, local, shared);
        doc = back;
    }
}

#[test]
fn valid_effective_local_cases_and_original_clear_preserve_shared_font_priority() {
    for (shared, local, expected, definition) in [
        ("ORIGINAL", "UPPER", TextCase::Upper, TextCase::Original),
        ("ORIGINAL", "LOWER", TextCase::Lower, TextCase::Original),
        ("ORIGINAL", "TITLE", TextCase::Title, TextCase::Original),
        ("ORIGINAL", "SMALL_CAPS", TextCase::SmallCaps, TextCase::Original),
        ("UPPER", "ORIGINAL", TextCase::Original, TextCase::Upper),
        ("UPPER", "SMALL_CAPS_FORCED", TextCase::SmallCaps, TextCase::Upper),
    ] {
        let value = raw(Some(shared), Some(local));
        let layer = value["document"]["children"][0]["children"].as_array().unwrap().iter().find(|node| node["name"] == "Label").unwrap();
        assert_eq!(layer["style"]["textCase"].as_str(), Some(local));
        two_rounds(native(&value, expected, definition), expected, definition);
    }
}

#[test]
fn absent_shared_case_can_create_native_mismatch_and_survives_materialized_defaults() {
    let value = raw(None, Some("UPPER"));
    // The source definition has no case: native default Original legitimately differs from the
    // layer's explicit Upper. REST retains the actual layer style reference independently.
    let doc = native(&value, TextCase::Upper, TextCase::Original);
    two_rounds(doc, TextCase::Upper, TextCase::Original);
}

#[test]
fn absent_or_invalid_local_case_keeps_valid_shared_fallback() {
    for local in [None, Some("UNSUPPORTED_CASE")] {
        let value = raw(Some("LOWER"), local);
        let layer = value["document"]["children"][0]["children"].as_array().unwrap().iter().find(|node| node["name"] == "Label").unwrap();
        assert_eq!(layer["style"]["textCase"].as_str(), Some("LOWER"));
        two_rounds(native(&value, TextCase::Lower, TextCase::Lower), TextCase::Lower, TextCase::Lower);
    }
}
