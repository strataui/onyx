//! Dynamic Interactive Event Dispatch & DOM Hit-Testing Router.
//!
//! Routes physical screen/viewport mouse events, touch interactions, text inputs,
//! and form submissions into the active tab's JavaScript runtime and W3C DOM tree.

use std::collections::BTreeMap;
use strata::prelude::*;
use strata_js::{DomNode, JsValue};
use strata_style::MediaContext;
use strata_web_reader::{ReaderTheme, WebTranspiler};

use crate::state::{BrowserState, BrowserTab};

/// W3C DOM Level 3 / UI Events compliant MouseEvent descriptor.
#[derive(Debug, Clone)]
pub struct W3cMouseEvent {
    pub event_type: String,
    pub client_x: f64,
    pub client_y: f64,
    pub screen_x: f64,
    pub screen_y: f64,
    pub page_x: f64,
    pub page_y: f64,
    pub button: i16,
    pub buttons: u16,
    pub alt_key: bool,
    pub ctrl_key: bool,
    pub meta_key: bool,
    pub shift_key: bool,
    pub detail: i32,
}

impl W3cMouseEvent {
    /// Constructs a standard primary left click mouse event.
    pub fn click(x: f64, y: f64) -> Self {
        Self {
            event_type: "click".to_string(),
            client_x: x,
            client_y: y,
            screen_x: x,
            screen_y: y,
            page_x: x,
            page_y: y,
            button: 0,
            buttons: 1,
            alt_key: false,
            ctrl_key: false,
            meta_key: false,
            shift_key: false,
            detail: 1,
        }
    }

    /// Constructs a mouse down event.
    pub fn mouse_down(x: f64, y: f64) -> Self {
        Self {
            event_type: "mousedown".to_string(),
            client_x: x,
            client_y: y,
            screen_x: x,
            screen_y: y,
            page_x: x,
            page_y: y,
            button: 0,
            buttons: 1,
            alt_key: false,
            ctrl_key: false,
            meta_key: false,
            shift_key: false,
            detail: 1,
        }
    }

    /// Constructs a mouse up event.
    pub fn mouse_up(x: f64, y: f64) -> Self {
        Self {
            event_type: "mouseup".to_string(),
            client_x: x,
            client_y: y,
            screen_x: x,
            screen_y: y,
            page_x: x,
            page_y: y,
            button: 0,
            buttons: 0,
            alt_key: false,
            ctrl_key: false,
            meta_key: false,
            shift_key: false,
            detail: 1,
        }
    }

    /// Serializes this mouse event descriptor into a standard W3C `MouseEvent` JavaScript object.
    pub fn to_js_value(&self, target_js: JsValue) -> JsValue {
        let mut map = BTreeMap::new();
        map.insert("type".to_string(), JsValue::String(self.event_type.clone()));
        map.insert("clientX".to_string(), JsValue::Number(self.client_x));
        map.insert("clientY".to_string(), JsValue::Number(self.client_y));
        map.insert("pageX".to_string(), JsValue::Number(self.page_x));
        map.insert("pageY".to_string(), JsValue::Number(self.page_y));
        map.insert("screenX".to_string(), JsValue::Number(self.screen_x));
        map.insert("screenY".to_string(), JsValue::Number(self.screen_y));
        map.insert("button".to_string(), JsValue::Number(self.button as f64));
        map.insert("buttons".to_string(), JsValue::Number(self.buttons as f64));
        map.insert("altKey".to_string(), JsValue::Bool(self.alt_key));
        map.insert("ctrlKey".to_string(), JsValue::Bool(self.ctrl_key));
        map.insert("metaKey".to_string(), JsValue::Bool(self.meta_key));
        map.insert("shiftKey".to_string(), JsValue::Bool(self.shift_key));
        map.insert("detail".to_string(), JsValue::Number(self.detail as f64));
        map.insert("bubbles".to_string(), JsValue::Bool(true));
        map.insert("cancelable".to_string(), JsValue::Bool(true));
        map.insert("defaultPrevented".to_string(), JsValue::Bool(false));
        map.insert("target".to_string(), target_js.clone());
        map.insert("currentTarget".to_string(), target_js);
        JsValue::new_object(map)
    }
}

