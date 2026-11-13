/* KittyConnect page. Commands go to the host through window.ipc; the host answers
   by calling window.onHost with a whole-state snapshot after every change. */

'use strict';

const $ = (id) => document.getElementById(id);
const BOOT = window.BOOT || { version: 'dev', tailcat: '?', os: /Mac/i.test(navigator.platform || '') ? 'macos' : 'windows' };
/* "windows" or "macos": the stylesheet hides what only one of them has. */
const MAC = BOOT.os === 'macos';
document.documentElement.dataset.os = BOOT.os || 'windows';

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

/* Everyone connected to something you opened, by name, each with Direct or
   Relay as they report it through the tunnel. */
function peersHtml(peers, empty) {
  const rows = peers.map((p) => {
    const at = Math.round(Date.now() / 1000 - p.checkedAgo);
    const link = linkPill({ path: p.path, rtt: p.rtt, checkedAgo: p.checkedAgo }, p.name)
      || `<span class="meta">Connecting… <span class="ago" data-at="${at}">${ago(at)}</span></span>`;
    // On a relay: ask them to reconnect, which tries for a direct path from scratch.
    const retry = p.path !== 'relay' ? ''
      : p.retrying ? '<span class="meta">Retrying…</span>'
        : `<button class="btn small" data-peer-retry="${esc(p.id)}" title="Ask ${esc(p.name)}'s KittyConnect to reconnect and try for a direct connection. Whatever they're doing through it drops for a moment.">Retry direct</button>`;
    return `
      <div class="person">
        <span class="avatar" style="--h:${hue(p.name)}">${esc(initials(p.name))}</span>
        <span class="person-name">${esc(p.name)}</span>
        <span class="grow"></span>
        ${retry}${link}
      </div>`;
  }).join('');
  return `
    <div class="peers">
      <div class="peers-head"><span class="field-label">Connected</span><span class="meta">${peers.length || ''}</span></div>
      ${rows || `<div class="peers-empty">${esc(empty)}</div>`}
    </div>`;
}

/* The name of whoever you joined, next to how you're connected to them, and
   on a relay a button to reconnect and try for a direct path. */
function hostHtml(name, link, fallback, retryAttr = '') {
  const who = name ? `<span class="hostname">${esc(fallback.label)} <b>${esc(name)}</b></span>` : '';
  const retry = retryAttr && link && link.path === 'relay'
    ? `<button class="btn small" ${retryAttr} title="Reconnect and try for a direct connection. Whatever is using it drops for a moment.">Retry direct</button>`
    : '';
  return who + linkPill(link, name || fallback.who) + retry;
}

function show(el, on) { el.classList.toggle('on', !!on); }
function hide(el, hidden) { el.classList.toggle('hide', !!hidden); }

