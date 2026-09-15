//! 启动 `npx -y @deepseek-ai/dsh web` 并把输出流式推送到 UI。
//!
//! Unix 上我们把命令放进 PTY（`forkpty`）里运行，使其表现得像真实
//! 终端一样、可接受交互输入；Windows 上回退为管道。输出会经过谨慎的
//! 解码（跨读取块被拆开的 UTF-8 会重新拼合），并且任何 `(y/N)` 形式的
//! 确认提示都会被检测到并自动回答一次。

use std::collections::HashMap;
use std::ffi::CString;
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::{Arc, Mutex};
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread;
use std::time::{Duration, Instant};

#[cfg(unix)]
use std::os::unix::io::RawFd;
#[cfg(windows)]
use std::process::{Child, Stdio};

use tao::event_loop::EventLoopProxy;

use crate::{ARGS, POLL_ADDR, TARGET_URL, InputSink, ServerHandle, UserEvent};
use crate::recovery::TAIL_LINES;

/// 一次 `launch_terminal` 的结局，由自动恢复流程（`recovery.rs`）消费：
/// 决定是正常结束、重试，还是彻底放弃。
pub enum LaunchOutcome {
    /// 服务端口已就绪，webview 已被导航（ServerReady 事件已发送）。
    Ready,
    /// 进程在端口就绪前退出（`crashed: true`）或等待超时（`crashed: false`）。
    Failed { crashed: bool },
    /// 已发生无法自动恢复的致命错误（Fatal 事件已发送），调用方应停止重试。
    Abort,
}

/// 追加式调试日志，用于排查启动/认证问题；任何线程都能安全调用，
/// 且绝不 panic。放在 /tmp 下方便用户分享。
pub fn dbg_log(msg: &str) {
    use std::io::Write as _;
    let ts = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis())
        .unwrap_or(0);
    if let Ok(mut f) = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open("/tmp/dsh-desktop-debug.log")
    {
        let _ = writeln!(f, "[{}] {}", ts, msg);
    }
}

/// 包装裸文件描述符，使我们可以把用户的按键写入 PTY。
#[cfg(unix)]
struct FdWriter(RawFd);
#[cfg(unix)]
impl Write for FdWriter {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        let n = unsafe { libc::write(self.0, buf.as_ptr() as *const libc::c_void, buf.len()) };
        if n < 0 {
            Err(io::Error::last_os_error())
        } else {
            Ok(n as usize)
        }
    }
    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

