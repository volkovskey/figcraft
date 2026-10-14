//! Decoded `.fig` message → REST API file JSON, which [`crate::rest`] then imports.
//!
//! The message lists node changes; each names its id (`guid`), its parent and its position among
//! the siblings (`parentIndex`). Nodes are read by field name, so files written with newer
//! schemas still import what is understood. Instances carry only their overrides: their layers
//! are expanded here from the main component (overrides and derived layout matched by id path,
//! component property values applied) so the import looks as saved.

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::rc::Rc;

use figcraft_doc::VectorNetwork;
use figcraft_doc::geom::{Affine, Point};
use figcraft_doc::proto::{MAX_ACTION_DEPTH, MAX_EXPR_DEPTH};
use serde_json::{Map, Value, json};

use super::kiwi::Kv;
use super::network;

type Obj = Map<String, Value>;

/// Most layers produced (instance expansion included) and deepest nesting followed.
const MAX_NODES: usize = crate::rest::MAX_NODES;
const MAX_DEPTH: usize = 256;
/// Most path commands read from one geometry blob.
const MAX_COMMANDS: usize = 200_000;
/// Id of "no style".
const NO_STYLE: &str = "4294967295:4294967295";

/// [`to_rest`]'s JSON, warnings and vector networks.
pub(super) type Rest = (Value, Vec<String>, Vec<Option<VectorNetwork>>);

/// The REST API file JSON for message `msg`, with the warnings about what was skipped and the
/// vector networks its layers name by `vectorNetworkRef` (for [`crate::rest::import_fig`]).
pub(super) fn to_rest(msg: &Kv, name: &str) -> Result<Rest, crate::FormatError> {
    let mut m = Mapper::new(msg);
    let doc = m.document();
    // Large values are moved in, never passed through `json!`, which serialises its arguments:
    // a deep copy of the whole tree, briefly holding it twice.
    let mut out = json!({"name": name});
    out["document"] = doc;
    if !m.library_contexts.is_empty() {
        out[crate::rest::FIG_LIBRARY_CONTEXTS_FIELD] = Value::Array(std::mem::take(&mut m.library_contexts));
    }
    if !m.styles.is_empty() {
        out["styles"] = Value::Object(std::mem::take(&mut m.styles));
    }
    if !m.components.is_empty() {
        out["components"] = Value::Object(std::mem::take(&mut m.components));
    }
    if !m.sets.is_empty() {
        out["componentSets"] = Value::Object(std::mem::take(&mut m.sets));
    }
    if let Some(vars) = m.variables() {
        out["meta"] = vars;
    }
    let styles = m.style_definitions();
    if !styles.is_empty() {
        out["styleDefinitions"] = Value::Array(styles);
    }
    m.properties.finish(&mut out);
    m.warnings.extend(m.properties.warnings);
    Ok((out, m.warnings, m.networks))
}

/// REST importer warnings reworded for a `.fig` import (`None`: drop it).
pub(super) fn adapt_warning(w: String) -> Option<String> {
    if w.contains("geometry=paths") {
        return None;
    }
    if w.contains("without image data") {
        let count = w.split_whitespace().next().unwrap_or("Some");
        return Some(format!("{count} image fill(s) whose image isn't in the file show a grey placeholder"));
    }
    Some(w.replace("the variables JSON", "the file").replace("the file JSON", "the file"))
}

// ---- small readers ----

fn num(n: &Kv, k: &str) -> Option<f64> {
    n.get(k).and_then(Kv::f64)
}

fn guid(v: &Kv) -> Option<String> {
    Some(format!("{}:{}", num(v, "sessionID")? as u64, num(v, "localID")? as u64))
}

fn normalize_override_segment(path: &mut [String], index: usize, scope: &OverrideScope) {
    let Some(segment) = path.get(index) else { return };
    if scope.current.contains(segment) {
        return;
    }
    if let Some(Some(current)) = scope.aliases.get(segment) {
        path[index] = current.clone();
    }
}

/// A `GUID`-holding reference (`StyleId`, `VariableID`, …): its `guid`.
fn ref_guid(v: &Kv) -> Option<String> {
    v.get("guid").and_then(guid).filter(|g| g != NO_STYLE)
}

/// The style a layer links in `key`, or in the older `legacy` field when `key` names none. An
/// explicit NO_STYLE in `key` is a detach and wins over the legacy field.
fn style_link(v: &View, key: &str, legacy: &str) -> Option<String> {
    match v.get(key).and_then(|r| r.get("guid")).and_then(guid) {
        Some(id) => Some(id).filter(|g| g != NO_STYLE),
        None => v.get(legacy).and_then(guid).filter(|g| g != NO_STYLE),
    }
}

fn color(c: &Kv) -> Value {
    let ch = |k: &str| num(c, k).unwrap_or(0.0);
    json!({"r": ch("r"), "g": ch("g"), "b": ch("b"), "a": num(c, "a").unwrap_or(1.0)})
}

fn vec2(v: &Kv) -> Option<Value> {
    Some(json!({"x": num(v, "x")?, "y": num(v, "y")?}))
}

fn matrix(m: &Kv) -> Option<Affine> {
    let c = |k: &str| num(m, k);
    let a = Affine::new([c("m00")?, c("m10")?, c("m01")?, c("m11")?, c("m02")?, c("m12")?]);
    a.as_coeffs().iter().all(|x| x.is_finite()).then_some(a)
}

fn rows(a: Affine) -> Value {
    let [a, b, c, d, e, f] = a.as_coeffs();
    json!([[a, c, e], [b, d, f]])
}

fn copy_num(o: &mut Obj, n: &View, from: &str, to: &str) {
    if let Some(x) = n.num(from) {
        o.insert(to.into(), json!(x));
    }
}

fn copy_str(o: &mut Obj, n: &View, from: &str, to: &str) {
    if let Some(s) = n.str(from) {
        o.insert(to.into(), json!(s));
    }
}

/// A node seen through override layers: the last layer with a field wins.
#[derive(Clone)]
struct View<'a> {
    layers: Vec<&'a Kv<'a>>,
}

impl<'a> View<'a> {
    fn of(n: &'a Kv<'a>) -> Self {
        Self { layers: vec![n] }
    }

    fn with(&self, top: impl IntoIterator<Item = &'a Kv<'a>>) -> Self {
        let mut v = self.clone();
        v.layers.extend(top);
        v
    }

    /// This view with `style` just above the node itself: the style beats the node's own copy of
    /// its fields (stale when the style changed since), and override rows beat both.
    fn styled(&self, style: Option<&'a Kv<'a>>) -> Self {
        let mut v = self.clone();
        if let Some(s) = style {
            v.layers.insert(v.layers.len().min(1), s);
        }
        v
    }

    fn get(&self, k: &str) -> Option<&'a Kv<'a>> {
        self.layers.iter().rev().find_map(|l| l.get(k))
    }

    fn num(&self, k: &str) -> Option<f64> {
        self.get(k).and_then(Kv::f64)
    }

    fn str(&self, k: &str) -> Option<&'a str> {
        self.get(k).and_then(Kv::str)
    }

    fn flag(&self, k: &str) -> Option<bool> {
        self.get(k).and_then(Kv::bool)
    }

    fn list(&self, k: &str) -> &'a [Kv<'a>] {
        self.get(k).map(Kv::list).unwrap_or_default()
    }
}

/// Where an instance's expanded layers are: the overrides and derived layout of the outermost
/// instance (by id path), the path of the instance being expanded, and the property values.
#[derive(Clone)]
struct Inst<'a> {
    root: String,
    path: Vec<String>,
    overrides: Rc<BTreeMap<Vec<String>, Vec<OverrideRow<'a>>>>,
    props: Rc<BTreeMap<String, &'a Kv<'a>>>,
    /// Mains being expanded (an instance inside its own main stops there).
    symbols: Vec<String>,
}

#[derive(Clone)]
struct OverrideRow<'a> {
    row: &'a Kv<'a>,
    source_depth: usize,
    ordinal: usize,
}

struct OverrideScope {
    current: BTreeSet<String>,
    aliases: BTreeMap<String, Option<String>>,
}

