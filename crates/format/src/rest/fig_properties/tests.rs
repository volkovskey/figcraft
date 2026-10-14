use super::*;
use r::{Alias, Binding, Bindings, Definition, NativePacket, Owner, Remap, WirePacket, WireResources};
use serde_json::json;
fn id(session: u32, local: u32) -> Id {
    Id { session, local }
}
fn fixture() -> (Value, Catalog, Bindings) {
    let catalog = r::make_catalog(vec![Owner {
        id: id(20, 2),
        page: id(20, 1),
        name: "Retained".into(),
        definitions: vec![Definition { id: id(30, 1), name: "Toggle".into(), initial: Some(false) }],
    }])
    .unwrap();
    let tag = MainTag { version: r::VERSION, scope: catalog.scope.clone(), origin: id(40, 1) };
    let bindings = Bindings {
        version: r::VERSION,
        scope: catalog.scope.clone(),
        main: tag.origin,
        visible_main: None,
        values: vec![Binding { key: "Toggle#30:1".into(), definition: id(30, 1) }],
        visible: None,
    };
    let main_packet =
        WirePacket { version: r::VERSION, native: NativePacket { main: Some(tag), ..Default::default() }, resources: None, aliases: vec![] };
    let use_packet = WirePacket {
        version: r::VERSION,
        native: NativePacket { bindings: Some(bindings.clone()), ..Default::default() },
        resources: None,
        aliases: vec![Alias { role: Role::Value, key: "Toggle#30:1".into(), definition: id(30, 1), wire: id(13, 1) }],
    };
    let page_packet = WirePacket {
        version: r::VERSION,
        native: NativePacket { catalog: Some(catalog.clone()), ..Default::default() },
        resources: Some(WireResources {
            scope: catalog.scope.clone(),
            page: id(12, 1),
            owners: vec![Remap { original: id(20, 2), wire: id(12, 2) }],
            definitions: vec![Remap { original: id(30, 1), wire: id(13, 1) }],
        }),
        aliases: vec![],
    };
    let mut main = json!({"id":"10:2","type":"COMPONENT","name":"Main","children":[]});
    main[r::FIG_PACKET_FIELD] = json!(r::write_wire(&main_packet).unwrap());
    let mut used = json!({"id":"10:3","type":"INSTANCE","name":"Use","componentId":"10:2","componentProperties":{"Toggle#13:1":{"type":"BOOLEAN","value":false}}});
    used[r::FIG_PACKET_FIELD] = json!(r::write_wire(&use_packet).unwrap());
    let mut page = json!({"id":"10:1","type":"CANVAS","name":"Page","children":[main,used]});
    page[r::FIG_PACKET_FIELD] = json!(r::write_wire(&page_packet).unwrap());
    let mut input = json!({"name":"Synthetic","document":{"id":"10:0","type":"DOCUMENT","children":[page]}});
    input[r::FIG_CATALOGS_FIELD] = json!([r::write_catalog(&catalog).unwrap()]);
    (input, catalog, bindings)
}
fn imported(v: &Value) -> Document {
    crate::rest::import_fig(v, &|_| None, Vec::new()).unwrap().0
}
fn used(doc: &Document) -> &figcraft_doc::Node {
    doc.nodes().find(|n| n.name == "Use").unwrap()
}

#[test]
fn public_rest_ignores_all_private_resource_packets_and_catalogs() {
    let (mut v, _, _) = fixture();
    v[r::FIG_CATALOGS_FIELD] = json!("invalid private catalog");
    v["document"]["children"][0][r::FIG_PACKET_FIELD] = json!("invalid private packet");
    let doc = crate::rest::import(&v, &|_| None).unwrap().0;
    assert!(doc.nodes().all(|n| n.plugin_data.is_empty()));
    assert!(matches!(&used(&doc).kind,NodeKind::Instance { props,.. } if props.get("Toggle")==Some(&json!(false))));
}

#[test]
fn alias_restore_retains_original_native_key_and_zero_main_definitions() {
    let (v, catalog, bindings) = fixture();
    let doc = imported(&v);
    let n = used(&doc);
    assert!(matches!(&n.kind,NodeKind::Instance { props,.. } if props.get("Toggle#30:1")==Some(&json!(false)) && !props.contains_key("Toggle")));
    assert_eq!(n.plugin_data.get(r::BINDINGS_KEY), Some(&r::write_bindings(&bindings).unwrap()));
    assert_eq!(doc.node(doc.first_page().unwrap()).unwrap().plugin_data.get(r::CATALOG_KEY), Some(&r::write_catalog(&catalog).unwrap()));
    assert!(
        doc.nodes()
            .filter_map(|n| match &n.kind {
                NodeKind::Component { props, .. } => Some(props),
                _ => None,
            })
            .all(BTreeMap::is_empty)
    );
}

