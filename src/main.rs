//! dsh-desktop — a minimal desktop shell that launches
//! `npx -y @deepseek-ai/dsh web` and shows `127.0.0.1:3080` in a WebView,
//! stopping the command when the window closes.
//!
//! Source layout:
//! - `main.rs`      — entry point, window/webview, event loop
//! - `environment.rs` — env check + portable Node install
//! - `terminal.rs`  — PTY/piped command launch + prompt detection
//! - `ui.rs`        — webview HTML/JS assets + string helpers

mod environment;
mod terminal;
mod ui;

use std::io::Write;
use std::sync::{Arc, Mutex};
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread;

use tao::{
    event::{Event, WindowEvent},
    event_loop::{ControlFlow, EventLoopBuilder},
    window::WindowBuilder,
};
use wry::WebViewBuilder;

// ---- Configuration -------------------------------------------------------
/// Arguments passed to `npx`. The `-y` auto-confirms the one-time package
/// install so the command never hangs waiting for input when launched from a
/// GUI `.app` (where stdin is not a TTY).
pub const ARGS: &[&str] = &["-y", "@deepseek-ai/dsh", "web"];
/// The local server the launched command exposes.
pub const TARGET_URL: &str = "http://127.0.0.1:3080";
/// Host:port we poll to know when the server is ready.
pub const POLL_ADDR: &str = "127.0.0.1:3080";

/// Portable Node.js version downloaded on first launch when no Node is found
/// on the machine. Pinned for reproducibility.
///
/// `@deepseek-ai/dsh` actually requires Node >= v22.15.0 at runtime: it uses
/// `node:zlib.createZstdDecompress` (v22.15.0+), `Promise.withResolvers`
/// (v22.0.0+) and `node:module.stripTypeScriptTypes` (v22.14.0+). We ship the
/// latest v22 LTS to stay well above that floor.
pub const NODE_VERSION: &str = "22.23.2";
/// Mirror used for the download (npmmirror is reliable from mainland China).
pub const NODE_MIRROR: &str = "https://cdn.npmmirror.com/binaries/node";
// --------------------------------------------------------------------------

/// Handle to the running server process; lets us kill it on exit.
#[cfg(unix)]
pub struct ServerHandle {
    pub(crate) pid: i32,
}
#[cfg(windows)]
pub struct ServerHandle {
    pub(crate) child: std::process::Child,
}
impl ServerHandle {
    fn kill(&mut self) {
        #[cfg(unix)]
        unsafe {
            libc::killpg(self.pid as i32, libc::SIGKILL);
        }
        #[cfg(windows)]
        {
            let _ = self.child.kill();
            let _ = self.child.wait();
        }
    }
}

/// Shared sink for keystrokes typed in the in-app terminal.
pub type InputSink = Arc<Mutex<Option<Box<dyn Write + Send>>>>;

/// Events sent from background threads to the UI thread.
pub enum UserEvent {
    /// Replace the checklist's inner HTML with a status list.
    Checklist(String),
    /// Big title line.
    Stage(String),
    /// Secondary line (may contain simple HTML).
    Sub(String),
    /// Download/install progress, 0..100.
    Progress(u8),
    /// Switch the webview to the interactive terminal view.
    EnterTerminal,
    /// A chunk of terminal output to append.
    Term(String),
    /// Lightweight status-line update (status bar only, no terminal log).
    Status(String),
    /// The running command appears to be waiting for a (y/N) confirmation.
    /// The UI shows the literal question text (and we auto-answer `y`).
    Prompt(String),
    /// The child process exited.
    TermDone(String),
    /// Server is up; navigate the webview to it.
    ServerReady,
    /// Fatal error — show it in the window.
    Fatal(String),
}

