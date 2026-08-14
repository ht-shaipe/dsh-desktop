//! Launches `npx -y @deepseek-ai/dsh web` and streams its output to the UI.
//!
//! On Unix we run the command inside a PTY (`forkpty`) so it behaves like a real
//! terminal and can accept interactive input; on Windows we fall back to pipes.
//! The output is decoded carefully (UTF-8 split across reads is reassembled) and
//! any `(y/N)`-style confirmation prompt is detected and auto-answered once.

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

use crate::{ARGS, POLL_ADDR, InputSink, ServerHandle, UserEvent};

/// Wraps a raw file descriptor so we can write user keystrokes into the PTY.
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

/// Launch `npx -y @deepseek-ai/dsh web` inside the in-app interactive
/// terminal, stream its output back to the UI, and wait for `127.0.0.1:3080`.
pub fn launch_terminal(
    npx: PathBuf,
    proxy: EventLoopProxy<UserEvent>,
    handle: Arc<Mutex<Option<ServerHandle>>>,
    input_writer: InputSink,
    user_took_over: Arc<AtomicBool>,
    exited: Arc<AtomicBool>,
) {
    let cmd = format!("{} {}", npx.display(), ARGS.join(" "));
    // Activity timestamp shared with the reader + server poller. Used to show a
    // "still working" heartbeat while npm downloads dependencies silently
    // (CI + non-TTY mode suppresses its progress output).
    let last_term = Arc::new(Mutex::new(Instant::now()));

    // The environment flow already switched the view to the interactive terminal
    // and logged the "准备启动" phase, so here we just make sure the view is up
    // (idempotent) and echo the command being run.
    let _ = proxy.send_event(UserEvent::EnterTerminal);

    // Make sure the server port is free. A stale `dsh web` left over from a
    // previous launch that wasn't cleaned up (e.g. the app was force-quit)
    // would hold 127.0.0.1:3080 and make the new command fail to bind — which
    // previously surfaced only as a confusing 5-minute timeout.
    #[cfg(unix)]
    if ensure_port_free(&proxy).is_err() {
        return;
    }

    #[cfg(unix)]
    let started = start_command_pty(
        &npx,
        proxy.clone(),
        handle.clone(),
        input_writer.clone(),
        user_took_over.clone(),
        exited.clone(),
        last_term.clone(),
    );
    #[cfg(windows)]
    let started = start_command_piped(
        &npx,
        proxy.clone(),
        handle.clone(),
        input_writer.clone(),
        user_took_over.clone(),
        exited.clone(),
        last_term.clone(),
    );

    match started {
        Ok(()) => {
            let _ = proxy.send_event(UserEvent::EnterTerminal);
            // Echo the command being run so the terminal reads like a real shell.
            *last_term.lock().unwrap() = Instant::now();
            let _ = proxy.send_event(UserEvent::Term(format!("\r\n$ {}\r\n", cmd)));
            wait_for_server(proxy, handle, exited, last_term);
        }
        Err(e) => {
            let _ = proxy.send_event(UserEvent::Fatal(format!("启动命令失败: {}", e)));
        }
    }
}

