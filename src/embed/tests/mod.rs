//! Tests of the embedder with a fake engine (vectors from hashed words), a local HTTP server for
//! the downloads and real sockets. The real model has its own `#[ignore]`d tests, and a small ONNX
//! graph built in the test checks the tract engine without it.

mod adapter;
mod download;
mod engine;
mod pool;
mod real;
mod service;
mod socket;
mod support;
