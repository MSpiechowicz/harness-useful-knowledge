//! The engine without the real model: a tiny word-level tokenizer and an ONNX graph built here
//! (hidden state = token id and mask scaled per dimension), so the prefixes, the truncation, the
//! input count read from the graph, the pooling and the shape check run through the real tract code.

use std::str::FromStr;

use tokenizers::Tokenizer;
use tract_onnx::pb::tensor_shape_proto::{Dimension, dimension};
use tract_onnx::pb::{
    AttributeProto, GraphProto, ModelProto, NodeProto, OperatorSetIdProto, TensorProto, TensorShapeProto, TypeProto,
    ValueInfoProto, tensor_proto::DataType, type_proto,
};
use tract_onnx::prelude::*;

use super::support::DIMS;
use crate::embed::engine::{Embed, Encoded, Encoder, Kind, TEXT_MAX, Tract, assemble, pooled};
use crate::embed::manifest::{MODEL, Spec};

/// Ids: `<s>` 0, `<pad>` 1, `</s>` 2, `<unk>` 3, `query` 4, `passage` 5, `:` 6, `word` 7, `alpha` 8.
fn tokenizer() -> Tokenizer {
    let special =
        |name: &str, id: u32| format!("\"{name}\":{{\"id\":\"{name}\",\"ids\":[{id}],\"tokens\":[\"{name}\"]}}");

    let json = format!(
        r#"{{
            "version":"1.0","truncation":null,"padding":null,"added_tokens":[],"normalizer":null,
            "pre_tokenizer":{{"type":"Whitespace"}},
            "post_processor":{{"type":"TemplateProcessing",
                "single":[{{"SpecialToken":{{"id":"<s>","type_id":0}}}},{{"Sequence":{{"id":"A","type_id":0}}}},{{"SpecialToken":{{"id":"</s>","type_id":0}}}}],
                "pair":[{{"Sequence":{{"id":"A","type_id":0}}}},{{"Sequence":{{"id":"B","type_id":0}}}}],
                "special_tokens":{{{},{}}}}},
            "decoder":null,
            "model":{{"type":"WordLevel","unk_token":"<unk>","vocab":{{
                "<s>":0,"<pad>":1,"</s>":2,"<unk>":3,"query":4,"passage":5,":":6,"word":7,"alpha":8}}}}
        }}"#,
        special("<s>", 0),
        special("</s>", 2)
    );

    Tokenizer::from_str(&json).expect("the test tokenizer")
}

fn encoder(max_tokens: usize) -> Encoder {
    Encoder::new(tokenizer(), max_tokens).expect("encoder")
}

#[test]
fn a_query_and_a_passage_get_their_own_prefix() {
    let encoder = encoder(512);

    assert_eq!(Kind::Query.prefix(), "query: ");
    assert_eq!(Kind::Passage.prefix(), "passage: ");

    let query = encoder.encode(Kind::Query, "word alpha").expect("query");
    assert_eq!(query.ids, vec![0, 4, 6, 7, 8, 2], "<s> query : word alpha </s>");
    assert_eq!(query.mask, vec![1; 6]);

    let passage = encoder.encode(Kind::Passage, "word alpha").expect("passage");
    assert_eq!(passage.ids, vec![0, 5, 6, 7, 8, 2], "<s> passage : word alpha </s>");
}

#[test]
fn a_text_over_the_limit_is_cut_and_keeps_its_end_marker() {
    let long = "word ".repeat(2000);
    let cut = encoder(512).encode(Kind::Passage, &long).expect("long text");

    assert_eq!(cut.ids.len(), 512);
    assert_eq!(cut.mask.len(), 512);
    assert_eq!(cut.ids.first(), Some(&0));
    assert_eq!(cut.ids.last(), Some(&2), "the end marker survives the cut");
    assert_eq!(&cut.ids[1..4], &[5, 6, 7], "the prefix is read first");

    let shorter = encoder(16).encode(Kind::Query, &long).expect("limit of 16");
    assert_eq!(shorter.ids.len(), 16);
}

#[test]
fn a_text_is_clipped_in_characters_before_it_is_tokenized() {
    // 5 characters per word: the clip leaves TEXT_MAX / 5 words, far under the 512-token limit.
    let long = "word ".repeat(TEXT_MAX);
    let encoded = encoder(100_000).encode(Kind::Passage, &long).expect("encode");

    assert!(encoded.ids.len() <= TEXT_MAX / 5 + 5, "{}", encoded.ids.len());
}