struct Mapper<'a> {
    nodes: BTreeMap<String, &'a Kv<'a>>,
    kids: BTreeMap<String, Vec<(&'a str, String)>>,
    blobs: Vec<&'a [u8]>,
    /// Component property definitions: id → (REST name "Name#id", type).
    prop_defs: BTreeMap<String, (String, &'a str)>,
    properties: property_capture::Capture<'a>,
    styles: Obj,
    /// Descriptions of components and component sets (REST `components` / `componentSets`).
    components: Obj,
    sets: Obj,
    /// Component-bearing branches from private .fig internal canvases.
    library_contexts: Vec<Value>,
    library_scan_count: usize,
    library_scan_limited: bool,
    override_scopes: BTreeMap<String, Rc<OverrideScope>>,
    override_scope_entries: usize,
    warnings: Vec<String>,
    count: usize,
    /// Vector networks, each built once: the layers that show one (every instance of a main
    /// holding it) name it by `vectorNetworkRef`. Kept out of the JSON, where a large file's
    /// networks took gigabytes.
    networks: Vec<Option<VectorNetwork>>,
    /// Index in `networks` by what a network is made from: its blob, its scale and its style table.
    network_ids: HashMap<(usize, u64, u64, usize), usize>,
}

impl<'a> Mapper<'a> {
    fn new(msg: &'a Kv<'a>) -> Self {
        let mut nodes = BTreeMap::new();
        let mut kids: BTreeMap<String, Vec<(&'a str, String)>> = BTreeMap::new();
        let mut prop_defs = BTreeMap::new();
        let changes = msg.get("nodeChanges").map(Kv::list).unwrap_or_default();
        // The last change of a GUID wins; a REMOVED change deletes what came before it.
        for n in changes {
            let Some(id) = n.get("guid").and_then(guid) else { continue };
            if n.get("phase").and_then(Kv::str) == Some("REMOVED") {
                nodes.remove(&id);
            } else {
                nodes.insert(id, n);
            }
        }
        for n in changes {
            let Some(id) = n.get("guid").and_then(guid) else { continue };
            if !nodes.get(&id).is_some_and(|latest| std::ptr::eq(*latest, n)) {
                continue;
            }
            if let Some(p) = n.get("parentIndex")
                && let Some(parent) = p.get("guid").and_then(guid)
            {
                kids.entry(parent).or_default().push((p.get("position").and_then(Kv::str).unwrap_or(""), id.clone()));
            }
            for d in n.get("componentPropDefs").map(Kv::list).unwrap_or_default() {
                if let (Some(did), Some(name), Some(ty)) =
                    (d.get("id").and_then(guid), d.get("name").and_then(Kv::str), d.get("type").and_then(Kv::str))
                {
                    prop_defs.insert(did.clone(), (format!("{name}#{did}"), ty));
                }
            }
        }
        for list in kids.values_mut() {
            list.sort();
            list.dedup_by(|a, b| a.1 == b.1);
        }
        let blobs =
            msg.get("blobs").map(Kv::list).unwrap_or_default().iter().map(|b| b.get("bytes").and_then(Kv::bytes).unwrap_or_default()).collect();
        Self {
            nodes,
            kids,
            blobs,
            prop_defs,
            properties: property_capture::Capture::new(msg),
            styles: Obj::new(),
            components: Obj::new(),
            sets: Obj::new(),
            library_contexts: Vec::new(),
            library_scan_count: 0,
            library_scan_limited: false,
            override_scopes: BTreeMap::new(),
            override_scope_entries: 0,
            warnings: Vec::new(),
            count: 0,
            networks: Vec::new(),
            network_ids: HashMap::new(),
        }
    }

    fn warn(&mut self, w: impl Into<String>) {
        let w = w.into();
        if !self.warnings.contains(&w) {
            self.warnings.push(w);
        }
    }

    fn children(&self, id: &str) -> Vec<String> {
        self.kids.get(id).map(|k| k.iter().map(|(_, c)| c.clone()).collect()).unwrap_or_default()
    }

    fn override_scope(&mut self, main: &str) -> Rc<OverrideScope> {
        if let Some(scope) = self.override_scopes.get(main) {
            return Rc::clone(scope);
        }
        let mut current = BTreeSet::new();
        let mut aliases: BTreeMap<String, Option<String>> = BTreeMap::new();
        let mut visited = BTreeSet::new();
        let roots = self.kids.get(main).map_or(&[][..], Vec::as_slice);
        let mut pending: Vec<(String, usize)> = Vec::new();
        let mut complete = roots.len() <= MAX_NODES;
        if complete {
            pending.extend(roots.iter().map(|(_, id)| (id.clone(), 1)));
        }
        while let Some((id, depth)) = pending.pop() {
            if depth > MAX_DEPTH {
                complete = false;
                break;
            }
            if !visited.insert(id.clone()) {
                continue;
            }
            if visited.len() > MAX_NODES {
                complete = false;
                break;
            }
            let Some(node) = self.nodes.get(&id) else { continue };
            current.insert(id.clone());
            if let Some(alias) = node.get("overrideKey").and_then(guid) {
                match aliases.entry(alias) {
                    std::collections::btree_map::Entry::Vacant(entry) => {
                        entry.insert(Some(id.clone()));
                    }
                    std::collections::btree_map::Entry::Occupied(mut entry) => {
                        if entry.get().as_deref().is_some_and(|target| target != id) {
                            entry.insert(None);
                        }
                    }
                }
            }
            if node.get("type").and_then(Kv::str) == Some("INSTANCE") {
                continue;
            }
            let children = self.kids.get(&id).map_or(&[][..], Vec::as_slice);
            if pending.len().saturating_add(visited.len()).saturating_add(children.len()) > MAX_NODES {
                complete = false;
                break;
            }
            pending.extend(children.iter().map(|(_, child)| (child.clone(), depth + 1)));
        }
        if !complete {
            aliases.clear();
        }
        let scope = Rc::new(OverrideScope { current, aliases });
        let cost = scope.current.len().saturating_add(scope.aliases.len());
        if cost <= MAX_NODES.saturating_sub(self.override_scope_entries) {
            self.override_scope_entries += cost;
            self.override_scopes.insert(main.to_string(), Rc::clone(&scope));
        }
        scope
    }

    /// The DOCUMENT with its pages.
    fn document(&mut self) -> Value {
        let doc_id = self.nodes.iter().find(|(_, n)| n.get("type").and_then(Kv::str) == Some("DOCUMENT")).map(|(id, _)| id.clone());
        let pages: Vec<String> = match &doc_id {
            Some(d) => self.children(d),
            None => self.nodes.iter().filter(|(_, n)| n.get("type").and_then(Kv::str) == Some("CANVAS")).map(|(id, _)| id.clone()).collect(),
        };
        let mut out = Vec::new();
        let mut internal = Vec::new();
        let mut visible_pages = Vec::new();
        for p in pages {
            let Some(&n) = self.nodes.get(&p) else { continue };
            if n.get("type").and_then(Kv::str) != Some("CANVAS") {
                self.warn("document children other than pages were skipped");
                continue;
            }
            // The hidden page that holds styles, variables and library copies.
            if n.get("internalOnly").and_then(Kv::bool) == Some(true) {
                internal.push((p, n));
                continue;
            }
            visible_pages.push(p.clone());
            out.push(self.page(&p, n));
        }
        let required_mains = self.referenced_component_mains(&visible_pages);
        for (id, canvas) in internal {
            if let Some(context) = self.library_context(&id, canvas, &required_mains) {
                self.library_contexts.push(context);
            }
        }
        if out.is_empty() {
            self.warn("the file has no pages");
        }
        let mut doc = json!({"id": doc_id.unwrap_or_else(|| "0:0".into()), "type": "DOCUMENT"});
        doc["children"] = Value::Array(out);
        doc
    }

    /// Keep only component definitions and the containers needed to preserve their source paths.
    fn referenced_component_mains(&mut self, visible_pages: &[String]) -> BTreeSet<String> {
        let mut mains = BTreeSet::new();
        let mut pending = Vec::new();
        for page in visible_pages {
            for id in self.children(page) {
                if pending.len() >= MAX_NODES {
                    break;
                }
                pending.push((id, 1));
            }
            if pending.len() >= MAX_NODES {
                break;
            }
        }
        if pending.len() >= MAX_NODES {
            self.warn(format!("visible component reference scan reached the {MAX_NODES}-layer limit"));
        }
        let mut visited = BTreeSet::new();
        let mut reference_limited = false;
        while let Some((id, depth)) = pending.pop() {
            if depth > MAX_DEPTH {
                self.warn(format!("component references below {MAX_DEPTH} visible-layer levels were skipped"));
                continue;
            }
            if !visited.insert(id.clone()) {
                continue;
            }
            if visited.len() > MAX_NODES {
                self.warn(format!("visible component reference scan reached the {MAX_NODES}-layer limit"));
                break;
            }
            let Some(node) = self.nodes.get(&id) else { continue };
            let targets = self.component_main_targets(node);
            self.add_component_targets(&mut mains, targets, &mut reference_limited);
            let children = self.children(&id);
            if pending.len().saturating_add(visited.len()).saturating_add(children.len()) > MAX_NODES {
                if !reference_limited {
                    self.warn(format!("visible component reference scan reached the {MAX_NODES}-layer limit"));
                    reference_limited = true;
                }
                break;
            }
            pending.extend(children.into_iter().map(|child| (child, depth + 1)));
        }

        // Resolve nested mains only from definitions already reachable from visible instances.
        let mut scanned_mains = BTreeSet::new();
        let mut scanned_layers = visited;
        let mut pending_mains: Vec<String> = mains.iter().cloned().collect();
        'dependencies: while let Some(main) = pending_mains.pop() {
            if !scanned_mains.insert(main.clone()) {
                continue;
            }
            if scanned_mains.len() > MAX_NODES {
                self.warn(format!("component dependency scan reached the {MAX_NODES}-main limit"));
                break;
            }
            let mut layers = vec![(main.clone(), 0usize)];
            let mut layer_seen = BTreeSet::new();
            while let Some((id, depth)) = layers.pop() {
                if depth > MAX_DEPTH {
                    self.warn(format!("component dependencies deeper than {MAX_DEPTH} layers were skipped"));
                    continue;
                }
                if !layer_seen.insert(id.clone()) {
                    continue;
                }
                if !scanned_layers.insert(id.clone()) {
                    continue;
                }
                if scanned_layers.len() > MAX_NODES {
                    self.warn(format!("component dependency traversal reached the {MAX_NODES}-layer limit"));
                    break 'dependencies;
                }
                let Some(node) = self.nodes.get(&id) else { continue };
                let targets = self.component_main_targets(node);
                let children = self.children(&id);
                let dependencies = self.add_component_targets(&mut mains, targets, &mut reference_limited);
                pending_mains.extend(dependencies);
                if layers.len().saturating_add(scanned_layers.len()).saturating_add(children.len()) > MAX_NODES {
                    self.warn(format!("component dependency traversal reached the {MAX_NODES}-layer limit"));
                    break 'dependencies;
                }
                layers.extend(children.into_iter().map(|child| (child, depth + 1)));
            }
        }
        mains
    }

    fn add_component_targets(&mut self, mains: &mut BTreeSet<String>, targets: Vec<String>, limited: &mut bool) -> Vec<String> {
        let mut added = Vec::new();
        for target in targets {
            if mains.contains(&target) {
                continue;
            }
            if mains.len() >= MAX_NODES {
                if !*limited {
                    self.warn(format!("component reference scan reached the {MAX_NODES}-main limit"));
                    *limited = true;
                }
                break;
            }
            mains.insert(target.clone());
            added.push(target);
        }
        added
    }

    fn instance_main_targets(&self, node: &Kv<'a>) -> Vec<String> {
        if node.get("type").and_then(Kv::str) != Some("INSTANCE") {
            return Vec::new();
        }
        let mut targets = Vec::new();
        if let Some(main) = node.get("overriddenSymbolID").and_then(guid) {
            targets.push(main);
        }
        if let Some(main) = node.get("symbolData").and_then(|data| data.get("symbolID")).and_then(guid) {
            targets.push(main);
        }
        targets.extend(self.instance_swap_targets(node.get("componentPropAssignments").map(Kv::list).unwrap_or_default()));
        let mut pending: Vec<(&Kv<'a>, usize)> = [
            node.get("symbolData").and_then(|data| data.get("symbolOverrides")).map(Kv::list).unwrap_or_default(),
            node.get("derivedSymbolData").map(Kv::list).unwrap_or_default(),
        ]
        .into_iter()
        .flat_map(|rows| rows.iter().map(|row| (row, 1)))
        .take(MAX_NODES)
        .collect();
        let mut visited = 0usize;
        while let Some((row, depth)) = pending.pop() {
            if depth > MAX_DEPTH || visited >= MAX_NODES {
                break;
            }
            visited += 1;
            if let Some(main) = row.get("overriddenSymbolID").and_then(guid) {
                targets.push(main);
            }
            if let Some(main) = row.get("symbolData").and_then(|data| data.get("symbolID")).and_then(guid) {
                targets.push(main);
            }
            targets.extend(self.instance_swap_targets(row.get("componentPropAssignments").map(Kv::list).unwrap_or_default()));
            let nested: Vec<_> = [
                row.get("symbolData").and_then(|data| data.get("symbolOverrides")).map(Kv::list).unwrap_or_default(),
                row.get("derivedSymbolData").map(Kv::list).unwrap_or_default(),
            ]
            .into_iter()
            .flat_map(|rows| rows.iter().map(|row| (row, depth + 1)))
            .take(MAX_NODES.saturating_sub(pending.len()))
            .collect();
            pending.extend(nested);
        }
        targets
    }

    fn component_main_targets(&self, node: &Kv<'a>) -> Vec<String> {
        let mut targets = self.instance_main_targets(node);
        targets.extend(node.get("componentPropDefs").map(Kv::list).unwrap_or_default().iter().filter_map(|definition| {
            let (_, "INSTANCE_SWAP") = definition.get("id").and_then(guid).and_then(|def| self.prop_defs.get(&def).cloned())? else {
                return None;
            };
            definition.get("initialValue").and_then(|value| value.get("guidValue")).and_then(guid)
        }));
        targets
    }

    fn instance_swap_targets(&self, assignments: &[Kv<'a>]) -> Vec<String> {
        assignments
            .iter()
            .filter_map(|assignment| {
                let (_, "INSTANCE_SWAP") = assignment.get("defID").and_then(guid).and_then(|def| self.prop_defs.get(&def).cloned())? else {
                    return None;
                };
                assignment.get("value").and_then(|value| value.get("guidValue")).and_then(guid)
            })
            .collect()
    }

    fn library_context(&mut self, id: &str, canvas: &'a Kv<'a>, required_mains: &BTreeSet<String>) -> Option<Value> {
        if self.library_scan_count >= MAX_NODES {
            if !self.library_scan_limited {
                self.warn(format!("internal library traversal reached the {MAX_NODES}-layer limit; remaining branches were skipped"));
                self.library_scan_limited = true;
            }
            return None;
        }
        let mut included = BTreeSet::new();
        let mut parents = BTreeMap::from([(id.to_string(), None::<String>)]);
        let mut pending: Vec<(String, usize, bool)> = self.children(id).into_iter().map(|child| (child, 1, false)).collect();
        let mut visited = BTreeSet::new();
        while let Some((node_id, depth, in_definition)) = pending.pop() {
            if depth > MAX_DEPTH {
                self.warn(format!("internal library branches deeper than {MAX_DEPTH} levels were skipped"));
                continue;
            }
            if !visited.insert(node_id.clone()) {
                let mut ancestry = BTreeSet::new();
                let mut ancestor = Some(node_id.clone());
                while let Some(parent) = ancestor {
                    if !ancestry.insert(parent.clone()) {
                        self.warn("cyclic parent links in internal library branches were truncated");
                        break;
                    }
                    if ancestry.len() > MAX_DEPTH {
                        break;
                    }
                    ancestor = parents.get(&parent).cloned().flatten();
                }
                continue;
            }
            self.library_scan_count += 1;
            if self.library_scan_count > MAX_NODES {
                if !self.library_scan_limited {
                    self.warn(format!("internal library traversal reached the {MAX_NODES}-layer limit; remaining branches were skipped"));
                    self.library_scan_limited = true;
                }
                break;
            }
            if visited.len() > MAX_NODES {
                self.warn(format!("the file has more than {MAX_NODES} internal library layers; the rest were skipped"));
                break;
            }
            let Some(node) = self.nodes.get(&node_id) else { continue };
            let kind = node.get("type").and_then(Kv::str);
            let definition = (kind == Some("SYMBOL") || (kind == Some("FRAME") && node.get("isStateGroup").and_then(Kv::bool) == Some(true)))
                && required_mains.contains(&node_id);
            let in_definition = in_definition || definition;
            if in_definition {
                included.insert(node_id.clone());
            }
            if definition {
                let mut ancestor = parents.get(&node_id).cloned().flatten();
                let mut ancestor_seen = BTreeSet::new();
                let mut ancestor_depth = 0;
                while let Some(parent) = ancestor {
                    if !ancestor_seen.insert(parent.clone()) {
                        self.warn("cyclic parent links in internal library branches were truncated");
                        break;
                    }
                    if ancestor_depth >= MAX_DEPTH {
                        self.warn(format!("internal library ancestry deeper than {MAX_DEPTH} levels was truncated"));
                        break;
                    }
                    ancestor_depth += 1;
                    included.insert(parent.clone());
                    ancestor = parents.get(&parent).cloned().flatten();
                }
            }
            for child in self.children(&node_id).into_iter().rev() {
                parents.insert(child.clone(), Some(node_id.clone()));
                pending.push((child, depth + 1, in_definition));
            }
        }
        if included.is_empty() {
            return None;
        }

        let mut context = Obj::new();
        context.insert("id".into(), json!(id));
        context.insert("type".into(), json!("FRAME"));
        copy_str(&mut context, &View::of(canvas), "name", "name");
        context.insert("size".into(), json!({"x": 0.0, "y": 0.0}));
        context.insert("relativeTransform".into(), rows(Affine::IDENTITY));
        context.insert(crate::rest::FIG_SOURCE_FIELD.into(), json!({"origin": id, "expanded": false, "wireFrame": true, "wireInstance": false}));
        self.variable_bindings(&mut context, &View::of(canvas));
        let mut children = Vec::new();
        for child in self.children(id) {
            if let Some(mapped) = self.layer_filtered(&child, None, Affine::IDENTITY, None, 0, Some(&included)) {
                children.push(mapped);
            }
        }
        if !children.is_empty() {
            context.insert("children".into(), Value::Array(children));
        }
        Some(Value::Object(context))
    }

    fn page(&mut self, id: &str, n: &'a Kv<'a>) -> Value {
        let v = View::of(n);
        let mut o = Obj::new();
        o.insert("id".into(), json!(id));
        o.insert("type".into(), json!("CANVAS"));
        self.properties.packet(&mut o, &v);
        copy_str(&mut o, &v, "name", "name");
        if let Some(c) = v.get("backgroundColor") {
            o.insert("backgroundColor".into(), color(c));
        }
        if let Some(c) = v.get("prototypeBackgroundColor") {
            o.insert("prototypeBackgrounds".into(), json!([color(c)]));
        }
        self.variable_bindings(&mut o, &v);
        let mut flows: Vec<(&str, Value)> = Vec::new();
        let mut kids = Vec::new();
        for c in self.children(id) {
            if let Some(sp) = self.nodes.get(&c).and_then(|k| k.get("prototypeStartingPoint")) {
                let name = sp.get("name").and_then(Kv::str).filter(|s| !s.is_empty()).unwrap_or("Flow");
                flows.push((sp.get("position").and_then(Kv::str).unwrap_or(""), json!({"nodeId": c, "name": name})));
            }
            kids.extend(self.layer(&c, None, Affine::IDENTITY, None, 0));
        }
        flows.sort_by(|a, b| a.0.cmp(b.0));
        if !flows.is_empty() {
            o.insert("flowStartingPoints".into(), Value::Array(flows.into_iter().map(|f| f.1).collect()));
        }
        o.insert("children".into(), Value::Array(kids));
        Value::Object(o)
    }

    /// Layer `id` (inside instance `inst` when expanding one) and its subtree. `space`: from the
    /// parent's coordinates to the enclosing frame's (groups and booleans place their children in
    /// the frame's space, like the REST JSON). `parent`: the parent's view (grid tracks).
    fn layer(&mut self, id: &str, inst: Option<&Inst<'a>>, space: Affine, parent: Option<&View<'a>>, depth: usize) -> Option<Value> {
        self.layer_filtered(id, inst, space, parent, depth, None)
    }

    fn layer_filtered(
        &mut self,
        id: &str,
        inst: Option<&Inst<'a>>,
        space: Affine,
        parent: Option<&View<'a>>,
        depth: usize,
        included: Option<&BTreeSet<String>>,
    ) -> Option<Value> {
        if included.is_some_and(|ids| !ids.contains(id)) {
            return None;
        }
        if depth > MAX_DEPTH {
            self.warn(format!("layers nested deeper than {MAX_DEPTH} levels were skipped"));
            return None;
        }
        if self.count >= MAX_NODES {
            self.warn(format!("the file has more than {MAX_NODES} layers; the rest were skipped"));
            return None;
        }
        let base = *self.nodes.get(id)?;
        let path = inst.map(|i| [i.path.as_slice(), &[id.to_string()]].concat());
        let view = match (inst, &path) {
            (Some(i), Some(p)) => View::of(base).with(i.overrides.get(p).into_iter().flatten().map(|row| row.row)),
            _ => View::of(base),
        };
        let rest_id = match (inst, &path) {
            (Some(i), Some(p)) => format!("I{};{}", i.root, p.join(";")),
            _ => id.to_string(),
        };
        let own = view.str("type").unwrap_or("");
        let stand = stand_in(own);
        let ty = match own {
            "ROUNDED_RECTANGLE" => "RECTANGLE",
            "SYMBOL" => "COMPONENT",
            "FRAME" if view.flag("isStateGroup") == Some(true) => "COMPONENT_SET",
            "FRAME" if view.flag("resizeToFit") == Some(true) => "GROUP",
            "VARIABLE" | "VARIABLE_SET" => return None,
            t => stand.map_or(t, |s| s.0),
        };
        if let Some((_, what)) = stand {
            self.warn(format!("{} layers import as {what}", own.to_ascii_lowercase().replace('_', " ")));
        }
        self.count += 1;
        let mut o = Obj::new();
        o.insert("id".into(), json!(rest_id));
        // Private provenance for the .fig importer; ordinary REST import ignores it.
        o.insert(
            crate::rest::FIG_SOURCE_FIELD.into(),
            json!({"origin": id, "expanded": inst.is_some(), "wireFrame": own == "FRAME", "wireInstance": own == "INSTANCE"}),
        );
        o.insert("type".into(), json!(ty));
        self.properties.packet(&mut o, &view);
        copy_str(&mut o, &view, "name", "name");
        self.common(&mut o, &view, ty, space, parent);
        match ty {
            "TEXT" => self.text(&mut o, &view),
            "VECTOR" | "STAR" | "REGULAR_POLYGON" => self.geometry(&mut o, &view),
            "ELLIPSE" => {
                if let Some(a) = view.get("arcData") {
                    let arc: Obj =
                        ["startingAngle", "endingAngle", "innerRadius"].iter().filter_map(|k| Some((k.to_string(), json!(num(a, k)?)))).collect();
                    o.insert("arcData".into(), Value::Object(arc));
                }
            }
            "BOOLEAN_OPERATION" => {
                if let Some(op) = view.str("booleanOperation") {
                    o.insert("booleanOperation".into(), json!(if op == "XOR" { "EXCLUDE" } else { op }));
                }
            }
            _ => {}
        }
        if own == "CONNECTOR" {
            self.connector(&mut o, &view, space);
        }
        if ty == "COMPONENT" || ty == "COMPONENT_SET" {
            self.component(&mut o, &view, &rest_id);
        }
        self.prop_refs(&mut o, &view, inst);
        let child_space = if matches!(ty, "GROUP" | "BOOLEAN_OPERATION") {
            space * view.get("transform").and_then(matrix).unwrap_or_default()
        } else {
            Affine::IDENTITY
        };
        let mut kids = Vec::new();
        if ty == "INSTANCE" {
            if let Some((sub, sym)) = self.instance(&mut o, &view, id, inst, &rest_id) {
                for c in self.children(&sym) {
                    // The source branch can live outside this physical cache context.
                    kids.extend(self.layer_filtered(&c, Some(&sub), child_space, Some(&view), depth + 1, None));
                }
            }
        } else {
            for c in self.children(id) {
                kids.extend(self.layer_filtered(&c, inst, child_space, Some(&view), depth + 1, included));
            }
        }
        if stand.is_some_and(|s| s.0 == "FRAME") {
            kids.splice(0..0, self.shape_with_text(&mut o, &view, own, &rest_id));
            kids.extend(self.inner_text(&o, &view, &rest_id, own == "STICKY"));
        }
        if !kids.is_empty() {
            o.insert("children".into(), Value::Array(kids));
        }
        Some(Value::Object(o))
    }

    /// A non-rectangular shape-with-text's shape, as a vector under its frame (which gives up its
    /// paints to it).
    fn shape_with_text(&mut self, o: &mut Obj, v: &View<'a>, own: &str, rest_id: &str) -> Option<Value> {
        let shape = v.str("shapeWithTextType").unwrap_or("SQUARE");
        let outlined = !v.list("fillGeometry").is_empty() || !v.list("strokeGeometry").is_empty();
        if own != "SHAPE_WITH_TEXT" || matches!(shape, "SQUARE" | "ROUNDED_RECTANGLE") || !outlined || self.count >= MAX_NODES {
            return None;
        }
        let mut s = Obj::new();
        s.insert("id".into(), json!(format!("{rest_id};shape")));
        s.insert("type".into(), json!("VECTOR"));
        s.insert("name".into(), json!(shape.to_ascii_lowercase().replace('_', " ")));
        for k in ["size", "strokeWeight", "strokeAlign", "strokeJoin", "strokeDashes"] {
            if let Some(x) = o.get(k) {
                s.insert(k.into(), x.clone());
            }
        }
        for k in ["fills", "strokes"] {
            s.insert(k.into(), o.insert(k.into(), json!([])).unwrap_or_else(|| json!([])));
        }
        s.insert("relativeTransform".into(), rows(Affine::IDENTITY));
        self.geometry(&mut s, v);
        if !s.contains_key("fillGeometry") && !s.contains_key("strokeGeometry") {
            // Damaged outlines: the frame keeps its paints and box.
            for k in ["fills", "strokes"] {
                o.insert(k.into(), s.remove(k).unwrap_or_else(|| json!([])));
            }
            return None;
        }
        self.count += 1;
        Some(Value::Object(s))
    }

    /// The text a stand-in frame holds (a sticky note's, a shape's, a table cell's) as a text
    /// layer filling it, inset a little; centred unless it's a note.
    fn inner_text(&mut self, o: &Obj, v: &View<'a>, rest_id: &str, note: bool) -> Option<Value> {
        let chars = own_text(v)?;
        if self.count >= MAX_NODES {
            return None;
        }
        self.count += 1;
        let size = |k: &str| o.get("size").and_then(|s| s.get(k)).and_then(Value::as_f64).unwrap_or(0.0);
        let (w, h) = (size("x"), size("y"));
        let pad = (w.min(h) / 4.0).clamp(0.0, 16.0);
        let mut t = Obj::new();
        t.insert("id".into(), json!(format!("{rest_id};text")));
        t.insert("type".into(), json!("TEXT"));
        t.insert("name".into(), json!(chars.chars().take(40).collect::<String>()));
        t.insert("size".into(), json!({"x": (w - 2.0 * pad).max(1.0), "y": (h - 2.0 * pad).max(1.0)}));
        t.insert("relativeTransform".into(), rows(Affine::translate((pad, pad))));
        self.text(&mut t, v);
        if let Some(Value::Object(st)) = t.get_mut("style") {
            let (hor, ver) = if note { ("LEFT", "TOP") } else { ("CENTER", "CENTER") };
            st.entry("textAlignHorizontal").or_insert_with(|| json!(hor));
            st.entry("textAlignVertical").or_insert_with(|| json!(ver));
            st.insert("textAutoResize".into(), json!("NONE"));
        }
        Some(Value::Object(t))
    }

    /// A connector's end caps, and when it has no saved outline or network a straight line
    /// between its ends: free points, or a point (the centre unless given) of a sibling layer.
    fn connector(&mut self, o: &mut Obj, v: &View<'a>, space: Affine) {
        for (from, to) in [("connectorStartCap", "strokeCapStart"), ("connectorEndCap", "strokeCapEnd")] {
            if let Some(c) = v.str(from) {
                o.insert(to.into(), json!(cap_name(c)));
            }
        }
        if own_text(v).is_some() {
            self.warn("connector labels were skipped");
        }
        if ["vectorNetwork", "vectorNetworkRef", "fillGeometry", "strokeGeometry"].iter().any(|k| o.contains_key(*k)) {
            return;
        }
        let parent = v.get("parentIndex").and_then(|p| p.get("guid")).and_then(guid);
        let end = |k: &str| -> Option<Point> {
            let e = v.get(k)?;
            let p = match e.get("endpointNodeID").and_then(guid).filter(|g| g != NO_STYLE) {
                Some(id) => {
                    let n = *self.nodes.get(&id)?;
                    // Only siblings share the connector's coordinates.
                    if n.get("parentIndex").and_then(|p| p.get("guid")).and_then(guid) != parent {
                        return None;
                    }
                    let (w, h) = n.get("size").map_or((0.0, 0.0), |s| (num(s, "x").unwrap_or(0.0), num(s, "y").unwrap_or(0.0)));
                    let (rx, ry) = e.get("relativePosition").and_then(|r| Some((num(r, "x")?, num(r, "y")?))).unwrap_or((0.5, 0.5));
                    n.get("transform").and_then(matrix).unwrap_or_default() * Point::new(w * rx, h * ry)
                }
                None => e.get("position").and_then(|p| Some(Point::new(num(p, "x")?, num(p, "y")?)))?,
            };
            (p.x.is_finite() && p.y.is_finite()).then_some(p)
        };
        let (Some(a), Some(b)) = (end("connectorStart"), end("connectorEnd")) else {
            self.warn("connectors whose ends can't be placed are empty");
            return;
        };
        if v.str("connectorLineStyle").is_some_and(|s| s != "STRAIGHT") {
            self.warn("elbowed and curved connectors without a saved path import as straight lines");
        }
        let (x0, y0) = (a.x.min(b.x), a.y.min(b.y));
        o.insert("relativeTransform".into(), rows(space * Affine::translate((x0, y0))));
        o.insert("size".into(), json!({"x": (a.x - b.x).abs(), "y": (a.y - b.y).abs()}));
        o.insert(
            "vectorNetwork".into(),
            json!({"vertices": [{"x": a.x - x0, "y": a.y - y0}, {"x": b.x - x0, "y": b.y - y0}], "segments": [{"a": 0, "b": 1}]}),
        );
    }

    /// Fields every layer type shares.
    fn common(&mut self, o: &mut Obj, v: &View<'a>, ty: &str, space: Affine, parent: Option<&View<'a>>) {
        for (from, to) in [("visible", "visible"), ("locked", "locked"), ("mask", "isMask")] {
            if let Some(b) = v.flag(from) {
                o.insert(to.into(), json!(b));
            }
        }
        if let Some(m) = v.str("maskType") {
            o.insert("maskType".into(), json!(if m == "OUTLINE" { "VECTOR" } else { m }));
        }
        copy_num(o, v, "opacity", "opacity");
        if let Some(b) = v.str("blendMode").filter(|b| *b != "UNSET") {
            o.insert("blendMode".into(), json!(b));
        }
        if let Some(s) = v.get("size").and_then(vec2) {
            o.insert("size".into(), s);
        }
        let t = v.get("transform").and_then(matrix).unwrap_or_default();
        o.insert("relativeTransform".into(), rows(space * t));
        // Paints, from the shared style when one is linked (the layer's own copy can be stale).
        let mut styles = Obj::new();
        let fill_style = self.style(v, "styleIdForFill", "inheritFillStyleID", "fill", &mut styles);
        let fills = fill_style.map_or_else(|| v.list("fillPaints"), |s| s.get("fillPaints").map(Kv::list).unwrap_or_default());
        o.insert("fills".into(), Value::Array(fills.iter().filter_map(paint).collect()));
        let stroke_style = self.style(v, "styleIdForStrokeFill", "inheritFillStyleIDForStroke", "stroke", &mut styles);
        let strokes = stroke_style.map_or_else(|| v.list("strokePaints"), |s| s.get("fillPaints").map(Kv::list).unwrap_or_default());
        o.insert("strokes".into(), Value::Array(strokes.iter().filter_map(paint).collect()));
        let effect_style = self.style(v, "styleIdForEffect", "inheritEffectStyleID", "effect", &mut styles);
        let effects = effect_style.map_or_else(|| v.list("effects"), |s| s.get("effects").map(Kv::list).unwrap_or_default());
        if !effects.is_empty() {
            o.insert("effects".into(), Value::Array(effects.iter().filter_map(effect).collect()));
        }
        let grid_style = self.style(v, "styleIdForGrid", "inheritGridStyleID", "grid", &mut styles);
        let grids = grid_style.map_or_else(|| v.list("layoutGrids"), |s| s.get("layoutGrids").map(Kv::list).unwrap_or_default());
        if !grids.is_empty() {
            o.insert("layoutGrids".into(), Value::Array(grids.iter().map(layout_grid).collect()));
        }
        if ty == "TEXT" {
            self.style(v, "styleIdForText", "inheritTextStyleID", "text", &mut styles);
        }
        if !styles.is_empty() {
            o.insert("styles".into(), Value::Object(styles));
        }
        stroke(o, v);
        copy_num(o, v, "cornerRadius", "cornerRadius");
        copy_num(o, v, "cornerSmoothing", "cornerSmoothing");
        if v.flag("rectangleCornerRadiiIndependent") == Some(true) {
            let r = |k: &str| v.num(k).unwrap_or(0.0);
            o.insert(
                "rectangleCornerRadii".into(),
                json!([
                    r("rectangleTopLeftCornerRadius"),
                    r("rectangleTopRightCornerRadius"),
                    r("rectangleBottomRightCornerRadius"),
                    r("rectangleBottomLeftCornerRadius")
                ]),
            );
        }
        let constraint = |k: &str, min: &'static str, max: &'static str, both: &'static str| match v.str(k) {
            Some("CENTER") => "CENTER",
            Some("MAX" | "FIXED_MAX") => max,
            Some("STRETCH") => both,
            Some("SCALE") => "SCALE",
            _ => min,
        };
        o.insert(
            "constraints".into(),
            json!({
                "horizontal": constraint("horizontalConstraint", "LEFT", "RIGHT", "LEFT_RIGHT"),
                "vertical": constraint("verticalConstraint", "TOP", "BOTTOM", "TOP_BOTTOM"),
            }),
        );
        if matches!(ty, "FRAME" | "COMPONENT" | "COMPONENT_SET" | "INSTANCE" | "SECTION") {
            o.insert("clipsContent".into(), json!(!v.flag("frameMaskDisabled").unwrap_or(false)));
            self.auto_layout(o, v);
        }
        child_layout(o, v, parent);
        let exports: Vec<Value> = v.list("exportSettings").iter().map(export_setting).collect();
        if !exports.is_empty() {
            o.insert("exportSettings".into(), Value::Array(exports));
        }
        if let Some(b) = v.flag("sectionContentsHidden") {
            o.insert("sectionContentsHidden".into(), json!(b));
        }
        self.prototype(o, v);
        self.variable_bindings(o, v);
    }

    /// The style node a layer links in `key` (or the older `legacy` field), recorded for the REST
    /// `styles` map under `slot`.
    fn style(&mut self, v: &View<'a>, key: &str, legacy: &str, slot: &str, out: &mut Obj) -> Option<&'a Kv<'a>> {
        let id = style_link(v, key, legacy)?;
        let node = *self.nodes.get(&id)?;
        node.get("styleType").and_then(Kv::str)?;
        out.insert(slot.into(), json!(id));
        if !self.styles.contains_key(&id) {
            let name = node.get("name").and_then(Kv::str).unwrap_or("Style");
            let desc = node.get("styleDescription").or_else(|| node.get("description")).and_then(Kv::str).unwrap_or("");
            self.styles.insert(id, json!({"name": name, "description": desc}));
        }
        Some(node)
    }

    fn auto_layout(&mut self, o: &mut Obj, v: &View<'a>) {
        let Some(mode) = v.str("stackMode").filter(|m| matches!(*m, "HORIZONTAL" | "VERTICAL" | "GRID")) else { return };
        o.insert("layoutMode".into(), json!(mode));
        copy_num(o, v, "stackSpacing", "itemSpacing");
        copy_num(o, v, "stackCounterSpacing", "counterAxisSpacing");
        if v.str("stackWrap") == Some("WRAP") {
            o.insert("layoutWrap".into(), json!("WRAP"));
        }
        if v.str("stackCounterAlignContent") == Some("SPACE_BETWEEN") {
            o.insert("counterAxisAlignContent".into(), json!("SPACE_BETWEEN"));
        }
        let (h, vert) = (v.num("stackHorizontalPadding").unwrap_or(0.0), v.num("stackVerticalPadding").unwrap_or(0.0));
        o.insert("paddingLeft".into(), json!(h));
        o.insert("paddingTop".into(), json!(vert));
        o.insert("paddingRight".into(), json!(v.num("stackPaddingRight").unwrap_or(h)));
        o.insert("paddingBottom".into(), json!(v.num("stackPaddingBottom").unwrap_or(vert)));
        let main = match v.str("stackPrimaryAlignItems").or_else(|| v.str("stackJustify")) {
            Some("CENTER") => "CENTER",
            Some("MAX") => "MAX",
            Some("SPACE_BETWEEN" | "SPACE_EVENLY" | "SPACE_AROUND" | "SPACE_EVENLY_CSS") => "SPACE_BETWEEN",
            _ => "MIN",
        };
        o.insert("primaryAxisAlignItems".into(), json!(main));
        let cross = match v.str("stackCounterAlignItems").or_else(|| v.str("stackAlign")) {
            Some("CENTER") => "CENTER",
            Some("MAX") => "MAX",
            Some("BASELINE") => "BASELINE",
            _ => "MIN",
        };
        o.insert("counterAxisAlignItems".into(), json!(cross));
        let sizing = |k: &str| if v.str(k).is_some_and(|s| s.starts_with("RESIZE_TO_FIT")) { "AUTO" } else { "FIXED" };
        o.insert("primaryAxisSizingMode".into(), json!(sizing("stackPrimarySizing")));
        o.insert("counterAxisSizingMode".into(), json!(sizing("stackCounterSizing")));
        if let Some(b) = v.flag("bordersTakeSpace") {
            o.insert("strokesIncludedInLayout".into(), json!(b));
        }
        if let Some(b) = v.flag("stackReverseZIndex") {
            o.insert("itemReverseZIndex".into(), json!(b));
        }
        if mode == "GRID" {
            copy_num(o, v, "gridColumnGap", "gridColumnGap");
            copy_num(o, v, "gridRowGap", "gridRowGap");
            o.insert("gridColumnCount".into(), json!(tracks(v, "gridColumns").len()));
            o.insert("gridRowCount".into(), json!(tracks(v, "gridRows").len()));
            let sizes = ["gridColumnsSizing", "gridRowsSizing"]
                .iter()
                .flat_map(|k| v.get(k).and_then(|m| m.get("entries")).map(Kv::list).unwrap_or_default());
            let flexible = sizes.into_iter().all(|e| {
                let t = e.get("trackSize");
                ["minSizing", "maxSizing"]
                    .iter()
                    .all(|s| t.and_then(|t| t.get(s)).and_then(|s| s.get("type")).and_then(Kv::str).is_none_or(|x| x == "FLEX"))
            });
            if !flexible {
                self.warn("grid tracks of fixed or content size import as equal tracks");
            }
        }
    }

    fn text(&mut self, o: &mut Obj, v: &View<'a>) {
        let style_node = style_link(v, "styleIdForText", "inheritTextStyleID").and_then(|s| self.nodes.get(&s).copied());
        // Instance overrides beat the shared style; the style beats the layer's own copy.
        let sv = v.styled(style_node.filter(|s| s.get("styleType").and_then(Kv::str) == Some("TEXT")));
        let td = v.get("textData");
        let chars = td.and_then(|t| t.get("characters")).and_then(Kv::str).unwrap_or("");
        o.insert("characters".into(), json!(chars));
        let mut style = text_style(&sv, None);
        // Decoration can be overridden locally while keeping the shared style's typography.
        if let Some(decoration @ ("NONE" | "UNDERLINE" | "STRIKETHROUGH")) = v.str("textDecoration") {
            style.insert("textDecoration".into(), json!(decoration));
        }
        if let Some(case @ ("ORIGINAL" | "UPPER" | "LOWER" | "TITLE" | "SMALL_CAPS" | "SMALL_CAPS_FORCED")) = v.str("textCase") {
            style.insert("textCase".into(), json!(case));
        }
        for k in ["textAlignHorizontal", "textAlignVertical", "textAutoResize", "textTruncation"] {
            copy_str(&mut style, v, k, k);
        }
        // Paragraph settings belong to the effective text layer, including explicit zero clears.
        for k in ["paragraphSpacing", "paragraphIndent"] {
            copy_num(&mut style, v, k, k);
        }
        if let Some(n) = v.num("maxLines").filter(|n| *n >= 1.0) {
            style.insert("maxLines".into(), json!(n));
        }
        let size = sv.num("fontSize");
        if let Some(b) = o.get_mut("style").and_then(|s| s.get_mut("boundVariables")).map(Value::take) {
            style.insert("boundVariables".into(), b);
        }
        o.insert("style".into(), Value::Object(style));
        let Some(td) = td else { return };
        char_styles(o, td, size);
        let lines: Vec<&str> = td
            .get("lines")
            .map(Kv::list)
            .unwrap_or_default()
            .iter()
            .map(|l| match l.get("lineType").and_then(Kv::str) {
                Some("ORDERED_LIST") => "ORDERED",
                Some("UNORDERED_LIST") => "UNORDERED",
                _ => "NONE",
            })
            .collect();
        if lines.iter().any(|l| *l != "NONE") {
            o.insert("lineTypes".into(), json!(lines));
        }
    }

    /// `fillGeometry` / `strokeGeometry` as SVG path data, and star/polygon parameters.
    fn geometry(&mut self, o: &mut Obj, v: &View<'a>) {
        for key in ["fillGeometry", "strokeGeometry"] {
            let mut out = Vec::new();
            for p in v.list(key) {
                let Some(blob) = num(p, "commandsBlob").and_then(|i| self.blobs.get(i as usize)).copied() else { continue };
                let Some(d) = svg_path(blob) else {
                    self.warn("damaged vector geometry was skipped");
                    continue;
                };
                if d.is_empty() {
                    continue;
                }
                let winding = if p.get("windingRule").and_then(Kv::str) == Some("ODD") { "EVENODD" } else { "NONZERO" };
                out.push(json!({"path": d, "windingRule": winding}));
            }
            if !out.is_empty() {
                o.insert(key.into(), Value::Array(out));
            }
        }
        let vector = o.get("type").and_then(Value::as_str) == Some("VECTOR");
        if vector && !self.network(o, v) && !o.contains_key("fillGeometry") && !o.contains_key("strokeGeometry") && v.str("type") != Some("CONNECTOR")
        {
            self.warn("vectors without geometry are empty");
        }
        if !vector {
            copy_num(o, v, "count", "pointCount");
            copy_num(o, v, "starInnerScale", "innerRadius");
        }
    }

    /// The editable network of `vectorData` as `vectorNetwork` (the REST importer's extension),
    /// scaled to the layer's size, with per-point corner radii, handle mirroring and end caps.
    /// False when there's none or it's damaged (the outlines stand in).
    fn network(&mut self, o: &mut Obj, v: &View<'a>) -> bool {
        let Some(vd) = v.get("vectorData") else { return false };
        let Some(blob_id) = num(vd, "vectorNetworkBlob").map(|i| i as usize) else { return false };
        let Some(blob) = self.blobs.get(blob_id).copied() else { return false };
        let Some((net, styled_segments)) = network::decode_styled(blob) else {
            self.warn("damaged vector networks import as outlines");
            return false;
        };
        if styled_segments > 0 {
            self.warn("stroke styles of single vector network segments were left out");
        }
        // An open path shows its fill, which regions can't hold: the outline keeps the look.
        let fills_shown =
            o.get("fills").and_then(Value::as_array).is_some_and(|f| f.iter().any(|p| p.get("visible").and_then(Value::as_bool) != Some(false)));
        if net.segments.is_empty() || (net.regions.is_empty() && fills_shown && o.contains_key("fillGeometry")) {
            return false;
        }
        let styles: BTreeMap<u64, &Kv> =
            vd.get("styleOverrideTable").map(Kv::list).unwrap_or_default().iter().filter_map(|s| Some((num(s, "styleID")? as u64, s))).collect();
        let style = |id: u32| styles.get(&u64::from(id)).copied().filter(|_| id != 0);
        let scale = |k: &str| {
            let (size, norm) = (v.get("size").and_then(|s| num(s, k)), vd.get("normalizedSize").and_then(|s| num(s, k)));
            size.zip(norm).filter(|(s, n)| s.is_finite() && *n > 1e-9).map_or(1.0, |(s, n)| s / n)
        };
        let (sx, sy) = (scale("x"), scale("y"));
        if net.regions.iter().any(|r| style(r.style).is_some_and(|s| s.get("fillPaints").is_some())) {
            self.warn("filled areas of a vector with their own fill use the layer's fill");
        }
        let join = v.str("strokeJoin");
        if net.vertices.iter().any(|p| style(p.style).and_then(|s| s.get("strokeJoin")).and_then(Kv::str).is_some_and(|j| Some(j) != join)) {
            self.warn("points with their own stroke join use the layer's");
        }
        // The stroke starts at the first end point and stops at the last (as the editor draws it).
        let mut degree = vec![0usize; net.vertices.len()];
        for s in &net.segments {
            for i in [s.a, s.b] {
                if let Some(d) = degree.get_mut(i) {
                    *d += 1;
                }
            }
        }
        let ends: Vec<usize> = (0..degree.len()).filter(|i| degree.get(*i) == Some(&1)).collect();
        let cap =
            |i: Option<&usize>| i.and_then(|i| net.vertices.get(*i)).and_then(|p| style(p.style)).and_then(|s| s.get("strokeCap")).and_then(Kv::str);
        let (first, last) = (cap(ends.first()), cap(ends.last()));
        if first.is_some() || last.is_some() {
            let node = v.str("strokeCap").unwrap_or("NONE");
            o.insert("strokeCapStart".into(), json!(cap_name(first.unwrap_or(node))));
            o.insert("strokeCapEnd".into(), json!(cap_name(last.unwrap_or(node))));
        }
        // Every instance of a main shows the same network: it is written once and shared.
        let key = (blob_id, sx.to_bits(), sy.to_bits(), std::ptr::from_ref(vd) as usize);
        let id = match self.network_ids.get(&key) {
            Some(id) => *id,
            None => {
                let vertices: Vec<Value> = net
                    .vertices
                    .iter()
                    .map(|p| {
                        let mut out = json!({"x": p.x * sx, "y": p.y * sy});
                        let st = style(p.style);
                        if let Some(r) = st.and_then(|s| num(s, "cornerRadius")).filter(|r| *r > 0.0) {
                            out["radius"] = json!(r);
                        }
                        match st.and_then(|s| s.get("handleMirroring")).and_then(Kv::str) {
                            Some("ANGLE") => out["mirror"] = json!("angle"),
                            Some("ANGLE_AND_LENGTH") => out["mirror"] = json!("angleAndLength"),
                            Some("NONE") => out["mirror"] = json!("none"),
                            _ => {}
                        }
                        out
                    })
                    .collect();
                let segments: Vec<Value> = net
                    .segments
                    .iter()
                    .map(|s| json!({"a": s.a, "b": s.b, "ta": [s.ta.0 * sx, s.ta.1 * sy], "tb": [s.tb.0 * sx, s.tb.1 * sy]}))
                    .collect();
                let regions: Vec<Value> =
                    net.regions.iter().map(|r| json!({"loops": r.loops, "winding": if r.non_zero { "nonZero" } else { "evenOdd" }})).collect();
                let mut entry = json!({});
                entry["vertices"] = Value::Array(vertices);
                entry["segments"] = Value::Array(segments);
                entry["regions"] = Value::Array(regions);
                self.networks.push(crate::rest::network(&entry));
                self.network_ids.insert(key, self.networks.len() - 1);
                self.networks.len() - 1
            }
        };
        o.insert("vectorNetworkRef".into(), json!(id));
        true
    }

    /// Component property definitions and description.
    fn component(&mut self, o: &mut Obj, v: &View<'a>, id: &str) {
        let mut defs = Obj::new();
        for d in v.list("componentPropDefs") {
            if d.get("isDeleted").and_then(Kv::bool) == Some(true) {
                continue;
            }
            let Some((name, ty)) = d.get("id").and_then(guid).and_then(|i| self.prop_defs.get(&i).cloned()) else { continue };
            let init = d.get("initialValue");
            let def = match ty {
                "BOOL" => json!({"type": "BOOLEAN", "defaultValue": init.and_then(|i| i.get("boolValue")).and_then(Kv::bool).unwrap_or(true)}),
                "TEXT" => json!({"type": "TEXT", "defaultValue": init.and_then(prop_text).unwrap_or("")}),
                "INSTANCE_SWAP" => match init.and_then(|i| i.get("guidValue")).and_then(guid) {
                    Some(g) => json!({"type": "INSTANCE_SWAP", "defaultValue": g}),
                    None => continue,
                },
                "VARIANT" => json!({"type": "VARIANT"}),
                _ => {
                    self.warn(format!("{} component properties are not supported and were skipped", ty.to_ascii_lowercase()));
                    continue;
                }
            };
            defs.insert(name, def);
        }
        if !defs.is_empty() {
            o.insert("componentPropertyDefinitions".into(), Value::Object(defs));
        }
        if let Some(d) = v.str("symbolDescription").or_else(|| v.str("description")).filter(|d| !d.is_empty()) {
            let into = if o.get("type").and_then(Value::as_str) == Some("COMPONENT_SET") { &mut self.sets } else { &mut self.components };
            into.insert(id.to_string(), json!({"description": d}));
        }
    }

    /// A layer's component property references, and inside an instance the values they take.
    fn prop_refs(&mut self, o: &mut Obj, v: &View<'a>, inst: Option<&Inst<'a>>) {
        let mut refs = Obj::new();
        for e in v.get("parameterConsumptionMap").and_then(|m| m.get("entries")).map(Kv::list).unwrap_or_default() {
            let field = match e.get("variableField").and_then(Kv::str) {
                Some("VISIBLE") => "visible",
                Some("TEXT_DATA") => "characters",
                Some("OVERRIDDEN_SYMBOL_ID") => "mainComponent",
                _ => continue,
            };
            let Some(def) =
                e.get("variableData").and_then(|d| d.get("value")).and_then(|x| x.get("propRefValue")).and_then(|p| p.get("defId")).and_then(guid)
            else {
                continue;
            };
            if let Some((name, _)) = self.prop_defs.get(&def) {
                refs.insert(field.into(), json!(name));
            }
            let Some(val) = inst.and_then(|i| i.props.get(&def)) else { continue };
            match field {
                "visible" => {
                    if let Some(b) = val.get("boolValue").and_then(Kv::bool) {
                        o.insert("visible".into(), json!(b));
                    }
                }
                "characters" => {
                    if let Some(t) = prop_text(val) {
                        o.insert("characters".into(), json!(t));
                        // Run styles count the chars they were made for: for other chars only
                        // the value's own apply.
                        if own_text(v) != Some(t) {
                            o.remove("characterStyleOverrides");
                            o.remove("styleOverrideTable");
                            if let Some(tv) = val.get("textValue") {
                                let size = o.get("style").and_then(|s| s.get("fontSize")).and_then(Value::as_f64);
                                char_styles(o, tv, size);
                            }
                        }
                    }
                }
                _ => {}
            }
        }
        if !refs.is_empty() {
            o.insert("componentPropertyReferences".into(), Value::Object(refs));
        }
    }

    /// An instance's main and properties; the context its expanded layers are built in.
    fn instance(&mut self, o: &mut Obj, v: &View<'a>, id: &str, outer: Option<&Inst<'a>>, rest_id: &str) -> Option<(Inst<'a>, String)> {
        let sd = v.get("symbolData");
        let mut sym = v.get("overriddenSymbolID").and_then(guid).or_else(|| sd.and_then(|s| s.get("symbolID")).and_then(guid));
        // An instance swap property value picks the main.
        if let Some(i) = outer
            && let Some(def) = prop_ref(v, "OVERRIDDEN_SYMBOL_ID")
            && let Some(g) = i.props.get(&def).and_then(|p| p.get("guidValue")).and_then(guid)
        {
            sym = Some(g);
        }
        let Some(sym) = sym else {
            self.warn("instances without a main component were imported as empty frames");
            return None;
        };
        o.insert("componentId".into(), json!(sym));
        let assignments = v.list("componentPropAssignments");
        let mut props = Obj::new();
        for a in assignments {
            let Some((name, ty)) = a.get("defID").and_then(guid).and_then(|d| self.prop_defs.get(&d).cloned()) else { continue };
            let val = a.get("value");
            let p = match ty {
                "BOOL" => val.and_then(|x| x.get("boolValue")).and_then(Kv::bool).map(|b| json!({"type": "BOOLEAN", "value": b})),
                "TEXT" => val.and_then(prop_text).map(|t| json!({"type": "TEXT", "value": t})),
                "INSTANCE_SWAP" => val.and_then(|x| x.get("guidValue")).and_then(guid).map(|g| json!({"type": "INSTANCE_SWAP", "value": g})),
                _ => None,
            };
            if let Some(p) = p {
                props.insert(name, p);
            }
        }
        if !props.is_empty() {
            o.insert("componentProperties".into(), Value::Object(props));
        }
        if !self.nodes.contains_key(&sym) {
            self.warn("instances of components that aren't in the file show no layers");
            return None;
        }
        if outer.is_some_and(|i| i.symbols.contains(&sym)) {
            return None;
        }
        // Overrides by id path below this instance (the instance itself is addressed by its main's id).
        let own_path: Vec<String> = outer.map(|i| [i.path.as_slice(), &[id.to_string()]].concat()).unwrap_or_default();
        let scope = self.override_scope(&sym);
        let mut overrides: BTreeMap<Vec<String>, Vec<OverrideRow<'a>>> = BTreeMap::new();
        if let Some(inherited) = outer {
            for (path, rows) in inherited.overrides.iter() {
                let mut path = path.clone();
                if path.starts_with(&own_path) && path.len() > own_path.len() {
                    normalize_override_segment(&mut path, own_path.len(), &scope);
                }
                overrides.entry(path).or_default().extend(rows.iter().cloned());
            }
        }
        let mut local: BTreeMap<Vec<String>, Vec<OverrideRow<'a>>> = BTreeMap::new();
        let lists = [sd.and_then(|s| s.get("symbolOverrides")).map(Kv::list).unwrap_or_default(), v.list("derivedSymbolData")];
        let source_depth = outer.map_or(0, |i| i.symbols.len());
        let mut ordinal = 0;
        for list in lists {
            for ov in list {
                let row_ordinal = ordinal;
                ordinal += 1;
                let rel: Vec<String> =
                    ov.get("guidPath").and_then(|g| g.get("guids")).map(Kv::list).unwrap_or_default().iter().filter_map(guid).collect();
                if rel.is_empty() || rel == [sym.clone()] {
                    continue;
                }
                let mut rel = rel;
                normalize_override_segment(&mut rel, 0, &scope);
                local.entry([own_path.as_slice(), &rel].concat()).or_default().push(OverrideRow { row: ov, source_depth, ordinal: row_ordinal });
            }
        }
        for (path, rows) in local {
            overrides.entry(path).or_default().extend(rows);
        }
        for rows in overrides.values_mut() {
            // Deeper source rows apply first; outer rows win, with wire order preserved per source.
            rows.sort_by_key(|row| (std::cmp::Reverse(row.source_depth), row.ordinal));
        }
        let mut all_props: BTreeMap<String, &'a Kv<'a>> = outer.map(|i| (*i.props).clone()).unwrap_or_default();
        for a in assignments {
            if let (Some(d), Some(val)) = (a.get("defID").and_then(guid), a.get("value")) {
                all_props.entry(d).or_insert(val);
            }
        }
        let mut symbols = outer.map(|i| i.symbols.clone()).unwrap_or_default();
        symbols.push(sym.clone());
        let root = outer.map_or_else(|| rest_id.to_string(), |i| i.root.clone());
        Some((Inst { root, path: own_path, overrides: Rc::new(overrides), props: Rc::new(all_props), symbols }, sym))
    }

    fn prototype(&mut self, o: &mut Obj, v: &View<'a>) {
        let mut list = Vec::new();
        for it in v.list("prototypeInteractions") {
            if it.get("isDeleted").and_then(Kv::bool) == Some(true) {
                continue;
            }
            if let Some(i) = self.interaction(it) {
                list.push(i);
            }
        }
        if !list.is_empty() {
            o.insert("interactions".into(), Value::Array(list));
        }
        let overflow = match v.str("scrollDirection") {
            Some("HORIZONTAL") => Some("HORIZONTAL_SCROLLING"),
            Some("VERTICAL") => Some("VERTICAL_SCROLLING"),
            Some("BOTH") => Some("HORIZONTAL_AND_VERTICAL_SCROLLING"),
            _ => None,
        };
        if let Some(d) = overflow {
            o.insert("overflowDirection".into(), json!(d));
        }
        let behavior = match v.str("scrollBehavior") {
            Some("FIXED_WHEN_CHILD_OF_SCROLLING_FRAME") => Some("FIXED"),
            Some("STICKY_SCROLLS") => Some("STICKY_SCROLLS"),
            _ => None,
        };
        if let Some(b) = behavior {
            o.insert("scrollBehavior".into(), json!(b));
        }
        copy_str(o, v, "overlayPositionType", "overlayPositionType");
        copy_str(o, v, "overlayBackgroundInteraction", "overlayBackgroundInteraction");
        if let Some(a) = v.get("overlayBackgroundAppearance") {
            let ty = a.get("backgroundType").and_then(Kv::str).unwrap_or("NONE");
            o.insert("overlayBackground".into(), json!({"type": ty, "color": a.get("backgroundColor").map_or(Value::Null, color)}));
        }
    }

    fn interaction(&mut self, it: &Kv) -> Option<Value> {
        let ev = it.get("event");
        let ty = match ev.and_then(|e| e.get("interactionType")).and_then(Kv::str).unwrap_or("ON_CLICK") {
            "DRAG" => "ON_DRAG",
            "MOUSE_IN" => "MOUSE_ENTER",
            "MOUSE_OUT" => "MOUSE_LEAVE",
            t => t,
        };
        let mut trigger = json!({"type": ty});
        if let Some(t) = ev.and_then(|e| num(e, "transitionTimeout")) {
            // Seconds as 32-bit floats: whole milliseconds.
            trigger["timeout"] = json!((t * 1000.0).round());
        }
        if let Some(k) = ev.and_then(|e| e.get("keyTrigger")) {
            let codes: Vec<f64> = k.get("keyCodes").map(Kv::list).unwrap_or_default().iter().filter_map(Kv::f64).collect();
            trigger["keyCodes"] = json!(codes);
            if let Some(d) = k.get("triggerDevice").and_then(Kv::str) {
                trigger["device"] = json!(if d.contains("KEYBOARD") { "KEYBOARD" } else { d });
            }
        }
        let actions: Vec<Value> = it.get("actions").map(Kv::list).unwrap_or_default().iter().filter_map(|a| self.action(a, 0)).collect();
        (!actions.is_empty()).then(|| json!({"trigger": trigger, "actions": actions}))
    }

    /// One prototype action in the REST shape (`depth`: conditionals it's nested in).
    fn action(&mut self, a: &Kv, depth: usize) -> Option<Value> {
        Some(match a.get("connectionType").and_then(Kv::str) {
            Some("INTERNAL_NODE") | None => {
                let dest = a.get("transitionNodeID").and_then(guid)?;
                let nav = match a.get("navigationType").and_then(Kv::str) {
                    Some("SWAP_STATE") => "CHANGE_TO",
                    Some(n) => n,
                    None => "NAVIGATE",
                };
                let mut act = json!({"type": "NODE", "destinationId": dest, "navigation": nav});
                if let Some(t) = transition(a) {
                    act["transition"] = t;
                }
                if let Some(b) = a.get("transitionPreserveScroll").and_then(Kv::bool) {
                    act["preserveScrollPosition"] = json!(b);
                }
                if let Some(p) = a.get("overlayRelativePosition") {
                    act["overlayRelativePosition"] = json!({"x": num(p, "x"), "y": num(p, "y")});
                }
                act
            }
            Some("URL") => json!({"type": "URL", "url": a.get("connectionURL").and_then(Kv::str)?}),
            Some("BACK") => json!({"type": "BACK"}),
            Some("CLOSE") => json!({"type": "CLOSE"}),
            Some("NONE") => return None,
            Some("SET_VARIABLE") => {
                let var = a.get("targetVariable").and_then(|t| t.get("id")).and_then(ref_guid).or_else(|| a.get("targetVariableID").and_then(guid));
                // Missing parts are left out: the REST importer skips the action with a warning.
                let mut act = json!({"type": "SET_VARIABLE", "variableId": var});
                if let Some(d) = a.get("targetVariableData").and_then(|d| variable_data(d, 0)) {
                    act["variableValue"] = d;
                }
                act
            }
            Some("SET_VARIABLE_MODE") => json!({
                "type": "SET_VARIABLE_MODE",
                "variableCollectionId": a.get("targetVariableSetID").and_then(ref_guid),
                "variableModeId": a.get("targetVariableModeID").and_then(guid),
            }),
            Some("CONDITIONAL") if depth < MAX_ACTION_DEPTH => {
                let mut blocks = Vec::new();
                for b in a.get("conditionalActions").map(Kv::list).unwrap_or_default() {
                    let actions: Vec<Value> =
                        b.get("actions").map(Kv::list).unwrap_or_default().iter().filter_map(|x| self.action(x, depth + 1)).collect();
                    let mut block = json!({"actions": actions});
                    // A condition that can't be read must not turn the block into an "else".
                    if let Some(c) = b.get("condition") {
                        block["condition"] = variable_data(c, 0).unwrap_or_else(|| json!({}));
                    }
                    blocks.push(block);
                }
                json!({"type": "CONDITIONAL", "conditionalBlocks": blocks})
            }
            Some(other) => {
                self.warn(format!("{} prototype actions are not imported from .fig files yet", other.to_ascii_lowercase().replace('_', " ")));
                return None;
            }
        })
    }

    fn variable_bindings(&mut self, o: &mut Obj, v: &View<'a>) {
        let mut bound = Obj::new();
        let mut text_bound = Obj::new();
        for e in v.get("variableConsumptionMap").and_then(|m| m.get("entries")).map(Kv::list).unwrap_or_default() {
            let Some(var) = alias(e.get("variableData")) else { continue };
            let a = json!({"type": "VARIABLE_ALIAS", "id": var});
            let field = e.get("variableField").and_then(Kv::str).unwrap_or("");
            match (binding_key(field), text_binding_key(field)) {
                (Some(k), _) => {
                    bound.insert(k.into(), a);
                }
                (_, Some(k)) => {
                    text_bound.insert(k.into(), a);
                }
                _ if field == "CORNER_RADIUS" => {
                    for k in ["topLeftRadius", "topRightRadius", "bottomLeftRadius", "bottomRightRadius"] {
                        bound.insert(k.into(), a.clone());
                    }
                }
                _ if field.starts_with("BORDER_") => {
                    let side = match field {
                        "BORDER_TOP_WEIGHT" => "strokeTopWeight",
                        "BORDER_RIGHT_WEIGHT" => "strokeRightWeight",
                        "BORDER_BOTTOM_WEIGHT" => "strokeBottomWeight",
                        _ => "strokeLeftWeight",
                    };
                    bound.insert(side.into(), a);
                }
                _ if field == "WIDTH" || field == "HEIGHT" => {
                    let axis = if field == "WIDTH" { "x" } else { "y" };
                    let size = bound.entry("size").or_insert_with(|| json!({}));
                    size[axis] = a;
                }
                _ => self.warn(format!("variables bound to {} are not imported from .fig files yet", field.to_ascii_lowercase().replace('_', " "))),
            }
        }
        if !bound.is_empty() {
            o.insert("boundVariables".into(), Value::Object(bound));
        }
        // (The text style itself is added after this: it keeps these.)
        if !text_bound.is_empty() {
            o.insert("style".into(), json!({"boundVariables": text_bound}));
        }
        let mut modes = Obj::new();
        for e in v.get("variableModeBySetMap").and_then(|m| m.get("entries")).map(Kv::list).unwrap_or_default() {
            if let (Some(set), Some(mode)) = (e.get("variableSetID").and_then(ref_guid), e.get("variableModeID").and_then(guid)) {
                modes.insert(set, json!(mode));
            }
        }
        if !modes.is_empty() {
            o.insert("explicitVariableModes".into(), Value::Object(modes));
        }
    }

    /// The shared styles the file defines, in their order: each with the sample layer that holds
    /// its values (`styleDefinitions`, our extension of the file JSON; the REST importer reads it).
    fn style_definitions(&mut self) -> Vec<Value> {
        let mut found: Vec<(&'a str, String, &'a Kv<'a>)> = Vec::new();
        for (id, n) in &self.nodes {
            let live = n.get("isSoftDeleted").and_then(Kv::bool) != Some(true) && n.get("isSoftDeletedStyle").and_then(Kv::bool) != Some(true);
            if live && n.get("styleType").and_then(Kv::str).is_some_and(|t| matches!(t, "FILL" | "TEXT" | "EFFECT" | "GRID")) {
                found.push((n.get("sortPosition").and_then(Kv::str).unwrap_or(""), id.clone(), n));
            }
        }
        found.sort_by(|a, b| (a.0, &a.1).cmp(&(b.0, &b.1)));
        let mut out = Vec::new();
        for (_, id, n) in found {
            let Some(node) = self.layer(&id, None, Affine::IDENTITY, None, 0) else { break };
            let name = n.get("name").and_then(Kv::str).unwrap_or("Style");
            let desc = n.get("styleDescription").or_else(|| n.get("description")).and_then(Kv::str).unwrap_or("");
            let ty = n.get("styleType").and_then(Kv::str).unwrap_or("");
            out.push(json!({"id": id, "name": name, "description": desc, "styleType": ty, "node": node}));
        }
        out
    }

    /// Collections and variables, as the local variables response's `meta`, with their ids in the
    /// file's order (`collectionOrder` and `variableOrder`, our extensions).
    fn variables(&self) -> Option<Value> {
        let of_type = |t: &'static str, key: &'static str| {
            let live = move |n: &Kv| n.get("type").and_then(Kv::str) == Some(t) && n.get("isSoftDeleted").and_then(Kv::bool) != Some(true);
            let mut list: Vec<(&str, &String, &'a Kv<'a>)> = self
                .nodes
                .iter()
                .filter(|(_, n)| live(n))
                .map(|(id, n)| (n.get(key).and_then(|p| p.get("position").or(Some(p))).and_then(Kv::str).unwrap_or(""), id, *n))
                .collect();
            list.sort_by(|a, b| (a.0, a.1).cmp(&(b.0, b.1)));
            list.into_iter().map(|(_, id, n)| (id, n))
        };
        let mut order = Vec::new();
        let mut collections = Obj::new();
        for (id, n) in of_type("VARIABLE_SET", "parentIndex") {
            order.push(json!(id));
            let mut modes: Vec<(&str, Value)> = n
                .get("variableSetModes")
                .map(Kv::list)
                .unwrap_or_default()
                .iter()
                .filter_map(|m| {
                    let mode = json!({"modeId": m.get("id").and_then(guid)?, "name": m.get("name").and_then(Kv::str).unwrap_or("Mode")});
                    Some((m.get("sortPosition").and_then(Kv::str).unwrap_or(""), mode))
                })
                .collect();
            modes.sort_by(|a, b| a.0.cmp(b.0));
            let default = modes.first().map_or(Value::Null, |m| m.1["modeId"].clone());
            let name = n.get("name").and_then(Kv::str).unwrap_or("Collection");
            let modes: Vec<Value> = modes.into_iter().map(|m| m.1).collect();
            collections.insert(id.clone(), json!({"id": id, "name": name, "modes": modes, "defaultModeId": default}));
        }
        let mut variable_order = Vec::new();
        let mut variables = Obj::new();
        for (id, n) in of_type("VARIABLE", "sortPosition") {
            let Some(set) = n.get("variableSetID").and_then(ref_guid).filter(|s| collections.contains_key(s)) else { continue };
            let ty = n.get("variableResolvedType").and_then(Kv::str).unwrap_or("");
            if !matches!(ty, "BOOLEAN" | "FLOAT" | "STRING" | "COLOR") {
                continue;
            }
            let mut values = Obj::new();
            for e in n.get("variableDataValues").and_then(|m| m.get("entries")).map(Kv::list).unwrap_or_default() {
                if let (Some(mode), Some(val)) = (e.get("modeID").and_then(guid), e.get("variableData").and_then(variable_value)) {
                    values.insert(mode, val);
                }
            }
            let name = n.get("name").and_then(Kv::str).unwrap_or("Variable");
            let desc = n.get("description").and_then(Kv::str).unwrap_or("");
            let mut var =
                json!({"id": id, "name": name, "variableCollectionId": set, "resolvedType": ty, "valuesByMode": values, "description": desc});
            // The properties pickers offer the variable for, under the REST names.
            if let Some(scopes) = n.get("variableScopes") {
                var["scopes"] = scopes.list().iter().filter_map(Kv::str).map(|s| if s == "STROKE" { "STROKE_COLOR" } else { s }).collect();
            }
            variable_order.push(json!(id));
            variables.insert(id.clone(), var);
        }
        (!collections.is_empty())
            .then(|| json!({"variableCollections": collections, "variables": variables, "collectionOrder": order, "variableOrder": variable_order}))
    }
}

/// The REST `boundVariables` key of a bound layer field.
fn binding_key(field: &str) -> Option<&'static str> {
    Some(match field {
        "STROKE_WEIGHT" => "strokeWeight",
        "STACK_SPACING" => "itemSpacing",
        "STACK_COUNTER_SPACING" => "counterAxisSpacing",
        "STACK_PADDING_LEFT" => "paddingLeft",
        "STACK_PADDING_TOP" => "paddingTop",
        "STACK_PADDING_RIGHT" => "paddingRight",
        "STACK_PADDING_BOTTOM" => "paddingBottom",
        "VISIBLE" => "visible",
        "TEXT_DATA" => "characters",
        "OPACITY" => "opacity",
        "MIN_WIDTH" => "minWidth",
        "MAX_WIDTH" => "maxWidth",
        "MIN_HEIGHT" => "minHeight",
        "MAX_HEIGHT" => "maxHeight",
        "RECTANGLE_TOP_LEFT_CORNER_RADIUS" => "topLeftRadius",
        "RECTANGLE_TOP_RIGHT_CORNER_RADIUS" => "topRightRadius",
        "RECTANGLE_BOTTOM_LEFT_CORNER_RADIUS" => "bottomLeftRadius",
        "RECTANGLE_BOTTOM_RIGHT_CORNER_RADIUS" => "bottomRightRadius",
        _ => return None,
    })
}

/// The REST text style `boundVariables` key of a bound text field.
fn text_binding_key(field: &str) -> Option<&'static str> {
    Some(match field {
        "FONT_SIZE" => "fontSize",
        "FONT_FAMILY" => "fontFamily",
        "FONT_STYLE" => "fontStyle",
        "LETTER_SPACING" => "letterSpacing",
        "LINE_HEIGHT" => "lineHeight",
        "PARAGRAPH_SPACING" => "paragraphSpacing",
        "PARAGRAPH_INDENT" => "paragraphIndent",
        _ => return None,
    })
}