/// 在应用内交互式终端中启动 `npx -y @deepseek-ai/dsh web`，
/// 把输出流式回传给 UI，并等待 `127.0.0.1:3080` 就绪。
///
/// `extra_args` 会插入到 dsh 启动器自己的 flags 之后、`--no-open` 之前
/// （例如安全模式的 `--patch <file>`）；`term_tail` 用于保留本次尝试的
/// 纯文本输出尾巴，供失败后诊断故障插件使用。
pub fn launch_terminal(
    npx: PathBuf,
    proxy: EventLoopProxy<UserEvent>,
    handle: Arc<Mutex<Option<ServerHandle>>>,
    input_writer: InputSink,
    user_took_over: Arc<AtomicBool>,
    exited: Arc<AtomicBool>,
    term_tail: Arc<Mutex<Vec<String>>>,
    extra_args: &[String],
) -> LaunchOutcome {
    // dsh 启动器只认出现在首个未知 token 之前的自己的 flags，所以
    // `--patch` 之类的启动器参数必须放在 `--no-open`（应用层参数）之前。
    let mut args: Vec<String> = ARGS.iter().map(|s| s.to_string()).collect();
    if let Some(pos) = args.iter().position(|a| a == "--no-open") {
        for (i, a) in extra_args.iter().enumerate() {
            args.insert(pos + i, a.clone());
        }
    } else {
        args.extend(extra_args.iter().cloned());
    }
    let cmd = format!("{} {}", npx.display(), args.join(" "));
    dbg_log(&format!("LAUNCH: {}", cmd));
    // 与读取线程、服务轮询线程共享的"最近活动"时间戳。npm 静默下载
    // 依赖期间（CI/非 TTY 模式会抑制其进度输出），用它来展示
    // "仍在工作中"的心跳提示。
    let last_term = Arc::new(Mutex::new(Instant::now()));
    // 由 `dsh web` 打印出来的带认证的 URL（例如
    // `dsh web: http://127.0.0.1:3080?token=…`）。读取线程从子进程
    // stdout 中解析它，`wait_for_server` 消费它 —— 让 webview 跳转到
    // 带 token 的 URL，而不是裸端口（裸端口会渲染空白页 ——
    // 服务器会拒绝未认证的请求）。
    let server_url: Arc<Mutex<Option<String>>> = Arc::new(Mutex::new(None));

    // 环境自检流程已经把视图切到交互式终端并打印了"准备启动"阶段，
    // 这里只做幂等确认，并回显将要执行的命令。
    let _ = proxy.send_event(UserEvent::EnterTerminal);

    // 确保服务端口空闲。上一次启动遗留的 `dsh web`（例如应用被强杀
    // 而没来得及清理）会占着 127.0.0.1:3080，导致新命令绑定失败 ——
    // 以前这只会表现为一个莫名其妙的 5 分钟超时。
    #[cfg(unix)]
    if ensure_port_free(&proxy).is_err() {
        return LaunchOutcome::Abort;
    }

    // Unix 走 PTY，Windows 走管道
    #[cfg(unix)]
    let started = start_command_pty(
        &npx,
        &args,
        proxy.clone(),
        handle.clone(),
        input_writer.clone(),
        user_took_over.clone(),
        exited.clone(),
        last_term.clone(),
        server_url.clone(),
        term_tail.clone(),
    );
    #[cfg(windows)]
    let started = start_command_piped(
        &npx,
        &args,
        proxy.clone(),
        handle.clone(),
        input_writer.clone(),
        user_took_over.clone(),
        exited.clone(),
        last_term.clone(),
        server_url.clone(),
        term_tail.clone(),
    );

    match started {
        Ok(()) => {
            let _ = proxy.send_event(UserEvent::EnterTerminal);
            // 回显将要执行的命令，让终端读起来像真实的 shell。
            *last_term.lock().unwrap() = Instant::now();
            let _ = proxy.send_event(UserEvent::Term(format!("\r\n$ {}\r\n", cmd)));
            wait_for_server(proxy, handle, exited, last_term, server_url)
        }
        Err(e) => {
            let _ = proxy.send_event(UserEvent::Fatal(format!("启动命令失败: {}", e)));
            LaunchOutcome::Abort
        }
    }
}

/// 确保没有其他进程正在监听服务端口。上次启动遗留的 `dsh web`
/// （例如应用被强杀而没清理）会占着 127.0.0.1:3080，导致新命令
/// 绑定失败。我们会尝试自动释放端口 —— 只杀监听进程，且只在它
/// 看起来是我们自己的 node/dsh 进程时才动手；否则给出清晰、
/// 可操作的错误提示，而不是让用户面对一个莫名其妙的超时。
#[cfg(unix)]
fn ensure_port_free(proxy: &EventLoopProxy<UserEvent>) -> Result<(), ()> {
    if std::net::TcpStream::connect(POLL_ADDR).is_err() {
        return Ok(()); // 端口空闲
    }
    let _ = proxy.send_event(UserEvent::Stage("端口 3080 被占用，正在清理残留进程…".into()));
    // 找出监听 3080 的进程
    let out = Command::new("lsof")
        .args(["-tiTCP:3080", "-sTCP:LISTEN"])
        .output();
    if let Ok(out) = out {
        let pids = String::from_utf8_lossy(&out.stdout);
        let mut killed = false;
        for line in pids.lines() {
            if let Ok(pid) = line.trim().parse::<i32>() {
                // 查看进程命令行，确认是 dsh/node 进程才杀
                let cmd = Command::new("ps")
                    .args(["-p", &pid.to_string(), "-o", "command="])
                    .output();
                let cmd = cmd
                    .map(|c| String::from_utf8_lossy(&c.stdout).to_lowercase())
                    .unwrap_or_default();
                if cmd.contains("dsh") || cmd.contains("node") {
                    unsafe { libc::kill(pid, libc::SIGKILL); }
                    killed = true;
                }
            }
        }
        if killed {
            // 给内核一点时间释放端口，再复查一次
            thread::sleep(Duration::from_millis(1000));
            if std::net::TcpStream::connect(POLL_ADDR).is_err() {
                return Ok(());
            }
        }
    }
    let _ = proxy.send_event(UserEvent::Fatal(
        "端口 127.0.0.1:3080 已被其他进程占用（可能是上次未退出的 dsh 服务），且无法自动清理。\n请先在终端执行：\n  lsof -iTCP:3080 -sTCP:LISTEN\n找到对应的 PID 后，用 kill -9 <PID> 结束它，再重新打开本应用。".into(),
    ));
    Err(())
}

