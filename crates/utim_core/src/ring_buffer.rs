//! Fixed-size bounded ring buffer for zero-heap logging and IPC datagrams.

/// A fixed-size byte ring buffer with capacity `N`.
pub struct ByteRingBuffer<const N: usize> {
    buf: [u8; N],
    head: usize,
    tail: usize,
    count: usize,
}

impl<const N: usize> Default for ByteRingBuffer<N> {
    fn default() -> Self {
        Self::new()
    }
}

impl<const N: usize> ByteRingBuffer<N> {
    pub const fn new() -> Self {
        Self {
            buf: [0u8; N],
            head: 0,
            tail: 0,
            count: 0,
        }
    }

    #[inline]
    pub fn capacity(&self) -> usize {
        N
    }

    #[inline]
    pub fn len(&self) -> usize {
        self.count
    }

    #[inline]
    pub fn is_empty(&self) -> bool {
        self.count == 0
    }

    #[inline]
    pub fn is_full(&self) -> bool {
        self.count == N
    }

    /// Push a byte, dropping oldest if full.
    pub fn push_overwrite(&mut self, byte: u8) {
        if self.count == N {
            // Overwrite oldest
            self.buf[self.tail] = byte;
            self.tail = (self.tail + 1) % N;
            self.head = (self.head + 1) % N;
        } else {
            self.buf[self.tail] = byte;
            self.tail = (self.tail + 1) % N;
            self.count += 1;
        }
    }

    /// Push a slice of bytes, overwriting oldest if capacity exceeded.
    pub fn write_overwrite(&mut self, data: &[u8]) {
        for &b in data {
            self.push_overwrite(b);
        }
    }

    /// Read available bytes into output slice. Returns number of bytes read.
    pub fn read(&mut self, out: &mut [u8]) -> usize {
        let to_read = out.len().min(self.count);
        for item in out.iter_mut().take(to_read) {
            *item = self.buf[self.head];
            self.head = (self.head + 1) % N;
        }
        self.count -= to_read;
        to_read
    }

    /// Read all available bytes into a String (for status/logs).
    pub fn to_string_lossy(&self) -> String {
        let mut s = String::with_capacity(self.count);
        let mut idx = self.head;
        for _ in 0..self.count {
            s.push(self.buf[idx] as char);
            idx = (idx + 1) % N;
        }
        s
    }

    /// Clear the ring buffer.
    pub fn clear(&mut self) {
        self.head = 0;
        self.tail = 0;
        self.count = 0;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_ring_buffer_basic() {
        let mut rb = ByteRingBuffer::<8>::new();
        assert_eq!(rb.len(), 0);
        assert!(rb.is_empty());

        rb.write_overwrite(b"hello");
        assert_eq!(rb.len(), 5);
        assert_eq!(rb.to_string_lossy(), "hello");

        let mut out = [0u8; 3];
        let n = rb.read(&mut out);
        assert_eq!(n, 3);
        assert_eq!(&out, b"hel");
        assert_eq!(rb.len(), 2);
        assert_eq!(rb.to_string_lossy(), "lo");

        rb.write_overwrite(b"world!");
        // buffer has 'lo' (2) + 'world!' (6) = 8 bytes (full)
        assert_eq!(rb.len(), 8);
        assert_eq!(rb.to_string_lossy(), "loworld!");

        // Overwrite by writing 3 more bytes
        rb.write_overwrite(b"123");
        assert_eq!(rb.len(), 8);
        assert_eq!(rb.to_string_lossy(), "orld!123");
    }
}
