//! Gralloc graphic buffer allocator and DMA-BUF zero-copy negotiation.
//! Supports Gralloc 0.3, 1.0, 2.0, 3.0, 4.0 and Stable AIDL IAllocator/IMapper.
//! Handles Qualcomm UBWC (Universal Bandwidth Compression) and ARM AFBC compression flags.

use std::fmt;
use std::os::unix::io::RawFd;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GrallocVersion {
    Gralloc0_3,
    Gralloc1_0,
    Gralloc2_0,
    Gralloc3_0,
    Gralloc4_0,
    AidlAllocator, // Android 12+ (android.hardware.graphics.allocator-service)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u32)]
pub enum PixelFormat {
    Rgba8888 = 1,
    Rgbx8888 = 2,
    Rgb888 = 3,
    Rgb565 = 4,
    Bgra8888 = 5,
    Yv12 = 0x32315659,
    Nv12 = 0x3231564E,
    Ycbcr420888 = 0x23,
    Blob = 0x21,
    ImplementationDefined = 0x22,
}

impl PixelFormat {
    pub fn from_u32(val: u32) -> Option<Self> {
        match val {
            1 => Some(PixelFormat::Rgba8888),
            2 => Some(PixelFormat::Rgbx8888),
            3 => Some(PixelFormat::Rgb888),
            4 => Some(PixelFormat::Rgb565),
            5 => Some(PixelFormat::Bgra8888),
            0x32315659 => Some(PixelFormat::Yv12),
            0x3231564E => Some(PixelFormat::Nv12),
            0x23 => Some(PixelFormat::Ycbcr420888),
            0x21 => Some(PixelFormat::Blob),
            0x22 => Some(PixelFormat::ImplementationDefined),
            _ => None,
        }
    }

    pub fn bytes_per_pixel(&self) -> usize {
        match self {
            PixelFormat::Rgba8888 | PixelFormat::Rgbx8888 | PixelFormat::Bgra8888 => 4,
            PixelFormat::Rgb888 => 3,
            PixelFormat::Rgb565 => 2,
            PixelFormat::Blob => 1,
            // Subsampled YUV formats return effective Y bytes
            PixelFormat::Yv12 | PixelFormat::Nv12 | PixelFormat::Ycbcr420888 => 1,
            PixelFormat::ImplementationDefined => 4,
        }
    }

    pub fn is_yuv(&self) -> bool {
        matches!(
            self,
            PixelFormat::Yv12 | PixelFormat::Nv12 | PixelFormat::Ycbcr420888
        )
    }

    pub fn name(&self) -> &'static str {
        match self {
            PixelFormat::Rgba8888 => "RGBA_8888",
            PixelFormat::Rgbx8888 => "RGBX_8888",
            PixelFormat::Rgb888 => "RGB_888",
            PixelFormat::Rgb565 => "RGB_565",
            PixelFormat::Bgra8888 => "BGRA_8888",
            PixelFormat::Yv12 => "YV12",
            PixelFormat::Nv12 => "NV12",
            PixelFormat::Ycbcr420888 => "YCBCR_420_888",
            PixelFormat::Blob => "BLOB",
            PixelFormat::ImplementationDefined => "IMPLEMENTATION_DEFINED",
        }
    }
}

pub mod usage {
    pub const SW_READ_NEVER: u64 = 0x0;
    pub const SW_READ_RARELY: u64 = 0x2;
    pub const SW_READ_OFTEN: u64 = 0x3;
    pub const SW_WRITE_RARELY: u64 = 0x20;
    pub const SW_WRITE_OFTEN: u64 = 0x30;
    pub const HW_TEXTURE: u64 = 0x0100;
    pub const HW_RENDER: u64 = 0x0200;
    pub const HW_2D: u64 = 0x0400;
    pub const HW_COMPOSER: u64 = 0x0800;
    pub const HW_VIDEO_ENCODER: u64 = 0x00010000;
    pub const HW_CAMERA_WRITE: u64 = 0x00020000;
    pub const HW_CAMERA_READ: u64 = 0x00040000;
    pub const PROTECTED: u64 = 0x00004000;
    pub const CURSOR: u64 = 0x00008000;

