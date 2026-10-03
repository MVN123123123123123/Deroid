//! Universal Treble Init Manager (UTIM) Core Library.
//! High-performance, zero-redundant-dependency foundation for phone-optimized PID 1 and systemd emulation.

pub mod android_rc;
pub mod audio;
pub mod camera;
pub mod compositor;
pub mod dag;
pub mod fstab;
pub mod graphics;
pub mod hal;
pub mod input;
pub mod ipc;
pub mod launcher_state;
pub mod mmps;
pub mod mpg;
pub mod net;
pub mod notification;
pub mod popup_config;
pub mod ring_buffer;
pub mod rotation;
pub mod sensors;
pub mod session;
pub mod settings;
pub mod telephony;
pub mod theme;
pub mod unit;

pub const UTIM_VERSION: &str = "0.1.0";