#[cfg(not(unix))]
fn ensure_port_free(_proxy: &EventLoopProxy<UserEvent>) -> Result<(), ()> {
    Ok(())
}

/// 轮询服务端口（无硬性超时），直到服务就绪、进程退出，或到达一个
/// 比较宽裕的上限为止 —— 用户全程可以在终端里查看/响应。
/// 返回结局给自动恢复流程决定后续（重试 / 放弃），不再直接发 Fatal。
#[allow(unused_variables)]
fn wait_for_server(
    proxy: EventLoopProxy<UserEvent>,
    handle: Arc<Mutex<Option<ServerHandle>>>,
    exited: Arc<AtomicBool>,
    last_term: Arc<Mutex<Instant>>,
    server_url: Arc<Mutex<Option<String>>>,
) -> LaunchOutcome {
    let mut ready = false;
    let mut last_beat: Option<Instant> = None;
    for _ in 0..1200 {
        // 约 10 分钟，但期间终端始终保持活跃。从原来的 5 分钟上调，
        // 因为首次运行时依赖下载慢是正常的，且应用内终端现在能
        // 同步展示进度。
        if exited.load(Ordering::SeqCst) {
            break;
        }
        // Windows（管道模式）没有 PTY 的退出通知，这里主动轮询子进程状态
        #[cfg(windows)]
        {
            if let Some(h) = handle.lock().unwrap().as_mut() {
                if h.child.try_wait().ok().flatten().is_some() {
                    exited.store(true, Ordering::SeqCst);
                }
            }
        }
        if std::net::TcpStream::connect(POLL_ADDR).is_ok() {
            ready = true;
            break;
        }

        // npm 下载依赖时没有任何输出。如果已经静默了几秒，就在状态栏
        // 安抚用户一下，而不是让终端看起来像卡死了。
        let silent = {
            let t = last_term.lock().unwrap();
            t.elapsed().as_millis() as u64
        };
        let now = Instant::now();
        let since_beat = last_beat
            .map(|b| now.duration_since(b).as_millis() as u64)
            .unwrap_or(u64::MAX);
        if silent >= 4000 && since_beat >= 4000 && !exited.load(Ordering::SeqCst) {
            let _ = proxy.send_event(UserEvent::Status(
                "命令仍在运行，正在下载 / 初始化依赖（首次运行通常较慢），请稍候…".into(),
            ));
            last_beat = Some(now);
        }

        thread::sleep(Duration::from_millis(500));
    }

    if ready {
        dbg_log("PORT_READY");
        // 端口已通，但 `dsh web: <url>` 这一行可能还没被读到 —— 先等
        // 读取线程解析出带认证的 URL，实在等不到才回退到裸端口。
        // 注意：锁的 guard 必须在进入等待循环前释放；若在同一语句里
        // 通过 `unwrap_or_else` 重新加锁会死锁（临时 guard 会活到
        // 语句结束）。
        let mut parsed: Option<String> = server_url.lock().unwrap().clone();
        if parsed.is_none() {
            let _ = proxy.send_event(UserEvent::Status(
                "服务端口已就绪，正在等待认证 URL…".into()
            ));
            // 服务器要求 token 时裸端口没用（只会渲染"需要认证"页面），
            // 所以要宽裕地等待 —— 某些 dsh 版本会先监听端口，
            // 过一会儿才打印 token URL。
            for i in 0..300 {
                if exited.load(Ordering::SeqCst) {
                    break;
                }
                thread::sleep(Duration::from_millis(100));
                parsed = server_url.lock().unwrap().clone();
                if parsed.is_some() {
                    break;
                }
                if i > 0 && i % 30 == 0 {
                    let _ = proxy.send_event(UserEvent::Status(format!(
                        "仍在等待认证 URL（{}s），可在上方终端查看 dsh 输出…",
                        i / 10
                    )));
                }
            }
        }
        let url = parsed.unwrap_or_else(|| {
            dbg_log("GRACE_EXPIRED_NO_URL -> fallback bare URL + watcher");
            // 最后手段：先打开裸 URL（可能显示"需要认证"页），
            // 同时在后台继续监听带认证的 URL —— 一旦解析到就重新跳转，
            // 无需用户任何操作即可恢复视图。
            spawn_late_url_watcher(proxy.clone(), server_url.clone(), exited.clone());
            let _ = proxy.send_event(UserEvent::Status(
                "暂未获取到认证 URL，先用默认地址打开；解析到认证地址后会自动跳转…".into()
            ));
            TARGET_URL.to_string()
        });
        dbg_log(&format!("NAVIGATE: {}", url));
        let _ = proxy.send_event(UserEvent::Status(format!("正在打开页面: {}", url)));
        let _ = proxy.send_event(UserEvent::ServerReady(url));
        LaunchOutcome::Ready
    } else {
        let crashed = exited.load(Ordering::SeqCst);
        dbg_log(&format!("WAIT_FAILED crashed={}", crashed));
        LaunchOutcome::Failed { crashed }
    }
}

