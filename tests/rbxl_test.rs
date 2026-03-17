//! Integration tests for the .rbxl / .rbxlx parser.

use std::io::Write;
use tempfile::NamedTempFile;
use vertigo_sync::rbxl::{PropertyValue, RbxlLoader, SceneGraph};

/// Minimal .rbxlx file with a Workspace containing a Part and a Script.
const TEST_RBXLX: &str = r#"
<roblox version="4">
  <Item class="Workspace" referent="WS001">
    <Properties>
      <string name="Name">Workspace</string>
    </Properties>
    <Item class="Part" referent="PART001">
      <Properties>
        <string name="Name">TestPart</string>
        <Vector3 name="Position">
          <X>10</X>
          <Y>20</Y>
          <Z>30</Z>
        </Vector3>
        <Vector3 name="Size">
          <X>4</X>
          <Y>1</Y>
          <Z>2</Z>
        </Vector3>
        <bool name="Anchored">true</bool>
        <token name="Material">256</token>
      </Properties>
    </Item>
    <Item class="ModuleScript" referent="MS001">
      <Properties>
        <string name="Name">TestModule</string>
        <ProtectedString name="Source"><![CDATA[
local M = {}
function M:Init()
    print("hello from module")
end
return M
]]></ProtectedString>
      </Properties>
    </Item>
    <Item class="Script" referent="SCR001">
      <Properties>
        <string name="Name">ServerScript</string>
        <ProtectedString name="Source"><![CDATA[print("server start")]]></ProtectedString>
      </Properties>
    </Item>
    <Item class="Folder" referent="FOLD001">
      <Properties>
        <string name="Name">Enemies</string>
      </Properties>
      <Item class="Part" referent="PART002">
        <Properties>
          <string name="Name">EnemySpawn</string>
          <bool name="Anchored">false</bool>
        </Properties>
      </Item>
    </Item>
  </Item>
</roblox>
"#;

#[test]
fn test_load_rbxlx_from_file() {
    let mut file = NamedTempFile::with_suffix(".rbxlx").unwrap();
    file.write_all(TEST_RBXLX.as_bytes()).unwrap();
    file.flush().unwrap();

    let dom = RbxlLoader::load_file(file.path()).expect("should parse .rbxlx file");
    let root = dom.root();
    assert!(!root.children().is_empty(), "root should have children");
}

#[test]
fn test_load_xml_str() {
    let dom = RbxlLoader::load_xml_str(TEST_RBXLX).expect("should parse inline XML");
    let tree = RbxlLoader::to_instance_tree(&dom);
    assert!(
        tree.len() >= 5,
        "should have root + workspace + part + scripts + folder children"
    );
}

#[test]
fn test_instance_tree_structure() {
    let dom = RbxlLoader::load_xml_str(TEST_RBXLX).unwrap();
    let tree = RbxlLoader::to_instance_tree(&dom);

    // Verify we can find all named instances.
    let names: Vec<&str> = tree.iter().map(|n| n.name.as_str()).collect();
    assert!(names.contains(&"Workspace"), "Workspace missing");
    assert!(names.contains(&"TestPart"), "TestPart missing");
    assert!(names.contains(&"TestModule"), "TestModule missing");
    assert!(names.contains(&"ServerScript"), "ServerScript missing");
    assert!(names.contains(&"Enemies"), "Enemies folder missing");
    assert!(names.contains(&"EnemySpawn"), "EnemySpawn missing");
}

#[test]
fn test_parent_child_relationships() {
    let dom = RbxlLoader::load_xml_str(TEST_RBXLX).unwrap();
    let tree = RbxlLoader::to_instance_tree(&dom);

    let workspace = tree.iter().find(|n| n.name == "Workspace").unwrap();
    let test_part = tree.iter().find(|n| n.name == "TestPart").unwrap();

    // TestPart's parent should be Workspace.
    assert_eq!(
        test_part.parent_id.as_deref(),
        Some(workspace.id.as_str()),
        "TestPart parent should be Workspace"
    );

    // Workspace should list TestPart as a child.
    assert!(
        workspace.children.contains(&test_part.id),
        "Workspace children should include TestPart"
    );
}

#[test]
fn test_scene_graph_envelope() {
    let dom = RbxlLoader::load_xml_str(TEST_RBXLX).unwrap();
    let sg = RbxlLoader::to_scene_graph(&dom);

    assert_eq!(sg.format_version, 1);
    assert_eq!(sg.generator, "vertigo-sync/rbxl");
    assert!(sg.instance_count >= 5);
    assert!(!sg.root_id.is_empty());

    // Scene graph should be JSON-serializable.
    let json = serde_json::to_string_pretty(&sg).expect("should serialize");
    let parsed: SceneGraph = serde_json::from_str(&json).expect("should deserialize");
    assert_eq!(parsed.instance_count, sg.instance_count);
}

#[test]
fn test_property_conversion_bool() {
    let dom = RbxlLoader::load_xml_str(TEST_RBXLX).unwrap();
    let tree = RbxlLoader::to_instance_tree(&dom);

    let part = tree.iter().find(|n| n.name == "TestPart").unwrap();
    match part.properties.get("Anchored") {
        Some(PropertyValue::Bool(true)) => {} // correct
        other => panic!("expected Bool(true), got {:?}", other),
    }

    let spawn = tree.iter().find(|n| n.name == "EnemySpawn").unwrap();
    match spawn.properties.get("Anchored") {
        Some(PropertyValue::Bool(false)) => {} // correct
        other => panic!("expected Bool(false), got {:?}", other),
    }
}

