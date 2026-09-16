//! 自动更新：检查 GitHub Releases 上是否有更新的 dsh-desktop 版本，
//! 下载对应平台的安装包并原地替换。
//!
//! 整个流程是非致命的：任何网络/API/安装失败只在交互式终端里打印
//! 一行提示，应用照常继续启动。
//!
//! 版本号来源是 crate 版本（`Cargo.toml`），发布流程会保证它与
//! 发布的 tag（如 `v0.2.0`）保持一致。

use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::Command;

use std::time::{Duration, Instant};

use base64::Engine as _;
use base64::engine::general_purpose::STANDARD;
use futures_util::StreamExt;
use minisign_verify::{PublicKey, Signature};
use tao::event_loop::EventLoopProxy;

use crate::UserEvent;

/// 发布 Release 的 GitHub 仓库。
const GITHUB_REPO: &str = "ht-shaipe/dsh-desktop";
/// Release API 探测的超时时间，避免离线机器被卡住。
const API_TIMEOUT_SECS: &str = "8";
/// 下载失败后的最大重试次数（断点续传）。
const MAX_DL_RETRIES: usize = 3;
/// 单次 chunk 读取超过此时长无数据即视为网络停滞，中止本轮重试。
/// 等价于 curl 的 `--speed-time/--speed-limit`，但粒度更精确（逐块超时）。
const STALL_TIMEOUT: Duration = Duration::from_secs(20);

/// 用于验证更新包签名的 minisign 公钥。
/// 该公钥应内嵌在应用二进制中。
///
/// 生成新密钥对的方法：
///   1. 执行: cargo install rsign2
///   2. 执行: rsign generate -s -p ~/.dsh-desktop-updater.key.pub -S ~/.dsh-desktop-updater.key
///   3. 用公钥内容（去掉第一行）更新此常量
///
/// CI/CD 配置：
///   - 私钥存入 GitHub Secret: DSH_UPDATER_PRIVATE_KEY
///   - 私钥密码存入: DSH_UPDATER_PRIVATE_KEY_PASSWORD
const UPDATER_PUBKEY: &str = "dW50cnVzdGVkIGNvbW1lbnQ6IG1pbmlzaWduIHB1YmxpYyBrZXk6IDk1OEM3NDNBNjI2NkExNTkKUldSWm9XWmlPblNNbGFuL1hBY0k3bm1Gc0pGY005c1hOaVdNdXJIdHpwOHB6K3FCMzB1TWtkQUQK";

/// 我们关心的最新 Release 元数据。
pub struct Release {
    pub version: (u32, u32, u32),
    pub tag: String,
    /// Release 说明正文（markdown 格式）。
    pub body: String,
}

/// 检查是否有新版本可用。离线或已是最新时返回 `None`。
pub fn check_for_update() -> Option<Release> {
    let release = fetch_latest_release()?;
    let current = parse_version(env!("CARGO_PKG_VERSION"))?;
    if release.version > current {
        Some(release)
    } else {
        None
    }
}

/// 执行更新检查 +（如果存在新版本）下载并安装。
/// 所有输出打印到应用内终端。绝不向上返回错误。
pub fn check_and_apply(proxy: &EventLoopProxy<UserEvent>) {
    let _ = proxy.send_event(UserEvent::Term(
        "• 应用本体（dsh-desktop）自动升级：正在检查最新版本…\r\n".into(),
    ));

    match check_for_update() {
        Some(release) => {
            let _ = proxy.send_event(UserEvent::Term(format!(
                "  ↑ 发现新版本 v{}（当前 v{}）\r\n",
                release.tag.trim_start_matches('v'),
                env!("CARGO_PKG_VERSION"),
            )));
            apply_update(&release.tag, proxy);
        }
        None => {
            let _ = proxy.send_event(UserEvent::Term(
                "  ✓ 已是最新版本（或跳过检查）\r\n".into(),
            ));
        }
    }
}

/// 验证已下载文件的签名。
/// 签名有效返回 Ok(())，验证失败返回 Err(错误信息)。
fn verify_signature(file_path: &Path, signature_content: &str) -> Result<(), String> {
    // 读取文件内容
    let file_content = fs::read(file_path)
        .map_err(|e| format!("无法读取文件进行签名验证: {}", e))?;

    // 公钥/签名兼容两种格式：minisign 原生格式（untrusted comment 行 + base64 行）
    // 或整体 base64 编码后的文本。
    let pubkey_str = decode_minisign_or_base64(UPDATER_PUBKEY)?;
    let public_key = PublicKey::decode(&pubkey_str)
        .map_err(|e| format!("无法解析公钥: {}", e))?;

    let sig_str = decode_minisign_or_base64(signature_content)?;
    let signature = Signature::decode(&sig_str)
        .map_err(|e| format!("无法解析签名: {}", e))?;

    // 校验签名（prehash = true 对应 minisign 的默认模式）
    public_key.verify(&file_content, &signature, true)
        .map_err(|e| format!("签名验证失败: {}", e))?;

    Ok(())
}

