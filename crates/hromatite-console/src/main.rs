//! Linux terminal driver and interactive loop. All UI decisions live in the
//! library, which is also what headless `--snapshot` exercises.

use hromatite_console::config::{Config, HELP};
use hromatite_console::demo;
use hromatite_console::fileio::CommandWriter;
use hromatite_console::input::KeyDecoder;
use hromatite_console::model::{Action, ConsoleModel};
use hromatite_console::render::{RenderContext, ansi_frame, body_rows, human_bytes, render};
use hromatite_console::sources::{self, SourceTail};
use hromatite_console::telemetry::AutoRefresh;
use std::fs::Metadata;
use std::io::{self, IsTerminal, Read, Write};
use std::os::raw::{c_int, c_ulong};
use std::time::{Duration, Instant, SystemTime};

fn main() {
    let config = Config::parse(std::env::args()).unwrap_or_else(|error| {
        eprintln!("hromatite-console: {error}");
        std::process::exit(2);
    });
    if config.help {
        print!("{HELP}");
        return;
    }
    if let Err(error) = run(config) {
        eprintln!("hromatite-console: {error}");
        std::process::exit(1);
    }
}

fn run(config: Config) -> io::Result<()> {
    if config.demo {
        let mut model = demo::model();
        if let Some(view) = &config.view {
            model.select_source(view);
        }
        if config.snapshot {
            snapshot(&model, &config, &[]);
            return Ok(());
        }
        return interactive(config, model, Vec::new());
    }
    let specs = sources::plan(&config);
    let labels: Vec<&'static str> = specs.iter().map(|spec| spec.label).collect();
    let mut tails: Vec<SourceTail> = specs
        .into_iter()
        .map(|spec| SourceTail::new(spec.pattern))
        .collect();
    let mut model = ConsoleModel::new(false);
    model.configure_sources(&labels);
    let mut failed = false;
    for (index, tail) in tails.iter_mut().enumerate() {
        match tail.poll() {
            Ok(polled) => model.ingest_replay(index, polled.lines),
            Err(error) => {
                failed = true;
                model.set_notice(format!("{}: {error}", labels[index]));
            }
        }
    }
    if !failed {
        match &config.view {
            Some(view) if model.select_source(view).is_some() => {}
            Some(view) => model.set_notice(format!(
                "no such log view: {view} — logs: {}",
                model.source_labels().join(" ")
            )),
            None => model.set_notice(format!(
                "following {} logs — F9/F10 switch, `logs` lists, F1 help",
                model.source_count()
            )),
        }
    }
    if config.snapshot {
        snapshot(&model, &config, &tails);
        return Ok(());
    }
    interactive(config, model, tails)
}

fn snapshot(model: &ConsoleModel, config: &Config, tails: &[SourceTail]) {
    let frame = render(
        model,
        &context(config, model, tails),
        config.width,
        config.height,
    );
    let mut out = io::stdout().lock();
    for line in &frame.lines {
        let _ = writeln!(out, "{line}");
    }
}

