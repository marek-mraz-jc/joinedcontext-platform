//! LLM integration via liter-llm.
//!
//! This module provides VLM OCR, VLM embeddings, and structured extraction
//! capabilities using liter-llm as the backend.

#[cfg(feature = "liter-llm")]
pub mod client;
#[cfg(feature = "liter-llm")]
pub mod prompts;
#[cfg(feature = "liter-llm")]
pub mod structured;
#[cfg(feature = "liter-llm")]
pub mod usage;
#[cfg(feature = "liter-llm")]
pub mod vlm_embeddings;
#[cfg(feature = "liter-llm")]
pub mod vlm_ocr;
