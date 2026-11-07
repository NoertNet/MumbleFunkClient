# Anforderungen des Clients an das Backend (Mumble-Funk `dev`)

Stand: 7. November 2026, geprüft gegen `Shawna7han/mumble-funk@dev`, Commit `e080e9b`.
Entwurf zur Abstimmung mit Shawna7han. Es wurde nichts im Backend-Repo geändert.

## Ausgangslage

Der Server liefert Simplex, Busy, Squelch Open/Close und Funkrauschen **nur als Audio**.
Eine Statusmeldung an den Client existiert nicht: Im Servercode (`src/murmur/`) werden
weder `PluginDataTransmission`, `TextMessage` noch `UserState` für Funkereignisse
erzeugt; `PluginDataTransmission` hat nur den Standard-Handler `Messages.cpp:2416`.
Ein Client kann deshalb nicht verlässlich erkennen,

- ob seine Aussendung angenommen oder abgewiesen wurde (außer durch Hören des Busy-Tons),
- wer den Kanal gerade belegt und wann er wieder frei ist (Timeout, Disconnect),
- ob er an einem Funk-Server hängt und welche Session den privaten Effektstrom sendet.

## Vorschlag: drei kleine Nachrichten über `PluginDataTransmission`

Transport: Mumble-Nachricht `PluginDataTransmission` (Typ 26), vom Server erzeugt.
`data_id` benennt die Nachricht, `data` ist UTF-8-JSON mit `"v": 1`. Unbekannte Felder
muss der Client ignorieren, neue Felder sind damit abwärtskompatibel.

Standard-Clients (Windows-Mumble, Mumla) verarbeiten unbekannte `data_id` laut Protokoll
nur über Plugins; **das ist mit den realen Clients noch nicht getestet** und sollte beim
ersten Prototyp geprüft werden. Die Funktion soll abschaltbar sein (z. B. `funkclient=false`
in der INI), falls ein Client damit Probleme hat.

### 1. `funk.hello` (Server → neu angemeldeter Benutzer, direkt nach `ServerSync`)

```json
{"v":1,"funk":"0.2","simplex":true,"effect_session":4242,"timeout_ms":500}
```

| Feld | Bedeutung |
| --- | --- |
| `funk` | Funk-Erweiterungsversion des Servers (nicht die Mumble-Version). |
| `simplex` | Ob Simplex aktiv ist. |
| `effect_session` | Session-ID, unter der der private Effektstrom (Busy/Open/Close/Noise) ankommt. So kann der Client Effekte von Sprache trennen, z. B. getrennte Lautstärke. `null`, falls Effekte aus sind. |
| `timeout_ms` | Inaktivitätszeit, nach der der Server eine Sendung als beendet wertet. |

### 2. `funk.tx` (Server → alle Benutzer im Kanal, auch der Sender)

```json
{"v":1,"channel":3,"state":"start","session":17}
{"v":1,"channel":3,"state":"end","session":17,"reason":"released"}
```

- `state=start`, sobald der Server die Aussendung annimmt (Simplex-Freigabe).
- `state=end`, wenn der Kanal frei wird. `reason`: `released` (bestätigtes Ende-Paket),
  `timeout` (Inaktivität, kein Beweis für Loslassen), `disconnect`, `channel_change`,
  `permission_lost`.
- Nur an Benutzer des betroffenen Kanals, nicht serverweit.
- Reihenfolge: `end` möglichst vor oder zusammen mit dem Close-Effekt, nie danach.
- Neu in den Kanal tretende Benutzer erhalten bei laufender Sendung ein `start`.

### 3. `funk.tx.result` (Server → nur der Sender)

```json
{"v":1,"result":"granted"}
{"v":1,"result":"rejected","holder":17}
```

Einmal pro Aussendeversuch, beim ersten Paket. Bei `rejected` ist `holder` die
Session, die den Kanal belegt. Der Busy-Ton bleibt unverändert zusätzlich bestehen.
Damit kann die App sofort "Kanal belegt" zeigen und das Senden-Symbol zurücksetzen.

## Dokumentation

Bitte die drei Nachrichten in `docs/funk/client-protocol.md` im Backend-Repo
festhalten (Felder, Beispiele, Timeout-Verhalten), damit Server und Client dieselbe
Referenz haben.

## Später, nicht für den MVP

- **Mindestpause bei schnellem PTT-Wechsel:** Klarstellung, wie sich der Server bei Ende-Paket
  und sofort folgendem neuen Start verhält.
- **Kanalmetadaten** (Rufzeichen/Frequenz) in der Kanalbeschreibung, falls die App wie ein
  Funkgerät anzeigen soll.
- **Scan / Mehrkanal-Mithören:** Simplex verbietet heute Links und Listener-Proxys.
  Eine Hör-Berechtigung für mehrere Kanäle bräuchte eine Serveränderung.
- **iOS:** zurückgestellt. Hintergrund-PTT bräuchte Apples Push-to-Talk-Framework samt Push-Dienst.

## Was der Client ohne diese Änderungen tut

Der Client funktioniert auch gegen den heutigen Server: Er sendet PTT-Pakete mit Ende-Marker,
spielt alle Audioströme ab und leitet die Sprecheranzeige aus ankommenden Sprachpaketen ab.
Er kann dann aber weder Freigabe/Ablehnung zuverlässig anzeigen noch ein Timeout-Ende erkennen.
Sobald `funk.hello` ankommt, schaltet er auf die genaue Anzeige um.
