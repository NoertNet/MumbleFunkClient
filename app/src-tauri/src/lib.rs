//! Tauri shell: wires `funk-core` (protocol, voice) and `funk-audio` (devices) to the UI.

mod android;
#[cfg(desktop)]
mod hotkey;
mod ptt_sources;
mod session;
mod settings;
mod status;
mod update;

use std::sync::Mutex;

use ptt_sources::{Effect, PttSources};
use session::{Session, StateDto};
use tauri::{AppHandle, Emitter, Manager, State};

#[derive(Default)]
struct App {
    session: Mutex<Option<Session>>,
    /// Who holds push-to-talk right now (window button/keys and the global hotkey).
    sources: Mutex<PttSources>,
    /// Why the saved global hotkey could not be registered (shown on the start screen).
    hotkey_error: Mutex<Option<String>>,
}

/// Applies a PTT change to the live session and tells the UI the effective state. The UI renders from
/// this event, because the hotkey can fire while the window is minimised.
fn apply_ptt(app: &AppHandle, effect: Effect) {
    let st = app.state::<App>();
    if effect != Effect::None {
        if let Some(s) = st.session.lock().unwrap().as_ref() {
            s.ptt(effect == Effect::Start);
        }
    }
    let transmitting = st.sources.lock().unwrap().transmitting();
    let _ = app.emit("ptt-state", transmitting);
}

pub(crate) fn reset_ptt(app: &AppHandle) {
    let eff = app.state::<App>().sources.lock().unwrap().reset();
    apply_ptt(app, eff);
}

#[derive(serde::Deserialize)]
#[serde(rename_all = "camelCase")]
struct ConnectArgs {
    host: String,
    port: u16,
    username: String,
    password: Option<String>,
    remember_password: bool,
    /// Accept a changed certificate and replace the stored pin.
    trust_new_cert: bool,
}

/// Result of `connect`: the server state plus the audio devices that were opened.
#[derive(serde::Serialize)]
struct ConnectDto {
    #[serde(flatten)]
    state: StateDto,
    audio: session::AudioDto,
}

#[tauri::command]
async fn connect(app: AppHandle, state: State<'_, App>, args: ConnectArgs) -> Result<ConnectDto, String> {
    if let Some(old) = state.session.lock().unwrap().take() {
        old.close();
    }
    reset_ptt(&app);
    let s = Session::connect(app, args).await?;
    let dto = ConnectDto { state: s.dto(), audio: s.audio.clone() };
    *state.session.lock().unwrap() = Some(s);
    Ok(dto)
}

#[tauri::command]
fn disconnect(app: AppHandle, state: State<'_, App>) {
    if let Some(s) = state.session.lock().unwrap().take() {
        s.close();
    }
    reset_ptt(&app);
}

#[tauri::command]
async fn join_channel(state: State<'_, App>, id: u32) -> Result<(), String> {
    let client = state.session.lock().unwrap().as_ref().map(|s| s.client());
    client.ok_or("not connected")?.join_channel(id).await.map_err(|e| e.to_string())
}

/// Push-to-talk from the window (button, space bar). `down=true` starts a transmission, `false` ends
/// it with a terminator packet, unless the global hotkey is still held.
#[tauri::command]
fn ptt(app: AppHandle, state: State<'_, App>, down: bool) -> Result<(), String> {
    match state.session.lock().unwrap().as_ref() {
        None => return Err("not connected".into()),
        // Nothing to transmit on while the connection is being re-established. Releasing is always fine.
        Some(s) if down && s.is_reconnecting() => return Err("Verbindung wird neu aufgebaut".into()),
        Some(_) => {}
    }
    let eff = state.sources.lock().unwrap().ui(down);
    apply_ptt(&app, eff);
    Ok(())
}

// ---------- global PTT hotkey (desktop only) ----------

#[derive(serde::Serialize)]
#[serde(rename_all = "camelCase")]
struct HotkeyDto {
    /// False on phones.
    supported: bool,
    /// Readable form of the saved hotkey, e.g. "Strg+Alt+T".
    display: Option<String>,
    /// Why registration failed (key taken by another program, reserved combination, ...).
    error: Option<String>,
}

#[cfg(desktop)]
fn register_error(raw: &str) -> String {
    if raw.to_lowercase().contains("already") {
        "Diese Tastenkombination ist schon von einem anderen Programm belegt.".into()
    } else {
        format!("Hotkey konnte nicht registriert werden: {raw}")
    }
}

