//! ELF64 segment alignment inspector and validator.
//! Validates that all PT_LOAD segments are aligned to at least 64 KB (0x10000)
//! to guarantee compatibility with Android 15+ 16 KB page size kernels and 64 KB kernels.

use std::fs::File;
use std::path::Path;

pub const REQUIRED_PAGE_ALIGNMENT: u64 = 65536; // 64 KB

pub const PT_LOAD: u32 = 1;
pub const PT_DYNAMIC: u32 = 2;
pub const PT_INTERP: u32 = 3;
pub const PT_NOTE: u32 = 4;
pub const PT_SHLIB: u32 = 5;
pub const PT_PHDR: u32 = 6;
pub const PT_TLS: u32 = 7;
pub const PT_GNU_EH_FRAME: u32 = 0x6474e550;
pub const PT_GNU_STACK: u32 = 0x6474e551;
pub const PT_GNU_RELRO: u32 = 0x6474e552;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ElfLoadSegment {
    pub index: usize,
    pub offset: u64,
    pub vaddr: u64,
    pub paddr: u64,
    pub filesz: u64,
    pub memsz: u64,
    pub flags: u32,
    pub align: u64,
    pub is_aligned_64k: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ElfAlignmentReport {
    pub path: Option<String>,
    pub is_64bit: bool,
    pub is_little_endian: bool,
    pub total_segments: usize,
    pub load_segments: Vec<ElfLoadSegment>,
    pub min_load_align: u64,
    pub is_64k_compatible: bool,
    /// Path recorded in PT_INTERP, if any. A dynamically linked binary is only
    /// as 64K-safe as its loader, which this check does not (and cannot) verify.
    pub interpreter: Option<String>,
}

impl ElfAlignmentReport {
    /// True when PT_INTERP is present: 64K safety of the loader/ld.so was NOT
    /// verified, so a build gate must not pass the binary on this report alone.
    pub fn dynamic_without_loader_check(&self) -> bool {
        self.interpreter.is_some()
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ElfAlignError {
    IoError(String),
    InvalidMagic,
    Not64Bit,
    InvalidHeader(String),
    InsufficientAlignment {
        segment_index: usize,
        align: u64,
        required: u64,
        vaddr: u64,
    },
    VaddrOffsetMismatch {
        segment_index: usize,
        vaddr: u64,
        offset: u64,
        align: u64,
    },
    DynamicWithoutLoaderCheck {
        interpreter: String,
    },
}

impl std::fmt::Display for ElfAlignError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ElfAlignError::IoError(s) => write!(f, "I/O error: {}", s),
            ElfAlignError::InvalidMagic => write!(f, "Invalid ELF magic bytes"),
            ElfAlignError::Not64Bit => write!(f, "Only ELF64 (AArch64 / x86_64) binaries are supported"),
            ElfAlignError::InvalidHeader(s) => write!(f, "Invalid ELF header: {}", s),
            ElfAlignError::InsufficientAlignment {
                segment_index,
                align,
                required,
                vaddr,
            } => write!(
                f,
                "PT_LOAD segment {} at vaddr 0x{:x} has alignment 0x{:x} ({} B), which is less than required 0x{:x} ({} B). Will fail with EINVAL or SIGSEGV on 16 KB kernels!",
                segment_index, vaddr, align, align, required, required
            ),
            ElfAlignError::VaddrOffsetMismatch {
                segment_index,
                vaddr,
                offset,
                align,
            } => write!(
                f,
                "PT_LOAD segment {} alignment constraint violated: (vaddr 0x{:x} - offset 0x{:x}) % align 0x{:x} != 0",
                segment_index, vaddr, offset, align
            ),
            ElfAlignError::DynamicWithoutLoaderCheck { interpreter } => write!(
                f,
                "dynamically linked via {interpreter}: 64K safety of the loader/ld.so was NOT verified; a 4 KB-aligned loader fails on 64 KB-page kernels even when this binary passes"
            ),
        }
    }
}

impl std::error::Error for ElfAlignError {}

