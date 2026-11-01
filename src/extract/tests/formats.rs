use pretty_assertions::assert_eq;

use super::{find, run};
use crate::extract::*;

const PETSTORE_JSON: &str = include_str!("../fixtures/petstore.json");
const PETSTORE_YAML: &str = include_str!("../fixtures/petstore.yaml");
const NOT_OPENAPI: &str = include_str!("../fixtures/not-openapi.yaml");
const GUIDE_ADOC: &str = include_str!("../fixtures/guide.adoc");
const GUIDE_RST: &str = include_str!("../fixtures/guide.rst");

#[test]
fn an_openapi_json_description_has_a_section_per_operation_and_schema() {
    let extracted = run(Format::OpenApi, "petstore.json", PETSTORE_JSON);

    assert_eq!(extracted.title.as_deref(), Some("Petstore"));
    assert_eq!(
        extracted
            .sections
            .iter()
            .map(|s| s.heading_path.join(" > "))
            .collect::<Vec<_>>(),
        [
            "Overview",
            "GET /pets",
            "POST /pets",
            "DELETE /pets/{petId}",
            "Schemas > Pet",
            "Schemas > Error"
        ]
    );
    assert_eq!(
        find(&extracted, "Overview").text,
        "Petstore 1.2.0\nManage pets in the store.\nServers: https://pets.example.com/v1\nTag pets: Everything about pets"
    );
    assert_eq!(
        find(&extracted, "GET /pets").text,
        "List pets\nReturns every pet, newest first.\nOperation: listPets\nTags: pets\nParameters:\n\
         - limit (query, integer int32): How many pets to return\n\
         - cursor (query, string): Opaque paging cursor\n\
         Responses:\n\
         - 200: A page of pets (application/json: array of Pet)\n\
         - default: Unexpected error (application/json: Error)"
    );
    assert_eq!(
        find(&extracted, "POST /pets").text,
        "Create a pet\nRequest body (application/json: Pet): The pet to add\nResponses:\n- 201: Created"
    );
    assert_eq!(
        find(&extracted, "DELETE /pets/{petId}").text,
        "Delete a pet\nDeprecated.\nParameters:\n- petId (path, required, string): The pet id\nResponses:\n- 204: Deleted"
    );
    assert_eq!(
        find(&extracted, "Schemas > Pet").text,
        "Pet: object\nA pet in the store.\nProperties:\n- id (integer int64, required): Unique id\n\
         - name (string, required): The pet's name\n- status (string enum [available, sold]):"
    );
    assert_eq!(find(&extracted, "GET /pets").level, 1);
    assert_eq!(find(&extracted, "Schemas > Pet").level, 2);
}

#[test]
fn a_swagger_yaml_description_reads_like_the_json_one() {
    let extracted = run(Format::OpenApi, "legacy.yaml", PETSTORE_YAML);

    assert_eq!(extracted.title.as_deref(), Some("Legacy shop"));
    assert_eq!(
        extracted
            .sections
            .iter()
            .map(|s| s.heading_path.join(" > "))
            .collect::<Vec<_>>(),
        ["Overview", "GET /orders", "POST /orders", "Schemas > Order"]
    );
    assert!(
        find(&extracted, "Overview")
            .text
            .contains("Servers: https://shop.example.com/api")
    );
    assert_eq!(
        find(&extracted, "GET /orders").text,
        "List orders\nParameters:\n- status (query, string): Filter by status\nResponses:\n- 200: The orders (array of Order)"
    );
    assert!(
        find(&extracted, "POST /orders")
            .text
            .contains("- body (body, required, Order):")
    );
    assert_eq!(
        find(&extracted, "Schemas > Order").text,
        "Order: object\nAn order.\nProperties:\n- id (integer, required): The order id\n- total (number float):"
    );
}

#[test]
fn json_and_yaml_that_are_not_openapi_are_refused_as_unsupported() {
    for (name, text) in [
        ("ci.yaml", NOT_OPENAPI),
        ("package.json", "{\"name\": \"x\", \"version\": \"1\"}"),
        ("list.json", "[1, 2]"),
    ] {
        let refusal = extract(Format::OpenApi, name, text.as_bytes()).expect_err("refused");

        assert!(
            matches!(&refusal, Refusal::Unsupported(reason) if reason.contains("OpenAPI")),
            "{name}: {refusal:?}"
        );
    }

    assert!(matches!(
        extract(Format::OpenApi, "bad.json", b"{ \"openapi\": "),
        Err(Refusal::Failed(_))
    ));
}

