//! Integrationstests: Master und probe lokal über Loopback-Multicast.
//!
//! Jeder Test nutzt eigene Ports und eine eigene Domain, damit die Tests parallel laufen
//! können. Unter Linux sind 319/320 privilegiert, deshalb hohe Ports.

use std::net::{Ipv4Addr, SocketAddrV4, UdpSocket};
use std::sync::Arc;
use std::sync::atomic::AtomicBool;
use std::time::{Duration, Instant};

use ptpmaster::config::{Config, Profile};
use ptpmaster::master::{MasterHandle, PortState};
use ptpmaster::probe::{self, ProbeOptions};
use ptpmaster::proto::{
    Body, ClockIdentity, Header, Management, Message, MessageType, PortIdentity,
};

fn cfg(domain: u8, event_port: u16) -> Config {
    let mut c = Config::from_profile(Profile::Aes67);
    c.interface = Some("127.0.0.1".into());
    c.domain = domain;
    c.event_port = event_port;
    c.general_port = event_port + 1;
    c.log_sync_interval = -4; // 16/s
    c.log_announce_interval = -3; // 8/s
    c.log_min_delay_req_interval = -3;
    c.log_file = false;
    c
}

fn run_probe(c: &Config, secs: u64) -> probe::ProbeReport {
    probe::run(
        c,
        &ProbeOptions {
            duration: Some(Duration::from_secs(secs)),
            delay_req_log_interval: Some(-3),
            print: false,
        },
        Arc::new(AtomicBool::new(false)),
    )
    .expect("probe")
}

