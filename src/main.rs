use std::path::PathBuf;

use anyhow::{Result, bail};
use clap::{Parser, Subcommand};

use ptpmaster::config::{self, Config, Overrides};
use ptpmaster::logging::{self, LogOptions, console};
use ptpmaster::master::Master;
use ptpmaster::probe::{self, ProbeOptions};
use ptpmaster::{iface, net, shutdown};
use tracing::info;

#[derive(Parser)]
#[command(
    name = "ptpmaster",
    version,
    about = "PTP-Grandmaster (IEEE 1588-2008, PTPv2, UDP/IPv4) mit der Systemuhr als Zeitquelle"
)]
struct Cli {
    /// Konfigurationsdatei (Standard: ptpmaster.toml neben der exe)
    #[arg(long, short = 'c', global = true)]
    config: Option<PathBuf>,

    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Master in der Konsole starten (mit Live-Status)
    Run {
        #[command(flatten)]
        overrides: Overrides,
    },
    /// Als Windows-Dienst installieren (Adminrechte nötig)
    Install {
        /// Dienst nach der Installation gleich starten
        #[arg(long)]
        start: bool,
        #[command(flatten)]
        overrides: Overrides,
    },
    /// Windows-Dienst entfernen (Adminrechte nötig)
    Uninstall,
    /// Netzwerkkarten mit Timestamping-Fähigkeiten auflisten
    Interfaces,
    /// Messmodus: Offset und Path Delay zum Master anzeigen (nur zum Testen)
    Probe {
        /// Nach so vielen Sekunden beenden (Standard: bis Ctrl+C)
        #[arg(long)]
        duration: Option<u64>,
        /// Delay_Req-Intervall als log2 Sekunden (Standard: aus dem Delay_Resp des Masters)
        #[arg(long, allow_negative_numbers = true)]
        delay_req_interval: Option<i8>,
        #[command(flatten)]
        overrides: Overrides,
    },
    /// Wird vom Dienststeuerungs-Manager aufgerufen
    #[command(hide = true)]
    Service {
        #[command(flatten)]
        overrides: Overrides,
    },
}

fn main() {
    let cli = Cli::parse();
    if let Err(e) = real_main(cli) {
        ptpmaster::logging::console::clear_status();
        eprintln!("Fehler: {e:#}");
        std::process::exit(1);
    }
}

fn real_main(cli: Cli) -> Result<()> {
    match cli.command {
        Command::Interfaces => cmd_interfaces(cli.config.as_deref()),
        Command::Run { overrides } => cmd_run(cli.config.as_deref(), &overrides),
        Command::Probe {
            duration,
            delay_req_interval,
            overrides,
        } => cmd_probe(
            cli.config.as_deref(),
            &overrides,
            duration,
            delay_req_interval,
        ),
        Command::Install { start, overrides } => {
            cmd_install(cli.config.as_deref(), &overrides, start)
        }
        Command::Uninstall => cmd_uninstall(),
        Command::Service { overrides } => cmd_service(cli.config, overrides),
    }
}

#[cfg(windows)]
fn cmd_install(
    config_path: Option<&std::path::Path>,
    overrides: &Overrides,
    start: bool,
) -> Result<()> {
    // Konfiguration vorab prüfen, damit der Dienst nicht erst beim Start scheitert.
    let (cfg, used) = config::load(config_path, overrides)?;
    match &used {
        Some(p) => println!("Konfiguration: {}", p.display()),
        None => println!(
            "Keine Konfigurationsdatei – der Dienst nutzt Profil '{}' (Datei anlegen: {})",
            cfg.profile,
            config::default_config_path().display()
        ),
    }
    ptpmaster::win::service::install(config_path, &overrides.to_args(), start)
}

#[cfg(windows)]
fn cmd_uninstall() -> Result<()> {
    ptpmaster::win::service::uninstall()
}

#[cfg(windows)]
fn cmd_service(config_path: Option<PathBuf>, overrides: Overrides) -> Result<()> {
    ptpmaster::win::service::run_dispatcher(Box::new(move |stop| {
        service_body(config_path.as_deref(), &overrides, stop)
    }))
}

