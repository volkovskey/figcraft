//! Small regressions for nested instance export and files within the importer's budgets.

use super::*;
use serde_json::json;

fn add(doc: &mut Document, parent: NodeId, kind: NodeKind, name: &str, width: f64, height: f64) -> NodeId {
    doc.create(parent, kind, name, 0.0, 0.0, width, height).unwrap()
}

fn component(doc: &mut Document, parent: NodeId, name: &str) -> NodeId {
    add(doc, parent, NodeKind::Component { description: String::new(), props: BTreeMap::new() }, name, 100.0, 40.0)
}

fn instance(doc: &mut Document, main: NodeId, parent: NodeId, name: &str, props: BTreeMap<String, Value>) -> NodeId {
    fn mirror(doc: &mut Document, source: NodeId, parent: NodeId) {
        let mut node = doc.node(source).unwrap().clone();
        let children = std::mem::take(&mut node.children);
        node.id = doc.alloc_id();
        node.source = Some(source);
        node.parent = None;
        let id = node.id;
        doc.insert(node, parent, None).unwrap();
        for child in children {
            mirror(doc, child, id);
        }
    }
    let mut node = doc.node(main).unwrap().clone();
    let children = std::mem::take(&mut node.children);
    node.id = doc.alloc_id();
    node.kind = NodeKind::Instance { component: main, props };
    node.name = name.into();
    node.source = None;
    node.parent = None;
    let id = node.id;
    doc.insert(node, parent, None).unwrap();
    for child in children {
        mirror(doc, child, id);
    }
    id
}

fn written(doc: &Document) -> Value {
    let (bytes, warnings) = export(doc, None).unwrap();
    assert!(warnings.is_empty(), "{warnings:?}");
    let archive = super::super::Archive::open(&bytes).unwrap();
    let (schema, bytes) = super::super::decode_canvas(&archive.canvas).unwrap();
    let msg = schema.decode_message(schema.find("Message").unwrap(), &bytes).unwrap();
    super::super::debug_json(&msg)
}

fn back(doc: &Document) -> Document {
    let (bytes, warnings) = export(doc, None).unwrap();
    assert!(warnings.is_empty(), "{warnings:?}");
    let (doc, warnings) = super::super::import(&bytes, "Stage B").unwrap();
    assert!(warnings.is_empty(), "{warnings:?}");
    doc
}

fn copied_nested(doc: &Document) -> &Node {
    let outer = doc.nodes().find(|n| n.name == "Outer copy").unwrap();
    doc.node(outer.children[0]).unwrap()
}

fn text_property_scene() -> (Document, NodeId, NodeId) {
    let mut doc = Document::new("Nested properties");
    let page = doc.first_page().unwrap();
    let inner = component(&mut doc, page, "Inner");
    if let NodeKind::Component { props, .. } = &mut doc.node_mut(inner).unwrap().kind {
        props.insert("Label".into(), PropDef::Text { default: "Action".into() });
    }
    let mut text = TextData::new("Action");
    text.auto_resize = TextAutoResize::None;
    let label = add(&mut doc, inner, NodeKind::Text(text), "Inner label", 80.0, 20.0);
    doc.node_mut(label).unwrap().prop_refs.insert("characters".into(), "Label".into());
    let outer = component(&mut doc, page, "Outer");
    instance(&mut doc, inner, outer, "Nested", BTreeMap::from([("Label".into(), json!("Action"))]));
    let copy = instance(&mut doc, outer, page, "Outer copy", BTreeMap::new());
    let nested = doc.node(copy).unwrap().children[0];
    let label = doc.node(nested).unwrap().children[0];
    (doc, nested, label)
}

#[test]
fn stage_b_nested_text_property_assignments_round_trip() {
    let (mut doc, nested, label) = text_property_scene();
    if let NodeKind::Instance { props, .. } = &mut doc.node_mut(nested).unwrap().kind {
        props.insert("Label".into(), json!("Label"));
    }
    doc.node_mut(label).unwrap().kind.text_mut().unwrap().characters = "Label".into();
    let msg = written(&doc);
    let outer = msg["nodeChanges"].as_array().unwrap().iter().find(|n| n["name"] == "Outer copy").unwrap();
    assert!(
        outer["symbolData"]["symbolOverrides"]
            .as_array()
            .unwrap()
            .iter()
            .any(|ov| { ov["componentPropAssignments"][0]["value"]["textValue"]["characters"] == "Label" })
    );
    let doc = back(&doc);
    let nested = copied_nested(&doc);
    let NodeKind::Instance { props, .. } = &nested.kind else { panic!() };
    assert_eq!(props.get("Label"), Some(&json!("Label")));
    assert_eq!(doc.node(nested.children[0]).unwrap().kind.text().unwrap().characters, "Label");
}

