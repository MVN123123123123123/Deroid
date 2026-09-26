//! Camera HAL3 to /dev/v4l2loopback Bridge.
//! Feeds ISP frames from Android Camera HAL3 into the V4L2 virtual video
//! device negotiation state machine.
//!
//! H28 (simulation disclosure): this type performs no I/O at all — no fd,
//! no `VIDIOC_*` ioctl, no mmap, no poll. `device_path` is retained for
//! diagnostics only and never opened; all methods are in-memory mutations
//! of the negotiated format and buffer ring. Bind a real v4l2loopback fd
//! before exposing this to desktop consumers.
//! Conforms strictly to GEMINI.md systems discipline.

use super::hal3::CapturedFrame;

// --- Linux V4L2 Constants (linux/videodev2.h) ---
pub const V4L2_CAP_VIDEO_CAPTURE: u32 = 0x00000001;
pub const V4L2_CAP_STREAMING: u32 = 0x04000000;
pub const V4L2_CAP_READWRITE: u32 = 0x01000000;

pub const V4L2_PIX_FMT_NV12: u32 = 0x3231564E; // 'NV12'
pub const V4L2_PIX_FMT_YUYV: u32 = 0x56595559; // 'YUYV'
pub const V4L2_PIX_FMT_MJPEG: u32 = 0x47504A4D; // 'MJPG'

pub const V4L2_BUF_TYPE_VIDEO_CAPTURE: u32 = 1;
pub const V4L2_MEMORY_MMAP: u32 = 1;
pub const V4L2_MEMORY_USERPTR: u32 = 2;
pub const V4L2_MEMORY_DMABUF: u32 = 4;

/// V4L2 Capability Structure
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct V4l2Capability {
    pub driver: String,
    pub card: String,
    pub bus_info: String,
    pub version: u32,
    pub capabilities: u32,
    pub device_caps: u32,
}

/// V4L2 Format Structure
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct V4l2Format {
    pub width: u32,
    pub height: u32,
    pub pixelformat: u32,
    pub bytesperline: u32,
    pub sizeimage: u32,
}

/// V4L2 Buffer in Queue
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct V4l2Buffer {
    pub index: u32,
    pub bytesused: u32,
    pub flags: u32,
    pub sequence: u32,
    pub timestamp_ns: u64,
    pub queued: bool,
    /// H27: set by `feed_hal_frame`, cleared by `dequeue_buffer`. A
    /// legitimately empty (0-byte, EOF) frame is `ready` and dequeues;
    /// a queued-but-never-fed slot is not and never strands the pool.
    pub ready: bool,
}

/// V4L2 Loopback Virtual Device Bridge
pub struct V4l2LoopbackBridge {
    pub device_path: String,
    pub capability: V4l2Capability,
    pub format: V4l2Format,
    pub is_streaming: bool,
    pub allocated_buffers: Vec<V4l2Buffer>,
    pub frame_sequence: u32,
    pub frames_delivered: u64,
}

impl V4l2LoopbackBridge {
    pub fn new(device_path: &str) -> Self {
        Self {
            device_path: device_path.to_string(),
            capability: V4l2Capability {
                driver: "v4l2 loopback".to_string(),
                card: "Droid Camera HAL3 Loopback".to_string(),
                bus_info: "platform:v4l2loopback".to_string(),
                version: 0x00050400,
                capabilities: V4L2_CAP_VIDEO_CAPTURE | V4L2_CAP_STREAMING | V4L2_CAP_READWRITE,
                device_caps: V4L2_CAP_VIDEO_CAPTURE | V4L2_CAP_STREAMING | V4L2_CAP_READWRITE,
            },
            format: V4l2Format {
                width: 1920,
                height: 1080,
                pixelformat: V4L2_PIX_FMT_NV12,
                bytesperline: 1920,
                sizeimage: 1920 * 1080 * 3 / 2,
            },
            is_streaming: false,
            allocated_buffers: Vec::with_capacity(8),
            frame_sequence: 0,
            frames_delivered: 0,
        }
    }

    /// ioctl VIDIOC_QUERYCAP
    pub fn query_cap(&self) -> &V4l2Capability {
        &self.capability
    }

    /// ioctl VIDIOC_G_FMT
    pub fn get_format(&self) -> &V4l2Format {
        &self.format
    }

    /// ioctl VIDIOC_S_FMT
    pub fn set_format(
        &mut self,
        width: u32,
        height: u32,
        pixelformat: u32,
    ) -> Result<&V4l2Format, &'static str> {
        if self.is_streaming {
            return Err("Cannot change format while streaming");
        }
        // H8: reject zero dimensions; u64 math avoids overflow.
        if width == 0 || height == 0 {
            return Err("Stream dimensions must be non-zero");
        }

        let bytesperline = match pixelformat {
            V4L2_PIX_FMT_NV12 => width,
            V4L2_PIX_FMT_YUYV => width.checked_mul(2).ok_or("dimensions overflow u32")?,
            V4L2_PIX_FMT_MJPEG => width,
            _ => return Err("Unsupported V4L2 pixel format"),
        };

