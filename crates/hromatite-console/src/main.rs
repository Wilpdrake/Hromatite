//! Linux terminal driver and interactive loop. All UI decisions live in the
//! library, which is also what headless `--snapshot` exercises.

use hromatite_console::config::{Config, HELP};
use hromatite_console::demo;
use hromatite_console::fileio::{CommandWriter, LogTail};
use hromatite_console::input::KeyDecoder;
use hromatite_console::model::{Action, ConsoleModel};
use hromatite_console::render::{RenderContext, ansi_frame, render};
use std::fs::Metadata;
use std::io::{self, IsTerminal, Read, Write};
use std::os::raw::{c_int, c_ulong};
use std::time::{Duration, SystemTime};

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
        if config.snapshot {
            snapshot(demo::model(), &config);
            return Ok(());
        }
        return interactive(config, demo::model(), None);
    }
    let mut tail = LogTail::open(&config.log_path, true)?;
    let mut model = ConsoleModel::new(false);
    model.ingest(tail.poll()?);
    if config.snapshot {
        snapshot(model, &config);
        return Ok(());
    }
    interactive(config, model, Some(tail))
}

fn snapshot(model: ConsoleModel, config: &Config) {
    let frame = render(&model, &context(config), config.width, config.height);
    let mut out = io::stdout().lock();
    for line in &frame.lines {
        let _ = writeln!(out, "{line}");
    }
}

fn interactive(
    config: Config,
    mut model: ConsoleModel,
    mut tail: Option<LogTail>,
) -> io::Result<()> {
    if !io::stdin().is_terminal() || !io::stdout().is_terminal() {
        return Err(io::Error::new(
            io::ErrorKind::NotConnected,
            "interactive mode needs a TTY (or use --demo --snapshot)",
        ));
    }
    let _terminal = TerminalGuard::enter()?;
    let writer = CommandWriter::new(config.command_path.clone());
    let mut decoder = KeyDecoder::default();
    let mut input = [0u8; 256];
    let mut last_size = (0, 0);
    let mut stdout = io::stdout().lock();
    loop {
        if let Some(source) = tail.as_mut() {
            match source.poll() {
                Ok(lines) => model.ingest(lines),
                Err(error) => model.set_notice(format!("log read: {error}")),
            }
        }
        let (width, height) = terminal_size();
        if (width, height) != last_size {
            write!(stdout, "\x1b[2J")?;
            last_size = (width, height);
        }
        let frame = render(&model, &context(&config), width, height);
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
                    } else if let Err(error) = writer.send(&command) {
                        model.set_notice(format!("send failed: {error}"));
                    }
                }
                Action::Refresh => {
                    if config.demo {
                        model.set_notice("DEMO: telemetry is synthetic; nothing requested");
                    } else if let Err(error) = writer.send_batch(["pos", "who", "late", "gct"]) {
                        model.set_notice(format!("telemetry request failed: {error}"));
                    }
                }
            }
        }
    }
}

fn context(config: &Config) -> RenderContext {
    if config.demo {
        return RenderContext {
            log_path: "<built-in demo fixture>".into(),
            command_path: "<disabled in demo>".into(),
            log_bytes: Some(
                demo::DEMO_LINES
                    .iter()
                    .map(|line| line.len() as u64 + 1)
                    .sum(),
            ),
            log_age_secs: Some(0),
        };
    }
    let metadata = std::fs::metadata(&config.log_path).ok();
    RenderContext {
        log_path: config.log_path.display().to_string(),
        command_path: config.command_path.display().to_string(),
        log_bytes: metadata.as_ref().map(Metadata::len),
        log_age_secs: metadata.and_then(|metadata| modified_age(&metadata)),
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
        write!(stdout, "\x1b[?1049h\x1b[?25h\x1b[2J")?;
        stdout.flush()?;
        Ok(Self { original })
    }
}

impl Drop for TerminalGuard {
    fn drop(&mut self) {
        let mut stdout = io::stdout().lock();
        let _ = write!(stdout, "\x1b[0m\x1b[?25h\x1b[?1049l");
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
