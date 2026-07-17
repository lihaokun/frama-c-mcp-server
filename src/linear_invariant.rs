//! Linear invariant CLI integration: JSON ↔ .in/.inv format conversion.

use serde_json::Value;

/// Convert a primed variable name from JSON format ("x'") to .in format ("'x").
fn to_in_primed(var: &str) -> String {
    if let Some(base) = var.strip_suffix('\'') {
        format!("'{base}")
    } else {
        var.to_string()
    }
}

/// Format a single constraint JSON object as a .in format line.
///
/// Input:  {"coeffs": [["x", 1], ["y", -2]], "constant": 3, "kind": "geq"}
/// Output: "1 * x -2 * y + 3 >= 0"
pub fn format_constraint_in(c: &Value) -> Result<String, String> {
    let coeffs = c["coeffs"]
        .as_array()
        .ok_or("constraint missing 'coeffs' array")?;
    let constant = c["constant"]
        .as_i64()
        .ok_or("constraint missing 'constant' integer")?;
    let kind = c["kind"]
        .as_str()
        .ok_or("constraint missing 'kind' string")?;

    let mut parts = Vec::new();

    for (i, pair) in coeffs.iter().enumerate() {
        let arr = pair
            .as_array()
            .ok_or("each coeff must be [var, number]")?;
        if arr.len() != 2 {
            return Err("each coeff must be [var, number]".into());
        }
        let var = arr[0].as_str().ok_or("variable name must be string")?;
        let coeff = arr[1].as_i64().ok_or("coefficient must be integer")?;
        if coeff == 0 {
            continue;
        }
        let var_in = to_in_primed(var);
        if i == 0 || parts.is_empty() {
            // First term: no leading space for positive
            if coeff == 1 {
                parts.push(format!("1 * {var_in}"));
            } else if coeff == -1 {
                parts.push(format!("-1 * {var_in}"));
            } else {
                parts.push(format!("{coeff} * {var_in}"));
            }
        } else if coeff > 0 {
            if coeff == 1 {
                parts.push(format!(" + 1 * {var_in}"));
            } else {
                parts.push(format!(" + {coeff} * {var_in}"));
            }
        } else {
            let abs = -coeff;
            if abs == 1 {
                parts.push(format!(" -1 * {var_in}"));
            } else {
                parts.push(format!(" -{abs} * {var_in}"));
            }
        }
    }

    // Constant term
    if constant != 0 {
        if parts.is_empty() {
            parts.push(format!("{constant}"));
        } else if constant > 0 {
            parts.push(format!(" + {constant}"));
        } else {
            let abs = -constant;
            parts.push(format!(" -{abs}"));
        }
    }

    if parts.is_empty() {
        parts.push("0".to_string());
    }

    let op = match kind {
        "geq" => ">=",
        "eq" => "=",
        _ => return Err(format!("unknown constraint kind '{kind}'")),
    };

    Ok(format!("{} {op} 0", parts.join("")))
}

/// Convert JSON transition system to .in format text.
pub fn json_to_in_format(input: &Value) -> Result<String, String> {
    let mut lines = Vec::new();

    // Variables
    let vars = input["variables"]
        .as_array()
        .ok_or("missing 'variables' array")?;
    let var_names: Vec<&str> = vars
        .iter()
        .map(|v| v.as_str().ok_or("variable must be string"))
        .collect::<Result<_, _>>()?;
    lines.push(format!("Variable [{}]", var_names.join(", ")));

    // Locations
    let locations = input["locations"]
        .as_array()
        .ok_or("missing 'locations' array")?;
    for loc in locations {
        let name = loc["name"].as_str().ok_or("location missing 'name'")?;
        lines.push(format!("Location {name}:"));
        if let Some(constrs) = loc["init_constraints"].as_array() {
            for c in constrs {
                lines.push(format!("  {}", format_constraint_in(c)?));
            }
        }
    }

    // Transitions
    let transitions = input["transitions"]
        .as_array()
        .ok_or("missing 'transitions' array")?;
    for tr in transitions {
        let name = tr["name"].as_str().ok_or("transition missing 'name'")?;
        let src = tr["src"].as_str().ok_or("transition missing 'src'")?;
        let dst = tr["dst"].as_str().ok_or("transition missing 'dst'")?;
        lines.push(format!("Transition {name}: {src}, {dst}"));

        if let Some(guard) = tr["guard"].as_array() {
            for c in guard {
                lines.push(format!("  {}", format_constraint_in(c)?));
            }
        }
        if let Some(update) = tr["update"].as_array() {
            for c in update {
                lines.push(format!("  {}", format_constraint_in(c)?));
            }
        }
        if let Some(preserved) = tr["preserved"].as_array() {
            let names: Vec<&str> = preserved
                .iter()
                .filter_map(|v| v.as_str())
                .collect();
            if !names.is_empty() {
                lines.push(format!("  preserve[{}]", names.join(", ")));
            }
        }
    }

    lines.push("End".to_string());
    Ok(lines.join("\n"))
}

