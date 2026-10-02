//! Universal Treble Mobile Wayland Compositor & Launcher (UTLC) Subsystem (Phase 3).
//! Integrates Smithay-compatible mobile Wayland protocols, HWC multi-plane overlay presentation,
//! Android 10+ QuickStep gesture navigation, paged home workspace grid, persistent dock,
//! zero-allocation desktop entry parsing, fuzzy search, SystemUI status bar and quick settings,
//! ambient lock screen with Android Fingerprint HAL bridge, virtual keyboard (IME),
//! and UTIM Mobile Power Governor / OOM hierarchy synchronization.

pub mod desktop;
pub mod gestures;
pub mod haptics;
pub mod icons;
pub mod ime;
pub mod input;
pub mod lockscreen;
pub mod power_sync;
pub mod protocols;
pub mod recents;
pub mod scene;
pub mod server;
pub mod super_extreme;
pub mod systemui;

pub use desktop::{fuzzy_match, parse_desktop_entry, DesktopApp, DesktopCatalogue};
pub use gestures::{
    cubic_bezier, emphasized, emphasized_accelerate, emphasized_decelerate, fast_out_linear_in,
    fast_out_slow_in, linear_out_slow_in, standard_decelerate, touch_response, EdgeSide,
    GestureAction, GestureConfig, GestureEngine, MotionHistory, MotionPause, RawTouchEvent,
    TouchPhase, MOTION_SAMPLES,
};
pub use haptics::{HapticEffect, Haptics};
pub use icons::{IconCache, ICON_MAX_EDGE};
pub use ime::{ImeAction, KeyboardLayout, VirtualKeyboard};
pub use input::{InputDispatchResult, InputDispatcher, LinuxInputEvent};
pub use lockscreen::{FingerprintHalBridge, FingerprintResult, LockScreen, LockState, MediaWidget};
pub use power_sync::{oom_roles, UtimPowerSync};
pub use protocols::{
    drm_formats, toplevel_state, ProtocolGlobal, ProtocolRegistry, WaylandInterface, WlHeader,
    WlMessage, WlrLayer,
};
pub use recents::{
    dismiss_recents_scale, DismissOutcome, FolderOpen, KillAction, KillQueue, KillState,
    PopupItem, PopupItems, Recents, TaskCard, MAX_DEEP_SHORTCUTS, MAX_TASKS, NO_THUMB,
};
pub use scene::{plane_z_order, MobileScene, ShellMode};
pub use server::{CompositorMetrics, WaylandServer};
pub use super_extreme::{SuperExtremeScreen, SuperExtremeState, TtyKey, VolumeHud};
pub use systemui::{
    CellularRat, NotificationCard, QuickTile, QuickTileKind, StatusBarState, SystemUiShade,
};
