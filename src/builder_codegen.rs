//! Builder code generation — convert model.json instance trees into Luau builder modules.
//!
//! The builder-first pattern: agents write builder modules (Luau code that generates
//! geometry procedurally) instead of making ephemeral DataModel mutations. Builder code
//! is diffable, versionable, reviewable, and deterministically reproducible.

use std::collections::HashSet;
use std::fmt::Write;

// ---------------------------------------------------------------------------
// Scaffold generation — create a new empty builder from template
// ---------------------------------------------------------------------------

/// Generate a scaffolded builder .luau file from parameters.
pub fn scaffold_builder(
    name: &str,
    zone: &str,
    y_range: Option<&str>,
    description: Option<&str>,
) -> Result<String, String> {
    if name.is_empty() {
        return Err("builder name must not be empty".into());
    }
    if !name.chars().next().unwrap_or('_').is_ascii_alphabetic() && name.chars().next() != Some('_')
    {
        return Err("builder name must start with a letter or underscore".into());
    }
    if !name.chars().all(|c| c.is_ascii_alphanumeric() || c == '_') {
        return Err(
            "builder name must contain only alphanumeric characters and underscores".into(),
        );
    }
    if zone.is_empty() {
        return Err("zone must not be empty".into());
    }

    let desc = description.unwrap_or("Procedurally generated zone geometry");
    let y_range_str = y_range.unwrap_or("TBD");

    // Compute y_mid from y_range for the example comment.
    let y_mid = parse_y_mid(y_range.unwrap_or("0 to 0"));

    let mut code = String::with_capacity(2048);
    writeln!(code, "--!strict").unwrap();
    writeln!(code, "--[[").unwrap();
    writeln!(code, "\t{name} — {desc}").unwrap();
    writeln!(code, "\tZone: {zone}").unwrap();
    writeln!(code, "\tY Range: {y_range_str}").unwrap();
    writeln!(code, "]]").unwrap();
    writeln!(code).unwrap();
    writeln!(
        code,
        "local CollectionService = game:GetService(\"CollectionService\")"
    )
    .unwrap();
    writeln!(code, "local Workspace = game:GetService(\"Workspace\")").unwrap();
    writeln!(code).unwrap();
    writeln!(code, "local {name} = {{}}").unwrap();
    writeln!(code).unwrap();
    writeln!(code, "function {name}:Init()").unwrap();
    writeln!(code, "\t-- Setup state, load config. No side effects.").unwrap();
    writeln!(code, "end").unwrap();
    writeln!(code).unwrap();
    writeln!(code, "function {name}:Build()").unwrap();
    writeln!(code, "\tlocal root = Instance.new(\"Model\")").unwrap();
    writeln!(code, "\troot.Name = \"{zone}\"").unwrap();
    writeln!(code).unwrap();
    writeln!(code, "\t-- TODO: Add geometry here").unwrap();
    writeln!(code, "\t-- Example:").unwrap();
    writeln!(code, "\t-- local part = Instance.new(\"Part\")").unwrap();
    writeln!(code, "\t-- part.Size = Vector3.new(100, 2, 100)").unwrap();
    writeln!(code, "\t-- part.Position = Vector3.new(0, {y_mid}, 0)").unwrap();
    writeln!(code, "\t-- part.Anchored = true").unwrap();
    writeln!(code, "\t-- part.Material = Enum.Material.Rock").unwrap();
    writeln!(code, "\t-- part.Color = Color3.fromRGB(80, 70, 60)").unwrap();
    writeln!(code, "\t-- CollectionService:AddTag(part, \"Terrain\")").unwrap();
    writeln!(code, "\t-- part.Parent = root").unwrap();
    writeln!(code).unwrap();
    writeln!(code, "\troot.Parent = Workspace").unwrap();
    writeln!(code, "end").unwrap();
    writeln!(code).unwrap();
    writeln!(code, "return {name}").unwrap();

    Ok(code)
}

/// Parse a y_range like "-50 to -20" and return the midpoint as an integer string.
fn parse_y_mid(y_range: &str) -> String {
    // Try to parse "N to M" format.
    let parts: Vec<&str> = y_range.split("to").collect();
    if parts.len() == 2 {
        if let (Ok(a), Ok(b)) = (
            parts[0].trim().parse::<f64>(),
            parts[1].trim().parse::<f64>(),
        ) {
            return format!("{}", ((a + b) / 2.0) as i64);
        }
    }
    "0".to_string()
}

