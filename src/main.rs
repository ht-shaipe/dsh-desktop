use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::Duration;

use tao::{
    event::{Event, WindowEvent},
    event_loop::{ControlFlow, EventLoopBuilder, EventLoopProxy},
    window::{Icon, WindowBuilder},
};
use wry::WebViewBuilder;

// ---- Configuration -------------------------------------------------------
/// Arguments passed to `npx`. The `-y` auto-confirms the one-time package
/// install so the command never hangs waiting for input when launched from a
/// GUI `.app` (where stdin is not a TTY).
const ARGS: &[&str] = &["-y", "@deepseek-ai/dsh", "web"];
/// The local server the launched command exposes.
const TARGET_URL: &str = "http://127.0.0.1:3080";
/// Host:port we poll to know when the server is ready.
const POLL_ADDR: &str = "127.0.0.1:3080";

/// Portable Node.js version downloaded on first launch when no Node is found
/// on the machine. Pinned for reproducibility.
const NODE_VERSION: &str = "20.18.0";
/// Mirror used for the download (npmmirror is reliable from mainland China).
const NODE_MIRROR: &str = "https://cdn.npmmirror.com/binaries/node";
// --------------------------------------------------------------------------

enum UserEvent {
    /// Replace the checklist's inner HTML with a status list.
    Checklist(String),
    /// Big title line.
    Stage(String),
    /// Secondary line (may contain simple HTML).
    Sub(String),
    /// Download/install progress, 0..100.
    Progress(u8),
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

    window.set_window_icon(load_window_icon());

    // Show the UI immediately; the background thread reports progress into it.
    let webview = WebViewBuilder::new()
        .with_html(LOADING_HTML)
        .build(&window)
        .expect("无法创建 webview");

    // The spawned command's process-group handle, shared with the event loop so
    // it can be killed on exit.
    let child = Arc::new(Mutex::new(None));
    let child_for_thread = child.clone();
    let ui_proxy = proxy.clone();
    thread::spawn(move || {
        run_environment_flow(ui_proxy, child_for_thread);
    });

    event_loop.run(move |event, _, control_flow| {
        *control_flow = ControlFlow::Wait;
        match event {
            Event::UserEvent(ev) => match ev {
                UserEvent::Checklist(html) => {
                    let _ = webview.evaluate_script(&format!("setChecklist(`{}`)", js_escape(&html)));
                }
                UserEvent::Stage(t) => {
                    let _ = webview.evaluate_script(&format!("setStage(`{}`)", js_escape(&t)));
                }
                UserEvent::Sub(t) => {
                    let _ = webview.evaluate_script(&format!("setSub(`{}`)", js_escape(&t)));
                }
                UserEvent::Progress(p) => {
                    let _ = webview.evaluate_script(&format!(
                        "showProgress(true); setProgress({});",
                        p
                    ));
                }
                UserEvent::ServerReady => {
                    let _ = webview.evaluate_script(&format!(
                        "window.location.href = '{}';",
                        TARGET_URL
                    ));
                }
                UserEvent::Fatal(msg) => {
                    let doc = js_escape(&error_html(&msg));
                    let _ = webview.evaluate_script(&format!(
                        "document.open(); document.write(`{}`); document.close();",
                        doc
                    ));
                }
            },
            Event::WindowEvent { event, .. } => match event {
                WindowEvent::CloseRequested => {
                    *control_flow = ControlFlow::Exit;
                }
                _ => {}
            },
            Event::LoopDestroyed => {
                if let Some(mut c) = child.lock().unwrap().take() {
                    terminate(&mut c);
                }
            }
            _ => {}
        }
    });
}

