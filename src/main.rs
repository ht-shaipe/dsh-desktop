//! dsh-desktop —— 一个轻量级桌面壳程序：
//! 启动 `npx -y @deepseek-ai/dsh web`，并在 WebView 中展示 `127.0.0.1:3080`
//! 的页面，窗口关闭时自动停止该命令。
//!
//! 源码结构：
//! - `main.rs`        —— 程序入口、窗口/WebView、事件循环
//! - `environment.rs` —— 环境自检 + 便携版 Node.js 自动安装
//! - `updater.rs`     —— 基于 GitHub Releases 的应用自更新
//! - `terminal.rs`    —— PTY/管道方式启动命令 + 交互提示检测
//! - `recovery.rs`    —— 启动失败自动恢复（禁用故障插件重试 / 安全模式）
//! - `ui.rs`          —— WebView 的 HTML/JS 资源 + 字符串工具

mod environment;
mod recovery;
mod terminal;
mod ui;
mod updater;

use std::io::Write;
use std::sync::{Arc, Mutex};
use std::sync::atomic::{AtomicBool, Ordering};
use std::process::Command;
use std::thread;

use tao::{
    event::{Event, WindowEvent},
    event_loop::{ControlFlow, EventLoopBuilder},
    window::WindowBuilder,
};
use wry::WebViewBuilder;

// ---- 配置项 ---------------------------------------------------------------
/// 传给 `npx` 的参数。`-y` 用于自动确认首次安装该包，
/// 避免从 GUI `.app` 启动（stdin 不是 TTY）时命令卡在等待用户输入。
pub const ARGS: &[&str] = &["-y", "@deepseek-ai/dsh", "web", "--no-open"];
/// 被启动命令对外提供的本地服务地址。
pub const TARGET_URL: &str = "http://127.0.0.1:3080";
/// 通过轮询该 地址:端口 来判断服务是否已就绪。
pub const POLL_ADDR: &str = "127.0.0.1:3080";

/// 本机未找到 Node 时，首次启动自动下载的便携版 Node.js 版本号。
/// 锁定具体版本以保证可复现性。
///
/// `@deepseek-ai/dsh` 运行时实际要求 Node >= v22.15.0：
/// 它用到了 `node:zlib.createZstdDecompress`（v22.15.0+）、
/// `Promise.withResolvers`（v22.0.0+）以及
/// `node:module.stripTypeScriptTypes`（v22.14.0+）。
/// 这里直接内置最新的 v22 LTS，稳稳高于该下限。
pub const NODE_VERSION: &str = "22.23.2";
/// 下载 Node 使用的镜像源（npmmirror 在中国大陆访问稳定）。
pub const NODE_MIRROR: &str = "https://cdn.npmmirror.com/binaries/node";
// --------------------------------------------------------------------------

