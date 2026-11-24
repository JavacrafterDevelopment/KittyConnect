/* Portside page. Commands go to the host through window.ipc; the host answers
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
   positions survive the frequent progress snapshots.
   The "checked 12s ago" times move by a second between snapshots, so they
   are left out of the comparison and patched in place instead; otherwise
   every peer row was rebuilt several times a second, losing hover and the
   odd click, and the row jumped as the text changed. */
const lastHtml = new Map();
const AGO_RE = / data-at="(-?\d+)">[^<]*</g;
function setHtml(el, html) {
  const key = html.replace(AGO_RE, '><');
  if (lastHtml.get(el) === key) {
    const ats = [...html.matchAll(AGO_RE)].map((m) => m[1]);
    el.querySelectorAll('[data-at]').forEach((n, i) => {
      if (ats[i] == null || n.dataset.at === ats[i]) return;
      n.dataset.at = ats[i];
      const t = ago(Number(ats[i]));
      if (n.textContent !== t) n.textContent = t;
    });
    return;
  }
  lastHtml.set(el, key);
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
  const rtt = link.rtt != null ? ` · <span class="rtt">${link.rtt} ms</span>` : '';
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
        : `<button class="btn small" data-peer-retry="${esc(p.id)}" title="Ask ${esc(p.name)}'s Portside to reconnect and try for a direct connection. Whatever they're doing through it drops for a moment.">Retry direct</button>`;
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
   into italics and drop them, breaking the code. Portside ignores the
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

/* ------------------------------------------------------------------- hub */

/* The hub: who's in it, what you can invite them to, invites you got, saved
   hubs, and every connection in the app. The codes behind invites are made by
   the app itself; the page only says which thing and to whom. */
const HUB_OFF = {
  st: 'off', mode: 'off', error: '', notice: '', name: '', id: '', kind: 'temporary', expiresIn: null, savedHere: false,
  canSave: false, making: null, code: null, me: 0, members: [], invites: [], offers: [], openToHub: false, sent: '',
  reconnecting: false, link: { path: 'unknown' }, log: [], saved: [], prompt: null,
};
const HUB_PILL = { starting: ['busy', 'Connecting'], live: ['ok', 'Connected'] };
const KIND_LABEL = { voip: 'Voice room', file: 'File share', port: 'Ports', game: 'Game server', mount: 'Folder' };
const KIND_A = { voip: 'a voice room', file: 'a file share', port: 'some ports', game: 'a game server', mount: 'a shared folder' };
let hubState = HUB_OFF;
/* People ticked to invite (by hub id); none ticked means everyone. */
const hubPicked = new Set();
/* Invites already shown as a toast, so each pops up once. */
const toasted = new Set();
let toastInvite = null;
let lastInvitables = [];

/* "3 days 4 h", "2 weeks 1 day", "45 min". */
function timeLeft(secs) {
  const w = Math.floor(secs / 604800), d = Math.floor((secs % 604800) / 86400), h = Math.floor((secs % 86400) / 3600);
  const parts = [];
  if (w) parts.push(`${w} week${w === 1 ? '' : 's'}`);
  if (d) parts.push(`${d} day${d === 1 ? '' : 's'}`);
  if (h && !w) parts.push(`${h} h`);
  if (!parts.length) parts.push(`${Math.max(1, Math.round(secs / 60))} min`);
  return parts.join(' ');
}

function when(secs) {
  return new Date(Date.now() + secs * 1000).toLocaleString(undefined,
    { weekday: 'short', month: 'short', day: 'numeric', hour: 'numeric', minute: '2-digit' });
}

/* ----------------------------------------------- start: name, kind, wheels */

let hubKind = 'temporary';
document.querySelectorAll('#hub-kind .seg-btn').forEach((b) => b.addEventListener('click', () => setHubKind(b.dataset.kind)));

function setHubKind(kind) {
  hubKind = kind;
  document.querySelectorAll('#hub-kind .seg-btn').forEach((b) => {
    b.classList.toggle('on', b.dataset.kind === kind);
    b.setAttribute('aria-checked', String(b.dataset.kind === kind));
  });
  hide($('hub-save-panel'), kind !== 'saved');
  $('hub-kind-desc').textContent = kind === 'saved'
    ? 'Kept on this PC with the same code until it expires. Open it again any time; anyone in it who saves it can open it too.'
    : 'A new code every time. Closing it deletes it.';
  if (kind === 'saved') requestAnimationFrame(() => WHEELS.forEach((w) => setWheel(w, wheelVals[w.dataset.wheel], false)));
}

/* Weeks, days and hours, each a column you scroll or click, like a clock's
   timer. Up to 13 weeks, at least an hour. */
const ITEM_H = 34;
const WHEELS = [...document.querySelectorAll('#hub-wheels .wheel')];
const wheelVals = { weeks: 1, days: 0, hours: 0 };

function wheelValue(el) {
  return Math.max(0, Math.min(Number(el.dataset.max), Math.round(el.scrollTop / ITEM_H)));
}

function markWheel(el) {
  const v = wheelValue(el);
  el.querySelectorAll('.wheel-item').forEach((i) => i.classList.toggle('on', Number(i.dataset.v) === v));
}

function setWheel(el, v, smooth) {
  wheelVals[el.dataset.wheel] = v;
  el.scrollTo({ top: v * ITEM_H, behavior: smooth ? 'smooth' : 'auto' });
  markWheel(el);
  updateUntil();
}

function wheel(name) {
  return WHEELS.find((w) => w.dataset.wheel === name);
}

/* Once a column stops: 13 weeks is the most, an hour the least. */
function settleWheels() {
  WHEELS.forEach((w) => { wheelVals[w.dataset.wheel] = wheelValue(w); });
  if (wheelVals.weeks === 13 && (wheelVals.days || wheelVals.hours)) {
    setWheel(wheel('days'), 0, true);
    setWheel(wheel('hours'), 0, true);
  }
  if (!wheelVals.weeks && !wheelVals.days && !wheelVals.hours) setWheel(wheel('hours'), 1, true);
  updateUntil();
}

function hubSecs() {
  return wheelVals.weeks * 604800 + wheelVals.days * 86400 + wheelVals.hours * 3600;
}

function updateUntil() {
  const secs = Math.min(hubSecs(), 13 * 604800);
  $('hub-until').textContent = `Kept until ${when(secs)}. Up to 13 weeks.`;
}

WHEELS.forEach((el) => {
  const max = Number(el.dataset.max);
  el.innerHTML = '<div class="wheel-pad"></div>'
    + Array.from({ length: max + 1 }, (_, i) => `<div class="wheel-item" data-v="${i}">${i}</div>`).join('')
    + '<div class="wheel-pad"></div>';
  let t = 0;
  el.addEventListener('scroll', () => {
    markWheel(el);
    clearTimeout(t);
    t = setTimeout(settleWheels, 120);
  });
  el.addEventListener('click', (e) => {
    const it = e.target.closest('.wheel-item');
    if (it) setWheel(el, Number(it.dataset.v), true);
  });
  el.addEventListener('keydown', (e) => {
    const step = { ArrowUp: -1, ArrowDown: 1 }[e.key];
    if (!step) return;
    e.preventDefault();
    setWheel(el, Math.max(0, Math.min(max, wheelValue(el) + step)), true);
    setTimeout(settleWheels, 250);
  });
});
updateUntil();

$('hub-host').addEventListener('click', () => {
  post({ cmd: 'hub.host', name: $('hub-name').value, kind: hubKind, secs: hubKind === 'saved' ? hubSecs() : 0 });
});
$('hub-join').addEventListener('click', () => post({ cmd: 'hub.join', code: $('hub-code').value }));
$('hub-code').addEventListener('keydown', (e) => { if (e.key === 'Enter') $('hub-join').click(); });

/* Leave (joined) or delete (a temporary hub of ours: closing it deletes it). */
$('hub-leave').addEventListener('click', () => {
  if (hubState.mode === 'host' && hubState.members.length > 1
    && !confirm('Delete your hub? Everyone in it is disconnected, and its code stops working.')) return;
  post({ cmd: 'hub.leave' });
});
$('hub-open-to-hub').addEventListener('change', (e) => post({ cmd: 'hub.openToHub', on: e.target.checked }));

/* ------------------------------------------------------------- delete */

let deleting = null;
function openDelete(id, name, here, mine) {
  deleting = id;
  $('hub-del-title').textContent = `Delete ${name || 'this hub'}?`;
  $('hub-del-text').textContent = (mine
    ? 'Delete for me removes it from this PC, its key too, and your code from everyone\'s address book. '
    : '')
    + (here
      ? 'Delete for everyone also deletes it for everyone in it right now. Anyone not in it keeps theirs until it expires.'
      : 'To delete it for everyone, open or join it first: it reaches everyone in it.');
  hide($('hub-del-me'), !mine);
  $('hub-del-all').disabled = !here;
  hide($('hub-del-modal'), false);
}
function closeDelete() {
  deleting = null;
  hide($('hub-del-modal'), true);
}
$('hub-del-me').addEventListener('click', () => { post({ cmd: 'hub.delete', id: deleting, everyone: false }); closeDelete(); });
$('hub-del-all').addEventListener('click', () => {
  if (!confirm('Delete it for everyone in it now? This can\'t be undone.')) return;
  post({ cmd: 'hub.delete', id: deleting, everyone: true });
  closeDelete();
});
$('hub-del-cancel').addEventListener('click', closeDelete);
$('hub-del-modal').addEventListener('click', (e) => { if (e.target === $('hub-del-modal')) closeDelete(); });

/* ------------------------------------------------------------- members */

function pathLabel(p) {
  if (p.path === 'host') return '<span class="meta">Host</span>';
  return linkPill({ path: p.path, rtt: p.rtt }, p.name) || '<span class="meta">Connecting…</span>';
}

function hubMembersHtml(h) {
  if (!h.members.length) return '<div class="peers-empty">Connecting…</div>';
  return h.members.map((m) => {
    const me = m.id === h.me;
    const kick = h.mode === 'host' && !me && !m.host
      ? `<button class="btn small ghost" data-hub-kick="${m.id}" title="Disconnect ${esc(m.name)} from the hub. They could join again with the code.">Remove</button>`
      : '';
    return `
      <div class="person">
        <span class="avatar" style="--h:${hue(m.name)}">${esc(initials(m.name))}</span>
        <span class="person-name">${esc(m.name)}${me ? ' <span class="meta">(you)</span>' : ''}${m.host ? ' <span class="meta">· host</span>' : ''}</span>
        <span class="grow"></span>
        ${kick}${me && h.mode === 'host' ? '' : pathLabel(m)}
      </div>`;
  }).join('');
}

/* Who an invite goes to: ticked people, or everyone. */
function hubPicksHtml(h) {
  const others = h.members.filter((m) => m.id !== h.me);
  if (!others.length) return '<div class="peers-empty">Nobody else is here yet.</div>';
  return `<div class="picks">${others.map((m) => `
    <label class="pick-chip${hubPicked.has(m.id) ? ' on' : ''}">
      <input type="checkbox" data-hub-pick="${m.id}"${hubPicked.has(m.id) ? ' checked' : ''}>
      <span class="avatar small" style="--h:${hue(m.name)}">${esc(initials(m.name))}</span>${esc(m.name)}
    </label>`).join('')}</div>
    <p class="set-desc">${hubPicked.size ? 'Only the people ticked get invites.' : 'Nobody ticked: invites go to everyone.'}</p>`;
}

function hubInvitablesHtml(h, items) {
  const others = h.members.filter((m) => m.id !== h.me);
  if (!items.length) {
    return '<div class="peers-empty">Open something first: a voice room, a file share, a port, a game server or a folder. It shows up here to invite people to.</div>';
  }
  const picked = others.filter((m) => hubPicked.has(m.id)).length;
  const label = picked ? `Invite ${picked}` : 'Invite everyone';
  return items.map((i) => `
    <div class="invitable">
      <span class="kind kind-${i.kind}">${esc(KIND_LABEL[i.kind] || i.kind)}</span>
      <span class="invitable-title">${esc(i.title)}</span>
      <span class="grow"></span>
      <button class="btn small primary" data-hub-invite="${esc(i.what)}"${others.length ? '' : ' disabled title="Nobody else is in the hub yet"'}>${label}</button>
    </div>`).join('');
}

/* What others have open ("Open to hub"), to join with one press. */
function offersHtml(list) {
  return list.map((o) => `
    <div class="sess invite">
      <div class="sess-head">
        <span class="avatar small" style="--h:${hue(o.name)}">${esc(initials(o.name))}</span>
        <span class="kind kind-${o.kind}">${esc(KIND_LABEL[o.kind] || o.kind)}</span>
        <span class="sess-title"><b>${esc(o.name)}</b>${o.title ? `: ${esc(o.title)}` : ''}</span>
        <span class="grow"></span>
        <button class="btn small primary" data-offer-from="${o.from}" data-offer-oid="${esc(o.oid)}" data-offer-kind="${o.kind}">Join</button>
      </div>
    </div>`).join('');
}

function inviteText(inv) {
  return `<b>${esc(inv.from)}</b> invited you to ${esc(KIND_A[inv.kind] || 'something')}${inv.title ? `: “${esc(inv.title)}”` : ''}`;
}

function invitesHtml(list) {
  if (!list.length) return '<div class="empty">No invites. When someone in your hub invites you, it shows up here (and pops up on any tab).</div>';
  return list.map((inv) => `
    <div class="sess invite">
      <div class="sess-head">
        <span class="avatar small" style="--h:${hue(inv.from)}">${esc(initials(inv.from))}</span>
        <span class="sess-title">${inviteText(inv)}</span>
        <span class="grow"></span>
        <button class="btn small primary" data-inv-join="${inv.id}">Join</button>
        <button class="btn small ghost" data-inv-dismiss="${inv.id}">Dismiss</button>
      </div>
    </div>`).join('');
}

/* ---------------------------------------------------------- saved hubs */

function hitsHtml(hits) {
  return hits.map((x) => `
    <div class="person">
      <span class="avatar" style="--h:${hue(x.host)}">${esc(initials(x.host))}</span>
      <span class="person-name"><b>${esc(x.host)}</b> has it open <span class="meta">· ${x.members} ${x.members === 1 ? 'person' : 'people'}</span></span>
      <span class="grow"></span>
      <button class="btn small primary" data-scan-join="${esc(x.code)}">Join</button>
    </div>`).join('');
}

/* Which saved hub's ⋯ menu is open. */
let tileMenu = null;

/* A saved hub's tile. The one you're in holds everything about it: whether
   it's open, its code, who's in it with their ping, and Close (or Leave).
   Delete is behind ⋯, so it isn't pressed by accident. */
function savedTile(s, h) {
  const sc = s.scan;
  const here = s.here && h.mode !== 'off';
  const prompt = h.prompt && h.prompt.id === s.id ? h.prompt : null;
  const busy = sc && sc.running;
  const updated = s.updatedAgo != null ? ` · book updated ${s.updatedAgo < 120 ? 'just now' : `${timeLeft(s.updatedAgo)} ago`}` : '';
  const meta = `<span class="meta">${timeLeft(s.expiresIn)} left · ${s.people} saved it${updated}</span>`;
  const more = `
    <div class="tile-more">
      <button class="btn small ghost more-btn" data-saved-more="${s.id}" title="More" aria-label="More">⋯</button>
      ${tileMenu === s.id ? `<div class="tile-menu"><button class="menu-item danger" data-saved-del="${s.id}" data-saved-name="${esc(s.name)}" data-saved-here="${here ? 1 : ''}">Delete…</button></div>` : ''}
    </div>`;
  let head, body = '';
  if (here) {
    const host = h.mode === 'host';
    const [cls, text] = h.reconnecting ? ['busy', 'Reconnecting'] : h.st === 'live' ? ['ok', host ? 'Hosting' : 'Connected'] : ['busy', 'Connecting'];
    head = `
      <span class="sess-title">${esc(s.name)}</span>
      <span class="pill ${cls}"><span class="dot"></span><span class="pill-text">${text}</span></span>
      ${!host && h.st === 'live' ? linkPill(h.link, 'whoever has it open') : ''}
      ${meta}
      <span class="grow"></span>
      <button class="btn small danger" data-tile-close="${host ? 'close' : 'leave'}">${host ? 'Close' : 'Leave'}</button>
      ${more}`;
    body = (host && h.code ? `
      <div class="sess-code">
        <code class="code">${esc(h.code)}</code>
        <div class="row">
          <button class="btn small primary" data-copytext="${esc(h.code)}" data-code="1">Copy code</button>
          <span class="meta">Friends join with this code. It's the same every time this hub is opened by you.</span>
        </div>
      </div>` : '')
      + `<div class="peers"><div class="peers-head"><span class="field-label">In the hub</span><span class="meta">${h.members.length || ''}</span></div>${hubMembersHtml(h)}</div>`;
  } else {
    head = `
      <span class="sess-title">${esc(s.name)}</span>
      ${meta}
      <span class="grow"></span>
      <button class="btn small primary" data-saved-open="${s.id}"${busy ? ' disabled' : ''}>Open</button>
      <button class="btn small" data-saved-scan="${s.id}"${busy ? ' disabled' : ''}>Scan</button>
      ${more}`;
    // Whoever is found shows at once, while the rest are still looked at.
    const still = busy ? `<p class="meta">Still checking ${sc.total - sc.checked} more…</p>` : '';
    if (prompt) {
      // Open found it already online: join them (your own copy only opens
      // when nobody has it open; two open copies would join up anyway).
      body = `<p class="okline">It's already online. Join them:</p>${hitsHtml(prompt.hits)}${still}
        <div class="row"><button class="btn small ghost" data-prompt-cancel="1">Cancel</button></div>`;
    } else if (sc && sc.opening) {
      body = `<p class="okline">Please wait while it looks for anyone hosting it. Checking to see if this hub is already online… This can take some time. (${sc.checked} of ${sc.total})</p>`;
    } else if (sc && sc.running) {
      body = sc.hits.length ? hitsHtml(sc.hits) + still
        : `<p class="meta">Looking for whoever has it open… This can take some time. (${sc.checked} of ${sc.total})</p>`;
    } else if (sc) {
      body = sc.hits.length ? hitsHtml(sc.hits)
        : `<p class="meta">${sc.total
          ? 'Nobody has it open right now. Press Open to open it: anyone who opens it after you finds you, and if someone opens a copy of their own at the same time, the two join up by themselves.'
          : 'Nobody else has saved it yet.'}</p>`;
    }
    if (sc && sc.learned && !sc.opening) {
      body += `<p class="okline">Its address book caught up: ${sc.learned} new ${sc.learned === 1 ? 'person' : 'people'}.</p>`;
    }
  }
  return `<div class="sess saved-tile${here ? ' live' : ''}"><div class="sess-head">${head}</div>${body}</div>`;
}

function savedHtml(h) {
  return h.saved.map((s) => savedTile(s, h)).join('');
}

document.addEventListener('click', (e) => {
  if (tileMenu && !e.target.closest('.tile-more')) {
    tileMenu = null;
    setHtml($('hub-saved'), savedHtml(hubState));
  }
});

/* Leaving the hub you're in for another one: say so first. */
function leaveFirst() {
  return hubState.mode === 'off' || confirm('Leave the hub you\'re in?');
}

/* ------------------------------------------------------------- render */

function renderHub(h, items) {
  hubState = h;
  const on = h.mode !== 'off';
  const saved = h.kind === 'saved';
  const live = on && h.st === 'live';
  hide($('hub-lobby'), on);
  // A temporary hub has its own bar; a saved one lives in its tile.
  hide($('hub-room'), !on || saved);
  hide($('hub-share'), !live);
  $('hub-error').textContent = h.error;
  show($('hub-error'), h.error);
  $('hub-notice').textContent = h.notice;
  show($('hub-notice'), h.notice && !on);
  $('hub-name').placeholder = myName ? `${myName}'s hub` : 'Hub name';
  const making = h.making === 'create';
  $('hub-host').disabled = making;
  $('hub-host').textContent = making ? 'Making its key…' : 'Start hub';
  $('hub-host-meta').textContent = making ? 'A few seconds: it measures the relays once.' : 'You get a code to send your friends.';
  const ids = new Set(h.members.map((m) => m.id));
  [...hubPicked].forEach((id) => { if (!ids.has(id)) hubPicked.delete(id); });
  if (on && !saved) {
    const host = h.mode === 'host';
    $('hub-title').textContent = h.name || 'Hub';
    const [cls, text] = h.reconnecting ? ['busy', 'Reconnecting'] : (HUB_PILL[h.st] || HUB_PILL.starting);
    $('hub-pill').className = `pill ${cls}`;
    $('hub-pill').querySelector('.pill-text').textContent = text;
    $('hub-kind-badge').textContent = 'Temporary';
    setHtml($('hub-link'), h.mode === 'join' && h.st === 'live' ? linkPill(h.link, 'the hub host') : '');
    $('hub-leave').textContent = host ? 'Delete hub' : 'Leave hub';
    $('hub-room-code').textContent = h.code || '';
    $('hub-codebox').classList.toggle('on', !!h.code && host);
    setHtml($('hub-members'), hubMembersHtml(h));
    $('hub-count').textContent = h.members.length ? `${h.members.length} in it` : '';
  }
  if (live) {
    setHtml($('hub-picks'), hubPicksHtml(h));
    setHtml($('hub-invitables'), hubInvitablesHtml(h, items));
    $('hub-sent').textContent = h.sent;
    $('hub-open-to-hub').checked = h.openToHub;
  }
  if (!on) hubPicked.clear();
  const offers = on ? h.offers : [];
  hide($('hub-offers-section'), !offers.length);
  $('hub-offer-count').textContent = offers.length ? String(offers.length) : '';
  setHtml($('hub-offers'), offersHtml(offers));
  $('hub-invite-count').textContent = h.invites.length ? String(h.invites.length) : '';
  setHtml($('hub-invites'), invitesHtml(h.invites));
  if (tileMenu && !h.saved.some((s) => s.id === tileMenu)) tileMenu = null;
  hide($('hub-saved-section'), !h.saved.length);
  $('hub-saved-count').textContent = h.saved.length ? String(h.saved.length) : '';
  setHtml($('hub-saved'), savedHtml(h));
  renderLog('hub', h.log);

  // The oldest invite not answered yet waits in the corner until it is.
  const pending = h.invites.length ? h.invites[h.invites.length - 1] : null;
  if (!pending) hideToast();
  else if (!toastInvite || toastInvite.id !== pending.id) showToast(pending);
}

/* Stays until it's answered: Join, or No. */
function showToast(inv) {
  toastInvite = inv;
  $('toast-avatar').style.setProperty('--h', hue(inv.from));
  $('toast-avatar').textContent = initials(inv.from);
  $('toast-text').innerHTML = inviteText(inv);
  hide($('invite-toast'), false);
}

function hideToast() {
  toastInvite = null;
  hide($('invite-toast'), true);
}

/* Joining a room leaves the room you're in: say so first. */
function okToJoin(kind) {
  return !(kind === 'voip' && voipState && voipState.st !== 'off' && voipState.st !== 'ended'
    && !confirm('Leave the room you\'re in to join this one?'));
}

function joinInvite(id) {
  const inv = hubState.invites.find((i) => i.id === id);
  if (!inv || !okToJoin(inv.kind)) return;
  hideToast();
  post({ cmd: 'hub.accept', id });
}

$('toast-join').addEventListener('click', () => { if (toastInvite) joinInvite(toastInvite.id); });
$('toast-dismiss').addEventListener('click', () => {
  if (toastInvite) post({ cmd: 'hub.dismiss', id: toastInvite.id });
  hideToast();
});

$('hub-room').addEventListener('click', (e) => {
  const inv = e.target.closest('[data-hub-invite]');
  if (inv) {
    const to = hubState.members.filter((m) => m.id !== hubState.me && hubPicked.has(m.id)).map((m) => m.id);
    return post({ cmd: 'hub.invite', what: inv.dataset.hubInvite, to });
  }
  const kick = e.target.closest('[data-hub-kick]');
  if (kick) {
    const m = hubState.members.find((x) => x.id === Number(kick.dataset.hubKick));
    if (m && confirm(`Remove ${m.name} from the hub?`)) post({ cmd: 'hub.kick', id: m.id });
  }
});
$('hub-room').addEventListener('change', (e) => {
  const pick = e.target.closest('[data-hub-pick]');
  if (!pick) return;
  const id = Number(pick.dataset.hubPick);
  if (pick.checked) hubPicked.add(id); else hubPicked.delete(id);
  setHtml($('hub-picks'), hubPicksHtml(hubState));
  setHtml($('hub-invitables'), hubInvitablesHtml(hubState, lastInvitables));
});
$('hub-invites').addEventListener('click', (e) => {
  const j = e.target.closest('[data-inv-join]');
  if (j) return joinInvite(Number(j.dataset.invJoin));
  const d = e.target.closest('[data-inv-dismiss]');
  if (d) post({ cmd: 'hub.dismiss', id: Number(d.dataset.invDismiss) });
});
$('hub-offers').addEventListener('click', (e) => {
  const b = e.target.closest('[data-offer-from]');
  if (b && okToJoin(b.dataset.offerKind)) post({ cmd: 'hub.acceptOffer', from: Number(b.dataset.offerFrom), oid: b.dataset.offerOid });
});
$('hub-saved').addEventListener('click', (e) => {
  const t = e.target;
  const more = t.closest('[data-saved-more]');
  if (more) {
    tileMenu = tileMenu === more.dataset.savedMore ? null : more.dataset.savedMore;
    return setHtml($('hub-saved'), savedHtml(hubState));
  }
  const close = t.closest('[data-tile-close]');
  if (close) {
    if (close.dataset.tileClose === 'close') {
      // Someone in it who saved it takes over, so the others stay in.
      const others = hubState.members.length > 1;
      if (others && !confirm('Close your copy? Someone else in it takes over, so everyone stays in the hub.')) return;
      return post({ cmd: 'hub.close' });
    }
    return post({ cmd: 'hub.leave' });
  }
  const copy = t.closest('[data-copytext]');
  if (copy) {
    copyCode(copy.dataset.copytext, !!copy.dataset.code);
    copy.classList.add('flash');
    return setTimeout(() => copy.classList.remove('flash'), 700);
  }
  const kick = t.closest('[data-hub-kick]');
  if (kick) {
    const m = hubState.members.find((x) => x.id === Number(kick.dataset.hubKick));
    if (m && confirm(`Remove ${m.name} from the hub?`)) post({ cmd: 'hub.kick', id: m.id });
    return;
  }
  const open = t.closest('[data-saved-open]');
  if (open && leaveFirst()) return post({ cmd: 'hub.open', id: open.dataset.savedOpen });
  const scan = t.closest('[data-saved-scan]');
  if (scan) return post({ cmd: 'hub.scan', id: scan.dataset.savedScan });
  const del = t.closest('[data-saved-del]');
  if (del) {
    tileMenu = null;
    return openDelete(del.dataset.savedDel, del.dataset.savedName, !!del.dataset.savedHere, true);
  }
  const join = t.closest('[data-scan-join]');
  if (join && leaveFirst()) return post({ cmd: 'hub.joinCode', code: join.dataset.scanJoin });
  if (t.closest('[data-prompt-cancel]')) post({ cmd: 'hub.dismissPrompt' });
});

/* --------------------------------------------------------- connections */

const VIA = {
  official: ['TailCat', 'The official TailCat, end-to-end encrypted'],
  voip: ['VOIP helper', 'Portside’s VOIP helper: TailCat’s own library, so voice can go over UDP. End-to-end encrypted the same way.'],
};

function connHtml(c) {
  const people = c.people.map((p) => `
    <div class="person">
      <span class="avatar" style="--h:${hue(p.name || '?')}">${esc(initials(p.name || '?'))}</span>
      <span class="person-name">${esc(p.name || 'Someone')}</span>
      <span class="grow"></span>
      ${pathLabel(p)}
      ${p.drop ? `<button class="btn small ghost" data-conn-drop="${esc(p.drop)}" data-conn-who="${esc(p.name || 'them')}" title="Disconnect ${esc(p.name || 'them')}">Remove</button>` : ''}
    </div>`).join('');
  const host = c.host ? `<span class="hostname">${c.section === 'File Share' ? 'From' : 'Host'} <b>${esc(c.host)}</b></span>` : '';
  const empty = c.role === 'Hosting' ? '<div class="peers-empty">Nobody connected yet.</div>' : '';
  return `
    <div class="sess conn">
      <div class="sess-head">
        <span class="kind">${esc(c.section)}</span>
        <span class="sess-title">${esc(c.title)}</span>
        <span class="meta">${esc(c.role)}</span>
        ${host}${c.link ? linkPill(c.link, c.host || 'them') : ''}
        <span class="grow"></span>
        <span class="via" title="${esc((VIA[c.via] || VIA.official)[1])}">${esc((VIA[c.via] || VIA.official)[0])}</span>
        <button class="btn small danger" data-conn-drop="${esc(c.id)}">${esc(c.dropLabel)}</button>
      </div>
      ${people || (c.role === 'Hosting' ? `<div class="peers">${empty}</div>` : '')}
    </div>`;
}

function renderConnections(rows) {
  $('conn-count').textContent = rows.length ? `${rows.length} open` : '';
  setHtml($('conn-list'), rows.length ? rows.map(connHtml).join('')
    : '<div class="empty">Nothing connected. Whatever you open or join in any tab shows up here.</div>');
}

$('conn-list').addEventListener('click', (e) => {
  const b = e.target.closest('[data-conn-drop]');
  if (!b) return;
  if (b.dataset.connWho && !confirm(`Disconnect ${b.dataset.connWho}?`)) return;
  post({ cmd: 'conn.drop', id: b.dataset.connDrop });
});

/* The VOIP tab's shortcut: send the room to everyone in the hub. */
$('voip-invite-hub').addEventListener('click', () => post({ cmd: 'hub.invite', what: 'voip', to: [] }));

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
      ${j.st === 'unreachable' ? '<p class="warn on">Can\'t reach your friend. Check the code, and that they still have the port open. Portside keeps trying.</p>' : ''}
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
$('voip-voice-join').addEventListener('click', () => post({ cmd: 'voip.voice', on: true, channel: (voipState && voipState.channel) || 1 }));
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
  const watching = (v.watching || []).includes(m.id);
  const watch = m.screen && !you
    ? `<button class="btn small ${watching ? '' : 'primary'}" data-watch="${m.id}" title="${watching ? 'Stop watching' : 'Watch (you can watch several at once)'}">${watching ? 'Watching' : 'Watch'}</button>`
    : '';
  // Ask someone who isn't in voice to come in: they hear a ring.
  const ring = !you && !m.voice && v.inVoice ? `<button class="btn small ghost" data-ring="${m.id}" title="Ring ${esc(m.name)} into voice">Ring</button>` : '';
  const chan = m.voice && (v.channels || []).length > 1 ? `<span class="meta">${esc(channelName(v, m.channel))}</span>` : '';
  return `
    <div class="person" data-member="${m.id}" title="Right-click for options">
      <span class="avatar${speaking.includes(m.id) ? ' speaking' : ''}" style="--h:${hue(m.name)}">${esc(initials(m.name))}</span>
      <span class="person-name">${esc(m.name)}</span>
      ${tags.join('')}${icons}${mine}${chan}
      <span class="grow"></span>
      ${ring}${watch}${link}
    </div>`;
}