    // Vendor Compression flags
    pub const QCOM_USAGE_UBWC: u64 = 0x10000000; // Qualcomm Universal Bandwidth Compression
    pub const ARM_USAGE_AFBC: u64 = 0x20000000; // ARM Frame Buffer Compression
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BufferPlane {
    pub offset: u64,
    pub stride_bytes: u32,
    pub size_bytes: usize,
}

fn dup_cloexec(fd: RawFd) -> Option<RawFd> {
    if fd >= 0 {
        let dup = unsafe { libc::fcntl(fd, libc::F_DUPFD_CLOEXEC, 0) };
        if dup >= 0 {
            Some(dup)
        } else {
            None
        }
    } else {
        Some(fd)
    }
}

#[derive(Debug, PartialEq, Eq)]
pub struct DmaBufBuffer {
    pub id: u64,
    pub fd: Option<RawFd>,
    pub width: u32,
    pub height: u32,
    pub format: PixelFormat,
    pub usage: u64,
    pub stride_pixels: u32,
    pub byte_stride: u32,
    pub slice_height: u32,
    pub total_size_bytes: usize,
    pub planes: Vec<BufferPlane>,
    pub is_ubwc: bool,
    pub is_afbc: bool,
    pub acquire_fence: Option<RawFd>,
    pub release_fence: Option<RawFd>,
}

impl Clone for DmaBufBuffer {
    fn clone(&self) -> Self {
        Self {
            id: self.id,
            fd: self.fd.and_then(dup_cloexec),
            width: self.width,
            height: self.height,
            format: self.format,
            usage: self.usage,
            stride_pixels: self.stride_pixels,
            byte_stride: self.byte_stride,
            slice_height: self.slice_height,
            total_size_bytes: self.total_size_bytes,
            planes: self.planes.clone(),
            is_ubwc: self.is_ubwc,
            is_afbc: self.is_afbc,
            acquire_fence: self.acquire_fence.and_then(dup_cloexec),
            release_fence: self.release_fence.and_then(dup_cloexec),
        }
    }
}

impl DmaBufBuffer {
    /// Format information as Wayland linux-dmabuf parameter tuple:
    /// (fourcc, modifier_hi, modifier_lo)
    pub fn wayland_dmabuf_params(&self) -> (u32, u32, u32) {
        let fourcc = match self.format {
            PixelFormat::Rgba8888 => 0x34324152, // DRM_FORMAT_RGBA8888 ('RA24')
            PixelFormat::Rgbx8888 => 0x34325852, // DRM_FORMAT_RGBX8888 ('RX24')
            PixelFormat::Bgra8888 => 0x34324142, // DRM_FORMAT_BGRA8888 ('BA24')
            PixelFormat::Rgb888 => 0x34324752,   // DRM_FORMAT_BGR888 ('RG24')
            PixelFormat::Rgb565 => 0x36314752,   // DRM_FORMAT_RGB565 ('RG16')
            PixelFormat::Yv12 => 0x32315659,     // DRM_FORMAT_YVU420 ('YV12')
            PixelFormat::Nv12 => 0x3231564E,     // DRM_FORMAT_NV12 ('NV12')
            PixelFormat::Ycbcr420888 => 0x3231564E, // DRM_FORMAT_NV12 ('NV12')
            PixelFormat::Blob => 0x20202020,     // DRM_FORMAT_RAW
            PixelFormat::ImplementationDefined => 0x34324152,
        };

        // Modifiers for compression
        let (mod_hi, mod_lo) = if self.is_ubwc {
            // DRM_FORMAT_MOD_QCOM_COMPRESSED: (0x0a << 56) | ...
            (0x0a000000, 0x00000001)
        } else if self.is_afbc {
            // DRM_FORMAT_MOD_ARM_AFBC: (0x08 << 56) | ...
            (0x08000000, 0x00000001)
        } else {
            // DRM_FORMAT_MOD_LINEAR = 0
            (0, 0)
        };

        (fourcc, mod_hi, mod_lo)
    }
}

impl Drop for DmaBufBuffer {
    fn drop(&mut self) {
        if let Some(fd) = self.fd.take() {
            if fd >= 0 {
                unsafe { libc::close(fd) };
            }
        }
        if let Some(fd) = self.acquire_fence.take() {
            if fd >= 0 {
                unsafe { libc::close(fd) };
            }
        }
        if let Some(fd) = self.release_fence.take() {
            if fd >= 0 {
                unsafe { libc::close(fd) };
            }
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum GrallocError {
    UnsupportedFormat(u32),
    UnsupportedUsage(u64),
    AllocationFailed(String),
    BufferNotFound(u64),
    InvalidDimensions(u32, u32),
    LockFailed(String),
}

impl fmt::Display for GrallocError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            GrallocError::UnsupportedFormat(fmt) => {
                write!(f, "Unsupported pixel format: 0x{:x}", fmt)
            }
            GrallocError::UnsupportedUsage(u) => write!(f, "Unsupported usage flags: 0x{:x}", u),
            GrallocError::AllocationFailed(s) => write!(f, "Gralloc allocation failed: {}", s),
            GrallocError::BufferNotFound(id) => write!(f, "Buffer handle {} not found", id),
            GrallocError::InvalidDimensions(w, h) => write!(f, "Invalid dimensions: {}x{}", w, h),
            GrallocError::LockFailed(s) => write!(f, "Gralloc lock failed: {}", s),
        }
    }
}

impl std::error::Error for GrallocError {}

/// Hybris Gralloc Manager orchestrating hardware buffer allocations and DMA-BUF negotiation.
pub struct GrallocManager {
    version: GrallocVersion,
    next_buffer_id: u64,
}

impl Default for GrallocManager {
    fn default() -> Self {
        Self::new(GrallocVersion::AidlAllocator)
    }
}

impl GrallocManager {
    pub fn new(version: GrallocVersion) -> Self {
        Self {
            version,
            next_buffer_id: 1,
        }
    }