fn stroke(o: &mut Obj, v: &View) {
    copy_num(o, v, "strokeWeight", "strokeWeight");
    if let Some(a) = v.str("strokeAlign").filter(|a| *a != "OFFSET") {
        o.insert("strokeAlign".into(), json!(a));
    }
    copy_str(o, v, "strokeJoin", "strokeJoin");
    if let Some(cap) = v.str("strokeCap") {
        o.insert("strokeCap".into(), json!(cap_name(cap)));
    }
    // A layer written as the outline of its stroke keeps what it was as plugin data
    // (`outlinedStroke`, our extension of the file JSON).
    let (id, key) = super::export::STASH;
    let kept = v.list("pluginData").iter().find(|p| p.get("pluginID").and_then(Kv::str) == Some(id) && p.get("key").and_then(Kv::str) == Some(key));
    if let Some(original) = kept.and_then(|p| p.get("value")).and_then(Kv::str).and_then(|s| serde_json::from_str::<Value>(s).ok()) {
        o.insert("outlinedStroke".into(), original);
    }
    // A variable width: each point's reach on the two sides of the path (shares of the stroke
    // weight), as `[t, scale, bias]` (`strokeWidthProfile`, our extension of the file JSON).
    let profile: Vec<Value> = v
        .list("variableWidthPoints")
        .iter()
        .filter_map(|p| {
            let (left, right) = (num(p, "ascent")?.max(0.0), num(p, "descent")?.max(0.0));
            let bias = if left + right > 0.0 { (right - left) / (left + right) } else { 0.0 };
            Some(json!([num(p, "position")?, left + right, bias]))
        })
        .collect();
    if !profile.is_empty() {
        o.insert("strokeWidthProfile".into(), Value::Array(profile));
    }
    let dashes: Vec<f64> = v.list("dashPattern").iter().filter_map(Kv::f64).collect();
    if !dashes.is_empty() {
        o.insert("strokeDashes".into(), json!(dashes));
    }
    // A miter limit L bevels joins sharper than 2·asin(1/L).
    if let Some(l) = v.num("miterLimit").filter(|l| *l > 1.0) {
        o.insert("strokeMiterAngle".into(), json!((2.0 * (1.0 / l).asin()).to_degrees()));
    }
    if v.flag("borderStrokeWeightsIndependent") == Some(true) {
        // A side that isn't written has no border.
        let side = |k: &str| v.num(k).unwrap_or(0.0);
        o.insert(
            "individualStrokeWeights".into(),
            json!({"top": side("borderTopWeight"), "right": side("borderRightWeight"), "bottom": side("borderBottomWeight"), "left": side("borderLeftWeight")}),
        );
    }
}

