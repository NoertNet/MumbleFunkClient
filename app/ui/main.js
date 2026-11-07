'use strict';
const { invoke } = window.__TAURI__.core;
const { listen } = window.__TAURI__.event;
const $ = (id) => document.getElementById(id);

let state = null;        // letzter Serverzustand (Kanäle, Benutzer)
let speaking = [];       // Sessions, die gerade zu hören sind
let pttDown = false;
let latencyMs = null;    // letzte Ping-Laufzeit
let latencyAt = 0;       // Zeitpunkt der letzten Antwort
let username = '';
let listOpen = false;

const STALE_MS = 13000;  // Pings kommen alle 4 s; danach gilt die Verbindung als ohne Signal

/* ---------- Anzeige-Hilfen ---------- */

function show(screen) {
  $('connect').hidden = screen !== 'connect';
  $('radio').hidden = screen !== 'radio';
}

function showError(msg) {
  $('error').textContent = msg || '';
  $('error').hidden = !msg;
}

// Balken nach Laufzeit zum Server: je kürzer, desto mehr Balken.
function levelFor(ms) {
  if (ms == null) return 0;
  if (ms < 50) return 4;
  if (ms < 100) return 3;
  if (ms < 200) return 2;
  if (ms < 500) return 1;
  return 0;
}

function renderSignal() {
  const stale = latencyMs == null || Date.now() - latencyAt > STALE_MS;
  const level = stale ? 0 : levelFor(latencyMs);
  const el = $('signal');
  el.dataset.level = String(level);
  $('ms').textContent = stale ? (latencyMs == null ? '— ms' : 'kein Signal') : `${latencyMs} ms`;
  const words = ['kein Signal', 'schwach', 'mäßig', 'gut', 'sehr gut'];
  el.setAttribute('aria-label', `Verbindung: ${words[level]}${stale ? '' : `, ${latencyMs} Millisekunden`}`);
}

function othersSpeaking() {
  return speaking.filter((s) => s !== state?.ownSession);
}

function nameOf(session) {
  for (const c of state?.channels ?? []) {
    const u = c.users.find((u) => u.session === session);
    if (u) return u.name;
  }
  return `#${session}`;
}

function currentMode() {
  const rx = othersSpeaking().length > 0;
  if (pttDown && rx) return 'busy';
  if (pttDown) return 'tx';
  if (rx) return 'rx';
  return 'idle';
}

function sortedChannels() {
  return [...(state?.channels ?? [])].sort((a, b) => a.id - b.id);
}

function render() {
  if (!state) return;
  const channels = sortedChannels();
  const idx = channels.findIndex((c) => c.id === state.ownChannel);
  const own = channels[idx];
  const mode = currentMode();
  const others = othersSpeaking();

  $('radio').dataset.mode = mode;
  $('lamp-net').classList.add('on');
  $('lamp-rx').classList.toggle('on', mode === 'rx' || mode === 'busy');
  $('lamp-tx').classList.toggle('on', mode === 'tx');

  $('chan-idx').textContent = idx >= 0 ? `KANAL ${idx + 1}/${channels.length}` : 'KANAL';
  $('chan-name').textContent = own ? (own.name || 'Root') : '—';
  $('mode').textContent = { idle: 'BEREIT', tx: 'SENDET', rx: 'EMPFANG', busy: 'BELEGT' }[mode];
  $('who-text').textContent = {
    idle: `ICH: ${username}`,
    tx: `ICH: ${username}`,
    rx: others.map(nameOf).join(', '),
    busy: `${others.map(nameOf).join(', ')} SENDET`,
  }[mode];

  // Teilnehmerliste im Display
  $('users').replaceChildren(...(own?.users ?? []).map((u) => {
    const li = document.createElement('li');
    li.textContent = u.name;
    if (u.session === state.ownSession) li.classList.add('me');
    if (speaking.includes(u.session)) li.classList.add('speaking');
    return li;
  }));

  // Kanalliste (ausklappbar)
  $('channels').replaceChildren(...channels.map((c) => {
    const li = document.createElement('li');
    if (c.id === state.ownChannel) li.className = 'current';
    const name = document.createElement('span');
    name.textContent = c.name || 'Root';
    const n = document.createElement('small');
    n.textContent = c.users.length ? `${c.users.length} TN` : '';
    li.append(name, n);
    li.addEventListener('click', () => joinChannel(c.id));
    return li;
  }));

  $('ptt').classList.toggle('down', pttDown);
  $('ptt').setAttribute('aria-pressed', String(pttDown));
  $('ptt-label').textContent = pttDown ? (mode === 'busy' ? 'BELEGT' : 'SENDET') : 'PTT';
  renderSignal();
}

/* ---------- Aktionen ---------- */

function joinChannel(id) {
  if (!state || id === state.ownChannel) return;
  invoke('join_channel', { id }).catch((e) => showError(String(e)));
}

function stepChannel(dir) {
  const list = sortedChannels();
  if (list.length < 2) return;
  const i = Math.max(0, list.findIndex((c) => c.id === state.ownChannel));
  joinChannel(list[(i + dir + list.length) % list.length].id);
}

function setPtt(down) {
  if (down === pttDown || $('radio').hidden) return;
  pttDown = down;
  try { if (down && navigator.vibrate) navigator.vibrate(12); } catch { /* optional */ }
  render();
  invoke('ptt', { down }).catch((e) => { pttDown = false; render(); showError(String(e)); });
}

