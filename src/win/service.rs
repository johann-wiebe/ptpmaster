//! Windows-Dienst: Installation, Entfernung und Dienst-Hauptfunktion.

use std::path::Path;
use std::sync::OnceLock;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use anyhow::{Result, bail};

use crate::cmdline::quote_arg;
use windows::Win32::Foundation::{
    ERROR_ACCESS_DENIED, ERROR_FAILED_SERVICE_CONTROLLER_CONNECT, ERROR_SERVICE_DOES_NOT_EXIST,
    ERROR_SERVICE_EXISTS, NO_ERROR,
};
use windows::Win32::System::Services::{
    ChangeServiceConfig2W, CloseServiceHandle, ControlService, CreateServiceW, DeleteService,
    OpenSCManagerW, OpenServiceW, QueryServiceStatus, RegisterServiceCtrlHandlerExW, SC_ACTION,
    SC_ACTION_RESTART, SC_HANDLE, SC_MANAGER_CONNECT, SC_MANAGER_CREATE_SERVICE,
    SERVICE_ACCEPT_SHUTDOWN, SERVICE_ACCEPT_STOP, SERVICE_ALL_ACCESS, SERVICE_AUTO_START,
    SERVICE_CONFIG_DESCRIPTION, SERVICE_CONFIG_FAILURE_ACTIONS, SERVICE_CONTROL_INTERROGATE,
    SERVICE_CONTROL_SHUTDOWN, SERVICE_CONTROL_STOP, SERVICE_DESCRIPTIONW, SERVICE_ERROR_NORMAL,
    SERVICE_FAILURE_ACTIONSW, SERVICE_QUERY_STATUS, SERVICE_RUNNING, SERVICE_START_PENDING,
    SERVICE_STATUS, SERVICE_STATUS_CURRENT_STATE, SERVICE_STATUS_HANDLE, SERVICE_STOP,
    SERVICE_STOP_PENDING, SERVICE_STOPPED, SERVICE_TABLE_ENTRYW, SERVICE_WIN32_OWN_PROCESS,
    SetServiceStatus, StartServiceCtrlDispatcherW, StartServiceW,
};
use windows::core::{HSTRING, PCWSTR, PWSTR};

pub const SERVICE_NAME: &str = "PTPMaster";
pub const DISPLAY_NAME: &str = "PTP Grandmaster (IEEE 1588)";
const DESCRIPTION: &str = "Stellt die Systemzeit dieses PCs per Precision Time Protocol (IEEE 1588-2008, PTPv2, UDP 319/320) im LAN bereit.";
const DELETE: u32 = 0x0001_0000;

type Runner = Box<dyn Fn(Arc<AtomicBool>) -> Result<()> + Send + Sync>;

static RUNNER: OnceLock<Runner> = OnceLock::new();
static STOP: OnceLock<Arc<AtomicBool>> = OnceLock::new();
static STATUS_HANDLE: AtomicUsize = AtomicUsize::new(0);
static CHECKPOINT: Mutex<u32> = Mutex::new(0);

struct Scm(SC_HANDLE);
impl Drop for Scm {
    fn drop(&mut self) {
        // SAFETY: Handle stammt aus OpenSCManagerW/OpenServiceW/CreateServiceW.
        let _ = unsafe { CloseServiceHandle(self.0) };
    }
}

fn access_hint(e: &windows::core::Error) -> String {
    if e.code() == ERROR_ACCESS_DENIED.to_hresult() {
        "Zugriff verweigert – bitte die Eingabeaufforderung/PowerShell als Administrator starten"
            .into()
    } else {
        e.message()
    }
}