const ICON_EYE = '<svg width="13" height="13" viewBox="0 0 16 16" fill="none"><path d="M1.5 8s2.4-4.5 6.5-4.5S14.5 8 14.5 8 12.1 12.5 8 12.5 1.5 8 1.5 8z" stroke="currentColor" stroke-width="1.4"/><circle cx="8" cy="8" r="2" fill="currentColor"/></svg>';

/* Sharers' latest previews (blob URLs), sent by the room every few seconds. */
const memberThumbs = new Map();
function onMemberThumb(msg) {
  const old = memberThumbs.get(msg.id);
  if (old) URL.revokeObjectURL(old);
  if (msg.img) memberThumbs.set(msg.id, URL.createObjectURL(new Blob([unb64(msg.img)], { type: 'image/jpeg' })));
  else memberThumbs.delete(msg.id);
  if (voipState) renderVoip(voipState);
}

function channelName(v, id) {
  const c = (v.channels || []).find((x) => x.id === id);
  return c ? c.name : 'General';
}

/* Someone in voice, as a tile: their initials big, name and mic state at the
   bottom, LIVE and Watch at the top when they're sharing. */
function tile(m, v, speaking) {
  const you = m.id === v.me;
  const icons = m.deaf ? `<span class="ico" title="Deafened">${ICON_DEAF}</span>` : m.muted || m.smuted ? `<span class="ico" title="${m.smuted ? 'Muted by the host' : 'Muted'}">${ICON_MUTED}</span>` : '';
  const top = [];
  if (m.screen) top.push('<span class="tag-sm live">LIVE</span>');
  if (m.host) top.push('<span class="tag-sm">Host</span>');
  top.push('<span class="grow"></span>');
  const watching = (v.watching || []).includes(m.id);
  if (m.screen && !you) {
    top.push(`<button class="btn small ${watching ? '' : 'primary'}" data-watch="${m.id}">${watching ? 'Watching' : 'Watch'}</button>`);
  }
  // A sharer's tile shows their stream's latest picture and how many watch it.
  const thumb = m.screen && memberThumbs.get(m.id);
  const seen = m.screen ? `<span class="tile-viewers" title="${m.watchers || 0} watching">${ICON_EYE}${m.watchers || 0}</span>` : '';
  return `
    <div class="tile${thumb ? ' has-thumb' : ''}" data-member="${m.id}" style="--h:${hue(m.name)}" title="Right-click for options">
      ${thumb ? `<img class="tile-thumb" src="${thumb}" alt="">` : ''}
      <div class="tile-top">${top.join('')}</div>
      ${seen}
      <span class="avatar${speaking.includes(m.id) ? ' speaking' : ''}" style="--h:${hue(m.name)}">${esc(initials(m.name))}</span>
      <span class="tile-name"><span>${esc(m.name)}${you ? ' (you)' : ''}</span>${icons}</span>
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
  setHtml(list, v.chat.length ? v.chat.map((m) => m.system
    ? `<div class="msg system"><span>${esc(m.text)}</span><span class="msg-time">${time(m.ts)}</span></div>`
    : `<div class="msg${m.from === v.me ? ' mine' : ''}">
         <div class="msg-head"><span class="msg-name" style="--h:${hue(m.name)}">${esc(m.name)}</span><span class="msg-time">${time(m.ts)}</span></div>
         ${m.text ? `<div class="msg-text">${esc(m.text)}</div>` : ''}
         ${m.file ? attachmentHtml(m.file, v) : ''}
       </div>`).join('') : '<div class="empty">No messages yet. Say hi.</div>');
  fillChatImages(list, v.gen);
  if (chatPinned) list.scrollTop = list.scrollHeight;
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

  // Voice: the people in our channel (rooms from before 0.15 have only one).
  const myChannel = v.channel || 1;
  const inChannel = (m) => (m.channel || 1) === myChannel;
  const voiceMembers = v.members.filter((m) => m.voice && inChannel(m));
  const allVoice = v.members.filter((m) => m.voice).length;
  $('voip-voice-count').textContent = allVoice ? `${allVoice} in voice` : '';
  renderChannels(v);
  voiceSounds(v, voiceMembers);
  for (const id of [...memberThumbs.keys()]) {
    if (!v.members.some((m) => m.id === id && m.screen)) {
      URL.revokeObjectURL(memberThumbs.get(id));
      memberThumbs.delete(id);
    }
  }
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
    ? voiceMembers.map((m) => tile(m, v, speaking)).join('')
    : `<div class="empty small">${v.inVoice ? 'Connecting to voice…' : 'Nobody is in voice yet. Join to start talking.'}</div>`);
  setHtml($('voip-members'), v.members.map((m) => person(m, v, speaking)).join(''));
  renderLog('voip', v.log);
}

/* Voice channels: a chip each, with who's in it. Click one to join it (or
   move to it). The host can add channels, and remove all but General. */
let addingChannel = false;
function renderChannels(v) {
  const chans = v.channels && v.channels.length ? v.channels : [{ id: 1, name: 'General' }];
  const el = $('voip-channels');
  const show = chans.length > 1 || v.isHost;
  hide(el, !show || v.st !== 'live');
  if (!show) return;
  const mine = v.channel || 1;
  const chips = chans.map((c) => {
    const people = v.members.filter((m) => m.voice && (m.channel || 1) === c.id);
    const here = v.inVoice && c.id === mine;
    const faces = people.slice(0, 5).map((m) => `<span class="avatar xs" style="--h:${hue(m.name)}" title="${esc(m.name)}">${esc(initials(m.name))}</span>`).join('');
    const more = people.length > 5 ? `<span class="meta">+${people.length - 5}</span>` : '';
    const del = v.isHost && c.id !== 1 ? `<span class="vch-del" data-ch-del="${c.id}" title="Remove this channel (its people go to General)">${ICON_CLOSE}</span>` : '';
    return `<button class="vch${here ? ' on' : ''}" data-ch="${c.id}" title="${here ? 'You\'re here' : `Join ${esc(c.name)}`}">
      <svg width="13" height="13" viewBox="0 0 16 16" fill="none"><path d="M2.5 6h2.5l3.5-3v10l-3.5-3H2.5z" stroke="currentColor" stroke-width="1.4" stroke-linejoin="round"/><path d="M11 5.5a3.5 3.5 0 0 1 0 5" stroke="currentColor" stroke-width="1.4" stroke-linecap="round"/></svg>
      <span class="vch-name">${esc(c.name)}</span>${faces}${more}${del}</button>`;
  });
  if (v.isHost && chans.length < 8) {
    chips.push(addingChannel
      ? '<form class="vch-add" id="vch-form"><input class="input" id="vch-name" maxlength="32" placeholder="Channel name" autocomplete="off"><button class="btn small primary">Add</button></form>'
      : '<button class="vch ghost" data-ch-add="1" title="Add a voice channel">+ Channel</button>');
  }
  setHtml(el, chips.join(''));
  if (addingChannel && document.activeElement !== $('vch-name')) $('vch-name').focus();
}

$('voip-channels').addEventListener('click', (e) => {
  const del = e.target.closest('[data-ch-del]');
  if (del) {
    e.stopPropagation();
    post({ cmd: 'voip.channels', action: 'remove', channel: Number(del.dataset.chDel) });
    return;
  }
  if (e.target.closest('[data-ch-add]')) {
    addingChannel = true;
    if (voipState) renderChannels(voipState);
    return;
  }
  const b = e.target.closest('[data-ch]');
  if (!b || !voipState) return;
  post({ cmd: 'voip.voice', on: true, channel: Number(b.dataset.ch) });
});
$('voip-channels').addEventListener('submit', (e) => {
  e.preventDefault();
  const name = $('vch-name').value.trim();
  if (name) post({ cmd: 'voip.channels', action: 'add', name });
  addingChannel = false;
  if (voipState) renderChannels(voipState);
});
$('voip-channels').addEventListener('keydown', (e) => {
  if (e.key === 'Escape' && addingChannel) {
    addingChannel = false;
    if (voipState) renderChannels(voipState);
  }
});

/* Sounds, made here (no sound files): a rising chime when someone joins your
   voice channel, falling when they leave, and a ring when someone calls you. */
let sfx = null;
function tones(notes) {
  if (vs.sounds === false) return;
  try {
    sfx = sfx || new (window.AudioContext || window.webkitAudioContext)();
    if (sfx.state === 'suspended') sfx.resume();
    let t = sfx.currentTime + 0.02;
    for (const [freq, dur] of notes) {
      const o = sfx.createOscillator();
      const g = sfx.createGain();
      o.type = 'sine';
      o.frequency.value = freq;
      g.gain.setValueAtTime(0, t);
      g.gain.linearRampToValueAtTime(0.16 * (vs.outputGain || 1), t + 0.015);
      g.gain.exponentialRampToValueAtTime(0.001, t + dur);
      o.connect(g).connect(sfx.destination);
      o.start(t);
      o.stop(t + dur + 0.02);
      t += dur * 0.8;
    }
  } catch (_) { /* no audio here */ }
}
const SOUND_JOIN = [[587, 0.12], [880, 0.18]];
const SOUND_LEAVE = [[740, 0.12], [494, 0.2]];
const SOUND_RING = [[659, 0.16], [880, 0.16], [659, 0.16], [880, 0.3]];

let lastVoice = null; // ids in our channel last time, while we're in voice
function voiceSounds(v, voiceMembers) {
  const ids = new Set(voiceMembers.map((m) => m.id));
  if (!v.inVoice) {
    if (lastVoice) tones(SOUND_LEAVE); // we left
    lastVoice = null;
    return;
  }
  if (!lastVoice) {
    tones(SOUND_JOIN); // we joined
  } else {
    const joined = [...ids].some((id) => !lastVoice.has(id) && id !== v.me);
    const left = [...lastVoice].some((id) => !ids.has(id) && id !== v.me);
    if (joined) tones(SOUND_JOIN);
    else if (left) tones(SOUND_LEAVE);
  }
  lastVoice = ids;
}

/* Someone rang us: ring, and offer to join their channel. */
let ringFrom = null;
let ringTimer = 0;
function onRing(msg) {
  ringFrom = msg;
  $('ring-avatar').style.setProperty('--h', hue(msg.name || '?'));
  $('ring-avatar').textContent = initials(msg.name || '?');
  $('ring-text').innerHTML = `<b>${esc(msg.name)}</b> is calling you into voice`;
  hide($('ring-toast'), false);
  tones(SOUND_RING);
  clearTimeout(ringTimer);
  ringTimer = setTimeout(() => hide($('ring-toast'), true), 30000);
}
$('ring-join').addEventListener('click', () => {
  hide($('ring-toast'), true);
  if (ringFrom) post({ cmd: 'voip.voice', on: true, channel: ringFrom.channel || 1 });
  page('voip');
});
$('ring-dismiss').addEventListener('click', () => hide($('ring-toast'), true));
document.getElementById('page-voip').addEventListener('click', (e) => {
  const r = e.target.closest('[data-ring]');
  if (!r) return;
  post({ cmd: 'voip.ring', id: Number(r.dataset.ring) });
  r.disabled = true;
  r.textContent = 'Ringing…';
});

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
const VS_DEFAULT = { inputGain: 1, autoSensitivity: true, thresholdDb: -50, noiseSuppression: true, echoCancellation: true, outputGain: 1, sounds: true };
let vs = { ...VS_DEFAULT };
try { vs = { ...VS_DEFAULT, ...JSON.parse(localStorage.getItem('kc-audio') || '{}') }; } catch (_) { /* defaults */ }

function vsRender() {
  $('vs-in').value = Math.round(vs.inputGain * 100);
  $('vs-in-val').textContent = `${Math.round(vs.inputGain * 100)}%`;
  $('vs-out').value = Math.round(vs.outputGain * 100);
  $('vs-out-val').textContent = `${Math.round(vs.outputGain * 100)}%`;
  $('vs-auto').checked = vs.autoSensitivity;
  $('vs-ns').checked = vs.noiseSuppression;
  $('vs-ec').checked = vs.echoCancellation !== false;
  $('vs-sounds').checked = vs.sounds !== false;
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
$('vs-ec').addEventListener('change', () => { vs.echoCancellation = $('vs-ec').checked; vsSave(); });
$('vs-sounds').addEventListener('change', () => { vs.sounds = $('vs-sounds').checked; vsSave(); if (vs.sounds) tones(SOUND_JOIN); });
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

/* Image previews are kept as loaded <img> nodes and moved into each new
   render of the chat. Rebuilding them from markup made every one reload
   (the app serves them uncached), so the chat shrank and grew again on each
   new message or download tick, and the list jumped. */
const chatImages = new Map();
let chatImagesGen = null;
function fillChatImages(list, gen) {
  if (gen !== chatImagesGen) { chatImages.clear(); chatImagesGen = gen; chatPinned = true; }
  list.querySelectorAll('.att-img-slot').forEach((slot) => {
    const src = slot.dataset.src;
    let img = chatImages.get(src);
    if (!img) {
      img = new Image();
      img.className = 'att-img';
      img.alt = slot.dataset.alt;
      img.draggable = false;
      // Loading makes the list taller; stay at the bottom if we were there.
      img.addEventListener('load', () => { if (chatPinned) list.scrollTop = list.scrollHeight; });
      img.src = src;
      chatImages.set(src, img);
    }
    slot.replaceWith(img);
  });
}

/* Stay pinned to the bottom unless the user has scrolled up to read. Tracked
   from scrolling rather than measured at render time, because an image that
   finished loading in between would make it look like they had scrolled. */
let chatPinned = true;
$('voip-chat').addEventListener('scroll', () => {
  const list = $('voip-chat');
  chatPinned = list.scrollHeight - list.scrollTop - list.clientHeight < 30;
});

function attachmentHtml(f, v) {
  const d = v.downloads[String(f.aid)];
  const img = v.previews.includes(f.aid)
    ? `<span class="att-img-slot" data-src="/att/${v.gen}/${f.aid}" data-alt="${esc(f.name)}"></span>`
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

/* Portside captures and encodes the share itself (Windows Graphics Capture and
   Media Foundation; ScreenCaptureKit and VideoToolbox on a Mac): a screen or
   one window, its sound, the GPU's H.264 encoder. The page only picks what to
   share and shows the result. The frames go from the app straight onto the
   room's UDP, inside the TailCat tunnel.

   The older path is kept as "Use the basic picker" (Windows only): capture
   with the web view, encode with WebCodecs, hand chunks to the app. */

const ss = { stream: null, encoder: null, reader: null, running: false, forceKey: false, bytes: 0, frames: 0, info: null, tick: 0, config: null, rate: 0, maxRate: 0, native: null, fpsNow: 0 };

function nativeRes() {
  const r = window.devicePixelRatio || 1;
  return { w: Math.round(screen.width * r), h: Math.round(screen.height * r) };
}

function fillResOptions() {
  const n = srcSize();
  const keep = $('ss-res').value;
  const opts = [`<option value="native">Full size (${n.w}×${n.h})</option>`];
  for (const h of [1440, 1080, 720, 480]) {
    if (h < n.h) opts.push(`<option value="${h}">${h}p</option>`);
  }
  $('ss-res').innerHTML = opts.join('');
  if ([...$('ss-res').options].some((o) => o.value === keep)) $('ss-res').value = keep;
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
  const n = srcSize();
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
  closePicker();
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

/* ---- the native picker: a popup with a live picture of each screen and app
   window. Only the windows people share (apps, browsers, games) are listed
   unless "Show all windows" is ticked. The pictures are made by the app
   (src/capture) every few seconds while the popup is open, and never leave
   this computer. */

const pick = { kind: 'window', sources: [], audio: [], chosen: null, error: '', all: false, thumbs: new Map(), asked: new Set(), timer: 0 };
const THUMB_EVERY = 2500;

function chosenSource() {
  return pick.sources.find((x) => x.id === pick.chosen) || null;
}

function srcSize() {
  const c = chosenSource();
  return c && c.w && c.h ? { w: c.w, h: c.h } : nativeRes();
}

function shownSources() {
  return pick.sources.filter((x) => x.kind === pick.kind && (pick.all || x.main));
}

function thumbHtml(x) {
  const t = pick.thumbs.get(x.id) || {};
  if (t.img) return `<img class="pic" src="${t.img}" alt="">`;
  const why = x.minimized ? 'Minimized. Shares when it\'s restored.' : '';
  return `${t.icon ? `<img class="big-icon" src="${t.icon}" alt="">` : ''}${why ? `<span class="ss-none">${why}</span>` : ''}`;
}

function sourceHtml(x) {
  const t = pick.thumbs.get(x.id) || {};
  const icon = x.kind === 'window' && t.icon ? `<img class="icon" src="${t.icon}" alt="">` : '';
  const app = x.kind === 'window' && x.app && x.app !== x.title ? `${esc(x.app)} · ` : '';
  return `<button class="ss-source${x.id === pick.chosen ? ' on' : ''}" data-src="${esc(x.id)}" title="${esc(x.title)}">
      <span class="ss-thumb" data-thumb="${esc(x.id)}">${thumbHtml(x)}</span>
      <span class="ss-label">${icon}<span class="ss-name">${esc(x.title)}</span></span>
      <span class="meta">${app}${x.w}×${x.h}${x.minimized ? ' · minimized' : ''}</span>
    </button>`;
}

function renderSources() {
  for (const b of document.querySelectorAll('#ss-kind .seg-btn')) b.classList.toggle('on', b.dataset.kind === pick.kind);
  hide($('ss-all-wrap'), pick.kind !== 'window');
  $('ss-all').checked = pick.all;
  const list = shownSources();
  if (pick.error) {
    setHtml($('ss-sources'), `<div class="empty">${esc(pick.error)}</div>`);
  } else if (!list.length) {
    const hidden = pick.sources.some((x) => x.kind === 'window' && !x.main);
    const none = !pick.sources.length ? 'Looking for screens and windows…'
      : pick.kind === 'window' ? `No app windows are open.${hidden ? ' Tick "Show all windows" to see the rest.' : ''}` : 'No screens found.';
    setHtml($('ss-sources'), `<div class="empty">${none}</div>`);
  } else {
    if (!list.some((x) => x.id === pick.chosen)) pick.chosen = list[0].id;
    setHtml($('ss-sources'), list.map(sourceHtml).join(''));
  }
  // Sound: "this app" only for a window; nothing the computer can't do.
  const can = new Set(pick.audio);
  const audio = $('ss-audio');
  for (const o of audio.options) {
    o.disabled = o.value !== 'off' && (!can.has(o.value) || (o.value === 'app' && pick.kind !== 'window'));
  }
  if (audio.selectedOptions[0] && audio.selectedOptions[0].disabled) audio.value = can.has('system') ? 'system' : 'off';
  audio.parentElement.title = can.size ? '' : 'Sharing sound needs Windows 10 2004+ or macOS 13+.';
  $('ss-go').disabled = !chosenSource();
  fillResOptions();
}

function setKind(kind) {
  pick.kind = kind;
  // Default sound: a window's own app; a screen, everything.
  $('ss-audio').value = kind === 'window' ? 'app' : 'system';
  renderSources();
  askThumbs();
}

/* Pictures for what's on show; icons the first time each window is shown. */
function askThumbs() {
  if ($('ss-box').classList.contains('hide') || pick.error) return;
  const ids = shownSources().map((x) => x.id);
  if (!ids.length) return;
  const icons = ids.some((id) => !pick.asked.has(id) && id.startsWith('window:'));
  ids.forEach((id) => pick.asked.add(id));
  post({ cmd: 'voip.thumbs', ids, icons });
}

function b64Url(data, type) {
  return URL.createObjectURL(new Blob([unb64(data)], { type }));
}

function onThumb(msg) {
  if ($('ss-box').classList.contains('hide')) return;
  const t = pick.thumbs.get(msg.id) || {};
  if (msg.img) {
    if (t.img) URL.revokeObjectURL(t.img);
    t.img = b64Url(msg.img, 'image/jpeg');
  }
  if (msg.icon) {
    if (t.icon) URL.revokeObjectURL(t.icon);
    t.icon = b64Url(msg.icon, 'image/png');
  }
  pick.thumbs.set(msg.id, t);
  const x = pick.sources.find((s) => s.id === msg.id);
  const el = document.querySelector(`#ss-sources [data-src="${CSS.escape(msg.id)}"]`);
  // Swap in just this one, so the rest don't flicker.
  if (x && el) {
    el.outerHTML = sourceHtml(x);
    lastHtml.delete($('ss-sources'));
  }
}

