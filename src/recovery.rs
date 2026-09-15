//! 启动失败自动恢复：诊断故障插件 → 禁用后重试 → 安全模式降级。
//!
//! dsh 的所有插件都跑在同一个 node 进程里，任何一个插件在 boot 阶段
//! 抛出未捕获异常（或让初始化死等）都会导致整个 `dsh web` 无法就绪。
//! 壳层虽然看不到进程内部，但可以从 PTY 输出的 stack trace / 报错信息
//! 中提取出故障插件，再借助 dsh profile 的补丁层
//! （`~/.dsh/profiles/web/cordis.patch.yml`）把它禁用后重启：
//!
//! 1. 第 1 次启动失败 → 从输出中提取故障插件（模块名 → 补丁行 id），
//!    写入 `disabled: true` 补丁条目后自动重试（最多 2 轮）；
//! 2. 仍失败且无法定位具体插件 → 降级为安全模式：通过 `--patch`
//!    叠加层一次性停用所有第三方（非官方 bundle）插件再试 1 次；
//! 3. 安全模式仍失败 → 放弃并给出完整的诊断报告。
//!
//! 所有自动写入的条目都带 `dsh-desktop auto-recovery` 注释标记，
//! 用户删除对应条目即可恢复插件。

use std::collections::HashSet;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread;
use std::time::Duration;

use tao::event_loop::EventLoopProxy;

use crate::terminal::{self, LaunchOutcome};
use crate::{InputSink, ServerHandle, UserEvent};

/// 崩溃诊断时最多禁用的插件数（一次失败可能牵出多个）。
const MAX_SUSPECTS_PER_ROUND: usize = 3;
/// "定位故障插件并禁用后重试" 的最大轮数。
const MAX_TARGETED_ROUNDS: usize = 2;
/// 每次尝试保留的纯文本输出行数（用于事后诊断）。
pub const TAIL_LINES: usize = 600;

/// web 服务的核心行 id：禁用其中任何一个，`dsh web` 都不可能再监听
/// 端口 —— 对它们做"禁用后重试"注定失败，只会浪费重试轮次。
/// 诊断命中这些行时跳过 targeted 禁用，直接走安全模式/放弃路径。
const CORE_ROW_DENYLIST: &[&str] = &[
    "webserver",
    "web_runtime",
    "web-startup",
    "typert-gateway",
    "cordis-host-runner",
    "cordis-client-runner",
    "connection",
    "modules",
    "resources",
    "client-hmr",
    "api-remotes",
];

/// 一个被自动禁用的插件条目。
#[derive(Clone, Debug)]
pub struct DisableEntry {
    /// 补丁行 id（写入 `cordis.patch.yml` 的目标）。
    pub id: String,
    /// 插件模块名（来自 stack trace / 报错信息）。
    pub module: String,
    /// 简短的失败原因（来自终端输出）。
    pub reason: String,
}

// ---- 路径 ------------------------------------------------------------------

/// dsh 的用户主目录（`DSH_HOME` 环境变量可覆盖，默认 `~/.dsh`）。
fn dsh_home() -> PathBuf {
    if let Ok(h) = std::env::var("DSH_HOME") {
        if !h.is_empty() {
            return PathBuf::from(h);
        }
    }
    PathBuf::from(std::env::var("HOME").unwrap_or_default()).join(".dsh")
}

/// web profile 的目录。
fn profile_dir() -> PathBuf {
    dsh_home().join("profiles").join("web")
}

/// 用户补丁层文件（自动禁用条目的持久化位置）。
fn user_patch_path() -> PathBuf {
    profile_dir().join("cordis.patch.yml")
}

/// 安全模式的一次性叠加层文件（由 `--patch` 参数注入，不持久化）。
fn safe_mode_path() -> PathBuf {
    profile_dir().join("dsh-desktop-safe-mode.yml")
}

/// 解析 profile `package.json` 里 `dsh.profile.bundles` 声明的 bundle 列表。
///
/// 该文件是固定形状的小 JSON，这里做面向 `"bundles": [...]` 的定点扫描；
/// 解析不出任何条目时回退到官方 web profile 的两个已知 bundle。
fn declared_bundles() -> Vec<String> {
    let fallback: Vec<String> = ["@deepseek-ai/dsh-base", "@deepseek-ai/dsh-web-app"]
        .iter()
        .map(|s| s.to_string())
        .collect();
    match fs::read_to_string(profile_dir().join("package.json")) {
        Ok(text) => {
            let out = parse_bundles_json(&text);
            if out.is_empty() {
                fallback
            } else {
                out
            }
        }
        Err(_) => fallback,
    }
}