/// webview 已经打开裸端口（最后手段的回退）之后，继续在后台监听
/// 带认证的 URL。某些 dsh 版本在端口开始监听后很久才打印 token URL；
/// 一旦它出现，我们就让 webview 重新跳转，用户无需手动复制 token。
fn spawn_late_url_watcher(
    proxy: EventLoopProxy<UserEvent>,
    server_url: Arc<Mutex<Option<String>>>,
    exited: Arc<AtomicBool>,
) {
    thread::spawn(move || {
        // 最多监听 10 分钟；子进程输出中出现 token URL 时，
        // 读取线程会填充 `server_url`。
        for _ in 0..1200 {
            if exited.load(Ordering::SeqCst) {
                return;
            }
            if let Some(u) = server_url.lock().unwrap().clone() {
                dbg_log(&format!("WATCHER_NAVIGATE: {}", u));
                let _ = proxy.send_event(UserEvent::Status(
                    "已获取认证 URL，正在重新打开页面…".into(),
                ));
                let _ = proxy.send_event(UserEvent::ServerReady(u));
                return;
            }
            thread::sleep(Duration::from_millis(500));
        }
    });
}

/// 从一行输出中提取带认证的服务 URL。优先匹配规范的
/// `dsh web: http://127.0.0.1:3080/?token=…` 前缀；作为对未来输出
/// 格式变化的兜底，也匹配任何指向我们端口、且带 `token=` 查询参数
/// 的 http(s) URL。
fn parse_server_url(line: &str) -> Option<String> {
    if let Some(pos) = line.find("dsh web:") {
        let rest = &line[pos + "dsh web:".len()..];
        if let Some(url) = rest.trim().split_whitespace().next() {
            if url.starts_with("http") {
                return Some(url.to_string());
            }
        }
    }
    // 兜底：扫描行内所有 http 开头的片段，找带 token 的本机端口 URL
    let mut idx = 0;
    while let Some(pos) = line[idx..].find("http") {
        let cand = &line[idx + pos..];
        let url = cand
            .split_whitespace()
            .next()
            .unwrap_or("")
            .trim_end_matches(|c: char| matches!(c, '\r' | ',' | ')' | '.' | ';' | '"'));
        if (url.contains("127.0.0.1:3080") || url.contains("localhost:3080"))
            && url.contains("token=")
        {
            return Some(url.to_string());
        }
        idx += pos + 4;
    }
    None
}

/// 启发式检测命令是否在等待 yes/no 确认（例如 npm 的 `(y/N)`、
/// `continue?` / `proceed?` 之类的问题）。刻意保持保守：
/// 误判最多只是多送一个 `y`，通常无害。
fn detect_prompt(s: &str) -> bool {
    if s.contains("(y/N)")
        || s.contains("[Y/n]")
        || s.contains("(Y/n)")
        || s.contains("(y/n)")
        || s.contains("[y/N]")
    {
        return true;
    }
    let lower = s.to_ascii_lowercase();
    if lower.contains("continue?")
        || lower.contains("proceed?")
        || lower.contains("confirm?")
        || lower.contains("yes or no")
        || lower.contains("do you want to")
    {
        return true;
    }
    for line in s.lines() {
        let t = line.trim();
        if t.ends_with('?') && (t.contains('y') || t.contains('n')) {
            return true;
        }
    }
    false
}