#[test]
fn stage_b_cleared_nested_property_assignments_round_trip() {
    let (mut doc, nested, _) = text_property_scene();
    if let NodeKind::Instance { props, .. } = &mut doc.node_mut(nested).unwrap().kind {
        props.clear();
    }
    let msg = written(&doc);
    let outer = msg["nodeChanges"].as_array().unwrap().iter().find(|n| n["name"] == "Outer copy").unwrap();
    assert!(outer["symbolData"]["symbolOverrides"].as_array().unwrap().iter().any(|ov| ov["componentPropAssignments"] == json!([])));
    let doc = back(&doc);
    let NodeKind::Instance { props, .. } = &copied_nested(&doc).kind else { panic!() };
    assert!(props.is_empty());
}

#[test]
fn stage_b_nested_layer_type_overrides_round_trip() {
    let mut doc = Document::new("Nested type");
    let page = doc.first_page().unwrap();
    let inner = component(&mut doc, page, "Empty inner");
    let outer = component(&mut doc, page, "Outer");
    instance(&mut doc, inner, outer, "Nested", BTreeMap::new());
    let copy = instance(&mut doc, outer, page, "Outer copy", BTreeMap::new());
    let nested = doc.node(copy).unwrap().children[0];
    doc.node_mut(nested).unwrap().kind = NodeKind::Frame;
    let msg = written(&doc);
    let outer = msg["nodeChanges"].as_array().unwrap().iter().find(|n| n["name"] == "Outer copy").unwrap();
    assert!(outer["symbolData"]["symbolOverrides"].as_array().unwrap().iter().any(|ov| ov["type"] == "FRAME"));
    let doc = back(&doc);
    assert!(matches!(&copied_nested(&doc).kind, NodeKind::Frame));
}

#[test]
fn stage_b_nested_type_changes_with_children_are_rejected() {
    let mut doc = Document::new("Nested type with children");
    let page = doc.first_page().unwrap();
    let inner = component(&mut doc, page, "Inner");
    add(&mut doc, inner, NodeKind::Rectangle, "Rect", 10.0, 10.0);
    let outer = component(&mut doc, page, "Outer");
    instance(&mut doc, inner, outer, "Nested", BTreeMap::new());
    let copy = instance(&mut doc, outer, page, "Outer copy", BTreeMap::new());
    let nested = doc.node(copy).unwrap().children[0];
    doc.node_mut(nested).unwrap().kind = NodeKind::Frame;
    assert_eq!(doc.node(nested).unwrap().children.len(), 1);
    let before = doc.clone();
    let Err(error) = export(&doc, None) else { panic!("a nested type change must not silently discard its children") };
    assert!(error.to_string().contains("nested instance type changes with children"), "{error}");
    assert!(doc == before, "a rejected export must leave the document unchanged");
}

#[test]
fn stage_b_nested_missing_main_with_children_is_rejected() {
    let mut doc = Document::new("Nested missing main with children");
    let page = doc.first_page().unwrap();
    let inner = component(&mut doc, page, "Inner");
    add(&mut doc, inner, NodeKind::Rectangle, "Rect", 10.0, 10.0);
    let outer = component(&mut doc, page, "Outer");
    instance(&mut doc, inner, outer, "Nested", BTreeMap::new());
    let copy = instance(&mut doc, outer, page, "Outer copy", BTreeMap::new());
    let nested = doc.node(copy).unwrap().children[0];
    doc.node_mut(nested).unwrap().kind = NodeKind::Instance { component: NodeId(u64::MAX), props: BTreeMap::new() };
    assert_eq!(doc.node(nested).unwrap().children.len(), 1);
    let before = doc.clone();
    let Err(error) = export(&doc, None) else { panic!("a missing main must not silently discard nested children") };
    assert!(error.to_string().contains("nested instance type changes with children"), "{error}");
    assert!(doc == before, "a rejected export must leave the document unchanged");
}