    pub fn version(&self) -> GrallocVersion {
        self.version
    }

    /// Auto-detect Gralloc version from vendor manifest or existing binder HAL services.
    pub fn detect_version(manifest_content: Option<&str>) -> GrallocVersion {
        if let Some(content) = manifest_content {
            if content.contains("android.hardware.graphics.allocator") && content.contains("aidl") {
                return GrallocVersion::AidlAllocator;
            }
            if content.contains("android.hardware.graphics.allocator@4.0") {
                return GrallocVersion::Gralloc4_0;
            }
            if content.contains("android.hardware.graphics.allocator@3.0") {
                return GrallocVersion::Gralloc3_0;
            }
            if content.contains("android.hardware.graphics.allocator@2.0") {
                return GrallocVersion::Gralloc2_0;
            }
        }
        // Fallback default for modern Treble GSI
        GrallocVersion::AidlAllocator
    }

    /// Allocate a DMA-BUF backed graphic buffer with negotiated vendor alignment and compression.
    pub fn allocate(
        &mut self,
        width: u32,
        height: u32,
        format: PixelFormat,
        usage: u64,
    ) -> Result<DmaBufBuffer, GrallocError> {
        if width == 0 || height == 0 || width > 16384 || height > 16384 {
            return Err(GrallocError::InvalidDimensions(width, height));
        }

        let is_ubwc = (usage & usage::QCOM_USAGE_UBWC) != 0;
        let is_afbc = (usage & usage::ARM_USAGE_AFBC) != 0;

        if is_ubwc && is_afbc {
            return Err(GrallocError::UnsupportedUsage(usage));
        }

        if is_ubwc
            && !matches!(
                format,
                PixelFormat::Rgba8888
                    | PixelFormat::Rgbx8888
                    | PixelFormat::Bgra8888
                    | PixelFormat::Nv12
                    | PixelFormat::Ycbcr420888
            )
        {
            return Err(GrallocError::UnsupportedUsage(usage));
        }

        if is_afbc
            && !matches!(
                format,
                PixelFormat::Rgba8888
                    | PixelFormat::Rgbx8888
                    | PixelFormat::Bgra8888
                    | PixelFormat::Rgb888
                    | PixelFormat::Rgb565
            )
        {
            return Err(GrallocError::UnsupportedUsage(usage));
        }

        let bpp = format.bytes_per_pixel();

        // Android vendor gralloc stride alignment requirements:
        // Most Qualcomm/MediaTek SoCs require 64 or 128 pixel stride alignment.
        // For UBWC: requires 16x4 tile boundary alignment.
        let stride_alignment = if is_ubwc { 64 } else { 32 };

        let stride_pixels = (width + stride_alignment - 1) & !(stride_alignment - 1);
        let byte_stride = stride_pixels * (bpp as u32);

        // Slice height alignment: typically 16 or 32 rows
        let slice_alignment = if is_ubwc { 32 } else { 16 };
        let slice_height = (height + slice_alignment - 1) & !(slice_alignment - 1);

        let mut planes = Vec::new();
        let total_size_bytes: usize;

        match format {
            PixelFormat::Rgba8888
            | PixelFormat::Rgbx8888
            | PixelFormat::Bgra8888
            | PixelFormat::Rgb888
            | PixelFormat::Rgb565
            | PixelFormat::ImplementationDefined => {
                let main_plane_size = (byte_stride as usize) * (slice_height as usize);
                planes.push(BufferPlane {
                    offset: 0,
                    stride_bytes: byte_stride,
                    size_bytes: main_plane_size,
                });

                if is_ubwc {
                    // Qualcomm UBWC RGB metadata calculation:
                    // Metadata plane contains compression headers for 16x4 pixel blocks.
                    // 1 byte per 64 bytes of pixel data, rounded up to 4096-byte page.
                    let meta_size_raw = main_plane_size.div_ceil(64);
                    let meta_size = (meta_size_raw + 4095) & !4095;
                    planes.push(BufferPlane {
                        offset: main_plane_size as u64,
                        stride_bytes: ((stride_pixels / 16) + 63) & !63,
                        size_bytes: meta_size,
                    });
                    total_size_bytes = main_plane_size + meta_size;
                } else if is_afbc {
                    // ARM AFBC header: 16 bytes per 16x16 superblock
                    let blocks_x = width.div_ceil(16);
                    let blocks_y = height.div_ceil(16);
                    let header_size = ((blocks_x * blocks_y * 16) as usize + 4095) & !4095;
                    planes.push(BufferPlane {
                        offset: main_plane_size as u64,
                        stride_bytes: blocks_x * 16,
                        size_bytes: header_size,
                    });
                    total_size_bytes = main_plane_size + header_size;
                } else {
                    total_size_bytes = main_plane_size;
                }
            }
            PixelFormat::Yv12 => {
                // Y plane
                let y_size = (stride_pixels as usize) * (slice_height as usize);
                planes.push(BufferPlane {
                    offset: 0,
                    stride_bytes: stride_pixels,
                    size_bytes: y_size,
                });
                // V plane (height/2, stride/2)
                let c_stride = ((stride_pixels / 2) + 15) & !15;
                let c_height = slice_height.div_ceil(2);
                let c_size = (c_stride as usize) * (c_height as usize);
                planes.push(BufferPlane {
                    offset: y_size as u64,
                    stride_bytes: c_stride,
                    size_bytes: c_size,
                });
                // U plane
                planes.push(BufferPlane {
                    offset: (y_size + c_size) as u64,
                    stride_bytes: c_stride,
                    size_bytes: c_size,
                });
                total_size_bytes = y_size + (2 * c_size);
            }
            PixelFormat::Nv12 | PixelFormat::Ycbcr420888 => {
                // Y plane
                let y_size = (stride_pixels as usize) * (slice_height as usize);
                planes.push(BufferPlane {
                    offset: 0,
                    stride_bytes: stride_pixels,
                    size_bytes: y_size,
                });

                if is_ubwc {
                    // Qualcomm UBWC NV12: 4 planes
                    // 1. Y Data
                    // 2. Y Metadata
                    // 3. UV Interleaved Data
                    // 4. UV Metadata
                    let y_meta_raw = y_size.div_ceil(64);
                    let y_meta_size = (y_meta_raw + 4095) & !4095;
                    planes.push(BufferPlane {
                        offset: y_size as u64,
                        stride_bytes: ((stride_pixels / 16) + 63) & !63,
                        size_bytes: y_meta_size,
                    });

                    let uv_size = (stride_pixels as usize) * ((slice_height as usize) / 2);
                    let uv_offset = (y_size + y_meta_size) as u64;
                    planes.push(BufferPlane {
                        offset: uv_offset,
                        stride_bytes: stride_pixels,
                        size_bytes: uv_size,
                    });

                    let uv_meta_raw = uv_size.div_ceil(64);
                    let uv_meta_size = (uv_meta_raw + 4095) & !4095;
                    planes.push(BufferPlane {
                        offset: uv_offset + uv_size as u64,
                        stride_bytes: ((stride_pixels / 16) + 63) & !63,
                        size_bytes: uv_meta_size,
                    });

                    total_size_bytes = y_size + y_meta_size + uv_size + uv_meta_size;
                } else {
                    // Linear NV12: Y plane + UV plane
                    let uv_size = (stride_pixels as usize) * ((slice_height as usize) / 2);
                    planes.push(BufferPlane {
                        offset: y_size as u64,
                        stride_bytes: stride_pixels,
                        size_bytes: uv_size,
                    });
                    total_size_bytes = y_size + uv_size;
                }
            }
            PixelFormat::Blob => {
                let size = (width as usize) * (height as usize);
                planes.push(BufferPlane {
                    offset: 0,
                    stride_bytes: width,
                    size_bytes: size,
                });
                total_size_bytes = size;
            }
        }

        // Create simulated or real memfd DMA-BUF file descriptor
        let memfd = Self::create_dmabuf_memfd(total_size_bytes)?;

        let id = self.next_buffer_id;
        self.next_buffer_id = self.next_buffer_id.wrapping_add(1);

        Ok(DmaBufBuffer {
            id,
            fd: Some(memfd),
            width,
            height,
            format,
            usage,
            stride_pixels,
            byte_stride,
            slice_height,
            total_size_bytes,
            planes,
            is_ubwc,
            is_afbc,
            acquire_fence: None,
            release_fence: None,
        })
    }

