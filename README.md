# MumbleFunkClient

Walkie-Talkie-Client für den [Mumble-Funk](https://github.com/Shawna7han/mumble-funk)-Server.
Rust-Kern + [Tauri 2](https://tauri.app)-Oberfläche für Windows, Linux und Android.

| Teil | Inhalt |
| --- | --- |
| `crates/funk-core` | Mumble-Protokoll (TLS, Zertifikat-Pinning), Zustand, Opus-Senden mit PTT, Jitter-Buffer/Mixer. Ohne Oberfläche und Audiogeräte. |
| `crates/funk-audio` | Mikrofon und Lautsprecher über `cpal`. |
| `crates/funk-probe` | Kommandozeilen-Testwerkzeug, braucht keine Audiohardware. |
| `app/` | Tauri-App (`src-tauri` = Rust, `ui` = HTML/CSS/JS ohne Bundler). |
| `docs/backend-requests.md` | Wünsche an das Server-Backend. |

## Stand

Funktioniert gegen einen normalen Mumble-Server (Verbinden, Kanalwechsel, Senden/Empfangen per Protobuf-Voice
über TCP). **Noch nicht** gegen den Funk-Server getestet und noch nicht mit echtem Mikrofon/Lautsprecher.
Die App nutzt das Standardformat der Windows-/Android-Audiogeräte und rechnet selbst auf 48 kHz um.

## Schnelltest ohne Oberfläche

Voraussetzung: [Rust](https://rustup.rs) (stable). Opus ist als reines Rust eingebunden, ein C-Compiler oder CMake ist dafür nicht nötig.

```bash
export FUNK_PASSWORD='dein-serverpasswort'
cargo run -p funk-probe -- <host> <benutzer> --port 64739 --channel 1 --tone 2
```

Die Ausgabe zeigt Kanäle, Benutzer, den Zertifikat-Fingerabdruck und alle `funk.*`-Plugin-Daten.
Mit `--tone 2` sendet das Tool 2 Sekunden Testton über den echten PTT-Pfad – ideal, um das Verhalten von
Simplex/Busy zu beobachten. Zweites Terminal mit anderem Benutzer startet man ohne `--tone`.

## App bauen und starten

**Fertige Programme** stehen unter *Releases* (rechts auf der GitHub-Startseite): `latest-dev` ist die jeweils neueste
Entwicklungsversion mit `MumbleFunk-Android.apk`, `MumbleFunk-Windows-setup.exe` und Prüfsummen. Ein Tag wie `v0.2.0`
erzeugt automatisch ein reguläres Release.

*Android-Signatur:* Damit eine neue APK über die alte installiert werden kann, muss sie immer mit demselben Schlüssel
signiert sein. CI erzeugt dafür beim ersten Lauf einen Schlüssel und legt ihn im Actions-Cache ab (nichts davon liegt im
Repository). Dauerhaft und unabhängig vom Cache wird es, wenn du die Repository-Secrets `ANDROID_KEYSTORE_BASE64` und
`ANDROID_KEYSTORE_PASSWORD` anlegst; dann nutzt der Build diese. Lokal bauen:

```bash
cargo install tauri-cli --version "^2" --locked
cd app/src-tauri
cargo tauri dev        # Entwicklungsfenster
cargo tauri build      # Installer
```

**Linux** (Ubuntu/Debian):
`sudo apt install libwebkit2gtk-4.1-dev libasound2-dev libssl-dev libayatana-appindicator3-dev librsvg2-dev libxdo-dev build-essential`

**Windows:** Visual Studio Build Tools (Workload „Desktop development with C++“); WebView2 ist ab Windows 11 vorhanden.

**Android:** Android Studio mit SDK und NDK, Umgebungsvariablen `ANDROID_HOME` und `NDK_HOME`, dann:
```bash
rustup target add aarch64-linux-android armv7-linux-androideabi
cd app/src-tauri
cargo tauri android init
cargo tauri android dev     # Handy per USB, Debugging aktiv
```
Die Android-Teile (Mikrofon-Berechtigung, Foreground-Service für Empfang bei ausgeschaltetem Display) sind noch nicht umgesetzt.

## Bedienung

Server, Port, Name (und Passwort) eintragen, verbinden. Erfolgreiche Server werden gespeichert und stehen beim nächsten Start unter „Gespeicherte Server“ bereit; das Passwort nur, wenn das Häkchen gesetzt ist. Kanal antippen zum Wechseln. Den großen Knopf gedrückt halten
(oder Leertaste) zum Senden; beim Loslassen wird das Ende der Übertragung an den Server gemeldet.
Beim ersten Verbinden merkt sich die App den Zertifikat-Fingerabdruck des Servers; ändert er sich, fragt sie nach.
Gespeichert wird unter dem App-Konfigurationsordner in `settings.json`; ein Passwort steht dort unverschlüsselt (Anbindung an den System-Schlüsselbund folgt).
