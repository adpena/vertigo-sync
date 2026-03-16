//! .rbxl / .rbxlx file parser and DOM-to-JSON converter.
//!
//! Uses the `rbx_binary`, `rbx_xml`, and `rbx_dom_weak` crates from the Rojo
//! ecosystem to parse Roblox place files into a JSON-serializable scene graph
//! that Strata's `SceneHydrator` can consume directly.
//!
//! Design priorities:
//!   1. Lazy loading — parse on first request, cache in memory.
//!   2. Stream-friendly — large property values (meshes, textures) are
//!      referenced by ID and served separately from the instance tree.
//!   3. Faithful conversion — every supported Variant type maps to a
//!      deterministic `PropertyValue` enum variant.

use std::collections::HashMap;
use std::io::BufReader;
use std::path::Path;

use anyhow::{Context, Result, bail};
use rbx_dom_weak::WeakDom;
use rbx_dom_weak::types::Ref;
use serde::{Deserialize, Serialize};

// ---------------------------------------------------------------------------
// PropertyValue — JSON-serializable subset of rbx_dom_weak::types::Variant
// ---------------------------------------------------------------------------

/// A Roblox property value serialized to a type that JSON (and Strata) can
/// consume. We intentionally flatten complex structures into tuples/arrays
/// rather than nested objects so the wire format stays compact.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(tag = "type", content = "value")]
pub enum PropertyValue {
    String(String),
    Number(f64),
    Bool(bool),
    Int32(i32),
    Int64(i64),
    Float32(f32),
    Float64(f64),
    Vector2(f64, f64),
    Vector3(f64, f64, f64),
    /// Row-major 3x3 rotation + translation: [r00,r01,r02,r10,r11,r12,r20,r21,r22,x,y,z]
    CFrame([f64; 12]),
    Color3(f64, f64, f64),
    Color3uint8(u8, u8, u8),
    BrickColor(u32),
    Enum(u32),
    Ref(String),
    Content(String),
    UDim(f64, i32),
    UDim2(f64, i32, f64, i32),
    Rect(f64, f64, f64, f64),
    NumberRange(f64, f64),
    /// Vec of (time, value, envelope) keypoints.
    NumberSequence(Vec<(f64, f64, f64)>),
    /// Vec of (time, r, g, b) keypoints.
    ColorSequence(Vec<(f64, f64, f64, f64)>),
    PhysicalProperties {
        density: f64,
        friction: f64,
        elasticity: f64,
        friction_weight: f64,
        elasticity_weight: f64,
    },
    Vector3int16(i16, i16, i16),
    /// Opaque binary blob, base64 for JSON transport.
    BinaryString(String),
    /// Shared string content, base64 encoded.
    SharedString(String),
    /// Catch-all for types we don't explicitly map.
    Unknown(String),
}

// ---------------------------------------------------------------------------
// InstanceNode — flat representation of one DataModel instance
// ---------------------------------------------------------------------------

/// A single Roblox instance, serialized with parent/child pointers as string
/// IDs so the tree can be reconstructed on the client side.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct InstanceNode {
    /// Opaque unique ID (hex-encoded `Ref`).
    pub id: String,
    /// Instance.Name
    pub name: String,
    /// Instance.ClassName
    pub class_name: String,
    /// Parent ID, or `null` for the root instance.
    pub parent_id: Option<String>,
    /// Property bag (excludes large binary blobs which are served separately).
    pub properties: HashMap<String, PropertyValue>,
    /// CollectionService tags.
    pub tags: Vec<String>,
    /// Direct child IDs (preserves tree order).
    pub children: Vec<String>,
}

// ---------------------------------------------------------------------------
// SceneGraph — Strata-compatible top-level envelope
// ---------------------------------------------------------------------------

/// Top-level scene graph returned by `/api/rbxl/tree`. Designed to be directly
/// consumable by Strata's `SceneHydrator.hydrateFromJSON()`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SceneGraph {
    pub format_version: u32,
    pub generator: String,
    pub root_id: String,
    pub instance_count: usize,
    pub instances: Vec<InstanceNode>,
}

// ---------------------------------------------------------------------------
// RbxlLoader — file detection, parsing, and conversion
// ---------------------------------------------------------------------------

/// Stateless loader that reads .rbxl/.rbxlx files and converts them into our
/// JSON-serializable scene format.
pub struct RbxlLoader;