    /// Create an anonymous sealed memfd descriptor behaving as an in-memory DMA-BUF backing store.
    fn create_dmabuf_memfd(size: usize) -> Result<RawFd, GrallocError> {
        let name = b"utim_gralloc_dmabuf\0";
        // memfd_create flags: MFD_CLOEXEC | MFD_ALLOW_SEALING
        let fd = unsafe {
            libc::syscall(
                libc::SYS_memfd_create,
                name.as_ptr() as *const libc::c_char,
                0x0001 | 0x0002, // MFD_CLOEXEC | MFD_ALLOW_SEALING
            ) as RawFd
        };

        if fd < 0 {
            // Fallback: /dev/null or error
            return Err(GrallocError::AllocationFailed(
                "Failed to create memfd DMA-BUF descriptor".into(),
            ));
        }

        let ret = unsafe { libc::ftruncate(fd, size as libc::off_t) };
        if ret != 0 {
            unsafe { libc::close(fd) };
            return Err(GrallocError::AllocationFailed(
                "Failed to truncate DMA-BUF descriptor".into(),
            ));
        }

        Ok(fd)
    }

    /// Duplicate buffer DMA-BUF file descriptor for export to Wayland client.
    pub fn export_dmabuf(&self, buffer: &DmaBufBuffer) -> Result<RawFd, GrallocError> {
        let Some(src_fd) = buffer.fd else {
            return Err(GrallocError::BufferNotFound(buffer.id));
        };

        let dup_fd = unsafe { libc::fcntl(src_fd, libc::F_DUPFD_CLOEXEC, 0) };
        if dup_fd < 0 {
            return Err(GrallocError::AllocationFailed(
                "Failed to duplicate DMA-BUF descriptor (F_DUPFD_CLOEXEC)".into(),
            ));
        }

        Ok(dup_fd)
    }