/// Parse .inv format output from linear_invariant CLI into JSON.
pub fn parse_inv_output(text: &str) -> Value {
    let lines: Vec<&str> = text.lines().collect();

    // Check if multi-location (has "Location:" prefix)
    let has_locations = lines.iter().any(|l| l.starts_with("Location:"));

    if has_locations {
        let mut result = serde_json::Map::new();
        let mut current_loc: Option<String> = None;
        let mut current_invs: Vec<Value> = Vec::new();

        for line in &lines {
            let trimmed = line.trim();
            if trimmed.is_empty() {
                continue;
            }
            if let Some(loc_name) = trimmed.strip_prefix("Location:") {
                // Save previous location
                if let Some(loc) = current_loc.take() {
                    result.insert(loc, Value::Array(current_invs.clone()));
                    current_invs.clear();
                }
                current_loc = Some(loc_name.trim().to_string());
            } else if current_loc.is_some() {
                current_invs.push(Value::String(trimmed.to_string()));
            }
        }
        // Save last location
        if let Some(loc) = current_loc {
            result.insert(loc, Value::Array(current_invs));
        }

        serde_json::json!({ "invariants": result })
    } else {
        // Single location: just list the invariants
        let invs: Vec<Value> = lines
            .iter()
            .map(|l| l.trim())
            .filter(|l| !l.is_empty())
            .map(|l| Value::String(l.to_string()))
            .collect();
        serde_json::json!({ "invariants": invs })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_format_constraint_geq() {
        let c = serde_json::json!({"coeffs": [["x", 1], ["y", -2]], "constant": 3, "kind": "geq"});
        let result = format_constraint_in(&c).unwrap();
        assert_eq!(result, "1 * x -2 * y + 3 >= 0");
    }

    #[test]
    fn test_format_constraint_primed() {
        let c = serde_json::json!({"coeffs": [["x'", 1], ["x", -1]], "constant": -1, "kind": "eq"});
        let result = format_constraint_in(&c).unwrap();
        assert_eq!(result, "1 * 'x -1 * x -1 = 0");
    }

    #[test]
    fn test_json_to_in_format() {
        let input = serde_json::json!({
            "variables": ["x", "n"],
            "locations": [{"name": "L", "init_constraints": [
                {"coeffs": [["x", 1]], "constant": 0, "kind": "geq"}
            ]}],
            "transitions": [{"name": "T1", "src": "L", "dst": "L",
                "guard": [{"coeffs": [["x", 1], ["n", -1]], "constant": 0, "kind": "geq"}],
                "update": [{"coeffs": [["x'", 1], ["x", -1]], "constant": -1, "kind": "eq"}],
                "preserved": ["n"]
            }]
        });
        let result = json_to_in_format(&input).unwrap();
        assert!(result.contains("Variable [x, n]"));
        assert!(result.contains("Location L:"));
        assert!(result.contains("Transition T1: L, L"));
        assert!(result.contains("preserve[n]"));
        assert!(result.contains("End"));
    }

    #[test]
    fn test_parse_inv_single() {
        let text = "1 * x >= 0\n-1 * x + 10 >= 0\n";
        let result = parse_inv_output(text);
        let invs = result["invariants"].as_array().unwrap();
        assert_eq!(invs.len(), 2);
    }

    #[test]
    fn test_parse_inv_multi() {
        let text = "Location:L1\n1 * x >= 0\n\nLocation:L2\n-1 * y >= 0\n";
        let result = parse_inv_output(text);
        let invs = &result["invariants"];
        assert!(invs["L1"].is_array());
        assert!(invs["L2"].is_array());
    }
}