/// Parse and validate ELF segment alignment from an in-memory byte slice.
pub fn inspect_elf_bytes(
    bytes: &[u8],
    path: Option<&str>,
) -> Result<ElfAlignmentReport, ElfAlignError> {
    if bytes.len() < 64 {
        return Err(ElfAlignError::InvalidHeader(
            "File is too small for ELF64 header".into(),
        ));
    }

    // Verify ELF magic: 0x7F, 'E', 'L', 'F'
    if &bytes[0..4] != b"\x7fELF" {
        return Err(ElfAlignError::InvalidMagic);
    }

    // EI_CLASS: 2 = ELF64
    if bytes[4] != 2 {
        return Err(ElfAlignError::Not64Bit);
    }

    // EI_DATA: 1 = Little-endian
    let is_little_endian = bytes[5] == 1;
    if !is_little_endian {
        return Err(ElfAlignError::InvalidHeader(
            "Only little-endian ELF is supported".into(),
        ));
    }

    let read_u16 =
        |offset: usize| -> u16 { u16::from_le_bytes([bytes[offset], bytes[offset + 1]]) };

    let read_u32 = |offset: usize| -> u32 {
        u32::from_le_bytes([
            bytes[offset],
            bytes[offset + 1],
            bytes[offset + 2],
            bytes[offset + 3],
        ])
    };

    let read_u64 = |offset: usize| -> u64 {
        u64::from_le_bytes([
            bytes[offset],
            bytes[offset + 1],
            bytes[offset + 2],
            bytes[offset + 3],
            bytes[offset + 4],
            bytes[offset + 5],
            bytes[offset + 6],
            bytes[offset + 7],
        ])
    };

    let e_phoff = read_u64(32) as usize;
    let e_phentsize = read_u16(54) as usize;
    let e_phnum = read_u16(56) as usize;

    if e_phentsize < 56 {
        return Err(ElfAlignError::InvalidHeader(format!(
            "Invalid program header entry size: {}",
            e_phentsize
        )));
    }

    let ph_table_end = e_phoff.checked_add(e_phnum * e_phentsize).ok_or_else(|| {
        ElfAlignError::InvalidHeader("Program header table offset overflow".into())
    })?;

    if ph_table_end > bytes.len() {
        return Err(ElfAlignError::InvalidHeader(
            "Program header table exceeds file size".into(),
        ));
    }

    let mut load_segments = Vec::new();
    let mut min_load_align = u64::MAX;
    let mut is_64k_compatible = true;
    let mut interpreter: Option<String> = None;

    for i in 0..e_phnum {
        let entry_offset = e_phoff + (i * e_phentsize);
        let p_type = read_u32(entry_offset);
        let p_flags = read_u32(entry_offset + 4);
        let p_offset = read_u64(entry_offset + 8);
        let p_vaddr = read_u64(entry_offset + 16);
        let p_paddr = read_u64(entry_offset + 24);
        let p_filesz = read_u64(entry_offset + 32);
        let p_memsz = read_u64(entry_offset + 40);
        let p_align = read_u64(entry_offset + 48);

        if p_type == PT_INTERP {
            // Best-effort: the interpreter string lives outside the program
            // header table, so callers that pass a truncated window (see
            // inspect_elf_file) may not have it in `bytes`.
            if p_filesz > 0 && p_filesz <= 4096 {
                if let Ok(s) = usize::try_from(p_offset) {
                    if let Some(e) = s.checked_add(p_filesz as usize) {
                        if e <= bytes.len() {
                            let z = bytes[s..e].iter().position(|&b| b == 0).unwrap_or(e - s);
                            interpreter =
                                Some(String::from_utf8_lossy(&bytes[s..s + z]).into_owned());
                        }
                    }
                }
            }
        }

        if p_type == PT_LOAD {
            let is_pow2 = p_align > 0 && (p_align & (p_align - 1)) == 0;
            let aligned = p_align >= REQUIRED_PAGE_ALIGNMENT && is_pow2;
            if p_align < min_load_align {
                min_load_align = p_align;
            }
            if !aligned {
                is_64k_compatible = false;
            }

            // Standard ELF requirement: (vaddr - offset) % align == 0
            if p_align > 1 {
                let vaddr_mod = p_vaddr % p_align;
                let offset_mod = p_offset % p_align;
                if vaddr_mod != offset_mod {
                    return Err(ElfAlignError::VaddrOffsetMismatch {
                        segment_index: i,
                        vaddr: p_vaddr,
                        offset: p_offset,
                        align: p_align,
                    });
                }
            }

            load_segments.push(ElfLoadSegment {
                index: i,
                offset: p_offset,
                vaddr: p_vaddr,
                paddr: p_paddr,
                filesz: p_filesz,
                memsz: p_memsz,
                flags: p_flags,
                align: p_align,
                is_aligned_64k: aligned,
            });
        }
    }

    if load_segments.is_empty() {
        min_load_align = 0;
        is_64k_compatible = false;
    }

    Ok(ElfAlignmentReport {
        path: path.map(|s| s.to_string()),
        is_64bit: true,
        is_little_endian: true,
        total_segments: e_phnum,
        load_segments,
        min_load_align,
        is_64k_compatible,
        interpreter,
    })
}

