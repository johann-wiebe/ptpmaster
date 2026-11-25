# ptpmaster – PTP-Grandmaster für Windows

`ptpmaster.exe` macht einen Windows-PC zum PTP-Grandmaster im LAN (IEEE 1588-2008, PTPv2 über
UDP/IPv4, Multicast 224.0.1.129, Ports 319/320). Zeitquelle ist die Systemuhr des PCs, die
Windows per NTP (w32time) stellt.

- Announce, Sync + Follow_Up (Two-Step, Standard) oder One-Step, Delay_Req/Delay_Resp (E2E)
- BMCA: Taucht ein besserer Master in derselben Domain auf, wechselt ptpmaster auf PASSIVE
  und meldet das laut. Verschwindet er, wird ptpmaster wieder MASTER.
- Zeitstempel: Hardware (Netzwerkkarte), Software (Treiber) oder Userspace, je nachdem, was
  Windows und die Netzwerkkarte können
- Konsolenprogramm mit Live-Status oder Windows-Dienst; Logdatei mit Rotation, im Dienst auch
  im Event Log
- `probe`: kleiner Messmodus, der von einem zweiten PC aus Offset und Path Delay anzeigt

Die Entscheidungen hinter dem Design stehen in [DECISIONS.md](DECISIONS.md).