/// 把 minisign 原生格式文本原样返回；否则视为整体 base64 编码解码后返回。
fn decode_minisign_or_base64(s: &str) -> Result<String, String> {
    let s = s.trim();
    if s.starts_with("untrusted comment:") {
        Ok(s.to_string())
    } else {
        let decoded = STANDARD.decode(s.as_bytes())
            .map_err(|e| format!("base64 解码失败: {}", e))?;
        String::from_utf8(decoded).map_err(|e| format!("解码后非 UTF-8 文本: {}", e))
    }
}

/// 通过 reqwest 请求 `https://api.github.com/repos/<repo>/releases/latest`
/// 并提取 `tag_name` 等字段。JSON 仍用手写极简提取（只取两个扁平字符串
/// 字段，不值得为此引入 serde_json）。
fn fetch_latest_release() -> Option<Release> {
    let url = format!("https://api.github.com/repos/{}/releases/latest", GITHUB_REPO);
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .ok()?;
    let body = rt.block_on(async {
        let client = reqwest::Client::builder()
            // GitHub API 拒绝不带 User-Agent 的请求（403），reqwest 默认不发 UA。
            .user_agent(concat!("dsh-desktop/", env!("CARGO_PKG_VERSION")))
            .timeout(Duration::from_secs(API_TIMEOUT_SECS.parse().unwrap_or(8)))
            .build()
            .ok()?;
        let resp = client
            .get(&url)
            .header("Accept", "application/vnd.github+json")
            .send()
            .await
            .ok()?;
        if !resp.status().is_success() {
            return None;
        }
        resp.text().await.ok()
    })?;
    let tag = json_string_field(&body, "tag_name")?;
    let version = parse_version(tag.trim().trim_start_matches('v'))?;
    let release_body = json_string_field(&body, "body").unwrap_or_default();
    Some(Release { version, tag, body: release_body })
}

/// 极简 JSON 字符串字段提取器：找到 `"key":"value"` 并返回 `value`。
/// 对我们要读的两个扁平字段来说已经够用了。
fn json_string_field(body: &str, key: &str) -> Option<String> {
    let needle = format!("\"{}\"", key);
    let start = body.find(&needle)? + needle.len();
    let rest = &body[start..];
    let colon = rest.find(':')?;
    let rest = rest[colon + 1..].trim_start();
    let rest = rest.strip_prefix('"')?;
    // 找到未被反斜杠转义的收尾引号：值里可能有 \"（转义引号）或
    // \\（转义反斜杠），不能简单取第一个 '"'。
    let bytes = rest.as_bytes();
    let mut end = None;
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'\\' {
            i += 2; // 连同被转义的字符一起跳过
            continue;
        }
        if bytes[i] == b'"' {
            end = Some(i);
            break;
        }
        i += 1;
    }
    let end = end?;
    // 反转义 JSON 转义序列（\n、\t、\uXXXX 等），还原真实文本。
    Some(unescape_json_string(&rest[..end]))
}

/// 还原 JSON 字符串字面量中的转义序列。
/// GitHub API 返回的 release notes 里换行是字面的 `\n`，
/// 不还原的话更新日志会挤成一行并显示反斜杠字符。
fn unescape_json_string(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut chars = s.chars();
    while let Some(c) = chars.next() {
        if c != '\\' {
            out.push(c);
            continue;
        }
        match chars.next() {
            Some('n') => out.push('\n'),
            Some('t') => out.push('\t'),
            Some('r') => out.push('\r'),
            Some('"') => out.push('"'),
            Some('\\') => out.push('\\'),
            Some('/') => out.push('/'),
            Some('b') => out.push('\u{0008}'),
            Some('f') => out.push('\u{000C}'),
            Some('u') => {
                // \uXXXX：取 4 位十六进制拼成字符；格式异常时原样保留。
                let hex: String = chars.by_ref().take(4).collect();
                match u32::from_str_radix(&hex, 16).ok().and_then(char::from_u32) {
                    Some(ch) => out.push(ch),
                    None => {
                        out.push_str("\\u");
                        out.push_str(&hex);
                    }
                }
            }
            // 未知转义：原样保留，宁可多显示也不要吞内容。
            Some(other) => {
                out.push('\\');
                out.push(other);
            }
            None => out.push('\\'),
        }
    }
    out
}

