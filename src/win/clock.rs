//! One clock for every stream: the performance counter in 100 ns ticks. Windows Graphics Capture
//! frame times and WASAPI buffer positions are already on it, so audio and video line up.

use std::sync::OnceLock;

use windows::Win32::System::Performance::{QueryPerformanceCounter, QueryPerformanceFrequency};

fn freq() -> i64 {
    static F: OnceLock<i64> = OnceLock::new();
    *F.get_or_init(|| {
        let mut f = 0i64;
        unsafe {
            let _ = QueryPerformanceFrequency(&mut f);
        }
        f.max(1)
    })
}

pub fn qpc_to_ticks(qpc: i64) -> i64 {
    (qpc as i128 * 10_000_000 / freq() as i128) as i64
}

/// Now, in 100 ns ticks.
pub fn now() -> i64 {
    let mut q = 0i64;
    unsafe {
        let _ = QueryPerformanceCounter(&mut q);
    }
    qpc_to_ticks(q)
}