fn interactive(
    config: Config,
    mut model: ConsoleModel,
    mut tails: Vec<SourceTail>,
) -> io::Result<()> {
    if !io::stdin().is_terminal() || !io::stdout().is_terminal() {
        return Err(io::Error::new(
            io::ErrorKind::NotConnected,
            "interactive mode needs a TTY (or use --demo --snapshot)",
        ));
    }
    let _terminal = TerminalGuard::enter()?;
    let writer = CommandWriter::new(config.command_path.clone());
    let mut auto = AutoRefresh::new(Instant::now());
    let mut decoder = KeyDecoder::default();
    let mut input = [0u8; 256];
    let mut last_size = (0, 0);
    let mut last_active = model.active_source();
    let mut stdout = io::stdout().lock();
    loop {
        for (index, source) in tails.iter_mut().enumerate() {
            match source.poll() {
                Ok(polled) => {
                    if polled.retargeted {
                        let label = model.source_labels()[index];
                        model.set_notice(format!("{label}: new file {}", source.display()));
                        model.ingest_replay(index, polled.lines);
                    } else {
                        model.ingest_from(index, polled.lines);
                    }
                }
                Err(error) => model.set_notice(format!("log read: {error}")),
            }
        }
        if model.active_source() != last_active {
            last_active = model.active_source();
            let label = model.source_labels()[last_active];
            model.set_notice(describe_view(label, &file_state(&tails[last_active])));
        }
        let render_context = context(&config, &model, &tails);
        let was_suspended = auto.suspended();
        let automatic = auto.poll(Instant::now(), render_context.log_age_secs);
        model.set_auto_suspended(auto.suspended());
        if was_suspended != auto.suspended() {
            model.set_notice(if auto.suspended() {
                "auto telemetry paused: payload log is stale"
            } else {
                "auto telemetry resumed"
            });
        }
        if !automatic.is_empty() && !config.demo {
            match writer.send_batch(&automatic) {
                Ok(()) => {
                    for command in &automatic {
                        model.record_sent(command);
                    }
                }
                Err(error) => model.set_notice(format!("auto telemetry failed: {error}")),
            }
        }
        let (width, height) = terminal_size();
        model.set_viewport(body_rows(height as usize));
        if (width, height) != last_size {
            write!(stdout, "\x1b[2J")?;
            last_size = (width, height);
        }
        let frame = render(&model, &render_context, width, height);
        write!(stdout, "{}", ansi_frame(&frame))?;
        stdout.flush()?;

        let keys = match io::stdin().read(&mut input) {
            Ok(0) => decoder.timeout(),
            Ok(count) => decoder.feed(&input[..count]),
            Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
            Err(error) => return Err(error),
        };
        for key in keys {
            match model.handle(key) {
                Action::None => {}
                Action::Exit => return Ok(()),
                Action::Submit(command) => {
                    if config.demo {
                        model.set_notice(format!("DEMO: not sent: {command}"));
                    } else {
                        match writer.send(&command) {
                            Ok(()) => model.record_sent(&command),
                            Err(error) => {
                                model.set_notice(format!("send failed: {command}: {error}"))
                            }
                        }
                    }
                }
                Action::Refresh => {
                    if config.demo {
                        model.set_notice("DEMO: telemetry is synthetic; nothing requested");
                    } else {
                        match writer.send_batch(["pos", "who", "late", "gct"]) {
                            Ok(()) => {
                                for command in ["pos", "who", "late", "gct"] {
                                    model.record_sent(command);
                                }
                            }
                            Err(error) => {
                                model.set_notice(format!("telemetry request failed: {error}"))
                            }
                        }
                    }
                }
                Action::ToggleAuto => auto.set_enabled(model.auto_refresh(), Instant::now()),
            }
        }
    }
}

#[derive(Clone)]
struct FileState {
    path: String,
    bytes: Option<u64>,
    age: Option<u64>,
}

fn file_state(source: &SourceTail) -> FileState {
    let path = source.display();
    match source
        .resolved()
        .and_then(|path| std::fs::metadata(path).ok())
    {
        Some(metadata) => FileState {
            path,
            bytes: Some(metadata.len()),
            age: modified_age(&metadata),
        },
        None => FileState {
            path,
            bytes: None,
            age: None,
        },
    }
}

fn describe_view(label: &str, state: &FileState) -> String {
    let bytes = state
        .bytes
        .map(human_bytes)
        .unwrap_or_else(|| "no file".into());
    let age = state
        .age
        .map_or_else(|| "--".into(), |age| format!("{age}s ago"));
    format!("view {label}: {} — {bytes}, {age}", state.path)
}

fn context(config: &Config, model: &ConsoleModel, tails: &[SourceTail]) -> RenderContext {
    let view_label = model
        .source_labels()
        .get(model.active_source())
        .copied()
        .unwrap_or("payload");
    if config.demo {
        let demo_bytes = |lines: &[&str]| -> Option<u64> {
            Some(lines.iter().map(|line| line.len() as u64 + 1).sum())
        };
        let view_bytes = match view_label {
            "game" => demo_bytes(demo::DEMO_GAME_LINES),
            "eac" => demo_bytes(demo::DEMO_EAC_LINES),
            _ => demo_bytes(demo::DEMO_LINES),
        };
        return RenderContext {
            log_path: "<built-in demo fixture>".into(),
            command_path: "<disabled in demo>".into(),
            log_bytes: demo_bytes(demo::DEMO_LINES),
            log_age_secs: Some(0),
            view_path: format!("<built-in demo: {view_label}>"),
            view_bytes,
            view_age_secs: Some(0),
        };
    }
    // The payload log drives LINKED/STALE and the auto-telemetry gate; the view
    // is whatever log the operator switched to.
    let payload = tails.first().map(file_state).unwrap_or_else(|| FileState {
        path: config.log_path.display().to_string(),
        bytes: None,
        age: None,
    });
    let view = tails
        .get(model.active_source())
        .map(file_state)
        .unwrap_or_else(|| payload.clone());
    RenderContext {
        log_path: payload.path,
        command_path: config.command_path.display().to_string(),
        log_bytes: payload.bytes,
        log_age_secs: payload.age,
        view_path: view.path,
        view_bytes: view.bytes,
        view_age_secs: view.age,
    }
}