/// 从 package.json 文本中扫描 `"bundles": [...]` 数组里的字符串列表。
fn parse_bundles_json(text: &str) -> Vec<String> {
    let Some(pos) = text.find("\"bundles\"") else {
        return Vec::new();
    };
    let Some(open) = text[pos..].find('[') else {
        return Vec::new();
    };
    // 扫描数组字面量，收集所有带引号的字符串直到闭括号
    let arr = &text[pos + open..];
    let mut out = Vec::new();
    let mut cur = String::new();
    let mut in_str = false;
    for c in arr.chars() {
        match c {
            '"' => {
                if in_str && !cur.trim().is_empty() {
                    out.push(cur.trim().to_string());
                }
                cur.clear();
                in_str = !in_str;
            }
            ']' if !in_str => break,
            _ if in_str => cur.push(c),
            _ => {}
        }
    }
    out
}

/// 官方 bundle 层文件的所有候选位置。
///
/// bundle 列表来自 profile `package.json` 的 `dsh.profile.bundles`
/// 声明（见 [`declared_bundles`]）。插件包由 pnpm（nodeLinker: hoisted）
/// 安装，可能被提升到 `~/.dsh/profiles/node_modules`，也可能留在
/// profile 自己的 `node_modules` 下 —— 两个位置都探测。
fn bundle_layer_paths() -> Vec<PathBuf> {
    let mut v = Vec::new();
    for root in [
        dsh_home().join("profiles").join("node_modules"),
        profile_dir().join("node_modules"),
    ] {
        for bundle in declared_bundles() {
            v.push(root.join(&bundle).join("cordis.patch.yml"));
        }
    }
    v
}

// ---- YAML 补丁层的轻量解析 ---------------------------------------------------
//
// dsh 的补丁层是"顶层 YAML 数组"的固定形状（`- id: x` + `name: 'y'` +
// `disabled: true` 等扁平键），这里只做面向该形状的行级解析 —— 不引入
// 完整 YAML 依赖，也不会被任意 YAML 混淆（`config:` 块里若出现 `id:`
// 键，因不带 `- ` 前缀而不会误命中）。

/// 从补丁层文本中提取所有 `(id, name)` 对 —— 即"带有模块名的插件行"。
/// 条目必须先出现 `- id:`，随后（下一条 `- ` 开始前）出现 `name:`。
fn scan_id_name_pairs(text: &str) -> Vec<(String, String)> {
    let mut out = Vec::new();
    let mut pending_id: Option<String> = None;
    for raw in text.lines() {
        let t = raw.trim_start();
        let unquote = |s: &str| s.trim().trim_matches('\'').trim_matches('"').to_string();
        if let Some(rest) = t.strip_prefix("- id:") {
            pending_id = Some(unquote(rest));
        } else if let Some(rest) = t.strip_prefix("- name:") {
            if let Some(id) = pending_id.take() {
                out.push((id, unquote(rest)));
            }
        } else if let Some(rest) = t.strip_prefix("name:") {
            if let Some(id) = pending_id.take() {
                out.push((id, unquote(rest)));
            }
        } else if t.starts_with("- ") {
            // 另一个条目开始了：作废未配对的 pending id
            pending_id = None;
        }
    }
    out
}

/// 从补丁层文本中提取所有 `id:` 行出现过的行 id（用于去重判断）。
fn scan_all_ids(text: &str) -> HashSet<String> {
    let mut out = HashSet::new();
    for raw in text.lines() {
        let t = raw.trim_start();
        if let Some(rest) = t.strip_prefix("- id:") {
            let id = rest.trim().trim_matches('\'').trim_matches('"');
            if !id.is_empty() {
                out.insert(id.to_string());
            }
        }
    }
    out
}

/// 加载官方 bundle 层的 `(id, name)` 映射（模块名 → 行 id）。
fn load_bundle_pairs() -> Vec<(String, String)> {
    let mut pairs = Vec::new();
    for p in bundle_layer_paths() {
        if let Ok(text) = fs::read_to_string(&p) {
            pairs.extend(scan_id_name_pairs(&text));
        }
    }
    pairs
}

// ---- 故障插件提取 -------------------------------------------------------------

