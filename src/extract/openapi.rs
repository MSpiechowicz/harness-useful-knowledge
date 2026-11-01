//! OpenAPI 3 and Swagger 2 descriptions, JSON or YAML.
//!
//! The description is read as data, nothing is resolved over the network. Its sections are an
//! overview (title, version, description, servers, tags), one section per operation, headed
//! `GET /pets/{id}`, with its summary, description, parameters, request body and responses, and
//! one per entry of `components.schemas` (`definitions` in Swagger 2) under a `Schemas` heading.
//! A `$ref` is shown by the name it points at, and a local parameter or response `$ref` is followed.
//!
//! JSON or YAML that is no description (no `openapi` or `swagger` key) is [`Refusal::Unsupported`].

use serde_json::Value;

use super::{Extracted, Refusal, Result, Section, heading_text, yaml};

const METHODS: [&str; 8] = ["get", "put", "post", "delete", "options", "head", "patch", "trace"];

/// Most properties of a schema listed in its section.
const PROPERTIES_MAX: usize = 120;

/// Most characters of a schema label.
const LABEL_MAX: usize = 120;

/// True when the start of a file (lowercased) looks like an OpenAPI or Swagger description, for
/// sniffing a file without an extension.
pub fn looks_like_description(start: &str) -> bool {
    let key = |name: &str| {
        start.contains(&format!("\"{name}\"")) || start.lines().any(|line| line.starts_with(&format!("{name}:")))
    };

    (start.starts_with('{') || start.starts_with("openapi:") || start.starts_with("swagger:") || start.contains('\n'))
        && (key("openapi") || key("swagger"))
}

pub fn extract(text: &str) -> Result<Extracted> {
    let root = parse(text)?;

    if !root.is_object() || (root.get("openapi").is_none() && root.get("swagger").is_none()) {
        return Err(Refusal::unsupported(
            "not an OpenAPI description (it has no openapi or swagger key)",
        ));
    }

    let mut sections = Vec::new();
    let title = root
        .pointer("/info/title")
        .and_then(Value::as_str)
        .map(heading_text)
        .filter(|title| !title.is_empty());

    sections.extend(overview(&root));

    if let Some(paths) = root.get("paths").and_then(Value::as_object) {
        for (path, item) in paths {
            sections.extend(operations(&root, path, item));
        }
    }

    sections.extend(schemas(&root));

    Ok(Extracted {
        title,
        sections,
        ..Extracted::default()
    })
}

fn parse(text: &str) -> Result<Value> {
    let start = text.trim_start();

    if start.starts_with('{') || start.starts_with('[') {
        return serde_json::from_str(text).map_err(|_| Refusal::failed("the JSON is invalid"));
    }

    yaml::parse(text).map_err(Refusal::failed)
}

fn section(path: &[&str], text: String) -> Option<Section> {
    (!text.trim().is_empty()).then(|| Section {
        heading_path: path.iter().map(|part| heading_text(part)).collect(),
        level: path.len(),
        text: text.trim().to_owned(),
    })
}

fn overview(root: &Value) -> Option<Section> {
    let info = root.get("info");
    let mut lines: Vec<String> = Vec::new();

    let title = text_of(info, "title");
    let version = text_of(info, "version");

    if !title.is_empty() {
        lines.push(if version.is_empty() {
            title.to_owned()
        } else {
            format!("{title} {version}")
        });
    }

    lines.push(text_of(info, "description").to_owned());

    let servers: Vec<String> = match root.get("servers").and_then(Value::as_array) {
        Some(servers) => servers
            .iter()
            .filter_map(|server| server.get("url").and_then(Value::as_str))
            .map(str::to_owned)
            .collect(),
        None => swagger_host(root).into_iter().collect(),
    };

    if !servers.is_empty() {
        lines.push(format!("Servers: {}", servers.join(", ")));
    }

    for tag in root.get("tags").and_then(Value::as_array).into_iter().flatten() {
        let description = text_of(Some(tag), "description");

        if !description.is_empty() {
            lines.push(format!("Tag {}: {description}", text_of(Some(tag), "name")));
        }
    }

    section(&["Overview"], join(&lines))
}

