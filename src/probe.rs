//! `ptpmaster probe`: minimaler Slave-Messmodus (stellt die Uhr NICHT nach).
//!
//! Misst wie ein E2E-Slave:
//!   t1 = preciseOriginTimestamp (Follow_Up) bzw. originTimestamp (One-Step) + Korrekturen
//!   t2 = Empfangszeit des Sync
//!   t3 = Sendezeit des Delay_Req
//!   t4 = receiveTimestamp aus dem Delay_Resp − correctionField
//!   meanPathDelay = ((t2 − t1) + (t4 − t3)) / 2
//!   offsetFromMaster = (t2 − t1) − meanPathDelay

use std::collections::HashMap;
use std::net::SocketAddrV4;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use anyhow::{Result, bail};

use crate::bmca::ForeignMasters;
use crate::clock::system_utc_ns;
use crate::config::Config;
use crate::net::{self, NetSetup, RxPacket};
use crate::proto::{
    Announce, Body, ClockIdentity, Header, LOG_INTERVAL_UNSPECIFIED, Message, MessageType,
    PortIdentity, flags,
};
use crate::time::{PtpTimestamp, correction_to_nanos, format_ns, log_interval_to_duration};

#[derive(Clone, Debug, Default)]
pub struct ProbeOptions {
    pub duration: Option<Duration>,
    pub delay_req_log_interval: Option<i8>,
    /// Jede Sekunde eine Zeile ausgeben
    pub print: bool,
}

#[derive(Clone, Copy, Debug)]
pub struct Sample {
    pub offset_ns: f64,
    pub path_delay_ns: f64,
}

#[derive(Clone, Debug, Default)]
pub struct ProbeReport {
    pub master: Option<PortIdentity>,
    pub master_addr: Option<SocketAddrV4>,
    pub announce: Option<Announce>,
    pub announces: u64,
    pub syncs: u64,
    pub follow_ups: u64,
    pub delay_reqs_sent: u64,
    pub delay_resps: u64,
    /// Vollständige Abläufe Sync → Follow_Up → Delay_Req → Delay_Resp
    pub complete_exchanges: u64,
    pub samples: Vec<Sample>,
    pub two_step: Option<bool>,
}

impl ProbeReport {
    pub fn stats(&self) -> Option<(Stats, Stats)> {
        if self.samples.is_empty() {
            return None;
        }
        Some((
            Stats::of(self.samples.iter().map(|s| s.offset_ns)),
            Stats::of(self.samples.iter().map(|s| s.path_delay_ns)),
        ))
    }
}

#[derive(Clone, Copy, Debug)]
pub struct Stats {
    pub min: f64,
    pub max: f64,
    pub mean: f64,
    pub stddev: f64,
}

impl Stats {
    fn of(v: impl Iterator<Item = f64> + Clone) -> Self {
        let n = v.clone().count().max(1) as f64;
        let mean = v.clone().sum::<f64>() / n;
        let var = v.clone().map(|x| (x - mean).powi(2)).sum::<f64>() / n;
        Self {
            min: v.clone().fold(f64::INFINITY, f64::min),
            max: v.fold(f64::NEG_INFINITY, f64::max),
            mean,
            stddev: var.sqrt(),
        }
    }
}

struct PendingSync {
    t2: i128,
    correction_ns: f64,
    at: Instant,
}

struct State {
    own: PortIdentity,
    foreign: ForeignMasters,
    master: Option<PortIdentity>,
    utc_offset: i16,
    pending_sync: HashMap<u16, PendingSync>,
    /// Follow_Up, das vor seinem Sync ankam (verschiedene Sockets/Threads): seq → (t1 ohne Sync-Korrektur, Zeitpunkt)
    early_follow_up: HashMap<u16, (f64, Instant)>,
    /// (t1 inkl. Korrekturen, t2)
    last_sync: Option<(f64, f64)>,
    pending_delay: HashMap<u16, (f64, Instant)>,
    path_delay: Option<f64>,
    master_delay_log_interval: Option<i8>,
    report: ProbeReport,
    second: SecondCounters,
}

#[derive(Default, Clone, Copy)]
struct SecondCounters {
    syncs: u64,
    follow_ups: u64,
    delay_resps: u64,
    last_offset: Option<f64>,
}