/// (Re-)registers the saved hotkey with the system and remembers a failure for the start screen.
#[cfg(desktop)]
fn register_saved(app: &AppHandle) -> HotkeyDto {
    use tauri_plugin_global_shortcut::GlobalShortcutExt;
    let saved = settings::load(app).ptt_hotkey;
    let gs = app.global_shortcut();
    let _ = gs.unregister_all();
    let result: Result<(), String> = match &saved {
        None => Ok(()),
        Some(spec) => hotkey::shortcut(spec).and_then(|sc| gs.register(sc).map_err(|e| register_error(&e.to_string()))),
    };
    let error = result.err();
    *app.state::<App>().hotkey_error.lock().unwrap() = error.clone();
    HotkeyDto { supported: true, display: saved.as_ref().map(hotkey::describe), error }
}

/// Called by the system hotkey: press starts, release stops (reports can repeat; see `PttSources`).
#[cfg(desktop)]
fn on_hotkey(app: &AppHandle, down: bool) {
    let st = app.state::<App>();
    if st.session.lock().unwrap().is_none() {
        return; // not connected: nothing to transmit on
    }
    let (eff, epoch) = {
        let mut p = st.sources.lock().unwrap();
        if down { p.hotkey_down() } else { (p.hotkey_up(), None) }
    };
    apply_ptt(app, eff);
    // If the release is never seen (lost focus to the lock screen, ...) the transmitter must not stay open.
    if let Some(epoch) = epoch {
        let app = app.clone();
        tauri::async_runtime::spawn(async move {
            tokio::time::sleep(hotkey::TX_LIMIT).await;
            let eff = app.state::<App>().sources.lock().unwrap().hotkey_expired(epoch);
            if eff != Effect::None {
                apply_ptt(&app, eff);
                let _ = app.emit("notice", "Sendezeit-Limit erreicht: PTT wurde freigegeben");
            }
        });
    }
}

#[tauri::command]
fn get_hotkey(app: AppHandle, state: State<'_, App>) -> HotkeyDto {
    #[cfg(desktop)]
    {
        let saved = settings::load(&app).ptt_hotkey;
        HotkeyDto { supported: true, display: saved.as_ref().map(hotkey::describe), error: state.hotkey_error.lock().unwrap().clone() }
    }
    #[cfg(not(desktop))]
    {
        let _ = (&app, &state);
        HotkeyDto { supported: false, display: None, error: None }
    }
}

/// Saves a recorded hotkey (or removes it with `None`). Refused keys and registration failures come
/// back as errors and leave the previous hotkey untouched.
#[tauri::command]
fn set_hotkey(app: AppHandle, spec: Option<settings::HotkeySpec>) -> Result<HotkeyDto, String> {
    #[cfg(desktop)]
    {
        let mut s = settings::load(&app);
        let previous = s.ptt_hotkey.clone();
        if let Some(spec) = &spec {
            hotkey::validate(spec)?;
        }
        s.ptt_hotkey = spec;
        settings::save(&app, &s);
        let dto = register_saved(&app);
        if let Some(err) = dto.error.clone() {
            s.ptt_hotkey = previous;
            settings::save(&app, &s);
            register_saved(&app);
            return Err(err);
        }
        Ok(dto)
    }
    #[cfg(not(desktop))]
    {
        let _ = (&app, &spec);
        Err("Globale Hotkeys gibt es nur am Computer".into())
    }
}

/// While the UI records a new key, the current hotkey must be released or the system would swallow
/// the very combination being recorded.
#[tauri::command]
fn pause_hotkey(app: AppHandle) {
    #[cfg(desktop)]
    {
        use tauri_plugin_global_shortcut::GlobalShortcutExt;
        let _ = app.global_shortcut().unregister_all();
    }
    #[cfg(not(desktop))]
    let _ = &app;
}

#[tauri::command]
fn resume_hotkey(app: AppHandle, state: State<'_, App>) -> HotkeyDto {
    #[cfg(desktop)]
    {
        let _ = &state;
        register_saved(&app)
    }
    #[cfg(not(desktop))]
    {
        let _ = (&app, &state);
        HotkeyDto { supported: false, display: None, error: None }
    }
}

/// The UI became visible again: refresh the latency readout right away (pings are rarer in the background).
#[tauri::command]
fn ui_visible(state: State<'_, App>) {
    if let Some(s) = state.session.lock().unwrap().as_ref() {
        s.client().ping_now();
    }
}

/// Android only: state of battery restrictions, memory, the last process exits and an event log, as text.
#[tauri::command]
async fn get_diagnostics() -> Option<String> {
    tauri::async_runtime::spawn_blocking(android::report).await.ok().flatten()
}

/// Android only: opens the system dialog that lets the user exempt the app from battery optimisation.
#[tauri::command]
fn request_battery_exemption() -> bool {
    android::request_battery_exemption()
}

#[tauri::command]
fn load_settings(app: AppHandle) -> settings::Settings {
    settings::load(&app)
}