function resetRadio() {
  setPtt(false);
  state = null; speaking = []; latencyMs = null; latencyAt = 0; listOpen = false;
  $('channels').hidden = true;
  $('chan-list-btn').setAttribute('aria-expanded', 'false');
  $('radio').dataset.mode = 'idle';
  $('lamp-net').classList.remove('on');
}

/* ---------- Verbinden ---------- */

function connectArgs(trustNewCert) {
  return {
    host: $('host').value.trim(),
    port: parseInt($('port').value, 10),
    username: $('username').value.trim(),
    password: $('password').value || null,
    rememberPassword: $('remember').checked,
    trustNewCert,
  };
}

async function doConnect(trustNewCert) {
  showError('');
  $('pin-box').hidden = true;
  $('connect-btn').disabled = true;
  $('busy').textContent = `VERBINDE ${$('host').value.trim().toUpperCase()}:${$('port').value} …`;
  $('busy').hidden = false;
  $('idle-msg').hidden = true;
  const args = connectArgs(trustNewCert);
  try {
    state = await invoke('connect', { args });
    username = args.username;
    $('server-name').textContent = args.host;
    latencyMs = null;
    render();
    show('radio');
  } catch (e) {
    if (e === 'PIN_MISMATCH') $('pin-box').hidden = false;
    else showError(String(e));
  } finally {
    $('connect-btn').disabled = false;
    $('busy').hidden = true;
    $('idle-msg').hidden = false;
  }
}

/* ---------- Gespeicherte Server ---------- */

let profiles = [];
const profileKey = (p) => `${p.username}@${p.host}:${p.port}`;

function fillForm(p) {
  $('host').value = p?.host ?? '';
  $('port').value = p?.port ?? 64738;
  $('username').value = p?.username ?? '';
  $('password').value = p?.password ?? '';
  $('remember').checked = p ? p.password != null : true;
}

function applySettings(s, selectKey) {
  profiles = s.profiles ?? [];
  $('saved').hidden = profiles.length === 0;
  const sel = $('profile-select');
  sel.replaceChildren(...profiles.map((p) => {
    const o = document.createElement('option');
    o.value = profileKey(p);
    o.textContent = `${p.username} @ ${p.host}:${p.port}`;
    return o;
  }));
  const key = selectKey ?? s.last ?? (profiles[0] && profileKey(profiles[0]));
  const cur = profiles.find((p) => profileKey(p) === key);
  if (cur) sel.value = key;
  fillForm(cur);
}

/* ---------- Ereignisse verdrahten ---------- */

// Zeiger-Ereignisse decken Maus, Touch und Stift ab; "capture" hält das Loslassen auch außerhalb des Knopfs.
const ptt = $('ptt');
ptt.addEventListener('pointerdown', (e) => { ptt.setPointerCapture(e.pointerId); setPtt(true); });
for (const t of ['pointerup', 'pointercancel', 'lostpointercapture']) ptt.addEventListener(t, () => setPtt(false));
ptt.addEventListener('contextmenu', (e) => e.preventDefault());
window.addEventListener('keydown', (e) => {
  if (e.code === 'Space' && !e.repeat && !['INPUT', 'SELECT', 'TEXTAREA'].includes(document.activeElement?.tagName)) {
    e.preventDefault();
    setPtt(true);
  }
});
window.addEventListener('keyup', (e) => { if (e.code === 'Space') setPtt(false); });
// Sender nie offen lassen, wenn das Fenster den Fokus verliert.
window.addEventListener('blur', () => setPtt(false));
document.addEventListener('visibilitychange', () => { if (document.hidden) setPtt(false); });

$('chan-prev').addEventListener('click', () => stepChannel(-1));
$('chan-next').addEventListener('click', () => stepChannel(1));
$('chan-list-btn').addEventListener('click', () => {
  listOpen = !listOpen;
  $('channels').hidden = !listOpen;
  $('chan-list-btn').setAttribute('aria-expanded', String(listOpen));
});

$('connect-form').addEventListener('submit', (e) => { e.preventDefault(); doConnect(false); });
$('trust-btn').addEventListener('click', () => doConnect(true));
$('profile-select').addEventListener('change', (e) => fillForm(profiles.find((p) => profileKey(p) === e.target.value)));
$('forget-btn').addEventListener('click', async () => {
  const key = $('profile-select').value;
  if (key) applySettings(await invoke('forget_profile', { key }));
});
$('disconnect-btn').addEventListener('click', async () => {
  setPtt(false);
  await invoke('disconnect');
  resetRadio();
  show('connect');
  invoke('load_settings').then((s) => applySettings(s)).catch(() => {});
});

listen('state', (e) => { state = e.payload; render(); });
listen('speaking', (e) => { speaking = e.payload; render(); });
listen('latency', (e) => { latencyMs = e.payload; latencyAt = Date.now(); renderSignal(); });
listen('notice', (e) => { $('who-text').textContent = String(e.payload).toUpperCase(); });
listen('plugin', (e) => console.debug('plugin data', e.payload));
listen('disconnected', (e) => {
  resetRadio();
  show('connect');
  showError(e.payload ? `Verbindung getrennt: ${e.payload}` : 'Verbindung getrennt.');
});

// Ohne neue Ping-Antwort muss die Anzeige von selbst auf "kein Signal" fallen.
setInterval(() => { if (!$('radio').hidden) renderSignal(); }, 1000);

invoke('load_settings').then((s) => applySettings(s)).catch(() => {});