/// 从一行终端输出中提取可能指向插件的模块名。
///
/// 覆盖三类线索：
/// - `Cannot find module 'X'` / `Cannot find package 'X'`（依赖缺失）；
/// - stack trace 里的 `.../node_modules/<pkg>/...` 路径（谁抛的异常）；
/// - `... load plugin '<x>'` 之类的加载失败信息。
fn extract_modules(line: &str) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    let mut push = |m: String| {
        if !m.is_empty() && !out.contains(&m) {
            out.push(m);
        }
    };

    for pat in ["Cannot find module '", "Cannot find package '"] {
        if let Some(pos) = line.find(pat) {
            let rest = &line[pos + pat.len()..];
            if let Some(end) = rest.find('\'') {
                push(rest[..end].to_string());
            }
        }
    }
    if let Some(pos) = line.find("load plugin '") {
        let rest = &line[pos + "load plugin '".len()..];
        if let Some(end) = rest.find('\'') {
            push(rest[..end].to_string());
        }
    }

    // node_modules/ 路径：提取其中的包名（支持 @scope/name）
    let mut rest = line;
    while let Some(pos) = rest.find("node_modules/") {
        rest = &rest[pos + "node_modules/".len()..];
        let pkg_end = if rest.starts_with('@') {
            // @scope/pkg/... → 第三个 '/' 之前（若无则到行尾）
            let first = rest.find('/');
            match first {
                Some(f) => match rest[f + 1..].find('/') {
                    Some(s) => f + 1 + s,
                    None => rest.len(),
                },
                None => rest.len(),
            }
        } else {
            rest.find('/').unwrap_or(rest.len())
        };
        let pkg = &rest[..pkg_end];
        // 框架自身 / Node 内置模块不是可禁用的插件行，跳过
        let noise = [
            "@deepseek-ai/cordis",
            "@deepseek-ai/cordis-plugin-loader",
            "@deepseek-ai/cosmokit",
            "@deepseek-ai/schemastery",
        ];
        if !pkg.is_empty() && !noise.iter().any(|n| pkg == *n) && !pkg.starts_with("node:") {
            push(pkg.to_string());
        }
        rest = &rest[pkg_end..];
    }
    out
}

/// 模块名的基础包名：`@scope/pkg/sub` → `@scope/pkg`；普通包原样返回。
fn base_package(module: &str) -> &str {
    if module.starts_with('@') {
        // 作用域包：取前两段（scope/pkg）
        let parts: Vec<&str> = module.splitn(3, '/').collect();
        if parts.len() >= 2 {
            // splitn 保证顺序，重新拼接等价于取前两段
            return &module[..parts[0].len() + 1 + parts[1].len()];
        }
        module
    } else {
        module.split('/').next().unwrap_or(module)
    }
}

/// 把模块名解析为补丁行 `(id, name)`。
///
/// 依次尝试：完整名精确匹配 → 基础包名精确匹配 → 行 name 以该模块为前缀
/// （覆盖"模块是基础包、行定义了子路径导出"的情形）。
fn resolve_row(module: &str, pairs: &[(String, String)]) -> Option<(String, String)> {
    for (id, name) in pairs {
        if name == module {
            return Some((id.clone(), name.clone()));
        }
    }
    let base = base_package(module);
    for (id, name) in pairs {
        if name == base {
            return Some((id.clone(), name.clone()));
        }
    }
    let prefix = format!("{}/", module);
    for (id, name) in pairs {
        if name.starts_with(&prefix) {
            return Some((id.clone(), name.clone()));
        }
    }
    None
}

/// 从失败尝试的输出尾巴中诊断故障插件。
///
/// 自后向前扫描（真正的报错通常在输出的最后），收集最多
/// [`MAX_SUSPECTS_PER_ROUND`] 个能映射到补丁行的模块；失败原因取离输出
/// 末尾最近的一条 Error/异常行。
fn diagnose(tail: &[String], bundle_pairs: &[(String, String)]) -> Vec<DisableEntry> {
    let mut user_pairs: Vec<(String, String)> = Vec::new();
    if let Ok(text) = fs::read_to_string(user_patch_path()) {
        user_pairs = scan_id_name_pairs(&text);
    }
    let all_pairs: Vec<(String, String)> = bundle_pairs.iter().chain(user_pairs.iter()).cloned().collect();

    // 最近的一条报错行作为 reason
    let mut reason = String::new();
    for line in tail.iter().rev() {
        let t = line.trim();
        if t.contains("Error") || t.contains("ERROR") || t.contains("error:") {
            reason = t.chars().take(160).collect();
            break;
        }
    }

    let mut suspects: Vec<DisableEntry> = Vec::new();
    let mut seen_ids: HashSet<String> = HashSet::new();
    let mut seen_modules: HashSet<String> = HashSet::new();
    for line in tail.iter().rev() {
        for module in extract_modules(line) {
            if seen_modules.contains(&module) {
                continue;
            }
            seen_modules.insert(module.clone());
            if let Some((id, _)) = resolve_row(&module, &all_pairs) {
                // id 必须是干净 token，避免把解析垃圾写进 YAML
                if !id.is_empty()
                    && !id.contains(char::is_whitespace)
                    && !CORE_ROW_DENYLIST.contains(&id.as_str())
                    && seen_ids.insert(id.clone())
                {
                    let r = if reason.is_empty() {
                        format!("failed to load {}", module)
                    } else {
                        reason.clone()
                    };
                    suspects.push(DisableEntry { id, module, reason: r });
                    if suspects.len() >= MAX_SUSPECTS_PER_ROUND {
                        return suspects;
                    }
                }
            }
        }
    }
    suspects
}