#[test]
fn a_graph_gets_two_or_three_inputs_as_it_has() {
    let encoded = Encoded {
        ids: vec![0, 7, 2],
        mask: vec![1, 1, 1],
    };

    let two = assemble(2, &encoded).expect("two inputs");
    assert_eq!(two.len(), 2);

    let three = assemble(3, &encoded).expect("three inputs");
    assert_eq!(three.len(), 3);
    assert_eq!(three[2].shape(), &[1, 3], "the segment ids have the text's length");
    assert!(
        three[2]
            .to_plain_array_view::<i64>()
            .expect("i64")
            .iter()
            .all(|id| *id == 0)
    );

    assert!(assemble(1, &encoded).is_err());
    assert!(assemble(4, &encoded).is_err());
}

#[test]
fn pooling_is_the_mean_of_the_unmasked_tokens_scaled_to_length_one() {
    // Three tokens of two numbers; the middle one is padding and must not count.
    let hidden = [3.0, 4.0, 100.0, 100.0, 0.0, 0.0];
    let vector = pooled(&hidden, 2, &[1, 0, 1]);

    assert!(
        (vector[0] - 0.6).abs() < 1e-6 && (vector[1] - 0.8).abs() < 1e-6,
        "{vector:?}"
    );

    let length = vector.iter().map(|value| value * value).sum::<f32>().sqrt();
    assert!((length - 1.0).abs() < 1e-6);

    assert!(
        pooled(&[0.0, 0.0], 2, &[1]).iter().all(|value| value.is_finite()),
        "a zero vector stays finite"
    );
}

fn spec() -> Spec {
    Spec {
        dims: DIMS,
        ..MODEL.spec()
    }
}

fn info(name: &str, kind: DataType, dims: &[&str]) -> ValueInfoProto {
    let dim = |label: &str| Dimension {
        denotation: String::new(),
        value: Some(match label.parse::<i64>() {
            Ok(number) => dimension::Value::DimValue(number),
            Err(_) => dimension::Value::DimParam(label.to_owned()),
        }),
    };

    ValueInfoProto {
        name: name.to_owned(),
        r#type: Some(TypeProto {
            denotation: String::new(),
            value: Some(type_proto::Value::TensorType(type_proto::Tensor {
                elem_type: kind as i32,
                shape: Some(TensorShapeProto {
                    dim: dims.iter().map(|label| dim(label)).collect(),
                }),
            })),
        }),
        doc_string: String::new(),
    }
}

fn node(op: &str, inputs: &[&str], output: &str, attribute: Vec<AttributeProto>) -> NodeProto {
    NodeProto {
        input: inputs.iter().map(|name| (*name).to_owned()).collect(),
        output: vec![output.to_owned()],
        name: output.to_owned(),
        op_type: op.to_owned(),
        attribute,
        ..Default::default()
    }
}

fn weights(name: &str, values: Vec<f32>) -> TensorProto {
    TensorProto {
        name: name.to_owned(),
        dims: vec![values.len() as i64],
        data_type: DataType::Float as i32,
        float_data: values,
        ..Default::default()
    }
}