/// 把类 semver 的 `X.Y.Z`（忽略额外后缀）解析成可比较的三元组。
fn parse_version(s: &str) -> Option<(u32, u32, u32)> {
    let s = s.trim().trim_start_matches('v');
    let mut parts = s.split('.');
    let major: u32 = parts.next()?.trim().parse().ok()?;
    let minor: u32 = parts.next().unwrap_or("0").trim().parse().ok()?;
    // patch 段可能带 "-beta.1" 之类的后缀，只取前导数字
    let patch: u32 = parts
        .next()
        .unwrap_or("0")
        .split(|c: char| !c.is_ascii_digit())
        .next()
        .unwrap_or("0")
        .trim()
        .parse()
        .ok()?;
    Some((major, minor, patch))
}

/// 与当前构建平台匹配的 Release 资源文件名。
fn asset_name() -> String {
    if cfg!(target_os = "macos") {
        if cfg!(target_arch = "aarch64") {
            "dsh-desktop-macos-aarch64.dmg".to_string()
        } else {
            "dsh-desktop-macos-x86_64.dmg".to_string()
        }
    } else if cfg!(target_os = "linux") {
        "dsh-desktop-linux-x86_64.tar.gz".to_string()
    } else {
        "dsh-desktop-windows-x86_64.zip".to_string()
    }
}

/// 用 reqwest 流式下载 `url` 到 `pkg`，支持断点续传与停滞检测。
///
/// 每轮：发 `Range: bytes=<offset>-` 续传请求 → `bytes_stream()` 逐块读 →
/// `tokio::time::timeout(STALL_TIMEOUT)` 包裹每个 chunk，超时即判定停滞，
/// 保留已下载部分进入下一轮重试。返回总字节数（用于完成时的 100% 事件）。
async fn download_with_retries(
    url: &str,
    pkg: &Path,
    proxy: &EventLoopProxy<UserEvent>,
) -> Result<u64, String> {
    let client = reqwest::Client::builder()
        .user_agent(concat!("dsh-desktop/", env!("CARGO_PKG_VERSION")))
        .connect_timeout(Duration::from_secs(20))
        .timeout(Duration::from_secs(600))
        .build()
        .map_err(|e| format!("构建客户端失败: {}", e))?;
    // HEAD 探测总大小（跟随重定向；CDN 不支持 HEAD 时回退 0，进度按字节显示）
    let total = match client.head(url).send().await {
        Ok(r) if r.status().is_success() => r.content_length().unwrap_or(0),
        _ => 0,
    };

    let mut last_err = String::new();
    for attempt in 1..=MAX_DL_RETRIES {
        let offset = fs::metadata(pkg).map(|m| m.len()).unwrap_or(0);
        let mut req = client.get(url);
        if offset > 0 {
            req = req.header("Range", format!("bytes={}-", offset));
        }
        let resp = match req.send().await {
            Ok(r) => r,
            Err(e) => {
                last_err = format!("请求失败: {}", e);
                if attempt < MAX_DL_RETRIES {
                    emit_retry(proxy, attempt, offset, &last_err).await;
                    continue;
                }
                return Err(last_err);
            }
        };
        let status = resp.status();
        if !status.is_success() {
            last_err = format!("HTTP {}", status);
            // 4xx 重试无意义
            if (400..500).contains(&status.as_u16()) {
                return Err(last_err);
            }
            if attempt < MAX_DL_RETRIES {
                emit_retry(proxy, attempt, offset, &last_err).await;
                continue;
            }
            return Err(last_err);
        }
        // 206 = 续传（append），200 = 服务器忽略 Range 全量重发（truncate）
        let is_partial = status.as_u16() == 206;
        let remaining = resp.content_length().unwrap_or(0);
        let effective_total = if is_partial { offset + remaining } else { remaining };
        let total = if total > 0 { total } else { effective_total };

        let mut file = if is_partial {
            std::fs::OpenOptions::new()
                .append(true)
                .open(pkg)
                .map_err(|e| format!("打开文件失败: {}", e))?
        } else {
            std::fs::File::create(pkg).map_err(|e| format!("创建文件失败: {}", e))?
        };

        let mut stream = resp.bytes_stream();
        let mut downloaded = offset;
        let mut last_emit = Instant::now();
        let mut last_emit_bytes = downloaded;
        let mut stalled = false;
        let mut read_err: Option<String> = None;
        loop {
            match tokio::time::timeout(STALL_TIMEOUT, stream.next()).await {
                Err(_) => {
                    stalled = true;
                    break;
                }
                Ok(None) => break,
                Ok(Some(Err(e))) => {
                    read_err = Some(format!("读取失败: {}", e));
                    break;
                }
                Ok(Some(Ok(bytes))) => {
                    if let Err(e) = file.write_all(&bytes) {
                        read_err = Some(format!("写入失败: {}", e));
                        break;
                    }
                    downloaded += bytes.len() as u64;
                    let now = Instant::now();
                    if downloaded - last_emit_bytes >= 65536
                        || now - last_emit >= Duration::from_millis(100)
                    {
                        last_emit = now;
                        last_emit_bytes = downloaded;
                        let pct = if total > 0 {
                            ((downloaded as f64 / total as f64) * 100.0).min(99.0) as u8
                        } else {
                            0
                        };
                        let line = if total > 0 {
                            format!(
                                "  升级包下载进度 {:3}% ({:.1}/{:.1} MB)\r",
                                pct,
                                downloaded as f64 / 1e6,
                                total as f64 / 1e6
                            )
                        } else {
                            format!("  已下载 {:.1} MB\r", downloaded as f64 / 1e6)
                        };
                        let _ = proxy.send_event(UserEvent::Term(line));
                        let _ = proxy.send_event(UserEvent::UpdateProgress(pct, downloaded, total));
                    }
                }
            }
        }
        drop(file);

        if stalled {
            last_err = "网络停滞".into();
        } else if let Some(e) = read_err {
            last_err = e;
        } else {
            // 大小校验
            let final_size = fs::metadata(pkg).map(|m| m.len()).unwrap_or(0);
            if total > 0 && final_size != total {
                last_err = format!("大小不匹配 {}/{}", final_size, total);
            } else {
                return Ok(total);
            }
        }
        if attempt < MAX_DL_RETRIES {
            let have = fs::metadata(pkg).map(|m| m.len()).unwrap_or(0);
            emit_retry(proxy, attempt, have, &last_err).await;
        }
    }
    Err(last_err)
}