> **Stand der Prüfung:** Entwickelt und getestet wurde unter Linux (Unit- und
> Integrationstests über Loopback-Multicast, Userspace-Zeitstempel). Die Windows-exe ist
> cross-kompiliert, lief aber noch nie auf echtem Windows. Hardware- und
> Software-Timestamping sind ungetestet. Bitte zuerst wie unter
> [Erste Schritte](#erste-schritte) ausprobieren.

## Erste Schritte

1. `ptpmaster-0.1.0-windows-x64.zip` z. B. nach `C:\ptpmaster` entpacken.
2. PowerShell **als Administrator** öffnen und die Firewall freigeben:
   ```powershell
   cd C:\ptpmaster
   Unblock-File .\scripts\firewall.ps1        # falls aus dem Internet/ZIP blockiert
   Set-ExecutionPolicy -Scope Process Bypass
   .\scripts\firewall.ps1
   ```
3. Interfaces ansehen:
   ```powershell
   .\ptpmaster.exe interfaces
   ```
   Hier steht die Windows-Version, ob die Timestamping-API vorhanden ist, und je Netzwerkkarte
   MAC, clockIdentity, IP und die aktiven Timestamp-Fähigkeiten. Das gewünschte Interface in
   `ptpmaster.toml` unter `[network] interface = "…"` eintragen, falls nicht schon das richtige
   mit „← wird verwendet“ markiert ist.
4. In der Konsole starten:
   ```powershell
   .\ptpmaster.exe run
   ```
   Im Log auf die Zeile `Timestamp-Modus: …` achten. Nach der LISTENING-Phase (6 s beim
   Profil `default`) folgt `MASTER: kein besserer Master gefunden …`. Unten zeigt eine
   Statuszeile Zustand, gesendete Pakete und Delay_Req-Clients. Beenden mit Ctrl+C.
5. Von einem zweiten PC aus prüfen, entweder mit `ptpmaster.exe probe` (Firewall-Skript dort
   ebenfalls ausführen) oder mit linuxptp, siehe [Test mit linuxptp](#test-mit-linuxptp).
6. Wenn alles passt, als Dienst einrichten:
   ```powershell
   .\ptpmaster.exe install --start
   ```

## Voraussetzungen

| Windows | Timestamp-Modi |
|---|---|
| Windows 11, Windows Server 2022 und neuer | Hardware, Software und Userspace, je nach Netzwerkkarte und Treiber |
| Windows 10 Build 20348 (Server 2022) | wie oben, Cross-Timestamps erst ab Windows 11 22000.194 |
| Windows 10 21H2/22H2 und älter | nur Userspace (die Winsock-Timestamping-API fehlt) |

Hardware-Timestamping braucht eine Netzwerkkarte, deren Windows-Treiber PTP-Timestamping und
Cross-Timestamps anbietet (z. B. Intel i210/i225/i226 mit aktuellem Intel-Treiber). WLAN,
USB-Adapter und virtuelle Switches (Hyper-V) liefern in der Regel keine Hardware-Timestamps.

Die exe braucht keine weiteren Laufzeitbibliotheken (C-Runtime statisch gelinkt). Adminrechte
sind nur für `install`/`uninstall` und das Firewall-Skript nötig; `run` und `probe` laufen als
normaler Benutzer.

## Hardware- oder Software-Timestamping aktivieren

Windows liefert Zeitstempel nur, wenn sie in den erweiterten Eigenschaften des
Netzwerkadapters eingeschaltet sind (Geräte-Manager → Netzwerkadapter → Eigenschaften →
Erweitert). Bei Intel-Treibern heißen die Einträge in der Regel „PTP Hardware Timestamp“ und
„Software Timestamp“. Laut Microsoft lassen sich beide nicht gleichzeitig aktivieren.

```powershell
# Was bietet der Treiber an?
Get-NetAdapterAdvancedProperty -Name "Ethernet" |
    Where-Object DisplayName -like "*Timestamp*" |
    Format-Table DisplayName, DisplayValue, RegistryKeyword, ValidDisplayValues

# Beispiel Hardware-Timestamping (Name und Werte aus der Ausgabe oben übernehmen)
Set-NetAdapterAdvancedProperty -Name "Ethernet" -DisplayName "PTP Hardware Timestamp" -DisplayValue "Enabled"
```

Danach zeigt `ptpmaster interfaces` unter „Timestamping aktiv“, was Windows meldet. `run`
nimmt mit `mode = "auto"` automatisch den besten Modus. Mit `--timestamping hardware` lässt
sich Hardware erzwingen; der Start bricht dann mit einer Begründung ab, falls es nicht geht.

## Konfiguration

`ptpmaster.toml` liegt neben `ptpmaster.exe` (andere Datei mit `--config <pfad>`). Die
kommentierte Vorlage ist [`ptpmaster.example.toml`](ptpmaster.example.toml); im ZIP liegt sie
schon als `ptpmaster.toml` bei. Reihenfolge: **Profil-Preset → Datei → Kommandozeile**.
Unbekannte Schlüssel sind ein Fehler, damit Tippfehler auffallen.

### Profile

| | `default` | `aes67` |
|---|---|---|
| Domain | 0 | 0 |
| Sync | 1/s (`log_sync_interval = 0`) | 8/s (`-3`) |
| Announce | alle 2 s (`1`) | 2/s (`-1`)¹ |
| Delay_Req (min.) | 1/s (`0`) | 1/s (`0`) |
| announceReceiptTimeout | 3 | 3 |

¹ AES67 Annex A sieht für Announce den Bereich 0..4 mit Default 1 (alle 2 s) vor. 2/s war
Vorgabe für dieses Projekt. Wer strikt nach AES67 will, setzt `log_announce_interval = 1`.

### Wichtige Einstellungen

| TOML (`[ptp]`, `[network]`, …) | CLI-Flag | Standard | Bedeutung |
|---|---|---|---|
| `profile` | `--profile` | `default` | Profil-Preset |
| `ptp.domain` | `--domain` | 0 | domainNumber (0–127) |
| `ptp.priority1` | `--priority1` | 128 | BMCA, kleiner gewinnt, wird zuerst verglichen |
| `ptp.priority2` | `--priority2` | 128 | BMCA, nach der Uhrqualität |
| `ptp.clock_class` | `--clock-class` | 248 | 248 = keine rückführbare Referenz |
| `ptp.clock_accuracy` | `--clock-accuracy` | `0xFE` | Tabelle 6 der Norm, 0xFE = unbekannt |
| `ptp.log_announce_interval` | `--log-announce-interval` | Profil | log2 Sekunden |
| `ptp.log_sync_interval` | `--log-sync-interval` | Profil | log2 Sekunden |
| `ptp.log_min_delay_req_interval` | `--log-min-delay-req-interval` | Profil | log2 Sekunden |
| `ptp.announce_receipt_timeout` | `--announce-receipt-timeout` | 3 | Intervalle bis „fremder Master weg“ |
| `ptp.utc_offset` | `--utc-offset` | 37 | currentUtcOffset (TAI − UTC) |
| `ptp.two_step` | `--one-step` | `true` | Two-Step (Sync + Follow_Up) |
| `ptp.time_source` | – | `ntp` | timeSource im Announce |
| `network.interface` | `-i`, `--interface` | automatisch | Name, Beschreibung, GUID oder IPv4-Adresse |
| `network.ttl` | `--ttl` | 1 | Multicast-TTL |
| `timestamping.mode` | `--timestamping` | `auto` | `auto`, `hardware`, `software`, `userspace` |
| `logging.level` | `--log-level` | `info` | oder Umgebungsvariable `RUST_LOG` |
| `logging.dir` / `max_files` | – | `logs` / 7 | relativ zur exe, tägliche Rotation |

Die Zeitskala ist immer die PTP-Timescale (TAI = UTC + currentUtcOffset). Im Announce sind
`ptpTimescale` und `currentUtcOffsetValid` gesetzt. Die clockIdentity wird als EUI-64 aus der
MAC des gewählten Interfaces gebildet (`00:1b:21:01:02:03` → `001b21.fffe.010203`).

## Befehle

| Befehl | Zweck |
|---|---|
| `ptpmaster run [Flags]` | Master in der Konsole, Live-Status, Ctrl+C beendet |
| `ptpmaster install [--start] [Flags]` | Dienst „PTPMaster“ anlegen (automatischer Start, Neustart bei Absturz). Flags werden in die Dienst-Befehlszeile übernommen. |
| `ptpmaster uninstall` | Dienst stoppen und entfernen |
| `ptpmaster interfaces` | Netzwerkkarten mit MAC, clockIdentity und Timestamp-Fähigkeiten |
| `ptpmaster probe [--duration s] [--delay-req-interval n] [Flags]` | Messmodus gegen den Master (stellt die Uhr nicht) |

Dienst und Logs:

```powershell
sc query PTPMaster
Get-Content C:\ptpmaster\logs\ptpmaster.*.log -Tail 30 -Wait
Get-EventLog -LogName Application -Source PTPMaster -Newest 20
```

Der Dienst schreibt den Status alle 5 Minuten in die Logdatei. Zustandswechsel (MASTER,
PASSIVE), Warnungen und Fehler landen zusätzlich im Event Log.

## Firewall

```powershell
.\scripts\firewall.ps1                                  # Profile Domain + Private
.\scripts\firewall.ps1 -NetworkProfile Domain,Private,Public
.\scripts\firewall.ps1 -Program C:\ptpmaster\ptpmaster.exe   # nur für diese exe
.\scripts\firewall.ps1 -Remove                          # Regeln entfernen
```

Das Skript legt eingehende und ausgehende Regeln für UDP 319 (Event) und UDP 320 (General)
in der Gruppe „ptpmaster (PTP IEEE 1588)“ an. Ohne die eingehenden Regeln sendet der Master
zwar Sync und Announce, bekommt aber keine Delay_Req: Slaves bleiben dann ohne Path Delay
stehen.

## Test mit Wireshark

Mitschnitt auf dem Master-PC (Npcap) oder einem zweiten PC im selben Netz, Anzeigefilter
`ptp`. Was zu sehen sein sollte:

- **Announce** (General, Port 320) im Announce-Intervall: grandmasterPriority1 128,
  clockClass 248, currentUtcOffset 37, Flags `PTP_TIMESCALE` und `UTC_OFFSET_VALID`,
  grandmasterIdentity = clockIdentity aus `ptpmaster interfaces`.
- **Sync** (Event, Port 319) mit `TWO_STEP`-Flag, direkt danach ein **Follow_Up** mit
  derselben sequenceId und dem preciseOriginTimestamp (TAI, also 37 s vor UTC).
- **Delay_Req** des Slaves und das **Delay_Resp** des Masters mit derselben sequenceId und
  der requestingPortIdentity des Slaves.

Nützliche Filter (Feldnamen können je nach Wireshark-Version leicht abweichen):
`ptp.v2.messagetype == 0x0b` (nur Announce), `ptp.v2.an.priority1`, `ptp.v2.sequenceid == 42`.

## Test mit linuxptp

Auf einem Linux-PC im selben Netz (Interface-Name anpassen):

```bash
sudo ptp4l -i eth0 -s -m
```

`-s` = nur Slave, `-m` = Ausgabe auf die Konsole. ptp4l nutzt standardmäßig
Hardware-Timestamping; ohne passende Netzwerkkarte `-S` anhängen (Software). So sieht die
Ausgabe aus, wenn alles läuft (gekürzt, Zahlen nur zur Illustration):

```
ptp4l[…]: port 1 (eth0): new foreign master 001b21.fffe.010203-1
ptp4l[…]: selected best master clock 001b21.fffe.010203
ptp4l[…]: port 1 (eth0): LISTENING to UNCALIBRATED on RS_SLAVE
ptp4l[…]: master offset      -2311 s0 freq   +1203 path delay     52107
ptp4l[…]: master offset       1876 s2 freq   +3409 path delay     51980
```

`s2` heißt eingeregelt; offset und path delay stehen in Nanosekunden. **Achtung:** ptp4l
stellt dabei die Uhr des Linux-PCs (mit `-S` die Systemuhr, sonst die PHC der Netzwerkkarte).
Nur beobachten, ohne zu stellen: `sudo ptp4l -i eth0 -s -m --free_running=1`.

Weitere Varianten:

```bash
sudo ptp4l -i eth0 -s -m --domainNumber=0           # Domain explizit
sudo ptp4l -i eth0 -s -m --hybrid_e2e=1             # Delay_Req per Unicast (ptpmaster antwortet unicast)
```

Mit `ptpmaster probe` auf einem zweiten Windows-PC geht es ohne Linux. probe zeigt jede
Sekunde Offset und Path Delay und am Ende Mittelwert, Standardabweichung, Minimum und Maximum.
Der Offset enthält die Abweichung der beiden Systemuhren (NTP), die Standardabweichung zeigt
den Jitter.

## Genauigkeit: was erreichbar ist und wofür das reicht

Kurz gesagt: **ptpmaster verteilt die Zeit der Windows-Systemuhr. Genauer als diese Uhr kann
kein Slave werden.** w32time hält die Uhr mit Standardeinstellungen nur auf einige
Millisekunden an UTC. Mit gut eingestelltem NTP sind es eher 0,1–1 ms. Außerdem korrigiert
w32time laufend die Frequenz, und alle Slaves machen diese Bewegungen mit. Für viele Zwecke
zählt aber nicht die absolute Zeit, sondern dass alle Geräte *dieselbe* Uhr haben.

Die folgenden Werte sind Erfahrungswerte für die Zeitstempel-Genauigkeit je Modus. Mit diesem
Programm wurden sie **nicht** gemessen; Messungen auf echter Hardware stehen noch aus.

| Modus | Jitter der Zeitstempel (typisch) | Ursache der Grenzen |
|---|---|---|
| Userspace | 10–100 µs, Ausreißer bis in den ms-Bereich | Thread-Scheduling, Interrupt Moderation, Energiesparmodi, Last |
| Software (Treiber) | etwa 1–10 µs | Zeitstempel im Treiber, aber nach DMA/Interrupt |
| Hardware (z. B. i210) | an der Karte deutlich unter 1 µs; die Umrechnung über Cross-Timestamps auf die Systemuhr kostet einige 100 ns bis µs | Qualität der Cross-Timestamps, Wandern der Systemuhr |

Dazu kommen Netzwerkeffekte, die nicht von ptpmaster abhängen: Switches ohne PTP-Unterstützung
(kein Transparent/Boundary Clock) erzeugen lastabhängige Verzögerungen und Asymmetrien von
einigen µs bis zu vielen µs.

**Wofür es reicht**
- Labor, Tests, Interop-Prüfung und Fehlersuche mit PTP-Geräten (AES67, Dante im
  AES67-Modus, ptp4l)
- Kleine, unkritische AES67-Setups, in denen alle Geräte einer gemeinsamen Uhr folgen sollen
  und kein besserer Grandmaster vorhanden ist. Am besten mit Hardware-Timestamping.
- Zeitkorrelation von Logs, Messdaten oder Kameras über mehrere Rechner im Bereich
  Millisekunden bis etwa 100 µs

**Wofür es nicht reicht**
- Messtechnik mit rückführbarer Zeit im µs- oder ns-Bereich (Synchrophasoren, Laufzeitmessung,
  Prüfstände mit Zertifikat). Dafür braucht es einen Grandmaster mit GNSS-Empfänger,
  stabilem Oszillator (OCXO) und Hardware-Timestamping.
- Produktiver Rundfunk- oder Live-Audio-Betrieb (SMPTE ST 2059, große AES67/Dante-Netze).
  Dort gehört ein dedizierter Grandmaster oder ein dafür vorgesehenes Gerät an die Spitze.
  Ein Windows-PC mit Updates, Energiesparen und w32time-Korrekturen ist dafür keine
  verlässliche Zeitquelle.
- Telekom-Profile (G.8275.x), Layer-2- oder Peer-to-Peer-Netze: Diese Teile von PTP setzt
  ptpmaster nicht um.

### Dante und AES67

- Dante selbst nutzt für die eigene Uhr **PTPv1**. ptpmaster spricht nur PTPv2. Dante-Geräte
  nehmen deshalb nur dann am PTPv2-Netz (Domain 0) teil, wenn in Dante Controller der
  **AES67-Modus** eingeschaltet ist. Ob ein Gerät einen externen PTPv2-Master als Leader
  akzeptiert, hängt von Gerät und Firmware ab. In Dante Controller unter „Clock Status“
  prüfen.
- **Wenn im Netz schon ein Dante-Gerät Leader ist, entscheidet die priority1-Einstellung, wer
  gewinnt.** Der kleinere Wert gewinnt. Bei Gleichstand entscheiden clockClass, clockAccuracy,
  Varianz, priority2 und am Ende die clockIdentity.
  - ptpmaster soll gewinnen: `priority1` kleiner setzen als beim Dante-Gerät (z. B. 64 oder 1).
    Den tatsächlichen Wert des Dante-Geräts zeigen `ptpmaster probe` oder Wireshark
    (`ptp.v2.an.priority1`).
  - Das Dante-Gerät soll Leader bleiben: `priority1` größer lassen (z. B. 250). ptpmaster geht
    dann auf PASSIVE, schreibt eine Warnung ins Log und springt nur ein, wenn der Leader
    verschwindet.
- Dante Virtual Soundcard oder Dante Via auf demselben PC nutzen selbst PTP auf den Ports
  319/320. ptpmaster bindet die Ports gemeinsam (SO_REUSEADDR). Belegt das andere Programm sie
  exklusiv, startet ptpmaster mit einer Fehlermeldung nicht. Sonst funktioniert Multicast weiter,
  Unicast-Pakete landen aber nur bei einem der Programme. Besser nicht beides auf einem PC
  betreiben.

### Systemuhr verbessern

```powershell
w32tm /query /status /verbose
w32tm /stripchart /computer:de.pool.ntp.org /samples:5 /dataonly
# Eigene NTP-Server setzen (als Administrator):
w32tm /config /manualpeerlist:"0.de.pool.ntp.org 1.de.pool.ntp.org" /syncfromflags:manual /update
Restart-Service w32time
```

Für engere Regelung kennt Microsoft eine „High Accuracy“-Konfiguration für w32time
(kürzere Poll-Intervalle). Auf einem PC, der selbst Grandmaster ist, darf der w32time-PTP-Client
nicht aktiv sein (er bindet ebenfalls 319/320). Prüfen mit `netstat -ano -p udp | findstr ":319 :320"`.

## Download

Die CI (`.github/workflows/windows.yml`) baut die exe bei jedem Push auf `main`:

- **Neuester Stand:** https://github.com/johann-wiebe/ptpmaster/actions → letzter Lauf von
  „Windows-Build“ → Artifact `ptpmaster-<version>-windows-x64`
- **Releases:** Ein Tag `v*` (z. B. `git tag v0.1.1 && git push origin v0.1.1`) erzeugt ein Release
  mit ZIP und SHA-256: https://github.com/johann-wiebe/ptpmaster/releases

## Bauen

Unter Windows (mit Visual Studio Build Tools): `cargo build --release`.

Von Linux aus mit cargo-xwin (so entsteht das Release, Details in DECISIONS.md, D-01):

```bash
rustup target add x86_64-pc-windows-msvc
cargo install --locked cargo-xwin      # oder Release-Binary von GitHub
./scripts/build-windows.sh             # → dist/ptpmaster-<version>-windows-x64.zip
```

Tests und Prüfungen (laufen unter Linux und Windows):

```bash
cargo test                                   # Unit- und Integrationstests (Loopback-Multicast)
cargo clippy --all-targets -- -D warnings
cargo fmt --check
cargo xwin clippy --target x86_64-pc-windows-msvc --all-targets -- -D warnings
```

Die Integrationstests starten Master und probe über 127.0.0.1 mit hohen Ports (41319 ff.) und
prüfen, dass Sync, Follow_Up, Delay_Req und Delay_Resp vollständig ablaufen. Außerdem prüfen
sie den Wechsel auf PASSIVE und zurück sowie die Robustheit gegen Management- und Müllpakete.