function dropThumbs() {
  for (const t of pick.thumbs.values()) {
    if (t.img) URL.revokeObjectURL(t.img);
    if (t.icon) URL.revokeObjectURL(t.icon);
  }
  pick.thumbs.clear();
  pick.asked.clear();
}

function listSources() {
  pick.error = '';
  pick.sources = [];
  renderSources();
  post({ cmd: 'voip.sources' });
}

function onSources(msg) {
  pick.sources = msg.sources || [];
  pick.audio = msg.audio || [];
  pick.error = msg.error || '';
  // Nothing to show on the apps side: start on the screens.
  if (pick.kind === 'window' && !pick.sources.some((x) => x.kind === 'window' && x.main)) pick.kind = 'screen';
  $('ss-audio').value = pick.kind === 'window' ? 'app' : 'system';
  // The basic picker, where the web view has one, if native capture can't run.
  hide($('ss-web'), !(pick.error && !MAC && navigator.mediaDevices && navigator.mediaDevices.getDisplayMedia));
  renderSources();
  askThumbs();
}

function openPicker() {
  shareError('');
  pick.kind = 'window';
  pick.chosen = null;
  dropThumbs();
  hide($('ss-box'), false);
  hide($('ss-row'), true);
  listSources();
  clearInterval(pick.timer);
  pick.timer = setInterval(askThumbs, THUMB_EVERY);
}

