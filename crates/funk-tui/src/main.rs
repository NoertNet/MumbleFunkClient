//! `funk-tui`: Mumble-Funk radio in the terminal (also runs headless-friendly on a Raspberry Pi).

mod app;
mod config;
mod hardware;
mod link;
mod ptt;
mod ui;

use std::io::{IsTerminal, stdout};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Receiver};
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use clap::{Parser, ValueEnum};
use funk_audio::DevicePrefs;
use ratatui::DefaultTerminal;
use ratatui::crossterm::event::{
    self, Event as TermEvent, KeyCode, KeyEventKind, KeyModifiers, KeyboardEnhancementFlags, PopKeyboardEnhancementFlags,
    PushKeyboardEnhancementFlags,
};
use ratatui::crossterm::execute;
use ratatui::crossterm::terminal::supports_keyboard_enhancement;

use app::{App, Msg};
use link::{Link, LinkError, LinkOpts};
use ptt::{Action, KeyMode, Ptt, StopReason};

#[derive(Clone, Copy, ValueEnum, PartialEq, Eq, Debug)]
enum PttModeArg {
    /// Hold if the terminal reports key releases, otherwise toggle.
    Auto,
    Hold,
    Toggle,
}

/// Mumble-Funk radio in the terminal.
#[derive(Parser)]
#[command(name = "funk-tui", version = option_env!("FUNK_VERSION").unwrap_or(env!("CARGO_PKG_VERSION")), about, long_about = None)]
struct Cli {
    /// Server name or address (default: the last used one)
    host: Option<String>,
    /// Call sign / user name (default: the last used one)
    #[arg(short, long)]
    user: Option<String>,
    #[arg(short, long)]
    port: Option<u16>,
    /// Join this channel id after connecting
    #[arg(long)]
    channel: Option<u32>,
    /// Ask for the server password (otherwise the FUNK_PASSWORD environment variable is used)
    #[arg(long)]
    ask_password: bool,
    /// Remember the password in the config file (plain text, readable by you only)
    #[arg(long)]
    save_password: bool,
    /// Microphone device (see --list-devices); default: system default
    #[arg(long)]
    input: Option<String>,
    /// Speaker device (see --list-devices)
    #[arg(long)]
    output: Option<String>,
    /// Print audio devices and exit
    #[arg(long)]
    list_devices: bool,
    /// Hardware PTT: evdev device, e.g. /dev/input/by-id/usb-...-event-kbd (see --list-input-devices)
    #[arg(long)]
    ptt_device: Option<String>,
    /// Key/button of that device, e.g. KEY_F13 (default KEY_F13)
    #[arg(long)]
    ptt_key: Option<String>,
    /// Print input devices usable as hardware PTT and exit
    #[arg(long)]
    list_input_devices: bool,
    #[arg(long, value_enum, default_value = "auto")]
    ptt_mode: PttModeArg,
    /// Maximum transmit time in seconds before PTT is released automatically
    #[arg(long, default_value_t = 90)]
    tx_limit: u64,
    /// No audio device: shows activity and sends a test tone on PTT (for testing / monitoring)
    #[arg(long)]
    no_audio: bool,
    /// Use plain ASCII instead of block characters (Linux console fonts)
    #[arg(long)]
    ascii: bool,
    /// Accept a changed server certificate and remember the new one
    #[arg(long)]
    trust_new_cert: bool,
    /// Config file (default ~/.config/mumble-funk/cli.json)
    #[arg(long)]
    config: Option<PathBuf>,
}

fn main() {
    match real_main() {
        Ok(code) => std::process::exit(code),
        Err(e) => {
            eprintln!("Fehler: {e:#}");
            std::process::exit(1);
        }
    }
}

