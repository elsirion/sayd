//! sayd's engine: queue, chunking, text cleanup, config and the state
//! machine that drives them.
//!
//! This crate deliberately depends on neither ONNX nor an audio backend nor
//! D-Bus. Synthesis reaches it through the `Synthesizer` trait and audio
//! through `AudioSink`, so the whole engine can be driven in a unit test.

pub mod audio;
pub mod config;
pub mod cleanup;
pub mod chunk;
pub mod synth;
pub mod queue;
pub mod engine;
pub mod handle;