function closePicker() {
  clearInterval(pick.timer);
  pick.timer = 0;
  hide($('ss-box'), true);
  hide($('ss-row'), false);
  dropThumbs();
}

$('ss-kind').addEventListener('click', (e) => {
  const b = e.target.closest('[data-kind]');
  if (b) setKind(b.dataset.kind);
});
$('ss-all').addEventListener('change', (e) => {
  pick.all = e.target.checked;
  renderSources();
  askThumbs();
});
$('ss-sources').addEventListener('click', (e) => {
  const b = e.target.closest('[data-src]');
  if (!b) return;
  pick.chosen = b.dataset.src;
  renderSources();
});
$('ss-sources').addEventListener('dblclick', (e) => {
  const b = e.target.closest('[data-src]');
  if (!b) return;
  pick.chosen = b.dataset.src;
  startNative();
});
$('ss-refresh').addEventListener('click', () => { dropThumbs(); listSources(); });
$('ss-close').addEventListener('click', () => closePicker());
$('ss-box').addEventListener('mousedown', (e) => { if (e.target === $('ss-box')) closePicker(); });
window.addEventListener('keydown', (e) => { if (e.key === 'Escape' && !$('ss-box').classList.contains('hide')) closePicker(); });

function startNative() {
  const c = chosenSource();
  if (!c) return;
  shareError('');
  const n = srcSize();
  const choice = $('ss-res').value || 'native';
  const { w, h } = outSize(n.w, n.h, choice);
  const fps = Number($('ss-fps').value);
  const audio = $('ss-audio').value;
  post({
    cmd: 'voip.share',
    source: c.id,
    height: choice === 'native' ? null : Number(choice),
    fps,
    bitrate: bitrateFor(w, h, fps),
    audio,
    hint: preset().hint,
    preview: $('ss-preview-on').checked,
  });
  closePicker();
  hide($('ss-row'), true);
}