fn wait_for(
    master: &MasterHandle,
    timeout: Duration,
    f: impl Fn(&ptpmaster::master::MasterStatus) -> bool,
) -> bool {
    let end = Instant::now() + timeout;
    while Instant::now() < end {
        if master.status().is_some_and(|s| f(&s)) {
            return true;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    false
}

#[test]
fn two_step_exchange_is_complete() {
    let c = cfg(11, 41319);
    let master = MasterHandle::spawn(c.clone()).expect("Master startet");
    let r = run_probe(&c, 4);

    probe::require_complete(&r).expect("vollständiger Ablauf");
    assert_eq!(r.two_step, Some(true));
    assert!(r.announces > 0, "{r:?}");
    assert!(r.syncs >= 20, "zu wenige Sync: {}", r.syncs);
    // Jeder Sync muss sein Follow_Up bekommen (höchstens die letzten im Flug fehlen)
    assert!(
        r.follow_ups + 2 >= r.syncs,
        "Sync {} Follow_Up {}",
        r.syncs,
        r.follow_ups
    );
    assert!(r.delay_reqs_sent >= 10, "{r:?}");
    assert!(
        r.delay_resps + 2 >= r.delay_reqs_sent,
        "Delay_Req {} Delay_Resp {}",
        r.delay_reqs_sent,
        r.delay_resps
    );
    assert!(r.complete_exchanges >= 5);

    // Gleiche Uhr auf beiden Seiten: Offset ≈ 0, Path Delay klein und positiv
    let (offset, delay) = r.stats().unwrap();
    assert!(offset.mean.abs() < 2_000_000.0, "Offset {offset:?}");
    assert!(
        delay.mean > -50_000.0 && delay.mean < 5_000_000.0,
        "Delay {delay:?}"
    );

    let s = master.status().unwrap();
    assert_eq!(s.state, PortState::Master);
    assert!(s.delay_req_clients >= 1, "{s:?}");
    assert!(s.recv_delay_req >= 10);
    assert_eq!(s.sent_sync, s.sent_follow_up);
    master.shutdown().unwrap();
}

#[test]
fn one_step_exchange_is_complete() {
    let mut c = cfg(12, 41419);
    c.two_step = false;
    let master = MasterHandle::spawn(c.clone()).expect("Master startet");
    let r = run_probe(&c, 3);
    probe::require_complete(&r).expect("vollständiger Ablauf");
    assert_eq!(r.two_step, Some(false));
    assert_eq!(r.follow_ups, 0);
    let s = master.status().unwrap();
    assert_eq!(s.sent_follow_up, 0);
    assert!(s.sent_sync > 0);
    master.shutdown().unwrap();
}

#[test]
fn bmca_goes_passive_and_returns_to_master() {
    let mut a = cfg(13, 41519);
    a.priority1 = 128;
    a.clock_identity = ClockIdentity::parse("02aaaa.fffe.000001");
    let mut b = a.clone();
    b.priority1 = 100; // besser
    b.clock_identity = ClockIdentity::parse("02bbbb.fffe.000002");

    let ma = MasterHandle::spawn(a).expect("Master A");
    assert!(
        wait_for(&ma, Duration::from_secs(3), |s| s.state
            == PortState::Master),
        "A wird Master"
    );
    let mb = MasterHandle::spawn(b).expect("Master B");
    assert!(
        wait_for(&ma, Duration::from_secs(3), |s| s.state
            == PortState::Passive),
        "A wechselt auf PASSIVE: {:?}",
        ma.status()
    );
    let sa = ma.status().unwrap();
    assert_eq!(sa.best_foreign.as_ref().map(|f| f.priority1), Some(100));
    assert!(
        wait_for(&mb, Duration::from_secs(3), |s| s.state
            == PortState::Master),
        "B ist Master"
    );
    // A sendet in PASSIVE nichts mehr
    let before = ma.status().unwrap().sent_sync;
    std::thread::sleep(Duration::from_millis(500));
    assert_eq!(ma.status().unwrap().sent_sync, before);

    // B verschwindet → A nach announceReceiptTimeout (3 × 125 ms) wieder Master
    mb.shutdown().unwrap();
    assert!(
        wait_for(&ma, Duration::from_secs(4), |s| s.state
            == PortState::Master),
        "A wird wieder Master: {:?}",
        ma.status()
    );
    ma.shutdown().unwrap();
}

#[test]
fn worse_foreign_master_is_ignored() {
    let mut a = cfg(14, 41619);
    a.clock_identity = ClockIdentity::parse("02cccc.fffe.000001");
    let mut b = a.clone();
    b.priority1 = 200; // schlechter
    b.clock_identity = ClockIdentity::parse("02dddd.fffe.000002");
    let ma = MasterHandle::spawn(a).unwrap();
    let mb = MasterHandle::spawn(b).unwrap();
    assert!(wait_for(&mb, Duration::from_secs(3), |s| s.state
        == PortState::Passive));
    assert!(wait_for(&ma, Duration::from_secs(3), |s| s.state
        == PortState::Master));
    std::thread::sleep(Duration::from_millis(500));
    assert_eq!(ma.status().unwrap().state, PortState::Master);
    mb.shutdown().unwrap();
    ma.shutdown().unwrap();
}

#[test]
fn management_and_garbage_do_not_disturb() {
    let c = cfg(15, 41719);
    let master = MasterHandle::spawn(c.clone()).unwrap();
    assert!(wait_for(&master, Duration::from_secs(3), |s| s.state
        == PortState::Master));

    let tx = UdpSocket::bind("127.0.0.1:0").unwrap();
    let general = SocketAddrV4::new(Ipv4Addr::LOCALHOST, c.general_port);
    let event = SocketAddrV4::new(Ipv4Addr::LOCALHOST, c.event_port);
    let src = PortIdentity {
        clock_identity: ClockIdentity([0x02, 1, 2, 3, 4, 5, 6, 7]),
        port_number: 1,
    };
    // Management GET (wie pmc), Signaling, Pdelay_Req, Müll, PTPv1, leeres Paket
    let mgmt = Message::new(
        Header::new(MessageType::Management, c.domain, src, 1, 0x7f),
        Body::Management(Management {
            target_port_identity: PortIdentity {
                clock_identity: ClockIdentity([0xff; 8]),
                port_number: 0xffff,
            },
            starting_boundary_hops: 0,
            boundary_hops: 0,
            action: 0,
            first_tlv: None,
            raw: vec![0xff; 10]
                .into_iter()
                .chain([0, 0, 0, 0, 0x00, 0x01, 0x00, 0x02, 0x20, 0x01])
                .collect(),
        }),
    )
    .to_bytes();
    for _ in 0..20 {
        tx.send_to(&mgmt, general).unwrap();
        tx.send_to(&[0xde, 0xad, 0xbe, 0xef], general).unwrap();
        tx.send_to(&[0u8; 0], event).unwrap();
        tx.send_to(&[0x00, 0x01, 0x00, 0x2c], event).unwrap();
        let mut pdelay = mgmt.clone();
        pdelay[0] = 0x02;
        tx.send_to(&pdelay, event).unwrap();
    }
    let before = master.status().unwrap().sent_sync;
    std::thread::sleep(Duration::from_millis(1500));
    let s = master.status().unwrap();
    assert_eq!(s.state, PortState::Master);
    assert!(s.sent_sync > before, "Master sendet weiter");
    // Der Master läuft und beantwortet weiterhin Delay_Req
    let r = run_probe(&c, 2);
    probe::require_complete(&r).unwrap();
    master.shutdown().unwrap();
}

#[test]
fn binary_run_and_probe() {
    let exe = env!("CARGO_BIN_EXE_ptpmaster");
    let args = [
        "--interface",
        "127.0.0.1",
        "--event-port",
        "41819",
        "--general-port",
        "41820",
        "--domain",
        "16",
        "--profile",
        "aes67",
    ];
    let mut master = std::process::Command::new(exe)
        .arg("run")
        .args(args)
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .expect("ptpmaster run");
    let out = std::process::Command::new(exe)
        .arg("probe")
        .args(args)
        .args(["--duration", "5", "--delay-req-interval", "-2"])
        .output()
        .expect("ptpmaster probe");
    let _ = master.kill();
    let mlog = master.wait_with_output().unwrap();
    let stdout = String::from_utf8_lossy(&out.stdout);
    let mstdout = String::from_utf8_lossy(&mlog.stdout);
    assert!(
        out.status.success(),
        "probe: {stdout}\n{}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(stdout.contains("Path Delay"), "{stdout}");
    assert!(stdout.contains("Zusammenfassung"), "{stdout}");
    assert!(mstdout.contains("Timestamp-Modus: Userspace"), "{mstdout}");
    assert!(mstdout.contains("MASTER"), "{mstdout}");
}