/// Dienstbetrieb: Logdatei + Event Log; bei Startfehlern (z. B. Netzwerk beim Booten noch nicht
/// bereit) alle 10 s neu versuchen.
#[cfg(windows)]
fn service_body(
    config_path: Option<&std::path::Path>,
    overrides: &Overrides,
    stop: std::sync::Arc<std::sync::atomic::AtomicBool>,
) -> Result<()> {
    use std::sync::atomic::Ordering;
    use std::time::Duration;
    use tracing::error;

    let (cfg, used) = match config::load(config_path, overrides) {
        Ok(c) => c,
        Err(e) => {
            let _guard = logging::init(LogOptions {
                level: "info".into(),
                console: false,
                file: None,
                eventlog: true,
            });
            error!("Konfiguration fehlerhaft: {e:#}");
            return Err(e);
        }
    };
    let _guard = logging::init(log_options(&cfg, false, true))?;
    log_startup(&cfg, used.as_deref());
    info!("Betrieb als Windows-Dienst");
    let mut attempt = 0u32;
    while !stop.load(Ordering::SeqCst) {
        match Master::start(cfg.clone(), stop.clone()) {
            Ok(master) => {
                let mut ticks = 0u64;
                master.run(|s| {
                    ticks += 1;
                    if ticks.is_multiple_of(300) {
                        info!(target: logging::STATUS_TARGET, "{}", s.line());
                    }
                })?;
            }
            Err(e) => {
                attempt += 1;
                if attempt == 1 || attempt.is_multiple_of(30) {
                    error!(
                        "Start fehlgeschlagen (Versuch {attempt}): {e:#} – neuer Versuch alle 10 s"
                    );
                }
                for _ in 0..100 {
                    if stop.load(Ordering::SeqCst) {
                        break;
                    }
                    std::thread::sleep(Duration::from_millis(100));
                }
            }
        }
    }
    info!("Dienst beendet");
    Ok(())
}

#[cfg(not(windows))]
fn cmd_install(_: Option<&std::path::Path>, _: &Overrides, _: bool) -> Result<()> {
    bail!("Der Windows-Dienst ist nur unter Windows verfügbar")
}

#[cfg(not(windows))]
fn cmd_uninstall() -> Result<()> {
    bail!("Der Windows-Dienst ist nur unter Windows verfügbar")
}

#[cfg(not(windows))]
fn cmd_service(_: Option<PathBuf>, _: Overrides) -> Result<()> {
    bail!("Der Windows-Dienst ist nur unter Windows verfügbar")
}

/// Startmeldungen, die für jeden Betriebsmodus gleich sind.
pub fn log_startup(cfg: &Config, used: Option<&std::path::Path>) {
    info!("ptpmaster {} startet", env!("CARGO_PKG_VERSION"));
    match used {
        Some(p) => info!("Konfiguration: {}", p.display()),
        None => info!(
            "Keine Konfigurationsdatei gefunden ({}), verwende Profil '{}'",
            config::default_config_path().display(),
            cfg.profile
        ),
    }
    #[cfg(windows)]
    info!(
        "{} – Winsock-Timestamping-API {}",
        ptpmaster::win::version_string(),
        if ptpmaster::win::tscaps::api_available() {
            "vorhanden"
        } else {
            "fehlt (ab Windows 10 Build 20348 / Windows 11)"
        }
    );
}

fn log_options(cfg: &Config, console: bool, eventlog: bool) -> LogOptions {
    LogOptions {
        level: cfg.log_level.clone(),
        console,
        file: cfg
            .log_file
            .then(|| (cfg.absolute_log_dir(), cfg.log_max_files)),
        eventlog,
    }
}

