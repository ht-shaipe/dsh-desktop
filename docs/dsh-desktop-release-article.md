# 把命令行装进窗口：我用一个周末做了个 dsh 桌面壳，并搞定了跨平台自动发版

> 当你只想用 DeepSeek 的 `dsh` 跑个本地 Web，却要先会和终端、Node 版本、端口残留斗智斗勇时——一个双击即用的桌面壳，就是最好的用户体验。

## 一、缘起：一个命令，一堆麻烦

DeepSeek 推出了一款命令行工具 `@deepseek-ai/dsh`。它的核心玩法之一是：

```bash
npx -y @deepseek-ai/dsh web
```

执行后，它会在本机拉起一个 Web 服务，地址是 `http://127.0.0.1:3080`，你用浏览器打开就能用。

听起来很优雅。但真正想把它交给"不懂命令行"的人用时，麻烦来了：

1. **环境门槛**：`dsh` 运行时其实需要 **Node ≥ v22.15**——它用到了 `node:zlib.createZstdDecompress`、`Promise.withResolvers`、`node:module.stripTypeScriptTypes` 这些很新的 API。机器上只要是 v20 的老 Node，跑到一半就崩，报一堆 `does not provide an export named ...`。
2. **交互卡死**：`npx` 首次安装会弹确认，而 GUI 环境下 stdin 不是 TTY，程序会默默卡住等输入。
3. **端口残留**：上次没关干净，3080 还被占着，再启动就 `EADDRINUSE`。
4. **退出不干净**：窗口关了，后台子进程还在跑。

于是我花了点时间，用 Rust 写了一个**桌面壳 `dsh-desktop`**：双击 App，自动拉起 `dsh web`，窗口里内嵌浏览器打开 3080；关窗即停；环境不对自动补齐。代码已开源并发布了跨平台版本。

项目地址：https://github.com/ht-shaipe/dsh-desktop

---

## 二、技术选型：为什么不直接上 Tauri

Tauri 当然能打包 Web 应用，但它更偏向"我有一个前端项目，帮我套个壳"。而我的诉求是：

- 启动的是一个**外部子进程命令**（`npx ...`），不是我自己的前端；
- 要能在窗口里**实时显示子进程的输出与进度**，还要像真终端一样有颜色、进度条、光标移动；
- 体积要小、依赖要少、跨平台要稳。

所以我选了和 Tauri 同源、但更底层的两件套：

- **`tao` 0.30**：窗口管理、事件循环（Tauri 的窗口层就是它）。
- **`wry` 0.49**：系统原生 WebView 封装（macOS 用 WKWebView、Windows 用 WebView2、Linux 用 webkit2gtk）。

再加上 Unix 侧的 **`libc::forkpty`** 做真正的伪终端（PTY），一套轻量但"像那么回事"的桌面壳就成型了。

代码按职责拆成四个模块，方便维护：

```
src/
├── main.rs        # 入口：窗口/webview、事件循环、进程句柄
├── environment.rs # 环境自检 + 便携 Node 下载
├── terminal.rs    # PTY/管道启动子进程 + 输出读取
└── ui.rs          # HTML/JS 资源 + 字符串转义工具
```

HTML / JS / 图标全部用 `include_str!` / `include_bytes!` **编译期内嵌**，产出的二进制完全自包含，各平台只需分发二进制本身。

---

## 三、核心实现拆解

### 1. 启动命令 + 内嵌 WebView

窗口创建后，后台线程通过 PTY 启动 `npx -y @deepseek-ai/dsh web`，并轮询 `127.0.0.1:3080` 探测就绪。一旦就绪，就通过事件把 WebView 导航到 `http://127.0.0.1:3080`，用户看到的就是 `dsh` 自己的 Web 界面。

`npx` 的 `-y` 参数在这里至关重要——它 auto-confirm 了首次安装，避免 GUI 下因等待 stdin 输入而卡死。

### 2. 真·ANSI 终端模拟器（最有意思的部分）

如果只在窗口里 `append` 纯文本，用户看不到 `dsh` 下载依赖、加载插件的彩色进度。所以我没用简单的 `<pre>`，而是**自己实现了一个 ANSI 终端模拟器**：

- 维护一个"行 × 单元格"的网格，每个单元格带前景/背景色与样式；
- 解析 **SGR**（颜色/加粗等）、**CSI 光标移动**（`\r` 原地重写进度条、`\b` 回退、`\n` 换行）；
- 处理**跨数据块的半截转义序列**——子进程输出是分块到达的，一个 `\x1b[31m` 可能被拆成两块，模拟器用一个 `escState` 跨块续接，避免乱码；
- 渲染用 `requestAnimationFrame` 节流，防止高频输出把 WebView 卡死。

效果：应用内终端和你在 iTerm / 终端.app 里看到的一模一样——彩色日志、动态进度条、光标回车刷新，全部还原。

