//! Command-line options, modeled on gpu-screen-recorder's: single-dash flags (`-w`, `-f`, `-a`,
//! `-r`, `-k`, `-bm`, `-q`, `-o`, ...), so people who know it on Linux feel at home.

use std::path::PathBuf;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Codec {
    H264,
    Hevc,
    Av1,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum BitrateMode {
    Cbr,
    Vbr,
    Qp,
}

#[derive(Clone, Debug, PartialEq)]
pub enum Quality {
    Preset(&'static str),
    /// kbit/s for cbr and vbr, 0 to 100 for qp.
    Number(u32),
}

#[derive(Clone, Debug, PartialEq)]
pub enum Window {
    Screen(Option<usize>),
    Focused,
    /// Part of a window title, matched without case.
    Title(String),
    Handle(isize),
}

#[derive(Clone, Debug, PartialEq)]
pub struct Options {
    pub window: Window,
    pub fps: u32,
    pub codec: Codec,
    pub bitrate_mode: BitrateMode,
    pub quality: Quality,
    pub audio: Vec<String>,
    /// Replay buffer length in seconds; `None` records straight to a file.
    pub replay: Option<u32>,
    pub output: Option<PathBuf>,
    pub size: Option<(u32, u32)>,
    pub cursor: bool,
    pub cfr: bool,
    pub dxgi: bool,
    pub gop_seconds: f64,
    pub ram_limit_mb: Option<u32>,
    pub hotkey_save: String,
    pub hotkey_record: String,
    /// Stop a recording after this many seconds.
    pub duration: Option<f64>,
    pub adapter: Option<u32>,
    pub verbose: bool,
}

impl Default for Options {
    fn default() -> Self {
        Options {
            window: Window::Screen(None),
            fps: 60,
            codec: Codec::H264,
            bitrate_mode: BitrateMode::Vbr,
            quality: Quality::Preset("very_high"),
            audio: Vec::new(),
            replay: None,
            output: None,
            size: None,
            cursor: true,
            cfr: true,
            dxgi: false,
            gop_seconds: 1.0,
            ram_limit_mb: None,
            hotkey_save: "ctrl+alt+f10".into(),
            hotkey_record: "ctrl+alt+f9".into(),
            duration: None,
            adapter: None,
            verbose: false,
        }
    }
}

#[derive(Clone, Debug, PartialEq)]
pub enum Command {
    Run(Options),
    /// Tell a running instance to save, start/stop recording, or quit.
    Control(&'static str),
    ListCaptureOptions,
    ListAudio,
    ListEncoders,
    Help,
    Version,
}

const PRESETS: [&str; 5] = ["low", "medium", "high", "very_high", "ultra"];

/// Bits per pixel per frame of each quality preset (H.264; HEVC and AV1 get 70% of it).
pub fn preset_bpp(p: &str) -> f64 {
    match p {
        "low" => 0.04,
        "medium" => 0.06,
        "high" => 0.09,
        "very_high" => 0.12,
        _ => 0.18,
    }
}

/// The bitrate a preset gives for a size and frame rate, in bit/s.
pub fn preset_bitrate(p: &str, codec: Codec, w: u32, h: u32, fps: u32) -> u32 {
    let k = if codec == Codec::H264 { 1.0 } else { 0.7 };
    ((w as f64 * h as f64 * fps as f64 * preset_bpp(p) * k) as u64).clamp(1_000_000, 300_000_000) as u32
}

/// `alt+f10` to (modifiers, virtual-key code). Modifiers: 1 alt, 2 ctrl, 4 shift, 8 win.
pub fn parse_hotkey(s: &str) -> Result<(u32, u32), String> {
    let mut mods = 0;
    let mut key = None;
    for part in s.split('+').map(|p| p.trim().to_ascii_lowercase()) {
        match part.as_str() {
            "alt" => mods |= 1,
            "ctrl" | "control" => mods |= 2,
            "shift" => mods |= 4,
            "win" | "super" => mods |= 8,
            p if p.len() > 1 && p.starts_with('f') && p[1..].parse::<u32>().is_ok_and(|n| (1..=24).contains(&n)) => {
                key = Some(0x70 + p[1..].parse::<u32>().unwrap() - 1)
            }
            p if p.len() == 1 && p.chars().next().unwrap().is_ascii_alphanumeric() => {
                key = Some(p.to_ascii_uppercase().chars().next().unwrap() as u32)
            }
            "printscreen" | "print" => key = Some(0x2c),
            "insert" => key = Some(0x2d),
            "home" => key = Some(0x24),
            "end" => key = Some(0x23),
            "pageup" => key = Some(0x21),
            "pagedown" => key = Some(0x22),
            "pause" => key = Some(0x13),
            "" => return Err(format!("`{s}` is not a hotkey")),
            other => return Err(format!("unknown key `{other}` in hotkey `{s}`")),
        }
    }
    match key {
        Some(k) => Ok((mods, k)),
        None => Err(format!("hotkey `{s}` has no key")),
    }
}

fn yes_no(v: &str, flag: &str) -> Result<bool, String> {
    match v {
        "yes" | "true" | "1" => Ok(true),
        "no" | "false" | "0" => Ok(false),
        _ => Err(format!("{flag} takes yes or no")),
    }
}

pub fn parse(args: &[String]) -> Result<Command, String> {
    match args.first().map(|s| s.as_str()) {
        Some("save") => return Ok(Command::Control("save")),
        Some("record") => return Ok(Command::Control("record")),
        Some("stop") => return Ok(Command::Control("stop")),
        Some("--list-capture-options") => return Ok(Command::ListCaptureOptions),
        Some("--list-audio-devices") | Some("--list-application-audio") => return Ok(Command::ListAudio),
        Some("--list-encoders") | Some("--info") => return Ok(Command::ListEncoders),
        Some("-h") | Some("--help") | None => return Ok(Command::Help),
        Some("--version") => return Ok(Command::Version),
        _ => {}
    }
    let mut o = Options::default();
    let mut it = args.iter();
    while let Some(flag) = it.next() {
        let mut val = || it.next().map(|s| s.as_str()).ok_or_else(|| format!("{flag} needs a value"));
        let num = |v: &str, what: &str| v.parse::<u32>().map_err(|_| format!("{what}: `{v}` is not a number"));
        match flag.as_str() {
            "-w" => {
                let v = val()?;
                o.window = match v {
                    "screen" => Window::Screen(None),
                    "focused" => Window::Focused,
                    _ if v.starts_with("screen:") => Window::Screen(Some(num(&v[7..], "-w screen:N")? as usize)),
                    _ if v.starts_with("window:") => Window::Title(v[7..].to_string()),
                    _ if v.starts_with("hwnd:") => {
                        let h = v[5..].trim_start_matches("0x");
                        Window::Handle(isize::from_str_radix(h, 16).map_err(|_| format!("-w hwnd: `{v}` is not a hex handle"))?)
                    }
                    _ => return Err(format!("-w: `{v}` (screen, screen:N, focused, window:TITLE or hwnd:0xHANDLE)")),
                };
            }
            "-c" => {
                let v = val()?;
                if v != "mp4" {
                    return Err(format!("-c: only mp4 is supported, not `{v}`"));
                }
            }
            "-f" => {
                o.fps = num(val()?, "-f")?;
                if !(1..=500).contains(&o.fps) {
                    return Err("-f: frame rate must be 1 to 500".into());
                }
            }
            "-k" => {
                o.codec = match val()? {
                    "h264" => Codec::H264,
                    "hevc" | "h265" => Codec::Hevc,
                    "av1" => Codec::Av1,
                    v => return Err(format!("-k: `{v}` (h264, hevc or av1)")),
                }
            }
            "-bm" => {
                o.bitrate_mode = match val()? {
                    "cbr" => BitrateMode::Cbr,
                    "vbr" | "auto" => BitrateMode::Vbr,
                    "qp" => BitrateMode::Qp,
                    v => return Err(format!("-bm: `{v}` (cbr, vbr or qp)")),
                }
            }
            "-q" => {
                let v = val()?;
                o.quality = match PRESETS.iter().find(|p| **p == v) {
                    Some(p) => Quality::Preset(p),
                    None => Quality::Number(num(v, "-q")?),
                };
            }
            "-a" => {
                let v = val()?;
                if v.contains('|') {
                    return Err("-a: merging sources into one track (a|b) is not supported; pass -a once per track".into());
                }
                o.audio.push(v.to_string());
            }
            "-r" => {
                let r = num(val()?, "-r")?;
                if !(2..=3600).contains(&r) {
                    return Err("-r: replay length must be 2 to 3600 seconds".into());
                }
                o.replay = Some(r);
            }
            "-o" => o.output = Some(PathBuf::from(val()?)),
            "-s" => {
                let v = val()?;
                let (w, h) = v.split_once('x').ok_or_else(|| format!("-s: `{v}` is not WIDTHxHEIGHT"))?;
                let (w, h) = (num(w, "-s")?, num(h, "-s")?);
                if w < 16 || h < 16 {
                    return Err("-s: size too small".into());
                }
                o.size = Some((w & !1, h & !1));
            }
            "-cursor" => o.cursor = yes_no(val()?, "-cursor")?,
            "-fm" => {
                o.cfr = match val()? {
                    "cfr" => true,
                    "vfr" => false,
                    v => return Err(format!("-fm: `{v}` (cfr or vfr)")),
                }
            }
            "-capture" => {
                o.dxgi = match val()? {
                    "wgc" => false,
                    "dxgi" => true,
                    v => return Err(format!("-capture: `{v}` (wgc or dxgi)")),
                }
            }
            "-gop" => {
                let v = val()?;
                o.gop_seconds = v
                    .parse::<f64>()
                    .ok()
                    .filter(|g| *g > 0.0 && *g <= 10.0)
                    .ok_or_else(|| format!("-gop: `{v}` (seconds, 0 to 10)"))?;
            }
            "-ram-limit" => o.ram_limit_mb = Some(num(val()?, "-ram-limit")?),
            "-hotkey-save" => {
                let v = val()?;
                parse_hotkey(v)?;
                o.hotkey_save = v.to_string();
            }
            "-hotkey-record" => {
                let v = val()?;
                parse_hotkey(v)?;
                o.hotkey_record = v.to_string();
            }
            "-t" => {
                let v = val()?;
                o.duration = Some(
                    v.parse::<f64>()
                        .ok()
                        .filter(|d| *d > 0.0)
                        .ok_or_else(|| format!("-t: `{v}` is not a duration in seconds"))?,
                );
            }
            "-gpu" => o.adapter = Some(num(val()?, "-gpu")?),
            "-v" => o.verbose = yes_no(val()?, "-v")?,
            other => return Err(format!("unknown option `{other}` (see rbuf -h)")),
        }
    }
    if o.replay.is_none() && o.output.is_none() {
        return Err("give -o FILE to record, or -r SECONDS (and optionally -o FOLDER) for a replay buffer".into());
    }
    if let Quality::Number(q) = o.quality {
        if o.bitrate_mode == BitrateMode::Qp && q > 100 {
            return Err("-q: with -bm qp the quality is 0 to 100".into());
        }
    }
    Ok(Command::Run(o))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn p(s: &str) -> Result<Command, String> {
        parse(&s.split_whitespace().map(String::from).collect::<Vec<_>>())
    }

    #[test]
    fn a_gpu_screen_recorder_command_line() {
        let Command::Run(o) =
            p("-w screen -f 144 -a default_output -a app:game.exe -c mp4 -r 60 -k hevc -bm cbr -q 50000 -o clips").unwrap()
        else {
            panic!()
        };
        assert_eq!(o.window, Window::Screen(None));
        assert_eq!((o.fps, o.codec, o.bitrate_mode, o.replay), (144, Codec::Hevc, BitrateMode::Cbr, Some(60)));
        assert_eq!(o.quality, Quality::Number(50000));
        assert_eq!(o.audio, vec!["default_output", "app:game.exe"]);
        assert_eq!(o.output, Some(PathBuf::from("clips")));
    }

    #[test]
    fn windows_sizes_and_switches() {
        let Command::Run(o) = p("-w window:Notepad -s 1281x721 -cursor no -fm vfr -capture dxgi -o x.mp4 -t 5").unwrap() else {
            panic!()
        };
        assert_eq!(o.window, Window::Title("Notepad".into()));
        assert_eq!(o.size, Some((1280, 720)));
        assert!(!o.cursor && !o.cfr && o.dxgi);
        assert_eq!(o.duration, Some(5.0));
        let Command::Run(o) = p("-w hwnd:0x1A2B -o x.mp4").unwrap() else { panic!() };
        assert_eq!(o.window, Window::Handle(0x1a2b));
        let Command::Run(o) = p("-w screen:1 -o x.mp4").unwrap() else { panic!() };
        assert_eq!(o.window, Window::Screen(Some(1)));
    }

    #[test]
    fn mistakes_are_explained() {
        assert!(p("-f 60").unwrap_err().contains("-o FILE"));
        assert!(p("-o x.mp4 -k vp9").unwrap_err().contains("h264"));
        assert!(p("-o x.mp4 -c mkv").unwrap_err().contains("only mp4"));
        assert!(p("-o x.mp4 -a default_output|default_input").unwrap_err().contains("not supported"));
        assert!(p("-o x.mp4 -bm qp -q 101").unwrap_err().contains("0 to 100"));
        assert!(p("-o x.mp4 -r 1").unwrap_err().contains("2 to 3600"));
        assert!(p("-o x.mp4 -f").unwrap_err().contains("needs a value"));
        assert!(p("-o x.mp4 --bogus").unwrap_err().contains("unknown option"));
        assert!(p("-o x.mp4 -hotkey-save alt+").unwrap_err().contains("not a hotkey"));
    }

    #[test]
    fn commands_and_hotkeys() {
        assert_eq!(p("save").unwrap(), Command::Control("save"));
        assert_eq!(p("").unwrap(), Command::Help);
        assert_eq!(parse_hotkey("alt+f10"), Ok((1, 0x79)));
        assert_eq!(parse_hotkey("Ctrl+Shift+S"), Ok((6, 'S' as u32)));
        assert_eq!(parse_hotkey("win+printscreen"), Ok((8, 0x2c)));
        assert!(parse_hotkey("alt+f25").is_err());
        assert!(parse_hotkey("alt").is_err());
    }

    #[test]
    fn preset_bitrates_scale_with_pixels_and_codec() {
        // 1440p60 very_high: 2560*1440*60*0.12 = 26.5 Mbit/s for H.264, 70% of it for HEVC.
        assert_eq!(preset_bitrate("very_high", Codec::H264, 2560, 1440, 60), 26_542_080);
        assert_eq!(preset_bitrate("very_high", Codec::Hevc, 2560, 1440, 60), 18_579_456);
        assert_eq!(preset_bitrate("low", Codec::H264, 16, 16, 1), 1_000_000);
    }
}
