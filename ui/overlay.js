/* Portside's overlay window: drawn over the game, ignoring the mouse except
   while it's open for changes (the hotkey). */
'use strict';

const $ = (id) => document.getElementById(id);
const hide = (el, h) => el.classList.toggle('hide', !!h);

function post(msg) {
  if (window.ipc) window.ipc.postMessage(JSON.stringify(msg));
}

const ICON_MIC_OFF = '<svg width="14" height="14" viewBox="0 0 16 16" fill="none"><rect x="5.5" y="1.5" width="5" height="8.5" rx="2.5" stroke="currentColor" stroke-width="1.5"/><path d="M3 7.5a5 5 0 0 0 10 0M8 12.5v2M2 2l12 12" stroke="currentColor" stroke-width="1.5" stroke-linecap="round"/></svg>';
const ICON_DEAF = '<svg width="14" height="14" viewBox="0 0 16 16" fill="none"><path d="M2.5 10V8a5.5 5.5 0 0 1 11 0v2" stroke="currentColor" stroke-width="1.5"/><rect x="1.8" y="9.5" width="3" height="4.5" rx="1.2" stroke="currentColor" stroke-width="1.5"/><rect x="11.2" y="9.5" width="3" height="4.5" rx="1.2" stroke="currentColor" stroke-width="1.5"/><path d="M2 2l12 12" stroke="currentColor" stroke-width="1.5" stroke-linecap="round"/></svg>';

let cfg = { showVoice: true, names: 'always', streamOpacity: 1 };
let st = null;
let editing = false;
let speaking = new Set();
let meTalking = false;

