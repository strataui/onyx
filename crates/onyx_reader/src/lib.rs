//! # Onyx Reader (Public Compatibility Facade — Deprecated Alias)
//!
//! `onyx_reader` is an intentional, zero-overhead consumer-facing facade crate
//! providing unified access to semantic web extraction, HTML5 document parsing,
//! Content Security Policy (CSP) enforcement, and reader mode transpilation
//! powered directly by Strata's authoritative `strata_web_reader` engine.
//!
//! ## Architectural Role & Deprecation Notice
//!
//! Rather than duplicating web parsing algorithms, this facade unifies consumer
//! imports across legacy Onyx browser configurations. Downstream components and
//! browser crates should consume `strata_web_reader` directly.
//!
//! All DOM construction, article scoring heuristics, and WHATWG HTML5 tokenization
//! execute natively within `strata_web_reader`.

pub use strata_web_reader::*;
