//! Universal Treble Mobile Wayland Compositor & Launcher (UTLC) Subsystem (Phase 3).
//! Integrates Smithay-compatible mobile Wayland protocols, HWC multi-plane overlay presentation,
//! Android 10+ QuickStep gesture navigation, paged home workspace grid, persistent dock,
//! zero-allocation desktop entry parsing, fuzzy search, SystemUI status bar and quick settings,
//! ambient lock screen with Android Fingerprint HAL bridge, virtual keyboard (IME),
//! and UTIM Mobile Power Governor / OOM hierarchy synchronization.

pub mod desktop;
pub mod gestures;
pub mod ime;
pub mod launcher;
pub mod lockscreen;
pub mod power_sync;
pub mod protocols;
pub mod recents;
pub mod scene;
pub mod server;
pub mod systemui;

pub use desktop::{fuzzy_match, parse_desktop_entry, DesktopApp, DesktopCatalogue};
pub use gestures::{
    cubic_bezier_ease_out, EdgeSide, GestureAction, GestureConfig, GestureEngine, RawTouchEvent,
    TouchPhase,
};
pub use ime::{ImeAction, KeyboardLayout, VirtualKeyboard};
pub use launcher::{AppDrawer, DrawerState, GridItem, HotseatDock, SpringConfig, SpringOscillator, WorkspaceGrid};
pub use lockscreen::{FingerprintHalBridge, FingerprintResult, LockScreen, LockState, MediaWidget};
pub use power_sync::{oom_roles, UtimPowerSync};
pub use protocols::{
    drm_formats, toplevel_state, ProtocolGlobal, ProtocolRegistry, WaylandInterface, WlHeader,
    WlMessage, WlMessageBuilder, WlrLayer,
};
pub use recents::{KillProgress, RecentsCard, RecentsCarousel, SplitScreenConfig};
pub use scene::{plane_z_order, MobileScene, ShellMode};
pub use server::{CompositorMetrics, WaylandServer};
pub use systemui::{CellularRat, NotificationCard, QuickTile, QuickTileKind, StatusBarState, SystemUiShade};
