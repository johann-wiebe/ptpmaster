//! Master-Port: Announce, Sync (+ Follow_Up), Delay_Resp; BMCA gegen fremde Master.

use std::collections::HashMap;
use std::net::SocketAddrV4;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use tracing::{debug, info, warn};

use crate::bmca::{self, Dataset, ForeignMasters, Recommendation};
use crate::clock::system_utc_ns;
use crate::config::Config;
use crate::net::{self, NetSetup, RxPacket, TimestampMode};
use crate::proto::{
    Announce, Body, ClockIdentity, ClockQuality, Header, Message, MessageType, PortIdentity, flags,
};
use crate::time::{
    PtpTimestamp, format_ns, log_interval_rate, log_interval_to_duration, utc_to_tai_ns,
};

/// Wie lange ein Delay_Req-Absender als aktiver Client zählt.
const CLIENT_WINDOW: Duration = Duration::from_secs(60);

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PortState {
    Listening,
    Master,
    Passive,
}

impl std::fmt::Display for PortState {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            PortState::Listening => "LISTENING",
            PortState::Master => "MASTER",
            PortState::Passive => "PASSIVE",
        })
    }
}

#[derive(Clone, Debug)]
pub struct ForeignSummary {
    pub port: PortIdentity,
    pub priority1: u8,
    pub clock_class: u8,
    pub priority2: u8,
    pub addr: Option<SocketAddrV4>,
}

/// Momentaufnahme für Statusanzeige und Tests.
#[derive(Clone, Debug)]
pub struct MasterStatus {
    pub state: PortState,
    pub mode: TimestampMode,
    pub clock_identity: ClockIdentity,
    pub interface: String,
    pub domain: u8,
    pub sent_announce: u64,
    pub sent_sync: u64,
    pub sent_follow_up: u64,
    pub sent_delay_resp: u64,
    pub recv_delay_req: u64,
    pub delay_req_clients: usize,
    pub foreign_masters: usize,
    pub best_foreign: Option<ForeignSummary>,
    pub tx_ts_fallback: u64,
    pub rx_ts_fallback: u64,
    pub uptime: Duration,
}

impl MasterStatus {
    pub fn line(&self) -> String {
        let up = self.uptime.as_secs();
        let mut s = format!(
            "{} | Domain {} | TS {} | Announce {} Sync {} Follow_Up {} Delay_Resp {} | Clients {} | Laufzeit {:02}:{:02}:{:02}",
            self.state,
            self.domain,
            self.mode,
            self.sent_announce,
            self.sent_sync,
            self.sent_follow_up,
            self.sent_delay_resp,
            self.delay_req_clients,
            up / 3600,
            (up / 60) % 60,
            up % 60
        );
        if let Some(f) = &self.best_foreign {
            s.push_str(&format!(
                " | Fremd: {} (prio1 {})",
                f.port.clock_identity, f.priority1
            ));
        }
        if self.tx_ts_fallback + self.rx_ts_fallback > 0 {
            s.push_str(&format!(
                " | TS-Ersatz TX {} RX {}",
                self.tx_ts_fallback, self.rx_ts_fallback
            ));
        }
        s
    }
}

#[derive(Default)]
struct Stats {
    announce: AtomicU64,
    sync: AtomicU64,
    follow_up: AtomicU64,
    delay_resp: AtomicU64,
    delay_req: AtomicU64,
    invalid: AtomicU64,
}

struct Inner {
    state: PortState,
    listen_until: Instant,
    foreign: ForeignMasters,
    clients: HashMap<PortIdentity, Instant>,
    announce_seq: u16,
    sync_seq: u16,
}

struct Shared {
    cfg: Config,
    port: PortIdentity,
    net: NetSetup,
    dest_event: SocketAddrV4,
    dest_general: SocketAddrV4,
    inner: Mutex<Inner>,
    stats: Stats,
    started: Instant,
}

/// Laufender Master (Hintergrund-Threads für den Empfang).
pub struct Master {
    shared: Arc<Shared>,
    stop: Arc<AtomicBool>,
    rx_threads: Vec<JoinHandle<()>>,
}