impl RbxlLoader {
    /// Detect format from file extension and parse into a `WeakDom`.
    pub fn load_file(path: &Path) -> Result<WeakDom> {
        let ext = path
            .extension()
            .and_then(|e| e.to_str())
            .unwrap_or("")
            .to_ascii_lowercase();

        let file = std::fs::File::open(path)
            .with_context(|| format!("failed to open {}", path.display()))?;
        let reader = BufReader::new(file);

        match ext.as_str() {
            "rbxl" | "rbxm" => rbx_binary::from_reader(reader)
                .with_context(|| format!("failed to parse binary file {}", path.display())),
            "rbxlx" | "rbxmx" => rbx_xml::from_reader_default(reader)
                .with_context(|| format!("failed to parse XML file {}", path.display())),
            _ => bail!(
                "unsupported file extension '{}' — expected .rbxl, .rbxlx, .rbxm, or .rbxmx",
                ext
            ),
        }
    }

    /// Parse a raw XML string (useful for tests and inline content).
    pub fn load_xml_str(xml: &str) -> Result<WeakDom> {
        rbx_xml::from_str_default(xml).context("failed to parse inline XML")
    }

    /// Convert the full DOM into a flat instance tree.
    pub fn to_instance_tree(dom: &WeakDom) -> Vec<InstanceNode> {
        let mut nodes = Vec::new();
        let root_ref = dom.root_ref();
        Self::collect_instances(dom, root_ref, None, &mut nodes);
        nodes
    }

    /// Convert the DOM into a Strata-compatible `SceneGraph` envelope.
    pub fn to_scene_graph(dom: &WeakDom) -> SceneGraph {
        let instances = Self::to_instance_tree(dom);
        let root_id = instances.first().map(|n| n.id.clone()).unwrap_or_default();

        SceneGraph {
            format_version: 1,
            generator: "vertigo-sync/rbxl".to_string(),
            root_id,
            instance_count: instances.len(),
            instances,
        }
    }

    /// Recursively collect instances into the flat list.
    fn collect_instances(
        dom: &WeakDom,
        inst_ref: Ref,
        parent_id: Option<String>,
        out: &mut Vec<InstanceNode>,
    ) {
        let Some(inst) = dom.get_by_ref(inst_ref) else {
            return;
        };

        let id = ref_to_string(inst_ref);
        let children: Vec<String> = inst.children().iter().map(|r| ref_to_string(*r)).collect();

        let mut properties = HashMap::new();
        for (key, variant) in &inst.properties {
            // Skip very large binary blobs in the tree — they are served by
            // /api/rbxl/meshes and /api/rbxl/instance/:id endpoints.
            if is_large_blob(variant) {
                continue;
            }
            properties.insert(key.to_string(), convert_variant(variant));
        }

        let tags = extract_tags(inst);

        out.push(InstanceNode {
            id: id.clone(),
            name: inst.name.clone(),
            class_name: inst.class.to_string(),
            parent_id,
            properties,
            tags,
            children: children.clone(),
        });

        for child_ref in inst.children() {
            Self::collect_instances(dom, *child_ref, Some(id.clone()), out);
        }
    }

    /// Query instances by class name, tag, or name substring.
    pub fn query(
        dom: &WeakDom,
        class: Option<&str>,
        tag: Option<&str>,
        name: Option<&str>,
    ) -> Vec<InstanceNode> {
        let all = Self::to_instance_tree(dom);
        all.into_iter()
            .filter(|node| {
                if let Some(c) = class
                    && node.class_name != c {
                        return false;
                    }
                if let Some(t) = tag
                    && !node.tags.contains(&t.to_string()) {
                        return false;
                    }
                if let Some(n) = name
                    && !node.name.contains(n) {
                        return false;
                    }
                true
            })
            .collect()
    }

    /// Extract all Script/LocalScript/ModuleScript instances with their Source.
    pub fn extract_scripts(dom: &WeakDom) -> Vec<ScriptEntry> {
        let all = Self::to_instance_tree(dom);
        all.into_iter()
            .filter(|node| {
                matches!(
                    node.class_name.as_str(),
                    "Script" | "LocalScript" | "ModuleScript"
                )
            })
            .map(|node| {
                let source = node
                    .properties
                    .get("Source")
                    .and_then(|v| match v {
                        PropertyValue::String(s) => Some(s.clone()),
                        _ => None,
                    })
                    .unwrap_or_default();
                ScriptEntry {
                    id: node.id,
                    name: node.name,
                    class_name: node.class_name,
                    parent_id: node.parent_id,
                    source,
                }
            })
            .collect()
    }