/// Full startup sequence: check the environment, list what's missing,
/// auto-install Node.js if needed, then launch the server.
fn run_environment_flow(
    proxy: EventLoopProxy<UserEvent>,
    child: Arc<Mutex<Option<Child>>>,
) {
    let _ = proxy.send_event(UserEvent::Stage("正在检查运行环境…".into()));
    let _ = proxy.send_event(UserEvent::Sub("检测 Node.js / npx 是否可用…".into()));

    // --- 1. Environment check -------------------------------------------
    let node = resolve_npx();
    let (node_ok, node_detail): (bool, String) = match &node {
        Some(p) => (true, format!("已找到: {}", p.display())),
        None => (false, "本机未安装".into()),
    };
    let checklist = format!(
        "{}{}",
        checklist_item(node_ok, "Node.js 运行环境 (npx)", &node_detail),
        checklist_item(
            true,
            "@deepseek-ai/dsh 命令包",
            "将通过 npx 首次运行时自动获取"
        ),
    );
    let _ = proxy.send_event(UserEvent::Checklist(checklist));

    // --- 2. Fast path: Node already present -----------------------------
    if let Some(npx) = node {
        let _ = proxy.send_event(UserEvent::Stage("准备启动服务…".into()));
        let _ = proxy.send_event(UserEvent::Sub(
            "运行 <code>npx -y @deepseek-ai/dsh web</code>".into(),
        ));
        launch_and_poll(npx, proxy, child);
        return;
    }

    // --- 3. Slow path: auto-install a portable Node ---------------------
    let _ = proxy.send_event(UserEvent::Stage("正在自动安装运行环境…".into()));
    let _ = proxy.send_event(UserEvent::Sub(
        "本机缺少 Node.js，将自动下载并安装便携版（无需管理员权限）".into(),
    ));

    let target = node_target();
    let cache = match node_cache_dir() {
        Ok(c) => c,
        Err(e) => {
            let _ = proxy.send_event(UserEvent::Fatal(e));
            return;
        }
    };
    let home = cache.join(format!("node-v{}-{}", NODE_VERSION, target));
    let npx = node_npx_path(&home);

    if !npx.is_file() {
        match download_and_extract_node(&target, &cache, &proxy) {
            Ok(()) => {}
            Err(e) => {
                let _ = proxy.send_event(UserEvent::Fatal(e));
                return;
            }
        }
    }

    if npx.is_file() {
        let done = format!(
            "{}{}",
            checklist_item(true, "Node.js 运行环境 (npx)", &format!("已安装: {}", npx.display())),
            checklist_item(true, "@deepseek-ai/dsh 命令包", "将通过 npx 自动获取"),
        );
        let _ = proxy.send_event(UserEvent::Checklist(done));
        launch_and_poll(npx, proxy, child);
    } else {
        let _ = proxy.send_event(UserEvent::Fatal(
            "未能在自动安装的 Node.js 中找到 npx，安装失败。".into(),
        ));
    }
}

/// Spawn the server and poll the local port; report progress to the UI.
fn launch_and_poll(
    npx: PathBuf,
    proxy: EventLoopProxy<UserEvent>,
    child: Arc<Mutex<Option<Child>>>,
) {
    let log = std::env::temp_dir().join("dsh-desktop.log");
    match spawn_server_with(&npx, &log) {
        Ok(c) => {
            *child.lock().unwrap() = Some(c);
            let _ = proxy.send_event(UserEvent::Stage("正在启动服务…".into()));
            let _ = proxy.send_event(UserEvent::Sub(
                "正在运行命令，等待 <code>127.0.0.1:3080</code> 就绪（最多 2 分钟）…".into(),
            ));
            // Poll up to ~120s, then report the command output if it's stuck.
            for _ in 0..240 {
                if std::net::TcpStream::connect(POLL_ADDR).is_ok() {
                    let _ = proxy.send_event(UserEvent::ServerReady);
                    return;
                }
                thread::sleep(Duration::from_millis(500));
            }
            // Timed out: stop the command and show what it printed.
            if let Some(mut c) = child.lock().unwrap().take() {
                terminate(&mut c);
            }
            let tail = read_log_tail(&log, 50);
            let _ = proxy.send_event(UserEvent::Fatal(format!(
                "命令已启动，但 {} 在 2 分钟内未就绪。命令自身可能有报错。\n\n命令输出（末尾）：\n{}",
                TARGET_URL, tail
            )));
        }
        Err(e) => {
            let _ = proxy.send_event(UserEvent::Fatal(e));
        }
    }
}

