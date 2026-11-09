//! Integration tests verifying GOAL-19: Support Live Client-Side DOM Mutation and Dynamic Hydration in Onyx.

use strata_browser::dom_router::DomHitTestRouter;
use strata_browser::state::BrowserTab;
use strata_dom::View;
use strata_style::MediaContext;
use strata_web_reader::{ReaderTheme, WebTranspiler};

#[test]
fn test_dispatch_click_to_runtime_incremental_hydration() {
    strata_platform::init_pango_text_measurement();

    let url = "data:text/html,<!DOCTYPE html><html><body><button id=\"btn\" onclick=\"handleClick()\">Increment</button><p id=\"count\">Count: 0</p><script>var c = 0; function handleClick() { c++; document.getElementById('count').textContent = 'Count: ' + c; }</script></body></html>";
    let mut tab = BrowserTab::new(1, url);

    // Build initial layout tree from tab DOM nodes
    let theme = ReaderTheme::light();
    let transpiler = WebTranspiler::new(&theme);
    let media_ctx = MediaContext::new(1280.0, 800.0, false);
    let bp = transpiler.transpile_full_document(&tab.dom_nodes, &media_ctx);

    let mut runtime = strata::AppRuntime::new(View::new(bp), 1280.0, 800.0, 1.0);
    runtime.layout_root.compute_layout(1280.0, 800.0);
    runtime.tick_frame();

    let count_uid = {
        let engine = tab.js_engine.borrow();
        engine.document.get_element_by_id("count").unwrap().uid()
    };
    let count_path = runtime
        .root_blueprint
        .find_path_by_dom_uid(count_uid)
        .expect("Count node must have path in blueprint");

    let initial_text = {
        let node = strata::AppRuntime::find_layout_node_mut(&mut runtime.layout_root, &count_path).unwrap();
        node.children.first().and_then(|c| c.text.clone()).or_else(|| node.text.clone()).unwrap_or_default()
    };
    assert_eq!(initial_text, "Count: 0");

    // Dispatch click into AppRuntime directly
    let mutated = DomHitTestRouter::dispatch_click_to_runtime(&mut tab, &mut runtime, 30.0, 10.0);
    assert!(mutated, "Click must mutate DOM and trigger hydration");

    // Verify layout node text updated in-place without page reload or update_view
    let updated_node = strata::AppRuntime::find_layout_node_mut(&mut runtime.layout_root, &count_path).unwrap();
    assert_eq!(
        updated_node.text.as_deref(),
        Some("Count: 1"),
        "LayoutNode text must incrementally hydrate to Count: 1"
    );
}

#[test]
fn test_dispatch_input_to_runtime_incremental_hydration() {
    strata_platform::init_pango_text_measurement();

    let url = "data:text/html,<!DOCTYPE html><html><body><input id=\"search-input\" value=\"\"><p id=\"mirror\">Query: </p><script>function onInput(e) { document.getElementById('mirror').textContent = 'Query: ' + e.target.value; } document.getElementById('search-input').addEventListener('input', onInput);</script></body></html>";
    let mut tab = BrowserTab::new(2, url);

    let theme = ReaderTheme::light();
    let transpiler = WebTranspiler::new(&theme);
    let media_ctx = MediaContext::new(1280.0, 800.0, false);
    let bp = transpiler.transpile_full_document(&tab.dom_nodes, &media_ctx);

    let mut runtime = strata::AppRuntime::new(View::new(bp), 1280.0, 800.0, 1.0);
    runtime.layout_root.compute_layout(1280.0, 800.0);
    runtime.tick_frame();

    let mirror_uid = {
        let engine = tab.js_engine.borrow();
        engine.document.get_element_by_id("mirror").unwrap().uid()
    };
    let mirror_path = runtime
        .root_blueprint
        .find_path_by_dom_uid(mirror_uid)
        .expect("Mirror path must exist in blueprint");

    // Dispatch input event to active AppRuntime
    let mutated = DomHitTestRouter::dispatch_input_to_runtime(
        &mut tab,
        &mut runtime,
        "search-input",
        "Autonomous Flight Platform",
    );
    assert!(mutated, "Input must mutate DOM and trigger hydration");

    let mirror_node = strata::AppRuntime::find_layout_node_mut(&mut runtime.layout_root, &mirror_path).unwrap();
    assert_eq!(
        mirror_node.text.as_deref(),
        Some("Query: Autonomous Flight Platform"),
        "Live input dispatch must hydrate layout node text incrementally"
    );
}

#[test]
fn test_eval_js_with_runtime_incremental_hydration() {
    strata_platform::init_pango_text_measurement();

    let url = "data:text/html,<!DOCTYPE html><html><body><div id=\"feed\" class=\"flex flex-col\"><div id=\"item-1\">Telemetry Log 1</div></div></body></html>";
    let mut tab = BrowserTab::new(3, url);

    let theme = ReaderTheme::light();
    let transpiler = WebTranspiler::new(&theme);
    let media_ctx = MediaContext::new(1280.0, 800.0, false);
    let bp = transpiler.transpile_full_document(&tab.dom_nodes, &media_ctx);

    let mut runtime = strata::AppRuntime::new(View::new(bp), 1280.0, 800.0, 1.0);
    runtime.layout_root.compute_layout(1280.0, 800.0);
    runtime.tick_frame();

    let feed_uid = {
        let engine = tab.js_engine.borrow();
        engine.document.get_element_by_id("feed").unwrap().uid()
    };
    let feed_path = runtime
        .root_blueprint
        .find_path_by_dom_uid(feed_uid)
        .expect("Feed path must exist in blueprint");

    let initial_count = {
        let node = strata::AppRuntime::find_layout_node_mut(&mut runtime.layout_root, &feed_path).unwrap();
        node.children.len()
    };
    assert_eq!(initial_count, 1);

    // Evaluate dynamic JS script appending item-2
    let script = r#"
        const item = document.createElement("div");
        item.setAttribute("id", "item-2");
        item.textContent = "Telemetry Log 2";
        document.getElementById("feed").appendChild(item);
    "#;
    let res = tab.eval_js_with_runtime(script, &mut runtime);
    assert!(res.is_ok());
    let (_val, mutated) = res.unwrap();
    assert!(mutated);

    let feed_layout = strata::AppRuntime::find_layout_node_mut(&mut runtime.layout_root, &feed_path).unwrap();
    assert_eq!(
        feed_layout.children.len(),
        2,
        "Feed layout node must contain 2 items after incremental hydration"
    );
}