    /// Import an existing DMA-BUF file descriptor from a Wayland client or foreign buffer producer.
    /// Duplicates the descriptor via F_DUPFD_CLOEXEC to establish clean lifecycle ownership.
    pub fn import_dmabuf(
        &mut self,
        raw_fd: RawFd,
        width: u32,
        height: u32,
        format: PixelFormat,
        usage: u64,
        stride_pixels: Option<u32>,
    ) -> Result<DmaBufBuffer, GrallocError> {
        if raw_fd < 0 {
            return Err(GrallocError::AllocationFailed(
                "Invalid DMA-BUF descriptor".into(),
            ));
        }
        if width == 0 || height == 0 || width > 16384 || height > 16384 {
            return Err(GrallocError::InvalidDimensions(width, height));
        }

        let is_ubwc = (usage & usage::QCOM_USAGE_UBWC) != 0;
        let is_afbc = (usage & usage::ARM_USAGE_AFBC) != 0;

        if is_ubwc && is_afbc {
            return Err(GrallocError::UnsupportedUsage(usage));
        }

        let dup_fd = unsafe { libc::fcntl(raw_fd, libc::F_DUPFD_CLOEXEC, 0) };
        if dup_fd < 0 {
            return Err(GrallocError::AllocationFailed(
                "Failed to duplicate imported DMA-BUF descriptor (F_DUPFD_CLOEXEC)".into(),
            ));
        }

        let bpp = format.bytes_per_pixel();
        let stride_alignment = if is_ubwc { 64 } else { 32 };
        let calc_stride_pixels = stride_pixels
            .unwrap_or_else(|| (width + stride_alignment - 1) & !(stride_alignment - 1));
        let byte_stride = calc_stride_pixels * (bpp as u32);
        let slice_alignment = if is_ubwc { 32 } else { 16 };
        let slice_height = (height + slice_alignment - 1) & !(slice_alignment - 1);

        let mut planes = Vec::new();
        let total_size_bytes = (byte_stride as usize) * (slice_height as usize);
        planes.push(BufferPlane {
            offset: 0,
            stride_bytes: byte_stride,
            size_bytes: total_size_bytes,
        });

        let id = self.next_buffer_id;
        self.next_buffer_id = self.next_buffer_id.wrapping_add(1);

        Ok(DmaBufBuffer {
            id,
            fd: Some(dup_fd),
            width,
            height,
            format,
            usage,
            stride_pixels: calc_stride_pixels,
            byte_stride,
            slice_height,
            total_size_bytes,
            planes,
            is_ubwc,
            is_afbc,
            acquire_fence: None,
            release_fence: None,
        })
    }

