# Entscheidungen

Der Auftrag lautete, selbstständig ohne Rückfragen zu arbeiten. Hier stehen die Entscheidungen,
die dabei gefallen sind, mit Begründung. Nummern (D-xx) werden im Code referenziert.

## Umgebung und Build

**D-01 Entwicklung unter Linux, Cross-Build mit cargo-xwin.**
Entwickelt wurde auf einer Ubuntu-26.04-VM ohne Windows, ohne Wine, ohne C-Compiler und ohne
sudo. Installiert wurde nur im Benutzerverzeichnis:

| Werkzeug | Version | Zweck |
|---|---|---|
| rustup + Rust stable | 1.100.0 | Toolchain, Targets `x86_64-unknown-linux-gnu` und `x86_64-pc-windows-msvc` |
| Zig | 0.17.1 | nur als `cc`-Ersatz für Linux-Host-Builds (kein gcc vorhanden), Wrapper `~/.local/opt/zigcc/cc`, eingetragen in `~/.cargo/config.toml` |
| cargo-xwin | 0.24.1 (Release-Binary, SHA-256 geprüft) | lädt MSVC-CRT + Windows SDK nach `~/.cache/cargo-xwin` und linkt mit `lld-link` (= rust-lld) |

Windows-Build: `scripts/build-windows.sh` (ruft `cargo xwin build --release --target
x86_64-pc-windows-msvc` und packt `dist/ptpmaster-<version>-windows-x64.zip`). Unter Windows
selbst reicht `cargo build --release` mit installierten MSVC Build Tools.

**D-02 Statische C-Runtime** (`.cargo/config.toml`, `+crt-static`). Ohne sie hing die exe an
`VCRUNTIME140.dll`, also am Visual-C++-Redistributable. Jetzt importiert sie nur System-DLLs.
Das ist mit einem PE-Import-Check geprüft.

**D-03 Keine C-Abhängigkeiten.** Genutzt werden: `windows` 0.62 (Win32-APIs), `socket2`,
`clap`, `serde`/`toml`, `tracing`/`tracing-subscriber`/`tracing-appender`, `anyhow`, unter Unix
`libc` (nur für Tests unter Linux). Den Windows-Dienst habe ich direkt mit `windows`
geschrieben statt mit dem Crate `windows-service`. Die Vorgabe war „Windows-APIs über das Crate
`windows`“.

## Architektur

**D-04 Plattformunabhängiger Kern.** Nachrichten, BMCA, Master-Logik und probe sind portabel.
Nur Timestamping, Interface-Liste, Dienst und Event Log sind Windows-spezifisch (`src/win/`).
Dadurch laufen alle Unit- und Integrationstests auch unter Linux, dort mit
Userspace-Zeitstempeln.

**D-05 Threads statt async.** Drei Threads: Empfang Port 319, Empfang Port 320, Hauptschleife
(Timer für Announce/Sync, BMCA alle 100 ms, Status jede Sekunde). Bei wenigen Paketen pro
Sekunde ist das einfacher und deterministischer als ein async-Runtime.

**D-06 Sockets** binden an `0.0.0.0:319/320` mit `SO_REUSEADDR`, treten der Gruppe
224.0.1.129 auf dem gewählten Interface bei (`IP_ADD_MEMBERSHIP`, `IP_MULTICAST_IF`), TTL
konfigurierbar (Standard 1), Multicast-Loopback an. Grund für `SO_REUSEADDR` und Loopback:
`ptpmaster probe` soll auch auf dem Master-PC selbst laufen können. Unter Windows ist
`SIO_UDP_CONNRESET` aus, damit ICMP „Port unreachable“ den Empfang nicht abbricht. Die Ports
lassen sich (versteckt) ändern, für die Tests unter Linux (Ports < 1024 brauchen dort root).

## Timestamping

**D-07 Verfügbarkeit (Recherche, Microsoft Learn).**

| API | Mindestversion |
|---|---|
| `GetInterfaceActiveTimestampCapabilities`, `GetInterfaceSupportedTimestampCapabilities`, `TIMESTAMPING_CONFIG`/`SIO_TIMESTAMPING` | Windows 10 Build 20348 (= Windows Server 2022); in der Praxis Windows 11 (Build 22000+) |
| `CaptureInterfaceHardwareCrossTimestamp`, `INTERFACE_HARDWARE_CROSSTIMESTAMP` | Windows 11 (10.0.22000.194) / Windows Server 2022; bis Windows 10 2004 „reserved for system use“ |
| `SIO_GET_TX_TIMESTAMP`, `SO_TIMESTAMP`, `SO_TIMESTAMP_ID` | gehören zur selben Winsock-Erweiterung (mstcpip.h); die Seiten nennen keine eigene Version |