/// Router performing physical coordinate hit-testing against DOM nodes and dispatching W3C events.
pub struct DomHitTestRouter;

impl DomHitTestRouter {
    /// Maps physical viewport coordinates to the deepest matching DOM element node in the tab.
    pub fn hit_test_tab(tab: &BrowserTab, px: f32, py: f32) -> Option<DomNode> {
        if tab.dom_nodes.is_empty() {
            return None;
        }

        let theme = ReaderTheme::light();
        let transpiler = WebTranspiler::new(&theme);
        let media_ctx = MediaContext::new(1280.0, 800.0, false);
        let bp = transpiler.transpile_full_document(&tab.dom_nodes, &media_ctx);

        let mut layout_root = strata::AppRuntime::build_layout_tree(&bp);
        layout_root.compute_layout(1280.0, 800.0);

        let path = strata::AppRuntime::hit_test_node_path(&bp, &layout_root, px, py)?;
        let (uid_opt, id_opt) = find_node_identifier_in_blueprint(&bp, &path)?;

        let engine = tab.js_engine.borrow();
        if let Some(uid) = uid_opt {
            if let Some(node) = engine.document.get_element_by_uid(uid) {
                return Some(node);
            }
        }
        if let Some(id) = id_opt {
            if let Some(node) = engine.document.get_element_by_id(&id) {
                return Some(node);
            }
        }
        None
    }

    /// Hit-tests physical window coordinates against an active AppRuntime and returns the matching DOM node.
    pub fn hit_test_runtime(
        runtime: &strata::AppRuntime,
        window_x: f32,
        window_y: f32,
        tab: &BrowserTab,
    ) -> Option<DomNode> {
        let path = strata::AppRuntime::hit_test_node_path(
            &runtime.root_blueprint,
            &runtime.layout_root,
            window_x,
            window_y,
        )?;
        let (uid_opt, id_opt) = find_node_identifier_in_blueprint(&runtime.root_blueprint, &path)?;

        let engine = tab.js_engine.borrow();
        if let Some(uid) = uid_opt {
            if let Some(node) = engine.document.get_element_by_uid(uid) {
                return Some(node);
            }
        }
        if let Some(id) = id_opt {
            if let Some(node) = engine.document.get_element_by_id(&id) {
                return Some(node);
            }
        }
        None
    }

    /// Dispatches a W3C MouseEvent at the specified coordinates, triggering inline handlers
    /// and registered event listeners on the hit DOM node (with event bubbling).
    /// Returns true if the DOM was mutated as a result of event handling.
    pub fn dispatch_mouse_event(tab: &mut BrowserTab, event: &W3cMouseEvent) -> bool {
        let hit_node = Self::hit_test_tab(tab, event.client_x as f32, event.client_y as f32);
        let target = hit_node.unwrap_or_else(|| {
            let engine = tab.js_engine.borrow();
            engine.document.body.clone()
        });

        let mutated = {
            let mut engine = tab.js_engine.borrow_mut();
            let target_val = engine.dom_node_to_js(&target);
            let ev_val = event.to_js_value(target_val);

            target.dispatch_event_with_bubbling(&event.event_type, ev_val, &mut engine);
            engine.document.has_mutated()
        };

        if mutated {
            tab.sync_from_engine();
            true
        } else {
            false
        }
    }