#[test]
fn a_yaml_alias_bomb_is_refused_quickly() {
    let mut bomb = String::from("openapi: 3.0.0\ninfo: {title: Bomb, version: '1'}\nx: &a0 [\"lol\"]\n");

    for level in 1..=9 {
        let previous = format!("*a{}", level - 1);
        let refs = vec![previous; 9].join(", ");

        bomb.push_str(&format!("x{level}: &a{level} [{refs}]\n"));
    }

    let started = std::time::Instant::now();
    let refusal = extract(Format::OpenApi, "bomb.yaml", bomb.as_bytes()).expect_err("refused");

    assert!(started.elapsed().as_secs() < 5);
    assert!(
        matches!(refusal, Refusal::Failed(ref reason) if reason.contains("YAML")),
        "{refusal:?}"
    );
}

#[test]
fn a_page_of_two_hundred_thousand_nested_tags_is_read_in_linear_time() {
    let depth = 200_000;
    let bold = "<b>".repeat(depth);
    let nested = format!(
        "<html><body><h1>Deep</h1><p>{bold}words in the middle{}</p></body></html>",
        "</b>".repeat(depth)
    );
    // End tags that close nothing, and a list item opened at the bottom.
    let strays = format!(
        "<html><body><h1>Deep</h1>{bold}<ul><li>last item</li></ul>{}</body></html>",
        "</i>".repeat(depth)
    );

    for page in [nested, strays] {
        let started = std::time::Instant::now();
        let extracted = extract(Format::Html, "deep.html", page.as_bytes()).expect("readable");

        // Debug builds tokenize 400k tags in about 0.6 s (2 s on a slow CI runner): a quadratic
        // walk takes minutes.
        assert!(started.elapsed().as_secs() < 15, "{:?}", started.elapsed());
        assert_eq!(extracted.title.as_deref(), Some("Deep"));
        assert!(
            extracted
                .sections
                .iter()
                .any(|section| section.text.contains("words in the middle") || section.text.contains("last item")),
            "{extracted:?}"
        );
    }
}

#[test]
fn asciidoc_has_headings_code_tables_and_no_attributes() {
    let extracted = run(Format::AsciiDoc, "guide.adoc", GUIDE_ADOC);

    assert_eq!(extracted.title.as_deref(), Some("Gateway Guide"));
    assert_eq!(
        extracted
            .sections
            .iter()
            .map(|s| s.heading_path.join(" > "))
            .collect::<Vec<_>>(),
        [
            "Gateway Guide",
            "Gateway Guide > Install",
            "Gateway Guide > Install > Linux",
            "Gateway Guide > Reference"
        ]
    );
    assert_eq!(
        find(&extracted, "Gateway Guide").text,
        "Jane Doe <jane@example.com>\n\nA short intro to the gateway."
    );
    assert_eq!(
        find(&extracted, "Gateway Guide > Install").text,
        "Install with the package manager.\n\n```\napt install gateway\n\ngateway --version\n```"
    );
    assert_eq!(
        find(&extracted, "Gateway Guide > Install > Linux").text,
        "* Debian\n* Ubuntu"
    );
    assert_eq!(
        find(&extracted, "Gateway Guide > Reference").text,
        "Flag | Meaning\n-v | verbose"
    );
}

#[test]
fn rst_levels_follow_the_order_adornments_first_appear() {
    let extracted = run(Format::Rst, "guide.rst", GUIDE_RST);

    assert_eq!(extracted.title.as_deref(), Some("Gateway Guide"));
    assert_eq!(
        extracted
            .sections
            .iter()
            .map(|s| (s.heading_path.join(" > "), s.level))
            .collect::<Vec<_>>(),
        [
            ("Gateway Guide".to_owned(), 1),
            ("Gateway Guide > Install".to_owned(), 2),
            ("Gateway Guide > Install > Linux".to_owned(), 3),
            ("Gateway Guide > Reference".to_owned(), 2),
        ]
    );
    assert_eq!(
        find(&extracted, "Gateway Guide > Install").text,
        "Install with the package manager\n\n```\napt install gateway\n\ngateway --version\n```\n\nNeeds root."
    );
}