impl Master {
    /// Netzwerk einrichten, Startinformationen loggen und Empfangsthreads starten.
    pub fn start(cfg: Config, stop: Arc<AtomicBool>) -> Result<Self> {
        let net = net::setup(&cfg)?;
        let (clock_identity, from_mac) = match cfg.clock_identity {
            Some(id) => (id, true),
            None => net::derive_clock_identity(&net.iface),
        };
        let port = PortIdentity {
            clock_identity,
            port_number: 1,
        };

        info!(
            "Interface: {} ({}) {} MAC {}",
            net.iface.name,
            if net.iface.description.is_empty() {
                "-"
            } else {
                &net.iface.description
            },
            net.ip,
            net.iface.mac_string()
        );
        info!(
            "clockIdentity {clock_identity}{}",
            if from_mac {
                ""
            } else {
                " (keine MAC am Interface, Ersatzwert)"
            }
        );
        info!("Timestamp-Modus: {}", net.mode_description());
        if net.mode != TimestampMode::Hardware {
            info!(
                "Hinweis: ohne Hardware-Timestamping ist mit Jitter im Bereich von Mikrosekunden (Software) bis zu einigen zehn Mikrosekunden (Userspace) zu rechnen"
            );
        }
        info!(
            "Profil {} | Domain {} | priority1 {} priority2 {} | clockClass {} clockAccuracy 0x{:02X} | currentUtcOffset {}",
            cfg.profile,
            cfg.domain,
            cfg.priority1,
            cfg.priority2,
            cfg.clock_class,
            cfg.clock_accuracy,
            cfg.utc_offset
        );
        info!(
            "Sync {}/s ({}), Announce {}/s, logMinDelayReqInterval {}, announceReceiptTimeout {}, TTL {}",
            log_interval_rate(cfg.log_sync_interval),
            if cfg.two_step { "Two-Step" } else { "One-Step" },
            log_interval_rate(cfg.log_announce_interval),
            cfg.log_min_delay_req_interval,
            cfg.announce_receipt_timeout,
            cfg.ttl
        );
        for w in cfg.warnings() {
            warn!("{w}");
        }

        let listen = log_interval_to_duration(cfg.log_announce_interval)
            * cfg.announce_receipt_timeout as u32;
        let now = Instant::now();
        let shared = Arc::new(Shared {
            dest_event: SocketAddrV4::new(cfg.multicast_group, cfg.event_port),
            dest_general: SocketAddrV4::new(cfg.multicast_group, cfg.general_port),
            inner: Mutex::new(Inner {
                state: PortState::Listening,
                listen_until: now + listen,
                foreign: ForeignMasters::new(cfg.announce_receipt_timeout),
                clients: HashMap::new(),
                announce_seq: 0,
                sync_seq: 0,
            }),
            port,
            net,
            cfg,
            stats: Stats::default(),
            started: now,
        });
        info!(
            "LISTENING: warte {} ms auf fremde Announce-Nachrichten",
            listen.as_millis()
        );

        let mut rx_threads = Vec::new();
        for event in [true, false] {
            let sh = shared.clone();
            let st = stop.clone();
            rx_threads.push(
                std::thread::Builder::new()
                    .name(if event { "rx-event" } else { "rx-general" }.into())
                    .spawn(move || rx_loop(&sh, &st, event))
                    .context("Thread nicht startbar")?,
            );
        }
        Ok(Self {
            shared,
            stop,
            rx_threads,
        })
    }

    pub fn status(&self) -> MasterStatus {
        snapshot(&self.shared)
    }

