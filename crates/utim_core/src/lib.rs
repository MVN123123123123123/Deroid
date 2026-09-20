//! Universal Treble Init Manager (UTIM) Core Library.
//! High-performance, zero-redundant-dependency foundation for phone-optimized PID 1 and systemd emulation.

pub mod android_rc;
pub mod compositor;
pub mod dag;
pub mod fstab;
pub mod graphics;
pub mod hal;
pub mod ipc;
pub mod mmps;
pub mod mpg;
pub mod ring_buffer;
pub mod unit;

pub const UTIM_VERSION: &str = "0.1.0";
