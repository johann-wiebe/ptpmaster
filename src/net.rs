//! UDP/IPv4-Sockets für PTP mit Timestamping (Hardware, Software oder Userspace).

use std::fmt;
use std::io;
use std::net::{Ipv4Addr, SocketAddrV4, UdpSocket};
use std::sync::atomic::{AtomicU32, AtomicU64};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use anyhow::{Context, Result, bail};
use socket2::{Domain, Protocol, Socket, Type};

use crate::clock::{HwClockModel, system_utc_ns};
use crate::config::{Config, TimestampRequest};
use crate::iface::{self, InterfaceInfo};
use crate::proto::ClockIdentity;

/// Timestamp-Fähigkeiten eines Interfaces (aus GetInterface*TimestampCapabilities).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct TsCaps {
    pub hw_rx: bool,
    pub hw_tx: bool,
    pub sw_rx: bool,
    pub sw_tx: bool,
    pub cross_timestamp: bool,
    pub hw_clock_hz: u64,
}

impl TsCaps {
    /// Hardware-Timestamps lassen sich nur mit Cross-Timestamps auf die Systemuhr abbilden.
    pub fn hardware_usable(&self) -> bool {
        self.hw_rx && self.hw_tx && self.cross_timestamp
    }

    pub fn software_usable(&self) -> bool {
        self.sw_rx && self.sw_tx
    }

    pub fn describe(&self) -> String {
        let yn = |b: bool| if b { "ja" } else { "nein" };
        format!(
            "HW RX {} / TX {}, Cross-Timestamp {}, HW-Uhr {}; SW RX {} / TX {}",
            yn(self.hw_rx),
            yn(self.hw_tx),
            yn(self.cross_timestamp),
            if self.hw_clock_hz > 0 {
                format!("{} MHz", self.hw_clock_hz / 1_000_000)
            } else {
                "-".into()
            },
            yn(self.sw_rx),
            yn(self.sw_tx)
        )
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TimestampMode {
    Hardware,
    Software,
    Userspace,
}

impl fmt::Display for TimestampMode {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            TimestampMode::Hardware => "Hardware",
            TimestampMode::Software => "Software",
            TimestampMode::Userspace => "Userspace",
        })
    }
}

/// Modus aus Wunsch und aktiven Fähigkeiten bestimmen.
/// `caps = Err(..)`: API fehlt oder Treiber kann kein Timestamping.
pub fn choose_mode(
    req: TimestampRequest,
    caps: &std::result::Result<TsCaps, String>,
) -> Result<(TimestampMode, String)> {
    let hw = matches!(caps, Ok(c) if c.hardware_usable());
    let sw = matches!(caps, Ok(c) if c.software_usable());
    let why_not = |what: &str| match caps {
        Err(e) => format!("{what} nicht verfügbar: {e}"),
        Ok(c) => format!("{what} nicht aktiv ({})", c.describe()),
    };
    Ok(match req {
        TimestampRequest::Auto if hw => (
            TimestampMode::Hardware,
            "Hardware-Timestamping aktiv".into(),
        ),
        TimestampRequest::Auto if sw => (
            TimestampMode::Software,
            format!(
                "Software-Timestamping des Treibers ({})",
                why_not("Hardware")
            ),
        ),
        TimestampRequest::Auto => (
            TimestampMode::Userspace,
            why_not("Hardware-/Software-Timestamping"),
        ),
        TimestampRequest::Hardware if hw => (
            TimestampMode::Hardware,
            "Hardware-Timestamping (erzwungen)".into(),
        ),
        TimestampRequest::Hardware => bail!("{}", why_not("Hardware-Timestamping")),
        TimestampRequest::Software if sw => (
            TimestampMode::Software,
            "Software-Timestamping (erzwungen)".into(),
        ),
        TimestampRequest::Software => bail!("{}", why_not("Software-Timestamping")),
        TimestampRequest::Userspace => (
            TimestampMode::Userspace,
            "Userspace-Zeitstempel (erzwungen)".into(),
        ),
    })
}

/// Rechnet rohe Socket-Timestamps in Systemzeit (UTC ns) um.
pub struct TimeBase {
    pub mode: TimestampMode,
    #[cfg_attr(not(windows), allow(dead_code))]
    luid: u64,
    hw: Mutex<HwClockModel>,
}

