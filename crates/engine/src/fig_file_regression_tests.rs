//! Reproduction tests for review findings (fig-import-export-findings.md). Each test asserts the correct behaviour, so a failure reproduces the finding.

use base64::Engine as _;
use figcraft_doc::{Document, NodeKind};
use serde_json::{Value, json};

use crate::Session;

const COLLECTION: &str = "VariableCollectionId:1:1";
const SHARED_VARIABLE: &str = "VariableID:1:4";

/// A REST variables payload with one COLOR variable under the shared raw Figma ids.
fn rest_variables(name: &str, rgb: (f64, f64, f64)) -> Value {
    json!({"status": 200, "error": false, "meta": {
        "variableCollections": {COLLECTION: {"id": COLLECTION, "name": "Theme", "defaultModeId": "1:0",
            "modes": [{"modeId": "1:0", "name": "Light"}]}},
        "variables": {SHARED_VARIABLE: {"id": SHARED_VARIABLE, "name": name, "variableCollectionId": COLLECTION, "resolvedType": "COLOR",
            "valuesByMode": {"1:0": {"r": rgb.0, "g": rgb.1, "b": rgb.2, "a": 1}}, "scopes": ["ALL_SCOPES"]}}
    }})
}

/// A REST file with one rectangle whose fill is bound to the shared variable id.
fn rest_file(file: &str, layer: &str) -> Value {
    let fill = json!({"type": "SOLID", "color": {"r": 0.5, "g": 0.5, "b": 0.5, "a": 1},
        "boundVariables": {"color": {"type": "VARIABLE_ALIAS", "id": SHARED_VARIABLE}}});
    json!({"name": file, "document": {"id": "0:0", "type": "DOCUMENT", "children": [
        {"id": "0:1", "name": "Page", "type": "CANVAS", "backgroundColor": {"r": 1, "g": 1, "b": 1, "a": 1}, "children": [
            {"id": "1:2", "name": layer, "type": "RECTANGLE", "relativeTransform": [[1, 0, 0], [0, 1, 0]], "size": {"x": 40, "y": 20},
             "fills": [fill]}
        ]}
    ]}})
}

fn rest_import_params(file: &str, layer: &str, variable: &str, rgb: (f64, f64, f64)) -> Value {
    json!({
        "name": format!("{file}.json"),
        "data": base64::engine::general_purpose::STANDARD.encode(serde_json::to_vec(&rest_file(file, layer)).unwrap()),
        "variables": rest_variables(variable, rgb)
    })
}

/// G3: file B's binding to its own `VariableID:1:4` must not silently resolve to file A's
/// variable that happens to have the same raw Figma id.
#[test]
fn repro_g3_append_pages_keeps_colliding_variable_ids_apart() {
    let mut s = Session::new();
    s.execute("file.import", &rest_import_params("A", "A rect", "brand", (1.0, 0.0, 0.0))).unwrap();
    let r = s.execute("file.import", &rest_import_params("B", "B rect", "accent", (0.0, 0.0, 1.0))).unwrap();
    let doc = s.doc().unwrap();
    let b_rect = doc.nodes().find(|n| n.name == "B rect").expect("B's layer was imported");
    let bound = b_rect.bound.get("fill").expect("B's fill keeps a variable binding");
    let variable = doc.variables.variables.iter().find(|v| v.id == *bound).expect("the bound variable exists");
    assert_eq!(
        variable.name,
        "accent",
        "B's layer must stay bound to B's variable, not A's same-id variable (variables: {:?}, warnings: {})",
        doc.variables.variables.iter().map(|v| (&v.id, &v.name)).collect::<Vec<_>>(),
        r["warnings"]
    );
}

fn g5_import_params() -> Value {
    rest_import_params("Kit", "Kit rect", "brand", (1.0, 0.0, 0.0))
}