    /// Detect active kernel DMA heap or legacy ION allocator.
    pub fn detect_dma_heap() -> Option<&'static str> {
        let candidates = [
            "/dev/dma_heap/system",
            "/dev/dma_heap/qcom,system",
            "/dev/dma_heap/system-uncached",
            "/dev/ion",
        ];
        candidates
            .into_iter()
            .find(|&candidate| std::path::Path::new(candidate).exists())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_pixel_format_properties() {
        assert_eq!(PixelFormat::Rgba8888.bytes_per_pixel(), 4);
        assert_eq!(PixelFormat::Rgb565.bytes_per_pixel(), 2);
        assert!(!PixelFormat::Rgba8888.is_yuv());
        assert!(PixelFormat::Yv12.is_yuv());
        assert!(PixelFormat::Nv12.is_yuv());
    }

    #[test]
    fn test_gralloc_version_detection() {
        let manifest_aidl = r#"
            <hal format="aidl">
                <name>android.hardware.graphics.allocator</name>
            </hal>
        "#;
        assert_eq!(
            GrallocManager::detect_version(Some(manifest_aidl)),
            GrallocVersion::AidlAllocator
        );

        let manifest_hidl = r#"
            <hal format="hidl">
                <name>android.hardware.graphics.allocator@4.0</name>
            </hal>
        "#;
        assert_eq!(
            GrallocManager::detect_version(Some(manifest_hidl)),
            GrallocVersion::Gralloc4_0
        );
    }