/// `hidden[t][d] = ids[t] * (d + 1) + mask[t] / (d + 1)`, with the segment ids (all zero) added in
/// when the graph has them.
fn graph(inputs: usize) -> InferenceModel {
    let names = ["input_ids", "attention_mask", "token_type_ids"];
    let scale: Vec<f32> = (0..DIMS).map(|d| (d + 1) as f32).collect();
    let inverse: Vec<f32> = scale.iter().map(|value| 1.0 / value).collect();

    let cast = AttributeProto {
        name: "to".to_owned(),
        r#type: 2,
        i: DataType::Float as i64,
        ..Default::default()
    };
    let axes = AttributeProto {
        name: "axes".to_owned(),
        r#type: 7,
        ints: vec![2],
        ..Default::default()
    };

    let mut nodes = Vec::new();
    let mut parts = Vec::new();

    for (index, name) in names.iter().take(inputs).enumerate() {
        let weight = if index == 1 { "inverse" } else { "scale" };

        nodes.push(node("Cast", &[name], &format!("{name}_f"), vec![cast.clone()]));
        nodes.push(node(
            "Unsqueeze",
            &[&format!("{name}_f")],
            &format!("{name}_u"),
            vec![axes.clone()],
        ));
        nodes.push(node(
            "Mul",
            &[&format!("{name}_u"), weight],
            &format!("{name}_p"),
            Vec::new(),
        ));
        parts.push(format!("{name}_p"));
    }

    nodes.push(node("Add", &[&parts[0], &parts[1]], "sum", Vec::new()));

    if inputs == 3 {
        nodes.push(node("Add", &["sum", &parts[2]], "last_hidden_state", Vec::new()));
    } else {
        nodes.push(node("Add", &["sum", "zero"], "last_hidden_state", Vec::new()));
    }

    let proto = ModelProto {
        ir_version: 7,
        opset_import: vec![OperatorSetIdProto {
            domain: String::new(),
            version: 11,
        }],
        graph: Some(GraphProto {
            name: "fake-e5".to_owned(),
            node: nodes,
            initializer: vec![
                weights("scale", scale),
                weights("inverse", inverse),
                weights("zero", vec![0.0; DIMS]),
            ],
            input: names
                .iter()
                .take(inputs)
                .map(|name| info(name, DataType::Int64, &["batch_size", "sequence_length"]))
                .collect(),
            output: vec![info(
                "last_hidden_state",
                DataType::Float,
                &["batch_size", "sequence_length", &DIMS.to_string()],
            )],
            ..Default::default()
        }),
        ..Default::default()
    };

    tract_onnx::onnx()
        .model_for_proto_model(&proto)
        .expect("the test graph")
}

/// What the graph's output is for `encoded`, pooled by hand.
fn expected(encoded: &Encoded) -> Vec<f32> {
    let mut hidden = Vec::new();

    for (id, bit) in encoded.ids.iter().zip(&encoded.mask) {
        for d in 0..DIMS {
            hidden.push(*id as f32 * (d + 1) as f32 + *bit as f32 / (d + 1) as f32);
        }
    }

    pooled(&hidden, DIMS, &encoded.mask)
}

fn close(left: &[f32], right: &[f32]) -> bool {
    left.len() == right.len() && left.iter().zip(right).all(|(a, b)| (a - b).abs() < 1e-4)
}

#[test]
fn the_engine_reads_the_input_count_from_the_graph_and_pools_the_output() {
    let encoder = encoder(512);

    for inputs in [2, 3] {
        let engine = Tract::build(tokenizer(), graph(inputs), &spec()).expect("build the engine");
        assert_eq!(engine.inputs(), inputs);

        for (kind, text) in [(Kind::Query, "word alpha word"), (Kind::Passage, "alpha")] {
            let vector = engine.embed(kind, text).expect("embed");
            let wanted = expected(&encoder.encode(kind, text).expect("encode"));

            assert!(close(&vector, &wanted), "{inputs} inputs: {vector:?} vs {wanted:?}");
            assert!((vector.iter().map(|v| v * v).sum::<f32>() - 1.0).abs() < 1e-4);
        }
    }
}

#[test]
fn a_query_and_a_passage_with_the_same_words_have_different_vectors() {
    let engine = Tract::build(tokenizer(), graph(3), &spec()).expect("build the engine");

    let query = engine.embed(Kind::Query, "word alpha").expect("query");
    let passage = engine.embed(Kind::Passage, "word alpha").expect("passage");

    assert!(!close(&query, &passage));
}

#[test]
fn a_model_with_another_dimension_than_its_spec_is_refused_at_the_first_text() {
    let wrong = Spec {
        dims: DIMS + 1,
        ..spec()
    };

    let engine = Tract::build(tokenizer(), graph(3), &wrong).expect("build the engine");

    assert!(
        engine
            .embed(Kind::Query, "word")
            .expect_err("shape")
            .contains("unexpected shape")
    );
}

#[test]
fn threads_share_one_loaded_plan_and_get_the_same_vectors() {
    let engine = std::sync::Arc::new(Tract::build(tokenizer(), graph(3), &spec()).expect("build the engine"));
    let alone = engine.embed(Kind::Passage, "word alpha word alpha").expect("embed");

    let threads: Vec<_> = (0..8)
        .map(|_| {
            let engine = std::sync::Arc::clone(&engine);

            std::thread::spawn(move || {
                (0..50)
                    .map(|_| engine.embed(Kind::Passage, "word alpha word alpha").expect("embed"))
                    .collect::<Vec<_>>()
            })
        })
        .collect();

    for thread in threads {
        for vector in thread.join().expect("thread") {
            assert_eq!(vector, alone, "a shared plan gives every thread the same bits");
        }
    }
}
