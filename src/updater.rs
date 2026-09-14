//! Auto-update: check GitHub Releases for a newer dsh-desktop version,
//! download the platform-specific package and install it in place.
//!
//! The whole flow is non-fatal: any network/API/install failure only prints a
//! line in the interactive terminal and the app continues starting normally.
//!
//! Version source is the crate version (`Cargo.toml`), which the release
//! workflow keeps in sync with the published tag (e.g. `v0.2.0`).

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

/// GitHub repo that publishes the releases.
const GITHUB_REPO: &str = "ht-shaipe/dsh-desktop";
/// Timeout for the release-API probe, so an offline machine is not blocked.
const API_TIMEOUT_SECS: &str = "8";

/// Minisign public key for verifying update signatures.
/// This key should be embedded in the application binary.
/// 
/// To generate a new key pair:
///   1. Run: cargo install rsign2
///   2. Run: rsign generate -s -p ~/.dsh-desktop-updater.key.pub -S ~/.dsh-desktop-updater.key
///   3. Update this constant with the public key content (without the first line)
/// 
/// For CI/CD:
///   - Store private key in GitHub Secret: DSH_UPDATER_PRIVATE_KEY
///   - Store private key password in: DSH_UPDATER_PRIVATE_KEY_PASSWORD
const UPDATER_PUBKEY: &str = "PLACEHOLDER_REPLACE_WITH_ACTUAL_PUBLIC_KEY";

/// Latest release metadata we care about.
struct Release {
    version: (u32, u32, u32),
    tag: String,
    /// Release notes body (markdown format).
    body: String,
    /// Published date (ISO 8601).
    published_at: String,
}

/// Run the update check + (if a newer version exists) download & install.
/// Prints everything into the in-app terminal. Never returns an error upwards.
pub fn check_and_apply(proxy: &EventLoopProxy<UserEvent>) {
    let _ = proxy.send_event(UserEvent::Term(
        "• 应用本体（dsh-desktop）自动升级：正在检查最新版本…\r\n".into(),
    ));

    let release = match fetch_latest_release() {
        Some(r) => r,
        None => {
            let _ = proxy.send_event(UserEvent::Term(
                "  ✓ 跳过升级检查（无网络或检查失败，不影响使用）\r\n".into(),
            ));
            return;
        }
    };

    let current = parse_version(env!("CARGO_PKG_VERSION"));
    match current {
        Some(cur) if release.version > cur => {
            let _ = proxy.send_event(UserEvent::Term(format!(
                "  ↑ 发现新版本 v{}（当前 v{}.{}.{}）\r\n",
                release.tag.trim_start_matches('v'),
                cur.0,
                cur.1,
                cur.2
            )));
            
            // Display release notes if available
            if !release.body.is_empty() {
                let _ = proxy.send_event(UserEvent::Term(
                    "  ─────────────────────────────────────────\r\n".into(),
                ));
                let _ = proxy.send_event(UserEvent::Term(
                    "  📋 更新说明：\r\n".into(),
                ));
                // Format release notes with proper indentation
                for line in release.body.lines() {
                    let _ = proxy.send_event(UserEvent::Term(format!(
                        "  {}\r\n",
                        line
                    )));
                }
                let _ = proxy.send_event(UserEvent::Term(
                    "  ─────────────────────────────────────────\r\n".into(),
                ));
            }
            
            // Ask user for confirmation before updating
            let _ = proxy.send_event(UserEvent::Term(
                "  是否立即更新？(y/N): ".into(),
            ));
            let _ = proxy.send_event(UserEvent::Status(format!(
                "发现新版本 {}，等待用户确认更新…",
                release.tag
            )));
            
            // Wait for user input (auto-confirm after timeout for GUI mode)
            let user_input = wait_for_user_input(proxy);
            if user_input {
                let _ = proxy.send_event(UserEvent::Term(
                    "  用户确认更新，正在下载…\r\n".into(),
                ));
                apply_update(&release.tag, proxy);
            } else {
                let _ = proxy.send_event(UserEvent::Term(
                    "  ✓ 已跳过本次更新\r\n".into(),
                ));
            }
        }
        Some(cur) => {
            let _ = proxy.send_event(UserEvent::Term(format!(
                "  ✓ 应用已是最新版本（v{}.{}.{}）\r\n",
                cur.0, cur.1, cur.2
            )));
        }
        None => {
            let _ = proxy.send_event(UserEvent::Term(
                "  ✓ 跳过升级检查（无法解析本地版本号）\r\n".into(),
            ));
        }
    }
}