/* ---- our own preview, from the frames we send */

const pv = { decoder: null, codec: null, waitKey: true };

function closePreview() {
  try { pv.decoder && pv.decoder.state !== 'closed' && pv.decoder.close(); } catch (_) { /* gone */ }
  pv.decoder = null;
  pv.codec = null;
  hide($('ss-preview-canvas'), true);
}

function onPreview(msg) {
  if (!window.VideoDecoder || !ss.native || !ss.native.codec) return;
  if (!pv.decoder || pv.codec !== ss.native.codec) {
    closePreview();
    const canvas = $('ss-preview-canvas');
    const ctx = canvas.getContext('2d');
    pv.codec = ss.native.codec;
    pv.waitKey = true;
    pv.decoder = new VideoDecoder({
      output: (frame) => {
        if (canvas.width !== frame.displayWidth || canvas.height !== frame.displayHeight) {
          canvas.width = frame.displayWidth;
          canvas.height = frame.displayHeight;
        }
        ctx.drawImage(frame, 0, 0);
        frame.close();
      },
      error: () => { pv.waitKey = true; },
    });
    pv.decoder.configure({ codec: pv.codec, optimizeForLatency: true });
    hide($('ss-preview-canvas'), false);
  }
  if (pv.waitKey && !msg.key) return;
  if (pv.decoder.decodeQueueSize > 4) { pv.waitKey = true; return; }
  pv.waitKey = false;
  try {
    pv.decoder.decode(new EncodedVideoChunk({ type: msg.key ? 'key' : 'delta', timestamp: msg.ts, data: unb64(msg.data) }));
  } catch (_) {
    pv.waitKey = true;
  }
}