fn real_main() -> Result<i32> {
    let cli = Cli::parse();
    let cfg_path = cli.config.clone().unwrap_or_else(config::default_path);
    let mut cfg = config::load_from(&cfg_path);

    if cli.list_devices {
        let d = funk_audio::list_devices();
        println!("Eingabegeräte (Mikrofon):");
        for n in &d.inputs {
            println!("  {n}{}", if Some(n) == d.default_input.as_ref() { "   [Standard]" } else { "" });
        }
        println!("Ausgabegeräte (Lautsprecher):");
        for n in &d.outputs {
            println!("  {n}{}", if Some(n) == d.default_output.as_ref() { "   [Standard]" } else { "" });
        }
        return Ok(0);
    }
    if cli.list_input_devices {
        let list = hardware::list();
        if list.is_empty() {
            println!("Keine Eingabegeräte gefunden (nur unter Linux; evtl. fehlen Leserechte: Gruppe 'input').");
        }
        for l in list {
            println!("{l}");
        }
        return Ok(0);
    }

    let host = cli.host.clone().filter(|h| !h.is_empty()).or_else(|| Some(cfg.host.clone()).filter(|h| !h.is_empty()));
    let username = cli.user.clone().or_else(|| Some(cfg.username.clone()).filter(|u| !u.is_empty()));
    let (Some(host), Some(username)) = (host, username) else {
        bail!("Server und Rufzeichen fehlen. Beispiel: funk-tui funk.example.org -u Alpha1   (beim nächsten Mal genügt `funk-tui`)");
    };
    let port = cli.port.or(cfg.port).unwrap_or(64738);
    let input = cli.input.clone().or(cfg.input.clone());
    let output = cli.output.clone().or(cfg.output.clone());
    let ptt_device = cli.ptt_device.clone().or(cfg.ptt_device.clone());
    let ptt_key_name = cli.ptt_key.clone().or(cfg.ptt_key.clone());

    // Password: environment first, then an explicit prompt, then the saved one.
    let mut password = std::env::var("FUNK_PASSWORD").ok().filter(|p| !p.is_empty());
    if password.is_none() && cli.ask_password {
        password = Some(rpassword::prompt_password("Passwort: ").context("Passwort lesen")?);
    }
    if password.is_none() {
        password = cfg.password.clone();
    }

    let pin_key = format!("{host}:{port}");
    let pin = if cli.trust_new_cert { None } else { cfg.pins.get(&pin_key).and_then(|h| config::parse_pin(h)) };
    let (msg_tx, msg_rx) = mpsc::channel::<Msg>();
    let mut opts = LinkOpts {
        host: host.clone(),
        port,
        username: username.clone(),
        password,
        pin,
        no_audio: cli.no_audio,
        prefs: DevicePrefs { input: input.clone(), output: output.clone() },
    };

    eprintln!("Verbinde mit {host}:{port} als {username} …");
    let mut result = link::connect(&opts, msg_tx.clone());
    // A server with a password rejects the first try: ask once, then retry.
    if let Err(LinkError::Connect(funk_core::Error::Rejected(reason))) = &result {
        if opts.password.is_none() && std::io::stdin().is_terminal() {
            eprintln!("Server lehnt ab: {reason}");
            opts.password = Some(rpassword::prompt_password("Passwort: ").context("Passwort lesen")?);
            result = link::connect(&opts, msg_tx.clone());
        }
    }
    let link = match result {
        Ok(l) => l,
        Err(LinkError::Connect(funk_core::Error::PinMismatch)) => {
            eprintln!(
                "Das Zertifikat von {host}:{port} hat sich geändert. Das kann ein Serverumzug sein – oder ein Angriff.\nNur wenn du dem neuen Zertifikat vertraust: funk-tui --trust-new-cert"
            );
            return Ok(2);
        }
        Err(LinkError::Connect(funk_core::Error::Rejected(r))) => bail!("Server hat die Anmeldung abgelehnt: {r}"),
        Err(LinkError::Connect(funk_core::Error::Unreachable(d))) => bail!("Server nicht erreichbar: {d}"),
        Err(LinkError::Connect(e)) => bail!("{e}"),
        Err(LinkError::Audio(e)) => bail!("Audio: {e}\nTipp: --list-devices zeigt die Geräte, --input/--output wählt sie, --no-audio testet ohne Audiogerät."),
    };

    // Remember what worked (pin on first use, devices, last server).
    cfg.host = host.clone();
    cfg.port = Some(port);
    cfg.username = username.clone();
    cfg.input = input;
    cfg.output = output;
    cfg.ptt_device = ptt_device.clone();
    cfg.ptt_key = ptt_key_name.clone();
    cfg.password = if cli.save_password { opts.password.clone() } else if cfg.password.is_some() { cfg.password.clone() } else { None };
    if cli.trust_new_cert || !cfg.pins.contains_key(&pin_key) {
        cfg.pins.insert(pin_key, config::hex(&link.client.cert_sha256()));
    }
    if let Err(e) = config::save_to(&cfg_path, &cfg) {
        eprintln!("Hinweis: Einstellungen konnten nicht gespeichert werden: {e}");
    }

    if let Some(ch) = cli.channel {
        link.join(ch);
    }

    // Hardware PTT (evdev): open now so mistakes are reported before the screen switches.
    let mut hw_label = None;
    if let Some(dev) = &ptt_device {
        let key_name = ptt_key_name.clone().unwrap_or_else(|| "KEY_F13".into());
        let key = hardware::parse_key(&key_name).map_err(anyhow::Error::msg)?;
        hardware::spawn(dev, key, msg_tx.clone()).map_err(anyhow::Error::msg)?;
        hw_label = Some(key_name.to_uppercase());
    }

    run_tui(&cli, link, msg_rx, host, username, hw_label)
}

