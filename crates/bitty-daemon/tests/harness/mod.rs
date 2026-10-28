//! Shared conformance harness for `bittyd` (bitty#2605).
//!
//! One machine-readable corpus (`corpus.json`) drives two drivers:
//!
//! - [`fake`], an in-process reference implementation labeled
//!   `candidate-not-bittyd`;
//! - [`real`], the built `bittyd` binary over its real Unix endpoint.
//!
//! Both drivers run the same vectors and the same typed expectations. The
//! parity test compares their observed outcomes; a fake run never marks a real
//! row passed, and the evidence files keep the two rows separate.
//!
//! The corpus is the source of truth: the parser rejects an unknown schema,
//! a missing field, an empty `forbidden` list, and a vector with no typed
//! expectation.

pub mod corpus;
pub mod evidence;
pub mod fake;
pub mod real;
pub mod runner;
