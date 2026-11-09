//! Integration tests verifying GOAL-35: Dynamic Interactive Event Dispatch & DOM Hit-Testing Router.

use strata_browser::dom_router::{DomHitTestRouter, W3cMouseEvent};
use strata_browser::state::BrowserState;
use strata_render::compositor::DrawCommand;

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

#[test]
fn test_w3c_click_event_dispatch_to_onclick_attribute() {
    let state = BrowserState::new();
    let url = "data:text/html,<!DOCTYPE html><html><body><button id=\"btn\" onclick=\"handleClick()\">Click Me</button><p id=\"counter\">Count: 0</p><script>var count = 0; function handleClick() { count++; document.getElementById('counter').textContent = 'Count: ' + count; }</script></body></html>";
    state.navigate(url);

    let active_tab = state.active_tab().expect("Active tab must exist");
    let initial_text = active_tab
        .dom_nodes
        .iter()
        .map(|n| n.text_content())
        .collect::<Vec<_>>()
        .join(" ");
    assert!(initial_text.contains("Count: 0"));

    // Find physical coordinates of the button in the rendered layout
    let mut tab = state.active_tab().unwrap();
    let hit_node = DomHitTestRouter::hit_test_tab(&tab, 30.0, 10.0);
    assert!(hit_node.is_some(), "Button at (30, 10) must be hit-tested");
    let node = hit_node.unwrap();
    assert_eq!(node.tag_name().unwrap_or_default().to_lowercase(), "button");
    assert_eq!(node.id().unwrap_or_default(), "btn");

    // Dispatch primary click at (30.0, 10.0)
    let mutated = DomHitTestRouter::dispatch_click(&mut tab, 30.0, 10.0);
    assert!(mutated, "Click must trigger DOM mutation");

    let updated_text = tab
        .dom_nodes
        .iter()
        .map(|n| n.text_content())
        .collect::<Vec<_>>()
        .join(" ");
    assert!(
        updated_text.contains("Count: 1"),
        "DOM text must reflect click increment: {updated_text}"
    );

    // Second click
    let mutated2 = DomHitTestRouter::dispatch_click(&mut tab, 30.0, 10.0);
    assert!(mutated2);
    let updated_text2 = tab
        .dom_nodes
        .iter()
        .map(|n| n.text_content())
        .collect::<Vec<_>>()
        .join(" ");
    assert!(
        updated_text2.contains("Count: 2"),
        "DOM text must reflect second click increment: {updated_text2}"
    );
}

#[test]
fn test_w3c_mouse_event_dispatch_to_add_event_listener() {
    let state = BrowserState::new();
    let url = "data:text/html,<html><body><div id=\"card\" style=\"width: 300px; height: 150px;\">Initial Card</div><script>document.getElementById('card').addEventListener('click', function(e) { document.getElementById('card').textContent = 'Position: ' + e.clientX + 'x' + e.clientY + ' btn=' + e.button; });</script></body></html>";
    state.navigate(url);

    let mut tab = state.active_tab().expect("Active tab must exist");

    // Dispatch W3C MouseEvent at (145.0, 75.0)
    let ev = W3cMouseEvent::click(145.0, 75.0);
    let mutated = DomHitTestRouter::dispatch_mouse_event(&mut tab, &ev);
    assert!(mutated, "Mouse event must trigger DOM mutation via event listener");

    let text = tab
        .dom_nodes
        .iter()
        .map(|n| n.text_content())
        .collect::<Vec<_>>()
        .join(" ");
    assert!(
        text.contains("Position: 145x75 btn=0"),
        "Event listener must receive W3C MouseEvent coordinates and properties: {text}"
    );
}