$('ss-preview-on').addEventListener('change', (e) => {
  if (ss.native) {
    post({ cmd: 'voip.preview', on: e.target.checked });
    if (!e.target.checked) closePreview();
  }
});

/* ---- showing our native share */

function renderNative(v) {
  const live = v.sharingNative;
  const info = live ? v.sharing : null;
  ss.native = live ? { codec: info && info.codec, starting: v.shareStarting } : null;
  if (!live) closePreview();
  if (live) {
    if (!$('ss-box').classList.contains('hide')) closePicker();
    hide($('ss-live'), false);
    hide($('ss-preview'), true);
    hide($('ss-preview-canvas'), !$('ss-preview-on').checked || !pv.decoder);
    if (v.shareStarting || !info) {
      $('ss-stats').textContent = 'Starting…';
    } else {
      const rate = ss.rate || info.bitrate;
      const fps = ss.fpsNow || info.fps;
      const fitted = rate < info.bitrate * 0.97 ? ' · lowered to fit a viewer\'s connection' : '';
      const what = info.label ? `${info.label} · ` : '';
      const paused = info.paused ? ' · window minimized' : '';
      const sound = info.audio ? ' · with sound' : '';
      const me = v.members.find((m) => m.id === v.me);
      const n = me && me.watchers ? me.watchers : 0;
      const seen = ` · ${n === 1 ? '1 person watching' : `${n} people watching`}`;
      $('ss-stats').textContent = `${what}${info.w}×${info.h} · ${fps} fps · ${(rate / 1e6).toFixed(1)} Mbit/s · ${info.hardware ? 'hardware' : 'software'} H.264${sound}${paused}${fitted}${seen}`;
    }
  } else if (!ss.running) {
    hide($('ss-live'), true);
    ss.rate = 0;
    ss.fpsNow = 0;
  }
  if (v.shareError) shareError(v.shareError);
  $('ss-notice').textContent = v.shareNotice || '';
  show($('ss-notice'), v.shareNotice);
}

$('ss-open').addEventListener('click', () => openPicker());
$('ss-cancel').addEventListener('click', () => closePicker());
$('ss-go').addEventListener('click', () => startNative());
$('ss-web').addEventListener('click', () => startShare());
$('ss-stop').addEventListener('click', () => {
  if (ss.native) post({ cmd: 'voip.unshare' });
  else stopShare();
});

/* Watching: up to four streams at once, each decoded with the browser's
   (hardware) decoder onto its own canvas. Each has its own sound control in
   the bottom right (yours only), full screen, pop-out and close. */

