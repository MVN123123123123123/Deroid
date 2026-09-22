//! Universal Treble Display & Graphics HAL subsystem (Phase 2).
//! Integrates Hardware Composer (HWC 2.x HIDL & composer3 AIDL), Gralloc buffer allocation,
//! DMA-BUF zero-copy negotiation, hardware VSYNC presentation, and GPU pipeline management.

pub mod composer;
pub mod elf_align;
pub mod gpu;
pub mod gralloc;
pub mod vsync;

pub use composer::{
    CompositionType, DisplayConfig, HwcComposer, HwcError, HwcLayer, HwcVersion, PresentFences,
    Rect, Transform,
};
pub use elf_align::{
    inspect_elf_bytes, inspect_elf_file, verify_64k_alignment, ElfAlignmentReport,
};
pub use gpu::{GpuArchitecture, GpuDetector, GpuDeviceInfo, GpuPipeline};
pub use gralloc::{DmaBufBuffer, GrallocError, GrallocManager, GrallocVersion, PixelFormat};
pub use vsync::{VsyncConfig, VsyncController, VsyncError, VsyncEvent, VsyncPresentationValidator};

/// Full Phase 2 Display & Graphics Bring-Up Status
#[derive(Debug, Clone, PartialEq)]
pub struct GraphicsBringupStatus {
    pub hwc_version: HwcVersion,
    pub gralloc_version: GrallocVersion,
    pub gpu_info: GpuDeviceInfo,
    pub displays: Vec<DisplayConfig>,
    pub vsync_verified_rates: Vec<f64>,
    pub elf_64k_compliant: bool,
}

impl GraphicsBringupStatus {
    pub fn is_ready(&self) -> bool {
        self.gpu_info.architecture.is_hardware_accelerated()
            && !self.displays.is_empty()
            && self.vsync_verified_rates.contains(&60.0)
            && self.elf_64k_compliant
    }
}
