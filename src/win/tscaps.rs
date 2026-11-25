//! Timestamp-Fähigkeiten der Netzwerkkarte (iphlpapi, ab Windows 10 Build 20348 /
//! Windows 11 / Server 2022) und Hardware-Cross-Timestamps.
//!
//! Die Funktionen werden per GetProcAddress geladen, damit ptpmaster.exe auch auf älteren
//! Windows-Versionen startet (dann im Userspace-Modus). Ein statischer Import würde dort
//! schon beim Laden der exe mit "Einsprungpunkt nicht gefunden" scheitern.

use std::sync::OnceLock;

use windows::Win32::NetworkManagement::IpHelper::{
    INTERFACE_HARDWARE_CROSSTIMESTAMP, INTERFACE_TIMESTAMP_CAPABILITIES,
};
use windows::Win32::NetworkManagement::Ndis::NET_LUID_LH;
use windows::Win32::System::LibraryLoader::{GetProcAddress, LoadLibraryW};
use windows::core::{s, w};

use crate::net::TsCaps;

type GetCapsFn =
    unsafe extern "system" fn(*const NET_LUID_LH, *mut INTERFACE_TIMESTAMP_CAPABILITIES) -> u32;
type CrossFn =
    unsafe extern "system" fn(*const NET_LUID_LH, *mut INTERFACE_HARDWARE_CROSSTIMESTAMP) -> u32;

struct Api {
    active: Option<GetCapsFn>,
    supported: Option<GetCapsFn>,
    cross: Option<CrossFn>,
}

fn api() -> &'static Api {
    static API: OnceLock<Api> = OnceLock::new();
    API.get_or_init(|| {
        // SAFETY: iphlpapi.dll ist eine Systembibliothek; die Signaturen entsprechen iphlpapi.h.
        unsafe {
            let Ok(lib) = LoadLibraryW(w!("iphlpapi.dll")) else {
                return Api {
                    active: None,
                    supported: None,
                    cross: None,
                };
            };
            let active = GetProcAddress(lib, s!("GetInterfaceActiveTimestampCapabilities"))
                .map(|f| std::mem::transmute::<_, GetCapsFn>(f));
            let supported = GetProcAddress(lib, s!("GetInterfaceSupportedTimestampCapabilities"))
                .map(|f| std::mem::transmute::<_, GetCapsFn>(f));
            let cross = GetProcAddress(lib, s!("CaptureInterfaceHardwareCrossTimestamp"))
                .map(|f| std::mem::transmute::<_, CrossFn>(f));
            Api {
                active,
                supported,
                cross,
            }
        }
    })
}

/// Bietet dieses Windows die Timestamping-API überhaupt an?
pub fn api_available() -> bool {
    api().active.is_some()
}

/// Großzügig gepolsterter Puffer, falls eine künftige Windows-Version die Structs verlängert.
#[repr(C)]
#[derive(Default)]
struct CapsBuf {
    caps: INTERFACE_TIMESTAMP_CAPABILITIES,
    _pad: [u64; 8],
}

#[repr(C)]
#[derive(Default)]
struct CrossBuf {
    cross: INTERFACE_HARDWARE_CROSSTIMESTAMP,
    _pad: [u64; 8],
}

fn query(f: Option<GetCapsFn>, luid: u64) -> Result<TsCaps, String> {
    let Some(f) = f else {
        return Err("Timestamping-API fehlt (Windows älter als Build 20348)".into());
    };
    let luid = NET_LUID_LH { Value: luid };
    let mut buf = CapsBuf::default();
    // SAFETY: gültige Zeiger; Puffer ist größer als die dokumentierte Struct.
    let rc = unsafe { f(&luid, &mut buf.caps) };
    match rc {
        0 => Ok(convert(&buf.caps)),
        50 => Err("Treiber unterstützt kein Timestamping (ERROR_NOT_SUPPORTED)".into()),
        2001 => Err("Treiber meldet ungültige Timestamp-Konfiguration (ERROR_BAD_DRIVER)".into()),
        other => Err(format!("Fehlercode {other}")),
    }
}

fn convert(c: &INTERFACE_TIMESTAMP_CAPABILITIES) -> TsCaps {
    let h = &c.HardwareCapabilities;
    let s = &c.SoftwareCapabilities;
    TsCaps {
        hw_rx: h.PtpV2OverUdpIPv4EventMessageReceive
            || h.PtpV2OverUdpIPv4AllMessageReceive
            || h.AllReceive,
        hw_tx: h.PtpV2OverUdpIPv4EventMessageTransmit
            || h.PtpV2OverUdpIPv4AllMessageTransmit
            || h.AllTransmit
            || h.TaggedTransmit,
        sw_rx: s.AllReceive,
        sw_tx: s.AllTransmit || s.TaggedTransmit,
        cross_timestamp: c.SupportsCrossTimestamp,
        hw_clock_hz: c.HardwareClockFrequencyHz,
    }
}

/// Aktuell aktivierte Fähigkeiten (abhängig von den erweiterten Treibereinstellungen).
pub fn active(luid: u64) -> Result<TsCaps, String> {
    query(api().active, luid)
}

/// Grundsätzlich unterstützte Fähigkeiten.
pub fn supported(luid: u64) -> Result<TsCaps, String> {
    query(api().supported, luid)
}

/// Ein Cross-Timestamp: (QPC vorher, Hardware-Uhr, QPC nachher).
#[derive(Clone, Copy, Debug)]
pub struct CrossTimestamp {
    pub qpc_before: i64,
    pub hw: u64,
    pub qpc_after: i64,
}

pub fn capture_cross_timestamp(luid: u64) -> Result<CrossTimestamp, String> {
    let Some(f) = api().cross else {
        return Err("CaptureInterfaceHardwareCrossTimestamp fehlt (vor Windows 11)".into());
    };
    let luid = NET_LUID_LH { Value: luid };
    let mut buf = CrossBuf::default();
    // SAFETY: gültige Zeiger; Puffer ist größer als die dokumentierte Struct.
    let rc = unsafe { f(&luid, &mut buf.cross) };
    if rc != 0 {
        return Err(format!(
            "CaptureInterfaceHardwareCrossTimestamp: Fehler {rc}"
        ));
    }
    let c = &buf.cross;
    let ct = CrossTimestamp {
        qpc_before: c.SystemTimestamp1 as i64,
        hw: c.HardwareClockTimestamp,
        qpc_after: c.SystemTimestamp2 as i64,
    };
    // Plausibilitätsprüfung: beide QPC-Werte müssen nah an "jetzt" liegen und geordnet sein.
    let now = super::qpc();
    let freq = super::qpc_frequency();
    if ct.qpc_before > ct.qpc_after
        || (now - ct.qpc_after).abs() > freq
        || ct.qpc_after - ct.qpc_before > freq / 100
    {
        return Err(format!(
            "unplausibler Cross-Timestamp {ct:?} (QPC jetzt {now})"
        ));
    }
    Ok(ct)
}
