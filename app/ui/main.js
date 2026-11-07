'use strict';
const { invoke } = window.__TAURI__.core;
const { listen } = window.__TAURI__.event;
const $ = (id) => document.getElementById(id);

let state = null;          // last StateDto from the backend
let speaking = [];         // session ids currently audible
let pttDown = false;
let lastArgs = null;

function show(screen) {
  $('connect').hidden = screen !== 'connect';
  $('radio').hidden = screen !== 'radio';
}

function showError(msg) {
  $('error').textContent = msg || '';
  $('error').hidden = !msg;
}

function connectArgs(trustNewCert) {
  return {
    host: $('host').value.trim(),
    port: parseInt($('port').value, 10),
    username: $('username').value.trim(),
    password: $('password').value || null,
    trustNewCert,
  };
}

async function doConnect(trustNewCert) {
  showError('');
  $('pin-box').hidden = true;
  $('connect-btn').disabled = true;
  lastArgs = connectArgs(trustNewCert);
  try {
    state = await invoke('connect', { args: lastArgs });
    $('server-name').textContent = lastArgs.host;
    render();
    show('radio');
  } catch (e) {
    if (e === 'PIN_MISMATCH') $('pin-box').hidden = false;
    else showError(String(e));
  } finally {
    $('connect-btn').disabled = false;
  }
}

function nameOf(session) {
  for (const c of state?.channels ?? []) {
    const u = c.users.find((u) => u.session === session);
    if (u) return u.name;
  }
  return `#${session}`;
}

function depth(c, byId) {
  let d = 0;
  for (let p = c.parent; p != null && byId.has(p) && d < 8; p = byId.get(p).parent) d++;
  return d;
}

function render() {
  if (!state) return;
  $('welcome').textContent = (state.welcome || '').replace(/<[^>]*>/g, '').trim();

  const byId = new Map(state.channels.map((c) => [c.id, c]));
  const ch = $('channels');
  ch.replaceChildren(...state.channels.map((c) => {
    const li = document.createElement('li');
    li.style.paddingLeft = `${14 + depth(c, byId) * 16}px`;
    if (c.id === state.ownChannel) li.className = 'current';
    const name = document.createElement('span');
    name.textContent = c.name || 'Root';
    const n = document.createElement('small');
    n.textContent = c.users.length ? `${c.users.length}` : '';
    li.append(name, n);
    li.addEventListener('click', () => {
      if (c.id !== state.ownChannel) invoke('join_channel', { id: c.id }).catch((e) => showError(String(e)));
    });
    return li;
  }));

  const mine = byId.get(state.ownChannel);
  $('users').replaceChildren(...(mine?.users ?? []).map((u) => {
    const li = document.createElement('li');
    li.textContent = u.name;
    if (speaking.includes(u.session)) li.className = 'speaking';
    return li;
  }));
  renderStatus();
}

function renderStatus() {
  const others = speaking.filter((s) => s !== state?.ownSession);
  const el = $('status');
  el.className = '';
  if (pttDown) {
    el.textContent = 'Du sendest';
    el.classList.add('tx');
  } else if (others.length) {
    el.textContent = `${others.map(nameOf).join(', ')} spricht`;
    el.classList.add('rx');
  } else {
    el.textContent = 'Kanal frei';
  }
  $('ptt').classList.toggle('down', pttDown);
  $('ptt').classList.toggle('rx', !pttDown && others.length > 0);
  $('ptt-label').textContent = pttDown ? 'SENDET' : 'SPRECHEN';
}

function setPtt(down) {
  if (down === pttDown || $('radio').hidden) return;
  pttDown = down;
  renderStatus();
  invoke('ptt', { down }).catch((e) => { pttDown = false; renderStatus(); showError(String(e)); });
}

// Pointer events cover mouse, touch and pen; capture keeps the release even if the finger slides off.
const ptt = $('ptt');
ptt.addEventListener('pointerdown', (e) => { ptt.setPointerCapture(e.pointerId); setPtt(true); });
for (const t of ['pointerup', 'pointercancel', 'lostpointercapture']) ptt.addEventListener(t, () => setPtt(false));
ptt.addEventListener('contextmenu', (e) => e.preventDefault());
window.addEventListener('keydown', (e) => {
  if (e.code === 'Space' && !e.repeat && !['INPUT', 'TEXTAREA'].includes(document.activeElement?.tagName)) {
    e.preventDefault();
    setPtt(true);
  }
});
window.addEventListener('keyup', (e) => { if (e.code === 'Space') setPtt(false); });
// Never leave the transmitter open when the window loses focus.
window.addEventListener('blur', () => setPtt(false));
document.addEventListener('visibilitychange', () => { if (document.hidden) setPtt(false); });

$('connect-form').addEventListener('submit', (e) => { e.preventDefault(); doConnect(false); });
$('trust-btn').addEventListener('click', () => doConnect(true));
$('disconnect-btn').addEventListener('click', async () => {
  setPtt(false);
  await invoke('disconnect');
  state = null; speaking = [];
  show('connect');
});

listen('state', (e) => { state = e.payload; render(); });
listen('speaking', (e) => { speaking = e.payload; render(); });
listen('notice', (e) => { $('status').textContent = String(e.payload); });
listen('plugin', (e) => console.debug('plugin data', e.payload));
listen('disconnected', (e) => {
  setPtt(false);
  state = null; speaking = [];
  show('connect');
  showError(e.payload ? `Verbindung getrennt: ${e.payload}` : 'Verbindung getrennt.');
});

invoke('load_settings').then((s) => {
  if (s.host) $('host').value = s.host;
  if (s.port) $('port').value = s.port;
  if (s.username) $('username').value = s.username;
}).catch(() => {});
