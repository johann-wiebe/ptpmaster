//! Konfiguration: Profil-Preset → TOML-Datei → CLI-Flags (spätere Quelle gewinnt).

use std::fmt;
use std::net::Ipv4Addr;
use std::path::{Path, PathBuf};
use std::str::FromStr;

use anyhow::{Context, Result, bail};
use serde::Deserialize;

use crate::proto::ClockIdentity;
use crate::time::DEFAULT_UTC_OFFSET;

pub const PTP_PRIMARY_MULTICAST: Ipv4Addr = Ipv4Addr::new(224, 0, 1, 129);
pub const PTP_EVENT_PORT: u16 = 319;
pub const PTP_GENERAL_PORT: u16 = 320;
pub const CONFIG_FILE_NAME: &str = "ptpmaster.toml";

#[derive(Clone, Copy, Debug, PartialEq, Eq, Deserialize, clap::ValueEnum, Default)]
#[serde(rename_all = "lowercase")]
pub enum Profile {
    /// IEEE 1588-2008 Annex J.3 Default Profile (Delay Request-Response)
    #[default]
    Default,
    /// AES67 Media Profile (Annex A), Werte wie in DECISIONS.md beschrieben
    Aes67,
}

impl fmt::Display for Profile {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Profile::Default => "default",
            Profile::Aes67 => "aes67",
        })
    }
}

/// Gewünschter Timestamp-Modus. `Auto` wählt den besten verfügbaren.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Deserialize, clap::ValueEnum, Default)]
#[serde(rename_all = "lowercase")]
pub enum TimestampRequest {
    #[default]
    Auto,
    Hardware,
    Software,
    Userspace,
}

impl fmt::Display for TimestampRequest {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            TimestampRequest::Auto => "auto",
            TimestampRequest::Hardware => "hardware",
            TimestampRequest::Software => "software",
            TimestampRequest::Userspace => "userspace",
        })
    }
}

/// timeSource nach IEEE 1588-2008, Tabelle 7.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Deserialize, clap::ValueEnum, Default)]
#[serde(rename_all = "snake_case")]
pub enum TimeSource {
    AtomicClock,
    Gps,
    TerrestrialRadio,
    Ptp,
    #[default]
    Ntp,
    HandSet,
    Other,
    InternalOscillator,
}

impl TimeSource {
    pub fn code(self) -> u8 {
        match self {
            TimeSource::AtomicClock => 0x10,
            TimeSource::Gps => 0x20,
            TimeSource::TerrestrialRadio => 0x30,
            TimeSource::Ptp => 0x40,
            TimeSource::Ntp => 0x50,
            TimeSource::HandSet => 0x60,
            TimeSource::Other => 0x90,
            TimeSource::InternalOscillator => 0xA0,
        }
    }
}

/// Profilwerte (IEEE 1588-2008 Annex J.3 bzw. AES67 Annex A).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ProfilePreset {
    pub domain: u8,
    pub log_announce_interval: i8,
    pub log_sync_interval: i8,
    pub log_min_delay_req_interval: i8,
    pub announce_receipt_timeout: u8,
    /// Erlaubte Bereiche laut Profil (für Warnungen)
    pub announce_range: (i8, i8),
    pub sync_range: (i8, i8),
    pub delay_req_range: (i8, i8),
}

impl Profile {
    pub fn preset(self) -> ProfilePreset {
        match self {
            Profile::Default => ProfilePreset {
                domain: 0,
                log_announce_interval: 1,
                log_sync_interval: 0,
                log_min_delay_req_interval: 0,
                announce_receipt_timeout: 3,
                announce_range: (0, 4),
                sync_range: (-1, 1),
                delay_req_range: (0, 5),
            },
            Profile::Aes67 => ProfilePreset {
                domain: 0,
                // Vorgabe aus dem Auftrag: Announce 2/s. AES67 Annex A nennt als Default 1 (alle 2 s),
                // siehe DECISIONS.md.
                log_announce_interval: -1,
                log_sync_interval: -3,
                log_min_delay_req_interval: 0,
                announce_receipt_timeout: 3,
                announce_range: (-3, 4),
                sync_range: (-4, 1),
                delay_req_range: (-3, 5),
            },
        }
    }
}