/// Inspect ELF segment alignment from a file path on disk.
pub fn inspect_elf_file<P: AsRef<Path>>(path: P) -> Result<ElfAlignmentReport, ElfAlignError> {
    use std::io::{Read, Seek, SeekFrom};
    let p = path.as_ref();
    let path_str = p.to_string_lossy().to_string();

    let mut f = File::open(p).map_err(|e| ElfAlignError::IoError(e.to_string()))?;

    // 64-byte ELF64 header is all we need to find the program header table.
    let mut ehdr = [0u8; 64];
    f.read_exact(&mut ehdr)
        .map_err(|e| ElfAlignError::IoError(e.to_string()))?;
    let u16at = |b: &[u8], o: usize| u16::from_le_bytes([b[o], b[o + 1]]);
    let u32at = |b: &[u8], o: usize| u32::from_le_bytes([b[o], b[o + 1], b[o + 2], b[o + 3]]);
    let u64at = |b: &[u8], o: usize| {
        u64::from_le_bytes([
            b[o],
            b[o + 1],
            b[o + 2],
            b[o + 3],
            b[o + 4],
            b[o + 5],
            b[o + 6],
            b[o + 7],
        ])
    };
    let e_phoff = u64at(&ehdr, 32) as usize;
    let e_phentsize = u16at(&ehdr, 54) as usize;
    let mut e_phnum = u16at(&ehdr, 56) as usize;
    if e_phentsize < 56 {
        return Err(ElfAlignError::InvalidHeader(format!(
            "bad e_phentsize {e_phentsize}"
        )));
    }
    // PN_XNUM (0xFFFF): the real program-header count lives in
    // shdr[0].sh_info. ELF64 sh_info is at shdr + 44 (sh_name:4, sh_type:4,
    // flags:8, addr:8, offset:8, size:8, link:4, info:4). `e_phnum == 0`
    // genuinely means zero segments and must not take this path.
    if e_phnum == 0xFFFF {
        let e_shoff = u64at(&ehdr, 40);
        let e_shentsize = u16at(&ehdr, 58) as u64;
        if e_shoff == 0 || e_shentsize < 64 {
            return Err(ElfAlignError::InvalidHeader(
                "PN_XNUM without a readable section header table".into(),
            ));
        }
        f.seek(SeekFrom::Start(e_shoff))
            .map_err(|e| ElfAlignError::IoError(e.to_string()))?;
        let mut shdr0 = [0u8; 64];
        f.read_exact(&mut shdr0)
            .map_err(|e| ElfAlignError::IoError(e.to_string()))?;
        let real = u32at(&shdr0, 44) as usize;
        if real == 0 {
            return Err(ElfAlignError::InvalidHeader(
                "PN_XNUM with zero sh_info count".into(),
            ));
        }
        e_phnum = real;
    }
    let end = e_phoff
        .checked_add(e_phnum.checked_mul(e_phentsize).ok_or_else(|| {
            ElfAlignError::InvalidHeader("program header table size overflow".into())
        })?)
        .ok_or_else(|| {
            ElfAlignError::InvalidHeader("program header table offset overflow".into())
        })?;

    // Reuse the single bounds-checked parser: header + phdrs in one small buffer.
    let mut bytes = vec![0u8; 64.max(end)];
    bytes[..64].copy_from_slice(&ehdr);
    if end > 64 {
        f.seek(SeekFrom::Start(64))
            .map_err(|e| ElfAlignError::IoError(e.to_string()))?;
        f.read_exact(&mut bytes[64..end])
            .map_err(|e| ElfAlignError::IoError(e.to_string()))?;
    }
    let mut report = inspect_elf_bytes(&bytes, Some(&path_str))?;
    // The PT_INTERP payload lives outside the header+phdr window, so read it
    // separately (bounded): without this a dynamic binary would be reported
    // as static whenever its interpreter string is past `end`.
    if report.interpreter.is_none() {
        for i in 0..e_phnum {
            let entry_offset = e_phoff + i * e_phentsize;
            if entry_offset + 56 > bytes.len() {
                break;
            }
            if u32at(&bytes, entry_offset) != PT_INTERP {
                continue;
            }
            let p_offset = u64at(&bytes, entry_offset + 8);
            let p_filesz = u64at(&bytes, entry_offset + 32);
            if p_filesz == 0 || p_filesz > 4096 {
                continue;
            }
            let mut buf = vec![0u8; p_filesz as usize];
            if f.seek(SeekFrom::Start(p_offset)).is_err() {
                continue;
            }
            if f.read_exact(&mut buf).is_err() {
                continue;
            }
            let z = buf.iter().position(|&b| b == 0).unwrap_or(buf.len());
            report.interpreter = Some(String::from_utf8_lossy(&buf[..z]).into_owned());
            break;
        }
    }
    Ok(report)
}