/// A `VariableData` (literal, variable reference or expression) in the REST shape; `None` for
/// kinds a prototype can't evaluate.
fn variable_data(d: &Kv, depth: usize) -> Option<Value> {
    let v = d.get("value")?;
    let ty = d.get("dataType").and_then(Kv::str)?;
    let value = match ty {
        "ALIAS" => json!({"type": "VARIABLE_ALIAS", "id": v.get("alias").and_then(ref_guid)?}),
        "EXPRESSION" if depth < MAX_EXPR_DEPTH => {
            let e = v.get("expressionValue")?;
            let function = match e.get("expressionFunction").and_then(Kv::str)? {
                "MULTIPLY" => "MULTIPLICATION",
                "DIVIDE" => "DIVISION",
                f => f,
            };
            let args = e.get("expressionArguments").map(Kv::list).unwrap_or_default();
            let args = args.iter().map(|x| variable_data(x, depth + 1)).collect::<Option<Vec<Value>>>()?;
            json!({"expressionFunction": function, "expressionArguments": args})
        }
        "BOOLEAN" => json!(v.get("boolValue").and_then(Kv::bool)?),
        "FLOAT" => json!(num(v, "floatValue").filter(|x| x.is_finite())?),
        "STRING" => json!(v.get("textValue").and_then(Kv::str)?),
        "COLOR" => color(v.get("colorValue")?),
        _ => return None,
    };
    let mut out = json!({"type": ty, "value": value});
    if let Some(r) = d.get("resolvedDataType").and_then(Kv::str) {
        out["resolvedType"] = json!(r);
    }
    Some(out)
}