/// 发送"下载中断，正在重试"的终端提示，并等待 2 秒后重连（避免立即重试
/// 撞上同一个网络抖动）。
async fn emit_retry(proxy: &EventLoopProxy<UserEvent>, attempt: usize, have: u64, reason: &str) {
    let _ = proxy.send_event(UserEvent::Term(format!(
        "  ⚠ 下载中断（{}），已保留 {:.1} MB，正在从断点重试（第 {}/{} 次）…\r\n",
        reason,
        have as f64 / 1e6,
        attempt,
        MAX_DL_RETRIES
    )));
    let _ = proxy.send_event(UserEvent::Term("\r\n".into()));
    tokio::time::sleep(Duration::from_millis(2000)).await;
}

/// 下载 `tag` 对应平台的安装包并安装，进度输出到终端。
/// 失败只打印提示，绝不向上传播。
pub fn apply_update(tag: &str, proxy: &EventLoopProxy<UserEvent>) {
    let name = asset_name();
    let url = format!(
        "https://github.com/{}/releases/download/{}/{}",
        GITHUB_REPO, tag, name
    );
    let sig_url = format!("{}.sig", url);

    let cache = match crate::environment::cache_dir() {
        Ok(c) => c,
        Err(e) => {
            let _ = proxy.send_event(UserEvent::UpdateFailed("无法定位缓存目录".into()));
            let _ = proxy.send_event(UserEvent::Term(format!("  ✗ 升级失败：{}\r\n", e)));
            return;
        }
    };
    if let Err(e) = fs::create_dir_all(&cache) {
        let _ = proxy.send_event(UserEvent::UpdateFailed("无法创建缓存目录".into()));
        let _ = proxy.send_event(UserEvent::Term(format!(
            "  ✗ 升级失败：无法创建缓存目录 {}: {}\r\n",
            cache.display(),
            e
        )));
        return;
    }
    let pkg = cache.join(&name);
    let sig_pkg = cache.join(format!("{}.sig", name));

    // --- 下载（Rust 原生 reqwest，流式 + 停滞检测 + 断点续传重试） ----------
    // 用 bytes_stream() 逐块读取响应体，每块用 tokio::time::timeout 包裹：
    // 超过 STALL_TIMEOUT 无新数据即判定网络停滞，中止本轮并从断点重试
    // （等价于 curl 的 --speed-time/--speed-limit，但粒度精确到单块）。
    // 失败时保留已下载部分，下一轮发 Range: bytes=<offset>- 续传。
    let _ = proxy.send_event(UserEvent::Term(format!("  正在下载 {} …\r\n", name)));
    let dl_result = {
        let rt = match tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
        {
            Ok(rt) => rt,
            Err(e) => {
                let _ = proxy.send_event(UserEvent::UpdateFailed(format!("初始化运行时失败: {}", e)));
                let _ = proxy.send_event(UserEvent::Term(format!("  ✗ 下载失败：{}\r\n", e)));
                return;
            }
        };
        rt.block_on(download_with_retries(&url, &pkg, &proxy))
    };
    match dl_result {
        Ok(total) => {
            let _ = proxy.send_event(UserEvent::UpdateProgress(100, total, total));
            let _ = proxy.send_event(UserEvent::Term("\r\n".into()));
            let _ = proxy.send_event(UserEvent::Term("  ✓ 下载完成，正在验证签名…\r\n".into()));
        }
        Err(msg) => {
            let _ = fs::remove_file(&pkg);
            let _ = proxy.send_event(UserEvent::UpdateFailed("下载失败，已跳过本次升级".into()));
            let _ = proxy.send_event(UserEvent::Term(format!(
                "  ✗ 升级包下载失败（{}）。{}本次跳过升级，不影响使用。\r\n",
                url, msg
            )));
            return;
        }
    }

    // --- 验证签名 ---------------------------------------------------------
    // 签名文件是小文本，直接用 reqwest 拉取整个 body 写入文件。
    let sig_downloaded = {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .ok();
        rt.and_then(|rt| {
            rt.block_on(async {
                let client = reqwest::Client::builder()
                    .user_agent(concat!("dsh-desktop/", env!("CARGO_PKG_VERSION")))
                    .connect_timeout(Duration::from_secs(20))
                    .timeout(Duration::from_secs(30))
                    .build()
                    .ok()?;
                let resp = client.get(&sig_url).send().await.ok()?;
                if !resp.status().is_success() {
                    return None;
                }
                let bytes = resp.bytes().await.ok()?;
                fs::write(&sig_pkg, &bytes).ok()
            })
        })
        .is_some()
    };

    if !sig_downloaded || !sig_pkg.is_file() {
        let _ = fs::remove_file(&pkg);
        let _ = proxy.send_event(UserEvent::UpdateFailed("签名文件下载失败".into()));
        let _ = proxy.send_event(UserEvent::Term(
            "  ✗ 签名文件下载失败，无法验证更新包完整性。\r\n".into(),
        ));
        let _ = proxy.send_event(UserEvent::Term(
            "  本次跳过升级，不影响使用。\r\n".into(),
        ));
        return;
    }

    // 读取签名文件内容
    let sig_content = match fs::read_to_string(&sig_pkg) {
        Ok(c) => c,
        Err(e) => {
            let _ = fs::remove_file(&pkg);
            let _ = fs::remove_file(&sig_pkg);
            let _ = proxy.send_event(UserEvent::UpdateFailed("无法读取签名文件".into()));
            let _ = proxy.send_event(UserEvent::Term(format!(
                "  ✗ 无法读取签名文件: {}\r\n", e
            )));
            return;
        }
    };

    // 校验签名
    match verify_signature(&pkg, &sig_content) {
        Ok(()) => {
            let _ = proxy.send_event(UserEvent::Term(
                "  ✓ 签名验证通过\r\n".into(),
            ));
        }
        Err(e) => {
            // 验签失败：删除已下载的包，拒绝安装
            let _ = fs::remove_file(&pkg);
            let _ = fs::remove_file(&sig_pkg);
            let _ = proxy.send_event(UserEvent::UpdateFailed("签名验证失败".into()));
            let _ = proxy.send_event(UserEvent::Term(format!(
                "  ✗ 签名验证失败: {}\r\n", e
            )));
            let _ = proxy.send_event(UserEvent::Term(
                "  本次跳过升级，不影响使用。\r\n".into(),
            ));
            return;
        }
    }

    let _ = proxy.send_event(UserEvent::Term("  ✓ 正在安装…\r\n".into()));

    // --- 按平台安装 -------------------------------------------------------
    let result = if cfg!(target_os = "macos") {
        install_macos(&pkg, proxy)
    } else if cfg!(target_os = "linux") {
        install_linux(&pkg)
    } else {
        install_windows(&pkg)
    };

    // 清理下载的安装包与签名文件
    let _ = fs::remove_file(&pkg);
    let _ = fs::remove_file(&sig_pkg);

    match result {
        Ok(()) => {
            let ver = tag.trim_start_matches('v');
            let _ = proxy.send_event(UserEvent::Term(format!(
                "✓ 已升级到 v{}，重启应用（关闭窗口后重新打开）即生效。\r\n",
                ver
            )));
            let _ = proxy.send_event(UserEvent::Status(format!(
                "已升级到 v{}，重启应用后生效",
                ver
            )));
            // 驱动"更新完成"UI（完成对话框 / 原生 toast + 重启按钮）。
            let _ = proxy.send_event(UserEvent::UpdateDone(tag.to_string()));
        }
        Err(msg) => {
            let _ = proxy.send_event(UserEvent::UpdateFailed("升级失败，已跳过".into()));
            let _ = proxy.send_event(UserEvent::Term(format!("  ✗ 升级失败：{}\r\n", msg)));
        }
    }
}

