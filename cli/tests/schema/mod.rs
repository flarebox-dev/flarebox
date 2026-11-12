// SPDX-License-Identifier: Apache-2.0

use std::path::Path;

use serde_json::Value;

pub fn check(envelope: &Value) {
    let command = match (envelope["command"].as_str(), envelope.get("event")) {
        (Some(command), _) => command,
        (None, Some(_)) => "_daemon",
        (None, None) => panic!("no command: {envelope:#}"),
    };
    let name = match command {
        "" => "koi.json".to_owned(),
        command if command == "help" || command.starts_with("help ") => "koi-help.json".to_owned(),
        command => format!("koi-{}.json", command.replace(' ', "-")),
    };
    let path = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../docs/json-schema/v1")
        .join(name);
    let text = std::fs::read_to_string(&path)
        .unwrap_or_else(|error| panic!("{}: {error}", path.display()));
    let mut schema: Value = serde_json::from_str(&text).unwrap();
    closed(&mut schema);
    let validator = jsonschema::validator_for(&schema).unwrap();
    let errors: Vec<String> = (validator.iter_errors(envelope))
        .map(|error| format!("{} at {}", error, error.instance_path()))
        .collect();
    assert!(
        errors.is_empty(),
        "{} refuses {envelope:#}: {errors:#?}",
        path.display()
    );
}

fn closed(schema: &mut Value) {
    close(schema, true);
}

fn close(schema: &mut Value, own: bool) {
    match schema {
        Value::Object(object) => {
            let shared = object.contains_key("properties");
            if own && shared && object.get("type") == Some(&Value::from("object")) {
                object
                    .entry("unevaluatedProperties")
                    .or_insert(Value::Bool(false));
            }
            for (key, value) in object.iter_mut() {
                let branches = matches!(key.as_str(), "oneOf" | "anyOf" | "allOf");
                close(value, !(branches && shared));
            }
        }
        Value::Array(items) => items.iter_mut().for_each(|item| close(item, own)),
        _ => {}
    }
}