// ---------------------------------------------------------------------------
// Model-to-builder conversion — convert instance tree JSON to Luau code
// ---------------------------------------------------------------------------

/// Reserved Luau keywords that cannot be used as variable names.
const LUAU_KEYWORDS: &[&str] = &[
    "and", "break", "continue", "do", "else", "elseif", "end", "false", "for", "function", "if",
    "in", "local", "nil", "not", "or", "repeat", "return", "then", "true", "until", "while",
    "type", "export",
];

/// Generate Luau builder code from a model.json instance tree.
pub fn generate_builder_luau(
    model: &serde_json::Value,
    builder_name: &str,
) -> Result<String, String> {
    if builder_name.is_empty() {
        return Err("builder_name must not be empty".into());
    }

    let mut code = String::with_capacity(4096);
    let mut var_counter = VarCounter::new();

    // Header
    writeln!(code, "--!strict").unwrap();
    writeln!(code, "-- Auto-generated builder from model.json capture").unwrap();
    writeln!(code).unwrap();
    writeln!(
        code,
        "local CollectionService = game:GetService(\"CollectionService\")"
    )
    .unwrap();
    writeln!(code, "local Workspace = game:GetService(\"Workspace\")").unwrap();
    writeln!(code).unwrap();
    writeln!(code, "local {builder_name} = {{}}").unwrap();
    writeln!(code).unwrap();
    writeln!(code, "function {builder_name}:Init()").unwrap();
    writeln!(code, "end").unwrap();
    writeln!(code).unwrap();
    writeln!(code, "function {builder_name}:Build()").unwrap();

    // If the root node is a Model or Folder, use it as-is; otherwise wrap in a root Model.
    let root_class = model
        .get("ClassName")
        .and_then(|v| v.as_str())
        .unwrap_or("Model");
    let root_name = model.get("Name").and_then(|v| v.as_str()).unwrap_or("Root");

    writeln!(code, "\tlocal root = Instance.new(\"{root_class}\")").unwrap();
    writeln!(code, "\troot.Name = \"{root_name}\"").unwrap();

    // Properties on root
    emit_properties(&mut code, model, "root", 1)?;
    emit_tags(&mut code, model, "root", 1);
    emit_attributes(&mut code, model, "root", 1);

    // Children
    if let Some(children) = model.get("Children").and_then(|v| v.as_array()) {
        for child in children {
            generate_instance_code(&mut code, child, "root", 1, &mut var_counter)?;
        }
    }

    writeln!(code).unwrap();
    writeln!(code, "\troot.Parent = Workspace").unwrap();
    writeln!(code, "end").unwrap();
    writeln!(code).unwrap();
    writeln!(code, "return {builder_name}").unwrap();

    Ok(code)
}

/// Counter to generate unique variable names.
struct VarCounter {
    next: usize,
    used: HashSet<String>,
}

impl VarCounter {
    fn new() -> Self {
        Self {
            next: 0,
            used: HashSet::new(),
        }
    }

    fn make_var(&mut self, hint: &str) -> String {
        let sanitized = sanitize_var_name(hint);
        let candidate = if self.used.contains(&sanitized) {
            loop {
                self.next += 1;
                let name = format!("{sanitized}_{}", self.next);
                if !self.used.contains(&name) {
                    break name;
                }
            }
        } else {
            sanitized
        };
        self.used.insert(candidate.clone());
        candidate
    }
}

/// Sanitize a string into a valid Luau variable name.
pub fn sanitize_var_name(name: &str) -> String {
    if name.is_empty() {
        return "unnamed".to_string();
    }

    // Replace non-alphanumeric chars with underscore.
    let sanitized: String = name
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '_' {
                c
            } else {
                '_'
            }
        })
        .collect();

    // Ensure it starts with a letter or underscore.
    let sanitized = if sanitized.chars().next().unwrap_or('_').is_ascii_digit() {
        format!("_{sanitized}")
    } else {
        sanitized
    };

    // If it's a keyword, prefix with underscore.
    let lower = sanitized.to_lowercase();
    if LUAU_KEYWORDS.contains(&lower.as_str()) {
        format!("_{sanitized}")
    } else {
        sanitized
    }
}

