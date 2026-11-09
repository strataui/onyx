//! Integration test verifying GOAL-27: Real JavaScript execution in Strata Browser tab workers
//! and reactive display list updates.

use std::time::Duration;
use strata_browser::tab_worker::{TabWorkerJob, TabWorkerPool};
use strata_render::compositor::DrawCommand;

#[test]
fn test_js_engine_execution_updates_display_list() {
    let pool = TabWorkerPool::new(1);

    // Acceptance criteria vector: document.body.innerHTML = "<h1>Hello from JS</h1>"
    let url = "data:text/html,<!DOCTYPE html><html><body><div id=\"app\">Original Body</div><script>document.body.innerHTML = \"<h1>Hello from JS</h1>\";</script></body></html>".to_string();

    let job = TabWorkerJob::new(0, 1, url);
    assert!(pool.submit_job(job), "Failed to submit job to tab worker");

    let resp = pool
        .wait_timeout(Duration::from_secs(3))
        .expect("Tab worker timed out processing JavaScript page");

    assert_eq!(resp.status, 200);
    assert!(!resp.is_error);
    assert!(!resp.is_crashed);

    // Verify DOM nodes reflect JavaScript mutation
    assert!(
        !resp.dom_nodes.is_empty(),
        "Mutated DOM nodes must be present"
    );
    let all_text = resp
        .dom_nodes
        .iter()
        .map(|n| n.text_content())
        .collect::<Vec<_>>()
        .join(" ");

    assert!(
        all_text.contains("Hello from JS"),
        "DOM text must reflect JS mutation: {all_text}"
    );

    // Verify zero-copy compositor display list was committed with updated text
    let ring_buffer = pool
        .get_ring_buffer(0)
        .expect("Failed to get ring buffer for worker 0");

    let (_seq, frame) = ring_buffer
        .acquire_latest(0)
        .expect("Expected committed frame in ShmRingBuffer");

    let mut found_js_text = false;
    for cmd in &frame.display_list.commands {
        if let DrawCommand::DrawText { text, .. } = cmd {
            if text.contains("Hello from JS") {
                found_js_text = true;
                break;
            }
        }
    }

    assert!(
        found_js_text,
        "Compositor DisplayList must contain 'Hello from JS' from executed script: {:?}",
        frame.display_list.commands
    );
}

#[test]
fn test_js_engine_dom_mutation_via_get_element_by_id() {
    let pool = TabWorkerPool::new(1);

    let url = "data:text/html,<html><body><h1 id=\"title\">Initial</h1><p id=\"status\">idle</p><script>document.getElementById(\"status\").textContent = \"active\";</script></body></html>".to_string();

    let job = TabWorkerJob::new(1, 2, url);
    assert!(pool.submit_job(job));

    let resp = pool
        .wait_timeout(Duration::from_secs(3))
        .expect("Tab worker timed out");

    assert_eq!(resp.status, 200);
    let all_text = resp
        .dom_nodes
        .iter()
        .map(|n| n.text_content())
        .collect::<Vec<_>>()
        .join(" ");

    assert!(all_text.contains("active"), "DOM must have text 'active' from JS: {all_text}");
}

