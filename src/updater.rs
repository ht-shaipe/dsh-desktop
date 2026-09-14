//! 自动更新：检查 GitHub Releases 上是否有更新的 dsh-desktop 版本，
//! 下载对应平台的安装包并原地替换。
//!
//! 整个流程是非致命的：任何网络/API/安装失败只在交互式终端里打印
//! 一行提示，应用照常继续启动。
//!
//! 版本号来源是 crate 版本（`Cargo.toml`），发布流程会保证它与
//! 发布的 tag（如 `v0.2.0`）保持一致。

use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::thread;
use std::time::Duration;

use base64::Engine as _;
use base64::engine::general_purpose::STANDARD;
use minisign_verify::{PublicKey, Signature};
use tao::event_loop::EventLoopProxy;

use crate::UserEvent;
use crate::environment::http_content_length;

/// 发布 Release 的 GitHub 仓库。
const GITHUB_REPO: &str = "ht-shaipe/dsh-desktop";
/// Release API 探测的超时时间，避免离线机器被卡住。
const API_TIMEOUT_SECS: &str = "8";

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
const UPDATER_PUBKEY: &str = "PLACEHOLDER_REPLACE_WITH_ACTUAL_PUBLIC_KEY";

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

    // 解码公钥（base64 -> minisign 格式）
    let pubkey_decoded = STANDARD.decode(UPDATER_PUBKEY.as_bytes())
        .map_err(|e| format!("无法解码公钥: {}", e))?;
    let pubkey_str = String::from_utf8(pubkey_decoded)
        .map_err(|e| format!("公钥格式无效: {}", e))?;

    // 用 decode 方法解析公钥（可解析 minisign 格式）
    let public_key = PublicKey::decode(&pubkey_str)
        .map_err(|e| format!("无法解析公钥: {}", e))?;

    // 解码签名（base64 -> minisign 格式）
    let sig_decoded = STANDARD.decode(signature_content.as_bytes())
        .map_err(|e| format!("无法解码签名: {}", e))?;
    let sig_str = String::from_utf8(sig_decoded)
        .map_err(|e| format!("签名格式无效: {}", e))?;

    // 用 decode 方法解析签名（可解析 minisign 格式）
    let signature = Signature::decode(&sig_str)
        .map_err(|e| format!("无法解析签名: {}", e))?;

    // 校验签名（prehash = true 对应 minisign 的默认模式）
    public_key.verify(&file_content, &signature, true)
        .map_err(|e| format!("签名验证失败: {}", e))?;

    Ok(())
}

/// 通过 curl 请求 `https://api.github.com/repos/<repo>/releases/latest`
/// 并提取 `tag_name` 等字段。我们刻意不引入 HTTP/JSON 依赖：
/// curl 在本应用支持的所有平台上都有预装（见 `environment.rs`），
/// 而我们要取的字段只是一个稳定的简单字符串。
fn fetch_latest_release() -> Option<Release> {
    let url = format!("https://api.github.com/repos/{}/releases/latest", GITHUB_REPO);
    let out = Command::new("curl")
        .args([
            "-fsSL",
            "--max-time",
            API_TIMEOUT_SECS,
            "-H",
            "Accept: application/vnd.github+json",
            &url,
        ])
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    let body = String::from_utf8_lossy(&out.stdout);
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
    let end = rest.find('"')?;
    // 还原 GitHub 可能输出的最小转义集合
    // （tag 名称都是纯文本，但稳妥起见处理一下）。
    Some(rest[..end].replace("\\/", "/"))
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
            let _ = proxy.send_event(UserEvent::Term(format!("  ✗ 升级失败：{}\r\n", e)));
            return;
        }
    };
    if let Err(e) = fs::create_dir_all(&cache) {
        let _ = proxy.send_event(UserEvent::Term(format!(
            "  ✗ 升级失败：无法创建缓存目录 {}: {}\r\n",
            cache.display(),
            e
        )));
        return;
    }
    let pkg = cache.join(&name);
    let sig_pkg = cache.join(format!("{}.sig", name));

    // --- 下载（与 Node 安装器相同的实时进度条样式） -----------------------
    let _ = proxy.send_event(UserEvent::Term(format!(
        "  正在下载 {} …\r\n",
        name
    )));
    let total = http_content_length(&url).unwrap_or(0);
    let mut dl = match Command::new("curl")
        .args(["-fsSL", "--max-time", "600", &url, "-o", &pkg.to_string_lossy()])
        .spawn()
    {
        Ok(c) => c,
        Err(e) => {
            let _ = proxy.send_event(UserEvent::Term(format!("  ✗ 下载失败：{}\r\n", e)));
            return;
        }
    };
    // 轮询文件大小刷新进度行
    let mut last_pct: i32 = -1;
    loop {
        if let Ok(m) = fs::metadata(&pkg) {
            let pct = if total > 0 {
                ((m.len() as f64 / total as f64) * 100.0) as i32
            } else {
                -1
            };
            if pct != last_pct {
                last_pct = pct;
                let line = if pct >= 0 {
                    format!("  升级包下载进度 {:3}%\r", pct.min(99))
                } else {
                    "  升级包下载中…\r".to_string()
                };
                let _ = proxy.send_event(UserEvent::Term(line));
            }
        }
        if dl.try_wait().ok().flatten().is_some() {
            break;
        }
        thread::sleep(Duration::from_millis(200));
    }
    let ok = dl.wait().ok().map(|s| s.success()) == Some(true) && pkg.is_file();
    if !ok {
        let _ = fs::remove_file(&pkg);
        let _ = proxy.send_event(UserEvent::Term(format!(
            "  ✗ 升级包下载失败（{}）。本次跳过升级，不影响使用。\r\n",
            url
        )));
        return;
    }
    let _ = proxy.send_event(UserEvent::Term("\r\n".into()));
    let _ = proxy.send_event(UserEvent::Term("  ✓ 下载完成，正在验证签名…\r\n".into()));

    // --- 验证签名 ---------------------------------------------------------
    let sig_downloaded = Command::new("curl")
        .args(["-fsSL", "--max-time", "30", &sig_url, "-o", &sig_pkg.to_string_lossy()])
        .status()
        .map(|s| s.success())
        .unwrap_or(false);

    if !sig_downloaded || !sig_pkg.is_file() {
        let _ = fs::remove_file(&pkg);
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
        }
        Err(msg) => {
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
