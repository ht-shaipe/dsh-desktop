//! UI 辅助模块：WebView 的 HTML/JS 资源加载与小工具函数。
//!
//! HTML 标记和 JavaScript 以纯文件形式放在 `resources/` 目录下，
//! 便于脱离 Rust 源码独立编辑。编译期通过 `include_str!` 内嵌，
//! 再由 `loading_html()` 拼接成完整页面。

use tao::window::Icon;

// 编译期内嵌的页面模板与脚本
const INDEX_HTML: &str = include_str!(concat!(env!("CARGO_MANIFEST_DIR"), "/resources/index.html"));
const APP_JS: &str = include_str!(concat!(env!("CARGO_MANIFEST_DIR"), "/resources/app.js"));

/// 构造 WebView 的初始文档：把内嵌的 JS 注入到 HTML 模板中。
/// 返回全新 `String`，供 `WebViewBuilder::with_html` 使用。
pub fn loading_html() -> String {
    let version = env!("CARGO_PKG_VERSION");
    let js_version = format!("var dshVersion='{}';", version);
    INDEX_HTML.replace("/*__APP_JS__*/", &(js_version + APP_JS))
}

/// 独立"帮助/关于"窗口的内容。完全自包含（内联样式、无外部依赖），
/// 加载在专属 WebView 里，与主窗口当前页面（启动页 / dsh web 界面）
/// 完全解耦，因此任何阶段都能正常展示。
pub fn help_html() -> String {
    let version = env!("CARGO_PKG_VERSION");
    format!(
        r#"<!DOCTYPE html>
<html lang="zh-CN">
<head>
<meta charset="utf-8">
<style>
  * {{ margin: 0; padding: 0; box-sizing: border-box; }}
  body {{
    font-family: -apple-system, "PingFang SC", "Helvetica Neue", sans-serif;
    background: #f6f7f9; color: #1f2329; padding: 28px 32px;
    line-height: 1.7; font-size: 14px;
  }}
  h1 {{ font-size: 20px; margin-bottom: 4px; }}
  .version {{ color: #86909c; font-size: 12px; margin-bottom: 20px; }}
  h2 {{ font-size: 14px; margin: 0 0 6px; color: #4e5969; }}
  p, li {{ color: #4e5969; }}
  ul {{ padding-left: 18px; }}
  li {{ margin-bottom: 4px; }}
  code {{
    background: #f2f3f5; border-radius: 4px; padding: 1px 5px;
    font-size: 12px; font-family: ui-monospace, Menlo, monospace;
  }}
  .card {{
    background: #fff; border: 1px solid #e5e6eb; border-radius: 10px;
    padding: 16px 20px; margin-top: 16px;
  }}
  kbd {{
    background: #f2f3f5; border: 1px solid #e5e6eb; border-bottom-width: 2px;
    border-radius: 4px; padding: 1px 6px; font-size: 12px; font-family: inherit;
  }}
  a {{ color: #165dff; text-decoration: none; }}
  .footer {{ margin-top: 22px; color: #86909c; font-size: 12px; text-align: center; }}
</style>
</head>
<body>
  <h1>DeepSeek dsh Desktop</h1>
  <div class="version">v{version} · 桌面版启动器</div>

  <div class="card">
    <h2>这是什么？</h2>
    <p>本应用是 <code>dsh</code> 的桌面壳：自动检查/安装 Node.js 运行环境，
    启动 <code>npx @deepseek-ai/dsh web</code> 并在内置页面中打开。
    关闭窗口时会自动停止后台服务。</p>
  </div>

  <div class="card">
    <h2>常见问题</h2>
    <ul>
      <li><b>启动很慢？</b> 首次运行需要下载依赖包，属正常现象，可查看启动时的终端输出了解进度。</li>
      <li><b>提示端口被占用？</b> 上次未正常退出的服务可能仍占用 3080 端口，
          可执行 <code>lsof -iTCP:3080</code> 找到并结束对应进程。</li>
      <li><b>没有 Node.js？</b> 无需手动安装，应用会自动下载便携版 Node 到
          <code>~/.cache/dsh-desktop</code>。</li>
      <li><b>如何更新？</b> 点击标题栏"检查更新"，应用会自动从 GitHub Releases
          下载并安装新版本（带签名校验）。</li>
    </ul>
  </div>

  <div class="card">
    <h2>快捷键</h2>
    <p><kbd>⌘C</kbd> 复制 · <kbd>⌘V</kbd> 粘贴 · <kbd>⌘A</kbd> 全选 ·
       <kbd>⌘Z</kbd> 撤销（在输入框内生效）</p>
  </div>

  <div class="footer">
    项目主页：
    <a href="https://github.com/ht-shaipe/dsh-desktop" target="_blank" onclick="window.open('https://github.com/ht-shaipe/dsh-desktop','_blank');return false;">github.com/ht-shaipe/dsh-desktop</a>
  </div>
</body>
</html>"#,
        version = version
    )
}

/// 构造合法的 JS 字符串*参数*（双引号包裹、JSON 风格转义），
/// 用于嵌入 `evaluate_script` 调用，例如 `appendTerm(<这里>)`。
///
/// 相比模板字符串注入，这种方式对任意终端输出都稳健得多 ——
/// 输出中可能含有反引号、`${`、裸换行、ESC 字节、`\r`/`\b` 等，
/// 全部都能在这里被正确处理。凡是携带用户/命令文本的事件载荷都应使用它。
pub fn js_string_arg(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 2);
    out.push('"');
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            '\u{08}' => out.push_str("\\b"),
            '\u{0c}' => out.push_str("\\f"),
            // 其余控制字符统一转成 \uXXXX 形式
            c if (c as u32) < 0x20 => {
                out.push_str(&format!("\\u{:04x}", c as u32));
            }
            c => out.push(c),
        }
    }
    out.push('"');
    out
}

/// 解码 `icon/logo-480.png` 为 `tao::window::Icon`，用作窗口/标题栏图标。
pub fn load_window_icon() -> Option<Icon> {
    let data = include_bytes!("../icon/logo-480.png");
    let decoder = png::Decoder::new(&data[..]);
    let mut reader = decoder.read_info().ok()?;
    let mut buf = vec![0u8; reader.output_buffer_size()];
    reader.next_frame(&mut buf).ok()?;
    let info = reader.info();
    Icon::from_rgba(buf, info.width, info.height).ok()
}