impl TimeBase {
    pub fn new(mode: TimestampMode, luid: u64, caps: Option<TsCaps>) -> Self {
        #[cfg(windows)]
        let qpc_hz = crate::win::qpc_frequency();
        #[cfg(not(windows))]
        let qpc_hz = 1_000_000_000;
        Self {
            mode,
            luid,
            hw: Mutex::new(HwClockModel::new(
                caps.map(|c| c.hw_clock_hz).unwrap_or(0),
                qpc_hz,
                16,
            )),
        }
    }

    /// Im Hardware-Modus einen neuen Cross-Timestamp aufnehmen (etwa einmal pro Sekunde).
    pub fn refresh(&self) -> std::result::Result<(), String> {
        if self.mode != TimestampMode::Hardware {
            return Ok(());
        }
        #[cfg(windows)]
        {
            let ct = crate::win::tscaps::capture_cross_timestamp(self.luid)?;
            if let Ok(mut m) = self.hw.lock() {
                m.add(ct.hw, ct.qpc_before, ct.qpc_after);
            }
            Ok(())
        }
        #[cfg(not(windows))]
        Err("Hardware-Timestamping gibt es nur unter Windows".into())
    }

    pub fn hw_clock_hz(&self) -> Option<f64> {
        self.hw.lock().ok()?.estimated_hz()
    }

    /// Roher Zählerstand (QPC oder NIC-Uhr) → UTC ns.
    pub fn raw_to_utc(&self, raw: u64) -> Option<i128> {
        #[cfg(windows)]
        {
            match self.mode {
                TimestampMode::Software => Some(crate::win::qpc_to_utc_ns(raw as i64)),
                TimestampMode::Hardware => {
                    let qpc = self.hw.lock().ok()?.hw_to_qpc(raw)?;
                    Some(crate::win::qpc_to_utc_ns(qpc as i64))
                }
                TimestampMode::Userspace => None,
            }
        }
        #[cfg(not(windows))]
        {
            let _ = raw;
            None
        }
    }
}

/// Woher ein einzelner Zeitstempel tatsächlich stammt.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TsSource {
    Hardware,
    Software,
    Userspace,
}

impl From<TimestampMode> for TsSource {
    fn from(m: TimestampMode) -> Self {
        match m {
            TimestampMode::Hardware => TsSource::Hardware,
            TimestampMode::Software => TsSource::Software,
            TimestampMode::Userspace => TsSource::Userspace,
        }
    }
}

#[derive(Clone, Copy, Debug)]
pub struct TxStamp {
    pub utc_ns: i128,
    pub source: TsSource,
}

#[derive(Clone, Copy, Debug)]
pub struct RxPacket {
    pub len: usize,
    pub src: SocketAddrV4,
    pub utc_ns: i128,
    pub source: TsSource,
}

#[derive(Default, Debug)]
pub struct SocketStats {
    /// TX-Timestamp kam nicht rechtzeitig → Userspace-Ersatz
    pub tx_ts_fallback: AtomicU64,
    /// RX-Paket ohne Timestamp → Userspace-Ersatz
    pub rx_ts_fallback: AtomicU64,
}

#[derive(Clone, Debug)]
pub struct SocketSpec {
    pub port: u16,
    pub iface_ip: Ipv4Addr,
    pub group: Ipv4Addr,
    pub ttl: u32,
}

pub struct PtpSocket {
    udp: UdpSocket,
    #[cfg_attr(not(windows), allow(dead_code))]
    timebase: Arc<TimeBase>,
    #[cfg(windows)]
    win: Option<crate::win::sock::WinTsSocket>,
    #[cfg_attr(not(windows), allow(dead_code))]
    next_id: AtomicU32,
    #[cfg_attr(not(windows), allow(dead_code))]
    abandoned: Mutex<Vec<u32>>,
    pub stats: SocketStats,
}

const RECV_TIMEOUT: Duration = Duration::from_millis(200);

