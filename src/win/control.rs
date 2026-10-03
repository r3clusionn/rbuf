//! How a running `rbuf` is told what to do: global hotkeys (ShadowPlay's Alt+F10 to save, Alt+F9
//! to record), `rbuf save` / `rbuf record` / `rbuf stop` from another terminal (named events), and
//! Ctrl+C.

use std::sync::mpsc::Sender;
use std::sync::{Mutex, OnceLock};

use windows::core::{Result, HSTRING};
use windows::Win32::Foundation::{CloseHandle, HANDLE, WAIT_OBJECT_0};
use windows::Win32::System::Console::SetConsoleCtrlHandler;
use windows::Win32::System::Threading::{
    CreateEventW, OpenEventW, SetEvent, WaitForMultipleObjects, EVENT_MODIFY_STATE, INFINITE,
};
use windows::Win32::UI::Input::KeyboardAndMouse::{RegisterHotKey, UnregisterHotKey, HOT_KEY_MODIFIERS, MOD_NOREPEAT};
use windows::Win32::UI::WindowsAndMessaging::{GetMessageW, MSG, WM_HOTKEY};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Cmd {
    Save,
    ToggleRecord,
    Stop,
}

pub const ACTIONS: [(&str, Cmd); 3] = [("save", Cmd::Save), ("record", Cmd::ToggleRecord), ("stop", Cmd::Stop)];

fn event_name(action: &str) -> HSTRING {
    HSTRING::from(format!("Local\\rbuf-{action}"))
}

/// Signals a running instance. Returns false when none is running.
pub fn signal(action: &str) -> bool {
    unsafe {
        match OpenEventW(EVENT_MODIFY_STATE, false, &event_name(action)) {
            Ok(h) => {
                let ok = SetEvent(h).is_ok();
                let _ = CloseHandle(h);
                ok
            }
            Err(_) => false,
        }
    }
}

struct Handles(Vec<HANDLE>);
unsafe impl Send for Handles {}

/// Listens for `rbuf save|record|stop` from other processes.
pub fn listen_events(tx: Sender<Cmd>) -> Result<()> {
    let mut hs = Vec::new();
    for (name, _) in ACTIONS {
        hs.push(unsafe { CreateEventW(None, false, false, &event_name(name))? });
    }
    let hs = Handles(hs);
    std::thread::spawn(move || {
        let hs = hs;
        loop {
            let r = unsafe { WaitForMultipleObjects(&hs.0, false, INFINITE) };
            let i = r.0.wrapping_sub(WAIT_OBJECT_0.0) as usize;
            if i >= ACTIONS.len() || tx.send(ACTIONS[i].1).is_err() {
                break;
            }
        }
    });
    Ok(())
}

/// Registers global hotkeys on a thread with a message loop. Errors (a key taken by another
/// program) are reported, not fatal.
pub fn listen_hotkeys(keys: Vec<((u32, u32), Cmd, String)>, tx: Sender<Cmd>) {
    std::thread::spawn(move || {
        let mut ids = Vec::new();
        for (i, ((mods, vk), cmd, label)) in keys.iter().enumerate() {
            let id = i as i32 + 1;
            if unsafe { RegisterHotKey(None, id, HOT_KEY_MODIFIERS(*mods) | MOD_NOREPEAT, *vk) }.is_ok() {
                ids.push((id, *cmd));
            } else {
                eprintln!(
                    "rbuf: hotkey {label} is taken by another program; use `rbuf {}` instead",
                    if *cmd == Cmd::Save { "save" } else { "record" }
                );
            }
        }
        let mut msg = MSG::default();
        while unsafe { GetMessageW(&mut msg, None, 0, 0) }.as_bool() {
            if msg.message == WM_HOTKEY {
                if let Some((_, cmd)) = ids.iter().find(|(id, _)| *id as usize == msg.wParam.0) {
                    if tx.send(*cmd).is_err() {
                        break;
                    }
                }
            }
        }
        for (id, _) in ids {
            unsafe {
                let _ = UnregisterHotKey(None, id);
            }
        }
    });
}

static CTRL_C: OnceLock<Mutex<Sender<Cmd>>> = OnceLock::new();

pub fn listen_ctrl_c(tx: Sender<Cmd>) {
    unsafe extern "system" fn handler(_: u32) -> windows::core::BOOL {
        if let Some(m) = CTRL_C.get() {
            let _ = m.lock().unwrap().send(Cmd::Stop);
        }
        true.into()
    }
    if CTRL_C.set(Mutex::new(tx)).is_ok() {
        unsafe {
            let _ = SetConsoleCtrlHandler(Some(handler), true);
        }
    }
}