#[test]
fn test_query_by_class_name() {
    let dom = RbxlLoader::load_xml_str(TEST_RBXLX).unwrap();

    let parts = RbxlLoader::query(&dom, Some("Part"), None, None);
    assert_eq!(
        parts.len(),
        2,
        "should find 2 Parts (TestPart + EnemySpawn)"
    );

    let scripts = RbxlLoader::query(&dom, Some("Script"), None, None);
    assert_eq!(scripts.len(), 1, "should find 1 Script");
    assert_eq!(scripts[0].name, "ServerScript");

    let modules = RbxlLoader::query(&dom, Some("ModuleScript"), None, None);
    assert_eq!(modules.len(), 1);
    assert_eq!(modules[0].name, "TestModule");
}

#[test]
fn test_query_by_name_substring() {
    let dom = RbxlLoader::load_xml_str(TEST_RBXLX).unwrap();

    let found = RbxlLoader::query(&dom, None, None, Some("Test"));
    let names: Vec<&str> = found.iter().map(|n| n.name.as_str()).collect();
    assert!(names.contains(&"TestPart"));
    assert!(names.contains(&"TestModule"));
    assert!(!names.contains(&"ServerScript"));
}

#[test]
fn test_extract_scripts() {
    let dom = RbxlLoader::load_xml_str(TEST_RBXLX).unwrap();
    let scripts = RbxlLoader::extract_scripts(&dom);

    assert_eq!(scripts.len(), 2, "should find Script + ModuleScript");

    let module = scripts.iter().find(|s| s.name == "TestModule").unwrap();
    assert_eq!(module.class_name, "ModuleScript");
    assert!(
        module.source.contains("hello from module"),
        "source should contain module code"
    );

    let server = scripts.iter().find(|s| s.name == "ServerScript").unwrap();
    assert_eq!(server.class_name, "Script");
    assert!(server.source.contains("server start"));
}

#[test]
fn test_extract_scripts_no_meshparts() {
    let dom = RbxlLoader::load_xml_str(TEST_RBXLX).unwrap();
    let meshes = RbxlLoader::extract_meshes(&dom);
    assert!(meshes.is_empty(), "no MeshParts in test data");
}

#[test]
fn test_ref_map_completeness() {
    let dom = RbxlLoader::load_xml_str(TEST_RBXLX).unwrap();
    let ref_map = vertigo_sync::rbxl::build_ref_map(&dom);
    let tree = RbxlLoader::to_instance_tree(&dom);

    // Every instance in the tree should have a corresponding ref_map entry.
    for node in &tree {
        assert!(
            ref_map.contains_key(&node.id),
            "ref_map missing entry for instance '{}' (id={})",
            node.name,
            node.id
        );
    }
}

#[test]
fn test_unsupported_extension() {
    let mut file = NamedTempFile::with_suffix(".txt").unwrap();
    file.write_all(b"not a roblox file").unwrap();
    file.flush().unwrap();

    let result = RbxlLoader::load_file(file.path());
    assert!(result.is_err(), "should reject .txt extension");
    let err = result.unwrap_err().to_string();
    assert!(
        err.contains("unsupported"),
        "error should mention unsupported"
    );
}

#[test]
fn test_nonexistent_file() {
    let result = RbxlLoader::load_file(std::path::Path::new("/tmp/nonexistent_vertigo_test.rbxl"));
    assert!(result.is_err(), "should fail for missing file");
}

#[test]
fn test_json_roundtrip_property_values() {
    // Verify that all PropertyValue variants survive JSON serialization.
    let values = vec![
        PropertyValue::String("hello".into()),
        PropertyValue::Number(42.5),
        PropertyValue::Bool(true),
        PropertyValue::Int32(-100),
        PropertyValue::Float32(std::f32::consts::PI),
        PropertyValue::Vector3(1.0, 2.0, 3.0),
        PropertyValue::CFrame([
            1.0, 0.0, 0.0, 0.0, 1.0, 0.0, 0.0, 0.0, 1.0, 10.0, 20.0, 30.0,
        ]),
        PropertyValue::Color3(0.5, 0.5, 0.5),
        PropertyValue::BrickColor(194),
        PropertyValue::Enum(6),
        PropertyValue::Ref("abc123".into()),
        PropertyValue::Content("rbxassetid://12345".into()),
        PropertyValue::UDim2(0.0, 100, 0.0, 50),
        PropertyValue::NumberRange(0.0, 1.0),
        PropertyValue::NumberSequence(vec![(0.0, 0.0, 0.0), (1.0, 1.0, 0.0)]),
        PropertyValue::ColorSequence(vec![(0.0, 1.0, 0.0, 0.0), (1.0, 0.0, 0.0, 1.0)]),
    ];

    for val in &values {
        let json = serde_json::to_string(val).expect("should serialize");
        let parsed: PropertyValue = serde_json::from_str(&json).expect("should deserialize");
        assert_eq!(&parsed, val, "roundtrip failed for {:?}", val);
    }
}