impl PtpSocket {
    /// Socket öffnen, an 0.0.0.0:port binden, Multicast-Gruppe auf dem Interface beitreten.
    /// `timestamping`: Winsock-Timestamping (RX+TX) einschalten (nur Event-Socket).
    pub fn open(spec: &SocketSpec, timestamping: bool, timebase: Arc<TimeBase>) -> Result<Self> {
        let sock = Socket::new(Domain::IPV4, Type::DGRAM, Some(Protocol::UDP))
            .context("UDP-Socket nicht anlegbar")?;
        // Mehrere Prozesse (z. B. ptpmaster run + probe auf demselben PC) dürfen 319/320 teilen.
        sock.set_reuse_address(true)?;
        #[cfg(unix)]
        sock.set_reuse_port(true)?;
        let bind = SocketAddrV4::new(Ipv4Addr::UNSPECIFIED, spec.port);
        sock.bind(&bind.into()).with_context(|| {
            format!(
                "Port {} nicht bindbar (fehlende Rechte oder von anderer Software exklusiv belegt)",
                spec.port
            )
        })?;
        sock.join_multicast_v4(&spec.group, &spec.iface_ip)
            .with_context(|| {
                format!(
                    "Multicast-Beitritt {} auf {} fehlgeschlagen",
                    spec.group, spec.iface_ip
                )
            })?;
        sock.set_multicast_if_v4(&spec.iface_ip)?;
        sock.set_multicast_ttl_v4(spec.ttl)?;
        sock.set_multicast_loop_v4(true)?;
        sock.set_read_timeout(Some(RECV_TIMEOUT))?;
        let udp: UdpSocket = sock.into();

        #[cfg(windows)]
        let win = {
            use std::os::windows::io::AsRawSocket;
            let raw = udp.as_raw_socket();
            crate::win::sock::disable_udp_connreset(raw);
            if timestamping && timebase.mode != TimestampMode::Userspace {
                let mut w = crate::win::sock::WinTsSocket::new(raw)
                    .context("WSARecvMsg nicht verfügbar")?;
                w.enable_timestamping(true, true)
                    .context("SIO_TIMESTAMPING abgelehnt")?;
                Some(w)
            } else {
                None
            }
        };
        #[cfg(not(windows))]
        let _ = timestamping;

        Ok(Self {
            udp,
            timebase,
            #[cfg(windows)]
            win,
            next_id: AtomicU32::new(1),
            abandoned: Mutex::new(Vec::new()),
            stats: SocketStats::default(),
        })
    }

    /// Senden ohne Zeitstempel (General-Nachrichten).
    pub fn send(&self, buf: &[u8], dest: SocketAddrV4) -> io::Result<()> {
        self.udp.send_to(buf, dest).map(|_| ())
    }

    /// Event-Nachricht senden und den Sendezeitpunkt liefern.
    pub fn send_event(&self, buf: &[u8], dest: SocketAddrV4) -> io::Result<TxStamp> {
        #[cfg(windows)]
        if let Some(w) = &self.win
            && w.tx_enabled
        {
            return self.send_event_win(w, buf, dest);
        }
        self.udp.send_to(buf, dest)?;
        Ok(TxStamp {
            utc_ns: system_utc_ns(),
            source: TsSource::Userspace,
        })
    }

    #[cfg(windows)]
    fn send_event_win(
        &self,
        w: &crate::win::sock::WinTsSocket,
        buf: &[u8],
        dest: SocketAddrV4,
    ) -> io::Result<TxStamp> {
        use std::time::Instant;
        // Liegengebliebene Timestamps abholen, damit der Puffer im Treiber nicht vollläuft.
        if let Ok(mut ab) = self.abandoned.lock() {
            ab.retain(|id| matches!(w.tx_timestamp(*id), Ok(None)));
            let excess = ab.len().saturating_sub(64);
            ab.drain(..excess);
        }
        let id = self
            .next_id
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        w.send_with_id(buf, dest, id)?;
        let after = system_utc_ns();
        let deadline = Instant::now() + Duration::from_millis(20);
        let mut wait = Duration::from_micros(50);
        loop {
            match w.tx_timestamp(id) {
                Ok(Some(raw)) => {
                    if let Some(utc_ns) = self.timebase.raw_to_utc(raw) {
                        return Ok(TxStamp {
                            utc_ns,
                            source: self.timebase.mode.into(),
                        });
                    }
                    break;
                }
                Ok(None) if Instant::now() < deadline => {
                    std::thread::sleep(wait);
                    wait = (wait * 2).min(Duration::from_millis(2));
                }
                Ok(None) => {
                    if let Ok(mut ab) = self.abandoned.lock() {
                        ab.push(id);
                    }
                    break;
                }
                Err(e) => {
                    tracing::debug!("SIO_GET_TX_TIMESTAMP: {e}");
                    break;
                }
            }
        }
        self.stats
            .tx_ts_fallback
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        Ok(TxStamp {
            utc_ns: after,
            source: TsSource::Userspace,
        })
    }