#[test]
fn stage_b_multiple_override_rows_keep_local_and_outer_priority() {
    fn node(id: u32, parent: Option<(u32, &'static str)>, ty: &'static str, name: &str, more: Fields) -> Kv<'static> {
        let id_guid = |id| if id == 0 { guid(0, 0) } else { guid(LAYERS, id) };
        let mut fields = vec![("guid", id_guid(id)), ("type", e(ty)), ("name", s(name))];
        if let Some((parent, position)) = parent {
            fields.push(("parentIndex", o(vec![("guid", id_guid(parent)), ("position", s(position))])));
        }
        fields.extend(more);
        o(fields)
    }
    fn rows(path: &[u32], values: [f64; 2]) -> Kv<'static> {
        arr(values
            .into_iter()
            .map(|value| o(vec![("guidPath", o(vec![("guids", arr(path.iter().map(|id| guid(LAYERS, *id)).collect()))])), ("opacity", fl(value))]))
            .collect())
    }
    fn collision_rows(alias_path: &[u32], current_path: &[u32], values: [f64; 2]) -> Kv<'static> {
        arr([alias_path, current_path]
            .into_iter()
            .zip(values)
            .map(|(path, value)| {
                o(vec![("guidPath", o(vec![("guids", arr(path.iter().map(|id| guid(LAYERS, *id)).collect()))])), ("opacity", fl(value))])
            })
            .collect())
    }
    fn symbol(main: u32, path: &[u32], values: [f64; 2]) -> Kv<'static> {
        o(vec![("symbolID", guid(LAYERS, main)), ("symbolOverrides", rows(path, values))])
    }
    let msg = o(vec![(
        "nodeChanges",
        arr(vec![
            node(0, None, "DOCUMENT", "Document", vec![]),
            node(1, Some((0, "a")), "CANVAS", "Page", vec![]),
            node(2, Some((1, "a")), "SYMBOL", "Inner", vec![]),
            node(3, Some((2, "a")), "ROUNDED_RECTANGLE", "Rect", vec![("overrideKey", guid(LAYERS, 33)), ("opacity", fl(1.0))]),
            node(4, Some((1, "b")), "SYMBOL", "Outer", vec![]),
            node(
                5,
                Some((4, "a")),
                "INSTANCE",
                "Nested",
                vec![("overrideKey", guid(LAYERS, 55)), ("symbolData", symbol(2, &[3], [0.1, 0.2])), ("derivedSymbolData", rows(&[3], [0.3, 0.4]))],
            ),
            node(
                6,
                Some((1, "c")),
                "INSTANCE",
                "Outer copy",
                vec![
                    ("symbolData", o(vec![("symbolID", guid(LAYERS, 4)), ("symbolOverrides", collision_rows(&[55, 33], &[5, 3], [0.5, 0.6]))])),
                    ("derivedSymbolData", collision_rows(&[55, 33], &[5, 3], [0.7, 0.8])),
                ],
            ),
            node(7, Some((1, "d")), "INSTANCE", "Local explicit", vec![("symbolData", symbol(2, &[3], [0.1, 0.2]))]),
            node(
                8,
                Some((1, "e")),
                "INSTANCE",
                "Local derived",
                vec![("symbolData", symbol(2, &[3], [0.1, 0.2])), ("derivedSymbolData", rows(&[3], [0.3, 0.4]))],
            ),
        ]),
    )]);
    let (rest, warnings, _) = super::super::map::to_rest(&msg, "Override order").unwrap();
    assert!(warnings.is_empty(), "{warnings:?}");
    let page = &rest["document"]["children"][0];
    let find = |name: &str| page["children"].as_array().unwrap().iter().find(|n| n["name"] == name).unwrap();
    let opacity = |node: &Value| node["opacity"].as_f64().unwrap();
    assert_eq!(opacity(&find("Local explicit")["children"][0]), f64::from(0.2f32));
    assert_eq!(opacity(&find("Local derived")["children"][0]), f64::from(0.4f32));
    assert_eq!(opacity(&find("Outer copy")["children"][0]["children"][0]), f64::from(0.8f32));
}

#[test]
fn stage_b_outer_derived_geometry_wins_over_local() {
    let mut doc = Document::new("Nested geometry");
    let page = doc.first_page().unwrap();
    let inner = component(&mut doc, page, "Inner");
    add(&mut doc, inner, NodeKind::Rectangle, "Rect", 10.0, 10.0);
    let outer = component(&mut doc, page, "Outer");
    let local = instance(&mut doc, inner, outer, "Nested", BTreeMap::new());
    let local_rect = doc.node(local).unwrap().children[0];
    doc.node_mut(local_rect).unwrap().width = 20.0;
    doc.node_mut(local_rect).unwrap().transform = Affine::translate((2.0, 0.0));
    let copy = instance(&mut doc, outer, page, "Outer copy", BTreeMap::new());
    let nested = doc.node(copy).unwrap().children[0];
    let outer_rect = doc.node(nested).unwrap().children[0];
    doc.node_mut(outer_rect).unwrap().width = 30.0;
    doc.node_mut(outer_rect).unwrap().transform = Affine::translate((3.0, 0.0));
    let doc = back(&doc);
    let nested = copied_nested(&doc);
    let rect = doc.node(nested.children[0]).unwrap();
    assert_eq!(rect.width, 30.0);
    assert_eq!(rect.transform.translation().x, 3.0);
}