/// The text a layer that isn't a text layer carries (a note's, a shape's), unless blank.
fn own_text<'a>(v: &View<'a>) -> Option<&'a str> {
    v.get("textData").and_then(|t| t.get("characters")).and_then(Kv::str).filter(|c| !c.trim().is_empty())
}

/// The REST name of a stroke cap.
fn cap_name(cap: &str) -> &str {
    match cap {
        "ARROW_LINES" => "LINE_ARROW",
        "ARROW_EQUILATERAL" => "TRIANGLE_ARROW",
        c => c,
    }
}

/// The layer kind (and how the warning names it) that a layer the editor has no counterpart for
/// imports as: frames for notes, shapes, tables, slides and the like (their text as a text layer),
/// vectors for connectors and marker strokes, plain text for text on a path.
fn stand_in(ty: &str) -> Option<(&'static str, &'static str)> {
    Some(match ty {
        "STICKY"
        | "SHAPE_WITH_TEXT"
        | "CODE_BLOCK"
        | "TABLE"
        | "TABLE_CELL"
        | "STAMP"
        | "MEDIA"
        | "WIDGET"
        | "SLIDE"
        | "SLIDE_GRID"
        | "SLIDE_ROW"
        | "INTERACTIVE_SLIDE_ELEMENT"
        | "MODULE" => ("FRAME", "frames with their text"),
        "CONNECTOR" | "HIGHLIGHT" | "WASHI_TAPE" => ("VECTOR", "vectors"),
        "TEXT_PATH" => ("TEXT", "text off its path"),
        _ => return None,
    })
}

/// How a layer sizes and sits in an auto layout parent.
fn child_layout(o: &mut Obj, v: &View, parent: Option<&View>) {
    copy_num(o, v, "stackChildPrimaryGrow", "layoutGrow");
    if v.str("stackChildAlignSelf") == Some("STRETCH") {
        o.insert("layoutAlign".into(), json!("STRETCH"));
    }
    if v.str("stackPositioning") == Some("ABSOLUTE") {
        o.insert("layoutPositioning".into(), json!("ABSOLUTE"));
    }
    for (k, w, h) in [("minSize", "minWidth", "minHeight"), ("maxSize", "maxWidth", "maxHeight")] {
        if let Some(val) = v.get(k).and_then(|s| s.get("value")) {
            for (axis, to) in [("x", w), ("y", h)] {
                if let Some(x) = num(val, axis).filter(|x| *x > 0.0 && *x < 1e9) {
                    o.insert(to.into(), json!(x));
                }
            }
        }
    }
    if let Some(p) = parent.filter(|p| p.str("stackMode") == Some("GRID")) {
        for (anchor, list, to) in [("gridColumnAnchor", "gridColumns", "gridColumnAnchorIndex"), ("gridRowAnchor", "gridRows", "gridRowAnchorIndex")]
        {
            if let Some(i) = v.get(anchor).and_then(guid).and_then(|g| tracks(p, list).iter().position(|t| *t == g)) {
                o.insert(to.into(), json!(i));
            }
        }
        copy_num(o, v, "gridColumnSpan", "gridColumnSpan");
        copy_num(o, v, "gridRowSpan", "gridRowSpan");
    }
}

fn paint(p: &Kv) -> Option<Value> {
    let ty = p.get("type").and_then(Kv::str)?;
    let mut o = Obj::new();
    o.insert("type".into(), json!(ty));
    if let Some(x) = num(p, "opacity") {
        o.insert("opacity".into(), json!(x));
    }
    if let Some(b) = p.get("visible").and_then(Kv::bool) {
        o.insert("visible".into(), json!(b));
    }
    if let Some(b) = p.get("blendMode").and_then(Kv::str) {
        o.insert("blendMode".into(), json!(b));
    }
    let t = p.get("transform").and_then(matrix);
    match ty {
        "SOLID" => {
            if let Some(c) = p.get("color") {
                o.insert("color".into(), color(c));
            }
            if let Some(var) = alias(p.get("colorVar")) {
                o.insert("boundVariables".into(), json!({"color": {"type": "VARIABLE_ALIAS", "id": var}}));
            }
        }
        _ if ty.starts_with("GRADIENT_") => {
            // The paint transform maps the layer's unit box to gradient space, where a linear
            // gradient runs from (0, ½) to (1, ½) and the others are centred on (½, ½).
            let inv = t.filter(|t| t.determinant().abs() > 1e-12).map_or(Affine::IDENTITY, Affine::inverse);
            let pts = if ty == "GRADIENT_LINEAR" { [(0.0, 0.5), (1.0, 0.5), (0.0, 1.0)] } else { [(0.5, 0.5), (1.0, 0.5), (0.5, 1.0)] };
            let handles: Vec<Value> = pts
                .iter()
                .map(|(x, y)| {
                    let q = inv * Point::new(*x, *y);
                    json!({"x": q.x, "y": q.y})
                })
                .collect();
            o.insert("gradientHandlePositions".into(), Value::Array(handles));
            // Stops bound to variables are listed again, each with its variable.
            let vars = p.get("stopsVar").map(Kv::list).unwrap_or_default();
            let stops: Vec<Value> = p
                .get("stops")
                .map(Kv::list)
                .unwrap_or_default()
                .iter()
                .enumerate()
                .map(|(i, s)| {
                    let mut stop = json!({"position": num(s, "position").unwrap_or(0.0), "color": s.get("color").map_or(Value::Null, color)});
                    if let Some(b) = bound(vars.get(i), &[("colorVar", "color")]) {
                        stop["boundVariables"] = b;
                    }
                    stop
                })
                .collect();
            o.insert("gradientStops".into(), Value::Array(stops));
        }
        "IMAGE" => {
            let hash: String =
                p.get("image").and_then(|i| i.get("hash")).and_then(Kv::bytes).unwrap_or_default().iter().map(|b| format!("{b:02x}")).collect();
            o.insert("imageRef".into(), json!(hash));
            let mode = p.get("imageScaleMode").and_then(Kv::str).unwrap_or("FILL");
            o.insert("scaleMode".into(), json!(mode));
            if let (Some(t), "STRETCH") = (t, mode) {
                o.insert("imageTransform".into(), rows(t));
            }
            if let Some(s) = num(p, "scale") {
                o.insert("scalingFactor".into(), json!(s));
            }
            if let Some(r) = num(p, "rotation") {
                o.insert("rotation".into(), json!(r));
            }
        }
        _ => {}
    }
    Some(Value::Object(o))
}

/// The variables bound to the fields of a paint stop, an effect or a layout grid, as its REST
/// `boundVariables`: `fields` pairs the format's field with the REST name.
fn bound(v: Option<&Kv>, fields: &[(&str, &str)]) -> Option<Value> {
    let v = v?;
    let found: Obj =
        fields.iter().filter_map(|(from, to)| Some((to.to_string(), json!({"type": "VARIABLE_ALIAS", "id": alias(v.get(from))?})))).collect();
    (!found.is_empty()).then_some(Value::Object(found))
}

/// The track ids of a grid's columns or rows, in order.
fn tracks(v: &View, key: &str) -> Vec<String> {
    let mut t: Vec<(&str, String)> = v
        .get(key)
        .and_then(|m| m.get("entries"))
        .map(Kv::list)
        .unwrap_or_default()
        .iter()
        .filter_map(|e| Some((e.get("position").and_then(Kv::str).unwrap_or(""), e.get("id").and_then(guid)?)))
        .collect();
    t.sort();
    t.into_iter().map(|e| e.1).collect()
}

/// The variable an `ALIAS` variable data points at.
fn alias(d: Option<&Kv>) -> Option<String> {
    let d = d?;
    if d.get("dataType").and_then(Kv::str) != Some("ALIAS") {
        return None;
    }
    d.get("value").and_then(|v| v.get("alias")).and_then(ref_guid)
}

fn variable_value(d: &Kv) -> Option<Value> {
    if let Some(a) = alias(Some(d)) {
        return Some(json!({"type": "VARIABLE_ALIAS", "id": a}));
    }
    let v = d.get("value")?;
    if let Some(c) = v.get("colorValue") {
        return Some(color(c));
    }
    if let Some(b) = v.get("boolValue").and_then(Kv::bool) {
        return Some(json!(b));
    }
    if let Some(x) = num(v, "floatValue") {
        return Some(json!(x));
    }
    v.get("textValue").and_then(Kv::str).map(|s| json!(s))
}

/// Per-character styles of text data `td` (`characterStyleIDs` + `styleOverrideTable`) as REST
/// `characterStyleOverrides` + `styleOverrideTable`; `size`: the base font size.
fn char_styles(o: &mut Obj, td: &Kv, size: Option<f64>) {
    let ids: Vec<u64> = td.get("characterStyleIDs").map(Kv::list).unwrap_or_default().iter().filter_map(Kv::f64).map(|i| i as u64).collect();
    if ids.iter().all(|i| *i == 0) {
        return;
    }
    let mut table = Obj::new();
    for e in td.get("styleOverrideTable").map(Kv::list).unwrap_or_default() {
        let Some(sid) = num(e, "styleID") else { continue };
        let mut s = text_style(&View::of(e), size);
        if let Some(fills) = e.get("fillPaints") {
            s.insert("fills".into(), Value::Array(fills.list().iter().filter_map(paint).collect()));
        }
        table.insert((sid as u64).to_string(), Value::Object(s));
    }
    o.insert("characterStyleOverrides".into(), json!(ids));
    o.insert("styleOverrideTable".into(), Value::Object(table));
}

/// The text of a component property value.
fn prop_text<'k>(v: &'k Kv) -> Option<&'k str> {
    v.get("textValue").and_then(|t| t.get("characters")).and_then(Kv::str)
}

/// The property a layer's `field` is bound to (`parameterConsumptionMap`).
fn prop_ref(v: &View, field: &str) -> Option<String> {
    v.get("parameterConsumptionMap")?.get("entries")?.list().iter().find_map(|e| {
        if e.get("variableField").and_then(Kv::str) != Some(field) {
            return None;
        }
        e.get("variableData")?.get("value")?.get("propRefValue")?.get("defId").and_then(guid)
    })
}

/// The REST TypeStyle fields a node (or text override entry) sets. `size`: the font size that
/// letter spacing in percent refers to when the entry doesn't set one.
fn text_style(v: &View, size: Option<f64>) -> Obj {
    let mut s = Obj::new();
    if let Some(f) = v.get("fontName") {
        for (from, to) in [("family", "fontFamily"), ("style", "fontStyle"), ("postscript", "fontPostScriptName")] {
            if let Some(x) = f.get(from).and_then(Kv::str).filter(|x| !x.is_empty()) {
                // Compact and spaced style names ("SemiBold", "Semi Bold") are the same style.
                s.insert(to.into(), if from == "style" { json!(super::style_name(x)) } else { json!(x) });
            }
        }
    }
    copy_num(&mut s, v, "fontSize", "fontSize");
    copy_num(&mut s, v, "paragraphSpacing", "paragraphSpacing");
    copy_num(&mut s, v, "paragraphIndent", "paragraphIndent");
    copy_str(&mut s, v, "textCase", "textCase");
    copy_str(&mut s, v, "textDecoration", "textDecoration");
    let font_size = v.num("fontSize").or(size).unwrap_or(12.0);
    if let Some(ls) = v.get("letterSpacing") {
        let val = num(ls, "value").unwrap_or(0.0);
        let px = match ls.get("units").and_then(Kv::str) {
            Some("PIXELS") => val,
            Some("PERCENT") => {
                // Private (read by `rest/text.rs`): REST spacing is in pixels; keep the percent.
                s.insert("_letterSpacingPercent".into(), json!(val));
                val / 100.0 * font_size
            }
            _ => val * font_size,
        };
        s.insert("letterSpacing".into(), json!(px));
    }
    if let Some(lh) = v.get("lineHeight") {
        let val = num(lh, "value").unwrap_or(100.0);
        match lh.get("units").and_then(Kv::str) {
            Some("PIXELS") => {
                s.insert("lineHeightUnit".into(), json!("PIXELS"));
                s.insert("lineHeightPx".into(), json!(val));
            }
            // shortcut: percent units count from the font's own line height (100 % is "auto"), which
            // the document can only say for 100 %; other values read as % of the font size. Upgrade
            // when line heights can be a share of the font's own.
            Some("PERCENT") if (val - 100.0).abs() < 1e-6 => {
                s.insert("lineHeightUnit".into(), json!("INTRINSIC_%"));
            }
            Some("PERCENT") => {
                s.insert("lineHeightUnit".into(), json!("FONT_SIZE_%"));
                s.insert("lineHeightPercentFontSize".into(), json!(val));
            }
            _ => {
                s.insert("lineHeightUnit".into(), json!("FONT_SIZE_%"));
                s.insert("lineHeightPercentFontSize".into(), json!(val * 100.0));
            }
        }
    }
    if let Some(h) = v.get("hyperlink") {
        match h.get("url").and_then(Kv::str) {
            Some(u) if !u.is_empty() => {
                s.insert("hyperlink".into(), json!({"type": "URL", "url": u}));
            }
            // An empty URL clears a link the entry would otherwise take from the base.
            Some(_) => {
                s.insert("hyperlink".into(), Value::Null);
            }
            None => {}
        }
    }
    s
}