Windows 10 21H2/22H2 (Build 1904x) hat die API also **nicht**. Dort läuft ptpmaster im
Userspace-Modus.

**D-08 Dynamisches Laden.** Die drei iphlpapi-Funktionen werden per `GetProcAddress` geladen.
`windows` würde sie statisch importieren, und dann startet die exe auf älteren Windows-Versionen
gar nicht („Einsprungpunkt nicht gefunden“). Geprüft: Die Importtabelle der exe enthält aus
`iphlpapi.dll` nur `GetAdaptersAddresses`.

**D-09 Moduswahl „auto“.** Hardware, wenn die *aktiven* Fähigkeiten HW-RX, HW-TX *und*
Cross-Timestamps melden. Sonst Software, wenn SW-RX und SW-TX aktiv sind. Sonst Userspace.
Laut Microsoft lassen sich HW- und SW-Timestamping nicht gleichzeitig aktivieren. Der Modus gilt
für den ganzen Event-Socket. Gemischte Modi pro Richtung gibt es nicht, damit t1/t4 immer aus
derselben Quelle stammen. Scheitert unterwegs etwas (Cross-Timestamp, `SIO_TIMESTAMPING`),
fällt „auto“ eine Stufe zurück. Ein erzwungener Modus bricht stattdessen mit Fehler ab. Der
aktive Modus wird beim Start geloggt.

**D-10 Zeitdomänen.** Winsock liefert Zählerstände: Software = QPC, Hardware = Uhr der
Netzwerkkarte.
- QPC → UTC: unmittelbar vor jeder Umrechnung wird ein Paar (QPC, `GetSystemTimePreciseAsFileTime`)
  gemessen, das engste von drei Fenstern. Die Frequenzkorrektur durch w32time spielt damit
  keine Rolle.
- NIC → QPC: lineares Modell aus Cross-Timestamps, einmal pro Sekunde aufgenommen, Fenster
  16 Punkte. Steigung aus ältestem und neuestem Punkt, mit nur einem Punkt die Nennfrequenz.
  Rücksprung der NIC-Uhr setzt das Modell zurück.
- **Annahme (nicht dokumentiert, nicht getestet):** Die `SO_TIMESTAMP`-Werte im Hardware-Modus
  liegen in derselben Uhr wie `HardwareClockTimestamp` der Cross-Timestamps. So rechnet auch
  das Microsoft-Beispiel.
- Cross-Timestamps werden auf Plausibilität geprüft (QPC-Werte geordnet, nahe „jetzt“,
  Fenster < 10 ms). Der Puffer ist großzügig gepolstert, falls sich das Struct-Layout ändert.
  Das Microsoft-Beispiel setzt ein `Version`-Feld, das im dokumentierten Struct nicht vorkommt.

**D-11 TX-Timestamps** werden bis zu 20 ms lang abgefragt (Backoff 50 µs → 2 ms). Kommt keiner,
dient der Userspace-Zeitpunkt nach `send` als Ersatz. Das wird gezählt und in der Statuszeile
angezeigt („TS-Ersatz“). Liegengebliebene IDs werden beim nächsten Senden abgeholt, damit der
Treiberpuffer (`TxTimestampsBuffered` = 16) nicht vollläuft.

**D-12 Userspace-Zeitstempel.** RX: direkt nach Rückkehr von `recv`. TX bei Two-Step: direkt
nach Rückkehr von `send`, das liegt näher am tatsächlichen Sendezeitpunkt als „davor“. TX bei
One-Step: direkt *vor* `send`, weil der Wert im Paket stehen muss. Alle Zeiten kommen aus
`GetSystemTimePreciseAsFileTime`.

**D-13 One-Step** ist „Software-One-Step“: Der originTimestamp wird vor dem Senden gesetzt,
die Netzwerkkarte fügt nichts ein. Das ist auch mit Hardware-Timestamping nur so genau wie
Userspace. Deshalb ist Two-Step der Standard.

## Protokoll