#[test]
fn stage_b_archive_budgets_reject_before_building_zip() {
    assert!(check_archive_budget(2, [3, 4], 3, 12, 2, 7).is_ok());
    assert!(check_archive_budget(2, [3, 4], 3, 11, 2, 7).is_err());
    assert!(check_archive_budget(2, [3, 4], 3, 12, 1, 7).is_err());
    assert!(check_archive_budget(2, [3, 4], 3, 12, 2, 6).is_err());
    assert!(check_archive_budget(u64::MAX, [], 1, u64::MAX, 2, 7).is_err());
    assert!(check_archive_budget(2, [u64::MAX], 3, u64::MAX, 2, u64::MAX).is_err());
}

#[test]
fn stage_b_actual_archive_length_is_capped() {
    let bytes = archive_with_limit("Tiny", Color::WHITE, b"fig-kiwi", &[], None, 1024).unwrap();
    let exact = bytes.len() as u64;
    assert!(archive_with_limit("Tiny", Color::WHITE, b"fig-kiwi", &[], None, exact).is_ok());
    assert!(archive_with_limit("Tiny", Color::WHITE, b"fig-kiwi", &[], None, exact - 1).is_err());
}

fn unlinked_nested_swap() -> (Document, NodeId, NodeId, NodeId, NodeId) {
    let mut doc = Document::new("Unlinked nested swap");
    let page = doc.first_page().unwrap();
    let left = component(&mut doc, page, "Left main");
    let left_frame = add(&mut doc, left, NodeKind::Frame, "Left frame", 39.0, 24.0);
    doc.node_mut(left_frame).unwrap().transform = Affine::translate((2.0, 2.0));
    let right = component(&mut doc, page, "Right main");
    let right_frame = add(&mut doc, right, NodeKind::Frame, "Right frame", 39.0, 24.0);
    doc.node_mut(right_frame).unwrap().transform = Affine::translate((23.0, 2.0));
    let wrapper = component(&mut doc, page, "Wrapper main");
    let slot = instance(&mut doc, right, wrapper, "Default nested", BTreeMap::new());
    let outer = component(&mut doc, page, "Outer");
    instance(&mut doc, wrapper, outer, "Wrapper", BTreeMap::new());
    let copy = instance(&mut doc, outer, page, "Outer copy", BTreeMap::new());
    let copied_wrapper = doc.node(copy).unwrap().children[0];
    let old = doc.node(copied_wrapper).unwrap().children[0];
    doc.remove(old).unwrap();
    let swapped = instance(&mut doc, left, copied_wrapper, "Renamed nested", BTreeMap::new());
    let child = doc.node(swapped).unwrap().children[0];
    doc.node_mut(child).unwrap().source = None;
    (doc, copied_wrapper, swapped, left_frame, slot)
}

fn assert_left_swap(doc: &Document) {
    let wrapper = copied_nested(doc);
    let swapped = doc.node(wrapper.children[0]).unwrap();
    let NodeKind::Instance { component, .. } = &swapped.kind else { panic!("the swapped child must remain an instance") };
    assert_eq!(doc.node(*component).unwrap().name, "Left main", "compare main identity independently of native IDs");
    assert_eq!(swapped.name, "Renamed nested");
    assert_eq!(swapped.children.len(), 1);
    let child = doc.node(swapped.children[0]).unwrap();
    assert!(matches!(&child.kind, NodeKind::Frame));
    assert_eq!((child.transform.translation().x, child.transform.translation().y, child.width, child.height), (2.0, 2.0, 39.0, 24.0));
}

#[test]
fn stage_b_unlinked_single_child_nested_swap_round_trips() {
    let (doc, _, _, _, _) = unlinked_nested_swap();
    let msg = written(&doc);
    let nodes = msg["nodeChanges"].as_array().unwrap();
    let left = &nodes.iter().find(|n| n["name"] == "Left main").unwrap()["guid"];
    let outer = nodes.iter().find(|n| n["name"] == "Outer copy").unwrap();
    assert!(
        outer["symbolData"]["symbolOverrides"]
            .as_array()
            .unwrap()
            .iter()
            .any(|row| { row["overriddenSymbolID"] == *left && row["guidPath"]["guids"].as_array().is_some_and(|path| path.len() == 2) }),
        "the desired nested main must be written at its source slot"
    );
    assert_left_swap(&back(&doc));
}