    /// Ein Datagramm empfangen; `Ok(None)` nach etwa 200 ms ohne Daten.
    pub fn recv(&self, buf: &mut [u8]) -> io::Result<Option<RxPacket>> {
        #[cfg(windows)]
        if let Some(w) = &self.win {
            if !w.poll_readable(RECV_TIMEOUT.as_millis() as i32)? {
                return Ok(None);
            }
            let (len, src, raw) = w.recv(buf)?;
            let now = system_utc_ns();
            if w.rx_enabled {
                if let Some(utc_ns) = raw.and_then(|r| self.timebase.raw_to_utc(r)) {
                    return Ok(Some(RxPacket {
                        len,
                        src,
                        utc_ns,
                        source: self.timebase.mode.into(),
                    }));
                }
                self.stats
                    .rx_ts_fallback
                    .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            }
            return Ok(Some(RxPacket {
                len,
                src,
                utc_ns: now,
                source: TsSource::Userspace,
            }));
        }
        match self.udp.recv_from(buf) {
            Ok((len, std::net::SocketAddr::V4(src))) => Ok(Some(RxPacket {
                len,
                src,
                utc_ns: system_utc_ns(),
                source: TsSource::Userspace,
            })),
            Ok(_) => Ok(None),
            Err(e)
                if matches!(
                    e.kind(),
                    io::ErrorKind::WouldBlock | io::ErrorKind::TimedOut
                ) =>
            {
                Ok(None)
            }
            Err(e) => Err(e),
        }
    }
}

/// Ergebnis der Netzwerk-Einrichtung für Master und probe.
pub struct NetSetup {
    pub iface: InterfaceInfo,
    pub ip: Ipv4Addr,
    pub caps: std::result::Result<TsCaps, String>,
    pub mode: TimestampMode,
    pub mode_reason: String,
    pub timebase: Arc<TimeBase>,
    pub event: PtpSocket,
    pub general: PtpSocket,
}

impl NetSetup {
    pub fn mode_description(&self) -> String {
        match self.mode {
            TimestampMode::Hardware => format!(
                "Hardware (NIC-Uhr {}) – {}",
                self.timebase
                    .hw_clock_hz()
                    .map(|hz| format!("{:.3} MHz", hz / 1e6))
                    .unwrap_or_else(|| "?".into()),
                self.mode_reason
            ),
            TimestampMode::Software => format!("Software (Treiber, QPC) – {}", self.mode_reason),
            TimestampMode::Userspace => format!(
                "Userspace (direkt vor/nach send/recv) – {}",
                self.mode_reason
            ),
        }
    }
}

/// Aktive Timestamp-Fähigkeiten eines Interfaces.
pub fn active_caps(iface: &InterfaceInfo) -> std::result::Result<TsCaps, String> {
    #[cfg(windows)]
    {
        if iface.luid == 0 {
            return Err("Interface ohne LUID".into());
        }
        crate::win::tscaps::active(iface.luid)
    }
    #[cfg(not(windows))]
    {
        let _ = iface;
        Err("Winsock-Timestamping gibt es nur unter Windows".into())
    }
}

/// Interface wählen, Timestamp-Modus festlegen und beide Sockets öffnen.
pub fn setup(cfg: &Config) -> Result<NetSetup> {
    let list = iface::list()?;
    let (iface, ip) = iface::select(&list, cfg.interface.as_deref())?;
    let caps = active_caps(&iface);
    let (mut mode, mut reason) = choose_mode(cfg.timestamping, &caps)?;
    let forced = cfg.timestamping != TimestampRequest::Auto;

    let mut timebase = Arc::new(TimeBase::new(mode, iface.luid, caps.clone().ok()));
    if mode == TimestampMode::Hardware {
        // Zwei Cross-Timestamps für eine erste Steigung
        let first = timebase.refresh();
        std::thread::sleep(Duration::from_millis(100));
        let second = timebase.refresh();
        if let Err(e) = first.and(second) {
            if forced {
                bail!("Hardware-Timestamping: Cross-Timestamp fehlgeschlagen: {e}");
            }
            let sw = matches!(&caps, Ok(c) if c.software_usable());
            mode = if sw {
                TimestampMode::Software
            } else {
                TimestampMode::Userspace
            };
            reason = format!("Cross-Timestamp fehlgeschlagen ({e}), weiter mit {mode}");
            timebase = Arc::new(TimeBase::new(mode, iface.luid, caps.clone().ok()));
        }
    }

    let spec_event = SocketSpec {
        port: cfg.event_port,
        iface_ip: ip,
        group: cfg.multicast_group,
        ttl: cfg.ttl,
    };
    let spec_general = SocketSpec {
        port: cfg.general_port,
        ..spec_event.clone()
    };

    let event = match PtpSocket::open(&spec_event, true, timebase.clone()) {
        Ok(s) => s,
        Err(e) if mode != TimestampMode::Userspace && !forced => {
            reason = format!(
                "{mode}-Timestamping am Socket nicht aktivierbar ({e:#}), weiter mit Userspace"
            );
            mode = TimestampMode::Userspace;
            timebase = Arc::new(TimeBase::new(mode, iface.luid, None));
            PtpSocket::open(&spec_event, false, timebase.clone())?
        }
        Err(e) => return Err(e),
    };
    let general = PtpSocket::open(&spec_general, false, timebase.clone())?;

    Ok(NetSetup {
        iface,
        ip,
        caps,
        mode,
        mode_reason: reason,
        timebase,
        event,
        general,
    })
}