    /// Dispatches a W3C MouseEvent at the specified coordinates into an active Strata AppRuntime,
    /// triggering incremental relayout and dynamic hydration if any mutations occur.
    pub fn dispatch_mouse_event_to_runtime(
        tab: &mut BrowserTab,
        runtime: &mut strata::AppRuntime,
        event: &W3cMouseEvent,
    ) -> bool {
        let hit_node = Self::hit_test_runtime(runtime, event.client_x as f32, event.client_y as f32, tab)
            .or_else(|| Self::hit_test_tab(tab, event.client_x as f32, event.client_y as f32));
        let target = hit_node.unwrap_or_else(|| {
            let engine = tab.js_engine.borrow();
            engine.document.body.clone()
        });

        let mutated = {
            let mut engine = tab.js_engine.borrow_mut();
            let target_val = engine.dom_node_to_js(&target);
            let ev_val = event.to_js_value(target_val);

            target.dispatch_event_with_bubbling(&event.event_type, ev_val, &mut engine);
            engine.document.has_mutated()
        };

        if mutated {
            let hydrated = tab.hydrate_mutations_into_runtime(runtime);
            if !hydrated {
                tab.sync_from_engine();
            }
            true
        } else {
            false
        }
    }

    /// Dispatches a primary click event at the specified viewport coordinates into an active AppRuntime.
    pub fn dispatch_click_to_runtime(
        tab: &mut BrowserTab,
        runtime: &mut strata::AppRuntime,
        x: f32,
        y: f32,
    ) -> bool {
        let ev = W3cMouseEvent::click(x as f64, y as f64);
        Self::dispatch_mouse_event_to_runtime(tab, runtime, &ev)
    }

    /// Dispatches a primary click event at the specified viewport coordinates.
    pub fn dispatch_click(tab: &mut BrowserTab, x: f32, y: f32) -> bool {
        let ev = W3cMouseEvent::click(x as f64, y as f64);
        Self::dispatch_mouse_event(tab, &ev)
    }

    /// Dispatches an `input` event on a target element matching `selector_or_id`,
    /// setting its value and invoking `oninput` handlers and listeners.
    pub fn dispatch_input(tab: &mut BrowserTab, selector_or_id: &str, new_value: &str) -> bool {
        let target = {
            let engine = tab.js_engine.borrow();
            engine
                .document
                .get_element_by_id(selector_or_id)
                .or_else(|| engine.document.query_selector(selector_or_id))
        };

        let target = match target {
            Some(t) => t,
            None => return false,
        };

        let mutated = {
            let mut engine = tab.js_engine.borrow_mut();
            target.set_attribute("value", new_value);
            engine.document.mark_mutated();

            let target_val = engine.dom_node_to_js(&target);
            let mut map = BTreeMap::new();
            map.insert("type".to_string(), JsValue::String("input".to_string()));
            map.insert("data".to_string(), JsValue::String(new_value.to_string()));
            map.insert("target".to_string(), target_val.clone());
            map.insert("currentTarget".to_string(), target_val);
            map.insert("bubbles".to_string(), JsValue::Bool(true));
            map.insert("cancelable".to_string(), JsValue::Bool(false));
            let ev_val = JsValue::new_object(map);

            target.dispatch_event_with_bubbling("input", ev_val, &mut engine);
            engine.document.has_mutated()
        };

        if mutated {
            tab.sync_from_engine();
            true
        } else {
            false
        }
    }

