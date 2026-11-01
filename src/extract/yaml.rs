//! YAML as JSON values, for the OpenAPI extractor.
//!
//! `saphyr` does not expand aliases, so an alias bomb costs nothing here: an alias becomes `null`
//! (an OpenAPI description uses `$ref`, not YAML anchors, to share a schema). The parser runs
//! behind `catch_unwind`, so a parser bug fails the file and not the import.

use std::panic::{AssertUnwindSafe, catch_unwind};

use saphyr::{LoadableYamlNode, Scalar, Yaml};
use serde_json::{Map, Number, Value};

/// How deep a YAML value may nest.
const DEPTH_MAX: usize = 100;

/// The first document of `text`, as a JSON value.
pub fn parse(text: &str) -> Result<Value, String> {
    let parsed = catch_unwind(AssertUnwindSafe(|| Yaml::load_from_str(text)))
        .map_err(|_| "the YAML parser failed on this file".to_owned())?
        .map_err(|error| format!("the YAML is invalid: {error}"))?;

    let first = parsed.first().ok_or_else(|| "the YAML file is empty".to_owned())?;

    convert(first, 0)
}

fn convert(node: &Yaml, depth: usize) -> Result<Value, String> {
    if depth > DEPTH_MAX {
        return Err("the YAML nests too deeply".to_owned());
    }

    Ok(match node {
        Yaml::Value(scalar) => scalar_value(scalar),
        Yaml::Representation(text, _, _) => Value::String(text.to_string()),
        Yaml::Sequence(items) => Value::Array(
            items
                .iter()
                .map(|item| convert(item, depth + 1))
                .collect::<Result<_, _>>()?,
        ),
        Yaml::Mapping(entries) => {
            let mut object = Map::new();

            for (key, value) in entries {
                object.insert(key_text(key), convert(value, depth + 1)?);
            }

            Value::Object(object)
        }
        Yaml::Tagged(_, inner) => convert(inner, depth + 1)?,
        Yaml::Alias(_) | Yaml::BadValue => Value::Null,
    })
}

fn scalar_value(scalar: &Scalar) -> Value {
    match scalar {
        Scalar::Null => Value::Null,
        Scalar::Boolean(value) => Value::Bool(*value),
        Scalar::Integer(value) => Value::Number((*value).into()),
        Scalar::FloatingPoint(value) => Number::from_f64(value.0).map_or(Value::Null, Value::Number),
        Scalar::String(text) => Value::String(text.to_string()),
    }
}

/// A mapping key as the text a JSON object would have: response codes are integers in YAML (`200:`).
fn key_text(key: &Yaml) -> String {
    match key {
        Yaml::Value(Scalar::String(text)) => text.to_string(),
        Yaml::Value(Scalar::Integer(number)) => number.to_string(),
        Yaml::Value(Scalar::Boolean(flag)) => flag.to_string(),
        Yaml::Value(Scalar::FloatingPoint(number)) => number.0.to_string(),
        Yaml::Value(Scalar::Null) => "null".to_owned(),
        Yaml::Representation(text, _, _) => text.to_string(),
        _ => String::new(),
    }
}