#[test]
fn same_scope_multiple_carriers_preserve_both_page_catalogs_and_actual_mains() {
    let (mut v, catalog, _) = fixture();
    let mut page = v["document"]["children"][0].clone();
    page["id"] = json!("10:11");
    page["children"][0]["id"] = json!("10:12");
    page["children"][1]["id"] = json!("10:13");
    page["children"][1]["componentId"] = json!("10:12");
    v["document"]["children"].as_array_mut().unwrap().push(page);
    let doc = imported(&v);
    assert_eq!(doc.pages.len(), 2);
    let text = r::write_catalog(&catalog).unwrap();
    assert!(doc.pages.iter().all(|p| doc.node(*p).unwrap().plugin_data.get(r::CATALOG_KEY) == Some(&text)));
    let actual: BTreeSet<_> = doc
        .nodes()
        .filter_map(|n| match &n.kind {
            NodeKind::Instance { component, props } => {
                assert_eq!(props.get("Toggle#30:1"), Some(&json!(false)));
                Some(*component)
            }
            _ => None,
        })
        .collect();
    assert_eq!(actual.len(), 2, "each instance must keep its actual decoded main");
}

fn props(doc: &Document) -> &BTreeMap<String, Value> {
    match &used(doc).kind {
        NodeKind::Instance { props, .. } => props,
        _ => panic!("instance lost its type"),
    }
}
fn stale(warnings: &[String]) -> bool {
    warnings.iter().any(|w| w.ends_with(r::DROPPED_STALE))
}

#[test]
fn foreign_main_and_native_key_collisions_leave_out_only_the_packet() {
    // A swap to a foreign main: the alias cannot claim it, its wire-named value goes too.
    let (mut v, _, _) = fixture();
    v["document"]["children"][0]["children"][1]["componentId"] = json!("10:9");
    v["document"]["children"][0]["children"].as_array_mut().unwrap().push(json!({"id":"10:9","type":"COMPONENT","name":"Foreign"}));
    let (doc, warnings) = crate::rest::import_fig(&v, &|_| None, Vec::new()).unwrap();
    assert!(stale(&warnings), "{warnings:?}");
    assert!(!used(&doc).plugin_data.contains_key(r::BINDINGS_KEY));
    assert!(props(&doc).is_empty(), "no value may be restored under the foreign main: {:?}", props(&doc));
    // A native key already taken: the other value is not overwritten by the alias.
    let (mut v, _, _) = fixture();
    v["document"]["children"][0]["children"][1]["componentProperties"]["Toggle#30:1"] = json!({"type":"BOOLEAN","value":true});
    let (doc, warnings) = crate::rest::import_fig(&v, &|_| None, Vec::new()).unwrap();
    assert!(stale(&warnings), "{warnings:?}");
    assert!(!used(&doc).plugin_data.contains_key(r::BINDINGS_KEY));
    assert_eq!(props(&doc).get("Toggle#30:1"), Some(&json!(true)));
    assert!(!props(&doc).contains_key("Toggle#13:1"));
    // A live main definition with the native key: the alias must not rebind it.
    let (mut v, _, _) = fixture();
    v["document"]["children"][0]["children"][0]["componentPropertyDefinitions"] = json!({"Toggle#30:1":{"type":"BOOLEAN","defaultValue":false}});
    let (doc, warnings) = crate::rest::import_fig(&v, &|_| None, Vec::new()).unwrap();
    assert!(stale(&warnings), "an active retained alias must not rebind a live main definition: {warnings:?}");
    assert!(!used(&doc).plugin_data.contains_key(r::BINDINGS_KEY));
    assert!(!props(&doc).contains_key("Toggle#30:1"), "the alias value was moved onto the live definition");
    let main = doc.nodes().find(|n| n.name == "Main").unwrap();
    assert!(matches!(&main.kind, NodeKind::Component { props, .. } if props.contains_key("Toggle#30:1")));
}

#[test]
fn inactive_old_bindings_survive_known_main_swap_without_an_alias_claim() {
    let (mut v, _, bindings) = fixture();
    let page = &mut v["document"]["children"][0];
    let node = &mut page["children"][1];
    let mut packet = r::read_wire(node[r::FIG_PACKET_FIELD].as_str().unwrap()).unwrap();
    packet.aliases.clear();
    node[r::FIG_PACKET_FIELD] = json!(r::write_wire(&packet).unwrap());
    node["componentId"] = json!("10:9");
    node["componentProperties"] = json!({"Live":{"type":"BOOLEAN","value":true}});
    page["children"]
        .as_array_mut()
        .unwrap()
        .push(json!({"id":"10:9","type":"COMPONENT","name":"Known","componentPropertyDefinitions":{"Live":{"type":"BOOLEAN","defaultValue":false}}}));
    let doc = imported(&v);
    let n = used(&doc);
    assert_eq!(n.plugin_data.get(r::BINDINGS_KEY), Some(&r::write_bindings(&bindings).unwrap()));
    assert!(matches!(&n.kind,NodeKind::Instance { props,.. } if props.get("Live")==Some(&json!(true)) && !props.contains_key("Toggle#30:1")));
}

