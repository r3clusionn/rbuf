//! `rbuf`: a replay buffer and screen recorder for Windows. The platform-independent parts (the
//! MP4 muxer, bitstream parsing and the packet ring) live here and are unit tested; the capture,
//! conversion, encoding and audio code is in the Windows-only modules.

pub mod aac;
pub mod args;
pub mod av1;
pub mod bits;
pub mod mp4;
pub mod nal;
pub mod ring;
#[cfg(windows)]
pub mod win;
