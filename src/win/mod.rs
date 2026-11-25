//! Windows-spezifische Teile: Systemuhr, Interfaces, Winsock-Timestamping,
//! Event Log, Dienst.

pub mod eventlog;
pub mod iface;
pub mod service;
pub mod sock;
pub mod tscaps;

use std::sync::OnceLock;

use windows::Win32::Foundation::FILETIME;
use windows::Win32::System::Console::{
    CONSOLE_MODE, ENABLE_PROCESSED_OUTPUT, ENABLE_VIRTUAL_TERMINAL_PROCESSING, GetConsoleMode,
    GetStdHandle, STD_OUTPUT_HANDLE, SetConsoleMode,
};
use windows::Win32::System::Performance::{QueryPerformanceCounter, QueryPerformanceFrequency};
use windows::Win32::System::SystemInformation::GetSystemTimePreciseAsFileTime;

use crate::time::{NANOS_PER_SEC, filetime_to_unix_ns};

/// Systemzeit (UTC) in ns seit Unix-Epoche, Auflösung < 1 µs.
pub fn system_utc_ns() -> i128 {
    // SAFETY: einfache Abfrage ohne Zeiger-Argumente.
    let ft: FILETIME = unsafe { GetSystemTimePreciseAsFileTime() };
    filetime_to_unix_ns(((ft.dwHighDateTime as u64) << 32) | ft.dwLowDateTime as u64)
}

pub fn qpc() -> i64 {
    let mut v = 0i64;
    // SAFETY: gültiger Zeiger auf lokale Variable.
    let _ = unsafe { QueryPerformanceCounter(&mut v) };
    v
}

pub fn qpc_frequency() -> i64 {
    static FREQ: OnceLock<i64> = OnceLock::new();
    *FREQ.get_or_init(|| {
        let mut f = 0i64;
        // SAFETY: gültiger Zeiger auf lokale Variable.
        let _ = unsafe { QueryPerformanceFrequency(&mut f) };
        f.max(1)
    })
}

/// QPC-Wert → Systemzeit (UTC ns). GetSystemTimePreciseAsFileTime basiert selbst auf QPC;
/// wir korrelieren beide Uhren unmittelbar vor der Umrechnung, sodass Frequenzkorrekturen
/// von w32time keine Rolle spielen (die Differenz liegt im Bereich weniger Millisekunden).
pub fn qpc_to_utc_ns(qpc_value: i64) -> i128 {
    let freq = qpc_frequency() as i128;
    // Das engste von drei Messfenstern verwenden.
    let mut best: Option<(i64, i128, i64)> = None;
    for _ in 0..3 {
        let q1 = qpc();
        let sys = system_utc_ns();
        let q2 = qpc();
        if best.is_none_or(|(a, _, b)| q2 - q1 < b - a) {
            best = Some((q1, sys, q2));
        }
    }
    let (q1, sys, q2) = best.unwrap_or_default();
    let qmid = (q1 as i128 + q2 as i128) / 2;
    sys - (qmid - qpc_value as i128) * NANOS_PER_SEC / freq
}

/// VT-Escape-Sequenzen in der Konsole aktivieren. Liefert false ohne Konsole (Dienst, Umleitung).
pub fn enable_vt_mode() -> bool {
    // SAFETY: Abfrage/Setzen des Konsolenmodus auf dem Standard-Handle.
    unsafe {
        let Ok(h) = GetStdHandle(STD_OUTPUT_HANDLE) else {
            return false;
        };
        let mut mode = CONSOLE_MODE::default();
        if GetConsoleMode(h, &mut mode).is_err() {
            return false;
        }
        SetConsoleMode(
            h,
            mode | ENABLE_VIRTUAL_TERMINAL_PROCESSING | ENABLE_PROCESSED_OUTPUT,
        )
        .is_ok()
    }
}