fn modified_age(metadata: &Metadata) -> Option<u64> {
    let modified = metadata.modified().ok()?;
    Some(
        SystemTime::now()
            .duration_since(modified)
            .unwrap_or(Duration::ZERO)
            .as_secs(),
    )
}

#[repr(C)]
#[derive(Clone, Copy)]
struct Termios {
    c_iflag: u32,
    c_oflag: u32,
    c_cflag: u32,
    c_lflag: u32,
    c_line: u8,
    c_cc: [u8; 32],
    c_ispeed: u32,
    c_ospeed: u32,
}

#[repr(C)]
struct WinSize {
    rows: u16,
    cols: u16,
    xpixel: u16,
    ypixel: u16,
}

unsafe extern "C" {
    fn tcgetattr(fd: c_int, termios: *mut Termios) -> c_int;
    fn tcsetattr(fd: c_int, action: c_int, termios: *const Termios) -> c_int;
    fn ioctl(fd: c_int, request: c_ulong, ...) -> c_int;
}

struct TerminalGuard {
    original: Termios,
}

impl TerminalGuard {
    fn enter() -> io::Result<Self> {
        let mut original = Termios {
            c_iflag: 0,
            c_oflag: 0,
            c_cflag: 0,
            c_lflag: 0,
            c_line: 0,
            c_cc: [0; 32],
            c_ispeed: 0,
            c_ospeed: 0,
        };
        // SAFETY: zeroed struct has the Linux termios layout and remains live;
        // fd 0 was verified to be a terminal immediately before this call.
        if unsafe { tcgetattr(0, &mut original) } != 0 {
            return Err(io::Error::last_os_error());
        }
        let mut raw = original;
        // cfmakeraw(3), written explicitly to avoid depending on libc crate.
        raw.c_iflag &= !(0x0001 | 0x0002 | 0x0010 | 0x0020 | 0x0040 | 0x0080 | 0x0100 | 0x0400);
        raw.c_oflag &= !0x0001;
        raw.c_lflag &= !(0x0001 | 0x0002 | 0x0008 | 0x8000);
        raw.c_cflag = (raw.c_cflag & !(0x0030 | 0x0100)) | 0x0030;
        raw.c_cc[6] = 0; // VMIN: return immediately when no bytes are ready
        raw.c_cc[5] = 1; // VTIME: wake every 100 ms to follow the log
        // SAFETY: raw was copied from this terminal's valid termios; TCSANOW=0.
        if unsafe { tcsetattr(0, 0, &raw) } != 0 {
            return Err(io::Error::last_os_error());
        }
        let mut stdout = io::stdout().lock();
        // 1049 alternate screen, cursor on. 1000+1006 turn on mouse reporting in
        // SGR form so the wheel can scroll the log; Shift+drag still selects text
        // in terminals that reserve plain selection for the application.
        write!(stdout, "\x1b[?1049h\x1b[?25h\x1b[?1000h\x1b[?1006h\x1b[2J")?;
        stdout.flush()?;
        Ok(Self { original })
    }
}

impl Drop for TerminalGuard {
    fn drop(&mut self) {
        let mut stdout = io::stdout().lock();
        // Release mouse reporting before leaving the alternate screen, otherwise
        // the operator's shell would keep receiving clicks as escape sequences.
        let _ = write!(stdout, "\x1b[?1006l\x1b[?1000l\x1b[0m\x1b[?25h\x1b[?1049l");
        let _ = stdout.flush();
        // SAFETY: original came from tcgetattr on this same fd and is still live.
        let _ = unsafe { tcsetattr(0, 0, &self.original) };
    }
}

fn terminal_size() -> (u16, u16) {
    let mut size = WinSize {
        rows: 0,
        cols: 0,
        xpixel: 0,
        ypixel: 0,
    };
    // SAFETY: TIOCGWINSZ=0x5413 writes exactly this repr(C) winsize to our
    // valid pointer. fd 1 is not modified by the call.
    let result = unsafe { ioctl(1, 0x5413, &mut size as *mut WinSize) };
    if result == 0 && size.rows > 0 && size.cols > 0 {
        (size.cols, size.rows)
    } else {
        (100, 30)
    }
}