// ---------------------------------------------------------------------------
// macOS：把 dmg 以只读方式挂载到临时挂载点，再用 `ditto` 把新的 .app
// 覆盖到正在运行的 bundle 上（Unix 允许替换正在运行的可执行文件）。
// 应用位于 /Applications（用户可写）或 ~/Applications 时无需管理员权限。
// ---------------------------------------------------------------------------
#[cfg(target_os = "macos")]
fn install_macos(dmg: &Path, proxy: &EventLoopProxy<UserEvent>) -> Result<(), String> {
    let bundle = match running_app_bundle() {
        Some(b) => b,
        None => {
            // 通过 `cargo run` / 裸二进制运行：没有可替换的目标。
            return Err(
                "当前不是以 .app 方式运行（开发模式），已跳过自动替换。请手动安装新版 dmg。"
                    .to_string(),
            );
        }
    };

    // 准备临时挂载点
    let mount = std::env::temp_dir().join("dsh-desktop-update-mount");
    let _ = fs::remove_dir_all(&mount);
    fs::create_dir_all(&mount)
        .map_err(|e| format!("无法创建挂载点 {}: {}", mount.display(), e))?;

    let attach = Command::new("hdiutil")
        .args([
            "attach",
            "-nobrowse",
            "-readonly",
            "-mountpoint",
            &mount.to_string_lossy(),
            &dmg.to_string_lossy(),
        ])
        .output()
        .map_err(|e| format!("无法执行 hdiutil: {}", e))?;
    if !attach.status.success() {
        let _ = fs::remove_dir_all(&mount);
        return Err(format!(
            "挂载 dmg 失败: {}",
            String::from_utf8_lossy(&attach.stderr).trim()
        ));
    }

    // 校验 dmg 内确实包含应用
    let new_app = mount.join("dsh-desktop.app");
    if !new_app.is_dir() {
        let _ = Command::new("hdiutil").args(["detach", &mount.to_string_lossy()]).status();
        let _ = fs::remove_dir_all(&mount);
        return Err("dmg 中未找到 dsh-desktop.app，安装包内容异常。".to_string());
    }

    let _ = proxy.send_event(UserEvent::Term(format!(
        "  正在替换应用 {}\r\n",
        bundle.display()
    )));
    // 用 ditto 覆盖复制（保留元数据/权限）
    let new_app_path = new_app.to_string_lossy().to_string();
    let bundle_path = bundle.to_string_lossy().to_string();
    let copy = Command::new("ditto")
        .args([&new_app_path, &bundle_path])
        .status()
        .map(|s| s.success())
        .unwrap_or(false);

    // 刷新 LaunchServices 使（新的）图标/版本生效；仅是锦上添花，失败无妨。
    let lsreg = "/System/Library/Frameworks/CoreServices.framework/Frameworks/LaunchServices.framework/Support/lsregister";
    if Command::new(lsreg).args(["-f", &bundle.to_string_lossy()]).status().is_err() {
        // 尽力而为
    }

    // 卸载 dmg 并清理挂载点
    let _ = Command::new("hdiutil").args(["detach", &mount.to_string_lossy()]).status();
    let _ = fs::remove_dir_all(&mount);

    if copy {
        Ok(())
    } else {
        Err("替换应用文件失败（目录可能没有写权限，例如应用不在 /Applications）。".to_string())
    }
}

