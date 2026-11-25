//! Best Master Clock Algorithm (IEEE 1588-2008, 9.3) für einen reinen Master-Port.
//!
//! Wir vergleichen den eigenen Datensatz D0 mit dem besten qualifizierten fremden Master
//! (Erbest). Ist der fremde besser, wechselt der Port auf PASSIVE (statt SLAVE, weil
//! ptpmaster keinen Slave-Modus hat), sonst bleibt bzw. wird er MASTER.

use std::cmp::Ordering;
use std::collections::{HashMap, VecDeque};
use std::net::SocketAddrV4;
use std::time::{Duration, Instant};

use crate::proto::{Announce, ClockIdentity, ClockQuality, Header, PortIdentity};
use crate::time::log_interval_to_duration;

/// FOREIGN_MASTER_TIME_WINDOW (9.3.2.4.4): 4 Announce-Intervalle
pub const FOREIGN_MASTER_TIME_WINDOW: u32 = 4;
/// FOREIGN_MASTER_THRESHOLD (9.3.2.4.4): 2 Announce innerhalb des Fensters
pub const FOREIGN_MASTER_THRESHOLD: usize = 2;

/// Vergleichsdatensatz (9.3.4, Tabelle 12).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Dataset {
    pub gm_priority1: u8,
    pub gm_identity: ClockIdentity,
    pub gm_quality: ClockQuality,
    pub gm_priority2: u8,
    pub steps_removed: u16,
    /// Absender des Announce (bei D0: der eigene Port)
    pub sender: PortIdentity,
    /// Empfangender Port (der eigene Port)
    pub receiver: PortIdentity,
}

impl Dataset {
    /// D0: der eigene Datensatz als Grandmaster.
    pub fn local(own: PortIdentity, priority1: u8, quality: ClockQuality, priority2: u8) -> Self {
        Self {
            gm_priority1: priority1,
            gm_identity: own.clock_identity,
            gm_quality: quality,
            gm_priority2: priority2,
            steps_removed: 0,
            sender: own,
            receiver: own,
        }
    }

