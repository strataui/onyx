//! # Onyx Reader (Public Compatibility Facade)
//!
//! `onyx_reader` is an intentional, zero-overhead consumer-facing facade crate
//! providing unified access to semantic web extraction, HTML5 document parsing,
//! Content Security Policy (CSP) enforcement, and reader mode transpilation
//! powered directly by Strata's authoritative `strata_web_reader` engine.
//!
//! ## Architectural Role
//!
//! Rather than duplicating web parsing algorithms, this facade unifies consumer
//! imports across the Onyx sovereign browser workspace, allowing browser components
//! to interact with `HtmlParser`, `WebTranspiler`, `ReaderTheme`, and `CspPolicy`
//! via the canonical `onyx_reader` namespace.
//!
//! All DOM construction, article scoring heuristics, and WHATWG HTML5 tokenization
//! execute natively within `strata_web_reader`.

pub use strata_web_reader::*;
