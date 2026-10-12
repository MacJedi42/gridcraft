//! GridCraft desktop app.
//!
//! Usage: `gridcraft [--control <port>] [--sample <name>] [files…]`
//!
//! `--control <port>` (or `GRIDCRAFT_CONTROL_PORT`) starts a localhost JSON-lines control
//! server: `{"id":1,"method":"ui.inspect","params":{}}` → `{"id":1,"ok":true,"result":…}`.
//! See `gridcraft_ui_egui::control` and `docs/control-protocol.md` for the methods.
#![cfg_attr(all(target_os = "windows", not(debug_assertions)), windows_subsystem = "windows")]
#![deny(clippy::unwrap_used, clippy::expect_used, clippy::panic, clippy::unimplemented, clippy::todo, clippy::unreachable)]

#[cfg(target_os = "macos")]
mod apple_events;
mod control_server;
#[cfg(any(target_os = "windows", test))]
mod graphics;
mod logging;
#[cfg(target_os = "macos")]
mod native_menu;

use std::cell::RefCell;
use std::rc::Rc;

use gridcraft_engine::Session;
use gridcraft_ui_egui::{Services, SheetApp};
use serde_json::json;

struct App(SheetApp, #[cfg(target_os = "macos")] Option<native_menu::NativeMenu>, #[cfg(target_os = "macos")] fmv_macos_events::Inbox);

impl eframe::App for App {
    fn logic(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        #[cfg(target_os = "macos")]
        {
            if self.1.is_none() && std::env::var_os("GRIDCRAFT_NO_NATIVE_MENU").is_none() {
                self.1 = Some(native_menu::NativeMenu::install(ctx));
            }
            if let Some(m) = &self.1 {
                m.poll(&mut self.0, ctx);
            }
            apple_events::poll(&self.2, &mut self.0, ctx);
        }
        self.0.logic(ctx);
    }
    fn raw_input_hook(&mut self, _ctx: &egui::Context, raw: &mut egui::RawInput) {
        self.0.raw_input_hook(raw);
        // Files dropped on the window open as workbooks.
        for f in std::mem::take(&mut raw.dropped_files) {
            if let Some(p) = f.path().to_str().map(str::to_string).filter(|p| !p.is_empty()) {
                self.0.open_path(&p);
            }
        }
    }
    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        self.0.ui(ui);
    }
    fn on_exit(&mut self) {
        save_prefs(&self.0);
        // macOS (#117): end the process here, before eframe tears the window down. AccessKit's
        // macOS adapter subclasses the window's content view and restores its class on teardown;
        // AppKit's Touch Bar support observes that view (KVO), so in the next display cycle
        // `-[_NSTouchBarFinderObservation invalidate]` throws "not registered" and the quit ends
        // in a crash report. eframe calls `on_exit` on every way of quitting (Quit/⌘Q, the Dock,
        // the close button, `app.quit`), and nothing after it is needed: the preferences are
        // saved and the log file is unbuffered. eframe does the same when it doesn't
        // run-and-return. `on_exit` only runs once the app exists, never on a failed start.
        #[cfg(target_os = "macos")]
        std::process::exit(0);
    }
}

fn config_dir() -> Option<std::path::PathBuf> {
    if cfg!(target_os = "macos") {
        std::env::var_os("HOME").map(|h| std::path::PathBuf::from(h).join("Library/Application Support/GridCraft"))
    } else if cfg!(windows) {
        std::env::var_os("APPDATA").map(|a| std::path::PathBuf::from(a).join("GridCraft"))
    } else {
        std::env::var_os("XDG_CONFIG_HOME")
            .map(std::path::PathBuf::from)
            .or_else(|| std::env::var_os("HOME").map(|h| std::path::PathBuf::from(h).join(".config")))
            .map(|c| c.join("gridcraft"))
    }
}

