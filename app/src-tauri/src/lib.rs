//! Tauri shell: wires `funk-core` (protocol, voice) and `funk-audio` (devices) to the UI.

mod android;
mod session;
mod settings;
mod status;

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

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    tauri::Builder::default()
        .manage(App::default())
        .invoke_handler(tauri::generate_handler![connect, disconnect, join_channel, ptt, load_settings, forget_profile])
        .on_window_event(|w, ev| {
            if let tauri::WindowEvent::Destroyed = ev {
                if let Some(s) = w.app_handle().state::<App>().session.lock().unwrap().take() {
                    s.close();
                }
            }
        })
        .run(tauri::generate_context!())
        .expect("error while running Mumble Funk");
}
