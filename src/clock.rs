//! Systemuhr und Abbildung der Hardware-Uhr der Netzwerkkarte auf QPC.

use std::collections::VecDeque;

use crate::time::{NANOS_PER_SEC, utc_to_tai_ns};

/// Systemzeit (UTC) in ns seit Unix-Epoche. Unter Windows GetSystemTimePreciseAsFileTime.
pub fn system_utc_ns() -> i128 {
    #[cfg(windows)]
    {
        crate::win::system_utc_ns()
    }
    #[cfg(not(windows))]
    {
        let d = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default();
        d.as_nanos() as i128
    }
}

/// Aktuelle PTP-Zeit (TAI) in ns.
pub fn now_tai_ns(utc_offset: i16) -> i128 {
    utc_to_tai_ns(system_utc_ns(), utc_offset)
}

/// Lineares Modell Hardware-Uhr → QPC aus Cross-Timestamps.
///
/// Jeder Punkt ist (Hardware-Zählerstand, QPC-Mittelwert). Die Steigung kommt aus dem ältesten
/// und dem neuesten Punkt im Fenster; mit nur einem Punkt wird die Nennfrequenz verwendet.
#[derive(Debug, Clone)]
pub struct HwClockModel {
    points: VecDeque<(u64, i128)>,
    max_points: usize,
    nominal_hz: u64,
    qpc_hz: i64,
}

impl HwClockModel {
    pub fn new(nominal_hz: u64, qpc_hz: i64, max_points: usize) -> Self {
        Self {
            points: VecDeque::new(),
            max_points: max_points.max(2),
            nominal_hz,
            qpc_hz: qpc_hz.max(1),
        }
    }

    pub fn add(&mut self, hw: u64, qpc_before: i64, qpc_after: i64) {
        let mid = (qpc_before as i128 + qpc_after as i128) / 2;
        if let Some(&(last_hw, _)) = self.points.back()
            && hw <= last_hw
        {
            // Uhr zurückgesetzt (Treiber-Neustart): Modell verwerfen
            self.points.clear();
        }
        self.points.push_back((hw, mid));
        while self.points.len() > self.max_points {
            self.points.pop_front();
        }
    }

    pub fn len(&self) -> usize {
        self.points.len()
    }

    pub fn is_empty(&self) -> bool {
        self.points.is_empty()
    }

    /// Hardware-Zählerstand → QPC-Zählerstand.
    pub fn hw_to_qpc(&self, hw: u64) -> Option<i128> {
        let &(h1, q1) = self.points.back()?;
        let dh = hw as i128 - h1 as i128;
        if self.points.len() >= 2 {
            let &(h0, q0) = self.points.front()?;
            let span_h = h1 as i128 - h0 as i128;
            if span_h > 0 {
                return Some(q1 + dh * (q1 - q0) / span_h);
            }
        }
        if self.nominal_hz == 0 {
            return None;
        }
        Some(q1 + dh * self.qpc_hz as i128 / self.nominal_hz as i128)
    }

    /// Geschätzte Frequenz der Hardware-Uhr in Hz (nur Anzeige).
    pub fn estimated_hz(&self) -> Option<f64> {
        if self.points.len() < 2 {
            return (self.nominal_hz > 0).then_some(self.nominal_hz as f64);
        }
        let (h0, q0) = *self.points.front()?;
        let (h1, q1) = *self.points.back()?;
        let secs = (q1 - q0) as f64 / self.qpc_hz as f64;
        (secs > 0.0).then(|| (h1 - h0) as f64 / secs)
    }
}

/// QPC-Differenz → Nanosekunden.
pub fn qpc_ticks_to_ns(ticks: i128, qpc_hz: i64) -> i128 {
    ticks * NANOS_PER_SEC / qpc_hz.max(1) as i128
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn model_with_regression_points() {
        // NIC-Uhr 1 GHz (ns-Zähler), QPC 10 MHz, NIC läuft 10 ppm schneller.
        let qpc_hz = 10_000_000;
        let mut m = HwClockModel::new(1_000_000_000, qpc_hz, 8);
        assert!(m.hw_to_qpc(1).is_none());
        for i in 0..5i64 {
            let q = 1_000_000 + i * qpc_hz; // jede Sekunde
            let hw = (5_000_000_000_f64 + i as f64 * 1_000_010_000.0) as u64;
            m.add(hw, q - 5, q + 5);
        }
        assert_eq!(m.len(), 5);
        // 0,5 s nach dem letzten Punkt
        let hw = 5_000_000_000u64 + 4 * 1_000_010_000 + 500_005_000;
        let q = m.hw_to_qpc(hw).unwrap();
        let expected = 1_000_000 + 4 * qpc_hz as i128 + qpc_hz as i128 / 2;
        assert!((q - expected).abs() <= 1, "q={q} expected={expected}");
        let hz = m.estimated_hz().unwrap();
        assert!((hz - 1_000_010_000.0).abs() < 1.0);
    }

    #[test]
    fn model_nominal_with_single_point() {
        let mut m = HwClockModel::new(125_000_000, 10_000_000, 4);
        m.add(1_000, 500, 500);
        // 125 Hardware-Ticks = 1 µs = 10 QPC-Ticks
        assert_eq!(m.hw_to_qpc(1_125), Some(510));
    }

    #[test]
    fn model_reset_on_clock_jump() {
        let mut m = HwClockModel::new(0, 10_000_000, 4);
        m.add(1_000_000, 0, 0);
        m.add(2_000_000, 10, 10);
        m.add(500, 20, 20); // Rücksprung
        assert_eq!(m.len(), 1);
        assert!(m.hw_to_qpc(600).is_none()); // keine Nennfrequenz, nur ein Punkt
    }

    #[test]
    fn window_is_bounded() {
        let mut m = HwClockModel::new(0, 1, 3);
        for i in 0..10 {
            m.add(i * 10 + 1, i as i64, i as i64);
        }
        assert_eq!(m.len(), 3);
    }

    #[test]
    fn qpc_conversion() {
        assert_eq!(qpc_ticks_to_ns(10_000_000, 10_000_000), 1_000_000_000);
        assert_eq!(qpc_ticks_to_ns(1, 10_000_000), 100);
    }

    #[test]
    fn tai_now_is_offset() {
        let utc = system_utc_ns();
        let tai = now_tai_ns(37);
        let d = tai - utc - 37 * NANOS_PER_SEC;
        assert!((0..1_000_000_000).contains(&d));
    }
}