#[test]
fn stage_b_unplaced_single_child_nested_swap_round_trips() {
    let (mut doc, _, swapped, left_frame, _) = unlinked_nested_swap();
    let component = match &doc.node(swapped).unwrap().kind {
        NodeKind::Instance { component, .. } => *component,
        _ => panic!(),
    };
    doc.node_mut(swapped).unwrap().source = Some(component);
    let child = doc.node(swapped).unwrap().children[0];
    doc.node_mut(child).unwrap().source = Some(left_frame);
    assert_left_swap(&back(&doc));
}

#[test]
fn stage_b_single_child_swap_recovers_derived_geometry() {
    let (mut doc, _, swapped, _, _) = unlinked_nested_swap();
    let child = doc.node(swapped).unwrap().children[0];
    doc.node_mut(child).unwrap().transform = Affine::translate((3.0, 4.0));
    let back = back(&doc);
    let nested = back.node(copied_nested(&back).children[0]).unwrap();
    let NodeKind::Instance { component, .. } = &nested.kind else { panic!() };
    assert_eq!(back.node(*component).unwrap().name, "Left main");
    let child = back.node(nested.children[0]).unwrap();
    assert_eq!((child.transform.translation().x, child.transform.translation().y), (3.0, 4.0));
}

#[test]
fn stage_b_explicit_nested_sources_take_priority() {
    let (mut doc, wrapper, swapped, left_frame, slot) = unlinked_nested_swap();
    doc.node_mut(swapped).unwrap().source = Some(slot);
    let child = doc.node(swapped).unwrap().children[0];
    doc.node_mut(child).unwrap().source = Some(left_frame);
    // Explicit links remain authoritative even when the conservative fallback is inapplicable.
    doc.node_mut(wrapper).unwrap().layout = Some(AutoLayout::default());
    assert_left_swap(&back(&doc));
}

#[test]
fn stage_b_ambiguous_nested_source_is_rejected() {
    let (mut doc, wrapper, _, _, _) = unlinked_nested_swap();
    add(&mut doc, wrapper, NodeKind::Frame, "Extra local child", 10.0, 10.0);
    let before = doc.clone();
    let Err(error) = export(&doc, None) else { panic!("multiple current child slots must not be guessed") };
    assert!(error.to_string().contains("no unambiguous source layer"), "{error}");
    assert!(doc == before, "a rejected export must leave the document unchanged");
}

#[test]
fn stage_b_nested_source_fallback_excludes_slots_and_layout_changes() {
    for mode in 0..3 {
        let (mut doc, wrapper, _, _, _) = unlinked_nested_swap();
        match mode {
            0 => {
                doc.node_mut(wrapper).unwrap().prop_refs.insert("slot".into(), "Content".into());
            }
            1 => {
                doc.node_mut(wrapper).unwrap().overrides.insert("slot".into());
            }
            _ => {
                doc.node_mut(wrapper).unwrap().layout = Some(AutoLayout::default());
            }
        }
        let before = doc.clone();
        let Err(error) = export(&doc, None) else { panic!("slot content and changed layout modes must not acquire guessed sources") };
        assert!(error.to_string().contains("no unambiguous source layer"), "{error}");
        assert!(doc == before, "a rejected export must leave the document unchanged");
    }
}

