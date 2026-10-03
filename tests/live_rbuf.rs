//! End-to-end tests on the real capture and encoder path. They need a desktop session, a GPU with
//! a hardware encoder and ffmpeg on PATH, and skip themselves (saying so) otherwise.
//!
//! * colours: record the `patches` example window with each codec and compare decoded pixels with
//!   the colours it draws;
//! * replay: run a replay buffer, save it with `rbuf save`, stop it with `rbuf stop`, and check
//!   the clip's length, first frame and decode.

#![cfg(windows)]

use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::time::Duration;

const PATCHES: [(u8, u8, u8); 8] =
    [(0, 0, 0), (255, 255, 255), (255, 0, 0), (0, 255, 0), (0, 0, 255), (128, 128, 128), (255, 255, 0), (40, 120, 200)];

fn example(name: &str) -> PathBuf {
    let deps = std::env::current_exe().unwrap().parent().unwrap().to_path_buf();
    deps.parent().unwrap().join("examples").join(format!("{name}.exe"))
}

fn ready() -> bool {
    if Command::new("ffprobe").arg("-version").output().is_err() {
        eprintln!("SKIPPED: ffmpeg not found");
        return false;
    }
    if rbuf::win::encoder::list().is_empty() {
        eprintln!("SKIPPED: no hardware encoder");
        return false;
    }
    true
}

fn rbuf(args: &[&str]) -> std::process::Output {
    Command::new(env!("CARGO_BIN_EXE_rbuf")).args(args).output().unwrap()
}

#[test]
fn colours_survive_capture_conversion_and_every_codec() {
    if !ready() {
        return;
    }
    let dir = tempfile::tempdir().unwrap();
    for codec in ["h264", "hevc", "av1"] {
        let mut win = Command::new(example("patches")).arg("6").stdout(Stdio::piped()).spawn().unwrap();
        let mut line = String::new();
        use std::io::BufRead;
        std::io::BufReader::new(win.stdout.as_mut().unwrap()).read_line(&mut line).unwrap();
        // "hwnd 0x1234 client 1 31"
        let f: Vec<&str> = line.split_whitespace().collect();
        let (hwnd, cx, cy): (&str, usize, usize) = (f[1], f[3].parse().unwrap(), f[4].parse().unwrap());
        let out = dir.path().join(format!("{codec}.mp4"));
        let r =
            rbuf(&["-w", &format!("hwnd:{hwnd}"), "-k", codec, "-bm", "qp", "-q", "90", "-o", out.to_str().unwrap(), "-t", "2"]);
        let _ = win.kill();
        let _ = win.wait();
        assert!(r.status.success(), "{}", String::from_utf8_lossy(&r.stderr));
        let probe = Command::new("ffprobe")
            .args(["-v", "error", "-show_entries", "stream=width", "-of", "csv=p=0"])
            .arg(&out)
            .output()
            .unwrap();
        let width: usize = String::from_utf8_lossy(&probe.stdout).trim().parse().unwrap();
        let raw = Command::new("ffmpeg")
            .args(["-v", "error", "-i"])
            .arg(&out)
            .args(["-vf", "select=eq(n\\,60)", "-frames:v", "1", "-f", "rawvideo", "-pix_fmt", "rgb24", "-"])
            .output()
            .unwrap()
            .stdout;
        let mut worst = 0;
        for (i, want) in PATCHES.iter().enumerate() {
            let (x, y) = (cx + (i % 4) * 200 + 100, cy + (i / 4) * 300 + 150);
            let o = (y * width + x) * 3;
            let got = (raw[o], raw[o + 1], raw[o + 2]);
            let err = [(got.0, want.0), (got.1, want.1), (got.2, want.2)]
                .iter()
                .map(|(a, b)| (*a as i32 - *b as i32).abs())
                .max()
                .unwrap();
            worst = worst.max(err);
            assert!(err <= 4, "{codec} patch {i}: drew {want:?}, decoded {got:?}");
        }
        eprintln!("{codec}: worst channel error {worst}/255");
    }
}

#[test]
fn replay_buffer_saves_the_last_seconds_from_a_keyframe() {
    if !ready() {
        return;
    }
    let dir = tempfile::tempdir().unwrap();
    let buf = Command::new(env!("CARGO_BIN_EXE_rbuf"))
        .args(["-w", "screen", "-r", "4", "-a", "default_output", "-o", dir.path().to_str().unwrap()])
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    std::thread::sleep(Duration::from_secs(7));
    assert!(rbuf(&["save"]).status.success());
    std::thread::sleep(Duration::from_secs(2));
    assert!(rbuf(&["stop"]).status.success());
    let out = buf.wait_with_output().unwrap();
    let log = String::from_utf8_lossy(&out.stderr);
    assert!(out.status.success(), "{log}");
    assert!(log.contains("rbuf: saved"), "{log}");
    let clips: Vec<PathBuf> = std::fs::read_dir(dir.path()).unwrap().map(|e| e.unwrap().path()).collect();
    assert_eq!(clips.len(), 1, "{clips:?}");
    let clip = &clips[0];
    let dur: f64 = String::from_utf8_lossy(
        &Command::new("ffprobe")
            .args(["-v", "error", "-show_entries", "format=duration", "-of", "csv=p=0"])
            .arg(clip)
            .output()
            .unwrap()
            .stdout,
    )
    .trim()
    .parse()
    .unwrap();
    // 4 s asked, plus up to one keyframe interval (1 s).
    assert!((4.0..5.1).contains(&dur), "duration {dur}");
    let first = Command::new("ffprobe")
        .args([
            "-v",
            "error",
            "-select_streams",
            "v",
            "-show_entries",
            "packet=flags",
            "-of",
            "csv=p=0",
            "-read_intervals",
            "%+#1",
        ])
        .arg(clip)
        .output()
        .unwrap();
    assert!(String::from_utf8_lossy(&first.stdout).starts_with('K'), "the clip starts on a keyframe");
    let dec = Command::new("ffmpeg").args(["-v", "error", "-i"]).arg(clip).args(["-f", "null", "-"]).output().unwrap();
    assert!(dec.status.success() && dec.stderr.is_empty(), "{}", String::from_utf8_lossy(&dec.stderr));
    // `rbuf save` with nothing running says so.
    assert!(!rbuf(&["save"]).status.success());
}