/// Read the last `max_lines` lines of a file (used to surface command output).
fn read_log_tail(path: &Path, max_lines: usize) -> String {
    let Ok(s) = std::fs::read_to_string(path) else {
        return "(无日志内容)".to_string();
    };
    if s.trim().is_empty() {
        return "(命令未输出任何内容)".to_string();
    }
    let lines: Vec<&str> = s.lines().collect();
    let start = lines.len().saturating_sub(max_lines);
    lines[start..].join("\n")
}

/// Build the Node.js distribution target suffix for the current platform,
/// e.g. `darwin-arm64`, `linux-x64`, `win-x64`.
fn node_target() -> String {
    let os = match std::env::consts::OS {
        "macos" => "darwin",
        "linux" => "linux",
        "windows" => "win",
        other => other,
    };
    let arch = match std::env::consts::ARCH {
        "aarch64" => "arm64",
        "x86_64" => "x64",
        "arm" => "armv7l",
        other => other,
    };
    format!("{}-{}", os, arch)
}

/// Local cache directory that holds the (optionally) downloaded portable Node.
fn node_cache_dir() -> Result<PathBuf, String> {
    let base = if cfg!(windows) {
        std::env::var("LOCALAPPDATA")
            .unwrap_or_else(|_| "C:\\Users\\Public\\.cache".to_string())
    } else {
        let home = std::env::var("HOME").map_err(|_| "无法确定用户目录（HOME 未设置）。".to_string())?;
        format!("{}/.cache", home)
    };
    Ok(PathBuf::from(base).join("dsh-desktop"))
}

#[cfg(windows)]
fn node_npx_path(home: &Path) -> PathBuf {
    home.join("npx.cmd")
}
#[cfg(not(windows))]
fn node_npx_path(home: &Path) -> PathBuf {
    home.join("bin").join("npx")
}

/// Download the portable Node.js archive for `target` into `cache`, reporting
/// download progress through `proxy`, then extract it. Uses `curl`
/// (falls back to `wget`), which ship by default on macOS, modern Windows 10+,
/// and most Linux distributions. No admin rights required.
fn download_and_extract_node(
    target: &str,
    cache: &Path,
    proxy: &EventLoopProxy<UserEvent>,
) -> Result<(), String> {
    fs::create_dir_all(cache)
        .map_err(|e| format!("无法创建缓存目录 {}: {}", cache.display(), e))?;

    let (url, ext) = if cfg!(windows) {
        (
            format!(
                "{}/v{}/node-v{}-{}.zip",
                NODE_MIRROR, NODE_VERSION, NODE_VERSION, target
            ),
            "zip",
        )
    } else {
        (
            format!(
                "{}/v{}/node-v{}-{}.tar.gz",
                NODE_MIRROR, NODE_VERSION, NODE_VERSION, target
            ),
            "tar.gz",
        )
    };

    let tmp = cache.join(format!("node-{}.{}", target, ext));

    // Total size for a determinate progress bar (best effort).
    let total = http_content_length(&url).unwrap_or(0);
    let _ = proxy.send_event(UserEvent::Sub(
        "正在从 npmmirror 镜像下载 Node.js 运行环境…".into(),
    ));
    let _ = proxy.send_event(UserEvent::Progress(0));

    // Start the download (writes straight to the temp file).
    let mut dl = Command::new("curl")
        .args(["-fsSL", &url, "-o", &tmp.to_string_lossy()])
        .spawn()
        .or_else(|_| {
            Command::new("wget")
                .args(["-q", &url, "-O", &tmp.to_string_lossy()])
                .spawn()
        })
        .map_err(|e| format!("无法启动下载工具（curl/wget 均不可用）: {}", e))?;

    // Poll the temp file size to drive the progress bar.
    loop {
        if let Ok(m) = fs::metadata(&tmp) {
            let pct = if total > 0 {
                ((m.len() as f64 / total as f64) * 100.0) as u8
            } else {
                0
            };
            let _ = proxy.send_event(UserEvent::Progress(pct.min(99)));
        }
        if dl.try_wait().ok().flatten().is_some() {
            break;
        }
        thread::sleep(Duration::from_millis(200));
    }

    let downloaded = dl.wait().ok().map(|s| s.success()) == Some(true) && tmp.is_file();
    if !downloaded {
        let _ = fs::remove_file(&tmp);
        return Err(format!(
            "下载 Node.js 运行环境失败（{}）。请检查网络连接后重试，或手动安装 Node.js 并用 DSH_NPX 指定 npx 路径。",
            url
        ));
    }

    let _ = proxy.send_event(UserEvent::Stage("正在解压运行环境…".into()));
    let _ = proxy.send_event(UserEvent::Sub("解压 Node.js…".into()));
    let _ = proxy.send_event(UserEvent::Progress(100));

    let ok = if cfg!(windows) {
        let ps = format!(
            "Expand-Archive -Force -Path '{}' -DestinationPath '{}'",
            tmp.display(),
            cache.display()
        );
        Command::new("powershell")
            .args(["-NoProfile", "-Command", &ps])
            .status()
            .map(|s| s.success())
            .unwrap_or(false)
    } else {
        Command::new("tar")
            .args(["-xzf", &tmp.to_string_lossy(), "-C", &cache.to_string_lossy()])
            .status()
            .map(|s| s.success())
            .unwrap_or(false)
    };

    let _ = fs::remove_file(&tmp);

    if !ok {
        return Err("已下载 Node.js 压缩包，但解压失败。".to_string());
    }
    Ok(())
}