fn contextual_ancestor_swap() -> (Document, NodeId, NodeId, NodeId, NodeId) {
    let mut doc = Document::new("Context address after ancestor swap");
    let page = doc.first_page().unwrap();
    let leaf = component(&mut doc, page, "Leaf main");
    doc.node_mut(leaf).unwrap().width = 200.0;
    doc.node_mut(leaf).unwrap().height = 200.0;
    add(&mut doc, leaf, NodeKind::Rectangle, "Leaf rect", 10.0, 10.0);
    let inner = component(&mut doc, page, "Inner main");
    doc.node_mut(inner).unwrap().height = 100.0;
    let raw_frame = add(&mut doc, inner, NodeKind::Frame, "Inner frame", 100.0, 100.0);
    doc.node_mut(raw_frame).unwrap().transform = Affine::translate((-26.0, -26.0));
    doc.node_mut(raw_frame).unwrap().constraints.horizontal = HConstraint::LeftRight;
    doc.node_mut(raw_frame).unwrap().constraints.vertical = VConstraint::TopBottom;
    let raw_slot = instance(&mut doc, leaf, raw_frame, "Leaf slot", BTreeMap::new());
    doc.node_mut(raw_slot).unwrap().transform = Affine::translate((-50.0, -50.0));
    doc.node_mut(raw_slot).unwrap().constraints.horizontal = HConstraint::LeftRight;
    doc.node_mut(raw_slot).unwrap().constraints.vertical = VConstraint::TopBottom;
    add(&mut doc, inner, NodeKind::Rectangle, "Second inner child", 6.0, 6.0);
    let actual = component(&mut doc, page, "Actual main");
    doc.node_mut(actual).unwrap().width = 121.0;
    doc.node_mut(actual).unwrap().height = 100.0;
    let actual_inner = instance(&mut doc, inner, actual, "First nested", BTreeMap::new());
    doc.node_mut(actual_inner).unwrap().width = 121.0;
    let context_frame = doc.node(actual_inner).unwrap().children[0];
    doc.node_mut(context_frame).unwrap().width = 121.0;
    let context_slot = doc.node(context_frame).unwrap().children[0];
    doc.node_mut(context_slot).unwrap().width = 221.0;
    instance(&mut doc, leaf, actual, "Second nested", BTreeMap::new());
    let default = component(&mut doc, page, "Default main");
    instance(&mut doc, inner, default, "Old nested", BTreeMap::new());
    let outer = component(&mut doc, page, "Outer main");
    let outer_frame = add(&mut doc, outer, NodeKind::Frame, "Outer frame", 100.0, 40.0);
    let default_slot = instance(&mut doc, default, outer_frame, "Default slot", BTreeMap::new());
    let copy = instance(&mut doc, outer, page, "Outer copy", BTreeMap::new());
    let copied_outer_frame = doc.node(copy).unwrap().children[0];
    let old = doc.node(copied_outer_frame).unwrap().children[0];
    doc.remove(old).unwrap();
    let current = instance(&mut doc, actual, copied_outer_frame, "Renamed actual", BTreeMap::new());
    doc.node_mut(current).unwrap().source = Some(default_slot);
    let current_inner = doc.node(current).unwrap().children[0];
    doc.node_mut(current_inner).unwrap().name = "Renamed inner".into();
    let current_frame = doc.node(current_inner).unwrap().children[0];
    let current_slot = doc.node(current_frame).unwrap().children[0];
    doc.node_mut(current_frame).unwrap().transform = Affine::translate((-25.0, -26.0));
    doc.node_mut(current_slot).unwrap().name = "Renamed leaf".into();
    doc.node_mut(current_slot).unwrap().visible = false;
    (doc, copy, current_frame, current_slot, raw_frame)
}

fn assert_contextual_address_round_trip(doc: &Document) {
    let copy = doc.nodes().find(|n| n.name == "Outer copy").unwrap();
    let outer_frame = doc.node(copy.children[0]).unwrap();
    let actual = doc.node(outer_frame.children[0]).unwrap();
    let NodeKind::Instance { component: actual_main, .. } = &actual.kind else { panic!() };
    assert_eq!(actual.name, "Renamed actual");
    assert_eq!(doc.node(*actual_main).unwrap().name, "Actual main");
    let default_slot = doc.node(actual.source.unwrap()).unwrap();
    let NodeKind::Instance { component: default_main, .. } = &default_slot.kind else { panic!() };
    assert_eq!(default_slot.name, "Default slot");
    assert_eq!(doc.node(*default_main).unwrap().name, "Default main");
    assert_ne!(actual_main, default_main);
    let inner = doc.node(actual.children[0]).unwrap();
    let main_inner = doc.node(doc.node(*actual_main).unwrap().children[0]).unwrap();
    assert_eq!(inner.source, Some(main_inner.id));
    assert_eq!(inner.name, "Renamed inner");
    let NodeKind::Instance { component: inner_main, .. } = &inner.kind else { panic!() };
    assert!(matches!(&main_inner.kind, NodeKind::Instance { component, .. } if component == inner_main));
    assert_eq!(actual.children.len(), 2);
    let raw_frame = doc.node(doc.node(*inner_main).unwrap().children[0]).unwrap();
    let frame = doc.node(inner.children[0]).unwrap();
    let context_frame = doc.node(main_inner.children[0]).unwrap();
    assert!(matches!(frame.kind, NodeKind::Frame));
    assert_eq!(frame.source, Some(context_frame.id));
    assert_eq!(context_frame.source, Some(raw_frame.id));
    assert_eq!((frame.width, frame.height), (121.0, 100.0));
    assert_eq!((context_frame.width, context_frame.height), (121.0, 100.0));
    assert_eq!((raw_frame.width, raw_frame.height), (100.0, 100.0));
    assert_eq!((frame.transform.translation().x, frame.transform.translation().y), (-25.0, -26.0));
    assert_eq!((context_frame.transform.translation().x, context_frame.transform.translation().y), (-26.0, -26.0));
    assert_eq!(frame.constraints, context_frame.constraints);
    let slot = doc.node(frame.children[0]).unwrap();
    let context_slot = doc.node(context_frame.children[0]).unwrap();
    let raw_slot = doc.node(raw_frame.children[0]).unwrap();
    assert_eq!(slot.source, Some(context_slot.id));
    assert_eq!(context_slot.source, Some(raw_slot.id));
    assert!(matches!(&slot.kind, NodeKind::Instance { component, .. } if doc.node(*component).unwrap().name == "Leaf main"));
    assert_eq!((slot.width, slot.height), (221.0, 200.0));
    assert_eq!(slot.name, "Renamed leaf");
    assert!(!slot.visible);
    assert_eq!(slot.children.len(), 1);
}