static KEYBOARD_FLAGS_PUSHED: AtomicBool = AtomicBool::new(false);

/// Puts the terminal back in order. Safe to call more than once and from the panic hook.
fn restore_terminal() {
    if KEYBOARD_FLAGS_PUSHED.swap(false, Ordering::SeqCst) {
        let _ = execute!(stdout(), PopKeyboardEnhancementFlags);
    }
    ratatui::restore();
}

fn run_tui(cli: &Cli, link: Link, rx: Receiver<Msg>, host: String, username: String, hw_label: Option<String>) -> Result<i32> {
    if !stdout().is_terminal() {
        bail!("funk-tui braucht ein Terminal (stdout ist keins).");
    }
    // SIGTERM/SIGHUP (e.g. a dropped SSH session) must end transmissions and restore the terminal.
    let quit = Arc::new(AtomicBool::new(false));
    for sig in [signal_hook::consts::SIGTERM, signal_hook::consts::SIGHUP, signal_hook::consts::SIGINT] {
        signal_hook::flag::register(sig, quit.clone()).context("Signalbehandlung")?;
    }

    let mut terminal = ratatui::init(); // raw mode + alternate screen + panic hook that restores them
    let prev_hook = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        restore_terminal();
        prev_hook(info);
    }));

    // Key release events (needed for hold-to-talk) only exist with the kitty keyboard protocol.
    let release_events = cli.ptt_mode != PttModeArg::Toggle && supports_keyboard_enhancement().unwrap_or(false);
    if release_events {
        let flags = KeyboardEnhancementFlags::DISAMBIGUATE_ESCAPE_CODES
            | KeyboardEnhancementFlags::REPORT_EVENT_TYPES
            | KeyboardEnhancementFlags::REPORT_ALL_KEYS_AS_ESCAPE_CODES;
        if execute!(stdout(), PushKeyboardEnhancementFlags(flags)).is_ok() {
            KEYBOARD_FLAGS_PUSHED.store(true, Ordering::SeqCst);
        }
    }
    let mode = if release_events { KeyMode::Hold } else { KeyMode::Toggle };
    let mut ptt_info = match mode {
        KeyMode::Hold => "HALTEN (Leertaste)".to_string(),
        KeyMode::Toggle => "UMSCHALTEN (Leertaste: Start, nochmal: Stopp)".to_string(),
    };
    if let Some(k) = hw_label {
        ptt_info.push_str(&format!(" · HARDWARE {k}"));
    }
    let mut app = App::new(host, username, ptt_info, link.devices.clone());
    if cli.ptt_mode == PttModeArg::Hold && !release_events {
        app.set_notice("Dieses Terminal meldet kein Loslassen: PTT arbeitet im UMSCHALT-Modus", Instant::now());
    }
    let mut ptt = Ptt::new(mode, Duration::from_secs(cli.tx_limit.max(5)));

    let result = event_loop(&mut terminal, &mut app, &link, &mut ptt, &rx, &quit, cli.ascii);

    // Every way out ends here: stop transmitting, then give the terminal back.
    if let Action::Stop(_) = ptt.shutdown() {
        link.ptt_stop();
    }
    restore_terminal();
    link.close();
    if let Err(e) = result {
        eprintln!("Fehler: {e:#}");
        return Ok(1);
    }
    if let Some(reason) = app.disconnected {
        eprintln!("Verbindung getrennt{}.", reason.map(|r| format!(": {r}")).unwrap_or_default());
        return Ok(3);
    }
    Ok(0)
}

fn apply_action(action: Action, app: &mut App, link: &Link, now: Instant) {
    match action {
        Action::None => {}
        Action::Start => {
            link.ptt_start();
            app.tx = true;
        }
        Action::Stop(reason) => {
            link.ptt_stop();
            app.tx = false;
            if reason == StopReason::Limit {
                app.set_notice("SENDEZEIT-LIMIT ERREICHT: PTT AUTOMATISCH FREIGEGEBEN", now);
            }
        }
    }
}