/// G5: a byte import must not keep its full Base64 payload in `last_command`.
#[test]
fn repro_g5_last_command_drops_import_bytes() {
    let mut s = Session::new();
    s.execute("file.import", &g5_import_params()).unwrap();
    let retained = s.last_command.as_ref().filter(|(id, p)| id == "file.import" && (p.get("data").is_some() || p.get("images").is_some()));
    assert!(
        retained.is_none(),
        "last_command retains the import payload ({} bytes of params)",
        retained.map(|(_, p)| p.to_string().len()).unwrap_or(0)
    );
}

/// G5: "Repeat" after a byte import must not silently import the file again.
#[test]
fn repro_g5_repeat_does_not_reimport_bytes() {
    let mut s = Session::new();
    s.execute("file.import", &g5_import_params()).unwrap();
    let pages = s.doc().unwrap().pages.len();
    let _ = s.execute("edit.repeat", &json!({}));
    assert_eq!(s.doc().unwrap().pages.len(), pages, "edit.repeat imported the Base64 payload again");
}

/// A `.fig` whose property-resource catalog sits on the first page, with the bound main and
/// instance on that page too (same synthetic shape as `fig_import_tests::resource_fig`).
fn resource_fig() -> Vec<u8> {
    use std::collections::BTreeMap;

    const SCOPE: &str = "2c7cae8357f32b36cc37a16bf66047222c9de358851c21da89c098114448c070";
    let mut doc = Document::new("Resource fixture");
    let page = doc.first_page().unwrap();
    let catalog = format!(
        "{{\"version\":1,\"scope\":\"{SCOPE}\",\"owners\":[{{\"id\":{{\"session\":8,\"local\":50}},\"page\":{{\"session\":8,\"local\":3}},\"name\":\"Hidden owner\",\"definitions\":[{{\"id\":{{\"session\":8,\"local\":100}},\"name\":\"Flag\",\"initial\":false}}]}}]}}"
    );
    doc.node_mut(page).unwrap().plugin_data.insert("figcraft.figProperties.catalog".into(), catalog);
    let main =
        doc.create(page, NodeKind::Component { description: String::new(), props: BTreeMap::new() }, "Resource main", 0.0, 0.0, 10.0, 10.0).unwrap();
    doc.node_mut(main)
        .unwrap()
        .plugin_data
        .insert("figcraft.figProperties.main".into(), format!("{{\"version\":1,\"scope\":\"{SCOPE}\",\"origin\":{{\"session\":8,\"local\":10}}}}"));
    let instance = doc
        .create(
            page,
            NodeKind::Instance { component: main, props: BTreeMap::from([("Flag".into(), Value::Bool(false))]) },
            "Resource instance",
            0.0,
            20.0,
            10.0,
            10.0,
        )
        .unwrap();
    doc.node_mut(instance).unwrap().plugin_data.insert("figcraft.figProperties.bindings".into(), format!("{{\"version\":1,\"scope\":\"{SCOPE}\",\"main\":{{\"session\":8,\"local\":10}},\"values\":[{{\"key\":\"Flag\",\"definition\":{{\"session\":8,\"local\":100}}}}],\"visible\":null}}"));
    figcraft_format::fig::export(&doc, None).unwrap().0
}