/// HEAD the URL and return its Content-Length, if available.
fn http_content_length(url: &str) -> Option<u64> {
    let out = Command::new("curl")
        .args(["-sI", "--max-time", "20", url])
        .output()
        .ok()?;
    let text = String::from_utf8_lossy(&out.stdout);
    for line in text.lines() {
        let lower = line.to_ascii_lowercase();
        if let Some(rest) = lower.strip_prefix("content-length:") {
            return rest.trim().parse::<u64>().ok();
        }
    }
    None
}

/// Locate an `npx` executable. We can't trust PATH when launched from a GUI
/// `.app` (macOS gives it a minimal PATH like `/usr/bin:/bin:/usr/sbin:/sbin`),
/// so we also probe common Node install locations. Override with `DSH_NPX`.
fn resolve_npx() -> Option<PathBuf> {
    if let Ok(p) = std::env::var("DSH_NPX") {
        let p = PathBuf::from(p);
        if p.is_file() {
            return Some(p);
        }
    }

    let home = std::env::var("HOME").unwrap_or_default();
    let mut candidates: Vec<PathBuf> = vec![
        PathBuf::from("/usr/local/bin/npx"),
        PathBuf::from("/opt/homebrew/bin/npx"),
        PathBuf::from("/usr/bin/npx"),
        // Managed runtime used in this environment.
        PathBuf::from("/Users/shaipe/.workbuddy/binaries/node/versions/22.22.2/bin/npx"),
    ];

    // nvm: $HOME/.nvm/versions/node/*/bin/npx
    let nvm_base = Path::new(&home).join(".nvm/versions/node");
    if let Ok(entries) = fs::read_dir(&nvm_base) {
        for entry in entries.flatten() {
            candidates.push(entry.path().join("bin/npx"));
        }
    }

    // Everything currently on PATH.
    if let Ok(path_env) = std::env::var("PATH") {
        for dir in path_env.split(':') {
            if !dir.is_empty() {
                candidates.push(PathBuf::from(dir).join("npx"));
            }
        }
    }

    candidates.into_iter().find(|p| p.is_file())
}

/// Spawn the server command as the leader of a fresh process group so that we
/// can kill it *and* any children it forks (e.g. the node process npx launches).
/// Returns a human-readable error instead of panicking if anything goes wrong.
#[cfg(unix)]
fn spawn_server_with(npx: &Path, log: &Path) -> Result<Child, String> {
    use std::os::unix::process::CommandExt;

    // Capture the command's output so we can surface it if the server never
    // comes up (the GUI app otherwise has no console to show it).
    let file = std::fs::File::create(log)
        .map_err(|e| format!("无法创建日志文件 {}: {}", log.display(), e))?;
    let stderr_file = file.try_clone().map_err(|e| e.to_string())?;

    // Make sure the spawned process can also find `node`: prepend npx's own
    // directory to PATH (the npx script is run via `#!/usr/bin/env node`).
    let npx_dir = npx.parent().map(|p| p.to_string_lossy().to_string()).unwrap_or_default();
    let inherited = std::env::var("PATH").unwrap_or_default();
    let new_path = if npx_dir.is_empty() {
        inherited
    } else {
        format!("{}:{}", npx_dir, inherited)
    };

    let mut cmd = Command::new(npx);
    cmd.args(ARGS)
        .process_group(0)
        .env("PATH", new_path)
        .env("NODE_OPTIONS", filter_node_options())
        .stdout(Stdio::from(file))
        .stderr(Stdio::from(stderr_file));
    cmd.spawn()
        .map_err(|e| format!("启动 npx 失败: {}（路径：{}）", e, npx.display()))
}