#[test]
fn test_tab_active_js_engine_eval_and_dom_mutation() {
    let state = strata_browser::BrowserState::new();
    let url = "data:text/html,<html><body><h1 id=\"title\">Initial Title</h1><p id=\"counter\">0</p></body></html>";
    state.navigate(url);

    let active_tab = state.active_tab().expect("Active tab must exist");
    assert_eq!(active_tab.url, url);

    // Initial check on DOM text
    let initial_text = active_tab
        .dom_nodes
        .iter()
        .map(|n| n.text_content())
        .collect::<Vec<_>>()
        .join(" ");
    assert!(initial_text.contains("Initial Title"));
    assert!(initial_text.contains("0"));

    let gen_before = active_tab.generation;

    // Evaluate script modifying DOM via active tab's JsEngine
    let res = state
        .eval_js_in_active_tab("document.getElementById('counter').textContent = '999'; 'counter updated';")
        .expect("JS evaluation should succeed");

    assert_eq!(res, strata_browser::JsValue::String("counter updated".to_string()));

    let updated_tab = state.active_tab().expect("Active tab must exist");
    assert!(updated_tab.generation > gen_before, "Generation should increment after DOM mutation");

    let updated_text = updated_tab
        .dom_nodes
        .iter()
        .map(|n| n.text_content())
        .collect::<Vec<_>>()
        .join(" ");
    assert!(updated_text.contains("999"), "DOM text must reflect JS mutation: {updated_text}");
    assert!(updated_tab.html_content.contains("999"), "HTML content must reflect JS mutation");
}

#[test]
fn test_tab_js_engine_reactive_display_list_updates() {
    strata_platform::init_pango_text_measurement();

    let state = strata_browser::BrowserState::new();
    let url = "data:text/html,<html><body><h1 id=\"heading\">Static Baseline</h1></body></html>";
    state.navigate(url);

    let app_view = strata_browser::BrowserApp(state.clone());
    let mut runtime = strata::AppRuntime::new(app_view, 1280.0, 800.0, 1.0);
    runtime.layout_root.compute_layout(1280.0, 800.0);
    runtime.tick_frame();

    fn display_list_contains(dl: &strata_render::compositor::DisplayList, query: &str) -> bool {
        for cmd in &dl.commands {
            match cmd {
                DrawCommand::DrawText { text, .. } if text.contains(query) => return true,
                DrawCommand::DrawTextFragments { fragments } => {
                    for f in fragments {
                        if f.text.contains(query) {
                            return true;
                        }
                    }
                }
                _ => {}
            }
        }
        false
    }

    assert!(
        display_list_contains(&runtime.display_list, "Static Baseline"),
        "Initial display list must contain 'Static Baseline'"
    );

    // Mutate DOM dynamically via active tab's JsEngine
    state
        .eval_js_in_active_tab("document.getElementById('heading').textContent = 'Reactive Dynamic Mutation';")
        .expect("DOM mutation script should evaluate cleanly");

    // Re-render UI and tick frame
    let updated_view = strata_browser::BrowserApp(state.clone());
    runtime.update_view(updated_view);
    runtime.layout_root.compute_layout(1280.0, 800.0);
    runtime.tick_frame();

    let has_mutated_text = display_list_contains(&runtime.display_list, "Reactive Dynamic Mutation");

    assert!(
        has_mutated_text,
        "DisplayList must reactively update with mutated text from tab JsEngine"
    );
}

#[test]
fn test_devtools_console_repl_javascript_execution() {
    let state = strata_browser::BrowserState::new();
    let url = "data:text/html,<html><body><div id=\"message\">Welcome</div></body></html>";
    state.navigate(url);

    // 1. Evaluate an arithmetic and logging expression
    state.execute_console_command("console.log('Console pipeline live'); 15 * 3");

    let logs = state.devtools_console_logs.get();
    assert!(
        logs.iter().any(|l| l.source == "console" && l.message.contains("Console pipeline live")),
        "Console logs must include console.log message: {logs:?}"
    );
    assert!(
        logs.iter().any(|l| l.source == "repl" && l.message == "45"),
        "REPL logs must include evaluated result '45': {logs:?}"
    );

    // 2. Evaluate DOM mutation through DevTools console
    state.execute_console_command("document.getElementById('message').textContent = 'Updated via DevTools';");

    let tab = state.active_tab().expect("Active tab must exist");
    let tab_text = tab
        .dom_nodes
        .iter()
        .map(|n| n.text_content())
        .collect::<Vec<_>>()
        .join(" ");
    assert!(
        tab_text.contains("Updated via DevTools"),
        "DOM must be updated from DevTools console execution: {tab_text}"
    );
}