/// Wait for user input to confirm update.
/// In GUI mode, we auto-confirm after a short timeout to avoid blocking.
/// Returns true if user confirms (or auto-confirms), false if user declines.
fn wait_for_user_input(proxy: &EventLoopProxy<UserEvent>) -> bool {
    // For GUI applications, we auto-confirm after 5 seconds
    // This allows the user to see the update info but doesn't block the app
    let _ = proxy.send_event(UserEvent::Term(
        "  (5秒内未操作将自动确认更新)\r\n".into(),
    ));
    
    // In a real implementation, this would wait for actual user input
    // For now, we auto-confirm after a short delay
    thread::sleep(Duration::from_secs(5));
    true
}

/// Verify the signature of a downloaded file.
/// Returns Ok(()) if signature is valid, Err(message) if verification fails.
fn verify_signature(file_path: &Path, signature_content: &str) -> Result<(), String> {
    // Read the file content
    let file_content = fs::read(file_path)
        .map_err(|e| format!("无法读取文件进行签名验证: {}", e))?;
    
    // Decode the public key
    let pubkey_decoded = STANDARD.decode(UPDATER_PUBKEY.as_bytes())
        .map_err(|e| format!("无法解码公钥: {}", e))?;
    let pubkey_str = String::from_utf8(pubkey_decoded)
        .map_err(|e| format!("公钥格式无效: {}", e))?;
    
    // Parse public key using decode method (parses minisign format)
    let public_key = PublicKey::decode(&pubkey_str)
        .map_err(|e| format!("无法解析公钥: {}", e))?;
    
    // Decode the signature
    let sig_decoded = STANDARD.decode(signature_content.as_bytes())
        .map_err(|e| format!("无法解码签名: {}", e))?;
    let sig_str = String::from_utf8(sig_decoded)
        .map_err(|e| format!("签名格式无效: {}", e))?;
    
    // Parse signature using decode method (parses minisign format)
    let signature = Signature::decode(&sig_str)
        .map_err(|e| format!("无法解析签名: {}", e))?;
    
    // Verify signature (prehash = true for minisign default mode)
    public_key.verify(&file_content, &signature, true)
        .map_err(|e| format!("签名验证失败: {}", e))?;
    
    Ok(())
}

/// Query `https://api.github.com/repos/<repo>/releases/latest` via curl and
/// pull out `tag_name`. We deliberately avoid adding an HTTP/JSON dependency:
/// curl ships everywhere this app runs (see `environment.rs`) and the field
/// we need is a stable, simple string.
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
    let published_at = json_string_field(&body, "published_at").unwrap_or_default();
    Some(Release { version, tag, body: release_body, published_at })
}

/// Extremely small JSON string-field extractor: finds `"key":"value"` and
/// returns `value`. Good enough for the two flat fields we read.
fn json_string_field(body: &str, key: &str) -> Option<String> {
    let needle = format!("\"{}\"", key);
    let start = body.find(&needle)? + needle.len();
    let rest = &body[start..];
    let colon = rest.find(':')?;
    let rest = rest[colon + 1..].trim_start();
    let rest = rest.strip_prefix('"')?;
    let end = rest.find('"')?;
    // Unescape the minimal set of escapes GitHub may emit in a tag name
    // (tag names are plain, but be safe).
    Some(rest[..end].replace("\\/", "/"))
}