#[test]
fn test_w3c_event_bubbling_to_ancestors() {
    let state = BrowserState::new();
    let url = "data:text/html,<html><body><div id=\"ancestor-box\" style=\"width: 400px; height: 200px;\"><button id=\"nested-button\"><span>Deep Text</span></button></div><p id=\"log\">idle</p><script>document.getElementById('ancestor-box').addEventListener('click', function(e) { document.getElementById('log').textContent = 'Bubbled to ancestor from ' + e.target.tagName; });</script></body></html>";
    state.navigate(url);

    let mut tab = state.active_tab().unwrap();

    // Click on the nested button
    let hit_node = DomHitTestRouter::hit_test_tab(&tab, 25.0, 15.0);
    assert!(hit_node.is_some());

    let ev = W3cMouseEvent::click(25.0, 15.0);
    let mutated = DomHitTestRouter::dispatch_mouse_event(&mut tab, &ev);
    assert!(mutated, "Bubbled event must trigger ancestor listener and mutate DOM");

    let text = tab
        .dom_nodes
        .iter()
        .map(|n| n.text_content())
        .collect::<Vec<_>>()
        .join(" ");
    assert!(
        text.contains("Bubbled to ancestor"),
        "Event must bubble up DOM hierarchy to ancestor element: {text}"
    );
}

#[test]
fn test_input_and_form_submit_event_dispatch() {
    let state = BrowserState::new();
    let url = "data:text/html,<html><body><form id=\"search-form\" onsubmit=\"handleSubmit()\"><input id=\"q\" name=\"query\" value=\"\" oninput=\"handleInput(this.value)\" /><span id=\"indicator\">idle</span></form><script>var queryStr = ''; function handleInput(val) { queryStr = val; document.getElementById('indicator').textContent = 'Query: ' + val; } function handleSubmit() { document.getElementById('indicator').textContent = 'Submitted: ' + queryStr; }</script></body></html>";
    state.navigate(url);

    let mut tab = state.active_tab().unwrap();

    // 1. Dispatch text input event
    let mutated_input = DomHitTestRouter::dispatch_input(&mut tab, "q", "strata browser engine");
    assert!(mutated_input, "Input event must trigger oninput handler");

    let input_text = tab
        .dom_nodes
        .iter()
        .map(|n| n.text_content())
        .collect::<Vec<_>>()
        .join(" ");
    assert!(
        input_text.contains("Query: strata browser engine"),
        "DOM must reflect input event execution: {input_text}"
    );

    // 2. Dispatch form submit event
    let mutated_submit = DomHitTestRouter::dispatch_submit(&mut tab, Some("search-form"));
    assert!(mutated_submit, "Submit event must trigger onsubmit handler");

    let submit_text = tab
        .dom_nodes
        .iter()
        .map(|n| n.text_content())
        .collect::<Vec<_>>()
        .join(" ");
    assert!(
        submit_text.contains("Submitted: strata browser engine"),
        "DOM must reflect submit event execution: {submit_text}"
    );
}

#[test]
fn test_browser_state_interactive_click_updates_viewport_display_list() {
    strata_platform::init_pango_text_measurement();

    let state = BrowserState::new();
    let url = "data:text/html,<html><body><button id=\"toggle-btn\" onclick=\"handleClick()\">Activate System</button><script>var active = false; function handleClick() { active = true; document.getElementById('toggle-btn').textContent = 'System Active'; }</script></body></html>";
    state.navigate(url);

    let app_view = strata_browser::BrowserApp(state.clone());
    let mut runtime = strata::AppRuntime::new(app_view, 1280.0, 800.0, 1.0);
    runtime.layout_root.compute_layout(1280.0, 800.0);
    runtime.tick_frame();

    assert!(
        display_list_contains(&runtime.display_list, "Activate System"),
        "Initial display list must contain 'Activate System'"
    );

    // Dispatch interactive click through BrowserState at the button's viewport coordinates
    let mutated = state.dispatch_click_in_active_tab(30.0, 10.0);
    assert!(mutated, "Click must mutate active tab DOM");

    // Re-render UI and tick frame
    let updated_view = strata_browser::BrowserApp(state.clone());
    runtime.update_view(updated_view);
    runtime.layout_root.compute_layout(1280.0, 800.0);
    runtime.tick_frame();

    assert!(
        display_list_contains(&runtime.display_list, "System Active"),
        "DisplayList must reactively update with 'System Active' after click dispatch"
    );
}
