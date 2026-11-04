//! Drift guards: docs are part of the contract agents read, so they must match
//! the code. These fail when someone adds an error code or action and forgets
//! the docs.

use rustplay::actions::Step;

const ERRORS_MD: &str = include_str!("../docs/ERRORS.md");
const AGENTS_MD: &str = include_str!("../AGENTS.md");
const SCHEMA: &str = include_str!("../docs/task.schema.json");
const ERRORS_RS: &str = include_str!("../src/errors.rs");

#[test]
fn every_error_code_is_documented() {
    // Pull every `=> "CODE"` literal out of errors.rs.
    let mut codes: Vec<&str> = ERRORS_RS
        .split("=> \"")
        .skip(1)
        .filter_map(|s| s.split('"').next())
        .filter(|c| c.chars().all(|ch| ch.is_ascii_uppercase() || ch == '_') && c.len() > 3)
        .collect();
    codes.extend(["BUSY", "UNAUTHORIZED", "INTERNAL"]); // API-only codes
    codes.sort();
    codes.dedup();
    assert!(codes.len() >= 20, "parser found too few codes: {codes:?}");
    for c in codes {
        assert!(ERRORS_MD.contains(&format!("`{c}`")), "docs/ERRORS.md is missing `{c}`");
    }
}

#[test]
fn schema_matches_step_enum() {
    let schema: serde_json::Value = serde_json::from_str(SCHEMA).unwrap();
    let variants = schema["properties"]["steps"]["items"]["oneOf"].as_array().unwrap();
    let mut documented: Vec<String> =
        variants.iter().map(|v| v["properties"]["action"]["const"].as_str().unwrap().to_string()).collect();
    documented.sort();

    // Build one minimal valid instance per schema variant and check serde accepts it,
    // then check the round-tripped action name matches.
    let mut parsed = Vec::new();
    for v in variants {
        let mut obj = serde_json::Map::new();
        for req in v["required"].as_array().unwrap() {
            let k = req.as_str().unwrap();
            let prop = &v["properties"][k];
            let val = if let Some(c) = prop.get("const") {
                c.clone()
            } else {
                match prop["type"].as_str() {
                    Some("integer") | Some("number") => serde_json::json!(1),
                    Some("boolean") => serde_json::json!(true),
                    _ => serde_json::json!("x"),
                }
            };
            obj.insert(k.to_string(), val);
        }
        if obj["action"] == "assert_text" {
            obj.insert("contains".into(), "x".into());
        }
        if obj["action"] == "press" {
            obj.insert("key".into(), "Enter".into());
        }
        let step: Step = serde_json::from_value(serde_json::Value::Object(obj.clone()))
            .unwrap_or_else(|e| panic!("schema variant {obj:?} rejected by serde: {e}"));
        parsed.push(step.name().to_string());
    }
    parsed.sort();
    assert_eq!(parsed, documented);
    let mut all: Vec<String> = rustplay::actions::ACTIONS.iter().map(|s| s.to_string()).collect();
    all.sort();
    assert_eq!(all, documented, "docs/task.schema.json is out of sync with actions::ACTIONS");
    for a in &documented {
        assert!(AGENTS_MD.contains(&format!("`{a}`")), "AGENTS.md action table is missing `{a}`");
    }
}

const README: &str = include_str!("../README.md");
const PATCH: &str = include_str!("../docs/upstream/chromiumoxide-cdp-transport.patch");

#[test]
fn every_cli_flag_is_in_the_readme() {
    use clap::CommandFactory;
    let cmd = rustplay::cli::Cli::command();
    let mut missing = vec![];
    for a in cmd.get_arguments() {
        if let Some(long) = a.get_long() {
            if !["help", "version"].contains(&long) && !README.contains(&format!("--{long}")) {
                missing.push(long.to_string());
            }
        }
    }
    for sub in cmd.get_subcommands() {
        for a in sub.get_arguments() {
            if let Some(long) = a.get_long() {
                if long != "help" && !README.contains(&format!("--{long}")) {
                    missing.push(format!("{} --{long}", sub.get_name()));
                }
            }
        }
    }
    assert!(missing.is_empty(), "README.md does not mention: {missing:?}");
}

#[test]
fn schema_task_options_match_struct() {
    let schema: serde_json::Value = serde_json::from_str(SCHEMA).unwrap();
    let mut documented: Vec<String> =
        schema["properties"]["options"]["properties"].as_object().unwrap().keys().cloned().collect();
    documented.sort();
    let v = serde_json::to_value(rustplay::config::TaskOptions::default()).unwrap();
    let mut actual: Vec<String> = v.as_object().unwrap().keys().cloned().collect();
    actual.sort();
    assert_eq!(documented, actual, "docs/task.schema.json options drifted from config::TaskOptions");
}

/// The vendored crate must contain every line the documented patch adds, so
/// nobody edits vendor/ without regenerating docs/upstream/*.patch (and the
/// patch cannot silently stop being applied).
#[test]
fn vendored_chromiumoxide_matches_patch() {
    let mut file = String::new();
    let mut checked = 0;
    for line in PATCH.lines() {
        if let Some(rest) = line.strip_prefix("+++ b/") {
            file = rest.split('\t').next().unwrap().to_string();
            continue;
        }
        if let Some(added) = line.strip_prefix('+') {
            let added = added.trim();
            if added.is_empty() {
                continue;
            }
            let path = format!("{}/vendor/chromiumoxide/{file}", env!("CARGO_MANIFEST_DIR"));
            let src = std::fs::read_to_string(&path).unwrap_or_else(|e| panic!("{path}: {e}"));
            assert!(src.contains(added), "{file} is missing patched line: {added}");
            checked += 1;
        }
    }
    assert!(checked > 50, "patch parse looks wrong ({checked} lines)");
    let toml = include_str!("../Cargo.toml");
    assert!(toml.contains(r#"chromiumoxide = { path = "vendor/chromiumoxide" }"#), "[patch.crates-io] removed?");
}