    /// Hauptschleife bis `stop` gesetzt ist. `on_status` wird etwa einmal pro Sekunde aufgerufen.
    pub fn run(self, mut on_status: impl FnMut(&MasterStatus)) -> Result<()> {
        let sh = &self.shared;
        let cfg = &sh.cfg;
        let announce_iv = log_interval_to_duration(cfg.log_announce_interval);
        let sync_iv = log_interval_to_duration(cfg.log_sync_interval);
        let mut next_announce = Instant::now();
        let mut next_sync = Instant::now();
        let mut next_bmca = Instant::now();
        let mut next_status = Instant::now();
        let mut next_refresh = Instant::now() + Duration::from_secs(1);
        let mut last_refresh_error: Option<Instant> = None;
        let mut clock_check = (Instant::now(), system_utc_ns());

        while !self.stop.load(Ordering::SeqCst) {
            let now = Instant::now();
            if now >= next_bmca {
                if evaluate_bmca(sh, now) == Some(PortState::Master) {
                    next_announce = now;
                    next_sync = now;
                }
                next_bmca = now + Duration::from_millis(100);
            }
            let state = sh
                .inner
                .lock()
                .map(|i| i.state)
                .unwrap_or(PortState::Passive);
            if state == PortState::Master {
                if now >= next_announce {
                    send_announce(sh);
                    next_announce = advance(next_announce, announce_iv, now);
                }
                if now >= next_sync {
                    send_sync(sh);
                    next_sync = advance(next_sync, sync_iv, now);
                }
            }
            if now >= next_refresh {
                if let Err(e) = sh.net.timebase.refresh()
                    && last_refresh_error.is_none_or(|t| now - t > Duration::from_secs(60))
                {
                    warn!("Cross-Timestamp fehlgeschlagen: {e}");
                    last_refresh_error = Some(now);
                }
                next_refresh = now + Duration::from_secs(1);
            }
            if now >= next_status {
                // Sprünge der Systemuhr erkennen (z. B. w32time stellt hart)
                let utc = system_utc_ns();
                let expected = clock_check.1 + now.duration_since(clock_check.0).as_nanos() as i128;
                let jump = (utc - expected) as f64;
                if jump.abs() > 5_000_000.0 {
                    warn!(
                        "Systemuhr ist um {} gesprungen (w32time/NTP?) – Slaves sehen einen Zeitsprung",
                        format_ns(jump)
                    );
                }
                clock_check = (now, utc);
                on_status(&snapshot(sh));
                next_status = now + Duration::from_secs(1);
            }
            let next = [next_bmca, next_status, next_refresh]
                .into_iter()
                .chain((state == PortState::Master).then_some(next_announce))
                .chain((state == PortState::Master).then_some(next_sync))
                .min()
                .unwrap_or(now + Duration::from_millis(50));
            let wait = next
                .saturating_duration_since(Instant::now())
                .min(Duration::from_millis(50));
            if !wait.is_zero() {
                std::thread::sleep(wait);
            }
        }
        for t in self.rx_threads {
            let _ = t.join();
        }
        info!("Master beendet");
        Ok(())
    }
}

/// Nächster Termin; nach langer Verzögerung nicht aufholen, sondern neu ansetzen.
fn advance(prev: Instant, iv: Duration, now: Instant) -> Instant {
    let next = prev + iv;
    if next <= now { now + iv } else { next }
}

fn now_tai(sh: &Shared) -> i128 {
    utc_to_tai_ns(system_utc_ns(), sh.cfg.utc_offset)
}

fn send_announce(sh: &Shared) {
    let cfg = &sh.cfg;
    let seq = match sh.inner.lock() {
        Ok(mut i) => {
            let s = i.announce_seq;
            i.announce_seq = s.wrapping_add(1);
            s
        }
        Err(_) => return,
    };
    let mut h = Header::new(
        MessageType::Announce,
        cfg.domain,
        sh.port,
        seq,
        cfg.log_announce_interval,
    );
    h.flags = flags::PTP_TIMESCALE | flags::CURRENT_UTC_OFFSET_VALID;
    if cfg.time_traceable {
        h.flags |= flags::TIME_TRACEABLE;
    }
    if cfg.frequency_traceable {
        h.flags |= flags::FREQUENCY_TRACEABLE;
    }
    let a = Announce {
        origin_timestamp: PtpTimestamp::from_nanos(now_tai(sh)),
        current_utc_offset: cfg.utc_offset,
        grandmaster_priority1: cfg.priority1,
        grandmaster_clock_quality: ClockQuality {
            clock_class: cfg.clock_class,
            clock_accuracy: cfg.clock_accuracy,
            offset_scaled_log_variance: cfg.offset_scaled_log_variance,
        },
        grandmaster_priority2: cfg.priority2,
        grandmaster_identity: sh.port.clock_identity,
        steps_removed: 0,
        time_source: cfg.time_source.code(),
    };
    let bytes = Message::new(h, Body::Announce(a)).to_bytes();
    match sh.net.general.send(&bytes, sh.dest_general) {
        Ok(()) => {
            sh.stats.announce.fetch_add(1, Ordering::Relaxed);
        }
        Err(e) => warn!("Announce nicht gesendet: {e}"),
    }
}