fn generate_instance_code(
    code: &mut String,
    node: &serde_json::Value,
    parent_var: &str,
    depth: usize,
    counter: &mut VarCounter,
) -> Result<(), String> {
    let indent = "\t".repeat(depth + 1);
    let class_name = node
        .get("ClassName")
        .and_then(|v| v.as_str())
        .unwrap_or("Part");
    let name = node
        .get("Name")
        .and_then(|v| v.as_str())
        .unwrap_or("unnamed");

    let var_name = counter.make_var(name);

    writeln!(
        code,
        "{indent}local {var_name} = Instance.new(\"{class_name}\")"
    )
    .unwrap();
    writeln!(code, "{indent}{var_name}.Name = \"{name}\"").unwrap();

    // Properties
    emit_properties(code, node, &var_name, depth + 1)?;

    // Tags
    emit_tags(code, node, &var_name, depth + 1);

    // Attributes
    emit_attributes(code, node, &var_name, depth + 1);

    // Parent assignment
    writeln!(code, "{indent}{var_name}.Parent = {parent_var}").unwrap();

    // Children (recursive)
    if let Some(children) = node.get("Children").and_then(|v| v.as_array()) {
        for child in children {
            generate_instance_code(code, child, &var_name, depth + 1, counter)?;
        }
    }

    Ok(())
}

fn emit_properties(
    code: &mut String,
    node: &serde_json::Value,
    var_name: &str,
    depth: usize,
) -> Result<(), String> {
    let indent = "\t".repeat(depth + 1);
    if let Some(props) = node.get("Properties").and_then(|v| v.as_object()) {
        for (key, value) in props {
            // Skip Source (script content) and Name (already handled).
            if key == "Source" || key == "Name" {
                continue;
            }
            let lua_value = json_value_to_lua(value);
            writeln!(code, "{indent}{var_name}.{key} = {lua_value}").unwrap();
        }
    }
    Ok(())
}

fn emit_tags(code: &mut String, node: &serde_json::Value, var_name: &str, depth: usize) {
    let indent = "\t".repeat(depth + 1);
    if let Some(tags) = node.get("Tags").and_then(|v| v.as_array()) {
        for tag in tags {
            if let Some(tag_str) = tag.as_str() {
                writeln!(
                    code,
                    "{indent}CollectionService:AddTag({var_name}, \"{tag_str}\")"
                )
                .unwrap();
            }
        }
    }
}

fn emit_attributes(code: &mut String, node: &serde_json::Value, var_name: &str, depth: usize) {
    let indent = "\t".repeat(depth + 1);
    if let Some(attrs) = node.get("Attributes").and_then(|v| v.as_object()) {
        for (key, value) in attrs {
            let lua_value = json_value_to_lua(value);
            writeln!(
                code,
                "{indent}{var_name}:SetAttribute(\"{key}\", {lua_value})"
            )
            .unwrap();
        }
    }
}

/// Convert a JSON value to a Lua literal string.
pub fn json_value_to_lua(value: &serde_json::Value) -> String {
    match value {
        serde_json::Value::Null => "nil".to_string(),
        serde_json::Value::Bool(b) => if *b { "true" } else { "false" }.to_string(),
        serde_json::Value::Number(n) => n.to_string(),
        serde_json::Value::String(s) => {
            // Detect known Roblox types encoded as strings.
            // Vector3: "X, Y, Z"
            if let Some(v3) = try_parse_vector3(s) {
                return v3;
            }
            // Color3: "R, G, B" (0-1 range)
            if let Some(c3) = try_parse_color3(s) {
                return c3;
            }
            // CFrame: "X, Y, Z, R00, R01, R02, R10, R11, R12, R20, R21, R22"
            if let Some(cf) = try_parse_cframe(s) {
                return cf;
            }
            // Enum: "Enum.Material.Rock"
            if s.starts_with("Enum.") {
                return s.clone();
            }
            // BrickColor: "BrickColor.new(...)"
            if s.starts_with("BrickColor.new(") {
                return s.clone();
            }
            // Default: quoted string
            format!("\"{}\"", s.replace('\\', "\\\\").replace('"', "\\\""))
        }
        serde_json::Value::Array(arr) => {
            // Arrays could be Vector3, Color3, etc.
            if arr.len() == 3 && arr.iter().all(|v| v.is_number()) {
                let x = arr[0].as_f64().unwrap_or(0.0);
                let y = arr[1].as_f64().unwrap_or(0.0);
                let z = arr[2].as_f64().unwrap_or(0.0);
                return format!("Vector3.new({x}, {y}, {z})");
            }
            format!(
                "{{{}}}",
                arr.iter()
                    .map(json_value_to_lua)
                    .collect::<Vec<_>>()
                    .join(", ")
            )
        }
        serde_json::Value::Object(obj) => {
            // Handle typed property objects like {"Type": "Vector3", "Value": [x, y, z]}
            if let (Some(typ), Some(val)) =
                (obj.get("Type").and_then(|v| v.as_str()), obj.get("Value"))
            {
                return typed_property_to_lua(typ, val);
            }
            // Generic table
            let entries: Vec<String> = obj
                .iter()
                .map(|(k, v)| format!("{k} = {}", json_value_to_lua(v)))
                .collect();
            format!("{{{}}}", entries.join(", "))
        }
    }
}