fn main() {
    let event_loop = EventLoopBuilder::<UserEvent>::with_user_event().build();
    let proxy = event_loop.create_proxy();

    let window = WindowBuilder::new()
        .with_title("DeepSeek dsh Web")
        .with_inner_size(tao::dpi::LogicalSize::new(1280.0, 800.0))
        .build(&event_loop)
        .expect("无法创建窗口");

    window.set_window_icon(ui::load_window_icon());

    // Shared state for the running command + in-app terminal.
    let handle: Arc<Mutex<Option<ServerHandle>>> = Arc::new(Mutex::new(None));
    let input_writer: InputSink = Arc::new(Mutex::new(None));
    let user_took_over = Arc::new(AtomicBool::new(false));
    let exited = Arc::new(AtomicBool::new(false));

    // The webview shows a loading/checklist UI, then switches to a terminal
    // where the user can watch progress and interact with the install.
    let webview = WebViewBuilder::new()
        .with_html(ui::loading_html())
    .with_ipc_handler({
        let input_writer = input_writer.clone();
        let user_took_over = user_took_over.clone();
        move |request| {
            // macOS delivers the posted string in request.body(); be defensive.
            let s = request.body().to_string();
            if let Some(d) = s.strip_prefix("IN:") {
                    user_took_over.store(true, Ordering::SeqCst);
                    if let Some(w) = input_writer.lock().unwrap().as_mut() {
                        let _ = w.write_all(d.as_bytes());
                        let _ = w.write_all(b"\n");
                    }
                }
            }
        })
        .build(&window)
        .expect("无法创建 webview");

    let bg_handle = handle.clone();
    let bg_input = input_writer.clone();
    let bg_took = user_took_over.clone();
    let bg_exited = exited.clone();
    let ui_proxy = proxy.clone();
    thread::spawn(move || {
        environment::run_environment_flow(ui_proxy, bg_handle, bg_input, bg_took, bg_exited);
    });

    event_loop.run(move |event, _, control_flow| {
        *control_flow = ControlFlow::Wait;

        // macOS: install a minimal Edit menu on the first event loop turn so
        // the ⌘X/⌘C/⌘V/⌘A shortcuts are routed into the webview's input. We do
        // it here (not before `run`) because tao finalizes its app state during
        // startup and could otherwise reset the main menu.
        #[cfg(target_os = "macos")]
        {
            static MENU_INSTALLED: AtomicBool = AtomicBool::new(false);
            if !MENU_INSTALLED.swap(true, Ordering::SeqCst) {
                setup_macos_app_menu();
            }
        }

        match event {
            Event::UserEvent(ev) => match ev {
                UserEvent::Checklist(html) => {
                    let _ = webview.evaluate_script(&format!("setChecklist({})", ui::js_string_arg(&html)));
                }
                UserEvent::Stage(t) => {
                    let _ = webview.evaluate_script(&format!("setStage({})", ui::js_string_arg(&t)));
                }
                UserEvent::Sub(t) => {
                    let _ = webview.evaluate_script(&format!("setSub({})", ui::js_string_arg(&t)));
                }
                UserEvent::Status(t) => {
                    let _ = webview.evaluate_script(&format!("setStatus({})", ui::js_string_arg(&t)));
                }
                UserEvent::Progress(p) => {
                    let _ = webview.evaluate_script(&format!(
                        "showProgress(true); setProgress({});",
                        p
                    ));
                }
                UserEvent::EnterTerminal => {
                    let _ = webview.evaluate_script("showTerminal()");
                }
                UserEvent::Term(s) => {
                    let _ = webview.evaluate_script(&format!("appendTerm({})", ui::js_string_arg(&s)));
                }
                UserEvent::Prompt(t) => {
                    let _ = webview.evaluate_script(&format!("showPrompt({})", ui::js_string_arg(&t)));
                }
                UserEvent::TermDone(s) => {
                    let msg = format!("\r\n[{}]\r\n", s);
                    let _ = webview.evaluate_script(&format!("appendTerm({})", ui::js_string_arg(&msg)));
                }
                UserEvent::ServerReady => {
                    let _ = webview.evaluate_script(&format!(
                        "window.location.href = '{}';",
                        TARGET_URL
                    ));
                }
                UserEvent::Fatal(msg) => {
                    // Keep the interactive terminal on screen and just overlay a
                    // red error banner (with the last lines of output) instead of
                    // wiping the whole page — the user needs to see what happened.
                    let _ = webview.evaluate_script(&format!("showFatal({})", ui::js_string_arg(&msg)));
                }
            },
            Event::WindowEvent { event, .. } => match event {
                WindowEvent::CloseRequested => {
                    *control_flow = ControlFlow::Exit;
                }
                _ => {}
            },
            Event::LoopDestroyed => {
                if let Some(mut h) = handle.lock().unwrap().take() {
                    h.kill();
                }
            }
            _ => {}
        }
    });
}

/// Build a minimal macOS application Edit menu (Undo, Redo, Cut, Copy, Paste,
/// Select All) so that the standard ⌘-shortcuts are delivered to the webview's
/// focused input. This is what makes ⌘V work inside WKWebView — without it the
/// key equivalent is never claimed by AppKit and the paste never reaches the
/// `<input>`. The items target the first responder (nil target), so WKWebView's
/// own `paste:`/`copy:`/… implementations handle them natively.
#[cfg(target_os = "macos")]
fn setup_macos_app_menu() {
    use objc2::MainThreadMarker;
    use objc2::runtime::Sel;
    use objc2_app_kit::{NSApplication, NSMenu, NSMenuItem};
    use objc2_foundation::NSString;

    let mtm = match MainThreadMarker::new() {
        Some(m) => m,
        None => return, // not on the main thread; menu would be unsafe
    };

    let app = NSApplication::sharedApplication(mtm);

    let main_menu = NSMenu::new(mtm);
    main_menu.setTitle(&NSString::from_str("MainMenu"));

    let edit_menu = NSMenu::new(mtm);
    edit_menu.setTitle(&NSString::from_str("Edit"));

    let edit_item = NSMenuItem::new(mtm);
    edit_item.setTitle(&NSString::from_str("Edit"));
    edit_item.setSubmenu(Some(&edit_menu));

    // Append one standard edit command. `action` targets the first responder,
    // so the webview fills in the actual behavior.
    let add = |menu: &NSMenu, title: &str, action: Option<Sel>, key: &str| {
        let item = NSMenuItem::new(mtm);
        item.setTitle(&NSString::from_str(title));
        // setAction is `unsafe` (the selector must be valid). The closure does
        // not inherit the outer `unsafe fn` context, so wrap it explicitly.
        unsafe { item.setAction(action); }
        item.setKeyEquivalent(&NSString::from_str(key));
        menu.addItem(&item);
    };

    add(&edit_menu, "Undo", Some(objc2::sel!(undo:)), "z");
    add(&edit_menu, "Redo", Some(objc2::sel!(redo:)), "Z");
    edit_menu.addItem(&NSMenuItem::separatorItem(mtm));
    add(&edit_menu, "Cut", Some(objc2::sel!(cut:)), "x");
    add(&edit_menu, "Copy", Some(objc2::sel!(copy:)), "c");
    add(&edit_menu, "Paste", Some(objc2::sel!(paste:)), "v");
    edit_menu.addItem(&NSMenuItem::separatorItem(mtm));
    add(&edit_menu, "Select All", Some(objc2::sel!(selectAll:)), "a");

    main_menu.addItem(&edit_item);
    app.setMainMenu(Some(&main_menu));
}
