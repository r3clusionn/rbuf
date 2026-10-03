use std::process::ExitCode;

use rbuf::args::{self, Command};

const HELP: &str = "\
rbuf: a replay buffer and screen recorder for Windows

usage:
  rbuf -w screen -f 60 -a default_output -r 60 -o FOLDER     replay buffer: keep the last 60 s, save with Ctrl+Alt+F10
  rbuf -w screen -f 60 -a default_output -o FILE.mp4          record until Ctrl+C (or -t SECONDS)
  rbuf save | record | stop                                   control a running replay buffer
  rbuf --list-capture-options | --list-application-audio | --list-encoders

options (as in gpu-screen-recorder):
  -w screen|screen:N|focused|window:TITLE|hwnd:0xHANDLE   what to capture (default screen: the primary monitor)
  -f FPS             frame rate (default 60)
  -k h264|hevc|av1   codec (default h264)
  -bm cbr|vbr|qp     bitrate mode (default vbr)
  -q PRESET|NUMBER   low, medium, high, very_high (default), ultra; or kbit/s (cbr, vbr) or 0-100 (qp)
  -a SOURCE          an audio track: default_output, default_input, app:NAME or app:PID; repeat for more tracks
  -r SECONDS         replay buffer length; without it rbuf records to -o FILE
  -o PATH            output file (recording) or folder (replay; default: your Videos folder)
  -s WxH             output size (default: the captured size)
  -c mp4             container (only mp4)
  -cursor yes|no     capture the mouse cursor (default yes)
  -fm cfr|vfr        constant or variable frame rate (default cfr)
  -gop SECONDS       keyframe interval, the precision of a saved clip's start (default 1)
  -capture wgc|dxgi  Windows Graphics Capture (default) or DXGI Desktop Duplication (screens only)
  -ram-limit MB      cap the replay buffer's memory
  -hotkey-save KEYS  default ctrl+alt+f10 (ShadowPlay uses alt+f10)
  -hotkey-record KEYS  default ctrl+alt+f9
  -t SECONDS         stop after this long
  -gpu N             capture and encode on this adapter (see --list-encoders)
  -v yes|no          print frame statistics every second
";

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    match args::parse(&args) {
        Ok(cmd) => match run(cmd) {
            Ok(()) => ExitCode::SUCCESS,
            Err(e) => {
                eprintln!("rbuf: {e}");
                ExitCode::from(1)
            }
        },
        Err(e) => {
            eprintln!("rbuf: {e}");
            ExitCode::from(2)
        }
    }
}

#[cfg(not(windows))]
fn run(_: Command) -> Result<(), String> {
    Err("rbuf runs on Windows only".into())
}

#[cfg(windows)]
fn run(cmd: Command) -> Result<(), String> {
    use rbuf::win::{audio, capture, control, d3d, encoder, recorder};
    match cmd {
        Command::Help => print!("{HELP}"),
        Command::Version => println!("rbuf {}", env!("CARGO_PKG_VERSION")),
        Command::Control(action) => {
            if !control::signal(action) {
                return Err("no replay buffer is running".into());
            }
        }
        Command::ListCaptureOptions => {
            println!("screens:");
            for (i, m) in capture::monitors().iter().enumerate() {
                println!(
                    "  screen:{i}  {}x{} at {},{}{}  {}",
                    m.rect.2,
                    m.rect.3,
                    m.rect.0,
                    m.rect.1,
                    if m.primary { " (primary)" } else { "" },
                    m.name
                );
            }
            println!("windows:");
            for (h, t) in capture::windows() {
                println!("  hwnd:0x{h:x}  {t}");
            }
        }
        Command::ListAudio => {
            println!("default_output   what the default speakers or headphones play");
            println!("default_input    the default microphone");
            println!("app:NAME         one program and its child processes, for example app:game.exe or app:1234");
            let _ = audio::find_process("");
        }
        Command::ListEncoders => {
            for (i, a) in d3d::adapters().map_err(|e| e.message().to_string())? {
                println!("gpu {i}: {a}");
            }
            let list = encoder::list();
            if list.is_empty() {
                println!("no hardware encoders found");
            }
            for e in list {
                println!("  {:<5} {}", format!("{:?}", e.codec).to_lowercase(), e.name);
            }
        }
        Command::Run(o) => recorder::run(o)?,
    }
    Ok(())
}
