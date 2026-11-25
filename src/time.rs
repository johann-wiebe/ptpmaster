//! Zeitdarstellung: PTP-Timestamps, TAI/UTC-Umrechnung, Log-Intervalle.
//!
//! Die PTP-Epoche ist 1970-01-01 00:00:00 TAI. Damit gilt für die PTP-Timescale
//! (ptpTimescale = TRUE): `PTP-Sekunden = Unix-UTC-Sekunden + currentUtcOffset`.
//! Mit dem Stand von 2017 (letzte Schaltsekunde Ende 2016) ist currentUtcOffset = 37 s.

use std::fmt;
use std::time::Duration;

pub const NANOS_PER_SEC: i128 = 1_000_000_000;

/// Differenz zwischen FILETIME-Epoche (1601-01-01) und Unix-Epoche (1970-01-01) in 100-ns-Einheiten.
pub const FILETIME_UNIX_EPOCH_DIFF_100NS: u64 = 116_444_736_000_000_000;

/// Standardwert für currentUtcOffset (TAI − UTC) seit 2017-01-01.
pub const DEFAULT_UTC_OFFSET: i16 = 37;

/// PTP-Timestamp nach IEEE 1588-2008, 5.3.3: 48 Bit Sekunden, 32 Bit Nanosekunden.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct PtpTimestamp {
    pub seconds: u64,
    pub nanoseconds: u32,
}

impl PtpTimestamp {
    pub const MAX_SECONDS: u64 = (1u64 << 48) - 1;

    pub const fn new(seconds: u64, nanoseconds: u32) -> Self {
        Self {
            seconds,
            nanoseconds,
        }
    }

    /// Aus Nanosekunden seit PTP-Epoche. Negative Werte werden auf 0 begrenzt,
    /// Werte jenseits von 2^48 s abgeschnitten (tritt in der Praxis nicht auf).
    pub fn from_nanos(ns: i128) -> Self {
        let ns = ns.max(0);
        let seconds = (ns / NANOS_PER_SEC) as u64 & Self::MAX_SECONDS;
        let nanoseconds = (ns % NANOS_PER_SEC) as u32;
        Self {
            seconds,
            nanoseconds,
        }
    }

    pub fn to_nanos(self) -> i128 {
        self.seconds as i128 * NANOS_PER_SEC + self.nanoseconds as i128
    }
}

impl fmt::Display for PtpTimestamp {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}.{:09}", self.seconds, self.nanoseconds)
    }
}

/// correctionField / TimeInterval: Nanosekunden × 2^16 (IEEE 1588-2008, 5.3.2).
pub fn correction_to_nanos(correction: i64) -> f64 {
    correction as f64 / 65536.0
}

pub fn nanos_to_correction(ns: f64) -> i64 {
    (ns * 65536.0).round() as i64
}

/// UTC (Nanosekunden seit Unix-Epoche) → PTP-Timescale (TAI, Nanosekunden seit PTP-Epoche).
pub fn utc_to_tai_ns(utc_ns: i128, utc_offset: i16) -> i128 {
    utc_ns + utc_offset as i128 * NANOS_PER_SEC
}

/// PTP-Timescale (TAI) → UTC.
pub fn tai_to_utc_ns(tai_ns: i128, utc_offset: i16) -> i128 {
    tai_ns - utc_offset as i128 * NANOS_PER_SEC
}

/// Windows-FILETIME (100 ns seit 1601-01-01 UTC) → Nanosekunden seit Unix-Epoche.
pub fn filetime_to_unix_ns(filetime: u64) -> i128 {
    (filetime as i128 - FILETIME_UNIX_EPOCH_DIFF_100NS as i128) * 100
}

/// Log2-Intervall (z. B. logSyncInterval = -3) → Dauer (2^-3 s = 125 ms).
/// Gültiger Bereich nach Spezifikation: -128..=127; praktisch begrenzen wir auf -10..=10.
pub fn log_interval_to_duration(log_interval: i8) -> Duration {
    let l = log_interval.clamp(-10, 10) as i32;
    if l >= 0 {
        Duration::from_secs(1u64 << l)
    } else {
        Duration::from_nanos(1_000_000_000u64 >> (-l))
    }
}