/// Make sure nothing is already listening on the server port. A stale
/// `dsh web` from a previous launch that wasn't cleaned up (e.g. the app was
/// force-quit) would hold 127.0.0.1:3080, causing the new command to fail to
/// bind. We try to free it automatically — only killing the listener, and only
/// if it looks like a node/dsh process we likely own — else we surface a clear,
/// actionable error instead of a confusing timeout.
#[cfg(unix)]
fn ensure_port_free(proxy: &EventLoopProxy<UserEvent>) -> Result<(), ()> {
    if std::net::TcpStream::connect(POLL_ADDR).is_err() {
        return Ok(()); // port is free
    }
    let _ = proxy.send_event(UserEvent::Stage("端口 3080 被占用，正在清理残留进程…".into()));
    let out = Command::new("lsof")
        .args(["-tiTCP:3080", "-sTCP:LISTEN"])
        .output();
    if let Ok(out) = out {
        let pids = String::from_utf8_lossy(&out.stdout);
        let mut killed = false;
        for line in pids.lines() {
            if let Ok(pid) = line.trim().parse::<i32>() {
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

/// Poll the server port (no hard timeout) until it's up, the process exits, or
/// a generous limit passes — the user can watch/respond in the terminal.
#[allow(unused_variables)]
fn wait_for_server(
    proxy: EventLoopProxy<UserEvent>,
    handle: Arc<Mutex<Option<ServerHandle>>>,
    exited: Arc<AtomicBool>,
    last_term: Arc<Mutex<Instant>>,
) {
    let mut ready = false;
    let mut last_beat: Option<Instant> = None;
    for _ in 0..1200 {
        // ~10 minutes, but the terminal stays live the whole time. Bumped from 5
        // minutes because a slow first-time dependency download can legitimately
        // take longer, and the in-app terminal now shows progress meanwhile.
        if exited.load(Ordering::SeqCst) {
            break;
        }
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

        // While npm downloads dependencies it produces no output. If we've been
        // silent for a few seconds, reassure the user via the status bar instead
        // of leaving the terminal looking frozen.
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
        let _ = proxy.send_event(UserEvent::Status("服务已就绪，正在打开页面…".into()));
        let _ = proxy.send_event(UserEvent::ServerReady);
    } else {
        let msg = if exited.load(Ordering::SeqCst) {
            "命令已退出，但 127.0.0.1:3080 未就绪。请查看上方终端输出，按需输入指令后重启应用重试。"
        } else {
            "命令运行超过 10 分钟仍未监听 127.0.0.1:3080。请查看上方终端输出，必要时输入指令继续；若端口被占用，请先结束占用 3080 的进程再试。"
        };
        let _ = proxy.send_event(UserEvent::Fatal(msg.into()));
    }
}

/// Heuristically detect that the command is prompting for a yes/no confirmation
/// (e.g. npm's `(y/N)`, a `continue?` / `proceed?` question, …). Conservative on
/// purpose: a false positive only sends an extra `y`, which is usually harmless.
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

/// Pull the actual question text out of recent output so the UI can show the
/// user exactly what they're being asked, instead of a generic notice.
fn extract_prompt_text(recent: &str) -> String {
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

/// Strip ANSI escape sequences (CSI + OSC) and other control characters so we
/// can run prompt detection and surface clean question text. The full ANSI
/// output is still forwarded to the UI for colored rendering; this is only for
/// our own (invisible) bookkeeping.
fn strip_ansi(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut it = s.chars().peekable();
    while let Some(c) = it.next() {
        if c == '\u{1b}' {
            match it.peek() {
                Some(&'[') => {
                    it.next(); // consume '['
                    // skip until the final CSI byte (0x40..=0x7e)
                    for n in it.by_ref() {
                        if ('\u{40}'..='\u{7e}').contains(&n) {
                            break;
                        }
                    }
                }
                Some(&']') => {
                    it.next(); // consume ']'
                    // OSC: until BEL or ST (ESC \)
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
                    it.next(); // skip one char after a stray ESC
                }
            }
            continue;
        }
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
    proxy: EventLoopProxy<UserEvent>,
    handle: Arc<Mutex<Option<ServerHandle>>>,
    input_writer: InputSink,
    user_took_over: Arc<AtomicBool>,
    exited: Arc<AtomicBool>,
    last_term: Arc<Mutex<Instant>>,
) -> io::Result<()> {
    use std::ptr;
    use std::os::unix::ffi::OsStrExt as _;

    let cz = |b: &[u8]| -> io::Result<CString> {
        CString::new(b).map_err(|e| io::Error::new(io::ErrorKind::InvalidInput, e.to_string()))
    };

    let cpath = cz(npx.as_os_str().as_bytes())?;
    let mut cstrings: Vec<CString> = vec![cpath];
    let mut argv: Vec<*const libc::c_char> = vec![cstrings[0].as_ptr()];
    for a in ARGS {
        let cs = cz(a.as_bytes())?;
        argv.push(cs.as_ptr());
        cstrings.push(cs);
    }
    argv.push(ptr::null());

    let npx_dir = npx.parent().map(|p| p.to_string_lossy().to_string()).unwrap_or_default();
    let inherited = std::env::var("PATH").unwrap_or_default();
    let new_path = if npx_dir.is_empty() {
        inherited
    } else {
        format!("{}:{}", npx_dir, inherited)
    };
    let node_opts = filter_node_options();

    let mut base: HashMap<String, String> = std::env::vars().collect();
    base.insert("PATH".into(), new_path);
    base.insert("NODE_OPTIONS".into(), node_opts);
    base.insert("npm_config_yes".into(), "true".into());
    // Let the child behave like a real interactive terminal: emit ANSI colors and
    // redraw progress bars via carriage returns. The in-app terminal renders
    // these properly now (ANSI emulator in resources/app.js) instead of showing
    // raw escape garbage.
    base.insert("TERM".into(), "xterm-256color".into());
    base.insert("FORCE_COLOR".into(), "1".into());
    // npm_config_progress left at its default (on for a TTY) so the download
    // progress bar actually shows.

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
            // Child: turn off terminal echo so auto-fed confirmations don't
            // get printed back as confusing `y` lines in our terminal view.
            let mut term: libc::termios = std::mem::zeroed();
            if libc::tcgetattr(0, &mut term) == 0 {
                term.c_lflag &= !libc::ECHO;
                libc::tcsetattr(0, libc::TCSANOW, &term);
            }
            // Child: exec the command in the PTY.
            libc::execve(cstrings[0].as_ptr(), argv.as_ptr(), envp.as_ptr());
            libc::_exit(127);
        }

        // Parent.
        *handle.lock().unwrap() = Some(ServerHandle { pid });
        *input_writer.lock().unwrap() = Some(Box::new(FdWriter(master)));

        // Stream output from the PTY master to the UI.
        let rmaster = master;
        let rproxy = proxy.clone();
        let rexit = exited.clone();
        let rauto_in = input_writer.clone();
        let rauto_took = user_took_over.clone();
        let rlast = last_term.clone();
        thread::spawn(move || {
            let mut buf = [0u8; 4096];
            // Accumulate raw bytes so multi-byte UTF-8 chars that span two
            // reads are not decoded mid-character (which produces garbage).
            let mut carry: Vec<u8> = Vec::with_capacity(1024);
            let mut last_auto: Option<Instant> = None;
            // Rolling window of recent output lines — used to surface the
            // *actual* question text when the command asks for confirmation.
            let mut recent_lines: Vec<String> = Vec::new();
            loop {
                let n = libc::read(rmaster, buf.as_mut_ptr() as *mut libc::c_void, buf.len());
                if n <= 0 {
                    break;
                }
                carry.extend_from_slice(&buf[..n as usize]);
                // Find the longest valid UTF-8 prefix.
                let valid = match std::str::from_utf8(&carry) {
                    Ok(_) => carry.len(),
                    Err(e) => e.valid_up_to(),
                };
                if valid == 0 && carry.len() >= 4 {
                    // A lone multibyte char keeps spanning chunks; flush it to
                    // avoid an unbounded stall.
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
                    // Forward the raw (ANSI-containing) output so the in-app
                    // terminal can render real colors / progress bars.
                    let _ = rproxy.send_event(UserEvent::Term(chunk.clone()));
                    // Keep a small rolling window of *plain* (ANSI-stripped) output
                    // so we can (a) detect prompts and (b) surface the literal
                    // question text without escape codes.
                    let plain = strip_ansi(&chunk);
                    for line in plain.lines() {
                        recent_lines.push(line.to_string());
                    }
                    if recent_lines.len() > 20 {
                        recent_lines.drain(..recent_lines.len() - 20);
                    }
                    // If the command is asking for a (y/N) confirmation, answer
                    // it once and show the exact question it asked.
                    if detect_prompt(&plain) {
                        let now = Instant::now();
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
    proxy: EventLoopProxy<UserEvent>,
    handle: Arc<Mutex<Option<ServerHandle>>>,
    input_writer: InputSink,
    _user_took_over: Arc<AtomicBool>,
    _exited: Arc<AtomicBool>,
    _last_term: Arc<Mutex<Instant>>,
) -> io::Result<()> {
    let npx_dir = npx.parent().map(|p| p.to_string_lossy().to_string()).unwrap_or_default();
    let inherited = std::env::var("PATH").unwrap_or_default();
    let new_path = format!("{};{}", npx_dir, inherited);

    let mut child = Command::new(npx)
        .args(ARGS)
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

    spawn_reader(stdout, proxy.clone());
    spawn_reader(stderr, proxy.clone());
    Ok(())
}

#[cfg(windows)]
fn spawn_reader(mut stream: impl io::Read + Send + 'static, proxy: EventLoopProxy<UserEvent>) {
    thread::spawn(move || {
        let mut buf = [0u8; 4096];
        loop {
            match stream.read(&mut buf) {
                Ok(0) => break,
                Ok(n) => {
                    let chunk = String::from_utf8_lossy(&buf[..n]).to_string();
                    let _ = proxy.send_event(UserEvent::Term(chunk));
                }
                Err(_) => break,
            }
        }
    });
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