#[cfg(windows)]
fn spawn_server_with(npx: &Path, log: &Path) -> Result<Child, String> {
    let file = std::fs::File::create(log)
        .map_err(|e| format!("无法创建日志文件 {}: {}", log.display(), e))?;
    let stderr_file = file.try_clone().map_err(|e| e.to_string())?;

    let npx_dir = npx.parent().map(|p| p.to_string_lossy().to_string()).unwrap_or_default();
    let inherited = std::env::var("PATH").unwrap_or_default();
    let new_path = format!("{};{}", npx_dir, inherited);
    Command::new(npx)
        .args(ARGS)
        .env("PATH", new_path)
        .env("NODE_OPTIONS", filter_node_options())
        .stdout(Stdio::from(file))
        .stderr(Stdio::from(stderr_file))
        .spawn()
        .map_err(|e| format!("启动 npx 失败: {}", e))
}

/// Strip options the bundled (older) Node doesn't understand — notably
/// `--use-system-ca`, which some corporate/machine setups inject into
/// NODE_OPTIONS and which Node 20 rejects.
fn filter_node_options() -> String {
    std::env::var("NODE_OPTIONS")
        .unwrap_or_default()
        .split_whitespace()
        .filter(|tok| !tok.starts_with("--use-system-ca"))
        .collect::<Vec<&str>>()
        .join(" ")
}

/// Stop the server and everything it spawned.
#[cfg(unix)]
fn terminate(child: &mut Child) {
    if child.try_wait().ok().flatten().is_none() {
        unsafe {
            libc::killpg(child.id() as i32, libc::SIGKILL);
        }
    }
    let _ = child.wait();
}

#[cfg(windows)]
fn terminate(child: &mut Child) {
    let _ = child.kill();
    let _ = child.wait();
}

/// Decode `icon/logo-480.png` into a `tao::window::Icon` for the window/title bar.
fn load_window_icon() -> Option<Icon> {
    let data = include_bytes!("../icon/logo-480.png");
    let decoder = png::Decoder::new(&data[..]);
    let mut reader = decoder.read_info().ok()?;
    let mut buf = vec![0u8; reader.output_buffer_size()];
    reader.next_frame(&mut buf).ok()?;
    let info = reader.info();
    Icon::from_rgba(buf, info.width, info.height).ok()
}

/// One row of the environment checklist.
fn checklist_item(ok: bool, label: &str, detail: &str) -> String {
    let cls = if ok { "ok" } else { "bad" };
    let mark = if ok { "✓" } else { "✗" };
    let det = if detail.is_empty() {
        String::new()
    } else {
        format!(" — {}", escape_html(detail))
    };
    format!(
        "<li class=\"{}\">{}{}{}</li>",
        cls,
        mark,
        escape_html(label),
        det
    )
}

/// Escape a string for safe embedding inside a JS template literal.
fn js_escape(s: &str) -> String {
    s.replace('\\', "\\\\")
        .replace('`', "\\`")
        .replace('$', "\\$")
}

fn escape_html(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
}

