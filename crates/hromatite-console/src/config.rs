//! Command-line parsing kept independent from terminal side effects.

use std::path::PathBuf;

pub const HELP: &str = "hromatite-console — host TUI over Hromatite's existing file channel

USAGE:
  hromatite-console [--prefix DIR] [--log FILE] [--demo] [--snapshot] [--size WxH]

  --prefix DIR   Proton prefix (default: $HROMATITE_PREFIX or Steam app 438100)
  --log FILE     Override the input log path
  --demo         Clearly marked synthetic fixture; never reads/writes live files
  --snapshot     Print one escape-free frame and exit (default 100x24)
  --size WxH     Snapshot dimensions, for example 120x32

KEYS:
  help or ? local field manual (no payload answer required)
  Enter send/confirm    Up/Down history    Tab complete    Ctrl-R or / filter
  F1 field manual       F2 follow          F3 errors+GC    F4 or : palette
  F5 refresh once       F6 auto telemetry  F7 reload       F8 clear view
  PgUp/PgDn scroll
  Esc back/cancel       Ctrl-C exit
  Palette searches names, syntax and descriptions. RISK commands require a
  second Enter; editing or Esc cancels the pending confirmation.
";

#[derive(Debug)]
pub struct Config {
    pub prefix: PathBuf,
    pub log_path: PathBuf,
    pub command_path: PathBuf,
    pub demo: bool,
    pub snapshot: bool,
    pub help: bool,
    pub width: u16,
    pub height: u16,
}

impl Config {
    pub fn parse(args: impl IntoIterator<Item = impl AsRef<str>>) -> Result<Self, String> {
        let home = std::env::var("HOME").unwrap_or_else(|_| ".".into());
        let default_prefix =
            PathBuf::from(home).join(".local/share/Steam/steamapps/compatdata/438100/pfx");
        let mut prefix = std::env::var_os("HROMATITE_PREFIX")
            .map(PathBuf::from)
            .unwrap_or(default_prefix);
        let mut log_path = None;
        let mut demo = false;
        let mut snapshot = false;
        let mut help = false;
        let mut width = 100;
        let mut height = 24;
        let mut args = args
            .into_iter()
            .map(|value| value.as_ref().to_string())
            .collect::<Vec<_>>()
            .into_iter();
        args.next();
        while let Some(value) = args.next() {
            match value.as_str() {
                "--prefix" => prefix = PathBuf::from(required(&mut args, "--prefix")?),
                "--log" => log_path = Some(PathBuf::from(required(&mut args, "--log")?)),
                "--demo" => demo = true,
                "--snapshot" => snapshot = true,
                "-h" | "--help" => help = true,
                "--size" => {
                    let size = required(&mut args, "--size")?;
                    (width, height) = parse_size(&size)?;
                }
                _ => return Err(format!("unknown option: {value} (try --help)")),
            }
        }
        let user_dir = prefix.join("drive_c/users/steamuser");
        Ok(Self {
            log_path: log_path.unwrap_or_else(|| user_dir.join("hromatite.log")),
            command_path: user_dir.join("hromatite-cmd.txt"),
            prefix,
            demo,
            snapshot,
            help,
            width,
            height,
        })
    }
}

fn required(args: &mut impl Iterator<Item = String>, option: &str) -> Result<String, String> {
    args.next().ok_or_else(|| format!("{option} needs a value"))
}

fn parse_size(size: &str) -> Result<(u16, u16), String> {
    let (width, height) = size
        .split_once('x')
        .ok_or_else(|| "size must be WxH".to_string())?;
    let width = width
        .parse::<u16>()
        .map_err(|_| "invalid width".to_string())?;
    let height = height
        .parse::<u16>()
        .map_err(|_| "invalid height".to_string())?;
    if width == 0 || height == 0 || width > 500 || height > 200 {
        return Err("size limits: width 1..=500, height 1..=200".into());
    }
    Ok((width, height))
}

#[cfg(test)]
mod tests {
    use super::Config;

    #[test]
    fn prefix_log_demo_snapshot_are_parsed() {
        let base = Config::parse(["console", "--prefix", "/pfx", "--demo", "--snapshot"]).unwrap();
        assert_eq!(
            base.log_path.to_string_lossy(),
            "/pfx/drive_c/users/steamuser/hromatite.log"
        );
        assert_eq!(
            base.command_path.to_string_lossy(),
            "/pfx/drive_c/users/steamuser/hromatite-cmd.txt"
        );
        assert!(base.demo && base.snapshot);
        let custom =
            Config::parse(["console", "--log", "/tmp/live.log", "--size", "72x19"]).unwrap();
        assert_eq!(custom.log_path.to_string_lossy(), "/tmp/live.log");
        assert_eq!((custom.width, custom.height), (72, 19));
    }

    #[test]
    fn bad_values_are_rejected_without_touching_terminal() {
        assert!(Config::parse(["console", "--prefix"]).is_err());
        assert!(Config::parse(["console", "--unknown"]).is_err());
        assert!(Config::parse(["console", "--size", "many"]).is_err());
    }
}