/// Runs without preferences (`GRIDCRAFT_NO_PREFS`, agents' test runs) neither read nor write
/// them, and keep no log file.
fn prefs_enabled() -> bool {
    std::env::var_os("GRIDCRAFT_NO_PREFS").is_none()
}

/// Where the log files live: `logs` in the settings folder, next to `ui.json` (see `logging`).
fn log_dir() -> Option<std::path::PathBuf> {
    Some(config_dir()?.join("logs"))
}

fn load_prefs(app: &mut SheetApp) {
    if !prefs_enabled() {
        return;
    }
    // First run (or a ui.json without a usable language): follow the desktop's language.
    app.ui.language = gridcraft_ui_egui::i18n::Language::system();
    // ...and its light or dark appearance (#239), until a theme is chosen and saved in ui.json.
    app.ui.system_theme = true;
    let Some(dir) = config_dir() else { return };
    if let Ok(b) = std::fs::read(dir.join("ui.json"))
        && let Ok(ui) = serde_json::from_slice::<gridcraft_ui_egui::UiState>(&b)
    {
        let saved = serde_json::from_slice(&b).ok().and_then(|v| gridcraft_ui_egui::i18n::saved_language(&v));
        let language = saved.unwrap_or(app.ui.language);
        app.ui = ui;
        app.ui.language = language;
    }
    if let Ok(b) = std::fs::read(dir.join("prefs.json"))
        && let Ok(p) = serde_json::from_slice(&b)
    {
        app.session.prefs = p;
    }
}

fn save_prefs(app: &SheetApp) {
    if !prefs_enabled() {
        return;
    }
    let Some(dir) = config_dir() else { return };
    let _ = std::fs::create_dir_all(&dir);
    if let Ok(b) = serde_json::to_vec_pretty(&app.ui) {
        let _ = std::fs::write(dir.join("ui.json"), b);
    }
    if let Ok(b) = serde_json::to_vec_pretty(&app.session.prefs) {
        let _ = std::fs::write(dir.join("prefs.json"), b);
    }
}

fn services() -> Services {
    // Keep the owner alive: X11/Wayland serve clipboard data from this handle.
    let clipboard: Rc<RefCell<Option<arboard::Clipboard>>> = Rc::default();
    let paste_clipboard = clipboard.clone();
    Services {
        copy_html: Some(Box::new(move |html, text| {
            let mut clipboard = clipboard.borrow_mut();
            if clipboard.is_none() {
                *clipboard = Some(arboard::Clipboard::new().map_err(|e| e.to_string())?);
            }
            clipboard.as_mut().ok_or("clipboard unavailable")?.set_html(html, Some(text)).map_err(|e| e.to_string())
        })),
        paste_html: Some(Box::new(move || {
            let mut clipboard = paste_clipboard.try_borrow_mut().ok()?;
            if clipboard.is_none() {
                *clipboard = arboard::Clipboard::new().ok();
            }
            clipboard.as_mut()?.get().html().ok()
        })),
        pick_open: Some(Box::new(|| {
            rfd::FileDialog::new()
                .add_filter("Spreadsheets", &["xlsx", "xlsm", "xls", "xlsb", "ods", "csv", "tsv", "txt", "json"])
                .add_filter("Excel Workbook", &["xlsx", "xlsm"])
                .add_filter("Excel 97-2003 Workbook (import)", &["xls"])
                .add_filter("Excel Binary Workbook (data import)", &["xlsb"])
                .add_filter("OpenDocument Spreadsheet (data import)", &["ods"])
                .add_filter("CSV", &["csv"])
                .pick_file()
                .and_then(|p| p.to_str().map(str::to_string))
        })),
        pick_picture: Some(Box::new(|| {
            rfd::FileDialog::new()
                .add_filter("Pictures (PNG, JPEG)", &["png", "jpg", "jpeg"])
                .pick_file()
                .and_then(|p| p.to_str().map(str::to_string))
        })),
        pick_save: Some(Box::new(|suggested: &str| {
            let path = std::path::Path::new(suggested);
            let stem = path.file_stem().and_then(|s| s.to_str()).unwrap_or("Book1").to_string();
            // The suggested name's type (an export asks for .pdf, .csv or .html) comes first.
            const TYPES: [(&str, &str); 4] =
                [("Excel Workbook (.xlsx)", "xlsx"), ("CSV UTF-8 (.csv)", "csv"), ("Web Page (.html)", "html"), ("PDF (.pdf)", "pdf")];
            let ext = path.extension().and_then(|e| e.to_str()).map(str::to_ascii_lowercase).filter(|e| TYPES.iter().any(|(_, x)| x == e));
            let ext = ext.unwrap_or_else(|| "xlsx".to_string());
            let mut dialog = rfd::FileDialog::new().set_file_name(format!("{stem}.{ext}"));
            for (label, x) in TYPES.iter().filter(|(_, x)| *x == ext).chain(TYPES.iter().filter(|(_, x)| *x != ext)) {
                dialog = dialog.add_filter(*label, &[*x]);
            }
            dialog.save_file().and_then(|p| p.to_str().map(str::to_string))
        })),
        open_url: Some(Box::new(|url: &str| open_with_system(url))),
        // File › Print, Ctrl+P: the print PDF opens in the system's PDF viewer, whose Print
        // command reaches the printer dialog (GridCraft has no printer driver of its own).
        print: Some(Box::new(|bytes: &[u8]| match write_print_pdf(&std::env::temp_dir(), bytes) {
            Ok(path) => open_with_system(&path.to_string_lossy()),
            Err(e) => log::warn!("print: {e}"),
        })),
        ..Default::default()
    }
}