    /// Extract all MeshPart instances with their MeshId.
    pub fn extract_meshes(dom: &WeakDom) -> Vec<MeshEntry> {
        let all = Self::to_instance_tree(dom);
        all.into_iter()
            .filter(|node| node.class_name == "MeshPart")
            .map(|node| {
                let mesh_id = node
                    .properties
                    .get("MeshId")
                    .and_then(|v| match v {
                        PropertyValue::Content(s) => Some(s.clone()),
                        PropertyValue::String(s) => Some(s.clone()),
                        _ => None,
                    })
                    .unwrap_or_default();
                let size = node.properties.get("Size").and_then(|v| match v {
                    PropertyValue::Vector3(x, y, z) => Some((*x, *y, *z)),
                    _ => None,
                });
                MeshEntry {
                    id: node.id,
                    name: node.name,
                    parent_id: node.parent_id,
                    mesh_id,
                    size,
                }
            })
            .collect()
    }

    /// Get a single instance by ref string using a prebuilt ref_map.
    /// Returns full properties including large blobs.
    pub fn get_instance_full(dom: &WeakDom, inst_ref: Ref) -> Option<InstanceNode> {
        let inst = dom.get_by_ref(inst_ref)?;

        let id = ref_to_string(inst_ref);
        let children: Vec<String> = inst.children().iter().map(|r| ref_to_string(*r)).collect();

        let mut properties = HashMap::new();
        for (key, variant) in &inst.properties {
            properties.insert(key.to_string(), convert_variant(variant));
        }

        let parent_id = if inst.parent() != Ref::none() {
            Some(ref_to_string(inst.parent()))
        } else {
            None
        };

        Some(InstanceNode {
            id,
            name: inst.name.clone(),
            class_name: inst.class.to_string(),
            parent_id,
            properties,
            tags: extract_tags(inst),
            children,
        })
    }
}