    /// Dispatches an `input` event on a target element matching `selector_or_id` into an active AppRuntime,
    /// triggering incremental relayout and dynamic hydration without full page reload.
    pub fn dispatch_input_to_runtime(
        tab: &mut BrowserTab,
        runtime: &mut strata::AppRuntime,
        selector_or_id: &str,
        new_value: &str,
    ) -> bool {
        let target = {
            let engine = tab.js_engine.borrow();
            engine
                .document
                .get_element_by_id(selector_or_id)
                .or_else(|| engine.document.query_selector(selector_or_id))
        };

        let target = match target {
            Some(t) => t,
            None => return false,
        };

        let mutated = {
            let mut engine = tab.js_engine.borrow_mut();
            target.set_attribute("value", new_value);
            engine.document.mark_mutated();

            let target_val = engine.dom_node_to_js(&target);
            let mut map = BTreeMap::new();
            map.insert("type".to_string(), JsValue::String("input".to_string()));
            map.insert("data".to_string(), JsValue::String(new_value.to_string()));
            map.insert("target".to_string(), target_val.clone());
            map.insert("currentTarget".to_string(), target_val);
            map.insert("bubbles".to_string(), JsValue::Bool(true));
            map.insert("cancelable".to_string(), JsValue::Bool(false));
            let ev_val = JsValue::new_object(map);

            target.dispatch_event_with_bubbling("input", ev_val, &mut engine);
            engine.document.has_mutated()
        };

        if mutated {
            let hydrated = tab.hydrate_mutations_into_runtime(runtime);
            if !hydrated {
                tab.sync_from_engine();
            }
            true
        } else {
            false
        }
    }

    /// Dispatches a `submit` event on a target form element, invoking `onsubmit` handlers.
    pub fn dispatch_submit(tab: &mut BrowserTab, form_selector_or_id: Option<&str>) -> bool {
        let target = {
            let engine = tab.js_engine.borrow();
            if let Some(sel) = form_selector_or_id {
                engine
                    .document
                    .get_element_by_id(sel)
                    .or_else(|| engine.document.query_selector(sel))
            } else {
                engine.document.query_selector("form")
            }
        };

        let target = match target {
            Some(t) => t,
            None => return false,
        };

        let mutated = {
            let mut engine = tab.js_engine.borrow_mut();
            let target_val = engine.dom_node_to_js(&target);
            let mut map = BTreeMap::new();
            map.insert("type".to_string(), JsValue::String("submit".to_string()));
            map.insert("target".to_string(), target_val.clone());
            map.insert("currentTarget".to_string(), target_val);
            map.insert("bubbles".to_string(), JsValue::Bool(true));
            map.insert("cancelable".to_string(), JsValue::Bool(true));
            map.insert("defaultPrevented".to_string(), JsValue::Bool(false));
            let ev_val = JsValue::new_object(map);

            target.dispatch_event_with_bubbling("submit", ev_val, &mut engine);
            engine.document.has_mutated()
        };

        if mutated {
            tab.sync_from_engine();
            true
        } else {
            false
        }
    }
}

/// Recursively traverses a blueprint down an index path to extract the deepest element's UID and ID.
fn find_node_identifier_in_blueprint(
    root: &NodeBlueprint,
    path: &[usize],
) -> Option<(Option<u64>, Option<String>)> {
    let mut current = root;
    let mut best_uid = None;
    let mut best_id = None;

    if let NodeBlueprint::Element { attributes, .. } = current {
        for attr in attributes {
            if let strata_dom::Attribute::Property { key, value } = attr {
                if key == "data-strata-node-uid" {
                    if let Ok(u) = value.parse::<u64>() {
                        best_uid = Some(u);
                    }
                } else if key == "id" {
                    best_id = Some(value.clone());
                }
            }
        }
    }

    for &idx in path {
        match current {
            NodeBlueprint::Element { children, .. } | NodeBlueprint::Fragment(children) => {
                if let Some(child) = children.get(idx) {
                    current = child;
                    if let NodeBlueprint::Element { attributes, .. } = current {
                        for attr in attributes {
                            if let strata_dom::Attribute::Property { key, value } = attr {
                                if key == "data-strata-node-uid" {
                                    if let Ok(u) = value.parse::<u64>() {
                                        best_uid = Some(u);
                                    }
                                } else if key == "id" {
                                    best_id = Some(value.clone());
                                }
                            }
                        }
                    }
                } else {
                    break;
                }
            }
            _ => break,
        }
    }

    if best_uid.is_some() || best_id.is_some() {
        Some((best_uid, best_id))
    } else {
        None
    }
}