#[test]
fn test_multi_tab_js_engine_isolation() {
    let state = strata_browser::BrowserState::new();

    // Tab 1
    state.navigate("data:text/html,<html><body><h1 id=\"t1\">Tab One</h1></body></html>");
    let tab1_id = state.active_tab_id.get();
    state
        .eval_js_in_tab(tab1_id, "var secretToken = 'tab1_secret_9999'; secretToken")
        .expect("Tab 1 eval should succeed");

    // Tab 2
    state.new_tab("data:text/html,<html><body><h1 id=\"t2\">Tab Two</h1></body></html>");
    let tab2_id = state.active_tab_id.get();
    assert_ne!(tab1_id, tab2_id);

    // Evaluate in Tab 2: secretToken should be undefined
    let res = state
        .eval_js_in_tab(tab2_id, "typeof secretToken")
        .expect("Tab 2 eval should succeed");

    assert_eq!(
        res,
        strata_browser::JsValue::String("undefined".to_string()),
        "Tab 2 runtime must be isolated and not leak variables from Tab 1"
    );

    // Verify Tab 1 still holds secretToken
    let res1 = state
        .eval_js_in_tab(tab1_id, "secretToken")
        .expect("Tab 1 eval should succeed");
    assert_eq!(
        res1,
        strata_browser::JsValue::String("tab1_secret_9999".to_string()),
        "Tab 1 must maintain its own scope independently"
    );
}

#[test]
fn test_external_script_bundle_execution() {
    let script_url = "https://cdn.strataui.org/app_bundle.js";
    let script_code = "document.getElementById('dyn').textContent = 'Hydrated by External Bundle';";
    strata_browser::network::register_script_cache(script_url, script_code);

    let html = format!(
        r#"data:text/html,<!DOCTYPE html><html><body><h1 id="dyn">Initial Header</h1><script src="{script_url}"></script></body></html>"#
    );

    let tab = strata_browser::state::BrowserTab::new(10, &html);
    let all_text = tab
        .dom_nodes
        .iter()
        .map(|n| n.text_content())
        .collect::<Vec<_>>()
        .join(" ");

    assert!(
        all_text.contains("Hydrated by External Bundle"),
        "External script bundle must execute and mutate DOM: {all_text}"
    );
}

#[test]
fn test_content_security_policy_enforcement_in_browser() {
    let html = r#"data:text/html,<!DOCTYPE html><html><head><meta http-equiv="Content-Security-Policy" content="script-src 'self' 'nonce-allowedNonce'"></head><body><p id="unauth">Unmodified</p><p id="auth">Unmodified</p><script>document.getElementById('unauth').textContent = 'Injected';</script><script nonce="allowedNonce">document.getElementById('auth').textContent = 'Secure';</script></body></html>"#;

    let mut tab = strata_browser::state::BrowserTab::new(11, html);
    assert!(tab.csp.is_some(), "Tab must detect and parse meta CSP");

    let all_text = tab
        .dom_nodes
        .iter()
        .map(|n| n.text_content())
        .collect::<Vec<_>>()
        .join(" ");

    // Unauthorized script without nonce must be blocked by CSP
    assert!(
        !all_text.contains("Injected"),
        "Unauthorized inline script must be blocked by CSP: {all_text}"
    );

    // Authorized script with nonce must execute
    assert!(
        all_text.contains("Secure"),
        "Authorized script with nonce must execute: {all_text}"
    );

    // eval() must be blocked when script-src lacks 'unsafe-eval'
    let eval_res = tab.eval_js("1 + 1");
    assert!(
        eval_res.is_err(),
        "eval() must fail when disallowed by Content Security Policy"
    );
    let err_msg = eval_res.unwrap_err();
    assert!(
        err_msg.contains("Content Security Policy violation"),
        "Error must clearly indicate CSP violation: {err_msg}"
    );
}