#[test]
fn active_resource_alias_cannot_take_a_live_main_wire_definition() {
    let (mut v, _, _) = fixture();
    v["document"]["children"][0]["children"][0]["componentPropertyDefinitions"] = json!({"Toggle#13:1":{"type":"BOOLEAN","defaultValue":true}});
    let (doc, warnings) = crate::rest::import_fig(&v, &|_| None, Vec::new()).unwrap();
    assert!(stale(&warnings), "{warnings:?}");
    assert!(!used(&doc).plugin_data.contains_key(r::BINDINGS_KEY));
    // The actual standard definition remains linked to its assignment.
    let main = doc.nodes().find(|n| n.name == "Main").unwrap();
    let NodeKind::Component { props: defs, .. } = &main.kind else { panic!("main lost its type") };
    assert_eq!(defs.len(), 1);
    let live = defs.keys().next().unwrap();
    assert_eq!(props(&doc).get(live), Some(&json!(false)));
    assert!(!props(&doc).contains_key("Toggle#30:1"));
}

#[test]
fn oversized_native_bindings_roll_back_aliases_and_keep_live_properties() {
    let (mut v, catalog, mut bindings) = fixture();
    let definitions: Vec<Definition> = (1..=17).map(|i| Definition { id: id(30, i), name: format!("Flag{i}"), initial: Some(false) }).collect();
    let mut owners = catalog.owners;
    owners[0].definitions = definitions.clone();
    let catalog = r::make_catalog(owners).unwrap();
    bindings.scope.clone_from(&catalog.scope);
    bindings.values = definitions.iter().enumerate().map(|(i, d)| Binding { key: format!("{i:02}{}", "x".repeat(4088)), definition: d.id }).collect();
    assert!(r::write_bindings(&bindings).is_err(), "native bindings exceed the 64 KiB cap");
    let aliases: Vec<Alias> = bindings
        .values
        .iter()
        .enumerate()
        .map(|(i, b)| Alias { role: Role::Value, key: b.key.clone(), definition: b.definition, wire: id(13, i as u32 + 1) })
        .collect();
    let packet = WirePacket {
        version: r::VERSION,
        native: NativePacket { bindings: Some(bindings), ..Default::default() },
        resources: None,
        aliases: aliases.clone(),
    };
    let text = r::write_wire(&packet).unwrap();
    assert!(text.len() < r::MAX_WIRE_PACKET_BYTES);
    assert_eq!(r::read_wire(&text).unwrap(), packet, "wire packet is valid before native serialization");
    let page = &mut v["document"]["children"][0];
    let mut page_packet = r::read_wire(page[r::FIG_PACKET_FIELD].as_str().unwrap()).unwrap();
    page_packet.native.catalog = Some(catalog.clone());
    let resources = page_packet.resources.as_mut().unwrap();
    resources.scope.clone_from(&catalog.scope);
    resources.definitions = definitions.iter().enumerate().map(|(i, d)| Remap { original: d.id, wire: id(13, i as u32 + 1) }).collect();
    page[r::FIG_PACKET_FIELD] = json!(r::write_wire(&page_packet).unwrap());
    let main = &mut page["children"][0];
    let mut main_packet = r::read_wire(main[r::FIG_PACKET_FIELD].as_str().unwrap()).unwrap();
    main_packet.native.main.as_mut().unwrap().scope.clone_from(&catalog.scope);
    main[r::FIG_PACKET_FIELD] = json!(r::write_wire(&main_packet).unwrap());
    main["componentPropertyDefinitions"] = json!({"Live":{"type":"BOOLEAN","defaultValue":false}});
    let mut fields: serde_json::Map<String, Value> =
        definitions.iter().zip(&aliases).map(|(d, a)| (format!("{}#{}", d.name, a.wire.key()), json!({"type":"BOOLEAN","value":false}))).collect();
    fields.insert("Live".into(), json!({"type":"BOOLEAN","value":true}));
    let node = &mut page["children"][1];
    node["componentProperties"] = Value::Object(fields);
    node[r::FIG_PACKET_FIELD] = json!(text);
    v[r::FIG_CATALOGS_FIELD] = json!([r::write_catalog(&catalog).unwrap()]);
    let original = v.clone();
    let (doc, warnings) = crate::rest::import_fig(&v, &|_| None, Vec::new()).unwrap();
    assert_eq!(v, original, "the input packet must remain unchanged");
    assert_eq!(warnings.iter().filter(|w| w.ends_with(r::DROPPED_LIMIT)).collect::<Vec<_>>(), [&format!("1 {}", r::DROPPED_LIMIT)]);
    assert!(!used(&doc).plugin_data.contains_key(r::BINDINGS_KEY));
    assert_eq!(props(&doc), &BTreeMap::from([("Live".into(), json!(true))]), "only the actual main's live property remains");
    let main = doc.nodes().find(|n| n.name == "Main").unwrap();
    assert!(
        matches!(&main.kind, NodeKind::Component { props, .. } if props.len() == 1 && props.contains_key("Live")),
        "no private definitions or defaults may be invented"
    );
}