确认提示（比如 `Proceed? (y/N)`）会**原样显示问题文字**，并智能地自动回一个 `y`，既不卡住命令，也不破坏"真人操作"的观感。

### 3. 环境自检 + 便携 Node：踩过的版本坑

这是最折腾的一环。我最初把内置便携 Node 定成 `v20.18.0`，以为够新，结果用户一跑还是崩——报错 `node:zlib does not provide an export named 'createZstdDecompress'`。

排查后定位：`dsh` 真正的运行时下限是 **Node ≥ v22.15.0**（三个新 API 分别要求 v22.0.0 / v22.14.0 / v22.15.0）。`v20.18.0` 只勉强过了 `util.parseEnv` 那一关，跑到真运行时就露馅。

修复策略：

- 便携 Node 升到 **v22.23.2**（最新 v22 LTS，国内用 `cdn.npmmirror.com` 镜像下载）；
- 最低门槛提到 **v22.15**，且 `resolve_npx()` 改为**优先复用机器上版本达标的 npx**——只有所有候选都太低时，才退回下载内置 v22.23.2。这样用户本地已有 Node 时，零额外下载。

> 小细节：`resolve_npx()` 最初只取"PATH 上第一个存在的 npx 文件"，结果在有些机器上选中了太老的 `/usr/local/bin/npx`，而交互式终端用的是另一个达标的。改成"取第一个版本达标者"后，行为和用户自己开终端完全一致。

### 4. 干净退出 + 端口清理

窗口关闭（`Event::LoopDestroyed`）时，进程句柄负责杀掉子进程：Unix 用 `killpg` 杀整个进程组，Windows 用 `Child::kill()`。这里还有个跨平台小坑——`Child::kill()` 需要 `&mut self`，方法签名和调用处的 `mut` 绑定都要对齐，否则 Windows 编译直接 `E0596`。

另外，启动前会主动清理 3080 端口的残留监听（Unix 用 `lsof` 查占用再 `kill`），避免上次的进程把新启动卡死。

---

## 四、跨平台自动发版：GitHub Actions 一条龙

功能写完，下一步是"别人怎么拿到"。我加了一套 **手动触发**的 GitHub Actions 工作流（`.github/workflows/release.yml`），运行时填个版本号（如 `v0.2.0`），就会：

- **macOS**（Apple Silicon runner 上交叉编译）：分别产出 `aarch64` 与 `x86_64` 两个 `.dmg`，同时支持 Apple Silicon 与 Intel Mac；
- **Linux**（`ubuntu-latest`）：装好 `libwebkit2gtk-4.1-dev` 等系统依赖后构建，产出 `.tar.gz`（含二进制 + 桌面入口 + 图标）；
- **Windows**（`windows-latest`）：构建后打包成 `.zip`；
- 最后由 `release` job 汇总三平台制品，生成 **SHA-256 校验和**，用 `softprops/action-gh-release` 一键创建 GitHub Release 并自动生成变更日志。

`package-macos.sh` 支持 `DSH_BIN` 环境变量，可直接打包预编译二进制，供 CI 在交叉架构下复用，不必每次都重新 `cargo build`。

---

## 五、怎么用

1. 到 **Releases** 页下载对应平台包：
   - macOS：双击 `.dmg` 拖入「应用程序」，首次打开若被拦截，右键「打开」或在终端执行 `xattr -cr /Applications/dsh-desktop.app`；
   - Linux：解压 `.tar.gz`，需系统已装 `webkit2gtk-4.1`；
   - Windows：解压 `.zip`，运行 `.exe`（Win11 自带 WebView2）。
2. 双击运行，App 会自动检测 Node 环境、必要时下载便携版，拉起 `dsh web`，并在窗口内打开 `127.0.0.1:3080`。
3. 关闭窗口即终止后台命令。

---

## 六、已知限制与后续

- **当前产物未签名**：macOS 会触发 Gatekeeper、Windows 会触发 SmartScreen，需用户手动放行。工作流里已预留 Apple 签名+公证的 Secrets 配置示例，后续若做正式发布可一键接上。
- Linux 依赖系统 WebView，发行说明已写明所需包。

后续想做的：自动更新（sparkle / 自研）、给 Windows 也加上签名、以及把"读取登录 shell 的 npx"探测补上，彻底避免特殊情况下的额外下载。

---

## 七、小结

从"一个命令跑不起来"到"双击即用 + 跨平台自动发版"，核心不在于多高深，而在于把**用户真正会踩的坑**（Node 版本、交互卡死、端口残留、退出清理）一个个填平，再用最少依赖把体验包成原生窗口。

如果你想基于 `npx xxx web` 这类命令做自己的桌面壳，这个项目的结构（tao + wry + PTY + 内嵌 ANSI 终端 + GitHub Actions 发版）可以直接参考。

欢迎 Star、提 Issue、提 PR：https://github.com/ht-shaipe/dsh-desktop