pub fn install(config: Option<&Path>, extra_args: &[String], start: bool) -> Result<()> {
    let exe = std::env::current_exe()?;
    let mut cmd = quote_arg(&exe.to_string_lossy());
    cmd.push_str(" service");
    if let Some(c) = config {
        let abs = std::path::absolute(c)?;
        cmd.push_str(" --config ");
        cmd.push_str(&quote_arg(&abs.to_string_lossy()));
    }
    for a in extra_args {
        cmd.push(' ');
        cmd.push_str(&quote_arg(a));
    }

    // SAFETY: Aufrufe der Dienst-API mit gültigen, nullterminierten Strings.
    unsafe {
        let scm = Scm(
            OpenSCManagerW(PCWSTR::null(), PCWSTR::null(), SC_MANAGER_CREATE_SERVICE).map_err(
                |e| anyhow::anyhow!("Dienststeuerung nicht erreichbar: {}", access_hint(&e)),
            )?,
        );
        let name = HSTRING::from(SERVICE_NAME);
        let display = HSTRING::from(DISPLAY_NAME);
        let bin = HSTRING::from(cmd.as_str());
        let deps: Vec<u16> = "Tcpip\0\0".encode_utf16().collect();
        let svc = match CreateServiceW(
            scm.0,
            PCWSTR(name.as_ptr()),
            PCWSTR(display.as_ptr()),
            SERVICE_ALL_ACCESS,
            SERVICE_WIN32_OWN_PROCESS,
            SERVICE_AUTO_START,
            SERVICE_ERROR_NORMAL,
            PCWSTR(bin.as_ptr()),
            PCWSTR::null(),
            None,
            PCWSTR(deps.as_ptr()),
            PCWSTR::null(), // LocalSystem
            PCWSTR::null(),
        ) {
            Ok(h) => Scm(h),
            Err(e) if e.code() == ERROR_SERVICE_EXISTS.to_hresult() => {
                bail!(
                    "Der Dienst '{SERVICE_NAME}' ist bereits installiert. Erst 'ptpmaster uninstall' ausführen."
                )
            }
            Err(e) => bail!("Dienst nicht anlegbar: {}", access_hint(&e)),
        };

        let mut desc: Vec<u16> = DESCRIPTION.encode_utf16().chain([0]).collect();
        let d = SERVICE_DESCRIPTIONW {
            lpDescription: PWSTR(desc.as_mut_ptr()),
        };
        let _ = ChangeServiceConfig2W(
            svc.0,
            SERVICE_CONFIG_DESCRIPTION,
            Some((&d as *const SERVICE_DESCRIPTIONW).cast()),
        );
        // Bei Absturz nach 5 s neu starten (dreimal, Zähler nach einem Tag zurücksetzen)
        let mut actions = [SC_ACTION {
            Type: SC_ACTION_RESTART,
            Delay: 5000,
        }; 3];
        let fa = SERVICE_FAILURE_ACTIONSW {
            dwResetPeriod: 86_400,
            lpRebootMsg: PWSTR::null(),
            lpCommand: PWSTR::null(),
            cActions: actions.len() as u32,
            lpsaActions: actions.as_mut_ptr(),
        };
        let _ = ChangeServiceConfig2W(
            svc.0,
            SERVICE_CONFIG_FAILURE_ACTIONS,
            Some((&fa as *const SERVICE_FAILURE_ACTIONSW).cast()),
        );

        if let Err(e) = super::eventlog::register_source() {
            eprintln!("Warnung: Event-Log-Quelle nicht registriert: {e}");
        }
        println!("Dienst '{SERVICE_NAME}' installiert (Starttyp automatisch).");
        println!("Befehlszeile: {cmd}");
        if start {
            StartServiceW(svc.0, None)
                .map_err(|e| anyhow::anyhow!("Dienst nicht startbar: {}", e.message()))?;
            println!("Dienst gestartet. Status: sc query {SERVICE_NAME}");
        } else {
            println!("Starten mit: sc start {SERVICE_NAME}   (oder ptpmaster install --start)");
        }
    }
    Ok(())
}

pub fn uninstall() -> Result<()> {
    // SAFETY: Aufrufe der Dienst-API mit gültigen Handles und Strings.
    unsafe {
        let scm = Scm(
            OpenSCManagerW(PCWSTR::null(), PCWSTR::null(), SC_MANAGER_CONNECT).map_err(|e| {
                anyhow::anyhow!("Dienststeuerung nicht erreichbar: {}", access_hint(&e))
            })?,
        );
        let name = HSTRING::from(SERVICE_NAME);
        let svc = match OpenServiceW(
            scm.0,
            PCWSTR(name.as_ptr()),
            SERVICE_STOP | SERVICE_QUERY_STATUS | DELETE,
        ) {
            Ok(h) => Scm(h),
            Err(e) if e.code() == ERROR_SERVICE_DOES_NOT_EXIST.to_hresult() => {
                super::eventlog::unregister_source();
                println!("Dienst '{SERVICE_NAME}' ist nicht installiert.");
                return Ok(());
            }
            Err(e) => bail!("Dienst nicht zu öffnen: {}", access_hint(&e)),
        };
        let mut st = SERVICE_STATUS::default();
        if QueryServiceStatus(svc.0, &mut st).is_ok() && st.dwCurrentState != SERVICE_STOPPED {
            let _ = ControlService(svc.0, SERVICE_CONTROL_STOP, &mut st);
            let deadline = Instant::now() + Duration::from_secs(15);
            while Instant::now() < deadline {
                if QueryServiceStatus(svc.0, &mut st).is_err()
                    || st.dwCurrentState == SERVICE_STOPPED
                {
                    break;
                }
                std::thread::sleep(Duration::from_millis(250));
            }
            println!("Dienst gestoppt.");
        }
        DeleteService(svc.0)
            .map_err(|e| anyhow::anyhow!("Dienst nicht löschbar: {}", access_hint(&e)))?;
    }
    super::eventlog::unregister_source();
    println!("Dienst '{SERVICE_NAME}' entfernt.");
    Ok(())
}

