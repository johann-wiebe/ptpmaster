//! Logging mit `tracing`: Konsole (mit Live-Statuszeile), Datei mit täglicher Rotation
//! und im Dienstbetrieb zusätzlich das Windows Event Log.

use std::io::{IsTerminal, Write};
use std::path::PathBuf;
use std::sync::Mutex;

use anyhow::{Context, Result};
use tracing_subscriber::filter::EnvFilter;
use tracing_subscriber::fmt::MakeWriter;
use tracing_subscriber::layer::SubscriberExt;
use tracing_subscriber::util::SubscriberInitExt;
use tracing_subscriber::{Layer, Registry};

/// Target für periodische Statusmeldungen; gehen nicht ins Event Log.
pub const STATUS_TARGET: &str = "ptpmaster::status";

pub struct LogOptions {
    pub level: String,
    pub console: bool,
    pub file: Option<(PathBuf, usize)>,
    pub eventlog: bool,
}

/// Muss bis zum Programmende leben, damit die Logdatei vollständig geschrieben wird.
pub struct LogGuard {
    _file: Option<tracing_appender::non_blocking::WorkerGuard>,
}

pub fn init(opts: LogOptions) -> Result<LogGuard> {
    let make_filter = || {
        EnvFilter::try_from_default_env()
            .or_else(|_| EnvFilter::try_new(&opts.level))
            .unwrap_or_else(|_| EnvFilter::new("info"))
    };

    let mut layers: Vec<Box<dyn Layer<Registry> + Send + Sync>> = Vec::new();

    if opts.console {
        let ansi = console::enable_ansi();
        layers.push(
            tracing_subscriber::fmt::layer()
                .with_writer(console::ConsoleMakeWriter)
                .with_ansi(ansi)
                .with_target(false)
                .with_filter(make_filter())
                .boxed(),
        );
    }

    let mut file_guard = None;
    if let Some((dir, max_files)) = &opts.file {
        std::fs::create_dir_all(dir)
            .with_context(|| format!("Log-Verzeichnis {} nicht anlegbar", dir.display()))?;
        let appender = tracing_appender::rolling::Builder::new()
            .rotation(tracing_appender::rolling::Rotation::DAILY)
            .filename_prefix("ptpmaster")
            .filename_suffix("log")
            .max_log_files((*max_files).max(1))
            .build(dir)
            .context("Logdatei nicht anlegbar")?;
        let (writer, guard) = tracing_appender::non_blocking(appender);
        file_guard = Some(guard);
        layers.push(
            tracing_subscriber::fmt::layer()
                .with_writer(writer)
                .with_ansi(false)
                .with_filter(make_filter())
                .boxed(),
        );
    }

    #[cfg(windows)]
    if opts.eventlog {
        match crate::win::eventlog::EventLogLayer::new() {
            Ok(layer) => layers.push(
                layer
                    .with_filter(tracing_subscriber::filter::LevelFilter::INFO)
                    .with_filter(tracing_subscriber::filter::FilterFn::new(|m| {
                        m.target() != STATUS_TARGET
                    }))
                    .boxed(),
            ),
            Err(e) => eprintln!("Windows Event Log nicht verfügbar: {e}"),
        }
    }
    #[cfg(not(windows))]
    let _ = opts.eventlog;

    tracing_subscriber::registry()
        .with(layers)
        .try_init()
        .context("Logging bereits initialisiert")?;
    Ok(LogGuard { _file: file_guard })
}

/// Konsole: Logzeilen und eine Statuszeile, die unten stehen bleibt.
pub mod console {
    use super::*;

    struct State {
        status: Option<String>,
        interactive: bool,
    }

    static STATE: Mutex<State> = Mutex::new(State {
        status: None,
        interactive: false,
    });

    /// ANSI/VT-Ausgabe einschalten (Windows-Konsole). Liefert, ob die Konsole interaktiv ist.
    pub fn enable_ansi() -> bool {
        let interactive = std::io::stdout().is_terminal();
        #[cfg(windows)]
        let interactive = interactive && crate::win::enable_vt_mode();
        if let Ok(mut s) = STATE.lock() {
            s.interactive = interactive;
        }
        interactive
    }

    pub fn is_interactive() -> bool {
        STATE.lock().map(|s| s.interactive).unwrap_or(false)
    }

    /// Statuszeile setzen und neu zeichnen (nur auf interaktiver Konsole).
    pub fn set_status(line: String) {
        let Ok(mut s) = STATE.lock() else { return };
        if !s.interactive {
            return;
        }
        let mut out = std::io::stdout().lock();
        let _ = write!(out, "\r\x1b[2K{line}");
        let _ = out.flush();
        s.status = Some(line);
    }

    /// Statuszeile entfernen (z. B. beim Beenden).
    pub fn clear_status() {
        let Ok(mut s) = STATE.lock() else { return };
        if s.interactive && s.status.take().is_some() {
            let mut out = std::io::stdout().lock();
            let _ = write!(out, "\r\x1b[2K");
            let _ = out.flush();
        }
    }

    pub struct ConsoleMakeWriter;

    pub struct ConsoleWriter {
        buf: Vec<u8>,
    }

    impl<'a> MakeWriter<'a> for ConsoleMakeWriter {
        type Writer = ConsoleWriter;
        fn make_writer(&'a self) -> Self::Writer {
            ConsoleWriter {
                buf: Vec::with_capacity(256),
            }
        }
    }

    impl Write for ConsoleWriter {
        fn write(&mut self, data: &[u8]) -> std::io::Result<usize> {
            self.buf.extend_from_slice(data);
            Ok(data.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    impl Drop for ConsoleWriter {
        fn drop(&mut self) {
            if self.buf.is_empty() {
                return;
            }
            let state = STATE.lock();
            let mut out = std::io::stdout().lock();
            match &state {
                Ok(s) if s.interactive && s.status.is_some() => {
                    let _ = out.write_all(b"\r\x1b[2K");
                    let _ = out.write_all(&self.buf);
                    if let Some(st) = &s.status {
                        let _ = out.write_all(st.as_bytes());
                    }
                }
                _ => {
                    let _ = out.write_all(&self.buf);
                }
            }
            let _ = out.flush();
        }
    }
}