    #[test]
    fn test_gralloc_allocate_linear_rgba() {
        let mut mgr = GrallocManager::new(GrallocVersion::AidlAllocator);
        let buf = mgr
            .allocate(
                1080,
                2400,
                PixelFormat::Rgba8888,
                usage::HW_RENDER | usage::HW_COMPOSER,
            )
            .expect("Allocate buffer");

        assert_eq!(buf.width, 1080);
        assert_eq!(buf.height, 2400);
        assert_eq!(buf.format, PixelFormat::Rgba8888);
        assert!(!buf.is_ubwc);
        assert!(!buf.is_afbc);
        assert!(buf.stride_pixels >= 1080);
        assert_eq!(buf.stride_pixels % 32, 0); // Stride alignment
        assert_eq!(buf.byte_stride, buf.stride_pixels * 4);
        assert!(buf.fd.is_some());
        assert!(buf.total_size_bytes >= (buf.byte_stride as usize * buf.slice_height as usize));
        assert_eq!(buf.planes.len(), 1);

        // Verify Wayland dmabuf parameters
        let (fourcc, mod_hi, mod_lo) = buf.wayland_dmabuf_params();
        assert_eq!(fourcc, 0x34324152); // DRM_FORMAT_RGBA8888
        assert_eq!((mod_hi, mod_lo), (0, 0)); // Linear modifier
    }

    #[test]
    fn test_gralloc_allocate_ubwc_qcom() {
        let mut mgr = GrallocManager::new(GrallocVersion::AidlAllocator);
        let buf = mgr
            .allocate(
                1080,
                2400,
                PixelFormat::Rgba8888,
                usage::HW_RENDER | usage::HW_COMPOSER | usage::QCOM_USAGE_UBWC,
            )
            .expect("Allocate UBWC buffer");

        assert!(buf.is_ubwc);
        assert_eq!(buf.stride_pixels % 64, 0); // UBWC stride alignment
        assert_eq!(buf.planes.len(), 2); // Main pixel plane + UBWC metadata plane
        assert!(buf.planes[1].size_bytes > 0);
        assert_eq!(buf.planes[1].offset, buf.planes[0].size_bytes as u64);

        let (_, mod_hi, _) = buf.wayland_dmabuf_params();
        assert_eq!(mod_hi, 0x0a000000); // QCOM modifier
    }

    #[test]
    fn test_gralloc_allocate_yuv_nv12() {
        let mut mgr = GrallocManager::new(GrallocVersion::AidlAllocator);
        let buf = mgr
            .allocate(1920, 1080, PixelFormat::Nv12, usage::HW_VIDEO_ENCODER)
            .expect("Allocate NV12 buffer");

        assert_eq!(buf.planes.len(), 2); // Y plane + UV plane
        assert_eq!(buf.planes[0].offset, 0);
        assert_eq!(buf.planes[1].offset, buf.planes[0].size_bytes as u64);
        assert_eq!(
            buf.total_size_bytes,
            buf.planes[0].size_bytes + buf.planes[1].size_bytes
        );
    }

    #[test]
    fn test_gralloc_allocate_ubwc_nv12_4planes() {
        let mut mgr = GrallocManager::new(GrallocVersion::AidlAllocator);
        let buf = mgr
            .allocate(
                1920,
                1080,
                PixelFormat::Nv12,
                usage::HW_VIDEO_ENCODER | usage::QCOM_USAGE_UBWC,
            )
            .expect("Allocate UBWC NV12");

        assert!(buf.is_ubwc);
        assert_eq!(buf.planes.len(), 4); // Y data + Y meta + UV data + UV meta
        assert_eq!(buf.planes[0].offset, 0);
        assert_eq!(buf.planes[1].offset, buf.planes[0].size_bytes as u64);
        assert_eq!(
            buf.planes[2].offset,
            (buf.planes[0].size_bytes + buf.planes[1].size_bytes) as u64
        );
        assert_eq!(
            buf.planes[3].offset,
            (buf.planes[0].size_bytes + buf.planes[1].size_bytes + buf.planes[2].size_bytes) as u64
        );
    }

    #[test]
    fn test_dmabuf_clone_fd_isolation() {
        let mut mgr = GrallocManager::new(GrallocVersion::AidlAllocator);
        let buf1 = mgr
            .allocate(64, 64, PixelFormat::Rgba8888, usage::HW_RENDER)
            .unwrap();
        let fd1 = buf1.fd.unwrap();

        // Clone buffer
        let buf2 = buf1.clone();
        let fd2 = buf2.fd.unwrap();

        // The file descriptors MUST be distinct numbers (duplicated via F_DUPFD_CLOEXEC)
        assert_ne!(
            fd1, fd2,
            "Cloning DmaBufBuffer must produce an independent, duplicated fd"
        );

        // Dropping buf2 closes fd2, but fd1 MUST remain valid!
        drop(buf2);

        // Verify fd1 is still open and valid
        let flags = unsafe { libc::fcntl(fd1, libc::F_GETFD) };
        assert!(
            flags >= 0,
            "Original fd1 must remain open after dropping cloned buf2"
        );
    }