/// 从当前可执行文件位置向上逐级查找，找到包含它的 `.app` bundle。
#[cfg(target_os = "macos")]
fn running_app_bundle() -> Option<PathBuf> {
    let exe = std::env::current_exe().ok()?;
    let mut dir = exe.parent()?.to_path_buf();
    loop {
        if dir.extension().and_then(|e| e.to_str()) == Some("app") {
            return Some(dir);
        }
        dir = dir.parent()?.to_path_buf();
    }
}

// ---------------------------------------------------------------------------
// Linux：tar.gz 里包含 `./dsh-desktop`（及 .desktop/图标）；
// 直接解压覆盖到当前正在运行的二进制文件所在目录。
// ---------------------------------------------------------------------------
#[cfg(target_os = "linux")]
fn install_linux(pkg: &Path) -> Result<(), String> {    let exe = std::env::current_exe()
        .map_err(|e| format!("无法定位当前可执行文件: {}", e))?;
    let dir = exe
        .parent()
        .ok_or("无法定位当前可执行文件所在目录")?
        .to_path_buf();
    let ok = Command::new("tar")
        .args(["-xzf", &pkg.to_string_lossy(), "-C", &dir.to_string_lossy()])
        .status()
        .map(|s| s.success())
        .unwrap_or(false);
    if ok {
        // 确保可执行位存在（tar 本身会保留，但显式处理更稳妥）。
        let _ = Command::new("chmod").args(["+x", &exe.to_string_lossy()]).status();
        Ok(())
    } else {
        Err("解压升级包失败。".to_string())
    }
}