fn effect(e: &Kv) -> Option<Value> {
    let ty = match e.get("type").and_then(Kv::str)? {
        "FOREGROUND_BLUR" => "LAYER_BLUR",
        "GRAIN" => "TEXTURE",
        t => t,
    };
    let mut o = Obj::new();
    o.insert("type".into(), json!(ty));
    for k in ["radius", "spread", "startRadius", "density", "opacity"] {
        if let Some(x) = num(e, k) {
            o.insert(k.into(), json!(x));
        }
    }
    for k in ["visible", "showShadowBehindNode", "clipToShape"] {
        if let Some(b) = e.get(k).and_then(Kv::bool) {
            o.insert(k.into(), json!(b));
        }
    }
    for k in ["blendMode", "noiseType"] {
        if let Some(s) = e.get(k).and_then(Kv::str) {
            o.insert(k.into(), json!(s));
        }
    }
    for k in ["color", "secondaryColor"] {
        if let Some(c) = e.get(k) {
            o.insert(k.into(), color(c));
        }
    }
    for (from, to) in [("offset", "offset"), ("startOffset", "startOffset"), ("endOffset", "endOffset"), ("noiseSize", "noiseSizeVector")] {
        if let Some(p) = e.get(from).and_then(vec2) {
            o.insert(to.into(), p);
        }
    }
    if e.get("blurOpType").and_then(Kv::str) == Some("PROGRESSIVE") {
        o.insert("blurType".into(), json!("PROGRESSIVE"));
    }
    let vars = [
        ("colorVar", "color"),
        ("radiusVar", "radius"),
        ("spreadVar", "spread"),
        ("xVar", "offsetX"),
        ("yVar", "offsetY"),
        ("startRadiusVar", "startRadius"),
    ];
    if let Some(b) = bound(Some(e), &vars) {
        o.insert("boundVariables".into(), b);
    }
    Some(Value::Object(o))
}

fn layout_grid(g: &Kv) -> Value {
    let pattern = match (g.get("pattern").and_then(Kv::str), g.get("axis").and_then(Kv::str)) {
        (Some("GRID"), _) => "GRID",
        (_, Some("Y")) => "ROWS",
        _ => "COLUMNS",
    };
    let mut o = json!({"pattern": pattern, "alignment": g.get("type").and_then(Kv::str).unwrap_or("STRETCH")});
    for (from, to) in [("sectionSize", "sectionSize"), ("numSections", "count"), ("gutterSize", "gutterSize"), ("offset", "offset")] {
        if let Some(x) = num(g, from) {
            o[to] = json!(x);
        }
    }
    if let Some(c) = g.get("color") {
        o["color"] = color(c);
    }
    if let Some(b) = g.get("visible").and_then(Kv::bool) {
        o["visible"] = json!(b);
    }
    let vars = [("numSectionsVar", "count"), ("offsetVar", "offset"), ("sectionSizeVar", "sectionSize"), ("gutterSizeVar", "gutterSize")];
    if let Some(b) = bound(Some(g), &vars) {
        o["boundVariables"] = b;
    }
    o
}

fn export_setting(e: &Kv) -> Value {
    let c = e.get("constraint");
    let ty = match c.and_then(|c| c.get("type")).and_then(Kv::str) {
        Some("CONTENT_WIDTH") => "WIDTH",
        Some("CONTENT_HEIGHT") => "HEIGHT",
        _ => "SCALE",
    };
    json!({
        "format": e.get("imageType").and_then(Kv::str).unwrap_or("PNG"),
        "suffix": e.get("suffix").and_then(Kv::str).unwrap_or(""),
        "constraint": {"type": ty, "value": c.and_then(|c| num(c, "value")).unwrap_or(1.0)},
    })
}

/// A prototype action's transition (`None`: instant).
fn transition(a: &Kv) -> Option<Value> {
    let ty = a.get("transitionType").and_then(Kv::str)?;
    // Directions name where the incoming screen moves (SLIDE_FROM_LEFT moves right).
    let (kind, dir) = match ty {
        "INSTANT_TRANSITION" => return None,
        "DISSOLVE" | "FADE" => ("DISSOLVE", None),
        "SMART_ANIMATE" | "MAGIC_MOVE" => ("SMART_ANIMATE", None),
        "SCROLL_ANIMATE" => ("SCROLL_ANIMATE", None),
        t => {
            let prefixes = [
                ("SLIDE_FROM_", "SLIDE_IN"),
                ("PUSH_FROM_", "PUSH"),
                ("MOVE_FROM_", "MOVE_IN"),
                ("SLIDE_OUT_TO_", "SLIDE_OUT"),
                ("MOVE_OUT_TO_", "MOVE_OUT"),
            ];
            let (kind, side) = prefixes.iter().find_map(|(p, k)| t.strip_prefix(p).map(|r| (*k, r)))?;
            let incoming = matches!(kind, "SLIDE_IN" | "PUSH" | "MOVE_IN");
            let dir = match (side, incoming) {
                ("LEFT", true) | ("RIGHT", false) => "RIGHT",
                ("RIGHT", true) | ("LEFT", false) => "LEFT",
                ("TOP", true) | ("BOTTOM", false) => "BOTTOM",
                _ => "TOP",
            };
            (kind, Some(dir))
        }
    };
    let easing = match a.get("easingType").and_then(Kv::str) {
        Some("IN_CUBIC" | "EASE_IN") => "EASE_IN",
        Some("INOUT_CUBIC") => "EASE_IN_AND_OUT",
        Some("LINEAR") => "LINEAR",
        Some("IN_BACK_CUBIC") => "EASE_IN_BACK",
        Some("OUT_BACK_CUBIC") => "EASE_OUT_BACK",
        Some("INOUT_BACK_CUBIC") => "EASE_IN_AND_OUT_BACK",
        Some("GENTLE_SPRING") => "GENTLE",
        Some("CUSTOM_CUBIC") => "CUSTOM_CUBIC_BEZIER",
        _ => "EASE_OUT",
    };
    let mut e = json!({"type": easing});
    let f: Vec<f64> = a.get("easingFunction").map(Kv::list).unwrap_or_default().iter().filter_map(Kv::f64).collect();
    if let (true, [x1, y1, x2, y2, ..]) = (easing == "CUSTOM_CUBIC_BEZIER", f.as_slice()) {
        e["easingFunctionCubicBezier"] = json!({"x1": x1, "y1": y1, "x2": x2, "y2": y2});
    }
    let mut t = json!({"type": kind, "duration": (num(a, "transitionDuration").unwrap_or(0.3) * 1000.0).round(), "easing": e});
    if let Some(d) = dir {
        t["direction"] = json!(d);
    }
    Some(t)
}

/// SVG path data of a geometry blob: commands `0` close, `1` move, `2` line, `3` quadratic and
/// `4` cubic, each followed by its points as little-endian `f32` pairs. `None` when damaged.
fn svg_path(b: &[u8]) -> Option<String> {
    let mut out = String::new();
    let mut i = 0usize;
    let mut n = 0usize;
    while i < b.len() {
        n += 1;
        if n > MAX_COMMANDS {
            return None;
        }
        let (letter, points) = match *b.get(i)? {
            0 => ('Z', 0),
            1 => ('M', 1),
            2 => ('L', 1),
            3 => ('Q', 2),
            4 => ('C', 3),
            _ => return None,
        };
        i += 1;
        if !out.is_empty() {
            out.push(' ');
        }
        out.push(letter);
        for _ in 0..points * 2 {
            let bytes: [u8; 4] = b.get(i..i + 4)?.try_into().ok()?;
            let x = f32::from_le_bytes(bytes);
            if !x.is_finite() {
                return None;
            }
            out.push_str(&format!(" {x}"));
            i += 4;
        }
    }
    Some(out)
}

/// Resource provenance is separate from the component model: no defaults are added to mains.
/// The records are private: an inconsistent, stale or oversized one is left out with one counted
/// warning per kind and never makes the whole file fail to open.
mod property_capture {
    use super::super::property_resources as r;
    use super::{Kv, Obj, View};
    use crate::FormatError;
    use r::{
        Binding, Bindings, Catalog, DROPPED_DAMAGED, DROPPED_LIMIT, DROPPED_STALE, DROPPED_UNBOUND, Definition, Id, MainTag, NativePacket, Owner,
        Role, WirePacket, WireResources,
    };
    use serde_json::{Value, json};
    use std::collections::{BTreeMap, BTreeSet};

    const DROPPED_DEFINITION: &str = "hidden Boolean property definition(s) with inconsistent or malformed declarations were not retained";
    /// Expanded instance copies repeat their source's packet. Distinct packets share
    /// [`r::MAX_TOTAL_BYTES`]; the copies together are bounded by this.
    const MAX_COPIED_BYTES: usize = 8 * r::MAX_TOTAL_BYTES;