/// `https://host/base` of a Swagger 2 description.
fn swagger_host(root: &Value) -> Option<String> {
    let host = root.get("host")?.as_str()?;
    let scheme = root
        .get("schemes")
        .and_then(Value::as_array)
        .and_then(|schemes| schemes.first())
        .and_then(Value::as_str)
        .unwrap_or("https");

    Some(format!(
        "{scheme}://{host}{}",
        root.get("basePath").and_then(Value::as_str).unwrap_or("")
    ))
}

fn operations(root: &Value, path: &str, item: &Value) -> Vec<Section> {
    let shared: Vec<&Value> = item
        .get("parameters")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .collect();
    let mut found = Vec::new();

    for method in METHODS {
        let Some(operation) = item.get(method).filter(|operation| operation.is_object()) else {
            continue;
        };

        let heading = format!("{} {path}", method.to_ascii_uppercase());
        found.extend(section(&[&heading], operation_text(root, operation, &shared)));
    }

    found
}

fn operation_text(root: &Value, operation: &Value, shared: &[&Value]) -> String {
    let mut lines = vec![
        text_of(Some(operation), "summary").to_owned(),
        text_of(Some(operation), "description").to_owned(),
    ];

    let id = text_of(Some(operation), "operationId");

    if !id.is_empty() {
        lines.push(format!("Operation: {id}"));
    }

    let tags = names(operation.get("tags"));

    if !tags.is_empty() {
        lines.push(format!("Tags: {tags}"));
    }

    if operation.get("deprecated").and_then(Value::as_bool) == Some(true) {
        lines.push("Deprecated.".to_owned());
    }

    let own = operation
        .get("parameters")
        .and_then(Value::as_array)
        .into_iter()
        .flatten();
    let parameters: Vec<String> = shared
        .iter()
        .copied()
        .chain(own)
        .map(|parameter| resolve(root, parameter))
        .filter_map(parameter_line)
        .collect();

    if !parameters.is_empty() {
        lines.push(format!("Parameters:\n{}", parameters.join("\n")));
    }

    if let Some(body) = operation.get("requestBody").map(|body| resolve(root, body)) {
        lines.push(request_body_line(body));
    }

    let responses: Vec<String> = operation
        .get("responses")
        .and_then(Value::as_object)
        .into_iter()
        .flatten()
        .map(|(code, response)| {
            let response = resolve(root, response);
            let description = text_of(Some(response), "description");
            let direct = response.get("schema").map(label);
            let types = [direct.unwrap_or_default(), media_types(response)]
                .into_iter()
                .filter(|part| !part.is_empty())
                .collect::<Vec<_>>()
                .join(", ");

            if types.is_empty() {
                format!("- {code}: {description}")
            } else {
                format!("- {code}: {description} ({types})")
            }
        })
        .collect();

    if !responses.is_empty() {
        lines.push(format!("Responses:\n{}", responses.join("\n")));
    }

    join(&lines)
}

/// A parameter as a list line: `- id (path, required, string): The pet id`.
fn parameter_line(parameter: &Value) -> Option<String> {
    let name = parameter.get("name")?.as_str()?;
    let place = text_of(Some(parameter), "in");
    let mut facts = vec![place.to_owned()];

    if parameter.get("required").and_then(Value::as_bool) == Some(true) {
        facts.push("required".to_owned());
    }

    facts.push(label(parameter.get("schema").unwrap_or(parameter)));
    facts.retain(|fact| !fact.is_empty());

    Some(format!(
        "- {name} ({}): {}",
        facts.join(", "),
        text_of(Some(parameter), "description")
    ))
}

/// `Request body (application/json: Pet): The pet to add`.
fn request_body_line(body: &Value) -> String {
    let types = media_types(body);
    let description = text_of(Some(body), "description");

    match (types.is_empty(), description.is_empty()) {
        (true, true) => "Request body".to_owned(),
        (true, false) => format!("Request body: {description}"),
        (false, true) => format!("Request body ({types})"),
        (false, false) => format!("Request body ({types}): {description}"),
    }
}

/// `application/json: Pet, text/plain: string`: the media types of a request body or response and
/// the schema of each.
fn media_types(holder: &Value) -> String {
    let types: Vec<String> = holder
        .get("content")
        .and_then(Value::as_object)
        .into_iter()
        .flatten()
        .map(|(media, content)| {
            let schema = content.get("schema").map(label).unwrap_or_default();

            if schema.is_empty() {
                media.clone()
            } else {
                format!("{media}: {schema}")
            }
        })
        .collect();

    types.join(", ")
}