const views = new Map(); // sharer id -> { decoder, codec, waitKey, frames, bytes, el, canvas, ctx, m }
let viewTick = 0;

const ICON_SPEAKER = '<svg width="16" height="16" viewBox="0 0 16 16" fill="none"><path d="M2.5 6h2.5l3.5-3v10l-3.5-3H2.5z" stroke="currentColor" stroke-width="1.4" stroke-linejoin="round"/><path d="M11 5.5a3.5 3.5 0 0 1 0 5M12.8 3.8a6 6 0 0 1 0 8.4" stroke="currentColor" stroke-width="1.4" stroke-linecap="round"/></svg>';
const ICON_SPEAKER_OFF = '<svg width="16" height="16" viewBox="0 0 16 16" fill="none"><path d="M2.5 6h2.5l3.5-3v10l-3.5-3H2.5z" stroke="currentColor" stroke-width="1.4" stroke-linejoin="round"/><path d="M11 6l4 4M15 6l-4 4" stroke="currentColor" stroke-width="1.4" stroke-linecap="round"/></svg>';
const ICON_POPOUT = '<svg width="16" height="16" viewBox="0 0 16 16" fill="none"><rect x="1.5" y="2.5" width="13" height="11" rx="1.5" stroke="currentColor" stroke-width="1.4"/><rect x="8" y="8" width="5" height="4" rx=".8" fill="currentColor"/></svg>';
const ICON_FULL = '<svg width="16" height="16" viewBox="0 0 16 16" fill="none"><path d="M2 6V2h4M10 2h4v4M14 10v4h-4M6 14H2v-4" stroke="currentColor" stroke-width="1.4" stroke-linecap="round" stroke-linejoin="round"/></svg>';
const ICON_CLOSE = '<svg width="14" height="14" viewBox="0 0 10 10"><path d="M2 2l6 6M8 2L2 8" stroke="currentColor" stroke-width="1.4" stroke-linecap="round"/></svg>';

function streamTitle(m) {
  return `${m.name}'s screen${m.screen && m.screen.label ? ` · ${m.screen.label}` : ''}`;
}

function streamPane(m) {
  const el = document.createElement('div');
  el.className = 'stream';
  el.dataset.stream = m.id;
  el.innerHTML = `
    <canvas></canvas>
    <video class="pip-src" muted playsinline></video>
    <div class="stream-wait">Waiting for the picture…</div>
    <div class="stream-paused hide">The shared window is minimized. The picture comes back when it's restored.</div>
    <div class="stream-popped hide">Playing in its own window. <button class="btn small" data-act="popin">Bring it back</button></div>
    <div class="stream-top"><span class="live-tag">LIVE</span><span class="stream-title"></span><span class="grow"></span><span class="stream-stats"></span></div>
    <div class="stream-bar">
      <button class="sbtn" data-act="close" title="Stop watching">${ICON_CLOSE}</button>
      <button class="sbtn" data-act="popout" title="Pop out into its own window">${ICON_POPOUT}</button>
      <button class="sbtn" data-act="full" title="Full screen (double-click)">${ICON_FULL}</button>
      <span class="grow"></span>
      <span class="stream-vol" title="The stream's sound, for you only">
        <button class="sbtn" data-act="mute" title="Mute the stream"></button>
        <input type="range" data-act="vol" min="0" max="200" step="5" value="100">
        <span class="stream-vol-val">100%</span>
      </span>
    </div>`;
  return el;
}

function openViewer(m) {
  const el = streamPane(m);
  $('voip-stage').appendChild(el);
  const canvas = el.querySelector('canvas');
  const ctx = canvas.getContext('2d');
  const vw = { decoder: null, codec: m.screen.codec, waitKey: true, frames: 0, bytes: 0, el, canvas, ctx, m };
  vw.decoder = new VideoDecoder({
    output: (frame) => {
      if (canvas.width !== frame.displayWidth || canvas.height !== frame.displayHeight) {
        canvas.width = frame.displayWidth;
        canvas.height = frame.displayHeight;
      }
      ctx.drawImage(frame, 0, 0);
      frame.close();
      vw.frames++;
      hide(el.querySelector('.stream-wait'), true);
    },
    error: () => resetDecoder(m.id),
  });
  vw.decoder.configure({ codec: vw.codec, optimizeForLatency: true, hardwareAcceleration: 'no-preference' });
  views.set(m.id, vw);
  if (!viewTick) {
    viewTick = setInterval(() => {
      for (const v of views.values()) {
        const s = v.m.screen || {};
        v.el.querySelector('.stream-stats').textContent = `${s.w}×${s.h} · ${Math.round(v.frames / 2)} fps · ${((v.bytes * 8) / 2 / 1e6).toFixed(1)} Mbit/s`;
        v.frames = 0;
        v.bytes = 0;
      }
    }, 2000);
  }
}

function closeViewer(id) {
  const vw = views.get(id);
  if (!vw) return;
  if (vw.el.classList.contains('full')) setTheater(null);
  popIn(vw);
  try { vw.decoder && vw.decoder.state !== 'closed' && vw.decoder.close(); } catch (_) { /* gone */ }
  vw.el.remove();
  views.delete(id);
  if (!views.size) {
    clearInterval(viewTick);
    viewTick = 0;
  }
}

// After a decode error or a backlog: start again from the next key frame.
function resetDecoder(id) {
  const vw = views.get(id);
  if (!vw) return;
  try { vw.decoder && vw.decoder.state !== 'closed' && vw.decoder.reset(); } catch (_) { /* gone */ }
  try { vw.decoder.configure({ codec: vw.codec, optimizeForLatency: true }); } catch (_) { /* gone */ }
  vw.waitKey = true;
  post({ cmd: 'voip.keyframe', id });
}

function onVideo(msg) {
  const vw = views.get(msg.from);
  if (!vw || !vw.decoder || vw.decoder.state === 'closed') return;
  if (vw.waitKey && !msg.key) return;
  if (vw.decoder.decodeQueueSize > 8) return resetDecoder(msg.from);
  vw.waitKey = false;
  const data = unb64(msg.data);
  vw.bytes += data.length;
  try {
    vw.decoder.decode(new EncodedVideoChunk({ type: msg.key ? 'key' : 'delta', timestamp: msg.ts, data }));
  } catch (_) {
    resetDecoder(msg.from);
  }
}

function shareVol(v, id) {
  const vol = v.shareVolume && v.shareVolume[id] != null ? v.shareVolume[id] : 1;
  return { vol, muted: (v.shareMuted || []).includes(id) };
}

function renderStreams(v) {
  const watching = (Array.isArray(v.watching) ? v.watching : v.watching != null ? [v.watching] : [])
    .map((id) => v.members.find((m) => m.id === id && m.screen)).filter(Boolean);
  for (const id of [...views.keys()]) {
    if (!watching.some((m) => m.id === id)) closeViewer(id);
  }
  for (const m of watching) {
    if (!views.has(m.id)) openViewer(m);
    const vw = views.get(m.id);
    vw.m = m;
    const el = vw.el;
    el.querySelector('.stream-title').textContent = streamTitle(m);
    hide(el.querySelector('.stream-paused'), !m.screen.paused);
    // Sound controls: only for a stream with sound.
    const { vol, muted } = shareVol(v, m.id);
    const sv = el.querySelector('.stream-vol');
    sv.classList.toggle('off', !m.screen.audio);
    sv.title = m.screen.audio ? 'The stream\'s sound, for you only' : 'This stream has no sound';
    const mb = el.querySelector('[data-act="mute"]');
    mb.innerHTML = muted || !m.screen.audio ? ICON_SPEAKER_OFF : ICON_SPEAKER;
    mb.title = muted ? 'Unmute the stream' : 'Mute the stream';
    mb.classList.toggle('active', muted);
    mb.disabled = !m.screen.audio;
    const range = el.querySelector('[data-act="vol"]');
    range.disabled = !m.screen.audio;
    if (document.activeElement !== range) range.value = Math.round((muted ? 0 : vol) * 100);
    el.querySelector('.stream-vol-val').textContent = `${muted ? 0 : Math.round(vol * 100)}%`;
  }
  $('voip-stage').dataset.count = String(watching.length);
  hide($('voip-stage'), !watching.length);
}

function renderScreen(v) {
  renderStreams(v);
  renderNative(v);
  // If the room dropped our share (we left, it ended), stop capturing.
  if (ss.running && !v.sharing && v.st !== 'live') stopShare();
  hide($('ss-row'), ss.running || v.sharingNative || !$('ss-box').classList.contains('hide'));
  $('ss-open').disabled = v.st !== 'live';
}

document.getElementById('page-voip').addEventListener('click', (e) => {
  const w = e.target.closest('[data-watch]');
  if (!w) return;
  const id = Number(w.dataset.watch);
  const on = !(voipState && (voipState.watching || []).includes(id));
  post({ cmd: 'voip.watch', id, on });
});

$('voip-stage').addEventListener('click', (e) => {
  const b = e.target.closest('[data-act]');
  const pane = e.target.closest('[data-stream]');
  if (!b || !pane) return;
  const id = Number(pane.dataset.stream);
  const vw = views.get(id);
  switch (b.dataset.act) {
    case 'close': post({ cmd: 'voip.watch', id, on: false }); break;
    case 'full': setTheater(pane.classList.contains('full') ? null : id); break;
    case 'popout': popOut(vw); break;
    case 'popin': popIn(vw); break;
    case 'mute': {
      const { muted } = shareVol(voipState, id);
      post({ cmd: 'voip.shareVolume', id, muted: !muted });
      break;
    }
    default:
  }
});
$('voip-stage').addEventListener('input', (e) => {
  if (e.target.dataset.act !== 'vol') return;
  const id = Number(e.target.closest('[data-stream]').dataset.stream);
  const vol = Number(e.target.value) / 100;
  e.target.closest('.stream-vol').querySelector('.stream-vol-val').textContent = `${Math.round(vol * 100)}%`;
  // Dragging the slider up unmutes, like everywhere else.
  post({ cmd: 'voip.shareVolume', id, volume: vol, muted: vol === 0 });
});
$('voip-stage').addEventListener('dblclick', (e) => {
  const pane = e.target.closest('[data-stream]');
  if (pane && !e.target.closest('.stream-bar')) setTheater(pane.classList.contains('full') ? null : Number(pane.dataset.stream));
});