/// Windows-Version aus der Registry, z. B. "Windows 11 Pro 24H2 (Build 26100.2605)".
pub fn version_string() -> String {
    use windows::Win32::System::Registry::{
        HKEY_LOCAL_MACHINE, RRF_RT_REG_DWORD, RRF_RT_REG_SZ, RegGetValueW,
    };
    use windows::core::{HSTRING, PCWSTR};

    let key = HSTRING::from(r"SOFTWARE\Microsoft\Windows NT\CurrentVersion");
    let read_sz = |name: &str| -> Option<String> {
        let name = HSTRING::from(name);
        let mut buf = [0u16; 256];
        let mut len = (buf.len() * 2) as u32;
        // SAFETY: Puffer und Längenangabe passen zusammen.
        let r = unsafe {
            RegGetValueW(
                HKEY_LOCAL_MACHINE,
                PCWSTR(key.as_ptr()),
                PCWSTR(name.as_ptr()),
                RRF_RT_REG_SZ,
                None,
                Some(buf.as_mut_ptr().cast()),
                Some(&mut len),
            )
        };
        r.is_ok().then(|| {
            let n = buf.iter().position(|&c| c == 0).unwrap_or(buf.len());
            String::from_utf16_lossy(&buf[..n])
        })
    };
    let read_dword = |name: &str| -> Option<u32> {
        let name = HSTRING::from(name);
        let mut v = 0u32;
        let mut len = 4u32;
        // SAFETY: DWORD-Puffer mit passender Länge.
        let r = unsafe {
            RegGetValueW(
                HKEY_LOCAL_MACHINE,
                PCWSTR(key.as_ptr()),
                PCWSTR(name.as_ptr()),
                RRF_RT_REG_DWORD,
                None,
                Some((&mut v as *mut u32).cast()),
                Some(&mut len),
            )
        };
        r.is_ok().then_some(v)
    };
    let build: u32 = build_number();
    let mut product = read_sz("ProductName").unwrap_or_else(|| "Windows".into());
    // ProductName sagt auch auf Windows 11 noch "Windows 10".
    if build >= 22000 && product.starts_with("Windows 10") {
        product = product.replacen("Windows 10", "Windows 11", 1);
    }
    let display = read_sz("DisplayVersion").unwrap_or_default();
    let ubr = read_dword("UBR")
        .map(|u| format!(".{u}"))
        .unwrap_or_default();
    format!("{product} {display} (Build {build}{ubr})")
}

/// Build-Nummer (z. B. 22631). 0, wenn unbekannt.
pub fn build_number() -> u32 {
    use windows::Win32::System::Registry::{HKEY_LOCAL_MACHINE, RRF_RT_REG_SZ, RegGetValueW};
    use windows::core::{HSTRING, PCWSTR};
    let key = HSTRING::from(r"SOFTWARE\Microsoft\Windows NT\CurrentVersion");
    let name = HSTRING::from("CurrentBuildNumber");
    let mut buf = [0u16; 32];
    let mut len = (buf.len() * 2) as u32;
    // SAFETY: Puffer und Längenangabe passen zusammen.
    let r = unsafe {
        RegGetValueW(
            HKEY_LOCAL_MACHINE,
            PCWSTR(key.as_ptr()),
            PCWSTR(name.as_ptr()),
            RRF_RT_REG_SZ,
            None,
            Some(buf.as_mut_ptr().cast()),
            Some(&mut len),
        )
    };
    if r.is_err() {
        return 0;
    }
    let n = buf.iter().position(|&c| c == 0).unwrap_or(buf.len());
    String::from_utf16_lossy(&buf[..n])
        .trim()
        .parse()
        .unwrap_or(0)
}

/// UTF-16-Zeiger (nullterminiert) → String.
///
/// # Safety
/// `p` muss null oder ein gültiger, nullterminierter UTF-16-String sein.
pub unsafe fn pwstr_to_string(p: windows::core::PWSTR) -> String {
    if p.is_null() {
        return String::new();
    }
    // SAFETY: laut Vertrag gültig und nullterminiert.
    unsafe { p.to_string().unwrap_or_default() }
}