/// Opens a URL or file with the system's default application.
fn open_with_system(target: &str) {
    let cmd = if cfg!(target_os = "macos") {
        "open"
    } else if cfg!(windows) {
        "explorer"
    } else {
        "xdg-open"
    };
    if let Err(e) = std::process::Command::new(cmd).arg(target).spawn() {
        log::warn!("{cmd} {target}: {e}");
    }
}

/// Writes a print PDF into `dir` under a fresh name (an earlier print may still be open in the
/// viewer) and returns its path. The temp folder is shared, so the file is created exclusively
/// (an existing file or a planted symlink is never followed or overwritten) and, on Unix, readable
/// only by the user.
fn write_print_pdf(dir: &std::path::Path, bytes: &[u8]) -> std::io::Result<std::path::PathBuf> {
    use std::io::Write;
    static PRINTS: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);
    for _ in 0..64 {
        let n = PRINTS.fetch_add(1, std::sync::atomic::Ordering::Relaxed).wrapping_add(1);
        let path = dir.join(format!("GridCraft print {}-{n}.pdf", std::process::id()));
        let mut options = std::fs::OpenOptions::new();
        options.write(true).create_new(true);
        #[cfg(unix)]
        std::os::unix::fs::OpenOptionsExt::mode(&mut options, 0o600);
        match options.open(&path) {
            Ok(mut file) => {
                file.write_all(bytes)?;
                return Ok(path);
            }
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => continue,
            Err(e) => return Err(e),
        }
    }
    Err(std::io::Error::new(std::io::ErrorKind::AlreadyExists, "no free print file name"))
}

fn icon() -> Option<egui::IconData> {
    #[cfg(target_os = "macos")]
    let bytes = include_bytes!("../../../assets/app-icon/gridcraft-macos-512.png");
    #[cfg(not(target_os = "macos"))]
    let bytes = include_bytes!("../../../assets/app-icon/hicolor/256x256/apps/ai.storyteller.gridcraft.png");
    let img = image::load_from_memory(bytes).ok()?.to_rgba8();
    Some(egui::IconData { width: img.width(), height: img.height(), rgba: img.into_raw() })
}