// ---------------------------------------------------------------------------
// Windows：运行中的 .exe 无法被覆盖，因此先解压出新 exe，
// 再生成一个小的 .bat 脚本，在本进程退出后立刻完成替换。
// ---------------------------------------------------------------------------
#[cfg(target_os = "windows")]
fn install_windows(pkg: &Path) -> Result<(), String> {
    use std::os::windows::process::CommandExt;

    let exe = std::env::current_exe()
        .map_err(|e| format!("无法定位当前可执行文件: {}", e))?;
    let dir = exe
        .parent()
        .ok_or("无法定位当前可执行文件所在目录")?
        .to_path_buf();

    // 解压到临时目录
    let tmp_dir = dir.join("update-tmp");
    let _ = fs::remove_dir_all(&tmp_dir);
    fs::create_dir_all(&tmp_dir)
        .map_err(|e| format!("无法创建临时目录: {}", e))?;
    let ok = Command::new("powershell")
        .args([
            "-NoProfile",
            "-Command",
            &format!(
                "Expand-Archive -Force -Path '{}' -DestinationPath '{}'",
                pkg.display(),
                tmp_dir.display()
            ),
        ])
        .status()
        .map(|s| s.success())
        .unwrap_or(false);
    if !ok {
        let _ = fs::remove_dir_all(&tmp_dir);
        return Err("解压升级包失败。".to_string());
    }
    let new_exe = tmp_dir.join("dsh-desktop.exe");
    if !new_exe.is_file() {
        let _ = fs::remove_dir_all(&tmp_dir);
        return Err("升级包中未找到 dsh-desktop.exe。".to_string());
    }

    // 生成替换脚本：等待本进程退出 -> 旧 exe 改名 -> 复制新 exe -> 清理
    let bat = dir.join("dsh-update.bat");
    let script = format!(
        "@echo off\r\n:wait\r\ntasklist /FI \"PID eq {pid}\" | find \"{pid}\" >nul\r\nif not errorlevel 1 (\r\n  timeout /t 1 /nul >nul\r\n  goto wait\r\n)\r\nmove /y \"{old}\" \"{old}.bak\"\r\ncopy /y \"{new}\" \"{old}\"\r\ndel \"{old}.bak\"\r\nrmdir /s /q \"{tmp}\"\r\ndel \"%~f0\"\r\n",
        pid = std::process::id(),
        old = exe.display(),
        new = new_exe.display(),
        tmp = tmp_dir.display()
    );
    fs::write(&bat, script).map_err(|e| format!("无法写入升级脚本: {}", e))?;
    Command::new("cmd")
        .args(["/C", &bat.to_string_lossy()])
        .creation_flags(0x00000008) // DETACHED_PROCESS：脱离当前进程独立运行
        .spawn()
        .map_err(|e| format!("无法启动升级脚本: {}", e))?;
    Ok(())
}

