//! Effective paragraph fields override shared typography defaults, with absent-field fallback.

use super::kiwi::Kv;
use super::{export, import};
use figcraft_doc::{Document, NodeKind, Style, StyleKind, TextAutoResize, TextData, TextStyle};
use serde_json::Value;

fn assert_native(doc: &Document, spacing: f64, indent: f64) {
    let node = doc.nodes().find(|node| node.name == "Label").unwrap();
    let text = node.kind.text().unwrap();
    assert_eq!(text.paragraph_spacing, spacing);
    assert_eq!(text.paragraph_indent, indent);
    assert_eq!(text.characters, "a\nb");
    let linked = doc.style(node.styles.text.as_deref().unwrap()).unwrap();
    assert_eq!(linked.name, "Shared");
    let StyleKind::Text { style } = &linked.kind else { panic!("linked definition must remain text") };
    assert_eq!(style.font_family, "Inter");
    assert_eq!(style.font_size, 20.0);
    assert_eq!(text.style.font_family, style.font_family);
    assert_eq!(text.style.font_size, style.font_size);
}

fn two_rounds(mut doc: Document, spacing: f64, indent: f64) {
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
        let local = nodes.iter().find(|node| node["name"] == "Label").unwrap();
        let shared = nodes.iter().find(|node| node["styleType"] == "TEXT").unwrap();
        assert_eq!(local["paragraphSpacing"].as_f64(), Some(spacing));
        assert_eq!(local["paragraphIndent"].as_f64(), Some(indent));
        assert_eq!(shared["paragraphSpacing"].as_f64(), Some(0.0));
        assert_eq!(shared["paragraphIndent"].as_f64(), Some(0.0));
        assert_eq!(local["styleIdForText"]["guid"], shared["guid"], "actual standard style reference");
        let (back, warnings) = import(&bytes, "Synthetic paragraphs").unwrap();
        assert!(warnings.is_empty(), "round {round}: {warnings:?}");
        assert_native(&back, spacing, indent);
        doc = back;
    }
}

#[test]
fn local_paragraph_values_keep_shared_typography_over_two_rounds() {
    let mut doc = Document::new("Synthetic paragraphs");
    let typography = TextStyle { font_family: "Inter".into(), font_size: 20.0, ..TextStyle::default() };
    doc.styles.push(Style {
        id: "shared-text".into(),
        name: "Shared".into(),
        description: String::new(),
        kind: StyleKind::Text { style: typography.clone() },
        bound: Default::default(),
    });
    let mut text = TextData::new("a\nb");
    text.style = typography;
    text.paragraph_spacing = 24.0;
    text.paragraph_indent = 8.0;
    text.auto_resize = TextAutoResize::None;
    let id = doc.create(doc.first_page().unwrap(), NodeKind::Text(text), "Label", 0.0, 0.0, 100.0, 60.0).unwrap();
    doc.node_mut(id).unwrap().styles.text = Some("shared-text".into());
    two_rounds(doc, 24.0, 8.0);
}

fn raw(local: Option<(f32, f32)>) -> Value {
    fn guid(id: u32) -> Kv<'static> {
        Kv::Object(vec![("sessionID", Kv::Uint(7)), ("localID", Kv::Uint(u64::from(id)))])
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
    let mut local_fields = vec![
        ("fontName", font("Stale copy")),
        ("fontSize", Kv::Float(40.0)),
        ("styleIdForText", object(vec![("guid", guid(10))])),
        ("textData", object(vec![("characters", Kv::Str("a\nb".into()))])),
    ];
    if let Some((spacing, indent)) = local {
        local_fields.extend([("paragraphSpacing", Kv::Float(spacing)), ("paragraphIndent", Kv::Float(indent))]);
    }
    let message = object(vec![(
        "nodeChanges",
        Kv::Array(vec![
            node(1, "DOCUMENT", "Document", None, vec![]),
            node(2, "CANVAS", "Page", Some(1), vec![]),
            node(
                10,
                "TEXT",
                "Shared",
                Some(2),
                vec![
                    ("styleType", Kv::Enum("TEXT")),
                    ("fontName", font("Inter")),
                    ("fontSize", Kv::Float(20.0)),
                    ("paragraphSpacing", Kv::Float(24.0)),
                    ("paragraphIndent", Kv::Float(12.0)),
                ],
            ),
            node(11, "TEXT", "Label", Some(2), local_fields),
        ]),
    )]);
    super::map::to_rest(&message, "Synthetic raw paragraphs").unwrap().0
}

#[test]
fn explicit_zero_paragraph_values_clear_shared_values() {
    let value = raw(Some((0.0, 0.0)));
    let local = value["document"]["children"][0]["children"].as_array().unwrap().iter().find(|node| node["name"] == "Label").unwrap();
    assert_eq!(local["style"]["paragraphSpacing"].as_f64(), Some(0.0));
    assert_eq!(local["style"]["paragraphIndent"].as_f64(), Some(0.0));
    let (doc, warnings) = crate::rest::import_fig(&value, &|_| None, Vec::new()).unwrap();
    assert!(warnings.is_empty(), "{warnings:?}");
    assert_native(&doc, 0.0, 0.0);
    two_rounds(doc, 0.0, 0.0);
}

#[test]
fn absent_local_paragraph_fields_keep_shared_fallback() {
    let value = raw(None);
    let local = value["document"]["children"][0]["children"].as_array().unwrap().iter().find(|node| node["name"] == "Label").unwrap();
    assert_eq!(local["style"]["paragraphSpacing"].as_f64(), Some(24.0));
    assert_eq!(local["style"]["paragraphIndent"].as_f64(), Some(12.0));
    let (doc, warnings) = crate::rest::import_fig(&value, &|_| None, Vec::new()).unwrap();
    assert!(warnings.is_empty(), "{warnings:?}");
    assert_native(&doc, 24.0, 12.0);
    two_rounds(doc, 24.0, 12.0);
}