function esc(s) {
  return String(s).replace(/[&<>"']/g, (c) => ({ '&': '&amp;', '<': '&lt;', '>': '&gt;', '"': '&quot;', "'": '&#39;' })[c]);
}
function initials(name) {
  const parts = String(name || '?').trim().split(/\s+/);
  return ((parts[0] || '?')[0] + (parts.length > 1 ? parts[parts.length - 1][0] : '')).toUpperCase();
}
function hue(name) {
  let h = 0;
  for (const c of String(name)) h = (h * 31 + c.charCodeAt(0)) >>> 0;
  return 200 + (h % 50);
}

/* ------------------------------------------------------------ layout */

const LAYOUT_KEY = 'kc-overlay-layout';
const LAYOUT_DEFAULT = { voice: { x: 0.015, y: 0.12 }, stream: { x: 0.72, y: 0.06, w: 0.26 } };
let layout = JSON.parse(JSON.stringify(LAYOUT_DEFAULT));
try { layout = { ...layout, ...JSON.parse(localStorage.getItem(LAYOUT_KEY) || '{}') }; } catch (_) { /* defaults */ }

function saveLayout() {
  try { localStorage.setItem(LAYOUT_KEY, JSON.stringify(layout)); } catch (_) { /* not kept */ }
}

function place() {
  const W = window.innerWidth;
  const H = window.innerHeight;
  const v = $('w-voice');
  v.style.left = `${Math.round(layout.voice.x * W)}px`;
  v.style.top = `${Math.round(layout.voice.y * H)}px`;
  const s = $('w-stream');
  s.style.width = `${Math.round(Math.max(200, layout.stream.w * W))}px`;
  s.style.left = `${Math.round(layout.stream.x * W)}px`;
  s.style.top = `${Math.round(layout.stream.y * H)}px`;
}
window.addEventListener('resize', place);

/* Dragging and resizing, only while open for changes. */
let drag = null;
document.addEventListener('mousedown', (e) => {
  if (!editing || e.button !== 0) return;
  if (e.target.closest('button, select, input')) return;
  const grip = e.target.closest('#ws-grip');
  const w = e.target.closest('.widget');
  if (!w) return;
  const r = w.getBoundingClientRect();
  drag = { el: w, key: w.id === 'w-voice' ? 'voice' : 'stream', resize: !!grip, dx: e.clientX - r.left, dy: e.clientY - r.top, w0: r.width, x0: e.clientX };
  e.preventDefault();
});
document.addEventListener('mousemove', (e) => {
  if (!drag) return;
  const W = window.innerWidth;
  const H = window.innerHeight;
  const r = drag.el.getBoundingClientRect();
  if (drag.resize) {
    const w = Math.min(W * 0.9, Math.max(200, drag.w0 + e.clientX - drag.x0));
    layout.stream.w = w / W;
  } else {
    const x = Math.min(W - Math.min(r.width, 80), Math.max(0, e.clientX - drag.dx));
    const y = Math.min(H - Math.min(r.height, 40), Math.max(0, e.clientY - drag.dy));
    layout[drag.key].x = x / W;
    layout[drag.key].y = y / H;
  }
  place();
});
document.addEventListener('mouseup', () => {
  if (drag) saveLayout();
  drag = null;
});

/* ------------------------------------------------------------ voice */

function talkingIds() {
  const s = new Set(speaking);
  if (st && meTalking) s.add(st.me);
  return s;
}

function voiceMembers() {
  if (!st || !st.inVoice) return [];
  return (st.members || []).filter((m) => m.voice && (m.channel == null || st.channel == null || m.channel === st.channel));
}

function renderVoice() {
  const list = voiceMembers();
  const show = !!st && st.inVoice && cfg.showVoice && list.length > 0;
  hide($('w-voice'), !show);
  if (!show) return;
  const ch = (st.channels || []).find((c) => c.id === st.channel);
  $('wv-tag').textContent = ch ? ch.name : 'Voice';
  const talk = talkingIds();
  $('wv-list').innerHTML = list.map((m) => {
    const t = talk.has(m.id) && !m.muted;
    const showName = cfg.names === 'always' || t || editing;
    const icon = m.deaf ? ICON_DEAF : m.muted || m.smuted ? ICON_MIC_OFF : '';
    return `<li class="vrow${t ? '' : ' quiet'}" data-id="${m.id}">
      <span class="av${t ? ' talk' : ''}" style="--h:${hue(m.name)}">${esc(initials(m.name))}</span>
      ${showName ? `<span class="vname">${esc(m.name)}${m.id === st.me ? ' (you)' : ''}</span>` : ''}
      ${icon ? `<span class="vico">${icon}</span>` : ''}
    </li>`;
  }).join('');
}

function liveUpdate() {
  // Just the talking rings, without rebuilding the list.
  const talk = talkingIds();
  let rebuild = false;
  document.querySelectorAll('#wv-list .vrow').forEach((row) => {
    const m = st && (st.members || []).find((x) => x.id === Number(row.dataset.id));
    const t = talk.has(Number(row.dataset.id)) && !(m && m.muted);
    const av = row.querySelector('.av');
    if (av.classList.contains('talk') !== t) {
      av.classList.toggle('talk', t);
      row.classList.toggle('quiet', !t);
      if (cfg.names === 'talking' && !editing) rebuild = true;
    }
  });
  if (rebuild) renderVoice();
}

/* ------------------------------------------------------------ pinned stream */

let viewer = null;

function unb64(str) {
  const s = atob(str);
  const out = new Uint8Array(s.length);
  for (let i = 0; i < s.length; i++) out[i] = s.charCodeAt(i);
  return out;
}

function closeViewer() {
  if (!viewer) return;
  try { viewer.decoder.state !== 'closed' && viewer.decoder.close(); } catch (_) { /* gone */ }
  viewer = null;
}

function openViewer(m) {
  closeViewer();
  const canvas = $('ws-canvas');
  const ctx = canvas.getContext('2d');
  ctx.fillStyle = '#000';
  ctx.fillRect(0, 0, canvas.width, canvas.height);
  hide($('ws-wait'), false);
  if (!window.VideoDecoder) {
    $('ws-wait').textContent = 'This system can\'t show the stream here.';
    viewer = { id: m.id, decoder: { state: 'closed' }, waitKey: true };
    return;
  }
  $('ws-wait').textContent = 'Waiting for the picture…';
  const v = { id: m.id, codec: m.screen.codec, waitKey: true, decoder: null };
  v.decoder = new VideoDecoder({
    output: (frame) => {
      if (canvas.width !== frame.displayWidth || canvas.height !== frame.displayHeight) {
        canvas.width = frame.displayWidth;
        canvas.height = frame.displayHeight;
      }
      ctx.drawImage(frame, 0, 0);
      frame.close();
      hide($('ws-wait'), true);
    },
    error: () => resetDecoder(),
  });
  try { v.decoder.configure({ codec: v.codec, optimizeForLatency: true }); } catch (_) { /* no codec */ }
  viewer = v;
  post({ cmd: 'voip.keyframe', id: m.id });
}

function resetDecoder() {
  if (!viewer || !viewer.decoder || !viewer.codec) return;
  try { viewer.decoder.state !== 'closed' && viewer.decoder.reset(); } catch (_) { /* gone */ }
  try { viewer.decoder.configure({ codec: viewer.codec, optimizeForLatency: true }); } catch (_) { /* gone */ }
  viewer.waitKey = true;
  post({ cmd: 'voip.keyframe', id: viewer.id });
}

function onVideo(msg) {
  const v = viewer;
  if (!v || v.id !== msg.from || !v.decoder || v.decoder.state === 'closed') return;
  if (v.waitKey && !msg.key) return;
  if (v.decoder.decodeQueueSize > 8) return resetDecoder();
  v.waitKey = false;
  try {
    v.decoder.decode(new EncodedVideoChunk({ type: msg.key ? 'key' : 'delta', timestamp: msg.ts, data: unb64(msg.data) }));
  } catch (_) {
    resetDecoder();
  }
}

function sharers() {
  return st && st.live ? (st.members || []).filter((m) => m.screen) : [];
}

function renderStream() {
  const pinned = st && st.pinned != null ? sharers().find((m) => m.id === st.pinned) : null;
  hide($('w-stream'), !pinned);
  if (!pinned) {
    closeViewer();
  } else {
    if (!viewer || viewer.id !== pinned.id) openViewer(pinned);
    $('ws-name').textContent = pinned.screen.label ? `${pinned.name} · ${pinned.screen.label}` : pinned.name;
  }
  $('w-stream').style.opacity = String(cfg.streamOpacity);
}

/* ------------------------------------------------------------ panel */

function renderPanel() {
  hide($('dim'), !editing);
  hide($('panel'), !editing);
  document.body.classList.toggle('edit', editing);
  if (!editing) return;
  const inVoice = !!(st && st.inVoice);
  hide($('voice-ctl'), !inVoice);
  hide($('voice-none'), inVoice);
  if (inVoice) {
    const ch = (st.channels || []).find((c) => c.id === st.channel);
    $('voice-where').textContent = `${st.room || 'Room'}${ch ? ` · ${ch.name}` : ''}`;
    $('b-mute').classList.toggle('bad', !!st.muted);
    $('b-mute').textContent = st.muted ? 'Unmute' : 'Mute';
    $('b-deaf').classList.toggle('bad', !!st.deaf);
    $('b-deaf').textContent = st.deaf ? 'Undeafen' : 'Deafen';
  }
  const list = sharers();
  hide($('streams-none'), list.length > 0);
  $('streams').innerHTML = list.map((m) => {
    const on = st.pinned === m.id;
    return `<div class="srow"><span class="live">LIVE</span><span class="nm">${esc(m.name)}${m.screen.label ? ` · ${esc(m.screen.label)}` : ''}</span>
      <button class="btn${on ? ' on' : ''}" data-pin="${m.id}">${on ? 'Pinned' : 'Pin over game'}</button></div>`;
  }).join('');
  $('o-voice').checked = !!cfg.showVoice;
  $('o-names').value = cfg.names;
  if (document.activeElement !== $('o-opacity')) $('o-opacity').value = Math.round(cfg.streamOpacity * 100);
  $('o-opacity-val').textContent = `${Math.round(cfg.streamOpacity * 100)}%`;
}

function render() {
  renderVoice();
  renderStream();
  renderPanel();
}

document.addEventListener('click', (e) => {
  const pin = e.target.closest('[data-pin]');
  if (pin) {
    const id = Number(pin.dataset.pin);
    post({ cmd: 'pin', id: st && st.pinned === id ? null : id });
    return;
  }
  const b = e.target.closest('[data-act]');
  if (!b) return;
  switch (b.dataset.act) {
    case 'close': post({ cmd: 'close' }); break;
    case 'unpin': post({ cmd: 'pin', id: null }); break;
    case 'mute': if (st) post({ cmd: 'voip.mute', muted: !st.muted, deaf: false }); break;
    case 'deaf': if (st) post({ cmd: 'voip.mute', muted: st.muted, deaf: !st.deaf }); break;
    case 'reset':
      layout = JSON.parse(JSON.stringify(LAYOUT_DEFAULT));
      saveLayout();
      place();
      break;
    default:
  }
});
// A click on the dimmed game goes back to it.
$('dim').addEventListener('mousedown', (e) => {
  if (e.target === $('dim')) post({ cmd: 'close' });
});
document.addEventListener('keydown', (e) => {
  if (e.key === 'Escape' && editing) post({ cmd: 'close' });
});
$('o-voice').addEventListener('change', () => post({ cmd: 'set', config: { showVoice: $('o-voice').checked } }));
$('o-names').addEventListener('change', () => post({ cmd: 'set', config: { names: $('o-names').value } }));
$('o-opacity').addEventListener('input', () => {
  cfg.streamOpacity = Number($('o-opacity').value) / 100;
  renderStream();
  $('o-opacity-val').textContent = `${$('o-opacity').value}%`;
  post({ cmd: 'set', config: { streamOpacity: cfg.streamOpacity } });
});

/* ------------------------------------------------------------ host */

window.onHost = (msg) => {
  switch (msg.type) {
    case 'config':
      cfg = { ...cfg, ...msg.config };
      if (msg.hotkey) $('hk').textContent = msg.hotkey;
      render();
      break;
    case 'state':
      st = msg;
      if (msg.speaking) speaking = new Set(msg.speaking);
      render();
      break;
    case 'pinned':
      if (st) st.pinned = msg.id;
      render();
      break;
    case 'edit':
      editing = !!msg.on;
      drag = null;
      render();
      break;
    case 'voipLive':
      if (msg.speaking) speaking = new Set(msg.speaking);
      if (msg.talking != null) meTalking = msg.talking;
      liveUpdate();
      break;
    case 'video':
      onVideo(msg);
      break;
    case 'resetLayout':
      layout = JSON.parse(JSON.stringify(LAYOUT_DEFAULT));
      saveLayout();
      place();
      break;
    default:
  }
};

place();
render();
post({ cmd: 'ready' });
