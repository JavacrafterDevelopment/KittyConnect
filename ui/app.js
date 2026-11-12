/* KittyConnect page. Commands go to the host through window.ipc; the host answers
   by calling window.onHost with a whole-state snapshot after every change. */

'use strict';

const $ = (id) => document.getElementById(id);
const BOOT = window.BOOT || { version: 'dev', tailcat: '?' };

function post(msg) {
  if (window.ipc) window.ipc.postMessage(JSON.stringify(msg));
  else demo(msg);
}

function esc(s) {
  return String(s).replace(/[&<>"']/g, (c) => ({ '&': '&amp;', '<': '&lt;', '>': '&gt;', '"': '&quot;', "'": '&#39;' }[c]));
}

function size(n) {
  if (n < 1024) return `${n} B`;
  const u = ['KB', 'MB', 'GB', 'TB'];
  let i = -1;
  do { n /= 1024; i++; } while (n >= 1024 && i < u.length - 1);
  return `${n.toFixed(n < 10 ? 1 : 0)} ${u[i]}`;
}

function pct(done, total) {
  return total > 0 ? Math.min(100, Math.round((done / total) * 100)) : 100;
}

function ext(name) {
  const base = name.split('/').pop();
  const i = base.lastIndexOf('.');
  return i > 0 && base.length - i <= 5 ? base.slice(i + 1) : 'file';
}

function nameHtml(name) {
  const i = name.lastIndexOf('/');
  return i < 0 ? esc(name) : `<span class="dir">${esc(name.slice(0, i + 1))}</span>${esc(name.slice(i + 1))}`;
}

/* Only touch the DOM when the content actually changed, so hover and scroll
   positions survive the frequent progress snapshots. */
const lastHtml = new Map();
function setHtml(el, html) {
  if (lastHtml.get(el) === html) return;
  lastHtml.set(el, html);
  el.innerHTML = html;
}

function speed(bps) {
  return bps > 0 ? `${size(bps)}/s` : '';
}

function ago(at) {
  const s = Math.max(0, Math.round(Date.now() / 1000 - at));
  if (s < 5) return 'checked just now';
  if (s < 60) return `checked ${s}s ago`;
  return `checked ${Math.floor(s / 60)} min ago`;
}

/* Relay or direct, with the round trip from the last 30-second check.
   `who` names the far end in the tooltip. Returns '' while nothing is known. */
function linkPill(link, who = 'your friend') {
  if (!link || link.path === 'unknown') return '';
  const direct = link.path === 'direct';
  const rtt = link.rtt != null ? ` · ${link.rtt} ms` : '';
  const at = link.checkedAgo != null ? Math.round(Date.now() / 1000 - link.checkedAgo) : null;
  const how = direct
    ? `Direct peer-to-peer connection to ${who}${link.endpoint ? ` (${link.endpoint})` : ''}`
    : `Going through a TailCat relay to ${who}. Still end-to-end encrypted; it switches to direct if it can.`;
  return `<span class="linkpill ${direct ? 'direct' : 'relay'}" title="${esc(how)}">`
    + `<span class="dot"></span>${direct ? 'Direct' : 'Relay'}${rtt}`
    + (at != null ? ` <span class="ago" data-at="${at}">${ago(at)}</span>` : '')
    + '</span>';
}

// Keeps "checked 12s ago" honest between snapshots.
setInterval(() => {
  document.querySelectorAll('[data-at]').forEach((el) => { el.textContent = ago(Number(el.dataset.at)); });
}, 5000);

function show(el, on) { el.classList.toggle('on', !!on); }
function hide(el, hidden) { el.classList.toggle('hide', !!hidden); }

const PILL = {
  share: { off: ['', 'Off'], starting: ['busy', 'Starting'], live: ['ok', 'Sharing'], error: ['bad', 'Stopped'] },
  recv:  { off: ['', 'Off'], starting: ['busy', 'Connecting'], live: ['ok', 'Connected'], error: ['bad', 'Failed'] },
  host:  { off: ['', 'Closed'], starting: ['busy', 'Opening'], live: ['ok', 'Open'], error: ['bad', 'Stopped'] },
  join:  { off: ['', 'Off'], starting: ['busy', 'Connecting'], live: ['ok', 'Connected'], error: ['bad', 'Failed'] },
};

function pill(id, kind, st) {
  const [cls, text] = PILL[kind][st] || PILL[kind].off;
  const el = $(id);
  el.className = `pill ${cls}`;
  el.querySelector('.pill-text').textContent = text;
}

/* ------------------------------------------------------------------ tabs */

document.querySelectorAll('.chip[data-page]').forEach((chip) => {
  chip.addEventListener('click', () => page(chip.dataset.page));
});

function page(name) {
  document.querySelectorAll('.chip[data-page]').forEach((c) => c.classList.toggle('on', c.dataset.page === name));
  document.querySelectorAll('.page').forEach((p) => p.classList.toggle('on', p.id === `page-${name}`));
}

/* ------------------------------------------------------------- window */

document.querySelectorAll('.winbtn').forEach((b) => {
  b.addEventListener('click', () => post({ cmd: 'window', action: b.dataset.win }));
});

/* The whole masthead is draggable except the controls sitting on it. */
$('top').addEventListener('mousedown', (e) => {
  if (e.button !== 0 || e.target.closest('button, a, input')) return;
  post({ cmd: 'window', action: 'drag' });
});
$('top').addEventListener('dblclick', (e) => {
  if (!e.target.closest('button, a, input')) post({ cmd: 'window', action: 'maximize' });
});

document.querySelectorAll('.grip').forEach((grip) => {
  grip.addEventListener('mousedown', (e) => {
    if (e.button !== 0) return;
    e.preventDefault();
    post({ cmd: 'resize', dir: grip.dataset.dir });
  });
});

/* -------------------------------------------------------------- logs */

document.querySelectorAll('.logbox').forEach((box) => {
  box.innerHTML = `
    <button class="log-toggle">
      <svg class="chev" width="10" height="10" viewBox="0 0 10 10" fill="none"><path d="M2 3.5 5 6.5 8 3.5" stroke="currentColor" stroke-width="1.4" stroke-linecap="round" stroke-linejoin="round"/></svg>
      <span class="log-label">TailCat log</span>
      <span class="log-last">Nothing yet</span>
    </button>
    <div class="log-body"></div>`;
  box.querySelector('.log-toggle').addEventListener('click', () => box.classList.toggle('open'));
});

function renderLog(name, lines) {
  const box = document.querySelector(`.logbox[data-log="${name}"]`);
  const body = box.querySelector('.log-body');
  const atBottom = body.scrollHeight - body.scrollTop - body.clientHeight < 8;
  box.querySelector('.log-last').textContent = lines.length ? lines[lines.length - 1].replace(/^#\s*/, '') : 'Nothing yet';
  setHtml(body, lines.length ? lines.map(esc).join('\n') : '<span class="meta">TailCat output shows up here.</span>');
  if (atBottom) body.scrollTop = body.scrollHeight;
}

/* ------------------------------------------------------ copy and paste */

document.querySelectorAll('[data-copy]').forEach((b) => {
  b.addEventListener('click', () => {
    const text = $(b.dataset.copy).textContent.trim();
    if (!text) return;
    const discord = !!b.dataset.discord;
    // Backticks stop Discord turning the underscores in the code into italics.
    post({ cmd: 'copy', text: discord ? '`' + text + '`' : text, discord });
    b.classList.add('flash');
    setTimeout(() => b.classList.remove('flash'), 700);
  });
});

document.querySelectorAll('[data-paste]').forEach((b) => {
  b.addEventListener('click', () => post({ cmd: 'paste', target: b.dataset.paste }));
});

/* ------------------------------------------------------------ file share */

$('share-add-files').addEventListener('click', () => post({ cmd: 'share.addFiles' }));
$('share-add-folder').addEventListener('click', () => post({ cmd: 'share.addFolder' }));
$('share-clear').addEventListener('click', () => post({ cmd: 'share.clear' }));
$('share-start').addEventListener('click', () => post({ cmd: 'share.start' }));
$('share-stop').addEventListener('click', () => post({ cmd: 'share.stop' }));
$('share-files').addEventListener('click', (e) => {
  const b = e.target.closest('[data-remove]');
  if (b) post({ cmd: 'share.remove', id: Number(b.dataset.remove) });
});

// The host receives the actual dropped paths; this only lights up the zone.
const drop = $('share-drop');
let dragDepth = 0;
window.addEventListener('dragenter', (e) => { e.preventDefault(); dragDepth++; drop.classList.add('hot'); page('files'); });
window.addEventListener('dragleave', () => { if (--dragDepth <= 0) { dragDepth = 0; drop.classList.remove('hot'); } });
window.addEventListener('dragover', (e) => e.preventDefault());
window.addEventListener('drop', (e) => { e.preventDefault(); dragDepth = 0; drop.classList.remove('hot'); });

$('recv-connect').addEventListener('click', () => post({ cmd: 'recv.connect', code: $('recv-code').value }));
$('recv-code').addEventListener('keydown', (e) => { if (e.key === 'Enter') $('recv-connect').click(); });
$('recv-stop').addEventListener('click', () => post({ cmd: 'recv.stop' }));
$('recv-refresh').addEventListener('click', () => post({ cmd: 'recv.refresh' }));
$('recv-all').addEventListener('click', () => post({ cmd: 'recv.getAll' }));
$('recv-pick').addEventListener('click', () => post({ cmd: 'recv.pickInbox' }));
$('recv-inbox').addEventListener('click', () => post({ cmd: 'recv.openInbox' }));
$('recv-files').addEventListener('click', (e) => {
  const g = e.target.closest('[data-get]');
  if (g) post({ cmd: 'recv.get', id: Number(g.dataset.get) });
  const r = e.target.closest('[data-reveal]');
  if (r) post({ cmd: 'recv.reveal', id: Number(r.dataset.reveal) });
});

const ICON_X = '<svg width="11" height="11" viewBox="0 0 10 10"><path d="M2 2l6 6M8 2L2 8" stroke="currentColor" stroke-width="1.4" stroke-linecap="round"/></svg>';
const ICON_DL = '<svg width="13" height="13" viewBox="0 0 16 16" fill="none"><path d="M8 2.5v8M4.6 7.3 8 10.7l3.4-3.4M3 13.5h10" stroke="currentColor" stroke-width="1.7" stroke-linecap="round" stroke-linejoin="round"/></svg>';
const ICON_FOLDER = '<svg width="13" height="13" viewBox="0 0 16 16" fill="none"><path d="M1.8 4.2c0-.7.5-1.2 1.2-1.2h3l1.5 1.6H13c.7 0 1.2.5 1.2 1.2v6.4c0 .7-.5 1.2-1.2 1.2H3c-.7 0-1.2-.5-1.2-1.2z" stroke="currentColor" stroke-width="1.4" stroke-linejoin="round"/></svg>';

function renderShare(s) {
  const live = s.st === 'live';
  const busy = s.st === 'starting' || live;
  pill('share-pill', 'share', s.st);
  $('share-card').classList.toggle('live', live);
  setHtml($('share-link'), live ? linkPill(s.link, 'whoever is downloading') : '');

  setHtml($('share-files'), s.files.map((f) => `
    <div class="file">
      <span class="file-ico">${esc(ext(f.name).slice(0, 4))}</span>
      <div class="file-main">
        <span class="file-name" title="${esc(f.name)}">${nameHtml(f.name)}</span>
        <span class="file-meta">${size(f.size)}</span>
      </div>
      <button class="iconbtn danger" data-remove="${f.id}" title="Stop sharing this file">${ICON_X}</button>
    </div>`).join(''));
  show($('share-files'), s.files.length);
  show($('share-foot'), s.files.length);
  $('share-summary').textContent = `${s.files.length} file${s.files.length === 1 ? '' : 's'} · ${size(s.totalSize)}`;

  $('share-code').textContent = s.code || '';
  show($('share-codebox'), live && s.code);

  setHtml($('share-activity'), s.sending.map((t) => `
    <div class="act">
      <div class="act-row"><span>Sending <b>${esc(t.name.split('/').pop())}</b></span><span class="meta">${size(t.done)} of ${size(t.total)}${t.speed ? ` · ${speed(t.speed)}` : ''}</span></div>
      <div class="bar-wrap"><div class="bar" style="width:${pct(t.done, t.total)}%"></div></div>
    </div>`).join(''));

  $('share-error').textContent = s.error;
  show($('share-error'), s.st === 'error' && s.error);
  hide($('share-start'), busy);
  hide($('share-stop'), !busy);
  $('share-sent').textContent = s.sent ? `${s.sent} file${s.sent === 1 ? '' : 's'} sent` : '';
  renderLog('share', s.log);
}

function renderRecv(r) {
  const on = r.st === 'starting' || r.st === 'live';
  pill('recv-pill', 'recv', r.st);
  $('recv-card').classList.toggle('live', r.st === 'live');
  setHtml($('recv-link'), r.st === 'live' ? linkPill(r.link, 'the sharer') : '');
  $('recv-code').disabled = on;
  hide($('recv-connect'), on);
  hide($('recv-stop'), !on);
  hide($('recv-refresh'), r.st !== 'live');
  $('recv-error').textContent = r.error;
  show($('recv-error'), r.st === 'error' && r.error);
  $('recv-inbox').textContent = r.inbox;
  $('recv-inbox').title = `Open ${r.inbox}`;

  let html = '';
  if (r.st === 'starting') {
    html = '<div class="empty">Reaching your friend through TailCat…<br>The first connection can take a few seconds.</div>';
  } else if (r.st === 'live' && !r.files.length) {
    html = '<div class="empty">Connected, but nothing is shared yet.<br>Ask your friend to add files, then press Refresh list.</div>';
  } else {
    html = r.files.map((f) => {
      let meta = size(f.size), cls = '', btn = `<button class="iconbtn" data-get="${f.id}" title="Download">${ICON_DL}</button>`, bar = '';
      if (f.st === 'queued') { meta = 'Waiting…'; btn = ''; }
      if (f.st === 'downloading') {
        meta = `${size(f.done)} of ${size(f.total)} · ${pct(f.done, f.total)}%${f.speed ? ` · ${speed(f.speed)}` : ''}`; btn = '';
        bar = `<div class="bar-wrap"><div class="bar" style="width:${pct(f.done, f.total)}%"></div></div>`;
      }
      if (f.st === 'done') { meta = `Saved · ${size(f.size)}`; cls = 'ok'; btn = `<button class="iconbtn" data-reveal="${f.id}" title="Show in folder">${ICON_FOLDER}</button>`; }
      if (f.st === 'error') { meta = f.error; cls = 'bad'; }
      return `
        <div class="file">
          <span class="file-ico">${esc(ext(f.name).slice(0, 4))}</span>
          <div class="file-main">
            <span class="file-name" title="${esc(f.name)}">${nameHtml(f.name)}</span>
            <span class="file-meta ${cls}">${esc(meta)}</span>${bar}
          </div>
          ${btn}
        </div>`;
    }).join('');
  }
  setHtml($('recv-files'), html);
  show($('recv-files'), html);
  show($('recv-foot'), r.st === 'live' && r.files.length);
  const total = r.files.reduce((a, f) => a + f.size, 0);
  $('recv-summary').textContent = `${r.files.length} file${r.files.length === 1 ? '' : 's'} · ${size(total)}`;
  renderLog('recv', r.log);
}

/* ------------------------------------------------------------------ ports */

$('host-start').addEventListener('click', () => post({ cmd: 'port.host', ports: $('host-ports').value }));
$('host-ports').addEventListener('keydown', (e) => { if (e.key === 'Enter') $('host-start').click(); });
document.querySelectorAll('[data-preset]').forEach((b) => {
  b.addEventListener('click', () => {
    const input = $('host-ports');
    if (input.disabled) return;
    const list = input.value.split(/[\s,;]+/).filter(Boolean);
    if (!list.includes(b.dataset.preset)) list.push(b.dataset.preset);
    input.value = list.join(', ');
  });
});

/* Each open port set and each joined one is its own entry in the lists below
   the forms, with its own status, code, log and Stop. */
const openLogs = new Set();
let lastHosts = 0, lastJoins = 0;

function sessLog(id, lines) {
  const last = lines.length ? lines[lines.length - 1].replace(/^#\s*/, '') : 'Nothing yet';
  return `
    <div class="logbox${openLogs.has(id) ? ' open' : ''}" data-sess-log="${id}">
      <button class="log-toggle">
        <svg class="chev" width="10" height="10" viewBox="0 0 10 10" fill="none"><path d="M2 3.5 5 6.5 8 3.5" stroke="currentColor" stroke-width="1.4" stroke-linecap="round" stroke-linejoin="round"/></svg>
        <span class="log-label">TailCat log</span>
        <span class="log-last">${esc(last)}</span>
      </button>
      <div class="log-body">${lines.length ? lines.map(esc).join('\n') : '<span class="meta">TailCat output shows up here.</span>'}</div>
    </div>`;
}

function sessHead(title, kind, st, stopAttr, link = '') {
  const [cls, text] = PILL[kind][st] || PILL[kind].off;
  const stop = st === 'error' ? 'Remove' : (kind === 'host' ? 'Stop' : 'Disconnect');
  return `
    <div class="sess-head">
      <span class="sess-title">${title}</span>
      <span class="pill ${cls}"><span class="dot"></span><span class="pill-text">${text}</span></span>
      ${st === 'live' ? link : ''}
      <span class="grow"></span>
      <button class="btn small ${st === 'error' ? 'ghost' : 'danger'}" ${stopAttr}>${stop}</button>
    </div>`;
}

function portList(ports) {
  return `Port${ports.length === 1 ? '' : 's'} <b>${ports.join(', ')}</b>`;
}

function renderHosts(hosts, error) {
  $('host-error').textContent = error;
  show($('host-error'), error);
  if (hosts.length > lastHosts) $('host-ports').value = '';
  lastHosts = hosts.length;
  const live = hosts.filter((h) => h.st === 'live').length;
  $('host-count').textContent = hosts.length ? `${live} open` : '';
  setHtml($('host-list'), hosts.length ? hosts.map((h) => `
    <div class="sess${h.st === 'live' ? ' live' : ''}">
      ${sessHead(portList(h.ports), 'host', h.st, `data-stop-host="${h.key}"`, linkPill(h.link))}
      ${h.st === 'starting' ? '<p class="meta">Getting a code from TailCat…</p>' : ''}
      ${h.st === 'live' && h.code ? `
        <div class="sess-code">
          <code class="code">${esc(h.code)}</code>
          <div class="row">
            <button class="btn small primary" data-copytext="${esc(h.code)}" data-discord="1">Copy for Discord</button>
            <button class="btn small" data-copytext="${esc(h.code)}">Copy code</button>
            <span class="meta">Send this code and port ${h.ports.join(', ')} to your friend.</span>
          </div>
        </div>` : ''}
      ${h.warn && h.st !== 'error' ? `<p class="warn on">${esc(h.warn)}</p>` : ''}
      ${h.st === 'error' && h.error ? `<p class="err on">${esc(h.error)}</p>` : ''}
      ${sessLog(`h${h.key}`, h.log)}
    </div>`).join('') : '<div class="empty">No ports open. Open one above and it shows up here with its code.</div>');
}

/* Join: each port typed gets a row where the local port can be changed (if
   that number is busy here). An older kitty:port code fills the ports in. */
$('join-code').addEventListener('input', () => {
  const m = /kitty:port:([\d,]+):/.exec($('join-code').value);
  if (m && !$('join-ports').value.trim()) $('join-ports').value = m[1].split(',').join(', ');
  renderMaps();
});
$('join-ports').addEventListener('input', renderMaps);
['join-code', 'join-ports'].forEach((id) => $(id).addEventListener('keydown', (e) => { if (e.key === 'Enter') $('join-start').click(); }));

function joinPorts() {
  const out = [];
  for (const t of $('join-ports').value.split(/[\s,;]+/)) {
    const p = Number(t);
    if (Number.isInteger(p) && p > 0 && p < 65536 && !out.includes(p)) out.push(p);
  }
  return out;
}

function renderMaps() {
  const old = {};
  document.querySelectorAll('#join-maps input').forEach((i) => { old[i.dataset.remote] = i.value; });
  const ports = joinPorts();
  $('join-maps').innerHTML = ports.map((p) => `
    <div class="map">
      <span>Their port <b>${p}</b></span>
      <span class="arrow">→</span>
      <span>your port</span>
      <input class="input small" data-remote="${p}" value="${esc(old[p] || p)}" inputmode="numeric">
    </div>`).join('');
}

$('join-start').addEventListener('click', () => {
  const locals = [...document.querySelectorAll('#join-maps input')].map((i) => Number(i.value) || 0);
  post({ cmd: 'port.join', code: $('join-code').value, ports: joinPorts().join(','), locals, lan: $('join-lan').checked });
});
['host-list', 'join-list'].forEach((id) => $(id).addEventListener('click', (e) => {
  const t = e.target;
  const sh = t.closest('[data-stop-host]');
  if (sh) return post({ cmd: 'port.hostStop', key: Number(sh.dataset.stopHost) });
  const sj = t.closest('[data-stop-join]');
  if (sj) return post({ cmd: 'port.joinStop', key: Number(sj.dataset.stopJoin) });
  const c = t.closest('[data-copytext]');
  if (c) {
    const discord = !!c.dataset.discord;
    post({ cmd: 'copy', text: discord ? '`' + c.dataset.copytext + '`' : c.dataset.copytext, discord });
    c.classList.add('flash');
    setTimeout(() => c.classList.remove('flash'), 700);
    return;
  }
  const lt = t.closest('.log-toggle');
  if (lt) {
    const box = lt.closest('[data-sess-log]');
    box.classList.toggle('open');
    const key = box.dataset.sessLog;
    if (box.classList.contains('open')) openLogs.add(key); else openLogs.delete(key);
  }
}));

function renderJoins(joins, error) {
  $('join-error').textContent = error;
  show($('join-error'), error);
  if (joins.length > lastJoins) {
    $('join-code').value = '';
    $('join-ports').value = '';
    renderMaps();
  }
  lastJoins = joins.length;
  const live = joins.filter((j) => j.st === 'live').length;
  $('join-count').textContent = joins.length ? `${live} connected` : '';
  setHtml($('join-list'), joins.length ? joins.map((j) => `
    <div class="sess${j.st === 'live' ? ' live' : ''}">
      ${sessHead(portList(j.maps.map((m) => m.remote)), 'join', j.st, `data-stop-join="${j.key}"`, linkPill(j.link))}
      ${j.st === 'starting' ? '<p class="meta">Reaching your friend through TailCat…</p>' : ''}
      ${j.st === 'live' ? j.maps.map((m) => `
        <div class="addr">
          <code>127.0.0.1:${m.local}</code>
          <span class="meta">their port ${m.remote}${j.lan ? ` · also this PC's network IP, port ${m.local}` : ''}</span>
          <button class="btn small" data-copytext="127.0.0.1:${m.local}">Copy</button>
        </div>`).join('') : ''}
      ${j.st === 'error' && j.error ? `<p class="err on">${esc(j.error)}</p>` : ''}
      ${sessLog(`j${j.key}`, j.log)}
    </div>`).join('') : '<div class="empty">Not connected to any ports. Paste a code above to join one.</div>');
}

/* ------------------------------------------------------------------- voip */

// The name is remembered for next time, in this app's own (portable) profile.
try { $('voip-name').value = localStorage.getItem('kc-name') || ''; } catch (_) { /* no storage */ }
$('voip-name').addEventListener('change', () => {
  try { localStorage.setItem('kc-name', $('voip-name').value.trim()); } catch (_) { /* no storage */ }
});

function voipName() {
  const n = $('voip-name').value.trim();
  if (!n) $('voip-name').focus();
  return n;
}

$('voip-host').addEventListener('click', () => {
  const name = voipName();
  if (name) post({ cmd: 'voip.host', name, room: $('voip-room-name').value.trim() || 'Hangout' });
});
$('voip-join').addEventListener('click', () => {
  const name = voipName();
  if (name) post({ cmd: 'voip.join', name, code: $('voip-code').value });
});
$('voip-code').addEventListener('keydown', (e) => { if (e.key === 'Enter') $('voip-join').click(); });
$('voip-room-name').addEventListener('keydown', (e) => { if (e.key === 'Enter') $('voip-host').click(); });
$('voip-leave').addEventListener('click', () => post({ cmd: 'voip.leave' }));
$('voip-chatform').addEventListener('submit', (e) => {
  e.preventDefault();
  const text = $('voip-text').value.trim();
  if (!text) return;
  post({ cmd: 'voip.chat', text });
  $('voip-text').value = '';
});
$('voip-voice-join').addEventListener('click', () => post({ cmd: 'voip.voice', on: true }));
$('voip-voice-leave').addEventListener('click', () => post({ cmd: 'voip.voice', on: false }));
let voipState = null;
$('voip-mute').addEventListener('click', () => {
  if (voipState) post({ cmd: 'voip.mute', muted: !voipState.muted, deaf: false });
});
$('voip-deaf').addEventListener('click', () => {
  if (voipState) post({ cmd: 'voip.mute', muted: voipState.muted, deaf: !voipState.deaf });
});

const VOIP_PILL = { starting: ['busy', 'Connecting'], live: ['ok', 'Connected'] };
const ICON_MUTED = '<svg width="13" height="13" viewBox="0 0 16 16" fill="none"><rect x="5.5" y="1.5" width="5" height="8.5" rx="2.5" stroke="currentColor" stroke-width="1.4"/><path d="M3 7.5a5 5 0 0 0 10 0M8 12.5v2M2 2l12 12" stroke="currentColor" stroke-width="1.4" stroke-linecap="round"/></svg>';
const ICON_DEAF = '<svg width="13" height="13" viewBox="0 0 16 16" fill="none"><path d="M2.5 10V8a5.5 5.5 0 0 1 11 0v2" stroke="currentColor" stroke-width="1.4"/><rect x="1.8" y="9.5" width="3" height="4.5" rx="1.2" stroke="currentColor" stroke-width="1.4"/><rect x="11.2" y="9.5" width="3" height="4.5" rx="1.2" stroke="currentColor" stroke-width="1.4"/><path d="M2 2l12 12" stroke="currentColor" stroke-width="1.4" stroke-linecap="round"/></svg>';

function initials(name) {
  const parts = name.trim().split(/\s+/);
  return ((parts[0] || '?')[0] + (parts.length > 1 ? parts[parts.length - 1][0] : '')).toUpperCase();
}

function hue(name) {
  let h = 0;
  for (const c of name) h = (h * 31 + c.charCodeAt(0)) >>> 0;
  return 200 + (h % 50); // blues and teals
}

function person(m, v, speaking) {
  const you = m.id === v.me;
  const tags = [];
  if (m.host) tags.push('<span class="tag-sm">Host</span>');
  if (you) tags.push('<span class="tag-sm you">You</span>');
  const link = m.host ? '' : linkPill({ path: m.path, rtt: m.rtt }, m.name);
  const icons = m.deaf ? `<span class="ico" title="Deafened">${ICON_DEAF}</span>` : m.muted ? `<span class="ico" title="Muted">${ICON_MUTED}</span>` : '';
  return `
    <div class="person" data-member="${m.id}">
      <span class="avatar${speaking.includes(m.id) ? ' speaking' : ''}" style="--h:${hue(m.name)}">${esc(initials(m.name))}</span>
      <span class="person-name">${esc(m.name)}</span>
      ${tags.join('')}${icons}
      <span class="grow"></span>
      ${link}
    </div>`;
}

function time(ts) {
  const d = new Date(ts * 1000);
  return d.toLocaleTimeString([], { hour: '2-digit', minute: '2-digit' });
}

function renderVoip(v) {
  voipState = v;
  const inRoom = v.st === 'starting' || v.st === 'live';
  hide($('voip-lobby'), inRoom);
  hide($('voip-room'), !inRoom);
  $('voip-error').textContent = v.error;
  show($('voip-error'), !inRoom && v.error);
  $('voip-notice').textContent = v.notice;
  show($('voip-notice'), !inRoom && v.notice);
  if (!inRoom) return;

  const [cls, text] = VOIP_PILL[v.st];
  $('voip-pill').className = `pill ${cls}`;
  $('voip-pill').querySelector('.pill-text').textContent = v.st === 'live' && v.mode === 'host' ? 'Hosting' : text;
  $('voip-room-title').textContent = v.room || (v.mode === 'host' ? 'Opening your room…' : 'Joining…');
  setHtml($('voip-link'), v.mode === 'join' && v.st === 'live' ? linkPill(v.link, 'the host') : '');
  $('voip-leave').textContent = v.mode === 'host' ? 'End room' : 'Leave room';
  $('voip-leave').title = v.mode === 'host' ? 'Closes the room for everyone' : '';
  $('voip-room-code').textContent = v.code || '';
  show($('voip-codebox'), v.code);
  $('voip-codebox').classList.toggle('on', !!v.code);

  // Chat: stay pinned to the bottom unless the user has scrolled up to read.
  const list = $('voip-chat');
  const atBottom = list.scrollHeight - list.scrollTop - list.clientHeight < 30;
  setHtml(list, v.chat.length ? v.chat.map((m) => m.system
    ? `<div class="msg system"><span>${esc(m.text)}</span><span class="msg-time">${time(m.ts)}</span></div>`
    : `<div class="msg${m.from === v.me ? ' mine' : ''}">
         <div class="msg-head"><span class="msg-name" style="--h:${hue(m.name)}">${esc(m.name)}</span><span class="msg-time">${time(m.ts)}</span></div>
         <div class="msg-text">${esc(m.text)}</div>
       </div>`).join('') : '<div class="empty">No messages yet. Say hi.</div>');
  if (atBottom) list.scrollTop = list.scrollHeight;
  $('voip-text').disabled = v.st !== 'live';

  // Voice.
  const voiceMembers = v.members.filter((m) => m.voice);
  $('voip-voice-count').textContent = voiceMembers.length ? `${voiceMembers.length} in voice` : '';
  hide($('voip-voice-join'), v.inVoice || v.st !== 'live');
  hide($('voip-voice-leave'), !v.inVoice);
  hide($('voip-mute'), !v.inVoice);
  hide($('voip-deaf'), !v.inVoice);
  $('voip-mute').classList.toggle('active', v.muted);
  $('voip-mute').textContent = v.muted ? 'Unmute' : 'Mute';
  $('voip-deaf').classList.toggle('active', v.deaf);
  $('voip-deaf').textContent = v.deaf ? 'Undeafen' : 'Deafen';
  hide($('voip-meterrow'), !v.inVoice);
  $('voip-devices').textContent = v.devices;
  $('voip-voice-error').textContent = v.voiceError;
  show($('voip-voice-error'), v.voiceError);
  const speaking = liveSpeaking(v);
  setHtml($('voip-invoice'), voiceMembers.length
    ? voiceMembers.map((m) => person(m, v, speaking)).join('')
    : '<div class="empty small">Nobody is in voice yet.</div>');
  setHtml($('voip-members'), v.members.map((m) => person(m, v, speaking)).join(''));
  renderLog('voip', v.log);
}

/* The quick updates: who's talking (from the audio) and your mic level. */
let lastSpeaking = [];
let meTalking = false;
function liveSpeaking(v) {
  return meTalking && v ? [...lastSpeaking, v.me] : lastSpeaking;
}

function voipLive(msg) {
  if (msg.speaking) lastSpeaking = msg.speaking;
  if (msg.talking != null) meTalking = msg.talking;
  if (msg.level != null) $('voip-meter').style.width = `${Math.round(msg.level * 100)}%`;
  const speaking = new Set(liveSpeaking(voipState && { me: msg.me }));
  document.querySelectorAll('#page-voip [data-member]').forEach((el) => {
    el.querySelector('.avatar').classList.toggle('speaking', speaking.has(Number(el.dataset.member)));
  });
}

/* ------------------------------------------------------------ host events */

window.onHost = (msg) => {
  switch (msg.type) {
    case 'state': {
      renderShare(msg.share);
      renderRecv(msg.recv);
      renderHosts(msg.hosts, msg.hostError);
      renderJoins(msg.joins, msg.joinError);
      renderVoip(msg.voip);
      $('statusText').textContent = msg.status.text;
      $('lamp').className = msg.status.kind;
      const filesLive = msg.share.st === 'live' || msg.recv.st === 'live';
      const portsLive = msg.hosts.some((h) => h.st === 'live') || msg.joins.some((j) => j.st === 'live');
      const voipLiveNow = msg.voip.st === 'live';
      show($('files-badge'), filesLive);
      show($('ports-badge'), portsLive);
      show($('voip-badge'), voipLiveNow);
      document.documentElement.classList.toggle('live', filesLive || portsLive || voipLiveNow);
      break;
    }
    case 'voipLive':
      voipLive(msg);
      break;
    case 'paste': {
      const input = $(msg.target);
      if (input && !input.disabled) {
        input.value = msg.text.trim();
        input.dispatchEvent(new Event('input'));
      }
      break;
    }
    case 'dropped':
      page('files');
      break;
    case 'window':
      document.documentElement.classList.toggle('maximized', msg.maximized);
      break;
  }
};

$('data-link').addEventListener('click', () => post({ cmd: 'openData' }));
$('version').textContent = `v${BOOT.version} · TailCat ${BOOT.tailcat}`;

// Keyboard: Ctrl+1 / Ctrl+2 switch tabs.
window.addEventListener('keydown', (e) => {
  if (e.ctrlKey && e.key === '1') page('files');
  if (e.ctrlKey && e.key === '2') page('ports');
  if (e.ctrlKey && e.key === '3') page('voip');
});
document.addEventListener('contextmenu', (e) => { if (!e.target.closest('input, .code, .log-body, .err')) e.preventDefault(); });

/* ------------------------------------------------------ browser preview */

/* Opened as a plain page (no host), show a sample state so the layout can be
   checked in a browser. Never runs inside the app. */
function demo() {}

if (window.ipc) {
  post({ cmd: 'ready' });
} else {
  const addr = 'tcomFwWCCcjS5nKNqAod034nWoJZW0LZqDhhC8U_dKdnDRYQ8uNGFpGQEuXk2pL9qR7vW3yZ8aB4cD6eF1gH5iJ0kLmN';
  window.onHost({
    type: 'state',
    status: { text: 'Sharing is live. Send the code to your friend.', kind: 'ok' },
    share: {
      st: 'live', error: '', code: `kitty:file:51234:${addr}`, totalSize: 734003200, sent: 2,
      files: [
        { id: 1, name: 'world-backup.zip', size: 524288000 },
        { id: 2, name: 'Screenshots/2026-11-10 raid.png', size: 3145728 },
        { id: 3, name: 'Screenshots/2026-11-11 base.png', size: 2883584 },
        { id: 4, name: 'modpack.mrpack', size: 203685888 },
      ],
      sending: [{ name: 'world-backup.zip', done: 199229440, total: 524288000, speed: 6815744 }],
      link: { path: 'direct', endpoint: '203.0.113.7:41641', rtt: null, checkedAgo: null },
      log: ['# Selected bootstrap relay region 302, San Francisco', `# 🐈 Server listening with new address: ${addr}`],
    },
    recv: {
      st: 'live', error: '', inbox: 'C:\\Users\\you\\Downloads',
      link: { path: 'direct', endpoint: '198.51.100.4:41641', rtt: 23, checkedAgo: 12 },
      files: [
        { id: 7, name: 'song.mp3', size: 8388608, st: 'done', done: 8388608, total: 8388608, error: '' },
        { id: 8, name: 'clips/funny.mp4', size: 94371840, st: 'downloading', done: 40000000, total: 94371840, speed: 5452595, error: '' },
        { id: 9, name: 'notes.txt', size: 2048, st: 'idle', done: 0, total: 2048, error: '' },
      ],
      log: [],
    },
    voip: {
      st: 'live', mode: 'host', error: '', notice: '', room: 'Friday raid', name: 'Sam', me: 1,
      code: `kitty:voip:50211:${addr}`, inVoice: true, muted: false, deaf: false, voiceError: '',
      devices: 'Headset Microphone · Headset Earphone', speaking: [2],
      link: { path: 'unknown' },
      members: [
        { id: 1, name: 'Sam', host: true, voice: true, muted: false, deaf: false, path: 'host', rtt: null },
        { id: 2, name: 'Alex Rivera', host: false, voice: true, muted: false, deaf: false, path: 'direct', rtt: 24 },
        { id: 3, name: 'Jo', host: false, voice: false, muted: true, deaf: false, path: 'relay', rtt: 88 },
      ],
      chat: [
        { system: true, text: 'Alex Rivera joined the room', ts: 1794443000 },
        { from: 2, name: 'Alex Rivera', text: 'yo, server is up?', ts: 1794443030 },
        { from: 1, name: 'Sam', text: 'yep, port 25565 is open in the Ports tab', ts: 1794443060 },
        { system: true, text: 'Jo joined the room', ts: 1794443090 },
      ],
      log: [],
    },
    hostError: '', joinError: '',
    hosts: [
      { key: 3, st: 'live', error: '', warn: '', ports: [8080], code: addr.slice(0, 70), log: [`# 🐈 Server listening with new address: ${addr.slice(0, 70)}`] },
      { key: 2, st: 'live', error: '', warn: 'Nothing is running on port 25565 on this PC yet. Start your server; your friend can connect once it\'s up.', ports: [25565], code: addr, log: [] },
    ],
    joins: [
      { key: 4, st: 'live', error: '', lan: false, maps: [{ local: 7777, remote: 7777 }], link: { path: 'relay', rtt: 91, checkedAgo: 40 }, log: [] },
    ],
  });
  voipLive({ speaking: [2], me: 1, level: 0.45, talking: false });
}