fn send_sync(sh: &Shared) {
    let cfg = &sh.cfg;
    let seq = match sh.inner.lock() {
        Ok(mut i) => {
            let s = i.sync_seq;
            i.sync_seq = s.wrapping_add(1);
            s
        }
        Err(_) => return,
    };
    let mut h = Header::new(
        MessageType::Sync,
        cfg.domain,
        sh.port,
        seq,
        cfg.log_sync_interval,
    );
    if cfg.two_step {
        h.flags = flags::TWO_STEP;
    }
    // One-Step: Zeitstempel direkt vor dem Senden; Two-Step: Schätzwert, der genaue Wert
    // folgt im Follow_Up.
    let origin = PtpTimestamp::from_nanos(now_tai(sh));
    let bytes = Message::new(
        h,
        Body::Sync {
            origin_timestamp: origin,
        },
    )
    .to_bytes();
    let tx = match sh.net.event.send_event(&bytes, sh.dest_event) {
        Ok(tx) => tx,
        Err(e) => {
            warn!("Sync nicht gesendet: {e}");
            return;
        }
    };
    sh.stats.sync.fetch_add(1, Ordering::Relaxed);
    if !cfg.two_step {
        return;
    }
    let t1 = utc_to_tai_ns(tx.utc_ns, cfg.utc_offset);
    let fh = Header::new(
        MessageType::FollowUp,
        cfg.domain,
        sh.port,
        seq,
        cfg.log_sync_interval,
    );
    let fu = Message::new(
        fh,
        Body::FollowUp {
            precise_origin_timestamp: PtpTimestamp::from_nanos(t1),
        },
    )
    .to_bytes();
    match sh.net.general.send(&fu, sh.dest_general) {
        Ok(()) => {
            sh.stats.follow_up.fetch_add(1, Ordering::Relaxed);
        }
        Err(e) => warn!("Follow_Up nicht gesendet: {e}"),
    }
}

fn rx_loop(sh: &Shared, stop: &AtomicBool, event: bool) {
    let sock = if event {
        &sh.net.event
    } else {
        &sh.net.general
    };
    let mut buf = [0u8; 1500];
    let mut last_error: Option<Instant> = None;
    while !stop.load(Ordering::SeqCst) {
        match sock.recv(&mut buf) {
            Ok(Some(pkt)) => handle_packet(sh, &buf[..pkt.len], pkt),
            Ok(None) => {}
            Err(e) => {
                if last_error.is_none_or(|t| t.elapsed() > Duration::from_secs(30)) {
                    warn!(
                        "Empfangsfehler auf Port {}: {e}",
                        if event {
                            sh.cfg.event_port
                        } else {
                            sh.cfg.general_port
                        }
                    );
                    last_error = Some(Instant::now());
                }
                std::thread::sleep(Duration::from_millis(10));
            }
        }
    }
}

fn handle_packet(sh: &Shared, data: &[u8], pkt: RxPacket) {
    let msg = match Message::parse(data) {
        Ok(m) => m,
        Err(e) => {
            sh.stats.invalid.fetch_add(1, Ordering::Relaxed);
            debug!("Ungültiges Paket von {}: {e}", pkt.src);
            return;
        }
    };
    let h = &msg.header;
    if h.source_port_identity.clock_identity == sh.port.clock_identity {
        return; // eigene Nachricht (Multicast-Loopback)
    }
    if h.domain_number != sh.cfg.domain {
        debug!(
            "{:?} aus Domain {} ignoriert",
            h.message_type, h.domain_number
        );
        return;
    }
    match &msg.body {
        Body::DelayReq { .. } => handle_delay_req(sh, &msg, pkt),
        Body::Announce(a) => {
            if a.steps_removed >= 255 {
                return;
            }
            if let Ok(mut i) = sh.inner.lock() {
                let new = i
                    .foreign
                    .record(h, a, sh.port, Some(pkt.src), Instant::now());
                if new {
                    info!(
                        "Fremder Master gesehen: {} von {} (GM {}, priority1 {}, clockClass {}, priority2 {}, stepsRemoved {})",
                        h.source_port_identity,
                        pkt.src.ip(),
                        a.grandmaster_identity,
                        a.grandmaster_priority1,
                        a.grandmaster_clock_quality.clock_class,
                        a.grandmaster_priority2,
                        a.steps_removed
                    );
                }
            }
        }
        Body::Management(m) => debug!(
            "Management-Nachricht von {} ignoriert (action {}, TLV {:?})",
            h.source_port_identity, m.action, m.first_tlv
        ),
        Body::Signaling { .. } => debug!("Signaling von {} ignoriert", h.source_port_identity),
        Body::Sync { .. } | Body::FollowUp { .. } => {
            debug!(
                "{:?} von fremdem Master {}",
                h.message_type, h.source_port_identity
            )
        }
        Body::DelayResp { .. } | Body::Other { .. } => {}
    }
}