/// E1: moving the imported main and instance to another page and deleting the catalog's
/// first page must leave a document that still exports as `.fig`.
#[test]
fn repro_e1_fig_export_after_deleting_catalog_page() {
    let mut s = Session::new();
    let params = json!({"data": base64::engine::general_purpose::STANDARD.encode(resource_fig()), "name": "resource.fig"});
    s.execute("file.open", &params).unwrap();
    let doc = s.doc().unwrap();
    let catalog_page = doc.first_page().unwrap();
    assert!(doc.node(catalog_page).unwrap().plugin_data.contains_key("figcraft.figProperties.catalog"), "fixture carries the catalog on page 1");
    let main = doc.nodes().find(|n| n.name == "Resource main").unwrap().id;
    let instance = doc.nodes().find(|n| n.name == "Resource instance").unwrap().id;
    // Sanity: the untouched import exports.
    s.execute("file.export", &json!({"format": "fig"})).unwrap();

    let added = s.execute("page.add", &json!({"name": "Kept"})).unwrap();
    let kept = s.doc().unwrap().pages.iter().copied().find(|p| *p != catalog_page).unwrap();
    s.execute("node.reparent", &json!({"ids": [main, instance], "parent": kept})).unwrap_or_else(|e| panic!("reparent: {e} ({added})"));
    s.execute("page.delete", &json!({"id": catalog_page})).unwrap();
    let doc = s.doc().unwrap();
    assert!(doc.nodes().any(|n| n.name == "Resource instance" && n.plugin_data.contains_key("figcraft.figProperties.bindings")));
    assert!(doc.pages.iter().all(|p| !doc.node(*p).unwrap().plugin_data.contains_key("figcraft.figProperties.catalog")));
    let exported = s.execute("file.export", &json!({"format": "fig"}));
    let exported = exported.unwrap_or_else(|e| panic!(".fig export failed after deleting the catalog page: {e:?}"));
    // The orphaned private records are left out and reported; the unbound value is not written.
    let warnings = exported["warnings"].to_string();
    assert!(warnings.contains("hidden definition catalog is missing") && warnings.contains("without a definition in the file"), "{warnings}");
}