fn cmd_run(config_path: Option<&std::path::Path>, overrides: &Overrides) -> Result<()> {
    let (cfg, used) = config::load(config_path, overrides)?;
    let _guard = logging::init(log_options(&cfg, true, false))?;
    log_startup(&cfg, used.as_deref());
    let stop = shutdown::install_handler();
    let master = Master::start(cfg, stop)?;
    let interactive = console::is_interactive();
    let mut ticks = 0u64;
    let result = master.run(|s| {
        ticks += 1;
        if interactive {
            console::set_status(s.line());
        }
        // In die Logdatei: jede Minute (interaktiv) bzw. alle 10 s (ohne Konsole)
        if ticks.is_multiple_of(if interactive { 60 } else { 10 }) {
            info!(target: logging::STATUS_TARGET, "{}", s.line());
        }
    });
    console::clear_status();
    result
}

fn cmd_probe(
    config_path: Option<&std::path::Path>,
    overrides: &Overrides,
    duration: Option<u64>,
    delay_req_interval: Option<i8>,
) -> Result<()> {
    let (cfg, _) = config::load(config_path, overrides)?;
    let _guard = logging::init(LogOptions {
        level: std::env::var("RUST_LOG").unwrap_or_else(|_| "warn".into()),
        console: true,
        file: None,
        eventlog: false,
    })?;
    let stop = shutdown::install_handler();
    let report = probe::run(
        &cfg,
        &ProbeOptions {
            duration: duration.map(std::time::Duration::from_secs),
            delay_req_log_interval: delay_req_interval,
            print: true,
        },
        stop,
    )?;
    if report.master.is_none() {
        bail!(
            "Kein Master in Domain {} gefunden. Firewall (scripts/firewall.ps1), Interface und Domain prüfen.",
            cfg.domain
        );
    }
    Ok(())
}

fn cmd_interfaces(config_path: Option<&std::path::Path>) -> Result<()> {
    let (cfg, _) = config::load(config_path, &Overrides::default())
        .unwrap_or_else(|_| (config::Config::from_profile(Default::default()), None));
    #[cfg(windows)]
    println!(
        "{} – Winsock-Timestamping-API: {}\n",
        ptpmaster::win::version_string(),
        if ptpmaster::win::tscaps::api_available() {
            "vorhanden"
        } else {
            "fehlt (erst ab Windows 10 Build 20348 / Windows 11 / Server 2022)"
        }
    );
    let list = iface::list()?;
    let default = iface::select(&list, cfg.interface.as_deref()).ok();
    for (n, i) in list.iter().enumerate() {
        let chosen = default.as_ref().is_some_and(|(d, _)| d.id == i.id);
        println!(
            "[{}] {}{}{}",
            n + 1,
            i.name,
            if i.description.is_empty() {
                String::new()
            } else {
                format!("  ({})", i.description)
            },
            if chosen { "   ← wird verwendet" } else { "" }
        );
        let (id, real) = net::derive_clock_identity(i);
        println!(
            "    MAC {}  → clockIdentity {}{}",
            i.mac_string(),
            id,
            if real { "" } else { " (ohne MAC abgeleitet)" }
        );
        println!(
            "    IPv4 {}   {}{}{}",
            i.ipv4
                .iter()
                .map(|a| a.to_string())
                .collect::<Vec<_>>()
                .join(", "),
            if i.up { "verbunden" } else { "nicht verbunden" },
            if i.has_gateway { ", Gateway" } else { "" },
            if i.loopback { ", Loopback" } else { "" }
        );
        let active = net::active_caps(i);
        match &active {
            Ok(c) => println!("    Timestamping aktiv:   {}", c.describe()),
            Err(e) => println!("    Timestamping aktiv:   {e}"),
        }
        #[cfg(windows)]
        if i.luid != 0 {
            match ptpmaster::win::tscaps::supported(i.luid) {
                Ok(c) => println!("    Timestamping möglich: {}", c.describe()),
                Err(e) => println!("    Timestamping möglich: {e}"),
            }
        }
        match net::choose_mode(cfg.timestamping, &active) {
            Ok((m, _)) => println!("    Modus bei \"{}\": {m}", cfg.timestamping),
            Err(e) => println!(
                "    Modus bei \"{}\": nicht möglich – {e}",
                cfg.timestamping
            ),
        }
        println!();
    }
    if list.is_empty() {
        println!("Keine Interfaces mit IPv4-Adresse gefunden.");
    }
    Ok(())
}