// ---------------------------------------------------------------------------
// TOML-Datei (alle Felder optional)
// ---------------------------------------------------------------------------

#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct FileConfig {
    pub profile: Option<Profile>,
    #[serde(default)]
    pub ptp: PtpSection,
    #[serde(default)]
    pub network: NetworkSection,
    #[serde(default)]
    pub timestamping: TimestampingSection,
    #[serde(default)]
    pub logging: LoggingSection,
}

#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PtpSection {
    pub domain: Option<u8>,
    pub priority1: Option<u8>,
    pub priority2: Option<u8>,
    pub clock_class: Option<u8>,
    pub clock_accuracy: Option<u8>,
    pub offset_scaled_log_variance: Option<u16>,
    pub log_announce_interval: Option<i8>,
    pub log_sync_interval: Option<i8>,
    pub log_min_delay_req_interval: Option<i8>,
    pub announce_receipt_timeout: Option<u8>,
    pub utc_offset: Option<i16>,
    pub two_step: Option<bool>,
    pub time_source: Option<TimeSource>,
    pub time_traceable: Option<bool>,
    pub frequency_traceable: Option<bool>,
    pub clock_identity: Option<String>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct NetworkSection {
    pub interface: Option<String>,
    pub ttl: Option<u32>,
    pub multicast_group: Option<Ipv4Addr>,
    pub event_port: Option<u16>,
    pub general_port: Option<u16>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TimestampingSection {
    pub mode: Option<TimestampRequest>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct LoggingSection {
    pub level: Option<String>,
    pub file: Option<bool>,
    pub dir: Option<PathBuf>,
    pub max_files: Option<usize>,
}

impl FileConfig {
    pub fn parse(text: &str) -> Result<Self> {
        toml::from_str(text).context("Konfiguration ist kein gültiges TOML")
    }

    pub fn load(path: &Path) -> Result<Self> {
        let text = std::fs::read_to_string(path)
            .with_context(|| format!("Konfiguration {} nicht lesbar", path.display()))?;
        Self::parse(&text).with_context(|| format!("Fehler in {}", path.display()))
    }
}

// ---------------------------------------------------------------------------
// CLI-Overrides
// ---------------------------------------------------------------------------

/// Flags, die Werte aus der Konfigurationsdatei überschreiben.
#[derive(Debug, Default, Clone, clap::Args)]
pub struct Overrides {
    /// Profil-Preset (default | aes67)
    #[arg(long)]
    pub profile: Option<Profile>,
    /// domainNumber (0–127)
    #[arg(long)]
    pub domain: Option<u8>,
    /// priority1 (0–255, kleiner gewinnt)
    #[arg(long)]
    pub priority1: Option<u8>,
    /// priority2 (0–255, kleiner gewinnt)
    #[arg(long)]
    pub priority2: Option<u8>,
    /// clockClass (Standard 248)
    #[arg(long)]
    pub clock_class: Option<u8>,
    /// clockAccuracy (Tabelle 6, z. B. 0x29 = 1 ms, 0xFE = unbekannt; dezimal oder 0x..)
    #[arg(long, value_parser = parse_u8_auto)]
    pub clock_accuracy: Option<u8>,
    /// logAnnounceInterval (log2 Sekunden)
    #[arg(long, allow_negative_numbers = true)]
    pub log_announce_interval: Option<i8>,
    /// logSyncInterval (log2 Sekunden, -3 = 8/s)
    #[arg(long, allow_negative_numbers = true)]
    pub log_sync_interval: Option<i8>,
    /// logMinDelayReqInterval (log2 Sekunden)
    #[arg(long, allow_negative_numbers = true)]
    pub log_min_delay_req_interval: Option<i8>,
    /// announceReceiptTimeout (Anzahl Announce-Intervalle)
    #[arg(long)]
    pub announce_receipt_timeout: Option<u8>,
    /// currentUtcOffset in Sekunden (TAI − UTC, Standard 37)
    #[arg(long, allow_negative_numbers = true)]
    pub utc_offset: Option<i16>,
    /// One-Step-Sync statt Two-Step (Sync + Follow_Up)
    #[arg(long)]
    pub one_step: bool,
    /// Netzwerkinterface: Name, Beschreibung, Adapter-GUID oder IPv4-Adresse
    #[arg(long, short = 'i')]
    pub interface: Option<String>,
    /// Multicast-TTL
    #[arg(long)]
    pub ttl: Option<u32>,
    /// Timestamp-Modus erzwingen
    #[arg(long)]
    pub timestamping: Option<TimestampRequest>,
    /// Log-Level (error, warn, info, debug, trace)
    #[arg(long)]
    pub log_level: Option<String>,
    /// Event-Port (nur für Tests, Standard 319)
    #[arg(long, hide = true)]
    pub event_port: Option<u16>,
    /// General-Port (nur für Tests, Standard 320)
    #[arg(long, hide = true)]
    pub general_port: Option<u16>,
}

fn parse_u8_auto(s: &str) -> Result<u8, String> {
    let s = s.trim();
    let r = if let Some(hex) = s.strip_prefix("0x").or_else(|| s.strip_prefix("0X")) {
        u8::from_str_radix(hex, 16)
    } else {
        u8::from_str(s)
    };
    r.map_err(|e| format!("ungültiger Wert '{s}': {e}"))
}

impl Overrides {
    /// Für `install`: Overrides als Argumente an die Dienst-Kommandozeile anhängen.
    pub fn to_args(&self) -> Vec<String> {
        let mut a = Vec::new();
        macro_rules! opt {
            ($field:ident, $flag:literal) => {
                if let Some(v) = &self.$field {
                    a.push(format!("--{}={}", $flag, v));
                }
            };
        }
        opt!(profile, "profile");
        opt!(domain, "domain");
        opt!(priority1, "priority1");
        opt!(priority2, "priority2");
        opt!(clock_class, "clock-class");
        opt!(clock_accuracy, "clock-accuracy");
        opt!(log_announce_interval, "log-announce-interval");
        opt!(log_sync_interval, "log-sync-interval");
        opt!(log_min_delay_req_interval, "log-min-delay-req-interval");
        opt!(announce_receipt_timeout, "announce-receipt-timeout");
        opt!(utc_offset, "utc-offset");
        opt!(interface, "interface");
        opt!(ttl, "ttl");
        opt!(timestamping, "timestamping");
        opt!(log_level, "log-level");
        opt!(event_port, "event-port");
        opt!(general_port, "general-port");
        if self.one_step {
            a.push("--one-step".into());
        }
        a
    }
}

// ---------------------------------------------------------------------------
// Aufgelöste Konfiguration
// ---------------------------------------------------------------------------

#[derive(Clone, Debug, PartialEq)]
pub struct Config {
    pub profile: Profile,
    pub domain: u8,
    pub priority1: u8,
    pub priority2: u8,
    pub clock_class: u8,
    pub clock_accuracy: u8,
    pub offset_scaled_log_variance: u16,
    pub log_announce_interval: i8,
    pub log_sync_interval: i8,
    pub log_min_delay_req_interval: i8,
    pub announce_receipt_timeout: u8,
    pub utc_offset: i16,
    pub two_step: bool,
    pub time_source: TimeSource,
    pub time_traceable: bool,
    pub frequency_traceable: bool,
    /// Feste clockIdentity statt EUI-64 aus der MAC (nur für Sonderfälle/Tests)
    pub clock_identity: Option<ClockIdentity>,
    pub interface: Option<String>,
    pub ttl: u32,
    pub multicast_group: Ipv4Addr,
    pub event_port: u16,
    pub general_port: u16,
    pub timestamping: TimestampRequest,
    pub log_level: String,
    pub log_file: bool,
    pub log_dir: PathBuf,
    pub log_max_files: usize,
}

impl Config {
    /// Standardwerte eines Profils.
    pub fn from_profile(profile: Profile) -> Self {
        let p = profile.preset();
        Self {
            profile,
            domain: p.domain,
            priority1: 128,
            priority2: 128,
            clock_class: 248,
            clock_accuracy: 0xFE,
            offset_scaled_log_variance: 0xFFFF,
            log_announce_interval: p.log_announce_interval,
            log_sync_interval: p.log_sync_interval,
            log_min_delay_req_interval: p.log_min_delay_req_interval,
            announce_receipt_timeout: p.announce_receipt_timeout,
            utc_offset: DEFAULT_UTC_OFFSET,
            two_step: true,
            time_source: TimeSource::Ntp,
            time_traceable: false,
            frequency_traceable: false,
            clock_identity: None,
            interface: None,
            ttl: 1,
            multicast_group: PTP_PRIMARY_MULTICAST,
            event_port: PTP_EVENT_PORT,
            general_port: PTP_GENERAL_PORT,
            timestamping: TimestampRequest::Auto,
            log_level: "info".into(),
            log_file: true,
            log_dir: PathBuf::from("logs"),
            log_max_files: 7,
        }
    }

    /// Profil-Preset → Datei → CLI.
    pub fn resolve(file: &FileConfig, cli: &Overrides) -> Result<Self> {
        let profile = cli.profile.or(file.profile).unwrap_or_default();
        let mut c = Self::from_profile(profile);

        let p = &file.ptp;
        macro_rules! set {
            ($dst:ident, $src:expr) => {
                if let Some(v) = $src.clone() {
                    c.$dst = v;
                }
            };
        }
        set!(domain, p.domain);
        set!(priority1, p.priority1);
        set!(priority2, p.priority2);
        set!(clock_class, p.clock_class);
        set!(clock_accuracy, p.clock_accuracy);
        set!(offset_scaled_log_variance, p.offset_scaled_log_variance);
        set!(log_announce_interval, p.log_announce_interval);
        set!(log_sync_interval, p.log_sync_interval);
        set!(log_min_delay_req_interval, p.log_min_delay_req_interval);
        set!(announce_receipt_timeout, p.announce_receipt_timeout);
        set!(utc_offset, p.utc_offset);
        set!(two_step, p.two_step);
        set!(time_source, p.time_source);
        set!(time_traceable, p.time_traceable);
        set!(frequency_traceable, p.frequency_traceable);
        if let Some(id) = &p.clock_identity {
            c.clock_identity = Some(ClockIdentity::parse(id).with_context(|| {
                format!("clock_identity '{id}' ist keine 8-Byte-Hexzahl (z. B. 001b21.fffe.010203)")
            })?);
        }
        let n = &file.network;
        if n.interface.is_some() {
            c.interface = n.interface.clone();
        }
        set!(ttl, n.ttl);
        set!(multicast_group, n.multicast_group);
        set!(event_port, n.event_port);
        set!(general_port, n.general_port);
        set!(timestamping, file.timestamping.mode);
        let l = &file.logging;
        set!(log_level, l.level);
        set!(log_file, l.file);
        set!(log_dir, l.dir);
        set!(log_max_files, l.max_files);

        set!(domain, cli.domain);
        set!(priority1, cli.priority1);
        set!(priority2, cli.priority2);
        set!(clock_class, cli.clock_class);
        set!(clock_accuracy, cli.clock_accuracy);
        set!(log_announce_interval, cli.log_announce_interval);
        set!(log_sync_interval, cli.log_sync_interval);
        set!(log_min_delay_req_interval, cli.log_min_delay_req_interval);
        set!(announce_receipt_timeout, cli.announce_receipt_timeout);
        set!(utc_offset, cli.utc_offset);
        if cli.one_step {
            c.two_step = false;
        }
        if cli.interface.is_some() {
            c.interface = cli.interface.clone();
        }
        set!(ttl, cli.ttl);
        set!(timestamping, cli.timestamping);
        set!(log_level, cli.log_level);
        set!(event_port, cli.event_port);
        set!(general_port, cli.general_port);

        c.validate()?;
        Ok(c)
    }

    /// Harte Fehler. Profilabweichungen liefert [`Config::warnings`].
    pub fn validate(&self) -> Result<()> {
        if self.domain > 127 {
            bail!(
                "domainNumber {} ist ungültig (0–127, 128–255 sind reserviert)",
                self.domain
            );
        }
        for (name, v) in [
            ("log_announce_interval", self.log_announce_interval),
            ("log_sync_interval", self.log_sync_interval),
            (
                "log_min_delay_req_interval",
                self.log_min_delay_req_interval,
            ),
        ] {
            if !(-7..=7).contains(&v) {
                bail!("{name} = {v} liegt außerhalb von -7..7");
            }
        }
        if self.announce_receipt_timeout < 2 {
            bail!("announce_receipt_timeout muss mindestens 2 sein");
        }
        if self.clock_class == 255 {
            bail!("clockClass 255 ist nur für Slave-only-Uhren erlaubt");
        }
        if self.ttl == 0 || self.ttl > 255 {
            bail!("ttl muss zwischen 1 und 255 liegen");
        }
        if !self.multicast_group.is_multicast() {
            bail!(
                "multicast_group {} ist keine Multicast-Adresse",
                self.multicast_group
            );
        }
        if self.event_port == self.general_port {
            bail!("event_port und general_port müssen verschieden sein");
        }
        Ok(())
    }

    /// Hinweise auf Werte außerhalb der Profilbereiche.
    pub fn warnings(&self) -> Vec<String> {
        let p = self.profile.preset();
        let mut w = Vec::new();
        let mut check = |name: &str, v: i8, (lo, hi): (i8, i8)| {
            if v < lo || v > hi {
                w.push(format!(
                    "{name} = {v} liegt außerhalb des Bereichs {lo}..{hi} des Profils '{}'",
                    self.profile
                ));
            }
        };
        check(
            "log_announce_interval",
            self.log_announce_interval,
            p.announce_range,
        );
        check("log_sync_interval", self.log_sync_interval, p.sync_range);
        check(
            "log_min_delay_req_interval",
            self.log_min_delay_req_interval,
            p.delay_req_range,
        );
        if self.event_port != PTP_EVENT_PORT || self.general_port != PTP_GENERAL_PORT {
            w.push(format!(
                "Nicht-Standard-Ports {}/{} – normale PTP-Geräte hören nur auf 319/320",
                self.event_port, self.general_port
            ));
        }
        w
    }

    /// Log-Verzeichnis absolut machen (relativ zur exe, nicht zum Arbeitsverzeichnis;
    /// ein Dienst startet in C:\Windows\System32).
    pub fn absolute_log_dir(&self) -> PathBuf {
        if self.log_dir.is_absolute() {
            self.log_dir.clone()
        } else {
            exe_dir().join(&self.log_dir)
        }
    }
}

/// Verzeichnis der laufenden exe (Fallback: Arbeitsverzeichnis).
pub fn exe_dir() -> PathBuf {
    std::env::current_exe()
        .ok()
        .and_then(|p| p.parent().map(Path::to_path_buf))
        .unwrap_or_else(|| PathBuf::from("."))
}

/// Standardpfad der Konfigurationsdatei: neben der exe.
pub fn default_config_path() -> PathBuf {
    exe_dir().join(CONFIG_FILE_NAME)
}

/// Lädt die Konfiguration. Fehlt die Datei am Standardpfad, gelten die Profilwerte.
/// Ein explizit angegebener Pfad muss existieren.
pub fn load(explicit: Option<&Path>, cli: &Overrides) -> Result<(Config, Option<PathBuf>)> {
    let (file, used) = match explicit {
        Some(p) => (FileConfig::load(p)?, Some(p.to_path_buf())),
        None => {
            let p = default_config_path();
            if p.exists() {
                (FileConfig::load(&p)?, Some(p))
            } else {
                (FileConfig::default(), None)
            }
        }
    };
    Ok((Config::resolve(&file, cli)?, used))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults() {
        let c = Config::resolve(&FileConfig::default(), &Overrides::default()).unwrap();
        assert_eq!(c.profile, Profile::Default);
        assert_eq!(c.domain, 0);
        assert_eq!(c.clock_class, 248);
        assert_eq!(c.utc_offset, 37);
        assert_eq!(c.log_sync_interval, 0);
        assert_eq!(c.log_announce_interval, 1);
        assert!(c.two_step);
        assert!(c.warnings().is_empty());
    }

    #[test]
    fn aes67_preset() {
        let file = FileConfig::parse("profile = \"aes67\"").unwrap();
        let c = Config::resolve(&file, &Overrides::default()).unwrap();
        assert_eq!(c.log_sync_interval, -3);
        assert_eq!(c.log_announce_interval, -1);
        assert_eq!(c.domain, 0);
    }

    #[test]
    fn precedence_file_then_cli() {
        let file = FileConfig::parse(
            r#"
            profile = "aes67"
            [ptp]
            priority1 = 100
            log_sync_interval = -2
            clock_accuracy = 0x31
            [network]
            interface = "Ethernet"
            ttl = 4
            "#,
        )
        .unwrap();
        let cli = Overrides {
            priority1: Some(90),
            one_step: true,
            ..Default::default()
        };
        let c = Config::resolve(&file, &cli).unwrap();
        assert_eq!(c.priority1, 90); // CLI schlägt Datei
        assert_eq!(c.log_sync_interval, -2); // Datei schlägt Profil
        assert_eq!(c.log_announce_interval, -1); // Profil
        assert_eq!(c.clock_accuracy, 0x31);
        assert_eq!(c.interface.as_deref(), Some("Ethernet"));
        assert_eq!(c.ttl, 4);
        assert!(!c.two_step);
    }

    #[test]
    fn cli_profile_overrides_file_profile() {
        let file = FileConfig::parse("profile = \"aes67\"").unwrap();
        let cli = Overrides {
            profile: Some(Profile::Default),
            ..Default::default()
        };
        assert_eq!(Config::resolve(&file, &cli).unwrap().log_sync_interval, 0);
    }

    #[test]
    fn rejects_invalid() {
        assert!(FileConfig::parse("[ptp]\nunknown_key = 1").is_err());
        let file = FileConfig::parse("[ptp]\ndomain = 200").unwrap();
        assert!(Config::resolve(&file, &Overrides::default()).is_err());
        let file = FileConfig::parse("[ptp]\nclock_class = 255").unwrap();
        assert!(Config::resolve(&file, &Overrides::default()).is_err());
        let file = FileConfig::parse("[network]\nmulticast_group = \"10.0.0.1\"").unwrap();
        assert!(Config::resolve(&file, &Overrides::default()).is_err());
        let file = FileConfig::parse("[ptp]\nclock_identity = \"xyz\"").unwrap();
        assert!(Config::resolve(&file, &Overrides::default()).is_err());
        let file = FileConfig::parse("[ptp]\nclock_identity = \"001b21.fffe.010203\"").unwrap();
        assert!(
            Config::resolve(&file, &Overrides::default())
                .unwrap()
                .clock_identity
                .is_some()
        );
    }

    #[test]
    fn profile_range_warning() {
        let cli = Overrides {
            log_sync_interval: Some(-5),
            ..Default::default()
        };
        let c = Config::resolve(&FileConfig::default(), &cli).unwrap();
        assert_eq!(c.warnings().len(), 1);
    }

    #[test]
    fn example_config_parses() {
        let text = include_str!("../ptpmaster.example.toml");
        let file = FileConfig::parse(text).unwrap();
        let c = Config::resolve(&file, &Overrides::default()).unwrap();
        assert_eq!(c, Config::from_profile(Profile::Default));
    }

    #[test]
    fn overrides_roundtrip_args() {
        let o = Overrides {
            domain: Some(3),
            log_sync_interval: Some(-3),
            interface: Some("Ethernet 2".into()),
            one_step: true,
            ..Default::default()
        };
        let args = o.to_args();
        assert!(args.contains(&"--domain=3".to_string()));
        assert!(args.contains(&"--log-sync-interval=-3".to_string()));
        assert!(args.contains(&"--interface=Ethernet 2".to_string()));
        assert!(args.contains(&"--one-step".to_string()));
    }

    #[test]
    fn hex_parser() {
        assert_eq!(parse_u8_auto("0x31"), Ok(0x31));
        assert_eq!(parse_u8_auto("254"), Ok(254));
        assert!(parse_u8_auto("0x1FF").is_err());
    }
}