/// N-1: the 256 MiB import cap must not apply to native documents and recovery copies (the
/// format writes larger files); imports keep their cap. The sparse files fail to parse, but not
/// because of their size.
#[test]
fn repro_n1_native_files_above_the_import_cap_are_read() {
    let dir = std::env::temp_dir().join(format!("figcraft-native-cap-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let size = 257 * 1024 * 1024;
    let make = |name: &str| {
        let path = dir.join(name);
        std::fs::File::create(&path).unwrap().set_len(size).unwrap();
        path.to_string_lossy().into_owned()
    };
    let (native, import) = (make("big.figcraft"), make("big.json"));
    let refused = |e: Option<crate::EngineError>| e.is_some_and(|e| e.to_string().contains("not a file of at most"));
    let mut s = Session::new();
    assert!(!refused(s.execute("file.open", &json!({"path": native})).err()), "native file refused by size");
    assert!(!refused(s.execute("file.recover", &json!({"path": native})).err()), "recovery copy refused by size");
    assert!(refused(s.execute("file.open", &json!({"path": import})).err()), "import source must keep its cap");
    std::fs::remove_dir_all(&dir).unwrap();
}

/// A `.fig` file carries its images, so real ones run past the import cap: it has its own. The
/// sparse file fails to parse, but not because of its size.
#[test]
fn fig_files_above_the_import_cap_are_read() {
    let dir = std::env::temp_dir().join(format!("figcraft-fig-cap-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join("big.fig");
    std::fs::File::create(&path).unwrap().set_len(257 * 1024 * 1024).unwrap();
    let err = Session::new().execute("file.open", &json!({"path": path.to_string_lossy()})).err();
    assert!(!err.is_some_and(|e| e.to_string().contains("not a file of at most")), ".fig file refused by size");
    std::fs::remove_dir_all(&dir).unwrap();
}

/// G3 follow-up: renaming a colliding collection also renames the identical-looking variable
/// that belongs to it, and the appended file keeps its image bytes.
#[test]
fn repro_g3_renamed_collections_take_their_variables_and_images_survive() {
    const PNG: &str = "iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAIAAACQd1PeAAAADElEQVR4nGP4z8AAAAMBAQDJ/pLvAAAAAElFTkSuQmCC";
    let variables = |collection: &str| {
        json!({"status": 200, "error": false, "meta": {
            "variableCollections": {COLLECTION: {"id": COLLECTION, "name": collection, "defaultModeId": "1:0",
                "modes": [{"modeId": "1:0", "name": "Light"}]}},
            "variables": {SHARED_VARIABLE: {"id": SHARED_VARIABLE, "name": "brand", "variableCollectionId": COLLECTION, "resolvedType": "COLOR",
                "valuesByMode": {"1:0": {"r": 1, "g": 0, "b": 0, "a": 1}}, "scopes": ["ALL_SCOPES"]}}
        }})
    };
    let mut s = Session::new();
    let mut a = rest_import_params("A", "A rect", "brand", (1.0, 0.0, 0.0));
    a["variables"] = variables("Theme A");
    s.execute("file.import", &a).unwrap();
    let mut file = rest_file("B", "B rect");
    file["document"]["children"][0]["children"].as_array_mut().unwrap().push(json!({"id": "1:3", "name": "B image", "type": "RECTANGLE",
        "relativeTransform": [[1, 0, 0], [0, 1, 0]], "size": {"x": 10, "y": 10}, "fills": [{"type": "IMAGE", "imageRef": "img", "scaleMode": "FILL"}]}));
    let b = json!({"name": "B.json", "data": base64::engine::general_purpose::STANDARD.encode(serde_json::to_vec(&file).unwrap()),
        "variables": variables("Theme B"), "images": {"img": PNG}});
    s.execute("file.import", &b).unwrap();
    let doc = s.doc().unwrap();
    let rect = doc.nodes().find(|n| n.name == "B rect").unwrap();
    let variable = doc.variables.variables.iter().find(|v| Some(&v.id) == rect.bound.get("fill")).unwrap();
    let collection = doc.variables.collections.iter().find(|c| c.id == variable.collection).unwrap();
    assert_eq!(collection.name, "Theme B", "B's variable must live in B's renamed collection");
    let image = doc.nodes().find(|n| n.name == "B image").unwrap();
    let hashes: Vec<_> = image
        .fills
        .iter()
        .filter_map(|p| match &p.kind {
            figcraft_doc::color::PaintKind::Image(image) => Some(image.image.clone()),
            _ => None,
        })
        .collect();
    assert!(!hashes.is_empty() && hashes.iter().all(|h| doc.images.contains_key(h)), "image bytes kept: {hashes:?}");
}

/// G3: only variable references follow a renamed id; literals, mode ids and plugin data do not.
#[test]
fn repro_g3_renaming_a_variable_keeps_equal_string_literals() {
    use figcraft_doc::{Collection, Mode, TextData, Variable, VariableType, VariableValue};
    use std::collections::BTreeMap;

    let mut a = Document::new("A");
    a.variables.collections.push(Collection {
        id: "c".into(),
        name: "Theme".into(),
        modes: vec![Mode { id: "v".into(), name: "Light".into() }],
        extends: None,
    });
    let variable: Variable = serde_json::from_value(json!({"id": "v", "name": "Target", "collection": "c", "resolvedType": "number",
        "values": {"v": {"type": "number", "value": 1}}}))
    .unwrap();
    a.variables.variables.push(variable);
    let mut b = a.clone();
    b.name = "B".into();
    let v = &mut b.variables.variables[0];
    v.name = "Incoming".into();
    v.kind = VariableType::String;
    v.values = BTreeMap::from([("v".into(), VariableValue::String("v".into()))]);
    v.code_syntax.insert("WEB".into(), "v".into());
    let mut alias = v.clone();
    alias.id = "alias".into();
    alias.name = "Alias".into();
    alias.values = BTreeMap::from([("v".into(), VariableValue::Alias("v".into()))]);
    b.variables.variables.push(alias);
    let text = b.create(b.first_page().unwrap(), NodeKind::Text(TextData::new("v")), "Incoming text", 0.0, 0.0, 40.0, 20.0).unwrap();
    let n = b.node_mut(text).unwrap();
    n.bound.insert("characters".into(), "v".into());
    n.var_modes.insert("c".into(), "v".into());
    n.plugin_data.insert("v".into(), "v".into());
    n.kind.text_mut().unwrap().style.font_family = "v".into();
    n.kind.text_mut().unwrap().style.link = Some("v".into());
    let mut s = Session::new();
    for doc in [&a, &b] {
        let bytes = figcraft_format::save_bytes(doc, None).unwrap();
        s.execute("file.import", &json!({"name": "test.figcraft", "data": base64::engine::general_purpose::STANDARD.encode(bytes)})).unwrap();
    }
    let doc = s.doc().unwrap();
    let v = doc.variables.variables.iter().find(|v| v.name == "Incoming").unwrap();
    assert_ne!(v.id, "v");
    assert_eq!(v.values, BTreeMap::from([("v".into(), VariableValue::String("v".into()))]));
    assert_eq!(v.code_syntax.get("WEB"), Some(&"v".into()));
    let alias = doc.variables.variables.iter().find(|v| v.name == "Alias").unwrap();
    assert_eq!(alias.values.get("v"), Some(&VariableValue::Alias(v.id.clone())));
    let n = doc.nodes().find(|n| n.name == "Incoming text").unwrap();
    assert_eq!(n.bound.get("characters"), Some(&v.id));
    assert_eq!(n.var_modes.get("c"), Some(&"v".into()));
    assert_eq!(n.plugin_data, BTreeMap::from([("v".into(), "v".into())]));
    let text = n.kind.text().unwrap();
    assert_eq!(text.characters, "v");
    assert_eq!(text.style.font_family, "v");
    assert_eq!(text.style.link.as_deref(), Some("v"));
}

/// G3: a colliding style id can equal an image hash without renaming the image reference.
#[test]
fn repro_g3_renaming_a_style_keeps_equal_image_hashes() {
    use figcraft_doc::color::{Color, ImageScaleMode, Paint, PaintKind};
    use figcraft_doc::{Style, StyleKind};

    const PNG: &str = "iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAIAAACQd1PeAAAADElEQVR4nGP4z8AAAAMBAQDJ/pLvAAAAAElFTkSuQmCC";
    let bytes = base64::engine::general_purpose::STANDARD.decode(PNG).unwrap();
    let hash = figcraft_format::image_hash(&bytes);
    let mut a = Document::new("A");
    a.styles.push(Style {
        id: hash.clone(),
        name: "Target paint".into(),
        description: String::new(),
        kind: StyleKind::Paint { paints: vec![Paint::solid(Color::BLACK)] },
        bound: Default::default(),
    });
    let mut b = a.clone();
    b.name = "B".into();
    figcraft_format::add_image(&mut b, bytes.clone()).unwrap();
    b.styles[0].name = "Incoming paint".into();
    b.styles[0].kind = StyleKind::Paint { paints: vec![Paint::image(&hash, ImageScaleMode::Fill)] };
    let image = b.create(b.first_page().unwrap(), NodeKind::Rectangle, "Incoming image", 0.0, 0.0, 10.0, 10.0).unwrap();
    let n = b.node_mut(image).unwrap();
    n.styles.fill = Some(hash.clone());
    n.fills = vec![Paint::image(&hash, ImageScaleMode::Fill)];
    let mut s = Session::new();
    for doc in [&a, &b] {
        let bytes = figcraft_format::save_bytes(doc, None).unwrap();
        s.execute("file.import", &json!({"name": "test.figcraft", "data": base64::engine::general_purpose::STANDARD.encode(bytes)})).unwrap();
    }
    let doc = s.doc().unwrap();
    let style = doc.styles.iter().find(|s| s.name == "Incoming paint").unwrap();
    assert_ne!(style.id, hash);
    let n = doc.nodes().find(|n| n.name == "Incoming image").unwrap();
    assert_eq!(n.styles.fill.as_deref(), Some(style.id.as_str()));
    let StyleKind::Paint { paints } = &style.kind else { panic!("paint style kept") };
    for p in n.fills.iter().chain(paints) {
        let PaintKind::Image(image) = &p.kind else { panic!("image paint kept") };
        assert_eq!(image.image, hash);
        assert_eq!(doc.images.get(&image.image).unwrap().bytes.as_ref(), &bytes);
    }
}