fn schemas(root: &Value) -> Vec<Section> {
    let entries = root
        .pointer("/components/schemas")
        .or_else(|| root.get("definitions"))
        .and_then(Value::as_object);

    entries
        .into_iter()
        .flatten()
        .filter_map(|(name, schema)| section(&["Schemas", name], schema_text(name, schema)))
        .collect()
}

fn schema_text(name: &str, schema: &Value) -> String {
    let mut lines = vec![format!("{name}: {}", label(schema))];

    lines.push(text_of(Some(schema), "description").to_owned());

    let required: Vec<&str> = schema
        .get("required")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(Value::as_str)
        .collect();
    let properties = schema.get("properties").and_then(Value::as_object);

    if let Some(properties) = properties {
        lines.push("Properties:".to_owned());

        for (property, value) in properties.iter().take(PROPERTIES_MAX) {
            let mut facts = vec![label(value)];

            if required.contains(&property.as_str()) {
                facts.push("required".to_owned());
            }

            facts.retain(|fact| !fact.is_empty());
            lines.push(format!(
                "- {property} ({}): {}",
                facts.join(", "),
                text_of(Some(value), "description")
            ));
        }

        if properties.len() > PROPERTIES_MAX {
            lines.push(format!("- and {} more", properties.len() - PROPERTIES_MAX));
        }
    }

    join(&lines)
}

/// A short description of a schema: its reference name, or its type, format and variants.
fn label(schema: &Value) -> String {
    let text = label_inner(schema, 0);

    text.chars().take(LABEL_MAX).collect()
}

fn label_inner(schema: &Value, depth: usize) -> String {
    if let Some(reference) = schema.get("$ref").and_then(Value::as_str) {
        return reference.rsplit('/').next().unwrap_or(reference).to_owned();
    }

    for key in ["oneOf", "anyOf", "allOf"] {
        if let Some(variants) = schema.get(key).and_then(Value::as_array) {
            let parts: Vec<String> = variants.iter().map(|variant| label_inner(variant, depth + 1)).collect();

            return format!("{key}({})", parts.join(", "));
        }
    }

    let kind = schema.get("type").and_then(Value::as_str).unwrap_or("");

    match kind {
        "array" if depth < 4 => format!(
            "array of {}",
            schema
                .get("items")
                .map_or_else(String::new, |items| label_inner(items, depth + 1))
        ),
        _ => {
            let format = text_of(Some(schema), "format");
            let mut text = kind.to_owned();

            if !format.is_empty() {
                text = format!("{text} {format}");
            }

            if let Some(values) = schema.get("enum").and_then(Value::as_array) {
                let values: Vec<String> = values.iter().take(12).map(value_text).collect();
                text = format!("{text} enum [{}]", values.join(", "));
            }

            text.trim().to_owned()
        }
    }
}

fn value_text(value: &Value) -> String {
    match value {
        Value::String(text) => text.clone(),
        other => other.to_string(),
    }
}

/// The value a local `$ref` (`#/components/parameters/Id`) names, else `value` itself. A chain of
/// references is followed a few steps.
fn resolve<'a>(root: &'a Value, value: &'a Value) -> &'a Value {
    let mut current = value;

    for _ in 0..4 {
        let Some(pointer) = current
            .get("$ref")
            .and_then(Value::as_str)
            .and_then(|reference| reference.strip_prefix('#'))
        else {
            return current;
        };

        match root.pointer(pointer) {
            Some(target) => current = target,
            None => return current,
        }
    }

    current
}

/// The string at `key` of an object, empty when there is none.
fn text_of<'a>(value: Option<&'a Value>, key: &str) -> &'a str {
    value
        .and_then(|value| value.get(key))
        .and_then(Value::as_str)
        .unwrap_or("")
        .trim()
}

fn names(value: Option<&Value>) -> String {
    let names: Vec<&str> = value
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(Value::as_str)
        .collect();

    names.join(", ")
}

/// The non-empty lines, one per line.
fn join(lines: &[String]) -> String {
    lines
        .iter()
        .filter(|line| !line.trim().is_empty())
        .cloned()
        .collect::<Vec<_>>()
        .join("\n")
}