/// 从最近的输出中提取出真正的问题文本，让 UI 能把"在问什么"
/// 原样展示给用户，而不是一句泛泛的提示。
fn extract_prompt_text(recent: &str) -> String {
    // 取最近的最多 4 个非空行（从后往前数，再恢复原顺序）
    let lines: Vec<&str> = recent.lines().collect();
    let mut m: Vec<&str> = lines
        .iter()
        .rev()
        .filter(|l| !l.trim().is_empty())
        .take(4)
        .cloned()
        .collect();
    m.reverse();
    let joined = m.join("\n").trim().to_string();
    if joined.is_empty() {
        "需要确认（y/N）".to_string()
    } else {
        joined
    }
}

/// 去除 ANSI 转义序列（CSI + OSC）及其他控制字符，便于做提示检测、
/// 展示干净的问题文本。完整的 ANSI 输出仍会转发给 UI 做彩色渲染；
/// 这里的处理只服务于我们自己的（不可见的）记录。
fn strip_ansi(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut it = s.chars().peekable();
    while let Some(c) = it.next() {
        if c == '\u{1b}' {
            match it.peek() {
                Some(&'[') => {
                    it.next(); // 消费 '['
                    // 跳到 CSI 序列的结束字节（0x40..=0x7e）
                    for n in it.by_ref() {
                        if ('\u{40}'..='\u{7e}').contains(&n) {
                            break;
                        }
                    }
                }
                Some(&']') => {
                    it.next(); // 消费 ']'
                    // OSC 序列：直到 BEL 或 ST（ESC \）为止
                    for n in it.by_ref() {
                        if n == '\u{07}' {
                            break;
                        }
                        if n == '\u{1b}' {
                            if it.peek() == Some(&'\\') {
                                it.next();
                            }
                            break;
                        }
                    }
                }
                _ => {
                    it.next(); // 跳过零散 ESC 后的一个字符
                }
            }
            continue;
        }
        // 保留换行/回车/制表符，其余控制字符丢弃
        if c == '\n' || c == '\r' || c == '\t' {
            out.push(c);
        } else if (c as u32) >= 0x20 {
            out.push(c);
        }
    }
    out
}

