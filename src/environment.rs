//! 环境自检 + 便携版 Node.js 自动安装。
//!
//! 应用启动时会在本机查找可用的 `npx`（由于 GUI `.app` 拿到的 `PATH`
//! 非常精简，所以需要主动探测常见安装位置）。如果没找到，就把一份
//! 便携版 Node.js 下载到 `~/.cache/dsh-desktop` —— 无需管理员权限 ——
//! 然后交给终端启动器继续执行。

use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::{Arc, Mutex};
use std::sync::atomic::AtomicBool;
use std::thread;
use std::time::Duration;

use tao::event_loop::EventLoopProxy;

use crate::{InputSink, ServerHandle, UserEvent};
use crate::recovery::launch_with_recovery;

/// 完整的启动流程：检查运行环境、列出缺失项、
/// 必要时自动安装 Node.js，然后在交互式终端中启动服务。
/// 启动失败时由 `recovery` 模块自动诊断并修复（禁用故障插件重试）。
pub fn run_environment_flow(
    proxy: EventLoopProxy<UserEvent>,
    handle: Arc<Mutex<Option<ServerHandle>>>,
    input_writer: InputSink,
    user_took_over: Arc<AtomicBool>,
) {
    // 立即切换到交互式终端视图，让整个启动过程 —— 环境自检、自动安装、
    // 命令自身的输出 —— 读起来就像一次完整的 shell 会话。
    let _ = proxy.send_event(UserEvent::EnterTerminal);
    let _ = proxy.send_event(UserEvent::Term("=== 启动前环境自检 ===\r\n".into()));
    let _ = proxy.send_event(UserEvent::Status("正在检查运行环境…".into()));

    // --- 0. 应用自更新检查（非致命；离线机器直接跳过） -------------------
    crate::updater::check_and_apply(&proxy);

    // --- 1. 环境检查 ------------------------------------------------------
    let node = resolve_npx();
    // 判断找到的 npx 是否可用。如果它对应的 Node 版本过低
    // （`@deepseek-ai/dsh` 运行时要求 Node >= v22.15.0，
    // 详见 MIN_NODE_MAJOR/MIN_NODE_MINOR 处列出的具体 API），
    // 则不再直接使用，转而走便携版 Node 安装流程，避免启动后才失败。
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

    // --- 2. 快速路径：本机已有可用的 Node ---------------------------------
    if let Some(npx) = usable_npx {
        let _ = proxy.send_event(UserEvent::Term("✓ 运行环境就绪，准备启动服务…\r\n".into()));
        launch_with_recovery(npx, proxy, handle, input_writer, user_took_over);
        return;
    }

    // --- 3. 慢速路径：自动安装便携版 Node ---------------------------------
    let _ = proxy.send_event(UserEvent::Term(format!("✗ {}\r\n", install_reason)));
    let _ = proxy.send_event(UserEvent::Status("正在准备自动安装 Node.js 运行环境…".into()));

    let target = node_target();
    let cache = match cache_dir() {
        Ok(c) => c,
        Err(e) => {
            let _ = proxy.send_event(UserEvent::Fatal(e));
            return;
        }
    };
    let home = cache.join(format!("node-v{}-{}", crate::NODE_VERSION, target));
    let npx = node_npx_path(&home);

    // 尚未安装过：下载并解压（下载进度通过 proxy 实时上报）
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
        launch_with_recovery(npx, proxy, handle, input_writer, user_took_over);
    } else {
        let _ = proxy.send_event(UserEvent::Fatal(
            "未能在自动安装的 Node.js 中找到 npx，安装失败。".into(),
        ));
    }
}