/// 运行中服务进程的句柄；用于在退出时杀死该进程。
#[cfg(unix)]
pub struct ServerHandle {
    pub(crate) pid: i32,
}
#[cfg(windows)]
pub struct ServerHandle {
    pub(crate) child: std::process::Child,
}
impl ServerHandle {
    /// 结束服务进程。Unix 上按进程组发送 SIGKILL（npx 会派生子进程，
    /// 只杀主进程会留下孤儿进程占用端口）；Windows 上直接杀子进程。
    pub(crate) fn kill(&mut self) {
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

/// 应用内终端中用户键盘输入的共享写入端（写入到子进程的 stdin/PTY）。
pub type InputSink = Arc<Mutex<Option<Box<dyn Write + Send>>>>;

/// 后台线程发送给 UI 线程（事件循环）的事件。
pub enum UserEvent {
    /// 用状态列表替换检查清单的 inner HTML。
    Checklist(String),
    /// 大标题行。
    Stage(String),
    /// 次要说明行（可包含简单 HTML）。
    Sub(String),
    /// 下载/安装进度，取值 0..100。
    Progress(u8),
    /// 将 WebView 切换到交互式终端视图。
    EnterTerminal,
    /// 需要追加到终端的一块输出内容。
    Term(String),
    /// 轻量状态栏更新（只刷新状态栏，不写终端日志）。
    Status(String),
    /// 运行中的命令似乎正在等待 (y/N) 确认。
    /// UI 会展示原始问题文本（同时我们会自动回答 `y`）。
    Prompt(String),
    /// 子进程已退出。
    TermDone(String),
    /// 服务已就绪；让 WebView 跳转到该地址。
    ServerReady(String),
    /// 致命错误 —— 直接展示在窗口中。
    Fatal(String),
    /// 启动失败的自动修复诊断（JSON 载荷，见 recovery.rs 的 diag_json）：
    /// 故障插件列表、当前动作（禁用重试/安全模式/修复成功/修复失败）。
    Diagnosis(String),
    /// 页面已进入 dsh web 后需要弹出的恢复提示（原生 DOM toast）。
    RecoveredToast(String),
    /// 用户点击了"检查更新"按钮。
    CheckUpdate,
    /// 用户点击了"帮助"按钮：打开独立的帮助/关于窗口。
    /// 不依赖主 webview 的页面 JS（进入 dsh web 界面后自定义 JS 已被替换），
    /// 而是单独创建一个窗口 + WebView 来展示帮助内容。
    ShowHelp,
    /// 发现新版本（version_tag, release_notes）。
    UpdateAvailable(String, String),
    /// 下载进度 0..100。
    UpdateProgress(u8),
    /// 更新下载+安装完成，参数为版本 tag。
    UpdateDone(String),
    /// 更新流程失败（下载/验签/安装），参数为简短提示文案。
    /// 详细原因仍通过 `Term` 输出到启动页终端。
    UpdateFailed(String),
}

/// dsh web 页面上的更新进度栏（注入式 DOM，自包含、不依赖 app.js ——
/// 整页导航后 app.js 的 showUpdateProgress 已被替换，不能再用）。
/// 固定在页面顶部（原生标题栏正下方）的横条：文本 + 渐变进度条 + 百分比，
/// 样式与启动页 app.js 的 showUpdateProgress 一致。
/// `installing` 为 true 时显示"正在安装…"并锁定 100%。
fn update_bar_show_js(pct: u8, installing: bool) -> String {
    let fill = if installing { "100".to_string() } else { pct.to_string() };
    let pct_text = if installing { "100%".to_string() } else { format!("{}%", pct) };
    let text = if installing { "正在安装更新…" } else { "正在下载更新…" };
    format!(
        "(function(){{try{{var b=document.getElementById('dshUpdBar');\
         if(!b){{b=document.createElement('div');b.id='dshUpdBar';\
         b.style.cssText='position:fixed;top:0;left:0;right:0;z-index:2147483647;\
         background:#1a1f2e;border-bottom:1px solid #2a3242;padding:10px 20px;\
         display:flex;align-items:center;gap:12px;font:13px -apple-system,BlinkMacSystemFont,sans-serif;\
         color:#e6e6e6;box-shadow:0 2px 8px rgba(0,0,0,.35);';\
         b.innerHTML='<span id=\"dshUpdText\"></span>\
           <div style=\"flex:1;height:6px;background:#2a3242;border-radius:3px;overflow:hidden;\">\
           <div id=\"dshUpdFill\" style=\"height:100%;width:0%;\
           background:linear-gradient(90deg,#4f8cff,#7ee0a0);transition:width .2s;\"></div></div>\
           <span id=\"dshUpdPct\" style=\"min-width:44px;text-align:right;\"></span>';\
         (document.body||document.documentElement).appendChild(b);}}\
         document.getElementById('dshUpdText').textContent={text};\
         document.getElementById('dshUpdFill').style.width='{fill}%';\
         document.getElementById('dshUpdPct').textContent='{pct_text}';\
         }}catch(e){{}}}})();",
        text = ui::js_string_arg(text),
        fill = fill,
        pct_text = pct_text,
    )
}

/// 移除注入式更新进度栏（幂等：不存在时为空操作）。
fn update_bar_hide_js() -> String {
    "(function(){try{var b=document.getElementById('dshUpdBar');if(b)b.remove();}catch(e){}})();"
        .to_string()
}

/// dsh web 页面上的"更新完成"横条：复用进度栏的位置（dshUpdBar），
/// 内容替换为绿色完成条 + "立即重启"/"稍后"按钮。
/// "立即重启"走 `RESTART_APP` IPC（webview 级通道，跨页面导航仍有效）；
/// 若页面上下文里 `window.ipc` 意外不可用，点击静默无效，用户仍可手动重启。
fn update_bar_complete_js(tag: &str) -> String {
    let ver = tag.trim_start_matches('v');
    format!(
        "(function(){{try{{var b=document.getElementById('dshUpdBar');\
         if(!b){{b=document.createElement('div');b.id='dshUpdBar';\
         b.style.cssText='position:fixed;top:0;left:0;right:0;z-index:2147483647;\
         background:#1a2332;border-bottom:1px solid #2f7d4f;padding:10px 20px;\
         display:flex;align-items:center;gap:12px;font:13px -apple-system,BlinkMacSystemFont,sans-serif;\
         color:#e6e6e6;box-shadow:0 2px 8px rgba(0,0,0,.35);';\
         (document.body||document.documentElement).appendChild(b);}}\
         b.innerHTML='<span id=\"dshUpdDoneText\" style=\"flex:1;\"></span>\
           <button id=\"dshUpdRestart\" style=\"flex:0 0 auto;cursor:pointer;border:none;\
           background:#4f8cff;color:#fff;border-radius:6px;padding:4px 14px;\
           font:13px -apple-system,BlinkMacSystemFont,sans-serif;\">立即重启</button>\
           <button id=\"dshUpdLater\" style=\"flex:0 0 auto;cursor:pointer;\
           background:transparent;color:#8b93a3;border:1px solid #3a4252;border-radius:6px;\
           padding:4px 14px;font:13px -apple-system,BlinkMacSystemFont,sans-serif;\">稍后</button>';\
         document.getElementById('dshUpdDoneText').textContent={text};\
         document.getElementById('dshUpdRestart').onclick=function(){{\
           try{{window.ipc.postMessage('RESTART_APP');}}catch(e){{}}}};\
         document.getElementById('dshUpdLater').onclick=function(){{\
           try{{b.remove();}}catch(e){{}}}};\
         }}catch(e){{}}}})();",
        text = ui::js_string_arg(&format!("✓ 已升级到 v{}，重启应用完成更新", ver)),
    )
}

/// 显示一个原生浮动提示窗口，2 秒后自动消失。
#[cfg(target_os = "macos")]
fn show_native_toast(text: &str) {
    use std::ffi::{CStr, CString};
    use objc2::runtime::{AnyClass, AnyObject};
    use objc2_foundation::{NSPoint, NSRect, NSSize};

    // Ensure null-terminated C string for stringWithUTF8String:
    let c_text = CString::new(text).unwrap_or_default();

    unsafe {
        let ns_str = CStr::from_bytes_with_nul_unchecked(b"NSString\0");
        let ns_str_cls = AnyClass::get(ns_str).unwrap();
        let ns_win = CStr::from_bytes_with_nul_unchecked(b"NSPanel\0");
        let ns_win_cls = AnyClass::get(ns_win).unwrap();
        let ns_color = CStr::from_bytes_with_nul_unchecked(b"NSColor\0");
        let ns_color_cls = AnyClass::get(ns_color).unwrap();
        let ns_font = CStr::from_bytes_with_nul_unchecked(b"NSFont\0");
        let ns_font_cls = AnyClass::get(ns_font).unwrap();
        let ns_text_cls = AnyClass::get(CStr::from_bytes_with_nul_unchecked(b"NSTextField\0")).unwrap();

        let rect = NSRect::new(NSPoint::new(0.0, 0.0), NSSize::new(220.0, 40.0));
        let style_mask: u64 = 1 | 2 | 4 | 8;
        let panel: *mut AnyObject = objc2::msg_send![ns_win_cls, alloc];
        let panel: *mut AnyObject = objc2::msg_send![panel,
            initWithContentRect: rect, styleMask: style_mask, backing: 2u64, defer: false
        ];

        let title: *mut AnyObject = objc2::msg_send![ns_str_cls, stringWithUTF8String: "提示\0".as_ptr()];
        let _: () = objc2::msg_send![panel, setTitle: title];
        let _: () = objc2::msg_send![panel, setLevel: 8i64];
        let _: () = objc2::msg_send![panel, setOpaque: false];
        let bg: *mut AnyObject = objc2::msg_send![ns_color_cls, colorWithCalibratedWhite: 0.18f64, alpha: 0.92f64];
        let _: () = objc2::msg_send![panel, setBackgroundColor: bg];

        let content_view: *mut AnyObject = objc2::msg_send![panel, contentView];
        let label: *mut AnyObject = objc2::msg_send![ns_text_cls, alloc];
        let label: *mut AnyObject = objc2::msg_send![label,
            initWithFrame: NSRect::new(NSPoint::new(16.0, 10.0), NSSize::new(188.0, 20.0))
        ];
        let text_str: *mut AnyObject = objc2::msg_send![ns_str_cls, stringWithUTF8String: c_text.as_ptr()];
        let font: *mut AnyObject = objc2::msg_send![ns_font_cls, systemFontOfSize: 13.0f64];
        let white: *mut AnyObject = objc2::msg_send![ns_color_cls, labelColor];
        let _: () = objc2::msg_send![label, setStringValue: text_str];
        let _: () = objc2::msg_send![label, setFont: font];
        let _: () = objc2::msg_send![label, setTextColor: white];
        let _: () = objc2::msg_send![label, setEditable: false];
        let _: () = objc2::msg_send![label, setSelectable: false];
        let _: () = objc2::msg_send![label, setBordered: false];
        let _: () = objc2::msg_send![label, setDrawsBackground: false];
        let _: () = objc2::msg_send![content_view, addSubview: label];

        let screen: *mut AnyObject = objc2::msg_send![AnyClass::get(CStr::from_bytes_with_nul_unchecked(b"NSScreen\0")).unwrap(), mainScreen];
        let visible_frame: NSRect = objc2::msg_send![screen, visibleFrame];
        let x = visible_frame.origin.x + visible_frame.size.width - 240.0;
        let y = visible_frame.origin.y + visible_frame.size.height - 80.0;
        let _: () = objc2::msg_send![panel, setFrameOrigin: NSPoint::new(x, y)];
        let _: () = objc2::msg_send![panel, orderFrontRegardless];

        // 2 秒后自动关闭面板。
        // 用 scheduledTimerWithTimeInterval:（自动挂到当前 run loop）。
        // 注意：旧的 timerWithFireDate:target:... 类方法在新版 macOS 运行时
        // 里已被移除（objc2 校验会 panic "method not found"），不能使用；
        // initWithFireDate:interval:... 虽然还在但已弃用。
        // NSTimer 会 retain target（即 panel），面板得以存活到触发 close。
        let timer_cls = AnyClass::get(CStr::from_bytes_with_nul_unchecked(b"NSTimer\0")).unwrap();
        let sel = objc2::sel!(close);
        let _: *mut AnyObject = objc2::msg_send![timer_cls,
            scheduledTimerWithTimeInterval: 2.0f64,
            target: panel,
            selector: sel,
            userInfo: std::ptr::null_mut::<AnyObject>(),
            repeats: false
        ];
    }
}

fn main() {
    // 创建带自定义事件通道的事件循环；proxy 用于后台线程向 UI 线程发事件。
    let event_loop = EventLoopBuilder::<UserEvent>::with_user_event().build();
    let proxy = event_loop.create_proxy();

    let window = WindowBuilder::new()
        .with_title("DeepSeek dsh Desktop")
        .with_inner_size(tao::dpi::LogicalSize::new(1280.0, 800.0))
        .build(&event_loop)
        .expect("无法创建窗口");

    window.set_window_icon(ui::load_window_icon());

    // 运行中命令 + 应用内终端的共享状态。
    let handle: Arc<Mutex<Option<ServerHandle>>> = Arc::new(Mutex::new(None));
    let input_writer: InputSink = Arc::new(Mutex::new(None));
    let user_took_over = Arc::new(AtomicBool::new(false));

    // WebView 先展示 加载中/检查清单 UI，随后切换到终端视图，
    // 用户可以在终端里观看进度并与安装过程交互。
    let webview = WebViewBuilder::new()
        .with_html(ui::loading_html())
    .with_ipc_handler({
        let input_writer = input_writer.clone();
        let user_took_over = user_took_over.clone();
        let proxy = proxy.clone();
        move |request| {
            // macOS 会把 post 的字符串放在 request.body() 中；这里做防御式处理。
            let s = request.body().to_string();
            if let Some(d) = s.strip_prefix("IN:") {
                    // 用户在终端输入框里输入了内容：标记"用户已接管"（此后不再自动应答），
                    // 并把内容写入子进程的 stdin。
                    user_took_over.store(true, Ordering::SeqCst);
                    if let Some(w) = input_writer.lock().unwrap().as_mut() {
                        let _ = w.write_all(d.as_bytes());
                        let _ = w.write_all(b"\n");
                    }
                } else if s == "CHECK_UPDATE" {
                    let _ = proxy.send_event(UserEvent::CheckUpdate);
                } else if let Some(tag) = s.strip_prefix("APPLY_UPDATE:") {
                    let tag = tag.to_string();
                    let apply_proxy = proxy.clone();
                    thread::spawn(move || {
                        updater::apply_update(&tag, &apply_proxy);
                    });
                } else if s == "RESTART_APP" {
                    // Relaunch the app then exit.
                    let exe = std::env::current_exe().unwrap();
                    let _ = Command::new(&exe).spawn();
                    std::process::exit(0);
                }
            }
        })
        .build(&window)
        .expect("无法创建 webview");

    // macOS：在原生标题栏上添加版本号标签和按钮。
    #[cfg(target_os = "macos")]
    setup_titlebar_accessory(&webview, &proxy);

    // 克隆共享状态，交给后台线程执行环境自检 / 自动安装 / 启动服务。
    let bg_handle = handle.clone();
    let bg_input = input_writer.clone();
    let bg_took = user_took_over.clone();
    let ui_proxy = proxy.clone();
    thread::spawn(move || {
        environment::run_environment_flow(ui_proxy, bg_handle, bg_input, bg_took);
    });

    // 主窗口 id：用于区分"关闭主窗口（退出应用）"与"关闭帮助窗口（仅销毁该窗口）"。
    let main_window_id = window.id();
    // 独立的帮助窗口（窗口 + 其专属 WebView）。
    // 已存在时再次点击"帮助"只做聚焦；用户关闭后置 None，下次点击重新创建。
    let mut help_window: Option<(tao::window::Window, wry::WebView)> = None;
    // 主 webview 是否已跳转到 dsh web 界面。
    // 跳转后我们注入的自定义 JS（showUpdateDialog 等）已被整页替换，
    // 更新相关的 UI 反馈必须改走原生 toast / 后台下载。
    let mut at_dsh_web = false;

    event_loop.run(move |event, el_target, control_flow| {
        *control_flow = ControlFlow::Wait;

        // macOS：在事件循环的第一轮安装最小化的"编辑"菜单，使
        // ⌘X/⌘C/⌘V/⌘A 快捷键能路由到 webview 的输入框。
        // 之所以放在这里（而不是 `run` 之前），是因为 tao 在启动过程中
        // 会完成应用状态初始化，提前设置的主菜单可能被它重置。
        #[cfg(target_os = "macos")]
        {
            static MENU_INSTALLED: AtomicBool = AtomicBool::new(false);
            if !MENU_INSTALLED.swap(true, Ordering::SeqCst) {
                setup_macos_app_menu();
            }
        }

        match event {
            // 后台线程发来的自定义事件：转换为对 webview 的 JS 调用，驱动 UI 更新。
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
                UserEvent::ServerReady(url) => {
                    crate::terminal::dbg_log(&format!("SERVER_READY_EVENT: {}", url));
                    // 标记：主页面即将离开启动页，自定义 JS 将不再可用。
                    at_dsh_web = true;
                    // 使用原生导航（load_url）而不是通过 evaluate_script 执行
                    // `window.location.href`：终端视图的 origin 为 null，由 JS
                    // 发起的跨源跳转会与服务器 SameSite=Strict 的认证 cookie
                    // 在 WKWebView 上产生兼容问题。
                    if let Err(e) = webview.load_url(&url) {
                        crate::terminal::dbg_log(&format!("LOAD_URL_ERR: {}", e));
                        let _ = webview.evaluate_script(&format!(
                            "window.location.href = '{}';",
                            url
                        ));
                    }
                }
                UserEvent::Fatal(msg) => {
                    // 保留交互式终端画面，只叠加一条红色错误横幅（带最后几行输出），
                    // 而不是清空整个页面 —— 用户需要看到究竟发生了什么。
                    let _ = webview.evaluate_script(&format!("showFatal({})", ui::js_string_arg(&msg)));
                }
                UserEvent::Diagnosis(payload) => {
                    // 启动自动修复的诊断信息（琥珀色横幅，区别于致命错误）。
                    let _ = webview.evaluate_script(&format!("showDiagnosis({})", payload));
                }
                UserEvent::RecoveredToast(msg) => {
                    // 主页面已经是 dsh web 界面：注入一个原生 DOM toast
                    // 提醒用户有插件被自动禁用。页面 CSP 不影响
                    // evaluate_script；失败也无需反馈。
                    let _ = webview.evaluate_script(&format!(
                        "(function(){{try{{var d=document.createElement('div');d.textContent={};\
                         d.style.cssText='position:fixed;top:16px;right:16px;z-index:2147483647;\
                         max-width:420px;background:#1a2332;color:#7ee0a0;border:1px solid #3a4252;\
                         border-radius:8px;padding:10px 16px;font:13px -apple-system,sans-serif;\
                         box-shadow:0 4px 12px rgba(0,0,0,.4);';document.body.appendChild(d);\
                         setTimeout(function(){{d.style.opacity='0';d.style.transition='opacity .4s';}},8000);\
                         setTimeout(function(){{d.remove();}},8500);}}catch(e){{}}}})();",
                        ui::js_string_arg(&msg)
                    ));
                }
                UserEvent::CheckUpdate => {
                    let _ = webview.evaluate_script("setUpdateBtn('检查中…', true)");
                    let check_proxy = proxy.clone();
                    thread::spawn(move || {
                        match updater::check_for_update() {
                            Some(release) => {
                                let _ = check_proxy.send_event(UserEvent::UpdateAvailable(
                                    release.tag, release.body,
                                ));
                            }
                            None => {
                                let _ = check_proxy.send_event(UserEvent::UpdateDone("".into()));
    }
}
                    });
                }
                UserEvent::UpdateAvailable(tag, notes) => {
                    if at_dsh_web {
                        // 主页面已是 dsh web 界面：我们注入的 JS（showUpdateDialog）
                        // 已被整页替换，确认对话框无处显示。改为注入式进度栏
                        // （标题栏下方）+ 原生 toast 提示 + 直接后台下载安装，
                        // 完成后再次 toast 提示重启。
                        let _ = webview.evaluate_script(&update_bar_show_js(0, false));
                        #[cfg(target_os = "macos")]
                        {
                            let t = format!("发现新版本 {}，正在后台下载更新…", tag);
                            let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| show_native_toast(&t)));
                        }
                        #[cfg(not(target_os = "macos"))]
                        let _ = webview.evaluate_script(&format!(
                            "if (typeof showUpdateToast === 'function') showUpdateToast('发现新版本 {}，正在后台下载更新…')",
                            tag
                        ));
                        let apply_proxy = proxy.clone();
                        let apply_tag = tag.clone();
                        thread::spawn(move || {
                            updater::apply_update(&apply_tag, &apply_proxy);
                        });
                    } else {
                        // 仍在启动页：显示确认对话框（让用户决定是否立即更新）。
                        let _ = webview.evaluate_script(&format!(
                            "if (typeof showUpdateDialog === 'function') showUpdateDialog({}, {})",
                            ui::js_string_arg(&tag),
                            ui::js_string_arg(&notes),
                        ));
                    }
                    let _ = webview.evaluate_script("setUpdateBtn('检查更新', false)");
                }
                UserEvent::UpdateProgress(p) => {
                    // 启动页：app.js 的 showUpdateProgress 仍然可用；
                    // dsh web 页面：app.js 已被整页替换，改为注入自绘进度栏
                    // （固定在原生标题栏正下方，显示实时下载进度）。
                    let _ = webview.evaluate_script(&format!(
                        "if (typeof showUpdateProgress === 'function') showUpdateProgress({});",
                        p
                    ));
                    if at_dsh_web {
                        let _ = webview.evaluate_script(&update_bar_show_js(p, p >= 100));
                    }
                }
                UserEvent::UpdateDone(tag) => {
                    if tag.is_empty() {
                        #[cfg(target_os = "macos")]
                        { let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| show_native_toast("已是最新版本"))); }
                        #[cfg(not(target_os = "macos"))]
                        let _ = webview.evaluate_script("showUpdateToast('已是最新版本')");
                    } else if at_dsh_web {
                        // dsh 界面：注入"更新完成"横条（带"立即重启"按钮）。
                        // 重启是需要用户明确行动的提示，原生 toast 2 秒即逝
                        // 容易错过，非 macOS 上 showUpdateToast 则根本不存在。
                        let _ = webview.evaluate_script(&update_bar_complete_js(&tag));
                    } else {
                        // 启动页：显示完成对话框（含"立即重启"按钮）。
                        let _ = webview.evaluate_script(&format!(
                            "if (typeof showUpdateComplete === 'function') showUpdateComplete({})",
                            ui::js_string_arg(&tag),
                        ));
                        let _ = webview.evaluate_script("setUpdateBtn('重启更新', false)");
                    }
                }
                UserEvent::UpdateFailed(msg) => {
                    // 失败提示：优先原生 toast；同时收起两个视图的进度条
                    // （启动页的 app.js 进度条 / dsh 页面的注入式进度栏）。
                    #[cfg(target_os = "macos")]
                    { let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| show_native_toast(&msg))); }
                    #[cfg(not(target_os = "macos"))]
                    let _ = webview.evaluate_script(&format!(
                        "if (typeof showUpdateToast === 'function') showUpdateToast({})",
                        ui::js_string_arg(&msg)
                    ));
                    let _ = webview.evaluate_script(
                        "if (typeof hideUpdateProgress === 'function') hideUpdateProgress();",
                    );
                    let _ = webview.evaluate_script(&update_bar_hide_js());
                }
                UserEvent::ShowHelp => {
                    if let Some((hw, _)) = &help_window {
                        // 已打开：前置聚焦即可，不重复创建
                        hw.set_focus();
                    } else {
                        // 首次打开：创建独立窗口 + 专属 WebView。
                        // 内容是内嵌的 help_html，与主窗口当前加载的页面
                        // （启动页或 dsh web 界面）完全解耦。
                        let hw = WindowBuilder::new()
                            .with_title("帮助 · DeepSeek dsh Desktop")
                            .with_inner_size(tao::dpi::LogicalSize::new(520.0, 620.0))
                            .with_resizable(false)
                            .build(el_target)
                            .expect("无法创建帮助窗口");
                        hw.set_window_icon(ui::load_window_icon());
                        let hwebview = WebViewBuilder::new()
                            .with_html(ui::help_html())
                            .build(&hw)
                            .expect("无法创建帮助 webview");
                        help_window = Some((hw, hwebview));
                    }
                }
            },
            Event::WindowEvent { window_id, event, .. } => match event {
                WindowEvent::CloseRequested => {
                    if window_id == main_window_id {
                        // 关闭主窗口：退出整个应用（事件循环销毁时会杀掉服务进程）
                        *control_flow = ControlFlow::Exit;
                    } else if help_window
                        .as_ref()
                        .map(|(hw, _)| hw.id() == window_id)
                        .unwrap_or(false)
                    {
                        // 关闭帮助窗口：只销毁它，应用继续运行
                        help_window = None;
                    }
                }
                _ => {}
            },
            // 事件循环销毁（窗口已关闭）：确保服务进程被杀死。
            Event::LoopDestroyed => {
                if let Some(mut h) = handle.lock().unwrap().take() {
                    h.kill();
                }
            }
            _ => {}
        }
    });
}

