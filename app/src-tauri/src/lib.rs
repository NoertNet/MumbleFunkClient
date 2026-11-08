//! Tauri shell: wires `funk-core` (protocol, voice) and `funk-audio` (devices) to the UI.

mod android;
mod session;
mod settings;
mod status;
mod update;

use std::sync::Mutex;

use session::{Session, StateDto};
use tauri::{AppHandle, Manager, State};

#[derive(Default)]
struct App {
    session: Mutex<Option<Session>>,
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

#[tauri::command]
async fn connect(app: AppHandle, state: State<'_, App>, args: ConnectArgs) -> Result<StateDto, String> {
    if let Some(old) = state.session.lock().unwrap().take() {
        old.close();
    }
    let s = Session::connect(app, args).await?;
    let dto = s.dto();
    *state.session.lock().unwrap() = Some(s);
    Ok(dto)
}

#[tauri::command]
fn disconnect(state: State<'_, App>) {
    if let Some(s) = state.session.lock().unwrap().take() {
        s.close();
    }
}

#[tauri::command]
async fn join_channel(state: State<'_, App>, id: u32) -> Result<(), String> {
    let client = state.session.lock().unwrap().as_ref().map(|s| s.client());
    client.ok_or("not connected")?.join_channel(id).await.map_err(|e| e.to_string())
}

/// Push-to-talk. `down=true` starts a transmission, `false` ends it with a terminator packet.
#[tauri::command]
fn ptt(state: State<'_, App>, down: bool) -> Result<(), String> {
    let guard = state.session.lock().unwrap();
    let s = guard.as_ref().ok_or("not connected")?;
    s.ptt(down);
    Ok(())
}

#[tauri::command]
fn load_settings(app: AppHandle) -> settings::Settings {
    settings::load(&app)
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
    tauri::Builder::default()
        .plugin(tauri_plugin_opener::init())
        .manage(App::default())
        .invoke_handler(tauri::generate_handler![connect, disconnect, join_channel, ptt, load_settings, forget_profile, app_version, check_update, open_update_url])
        .on_window_event(|w, ev| {
            if let tauri::WindowEvent::Destroyed = ev {
                if let Some(s) = w.app_handle().state::<App>().session.lock().unwrap().take() {
                    s.close();
                }
            }
        })
        .run(context)
        .expect("error while running Mumble Funk");
}
