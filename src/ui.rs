//! UI helpers: the webview's HTML/JS assets and small string utilities.
//!
//! The HTML markup and the JavaScript live in `resources/` as plain files so
//! they can be edited independently of the Rust source. At compile time they
//! are embedded with `include_str!` and spliced together by `loading_html()`.

use tao::window::Icon;

const INDEX_HTML: &str = include_str!(concat!(env!("CARGO_MANIFEST_DIR"), "/resources/index.html"));
const APP_JS: &str = include_str!(concat!(env!("CARGO_MANIFEST_DIR"), "/resources/app.js"));

/// Build the initial webview document by injecting the bundled JS into the
/// HTML template. Returns a fresh `String` for `WebViewBuilder::with_html`.
pub fn loading_html() -> String {
    INDEX_HTML.replace("/*__APP_JS__*/", APP_JS)
}

/// Build a valid JS string *argument* (double-quoted, JSON-style escaped) for
/// embedding inside an `evaluate_script` call, e.g. `appendTerm(<here>)`.
///
/// This is far more robust than template-literal injection for arbitrary
/// terminal output, which may contain backticks, `${`, raw newlines, ESC
/// bytes, `\r`/`\b`/… — all of which are handled correctly here. Use it for
/// every event payload that carries user/command text.
pub fn js_string_arg(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 2);
    out.push('"');
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            '\u{08}' => out.push_str("\\b"),
            '\u{0c}' => out.push_str("\\f"),
            c if (c as u32) < 0x20 => {
                out.push_str(&format!("\\u{:04x}", c as u32));
            }
            c => out.push(c),
        }
    }
    out.push('"');
    out
}

/// Decode `icon/logo-480.png` into a `tao::window::Icon` for the window/title bar.
pub fn load_window_icon() -> Option<Icon> {
    let data = include_bytes!("../icon/logo-480.png");
    let decoder = png::Decoder::new(&data[..]);
    let mut reader = decoder.read_info().ok()?;
    let mut buf = vec![0u8; reader.output_buffer_size()];
    reader.next_frame(&mut buf).ok()?;
    let info = reader.info();
    Icon::from_rgba(buf, info.width, info.height).ok()
}