/// Handle typed property values (common in .model.json format).
fn typed_property_to_lua(typ: &str, value: &serde_json::Value) -> String {
    match typ {
        "Vector3" => {
            if let Some(arr) = value.as_array() {
                if arr.len() == 3 {
                    let x = arr[0].as_f64().unwrap_or(0.0);
                    let y = arr[1].as_f64().unwrap_or(0.0);
                    let z = arr[2].as_f64().unwrap_or(0.0);
                    return format!("Vector3.new({x}, {y}, {z})");
                }
            }
            json_value_to_lua(value)
        }
        "Vector2" => {
            if let Some(arr) = value.as_array() {
                if arr.len() == 2 {
                    let x = arr[0].as_f64().unwrap_or(0.0);
                    let y = arr[1].as_f64().unwrap_or(0.0);
                    return format!("Vector2.new({x}, {y})");
                }
            }
            json_value_to_lua(value)
        }
        "CFrame" => {
            if let Some(arr) = value.as_array() {
                if arr.len() == 12 {
                    let nums: Vec<String> = arr
                        .iter()
                        .map(|v| format!("{}", v.as_f64().unwrap_or(0.0)))
                        .collect();
                    return format!("CFrame.new({})", nums.join(", "));
                }
                if arr.len() == 3 {
                    let x = arr[0].as_f64().unwrap_or(0.0);
                    let y = arr[1].as_f64().unwrap_or(0.0);
                    let z = arr[2].as_f64().unwrap_or(0.0);
                    return format!("CFrame.new({x}, {y}, {z})");
                }
            }
            json_value_to_lua(value)
        }
        "Color3" => {
            if let Some(arr) = value.as_array() {
                if arr.len() == 3 {
                    let r = arr[0].as_f64().unwrap_or(0.0);
                    let g = arr[1].as_f64().unwrap_or(0.0);
                    let b = arr[2].as_f64().unwrap_or(0.0);
                    // If all values are <= 1, use Color3.new; else use fromRGB
                    if r <= 1.0 && g <= 1.0 && b <= 1.0 {
                        return format!("Color3.new({r}, {g}, {b})");
                    } else {
                        return format!("Color3.fromRGB({}, {}, {})", r as u8, g as u8, b as u8);
                    }
                }
            }
            json_value_to_lua(value)
        }
        "Color3uint8" => {
            if let Some(arr) = value.as_array() {
                if arr.len() == 3 {
                    let r = arr[0].as_u64().unwrap_or(0);
                    let g = arr[1].as_u64().unwrap_or(0);
                    let b = arr[2].as_u64().unwrap_or(0);
                    return format!("Color3.fromRGB({r}, {g}, {b})");
                }
            }
            json_value_to_lua(value)
        }
        "UDim2" => {
            if let Some(arr) = value.as_array() {
                if arr.len() == 4 {
                    let sx = arr[0].as_f64().unwrap_or(0.0);
                    let ox = arr[1].as_f64().unwrap_or(0.0);
                    let sy = arr[2].as_f64().unwrap_or(0.0);
                    let oy = arr[3].as_f64().unwrap_or(0.0);
                    return format!("UDim2.new({sx}, {ox}, {sy}, {oy})");
                }
            }
            json_value_to_lua(value)
        }
        "UDim" => {
            if let Some(arr) = value.as_array() {
                if arr.len() == 2 {
                    let s = arr[0].as_f64().unwrap_or(0.0);
                    let o = arr[1].as_f64().unwrap_or(0.0);
                    return format!("UDim.new({s}, {o})");
                }
            }
            json_value_to_lua(value)
        }
        "NumberRange" => {
            if let Some(arr) = value.as_array() {
                if arr.len() == 2 {
                    let min = arr[0].as_f64().unwrap_or(0.0);
                    let max = arr[1].as_f64().unwrap_or(0.0);
                    return format!("NumberRange.new({min}, {max})");
                }
            }
            json_value_to_lua(value)
        }
        "Enum" => {
            if let Some(s) = value.as_str() {
                return s.to_string();
            }
            json_value_to_lua(value)
        }
        "Bool" => {
            if let Some(b) = value.as_bool() {
                return if b { "true" } else { "false" }.to_string();
            }
            json_value_to_lua(value)
        }
        "Float32" | "Float64" | "Int32" | "Int64" => {
            if let Some(n) = value.as_f64() {
                return format!("{n}");
            }
            json_value_to_lua(value)
        }
        "String" | "Content" | "BinaryString" => {
            if let Some(s) = value.as_str() {
                return format!("\"{}\"", s.replace('\\', "\\\\").replace('"', "\\\""));
            }
            json_value_to_lua(value)
        }
        _ => {
            // Unknown type — fall back to generic conversion with a comment.
            format!("{} --[[{typ}]]", json_value_to_lua(value))
        }
    }
}