#[derive(serde::Serialize)]
#[serde(rename_all = "camelCase")]
struct AudioDevicesDto {
    /// False on phones, where the system decides the routing.
    supported: bool,
    inputs: Vec<String>,
    outputs: Vec<String>,
    default_input: Option<String>,
    default_output: Option<String>,
    selected_input: Option<String>,
    selected_output: Option<String>,
}

/// Lists the audio devices for the desktop settings and the current choice.
#[tauri::command]
async fn list_audio_devices(app: AppHandle) -> AudioDevicesDto {
    let saved = settings::load(&app);
    if cfg!(target_os = "android") {
        return AudioDevicesDto { supported: false, inputs: vec![], outputs: vec![], default_input: None, default_output: None, selected_input: None, selected_output: None };
    }
    // Enumerating can take a moment on some drivers; keep it off the UI thread.
    let list = tauri::async_runtime::spawn_blocking(funk_audio::list_devices).await.unwrap_or_default();
    AudioDevicesDto {
        supported: true,
        inputs: list.inputs,
        outputs: list.outputs,
        default_input: list.default_input,
        default_output: list.default_output,
        selected_input: saved.input_device,
        selected_output: saved.output_device,
    }
}

/// Remembers the chosen devices; used from the next connection on. `None` = system default.
#[tauri::command]
fn set_audio_devices(app: AppHandle, input: Option<String>, output: Option<String>) {
    let mut s = settings::load(&app);
    s.input_device = input.filter(|n| !n.is_empty());
    s.output_device = output.filter(|n| !n.is_empty());
    settings::save(&app, &s);
}

#[tauri::command]
fn forget_profile(app: AppHandle, key: String) -> settings::Settings {
    let mut s = settings::load(&app);
    s.forget(&key);
    settings::save(&app, &s);
    s
}

/// Version of this build. CI stamps `FUNK_VERSION` (0.1.<run number>) into the compile step, which
/// works the same on every platform, including Android where the build runs inside Gradle. Local
/// builds fall back to the Tauri config version (0.1.0 = "development build").
fn build_version(config_version: &str) -> String {
    option_env!("FUNK_VERSION").unwrap_or(config_version).to_string()
}

#[tauri::command]
fn app_version(app: AppHandle) -> String {
    build_version(&app.package_info().version.to_string())
}

/// Asks the release page whether a newer build exists. Network problems come back as `Err`; the UI
/// stays silent about them at start-up and shows them only for the manual check.
#[tauri::command]
async fn check_update(app: AppHandle) -> Result<update::CheckResult, String> {
    let current = build_version(&app.package_info().version.to_string());
    tauri::async_runtime::spawn_blocking(move || update::check_blocking(&current))
        .await
        .map_err(|e| e.to_string())?
}

/// Opens a download link in the system browser. Only our own release URLs are accepted.
#[tauri::command]
fn open_update_url(app: AppHandle, url: String) -> Result<(), String> {
    use tauri_plugin_opener::OpenerExt;
    if !url.starts_with(update::ALLOWED_PREFIX) {
        return Err("Link nicht erlaubt".into());
    }
    app.opener().open_url(url, None::<&str>).map_err(|e| e.to_string())
}

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    let context = tauri::generate_context!();
    // `funk-app --version` prints the build's version without opening a window (useful for support
    // and for checking that CI stamped the right number).
    if std::env::args().any(|a| a == "--version") {
        println!("{}", build_version(&context.package_info().version.to_string()));
        return;
    }
    let builder = tauri::Builder::default().plugin(tauri_plugin_opener::init()).manage(App::default());
    #[cfg(desktop)]
    let builder = builder
        .plugin(
            tauri_plugin_global_shortcut::Builder::new()
                .with_handler(|app, _shortcut, event| {
                    on_hotkey(app, event.state == tauri_plugin_global_shortcut::ShortcutState::Pressed);
                })
                .build(),
        )
        .setup(|app| {
            register_saved(app.handle());
            Ok(())
        });
    builder
        .invoke_handler(tauri::generate_handler![
            connect, disconnect, join_channel, ptt, load_settings, forget_profile, list_audio_devices, set_audio_devices,
            get_hotkey, set_hotkey, pause_hotkey, resume_hotkey, ui_visible, get_diagnostics, request_battery_exemption, app_version, check_update, open_update_url
        ])
        .on_window_event(|w, ev| {
            if let tauri::WindowEvent::Destroyed = ev {
                if let Some(s) = w.app_handle().state::<App>().session.lock().unwrap().take() {
                    // On Android this also happens when the system or the user ends the activity while
                    // the connection is up; the log makes such cases visible in the diagnostics.
                    android::log("Fenster zerstört: Verbindung wird beendet");
                    s.close();
                }
            }
        })
        .run(context)
        .expect("error while running Mumble Funk");
}
