//! ptpmaster – PTP-Grandmaster (IEEE 1588-2008, PTPv2 über UDP/IPv4) für Windows.
//!
//! Der Protokollkern (Nachrichten, BMCA, Master-Logik, probe) ist plattformunabhängig und
//! läuft für Tests auch unter Linux; Winsock-Timestamping, Dienst und Event Log gibt es nur
//! unter Windows.

pub mod clock;
pub mod config;
pub mod iface;
pub mod logging;
pub mod net;
pub mod proto;
pub mod shutdown;
pub mod time;
#[cfg(windows)]
pub mod win;