#[test]
fn stage_b_context_addresses_after_ancestor_swap_keep_scalar_baselines() {
    let (mut doc, copy, frame, slot, raw_frame) = contextual_ancestor_swap();
    let outer = match &doc.node(copy).unwrap().kind {
        NodeKind::Instance { component, .. } => *component,
        _ => panic!(),
    };
    let actual = doc.node(doc.node(copy).unwrap().children[0]).unwrap().children[0];
    let default_slot = doc.node(actual).unwrap().source.unwrap();
    let inner = doc.node(frame).unwrap().parent.unwrap();
    let main_inner = doc.node(inner).unwrap().source.unwrap();
    let raw_slot = doc.node(raw_frame).unwrap().children[0];
    let mut x = Exporter::new(&doc).unwrap();
    assert_eq!(x.layer_path(doc.node(frame).unwrap(), copy, outer, 0), Some(vec![default_slot, main_inner, raw_frame]));
    assert_eq!(x.layer_path(doc.node(slot).unwrap(), copy, outer, 0), Some(vec![default_slot, main_inner, raw_slot]));
    let scalar_source = x.layer_source(doc.node(frame).unwrap(), 0).unwrap();
    assert_eq!(scalar_source, doc.node(main_inner).unwrap().children[0]);
    assert_eq!(doc.node(scalar_source).unwrap().width, 121.0);
    assert_eq!(doc.node(raw_frame).unwrap().width, 100.0);
    x.assign();
    let guid = |id| json!({"sessionID": LAYERS, "localID": x.ids[&id]});
    let frame_path = json!([guid(default_slot), guid(main_inner), guid(raw_frame)]);
    let slot_path = json!([guid(default_slot), guid(main_inner), guid(raw_slot)]);
    let msg = written(&doc);
    let outer = msg["nodeChanges"].as_array().unwrap().iter().find(|n| n["name"] == "Outer copy").unwrap();
    assert!(
        outer["derivedSymbolData"]
            .as_array()
            .unwrap()
            .iter()
            .any(|row| { row["guidPath"]["guids"] == frame_path && row["transform"]["m02"] == -25.0 && row.get("size").is_none() }),
        "a geometry delta uses the canonical GUID but keeps inherited 121px as its scalar baseline"
    );
    let overrides = outer["symbolData"]["symbolOverrides"].as_array().unwrap();
    assert!(
        !overrides.iter().any(|row| row["guidPath"]["guids"] == frame_path && row.get("size").is_some()),
        "contextual 121px is inherited, not a new size override against raw 100px"
    );
    assert!(overrides.iter().any(|row| row["guidPath"]["guids"] == slot_path && row["visible"] == false));
    assert_contextual_address_round_trip(&doc);
    for _ in 0..2 {
        doc = back(&doc);
        assert_contextual_address_round_trip(&doc);
    }
}

