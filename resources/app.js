  function setStage(t) {
    var a = document.getElementById('title'); if (a) a.textContent = t;
    var b = document.getElementById('statusTitle'); if (b) b.textContent = t;
    if (document.getElementById('termWrap').style.display === 'flex') termLog('● ' + t);
  }
  function setSub(t) {
    var a = document.getElementById('sub'); if (a) a.innerHTML = t;
    var b = document.getElementById('statusSub'); if (b) b.innerHTML = t;
    if (document.getElementById('termWrap').style.display === 'flex') termLog('  ' + stripTags(t));
  }
  function setChecklist(html) { document.getElementById('checklist').innerHTML = html; }
  function setStatus(t) {
    var b = document.getElementById('statusSub');
    if (b) b.textContent = t;
  }
  function showPrompt(text) {
    var b = document.getElementById('promptBanner');
    if (b) {
      b.style.display = 'block';
      b.textContent = '需要你的确认：\n' + text + '\n→ 已自动回复 y（继续）。如需手动输入，请在下方输入框操作。';
    }
  }
  function showProgress(show) { document.getElementById('progressWrap').style.display = show ? 'block' : 'none'; }
  function setProgress(p) { document.getElementById('bar').style.width = p + '%'; document.getElementById('pct').textContent = p + '%'; }
  // ================= ANSI terminal emulator =================
  // Renders genuine terminal output: SGR colors/bold/underline, carriage-return
  // progress bars, cursor moves — like a real xterm, but kept lightweight (a
  // row/cell grid, not a full VT100). The child process now runs with
  // TERM=xterm-256color so it emits real ANSI instead of plain text.
  var MAX_ROWS = 2000;
  var termRows = [];          // each row: array of { c: char, s: styleObj }
  var termRow = 0, termCol = 0;
  var escState = '';          // incomplete escape sequence carried across chunks
  var curStyle = null;        // current cell style
  var savedCursor = null;

  function newStyle() {
    return { fg: null, bg: null, bold: false, dim: false, italic: false, underline: false, inverse: false };
  }
  curStyle = newStyle();

  function cloneStyle(s) {
    return { fg: s.fg, bg: s.bg, bold: s.bold, dim: s.dim, italic: s.italic, underline: s.underline, inverse: s.inverse };
  }
  function sameStyle(a, b) {
    return a.fg === b.fg && a.bg === b.bg && a.bold === b.bold && a.dim === b.dim &&
           a.italic === b.italic && a.underline === b.underline && a.inverse === b.inverse;
  }
  function ensureRow() { while (termRows.length <= termRow) termRows.push([]); }

  function writeCell(ch) {
    ensureRow();
    var row = termRows[termRow];
    while (row.length < termCol) row.push({ c: ' ', s: newStyle() });
    var cell = { c: ch, s: cloneStyle(curStyle) };
    if (row.length === termCol) row.push(cell);
    else row[termCol] = cell;
    termCol++;
  }
  function writeNewline() {
    termRow++; termCol = 0;
    if (termRow >= MAX_ROWS) { termRows.shift(); termRow = MAX_ROWS - 1; }
  }
  function cursorUp(n) { termRow = Math.max(0, termRow - n); }
  function cursorDown(n) { termRow += n; }
  function cursorRight(n) { termCol = Math.max(0, termCol + n); }
  function cursorLeft(n) { termCol = Math.max(0, termCol - n); }
  function eraseInLine(mode) {
    ensureRow();
    var row = termRows[termRow];
    if (mode === 1) { for (var i = 0; i < termCol && i < row.length; i++) row[i] = { c: ' ', s: newStyle() }; }
    else if (mode === 2) { termRows[termRow] = []; }
    else { row.length = Math.min(row.length, termCol); }
  }
  function eraseInDisplay(mode) {
    if (mode === 2) { termRows = []; termRow = 0; termCol = 0; }
    else if (mode === 0) { eraseInLine(0); for (var r = termRow + 1; r < termRows.length; r++) termRows[r] = []; }
    else { for (var r2 = 0; r2 < termRow; r2++) termRows[r2] = []; }
  }

  function applySgr(params) {
    if (!params.length) params = [0];
    var i = 0;
    while (i < params.length) {
      var code = params[i];
      if (code === 0) curStyle = newStyle();
      else if (code === 1) curStyle.bold = true;
      else if (code === 2) curStyle.dim = true;
      else if (code === 3) curStyle.italic = true;
      else if (code === 4) curStyle.underline = true;
      else if (code === 7) curStyle.inverse = true;
      else if (code === 22) { curStyle.bold = false; curStyle.dim = false; }
      else if (code === 23) curStyle.italic = false;
      else if (code === 24) curStyle.underline = false;
      else if (code === 27) curStyle.inverse = false;
      else if (code >= 30 && code <= 37) curStyle.fg = ansi16(code - 30);
      else if (code === 39) curStyle.fg = null;
      else if (code >= 90 && code <= 97) curStyle.fg = ansi16(code - 90 + 8);
      else if (code >= 40 && code <= 47) curStyle.bg = ansi16(code - 40);
      else if (code === 49) curStyle.bg = null;
      else if (code >= 100 && code <= 107) curStyle.bg = ansi16(code - 100 + 8);
      else if (code === 38 || code === 48) {
        var isFg = code === 38;
        if (params[i + 1] === 5) { var col = ansi256(params[i + 2] || 0); if (isFg) curStyle.fg = col; else curStyle.bg = col; i += 2; }
        else if (params[i + 1] === 2) { var rgb = 'rgb(' + (params[i + 2] || 0) + ',' + (params[i + 3] || 0) + ',' + (params[i + 4] || 0) + ')'; if (isFg) curStyle.fg = rgb; else curStyle.bg = rgb; i += 4; }
      }
      i++;
    }
  }

  var ANSI16 = ['#000000','#cd3131','#0dbc79','#e5e510','#2472c8','#bc3fbc','#11a8cd','#e5e5e5','#666666','#f14c4c','#23d18b','#f5f543','#3b8eea','#d670d6','#29b8db','#ffffff'];
  function ansi16(n) { return ANSI16[n] || '#ffffff'; }
  var ANSI256 = null;
  function ansi256(idx) {
    if (!ANSI256) {
      ANSI256 = ANSI16.slice();
      var lv = [0, 95, 135, 175, 215, 255];
      for (var r = 0; r < 6; r++) for (var g = 0; g < 6; g++) for (var b = 0; b < 6; b++)
        ANSI256.push('rgb(' + lv[r] + ',' + lv[g] + ',' + lv[b] + ')');
      for (var s = 0; s < 24; s++) { var v = 8 + s * 10; ANSI256.push('rgb(' + v + ',' + v + ',' + v + ')'); }
    }
    return ANSI256[idx] || '#ffffff';
  }

  function handleCsi(p, letter) {
    var params = (p && p.length) ? p.split(';').map(function (x) { return x === '' ? 0 : parseInt(x, 10); }) : [];
    switch (letter) {
      case 'A': cursorUp(params[0] || 1); break;
      case 'B': cursorDown(params[0] || 1); break;
      case 'C': cursorRight(params[0] || 1); break;
      case 'D': cursorLeft(params[0] || 1); break;
      case 'E': termRow += (params[0] || 1); termCol = 0; break;
      case 'F': termRow -= (params[0] || 1); termCol = 0; break;
      case 'G': termCol = Math.max(0, (params[0] || 1) - 1); break;
      case 'H': case 'f': ensureRow(); termRow = Math.max(0, (params[0] || 1) - 1); termCol = Math.max(0, (params[1] || 1) - 1); break;
      case 'K': eraseInLine(params[0] || 0); break;
      case 'J': eraseInDisplay(params[0] || 0); break;
      case 'm': applySgr(params); break;
      case 's': savedCursor = { r: termRow, c: termCol }; break;
      case 'u': if (savedCursor) { termRow = savedCursor.r; termCol = savedCursor.c; } break;
      default: break;
    }
  }

  function escHtml(s) { return s.replace(/&/g, '&amp;').replace(/</g, '&lt;').replace(/>/g, '&gt;'); }
  function styleAttr(s) {
    if (!s) return '';
    var fg = s.inverse ? s.bg : s.fg;
    var bg = s.inverse ? s.fg : s.bg;
    var p = [];
    if (fg) p.push('color:' + fg);
    if (bg) p.push('background:' + bg);
    if (s.bold) p.push('font-weight:bold');
    if (s.dim) p.push('opacity:.7');
    if (s.italic) p.push('font-style:italic');
    if (s.underline) p.push('text-decoration:underline');
    return p.length ? ' style="' + p.join(';') + '"' : '';
  }

  // ---- rendering (batched via requestAnimationFrame so high-frequency output
  // like npm progress bars never freezes the UI) ----
  var needRender = false;
  function scheduleRender() {
    if (needRender) return;
    needRender = true;
    requestAnimationFrame(function () { needRender = false; renderTerm(); });
  }

  function renderTerm() {
    var el = document.getElementById('term');
    if (!el) return;
    // Only paint the most recent rows — the user watches the tail, and this
    // keeps innerHTML rebuilds cheap even with long scrollback.
    var start = Math.max(0, termRows.length - 600);
    var html = '';
    for (var r = start; r < termRows.length; r++) {
      var cells = termRows[r];
      if (!cells.length) { html += '\n'; continue; }
      var i = 0;
      while (i < cells.length) {
        var st = cells[i].s, txt = '';
        while (i < cells.length && sameStyle(cells[i].s, st)) { txt += cells[i].c; i++; }
        html += '<span' + styleAttr(st) + '>' + escHtml(txt) + '</span>';
      }
      html += '\n';
    }
    el.innerHTML = html;
    el.scrollTop = el.scrollHeight;
  }

  function lastTerminalText(n) {
    var start = Math.max(0, termRows.length - n);
    var out = [];
    for (var r = start; r < termRows.length; r++) {
      out.push(termRows[r].map(function (c) { return c.c; }).join(''));
    }
    return out.join('\n');
  }

  // Parse a chunk of (possibly ANSI-bearing) output into the terminal model,
  // like a real xterm would. Incomplete escape sequences that span chunk
  // boundaries are stashed in escState and resumed on the next chunk.
  function parseTerm(raw) {
    var s = escState + raw;
    escState = '';
    var i = 0;
    while (i < s.length) {
      var ch = s[i];
      if (ch === '\u001b') {
        var rest = s.slice(i);
        if (rest.length === 1) { escState = rest; break; }
        var m = rest.match(/^\u001b\[([0-9;?]*)([A-Za-z])/);
        if (m) { handleCsi(m[1], m[2]); i += m[0].length; continue; }
        var o = rest.match(/^\u001b\][^\u001b]*(?:\u001b\\|\u0007)/);
        if (o) { i += o[0].length; continue; }
        var c = rest.match(/^\u001b[()][AB0]/);
        if (c) { i += c[0].length; continue; }
        var tail = rest.slice(1);
        if (rest[1] === '[' || rest[1] === ']' || rest[1] === '(' || rest[1] === ')') {
          if (!/[A-Za-z\u001b]/.test(tail)) { escState = rest; break; }
        }
        i++; continue;
      } else if (ch === '\n') { writeNewline(); i++; continue; }
      else if (ch === '\r') { termCol = 0; i++; continue; }
      else if (ch === '\b') { termCol = Math.max(0, termCol - 1); i++; continue; }
      else if (ch === '\t') { for (var t = 0; t < 8; t++) writeCell(' '); i++; continue; }
      else if (ch.charCodeAt(0) < 32) { i++; continue; }
      else { writeCell(ch); i++; }
    }
  }

  function appendTerm(raw) {
    try {
      parseTerm(raw);
      scheduleRender();
    } catch (e) {
      // Safety net: never let a malformed sequence blank the whole session.
      try {
        var el = document.getElementById('term');
        var pre = document.createElement('div');
        pre.textContent = raw;
        el.appendChild(pre);
        el.scrollTop = el.scrollHeight;
      } catch (_) {}
    }
  }

  // Print one of our own status lines straight into the terminal, like a real
  // shell session would log what it is currently doing.
  function termLog(text) {
    appendTerm(text + '\r\n');
  }

  // Fatal error: keep the terminal intact and overlay a red banner with the
  // message + the last lines of output, so the cause is never hidden. The
  // banner has a close button so the user can dismiss it.
  function showFatal(msg) {
    var banner = document.getElementById('fatalBanner');
    if (!banner) {
      banner = document.createElement('div');
      banner.id = 'fatalBanner';
      banner.style.cssText = 'position:fixed;left:0;right:0;bottom:0;z-index:50;background:#3a1212;color:#ffc9c9;border-top:2px solid #ff6b6b;font:13px/1.6 -apple-system,BlinkMacSystemFont,sans-serif;max-height:45%;display:flex;flex-direction:column;';

      var bar = document.createElement('div');
      bar.style.cssText = 'display:flex;align-items:center;justify-content:space-between;gap:12px;padding:10px 12px 6px;';

      var title = document.createElement('div');
      title.style.cssText = 'font-weight:600;white-space:pre-wrap;';

      var closeBtn = document.createElement('button');
      closeBtn.textContent = '✕ 关闭';
      closeBtn.title = '关闭此提示';
      closeBtn.style.cssText = 'flex:0 0 auto;cursor:pointer;background:#5a1c1c;color:#ffd6d6;border:1px solid #ff6b6b;border-radius:6px;padding:4px 12px;font:12px -apple-system,BlinkMacSystemFont,sans-serif;';
      closeBtn.onclick = function () { banner.style.display = 'none'; };

      bar.appendChild(title);
      bar.appendChild(closeBtn);

      var body = document.createElement('div');
      body.style.cssText = 'padding:0 12px 12px;white-space:pre-wrap;overflow:auto;';

      banner.appendChild(bar);
      banner.appendChild(body);
      (document.body || document.documentElement).appendChild(banner);
      banner._title = title;
      banner._body = body;
    }
    banner.style.display = 'flex';   // re-show if it was dismissed earlier
    var last = lastTerminalText(40);
    banner._title.textContent = '⛔ ' + msg;
    banner._body.textContent = last ? ('最近输出：\n' + last) : '';
    banner._body.scrollTop = banner._body.scrollHeight;
  }

  function stripTags(html) {
    var d = document.createElement('div');
    d.innerHTML = html;
    return d.textContent || d.innerText || '';
  }

  // ==================== Startup auto-recovery diagnostics ====================
  // Amber banner (distinct from the red fatal one): shows which plugins failed,
  // what dsh-desktop did about it (disable & retry / safe mode / recovered),
  // and how to restore a plugin. Payload shape (built in Rust, recovery.rs):
  // { action: 'retry-disabled'|'safe-mode'|'recovered'|'failed',
  //   attempt: 2, plugins: [{ id, module, reason }], note: '...' }
  function showDiagnosis(d) {
    if (!d) return;
    var wrap = document.getElementById('termWrap');
    if (!wrap) return;
    var banner = document.getElementById('diagBanner');
    if (!banner) {
      banner = document.createElement('div');
      banner.id = 'diagBanner';
      banner.style.cssText = 'background:#33270f;border:1px solid #8a6a24;color:#ffd98a;padding:10px 14px;border-radius:8px;font-size:13px;line-height:1.7;margin-bottom:8px;text-align:left;';
      var head = document.createElement('div');
      head.style.cssText = 'display:flex;align-items:center;justify-content:space-between;gap:10px;';
      var title = document.createElement('div');
      title.style.cssText = 'font-weight:600;white-space:pre-wrap;';
      title.id = 'diagTitle';
      var closeBtn = document.createElement('button');
      closeBtn.textContent = '✕';
      closeBtn.title = '关闭此提示';
      closeBtn.style.cssText = 'flex:0 0 auto;cursor:pointer;background:transparent;color:#ffd98a;border:1px solid #8a6a24;border-radius:6px;padding:2px 10px;font:12px inherit;';
      closeBtn.onclick = function () { banner.style.display = 'none'; };
      head.appendChild(title);
      head.appendChild(closeBtn);
      var body = document.createElement('div');
      body.id = 'diagBody';
      body.style.cssText = 'margin-top:6px;white-space:pre-wrap;';
      banner.appendChild(head);
      banner.appendChild(body);
      wrap.insertBefore(banner, wrap.firstChild);
    }
    banner.style.display = 'block';

    var titles = {
      'retry-disabled': '⚠ 检测到故障插件，已自动禁用并重启（第 ' + d.attempt + ' 次失败后）',
      'safe-mode': '⚠ 仍无法启动，已进入安全模式（停用全部第三方插件）重试',
      'recovered': '✓ 自动修复成功 —— 应用已恢复启动',
      'failed': '✗ 自动修复未能恢复启动'
    };
    var t = document.getElementById('diagTitle');
    t.textContent = titles[d.action] || '启动诊断';
    t.style.color = (d.action === 'recovered') ? '#7ee0a0' : (d.action === 'failed' ? '#ff9c9c' : '#ffd98a');

    var html = '';
    if (d.plugins && d.plugins.length) {
      html += '已禁用/停用的插件：\n';
      for (var i = 0; i < d.plugins.length; i++) {
        var p = d.plugins[i];
        html += '  • ' + p.id + (p.module ? '（' + p.module + '）' : '') + '\n';
        if (p.reason) html += '      原因: ' + p.reason + '\n';
      }
      html += '\n';
    }
    if (d.note) html += escHtml(d.note) + '\n';
    if (d.action !== 'recovered') {
      html += '恢复插件：编辑 ~/.dsh/profiles/web/cordis.patch.yml，删除标记为 dsh-desktop auto-recovery 的条目后重启应用。';
    }
    document.getElementById('diagBody').textContent = html;
  }

  var terminalShown = false;
  function showTerminal() {
    if (terminalShown) return;          // idempotent: never double-bind / double-header
    terminalShown = true;
    document.getElementById('spinner').style.display = 'none';
    document.getElementById('title').style.display = 'none';
    document.getElementById('checklist').style.display = 'none';
    document.getElementById('progressWrap').style.display = 'none';
    document.getElementById('sub').style.display = 'none';
    document.getElementById('termWrap').style.display = 'flex';
    termLog('=== 启动 DeepSeek dsh Web ===');
    termLog('环境自检与启动状态将实时显示如下；命令运行输出也会在此呈现。');
    var c = document.getElementById('cmd');
    c.addEventListener('keydown', function (e) {
      if (e.key === 'Enter') {
        var v = c.value; c.value = '';
        appendTerm('\r\ndsh> ' + v + '\r\n');
        window.ipc.postMessage('IN:' + v);
      }
    });
    c.focus();
  }

  // ==================== Update UI ====================
  function setUpdateBtn(text, disabled) {
    // This is called from Rust but the title bar is native NSButton.
    // We keep it for status bar updates if needed.
  }

  // GitHub Release 的正文是 markdown：这里做一个覆盖常用语法的轻量渲染
  // （标题/列表/粗体/行内代码/链接/引用/分隔线），避免把 ## 和 ** 当
  // 纯文本显示。全部使用内联样式，与对话框整体风格保持一致。
  function mdInline(s) {
    s = escHtml(s);
    return s
      // `code` 行内代码
      .replace(/`([^`]+)`/g, function (_, c) {
        return '<code style="background:#252c3b;padding:1px 5px;border-radius:4px;font-family:SF Mono,Menlo,Consolas,monospace;font-size:12px;color:#c9d4e5;">' + c + '</code>';
      })
      // **bold**
      .replace(/\*\*([^*]+)\*\*/g, '<b style="color:#c9d4e5;">$1</b>')
      // [text](url) 链接（仅 http/https）
      .replace(/\[([^\]]+)\]\((https?:[^)\s]+)\)/g, function (_, t, u) {
        return '<a href="' + u + '" target="_blank" style="color:#4f8cff;text-decoration:none;">' + t + '</a>';
      })
      // 裸链接（前面是空白或行首；href 里的 URL 前是引号，不会被二次匹配）
      .replace(/(^|[\s(])(https?:\/\/[^\s<)">]+)/g, function (_, pre, u) {
        return pre + '<a href="' + u + '" target="_blank" style="color:#4f8cff;text-decoration:none;">' + u + '</a>';
      });
  }

  function mdToHtml(md) {
    if (!md) return '';
    var lines = String(md).split(/\r?\n/);
    var out = [];
    var inList = false;
    function closeList() { if (inList) { out.push('</ul>'); inList = false; } }
    for (var i = 0; i < lines.length; i++) {
      var t = lines[i].trim();
      if (!t) { closeList(); continue; }
      var m;
      if ((m = t.match(/^#{1,6}\s+(.+)$/))) {
        // # 标题：统一按小节标题样式渲染
        closeList();
        out.push('<div style="font-weight:600;color:#c9d4e5;margin:10px 0 4px;">' + mdInline(m[1]) + '</div>');
      } else if (/^(-{3,}|\*{3,}|_{3,})$/.test(t)) {
        closeList();
        out.push('<hr style="border:none;border-top:1px solid #2a3242;margin:10px 0;">');
      } else if ((m = t.match(/^[-*+]\s+(.+)$/)) || (m = t.match(/^\d+[.)]\s+(.+)$/))) {
        // 无序/有序列表项：统一渲染为圆点列表
        if (!inList) { out.push('<ul style="margin:6px 0;padding-left:18px;">'); inList = true; }
        out.push('<li style="margin:3px 0;">' + mdInline(m[1]) + '</li>');
      } else if ((m = t.match(/^>\s?(.+)$/))) {
        // > 引用
        closeList();
        out.push('<div style="border-left:3px solid #3a4252;padding-left:10px;margin:6px 0;color:#8b93a3;">' + mdInline(m[1]) + '</div>');
      } else {
        closeList();
        out.push('<p style="margin:6px 0;">' + mdInline(t) + '</p>');
      }
    }
    closeList();
    return out.join('');
  }

  function showUpdateDialog(tag, notes) {
    var overlay = document.createElement('div');
    overlay.style.cssText = 'position:fixed;top:0;left:0;right:0;bottom:0;z-index:100;background:rgba(0,0,0,.55);display:flex;align-items:center;justify-content:center;';
    var box = document.createElement('div');
    box.style.cssText = 'background:#1a1f2e;border:1px solid #2a3242;border-radius:12px;padding:24px;max-width:420px;width:90%;font-family:-apple-system,BlinkMacSystemFont,sans-serif;color:#e6e6e6;';
    box.innerHTML = '<div style="font-size:16px;font-weight:600;margin-bottom:12px;">发现新版本 ' + escHtml(tag) + '</div>'
      + (notes ? '<div style="font-size:13px;color:#8b93a3;max-height:220px;overflow:auto;margin-bottom:16px;line-height:1.6;">' + mdToHtml(notes) + '</div>' : '')
      + '<div style="display:flex;gap:8px;justify-content:flex-end;">'
      + '<button id="updCancel" style="padding:6px 16px;border-radius:6px;border:1px solid #3a4252;background:transparent;color:#8b93a3;cursor:pointer;font:13px -apple-system,sans-serif;">稍后</button>'
      + '<button id="updApply" style="padding:6px 16px;border-radius:6px;border:none;background:#4f8cff;color:#fff;cursor:pointer;font:13px -apple-system,sans-serif;">立即更新</button>'
      + '</div>';
    overlay.appendChild(box);
    (document.body || document.documentElement).appendChild(overlay);
    document.getElementById('updCancel').onclick = function () { overlay.remove(); };
    document.getElementById('updApply').onclick = function () {
      overlay.remove();
      showUpdateProgress(0);
      window.ipc.postMessage('APPLY_UPDATE:' + tag);
    };
  }

  function showUpdateProgress(pct) {
    var el = document.getElementById('updProgress');
    if (!el) {
      el = document.createElement('div');
      el.id = 'updProgress';
      el.style.cssText = 'position:fixed;top:0;left:0;right:0;z-index:100;background:#1a1f2e;border-bottom:1px solid #2a3242;padding:12px 20px;display:flex;align-items:center;gap:12px;font:13px -apple-system,sans-serif;color:#e6e6e6;';
      el.innerHTML = '<span id="updProgText">正在下载更新…</span>'
        + '<div style="flex:1;height:6px;background:#2a3242;border-radius:3px;overflow:hidden;">'
        + '<div id="updProgBar" style="height:100%;width:0%;background:linear-gradient(90deg,#4f8cff,#7ee0a0);transition:width .2s;"></div></div>'
        + '<span id="updProgPct" style="min-width:36px;text-align:right;">0%</span>';
      (document.body || document.documentElement).appendChild(el);
    }
    el.style.display = 'flex';
    if (pct >= 100) {
      document.getElementById('updProgText').textContent = '正在安装…';
      document.getElementById('updProgBar').style.width = '100%';
      document.getElementById('updProgPct').textContent = '100%';
    } else {
      document.getElementById('updProgBar').style.width = pct + '%';
      document.getElementById('updProgPct').textContent = pct + '%';
    }
  }

  function hideUpdateProgress() {
    var el = document.getElementById('updProgress');
    if (el) el.remove();
  }

  function showUpdateComplete(tag) {
    var el = document.getElementById('updProgress');
    if (el) el.remove();
    var overlay = document.createElement('div');
    overlay.style.cssText = 'position:fixed;top:0;left:0;right:0;bottom:0;z-index:100;background:rgba(0,0,0,.55);display:flex;align-items:center;justify-content:center;';
    var box = document.createElement('div');
    box.style.cssText = 'background:#1a1f2e;border:1px solid #2a3242;border-radius:12px;padding:24px;max-width:380px;width:90%;font-family:-apple-system,BlinkMacSystemFont,sans-serif;color:#e6e6e6;text-align:center;';
    box.innerHTML = '<div style="font-size:16px;font-weight:600;margin-bottom:8px;">更新已完成</div>'
      + '<div style="font-size:13px;color:#8b93a3;margin-bottom:16px;">' + escHtml(tag) + ' 已下载并安装，重启后生效。</div>'
      + '<div style="display:flex;gap:8px;justify-content:center;">'
      + '<button id="updLater" style="padding:6px 16px;border-radius:6px;border:1px solid #3a4252;background:transparent;color:#8b93a3;cursor:pointer;font:13px -apple-system,sans-serif;">稍后重启</button>'
      + '<button id="updRestart" style="padding:6px 16px;border-radius:6px;border:none;background:#4f8cff;color:#fff;cursor:pointer;font:13px -apple-system,sans-serif;">立即重启</button>'
      + '</div>';
    overlay.appendChild(box);
    (document.body || document.documentElement).appendChild(overlay);
    document.getElementById('updLater').onclick = function () { overlay.remove(); };
    document.getElementById('updRestart').onclick = function () {
      window.ipc.postMessage('RESTART_APP');
    };
  }

  function showUpdateToast(msg) {
    var toast = document.createElement('div');
    toast.style.cssText = 'position:fixed;top:12px;right:12px;z-index:100;background:#1a2332;border:1px solid #2a3242;color:#7ee0a0;border-radius:8px;padding:8px 16px;font:13px -apple-system,sans-serif;animation:fadeIn .2s;';
    toast.textContent = msg;
    (document.body || document.documentElement).appendChild(toast);
    setTimeout(function () { toast.style.opacity = '0'; toast.style.transition = 'opacity .3s'; }, 2000);
    setTimeout(function () { toast.remove(); }, 2500);
  }

  function showAboutDialog() {
    var overlay = document.createElement('div');
    overlay.style.cssText = 'position:fixed;top:0;left:0;right:0;bottom:0;z-index:100;background:rgba(0,0,0,.55);display:flex;align-items:center;justify-content:center;';
    var box = document.createElement('div');
    box.style.cssText = 'background:#1a1f2e;border:1px solid #2a3242;border-radius:12px;padding:28px;max-width:360px;width:90%;font-family:-apple-system,BlinkMacSystemFont,sans-serif;color:#e6e6e6;text-align:center;';
    box.innerHTML = '<div style="font-size:18px;font-weight:600;margin-bottom:8px;">DeepSeek dsh Desktop</div>'
      + '<div style="font-size:13px;color:#8b93a3;margin-bottom:4px;">版本 ' + (typeof dshVersion !== 'undefined' ? dshVersion : 'unknown') + '</div>'
      + '<div style="font-size:13px;color:#8b93a3;margin-bottom:16px;">基于 DeepSeek dsh Web 构建</div>'
      + '<div style="margin-bottom:16px;"><a href="#" id="aboutGH" style="color:#4f8cff;text-decoration:none;font-size:13px;">github.com/ht-shaipe/dsh-desktop</a></div>'
      + '<button id="aboutOk" style="padding:6px 24px;border-radius:6px;border:none;background:#4f8cff;color:#fff;cursor:pointer;font:13px -apple-system,sans-serif;">好</button>';
    overlay.appendChild(box);
    (document.body || document.documentElement).appendChild(overlay);
    document.getElementById('aboutOk').onclick = function () { overlay.remove(); };
    overlay.addEventListener('click', function (e) { if (e.target === overlay) overlay.remove(); });
    document.getElementById('aboutGH').addEventListener('click', function (e) {
      e.preventDefault();
      window.open('https://github.com/ht-shaipe/dsh-desktop', '_blank');
    });
  }
