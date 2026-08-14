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