#[test]
fn stage_b_context_address_rejects_foreign_siblings_and_wrong_parents() {
    for mode in 0..3 {
        let (mut doc, _, frame, slot, raw_frame) = contextual_ancestor_swap();
        let current_inner = doc.node(frame).unwrap().parent.unwrap();
        let main_inner = doc.node(current_inner).unwrap().source.unwrap();
        let context_frame = doc.node(frame).unwrap().source.unwrap();
        let inner_main = match &doc.node(main_inner).unwrap().kind {
            NodeKind::Instance { component, .. } => *component,
            _ => panic!(),
        };
        let actual_main = doc.node(main_inner).unwrap().parent.unwrap();
        let candidate = if mode < 2 {
            let foreign_main = if mode == 0 {
                inner_main
            } else {
                let page = doc.first_page().unwrap();
                let foreign = component(&mut doc, page, "Foreign main");
                add(&mut doc, foreign, NodeKind::Frame, "Foreign frame", 100.0, 100.0);
                foreign
            };
            let foreign_owner = instance(&mut doc, foreign_main, actual_main, "Foreign sibling", BTreeMap::new());
            let foreign_frame = doc.node(foreign_owner).unwrap().children[0];
            if mode == 1 {
                doc.node_mut(main_inner).unwrap().source = Some(foreign_owner);
            }
            doc.node_mut(context_frame).unwrap().source = Some(foreign_frame);
            context_frame
        } else {
            let other_frame = add(&mut doc, inner_main, NodeKind::Frame, "Other parent", 100.0, 100.0);
            let leaf = match &doc.node(doc.node(raw_frame).unwrap().children[0]).unwrap().kind {
                NodeKind::Instance { component, .. } => *component,
                _ => panic!(),
            };
            let other_slot = instance(&mut doc, leaf, other_frame, "Foreign slot", BTreeMap::new());
            let context_slot = doc.node(slot).unwrap().source.unwrap();
            doc.node_mut(context_slot).unwrap().source = Some(other_slot);
            context_slot
        };
        assert_eq!(Exporter::new(&doc).unwrap().path_in(candidate, inner_main, 0), None, "invalid context mode {mode}");
    }
}

#[test]
fn stage_b_context_address_validates_whole_source_and_parent_cycles() {
    let mut doc = Document::new("Address cycles");
    let page = doc.first_page().unwrap();
    let inner = component(&mut doc, page, "Inner");
    let raw = add(&mut doc, inner, NodeKind::Frame, "Raw", 10.0, 10.0);
    let outer = component(&mut doc, page, "Outer");
    let a = instance(&mut doc, inner, outer, "A", BTreeMap::new());
    let b = instance(&mut doc, inner, outer, "B", BTreeMap::new());
    let ca = doc.node(a).unwrap().children[0];
    let cb = doc.node(b).unwrap().children[0];
    doc.node_mut(a).unwrap().source = Some(b);
    doc.node_mut(b).unwrap().source = Some(a);
    doc.node_mut(ca).unwrap().source = Some(cb);
    doc.node_mut(cb).unwrap().source = Some(ca);
    assert_eq!(Exporter::new(&doc).unwrap().path_in(ca, outer, 0), None, "a physical address must not hide a later source cycle");
    doc.node_mut(ca).unwrap().source = Some(ca);
    assert_eq!(Exporter::new(&doc).unwrap().path_in(ca, outer, 0), None);
    doc.node_mut(raw).unwrap().parent = Some(raw);
    assert_eq!(Exporter::new(&doc).unwrap().path_in(raw, inner, 0), None, "a physical address must not hide a parent cycle");
}

#[test]
fn stage_b_context_address_bounds_sources_and_allows_format_depth() {
    let mut doc = Document::new("Address budgets");
    let page = doc.first_page().unwrap();
    let inner = component(&mut doc, page, "Inner");
    let raw = add(&mut doc, inner, NodeKind::Frame, "Raw", 10.0, 10.0);
    let outer = component(&mut doc, page, "Outer");
    let mut previous = None;
    let mut last = raw;
    for _ in 0..40 {
        let owner = instance(&mut doc, inner, outer, "Context", BTreeMap::new());
        let child = doc.node(owner).unwrap().children[0];
        if let Some((previous_owner, previous_child)) = previous {
            doc.node_mut(owner).unwrap().source = Some(previous_owner);
            doc.node_mut(child).unwrap().source = Some(previous_child);
        }
        previous = Some((owner, child));
        last = child;
    }
    assert_eq!(Exporter::new(&doc).unwrap().path_in(last, outer, 0), None);
    let deep = component(&mut doc, page, "Deep");
    let mut parent = deep;
    for _ in 0..256 {
        parent = add(&mut doc, parent, NodeKind::Frame, "Frame", 10.0, 10.0);
    }
    let leaf = add(&mut doc, parent, NodeKind::Rectangle, "Deep leaf", 1.0, 1.0);
    assert_eq!(Exporter::new(&doc).unwrap().path_in(leaf, deep, 0), Some(vec![leaf]));
    for _ in 0..256 {
        parent = add(&mut doc, parent, NodeKind::Frame, "Over budget", 10.0, 10.0);
    }
    let too_deep = add(&mut doc, parent, NodeKind::Rectangle, "Over budget leaf", 1.0, 1.0);
    assert_eq!(Exporter::new(&doc).unwrap().path_in(too_deep, deep, 0), None);
}
