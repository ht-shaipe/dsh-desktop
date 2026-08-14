//! Environment check + portable Node.js install.
//!
//! On launch we look for an `npx` on the machine (probing common locations,
//! since a GUI `.app` gets a minimal `PATH`). If none is found we download a
//! portable Node.js build into `~/.cache/dsh-desktop` — no admin rights needed
//! — then hand off to the terminal launcher.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::{Arc, Mutex};
use std::sync::atomic::AtomicBool;
use std::thread;
use std::time::Duration;

use tao::event_loop::EventLoopProxy;

use crate::{InputSink, ServerHandle, UserEvent};
use crate::terminal::launch_terminal;

/// Full startup sequence: check the environment, list what's missing,
/// auto-install Node.js if needed, then launch the server in an interactive
/// terminal.
pub fn run_environment_flow(
    proxy: EventLoopProxy<UserEvent>,
    handle: Arc<Mutex<Option<ServerHandle>>>,
    input_writer: InputSink,
    user_took_over: Arc<AtomicBool>,
    exited: Arc<AtomicBool>,
) {
    // Show the interactive terminal right away so the whole startup — env check,
    // auto-install, and the command's own output — reads like one shell session.
    let _ = proxy.send_event(UserEvent::EnterTerminal);
    let _ = proxy.send_event(UserEvent::Term("=== 启动前环境自检 ===\r\n".into()));
    let _ = proxy.send_event(UserEvent::Status("正在检查运行环境…".into()));

    // --- 1. Environment check -------------------------------------------
    let node = resolve_npx();
    // Decide whether the found npx is usable. If its Node is too old (the
    // `@deepseek-ai/dsh` package requires Node >= v22.15.0 at runtime — see
    // MIN_NODE_MAJOR/MIN_NODE_MINOR for the exact APIs), we fall through to the
    // portable-Node install instead of failing later.
    let (usable_npx, node_line, install_reason): (Option<PathBuf>, String, String) = match &node {
        Some(p) => match npx_node_version(p) {
            Some(v) if node_meets_min(v) => (
                Some(p.clone()),
                format!(
                    "✓ Node.js 运行环境 (npx)\r\n    已找到: {} (Node v{}.{}.{})\r\n",
                    p.display(),
                    v.0,
                    v.1,
                    v.2
                ),
                String::new(),
            ),
            Some(v) => (
                None,
                format!(
                    "✗ 已检测到 Node.js，但版本过低\r\n    已找到: {} (Node v{}.{}.{})，需 >= v{}.{}\r\n",
                    p.display(),
                    v.0,
                    v.1,
                    v.2,
                    MIN_NODE_MAJOR,
                    MIN_NODE_MINOR
                ),
                format!(
                    "Node.js 版本过低 (v{}.{}.{})，正在自动下载兼容的便携版 Node v{}…",
                    v.0, v.1, v.2, crate::NODE_VERSION
                ),
            ),
            None => (
                Some(p.clone()),
                format!(
                    "✓ Node.js 运行环境 (npx)\r\n    已找到: {}（未能检测版本，将尝试使用）\r\n",
                    p.display()
                ),
                String::new(),
            ),
        },
        None => (
            None,
            "✗ Node.js 运行环境 (npx)\r\n    本机未安装\r\n".to_string(),
            "本机缺少 Node.js，正在自动下载并安装便携版…".to_string(),
        ),
    };

    let _ = proxy.send_event(UserEvent::Term(node_line));
    let _ = proxy.send_event(UserEvent::Term(
        "• @deepseek-ai/dsh 命令包：将通过 npx 首次运行时自动获取（无需单独安装）\r\n".into(),
    ));

    // --- 2. Fast path: a usable Node is already present ----------------
    if let Some(npx) = usable_npx {
        let _ = proxy.send_event(UserEvent::Term("✓ 运行环境就绪，准备启动服务…\r\n".into()));
        launch_terminal(npx, proxy, handle, input_writer, user_took_over, exited);
        return;
    }

    // --- 3. Slow path: auto-install a portable Node ---------------------
    let _ = proxy.send_event(UserEvent::Term(format!("✗ {}\r\n", install_reason)));
    let _ = proxy.send_event(UserEvent::Status("正在准备自动安装 Node.js 运行环境…".into()));

    let target = node_target();
    let cache = match node_cache_dir() {
        Ok(c) => c,
        Err(e) => {
            let _ = proxy.send_event(UserEvent::Fatal(e));
            return;
        }
    };
    let home = cache.join(format!("node-v{}-{}", crate::NODE_VERSION, target));
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
        let _ = proxy.send_event(UserEvent::Term(format!(
            "✓ 已安装 Node.js: {}\r\n",
            npx.display()
        )));
        launch_terminal(npx, proxy, handle, input_writer, user_took_over, exited);
    } else {
        let _ = proxy.send_event(UserEvent::Fatal(
            "未能在自动安装的 Node.js 中找到 npx，安装失败。".into(),
        ));
    }
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
                crate::NODE_MIRROR, crate::NODE_VERSION, crate::NODE_VERSION, target
            ),
            "zip",
        )
    } else {
        (
            format!(
                "{}/v{}/node-v{}-{}.tar.gz",
                crate::NODE_MIRROR, crate::NODE_VERSION, crate::NODE_VERSION, target
            ),
            "tar.gz",
        )
    };

    let tmp = cache.join(format!("node-{}.{}", target, ext));

    // Total size for a determinate progress bar (best effort).
    let total = http_content_length(&url).unwrap_or(0);

    let _ = proxy.send_event(UserEvent::Term(
        "正在从 npmmirror 镜像下载 Node.js 运行环境…\r\n".into(),
    ));
    let _ = proxy.send_event(UserEvent::Status("正在下载 Node.js 运行环境…".into()));

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

    // Poll the temp file size and render it as a live, rewriting terminal line
    // (carriage-return progress, just like a real CLI download bar).
    let mut last_pct: i32 = -1;
    loop {
        if let Ok(m) = fs::metadata(&tmp) {
            let pct = if total > 0 {
                ((m.len() as f64 / total as f64) * 100.0) as i32
            } else {
                -1
            };
            if pct != last_pct {
                last_pct = pct;
                if pct >= 0 {
                    let line = format!("  下载进度 {:3}%\r", pct.min(99));
                    let _ = proxy.send_event(UserEvent::Term(line));
                    let _ = proxy.send_event(UserEvent::Status(format!(
                        "正在下载 Node.js 运行环境… {}%",
                        pct.min(99)
                    )));
                } else {
                    let _ = proxy.send_event(UserEvent::Term("  下载中…\r".into()));
                    let _ = proxy.send_event(UserEvent::Status(
                        "正在下载 Node.js 运行环境…".into(),
                    ));
                }
            }
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

    // Finish the rewriting progress line, then move on.
    let _ = proxy.send_event(UserEvent::Term("\r\n".into()));
    let _ = proxy.send_event(UserEvent::Term("✓ 下载完成，正在解压 Node.js…\r\n".into()));
    let _ = proxy.send_event(UserEvent::Status("正在解压 Node.js 运行环境…".into()));

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
    let _ = proxy.send_event(UserEvent::Term("✓ 解压完成。\r\n".into()));
    let _ = proxy.send_event(UserEvent::Status("Node.js 运行环境已就绪".into()));
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

/// Minimum Node.js version required by `@deepseek-ai/dsh` at runtime. The
/// package uses several v22-era APIs:
///   - `node:zlib.createZstdDecompress`  (v22.15.0+)
///   - `Promise.withResolvers`           (v22.0.0+)
///   - `node:module.stripTypeScriptTypes`(v22.14.0+)
/// so the floor is **v22.15.0**. Anything below that — including Node 20.x —
/// is rejected and the portable v22 build is used instead.
const MIN_NODE_MAJOR: u32 = 22;
const MIN_NODE_MINOR: u32 = 15;

/// True when `v` is at least the minimum supported Node.js version.
fn node_meets_min(v: (u32, u32, u32)) -> bool {
    v.0 > MIN_NODE_MAJOR || (v.0 == MIN_NODE_MAJOR && v.1 >= MIN_NODE_MINOR)
}

/// Parse a `node --version` string like `v20.9.0` into (major, minor, patch).
fn parse_node_version(s: &str) -> Option<(u32, u32, u32)> {
    let s = s.trim().trim_start_matches('v');
    let parts: Vec<&str> = s.split('.').collect();
    if parts.len() < 2 {
        return None;
    }
    let major: u32 = parts[0].parse().ok()?;
    let minor: u32 = parts[1].parse().ok()?;
    let patch: u32 = if parts.len() > 2 {
        parts[2]
            .split(|c: char| !c.is_ascii_digit())
            .next()
            .and_then(|p| p.parse().ok())
            .unwrap_or(0)
    } else {
        0
    };
    Some((major, minor, patch))
}

/// Return the version of the Node.js that owns `npx` (its sibling `node`).
/// Probes the binary directly instead of relying on `PATH`, which is minimal
/// when launched from a GUI `.app`.
fn npx_node_version(npx: &Path) -> Option<(u32, u32, u32)> {
    let node = npx.parent()?.join("node");
    if !node.is_file() {
        return None;
    }
    let out = Command::new(&node).arg("--version").output().ok()?;
    if !out.status.success() {
        return None;
    }
    parse_node_version(&String::from_utf8_lossy(&out.stdout))
}

/// Locate an `npx` executable we can actually run `@deepseek-ai/dsh` with.
///
/// A GUI `.app` gets a minimal `PATH`, so we probe common Node install
/// locations, nvm, and the (minimal) PATH. Crucially we do **not** just return
/// the first existing `npx` file: a machine often has several Node installs
/// side by side — e.g. an old system Node v20 at `/usr/local/bin` *and* a
/// working v22 installed via nvm — and the first file on disk is usually the
/// wrong, too-old one (which is exactly why the same command can work in the
/// user's terminal but fail inside this app). So we scan in priority order and
/// return the **first `npx` whose Node meets the minimum version**
/// (`node_meets_min`). If none qualify we return the first existing `npx`
/// anyway, so the caller can report "version too low" rather than "not
/// installed", and only then do we fall back to the downloaded portable Node.
/// Override everything with `DSH_NPX`.
fn resolve_npx() -> Option<PathBuf> {
    if let Ok(p) = std::env::var("DSH_NPX") {
        let p = PathBuf::from(p);
        if p.is_file() {
            return Some(p);
        }
    }

    let mut first_existing: Option<PathBuf> = None;
    for c in npx_candidates() {
        if !c.is_file() {
            continue;
        }
        if first_existing.is_none() {
            first_existing = Some(c.clone());
        }
        // Prefer the first candidate whose Node is new enough.
        if let Some(v) = npx_node_version(&c) {
            if node_meets_min(v) {
                return Some(c);
            }
        }
        // Unreadable version: keep scanning; fall back to it only if nothing
        // better turns up.
    }
    first_existing
}

/// Build the ordered list of candidate `npx` paths.
fn npx_candidates() -> Vec<PathBuf> {
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

    candidates
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