/// Assert that an ELF binary strictly complies with 64 KB page alignment.
/// Returns an error if any PT_LOAD segment has alignment < 65536.
pub fn verify_64k_alignment<P: AsRef<Path>>(path: P) -> Result<ElfAlignmentReport, ElfAlignError> {
    let report = inspect_elf_file(path)?;
    if report.load_segments.is_empty() {
        return Err(ElfAlignError::InvalidHeader(
            "ELF binary contains no PT_LOAD segments".into(),
        ));
    }
    // Do not claim 64K safety we have not proven: a dynamically linked binary
    // is only as 64K-safe as its loader, which this check does not verify.
    if let Some(interp) = &report.interpreter {
        return Err(ElfAlignError::DynamicWithoutLoaderCheck {
            interpreter: interp.clone(),
        });
    }
    for seg in &report.load_segments {
        if !seg.is_aligned_64k {
            return Err(ElfAlignError::InsufficientAlignment {
                segment_index: seg.index,
                align: seg.align,
                required: REQUIRED_PAGE_ALIGNMENT,
                vaddr: seg.vaddr,
            });
        }
    }
    Ok(report)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn create_mock_elf64(load_aligns: &[u64]) -> Vec<u8> {
        let mut bytes = vec![0u8; 64 + load_aligns.len() * 56];
        // Magic
        bytes[0..4].copy_from_slice(b"\x7fELF");
        bytes[4] = 2; // ELF64
        bytes[5] = 1; // Little endian
        bytes[6] = 1; // Version
        bytes[16] = 2; // ET_EXEC / ET_DYN

        let phoff = 64u64;
        bytes[32..40].copy_from_slice(&phoff.to_le_bytes());
        let phentsize = 56u16;
        bytes[54..56].copy_from_slice(&phentsize.to_le_bytes());
        let phnum = load_aligns.len() as u16;
        bytes[56..58].copy_from_slice(&phnum.to_le_bytes());

        for (i, &align) in load_aligns.iter().enumerate() {
            let offset = 64 + i * 56;
            let p_type = PT_LOAD;
            let p_flags = 5u32; // R-X
            let p_offset = (i as u64) * align;
            let base_vaddr = if align > 1 {
                (0x400000 / align) * align
            } else {
                0x400000
            };
            let p_vaddr = base_vaddr + (i as u64) * align;
            let p_paddr = p_vaddr;
            let p_filesz = 0x1000u64;
            let p_memsz = 0x1000u64;
            let p_align = align;

            bytes[offset..offset + 4].copy_from_slice(&p_type.to_le_bytes());
            bytes[offset + 4..offset + 8].copy_from_slice(&p_flags.to_le_bytes());
            bytes[offset + 8..offset + 16].copy_from_slice(&p_offset.to_le_bytes());
            bytes[offset + 16..offset + 24].copy_from_slice(&p_vaddr.to_le_bytes());
            bytes[offset + 24..offset + 32].copy_from_slice(&p_paddr.to_le_bytes());
            bytes[offset + 32..offset + 40].copy_from_slice(&p_filesz.to_le_bytes());
            bytes[offset + 40..offset + 48].copy_from_slice(&p_memsz.to_le_bytes());
            bytes[offset + 48..offset + 56].copy_from_slice(&p_align.to_le_bytes());
        }

        bytes
    }

    #[test]
    fn test_elf_align_success_64k() {
        let mock_elf = create_mock_elf64(&[65536, 65536]);
        let report = inspect_elf_bytes(&mock_elf, None).expect("Valid ELF");
        assert!(report.is_64k_compatible);
        assert_eq!(report.load_segments.len(), 2);
        assert_eq!(report.min_load_align, 65536);
    }

    #[test]
    fn test_elf_align_failure_4k() {
        let mock_elf = create_mock_elf64(&[4096, 65536]);
        let report = inspect_elf_bytes(&mock_elf, None).expect("Parsed ELF");
        assert!(!report.is_64k_compatible);
        assert_eq!(report.min_load_align, 4096);
        assert!(!report.load_segments[0].is_aligned_64k);
        assert!(report.load_segments[1].is_aligned_64k);
    }

    #[test]
    fn test_elf_align_invalid_magic() {
        let mut mock_elf = create_mock_elf64(&[65536]);
        mock_elf[0] = 0x00;
        let err = inspect_elf_bytes(&mock_elf, None).unwrap_err();
        assert_eq!(err, ElfAlignError::InvalidMagic);
    }

    #[test]
    fn test_elf_align_not_64bit() {
        let mut mock_elf = create_mock_elf64(&[65536]);
        mock_elf[4] = 1; // 32-bit
        let err = inspect_elf_bytes(&mock_elf, None).unwrap_err();
        assert_eq!(err, ElfAlignError::Not64Bit);
    }

    #[test]
    fn test_elf_align_non_power_of_two() {
        // Alignment is >= 64KB (65537) but NOT a power of two!
        let mock_elf = create_mock_elf64(&[65537]);
        let report = inspect_elf_bytes(&mock_elf, None).expect("Parsed ELF");
        assert!(
            !report.is_64k_compatible,
            "Non-power-of-two alignment must not be compatible"
        );
    }

    #[test]
    fn test_elf_align_no_load_segments() {
        let mock_elf = create_mock_elf64(&[]);
        let report = inspect_elf_bytes(&mock_elf, None).expect("Parsed ELF");
        assert!(
            !report.is_64k_compatible,
            "ELF without PT_LOAD must not be compatible"
        );
        assert_eq!(report.load_segments.len(), 0);
    }

    #[test]
    fn test_elf_verify_64k_rejects_empty_load_segments() {
        let temp = std::env::temp_dir().join("utim_test_empty_elf");
        let mock_elf = create_mock_elf64(&[]);
        std::fs::write(&temp, mock_elf).unwrap();

        let err = verify_64k_alignment(&temp).unwrap_err();
        match err {
            ElfAlignError::InvalidHeader(s) => {
                assert!(s.contains("no PT_LOAD segments"));
            }
            _ => panic!("Expected InvalidHeader error for empty PT_LOAD"),
        }

        let _ = std::fs::remove_file(&temp);
    }

    fn create_mock_elf64_with_interp(load_aligns: &[u64], interp: &str) -> Vec<u8> {
        let interp_bytes = interp.as_bytes();
        let phnum = load_aligns.len() + 1;
        let interp_off = (64 + phnum * 56) as u64;
        let mut bytes = vec![0u8; interp_off as usize + interp_bytes.len() + 1];
        bytes[0..4].copy_from_slice(b"\x7fELF");
        bytes[4] = 2;
        bytes[5] = 1;
        bytes[6] = 1;
        bytes[16] = 2;

        let phoff = 64u64;
        bytes[32..40].copy_from_slice(&phoff.to_le_bytes());
        bytes[54..56].copy_from_slice(&(56u16).to_le_bytes());
        bytes[56..58].copy_from_slice(&(phnum as u16).to_le_bytes());

        for (i, &align) in load_aligns.iter().enumerate() {
            let offset = 64 + i * 56;
            let p_offset = (i as u64) * align;
            let base_vaddr = if align > 1 {
                (0x400000 / align) * align
            } else {
                0x400000
            };
            let p_vaddr = base_vaddr + (i as u64) * align;
            bytes[offset..offset + 4].copy_from_slice(&PT_LOAD.to_le_bytes());
            bytes[offset + 4..offset + 8].copy_from_slice(&5u32.to_le_bytes());
            bytes[offset + 8..offset + 16].copy_from_slice(&p_offset.to_le_bytes());
            bytes[offset + 16..offset + 24].copy_from_slice(&p_vaddr.to_le_bytes());
            bytes[offset + 24..offset + 32].copy_from_slice(&p_vaddr.to_le_bytes());
            bytes[offset + 32..offset + 40].copy_from_slice(&0x1000u64.to_le_bytes());
            bytes[offset + 40..offset + 48].copy_from_slice(&0x1000u64.to_le_bytes());
            bytes[offset + 48..offset + 56].copy_from_slice(&align.to_le_bytes());
        }

        let offset = 64 + load_aligns.len() * 56;
        let filesz = interp_bytes.len() as u64 + 1;
        bytes[offset..offset + 4].copy_from_slice(&PT_INTERP.to_le_bytes());
        bytes[offset + 4..offset + 8].copy_from_slice(&4u32.to_le_bytes());
        bytes[offset + 8..offset + 16].copy_from_slice(&interp_off.to_le_bytes());
        bytes[offset + 32..offset + 40].copy_from_slice(&filesz.to_le_bytes());
        bytes[offset + 40..offset + 48].copy_from_slice(&filesz.to_le_bytes());
        bytes[offset + 48..offset + 56].copy_from_slice(&1u64.to_le_bytes());
        bytes[interp_off as usize..interp_off as usize + interp_bytes.len()]
            .copy_from_slice(interp_bytes);

        bytes
    }

    #[test]
    fn test_elf_interp_is_reported() {
        // S22: PT_INTERP must be reported, not silently skipped.
        let mock = create_mock_elf64_with_interp(&[65536], "/lib/ld-linux-aarch64.so.1");
        let report = inspect_elf_bytes(&mock, None).expect("Parsed ELF");
        assert!(report.is_64k_compatible);
        assert_eq!(
            report.interpreter.as_deref(),
            Some("/lib/ld-linux-aarch64.so.1")
        );
        assert!(report.dynamic_without_loader_check());
    }

    #[test]
    fn test_elf_static_binary_has_no_interpreter() {
        let mock_elf = create_mock_elf64(&[65536]);
        let report = inspect_elf_bytes(&mock_elf, None).expect("Parsed ELF");
        assert_eq!(report.interpreter, None);
        assert!(!report.dynamic_without_loader_check());
    }

    #[test]
    fn test_elf_verify_64k_errors_on_dynamic_binary() {
        // S22: a build gate must not pass a dynamic binary silently, even
        // when its own PT_LOADs are 64K-aligned.
        let temp = std::env::temp_dir().join("utim_test_dynamic_elf");
        let mock = create_mock_elf64_with_interp(&[65536, 65536], "/lib/ld-linux-aarch64.so.1");
        std::fs::write(&temp, mock).unwrap();

        // inspect_elf_file reads the interp payload past the phdr window.
        let report = inspect_elf_file(&temp).expect("Parsed ELF file");
        assert_eq!(
            report.interpreter.as_deref(),
            Some("/lib/ld-linux-aarch64.so.1")
        );

        let err = verify_64k_alignment(&temp).unwrap_err();
        match err {
            ElfAlignError::DynamicWithoutLoaderCheck { interpreter } => {
                assert_eq!(interpreter, "/lib/ld-linux-aarch64.so.1");
            }
            other => panic!("expected DynamicWithoutLoaderCheck, got {other}"),
        }

        let _ = std::fs::remove_file(&temp);
    }
}