fn event_loop(
    terminal: &mut DefaultTerminal,
    app: &mut App,
    link: &Link,
    ptt: &mut Ptt,
    rx: &Receiver<Msg>,
    quit: &AtomicBool,
    ascii: bool,
) -> Result<()> {
    let mut dirty = true;
    let mut last_draw = Instant::now() - Duration::from_secs(10);
    loop {
        if quit.load(Ordering::Relaxed) {
            return Ok(());
        }
        let now = Instant::now();
        while let Ok(msg) = rx.try_recv() {
            dirty = true;
            match msg {
                Msg::HardwarePtt(down) => {
                    // No new transmission while the connection is being re-established.
                    let a = if down && !app.reconnecting { ptt.hardware_down(now) } else if down { Action::None } else { ptt.hardware_up() };
                    apply_action(a, app, link, now);
                }
                Msg::Reconnecting(true) => {
                    // Whatever was being transmitted is over; a fresh press is needed after the reconnect.
                    apply_action(ptt.shutdown(), app, link, now);
                    app.apply(Msg::Reconnecting(true), now);
                }
                other => app.apply(other, now),
            }
        }
        if app.disconnected.is_some() {
            return Ok(());
        }
        apply_action(ptt.tick(now), app, link, now);

        let animating = app.tick(now) || app.tx || !app.speaking.is_empty();
        if dirty || animating || now.duration_since(last_draw) > Duration::from_secs(1) {
            terminal.draw(|f| ui::draw(f, app, now, ascii)).context("Zeichnen")?;
            last_draw = now;
            dirty = false;
        }

        // Idle: wake up rarely (battery / SD-card friendly); animating: ~25 frames per second.
        let timeout = if animating { Duration::from_millis(40) } else { Duration::from_millis(150) };
        if event::poll(timeout)? {
            dirty = true;
            match event::read()? {
                TermEvent::Key(key) => {
                    if handle_key(key, app, link, ptt, Instant::now()) {
                        return Ok(());
                    }
                }
                TermEvent::Resize(..) => {}
                _ => {}
            }
        }
    }
}

/// Returns true to quit.
fn handle_key(key: ratatui::crossterm::event::KeyEvent, app: &mut App, link: &Link, ptt: &mut Ptt, now: Instant) -> bool {
    let is_ptt_key = matches!(key.code, KeyCode::Char(' ') | KeyCode::Enter);
    if key.kind == KeyEventKind::Release {
        if is_ptt_key && !app.list_open {
            apply_action(ptt.key_release(), app, link, now);
        }
        return false;
    }
    if key.kind == KeyEventKind::Repeat {
        return false; // terminals that report repeats: the first press already did the work
    }
    if key.modifiers.contains(KeyModifiers::CONTROL) && key.code == KeyCode::Char('c') {
        return true;
    }
    if app.list_open {
        let n = app.channels_sorted().len();
        match key.code {
            KeyCode::Esc | KeyCode::Char('l') | KeyCode::Char('q') => app.list_open = false,
            KeyCode::Up => app.list_sel = app.list_sel.saturating_sub(1),
            KeyCode::Down => app.list_sel = (app.list_sel + 1).min(n.saturating_sub(1)),
            KeyCode::Enter => {
                if let Some(c) = app.channels_sorted().get(app.list_sel) {
                    link.join(c.id);
                }
                app.list_open = false;
            }
            _ => {}
        }
        return false;
    }
    match key.code {
        KeyCode::Char('q') => return true,
        KeyCode::Left | KeyCode::Char('h') => {
            if let Some(id) = app.step_channel(-1) {
                link.join(id);
            }
        }
        KeyCode::Right => {
            if let Some(id) = app.step_channel(1) {
                link.join(id);
            }
        }
        KeyCode::Char('l') => {
            app.list_open = true;
            let own = app.st.own_channel();
            app.list_sel = app.channels_sorted().iter().position(|c| Some(c.id) == own).unwrap_or(0);
        }
        KeyCode::Char(' ') | KeyCode::Enter => {
            if app.reconnecting {
                app.set_notice("VERBINDUNG WIRD NEU AUFGEBAUT: PTT GESPERRT", now);
            } else {
                apply_action(ptt.key_press(now), app, link, now);
            }
        }
        _ => {}
    }
    false
}
