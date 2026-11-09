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
Entwicklungsversion mit `MumbleFunk-Android.apk`, `MumbleFunk-Windows-setup.exe`, `MumbleFunk-Linux.AppImage` / `.deb` und Prüfsummen. Ein Tag wie `v0.2.0`
erzeugt automatisch ein reguläres Release.

**Ubuntu/Linux:** `MumbleFunk-Linux.AppImage` ausführbar machen (`chmod +x`) und starten, oder
`sudo apt install ./MumbleFunk-Linux.deb`. Gebaut wird auf Ubuntu 22.04, läuft damit auch auf neueren Versionen.

**Updates:** Die App prüft beim Start (und auf Knopfdruck über „Update suchen“), ob unter *Releases* eine neuere Version liegt
(Datei `latest.json`), und zeigt dann einen Hinweis mit „Holen“. Das öffnet den Download im Browser; installiert wird nicht
automatisch. Jede Version trägt `0.1.<Laufnummer>`; `funk-app --version` zeigt sie auch ohne Fenster. Lokal gebaute Versionen
(`0.1.0`) werden nicht geprüft.

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

## Terminal-Version (Linux): `funk-tui`

Dasselbe Funkgerät im Terminal, mit Display, Signalbalken, Equalizer und PTT-Feld (Grün = du sendest, Rot = jemand anderes).
Gedacht für Rechner ohne Oberfläche, SSH und den Raspberry Pi. Download: `MumbleFunk-CLI-Linux-x86_64.tar.gz` bzw.
`-aarch64.tar.gz` (Pi mit 64-Bit-System) unter *Releases*, entpacken, starten. Zum Selberbauen: `cargo build --release -p funk-tui`
(braucht `libasound2-dev`).

```bash
./funk-tui funk.example.org -u Alpha1      # beim ersten Mal; danach genügt ./funk-tui
FUNK_PASSWORD='geheim' ./funk-tui           # Passwort nie als Argument (sichtbar in `ps`); oder --ask-password
./funk-tui --list-devices                   # Audio-Geräte anzeigen
./funk-tui --input "USB Audio" --output "USB Audio"
./funk-tui --no-audio                       # Testmodus ohne Audiogerät: zeigt Aktivität, PTT sendet Testton
```

Tasten: **Leertaste/Enter** = PTT, **←/→** = Kanal wechseln, **l** = Kanalliste, **q** oder Strg+C = Ende.
Server, Rufzeichen, Geräte und Zertifikat werden in `~/.config/mumble-funk/cli.json` gemerkt (nur für dich lesbar).
Ändert sich das Zertifikat, bricht das Programm ab; nur `--trust-new-cert` übernimmt das neue.

**PTT im Terminal, bitte lesen:** Normale Terminals melden nicht, wann eine Taste *losgelassen* wird.
- In Terminals mit dem Kitty-Tastaturprotokoll (kitty, WezTerm, foot, Ghostty, Alacritty) funktioniert **Halten**: gedrückt = senden, loslassen = Ende.
- Überall sonst (GNOME-Terminal, xterm, Linux-Textkonsole, SSH aus vielen Clients) gilt **Umschalten**: einmal tippen startet, nochmal tippen beendet.
  Die Titelzeile des PTT-Felds zeigt immer, welcher Modus gerade gilt. Nach `--tx-limit` Sekunden (Standard 90) wird automatisch freigegeben.
- **Echte Hardware-Taste (empfohlen fürs Funkmikrofon):** per evdev, mit echtem Drücken und Loslassen, auch über SSH und auf der Textkonsole:
  `./funk-tui --list-input-devices`, dann `./funk-tui --ptt-device /dev/input/by-id/<gerät> --ptt-key KEY_F13`. Der Benutzer braucht
  Leserechte auf das Gerät (Gruppe `input`). USB-Fußschalter und Tasten-Mikrofone erscheinen meist als Tastatur.

Auf der Textkonsole (ohne Unicode-Blockzeichen) hilft `--ascii`. Beim Beenden, bei SIGTERM und bei abgerissener SSH-Verbindung
wird ein laufendes Senden sauber beendet und das Terminal zurückgesetzt.

## Zuverlässigkeit und Akku (alle Systeme, besonders Android)