#[cfg(unix)]
fn start_command_pty(
    npx: &Path,
    args: &[String],
    proxy: EventLoopProxy<UserEvent>,
    handle: Arc<Mutex<Option<ServerHandle>>>,
    input_writer: InputSink,
    user_took_over: Arc<AtomicBool>,
    exited: Arc<AtomicBool>,
    last_term: Arc<Mutex<Instant>>,
    server_url: Arc<Mutex<Option<String>>>,
    term_tail: Arc<Mutex<Vec<String>>>,
) -> io::Result<()> {
    use std::ptr;
    use std::os::unix::ffi::OsStrExt as _;

    // 字节串 -> CString（内部含 NUL 的参数会报错）
    let cz = |b: &[u8]| -> io::Result<CString> {
        CString::new(b).map_err(|e| io::Error::new(io::ErrorKind::InvalidInput, e.to_string()))
    };

    // 组装 argv: [npx, args..., NULL]
    let cpath = cz(npx.as_os_str().as_bytes())?;
    let mut cstrings: Vec<CString> = vec![cpath];
    let mut argv: Vec<*const libc::c_char> = vec![cstrings[0].as_ptr()];
    for a in args {
        let cs = cz(a.as_bytes())?;
        argv.push(cs.as_ptr());
        cstrings.push(cs);
    }
    argv.push(ptr::null());

    // 把 npx 所在目录前置到 PATH，保证子进程能找到同目录的 node 等工具
    let npx_dir = npx.parent().map(|p| p.to_string_lossy().to_string()).unwrap_or_default();
    let inherited = std::env::var("PATH").unwrap_or_default();
    let new_path = if npx_dir.is_empty() {
        inherited
    } else {
        format!("{}:{}", npx_dir, inherited)
    };
    let node_opts = filter_node_options();

    // 组装子进程环境变量
    let mut base: HashMap<String, String> = std::env::vars().collect();
    base.insert("PATH".into(), new_path);
    base.insert("NODE_OPTIONS".into(), node_opts);
    base.insert("npm_config_yes".into(), "true".into());
    // 让子进程表现得像真实的交互终端：输出 ANSI 颜色、用回车符重绘
    // 进度条。应用内终端现在已经能正确渲染这些（resources/app.js
    // 里的 ANSI 模拟器），而不是显示一堆原始转义乱码。
    base.insert("TERM".into(), "xterm-256color".into());
    base.insert("FORCE_COLOR".into(), "1".into());
    // npm_config_progress 保持默认（TTY 下开启），让下载进度条能显示。

    // 组装 envp: ["K=V", ..., NULL]
    let mut env_strings: Vec<CString> = Vec::new();
    let mut envp: Vec<*const libc::c_char> = Vec::new();
    for (k, v) in &base {
        let cs = cz(format!("{}={}", k, v).as_bytes())?;
        envp.push(cs.as_ptr());
        env_strings.push(cs);
    }
    envp.push(ptr::null());

    unsafe {
        let mut master: libc::c_int = -1;
        // PTY 的初始窗口尺寸：30 行 x 120 列
        let mut ws = libc::winsize {
            ws_row: 30,
            ws_col: 120,
            ws_xpixel: 0,
            ws_ypixel: 0,
        };
        let pid = libc::forkpty(
            &mut master,
            ptr::null_mut(),
            ptr::null_mut(),
            &mut ws as *mut libc::winsize,
        );
        if pid < 0 {
            return Err(io::Error::last_os_error());
        }
        if pid == 0 {
            // 子进程：关闭终端回显，避免自动喂进去的确认回答被回显成
            // 一行让人困惑的 `y`。
            let mut term: libc::termios = std::mem::zeroed();
            if libc::tcgetattr(0, &mut term) == 0 {
                term.c_lflag &= !libc::ECHO;
                libc::tcsetattr(0, libc::TCSANOW, &mut term);
            }
            // 子进程：在 PTY 中 exec 目标命令。
            libc::execve(cstrings[0].as_ptr(), argv.as_ptr(), envp.as_ptr());
            libc::_exit(127);
        }

        // ---- 父进程 ----
        *handle.lock().unwrap() = Some(ServerHandle { pid });
        *input_writer.lock().unwrap() = Some(Box::new(FdWriter(master)));

        // 起一个线程：把 PTY master 的输出流式转发给 UI。
        let rmaster = master;
        let rproxy = proxy.clone();
        let rexit = exited.clone();
        let rauto_in = input_writer.clone();
        let rauto_took = user_took_over.clone();
        let rlast = last_term.clone();
        let rurl = server_url.clone();
        let rtail = term_tail.clone();
        thread::spawn(move || {
            let mut buf = [0u8; 4096];
            // 累积原始字节：跨两次读取被拆开的多字节 UTF-8 字符
            // 不会在半截被解码（否则会出乱码）。
            let mut carry: Vec<u8> = Vec::with_capacity(1024);
            // 累积尚未换行的半截行：一个 URL 如果横跨两次 PTY 读取，
            // 要拼成一段再解析，而不是当成两个碎片。
            let mut line_buf: String = String::new();
            let mut last_auto: Option<Instant> = None;
            // 最近输出的滚动窗口 —— 用于在命令请求确认时展示
            // *真正的* 问题文本。
            let mut recent_lines: Vec<String> = Vec::new();
            loop {
                let n = libc::read(rmaster, buf.as_mut_ptr() as *mut libc::c_void, buf.len());
                if n <= 0 {
                    break;
                }
                carry.extend_from_slice(&buf[..n as usize]);
                // 找出最长的合法 UTF-8 前缀。
                let valid = match std::str::from_utf8(&carry) {
                    Ok(_) => carry.len(),
                    Err(e) => e.valid_up_to(),
                };
                if valid == 0 && carry.len() >= 4 {
                    // 一个多字节字符一直跨块：直接冲掉，
                    // 避免无限期卡住。
                    let s = String::from_utf8_lossy(&carry).into_owned();
                    carry.clear();
                    *rlast.lock().unwrap() = Instant::now();
                    let _ = rproxy.send_event(UserEvent::Term(s));
                    continue;
                }
                if valid > 0 {
                    let chunk = String::from_utf8_lossy(&carry[..valid]).into_owned();
                    carry.drain(..valid);
                    *rlast.lock().unwrap() = Instant::now();
                    // 转发原始（含 ANSI）输出，让应用内终端
                    // 能渲染真正的颜色 / 进度条。
                    let _ = rproxy.send_event(UserEvent::Term(chunk.clone()));
                    // 同时维护一小段*纯文本*（去掉 ANSI）滚动窗口，
                    // 用于 (a) 检测确认提示 (b) 展示不含转义序列的
                    // 问题原文。
                    let plain = strip_ansi(&chunk);
                    line_buf.push_str(&plain);
                    // 以 '\r' 或 '\n' 分行：有些 CLI 输出用裸回车符
                    // 结束一行（进度条重绘），而 token URL 不能被困在
                    // 一个永远不结束的行里。
                    while let Some(nl) =
                        line_buf.find(|c: char| c == '\n' || c == '\r')
                    {
                        let line = line_buf[..nl].to_string();
                        line_buf.drain(..=nl);
                        if line.trim().is_empty() {
                            continue; // \r\n 会留下一个空的第二行
                        }
                        dbg_log(&format!("LINE: {}", line));
                        recent_lines.push(line.clone());
                        // 同时保留一份更长的纯文本尾巴（覆盖整个失败现场），
                        // 供启动失败后诊断故障插件使用。
                        {
                            let mut t = rtail.lock().unwrap();
                            t.push(line.clone());
                            if t.len() > TAIL_LINES {
                                let cut = t.len() - TAIL_LINES;
                                t.drain(..cut);
                            }
                        }
                        // 解析 `dsh web: http://127.0.0.1:3080?token=…`
                        // 打印出来的带认证 URL，让 webview 跳转到它，
                        // 而不是裸端口（裸端口会是空白页）。
                        if let Some(url) = parse_server_url(&line) {
                            *rurl.lock().unwrap() = Some(url);
                        }
                    }
                    // 窗口只保留最近 20 行
                    if recent_lines.len() > 20 {
                        let drain_end = recent_lines.len() - 20;
                        recent_lines.drain(..drain_end);
                    }
                    // 命令在请求 (y/N) 确认时：自动回答一次，
                    // 并把它问的原话展示出来。
                    if detect_prompt(&plain) {
                        let now = Instant::now();
                        // 至少间隔 1.5s 才能再次自动回答，防止连续误触发
                        let can = match last_auto {
                            Some(t) => now.duration_since(t).as_millis() >= 1500,
                            None => true,
                        };
                        if can && !rauto_took.load(Ordering::SeqCst) {
                            if let Some(w) = rauto_in.lock().unwrap().as_mut() {
                                let _ = w.write_all(b"y\n");
                            }
                            last_auto = Some(now);
                            let q = extract_prompt_text(&recent_lines.join("\n"));
                            let _ = rproxy.send_event(UserEvent::Prompt(q));
                            let _ = rproxy.send_event(UserEvent::Term(
                                "\r\n[需要确认] 已自动回复 y（继续）。如需手动，请在下方输入框输入。\r\n".into(),
                            ));
                        }
                    }
                }
            }
            // PTY 读到了 EOF：子进程已退出。
            // 先把 line_buf 里没有换行符结尾的残留行（崩溃现场的最后一行
            // 往往如此）补进诊断尾巴，再标记退出。
            if !line_buf.trim().is_empty() {
                let leftover = line_buf.trim().to_string();
                dbg_log(&format!("LINE: {}", leftover));
                let mut t = rtail.lock().unwrap();
                t.push(leftover);
                if t.len() > TAIL_LINES {
                    let cut = t.len() - TAIL_LINES;
                    t.drain(..cut);
                }
            }
            dbg_log("READER_EXIT");
            rexit.store(true, Ordering::SeqCst);
            let _ = rproxy.send_event(UserEvent::TermDone("进程已退出".into()));
            let _ = libc::close(rmaster);
        });

        Ok(())
    }
}