/// Try to parse a string as "X, Y, Z" Vector3.
fn try_parse_vector3(s: &str) -> Option<String> {
    let parts: Vec<&str> = s.split(',').collect();
    if parts.len() != 3 {
        return None;
    }
    let x = parts[0].trim().parse::<f64>().ok()?;
    let y = parts[1].trim().parse::<f64>().ok()?;
    let z = parts[2].trim().parse::<f64>().ok()?;
    // Only return Vector3 if this looks numeric (not just random comma-separated text)
    Some(format!("Vector3.new({x}, {y}, {z})"))
}

/// Try to parse a string as Color3 (0-1 range floats).
fn try_parse_color3(s: &str) -> Option<String> {
    // Don't try if it could also be a Vector3 — heuristic: all values 0..1
    let parts: Vec<&str> = s.split(',').collect();
    if parts.len() != 3 {
        return None;
    }
    let r = parts[0].trim().parse::<f64>().ok()?;
    let g = parts[1].trim().parse::<f64>().ok()?;
    let b = parts[2].trim().parse::<f64>().ok()?;
    if r >= 0.0 && r <= 1.0 && g >= 0.0 && g <= 1.0 && b >= 0.0 && b <= 1.0 {
        // Ambiguous with Vector3, so don't auto-detect from string
        return None;
    }
    None
}