    #[test]
    fn test_gralloc_invalid_dimensions() {
        let mut mgr = GrallocManager::new(GrallocVersion::AidlAllocator);
        assert!(mgr.allocate(0, 1080, PixelFormat::Rgba8888, 0).is_err());
        assert!(mgr.allocate(1080, 0, PixelFormat::Rgba8888, 0).is_err());
        assert!(mgr.allocate(20000, 1080, PixelFormat::Rgba8888, 0).is_err());
        assert!(mgr.allocate(1080, 20000, PixelFormat::Rgba8888, 0).is_err());
    }

    #[test]
    fn test_export_dmabuf() {
        let mut mgr = GrallocManager::new(GrallocVersion::AidlAllocator);
        let buf = mgr
            .allocate(100, 100, PixelFormat::Rgba8888, usage::SW_READ_OFTEN)
            .expect("Allocate buffer");

        let exported_fd = mgr.export_dmabuf(&buf).expect("Export DMA-BUF");
        assert!(exported_fd >= 0);
        unsafe { libc::close(exported_fd) };
    }

    #[test]
    fn test_gralloc_import_dmabuf() {
        let mut mgr = GrallocManager::new(GrallocVersion::AidlAllocator);
        let buf = mgr
            .allocate(256, 256, PixelFormat::Rgba8888, usage::HW_RENDER)
            .expect("Allocate");
        let exported_fd = mgr.export_dmabuf(&buf).expect("Export");

        // Import the exported DMA-BUF fd
        let imported = mgr
            .import_dmabuf(
                exported_fd,
                256,
                256,
                PixelFormat::Rgba8888,
                usage::HW_RENDER,
                None,
            )
            .expect("Import");

        assert_eq!(imported.width, 256);
        assert_eq!(imported.height, 256);
        assert!(imported.fd.is_some());
        assert_ne!(
            imported.fd.unwrap(),
            exported_fd,
            "Import must duplicate descriptor"
        );

        // Close exported_fd
        unsafe { libc::close(exported_fd) };

        // Imported buffer fd must remain valid
        let flags = unsafe { libc::fcntl(imported.fd.unwrap(), libc::F_GETFD) };
        assert!(flags >= 0);
    }

    #[test]
    fn test_gralloc_conflicting_and_incompatible_compression() {
        let mut mgr = GrallocManager::new(GrallocVersion::AidlAllocator);

        // Conflicting UBWC and AFBC
        let err_both = mgr.allocate(
            1080,
            2400,
            PixelFormat::Rgba8888,
            usage::QCOM_USAGE_UBWC | usage::ARM_USAGE_AFBC,
        );
        assert_eq!(
            err_both.unwrap_err(),
            GrallocError::UnsupportedUsage(usage::QCOM_USAGE_UBWC | usage::ARM_USAGE_AFBC)
        );

        // UBWC on unsupported format (YV12)
        let err_yv12_ubwc = mgr.allocate(1920, 1080, PixelFormat::Yv12, usage::QCOM_USAGE_UBWC);
        assert_eq!(
            err_yv12_ubwc.unwrap_err(),
            GrallocError::UnsupportedUsage(usage::QCOM_USAGE_UBWC)
        );

        // AFBC on unsupported format (NV12)
        let err_nv12_afbc = mgr.allocate(1920, 1080, PixelFormat::Nv12, usage::ARM_USAGE_AFBC);
        assert_eq!(
            err_nv12_afbc.unwrap_err(),
            GrallocError::UnsupportedUsage(usage::ARM_USAGE_AFBC)
        );
    }

    #[test]
    fn test_gralloc_detect_dma_heap() {
        // Function executes cleanly without panic
        let _ = GrallocManager::detect_dma_heap();
    }
}