// 兜底桩函数：保证 `apply_update` 里的 `cfg!` 分发在每个平台都能编译。
#[cfg(not(target_os = "macos"))]
fn install_macos(_dmg: &Path, _proxy: &EventLoopProxy<UserEvent>) -> Result<(), String> {
    Err("仅支持在 macOS 上安装 dmg 升级包。".to_string())
}
#[cfg(not(target_os = "linux"))]
fn install_linux(_pkg: &Path) -> Result<(), String> {
    Err("仅支持在 Linux 上安装 tar.gz 升级包。".to_string())
}
#[cfg(not(target_os = "windows"))]
fn install_windows(_pkg: &Path) -> Result<(), String> {
    Err("仅支持在 Windows 上安装 zip 升级包。".to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 端到端验证 reqwest 的 Range 续传 + tokio::time::timeout 停滞检测。
    /// 需要本地服务器配合，手动跑：
    ///   bun -e 'const f=new Uint8Array(8*1024*1024); ...'  # 见验证脚本
    ///   cargo test reqwest_range_and_stall -- --ignored --nocapture
    #[test]
    #[ignore]
    fn reqwest_range_and_stall() {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        rt.block_on(async {
            let client = reqwest::Client::builder().build().unwrap();

            // 1) Range 续传：请求 bytes=3000000-，期望 206 + 剩余字节
            let resp = client
                .get("http://127.0.0.1:18925/f")
                .header("Range", "bytes=3000000-")
                .send()
                .await
                .unwrap();
            assert_eq!(
                resp.status().as_u16(),
                206,
                "支持 Range 的服务器应返回 206 Partial Content"
            );
            let bytes = resp.bytes().await.unwrap();
            assert_eq!(bytes.len(), 8388608 - 3000000, "续传返回剩余字节");

            // 2) 停滞检测：慢端点 5s 才发数据，2s timeout 应超时
            let resp = client.get("http://127.0.0.1:18925/slow").send().await.unwrap();
            let mut stream = resp.bytes_stream();
            let r = tokio::time::timeout(Duration::from_secs(2), stream.next()).await;
            assert!(r.is_err(), "超过 timeout 无数据应判定为停滞");
        });
    }

    #[test]
    fn parses_versions() {
        assert_eq!(parse_version("0.1.0"), Some((0, 1, 0)));
        assert_eq!(parse_version("v1.2.3"), Some((1, 2, 3)));
        assert_eq!(parse_version("v2.0"), Some((2, 0, 0)));
        assert_eq!(parse_version("1.2.3-beta.1"), Some((1, 2, 3)));
        assert_eq!(parse_version("x"), None);
    }

    #[test]
    fn extracts_json_fields() {
        let body = r#"{"url":"https:\/\/api.github.com\/x","tag_name":"v0.2.0","name":"v0.2.0"}"#;
        assert_eq!(json_string_field(body, "tag_name").as_deref(), Some("v0.2.0"));
        assert_eq!(json_string_field(body, "missing"), None);
    }

    #[test]
    fn unescapes_release_notes() {
        // GitHub API 返回的 body：换行是 \n，双引号是 \"，反斜杠是 \\。
        // 注意：body 值以 "## 开头（含 "# 与 "## 序列），需要用三个 # 作原始字符串定界。
        let body = r###"{"tag_name":"v0.2.0","body":"## 更新内容\n\n- 修复 \"进度条\" 不动的问题\n- 路径 C:\\Users"}"###;
        let notes = json_string_field(body, "body").unwrap();
        assert_eq!(notes, "## 更新内容\n\n- 修复 \"进度条\" 不动的问题\n- 路径 C:\\Users");
    }

    #[test]
    fn unescapes_unicode_sequences() {
        assert_eq!(unescape_json_string("a\\u4e2db"), "a中b");
        // 格式异常的 \u 原样保留，不 panic。
        assert_eq!(unescape_json_string("\\uZZZZ"), "\\uZZZZ");
        // 行尾孤立的反斜杠不丢字符。
        assert_eq!(unescape_json_string("x\\"), "x\\");
    }

    #[test]
    fn newer_release_wins() {
        let rel = Release { 
            version: (0, 2, 0), 
            tag: "v0.2.0".into(),
            body: "Test release notes".into(),
        };
        let cur = parse_version("0.1.0").unwrap();
        assert!(rel.version > cur);
    }
}