/// Try to parse a string as CFrame.
fn try_parse_cframe(s: &str) -> Option<String> {
    let parts: Vec<&str> = s.split(',').collect();
    if parts.len() != 12 {
        return None;
    }
    let nums: Result<Vec<f64>, _> = parts.iter().map(|p| p.trim().parse::<f64>()).collect();
    let nums = nums.ok()?;
    let strs: Vec<String> = nums.iter().map(|n| format!("{n}")).collect();
    Some(format!("CFrame.new({})", strs.join(", ")))
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn scaffold_basic_builder() {
        let code = scaffold_builder("TestBuilder", "Test Zone", None, None).unwrap();
        assert!(code.starts_with("--!strict"));
        assert!(code.contains("local TestBuilder = {}"));
        assert!(code.contains("function TestBuilder:Init()"));
        assert!(code.contains("function TestBuilder:Build()"));
        assert!(code.contains("root.Name = \"Test Zone\""));
        assert!(code.contains("root.Parent = Workspace"));
        assert!(code.contains("return TestBuilder"));
        assert!(code.contains("CollectionService"));
    }

    #[test]
    fn scaffold_with_y_range() {
        let code = scaffold_builder("DeepBuilder", "Deep Zone", Some("-50 to -20"), None).unwrap();
        assert!(code.contains("Y Range: -50 to -20"));
        assert!(code.contains("Vector3.new(0, -35, 0)"));
    }

    #[test]
    fn scaffold_with_description() {
        let code = scaffold_builder(
            "CoralBuilder",
            "Coral Cave",
            None,
            Some("Underwater coral cave with bioluminescence"),
        )
        .unwrap();
        assert!(code.contains("Underwater coral cave with bioluminescence"));
    }

    #[test]
    fn scaffold_rejects_empty_name() {
        assert!(scaffold_builder("", "Zone", None, None).is_err());
    }

    #[test]
    fn scaffold_rejects_invalid_name() {
        assert!(scaffold_builder("123Bad", "Zone", None, None).is_err());
        assert!(scaffold_builder("has space", "Zone", None, None).is_err());
    }

    #[test]
    fn scaffold_rejects_empty_zone() {
        assert!(scaffold_builder("TestBuilder", "", None, None).is_err());
    }

    #[test]
    fn convert_simple_model() {
        let model = serde_json::json!({
            "Name": "TestModel",
            "ClassName": "Model",
            "Children": [
                {
                    "Name": "Floor",
                    "ClassName": "Part",
                    "Properties": {
                        "Size": {"Type": "Vector3", "Value": [100.0, 2.0, 100.0]},
                        "Position": {"Type": "Vector3", "Value": [0.0, 0.0, 0.0]},
                        "Anchored": {"Type": "Bool", "Value": true},
                        "Material": {"Type": "Enum", "Value": "Enum.Material.Rock"},
                        "Color": {"Type": "Color3", "Value": [0.3, 0.3, 0.3]}
                    },
                    "Tags": ["Terrain"],
                    "Attributes": {
                        "ZoneId": "hub_v1"
                    }
                }
            ]
        });

        let code = generate_builder_luau(&model, "TestModelBuilder").unwrap();
        assert!(code.starts_with("--!strict"));
        assert!(code.contains("local TestModelBuilder = {}"));
        assert!(code.contains("function TestModelBuilder:Init()"));
        assert!(code.contains("function TestModelBuilder:Build()"));
        assert!(code.contains("local root = Instance.new(\"Model\")"));
        assert!(code.contains("root.Name = \"TestModel\""));
        assert!(code.contains("local Floor = Instance.new(\"Part\")"));
        assert!(code.contains("Floor.Name = \"Floor\""));
        assert!(code.contains("Vector3.new(100, 2, 100)"));
        assert!(code.contains("Enum.Material.Rock"));
        assert!(code.contains("Color3.new(0.3, 0.3, 0.3)"));
        assert!(code.contains("CollectionService:AddTag(Floor, \"Terrain\")"));
        assert!(code.contains("Floor:SetAttribute(\"ZoneId\", \"hub_v1\")"));
        assert!(code.contains("Floor.Parent = root"));
        assert!(code.contains("root.Parent = Workspace"));
        assert!(code.contains("return TestModelBuilder"));
    }

    #[test]
    fn convert_nested_instances() {
        let model = serde_json::json!({
            "Name": "Outer",
            "ClassName": "Model",
            "Children": [
                {
                    "Name": "Inner",
                    "ClassName": "Folder",
                    "Children": [
                        {
                            "Name": "DeepPart",
                            "ClassName": "Part",
                            "Properties": {
                                "Anchored": {"Type": "Bool", "Value": true}
                            }
                        }
                    ]
                }
            ]
        });

        let code = generate_builder_luau(&model, "NestedBuilder").unwrap();
        assert!(code.contains("local Inner = Instance.new(\"Folder\")"));
        assert!(code.contains("Inner.Parent = root"));
        assert!(code.contains("local DeepPart = Instance.new(\"Part\")"));
        assert!(code.contains("DeepPart.Parent = Inner"));
    }

    #[test]
    fn sanitize_var_name_basics() {
        assert_eq!(sanitize_var_name("hello"), "hello");
        assert_eq!(sanitize_var_name("Hello_World"), "Hello_World");
        assert_eq!(sanitize_var_name("123start"), "_123start");
        assert_eq!(sanitize_var_name("has space"), "has_space");
        assert_eq!(sanitize_var_name("has-dash"), "has_dash");
        assert_eq!(sanitize_var_name(""), "unnamed");
    }

    #[test]
    fn sanitize_var_name_keywords() {
        assert_eq!(sanitize_var_name("end"), "_end");
        assert_eq!(sanitize_var_name("local"), "_local");
        assert_eq!(sanitize_var_name("return"), "_return");
        assert_eq!(sanitize_var_name("true"), "_true");
        assert_eq!(sanitize_var_name("false"), "_false");
        assert_eq!(sanitize_var_name("nil"), "_nil");
    }

    #[test]
    fn json_value_to_lua_primitives() {
        assert_eq!(json_value_to_lua(&serde_json::json!(null)), "nil");
        assert_eq!(json_value_to_lua(&serde_json::json!(true)), "true");
        assert_eq!(json_value_to_lua(&serde_json::json!(false)), "false");
        assert_eq!(json_value_to_lua(&serde_json::json!(42)), "42");
        assert_eq!(json_value_to_lua(&serde_json::json!(1.5)), "1.5");
        assert_eq!(json_value_to_lua(&serde_json::json!("hello")), "\"hello\"");
    }

    #[test]
    fn json_value_to_lua_enum_string() {
        assert_eq!(
            json_value_to_lua(&serde_json::json!("Enum.Material.Rock")),
            "Enum.Material.Rock"
        );
    }

    #[test]
    fn typed_property_vector3() {
        let val = serde_json::json!({"Type": "Vector3", "Value": [1.0, 2.0, 3.0]});
        assert_eq!(json_value_to_lua(&val), "Vector3.new(1, 2, 3)");
    }

    #[test]
    fn typed_property_color3() {
        let val = serde_json::json!({"Type": "Color3", "Value": [0.5, 0.5, 0.5]});
        assert_eq!(json_value_to_lua(&val), "Color3.new(0.5, 0.5, 0.5)");
    }

    #[test]
    fn typed_property_color3uint8() {
        let val = serde_json::json!({"Type": "Color3uint8", "Value": [128, 64, 32]});
        assert_eq!(json_value_to_lua(&val), "Color3.fromRGB(128, 64, 32)");
    }

    #[test]
    fn typed_property_cframe() {
        let val = serde_json::json!({"Type": "CFrame", "Value": [1.0, 2.0, 3.0]});
        assert_eq!(json_value_to_lua(&val), "CFrame.new(1, 2, 3)");
    }

    #[test]
    fn typed_property_number_range() {
        let val = serde_json::json!({"Type": "NumberRange", "Value": [0.5, 1.5]});
        assert_eq!(json_value_to_lua(&val), "NumberRange.new(0.5, 1.5)");
    }

    #[test]
    fn convert_empty_builder_name_rejected() {
        let model = serde_json::json!({"Name": "X", "ClassName": "Model"});
        assert!(generate_builder_luau(&model, "").is_err());
    }

    #[test]
    fn duplicate_names_get_unique_vars() {
        let model = serde_json::json!({
            "Name": "Root",
            "ClassName": "Model",
            "Children": [
                {"Name": "Part", "ClassName": "Part"},
                {"Name": "Part", "ClassName": "Part"},
                {"Name": "Part", "ClassName": "Part"}
            ]
        });

        let code = generate_builder_luau(&model, "DupBuilder").unwrap();
        // Should have Part, Part_1 (or similar), Part_2
        assert!(code.contains("local Part = Instance.new(\"Part\")"));
        // The second and third should get unique suffixes
        let part_count = code.matches("Instance.new(\"Part\")").count();
        assert_eq!(part_count, 3, "should have 3 Part instances");
        // All should have unique var names (no duplicate `local X =` lines)
        let local_lines: Vec<&str> = code
            .lines()
            .filter(|l| l.trim_start().starts_with("local ") && l.contains("Instance.new"))
            .collect();
        let var_names: Vec<&str> = local_lines
            .iter()
            .map(|l| {
                let trimmed = l.trim_start().strip_prefix("local ").unwrap();
                trimmed.split_whitespace().next().unwrap()
            })
            .collect();
        let unique: HashSet<&str> = var_names.iter().cloned().collect();
        assert_eq!(
            var_names.len(),
            unique.len(),
            "all variable names should be unique: {:?}",
            var_names
        );
    }

    #[test]
    fn y_mid_parsing() {
        assert_eq!(parse_y_mid("-50 to -20"), "-35");
        assert_eq!(parse_y_mid("0 to 100"), "50");
        assert_eq!(parse_y_mid("invalid"), "0");
        assert_eq!(parse_y_mid("-10 to 10"), "0");
    }

    #[test]
    fn generated_code_has_strict_and_collection_service() {
        let model = serde_json::json!({"Name": "X", "ClassName": "Model"});
        let code = generate_builder_luau(&model, "StrictCheck").unwrap();
        assert!(code.starts_with("--!strict"));
        assert!(code.contains("local CollectionService = game:GetService(\"CollectionService\")"));
    }
}