    pub(super) struct Capture<'a> {
        nodes: BTreeMap<Id, &'a Kv<'a>>,
        pub(super) warnings: Vec<String>,
        dropped: BTreeMap<&'static str, usize>,
        /// Distinct source packets by address and length: `Some(carries a catalog)` when valid.
        sources: BTreeMap<(usize, usize), Option<bool>>,
        bytes: usize,
        copied: usize,
    }

    #[derive(Clone)]
    struct Pending {
        node: String,
        main: Id,
        visible_main: Option<Id>,
        values: Vec<Binding>,
        visible: Option<Binding>,
    }

    impl<'a> Capture<'a> {
        pub fn new(msg: &'a Kv<'a>) -> Self {
            let mut nodes = BTreeMap::new();
            // As in the mapper: the last change of a GUID wins; a REMOVED change deletes it.
            for n in msg.get("nodeChanges").map(Kv::list).unwrap_or_default() {
                let Some(id) = n.get("guid").and_then(r::id) else { continue };
                if n.get("phase").and_then(Kv::str) == Some("REMOVED") {
                    nodes.remove(&id);
                } else {
                    nodes.insert(id, n);
                }
            }
            Self { nodes, warnings: Vec::new(), dropped: BTreeMap::new(), sources: BTreeMap::new(), bytes: 0, copied: 0 }
        }

        fn left_out(&mut self, kind: &'static str, count: usize) {
            if count > 0 {
                *self.dropped.entry(kind).or_default() += count;
            }
        }

        fn charge(&mut self, size: usize) -> Result<(), FormatError> {
            let total = self.bytes.checked_add(size).ok_or_else(|| r::invalid("input byte count overflow"))?;
            if total > r::MAX_TOTAL_BYTES {
                return Err(r::invalid("input metadata exceeds its total byte limit"));
            }
            self.bytes = total;
            Ok(())
        }

        pub fn packet(&mut self, out: &mut Obj, view: &View<'a>) {
            let mut found = None;
            let mut ambiguous = false;
            // The effective list is deliberate: an explicit empty list clears inherited data.
            for p in view.list("pluginData") {
                if p.get("pluginID").and_then(Kv::str) != Some(r::PLUGIN_ID) || p.get("key").and_then(Kv::str) != Some(r::PLUGIN_KEY) {
                    continue;
                }
                match p.get("value").and_then(Kv::str) {
                    Some(value) if found.is_none() => found = Some(value),
                    _ => ambiguous = true,
                }
            }
            if ambiguous {
                // A duplicate or non-text packet: none of this layer's packets is trusted.
                self.left_out(DROPPED_DAMAGED, 1);
                return;
            }
            let Some(value) = found else { return };
            // Expanded copies inherit their source's packet: validate and charge it once.
            let source = (value.as_ptr() as usize, value.len());
            let catalog = match self.sources.get(&source) {
                Some(state) => *state,
                None => {
                    let state = if self.charge(value.len()).is_err() {
                        self.left_out(DROPPED_LIMIT, 1);
                        None
                    } else if let Ok(packet) = r::read_wire(value) {
                        Some(packet.native.catalog.is_some())
                    } else {
                        self.left_out(DROPPED_DAMAGED, 1);
                        None
                    };
                    self.sources.insert(source, state);
                    state
                }
            };
            let Some(catalog) = catalog else { return };
            if catalog && out.get("type").and_then(Value::as_str) != Some("CANVAS") {
                // A catalog packet is carried by a page only.
                self.left_out(DROPPED_DAMAGED, 1);
                return;
            }
            match self.copied.checked_add(value.len()).filter(|n| *n <= MAX_COPIED_BYTES) {
                Some(total) => self.copied = total,
                None => {
                    self.left_out(DROPPED_LIMIT, 1);
                    return;
                }
            }
            out.insert(r::FIG_PACKET_FIELD.into(), Value::String(value.to_string()));
        }

        /// Check the private records against the mapped file and leave out what does not hold.
        /// Never fails: a problem with the records as a whole leaves all of them out.
        pub fn finish(&mut self, out: &mut Value) {
            if let Err(error) = self.finish_inner(out) {
                strip_all(out);
                let reason = match &error {
                    FormatError::Io(reason) | FormatError::Corrupt(reason) => reason.trim_start_matches(".fig property resources: ").to_string(),
                    other => other.to_string(),
                };
                self.warnings.push(format!("all private Boolean property records were left out ({reason})"));
            }
            let dropped = r::dropped_warnings(&self.dropped);
            self.warnings.extend(dropped);
        }

        fn finish_inner(&mut self, out: &mut Value) -> Result<(), FormatError> {
            let root = out.get("document").ok_or_else(|| r::invalid("missing mapped document"))?;
            let mut components = BTreeMap::new();
            let mut ambiguous = BTreeSet::new();
            let mut visible = BTreeSet::new();
            let mut packets = Vec::new();
            index(root, None, &mut components, &mut ambiguous, &mut visible, &mut packets, 0)?;
            // An actual main id that names two components cannot anchor provenance.
            components.retain(|id, _| !ambiguous.contains(id));
            if !packets.is_empty() {
                let parsed: Vec<(&Value, Option<WirePacket>)> = packets.iter().map(|(node, raw)| (*node, r::read_wire(raw).ok())).collect();
                let (kept, catalogs, reverse) = self.check_written(&parsed, &ambiguous, &visible);
                let packets: Vec<Option<WirePacket>> = parsed.into_iter().zip(kept).map(|((_, packet), keep)| packet.filter(|_| keep)).collect();
                let root = out.get_mut("document").ok_or_else(|| r::invalid("missing mapped document"))?;
                let mut walk = Walk { packets: &packets, next: 0, reverse: &reverse, catalogs: &catalogs, unbound: 0 };
                walk.apply(root, 0)?;
                let unbound = walk.unbound;
                self.left_out(DROPPED_UNBOUND, unbound);
                let mut strings = Vec::new();
                for c in catalogs.values() {
                    let value = r::write_catalog(c)?;
                    self.charge(value.len())?;
                    strings.push(Value::String(value));
                }
                if !strings.is_empty() {
                    out[r::FIG_CATALOGS_FIELD] = Value::Array(strings);
                }
                return Ok(());
            }

            let mut pending = Vec::new();
            let mut needed = BTreeSet::new();
            let mut budget = Budget::default();
            // Discover IDs without copying property keys. Only retained definitions are materialized.
            collect(root, None, &components, &mut pending, &mut needed, None, &mut budget, 0)?;
            self.left_out(DROPPED_LIMIT, std::mem::take(&mut budget.over));
            if needed.is_empty() {
                return Ok(());
            }
            let Some(catalog) = self.retained_catalog(&needed) else {
                return Ok(());
            };
            let names: BTreeMap<_, _> = catalog.owners.iter().flat_map(|o| &o.definitions).map(|d| (d.id, d.name.as_str())).collect();
            let defined: BTreeSet<_> = names.keys().copied().collect();
            budget.bytes = self.bytes;
            collect(root, None, &components, &mut pending, &mut needed, Some(&defined), &mut budget, 0)?;
            self.left_out(DROPPED_LIMIT, budget.over);
            let mut node_packets = BTreeMap::new();
            let mut ambiguous_carriers = BTreeSet::new();
            let mut mains = BTreeSet::new();
            let mut count = 0usize;
            for mut p in pending {
                p.values.retain(|b| defined.contains(&b.definition));
                p.visible = p.visible.filter(|b| defined.contains(&b.definition));
                if p.values.is_empty() && p.visible.is_none() {
                    continue;
                }
                if p.values.is_empty()
                    && let Some(main) = p.visible_main
                {
                    p.main = main;
                }
                if p.visible.is_none() || p.visible_main == Some(p.main) {
                    p.visible_main = None;
                }
                // The effective property name must be the retained definition's.
                if p.values
                    .iter()
                    .chain(p.visible.iter())
                    .any(|b| names.get(&b.definition).is_none_or(|name| b.key != format!("{name}#{}", b.definition.key())))
                {
                    self.left_out(DROPPED_DAMAGED, 1);
                    continue;
                }
                match count.checked_add(p.values.len() + usize::from(p.visible.is_some())).filter(|n| *n <= r::MAX_BINDINGS) {
                    Some(total) => count = total,
                    None => {
                        self.left_out(DROPPED_LIMIT, 1);
                        continue;
                    }
                }
                if ambiguous_carriers.contains(&p.node) {
                    self.left_out(DROPPED_DAMAGED, 1);
                    continue;
                }
                p.values.sort_by(|a, b| a.key.cmp(&b.key));
                mains.insert(p.main);
                mains.extend(p.visible_main);
                let packet = WirePacket {
                    version: r::VERSION,
                    native: NativePacket {
                        catalog: None,
                        main: None,
                        bindings: Some(Bindings {
                            version: r::VERSION,
                            scope: catalog.scope.clone(),
                            main: p.main,
                            visible_main: p.visible_main,
                            values: p.values,
                            visible: p.visible,
                        }),
                    },
                    resources: None,
                    aliases: Vec::new(),
                };
                if node_packets.insert(p.node.clone(), packet).is_some() {
                    // Two carriers share one id: neither can be told apart on import.
                    node_packets.remove(&p.node);
                    ambiguous_carriers.insert(p.node);
                    self.left_out(DROPPED_DAMAGED, 2);
                }
            }
            if node_packets.is_empty() {
                return Ok(());
            }
            let string = r::write_catalog(&catalog)?;
            if self.charge(string.len()).is_err() {
                self.left_out(DROPPED_LIMIT, 1);
                return Ok(());
            }
            let root = out.get_mut("document").ok_or_else(|| r::invalid("missing mapped document"))?;
            attach(root, &catalog.scope, &mains, &mut node_packets, self, 0)?;
            self.left_out(DROPPED_STALE, node_packets.len());
            out[r::FIG_CATALOGS_FIELD] = json!([string]);
            Ok(())
        }

        /// Which written packets hold (by index into `parsed`), the catalogs that remain, and
        /// every wire definition id that a written catalog claims, with its scope.
        fn check_written(
            &mut self,
            parsed: &[(&Value, Option<WirePacket>)],
            ambiguous: &BTreeSet<Id>,
            visible: &BTreeSet<Id>,
        ) -> (Vec<bool>, BTreeMap<String, Catalog>, BTreeMap<Id, String>) {
            let mut kept: Vec<bool> = parsed.iter().map(|(_, packet)| packet.is_some()).collect();
            self.left_out(DROPPED_DAMAGED, kept.iter().filter(|k| !**k).count());
            // Wire ids of verified hidden definitions, kept or not: a field naming one needs an alias.
            let mut reverse = BTreeMap::new();
            let mut catalogs: BTreeMap<String, (Catalog, WireResources)> = BTreeMap::new();
            let mut carriers: BTreeMap<String, Vec<usize>> = BTreeMap::new();
            let (mut owners, mut definitions, mut bindings) = (0usize, 0usize, 0usize);
            for (i, (_, packet)) in parsed.iter().enumerate() {
                let Some(packet) = packet else { continue };
                let count = packet.native.bindings.as_ref().map_or(0, |b| b.values.len() + usize::from(b.visible.is_some()));
                match bindings.checked_add(count).filter(|n| *n <= r::MAX_BINDINGS) {
                    Some(total) => bindings = total,
                    None => {
                        kept[i] = false;
                        self.left_out(DROPPED_LIMIT, 1);
                        continue;
                    }
                }
                let Some(catalog) = &packet.native.catalog else { continue };
                let verified = packet.resources.as_ref().filter(|wire| self.validate_resources(catalog, wire, visible).is_ok());
                if let Some(wire) = verified {
                    for d in &wire.definitions {
                        reverse.entry(d.wire).or_insert_with(|| wire.scope.clone());
                    }
                }
                let problem = match verified {
                    None => Some(DROPPED_DAMAGED),
                    Some(wire) => match catalogs.get(&catalog.scope) {
                        Some((previous, resources)) => (previous != catalog || resources != wire).then_some(DROPPED_DAMAGED),
                        None => {
                            let rows = catalog.owners.iter().map(|o| o.definitions.len()).sum::<usize>();
                            if owners + catalog.owners.len() > r::MAX_OWNERS || definitions + rows > r::MAX_DEFINITIONS {
                                Some(DROPPED_LIMIT)
                            } else {
                                owners += catalog.owners.len();
                                definitions += rows;
                                catalogs.insert(catalog.scope.clone(), (catalog.clone(), wire.clone()));
                                None
                            }
                        }
                    },
                };
                match problem {
                    Some(kind) => {
                        kept[i] = false;
                        self.left_out(kind, 1);
                    }
                    None => carriers.entry(catalog.scope.clone()).or_default().push(i),
                }
            }
            // A wire id claimed by two catalogs belongs to neither.
            let mut allocated = BTreeSet::new();
            let mut shared = Vec::new();
            for (scope, (_, wire)) in &catalogs {
                let ids: Vec<Id> = wire.owners.iter().map(|o| o.wire).chain(wire.definitions.iter().map(|d| d.wire)).collect();
                if ids.iter().any(|id| allocated.contains(id)) {
                    shared.push(scope.clone());
                } else {
                    allocated.extend(ids);
                }
            }
            for scope in shared {
                catalogs.remove(&scope);
                for i in carriers.remove(&scope).unwrap_or_default() {
                    if let Some(k) = kept.get_mut(i) {
                        *k = false;
                    }
                    self.left_out(DROPPED_DAMAGED, 1);
                }
            }
            let names: BTreeMap<&str, BTreeMap<Id, &str>> = catalogs
                .iter()
                .map(|(scope, (c, _))| (scope.as_str(), c.owners.iter().flat_map(|o| &o.definitions).map(|d| (d.id, d.name.as_str())).collect()))
                .collect();
            let remaps: BTreeMap<&str, BTreeMap<Id, Id>> =
                catalogs.iter().map(|(scope, (_, w))| (scope.as_str(), w.definitions.iter().map(|d| (d.original, d.wire)).collect())).collect();
            // Leaving out a catalog carrier can orphan other packets: repeat until nothing changes.
            let mut live: BTreeSet<String> = catalogs.keys().cloned().collect();
            loop {
                let mut changed = false;
                for (i, (node, packet)) in parsed.iter().enumerate() {
                    let Some(packet) = packet.as_ref().filter(|_| kept.get(i).copied().unwrap_or(false)) else { continue };
                    let Some(kind) = check_packet(node, packet, &live, &names, &remaps, ambiguous) else { continue };
                    if let Some(k) = kept.get_mut(i) {
                        *k = false;
                    }
                    self.left_out(kind, 1);
                    changed = true;
                    if let Some(scope) = packet.native.catalog.as_ref().map(|c| &c.scope)
                        && let Some(list) = carriers.get_mut(scope)
                    {
                        list.retain(|j| *j != i);
                        if list.is_empty() {
                            live.remove(scope);
                        }
                    }
                }
                if !changed {
                    break;
                }
            }
            let catalogs = catalogs.into_iter().filter(|(scope, _)| live.contains(scope)).map(|(scope, (c, _))| (scope, c)).collect();
            (kept, catalogs, reverse)
        }

        /// The catalog of the hidden definitions `needed` names, without any definition whose
        /// declarations are malformed or disagree (no default or name is guessed).
        fn retained_catalog(&mut self, needed: &BTreeSet<Id>) -> Option<Catalog> {
            let mut owner_rows = Vec::new();
            let mut bad = BTreeSet::new();
            let mut seen_names: BTreeMap<Id, &str> = BTreeMap::new();
            let mut damaged = 0;
            for (id, node) in &self.nodes {
                if node.get("type").and_then(Kv::str) != Some("SYMBOL") {
                    continue;
                }
                let defs = node.get("componentPropDefs").map(Kv::list).unwrap_or_default();
                if !defs.iter().any(|d| d.get("id").and_then(r::id).is_some_and(|d| needed.contains(&d))) {
                    continue;
                }
                let page = match self.hidden_page(*id) {
                    Ok(Some(page)) => page,
                    Ok(None) => continue,
                    Err(_) => {
                        damaged += 1;
                        continue;
                    }
                };
                let Some(name) = node.get("name").and_then(Kv::str).filter(|name| bounded_name(name).is_ok()) else {
                    damaged += 1;
                    continue;
                };
                let mut selected = Vec::new();
                let mut seen = BTreeSet::new();
                for d in defs {
                    let Some(definition) = d.get("id").and_then(r::id).filter(|d| needed.contains(d)) else {
                        continue;
                    };
                    if d.get("isDeleted").and_then(Kv::bool) == Some(true) {
                        continue;
                    }
                    let key = d.get("name").and_then(Kv::str).filter(|key| bounded_name(key).is_ok());
                    let boolean = d.get("type").and_then(Kv::str) == Some("BOOL");
                    let (Some(key), true, Ok(initial), true) = (key, boolean, initial(d), seen.insert(definition)) else {
                        bad.insert(definition);
                        continue;
                    };
                    if seen_names.insert(definition, key).is_some_and(|previous| previous != key) {
                        bad.insert(definition);
                    }
                    selected.push(Definition { id: definition, name: key.to_string(), initial });
                }
                if !selected.is_empty() {
                    owner_rows.push(Owner { id: *id, page, name: name.to_string(), definitions: selected });
                }
            }
            self.left_out(DROPPED_DAMAGED, damaged);
            self.left_out(DROPPED_DEFINITION, bad.len());
            let mut kept = Vec::new();
            let (mut rows, mut copied, mut over) = (0usize, 0usize, 0usize);
            for mut owner in owner_rows {
                owner.definitions.retain(|d| !bad.contains(&d.id));
                if owner.definitions.is_empty() {
                    continue;
                }
                let size = owner.name.len() + owner.definitions.iter().map(|d| d.name.len()).sum::<usize>();
                if kept.len() >= r::MAX_OWNERS || rows + owner.definitions.len() > r::MAX_DEFINITIONS || copied + size > r::MAX_CATALOG_BYTES {
                    over += 1;
                    continue;
                }
                rows += owner.definitions.len();
                copied += size;
                kept.push(owner);
            }
            self.left_out(DROPPED_LIMIT, over);
            if kept.is_empty() {
                return None;
            }
            match r::make_catalog(kept) {
                Ok(catalog) => Some(catalog),
                Err(_) => {
                    self.left_out(DROPPED_LIMIT, 1);
                    None
                }
            }
        }

        fn hidden_page(&self, start: Id) -> Result<Option<Id>, FormatError> {
            let mut current = start;
            let mut seen = BTreeSet::new();
            for _ in 0..r::MAX_CHAIN {
                if !seen.insert(current) {
                    return Err(r::invalid("retained owner parent cycle"));
                }
                let Some(n) = self.nodes.get(&current) else {
                    return Ok(None);
                };
                if n.get("type").and_then(Kv::str) == Some("CANVAS") {
                    return Ok((n.get("internalOnly").and_then(Kv::bool) == Some(true)).then_some(current));
                }
                let Some(parent) = n.get("parentIndex").and_then(|p| p.get("guid")).and_then(r::id) else {
                    return Ok(None);
                };
                current = parent;
            }
            Err(r::invalid("retained owner ancestry exceeds its limit"))
        }

        fn validate_resources(&self, catalog: &Catalog, wire: &WireResources, visible: &BTreeSet<Id>) -> Result<(), FormatError> {
            r::validate_catalog(catalog)?;
            if wire.scope != catalog.scope {
                return Err(r::invalid("resource scope disagrees with catalog"));
            }
            let page = self.nodes.get(&wire.page).ok_or_else(|| r::invalid("resource page is missing"))?;
            if visible.contains(&wire.page)
                || page.get("type").and_then(Kv::str) != Some("CANVAS")
                || page.get("internalOnly").and_then(Kv::bool) != Some(true)
            {
                return Err(r::invalid("resource page is not an unambiguous hidden canvas"));
            }
            let definitions: BTreeMap<_, _> = wire.definitions.iter().map(|d| (d.original, d.wire)).collect();
            let owners: BTreeMap<_, _> = wire.owners.iter().map(|o| (o.original, o.wire)).collect();
            let expected: BTreeSet<_> = catalog.owners.iter().flat_map(|o| &o.definitions).map(|d| d.id).collect();
            if owners.len() != catalog.owners.len()
                || definitions.len() != expected.len()
                || definitions.keys().copied().collect::<BTreeSet<_>>() != expected
            {
                return Err(r::invalid("resource remaps do not cover the catalog exactly"));
            }
            let mut ids = BTreeSet::from([wire.page]);
            for id in owners.values().chain(definitions.values()) {
                if !ids.insert(*id) || visible.contains(id) {
                    return Err(r::invalid("resource id collision"));
                }
            }
            for owner in &catalog.owners {
                let mapped = owners.get(&owner.id).ok_or_else(|| r::invalid("missing owner remap"))?;
                let node = self.nodes.get(mapped).ok_or_else(|| r::invalid("standard resource owner is missing"))?;
                if node.get("type").and_then(Kv::str) != Some("SYMBOL")
                    || node.get("name").and_then(Kv::str) != Some(owner.name.as_str())
                    || node.get("parentIndex").and_then(|p| p.get("guid")).and_then(r::id) != Some(wire.page)
                {
                    return Err(r::invalid("standard resource owner does not match its hidden carrier"));
                }
                let actual = node.get("componentPropDefs").map(Kv::list).unwrap_or_default();
                if actual.len() != owner.definitions.len() {
                    return Err(r::invalid("standard owner definition count differs"));
                }
                let mut by_id = BTreeMap::new();
                for d in actual {
                    let id = d.get("id").and_then(r::id).ok_or_else(|| r::invalid("standard definition lacks its id"))?;
                    if by_id.insert(id, d).is_some() {
                        return Err(r::invalid("duplicate standard definition id"));
                    }
                }
                for definition in &owner.definitions {
                    let remapped = definitions.get(&definition.id).ok_or_else(|| r::invalid("missing definition remap"))?;
                    let d = by_id.get(remapped).ok_or_else(|| r::invalid("standard Boolean definition is missing"))?;
                    if d.get("type").and_then(Kv::str) != Some("BOOL")
                        || d.get("isDeleted").and_then(Kv::bool) == Some(true)
                        || d.get("name").and_then(Kv::str) != Some(definition.name.as_str())
                        || initial(d)? != definition.initial
                    {
                        return Err(r::invalid("standard Boolean definition disagrees with original resource"));
                    }
                }
            }
            Ok(())
        }
    }

    /// Why a written packet does not hold against the mapped file, if it does not.
    fn check_packet(
        node: &Value,
        packet: &WirePacket,
        live: &BTreeSet<String>,
        names: &BTreeMap<&str, BTreeMap<Id, &str>>,
        remaps: &BTreeMap<&str, BTreeMap<Id, Id>>,
        ambiguous: &BTreeSet<Id>,
    ) -> Option<&'static str> {
        if let Some(main) = &packet.native.main {
            let component = node.get("type").and_then(Value::as_str) == Some("COMPONENT") && node_id(node).is_some_and(|id| !ambiguous.contains(&id));
            if !component || !live.contains(&main.scope) {
                // The main tag lacks its actual component or catalog.
                return Some(DROPPED_STALE);
            }
        }
        let Some(binding) = &packet.native.bindings else { return None };
        let (true, Some(names), Some(remaps)) =
            (live.contains(&binding.scope), names.get(binding.scope.as_str()), remaps.get(binding.scope.as_str()))
        else {
            // The bindings lack their catalog (e.g. its page was deleted).
            return Some(DROPPED_STALE);
        };
        for alias in &packet.aliases {
            let Some(name) = names.get(&alias.definition).filter(|_| remaps.get(&alias.definition).copied() == Some(alias.wire)) else {
                return Some(DROPPED_DAMAGED);
            };
            let raw_key = format!("{name}#{}", alias.wire.key());
            let holds = match alias.role {
                Role::Value => {
                    node.get("type").and_then(Value::as_str) == Some("INSTANCE")
                        && node.get("componentProperties").and_then(|p| p.get(&raw_key)).is_some_and(|p| {
                            p.get("type").and_then(Value::as_str) == Some("BOOLEAN") && p.get("value").is_some_and(Value::is_boolean)
                        })
                }
                Role::Visible => {
                    node.get("componentPropertyReferences").and_then(|p| p.get("visible")).and_then(Value::as_str) == Some(raw_key.as_str())
                }
            };
            if !holds {
                // The alias lacks its effective standard Boolean field (e.g. the assignment was
                // removed in another editor and the packet stayed behind).
                return Some(DROPPED_STALE);
            }
        }
        None
    }

    /// The second pass over a file with written packets, in the same pre-order as [`index`].
    struct Walk<'w> {
        packets: &'w [Option<WirePacket>],
        next: usize,
        reverse: &'w BTreeMap<Id, String>,
        catalogs: &'w BTreeMap<String, Catalog>,
        unbound: usize,
    }

    impl Walk<'_> {
        /// Remove the packets that do not hold, and the standard fields that name a written
        /// resource definition without a kept alias (their provenance is gone).
        fn apply(&mut self, node: &mut Value, depth: usize) -> Result<(), FormatError> {
            if depth > super::MAX_DEPTH + 4 {
                return Err(r::invalid("mapped resource tree exceeds its depth limit"));
            }
            let packets = self.packets;
            let mut packet = None;
            if node.get(r::FIG_PACKET_FIELD).and_then(Value::as_str).is_some() {
                packet = packets.get(self.next).and_then(Option::as_ref);
                self.next += 1;
                if packet.is_none()
                    && let Some(fields) = node.as_object_mut()
                {
                    fields.remove(r::FIG_PACKET_FIELD);
                }
            }
            let aliases: BTreeSet<(Role, Id)> = packet.into_iter().flat_map(|p| &p.aliases).map(|a| (a.role, a.wire)).collect();
            let scope = packet.and_then(|p| p.native.bindings.as_ref()).map(|b| b.scope.as_str());
            let (reverse, catalogs) = (self.reverse, self.catalogs);
            let holds = |role, key: &str| {
                let Some(id) = key.rsplit_once('#').and_then(|(_, id)| Id::parse(id)) else {
                    return true;
                };
                let Some(owner) = reverse.get(&id) else {
                    return true;
                };
                catalogs.contains_key(owner) && aliases.contains(&(role, id)) && scope == Some(owner.as_str())
            };
            if let Some(props) = node.get_mut("componentProperties").and_then(Value::as_object_mut) {
                let before = props.len();
                props.retain(|key, p| {
                    !(p.get("type").and_then(Value::as_str) == Some("BOOLEAN") && p.get("value").is_some_and(Value::is_boolean))
                        || holds(Role::Value, key)
                });
                self.unbound += before - props.len();
            }
            if let Some(refs) = node.get_mut("componentPropertyReferences").and_then(Value::as_object_mut)
                && refs.get("visible").and_then(Value::as_str).is_some_and(|key| !holds(Role::Visible, key))
            {
                refs.remove("visible");
                self.unbound += 1;
            }
            if let Some(children) = node.get_mut("children").and_then(Value::as_array_mut) {
                for child in children {
                    self.apply(child, depth + 1)?;
                }
            }
            Ok(())
        }
    }

    /// Remove every private record from the mapped file.
    fn strip_all(out: &mut Value) {
        if let Some(fields) = out.as_object_mut() {
            fields.remove(r::FIG_CATALOGS_FIELD);
        }
        let mut stack: Vec<&mut Value> = out.get_mut("document").into_iter().collect();
        while let Some(node) = stack.pop() {
            let Some(fields) = node.as_object_mut() else { continue };
            fields.remove(r::FIG_PACKET_FIELD);
            if let Some(Value::Array(children)) = fields.get_mut("children") {
                stack.extend(children.iter_mut());
            }
        }
    }

    fn initial(d: &Kv<'_>) -> Result<Option<bool>, FormatError> {
        match d.get("initialValue") {
            None => Ok(None),
            Some(Kv::Object(fields)) if fields.len() == 1 && fields.first().is_some_and(|(key, _)| *key == "boolValue") => d
                .get("initialValue")
                .and_then(|v| v.get("boolValue"))
                .and_then(Kv::bool)
                .map(Some)
                .ok_or_else(|| r::invalid("Boolean initialValue has a non-Boolean value")),
            Some(_) => Err(r::invalid("present Boolean initialValue is malformed")),
        }
    }

    fn bounded_name(name: &str) -> Result<(), FormatError> {
        if name.is_empty() || name.len() > r::MAX_NAME_BYTES || name.contains('\0') {
            return Err(r::invalid("resource name is invalid or oversized"));
        }
        Ok(())
    }

    fn node_id(node: &Value) -> Option<Id> {
        node.get("id").and_then(Value::as_str).and_then(Id::parse)
    }
    fn fields(node: &Value) -> BTreeSet<&str> {
        node.get("componentPropertyDefinitions")
            .and_then(Value::as_object)
            .into_iter()
            .flatten()
            .take(256)
            .filter(|(_, d)| matches!(d.get("type").and_then(Value::as_str), Some("BOOLEAN" | "TEXT" | "INSTANCE_SWAP")))
            .map(|(key, _)| key.as_str())
            .collect()
    }
    fn index<'b>(
        node: &'b Value,
        set: Option<&'b Value>,
        components: &mut BTreeMap<Id, BTreeSet<&'b str>>,
        ambiguous: &mut BTreeSet<Id>,
        visible: &mut BTreeSet<Id>,
        packets: &mut Vec<(&'b Value, &'b str)>,
        depth: usize,
    ) -> Result<(), FormatError> {
        if depth > super::MAX_DEPTH + 4 {
            return Err(r::invalid("mapped resource tree exceeds its depth limit"));
        }
        if let Some(origin) = node
            .get(crate::rest::FIG_SOURCE_FIELD)
            .and_then(|v| v.get("origin"))
            .and_then(Value::as_str)
            .and_then(Id::parse)
            .or_else(|| node_id(node))
        {
            visible.insert(origin);
        }
        let ty = node.get("type").and_then(Value::as_str);
        if ty == Some("COMPONENT")
            && let Some(id) = node_id(node)
        {
            let mut known = fields(node);
            if let Some(set) = set {
                known.extend(fields(set));
            }
            if components.insert(id, known).is_some() {
                ambiguous.insert(id);
            }
        }
        if let Some(raw) = node.get(r::FIG_PACKET_FIELD).and_then(Value::as_str) {
            packets.push((node, raw));
        }
        let next = (ty == Some("COMPONENT_SET")).then_some(node);
        for child in node.get("children").and_then(Value::as_array).into_iter().flatten() {
            index(child, next, components, ambiguous, visible, packets, depth + 1)?;
        }
        Ok(())
    }

    #[derive(Default)]
    struct Budget {
        count: usize,
        bytes: usize,
        /// Bindings and carriers left out for a limit.
        over: usize,
    }
    impl Budget {
        fn charge(&mut self, bytes: usize) -> bool {
            match self.bytes.checked_add(bytes).filter(|n| *n <= r::MAX_TOTAL_BYTES) {
                Some(total) => {
                    self.bytes = total;
                    true
                }
                None => false,
            }
        }
    }
    /// The binding `key` names, if it is retained and fits the budget (else it is counted as
    /// left out). Without `eligible`, only records the definition ids that are referenced.
    fn binding(key: &str, eligible: Option<&BTreeSet<Id>>, needed: &mut BTreeSet<Id>, budget: &mut Budget) -> Option<Binding> {
        let (_, id) = key.rsplit_once('#')?;
        let definition = Id::parse(id)?;
        let Some(eligible) = eligible else {
            if needed.len() < r::MAX_DEFINITIONS {
                needed.insert(definition);
            } else if !needed.contains(&definition) {
                budget.over += 1;
            }
            return None;
        };
        if !eligible.contains(&definition) {
            return None;
        }
        // Six bytes per input byte bounds JSON escaping before copying; reserve packet overhead too.
        let estimate = key.len().checked_mul(6).and_then(|n| n.checked_add(256));
        if bounded_name(key).is_err() || budget.count >= r::MAX_BINDINGS || !estimate.is_some_and(|n| budget.charge(n)) {
            budget.over += 1;
            return None;
        }
        budget.count += 1;
        Some(Binding { key: key.to_string(), definition })
    }
    fn collect(
        node: &Value,
        ancestor: Option<Id>,
        components: &BTreeMap<Id, BTreeSet<&str>>,
        pending: &mut Vec<Pending>,
        needed: &mut BTreeSet<Id>,
        eligible: Option<&BTreeSet<Id>>,
        budget: &mut Budget,
        depth: usize,
    ) -> Result<(), FormatError> {
        if depth > super::MAX_DEPTH + 4 {
            return Err(r::invalid("mapped resource tree exceeds its depth limit"));
        }
        let ty = node.get("type").and_then(Value::as_str);
        let own = if ty == Some("INSTANCE") {
            node.get("componentId").and_then(Value::as_str).and_then(Id::parse)
        } else if ty == Some("COMPONENT") {
            node_id(node)
        } else {
            ancestor
        };
        let mut values = Vec::new();
        if ty == Some("INSTANCE")
            && let Some(main) = own.filter(|m| components.contains_key(m))
        {
            // The REST reader keeps 256 values per instance (and warns about the rest).
            for (key, prop) in node.get("componentProperties").and_then(Value::as_object).into_iter().flatten().take(256) {
                if prop.get("type").and_then(Value::as_str) == Some("BOOLEAN")
                    && prop.get("value").is_some_and(Value::is_boolean)
                    && !components.get(&main).is_some_and(|d| d.contains(key.as_str()))
                    && let Some(b) = binding(key, eligible, needed, budget)
                {
                    values.push(b);
                }
            }
        }
        let visible_main = if ty == Some("COMPONENT") { own } else { ancestor };
        let visible = if let Some(main) = visible_main.filter(|m| components.contains_key(m)) {
            let key = node.get("componentPropertyReferences").and_then(|v| v.get("visible")).and_then(Value::as_str);
            match key {
                Some(key) if !components.get(&main).is_some_and(|d| d.contains(key)) => binding(key, eligible, needed, budget),
                _ => None,
            }
        } else {
            None
        };
        let main = if values.is_empty() { visible_main } else { own };
        if let Some(main) = main
            && (!values.is_empty() || visible.is_some())
        {
            let id = node.get("id").and_then(Value::as_str).filter(|id| id.len() <= (super::MAX_DEPTH + 2) * 24);
            let estimate = id.and_then(|id| id.len().checked_mul(6)).and_then(|n| n.checked_add(256));
            match id {
                Some(id) if pending.len() < super::MAX_NODES && estimate.is_some_and(|n| budget.charge(n)) => {
                    pending.push(Pending { node: id.to_string(), main, visible_main, values, visible });
                }
                _ => budget.over += 1,
            }
        }
        let next = if matches!(ty, Some("INSTANCE" | "COMPONENT")) { own } else { ancestor };
        for child in node.get("children").and_then(Value::as_array).into_iter().flatten() {
            collect(child, next, components, pending, needed, eligible, budget, depth + 1)?;
        }
        Ok(())
    }

    fn attach(
        node: &mut Value,
        scope: &str,
        mains: &BTreeSet<Id>,
        pending: &mut BTreeMap<String, WirePacket>,
        capture: &mut Capture<'_>,
        depth: usize,
    ) -> Result<(), FormatError> {
        if depth > super::MAX_DEPTH + 4 {
            return Err(r::invalid("mapped resource tree exceeds its depth limit"));
        }
        let id = node.get("id").and_then(Value::as_str).map(str::to_string);
        let mut packet = id.as_ref().and_then(|id| pending.remove(id));
        if node.get("type").and_then(Value::as_str) == Some("COMPONENT")
            && let Some(id) = node_id(node).filter(|id| mains.contains(id))
        {
            let p = packet.get_or_insert(WirePacket { version: r::VERSION, native: NativePacket::default(), resources: None, aliases: Vec::new() });
            p.native.main = Some(MainTag { version: r::VERSION, scope: scope.to_string(), origin: id });
        }
        if let Some(packet) = packet {
            match r::write_wire(&packet) {
                Ok(value) if capture.charge(value.len()).is_ok() => node[r::FIG_PACKET_FIELD] = Value::String(value),
                _ => capture.left_out(DROPPED_LIMIT, 1),
            }
        }
        if let Some(children) = node.get_mut("children").and_then(Value::as_array_mut) {
            for child in children {
                attach(child, scope, mains, pending, capture, depth + 1)?;
            }
        }
        Ok(())
    }
    #[cfg(test)]
    mod capture_tests {
        use super::*;
        fn gid(local: u32) -> Id {
            Id { session: 9, local }
        }
        fn g(local: u32) -> Kv<'static> {
            Kv::Object(vec![("sessionID", Kv::Uint(9)), ("localID", Kv::Uint(u64::from(local)))])
        }
        fn o(fields: Vec<(&'static str, Kv<'static>)>) -> Kv<'static> {
            Kv::Object(fields)
        }
        fn node(local: u32, kind: &'static str, parent: Option<u32>, mut fields: Vec<(&'static str, Kv<'static>)>) -> Kv<'static> {
            let mut all = vec![("guid", g(local)), ("type", Kv::Enum(kind)), ("name", Kv::Str("Synthetic".into()))];
            if let Some(parent) = parent {
                all.push(("parentIndex", o(vec![("guid", g(parent)), ("position", Kv::Str("!".into()))])));
            }
            all.append(&mut fields);
            o(all)
        }
        fn visible() -> Kv<'static> {
            o(vec![(
                "entries",
                Kv::Array(vec![o(vec![
                    ("variableField", Kv::Enum("VISIBLE")),
                    ("variableData", o(vec![("value", o(vec![("propRefValue", o(vec![("defId", g(70))]))]))])),
                ])]),
            )])
        }
        fn owner(initial: Option<Kv<'static>>, phase: &'static str) -> Kv<'static> {
            let mut def = vec![("id", g(70)), ("name", Kv::Str("Toggle".into())), ("type", Kv::Enum("BOOL"))];
            if let Some(initial) = initial {
                def.push(("initialValue", initial));
            }
            node(50, "SYMBOL", Some(3), vec![("phase", Kv::Enum(phase)), ("componentPropDefs", Kv::Array(vec![o(def)]))])
        }
        fn message(initial: Option<Kv<'static>>, clear: bool, root_ref: bool) -> Kv<'static> {
            let main = node(10, "SYMBOL", Some(2), if root_ref { vec![("parameterConsumptionMap", visible())] } else { vec![] });
            let mut sd = vec![("symbolID", g(10))];
            if clear {
                sd.push((
                    "symbolOverrides",
                    Kv::Array(vec![o(vec![
                        ("guidPath", o(vec![("guids", Kv::Array(vec![g(12)]))])),
                        ("parameterConsumptionMap", o(vec![("entries", Kv::Array(vec![]))])),
                    ])]),
                ));
            }
            let used = node(
                11,
                "INSTANCE",
                Some(2),
                vec![
                    ("symbolData", o(sd)),
                    ("componentPropAssignments", Kv::Array(vec![o(vec![("defID", g(70)), ("value", o(vec![("boolValue", Kv::Bool(false))]))])])),
                ],
            );
            o(vec![(
                "nodeChanges",
                Kv::Array(vec![
                    node(1, "DOCUMENT", None, vec![]),
                    node(2, "CANVAS", Some(1), vec![]),
                    node(3, "CANVAS", Some(1), vec![("internalOnly", Kv::Bool(true))]),
                    main,
                    used,
                    node(12, "FRAME", Some(10), vec![("parameterConsumptionMap", visible())]),
                    owner(initial, "CREATED"),
                ]),
            )])
        }
        fn mapped(msg: &Kv<'_>) -> Value {
            super::super::to_rest(msg, "Synthetic resources").unwrap().0
        }
        fn catalog(v: &Value) -> Catalog {
            r::read_catalog(v[r::FIG_CATALOGS_FIELD][0].as_str().unwrap()).unwrap()
        }
        fn named_id<'b>(v: &'b Value, id: &str) -> Option<&'b Value> {
            if v.get("id").and_then(Value::as_str) == Some(id) {
                return Some(v);
            }
            v.get("children").and_then(Value::as_array).into_iter().flatten().find_map(|v| named_id(v, id))
        }
        fn changes<'a>(msg: &'a mut Kv<'static>) -> &'a mut Vec<Kv<'static>> {
            let Kv::Object(fields) = msg else { panic!() };
            let (_, Kv::Array(rows)) = fields.iter_mut().find(|(key, _)| *key == "nodeChanges").unwrap() else { panic!() };
            rows
        }
        /// The private records are left out with a warning; the file still opens.
        fn left_out(warnings: &[String], kind: &str) -> bool {
            warnings.iter().any(|w| w.ends_with(kind))
        }
        #[test]
        fn absent_default_is_retained_but_present_malformed_defaults_are_not() {
            let v = mapped(&message(None, false, false));
            assert_eq!(catalog(&v).owners[0].definitions[0].initial, None);
            for bad in [
                o(vec![]),
                o(vec![("boolValue", Kv::Str("false".into()))]),
                o(vec![("boolValue", Kv::Bool(false)), ("textValue", Kv::Str("other".into()))]),
            ] {
                // A malformed present default is neither retained nor guessed as absent.
                let (v, warnings, _) = super::super::to_rest(&message(Some(bad), false, false), "Synthetic").unwrap();
                assert!(v.get(r::FIG_CATALOGS_FIELD).is_none(), "a malformed default must not be retained");
                assert!(left_out(&warnings, DROPPED_DEFINITION), "{warnings:?}");
            }
        }
        #[test]
        fn current_owner_snapshot_accepts_repeated_and_updated_history() {
            let mut msg = message(Some(o(vec![("boolValue", Kv::Bool(false))])), false, false);
            let first = catalog(&mapped(&msg));
            changes(&mut msg).push(owner(Some(o(vec![("boolValue", Kv::Bool(false))])), "CREATED"));
            assert_eq!(catalog(&mapped(&msg)), first);
            changes(&mut msg).push(owner(Some(o(vec![("boolValue", Kv::Bool(true))])), "UPDATED"));
            assert_eq!(catalog(&mapped(&msg)).owners[0].definitions[0].initial, Some(true));
        }
        #[test]
        fn removed_owner_is_not_retained_as_the_catalog() {
            let mut msg = message(Some(o(vec![("boolValue", Kv::Bool(false))])), false, false);
            // The owner is removed and the definition moves to another hidden owner: only the
            // live one may anchor the catalog, as in the mapper (last change wins; REMOVED deletes).
            changes(&mut msg).push(owner(None, "REMOVED"));
            let mut moved = owner(Some(o(vec![("boolValue", Kv::Bool(true))])), "CREATED");
            let Kv::Object(fields) = &mut moved else { panic!() };
            fields.iter_mut().find(|(key, _)| *key == "guid").unwrap().1 = g(51);
            changes(&mut msg).push(moved);
            let current = catalog(&mapped(&msg));
            assert_eq!(current.owners.len(), 1, "{current:?}");
            assert_eq!(current.owners[0].id, gid(51));
            assert_eq!(current.owners[0].definitions[0].initial, Some(true));
        }
        #[test]
        fn inconsistent_definitions_in_the_final_owner_are_not_retained() {
            let mut msg = message(None, false, false);
            let current = changes(&mut msg).last_mut().unwrap();
            let Kv::Object(fields) = current else { panic!() };
            let (_, Kv::Array(defs)) = fields.iter_mut().find(|(key, _)| *key == "componentPropDefs").unwrap() else { panic!() };
            defs.push(defs[0].clone());
            let (v, warnings, _) = super::super::to_rest(&msg, "Synthetic").unwrap();
            assert!(v.get(r::FIG_CATALOGS_FIELD).is_none(), "an ambiguous declaration must not be retained");
            assert!(named_id(&v["document"], "9:11").unwrap().get(r::FIG_PACKET_FIELD).is_none());
            assert!(left_out(&warnings, DROPPED_DEFINITION), "{warnings:?}");
        }
        #[test]
        fn expanded_visibility_inherits_and_explicit_empty_entries_clear_it() {
            let kept = mapped(&message(None, false, false));
            let child = named_id(&kept["document"], "I9:11;9:12").unwrap();
            let packet = r::read_wire(child[r::FIG_PACKET_FIELD].as_str().unwrap()).unwrap();
            assert_eq!(packet.native.bindings.unwrap().visible.unwrap().definition, gid(70));
            let cleared = mapped(&message(None, true, false));
            let child = named_id(&cleared["document"], "I9:11;9:12").unwrap();
            assert!(child.get(r::FIG_PACKET_FIELD).is_none());
            assert!(child.get("componentPropertyReferences").is_none());
        }
        #[test]
        fn component_root_visibility_tags_its_actual_self_main() {
            let v = mapped(&message(None, false, true));
            let main = named_id(&v["document"], "9:10").unwrap();
            let packet = r::read_wire(main[r::FIG_PACKET_FIELD].as_str().unwrap()).unwrap();
            assert_eq!(packet.native.main.unwrap().origin, gid(10));
            assert_eq!(packet.native.bindings.unwrap().main, gid(10));
        }
        #[test]
        fn nested_value_and_visibility_keep_their_distinct_actual_mains() {
            let mut msg = message(None, false, false);
            changes(&mut msg).push(node(20, "SYMBOL", Some(2), vec![]));
            changes(&mut msg).push(node(
                21,
                "INSTANCE",
                Some(10),
                vec![
                    ("symbolData", o(vec![("symbolID", g(20))])),
                    ("componentPropAssignments", Kv::Array(vec![o(vec![("defID", g(70)), ("value", o(vec![("boolValue", Kv::Bool(false))]))])])),
                    ("parameterConsumptionMap", visible()),
                ],
            ));
            let value = mapped(&msg);
            for id in ["9:21", "I9:11;9:21"] {
                let node = named_id(&value["document"], id).unwrap();
                let packet = r::read_wire(node[r::FIG_PACKET_FIELD].as_str().unwrap()).unwrap();
                let bindings = packet.native.bindings.unwrap();
                assert_eq!(bindings.main, gid(20));
                assert_eq!(bindings.visible_main, Some(gid(10)));
                assert_eq!(bindings.values[0].definition, gid(70));
                assert_eq!(bindings.visible.unwrap().definition, gid(70));
            }
            let (doc, _) = crate::rest::import_fig(&value, &|_| None, Vec::new()).unwrap();
            let dual: Vec<_> = doc
                .nodes()
                .filter_map(|n| n.plugin_data.get(r::BINDINGS_KEY))
                .map(|s| r::read_bindings(s).unwrap())
                .filter(|b| b.visible_main.is_some())
                .collect();
            assert_eq!(dual.len(), 2);
            assert!(dual.iter().all(|b| b.main == gid(20) && b.visible_main == Some(gid(10))));
        }
        #[test]
        fn retained_binding_budget_is_checked_before_copying_keys() {
            let eligible = BTreeSet::from([gid(70)]);
            let mut needed = BTreeSet::new();
            let mut budget = Budget { count: r::MAX_BINDINGS, bytes: 0, over: 0 };
            assert!(binding("Toggle#9:70", Some(&eligible), &mut needed, &mut budget).is_none());
            assert_eq!((budget.count, budget.bytes, budget.over), (r::MAX_BINDINGS, 0, 1), "an over-limit binding is counted, not copied");
            let mut budget = Budget { count: 0, bytes: r::MAX_TOTAL_BYTES, over: 0 };
            assert!(binding("Toggle#9:70", Some(&eligible), &mut needed, &mut budget).is_none());
            assert_eq!((budget.count, budget.bytes, budget.over), (0, r::MAX_TOTAL_BYTES, 1));
            let mut budget = Budget::default();
            assert!(binding("Unused#9:71", Some(&eligible), &mut needed, &mut budget).is_none());
            assert_eq!((budget.count, budget.bytes, budget.over), (0, 0, 0));
            assert!(binding("Toggle#9:70", Some(&eligible), &mut needed, &mut budget).is_some());
            assert_eq!(budget.count, 1);
        }
        #[test]
        fn missing_alias_for_an_active_standard_resource_field_drops_that_field() {
            let value = mapped(&message(None, false, false));
            let (doc, _) = crate::rest::import_fig(&value, &|_| None, Vec::new()).unwrap();
            let (bytes, _) = crate::fig::export(&doc, None).unwrap();
            let archive = crate::fig::Archive::open(&bytes).unwrap();
            let (schema, bytes) = crate::fig::decode_canvas(&archive.canvas).unwrap();
            let root = schema.find("Message").unwrap();
            let mut decoded = schema.decode_message(root, &bytes).unwrap();
            let Kv::Object(fields) = &mut decoded else { panic!() };
            let (_, Kv::Array(nodes)) = fields.iter_mut().find(|(key, _)| *key == "nodeChanges").unwrap() else { panic!() };
            let mut changed = 0;
            for node in nodes {
                let Kv::Object(fields) = node else { continue };
                let Some((_, Kv::Array(plugins))) = fields.iter_mut().find(|(key, _)| *key == "pluginData") else { continue };
                for plugin in plugins {
                    if plugin.get("pluginID").and_then(Kv::str) != Some(r::PLUGIN_ID) || plugin.get("key").and_then(Kv::str) != Some(r::PLUGIN_KEY) {
                        continue;
                    }
                    let Kv::Object(fields) = plugin else { continue };
                    let (_, Kv::Str(value)) = fields.iter_mut().find(|(key, _)| *key == "value").unwrap() else { panic!() };
                    let mut packet = r::read_wire(value).unwrap();
                    if packet.native.bindings.as_ref().is_some_and(|b| !b.values.is_empty()) {
                        packet.aliases.clear();
                        *value = r::write_wire(&packet).unwrap();
                        changed += 1;
                    }
                }
            }
            assert!(changed > 0, "a real written packet must have been mutated");
            let (v, warnings, _) = super::super::to_rest(&decoded, "Synthetic missing alias").unwrap();
            assert!(left_out(&warnings, DROPPED_UNBOUND), "{warnings:?}");
            // The value named a hidden resource definition by its wire id only: without the alias it
            // is left out rather than imported under a meaningless name.
            fn bool_props(v: &Value, out: &mut usize) {
                for p in v.get("componentProperties").and_then(Value::as_object).into_iter().flatten().map(|(_, p)| p) {
                    if p.get("type").and_then(Value::as_str) == Some("BOOLEAN") {
                        *out += 1;
                    }
                }
                v.get("children").and_then(Value::as_array).into_iter().flatten().for_each(|c| bool_props(c, out));
            }
            let mut count = 0;
            bool_props(&v["document"], &mut count);
            assert_eq!(count, 0);
            let (doc, _) = crate::rest::import_fig(&v, &|_| None, Vec::new()).unwrap();
            assert!(doc.nodes().all(|n| !matches!(&n.kind, figcraft_doc::NodeKind::Instance { props, .. } if !props.is_empty())));
        }
    }
}