// ---- 补丁写入 -----------------------------------------------------------------

/// 把 `reason` 压成适合放进 YAML 注释的单行文本。
fn comment_safe(reason: &str) -> String {
    reason
        .chars()
        .map(|c| if c == '\n' || c == '\r' || c == '\t' { ' ' } else { c })
        .take(120)
        .collect()
}

/// 向用户的 `cordis.patch.yml` 追加 `disabled: true` 条目。
///
/// - 文件为空或为 `[]`：重写为带说明头的条目列表；
/// - 文件是行内数组（`[` 开头）：无法安全追加 —— 先备份到
///   `cordis.patch.yml.bak-<ts>` 再重写；
/// - 其余（顶层块序列）：直接在末尾追加新条目。
///
/// 返回实际新增的条目 id（已存在的 id 会被跳过）。
fn append_disable_entries(path: &Path, entries: &[DisableEntry]) -> Result<Vec<String>, String> {
    let content = fs::read_to_string(path).unwrap_or_default();
    let existing = scan_all_ids(&content);
    let fresh: Vec<&DisableEntry> = entries
        .iter()
        .filter(|e| !existing.contains(&e.id) && !e.id.is_empty() && !e.id.contains(char::is_whitespace))
        .collect();
    if fresh.is_empty() {
        return Ok(Vec::new());
    }

    let ts = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);

    let mut out = String::new();
    // 去掉注释行和空行后判断文件的实际形状：
    // 空 / `[]` → 重写；行内数组 `[...]` → 备份后重写；块序列 → 追加。
    let significant: String = content
        .lines()
        .map(|l| l.trim())
        .filter(|l| !l.is_empty() && !l.starts_with('#'))
        .collect::<Vec<_>>()
        .join("\n");
    if significant.is_empty() || significant == "[]" {
        out.push_str(
            "# Your patch layer for this dsh profile, applied after every bundle layer:\n\
             # a top-level YAML array of loader patch entries (id-targeted config\n\
             # overrides, disables, and insert lists; `!!js` expressions allowed).\n",
        );
    } else if significant.starts_with('[') {
        // 行内数组：备份后重写（dsh 只认顶层数组，这样最稳妥）
        let bak = path.with_extension(format!("yml.bak-{}", ts));
        fs::copy(path, &bak).map_err(|e| format!("备份 {} 失败: {}", path.display(), e))?;
        out.push_str(&format!(
            "# dsh-desktop: 原行内补丁列表无法安全追加，已备份到 {}\n",
            bak.display()
        ));
    } else {
        out.push_str(&content);
        if !out.ends_with('\n') {
            out.push('\n');
        }
    }

    let mut added = Vec::new();
    for e in fresh {
        out.push_str(&format!(
            "\n# dsh-desktop auto-recovery ({}): 该插件导致启动失败 —— {}\n\
             - id: {}\n  disabled: true\n",
            ts,
            comment_safe(&e.reason),
            e.id
        ));
        added.push(e.id.clone());
    }

    if let Some(dir) = path.parent() {
        let _ = fs::create_dir_all(dir);
    }
    fs::write(path, out).map_err(|e| format!("写入 {} 失败: {}", path.display(), e))?;
    Ok(added)
}

/// 生成安全模式的一次性叠加层：停用所有第三方（非官方 bundle）插件行。
///
/// 第三方插件的识别依据：用户补丁层中带 `name:` 的插入行，且其模块名
/// 不在官方 bundle 层里、也不是 `@deepseek-ai/` 官方包。纯配置覆盖行
/// （无 `name:`）不会被触碰。
fn write_safe_mode_overlay(path: &Path, bundle_pairs: &[(String, String)]) -> Result<Vec<String>, String> {
    let user_text = fs::read_to_string(user_patch_path()).unwrap_or_default();
    let bundle_names: HashSet<String> = bundle_pairs.iter().map(|(_, n)| n.clone()).collect();

    let mut ids = Vec::new();
    for (id, name) in scan_id_name_pairs(&user_text) {
        let base = base_package(&name).to_string();
        if !name.starts_with("@deepseek-ai/") && !bundle_names.contains(&name) && !bundle_names.contains(&base) {
            if !id.is_empty() && !id.contains(char::is_whitespace) {
                ids.push(id);
            }
        }
    }
    if ids.is_empty() {
        return Ok(ids);
    }

    let mut out = String::from(
        "# dsh-desktop safe-mode overlay: 本文件由桌面壳在安全模式下通过\n\
         # `dsh web --patch <此文件>` 一次性注入，用于停用所有第三方插件；\n\
         # 正常启动不会加载它，可随时删除。\n",
    );
    for id in &ids {
        out.push_str(&format!("- id: {}\n  disabled: true\n", id));
    }
    if let Some(dir) = path.parent() {
        let _ = fs::create_dir_all(dir);
    }
    fs::write(path, out).map_err(|e| format!("写入 {} 失败: {}", path.display(), e))?;
    Ok(ids)
}