fn main() -> eframe::Result<()> {
    // First, so the panic hook and every start-up warning are recorded (`logging`).
    let logger = logging::install();
    // Commands catch their panics (`Session::execute`), but the default hook only prints to
    // standard error; this one sends every panic, caught or not, to the log file too, with a
    // backtrace when RUST_BACKTRACE is set.
    std::panic::set_hook(Box::new(|info| {
        let trace = std::backtrace::Backtrace::capture();
        let report = if trace.status() == std::backtrace::BacktraceStatus::Captured {
            format!("internal error: {info}\n{trace}")
        } else {
            format!("internal error: {info}")
        };
        // Standard error and the log file; standard error alone when RUST_LOG turned errors off.
        if log::log_enabled!(log::Level::Error) {
            log::error!("{report}");
        } else {
            // `eprintln!` panics on a broken stderr pipe, and a panic inside the panic hook aborts.
            let _ = std::io::Write::write_fmt(&mut std::io::stderr(), format_args!("gridcraft: {report}\n"));
        }
    }));
    let args: Vec<String> = std::env::args().skip(1).collect();
    if args.iter().any(|a| a == "--version" || a == "-V") {
        println!("GridCraft {}", env!("CARGO_PKG_VERSION"));
        return Ok(());
    }
    let mut control_port: Option<u16> = std::env::var("GRIDCRAFT_CONTROL_PORT").ok().and_then(|p| p.parse().ok());
    let mut sample: Option<String> = None;
    let mut files: Vec<String> = Vec::new();
    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            "--control" => {
                i += 1;
                control_port = args.get(i).and_then(|p| p.parse().ok());
            }
            "--sample" => {
                i += 1;
                sample = Some(args.get(i).cloned().unwrap_or_else(|| "sales".into()));
            }
            a if a.starts_with("-psn_") => {}
            a => files.push(a.to_string()),
        }
        i += 1;
    }
    // The log file lives in the settings folder, next to the preferences; opened after the
    // arguments, so `--version` leaves no file behind. Records logged until now are written to it
    // first. Runs without preferences (agents' test runs) log to standard error only, so they
    // don't rotate away the user's own logs.
    if let Some(logger) = logger {
        match log_dir().filter(|_| prefs_enabled()) {
            Some(dir) => match logger.attach_dir(&dir) {
                Ok(path) => log::info!("GridCraft {}, log file {}", env!("CARGO_PKG_VERSION"), path.display()),
                // Standard error only by now (`attach_dir` gave up on the file); unlike `eprintln!`, never panics.
                Err(e) => log::warn!("no log file: {e}"),
            },
            None => logger.no_file(),
        }
    }
    let mut session = Session::new();
    if let Some(s) = &sample
        && let Err(e) = session.execute("file.new", json!({"sample": s}))
    {
        log::warn!("sample {s}: {e}");
    }
    if session.documents().is_empty() {
        session.new_workbook();
    }
    let mut app = SheetApp::new(session, services());
    app.watch_desktop_theme();
    app.after_engine(); // Show warnings from the sample.
    let control_port = control_port;
    load_prefs(&mut app);
    // Files named on the command line (a double-click on Windows and Linux) open as File › Open
    // opens them: a password-protected one asks for its password, and errors are shown, not
    // only logged. Each replaces the untouched blank Book1, as a file opened from Finder does.
    for f in &files {
        app.open_path(f);
    }
    let mut viewport = egui::ViewportBuilder::default()
        .with_title("GridCraft")
        .with_inner_size([1440.0, 900.0])
        .with_min_inner_size([640.0, 420.0])
        .with_app_id("ai.storyteller.gridcraft")
        .with_drag_and_drop(true);
    if cfg!(target_os = "macos") {
        viewport = viewport.with_fullsize_content_view(true).with_titlebar_shown(false).with_title_shown(false);
    }
    if let Some(i) = icon() {
        viewport = viewport.with_icon(i);
    }
    let options = eframe::NativeOptions { viewport, ..Default::default() };
    // Before eframe creates the wgpu instance: Windows starts on DirectX 12, or OpenGL when
    // DirectX 12 has no GPU driver, and never touches Vulkan unless asked (see graphics.rs).
    #[cfg(target_os = "windows")]
    let backend_override = eframe::wgpu::Backends::from_env();
    #[cfg(target_os = "windows")]
    let (options, first_backends) = {
        let mut options = options;
        let backends = graphics::configure(&mut options, backend_override);
        (options, backends)
    };
    #[cfg(target_os = "windows")]
    let retry_options = options.clone();
    // Registered before the event loop starts, so it catches the Finder event that launched us as
    // well as later ones. Lives until the event loop returns; the app creator only borrows it.
    #[cfg(target_os = "macos")]
    let apple_events = apple_events::AppleEvents::install();
    #[cfg(target_os = "macos")]
    let apple_events = &apple_events;
    // The app waits here until a start gets as far as creating it; a start that fails before
    // (no graphics adapter, device or surface) leaves it here for the next attempt.
    let pending = std::cell::Cell::new(Some(app));
    let run = |options: eframe::NativeOptions| {
        eframe::run_native(
            "GridCraft",
            options,
            Box::new(|cc| {
                let Some(mut app) = pending.take() else {
                    return Err("the app was already created".into());
                };
                SheetApp::setup_context_for_language(&cc.egui_ctx, app.ui.dark, app.ui.language);
                if let Some(port) = control_port {
                    match control_server::start(port, cc.egui_ctx.clone()) {
                        Ok(rx) => app.control_rx = Some(rx),
                        Err(e) => log::error!("control channel on port {port}: {e}"),
                    }
                }
                Ok(Box::new(App(
                    app,
                    #[cfg(target_os = "macos")]
                    None,
                    #[cfg(target_os = "macos")]
                    apple_events.connect(&cc.egui_ctx),
                )))
            }),
        )
    };
    let result = run(options);
    // A graphics error before the app existed: start once more with the other backend (eframe
    // reuses its event loop for the second run). Errors after that, or from winit, are returned.
    #[cfg(target_os = "windows")]
    if let Err(eframe::Error::Wgpu(e)) = &result
        && let Some(app) = pending.take()
        && let Some(backends) = graphics::fallback_backends(backend_override, first_backends)
    {
        log::error!("graphics: the renderer didn't start on {first_backends:?} ({e}); trying {backends:?}");
        pending.set(Some(app));
        let mut options = retry_options;
        graphics::configure_backends(&mut options, backends);
        return run(options);
    }
    result
}

