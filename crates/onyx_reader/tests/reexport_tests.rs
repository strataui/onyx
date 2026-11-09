//! Tests verifying Onyx Reader re-export parity with strata_web_reader.

use onyx_reader::{HtmlParser, NodeBlueprint, ReaderTheme, WebTranspiler};

#[test]
fn test_onyx_reader_reexports_strata_web_reader() {
    let raw = "<h1>Hello from Onyx</h1>";
    let nodes = HtmlParser::parse(raw);
    assert_eq!(nodes.len(), 1);

    let theme = ReaderTheme::dark();
    let transpiler = WebTranspiler::new(&theme);
    let blueprint = transpiler.transpile_document(&nodes, Some("Onyx"));
    assert!(matches!(blueprint, NodeBlueprint::Element { .. }));
}