// ---- JSON 构造（诊断 UI 载荷） -------------------------------------------------

fn json_escape(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 2);
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if (c as u32) < 0x20 => out.push_str(&format!("\\u{:04x}", c as u32)),
            c => out.push(c),
        }
    }
    out
}

fn diag_json(action: &str, attempt: usize, entries: &[DisableEntry], note: &str) -> String {
    let items: Vec<String> = entries
        .iter()
        .map(|e| {
            format!(
                "{{\"id\":\"{}\",\"module\":\"{}\",\"reason\":\"{}\"}}",
                json_escape(&e.id),
                json_escape(&e.module),
                json_escape(&e.reason)
            )
        })
        .collect();
    format!(
        "{{\"action\":\"{}\",\"attempt\":{},\"plugins\":[{}],\"note\":\"{}\"}}",
        json_escape(action),
        attempt,
        items.join(","),
        json_escape(note)
    )
}

// ---- 恢复主循环 ----------------------------------------------------------------

/// 带自动恢复的启动入口：失败时诊断 → 禁用故障插件重试 → 安全模式 → 放弃。
pub fn launch_with_recovery(
    npx: PathBuf,
    proxy: EventLoopProxy<UserEvent>,
    handle: Arc<Mutex<Option<ServerHandle>>>,
    input_writer: InputSink,
    user_took_over: Arc<AtomicBool>,
) {
    let bundle_pairs = load_bundle_pairs();
    let mut disabled_by_us: Vec<DisableEntry> = Vec::new();
    let mut targeted_rounds = 0usize;
    let mut safe_mode = false;
    let mut attempt = 0usize;

    loop {
        attempt += 1;
        let tail = Arc::new(Mutex::new(Vec::<String>::new()));
        let extra: Vec<String> = if safe_mode {
            vec!["--patch".to_string(), safe_mode_path().to_string_lossy().into_owned()]
        } else {
            Vec::new()
        };

        let outcome = terminal::launch_terminal(
            npx.clone(),
            proxy.clone(),
            handle.clone(),
            input_writer.clone(),
            user_took_over.clone(),
            Arc::new(AtomicBool::new(false)),
            tail.clone(),
            &extra,
        );

        match outcome {
            LaunchOutcome::Ready => {
                if !disabled_by_us.is_empty() || safe_mode {
                    let payload = diag_json(
                        "recovered",
                        attempt,
                        &disabled_by_us,
                        "应用已恢复启动。被禁用的插件列表如下，恢复方法见横幅底部说明。",
                    );
                    let _ = proxy.send_event(UserEvent::Diagnosis(payload));
                    // 页面跳转到 dsh web 后再弹一个原生 DOM toast 提醒用户
                    let tp = proxy.clone();
                    let names: Vec<String> = disabled_by_us.iter().map(|e| e.id.clone()).collect();
                    thread::spawn(move || {
                        thread::sleep(Duration::from_secs(5));
                        let msg = if names.is_empty() {
                            "dsh 已在安全模式下启动（第三方插件已停用）".to_string()
                        } else {
                            format!(
                                "已自动禁用故障插件 {}，恢复方法见 ~/.dsh/profiles/web/cordis.patch.yml",
                                names.join(", ")
                            )
                        };
                        let _ = tp.send_event(UserEvent::RecoveredToast(msg));
                    });
                }
                return;
            }
            LaunchOutcome::Abort => return,
            LaunchOutcome::Failed { crashed } => {
                // 超时（而非崩溃）且用户已接管终端时，进程可能仍在被用户
                // 交互使用 —— 不杀进程，交还给用户手动处理。
                if !crashed && user_took_over.load(Ordering::SeqCst) {
                    let _ = proxy.send_event(UserEvent::Fatal(
                        "命令长时间未就绪，但看起来你正在终端中操作，已停止自动修复以免打断。\
                         处理完毕后可重启应用重试。"
                            .into(),
                    ));
                    return;
                }

                // 超时的进程可能还挂着（占资源/端口）：重试前先杀掉，
                // 并给旧 PTY 的读取线程一点时间收尾（EOF、收尾日志），
                // 避免旧会话的"[进程已退出]"混进新一轮终端输出。
                if !crashed {
                    if let Some(mut h) = handle.lock().unwrap().take() {
                        h.kill();
                    }
                    thread::sleep(Duration::from_millis(800));
                }

                let tail_snapshot: Vec<String> = tail.lock().unwrap().clone();
                let suspects = diagnose(&tail_snapshot, &bundle_pairs);
                let new_suspects: Vec<DisableEntry> = suspects
                    .into_iter()
                    .filter(|s| !disabled_by_us.iter().any(|d| d.id == s.id))
                    .collect();

                let _ = proxy.send_event(UserEvent::Term(format!(
                    "\r\n\r\n[dsh-desktop] ─── 第 {} 次启动失败（{}）───\r\n",
                    attempt,
                    if crashed { "进程异常退出" } else { "超时未就绪" }
                )));

                // 策略 1：定位到新的故障插件 → 禁用后重试
                if !new_suspects.is_empty() && targeted_rounds < MAX_TARGETED_ROUNDS {
                    targeted_rounds += 1;
                    match append_disable_entries(&user_patch_path(), &new_suspects) {
                        Ok(added) if !added.is_empty() => {
                            let actually: Vec<DisableEntry> = new_suspects
                                .iter()
                                .filter(|e| added.contains(&e.id))
                                .cloned()
                                .collect();
                            disabled_by_us.extend(actually.iter().cloned());
                            for e in &actually {
                                let _ = proxy.send_event(UserEvent::Term(format!(
                                    "[dsh-desktop] 已禁用故障插件 {}（{}），正在重试…\r\n",
                                    e.id, e.module
                                )));
                            }
                            let _ = proxy.send_event(UserEvent::Diagnosis(diag_json(
                                "retry-disabled",
                                attempt,
                                &actually,
                                "已自动禁用上述插件并重新启动，请稍候…",
                            )));
                            continue;
                        }
                        Ok(_) => {}
                        Err(e) => {
                            terminal::dbg_log(&format!("RECOVERY_DISABLE_ERR: {}", e));
                        }
                    }
                }

                // 策略 2：安全模式（一次性停用所有第三方插件）
                if !safe_mode {
                    match write_safe_mode_overlay(&safe_mode_path(), &bundle_pairs) {
                        Ok(ids) if !ids.is_empty() => {
                            safe_mode = true;
                            let entries: Vec<DisableEntry> = ids
                                .iter()
                                .map(|id| DisableEntry {
                                    id: id.clone(),
                                    module: String::new(),
                                    reason: "安全模式：第三方插件".into(),
                                })
                                .collect();
                            let _ = proxy.send_event(UserEvent::Term(format!(
                                "[dsh-desktop] 进入安全模式：已停用 {} 个第三方插件，正在重试…\r\n",
                                ids.len()
                            )));
                            let _ = proxy.send_event(UserEvent::Diagnosis(diag_json(
                                "safe-mode",
                                attempt,
                                &entries,
                                "未能定位具体故障插件，已停用所有第三方插件（安全模式）并重试。",
                            )));
                            continue;
                        }
                        Ok(_) => {
                            terminal::dbg_log("RECOVERY_SAFE_MODE_EMPTY: no third-party rows");
                        }
                        Err(e) => {
                            terminal::dbg_log(&format!("RECOVERY_SAFE_MODE_ERR: {}", e));
                        }
                    }
                }

                // 策略 3：放弃，给出诊断报告
                let hint = if disabled_by_us.is_empty() && !safe_mode {
                    "多次启动失败且未能定位故障插件。请查看上方终端输出，或删除 ~/.dsh/profiles/web 后重启应用重建 profile。".to_string()
                } else {
                    format!(
                        "自动修复未能恢复启动。已禁用的插件：{}。\
                     可编辑 ~/.dsh/profiles/web/cordis.patch.yml（删除标记为 dsh-desktop auto-recovery 的条目）恢复插件；\
                     也可以删除 ~/.dsh/profiles/web 后重启应用重建 profile。",
                        disabled_by_us.iter().map(|e| e.id.as_str()).collect::<Vec<_>>().join(", ")
                    )
                };
                let _ = proxy.send_event(UserEvent::Diagnosis(diag_json(
                    "failed",
                    attempt,
                    &disabled_by_us,
                    &hint,
                )));
                let _ = proxy.send_event(UserEvent::Fatal(hint));
                return;
            }
        }
    }
}