        let (w, h) = (width as u64, height as u64);
        let sizeimage_u64 = match pixelformat {
            V4L2_PIX_FMT_NV12 => w * h * 3 / 2,
            V4L2_PIX_FMT_YUYV => w * h * 2,
            V4L2_PIX_FMT_MJPEG => w * h / 4,
            _ => return Err("Unsupported V4L2 pixel format"),
        };
        let sizeimage: u32 = sizeimage_u64
            .try_into()
            .map_err(|_| "frame size exceeds u32")?;

        self.format = V4l2Format {
            width,
            height,
            pixelformat,
            bytesperline,
            sizeimage,
        };

        Ok(&self.format)
    }

    /// ioctl VIDIOC_REQBUFS
    pub fn request_buffers(&mut self, count: u32) -> Result<u32, &'static str> {
        if self.is_streaming {
            return Err("Cannot request buffers while streaming");
        }
        self.allocated_buffers.clear();
        if count == 0 {
            // Per V4L2 spec, requesting 0 buffers frees all allocated buffers and returns Ok(0)
            return Ok(0);
        }
        let alloc_count = count.clamp(1, 16);
        for i in 0..alloc_count {
            self.allocated_buffers.push(V4l2Buffer {
                index: i,
                bytesused: 0,
                flags: 0,
                sequence: 0,
                timestamp_ns: 0,
                queued: false,
                ready: false,
            });
        }
        Ok(alloc_count)
    }

    /// ioctl VIDIOC_QBUF
    pub fn queue_buffer(&mut self, index: u32) -> Result<(), &'static str> {
        let buf = self
            .allocated_buffers
            .iter_mut()
            .find(|b| b.index == index)
            .ok_or("Invalid buffer index")?;
        buf.queued = true;
        Ok(())
    }

    /// ioctl VIDIOC_DQBUF: dequeue next available frame in strict FIFO sequence
    /// order into caller-supplied `out`.
    ///
    /// H27: readiness is tracked separately from `bytesused`, so a legitimate
    /// 0-byte frame (V4L2 EOF/stream-end event) dequeues instead of stranding
    /// its slot forever; the old `bytesused > 0` predicate is gone. H29: the
    /// descriptor is copied into `out` (plain scalar copy, no heap) instead
    /// of returning a clone per frame.
    pub fn dequeue_buffer(&mut self, out: &mut V4l2Buffer) -> Result<(), &'static str> {
        let best_idx = self
            .allocated_buffers
            .iter()
            .enumerate()
            .filter(|(_, b)| b.queued && b.ready)
            .min_by_key(|(_, b)| b.sequence)
            .map(|(idx, _)| idx)
            .ok_or("No queued buffer with data ready")?;

        let buf = &mut self.allocated_buffers[best_idx];
        buf.queued = false;
        buf.ready = false;
        *out = buf.clone();
        buf.bytesused = 0;
        Ok(())
    }

    /// ioctl VIDIOC_STREAMON
    pub fn stream_on(&mut self) -> Result<(), &'static str> {
        if self.allocated_buffers.is_empty() {
            return Err("Buffers not allocated");
        }
        self.is_streaming = true;
        Ok(())
    }

    /// ioctl VIDIOC_STREAMOFF
    pub fn stream_off(&mut self) {
        self.is_streaming = false;
        for buf in self.allocated_buffers.iter_mut() {
            buf.queued = false;
            buf.bytesused = 0;
        }
    }

    /// Push a frame from Camera HAL3 into the V4L2 buffer ring
    pub fn feed_hal_frame(
        &mut self,
        frame: &CapturedFrame,
    ) -> Result<u32, &'static str> {
        if !self.is_streaming {
            return Err("V4L2 loopback is not streaming");
        }

        if frame.width != self.format.width || frame.height != self.format.height {
            return Err("Frame resolution does not match negotiated V4L2 format");
        }
        // H9: format, stride and size must all fit the negotiated sizeimage.
        // bytesused is the length a consumer trusts, so an oversize frame
        // would advertise an out-of-bounds read.
        let expected_pix = match frame.format {
            super::hal3::CameraPixelFormat::Nv12
            | super::hal3::CameraPixelFormat::Yuv420Planar => V4L2_PIX_FMT_NV12,
            super::hal3::CameraPixelFormat::Yuyv => V4L2_PIX_FMT_YUYV,
            super::hal3::CameraPixelFormat::JpegBlob => V4L2_PIX_FMT_MJPEG,
            super::hal3::CameraPixelFormat::RawSensor => V4L2_PIX_FMT_YUYV,
        };
        if expected_pix != self.format.pixelformat {
            return Err("Frame pixel format does not match negotiated V4L2 format");
        }
        if frame.stride < self.format.bytesperline {
            return Err("Frame stride is narrower than the negotiated bytesperline");
        }
        if frame.buffer_size > self.format.sizeimage as usize {
            return Err("Frame buffer_size exceeds negotiated sizeimage");
        }

        let buf = self
            .allocated_buffers
            .iter_mut()
            .find(|b| b.queued && !b.ready)
            .ok_or("Buffer starvation: no empty queued buffer available")?;

        self.frame_sequence += 1;
        self.frames_delivered += 1;

        buf.bytesused = frame.buffer_size as u32;
        buf.sequence = self.frame_sequence;
        buf.timestamp_ns = frame.timestamp_ns;
        buf.ready = true;

        Ok(buf.index)
    }
}
