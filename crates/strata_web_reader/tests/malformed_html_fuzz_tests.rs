//! Malformed HTML fuzzing & mount stress tests for `strata-web-reader`
//! (migrated from `strata` core when `strata-web-reader` moved to `onyx/`).

use strata::prelude::*;
use strata_web_reader::parser::HtmlParser;
use strata_web_reader::reader_theme::ReaderTheme;
use strata_web_reader::transpiler::WebTranspiler;

#[test]
fn test_stress_web_reader_malformed_html_fuzzing() {
    let broken_html = r#"
        <!DOCTYPE html>
        <html>
        <head><title>Unclosed test
        <style>body { color: red; div { font-size: 14px;
        <body>
            <div class="outer" style="width: 100px;
                <span><p><b><i>Missing all closing tags!
                <img src="foo.png" alt="unclosed"
                <table class="wikitable"><tr><td>cell 1<td>cell 2<tr><td>cell 3
                <script>alert("should be stripped"); console.log("evil");
                <div class="nested"><div class="nested2"><div class="nested3">
                <a href="/target">link with unclosed tag
    "#;

    let nodes = HtmlParser::parse(broken_html);
    assert!(
        !nodes.is_empty(),
        "Parser must gracefully recover from broken HTML"
    );

    let theme = ReaderTheme::default();
    let transpiler = WebTranspiler::new(&theme);
    let blueprint = transpiler.transpile_document(&nodes, Some("Malformed Document"));
    assert!(matches!(blueprint, NodeBlueprint::Element { .. }));

    let view = View::new(blueprint);
    let mut runtime = AppRuntime::new(view, 1024.0, 768.0, 1.0);
    let redraw = runtime.tick_frame();
    assert!(!redraw.is_empty());
}
