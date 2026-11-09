'use strict';
const { invoke } = window.__TAURI__.core;
const { listen } = window.__TAURI__.event;
const $ = (id) => document.getElementById(id);

let state = null;        // letzter Serverzustand (Kanäle, Benutzer)
let speaking = [];       // Sessions, die gerade zu hören sind
let pttDown = false;     // wir senden gerade (Fenster oder globaler Hotkey), vom Programm gemeldet
let uiHeld = false;      // Knopf/Leertaste im Fenster wird gehalten
let reconnecting = false; // Verbindung verloren, wird automatisch neu aufgebaut
let latencyMs = null;    // letzte Ping-Laufzeit
let latencyAt = 0;       // Zeitpunkt der letzten Antwort
let username = '';
let listOpen = false;
let version = '';
let pendingUpdate = null;

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
  const mode = reconnecting ? 'lost' : currentMode();
  const others = othersSpeaking();

  $('radio').dataset.mode = mode;
  $('lamp-net').classList.toggle('on', !reconnecting);
  $('lamp-rx').classList.toggle('on', mode === 'rx' || mode === 'busy');
  $('lamp-tx').classList.toggle('on', mode === 'tx');

  $('chan-idx').textContent = idx >= 0 ? `KANAL ${idx + 1}/${channels.length}` : 'KANAL';
  $('chan-name').textContent = own ? (own.name || 'Root') : '—';
  $('mode').textContent = { idle: 'BEREIT', tx: 'SENDET', rx: 'EMPFANG', busy: 'BELEGT', lost: 'VERBINDUNG VERLOREN' }[mode];
  $('who-text').textContent = {
    idle: `ICH: ${username}`,
    tx: `ICH: ${username}`,
    rx: others.map(nameOf).join(', '),
    busy: `${others.map(nameOf).join(', ')} SENDET`,
    lost: 'VERBINDE NEU …',
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

  $('ptt').disabled = reconnecting;
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
  if (down === uiHeld || $('radio').hidden) return;
  uiHeld = down;
  if (down) {
    pttDown = true; // sofort sichtbar; das Programm bestätigt gleich per "ptt-state"
    try { if (navigator.vibrate) navigator.vibrate(12); } catch { /* optional */ }
  }
  render();
  invoke('ptt', { down }).catch((e) => { uiHeld = false; pttDown = false; render(); showError(String(e)); });
}