/* Full screen: one stream fills the window and the window goes borderless
   full screen. Esc or double-click again comes back. */
function setTheater(id) {
  for (const vw of views.values()) vw.el.classList.toggle('full', vw.m.id === id);
  const on = id != null && views.has(id);
  document.documentElement.classList.toggle('theater', on);
  post({ cmd: 'window.fullscreen', on });
}
window.addEventListener('keydown', (e) => {
  if (e.key === 'Escape' && document.documentElement.classList.contains('theater')) setTheater(null);
});

/* Pop-out: the stream plays in the system's own floating picture-in-picture
   window, which stays on top of other apps. The canvas feeds it. */
async function popOut(vw) {
  if (!vw) return;
  const video = vw.el.querySelector('.pip-src');
  try {
    if (!video.srcObject) video.srcObject = vw.canvas.captureStream();
    await video.play();
    if (video.requestPictureInPicture) {
      await video.requestPictureInPicture();
    } else if (video.webkitSetPresentationMode) {
      video.webkitSetPresentationMode('picture-in-picture');
    } else {
      throw new Error('no picture-in-picture');
    }
    vw.el.classList.add('popped');
    hide(vw.el.querySelector('.stream-popped'), false);
    const back = () => popIn(vw);
    video.addEventListener('leavepictureinpicture', back, { once: true });
    video.addEventListener('webkitpresentationmodechanged', () => { if (video.webkitPresentationMode !== 'picture-in-picture') back(); });
  } catch (_) {
    shareError('This computer can\'t pop the stream out into its own window. Try full screen instead.');
  }
}

function popIn(vw) {
  if (!vw) return;
  const video = vw.el.querySelector('.pip-src');
  try {
    if (document.pictureInPictureElement === video) document.exitPictureInPicture();
    else if (video.webkitPresentationMode === 'picture-in-picture') video.webkitSetPresentationMode('inline');
  } catch (_) { /* already back */ }
  vw.el.classList.remove('popped');
  hide(vw.el.querySelector('.stream-popped'), true);
}

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
  const chans = v.channels || [];
  if (v.isHost && chans.length > 1) {
    const others = chans.filter((c) => c.id !== (m.channel || 1));
    parts.push(`<div class="ctx-sep"></div><div class="ctx-head">Move to</div>${others.map((c) => `<button class="ctx-item" data-move="${c.id}">${esc(c.name)}</button>`).join('')}`);
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
  const mv = e.target.closest('[data-move]');
  if (mv) {
    post({ cmd: 'voip.channels', action: 'move', id: Number(menu.dataset.member), channel: Number(mv.dataset.move) });
    closeMenu();
    return;
  }
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
      ${j.st === 'unreachable' ? '<p class="warn on">Can\'t reach your friend. Check the code, and that they\'re still sharing the folder. Portside keeps trying.</p>' : ''}
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
// The name from last time is kept; it's only asked for the first time.
if (BOOT.name) {
  setMe(BOOT.name);
} else {
  hide($('me-chip'), true);
  openName(true);
}

window.onHost = (msg) => {
  switch (msg.type) {
    case 'firewall':
      $('fw-go').disabled = false;
      $('fw-state').textContent = msg.ok
        ? 'Done. Reconnect, and press Allow when Windows asks.'
        : 'Not changed. See the message at the bottom.';
      break;
    case 'state': {
      lastInvitables = msg.invitables || [];
      renderHub(msg.hub || HUB_OFF, lastInvitables);
      renderConnections(msg.connections || []);
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
      const hub = msg.hub || HUB_OFF;
      show($('hub-badge'), hub.st === 'live' || hub.invites.length > 0);
      // In a room and a hub: one press sends the room to everyone in it.
      hide($('voip-invite-hub'), !(hub.st === 'live' && msg.voip.st === 'live' && msg.voip.code));
      document.documentElement.classList.toggle('live', filesLive || portsLive || voipLiveNow || mountsLive || hub.st === 'live');
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
    case 'shareSources':
      onSources(msg);
      break;
    case 'shareThumb':
      onThumb(msg);
      break;
    case 'voipThumb':
      onMemberThumb(msg);
      break;
    case 'voipRing':
      onRing(msg);
      break;
    case 'preview':
      onPreview(msg);
      break;
    case 'videoRate':
      // Watchers' connections decide the bitrate (see media.rs RateControl).
      if (msg.native) {
        ss.rate = msg.bitrate;
        ss.fpsNow = msg.fps;
        break;
      }
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
    case 'goto':
      // An invite was joined: show where it's connecting (and any error there).
      page(msg.page);
      if (msg.view) pfView(msg.view);
      break;
    case 'window':
      document.documentElement.classList.toggle('maximized', msg.maximized);
      break;
  }
};

$('data-link').addEventListener('click', () => post({ cmd: 'openData' }));
$('version').textContent = `v${BOOT.version} · TailCat ${BOOT.tailcat}`;

// Keyboard: Ctrl+1 to 5 (⌘ on a Mac) switch tabs, left to right.
window.addEventListener('keydown', (e) => {
  if (!(MAC ? e.metaKey : e.ctrlKey)) return;
  const tab = { 1: 'hub', 2: 'files', 3: 'ports', 4: 'voip', 5: 'mounts' }[e.key];
  if (tab) page(tab);
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
        { id: 1, name: 'Sam', host: true, voice: true, muted: false, deaf: false, smuted: false, path: 'host', rtt: null, screen: null, channel: 1 },
        { id: 2, name: 'Alex Rivera', host: false, voice: true, muted: false, deaf: false, smuted: false, path: 'direct', rtt: 24, screen: { w: 1920, h: 1080, fps: 30, codec: 'avc1.640028', audio: true, label: 'Minecraft' }, watchers: 2, channel: 1 },
        { id: 3, name: 'Jo', host: false, voice: true, muted: false, deaf: false, smuted: true, path: 'relay', rtt: 88, screen: null, channel: 1 },
        { id: 4, name: 'Priya', host: false, voice: true, muted: false, deaf: false, smuted: false, path: 'direct', rtt: 31, screen: null, channel: 5 },
        { id: 5, name: 'Kit', host: false, voice: false, muted: false, deaf: false, smuted: false, path: 'relay', rtt: 64, screen: null, channel: 1 },
      ],
      channels: [{ id: 1, name: 'General' }, { id: 5, name: 'Raid team' }], channel: 1, shareVolume: {}, shareMuted: [],
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
    hub: {
      st: 'live', mode: 'host', error: '', notice: '', name: 'Minecraft Hub', code: `kitty:hub:24818:${addr}`, me: 1,
      id: 'a1', kind: 'saved', expiresIn: 6 * 86400 + 5 * 3600, savedHere: true, canSave: false, making: null, openToHub: true,
      offers: [{ from: 2, oid: 'host:3', kind: 'game', code: '', name: 'Priya', title: 'Minecraft: Java Edition · port 25565' }],
      saved: [
        { id: 'a1', name: 'Minecraft Hub', expiresIn: 6 * 86400 + 5 * 3600, people: 3, here: true, scan: null, updatedAgo: 7200 },
        { id: 'c3', name: 'Survival SMP', expiresIn: 30 * 86400, people: 4, here: false,
          scan: { running: true, opening: true, checked: 1, total: 3, hits: [], learned: 0 } },
        { id: 'b2', name: 'Friday crew', expiresIn: 9 * 604800, people: 5, here: false,
          scan: { running: false, checked: 4, total: 4, hits: [{ code: 'x', host: 'Sam', members: 3 }] } },
      ],
      prompt: null,
      members: [
        { id: 1, name: 'Kit', host: true, path: 'host', rtt: null },
        { id: 2, name: 'Priya', host: false, path: 'direct', rtt: 18 },
        { id: 3, name: 'Sam', host: false, path: 'relay', rtt: 74 },
      ],
      invites: [{ id: 4, from: 'Priya', kind: 'mount', code: '', title: 'Minecraft Worlds', ago: 40 }],
      sent: 'Invited 2 people.', reconnecting: false, link: { path: 'unknown' }, log: ['# \u{1F408} Server listening with new address: tc\u2026'],
    },
    invitables: [
      { what: 'voip', kind: 'voip', title: 'Hangout' },
      { what: 'share', kind: 'file', title: '3 files' },
      { what: 'host:3', kind: 'game', title: 'Minecraft: Java Edition \u00b7 port 25565' },
    ],
    connections: [
      { id: 'hub', section: 'Hub', title: 'Kit\'s hub', role: 'Hosting', link: null, via: 'official', dropLabel: 'Close',
        people: [{ name: 'Priya', path: 'direct', rtt: 18, drop: 'hub-member:2' }, { name: 'Sam', path: 'relay', rtt: 74, drop: 'hub-member:3' }] },
      { id: 'voip', section: 'VOIP', title: 'Hangout', role: 'Hosting', link: null, via: 'voip', dropLabel: 'Leave',
        people: [{ name: 'Priya', path: 'direct', rtt: 21, drop: 'voip-member:2' }] },
      { id: 'host:3', section: 'Game servers', title: 'Minecraft: Java Edition \u00b7 port 25565', role: 'Hosting', link: null, via: 'official', dropLabel: 'Stop',
        people: [{ name: 'Sam', path: 'relay', rtt: 80, drop: null }] },
      { id: 'join:12', section: 'Port Forward', title: 'Port 50200', role: 'Joined', host: 'Priya', link: { path: 'direct', rtt: 16, checkedAgo: 5 }, via: 'official', dropLabel: 'Disconnect', people: [] },
    ],
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