fn handle_delay_req(sh: &Shared, req: &Message, pkt: RxPacket) {
    sh.stats.delay_req.fetch_add(1, Ordering::Relaxed);
    let requester = req.header.source_port_identity;
    {
        let Ok(mut i) = sh.inner.lock() else { return };
        if i.state != PortState::Master {
            return;
        }
        i.clients.insert(requester, Instant::now());
    }
    let cfg = &sh.cfg;
    let t4 = utc_to_tai_ns(pkt.utc_ns, cfg.utc_offset);
    let mut h = Header::new(
        MessageType::DelayResp,
        cfg.domain,
        sh.port,
        req.header.sequence_id,
        cfg.log_min_delay_req_interval,
    );
    // 11.3.2: correctionField des Delay_Req übernehmen (t4 hat keine Sub-ns-Anteile)
    h.correction = req.header.correction;
    // Hybrid-Modus: unicast gestellte Anfrage (unicastFlag) unicast beantworten
    let unicast = req.header.has_flag(flags::UNICAST);
    if unicast {
        h.flags |= flags::UNICAST;
    }
    let bytes = Message::new(
        h,
        Body::DelayResp {
            receive_timestamp: PtpTimestamp::from_nanos(t4),
            requesting_port_identity: requester,
        },
    )
    .to_bytes();
    let dest = if unicast {
        SocketAddrV4::new(*pkt.src.ip(), cfg.general_port)
    } else {
        sh.dest_general
    };
    match sh.net.general.send(&bytes, dest) {
        Ok(()) => {
            sh.stats.delay_resp.fetch_add(1, Ordering::Relaxed);
        }
        Err(e) => warn!("Delay_Resp an {dest} nicht gesendet: {e}"),
    }
}

/// BMCA auswerten; liefert den neuen Zustand, falls er sich geändert hat.
fn evaluate_bmca(sh: &Shared, now: Instant) -> Option<PortState> {
    let cfg = &sh.cfg;
    let mut i = sh.inner.lock().ok()?;
    for gone in i.foreign.prune(now) {
        info!(
            "Fremder Master {} verschwunden (kein Announce seit {:.1} s)",
            gone.dataset.sender,
            now.duration_since(gone.last_seen).as_secs_f64()
        );
    }
    let d0 = Dataset::local(
        sh.port,
        cfg.priority1,
        ClockQuality {
            clock_class: cfg.clock_class,
            clock_accuracy: cfg.clock_accuracy,
            offset_scaled_log_variance: cfg.offset_scaled_log_variance,
        },
        cfg.priority2,
    );
    let best = i.foreign.best(now).cloned();
    let rec = bmca::decide(&d0, best.as_ref().map(|b| &b.dataset));
    let old = i.state;
    let new = match rec {
        Recommendation::Passive => PortState::Passive,
        Recommendation::Master if old == PortState::Listening && now < i.listen_until => {
            PortState::Listening
        }
        Recommendation::Master => PortState::Master,
    };
    if new == old {
        return None;
    }
    i.state = new;
    drop(i);
    match (old, new) {
        (_, PortState::Passive) => {
            let b = best.as_ref().map(|b| &b.dataset);
            warn!(
                "PASSIVE: besserer Master in Domain {}: {} (GM {}, priority1 {}, clockClass {}, priority2 {}) – ptpmaster sendet nicht mehr",
                cfg.domain,
                b.map(|d| d.sender.to_string()).unwrap_or_default(),
                b.map(|d| d.gm_identity.to_string()).unwrap_or_default(),
                b.map(|d| d.gm_priority1).unwrap_or_default(),
                b.map(|d| d.gm_quality.clock_class).unwrap_or_default(),
                b.map(|d| d.gm_priority2).unwrap_or_default(),
            );
            if cfg.priority1 > 0 {
                warn!(
                    "Hinweis: Um Master zu werden, priority1 kleiner als {} setzen (aktuell {})",
                    b.map(|d| d.gm_priority1).unwrap_or_default(),
                    cfg.priority1
                );
            }
        }
        (PortState::Passive, PortState::Master) => {
            warn!("MASTER: kein besserer Master mehr im Netz – ptpmaster sendet wieder")
        }
        (_, PortState::Master) => {
            info!("MASTER: kein besserer Master gefunden, sende Announce und Sync")
        }
        (_, PortState::Listening) => {}
    }
    Some(new)
}