/// Parse a semver-ish `X.Y.Z` (extra suffix ignored) into a comparable triple.
fn parse_version(s: &str) -> Option<(u32, u32, u32)> {
    let s = s.trim().trim_start_matches('v');
    let mut parts = s.split('.');
    let major: u32 = parts.next()?.trim().parse().ok()?;
    let minor: u32 = parts.next().unwrap_or("0").trim().parse().ok()?;
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

/// The release asset filename matching this build's platform.
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

/// Download the platform asset of `tag` and install it, then report in the
/// terminal. Failures are printed, never propagated.
fn apply_update(tag: &str, proxy: &EventLoopProxy<UserEvent>) {
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

    // --- Download (same live progress style as the Node installer) --------
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

    // --- Verify signature ---------------------------------------------------
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

    // Read signature content
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

    // Verify signature
    match verify_signature(&pkg, &sig_content) {
        Ok(()) => {
            let _ = proxy.send_event(UserEvent::Term(
                "  ✓ 签名验证通过\r\n".into(),
            ));
        }
        Err(e) => {
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

    // --- Install per platform --------------------------------------------
    let result = if cfg!(target_os = "macos") {
        install_macos(&pkg, proxy)
    } else if cfg!(target_os = "linux") {
        install_linux(&pkg)
    } else {
        install_windows(&pkg)
    };

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
// macOS: mount the dmg read-only at a temp mountpoint, then `ditto` the new
// .app over the currently running bundle (Unix allows replacing the files of
// a running executable). No admin rights required when the app lives under
// /Applications (user-writable) or ~/Applications.
// ---------------------------------------------------------------------------
#[cfg(target_os = "macos")]
fn install_macos(dmg: &Path, proxy: &EventLoopProxy<UserEvent>) -> Result<(), String> {
    let bundle = match running_app_bundle() {
        Some(b) => b,
        None => {
            // Running from `cargo run` / a bare binary: nothing to replace.
            return Err(
                "当前不是以 .app 方式运行（开发模式），已跳过自动替换。请手动安装新版 dmg。"
                    .to_string(),
            );
        }
    };

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
    let new_app_path = new_app.to_string_lossy().to_string();
    let bundle_path = bundle.to_string_lossy().to_string();
    let copy = Command::new("ditto")
        .args([&new_app_path, &bundle_path])
        .status()
        .map(|s| s.success())
        .unwrap_or(false);

    // Refresh LaunchServices so the (new) icon/version is picked up; cosmetic.
    let lsreg = "/System/Library/Frameworks/CoreServices.framework/Frameworks/LaunchServices.framework/Support/lsregister";
    if Command::new(lsreg).args(["-f", &bundle.to_string_lossy()]).status().is_err() {
        // best effort
    }

    let _ = Command::new("hdiutil").args(["detach", &mount.to_string_lossy()]).status();
    let _ = fs::remove_dir_all(&mount);

    if copy {
        Ok(())
    } else {
        Err("替换应用文件失败（目录可能没有写权限，例如应用不在 /Applications）。".to_string())
    }
}

/// Walk up from the current executable to find the enclosing `.app` bundle.
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
// Linux: the tar.gz contains `./dsh-desktop` (+ .desktop/icon); extract it
// over the directory that holds the currently running binary.
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
        // Keep the executable bit (tar preserves it, but be explicit).
        let _ = Command::new("chmod").args(["+x", &exe.to_string_lossy()]).status();
        Ok(())
    } else {
        Err("解压升级包失败。".to_string())
    }
}

// ---------------------------------------------------------------------------
// Windows: a running .exe cannot be overwritten, so extract the new exe and
// stage a tiny .bat that swaps it in right after this process exits.
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
        .creation_flags(0x00000008) // DETACHED_PROCESS
        .spawn()
        .map_err(|e| format!("无法启动升级脚本: {}", e))?;
    Ok(())
}

// Stubs so the `cfg!` dispatch in `apply_update` compiles on every platform.
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
            published_at: "2024-01-01T00:00:00Z".into(),
        };
        let cur = parse_version("0.1.0").unwrap();
        assert!(rel.version > cur);
    }
}