- **Automatische Wiederverbindung:** Bricht die Verbindung ab (WLAN-Wechsel, Standby, Serverneustart), baut die App sie selbst wieder auf
  (nach 1, 2, 4, 8, dann alle 15 Sekunden) und tritt dem vorherigen Kanal wieder bei. Währenddessen steht „VERBINDUNG VERLOREN“ im Display und
  PTT ist gesperrt; nach dem Wiederaufbau muss PTT neu gedrückt werden. Zurück zum Startbildschirm geht es nur bei „AUS“ oder einem
  endgültigen Fehler (falsches Passwort, geändertes Zertifikat). Der Android-Dienst bleibt dabei bestehen.
- **Gleicher Name auf zwei Geräten:** Der Server beendet die *ältere* Sitzung, wenn sich derselbe Name erneut anmeldet. Damit sich zwei Geräte
  nicht endlos gegenseitig hinauswerfen, gibt eines nach zwei kurzen Abbrüchen auf und meldet den Grund; das andere bleibt verbunden.
  Besser: pro Gerät ein eigenes Rufzeichen.
- **Ping:** Bei sichtbarem Fenster alle 4 s (Latenzanzeige), im Hintergrund alle 8 s. Das bleibt unter dem 30-s-Limit des Servers und unter
  der 13-s-Schwelle der Anzeige „kein Signal“; beim Zurückkehren in die App wird sofort gepingt. Eine tote Verbindung wird nach
  2 × Intervall + 10 s erkannt.
- **Android-Diagnose** (Startbildschirm → „Android-Diagnose“): zeigt, ob Android die App eingeschränkt oder beendet hat (letzte Prozess-Enden mit Grund,
  Akku-Optimierung, Standby-Bucket, Speicherbedarf) und ein kleines Ereignisprotokoll. „Kopieren“ und an den Entwickler schicken; es stehen keine Passwörter darin.
  „Akku-Optimierung abschalten“ öffnet den Android-Dialog (empfohlen, damit das System die App im Hintergrund nicht einschränkt).

## Globaler PTT-Hotkey (Computer)

Auf dem Einschalt-Bildschirm unter „Globaler PTT-Hotkey“ lässt sich eine Taste festlegen („Taste aufnehmen“), mit der man auch bei
minimiertem oder nicht aktivem Fenster sendet: gedrückt halten = senden, loslassen = Ende (nach 90 s wird automatisch freigegeben).
Die Taste ist danach in allen anderen Programmen nicht mehr benutzbar, deshalb gelten Regeln: **erlaubt** sind F-Tasten (außer F12),
Pause, Rollen oder Kombinationen mit mindestens zwei Zusatztasten (z. B. Strg+Alt+T); **nicht erlaubt** sind die Leertaste (auch mit
Zusatztasten), einzelne Buchstaben/Ziffern und Kürzel wie Strg+C. Buchstaben richten sich nach dem Tastaturlayout (auch QWERTZ).
Ist die Kombination schon von einem anderen Programm belegt, steht das dort; der bisherige Hotkey bleibt dann aktiv. Unter Linux
funktioniert der Hotkey nur mit X11, nicht unter Wayland. Maus- und Spezialtasten sind nicht möglich.

## Audio-Geräte (Computer)

Auf dem Einschalt-Bildschirm gibt es den Bereich „Audio-Geräte“ mit je einer Auswahl für Mikrofon und Lautsprecher
(„Liste aktualisieren“ liest die Geräte neu ein, etwa nach dem Einstecken eines USB-Mikrofons). Die Wahl wird gespeichert und
gilt ab der nächsten Verbindung; ohne Auswahl nutzt die App das Standardgerät. Ist das gewählte Gerät nicht erreichbar,
weicht sie auf das Standardgerät aus und zeigt das im Equalizer-Feld an (Name der tatsächlich benutzten Geräte, bei Ausweichen
in Bernstein). Auf Android bestimmt das System die Ausgabe, dort gibt es die Auswahl nicht.

## Bedienung

Server, Port, Name (und Passwort) eintragen, verbinden. Erfolgreiche Server werden gespeichert und stehen beim nächsten Start unter „Gespeicherte Server“ bereit; das Passwort nur, wenn das Häkchen gesetzt ist. Kanal antippen zum Wechseln. Den großen Knopf gedrückt halten
(oder Leertaste) zum Senden; beim Loslassen wird das Ende der Übertragung an den Server gemeldet.
Beim ersten Verbinden merkt sich die App den Zertifikat-Fingerabdruck des Servers; ändert er sich, fragt sie nach.
Gespeichert wird unter dem App-Konfigurationsordner in `settings.json`; ein Passwort steht dort unverschlüsselt (Anbindung an den System-Schlüsselbund folgt).