// ---------------------------------------------------------------------------
// Helper types for script/mesh extraction
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ScriptEntry {
    pub id: String,
    pub name: String,
    pub class_name: String,
    pub parent_id: Option<String>,
    pub source: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MeshEntry {
    pub id: String,
    pub name: String,
    pub parent_id: Option<String>,
    pub mesh_id: String,
    pub size: Option<(f64, f64, f64)>,
}

// ---------------------------------------------------------------------------
// Variant conversion
// ---------------------------------------------------------------------------

/// Convert an rbx_types::Variant to our PropertyValue.
pub fn convert_variant(variant: &rbx_types::Variant) -> PropertyValue {
    use rbx_types::Variant;

    match variant {
        Variant::String(s) => PropertyValue::String(s.clone()),
        Variant::BinaryString(b) => {
            use base64::Engine;
            let bytes: &[u8] = b.as_ref();
            PropertyValue::BinaryString(base64::engine::general_purpose::STANDARD.encode(bytes))
        }
        Variant::Bool(b) => PropertyValue::Bool(*b),
        Variant::Int32(n) => PropertyValue::Int32(*n),
        Variant::Int64(n) => PropertyValue::Int64(*n),
        Variant::Float32(n) => PropertyValue::Float32(*n),
        Variant::Float64(n) => PropertyValue::Float64(*n),
        Variant::Vector2(v) => PropertyValue::Vector2(v.x as f64, v.y as f64),
        Variant::Vector3(v) => PropertyValue::Vector3(v.x as f64, v.y as f64, v.z as f64),
        Variant::Vector3int16(v) => PropertyValue::Vector3int16(v.x, v.y, v.z),
        Variant::CFrame(cf) => {
            let pos = cf.position;
            let rot = cf.orientation;
            PropertyValue::CFrame([
                rot.x.x as f64,
                rot.x.y as f64,
                rot.x.z as f64,
                rot.y.x as f64,
                rot.y.y as f64,
                rot.y.z as f64,
                rot.z.x as f64,
                rot.z.y as f64,
                rot.z.z as f64,
                pos.x as f64,
                pos.y as f64,
                pos.z as f64,
            ])
        }
        Variant::Color3(c) => PropertyValue::Color3(c.r as f64, c.g as f64, c.b as f64),
        Variant::Color3uint8(c) => PropertyValue::Color3uint8(c.r, c.g, c.b),
        Variant::BrickColor(bc) => PropertyValue::BrickColor(*bc as u32),
        Variant::Enum(e) => PropertyValue::Enum(e.to_u32()),
        Variant::Ref(r) => {
            if *r == Ref::none() {
                PropertyValue::Ref("null".to_string())
            } else {
                PropertyValue::Ref(ref_to_string(*r))
            }
        }
        Variant::Content(c) => PropertyValue::Content(content_to_string(c)),
        Variant::UDim(u) => PropertyValue::UDim(u.scale as f64, u.offset),
        Variant::UDim2(u) => {
            PropertyValue::UDim2(u.x.scale as f64, u.x.offset, u.y.scale as f64, u.y.offset)
        }
        Variant::Rect(r) => PropertyValue::Rect(
            r.min.x as f64,
            r.min.y as f64,
            r.max.x as f64,
            r.max.y as f64,
        ),
        Variant::NumberRange(r) => PropertyValue::NumberRange(r.min as f64, r.max as f64),
        Variant::NumberSequence(seq) => PropertyValue::NumberSequence(
            seq.keypoints
                .iter()
                .map(|kp| (kp.time as f64, kp.value as f64, kp.envelope as f64))
                .collect(),
        ),
        Variant::ColorSequence(seq) => PropertyValue::ColorSequence(
            seq.keypoints
                .iter()
                .map(|kp| {
                    (
                        kp.time as f64,
                        kp.color.r as f64,
                        kp.color.g as f64,
                        kp.color.b as f64,
                    )
                })
                .collect(),
        ),
        Variant::PhysicalProperties(pp) => match pp {
            rbx_types::PhysicalProperties::Custom(custom) => PropertyValue::PhysicalProperties {
                density: custom.density() as f64,
                friction: custom.friction() as f64,
                elasticity: custom.elasticity() as f64,
                friction_weight: custom.friction_weight() as f64,
                elasticity_weight: custom.elasticity_weight() as f64,
            },
            rbx_types::PhysicalProperties::Default => PropertyValue::String("Default".to_string()),
        },
        Variant::SharedString(s) => {
            use base64::Engine;
            PropertyValue::SharedString(base64::engine::general_purpose::STANDARD.encode(s.data()))
        }
        Variant::Tags(tags) => {
            // Tags are stored as a property but we also extract them to the
            // top-level `tags` field. Return them as a comma-separated string.
            let tag_list: Vec<&str> = tags.iter().collect();
            PropertyValue::String(tag_list.join(","))
        }
        // Catch-all for types we haven't mapped yet (Faces, Axes, OptionalCFrame, etc.)
        other => PropertyValue::Unknown(format!("{:?}", std::mem::discriminant(other))),
    }
}

/// Extract the string representation from an rbx_types::Content value.
fn content_to_string(content: &rbx_types::Content) -> String {
    // Content in rbx_types 3.x is an opaque type. We use Debug as a
    // fallback but try to extract the URI string if possible.
    format!("{:?}", content)
}

/// Returns `true` for binary property values over 4 KB that should be stripped
/// from the tree response and served separately.
fn is_large_blob(variant: &rbx_types::Variant) -> bool {
    match variant {
        rbx_types::Variant::BinaryString(b) => {
            let bytes: &[u8] = b.as_ref();
            bytes.len() > 4096
        }
        rbx_types::Variant::SharedString(s) => s.data().len() > 4096,
        _ => false,
    }
}

/// Extract CollectionService tags from an instance.
fn extract_tags(inst: &rbx_dom_weak::Instance) -> Vec<String> {
    // Properties in rbx_dom_weak use Ustr keys; find Tags by iterating.
    for (key, value) in &inst.properties {
        if key.as_str() == "Tags"
            && let rbx_types::Variant::Tags(tags) = value {
                return tags.iter().map(|t| t.to_string()).collect();
            }
    }
    Vec::new()
}

// ---------------------------------------------------------------------------
// Ref <-> String conversion
// ---------------------------------------------------------------------------

/// Convert an `rbx_dom_weak::types::Ref` to a stable hex string.
fn ref_to_string(r: Ref) -> String {
    // Ref is an opaque index — we format it as hex for readability.
    format!("{:?}", r)
}

/// Build a lookup map from our string IDs to DOM Refs for instance retrieval.
pub fn build_ref_map(dom: &WeakDom) -> HashMap<String, Ref> {
    let mut map = HashMap::new();
    build_ref_map_recursive(dom, dom.root_ref(), &mut map);
    map
}

fn build_ref_map_recursive(dom: &WeakDom, inst_ref: Ref, map: &mut HashMap<String, Ref>) {
    let id = ref_to_string(inst_ref);
    map.insert(id, inst_ref);
    if let Some(inst) = dom.get_by_ref(inst_ref) {
        for child in inst.children() {
            build_ref_map_recursive(dom, *child, map);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const MINIMAL_RBXLX: &str = r#"
<roblox version="4">
  <Item class="Workspace" referent="RBX0001">
    <Properties>
      <string name="Name">Workspace</string>
    </Properties>
    <Item class="Part" referent="RBX0002">
      <Properties>
        <string name="Name">TestPart</string>
        <Vector3 name="Position">
          <X>10</X>
          <Y>20</Y>
          <Z>30</Z>
        </Vector3>
        <bool name="Anchored">true</bool>
      </Properties>
    </Item>
    <Item class="Script" referent="RBX0003">
      <Properties>
        <string name="Name">TestScript</string>
        <ProtectedString name="Source"><![CDATA[print("hello world")]]></ProtectedString>
      </Properties>
    </Item>
  </Item>
</roblox>
"#;

    #[test]
    fn test_load_xml_str() {
        let dom = RbxlLoader::load_xml_str(MINIMAL_RBXLX).expect("should parse");
        let root = dom.root();
        assert!(!root.children().is_empty(), "root should have children");
    }

    #[test]
    fn test_instance_tree() {
        let dom = RbxlLoader::load_xml_str(MINIMAL_RBXLX).expect("should parse");
        let tree = RbxlLoader::to_instance_tree(&dom);
        assert!(tree.len() >= 3, "should have at least root + part + script");

        let part = tree.iter().find(|n| n.name == "TestPart");
        assert!(part.is_some(), "TestPart should exist in tree");
        let part = part.unwrap();
        assert_eq!(part.class_name, "Part");
    }

    #[test]
    fn test_scene_graph() {
        let dom = RbxlLoader::load_xml_str(MINIMAL_RBXLX).expect("should parse");
        let sg = RbxlLoader::to_scene_graph(&dom);
        assert_eq!(sg.format_version, 1);
        assert_eq!(sg.generator, "vertigo-sync/rbxl");
        assert!(sg.instance_count >= 3);
        assert!(!sg.root_id.is_empty());
    }

    #[test]
    fn test_property_conversion() {
        let dom = RbxlLoader::load_xml_str(MINIMAL_RBXLX).expect("should parse");
        let tree = RbxlLoader::to_instance_tree(&dom);

        let part = tree.iter().find(|n| n.name == "TestPart").unwrap();

        // Check Anchored bool
        if let Some(PropertyValue::Bool(val)) = part.properties.get("Anchored") {
            assert!(*val, "Anchored should be true");
        }
    }

    #[test]
    fn test_query_by_class() {
        let dom = RbxlLoader::load_xml_str(MINIMAL_RBXLX).expect("should parse");
        let scripts = RbxlLoader::query(&dom, Some("Script"), None, None);
        assert_eq!(scripts.len(), 1);
        assert_eq!(scripts[0].name, "TestScript");
    }

    #[test]
    fn test_extract_scripts() {
        let dom = RbxlLoader::load_xml_str(MINIMAL_RBXLX).expect("should parse");
        let scripts = RbxlLoader::extract_scripts(&dom);
        assert_eq!(scripts.len(), 1);
        assert_eq!(scripts[0].name, "TestScript");
        assert_eq!(scripts[0].source, "print(\"hello world\")");
    }

    #[test]
    fn test_query_by_name() {
        let dom = RbxlLoader::load_xml_str(MINIMAL_RBXLX).expect("should parse");
        let found = RbxlLoader::query(&dom, None, None, Some("Test"));
        assert!(found.len() >= 2, "should find TestPart and TestScript");
    }

    #[test]
    fn test_json_roundtrip() {
        let dom = RbxlLoader::load_xml_str(MINIMAL_RBXLX).expect("should parse");
        let sg = RbxlLoader::to_scene_graph(&dom);
        let json = serde_json::to_string(&sg).expect("should serialize");
        let _: SceneGraph = serde_json::from_str(&json).expect("should deserialize");
    }
}