fn set_status(state: SERVICE_STATUS_CURRENT_STATE, exit_code: u32, wait_hint_ms: u32) {
    let h = STATUS_HANDLE.load(Ordering::SeqCst);
    if h == 0 {
        return;
    }
    let accepted = if state == SERVICE_RUNNING {
        SERVICE_ACCEPT_STOP | SERVICE_ACCEPT_SHUTDOWN
    } else {
        0
    };
    let checkpoint = if state == SERVICE_RUNNING || state == SERVICE_STOPPED {
        0
    } else {
        CHECKPOINT
            .lock()
            .map(|mut c| {
                *c += 1;
                *c
            })
            .unwrap_or(1)
    };
    let st = SERVICE_STATUS {
        dwServiceType: SERVICE_WIN32_OWN_PROCESS,
        dwCurrentState: state,
        dwControlsAccepted: accepted,
        dwWin32ExitCode: exit_code,
        dwServiceSpecificExitCode: 0,
        dwCheckPoint: checkpoint,
        dwWaitHint: wait_hint_ms,
    };
    // SAFETY: Handle stammt aus RegisterServiceCtrlHandlerExW.
    let _ = unsafe { SetServiceStatus(SERVICE_STATUS_HANDLE(h as *mut _), &st) };
}

unsafe extern "system" fn control_handler(
    control: u32,
    _event_type: u32,
    _event_data: *mut core::ffi::c_void,
    _context: *mut core::ffi::c_void,
) -> u32 {
    match control {
        SERVICE_CONTROL_STOP | SERVICE_CONTROL_SHUTDOWN => {
            set_status(SERVICE_STOP_PENDING, 0, 5000);
            if let Some(s) = STOP.get() {
                s.store(true, Ordering::SeqCst);
            }
            NO_ERROR.0
        }
        SERVICE_CONTROL_INTERROGATE => NO_ERROR.0,
        _ => 120, // ERROR_CALL_NOT_IMPLEMENTED
    }
}

unsafe extern "system" fn service_main(_argc: u32, _argv: *mut PWSTR) {
    let stop = STOP
        .get_or_init(|| Arc::new(AtomicBool::new(false)))
        .clone();
    let name = HSTRING::from(SERVICE_NAME);
    // SAFETY: Handler ist eine statische Funktion.
    let handle = match unsafe {
        RegisterServiceCtrlHandlerExW(PCWSTR(name.as_ptr()), Some(control_handler), None)
    } {
        Ok(h) => h,
        Err(_) => return,
    };
    STATUS_HANDLE.store(handle.0 as usize, Ordering::SeqCst);
    set_status(SERVICE_START_PENDING, 0, 10_000);
    set_status(SERVICE_RUNNING, 0, 0);
    let result = match RUNNER.get() {
        Some(run) => run(stop),
        None => Err(anyhow::anyhow!("kein Dienst-Runner gesetzt")),
    };
    let code = match &result {
        Ok(()) => 0,
        Err(e) => {
            tracing::error!("Dienst beendet mit Fehler: {e:#}");
            1066 // ERROR_SERVICE_SPECIFIC_ERROR
        }
    };
    set_status(SERVICE_STOPPED, code, 0);
}

/// Vom Dienststeuerungs-Manager aufgerufen (`ptpmaster service`). Blockiert bis zum Dienstende.
pub fn run_dispatcher(runner: Runner) -> Result<()> {
    let _ = RUNNER.set(runner);
    let mut name: Vec<u16> = SERVICE_NAME.encode_utf16().chain([0]).collect();
    let table = [
        SERVICE_TABLE_ENTRYW {
            lpServiceName: PWSTR(name.as_mut_ptr()),
            lpServiceProc: Some(service_main),
        },
        SERVICE_TABLE_ENTRYW::default(),
    ];
    // SAFETY: Tabelle ist nullterminiert und lebt bis zum Ende des Aufrufs.
    match unsafe { StartServiceCtrlDispatcherW(table.as_ptr()) } {
        Ok(()) => Ok(()),
        Err(e) if e.code() == ERROR_FAILED_SERVICE_CONTROLLER_CONNECT.to_hresult() => bail!(
            "'ptpmaster service' ist für den Dienststeuerungs-Manager gedacht. In der Konsole 'ptpmaster run' verwenden."
        ),
        Err(e) => bail!("StartServiceCtrlDispatcher: {}", e.message()),
    }
}