function resetRadio() {
  setPtt(false);
  uiHeld = false; pttDown = false; reconnecting = false;
  state = null; speaking = []; latencyMs = null; latencyAt = 0; listOpen = false;
  $('channels').hidden = true;
  $('chan-list-btn').setAttribute('aria-expanded', 'false');
  $('radio').dataset.mode = 'idle';
  $('lamp-net').classList.remove('on');
  eqTarget.fill(0);
  $('eq-devices').textContent = '';
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
    setEqDevices(state.audio);
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

/* ---------- Equalizer ---------- */

const EQ_BANDS = 16;
const eqEls = [];
const eqCur = new Float32Array(EQ_BANDS);
const eqTarget = new Float32Array(EQ_BANDS);
const eqShown = new Int16Array(EQ_BANDS).fill(-1);
let eqLastEvent = 0;
let eqRunning = false;

function buildEq() {
  for (let i = 0; i < EQ_BANDS; i++) {
    const el = document.createElement('i');
    $('eq-bars').append(el);
    eqEls.push(el);
  }
}

// Messwerte kommen nur, solange Ton da ist (ca. 25 pro Sekunde); dazwischen und danach klingt die
// Anzeige selbst ab. Die Animation läuft nur, solange noch etwas leuchtet (spart Akku im Leerlauf).
function onLevels(p) {
  eqLastEvent = performance.now();
  for (let i = 0; i < EQ_BANDS; i++) eqTarget[i] = Math.min(1, (p.bands[i] || 0) / 255);
  if (!eqRunning) { eqRunning = true; requestAnimationFrame(eqFrame); }
}

function eqFrame(now) {
  const silent = now - eqLastEvent > 150;
  let active = false;
  for (let i = 0; i < EQ_BANDS; i++) {
    if (silent) eqTarget[i] = 0;
    const t = eqTarget[i];
    const c = eqCur[i];
    // schnell hoch, langsamer runter: wie bei einem echten Analyser
    let next = t > c ? c + (t - c) * 0.65 : c + (t - c) * 0.16;
    if (t === 0 && next < 0.015) next = 0;
    eqCur[i] = next;
    if (next > 0 || t > 0) active = true;
    const pct = Math.round(next * 100);
    if (pct !== eqShown[i]) {
      eqShown[i] = pct;
      eqEls[i].style.setProperty('--cut', `${100 - pct}%`);
    }
  }
  if (active) requestAnimationFrame(eqFrame); else eqRunning = false;
}

function setEqDevices(audio) {
  const el = $('eq-devices');
  if (!audio || (!audio.input && !audio.output)) { el.textContent = ''; return; }
  el.textContent = `MIC ${audio.input || '–'} · LS ${audio.output || '–'}`;
  const warn = (audio.notes || []).length > 0;
  el.classList.toggle('warn', warn);
  el.title = warn ? `Hinweis: ${audio.notes.join('; ')}` : '';
}

/* ---------- Audio-Geräte (nur Computer) ---------- */

function fillSelect(sel, names, def, chosen) {
  const first = document.createElement('option');
  first.value = '';
  first.textContent = def ? `Standard (${def})` : 'Standard';
  const opts = [first, ...names.map((n) => {
    const o = document.createElement('option');
    o.value = n;
    o.textContent = n;
    return o;
  })];
  if (chosen && !names.includes(chosen)) {
    const o = document.createElement('option');
    o.value = chosen;
    o.textContent = `${chosen} (nicht gefunden)`;
    opts.push(o);
  }
  sel.replaceChildren(...opts);
  sel.value = chosen ?? '';
}

async function loadDevices() {
  try {
    const d = await invoke('list_audio_devices');
    $('audio-box').hidden = !d.supported;
    if (!d.supported) return;
    fillSelect($('in-dev'), d.inputs, d.defaultInput, d.selectedInput);
    fillSelect($('out-dev'), d.outputs, d.defaultOutput, d.selectedOutput);
  } catch {
    $('audio-box').hidden = true;
  }
}

function saveDevices() {
  invoke('set_audio_devices', { input: $('in-dev').value || null, output: $('out-dev').value || null }).catch(() => {});
}

$('in-dev').addEventListener('change', saveDevices);
$('out-dev').addEventListener('change', saveDevices);
$('dev-refresh').addEventListener('click', loadDevices);

/* ---------- Globaler PTT-Hotkey (nur Computer) ---------- */

let hotkeyRecording = false;

function hotkeyMessage(text, warn) {
  $('hotkey-msg').textContent = text || '';
  $('hotkey-msg').classList.toggle('warn', !!warn);
}

function renderHotkey(d) {
  $('hotkey-box').hidden = !d.supported;
  if (!d.supported) return;
  $('hotkey-current').textContent = d.display ? `Aktuell: ${d.display}` : 'Aktuell: keiner';
  $('hotkey-clear').hidden = !d.display;
  hotkeyMessage(d.error, !!d.error);
  $('ptt-hint').textContent = d.display ? `gedrückt halten · Leertaste · Hotkey ${d.display}` : 'gedrückt halten · Leertaste';
}

async function loadHotkey() {
  try { renderHotkey(await invoke('get_hotkey')); } catch { $('hotkey-box').hidden = true; }
}

// Buchstaben nach dem, was auf der Taste steht (Tastaturlayout!), nicht nach ihrer Position: auf einer
// deutschen Tastatur meldet event.code "KeyY" für die Taste mit dem Z. Windows registriert Hotkeys nach
// dem Buchstaben der Taste.
async function letterFor(e) {
  if (/^[a-zA-Z]$/.test(e.key)) return e.key.toUpperCase();
  try { // AltGr-Kombinationen liefern in e.key ein Sonderzeichen; dann die Layout-Tabelle fragen
    const map = await navigator.keyboard?.getLayoutMap?.();
    const ch = map?.get(e.code);
    if (ch && /^[a-z]$/i.test(ch)) return ch.toUpperCase();
  } catch { /* nicht verfügbar */ }
  return null;
}

async function codeFor(e) {
  if (/^F([1-9]|1\d|2[0-4])$/.test(e.code)) return e.code;   // Funktionstasten sind layoutunabhängig
  if (e.code === 'Pause' || e.code === 'ScrollLock') return e.code;
  if (/^Digit\d$/.test(e.code)) return e.code;
  const letter = await letterFor(e);
  return letter ? `Key${letter}` : null;
}

const MODIFIER_KEYS = ['Control', 'Alt', 'Shift', 'Meta', 'AltGraph', 'OS'];

async function stopRecording() {
  hotkeyRecording = false;
  window.removeEventListener('keydown', onHotkeyKey, true);
  $('hotkey-record').textContent = 'Taste aufnehmen';
  $('hotkey-record').classList.remove('recording');
}

async function onHotkeyKey(e) {
  if (!hotkeyRecording) return;
  e.preventDefault();
  e.stopPropagation();
  if (e.repeat || MODIFIER_KEYS.includes(e.key)) return; // erst die eigentliche Taste abwarten
  if (e.key === 'Escape') {
    await stopRecording();
    renderHotkey(await invoke('resume_hotkey'));
    return;
  }
  if (e.code === 'Space') {
    hotkeyMessage('Die Leertaste ist als globaler Hotkey ausgeschlossen: sie wäre sonst in allen Programmen blockiert.', true);
    return; // weiter aufnehmen
  }
  const code = await codeFor(e);
  if (!code) {
    hotkeyMessage('Diese Taste wird nicht unterstützt. Nimm eine F-Taste, Pause, Rollen oder Buchstabe/Ziffer mit Zusatztasten.', true);
    return; // weiter aufnehmen
  }
  await stopRecording();
  try {
    renderHotkey(await invoke('set_hotkey', { spec: { code, ctrl: e.ctrlKey, alt: e.altKey, shift: e.shiftKey, meta: e.metaKey } }));
    hotkeyMessage('Gespeichert.', false);
  } catch (err) {
    const msg = String(err);
    renderHotkey(await invoke('resume_hotkey')); // der bisherige Hotkey bleibt aktiv
    hotkeyMessage(msg, true);
  }
}

$('hotkey-record').addEventListener('click', async () => {
  if (hotkeyRecording) { // zweiter Klick bricht ab
    await stopRecording();
    renderHotkey(await invoke('resume_hotkey'));
    return;
  }
  // Der bisherige Hotkey wird freigegeben, sonst verschluckt ihn das System und das Fenster sieht ihn nie.
  await invoke('pause_hotkey');
  hotkeyRecording = true;
  $('hotkey-record').textContent = 'Abbrechen (Esc)';
  $('hotkey-record').classList.add('recording');
  hotkeyMessage('Drücke jetzt die gewünschte Taste oder Tastenkombination …', false);
  window.addEventListener('keydown', onHotkeyKey, true);
});

$('hotkey-clear').addEventListener('click', async () => {
  try { renderHotkey(await invoke('set_hotkey', { spec: null })); hotkeyMessage('Hotkey entfernt.', false); } catch (err) { hotkeyMessage(String(err), true); }
});

/* ---------- Android-Diagnose ---------- */

function diagMessage(text) { $('diag-msg').textContent = text || ''; }

async function loadDiag() {
  try {
    const text = await invoke('get_diagnostics');
    if (text == null) return false;      // kein Android
    $('diag-box').hidden = false;
    $('diag-text').textContent = text;
    return true;
  } catch { return false; }
}

$('diag-box').addEventListener('toggle', () => { if ($('diag-box').open) loadDiag(); });
$('diag-refresh').addEventListener('click', async () => { await loadDiag(); diagMessage('Aktualisiert.'); });
$('diag-copy').addEventListener('click', async () => {
  const text = $('diag-text').textContent;
  try {
    await navigator.clipboard.writeText(text);
    diagMessage('Kopiert. Jetzt in eine Nachricht einfügen.');
  } catch {
    // Ersatz, falls die Zwischenablage-Schnittstelle gesperrt ist: Text markieren
    const range = document.createRange();
    range.selectNodeContents($('diag-text'));
    const sel = window.getSelection();
    sel.removeAllRanges(); sel.addRange(range);
    diagMessage('Text ist markiert: lange drücken und „Kopieren“ wählen.');
  }
});
$('battery-btn').addEventListener('click', async () => {
  const ok = await invoke('request_battery_exemption').catch(() => false);
  diagMessage(ok ? 'Android-Dialog geöffnet. Dort „Zulassen“ wählen.' : 'Der Dialog ließ sich nicht öffnen. Einstellungen → Apps → Mumble Funk → Akku → „Uneingeschränkt“.');
});

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

/* ---------- Version und Updates ---------- */

function setBootMsg(text) {
  $('idle-msg').textContent = text;
}

function showUpdate(u) {
  pendingUpdate = u;
  $('update-text').textContent = `Update v${u.version} verfügbar (installiert: v${version})`;
  $('update').hidden = false;
  document.body.classList.add('has-update');
}

function hideUpdate() {
  $('update').hidden = true;
  document.body.classList.remove('has-update');
}

// Beim Start still (ohne Netz soll nichts stören); manuell mit Rückmeldung.
async function checkUpdate(manual) {
  if (manual) setBootMsg('PRÜFE UPDATE …');
  try {
    const r = await invoke('check_update');
    if (r.update) {
      showUpdate(r.update);
      if (manual) setBootMsg(`UPDATE V${r.update.version}`);
    } else if (manual) {
      setBootMsg(r.devBuild ? 'ENTWICKLUNGSVERSION' : 'AKTUELL');
    }
  } catch (e) {
    console.debug('update check failed', e);
    if (manual) setBootMsg('PRÜFUNG FEHLGESCHLAGEN');
  }
}

async function initVersion() {
  try {
    version = await invoke('app_version');
    $('boot-ver').textContent = `V${version}`;
    $('version-plate').textContent = `MUMBLE·FUNK MF-1 · V${version}`;
    $('version-plate-radio').textContent = `MUMBLE·FUNK MF-1 · V${version}`;
  } catch { /* Anzeige ist optional */ }
}

$('update-get').addEventListener('click', () => {
  if (!pendingUpdate) return;
  // Direkter Download; falls das System den Link nicht öffnet, die Release-Seite.
  invoke('open_update_url', { url: pendingUpdate.url })
    .catch(() => invoke('open_update_url', { url: pendingUpdate.page }))
    .catch((e) => showError(String(e)));
});
$('update-later').addEventListener('click', hideUpdate);
$('update-check').addEventListener('click', () => checkUpdate(true));

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
document.addEventListener('visibilitychange', () => {
  if (document.hidden) setPtt(false);
  else invoke('ui_visible').catch(() => {}); // im Hintergrund wird seltener gepingt: Anzeige sofort auffrischen
});

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
  loadDevices();
});

listen('state', (e) => { state = e.payload; render(); });
listen('speaking', (e) => { speaking = e.payload; render(); });
listen('reconnecting', () => {
  // Alles Laufende ist beendet (das Programm hat PTT freigegeben); nach dem Wiederaufbau neu drücken.
  reconnecting = true; uiHeld = false; pttDown = false; speaking = []; latencyMs = null; latencyAt = 0;
  render();
});
listen('reconnected', () => { reconnecting = false; render(); });
listen('ptt-state', (e) => { pttDown = !!e.payload; render(); });
listen('levels', (e) => onLevels(e.payload));
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

buildEq();
loadDevices();
loadHotkey();
loadDiag();

// Version anzeigen, danach einmal still nach einem Update sehen.
initVersion().then(() => checkUpdate(false));