/// 构建一个最小化的 macOS "编辑"菜单（撤销、重做、剪切、复制、粘贴、
/// 全选），使标准 ⌘ 快捷键能送达 webview 中获得焦点的输入框。
/// 这正是 ⌘V 能在 WKWebView 中生效的关键 —— 没有它，按键等价物
/// 不会被 AppKit 接管，粘贴事件也永远到不了 `<input>`。
/// 菜单项的 target 为 first responder（nil target），因此 WKWebView
/// 自带的 `paste:`/`copy:` 等实现会原生地处理这些操作。
#[cfg(target_os = "macos")]
fn setup_macos_app_menu() {
    use objc2::MainThreadMarker;
    use objc2::runtime::Sel;
    use objc2_app_kit::{NSApplication, NSMenu, NSMenuItem};
    use objc2_foundation::NSString;

    let mtm = match MainThreadMarker::new() {
        Some(m) => m,
        None => return, // 不在主线程；此时操作菜单不安全
    };

    let app = NSApplication::sharedApplication(mtm);

    let main_menu = NSMenu::new(mtm);
    main_menu.setTitle(&NSString::from_str("MainMenu"));

    let edit_menu = NSMenu::new(mtm);
    edit_menu.setTitle(&NSString::from_str("Edit"));

    let edit_item = NSMenuItem::new(mtm);
    edit_item.setTitle(&NSString::from_str("Edit"));
    edit_item.setSubmenu(Some(&edit_menu));

    // 追加一个标准编辑命令。`action` 的目标是 first responder，
    // 因此具体行为由 webview 自己实现。
    let add = |menu: &NSMenu, title: &str, action: Option<Sel>, key: &str| {
        let item = NSMenuItem::new(mtm);
        item.setTitle(&NSString::from_str(title));
        // setAction 是 `unsafe` 的（selector 必须有效）。闭包不会继承
        // 外层 `unsafe fn` 的上下文，所以需要显式包裹。
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

/// 在 macOS 标题栏上添加 版本号标签 + 检查更新/帮助 按钮。
#[cfg(target_os = "macos")]
fn setup_titlebar_accessory(webview: &wry::WebView, proxy: &tao::event_loop::EventLoopProxy<UserEvent>) {
    use std::ffi::{c_void, CStr};
    use objc2::runtime::{AnyClass, AnyObject, Sel};
    use objc2::sel;
    use objc2_foundation::{NSPoint, NSRect, NSSize};
    use wry::WebViewExtMacOS;

    fn ns_make_rect(x: f64, y: f64, w: f64, h: f64) -> NSRect {
        NSRect::new(NSPoint::new(x, y), NSSize::new(w, h))
    }

    static mut PROXY_PTR: Option<std::ptr::NonNull<c_void>> = None;

    extern "C" fn on_update_click(_this: *mut AnyObject, _sel: Sel, _sender: *mut AnyObject) {
        unsafe {
            if let Some(ptr) = PROXY_PTR {
                let proxy = &*(ptr.as_ptr() as *const tao::event_loop::EventLoopProxy<UserEvent>);
                let _ = proxy.send_event(UserEvent::CheckUpdate);
            }
        }
    }

    extern "C" fn on_help_click(_this: *mut AnyObject, _sel: Sel, _sender: *mut AnyObject) {
        unsafe {
            // 通过事件循环打开独立的帮助窗口。
            // 不再用 evaluate_script("showAboutDialog()")：主 webview 进入
            // dsh web 界面后，我们注入的自定义 JS 已被整页替换，调用必然落空；
            // 独立窗口 + 专属 WebView 与主页面完全解耦，任何阶段都能弹出。
            if let Some(ptr) = PROXY_PTR {
                let proxy = &*(ptr.as_ptr() as *const tao::event_loop::EventLoopProxy<UserEvent>);
                let _ = proxy.send_event(UserEvent::ShowHelp);
            }
        }
    }

    unsafe {
        let ns_window = webview.ns_window();
        PROXY_PTR = Some(std::ptr::NonNull::new_unchecked(
            proxy as *const tao::event_loop::EventLoopProxy<UserEvent> as *mut c_void
        ));

        let ns_view_sup = CStr::from_bytes_with_nul_unchecked(b"NSView\0");
        let superclass = AnyClass::get(ns_view_sup).unwrap();

        // Action target class
        let target_cls = {
            let name = CStr::from_bytes_with_nul_unchecked(b"DSHTitlebarTarget\0");
            if let Some(cls) = AnyClass::get(name) {
                cls
            } else {
                let ns_obj_sup = CStr::from_bytes_with_nul_unchecked(b"NSObject\0");
                let ns_obj_cls = AnyClass::get(ns_obj_sup).unwrap();
                let mut b = objc2::runtime::ClassBuilder::new(name, ns_obj_cls).unwrap();
                b.add_method(sel!(onUpdateClick:), on_update_click as extern "C" fn(*mut AnyObject, Sel, *mut AnyObject));
                b.add_method(sel!(onHelpClick:), on_help_click as extern "C" fn(*mut AnyObject, Sel, *mut AnyObject));
                b.register()
            }
        };

        let target: *mut AnyObject = objc2::msg_send![target_cls, alloc];
        let target: *mut AnyObject = objc2::msg_send![target, init];

        let ns_str = CStr::from_bytes_with_nul_unchecked(b"NSString\0");
        let ns_str_cls = AnyClass::get(ns_str).unwrap();
        let ns_font = CStr::from_bytes_with_nul_unchecked(b"NSFont\0");
        let font: *mut AnyObject = objc2::msg_send![
            AnyClass::get(ns_font).unwrap(), systemFontOfSize: 11.0f64
        ];
        let ns_color = CStr::from_bytes_with_nul_unchecked(b"NSColor\0");
        let label_color: *mut AnyObject = objc2::msg_send![
            AnyClass::get(ns_color).unwrap(), labelColor
        ];
        let ns_button = CStr::from_bytes_with_nul_unchecked(b"NSButton\0");
        let ns_button_cls = AnyClass::get(ns_button).unwrap();

        // --- Container ---
        let container: *mut AnyObject = objc2::msg_send![superclass, alloc];
        let container: *mut AnyObject = objc2::msg_send![container,
            // 宽度需容纳右侧版本号标签（x=110 起，60pt 约可显示 9 个字符，
            // 覆盖 v0.1.13 / v0.1.100 之类的长度）
            initWithFrame: ns_make_rect(0.0, 0.0, 170.0, 28.0)
        ];

        // --- "检查更新" NSButton ---
        let update_btn: *mut AnyObject = objc2::msg_send![ns_button_cls, alloc];
        let update_btn: *mut AnyObject = objc2::msg_send![update_btn,
            initWithFrame: ns_make_rect(0.0, 0.0, 72.0, 28.0)
        ];
        let _: () = objc2::msg_send![update_btn, setBezelStyle: 15i64];
        let _: () = objc2::msg_send![update_btn, setBordered: false];
        let _: () = objc2::msg_send![update_btn, setTarget: target];
        let _: () = objc2::msg_send![update_btn, setAction: sel!(onUpdateClick:)];
        let upd_title: *mut AnyObject = objc2::msg_send![
            ns_str_cls, stringWithUTF8String: "检查更新\0".as_ptr()
        ];
        let _: () = objc2::msg_send![update_btn, setTitle: upd_title];
        let _: () = objc2::msg_send![update_btn, setFont: font];
        let _: () = objc2::msg_send![update_btn, setAutoresizingMask: 1i64];
        let _: () = objc2::msg_send![container, addSubview: update_btn];

        // --- "帮助" NSButton ---
        let help_btn: *mut AnyObject = objc2::msg_send![ns_button_cls, alloc];
        let help_btn: *mut AnyObject = objc2::msg_send![help_btn,
            initWithFrame: ns_make_rect(72.0, 0.0, 32.0, 28.0)
        ];
        let _: () = objc2::msg_send![help_btn, setBezelStyle: 15i64];
        let _: () = objc2::msg_send![help_btn, setBordered: false];
        let _: () = objc2::msg_send![help_btn, setTarget: target];
        let _: () = objc2::msg_send![help_btn, setAction: sel!(onHelpClick:)];
        let help_title: *mut AnyObject = objc2::msg_send![
            ns_str_cls, stringWithUTF8String: "帮助\0".as_ptr()
        ];
        let _: () = objc2::msg_send![help_btn, setTitle: help_title];
        let _: () = objc2::msg_send![help_btn, setFont: font];
        let _: () = objc2::msg_send![help_btn, setAutoresizingMask: 1i64];
        let _: () = objc2::msg_send![container, addSubview: help_btn];

        // --- Version label ---
        let version = env!("CARGO_PKG_VERSION");
        let ns_text = CStr::from_bytes_with_nul_unchecked(b"NSTextField\0");
        let ns_text_cls = AnyClass::get(ns_text).unwrap();
        let ver_label: *mut AnyObject = objc2::msg_send![ns_text_cls, alloc];
        let ver_label: *mut AnyObject = objc2::msg_send![ver_label,
            // 38pt 只够显示 v0.1.1（7 字符的 v0.1.13 会被截断），放宽到 60pt
            initWithFrame: ns_make_rect(110.0, 6.0, 60.0, 16.0)
        ];
        let ver_str: *mut AnyObject = objc2::msg_send![
            ns_str_cls, stringWithUTF8String: format!("v{}\0", version).as_ptr()
        ];
        let _: () = objc2::msg_send![ver_label, setStringValue: ver_str];
        let _: () = objc2::msg_send![ver_label, setEditable: false];
        let _: () = objc2::msg_send![ver_label, setSelectable: false];
        let _: () = objc2::msg_send![ver_label, setBordered: false];
        let _: () = objc2::msg_send![ver_label, setDrawsBackground: false];
        let _: () = objc2::msg_send![ver_label, setFont: font];
        let _: () = objc2::msg_send![ver_label, setTextColor: label_color];
        let _: () = objc2::msg_send![ver_label, setAutoresizingMask: 1i64];
        let _: () = objc2::msg_send![container, addSubview: ver_label];

        // --- Attach to title bar (trailing) ---
        let vc_name = CStr::from_bytes_with_nul_unchecked(b"NSTitlebarAccessoryViewController\0");
        let vc_cls = AnyClass::get(vc_name).unwrap();
        let vc: *mut AnyObject = objc2::msg_send![vc_cls, alloc];
        let vc: *mut AnyObject = objc2::msg_send![vc, init];
        let _: () = objc2::msg_send![vc, setView: container];
        let _: () = objc2::msg_send![vc, setLayoutAttribute: 2i64];
        let _: () = objc2::msg_send![
            &*ns_window as *const _ as *mut AnyObject,
            addTitlebarAccessoryViewController: vc
        ];
    }
}
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn update_bar_js_is_well_formed() {
        let js = update_bar_show_js(42, false);
        assert!(js.starts_with("(function()"));
        assert!(js.ends_with("})();"));
        assert!(js.contains("dshUpdBar"));
        assert!(js.contains("dshUpdFill"));
        assert!(js.contains("textContent=\"正在下载更新…\""));
        assert!(js.contains("width='42%'"));
        assert!(js.contains("'42%'"));

        let js = update_bar_show_js(100, true);
        assert!(js.contains("textContent=\"正在安装更新…\""));
        assert!(js.contains("width='100%'"));

        let js = update_bar_hide_js();
        assert!(js.contains("dshUpdBar"));
        assert!(js.contains(".remove()"));

        let js = update_bar_complete_js("v0.1.13");
        assert!(js.contains("textContent=\"✓ 已升级到 v0.1.13，重启应用完成更新\""));
        assert!(js.contains("dshUpdRestart"));
        assert!(js.contains("dshUpdLater"));
        assert!(js.contains("RESTART_APP"));
        assert!(!js.contains("v0.1.13\n"));
    }

    #[test]
    fn update_bar_js_printable() {
        // 人工核对 / 语法检查用：
        // cargo test update_bar_js_printable -- --nocapture
        println!("SHOW42={}", update_bar_show_js(42, false));
        println!("SHOW100={}", update_bar_show_js(100, true));
        println!("HIDE={}", update_bar_hide_js());
        println!("DONE={}", update_bar_complete_js("v0.1.13"));
    }
}