const PILL = {
  share: { off: ['', 'Off'], starting: ['busy', 'Starting'], live: ['ok', 'Sharing'], error: ['bad', 'Stopped'] },
  recv:  { off: ['', 'Off'], starting: ['busy', 'Connecting'], live: ['ok', 'Connected'], error: ['bad', 'Failed'] },
  host:  { off: ['', 'Closed'], starting: ['busy', 'Opening'], live: ['ok', 'Open'], error: ['bad', 'Stopped'] },
  join:  { off: ['', 'Off'], starting: ['busy', 'Reaching'], live: ['ok', 'Connected'], unreachable: ['bad', 'Not reached'], error: ['bad', 'Failed'] },
  mount: { off: ['', 'Stopped'], starting: ['busy', 'Starting'], live: ['ok', 'Sharing'], error: ['bad', 'Stopped'] },
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
  post({ cmd: 'page', page: name });
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

/* One Copy code button everywhere. A code always goes out in backticks:
   Discord (and Slack, Telegram) would otherwise turn the underscores in it
   into italics and drop them, breaking the code. KittyConnect ignores the
   backticks when it's pasted back in. Addresses (127.0.0.1:…) copy as they are. */
function copyCode(text, code) {
  post({ cmd: 'copy', text: code ? '`' + text + '`' : text, code });
}

document.querySelectorAll('[data-copy]').forEach((b) => {
  b.addEventListener('click', () => {
    const text = $(b.dataset.copy).textContent.trim();
    if (!text) return;
    copyCode(text, !!b.dataset.code);
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
  setHtml($('share-peers'), live ? peersHtml(s.peers, 'Nobody yet. They show up here when they connect.') : '');

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
  const downloading = r.files.some((f) => f.st === 'downloading' || f.st === 'queued');
  setHtml($('recv-link'), r.st === 'live'
    ? hostHtml(r.hostName, r.link, { label: 'From', who: 'the sharer' }, downloading ? '' : 'data-recv-retry')
    : '');
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

$('host-start').addEventListener('click', () => { pfFrom = 'manual'; post({ cmd: 'port.host', ports: $('host-ports').value }); });
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
let lastHosts = 0, lastJoins = 0, lastGameHosts = 0, lastGameJoins = 0;
/* Which Port Forward view the last Host/Join press came from, so its error shows there. */
let pfFrom = 'manual';

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
  const stop = st === 'error' ? 'Remove' : (kind === 'host' || kind === 'mount' ? 'Stop' : 'Disconnect');
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

/* One open port (manual) or game server, with its code, who's connected, and
   for Minecraft what friends see. */
function hostCard(h) {
  const g = h.game ? gameInfo(h.game) : null;
  const title = g ? `${esc(g.name)} · port <b>${h.ports[0]}</b>` : portList(h.ports);
  // A manual port's code can carry its port numbers, so they fill in when
  // your friend joins (the checkbox, remembered on this PC).
  const withPorts = !g && inclPorts && h.code;
  const code = withPorts ? `kitty:port:${h.ports.join(',')}:${h.code}` : h.code;
  const hint = g
    ? 'Send this code to your friends. They paste it in Port Forward → Game servers → Join.'
    : withPorts
      ? 'Send this code to your friend. The port numbers are in it.'
      : `Send this code and port ${h.ports.join(', ')} to your friend.`;
  return `
    <div class="sess${h.st === 'live' ? ' live' : ''}">
      ${sessHead(title, 'host', h.st, `data-stop-host="${h.key}"`)}
      ${h.st === 'starting' ? '<p class="meta">Getting a code from TailCat…</p>' : ''}
      ${h.st === 'live' && h.code ? `
        <div class="sess-code">
          <code class="code">${esc(code)}</code>
          <div class="row">
            <button class="btn small primary" data-copytext="${esc(code)}" data-code="1">Copy code</button>
            ${g ? '' : `<label class="inclports" title="Puts the port numbers in the code, so they fill in by themselves when your friend joins">
              <input type="checkbox" data-incl-ports${inclPorts ? ' checked' : ''}> Include port numbers</label>`}
            <span class="meta">${hint}</span>
          </div>
        </div>` : ''}
      ${h.st === 'live' && h.lanNote ? `<p class="okline">${esc(h.lanNote)}</p>` : ''}
      ${h.st === 'live' ? peersHtml(h.peers, 'Nobody has joined yet. Everyone who does shows up here.') : ''}
      ${h.warn && h.st !== 'error' ? `<p class="warn on">${esc(h.warn)}</p>` : ''}
      ${h.st === 'error' && h.error ? `<p class="err on">${esc(h.error)}</p>` : ''}
      ${sessLog(`h${h.key}`, h.log)}
    </div>`;
}

/* "Include port numbers", remembered on this PC. */
let inclPorts = false;
try { inclPorts = localStorage.getItem('kc-incl-ports') === '1'; } catch (_) { /* no storage */ }
let shownHosts = [], shownHostError = '';

function renderHosts(hosts, error) {
  shownHosts = hosts;
  shownHostError = error;
  const manual = hosts.filter((h) => !h.game && !h.mount);
  const games = hosts.filter((h) => h.game);
  $('host-error').textContent = pfFrom === 'manual' ? error : '';
  show($('host-error'), pfFrom === 'manual' && error);
  $('game-host-error').textContent = pfFrom === 'games' ? error : '';
  show($('game-host-error'), pfFrom === 'games' && error);
  if (manual.length > lastHosts) $('host-ports').value = '';
  if (games.length > lastGameHosts) document.querySelectorAll('#game-list .input.port').forEach((i) => { i.value = ''; });
  lastHosts = manual.length;
  lastGameHosts = games.length;
  const count = (list) => (list.length ? `${list.filter((h) => h.st === 'live').length} open` : '');
  $('host-count').textContent = count(manual);
  $('game-host-count').textContent = count(games);
  setHtml($('host-list'), manual.length ? manual.map(hostCard).join('')
    : '<div class="empty">No ports open. Open one above and it shows up here with its code.</div>');
  setHtml($('game-host-list'), games.length ? games.map(hostCard).join('')
    : '<div class="empty">Not hosting a game. Press Host next to one above.</div>');
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
  pfFrom = 'manual';
  post({ cmd: 'port.join', code: $('join-code').value, ports: joinPorts().join(','), locals, lan: $('join-lan').checked });
});
function sessClick(e) {
  const t = e.target;
  const pr = t.closest('[data-peer-retry]');
  if (pr) return post({ cmd: 'peer.retry', id: pr.dataset.peerRetry });
  const jr = t.closest('[data-join-retry]');
  if (jr) return post({ cmd: 'port.retry', key: Number(jr.dataset.joinRetry) });
  const sh = t.closest('[data-stop-host]');
  if (sh) return post({ cmd: 'port.hostStop', key: Number(sh.dataset.stopHost) });
  const sj = t.closest('[data-stop-join]');
  if (sj) return post({ cmd: 'port.joinStop', key: Number(sj.dataset.stopJoin) });
  const c = t.closest('[data-copytext]');
  if (c) {
    copyCode(c.dataset.copytext, !!c.dataset.code);
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
}
['host-list', 'join-list'].forEach((id) => $(id).addEventListener('click', sessClick));
$('host-list').addEventListener('change', (e) => {
  if (!e.target.matches('[data-incl-ports]')) return;
  inclPorts = e.target.checked;
  try { localStorage.setItem('kc-incl-ports', inclPorts ? '1' : '0'); } catch (_) { /* no storage */ }
  renderHosts(shownHosts, shownHostError);
});

/* One joined port set or game server: where to connect, and for Minecraft
   whether the host's world is showing under LAN. */
function joinCard(j) {
  const g = j.game ? gameInfo(j.game) : null;
  const title = g ? `${esc(g.name)}` : portList(j.maps.map((m) => m.remote));
  const host = j.hostName ? esc(j.hostName) : 'your friend';
  let body = '';
  if (j.st === 'live') {
    const addrs = j.maps.map((m) => `
        <div class="addr">
          <code>127.0.0.1:${m.local}</code>
          <span class="meta">${g ? (g.lan ? 'or add it as a server by this address' : `connect ${esc(g.name)} to this address`) : `their port ${m.remote}`}${j.lan ? ` · also this PC's network IP, port ${m.local}` : ''}</span>
          <button class="btn small" data-copytext="127.0.0.1:${m.local}">Copy</button>
        </div>`).join('');
    if (g && g.lan) {
      body = j.worlds.length
        ? j.worlds.map((w) => `<p class="okline">Open <b>Multiplayer</b> in Minecraft: \u201c${esc(w.motd)}\u201d is under LAN. Join it from there.</p>`).join('')
        : `<p class="meta">Waiting for ${host} to open their world to LAN. It shows up in your Multiplayer screen as soon as they do.</p>`;
    }
    body += addrs;
  }
  return `
    <div class="sess${j.st === 'live' ? ' live' : ''}">
      ${sessHead(title, 'join', j.st, `data-stop-join="${j.key}"`, hostHtml(j.hostName, j.link, { label: 'Host', who: 'your friend' }, `data-join-retry="${j.key}"`))}
      ${j.st === 'starting' ? '<p class="meta">Reaching your friend through TailCat…</p>' : ''}
      ${j.st === 'unreachable' ? '<p class="warn on">Can\'t reach your friend. Check the code, and that they still have the port open. KittyConnect keeps trying.</p>' : ''}
      ${body}
      ${j.st === 'error' && j.error ? `<p class="err on">${esc(j.error)}</p>` : ''}
      ${sessLog(`j${j.key}`, j.log)}
    </div>`;
}

function renderJoins(joins, error) {
  const manual = joins.filter((j) => !j.game && !j.mount);
  const games = joins.filter((j) => j.game);
  $('join-error').textContent = pfFrom === 'manual' ? error : '';
  show($('join-error'), pfFrom === 'manual' && error);
  $('game-join-error').textContent = pfFrom === 'games' ? error : '';
  show($('game-join-error'), pfFrom === 'games' && error);
  if (manual.length > lastJoins) {
    $('join-code').value = '';
    $('join-ports').value = '';
    renderMaps();
  }
  if (games.length > lastGameJoins) $('game-code').value = '';
  lastJoins = manual.length;
  lastGameJoins = games.length;
  const count = (list) => (list.length ? `${list.filter((j) => j.st === 'live').length} connected` : '');
  $('join-count').textContent = count(manual);
  $('game-join-count').textContent = count(games);
  setHtml($('join-list'), manual.length ? manual.map(joinCard).join('')
    : '<div class="empty">Not connected to any ports. Paste a code above to join one.</div>');
  setHtml($('game-join-list'), games.length ? games.map(joinCard).join('')
    : '<div class="empty">Not in any game. Paste a game code above to join.</div>');
}

/* ----------------------------------------------------------- game servers */

/* One-click games. TCP only, since that's what TailCat carries. Minecraft also
   gets its LAN discovery carried through the tunnel (`lan` in the app). */
const GAMES = [
  { id: 'minecraft', name: 'Minecraft: Java Edition', short: 'MC', port: 25565, h: 110, lan: true,
    desc: 'Open your world to LAN on this port. Friends see it under LAN in Multiplayer.' },
  { id: 'terraria', name: 'Terraria / tModLoader', short: 'TR', port: 7777, h: 150,
    desc: 'Host &amp; Play in Terraria. Friends join via IP with the address they get.' },
  { id: 'starbound', name: 'Starbound', short: 'SB', port: 21025, h: 265,
    desc: 'Run your server, then friends join by the address they get.' },
  { id: 'vintagestory', name: 'Vintage Story', short: 'VS', port: 42420, h: 30,
    desc: 'Run your server, then friends join by the address they get.' },
];

function gameInfo(id) {
  return GAMES.find((g) => g.id === id) || { id, name: id, short: id.slice(0, 2).toUpperCase(), port: 0, h: 210, desc: '' };
}

$('game-list').innerHTML = GAMES.map((g) => `
  <div class="game">
    <span class="game-ico" style="--h:${g.h}">${g.short}</span>
    <div class="game-main">
      <span class="game-name">${esc(g.name)}</span>
      <span class="game-desc">${g.desc}</span>
    </div>
    <input class="input small port" data-game-port="${g.id}" inputmode="numeric" maxlength="5" placeholder="${g.port}" title="Port (${g.port} unless you change it)">
    <button class="btn small primary" data-game-host="${g.id}">Host</button>
  </div>`).join('');

$('game-list').addEventListener('click', (e) => {
  const b = e.target.closest('[data-game-host]');
  if (!b) return;
  const g = gameInfo(b.dataset.gameHost);
  const input = document.querySelector(`[data-game-port="${g.id}"]`);
  pfFrom = 'games';
  post({ cmd: 'game.host', game: g.id, port: input.value.trim() || String(g.port) });
});
$('game-list').addEventListener('keydown', (e) => {
  const i = e.target.closest('[data-game-port]');
  if (i && e.key === 'Enter') document.querySelector(`[data-game-host="${i.dataset.gamePort}"]`).click();
});
$('game-join').addEventListener('click', () => {
  pfFrom = 'games';
  post({ cmd: 'game.join', code: $('game-code').value });
});
$('game-code').addEventListener('keydown', (e) => { if (e.key === 'Enter') $('game-join').click(); });
['game-host-list', 'game-join-list'].forEach((id) => $(id).addEventListener('click', sessClick));

/* Port Forward opens on a choice between game servers and manual; the arrow
   goes back to it. */
function pfView(name) {
  for (const v of ['home', 'games', 'manual']) hide($(`pf-${v}`), v !== name);
  hide($('pf-back'), name === 'home');
  $('pf-title').textContent = { home: 'Port Forward', games: 'Game servers', manual: 'Manual port forward' }[name];
  pfFrom = name === 'home' ? pfFrom : name;
}
document.querySelectorAll('[data-pf]').forEach((b) => b.addEventListener('click', () => pfView(b.dataset.pf)));
$('pf-back').addEventListener('click', () => pfView('home'));

function renderPfCounts(hosts, joins) {
  const sum = (h, j) => [h.length && `${h.length} hosting`, j.length && `${j.length} joined`].filter(Boolean).join(' · ');
  $('pf-games-count').textContent = sum(hosts.filter((h) => h.game), joins.filter((j) => j.game));
  $('pf-manual-count').textContent = sum(hosts.filter((h) => !h.game && !h.mount), joins.filter((j) => !j.game && !j.mount));
}

/* ------------------------------------------------------------------- voip */

// Rooms use the name picked when the app opened (the chip, top right).
$('voip-host').addEventListener('click', () => {
  post({ cmd: 'voip.host', room: $('voip-room-name').value.trim() || 'Hangout' });
});
$('voip-join').addEventListener('click', () => {
  post({ cmd: 'voip.join', code: $('voip-code').value });
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
  if (m.smuted) tags.push('<span class="tag-sm bad" title="Muted for everyone by the host">Muted by host</span>');
  if (m.screen) tags.push('<span class="tag-sm live">LIVE</span>');
  const link = m.host ? '' : linkPill({ path: m.path, rtt: m.rtt }, m.name);
  const icons = m.deaf ? `<span class="ico" title="Deafened">${ICON_DEAF}</span>` : m.muted ? `<span class="ico" title="Muted">${ICON_MUTED}</span>` : '';
  const g = v.gains[m.id];
  const mine = !you && g != null && g !== 1 ? `<span class="ico soft" title="Your volume for ${esc(m.name)}">${g === 0 ? ICON_MUTED : `${Math.round(g * 100)}%`}</span>` : '';
  const watch = m.screen && !you
    ? `<button class="btn small ${v.watching === m.id ? '' : 'primary'}" data-watch="${v.watching === m.id ? '' : m.id}">${v.watching === m.id ? 'Watching' : 'Watch'}</button>`
    : '';
  return `
    <div class="person" data-member="${m.id}" title="Right-click for options">
      <span class="avatar${speaking.includes(m.id) ? ' speaking' : ''}" style="--h:${hue(m.name)}">${esc(initials(m.name))}</span>
      <span class="person-name">${esc(m.name)}</span>
      ${tags.join('')}${icons}${mine}
      <span class="grow"></span>
      ${watch}${link}
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
         ${m.text ? `<div class="msg-text">${esc(m.text)}</div>` : ''}
         ${m.file ? attachmentHtml(m.file, v) : ''}
       </div>`).join('') : '<div class="empty">No messages yet. Say hi.</div>');
  if (atBottom) list.scrollTop = list.scrollHeight;
  $('voip-text').disabled = v.st !== 'live';
  $('voip-attach').disabled = v.st !== 'live';
  setHtml($('voip-uploads'), v.uploads.map((u) => u.error
    ? `<div class="upload bad"><span>${esc(u.name)}: ${esc(u.error)}</span></div>`
    : `<div class="upload"><span>Sending <b>${esc(u.name)}</b></span><span class="meta">${u.size ? `${pct(u.sent, u.size)}%` : '…'}</span>
         <div class="bar-wrap"><div class="bar" style="width:${pct(u.sent, u.size || 1)}%"></div></div></div>`).join(''));
  $('voip-room-notice').textContent = v.roomNotice;
  show($('voip-room-notice'), v.roomNotice);
  renderScreen(v);
  applySavedVolumes(v);

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
  // Voice and screen share travel over UDP inside the tunnel; chat over TCP.
  const mediaText = v.st !== 'live' ? '' : {
    lost: 'Voice and screen share lost their connection to the room. Reconnecting… Chat still works.',
    failed: 'Voice and screen share couldn\'t connect to the room. Chat still works. Try leaving and joining again.',
  }[v.media] || '';
  $('voip-media-state').textContent = mediaText;
  show($('voip-media-state'), mediaText);
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
  if (msg.level != null) showLevel(msg.level, msg.talking);
  const speaking = new Set(liveSpeaking(voipState && { me: msg.me }));
  document.querySelectorAll('#page-voip [data-member]').forEach((el) => {
    el.querySelector('.avatar').classList.toggle('speaking', speaking.has(Number(el.dataset.member)));
  });
}

/* ------------------------------------------------------- voice settings */

/* Yours only, remembered on this PC, and applied at once (in a call or not). */
const VS_DEFAULT = { inputGain: 1, autoSensitivity: true, thresholdDb: -50, noiseSuppression: true, outputGain: 1 };
let vs = { ...VS_DEFAULT };
try { vs = { ...VS_DEFAULT, ...JSON.parse(localStorage.getItem('kc-audio') || '{}') }; } catch (_) { /* defaults */ }

function vsRender() {
  $('vs-in').value = Math.round(vs.inputGain * 100);
  $('vs-in-val').textContent = `${Math.round(vs.inputGain * 100)}%`;
  $('vs-out').value = Math.round(vs.outputGain * 100);
  $('vs-out-val').textContent = `${Math.round(vs.outputGain * 100)}%`;
  $('vs-auto').checked = vs.autoSensitivity;
  $('vs-ns').checked = vs.noiseSuppression;
  $('vs-thresh').value = vs.thresholdDb;
  $('vs-thresh').disabled = vs.autoSensitivity;
  $('vs-mark').style.left = `${vs.thresholdDb + 100}%`;
  $('vs-mark').classList.toggle('hide', vs.autoSensitivity);
  $('vs-sens-desc').textContent = vs.autoSensitivity
    ? 'Automatic. Turn it off to set the level yourself.'
    : `Sends when your mic is above ${vs.thresholdDb} dB. Drag left if you cut out, right if noise gets through.`;
}

function vsSave() {
  try { localStorage.setItem('kc-audio', JSON.stringify(vs)); } catch (_) { /* no storage */ }
  post({ cmd: 'voip.audio', settings: vs });
  vsRender();
}

$('vs-in').addEventListener('input', () => { vs.inputGain = Number($('vs-in').value) / 100; vsSave(); });
$('vs-out').addEventListener('input', () => { vs.outputGain = Number($('vs-out').value) / 100; vsSave(); });
$('vs-auto').addEventListener('change', () => { vs.autoSensitivity = $('vs-auto').checked; vsSave(); });
$('vs-ns').addEventListener('change', () => { vs.noiseSuppression = $('vs-ns').checked; vsSave(); });
$('vs-thresh').addEventListener('input', () => { vs.thresholdDb = Number($('vs-thresh').value); vsSave(); });
$('vs-open').addEventListener('click', () => { vsRender(); hide($('vs-modal'), false); });
$('vs-close').addEventListener('click', () => hide($('vs-modal'), true));
$('vs-modal').addEventListener('mousedown', (e) => { if (e.target === $('vs-modal')) hide($('vs-modal'), true); });
window.addEventListener('keydown', (e) => { if (e.key === 'Escape') hide($('vs-modal'), true); });

/* The mic level (dBFS, -100..0) on both meters. Green while you're being sent. */
function showLevel(db, talking) {
  const pctW = `${Math.max(0, Math.min(100, db + 100))}%`;
  $('voip-meter').style.width = pctW;
  $('vs-level').style.width = pctW;
  $('vs-level').classList.toggle('on', !!talking);
  $('voip-meter').classList.toggle('on', !!talking);
}

// The saved settings go to the app as soon as the page starts.
post({ cmd: 'voip.audio', settings: vs });

/* -------------------------------------------------------- attachments */

function attachmentHtml(f, v) {
  const d = v.downloads[String(f.aid)];
  const img = v.previews.includes(f.aid)
    ? `<img class="att-img" src="/att/${v.gen}/${f.aid}" alt="${esc(f.name)}" draggable="false">`
    : '';
  let action = `<button class="btn small" data-dl="${f.aid}">${ICON_DL} Download</button>`;
  let meta = size(f.size);
  if (d && d.st === 'downloading') {
    action = '';
    meta = d.size ? `${size(d.got)} of ${size(d.size)} · ${pct(d.got, d.size)}%` : 'Starting…';
  } else if (d && d.st === 'done') {
    action = `<button class="btn small" data-show="${f.aid}">${ICON_FOLDER} Show in folder</button>`;
    meta = `Saved · ${size(f.size)}`;
  } else if (d && d.st === 'error') {
    meta = d.error;
  }
  return `${img}
    <div class="att">
      <span class="file-ico">${esc(ext(f.name).slice(0, 4))}</span>
      <div class="file-main">
        <span class="file-name" title="${esc(f.name)}">${esc(f.name)}</span>
        <span class="file-meta${d && d.st === 'error' ? ' bad' : ''}">${esc(meta)}</span>
      </div>
      ${action}
    </div>`;
}

$('voip-attach').addEventListener('click', () => {
  post({ cmd: 'voip.attach', text: $('voip-text').value.trim() });
  $('voip-text').value = '';
});
$('voip-chat').addEventListener('click', (e) => {
  const dl = e.target.closest('[data-dl]');
  if (dl) post({ cmd: 'voip.download', aid: Number(dl.dataset.dl) });
  const sh = e.target.closest('[data-show]');
  if (sh) post({ cmd: 'voip.reveal', aid: Number(sh.dataset.show) });
});

/* -------------------------------------------------------- screen share */

/* Like Discord: capture with the browser, encode with a hardware video encoder
   where there is one (H.264, else VP8), at a bitrate that fits the size and
   frame rate, with a key frame every two seconds so late joiners and dropped
   frames recover quickly. Chunks go to the room through the TailCat tunnel. */

const ss = { stream: null, encoder: null, reader: null, running: false, forceKey: false, bytes: 0, frames: 0, info: null, tick: 0, config: null, rate: 0, maxRate: 0 };

function nativeRes() {
  const r = window.devicePixelRatio || 1;
  return { w: Math.round(screen.width * r), h: Math.round(screen.height * r) };
}

function fillResOptions() {
  const n = nativeRes();
  const opts = [`<option value="native">Your screen (${n.w}×${n.h})</option>`];
  for (const h of [1440, 1080, 720, 480]) {
    if (h < n.h) opts.push(`<option value="${h}">${h}p</option>`);
  }
  $('ss-res').innerHTML = opts.join('');
  estimate();
}

function outSize(srcW, srcH, choice) {
  let h = choice === 'native' ? srcH : Math.min(srcH, Number(choice));
  let w = Math.round((srcW * h) / srcH);
  return { w: w - (w % 2), h: h - (h % 2) };
}

/* Compression presets. Bits per pixel per frame sets the bitrate: Balanced is
   roughly what Discord streams at (~2.5 Mbit/s for 720p30, ~4.5 for 1080p30).
   Text and detail spends about three times that and tells the encoder to keep
   edges sharp rather than motion smooth, so small text stays readable. */
const QUALITY = {
  text:     { bpp: 0.22, max: 25e6, hint: 'text',   label: 'sharp text' },
  balanced: { bpp: 0.08, max: 12e6, hint: 'detail', label: 'balanced' },
  motion:   { bpp: 0.08, max: 12e6, hint: 'motion', label: 'smooth motion' },
  saver:    { bpp: 0.035, max: 4e6, hint: 'detail', label: 'data saver' },
};

function preset() {
  return QUALITY[$('ss-quality').value] || QUALITY.balanced;
}

function bitrateFor(w, h, fps) {
  const p = preset();
  return Math.round(Math.min(p.max, Math.max(400e3, w * h * fps * p.bpp)));
}

function estimate() {
  const n = nativeRes();
  const { w, h } = outSize(n.w, n.h, $('ss-res').value || 'native');
  const fps = Number($('ss-fps').value);
  const mbps = (bitrateFor(w, h, fps) / 1e6).toFixed(1);
  const tip = preset() === QUALITY.text
    ? ' For reading code or documents, 15 to 30 fps at full resolution works best.'
    : ' On a relay, a lower setting stays smoother.';
  $('ss-est').textContent = `${w}×${h} at ${fps} fps, about ${mbps} Mbit/s.${tip}`;
}
$('ss-res').addEventListener('change', estimate);
$('ss-fps').addEventListener('change', estimate);
$('ss-quality').addEventListener('change', estimate);

async function pickConfig(w, h, fps) {
  const bitrate = bitrateFor(w, h, fps);
  const base = { width: w, height: h, bitrate, framerate: fps, latencyMode: 'realtime', bitrateMode: 'variable' };
  const high = w * h > 2073600 || fps > 30 ? 'avc1.640033' : 'avc1.640028';
  const gpu = { codec: high, avc: { format: 'annexb' }, hardwareAcceleration: 'prefer-hardware' };
  // Frame-rate layers (L1T3: three of them) let the room send a viewer whose
  // connection can't keep up fewer frames, without lowering the picture for
  // everyone else. Best on the graphics card; in software only where it keeps
  // up easily (1080p at 30 fps or less). Otherwise, no layers: a slow viewer
  // then gets key frames only until they catch up.
  const light = w * h <= 2073600 && fps <= 30;
  const candidates = [
    { ...base, ...gpu, scalabilityMode: 'L1T3' },
    { ...base, ...gpu, scalabilityMode: 'L1T2' },
    ...(light ? [
      { ...base, codec: 'avc1.42E033', avc: { format: 'annexb' }, scalabilityMode: 'L1T3' },
      { ...base, codec: 'vp8', scalabilityMode: 'L1T3' },
    ] : []),
    { ...base, ...gpu },
    { ...base, codec: 'avc1.42E033', avc: { format: 'annexb' } },
    { ...base, codec: 'vp8' },
  ];
  for (const c of candidates) {
    try {
      const r = await VideoEncoder.isConfigSupported(c);
      if (r.supported) return c;
    } catch (_) { /* try the next */ }
  }
  return null;
}

function b64(bytes) {
  let s = '';
  for (let i = 0; i < bytes.length; i += 0x8000) s += String.fromCharCode.apply(null, bytes.subarray(i, i + 0x8000));
  return btoa(s);
}

function unb64(str) {
  const s = atob(str);
  const out = new Uint8Array(s.length);
  for (let i = 0; i < s.length; i++) out[i] = s.charCodeAt(i);
  return out;
}

function shareError(text) {
  $('ss-error').textContent = text;
  show($('ss-error'), text);
}

async function startShare() {
  shareError('');
  if (!navigator.mediaDevices || !navigator.mediaDevices.getDisplayMedia || !window.VideoEncoder || !window.MediaStreamTrackProcessor) {
    return shareError('Screen sharing needs a newer Microsoft Edge WebView2 runtime. Update Edge and try again.');
  }
  const fps = Number($('ss-fps').value);
  let stream;
  try {
    stream = await navigator.mediaDevices.getDisplayMedia({ video: { frameRate: { ideal: fps, max: fps } }, audio: false });
  } catch (e) {
    return shareError(e && e.name === 'NotAllowedError' ? 'Screen sharing was cancelled.' : `Couldn't capture the screen: ${e && e.message}`);
  }
  const track = stream.getVideoTracks()[0];
  const set = track.getSettings();
  const n = nativeRes();
  const { w, h } = outSize(set.width || n.w, set.height || n.h, $('ss-res').value);
  const config = await pickConfig(w, h, fps);
  if (!config) {
    stream.getTracks().forEach((t) => t.stop());
    return shareError('No video encoder is available for that size. Try a lower resolution.');
  }
  ss.stream = stream;
  ss.running = true;
  ss.bytes = 0;
  ss.frames = 0;
  ss.forceKey = true;
  ss.config = config;
  // The chosen quality's bitrate is the most it will use; it's lowered while
  // a watcher's connection can't keep up, and raised again when it can.
  ss.maxRate = config.bitrate;
  ss.rate = config.bitrate;
  ss.info = { w, h, fps, codec: config.codec, bitrate: config.bitrate };
  // Tells the capture and encoder what to protect: sharp edges or smooth motion.
  try { track.contentHint = preset().hint; } catch (_) { /* older runtime */ }
  ss.encoder = new VideoEncoder({
    output: (chunk, meta) => {
      const buf = new Uint8Array(chunk.byteLength);
      chunk.copyTo(buf);
      ss.bytes += buf.length;
      // Its frame-rate layer (0 without layers), so the room can leave the
      // higher ones out for a viewer who can't keep up.
      const layer = meta && meta.svc ? meta.svc.temporalLayerId || 0 : 0;
      post({ cmd: 'voip.video', key: chunk.type === 'key', layer, ts: Math.round(chunk.timestamp), data: b64(buf) });
    },
    error: (e) => stopShare(`The video encoder stopped: ${e.message}`),
  });
  ss.encoder.configure(config);
  post({ cmd: 'voip.screen', info: ss.info });
  $('ss-preview').srcObject = stream;
  track.addEventListener('ended', () => stopShare());
  hide($('ss-box'), true);
  hide($('ss-live'), false);
  hide($('ss-row'), true);

  // Frames come in at the screen's pace; this keeps to the chosen rate and
  // size, and never lets the encoder fall behind.
  const canvas = new OffscreenCanvas(w, h);
  const ctx = canvas.getContext('2d');
  ctx.imageSmoothingQuality = 'high'; // a sharper downscale keeps text legible
  ss.reader = new MediaStreamTrackProcessor({ track }).readable.getReader();
  const interval = 1e6 / fps;
  let last = -Infinity;
  let sent = 0;
  ss.tick = setInterval(() => {
    const mbps = ((ss.bytes * 8) / 2 / 1e6).toFixed(1);
    const fitted = ss.rate < ss.maxRate * 0.97 ? ' · lowered to fit a viewer\'s connection' : '';
    // "layered": a viewer who can't keep up can be sent fewer frames on their own.
    const layered = config.scalabilityMode ? ' · layered' : '';
    $('ss-stats').textContent = `${w}×${h} · ${Math.round(ss.frames / 2)} fps · ${mbps} Mbit/s · ${config.codec.startsWith('avc') ? 'H.264' : 'VP8'}${layered}${fitted}`;
    ss.bytes = 0;
    ss.frames = 0;
  }, 2000);
  while (ss.running) {
    let r;
    try { r = await ss.reader.read(); } catch (_) { break; }
    if (r.done) break;
    const frame = r.value;
    if (frame.timestamp - last < interval * 0.9 || ss.encoder.encodeQueueSize > 2) {
      frame.close();
      continue;
    }
    last = frame.timestamp;
    const keyFrame = ss.forceKey || sent % (fps * 2) === 0;
    ss.forceKey = false;
    sent++;
    ss.frames++;
    let input = frame;
    if (frame.displayWidth !== w || frame.displayHeight !== h) {
      ctx.drawImage(frame, 0, 0, w, h);
      input = new VideoFrame(canvas, { timestamp: frame.timestamp });
      frame.close();
    }
    try { ss.encoder.encode(input, { keyFrame }); } catch (_) { /* closed */ }
    input.close();
  }
}

function stopShare(error) {
  if (!ss.running && !ss.stream) return;
  ss.running = false;
  clearInterval(ss.tick);
  try { ss.reader && ss.reader.cancel(); } catch (_) { /* gone */ }
  try { ss.encoder && ss.encoder.state !== 'closed' && ss.encoder.close(); } catch (_) { /* gone */ }
  if (ss.stream) ss.stream.getTracks().forEach((t) => t.stop());
  ss.stream = null;
  ss.encoder = null;
  $('ss-preview').srcObject = null;
  post({ cmd: 'voip.screen', info: null });
  hide($('ss-live'), true);
  hide($('ss-row'), false);
  if (error) shareError(error);
}

/* Going live needs screen capture and a frame reader that Apple's web view
   doesn't give apps; watching works. */
if (MAC) {
  $('ss-open').disabled = true;
  $('ss-open').title = 'Sharing your screen from a Mac isn\'t available yet. You can still watch others.';
  $('ss-row').insertAdjacentHTML('beforeend', '<span class="meta">Not available on Mac yet. You can watch others.</span>');
}
$('ss-open').addEventListener('click', () => {
  if (MAC) return;
  fillResOptions();
  hide($('ss-box'), false);
  hide($('ss-row'), true);
});
$('ss-cancel').addEventListener('click', () => { hide($('ss-box'), true); hide($('ss-row'), false); });
$('ss-go').addEventListener('click', () => startShare());
$('ss-stop').addEventListener('click', () => stopShare());

/* Watching: decode with the browser's (hardware) decoder onto a canvas. */
const view = { id: null, decoder: null, codec: null, waitKey: true, frames: 0, bytes: 0, tick: 0 };

function closeViewer() {
  if (document.documentElement.classList.contains('theater')) setTheater(false);
  try { view.decoder && view.decoder.state !== 'closed' && view.decoder.close(); } catch (_) { /* gone */ }
  view.decoder = null;
  view.id = null;
  clearInterval(view.tick);
  hide($('voip-stage'), true);
}

function openViewer(m) {
  closeViewer();
  view.id = m.id;
  view.codec = m.screen.codec;
  view.waitKey = true;
  view.frames = 0;
  view.bytes = 0;
  const canvas = $('stage-canvas');
  const ctx = canvas.getContext('2d');
  view.decoder = new VideoDecoder({
    output: (frame) => {
      if (canvas.width !== frame.displayWidth || canvas.height !== frame.displayHeight) {
        canvas.width = frame.displayWidth;
        canvas.height = frame.displayHeight;
      }
      ctx.drawImage(frame, 0, 0);
      frame.close();
      view.frames++;
      hide($('stage-wait'), true);
    },
    error: () => resetDecoder(),
  });
  view.decoder.configure({ codec: view.codec, optimizeForLatency: true, hardwareAcceleration: 'no-preference' });
  $('stage-title').textContent = `${m.name}'s screen`;
  hide($('stage-wait'), false);
  hide($('voip-stage'), false);
  view.tick = setInterval(() => {
    $('stage-stats').textContent = `${m.screen.w}×${m.screen.h} · ${Math.round(view.frames / 2)} fps · ${((view.bytes * 8) / 2 / 1e6).toFixed(1)} Mbit/s`;
    view.frames = 0;
    view.bytes = 0;
  }, 2000);
}

// After a decode error or a backlog: start again from the next key frame.
function resetDecoder() {
  if (view.id == null) return;
  try { view.decoder && view.decoder.state !== 'closed' && view.decoder.reset(); } catch (_) { /* gone */ }
  try { view.decoder.configure({ codec: view.codec, optimizeForLatency: true }); } catch (_) { /* gone */ }
  view.waitKey = true;
  post({ cmd: 'voip.keyframe', id: view.id });
}

function onVideo(msg) {
  if (msg.from !== view.id || !view.decoder || view.decoder.state === 'closed') return;
  if (view.waitKey && !msg.key) return;
  if (view.decoder.decodeQueueSize > 8) return resetDecoder();
  view.waitKey = false;
  const data = unb64(msg.data);
  view.bytes += data.length;
  try {
    view.decoder.decode(new EncodedVideoChunk({ type: msg.key ? 'key' : 'delta', timestamp: msg.ts, data }));
  } catch (_) {
    resetDecoder();
  }
}

function renderScreen(v) {
  // Follow what the room says: stop viewing a share that ended.
  const target = v.watching != null ? v.members.find((m) => m.id === v.watching && m.screen) : null;
  if (target && view.id !== target.id) openViewer(target);
  if (!target && view.id != null) closeViewer();
  // If the room dropped our share (we left, it ended), stop capturing.
  if (ss.running && !v.sharing && v.st !== 'live') stopShare();
  hide($('ss-row'), ss.running || !$('ss-box').classList.contains('hide'));
}

document.getElementById('page-voip').addEventListener('click', (e) => {
  const w = e.target.closest('[data-watch]');
  if (!w) return;
  const id = w.dataset.watch === '' ? null : Number(w.dataset.watch);
  post({ cmd: 'voip.watch', id });
});
$('stage-stop').addEventListener('click', () => post({ cmd: 'voip.watch', id: null }));
/* Full screen: the stream fills the window and the window goes borderless
   full screen. Double-click the picture or press the button; Esc or
   double-click again comes back. */
function setTheater(on) {
  document.documentElement.classList.toggle('theater', on);
  $('stage-bar-title').textContent = $('stage-title').textContent;
  post({ cmd: 'window.fullscreen', on });
}
$('stage-full').addEventListener('click', () => setTheater(true));
$('stage-exit').addEventListener('click', () => setTheater(false));
$('stage-view').addEventListener('dblclick', () => setTheater(!document.documentElement.classList.contains('theater')));
window.addEventListener('keydown', (e) => {
  if (e.key === 'Escape' && document.documentElement.classList.contains('theater')) setTheater(false);
});

/* ---------------------------------------------------- right-click menu */

/* Volume and mute are yours alone and remembered by name; the host's mute and
   disconnect apply to everyone. */
function savedVolume(name) {
  try {
    const v = JSON.parse(localStorage.getItem(`kc-vol:${name}`) || 'null');
    return v && typeof v.vol === 'number' ? v : null;
  } catch (_) { return null; }
}

function saveVolume(name, vol, muted) {
  try { localStorage.setItem(`kc-vol:${name}`, JSON.stringify({ vol, muted })); } catch (_) { /* no storage */ }
}

function applySavedVolumes(v) {
  for (const m of v.members) {
    if (m.id === v.me || v.gains[m.id] != null) continue;
    const s = savedVolume(m.name);
    if (s && (s.muted || s.vol !== 1)) post({ cmd: 'voip.volume', id: m.id, gain: s.muted ? 0 : s.vol });
  }
}

const menu = $('ctxmenu');
function closeMenu() { hide(menu, true); }

function openMenu(m, x, y) {
  const v = voipState;
  const you = m.id === v.me;
  const g = v.gains[m.id];
  const s = savedVolume(m.name) || { vol: g == null || g === 0 ? 1 : g, muted: g === 0 };
  const parts = [`<div class="ctx-title"><span class="avatar" style="--h:${hue(m.name)}">${esc(initials(m.name))}</span>${esc(m.name)}</div>`];
  if (!you) {
    parts.push(`
      <div class="ctx-sec">
        <div class="ctx-label"><span>User volume</span><span id="ctx-vol-val">${Math.round(s.vol * 100)}%</span></div>
        <input type="range" class="ctx-range" id="ctx-vol" min="0" max="200" step="5" value="${Math.round(s.vol * 100)}">
      </div>
      <label class="ctx-item"><input type="checkbox" id="ctx-mute" ${s.muted ? 'checked' : ''}> Mute for me</label>`);
  } else {
    parts.push('<div class="ctx-note">That\'s you. Use Mute and Deafen in the voice panel.</div>');
  }
  if (v.isHost && !m.host) {
    parts.push(`
      <div class="ctx-sep"></div>
      <div class="ctx-head">Host</div>
      <button class="ctx-item" data-mod="${m.smuted ? 'unmute' : 'mute'}">${m.smuted ? 'Unmute for everyone' : 'Mute for everyone'}</button>
      <button class="ctx-item danger" data-mod="kick">Disconnect from room</button>`);
  }
  menu.innerHTML = parts.join('');
  menu.dataset.member = m.id;
  menu.dataset.name = m.name;
  hide(menu, false);
  const r = menu.getBoundingClientRect();
  menu.style.left = `${Math.min(x, window.innerWidth - r.width - 8)}px`;
  menu.style.top = `${Math.min(y, window.innerHeight - r.height - 8)}px`;

  const apply = () => {
    const vol = Number($('ctx-vol').value) / 100;
    const muted = $('ctx-mute').checked;
    $('ctx-vol-val').textContent = `${Math.round(vol * 100)}%`;
    saveVolume(m.name, vol, muted);
    post({ cmd: 'voip.volume', id: m.id, gain: muted ? 0 : vol });
  };
  if (!you) {
    $('ctx-vol').addEventListener('input', apply);
    $('ctx-mute').addEventListener('change', apply);
  }
}

menu.addEventListener('click', (e) => {
  const b = e.target.closest('[data-mod]');
  if (!b) return;
  const id = Number(menu.dataset.member);
  if (b.dataset.mod === 'kick' && !confirm(`Disconnect ${menu.dataset.name} from the room?`)) return;
  post({ cmd: 'voip.mod', action: b.dataset.mod, id });
  closeMenu();
});

document.getElementById('page-voip').addEventListener('contextmenu', (e) => {
  const row = e.target.closest('[data-member]');
  if (!row || !voipState) return;
  e.preventDefault();
  const m = voipState.members.find((x) => x.id === Number(row.dataset.member));
  if (m) openMenu(m, e.clientX, e.clientY);
});
document.addEventListener('mousedown', (e) => { if (!menu.contains(e.target)) closeMenu(); });
window.addEventListener('keydown', (e) => { if (e.key === 'Escape') closeMenu(); });
window.addEventListener('blur', closeMenu);
document.querySelectorAll('.page').forEach((p) => p.addEventListener('scroll', closeMenu));

/* ------------------------------------------------------------ host events */

$('share-peers').addEventListener('click', (e) => {
  const pr = e.target.closest('[data-peer-retry]');
  if (pr) post({ cmd: 'peer.retry', id: pr.dataset.peerRetry });
});
$('recv-link').addEventListener('click', (e) => {
  if (e.target.closest('[data-recv-retry]')) post({ cmd: 'recv.retry' });
});

/* ------------------------------------------------------- network permission */

function openFirewall() {
  $('fw-state').textContent = '';
  $('fw-go').disabled = false;
  hide($('fw-modal'), false);
}
$('fw-link').addEventListener('click', openFirewall);
$('fw-close').addEventListener('click', () => hide($('fw-modal'), true));
$('fw-modal').addEventListener('mousedown', (e) => { if (e.target === $('fw-modal')) hide($('fw-modal'), true); });
window.addEventListener('keydown', (e) => { if (e.key === 'Escape') hide($('fw-modal'), true); });
$('fw-go').addEventListener('click', () => {
  $('fw-go').disabled = true;
  $('fw-state').textContent = 'Waiting for Windows…';
  post({ cmd: 'firewall.reset' });
});

/* ----------------------------------------------------------------- mounts */

/* Sharing a folder: choose it, choose read only or read and write (the
   checkbox), start. Opening one: paste the code, then map it as a drive
   (Windows) or mount it (Mac), or browse it here. */
const ICON_DIR = '<svg width="15" height="15" viewBox="0 0 16 16" fill="none"><path d="M1.8 4.2c0-.7.5-1.2 1.2-1.2h3l1.5 1.6H13c.7 0 1.2.5 1.2 1.2v6.4c0 .7-.5 1.2-1.2 1.2H3c-.7 0-1.2-.5-1.2-1.2z" stroke="currentColor" stroke-width="1.4" stroke-linejoin="round"/></svg>';
const ICON_UP = '<svg width="13" height="13" viewBox="0 0 16 16" fill="none"><path d="M8 13.5v-8M4.6 8.7 8 5.3l3.4 3.4M3 2.5h10" stroke="currentColor" stroke-width="1.7" stroke-linecap="round" stroke-linejoin="round"/></svg>';
let lastMountJoins = 0, mountState = { os: BOOT.os, limit: null };

$('mount-pick').addEventListener('click', () => post({ cmd: 'mount.pick' }));
$('mount-write').addEventListener('change', () => {
  $('mount-write-desc').textContent = $('mount-write').checked
    ? 'On: read and write. They can also add, rename, change and delete files and folders in it.'
    : 'Off: read only. They can open and copy files, nothing else.';
});
$('mount-start').addEventListener('click', () => post({ cmd: 'mount.host', write: $('mount-write').checked }));
$('mount-join').addEventListener('click', () => post({ cmd: 'mount.join', code: $('mount-code').value }));
$('mount-code').addEventListener('keydown', (e) => { if (e.key === 'Enter') $('mount-join').click(); });
if (MAC) $('mount-tip').textContent = 'Then mount it (it shows in Finder\'s sidebar), or browse it here.';

function mountHostCard(h) {
  const m = h.mount;
  const tag = m.write ? '<span class="tag-sm rw">Read &amp; write</span>' : '<span class="tag-sm">Read only</span>';
  return `
    <div class="sess${h.st === 'live' ? ' live' : ''}">
      ${sessHead(`<span class="mount-title">${ICON_DIR}<b>${esc(m.name)}</b></span> ${tag}`, 'mount', h.st, `data-stop-host="${h.key}"`)}
      <p class="meta mount-path" title="${esc(m.folder)}">${esc(m.folder)}</p>
      ${h.st === 'starting' ? '<p class="meta">Getting a code from TailCat…</p>' : ''}
      ${h.st === 'live' && h.code ? `
        <div class="sess-code">
          <code class="code">${esc(h.code)}</code>
          <div class="row">
            <button class="btn small primary" data-copytext="${esc(h.code)}" data-code="1">Copy code</button>
            <span class="meta">Send this code to your friends. They paste it in Mounts → Open a shared folder.</span>
          </div>
        </div>` : ''}
      ${h.st === 'live' ? peersHtml(h.peers, 'Nobody has it open yet. Everyone who does shows up here.') : ''}
      ${h.st === 'error' && h.error ? `<p class="err on">${esc(h.error)}</p>` : ''}
      ${sessLog(`h${h.key}`, h.log)}
    </div>`;
}

/* The path parts of the browser's place, as a clickable trail. */
function crumbs(key, path) {
  const parts = path.split('/').filter(Boolean);
  let at = '';
  const links = [`<a class="crumb" data-mount-cd="${key}" data-path="">Top</a>`];
  for (const p of parts) {
    at = at ? `${at}/${p}` : p;
    links.push(`<a class="crumb" data-mount-cd="${key}" data-path="${esc(at)}">${esc(p)}</a>`);
  }
  return links.join('<span class="crumb-sep">/</span>');
}

function browserHtml(j, m) {
  const b = m.browse;
  const key = j.key;
  const join = (name) => (b.path ? `${b.path}/${name}` : name);
  let rows = '';
  if (b.loading && !b.entries.length) rows = '<div class="empty small">Loading…</div>';
  else if (b.error) rows = `<p class="err on">${esc(b.error)}</p>`;
  else if (!b.entries.length) rows = '<div class="empty small">This folder is empty.</div>';
  else {
    rows = b.entries.map((e) => `
      <div class="file">
        <span class="file-ico ${e.dir ? 'dir' : ''}">${e.dir ? ICON_DIR : esc(ext(e.name).slice(0, 4))}</span>
        <div class="file-main">
          ${e.dir
            ? `<a class="file-name link" data-mount-cd="${key}" data-path="${esc(join(e.name))}">${esc(e.name)}</a>`
            : `<span class="file-name">${esc(e.name)}</span>`}
          <span class="file-meta">${e.dir ? 'Folder' : size(e.size)}</span>
        </div>
        <button class="iconbtn" data-mount-get="${key}" data-path="${esc(join(e.name))}" data-dir="${e.dir ? 1 : 0}"
          title="${e.dir ? 'Download this folder and everything in it' : 'Download'}">${ICON_DL}</button>
      </div>`).join('');
  }
  return `
    <div class="browser">
      <div class="browser-head">
        <span class="crumbs">${crumbs(key, b.path)}</span>
        <span class="grow"></span>
        ${m.write ? `<button class="btn small" data-mount-put="${key}" data-path="${esc(b.path)}">${ICON_UP} Upload here</button>` : ''}
        <button class="btn small ghost" data-mount-close="${key}">Close</button>
      </div>
      <div class="filelist remote on">${rows}</div>
    </div>`;
}

function transfersHtml(j, m) {
  if (!m.transfers.length) return '';
  return `<div class="activity">${m.transfers.map((t) => {
    const what = `${t.up ? 'Uploading' : 'Downloading'} <b>${esc(t.name)}</b>`;
    let meta = t.total ? `${size(t.done)} of ${size(t.total)}${t.speed ? ` · ${speed(t.speed)}` : ''}` : (t.done ? size(t.done) : '');
    let btn = `<button class="btn small ghost" data-mount-cancel="${j.key}" data-id="${t.id}">Cancel</button>`;
    if (t.st === 'done') { meta = t.up ? 'Uploaded' : 'Saved'; btn = t.saved ? `<button class="iconbtn" data-mount-reveal="${j.key}" data-id="${t.id}" title="Show in folder">${ICON_FOLDER}</button>` : ''; }
    if (t.st === 'error') { meta = t.error; btn = ''; }
    return `
      <div class="act">
        <div class="act-row"><span>${t.st === 'done' ? (t.up ? 'Uploaded' : 'Downloaded') + ` <b>${esc(t.name)}</b>` : what}</span>
          <span class="meta ${t.st === 'error' ? 'bad' : ''}">${esc(meta)}</span>${btn}</div>
        ${t.st === 'going' ? `<div class="bar-wrap"><div class="bar" style="width:${pct(t.done, t.total)}%"></div></div>` : ''}
      </div>`;
  }).join('')}</div>`;
}

function mountJoinCard(j) {
  const m = j.mount;
  const name = m.name || 'Shared folder';
  const tag = m.name ? (m.write ? '<span class="tag-sm rw">Read &amp; write</span>' : '<span class="tag-sm">Read only</span>') : '';
  let body = '';
  if (j.st === 'live') {
    if (!m.name) {
      body = `<p class="meta">Getting the folder's details…</p>${m.error ? `<p class="warn on">${esc(m.error)}</p>` : ''}`;
    } else {
      const mapLabel = MAC ? 'Mount' : 'Map as drive';
      const actions = m.mounted
        ? `<span class="okline">Open as <b>${esc(m.mounted)}</b></span>
           <button class="btn small primary" data-mount-open="${j.key}">${MAC ? 'Show in Finder' : 'Open in Explorer'}</button>
           <button class="btn small" data-mount-unmap="${j.key}" ${m.busy ? 'disabled' : ''}>${m.busy === 'unmounting' ? 'Unmounting…' : 'Unmount'}</button>`
        : `<button class="btn small primary" data-mount-map="${j.key}" ${m.busy ? 'disabled' : ''}>${m.busy === 'mounting' ? (MAC ? 'Mounting…' : 'Mapping…') : mapLabel}</button>`;
      // Windows' WebDAV only opens files up to 50 MB unless that's raised.
      const lim = mountState.limit;
      const limitNote = mountState.os === 'windows' && lim != null && lim < 1e9
        ? `<p class="meta limit">Windows opens files up to ${Math.round(lim / 1e6)} MB from a drive like this.
             <a class="link" data-big-files="1">Allow big files (up to 4 GB)</a>, or download bigger ones below.</p>`
        : '';
      body = `
        <div class="mount-actions">
          ${actions}
          <button class="btn small" data-mount-browse="${j.key}">${m.browse ? 'Refresh' : 'Browse here'}</button>
        </div>
        ${m.error ? `<p class="warn on">${esc(m.error)}</p>` : ''}
        ${limitNote}
        ${m.browse ? browserHtml(j, m) : ''}
        ${transfersHtml(j, m)}`;
    }
  }
  return `
    <div class="sess${j.st === 'live' ? ' live' : ''}">
      ${sessHead(`<span class="mount-title">${ICON_DIR}<b>${esc(name)}</b></span> ${tag}`, 'join', j.st, `data-stop-join="${j.key}"`, hostHtml(j.hostName, j.link, { label: 'From', who: 'your friend' }, `data-join-retry="${j.key}"`))}
      ${j.st === 'starting' ? '<p class="meta">Reaching your friend through TailCat…</p>' : ''}
      ${j.st === 'unreachable' ? '<p class="warn on">Can\'t reach your friend. Check the code, and that they\'re still sharing the folder. KittyConnect keeps trying.</p>' : ''}
      ${body}
      ${j.st === 'error' && j.error ? `<p class="err on">${esc(j.error)}</p>` : ''}
      ${sessLog(`j${j.key}`, j.log)}
    </div>`;
}

function renderMounts(state, hosts, joins) {
  mountState = state || mountState;
  $('mount-picked').textContent = state.pick || 'No folder chosen';
  $('mount-picked').title = state.pick || '';
  $('mount-picked').classList.toggle('chosen', !!state.pick);
  $('mount-start').disabled = !state.pick;
  $('mount-host-error').textContent = state.hostError;
  show($('mount-host-error'), state.hostError);
  $('mount-join-error').textContent = state.joinError;
  show($('mount-join-error'), state.joinError);
  if (joins.length > lastMountJoins) $('mount-code').value = '';
  lastMountJoins = joins.length;
  const count = (list, word) => (list.length ? `${list.filter((x) => x.st === 'live').length} ${word}` : '');
  $('mount-host-count').textContent = count(hosts, 'sharing');
  $('mount-join-count').textContent = count(joins, 'connected');
  setHtml($('mount-host-list'), hosts.length ? hosts.map(mountHostCard).join('')
    : '<div class="empty">Not sharing a folder. Choose one above and it shows up here with its code.</div>');
  setHtml($('mount-join-list'), joins.length ? joins.map(mountJoinCard).join('')
    : '<div class="empty">No shared folders open. Paste a mount code above to open one.</div>');
}

['mount-host-list', 'mount-join-list'].forEach((id) => $(id).addEventListener('click', (e) => {
  const t = e.target;
  const on = (attr) => t.closest(`[${attr}]`);
  let el;
  if ((el = on('data-mount-map'))) return post({ cmd: 'mount.map', key: Number(el.dataset.mountMap) });
  if ((el = on('data-mount-unmap'))) return post({ cmd: 'mount.unmap', key: Number(el.dataset.mountUnmap) });
  if ((el = on('data-mount-open'))) return post({ cmd: 'mount.open', key: Number(el.dataset.mountOpen) });
  if ((el = on('data-mount-browse'))) return post({ cmd: 'mount.browse', key: Number(el.dataset.mountBrowse), path: null });
  if ((el = on('data-mount-close'))) return post({ cmd: 'mount.browseClose', key: Number(el.dataset.mountClose) });
  if ((el = on('data-mount-cd'))) return post({ cmd: 'mount.browse', key: Number(el.dataset.mountCd), path: el.dataset.path });
  if ((el = on('data-mount-get'))) return post({ cmd: 'mount.get', key: Number(el.dataset.mountGet), path: el.dataset.path, dir: el.dataset.dir === '1' });
  if ((el = on('data-mount-put'))) return post({ cmd: 'mount.put', key: Number(el.dataset.mountPut), path: el.dataset.path });
  if ((el = on('data-mount-cancel'))) return post({ cmd: 'mount.cancel', key: Number(el.dataset.mountCancel), id: Number(el.dataset.id) });
  if ((el = on('data-mount-reveal'))) return post({ cmd: 'mount.reveal', key: Number(el.dataset.mountReveal), id: Number(el.dataset.id) });
  if (on('data-big-files')) return post({ cmd: 'mount.bigFiles' });
  return sessClick(e);
}));

/* ------------------------------------------------------------------- name */

/* Asked every time the app opens. The input starts empty with a random name
   as its grey placeholder: OK on its own takes the suggestion, and typing
   replaces it. It's used everywhere: rooms, and the hosts of ports and file
   shares you join see it next to how you're connected. */
const NAME_A = ['Sleepy', 'Fuzzy', 'Sneaky', 'Cosy', 'Zippy', 'Mellow', 'Brave', 'Lucky', 'Jolly', 'Sunny',
  'Quiet', 'Swift', 'Bouncy', 'Dapper', 'Cheeky', 'Misty', 'Velvet', 'Tiny', 'Clever', 'Breezy'];
const NAME_B = ['Kitten', 'Tabby', 'Panther', 'Lynx', 'Ocelot', 'Bobcat', 'Tiger', 'Puma', 'Cheetah', 'Jaguar',
  'Otter', 'Fox', 'Badger', 'Panda', 'Falcon', 'Owl', 'Hedgehog', 'Raccoon', 'Koala', 'Wombat'];
let myName = '';
let nameRequired = true;

function randomName() {
  const pick = (a) => a[Math.floor(Math.random() * a.length)];
  return `${pick(NAME_A)} ${pick(NAME_B)}`;
}

function openName(required) {
  nameRequired = required;
  const input = $('name-input');
  input.value = required ? '' : myName;
  input.placeholder = randomName();
  hide($('name-modal'), false);
  setTimeout(() => { input.focus(); input.select(); }, 30);
}

function setMe(name) {
  myName = name;
  $('me-name').textContent = name;
  $('me-avatar').textContent = initials(name);
  $('me-avatar').style.setProperty('--h', hue(name));
  hide($('me-chip'), !name);
}

$('name-form').addEventListener('submit', (e) => {
  e.preventDefault();
  const input = $('name-input');
  const name = (input.value.trim() || input.placeholder).slice(0, 32);
  post({ cmd: 'name.set', name });
  setMe(name);
  hide($('name-modal'), true);
});
$('name-shuffle').addEventListener('click', () => {
  $('name-input').value = '';
  $('name-input').placeholder = randomName();
  $('name-input').focus();
});
$('me-chip').addEventListener('click', () => openName(false));
window.addEventListener('keydown', (e) => {
  if (e.key === 'Escape' && !nameRequired) hide($('name-modal'), true);
});
hide($('me-chip'), true);
openName(true);

window.onHost = (msg) => {
  switch (msg.type) {
    case 'firewall':
      $('fw-go').disabled = false;
      $('fw-state').textContent = msg.ok
        ? 'Done. Reconnect, and press Allow when Windows asks.'
        : 'Not changed. See the message at the bottom.';
      break;
    case 'state': {
      renderShare(msg.share);
      renderRecv(msg.recv);
      renderHosts(msg.hosts, msg.hostError);
      renderJoins(msg.joins, msg.joinError);
      renderPfCounts(msg.hosts, msg.joins);
      renderMounts(msg.mounts, msg.hosts.filter((h) => h.mount), msg.joins.filter((j) => j.mount));
      renderVoip(msg.voip);
      if (msg.name && msg.name !== myName) setMe(msg.name);
      $('statusText').textContent = msg.status.text;
      $('lamp').className = msg.status.kind;
      const filesLive = msg.share.st === 'live' || msg.recv.st === 'live';
      const live = (x) => x.st === 'live';
      const portsLive = msg.hosts.some((h) => !h.mount && live(h)) || msg.joins.some((j) => !j.mount && live(j));
      const mountsLive = msg.hosts.some((h) => h.mount && live(h)) || msg.joins.some((j) => j.mount && live(j));
      const voipLiveNow = msg.voip.st === 'live';
      show($('files-badge'), filesLive);
      show($('ports-badge'), portsLive);
      show($('voip-badge'), voipLiveNow);
      show($('mounts-badge'), mountsLive);
      document.documentElement.classList.toggle('live', filesLive || portsLive || voipLiveNow || mountsLive);
      break;
    }
    case 'voipLive':
      voipLive(msg);
      break;
    case 'video':
      onVideo(msg);
      break;
    case 'keyframe':
      ss.forceKey = true;
      break;
    case 'videoRate':
      // Watchers' connections decide the bitrate (see media.rs RateControl).
      if (ss.encoder && ss.encoder.state === 'configured' && ss.config) {
        ss.config = { ...ss.config, bitrate: msg.bitrate };
        ss.rate = msg.bitrate;
        try { ss.encoder.configure(ss.config); } catch (_) { /* keeps the old rate */ }
      }
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

// Keyboard: Ctrl+1 / 2 / 3 (⌘ on a Mac) switch tabs.
window.addEventListener('keydown', (e) => {
  if (!(MAC ? e.metaKey : e.ctrlKey)) return;
  if (e.key === '1') page('files');
  if (e.key === '2') page('ports');
  if (e.key === '3') page('voip');
});
if (MAC) $('data-link').textContent = 'Portable · KittyConnectData';
document.addEventListener('contextmenu', (e) => { if (!e.target.closest('input, .code, .log-body, .err')) e.preventDefault(); });

/* ------------------------------------------------------ browser preview */

/* Opened as a plain page (no host), show a sample state so the layout can be
   checked in a browser. Never runs inside the app. */
function demo() {}

if (window.ipc) {
  post({ cmd: 'ready', origin: `${location.origin} secure=${window.isSecureContext} encoder=${!!window.VideoEncoder} capture=${!!(navigator.mediaDevices && navigator.mediaDevices.getDisplayMedia)}` });
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
      peers: [
        { id: '1', name: 'Alex Rivera', path: 'direct', rtt: 24, checkedAgo: 8 },
        { id: '2', name: 'Jo', path: 'relay', rtt: 91, checkedAgo: 20 },
      ],
      log: ['# Selected bootstrap relay region 302, San Francisco', `# 🐈 Server listening with new address: ${addr}`],
    },
    recv: {
      st: 'live', error: '', inbox: 'C:\\Users\\you\\Downloads', hostName: 'Sam',
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
      code: `kitty:voip:50211:${addr}`, inVoice: true, muted: false, deaf: false, voiceError: '', media: 'ok',
      devices: 'Headset Microphone · Headset Earphone', speaking: [2],
      link: { path: 'unknown' },
      gen: 1, isHost: true, roomNotice: '', sharing: null, watching: null,
      uploads: [{ tid: 4, name: 'clip.mp4', sent: 31457280, size: 52428800, error: null }],
      downloads: { 7: { st: 'downloading', got: 5242880, size: 12582912, error: '' } },
      previews: [], gains: { 3: 0.6 },
      members: [
        { id: 1, name: 'Sam', host: true, voice: true, muted: false, deaf: false, smuted: false, path: 'host', rtt: null, screen: null },
        { id: 2, name: 'Alex Rivera', host: false, voice: true, muted: false, deaf: false, smuted: false, path: 'direct', rtt: 24, screen: { w: 1920, h: 1080, fps: 30, codec: 'avc1.640028' } },
        { id: 3, name: 'Jo', host: false, voice: true, muted: false, deaf: false, smuted: true, path: 'relay', rtt: 88, screen: null },
      ],
      chat: [
        { system: true, text: 'Alex Rivera joined the room', ts: 1794443000 },
        { from: 2, name: 'Alex Rivera', text: 'yo, server is up?', ts: 1794443030 },
        { from: 1, name: 'Sam', text: 'yep, here are the mods', ts: 1794443060, file: { aid: 7, name: 'modpack-1.2.zip', size: 12582912, image: false } },
        { system: true, text: 'Alex Rivera started sharing their screen', ts: 1794443075 },
        { system: true, text: 'Sam muted Jo for everyone', ts: 1794443090 },
      ],
      log: [],
    },
    hostError: '', joinError: '',
    mounts: { pick: 'C:\\Users\\you\\Documents\\Minecraft Worlds', hostError: '', joinError: '', limit: 50000000, os: 'windows' },
    hosts: [
      { key: 11, st: 'live', error: '', warn: '', ports: [50123], game: null, code: `kitty:mount:50123:${addr.slice(0, 60)}`, lanNote: '', log: [],
        mount: { folder: 'C:\\Users\\you\\Pictures\\Trip 2026', name: 'Trip 2026', write: false },
        peers: [{ id: '9', name: 'Alex Rivera', path: 'direct', rtt: 21, checkedAgo: 4 }] },
      { key: 9, st: 'live', error: '', warn: '', ports: [25565], game: 'minecraft', code: `kitty:game.minecraft:25565:${addr.slice(0, 60)}`,
        lanNote: 'Friends who join see \u201cSam - Survival Island\u201d under LAN in Multiplayer.', log: [],
        peers: [{ id: '7', name: 'Alex Rivera', path: 'direct', rtt: 22, checkedAgo: 3 }, { id: '8', name: 'Jo', path: 'relay', rtt: 95, checkedAgo: 9 }] },
      { key: 3, st: 'live', error: '', warn: '', ports: [8080], code: addr.slice(0, 70), peers: [], log: [`# 🐈 Server listening with new address: ${addr.slice(0, 70)}`] },
      { key: 2, st: 'live', error: '', warn: 'Nothing is running on port 25565 on this PC yet. Start your server; your friend can connect once it\'s up.', ports: [25565], code: addr, log: [],
        peers: [
          { id: '1', name: 'Alex Rivera', path: 'direct', rtt: 24, checkedAgo: 4 },
          { id: '2', name: 'Jo', path: 'relay', rtt: 88, checkedAgo: 12, retrying: true },
          { id: '3', name: 'Mellow Otter', path: 'direct', rtt: 31, checkedAgo: 2 },
          { id: '4', name: 'Priya', path: 'relay', rtt: 120, checkedAgo: 25 },
          { id: '5', name: 'Kit', path: 'unknown', rtt: null, checkedAgo: 1 },
        ] },
    ],
    joins: [
      { key: 12, st: 'live', error: '', lan: false, hostName: 'Priya', game: null, worlds: [], maps: [{ local: 51999, remote: 50200 }],
        link: { path: 'direct', rtt: 19, checkedAgo: 6 }, log: [],
        mount: { name: 'Shared Projects', write: true, mounted: 'Z:', busy: '', error: '',
          browse: { path: 'Designs/2026', loading: false, error: '', entries: [
            { name: 'Drafts', dir: true, size: 0, modified: 0 },
            { name: 'logo-final.png', dir: false, size: 482133, modified: 0 },
            { name: 'brief.docx', dir: false, size: 38912, modified: 0 },
            { name: 'promo.mp4', dir: false, size: 214748364, modified: 0 },
          ] },
          transfers: [
            { id: 2, name: 'promo.mp4', up: false, done: 90000000, total: 214748364, st: 'going', error: '', speed: 6815744, saved: false },
            { id: 1, name: 'notes.txt', up: true, done: 2048, total: 2048, st: 'done', error: '', speed: 0, saved: false },
          ] } },
      { key: 13, st: 'live', error: '', lan: false, hostName: 'Sam', game: null, worlds: [], maps: [{ local: 52000, remote: 50300 }],
        link: { path: 'relay', rtt: 80, checkedAgo: 6 }, log: [],
        mount: { name: 'Trip Photos', write: false, mounted: null, busy: '', error: '', browse: null, transfers: [] } },
      { key: 6, st: 'live', error: '', lan: false, hostName: 'Priya', game: 'minecraft', worlds: [{ port: 25565, motd: 'Priya - Skyblock' }],
        maps: [{ local: 25565, remote: 25565 }], link: { path: 'direct', rtt: 18, checkedAgo: 5 }, log: [] },
      { key: 5, st: 'live', error: '', lan: false, hostName: 'Kit', game: 'terraria', worlds: [],
        maps: [{ local: 7777, remote: 7777 }], link: { path: 'relay', rtt: 70, checkedAgo: 5 }, log: [] },
      { key: 4, st: 'live', error: '', lan: false, hostName: 'Sam', maps: [{ local: 7777, remote: 7777 }], link: { path: 'relay', rtt: 91, checkedAgo: 40 }, log: [] },
      { key: 7, st: 'unreachable', error: '', lan: false, hostName: null, maps: [{ local: 12345, remote: 12345 }], link: { path: 'unknown' }, log: ['# forwarding 127.0.0.1:12345 -> remote localhost:12345'] },
    ],
  });
  voipLive({ speaking: [2], me: 1, level: -24, talking: true });
}