/// probe starten und bis `stop`, Ablauf von `duration` oder Fehler laufen lassen.
pub fn run(cfg: &Config, opts: &ProbeOptions, stop: Arc<AtomicBool>) -> Result<ProbeReport> {
    let net = net::setup(cfg)?;
    let own = PortIdentity {
        clock_identity: probe_identity(&net),
        port_number: 1,
    };
    if opts.print {
        println!(
            "probe auf {} ({}), Domain {}, Timestamp-Modus: {}",
            net.iface.name,
            net.ip,
            cfg.domain,
            net.mode_description()
        );
        println!(
            "Eigene clockIdentity {} – warte auf Announce …",
            own.clock_identity
        );
    }
    let net = Arc::new(net);
    let state = Arc::new(Mutex::new(State {
        own,
        foreign: ForeignMasters::new(cfg.announce_receipt_timeout),
        master: None,
        utc_offset: cfg.utc_offset,
        pending_sync: HashMap::new(),
        early_follow_up: HashMap::new(),
        last_sync: None,
        pending_delay: HashMap::new(),
        path_delay: None,
        master_delay_log_interval: None,
        report: ProbeReport::default(),
        second: SecondCounters::default(),
    }));

    let local_stop = Arc::new(AtomicBool::new(false));
    let mut threads = Vec::new();
    for event in [true, false] {
        let (net, state, st, cfg) = (net.clone(), state.clone(), local_stop.clone(), cfg.clone());
        let print = opts.print;
        threads.push(std::thread::spawn(move || {
            let sock = if event { &net.event } else { &net.general };
            let mut buf = [0u8; 1500];
            while !st.load(Ordering::SeqCst) {
                match sock.recv(&mut buf) {
                    Ok(Some(pkt)) => handle(&cfg, print, &state, &buf[..pkt.len], pkt),
                    Ok(None) => {}
                    Err(_) => std::thread::sleep(Duration::from_millis(10)),
                }
            }
        }));
    }

    let started = Instant::now();
    let mut next_delay_req = Instant::now();
    let mut next_print = Instant::now() + Duration::from_secs(1);
    let mut delay_seq: u16 = 0;
    let dest_event = SocketAddrV4::new(cfg.multicast_group, cfg.event_port);
    let result: Result<()> = loop {
        if stop.load(Ordering::SeqCst) {
            break Ok(());
        }
        if opts.duration.is_some_and(|d| started.elapsed() >= d) {
            break Ok(());
        }
        let now = Instant::now();
        if now >= next_delay_req {
            let (ready, log_iv, utc_offset, domain) = {
                let s = state
                    .lock()
                    .map_err(|_| anyhow::anyhow!("Lock vergiftet"))?;
                (
                    s.master.is_some() && s.last_sync.is_some(),
                    opts.delay_req_log_interval
                        .or(s.master_delay_log_interval)
                        .unwrap_or(cfg.log_min_delay_req_interval),
                    s.utc_offset,
                    cfg.domain,
                )
            };
            if ready {
                let seq = delay_seq;
                delay_seq = delay_seq.wrapping_add(1);
                let h = Header::new(
                    MessageType::DelayReq,
                    domain,
                    own,
                    seq,
                    LOG_INTERVAL_UNSPECIFIED,
                );
                let origin = PtpTimestamp::from_nanos(crate::time::utc_to_tai_ns(
                    system_utc_ns(),
                    utc_offset,
                ));
                let bytes = Message::new(
                    h,
                    Body::DelayReq {
                        origin_timestamp: origin,
                    },
                )
                .to_bytes();
                match net.event.send_event(&bytes, dest_event) {
                    Ok(tx) => {
                        let t3 = crate::time::utc_to_tai_ns(tx.utc_ns, utc_offset) as f64;
                        if let Ok(mut s) = state.lock() {
                            s.pending_delay.insert(seq, (t3, Instant::now()));
                            s.pending_delay
                                .retain(|_, (_, at)| at.elapsed() < Duration::from_secs(10));
                            s.report.delay_reqs_sent += 1;
                        }
                    }
                    Err(e) => eprintln!("Delay_Req nicht gesendet: {e}"),
                }
            }
            let iv = log_interval_to_duration(if (-7..=7).contains(&log_iv) {
                log_iv
            } else {
                0
            });
            next_delay_req = now
                + if ready {
                    iv
                } else {
                    Duration::from_millis(100)
                };
        }
        if now >= next_print {
            next_print = now + Duration::from_secs(1);
            if opts.print {
                print_second(&state);
            }
        }
        std::thread::sleep(
            next_delay_req
                .min(next_print)
                .saturating_duration_since(Instant::now())
                .min(Duration::from_millis(50)),
        );
    };
    local_stop.store(true, Ordering::SeqCst);
    for t in threads {
        let _ = t.join();
    }
    result?;
    let report = state
        .lock()
        .map(|s| s.report.clone())
        .map_err(|_| anyhow::anyhow!("Lock vergiftet"))?;
    if opts.print {
        print_summary(&report);
    }
    Ok(report)
}