// ---- 单元测试 -------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    const BUNDLE_YML: &str = r#"
- insert:
    - id: timer
      name: '@deepseek-ai/cordis-plugin-timer'
    - id: llm
      name: '@deepseek-ai/dsh-llm'
    - id: session-title-llm
      name: '@deepseek-ai/dsh-session-title-first-prompt-llm'
    - id: tool-subagent
      name: '@deepseek-ai/dsh-tool-subagent'
    - id: subagent-model-selection-settings
      name: '@deepseek-ai/dsh-tool-subagent/model-selection-settings'
    - id: webserver
      name: '@deepseek-ai/dsh-host-webserver'
"#;

    #[test]
    fn scans_bundle_pairs() {
        let pairs = scan_id_name_pairs(BUNDLE_YML);
        assert!(pairs.contains(&("llm".to_string(), "@deepseek-ai/dsh-llm".to_string())));
        assert!(pairs.contains(&(
            "subagent-model-selection-settings".to_string(),
            "@deepseek-ai/dsh-tool-subagent/model-selection-settings".to_string()
        )));
        assert_eq!(pairs.len(), 6);
    }

    #[test]
    fn ignores_config_keys_named_id() {
        let text = "- id: a\n  name: 'x'\n  config:\n    id: not-an-entry\n";
        let pairs = scan_id_name_pairs(text);
        assert_eq!(pairs, vec![("a".to_string(), "x".to_string())]);
    }

    #[test]
    fn extracts_module_from_cannot_find() {
        let m = extract_modules("Error: Cannot find module '@deepseek-ai/dsh-llm'");
        assert_eq!(m, vec!["@deepseek-ai/dsh-llm".to_string()]);
    }

    #[test]
    fn extracts_module_from_stack_frame() {
        let m = extract_modules(
            "    at load (/home/u/.dsh/profiles/node_modules/@deepseek-ai/dsh-session-title-first-prompt-llm/lib/index.js:10:15)",
        );
        assert_eq!(m, vec!["@deepseek-ai/dsh-session-title-first-prompt-llm".to_string()]);
    }

    #[test]
    fn skips_framework_noise() {
        let m = extract_modules(
            "    at Loader.load (/x/node_modules/@deepseek-ai/cordis-plugin-loader/lib/index.js:1:1)",
        );
        assert!(m.is_empty());
    }

    #[test]
    fn resolves_exact_and_subpath() {
        let pairs = scan_id_name_pairs(BUNDLE_YML);
        // 精确
        assert_eq!(resolve_row("@deepseek-ai/dsh-llm", &pairs).unwrap().0, "llm");
        // 基础包名（行定义了子路径导出）
        assert_eq!(
            resolve_row("@deepseek-ai/dsh-tool-subagent/model-selection-settings", &pairs).unwrap().0,
            "subagent-model-selection-settings"
        );
        // 模块是基础包、行带子路径
        assert_eq!(
            resolve_row("@deepseek-ai/dsh-tool-subagent", &pairs).unwrap().0,
            "tool-subagent"
        );
        // 未知模块
        assert!(resolve_row("some-random-pkg", &pairs).is_none());
    }

    fn tmpfile(name: &str) -> PathBuf {
        let mut p = std::env::temp_dir();
        p.push(format!("dsh-desktop-test-{}-{}", std::process::id(), name));
        let _ = std::fs::remove_file(&p);
        p
    }

    fn entry(id: &str) -> DisableEntry {
        DisableEntry {
            id: id.into(),
            module: format!("pkg-{}", id),
            reason: "boom".into(),
        }
    }

    #[test]
    fn appends_to_empty_patch() {
        let p = tmpfile("empty.yml");
        let added = append_disable_entries(&p, &[entry("llm")]).unwrap();
        assert_eq!(added, vec!["llm"]);
        let text = std::fs::read_to_string(&p).unwrap();
        assert!(text.contains("- id: llm\n  disabled: true"));
        assert!(text.contains("dsh-desktop auto-recovery"));
        // 再写同 id：去重
        let added2 = append_disable_entries(&p, &[entry("llm")]).unwrap();
        assert!(added2.is_empty());
        let _ = std::fs::remove_file(&p);
    }

    #[test]
    fn replaces_empty_array_patch() {
        let p = tmpfile("arr.yml");
        std::fs::write(&p, "# comment\n[]\n").unwrap();
        let added = append_disable_entries(&p, &[entry("webserver")]).unwrap();
        assert_eq!(added, vec!["webserver"]);
        let text = std::fs::read_to_string(&p).unwrap();
        assert!(text.contains("- id: webserver"));
        assert!(!text.contains("[]"));
        let _ = std::fs::remove_file(&p);
    }

    #[test]
    fn appends_after_user_entries() {
        let p = tmpfile("user.yml");
        std::fs::write(
            &p,
            "# user layer\n- id: my-plugin\n  name: 'third-party-pkg'\n  config:\n    foo: 1\n",
        )
        .unwrap();
        let added = append_disable_entries(&p, &[entry("llm")]).unwrap();
        assert_eq!(added, vec!["llm"]);
        let text = std::fs::read_to_string(&p).unwrap();
        // 用户条目保留，新条目追加在末尾
        let user_pos = text.find("- id: my-plugin").unwrap();
        let new_pos = text.find("- id: llm").unwrap();
        assert!(user_pos < new_pos);
        let _ = std::fs::remove_file(&p);
    }

    #[test]
    fn backs_up_inline_array() {
        let p = tmpfile("inline.yml");
        std::fs::write(&p, "[{id: a, disabled: true}]\n").unwrap();
        let added = append_disable_entries(&p, &[entry("llm")]).unwrap();
        assert_eq!(added, vec!["llm"]);
        let text = std::fs::read_to_string(&p).unwrap();
        assert!(text.contains(".bak-"));
        assert!(text.contains("- id: llm"));
        // 备份文件存在且保留原内容
        let bak = {
            let mut b = None;
            for f in std::fs::read_dir(p.parent().unwrap()).unwrap().flatten() {
                let name = f.file_name().to_string_lossy().into_owned();
                if name.contains("inline.yml.bak-") {
                    b = Some(f.path());
                }
            }
            b.unwrap()
        };
        assert_eq!(std::fs::read_to_string(&bak).unwrap(), "[{id: a, disabled: true}]\n");
        let _ = std::fs::remove_file(&p);
        let _ = std::fs::remove_file(&bak);
    }

    #[test]
    fn safe_mode_only_touches_third_party() {
        let user = tmpfile("userpatch.yml");
        std::fs::write(
            &user,
            "- id: my-plugin\n  name: 'cool-plugin'\n- id: override-only\n  config:\n    x: 1\n",
        )
        .unwrap();

        // 与 write_safe_mode_overlay 相同的过滤逻辑（该函数读取真实
        // HOME 下的文件，这里用临时文件验证过滤规则本身）
        let bundle_pairs = scan_id_name_pairs(BUNDLE_YML);
        let user_text = std::fs::read_to_string(&user).unwrap();
        let bundle_names: HashSet<String> = bundle_pairs.iter().map(|(_, n)| n.clone()).collect();
        let ids: Vec<String> = scan_id_name_pairs(&user_text)
            .into_iter()
            .filter(|(id, name)| {
                let base = base_package(name).to_string();
                !name.starts_with("@deepseek-ai/")
                    && !bundle_names.contains(name)
                    && !bundle_names.contains(&base)
                    && !id.is_empty()
            })
            .map(|(id, _)| id)
            .collect();
        // 只有带 name 的第三方插入行（my-plugin）被停用；
        // 纯配置覆盖行（override-only）不受影响
        assert_eq!(ids, vec!["my-plugin"]);
        let _ = std::fs::remove_file(&user);
    }

    #[test]
    fn diag_json_is_valid_shape() {
        let e = DisableEntry {
            id: "llm".into(),
            module: "@deepseek-ai/dsh-llm".into(),
            reason: "Error: \"boom\"\nsecond line".into(),
        };
        let j = diag_json("retry-disabled", 2, &[e], "note");
        assert!(j.starts_with("{\"action\":\"retry-disabled\",\"attempt\":2"));
        assert!(j.contains("\\\"boom\\\""));
        assert!(j.contains("\\n"));
    }

    #[test]
    fn parses_real_profile_package_json() {
        // 真实 web profile 的 package.json 片段
        let text = r#"{
  "name": "dsh-profile-web",
  "private": true,
  "dependencies": {},
  "dsh": {
    "profile": {
      "bundles": [
        "@deepseek-ai/dsh-base",
        "@deepseek-ai/dsh-web-app"
      ],
      "patchReload": "live"
    }
  }
}"#;
        assert_eq!(
            parse_bundles_json(text),
            vec!["@deepseek-ai/dsh-base".to_string(), "@deepseek-ai/dsh-web-app".to_string()]
        );
        // 没有 bundles 字段 / 数组为空
        assert!(parse_bundles_json("{\"name\": \"x\"}").is_empty());
        assert!(parse_bundles_json("\"bundles\": []").is_empty());
        // 后续其他数组不会被误扫（遇到闭括号即停）
        assert_eq!(
            parse_bundles_json("\"bundles\": [\"a\"], \"other\": [\"b\", \"c\"]"),
            vec!["a".to_string()]
        );
    }

    #[test]
    fn base_package_handles_paths() {
        assert_eq!(base_package("@deepseek-ai/dsh-web-app/startup"), "@deepseek-ai/dsh-web-app");
        assert_eq!(base_package("@deepseek-ai/dsh-llm"), "@deepseek-ai/dsh-llm");
        assert_eq!(base_package("plain-pkg/sub"), "plain-pkg");
        assert_eq!(base_package("plain-pkg"), "plain-pkg");
        // 病态输入：scope 里包含与 pkg 相同的子串也不会切错
        assert_eq!(base_package("@ab/a/ab"), "@ab/a");
    }

    #[test]
    fn diagnose_skips_core_rows() {
        // diagnose() 读取真实 HOME 下的文件，这里只验证 deny-list 常量
        // 与 resolve_row 的组合行为：核心行不应成为可禁用对象。
        let pairs = scan_id_name_pairs(BUNDLE_YML);
        let (id, _) = resolve_row("@deepseek-ai/dsh-host-webserver", &pairs).unwrap();
        assert!(CORE_ROW_DENYLIST.contains(&id.as_str()));
    }
}