/// 把 `target` 平台的便携版 Node.js 压缩包下载到 `cache` 目录（下载进度
/// 通过 `proxy` 上报），然后解压。使用 `curl`（失败时回退到 `wget`）——
/// 这两个工具在 macOS、Windows 10+ 和大多数 Linux 发行版上都预装。
/// 无需管理员权限。
fn download_and_extract_node(
    target: &str,
    cache: &Path,
    proxy: &EventLoopProxy<UserEvent>,
) -> Result<(), String> {
    fs::create_dir_all(cache)
        .map_err(|e| format!("无法创建缓存目录 {}: {}", cache.display(), e))?;

    // Windows 用 zip 包，其余平台用 tar.gz 包
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

    // 提前拿到总大小，用于展示确定性进度条（尽力而为，失败则不显示百分比）。
    let total = http_content_length(&url).unwrap_or(0);

    let _ = proxy.send_event(UserEvent::Term(
        "正在从 npmmirror 镜像下载 Node.js 运行环境…\r\n".into(),
    ));
    let _ = proxy.send_event(UserEvent::Status("正在下载 Node.js 运行环境…".into()));

    // 启动下载（直接写入临时文件）
    let mut dl = Command::new("curl")
        .args(["-fsSL", &url, "-o", &tmp.to_string_lossy()])
        .spawn()
        .or_else(|_| {
            Command::new("wget")
                .args(["-q", &url, "-O", &tmp.to_string_lossy()])
                .spawn()
        })
        .map_err(|e| format!("无法启动下载工具（curl/wget 均不可用）: {}", e))?;

    // 轮询临时文件大小，在终端里渲染成不断原地刷新的进度行
    // （用回车符覆盖，就像真实 CLI 的下载进度条）。
    let mut last_pct: i32 = -1;
    loop {
        if let Ok(m) = fs::metadata(&tmp) {
            let pct = if total > 0 {
                ((m.len() as f64 / total as f64) * 100.0) as i32
            } else {
                -1
            };
            // 百分比变化时才刷新，避免刷屏
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

    // 结束原地刷新的进度行，进入解压阶段。
    let _ = proxy.send_event(UserEvent::Term("\r\n".into()));
    let _ = proxy.send_event(UserEvent::Term("✓ 下载完成，正在解压 Node.js…\r\n".into()));
    let _ = proxy.send_event(UserEvent::Status("正在解压 Node.js 运行环境…".into()));

    // Windows 用 PowerShell 的 Expand-Archive 解压，其余平台用 tar
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

    // 压缩包已无用，清理掉。
    let _ = fs::remove_file(&tmp);

    if !ok {
        return Err("已下载 Node.js 压缩包，但解压失败。".to_string());
    }
    let _ = proxy.send_event(UserEvent::Term("✓ 解压完成。\r\n".into()));
    let _ = proxy.send_event(UserEvent::Status("Node.js 运行环境已就绪".into()));
    Ok(())
}

/// 对 URL 发 HEAD 请求，返回 Content-Length（如果服务端提供）。
///
/// 必须带 `-L` 跟随重定向：GitHub Releases 的下载地址会 302 跳转到
/// `objects.githubusercontent.com`，真正的文件大小在最后一跳的响应头里
/// （取最后一个 content-length，忽略中间 302 响应的头）。
pub(crate) fn http_content_length(url: &str) -> Option<u64> {
    let out = Command::new("curl")
        .args(["-sIL", "--max-time", "20", url])
        .output()
        .ok()?;
    let text = String::from_utf8_lossy(&out.stdout);
    let mut len: Option<u64> = None;
    for line in text.lines() {
        let lower = line.to_ascii_lowercase();
        if let Some(rest) = lower.strip_prefix("content-length:") {
            // 持续覆盖：重定向链中最后一跳的头才是最终文件的大小。
            len = rest.trim().parse::<u64>().ok();
        }
    }
    len
}

/// `@deepseek-ai/dsh` 运行时所需的最低 Node.js 版本。
/// 该包使用了多个 v22 时代的 API：
///   - `node:zlib.createZstdDecompress`  (v22.15.0+)
///   - `Promise.withResolvers`           (v22.0.0+)
///   - `node:module.stripTypeScriptTypes`(v22.14.0+)
/// 因而下限是 **v22.15.0**。低于该版本 —— 包括 Node 20.x ——
/// 都会被拒绝，转而使用便携版 v22。
const MIN_NODE_MAJOR: u32 = 22;
const MIN_NODE_MINOR: u32 = 15;

/// `v` 是否不低于最低支持的 Node.js 版本。
fn node_meets_min(v: (u32, u32, u32)) -> bool {
    v.0 > MIN_NODE_MAJOR || (v.0 == MIN_NODE_MAJOR && v.1 >= MIN_NODE_MINOR)
}

/// 把 `node --version` 输出（如 `v20.9.0`）解析为 (major, minor, patch)。
fn parse_node_version(s: &str) -> Option<(u32, u32, u32)> {
    let s = s.trim().trim_start_matches('v');
    let parts: Vec<&str> = s.split('.').collect();
    if parts.len() < 2 {
        return None;
    }
    let major: u32 = parts[0].parse().ok()?;
    let minor: u32 = parts[1].parse().ok()?;
    // patch 段可能带后缀（如 "9.0-nightly20240101"），只取前导数字。
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

/// 返回 `npx` 所属 Node.js（同目录下的 `node`）的版本号。
/// 直接探测二进制文件，而不是依赖 `PATH` —— 从 GUI `.app` 启动时
/// `PATH` 非常精简，不可靠。
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

/// 定位一个真正能用来运行 `@deepseek-ai/dsh` 的 `npx` 可执行文件。
///
/// GUI `.app` 拿到的 `PATH` 非常精简，因此我们主动探测常见的 Node
/// 安装位置、nvm 目录以及（精简版的）PATH。关键在于：**不是**找到
/// 第一个存在的 `npx` 文件就直接返回 —— 一台机器上往往并存着多个
/// Node 安装，例如 `/usr/local/bin` 里一个过时的系统级 Node v20，
/// 同时 nvm 里又装了能用的 v22 —— 而排在最前的那个文件通常是错的、
/// 版本过低的那个（这正是"同一条命令在用户终端里能跑、在这个应用里
/// 失败"的原因）。所以我们按优先级顺序扫描，返回**第一个其 Node
/// 满足最低版本要求**的 `npx`（`node_meets_min`）。如果一个合格的
/// 都没有，就返回第一个存在的 `npx`，这样调用方能报告"版本过低"
/// 而不是"未安装"，之后才回退到下载的便携版 Node。
/// 可用环境变量 `DSH_NPX` 覆盖以上所有逻辑。
fn resolve_npx() -> Option<PathBuf> {
    // 环境变量指定的 npx 优先级最高
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
        // 记住第一个存在的 npx，作为"找不到合格版本"时的兜底
        if first_existing.is_none() {
            first_existing = Some(c.clone());
        }
        // 优先返回第一个 Node 版本达标的候选
        if let Some(v) = npx_node_version(&c) {
            if node_meets_min(v) {
                return Some(c);
            }
        }
        // 版本读不出来：继续扫描；仅在没有更好选择时才会兜底到它。
    }
    first_existing
}

/// 构造按优先级排序的 `npx` 候选路径列表。
fn npx_candidates() -> Vec<PathBuf> {
    let home = std::env::var("HOME").unwrap_or_default();
    let mut candidates: Vec<PathBuf> = vec![
        PathBuf::from("/usr/local/bin/npx"),
        PathBuf::from("/opt/homebrew/bin/npx"),
        PathBuf::from("/usr/bin/npx"),
        // 本开发环境中使用的托管运行时
        PathBuf::from("/Users/shaipe/.workbuddy/binaries/node/versions/22.22.2/bin/npx"),
    ];

    // nvm: $HOME/.nvm/versions/node/*/bin/npx
    let nvm_base = Path::new(&home).join(".nvm/versions/node");
    if let Ok(entries) = fs::read_dir(&nvm_base) {
        for entry in entries.flatten() {
            candidates.push(entry.path().join("bin/npx"));
        }
    }

    // 当前 PATH 上的所有目录
    if let Ok(path_env) = std::env::var("PATH") {
        for dir in path_env.split(':') {
            if !dir.is_empty() {
                candidates.push(PathBuf::from(dir).join("npx"));
            }
        }
    }

    candidates
}

/// 构造当前平台对应的 Node.js 发行包 target 后缀，
/// 例如 `darwin-arm64`、`linux-x64`、`win-x64`。
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

/// 应用管理下载的本地缓存目录（便携版 Node、升级包）。
/// 与 `updater.rs` 共用。
pub(crate) fn cache_dir() -> Result<PathBuf, String> {
    let base = if cfg!(windows) {
        std::env::var("LOCALAPPDATA")
            .unwrap_or_else(|_| "C:\\Users\\Public\\.cache".to_string())
    } else {
        let home = std::env::var("HOME").map_err(|_| "无法确定用户目录（HOME 未设置）。".to_string())?;
        format!("{}/.cache", home)
    };
    Ok(PathBuf::from(base).join("dsh-desktop"))
}

/// 便携版 Node 中 npx 的路径（Windows 与 Unix 目录结构不同）。
#[cfg(windows)]
fn node_npx_path(home: &Path) -> PathBuf {
    home.join("npx.cmd")
}
#[cfg(not(windows))]
fn node_npx_path(home: &Path) -> PathBuf {
    home.join("bin").join("npx")
}