fn probe_identity(net: &NetSetup) -> ClockIdentity {
    // Eigene, zufällige Identität, damit probe auch auf dem Master-PC selbst läuft.
    let seed = format!(
        "{}-{}-{:?}",
        std::process::id(),
        system_utc_ns(),
        net.iface.mac
    );
    let b = net::fnv1a(seed.as_bytes()).to_be_bytes();
    ClockIdentity([
        (b[0] & 0xFC) | 0x02,
        b[1],
        b[2],
        b[3],
        b[4],
        b[5],
        b[6],
        b[7],
    ])
}

fn handle(cfg: &Config, print: bool, state: &Mutex<State>, data: &[u8], pkt: RxPacket) {
    let Ok(msg) = Message::parse(data) else {
        return;
    };
    let h = msg.header;
    if h.domain_number != cfg.domain {
        return;
    }
    let Ok(mut s) = state.lock() else { return };
    if h.source_port_identity == s.own {
        return;
    }
    let now = Instant::now();
    match &msg.body {
        Body::Announce(a) => {
            if a.steps_removed >= 255 {
                return;
            }
            let own = s.own;
            s.foreign.record(&h, a, own, Some(pkt.src), now);
            s.foreign.prune(now);
            let best = s
                .foreign
                .best(now)
                .map(|f| (f.dataset.sender, f.addr, f.announce, f.flags));
            if let Some((sender, addr, ann, fl)) = best {
                if s.master != Some(sender) && print {
                    println!(
                        "Master: {sender} @ {} | GM {} priority1 {} clockClass {} clockAccuracy 0x{:02X} priority2 {} | currentUtcOffset {}{} | timeSource 0x{:02X}",
                        addr.map(|a| a.ip().to_string()).unwrap_or_default(),
                        ann.grandmaster_identity,
                        ann.grandmaster_priority1,
                        ann.grandmaster_clock_quality.clock_class,
                        ann.grandmaster_clock_quality.clock_accuracy,
                        ann.grandmaster_priority2,
                        ann.current_utc_offset,
                        if fl & flags::PTP_TIMESCALE != 0 {
                            ", PTP-Timescale"
                        } else {
                            ", ARB-Timescale"
                        },
                        ann.time_source
                    );
                }
                if s.master != Some(sender) {
                    s.master = Some(sender);
                    s.pending_sync.clear();
                    s.last_sync = None;
                    s.path_delay = None;
                }
                if fl & flags::CURRENT_UTC_OFFSET_VALID != 0 {
                    s.utc_offset = ann.current_utc_offset;
                }
                s.report.master = Some(sender);
                s.report.master_addr = addr;
                s.report.announce = Some(ann);
            }
            if s.master == Some(h.source_port_identity) {
                s.report.announces += 1;
            }
        }
        Body::Sync { origin_timestamp } => {
            if s.master != Some(h.source_port_identity) {
                return;
            }
            let t2 = crate::time::utc_to_tai_ns(pkt.utc_ns, s.utc_offset);
            s.report.syncs += 1;
            s.second.syncs += 1;
            let two_step = h.has_flag(flags::TWO_STEP);
            s.report.two_step = Some(two_step);
            if two_step {
                let corr = correction_to_nanos(h.correction);
                if let Some((t1_fu, _)) = s.early_follow_up.remove(&h.sequence_id) {
                    s.report.follow_ups += 1;
                    s.second.follow_ups += 1;
                    complete_sync(&mut s, t1_fu + corr, t2 as f64);
                } else {
                    s.pending_sync.insert(
                        h.sequence_id,
                        PendingSync {
                            t2,
                            correction_ns: corr,
                            at: now,
                        },
                    );
                    s.pending_sync
                        .retain(|_, p| now.duration_since(p.at) < Duration::from_secs(5));
                }
            } else {
                let t1 = origin_timestamp.to_nanos() as f64 + correction_to_nanos(h.correction);
                complete_sync(&mut s, t1, t2 as f64);
            }
        }
        Body::FollowUp {
            precise_origin_timestamp,
        } => {
            if s.master != Some(h.source_port_identity) {
                return;
            }
            let t1_fu =
                precise_origin_timestamp.to_nanos() as f64 + correction_to_nanos(h.correction);
            let Some(p) = s.pending_sync.remove(&h.sequence_id) else {
                s.early_follow_up.insert(h.sequence_id, (t1_fu, now));
                s.early_follow_up
                    .retain(|_, (_, at)| now.duration_since(*at) < Duration::from_secs(5));
                return;
            };
            s.report.follow_ups += 1;
            s.second.follow_ups += 1;
            complete_sync(&mut s, t1_fu + p.correction_ns, p.t2 as f64);
        }
        Body::DelayResp {
            receive_timestamp,
            requesting_port_identity,
        } => {
            if s.master != Some(h.source_port_identity) || *requesting_port_identity != s.own {
                return;
            }
            let Some((t3, _)) = s.pending_delay.remove(&h.sequence_id) else {
                return;
            };
            s.report.delay_resps += 1;
            s.second.delay_resps += 1;
            s.master_delay_log_interval = Some(h.log_message_interval);
            let t4 = receive_timestamp.to_nanos() as f64 - correction_to_nanos(h.correction);
            if let Some((t1, t2)) = s.last_sync {
                let delay = ((t2 - t1) + (t4 - t3)) / 2.0;
                s.path_delay = Some(delay);
                s.report.complete_exchanges += 1;
            }
        }
        _ => {}
    }
}