/// Nachrichten pro Sekunde für ein Log-Intervall (nur für Anzeige).
pub fn log_interval_rate(log_interval: i8) -> f64 {
    2f64.powi(-(log_interval as i32))
}

/// Menschenlesbare Ausgabe eines Zeitabstands in ns mit passender Einheit.
pub fn format_ns(ns: f64) -> String {
    let a = ns.abs();
    if a < 1_000.0 {
        format!("{ns:+.0} ns")
    } else if a < 1_000_000.0 {
        format!("{:+.2} µs", ns / 1_000.0)
    } else if a < 1_000_000_000.0 {
        format!("{:+.3} ms", ns / 1_000_000.0)
    } else {
        format!("{:+.6} s", ns / 1_000_000_000.0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn timestamp_roundtrip_nanos() {
        let ts = PtpTimestamp::new(1_700_000_037, 123_456_789);
        assert_eq!(PtpTimestamp::from_nanos(ts.to_nanos()), ts);
        assert_eq!(PtpTimestamp::from_nanos(-5), PtpTimestamp::new(0, 0));
    }

    #[test]
    fn tai_utc_conversion() {
        // 2026-01-01T00:00:00Z = Unix 1767225600
        let utc = 1_767_225_600i128 * NANOS_PER_SEC + 250;
        let tai = utc_to_tai_ns(utc, 37);
        assert_eq!(tai, (1_767_225_600i128 + 37) * NANOS_PER_SEC + 250);
        assert_eq!(tai_to_utc_ns(tai, 37), utc);
        // Abweichender Offset (z. B. nach künftiger Schaltsekunde)
        assert_eq!(utc_to_tai_ns(0, 38), 38 * NANOS_PER_SEC);
        // Offset 0 = PTP-Timescale identisch mit UTC (ARB-ähnlich, nicht empfohlen)
        assert_eq!(utc_to_tai_ns(utc, 0), utc);
    }

    #[test]
    fn filetime_conversion() {
        // Unix-Epoche als FILETIME
        assert_eq!(filetime_to_unix_ns(FILETIME_UNIX_EPOCH_DIFF_100NS), 0);
        // 2026-01-01T00:00:00Z
        let ft = FILETIME_UNIX_EPOCH_DIFF_100NS + 1_767_225_600u64 * 10_000_000 + 3;
        assert_eq!(
            filetime_to_unix_ns(ft),
            1_767_225_600i128 * NANOS_PER_SEC + 300
        );
    }

    #[test]
    fn log_intervals() {
        assert_eq!(log_interval_to_duration(0), Duration::from_secs(1));
        assert_eq!(log_interval_to_duration(1), Duration::from_secs(2));
        assert_eq!(log_interval_to_duration(4), Duration::from_secs(16));
        assert_eq!(log_interval_to_duration(-1), Duration::from_millis(500));
        assert_eq!(log_interval_to_duration(-3), Duration::from_millis(125));
        assert_eq!(log_interval_to_duration(-4), Duration::from_micros(62_500));
        assert_eq!(
            log_interval_to_duration(-7),
            Duration::from_nanos(7_812_500)
        );
        // Begrenzung
        assert_eq!(log_interval_to_duration(100), Duration::from_secs(1024));
        assert_eq!(log_interval_rate(-3), 8.0);
        assert_eq!(log_interval_rate(1), 0.5);
    }

    #[test]
    fn correction_field() {
        assert_eq!(nanos_to_correction(1.0), 65536);
        assert_eq!(nanos_to_correction(-2.5), -163_840);
        assert_eq!(correction_to_nanos(0x0001_8000), 1.5);
    }

    #[test]
    fn formatting() {
        assert_eq!(format_ns(12.0), "+12 ns");
        assert_eq!(format_ns(-1500.0), "-1.50 µs");
        assert_eq!(format_ns(2_500_000.0), "+2.500 ms");
    }
}