/// clockIdentity aus der MAC (EUI-64). Ohne MAC (Loopback) eine stabile Pseudo-MAC aus
/// Rechner- und Interfacename (lokal administriert). Rückgabe: (Identity, aus echter MAC?)
pub fn derive_clock_identity(iface: &InterfaceInfo) -> (ClockIdentity, bool) {
    if let Some(mac) = iface.mac {
        return (ClockIdentity::from_mac(mac), true);
    }
    let host = std::env::var("COMPUTERNAME")
        .or_else(|_| std::env::var("HOSTNAME"))
        .or_else(|_| std::fs::read_to_string("/etc/hostname").map(|s| s.trim().to_string()))
        .unwrap_or_default();
    let h = fnv1a(format!("{host}/{}", iface.name).as_bytes());
    let b = h.to_be_bytes();
    let mac = [(b[0] & 0xFC) | 0x02, b[1], b[2], b[3], b[4], b[5]];
    (ClockIdentity::from_mac(mac), false)
}

pub fn fnv1a(data: &[u8]) -> u64 {
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    for &b in data {
        h ^= b as u64;
        h = h.wrapping_mul(0x0100_0000_01b3);
    }
    h
}

#[cfg(test)]
mod tests {
    use super::*;

    fn caps(hw: bool, sw: bool, cross: bool) -> TsCaps {
        TsCaps {
            hw_rx: hw,
            hw_tx: hw,
            sw_rx: sw,
            sw_tx: sw,
            cross_timestamp: cross,
            hw_clock_hz: 0,
        }
    }

    #[test]
    fn mode_selection() {
        use TimestampRequest::*;
        let full = Ok(caps(true, true, true));
        let sw_only = Ok(caps(false, true, false));
        let hw_no_cross = Ok(caps(true, false, false));
        let none: std::result::Result<TsCaps, String> = Err("API fehlt".into());

        assert_eq!(choose_mode(Auto, &full).unwrap().0, TimestampMode::Hardware);
        assert_eq!(
            choose_mode(Auto, &sw_only).unwrap().0,
            TimestampMode::Software
        );
        assert_eq!(
            choose_mode(Auto, &hw_no_cross).unwrap().0,
            TimestampMode::Userspace
        );
        assert_eq!(
            choose_mode(Auto, &none).unwrap().0,
            TimestampMode::Userspace
        );
        assert!(choose_mode(Hardware, &sw_only).is_err());
        assert!(choose_mode(Software, &none).is_err());
        assert_eq!(
            choose_mode(Software, &full).unwrap().0,
            TimestampMode::Software
        );
        assert_eq!(
            choose_mode(Userspace, &full).unwrap().0,
            TimestampMode::Userspace
        );
    }

    #[test]
    fn pseudo_identity_is_stable_and_local() {
        let i = InterfaceInfo {
            name: "lo".into(),
            id: "lo".into(),
            description: String::new(),
            mac: None,
            ipv4: vec![Ipv4Addr::LOCALHOST],
            up: true,
            loopback: true,
            has_gateway: false,
            index: 1,
            luid: 0,
        };
        let (a, real) = derive_clock_identity(&i);
        let (b, _) = derive_clock_identity(&i);
        assert!(!real);
        assert_eq!(a, b);
        assert_eq!(a.0[0] & 0x03, 0x02); // lokal administriert, unicast
        assert_eq!(&a.0[3..5], &[0xff, 0xfe]);
    }
}