#[cfg(test)]
mod print_tests {
    #[test]
    fn print_pdfs_get_fresh_names_in_the_given_folder() {
        let dir = std::env::temp_dir().join(format!("gridcraft-print-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let a = super::write_print_pdf(&dir, b"%PDF-1.4 a").unwrap();
        let b = super::write_print_pdf(&dir, b"%PDF-1.4 b").unwrap();
        assert_ne!(a, b);
        assert!(a.starts_with(&dir) && a.extension().is_some_and(|e| e == "pdf"));
        assert_eq!(std::fs::read(&b).unwrap(), b"%PDF-1.4 b");
        assert!(super::write_print_pdf(&dir.join("missing"), b"x").is_err());
        // A file (or symlink) already at the next name is skipped, never overwritten.
        let pid = std::process::id();
        let taken: Vec<_> = (1..=200).map(|n| dir.join(format!("GridCraft print {pid}-{n}.pdf"))).filter(|p| !p.exists()).take(3).collect();
        for p in &taken {
            std::fs::write(p, b"keep").unwrap();
        }
        let c = super::write_print_pdf(&dir, b"%PDF-1.4 c").unwrap();
        assert!(taken.iter().all(|p| std::fs::read(p).unwrap() == b"keep"));
        assert_eq!(std::fs::read(&c).unwrap(), b"%PDF-1.4 c");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(std::fs::metadata(&c).unwrap().permissions().mode() & 0o077, 0, "only the user can read it");
        }
        let _ = std::fs::remove_dir_all(&dir);
    }
}