#[cfg(windows)]
fn start_command_piped(
    npx: &Path,
    args: &[String],
    proxy: EventLoopProxy<UserEvent>,
    handle: Arc<Mutex<Option<ServerHandle>>>,
    input_writer: InputSink,
    _user_took_over: Arc<AtomicBool>,
    exited: Arc<AtomicBool>,
    _last_term: Arc<Mutex<Instant>>,
    server_url: Arc<Mutex<Option<String>>>,
    term_tail: Arc<Mutex<Vec<String>>>,
) -> io::Result<()> {
    // 把 npx 所在目录前置到 PATH（Windows 用分号分隔）
    let npx_dir = npx.parent().map(|p| p.to_string_lossy().to_string()).unwrap_or_default();
    let inherited = std::env::var("PATH").unwrap_or_default();
    let new_path = format!("{};{}", npx_dir, inherited);

    // Windows 没有 forkpty，用管道方式启动
    let mut child = Command::new(npx)
        .args(args)
        .env("PATH", new_path)
        .env("NODE_OPTIONS", filter_node_options())
        .env("npm_config_yes", "true")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|e| io::Error::new(io::ErrorKind::Other, format!("启动 npx 失败: {}", e)))?;

    let mut stdin = child.stdin.take().unwrap();
    let stdout = child.stdout.take().unwrap();
    let stderr = child.stderr.take().unwrap();
    *input_writer.lock().unwrap() = Some(Box::new(stdin));
    *handle.lock().unwrap() = Some(ServerHandle { child });

    // stdout / stderr 各起一个读取线程
    spawn_reader(stdout, proxy.clone(), server_url.clone(), term_tail.clone());
    spawn_reader(stderr, proxy.clone(), server_url, term_tail);
    Ok(())
}