**D-14 Profile.**
- `default`: IEEE 1588-2008 J.3. Announce log 1 (alle 2 s), Sync log 0, Delay_Req log 0,
  announceReceiptTimeout 3, Domain 0.
- `aes67`: Sync log −3 (8/s), Delay_Req log 0, announceReceiptTimeout 3, Domain 0,
  **Announce log −1 (2/s) wie im Auftrag vorgegeben.** AES67 Annex A nennt für
  logAnnounceInterval den Bereich 0..4 mit Default 1 (alle 2 s). −1 liegt also außerhalb des
  Profils. In der Praxis stört schnelleres Announce keinen Slave. Wer streng nach AES67 will,
  setzt `log_announce_interval = 1`.

Werte außerhalb der Profilbereiche erzeugen beim Start eine Warnung, keinen Fehler.

**D-15 Zeitskala.** PTP-Timescale (TAI): `PTP = UTC + currentUtcOffset`, Standard 37,
konfigurierbar. Im Announce sind `ptpTimescale` und `currentUtcOffsetValid` gesetzt.
`leap59`/`leap61` werden nicht gesetzt (ptpmaster kennt keine Schaltsekunden-Ankündigung).
Bis 2035 sind keine weiteren Schaltsekunden geplant.

**D-16 Uhrqualität.** clockClass 248, clockAccuracy 0xFE (unbekannt), offsetScaledLogVariance
0xFFFF, timeSource 0x50 (NTP), timeTraceable/frequencyTraceable false. Alle Werte sind
konfigurierbar. Ein PC mit NTP-Systemuhr ist keine rückführbare Referenz, deshalb sind das die
ehrlichen Standardwerte.

**D-17 clockIdentity** = EUI-64 aus der MAC des gewählten Interfaces (OUI + FF FE + Rest,
1588-2008 7.5.2.2.2), portNumber 1. Ohne MAC (Loopback) gibt es eine stabile, lokal
administrierte Ersatz-MAC aus Rechner- und Interfacename. Für Sonderfälle und Tests lässt sich
die Identity über `ptp.clock_identity` fest vorgeben. probe erzeugt bei jedem Start eine
zufällige Identity, damit es neben dem Master auf demselben PC laufen kann.

**D-18 Zustände.** INITIALIZING → LISTENING (announceReceiptTimeout × Announce-Intervall, also
6 s bei `default` und 1,5 s bei `aes67`) → MASTER oder PASSIVE. Eine eigene
PRE_MASTER-Qualifikationszeit gibt es nicht: Bei stepsRemoved = 0 ist sie ohnehin ein
Announce-Intervall, und LISTENING deckt das ab.

**D-19 BMCA.** Datensatzvergleich vollständig nach Figure 27/28 (inklusive Topologie-Fall bei
gleichem Grandmaster). Foreign-Master-Qualifikation: 2 Announce in 4 Intervallen. Ablauf nach
announceReceiptTimeout Intervallen des *fremden* Announce-Intervalls; unplausible Intervalle
werden als 2 s gewertet. Ist der fremde Master besser → PASSIVE (statt SLAVE, kein Slave-Modus),
mit WARN im Log und Hinweis auf priority1. In PASSIVE sendet ptpmaster weder Announce noch Sync
und beantwortet keine Delay_Req. Verschwindet der bessere Master → WARN und wieder MASTER.
Ignoriert werden Announce aus anderen Domains, eigene Nachrichten (gleiche clockIdentity) und
stepsRemoved ≥ 255.

**D-20 Delay_Resp** geht per Multicast an 224.0.1.129:320. Hat der Delay_Req das unicastFlag
gesetzt (Hybrid-Modus, z. B. `ptp4l` mit `hybrid_e2e 1`), geht die Antwort per Unicast an den
Absender. Das correctionField wird aus dem Delay_Req übernommen (11.3.2). logMessageInterval =
logMinDelayReqInterval.

**D-21 Management, Signaling, Pdelay** werden geparst bzw. erkannt und ignoriert (Log auf
debug). Der Parser ist gegen kaputte Pakete abgesichert: Fuzz-artiger Test mit 12 000
gekippten oder abgeschnittenen Varianten. Unbekannte Typen und Versionen ≠ 2 werden verworfen.
PTPv2.1 (minorVersion 1, IEEE 1588-2019) wird akzeptiert. Angehängte TLVs und Padding werden
ignoriert.