fn complete_sync(s: &mut State, t1: f64, t2: f64) {
    s.last_sync = Some((t1, t2));
    if let Some(delay) = s.path_delay {
        let offset = (t2 - t1) - delay;
        s.report.samples.push(Sample {
            offset_ns: offset,
            path_delay_ns: delay,
        });
        if s.report.samples.len() > 100_000 {
            s.report.samples.drain(..50_000);
        }
        s.second.last_offset = Some(offset);
    }
}

fn print_second(state: &Mutex<State>) {
    let Ok(mut s) = state.lock() else { return };
    let c = std::mem::take(&mut s.second);
    let Some(master) = s.master else {
        println!("kein Master (noch kein qualifiziertes Announce in Domain)");
        return;
    };
    match (c.last_offset, s.path_delay) {
        (Some(o), Some(d)) => println!(
            "{} | Offset {:>12} | Path Delay {:>12} | Sync {}/s Follow_Up {}/s Delay_Resp {}/s",
            master.clock_identity,
            format_ns(o),
            format_ns(d),
            c.syncs,
            c.follow_ups,
            c.delay_resps
        ),
        _ => println!(
            "{} | warte auf vollständigen Ablauf … Sync {}/s Follow_Up {}/s Delay_Resp {}/s",
            master.clock_identity, c.syncs, c.follow_ups, c.delay_resps
        ),
    }
}

fn print_summary(r: &ProbeReport) {
    println!();
    println!(
        "Zusammenfassung: Announce {} | Sync {} | Follow_Up {} | Delay_Req {} | Delay_Resp {} | vollständige Abläufe {}",
        r.announces, r.syncs, r.follow_ups, r.delay_reqs_sent, r.delay_resps, r.complete_exchanges
    );
    if let Some((o, d)) = r.stats() {
        println!(
            "Offset:     Mittel {} | Std.-Abw. {} | Min {} | Max {}",
            format_ns(o.mean),
            format_ns(o.stddev).trim_start_matches('+'),
            format_ns(o.min),
            format_ns(o.max)
        );
        println!(
            "Path Delay: Mittel {} | Std.-Abw. {} | Min {} | Max {}",
            format_ns(d.mean),
            format_ns(d.stddev).trim_start_matches('+'),
            format_ns(d.min),
            format_ns(d.max)
        );
        println!(
            "Hinweis: Der Offset enthält die Abweichung zwischen den Systemuhren beider PCs (NTP: typ. ±1–10 ms)."
        );
    }
}

/// Für Tests: prüfen, ob ein Bericht einen vollständigen Ablauf enthält.
pub fn require_complete(r: &ProbeReport) -> Result<()> {
    if r.syncs == 0 {
        bail!("keine Sync-Nachrichten empfangen");
    }
    if r.two_step == Some(true) && r.follow_ups == 0 {
        bail!("keine Follow_Up-Nachrichten empfangen");
    }
    if r.delay_resps == 0 {
        bail!("keine Delay_Resp-Nachrichten empfangen");
    }
    if r.samples.is_empty() {
        bail!("keine Offset-Messung zustande gekommen");
    }
    Ok(())
}
