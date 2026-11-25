//! tracing-Layer für das Windows Event Log (Protokoll "Anwendung", Quelle "PTPMaster").
//!
//! Damit die Ereignisanzeige den Text ohne "Die Beschreibung für Ereignis-ID … wurde nicht
//! gefunden" zeigt, registriert `ptpmaster install` die Quelle mit der .NET-Nachrichtendatei
//! EventLogMessages.dll, die für jede Ereignis-ID nur "%1" enthält. Auch ohne Registrierung
//! landen die Meldungen im Log, dann mit diesem Hinweistext davor.

use std::fmt::Write as _;

use tracing::field::{Field, Visit};
use tracing::{Event, Level, Subscriber};
use tracing_subscriber::Layer;
use tracing_subscriber::layer::Context;
use windows::Win32::Foundation::HANDLE;
use windows::Win32::System::EventLog::{
    DeregisterEventSource, EVENTLOG_ERROR_TYPE, EVENTLOG_INFORMATION_TYPE, EVENTLOG_WARNING_TYPE,
    RegisterEventSourceW, ReportEventW,
};
use windows::Win32::System::Registry::{
    HKEY, HKEY_LOCAL_MACHINE, KEY_WRITE, REG_DWORD, REG_EXPAND_SZ, REG_OPTION_NON_VOLATILE,
    RegCloseKey, RegCreateKeyExW, RegDeleteTreeW, RegSetValueExW,
};
use windows::core::{HSTRING, PCWSTR};

pub const SOURCE_NAME: &str = "PTPMaster";
const REG_PATH: &str = r"SYSTEM\CurrentControlSet\Services\EventLog\Application\PTPMaster";
const MESSAGE_FILE: &str =
    r"%SystemRoot%\Microsoft.NET\Framework64\v4.0.30319\EventLogMessages.dll";

pub struct EventLogLayer {
    handle: HANDLE,
}

// Das Handle aus RegisterEventSourceW darf threadübergreifend benutzt werden.
unsafe impl Send for EventLogLayer {}
unsafe impl Sync for EventLogLayer {}

impl EventLogLayer {
    pub fn new() -> windows::core::Result<Self> {
        let name = HSTRING::from(SOURCE_NAME);
        // SAFETY: lokaler Rechner, gültiger Quellname.
        let handle = unsafe { RegisterEventSourceW(PCWSTR::null(), PCWSTR(name.as_ptr()))? };
        Ok(Self { handle })
    }
}

impl Drop for EventLogLayer {
    fn drop(&mut self) {
        // SAFETY: Handle stammt aus RegisterEventSourceW.
        let _ = unsafe { DeregisterEventSource(self.handle) };
    }
}

struct MessageVisitor(String);

impl Visit for MessageVisitor {
    fn record_debug(&mut self, field: &Field, value: &dyn std::fmt::Debug) {
        if field.name() == "message" {
            let _ = write!(self.0, "{value:?}");
        } else {
            let _ = write!(self.0, " {}={value:?}", field.name());
        }
    }
    fn record_str(&mut self, field: &Field, value: &str) {
        if field.name() == "message" {
            self.0.push_str(value);
        } else {
            let _ = write!(self.0, " {}={value}", field.name());
        }
    }
}

impl<S: Subscriber> Layer<S> for EventLogLayer {
    fn on_event(&self, event: &Event<'_>, _ctx: Context<'_, S>) {
        let mut v = MessageVisitor(String::new());
        event.record(&mut v);
        let (ty, id) = match *event.metadata().level() {
            Level::ERROR => (EVENTLOG_ERROR_TYPE, 3),
            Level::WARN => (EVENTLOG_WARNING_TYPE, 2),
            _ => (EVENTLOG_INFORMATION_TYPE, 1),
        };
        let text = HSTRING::from(v.0);
        let strings = [PCWSTR(text.as_ptr())];
        // SAFETY: Handle gültig; ein String-Zeiger, der bis zum Ende des Aufrufs lebt.
        let _ = unsafe { ReportEventW(self.handle, ty, 0, id, None, 0, Some(&strings), None) };
    }
}

/// Quelle in der Registry eintragen (braucht Adminrechte).
pub fn register_source() -> Result<(), String> {
    let path = HSTRING::from(REG_PATH);
    let mut key = HKEY::default();
    // SAFETY: gültige Zeiger; Schlüssel wird unten geschlossen.
    unsafe {
        let rc = RegCreateKeyExW(
            HKEY_LOCAL_MACHINE,
            PCWSTR(path.as_ptr()),
            None,
            PCWSTR::null(),
            REG_OPTION_NON_VOLATILE,
            KEY_WRITE,
            None,
            &mut key,
            None,
        );
        if rc.is_err() {
            return Err(format!("Registry-Schlüssel nicht anlegbar: {rc:?}"));
        }
        let file: Vec<u8> = MESSAGE_FILE
            .encode_utf16()
            .chain(std::iter::once(0))
            .flat_map(u16::to_le_bytes)
            .collect();
        let name = HSTRING::from("EventMessageFile");
        let r1 = RegSetValueExW(key, PCWSTR(name.as_ptr()), None, REG_EXPAND_SZ, Some(&file));
        let types: u32 = 7; // Fehler | Warnung | Information
        let name = HSTRING::from("TypesSupported");
        let r2 = RegSetValueExW(
            key,
            PCWSTR(name.as_ptr()),
            None,
            REG_DWORD,
            Some(&types.to_le_bytes()),
        );
        let _ = RegCloseKey(key);
        if r1.is_err() || r2.is_err() {
            return Err(format!("Registry-Werte nicht schreibbar: {r1:?} {r2:?}"));
        }
    }
    Ok(())
}

pub fn unregister_source() {
    let path = HSTRING::from(REG_PATH);
    // SAFETY: löscht nur unseren eigenen Schlüssel.
    let _ = unsafe { RegDeleteTreeW(HKEY_LOCAL_MACHINE, PCWSTR(path.as_ptr())) };
}