**D-22 Nicht umgesetzt** (laut Auftrag nicht nötig oder bewusst weggelassen): IPv6, Layer 2,
Peer-to-Peer-Delay, Slave-Modus, Antworten auf Management-Nachrichten, Unicast-Negotiation,
Schaltsekunden-Flags, PTPv1 (wichtig für Dante, siehe README).

## Betrieb

**D-23 Konfiguration.** Reihenfolge: Profil-Preset → `ptpmaster.toml` neben der exe (oder
`--config`) → CLI-Flags. Unbekannte Schlüssel sind ein Fehler (`deny_unknown_fields`), damit
Tippfehler nicht still ignoriert werden. Fehlt die Datei am Standardpfad, gilt das Profil. Ein
explizit angegebener Pfad muss existieren.

**D-24 Dienst.** Name `PTPMaster`, Konto LocalSystem, Starttyp automatisch, abhängig von
`Tcpip`, Neustart nach 5 s bei Absturz (3×, Zähler-Reset nach 1 Tag). CLI-Overrides bei
`install` werden an die Dienst-Befehlszeile angehängt (Windows-Quoting nach MSVCRT-Regeln, mit
Tests). Startet der Master nicht, z. B. weil das Netzwerk beim Booten noch fehlt, versucht der
Dienst es alle 10 s erneut. Release-Builds nutzen `panic = "abort"`, damit ein Absturz den
Dienst beendet und das Neustart-Verhalten des SCM greift.

**D-25 Logging.** Konsole mit Live-Statuszeile, die unten stehen bleibt (VT-Sequenzen, nur bei
echter Konsole). Logdatei `logs/ptpmaster.<datum>.log` relativ zur exe, tägliche Rotation,
7 Dateien. Im Dienst zusätzlich Event Log „Anwendung“, Quelle `PTPMaster`, ab INFO, ohne die
periodischen Statuszeilen. Damit die Ereignisanzeige den Text sauber zeigt, registriert
`install` die Quelle mit `EventLogMessages.dll` aus .NET Framework 4 (enthält für jede ID nur
„%1“). `RUST_LOG` überschreibt das Log-Level.

**D-26 Systemuhr-Sprünge** (> 5 ms gegenüber der monotonen Uhr, z. B. hartes Stellen durch
w32time) werden als WARN geloggt, weil alle Slaves den Sprung mitmachen.

**D-27 Firewall-Skript** legt je eine Ein- und Ausgangsregel für UDP 319 und 320 in der Gruppe
„ptpmaster (PTP IEEE 1588)“ an. Standard sind die Profile Domain und Private, optional
`-NetworkProfile … Public` und `-Program <exe>`. Es ist idempotent und gespeichert als UTF-8 mit
BOM und CRLF, damit Windows PowerShell 5.1 die Umlaute richtig liest.

## Tests und Grenzen der Prüfung

**D-28 Was geprüft ist.** 49 Unit-Tests: Nachrichten gegen handkonstruierte Byte-Vektoren,
BMCA, Intervalle, TAI/UTC, FILETIME, Konfiguration, Interface-Auswahl, HW-Uhr-Modell, Quoting,
Moduswahl. Dazu 6 Integrationstests über Loopback-Multicast (Two-Step, One-Step, PASSIVE ↔
MASTER, schlechterer Fremdmaster, Management/Müll, Ende-zu-Ende über die Binary), auch unter
Volllast auf allen Kernen wiederholt. Dabei kam eine Race Condition in probe ans Licht und
wurde behoben: Delay_Resp kam vor der Rückkehr von `send_event` an. `cargo clippy -D warnings`
und `cargo fmt` sind für Linux und Windows sauber.

**D-29 Was nicht geprüft werden konnte.** Hier gab es kein Windows und kein Wine. Der
Windows-Code (Winsock-Timestamping, Interface-Liste, Dienst, Event Log, Konsole) ist kompiliert
und mit Clippy geprüft, aber **nie ausgeführt** worden. Hardware-Timestamping (z. B. Intel i210)
und Software-Timestamping des Treibers sind ungetestet. Ebenso ungetestet ist das Zusammenspiel
mit echten Slaves (ptp4l, Dante/AES67). Das PowerShell-Skript ist nur von Hand geprüft; eine
PowerShell zum Testen stand nicht zur Verfügung.

**D-30 Git.** Lokales Repo, Branch `main`, nicht gepusht. Autor „Johann Wiebe“ mit der
noreply-Adresse von GitHub.