fn snapshot(sh: &Shared) -> MasterStatus {
    let now = Instant::now();
    let (state, clients, foreign, best) = match sh.inner.lock() {
        Ok(mut i) => {
            i.clients
                .retain(|_, t| now.duration_since(*t) < CLIENT_WINDOW);
            let best = i.foreign.best(now).map(|f| ForeignSummary {
                port: f.dataset.sender,
                priority1: f.dataset.gm_priority1,
                clock_class: f.dataset.gm_quality.clock_class,
                priority2: f.dataset.gm_priority2,
                addr: f.addr,
            });
            (i.state, i.clients.len(), i.foreign.len(), best)
        }
        Err(_) => (PortState::Passive, 0, 0, None),
    };
    let ev = &sh.net.event.stats;
    MasterStatus {
        state,
        mode: sh.net.mode,
        clock_identity: sh.port.clock_identity,
        interface: sh.net.iface.name.clone(),
        domain: sh.cfg.domain,
        sent_announce: sh.stats.announce.load(Ordering::Relaxed),
        sent_sync: sh.stats.sync.load(Ordering::Relaxed),
        sent_follow_up: sh.stats.follow_up.load(Ordering::Relaxed),
        sent_delay_resp: sh.stats.delay_resp.load(Ordering::Relaxed),
        recv_delay_req: sh.stats.delay_req.load(Ordering::Relaxed),
        delay_req_clients: clients,
        foreign_masters: foreign,
        best_foreign: best,
        tx_ts_fallback: ev.tx_ts_fallback.load(Ordering::Relaxed),
        rx_ts_fallback: ev.rx_ts_fallback.load(Ordering::Relaxed),
        uptime: now.duration_since(sh.started),
    }
}

/// Master in einem eigenen Thread (für Tests und den Dienst).
pub struct MasterHandle {
    pub stop: Arc<AtomicBool>,
    status: Arc<Mutex<Option<MasterStatus>>>,
    thread: Option<JoinHandle<Result<()>>>,
}

impl MasterHandle {
    pub fn spawn(cfg: Config) -> Result<Self> {
        let stop = Arc::new(AtomicBool::new(false));
        let master = Master::start(cfg, stop.clone())?;
        let status = Arc::new(Mutex::new(Some(master.status())));
        let st = status.clone();
        let thread = std::thread::Builder::new()
            .name("master".into())
            .spawn(move || {
                master.run(|s| {
                    if let Ok(mut g) = st.lock() {
                        *g = Some(s.clone());
                    }
                })
            })?;
        Ok(Self {
            stop,
            status,
            thread: Some(thread),
        })
    }

    /// Letzter Status (wird etwa einmal pro Sekunde aktualisiert).
    pub fn status(&self) -> Option<MasterStatus> {
        self.status.lock().ok()?.clone()
    }

    pub fn shutdown(mut self) -> Result<()> {
        self.stop.store(true, Ordering::SeqCst);
        match self.thread.take() {
            Some(t) => t
                .join()
                .map_err(|_| anyhow::anyhow!("Master-Thread abgestürzt"))?,
            None => Ok(()),
        }
    }
}

impl Drop for MasterHandle {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
    }
}