    pub fn from_announce(header: &Header, a: &Announce, receiver: PortIdentity) -> Self {
        Self {
            gm_priority1: a.grandmaster_priority1,
            gm_identity: a.grandmaster_identity,
            gm_quality: a.grandmaster_clock_quality,
            gm_priority2: a.grandmaster_priority2,
            steps_removed: a.steps_removed,
            sender: header.source_port_identity,
            receiver,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Comparison {
    ABetter,
    ABetterByTopology,
    BBetter,
    BBetterByTopology,
    /// Error-1 (Absender = Empfänger, eigene Nachricht)
    Error1,
    /// Error-2 (identische Datensätze)
    Error2,
}

impl Comparison {
    pub fn a_wins(self) -> bool {
        matches!(self, Comparison::ABetter | Comparison::ABetterByTopology)
    }
    pub fn b_wins(self) -> bool {
        matches!(self, Comparison::BBetter | Comparison::BBetterByTopology)
    }
}

/// Datensatzvergleich nach Figure 27 und Figure 28.
pub fn compare(a: &Dataset, b: &Dataset) -> Comparison {
    if a.gm_identity != b.gm_identity {
        // Figure 27: verschiedene Grandmaster
        let ord = a
            .gm_priority1
            .cmp(&b.gm_priority1)
            .then(a.gm_quality.clock_class.cmp(&b.gm_quality.clock_class))
            .then(
                a.gm_quality
                    .clock_accuracy
                    .cmp(&b.gm_quality.clock_accuracy),
            )
            .then(
                a.gm_quality
                    .offset_scaled_log_variance
                    .cmp(&b.gm_quality.offset_scaled_log_variance),
            )
            .then(a.gm_priority2.cmp(&b.gm_priority2))
            .then(a.gm_identity.cmp(&b.gm_identity));
        return match ord {
            Ordering::Less => Comparison::ABetter,
            Ordering::Greater => Comparison::BBetter,
            Ordering::Equal => Comparison::Error2,
        };
    }
    // Figure 28: gleicher Grandmaster, Topologie entscheidet
    let (sa, sb) = (a.steps_removed as u32, b.steps_removed as u32);
    if sa > sb + 1 {
        return Comparison::BBetter;
    }
    if sb > sa + 1 {
        return Comparison::ABetter;
    }
    if sa > sb {
        return match a.receiver.cmp(&a.sender) {
            Ordering::Less => Comparison::BBetter,
            Ordering::Greater => Comparison::BBetterByTopology,
            Ordering::Equal => Comparison::Error1,
        };
    }
    if sb > sa {
        return match b.receiver.cmp(&b.sender) {
            Ordering::Less => Comparison::ABetter,
            Ordering::Greater => Comparison::ABetterByTopology,
            Ordering::Equal => Comparison::Error1,
        };
    }
    match a.sender.cmp(&b.sender) {
        Ordering::Less => Comparison::ABetterByTopology,
        Ordering::Greater => Comparison::BBetterByTopology,
        Ordering::Equal => match a.receiver.port_number.cmp(&b.receiver.port_number) {
            Ordering::Less => Comparison::ABetterByTopology,
            Ordering::Greater => Comparison::BBetterByTopology,
            Ordering::Equal => Comparison::Error2,
        },
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Recommendation {
    Master,
    Passive,
}

/// Zustandsentscheidung (9.3.3, vereinfacht für einen Port ohne Slave-Modus).
pub fn decide(d0: &Dataset, erbest: Option<&Dataset>) -> Recommendation {
    match erbest {
        None => Recommendation::Master,
        Some(e) if compare(d0, e).a_wins() => Recommendation::Master,
        Some(_) => Recommendation::Passive,
    }
}

/// Ein fremder Master (Foreign Master Record, 9.3.2.4).
#[derive(Clone, Debug)]
pub struct ForeignMaster {
    pub dataset: Dataset,
    pub announce: Announce,
    pub flags: u16,
    pub log_announce_interval: i8,
    pub addr: Option<SocketAddrV4>,
    pub first_seen: Instant,
    pub last_seen: Instant,
    arrivals: VecDeque<Instant>,
}

impl ForeignMaster {
    /// Announce-Intervall des fremden Masters; unplausible Werte → 2 s.
    pub fn announce_interval(&self) -> Duration {
        let l = self.log_announce_interval;
        log_interval_to_duration(if (-7..=7).contains(&l) { l } else { 1 })
    }

    /// Mindestens FOREIGN_MASTER_THRESHOLD Announce im Zeitfenster.
    pub fn qualified(&self, now: Instant) -> bool {
        let window = self.announce_interval() * FOREIGN_MASTER_TIME_WINDOW;
        self.arrivals
            .iter()
            .filter(|t| now.saturating_duration_since(**t) <= window)
            .count()
            >= FOREIGN_MASTER_THRESHOLD
    }
}

/// Tabelle der fremden Master.
#[derive(Debug, Default)]
pub struct ForeignMasters {
    map: HashMap<PortIdentity, ForeignMaster>,
    receipt_timeout: u32,
}

impl ForeignMasters {
    pub fn new(announce_receipt_timeout: u8) -> Self {
        Self {
            map: HashMap::new(),
            receipt_timeout: announce_receipt_timeout.max(2) as u32,
        }
    }

    /// Announce eintragen. Liefert true, wenn der Absender neu ist.
    pub fn record(
        &mut self,
        header: &Header,
        announce: &Announce,
        receiver: PortIdentity,
        addr: Option<SocketAddrV4>,
        now: Instant,
    ) -> bool {
        let ds = Dataset::from_announce(header, announce, receiver);
        let key = header.source_port_identity;
        let new = !self.map.contains_key(&key);
        let fm = self.map.entry(key).or_insert_with(|| ForeignMaster {
            dataset: ds,
            announce: *announce,
            flags: header.flags,
            log_announce_interval: header.log_message_interval,
            addr,
            first_seen: now,
            last_seen: now,
            arrivals: VecDeque::new(),
        });
        fm.dataset = ds;
        fm.announce = *announce;
        fm.flags = header.flags;
        fm.log_announce_interval = header.log_message_interval;
        fm.addr = addr.or(fm.addr);
        fm.last_seen = now;
        fm.arrivals.push_back(now);
        let window = fm.announce_interval() * FOREIGN_MASTER_TIME_WINDOW;
        while fm
            .arrivals
            .front()
            .is_some_and(|t| now.saturating_duration_since(*t) > window)
        {
            fm.arrivals.pop_front();
        }
        new
    }

    /// Einträge ohne Announce seit announceReceiptTimeout Intervallen entfernen.
    pub fn prune(&mut self, now: Instant) -> Vec<ForeignMaster> {
        let timeout = self.receipt_timeout;
        let expired: Vec<PortIdentity> = self
            .map
            .iter()
            .filter(|(_, f)| {
                now.saturating_duration_since(f.last_seen) > f.announce_interval() * timeout
            })
            .map(|(k, _)| *k)
            .collect();
        expired
            .into_iter()
            .filter_map(|k| self.map.remove(&k))
            .collect()
    }

    /// Bester qualifizierter fremder Master (Erbest).
    pub fn best(&self, now: Instant) -> Option<&ForeignMaster> {
        self.map
            .values()
            .filter(|f| f.qualified(now))
            .reduce(|a, b| {
                if compare(&b.dataset, &a.dataset).a_wins() {
                    b
                } else {
                    a
                }
            })
    }

    pub fn len(&self) -> usize {
        self.map.len()
    }

    pub fn is_empty(&self) -> bool {
        self.map.is_empty()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::proto::{MessageType, PortIdentity};

    fn pid(last: u8, port: u16) -> PortIdentity {
        PortIdentity {
            clock_identity: ClockIdentity([0, 0x1b, 0x21, 0xff, 0xfe, 0, 0, last]),
            port_number: port,
        }
    }

    fn q(class: u8, acc: u8, var: u16) -> ClockQuality {
        ClockQuality {
            clock_class: class,
            clock_accuracy: acc,
            offset_scaled_log_variance: var,
        }
    }

    fn own() -> Dataset {
        Dataset::local(pid(1, 1), 128, q(248, 0xfe, 0xffff), 128)
    }

    fn foreign(id: u8, p1: u8, class: u8, p2: u8) -> Dataset {
        Dataset {
            gm_priority1: p1,
            gm_identity: pid(id, 1).clock_identity,
            gm_quality: q(class, 0xfe, 0xffff),
            gm_priority2: p2,
            steps_removed: 0,
            sender: pid(id, 1),
            receiver: pid(1, 1),
        }
    }

    #[test]
    fn priority1_beats_everything() {
        // Dante-Gerät als "Preferred Leader" (priority1 = 1) gegen uns mit 128
        let dante = foreign(9, 1, 248, 128);
        assert_eq!(compare(&own(), &dante), Comparison::BBetter);
        assert_eq!(decide(&own(), Some(&dante)), Recommendation::Passive);
        // Wir mit priority1 = 0 gewinnen auch gegen bessere clockClass
        let us = Dataset::local(pid(1, 1), 0, q(248, 0xfe, 0xffff), 128);
        let gps = foreign(9, 1, 6, 128);
        assert_eq!(decide(&us, Some(&gps)), Recommendation::Master);
    }

    #[test]
    fn attribute_order() {
        let base = foreign(5, 128, 248, 128);
        // Varianten mit größerer (schlechterer) clockIdentity: das Attribut muss trotzdem
        // entscheiden, weil es vor der Identity verglichen wird.
        let variant = || foreign(6, 128, 248, 128);

        let mut better_class = variant();
        better_class.gm_quality.clock_class = 6;
        assert_eq!(compare(&better_class, &base), Comparison::ABetter);

        let mut better_acc = variant();
        better_acc.gm_quality.clock_accuracy = 0x21;
        assert_eq!(compare(&better_acc, &base), Comparison::ABetter);

        let mut better_var = variant();
        better_var.gm_quality.offset_scaled_log_variance = 0x4e5d;
        assert_eq!(compare(&base, &better_var), Comparison::BBetter);

        let mut better_p2 = variant();
        better_p2.gm_priority2 = 127;
        assert_eq!(compare(&better_p2, &base), Comparison::ABetter);

        // clockClass schlägt priority2
        let mut c = variant();
        c.gm_quality.clock_class = 247;
        c.gm_priority2 = 255;
        assert_eq!(compare(&c, &base), Comparison::ABetter);

        // Ohne Unterschied in den Attributen entscheidet die Identity
        assert_eq!(compare(&variant(), &base), Comparison::BBetter);
    }

    #[test]
    fn identity_is_tiebreaker() {
        // Alles gleich: kleinere clockIdentity gewinnt
        let a = foreign(2, 128, 248, 128);
        let b = foreign(3, 128, 248, 128);
        assert_eq!(compare(&a, &b), Comparison::ABetter);
        assert_eq!(compare(&b, &a), Comparison::BBetter);
        // Gegen uns (…:01): wir gewinnen gegen …:02
        assert_eq!(decide(&own(), Some(&a)), Recommendation::Master);
    }

    #[test]
    fn same_grandmaster_topology() {
        let gm = foreign(7, 128, 248, 128);
        let mut via_bc = gm;
        via_bc.steps_removed = 2;
        via_bc.sender = pid(8, 1);
        assert_eq!(compare(&gm, &via_bc), Comparison::ABetter);
        assert_eq!(compare(&via_bc, &gm), Comparison::BBetter);

        // Ein Schritt Unterschied: Empfänger vs. Absender von A
        let mut one = gm;
        one.steps_removed = 1;
        one.sender = pid(8, 1);
        one.receiver = pid(1, 1); // Empfänger < Absender
        assert_eq!(compare(&one, &gm), Comparison::BBetter);
        one.receiver = pid(9, 1); // Empfänger > Absender
        assert_eq!(compare(&one, &gm), Comparison::BBetterByTopology);
        one.receiver = one.sender;
        assert_eq!(compare(&one, &gm), Comparison::Error1);

        // Gleiche stepsRemoved: kleinerer Absender gewinnt nach Topologie
        let mut x = gm;
        x.sender = pid(3, 1);
        let mut y = gm;
        y.sender = pid(4, 1);
        assert_eq!(compare(&x, &y), Comparison::ABetterByTopology);
        // Gleicher Absender, verschiedene Empfänger-Ports
        let mut r2 = x;
        r2.receiver.port_number = 2;
        assert_eq!(compare(&x, &r2), Comparison::ABetterByTopology);
        assert_eq!(compare(&x, &x), Comparison::Error2);
    }

    #[test]
    fn no_foreign_means_master() {
        assert_eq!(decide(&own(), None), Recommendation::Master);
    }

    fn announce(p1: u8, gm: ClockIdentity) -> Announce {
        Announce {
            origin_timestamp: Default::default(),
            current_utc_offset: 37,
            grandmaster_priority1: p1,
            grandmaster_clock_quality: q(248, 0xfe, 0xffff),
            grandmaster_priority2: 128,
            grandmaster_identity: gm,
            steps_removed: 0,
            time_source: 0xa0,
        }
    }

    #[test]
    fn foreign_master_qualification_and_timeout() {
        let t0 = Instant::now();
        let src = pid(9, 1);
        let mut h = Header::new(MessageType::Announce, 0, src, 0, 0); // 1 s
        let a = announce(100, src.clock_identity);
        let mut fm = ForeignMasters::new(3);

        assert!(fm.record(&h, &a, pid(1, 1), None, t0));
        // Ein Announce reicht nicht (Schwelle 2)
        assert!(fm.best(t0).is_none());
        h.sequence_id = 1;
        assert!(!fm.record(&h, &a, pid(1, 1), None, t0 + Duration::from_secs(1)));
        let b = fm.best(t0 + Duration::from_secs(1)).unwrap();
        assert_eq!(b.dataset.gm_priority1, 100);

        // Zwei Announce, die weiter als 4 Intervalle auseinanderliegen, qualifizieren nicht
        let mut fm2 = ForeignMasters::new(10);
        fm2.record(&h, &a, pid(1, 1), None, t0);
        fm2.record(&h, &a, pid(1, 1), None, t0 + Duration::from_secs(5));
        assert!(fm2.best(t0 + Duration::from_secs(5)).is_none());

        // Timeout: 3 × 1 s ohne Announce
        assert!(fm.prune(t0 + Duration::from_millis(3900)).is_empty());
        let gone = fm.prune(t0 + Duration::from_millis(4100));
        assert_eq!(gone.len(), 1);
        assert!(fm.is_empty());
    }

    #[test]
    fn best_of_several() {
        let t0 = Instant::now();
        let mut fm = ForeignMasters::new(3);
        for (id, p1) in [(5u8, 120u8), (6, 90), (7, 200)] {
            let src = pid(id, 1);
            let h = Header::new(MessageType::Announce, 0, src, 0, -1);
            let a = announce(p1, src.clock_identity);
            fm.record(&h, &a, pid(1, 1), None, t0);
            fm.record(&h, &a, pid(1, 1), None, t0 + Duration::from_millis(500));
        }
        let best = fm.best(t0 + Duration::from_millis(500)).unwrap();
        assert_eq!(best.dataset.gm_priority1, 90);
        assert_eq!(fm.len(), 3);
    }

    #[test]
    fn interval_of_foreign_is_sanitised() {
        let t0 = Instant::now();
        let mut fm = ForeignMasters::new(3);
        let src = pid(9, 1);
        let h = Header::new(MessageType::Announce, 0, src, 0, 0x7f);
        fm.record(&h, &announce(1, src.clock_identity), pid(1, 1), None, t0);
        // 0x7F → 2 s angenommen → Timeout 6 s
        assert!(fm.prune(t0 + Duration::from_secs(5)).is_empty());
        assert_eq!(fm.prune(t0 + Duration::from_secs(7)).len(), 1);
    }
}
