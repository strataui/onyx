# Onyx

High-performance, sovereign Zero-Chromium native web browser built in pure Rust on Strata and Mica.

## Overview

Onyx is a sovereign, memory-safe, zero-Chromium web browser engine engineered from first principles in 100% pure Rust. Rather than wrapping CEF, WebKit, Chromium, or system WebViews, Onyx renders web documents using Strata's subpixel layout pipeline and executes dynamic scripts through the Mica pure-Rust ECMAScript runtime.

By eliminating Google Chromium, Onyx eliminates hundreds of megabytes of process bloat, reduces cold boot latency from seconds to milliseconds, and provides absolute memory safety with zero C/C++ dependencies.

## Architectural Capabilities

- **Zero Chromium / Zero WebKit:** Pure standard library Rust layout and GPU rendering.
- **Pure-Rust Networking & TLS:** Fully encrypted HTTP/1.1 and HTTP/2 transport via `rustls` 0.23 with verified Mozilla trust anchors (`webpki-roots`).
- **Semantic HTML & CSS Transpiler (`onyx_reader`):** Real-time conversion of HTML DOM trees and CSS stylesheets into Strata RSX render blueprints.
- **Mica ECMAScript Integration:** Safe AST interpretation of client-side JavaScript without JIT vulnerabilities or C++ runtime overhead.
- **Multiprocess-Ready Tab Architecture:** Isolated tab worker threads with clean message-passing channels.
- **Native OS Compositing:** Linux (Wayland / X11), macOS (AppKit), and Windows (Win32) native window backends.

## Repository Workspace

- `crates/onyx_reader`: HTML parser, CSS stylesheet parser, reader theme, Wikipedia engine, and RSX transpiler.
- `crates/onyx_browser`: The sovereign browser application and tab management runtime.

## Verification & Testing

```bash
cargo check --workspace --all-targets
cargo test --workspace
```

## License

Licensed under the MIT License. See [LICENSE](file:///home/usr/Projects/strataui/onyx/LICENSE) for details.