#[cfg(windows)]
fn spawn_reader(
    mut stream: impl io::Read + Send + 'static,
    proxy: EventLoopProxy<UserEvent>,
    server_url: Arc<Mutex<Option<String>>>,
    term_tail: Arc<Mutex<Vec<String>>>,
) {
    thread::spawn(move || {
        let mut buf = [0u8; 4096];
        loop {
            match stream.read(&mut buf) {
                Ok(0) => break,
                Ok(n) => {
                    let chunk = String::from_utf8_lossy(&buf[..n]).to_string();
                    let plain = strip_ansi(&chunk);
                    for line in plain.lines() {
                        if let Some(url) = parse_server_url(line) {
                            *server_url.lock().unwrap() = Some(url);
                        }
                        let mut t = term_tail.lock().unwrap();
                        t.push(line.to_string());
                        if t.len() > TAIL_LINES {
                            let tail_start = t.len() - TAIL_LINES;
                            t.drain(..tail_start);
                        }
                    }
                    let _ = proxy.send_event(UserEvent::Term(chunk));
                }
                Err(_) => break,
            }
        }
    });
}

/// 过滤掉内置（较旧）Node 不认识的选项 —— 尤其是
/// `--use-system-ca`：某些公司/机器环境会把它注入 NODE_OPTIONS，
/// 而 Node 20 会直接拒绝该选项。
fn filter_node_options() -> String {
    std::env::var("NODE_OPTIONS")
        .unwrap_or_default()
        .split_whitespace()
        .filter(|tok| !tok.starts_with("--use-system-ca"))
        .collect::<Vec<&str>>()
        .join(" ")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_real_dsh_output() {
        // 真实抓取的输出行：转圈字符 + 无 ANSI 的纯文本，已去掉 \r\n。
        let line = "⠙⠹⠸⠼⠴⠦⠧⠇⠏⠋dsh web: http://127.0.0.1:3080/?token=3I3a0EnMMRmHOqnVxOSLFhorURk-N8xCSYuvEvsikY0";
        assert_eq!(
            parse_server_url(line).unwrap(),
            "http://127.0.0.1:3080/?token=3I3a0EnMMRmHOqnVxOSLFhorURk-N8xCSYuvEvsikY0"
        );
    }

    #[test]
    fn parses_url_without_prefix() {
        let line = "Open in browser: http://localhost:3080/?token=abc123, enjoy!";
        assert_eq!(
            parse_server_url(line).unwrap(),
            "http://localhost:3080/?token=abc123"
        );
    }

    #[test]
    fn ignores_noise() {
        assert!(parse_server_url("npm warn deprecated foo@1.0.0").is_none());
        assert!(parse_server_url("http://127.0.0.1:3080 no token here").is_none());
    }
}