/// HTML shown when the command could not be started.
fn error_html(msg: &str) -> String {
    format!(
        r#"<!doctype html>
<html lang="zh"><head><meta charset="utf-8"/><title>启动失败</title>
<style>
  :root {{ color-scheme: dark; }}
  * {{ box-sizing: border-box; }}
  html, body {{ height: 100%; margin: 0; }}
  body {{ display:flex; flex-direction:column; align-items:center; justify-content:center; gap:18px;
         background:#0f1115; color:#e6e6e6; padding:32px; text-align:center;
         font-family:-apple-system,BlinkMacSystemFont,"Segoe UI",Roboto,"PingFang SC",sans-serif; }}
  h1 {{ font-size:20px; margin:0; color:#ff6b6b; }}
  .box {{ background:#1c2230; border:1px solid #2a3242; border-radius:8px; padding:16px 20px;
          max-width:560px; color:#9ecbff; font-size:13px; line-height:1.7; white-space:pre-wrap; word-break:break-word; }}
  code {{ background:#11151d; padding:2px 6px; border-radius:4px; }}
  .hint {{ font-size:12px; color:#8b93a3; }}
</style></head>
<body>
  <h1>无法启动命令</h1>
  <div class="box">{msg}</div>
  <div class="hint">关闭窗口即可退出。请确认网络可访问，或手动安装 Node.js 后用 <code>DSH_NPX</code> 指定 npx 的绝对路径重试。</div>
</body></html>"#,
        msg = escape_html(msg)
    )
}

const LOADING_HTML: &str = r#"<!doctype html>
<html lang="zh">
<head>
<meta charset="utf-8" />
<title>启动中…</title>
<style>
  :root { color-scheme: dark; }
  * { box-sizing: border-box; }
  html, body { height: 100%; margin: 0; }
  body {
    display: flex; flex-direction: column; align-items: center; justify-content: center;
    gap: 16px; background: #0f1115; color: #e6e6e6; padding: 32px;
    font-family: -apple-system, BlinkMacSystemFont, "Segoe UI", Roboto, "PingFang SC", sans-serif;
  }
  .spinner {
    width: 44px; height: 44px; border-radius: 50%;
    border: 4px solid #2a2f3a; border-top-color: #4f8cff;
    animation: spin 0.9s linear infinite;
  }
  @keyframes spin { to { transform: rotate(360deg); } }
  .title { font-size: 18px; font-weight: 600; text-align: center; }
  .sub { font-size: 13px; color: #8b93a3; max-width: 480px; text-align: center; line-height: 1.6; min-height: 18px; }
  .checklist { list-style: none; padding: 0; margin: 2px 0; width: 480px; max-width: 92vw; }
  .checklist li { font-size: 13px; padding: 8px 12px; border-radius: 6px; margin-bottom: 6px;
                  background: #161b25; border: 1px solid #232b39; word-break: break-all; }
  .checklist li.ok { color: #7ee0a0; border-color: #234a35; }
  .checklist li.bad { color: #ffb86b; border-color: #5a3d1f; }
  .progress-wrap { width: 480px; max-width: 92vw; }
  .bar { width: 100%; height: 10px; background: #1c2230; border-radius: 6px; overflow: hidden; }
  .bar-fill { width: 0%; height: 100%; background: linear-gradient(90deg,#4f8cff,#7ee0a0); transition: width .2s; }
  .pct { font-size: 12px; color: #8b93a3; text-align: right; margin-top: 4px; }
  code { background: #1c2230; padding: 2px 6px; border-radius: 4px; color: #9ecbff; }
</style>
</head>
<body>
  <div class="spinner" id="spinner"></div>
  <div class="title" id="title">正在启动 DeepSeek dsh Web…</div>
  <ul class="checklist" id="checklist"></ul>
  <div class="progress-wrap" id="progressWrap" style="display:none">
    <div class="bar"><div class="bar-fill" id="bar"></div></div>
    <div class="pct" id="pct">0%</div>
  </div>
  <div class="sub" id="sub"></div>

<script>
  function setStage(t) { document.getElementById('title').textContent = t; }
  function setSub(t) { document.getElementById('sub').innerHTML = t; }
  function setChecklist(html) { document.getElementById('checklist').innerHTML = html; }
  function showProgress(show) { document.getElementById('progressWrap').style.display = show ? 'block' : 'none'; }
  function setProgress(p) { document.getElementById('bar').style.width = p + '%'; document.getElementById('pct').textContent = p + '%'; }
</script>
</body>
</html>"#;
