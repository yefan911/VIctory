//! The DXIL program inside a container's `DXIL` (or `STAT`) part.
//!
//! ```text
//! 0x00  program version u32   (kind << 16 | major << 4 | minor)
//! 0x04  size in dwords u32
//! 0x08  "DXIL"                <- bitcode offset is relative to here
//! 0x0C  dxil version u32
//! 0x10  bitcode offset u32    (16 in standard PC DXIL)
//! 0x14  bitcode size u32
//! ```

use crate::ShaderError;
use crate::dxbc::u32_at;

const LLVM_BITCODE_MAGIC: [u8; 4] = [0x42, 0x43, 0xC0, 0xDE];
const BITCODE_HEADER_SIZE: usize = 16;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ShaderKind {
    Pixel,
    Vertex,
    Geometry,
    Hull,
    Domain,
    Compute,
    Library,
    RayGeneration,
    Intersection,
    AnyHit,
    ClosestHit,
    Miss,
    Callable,
    Mesh,
    Amplification,
    Node,
    Unknown(u16),
}

impl ShaderKind {
    fn from_raw(v: u16) -> Self {
        use ShaderKind::*;
        match v {
            0 => Pixel,
            1 => Vertex,
            2 => Geometry,
            3 => Hull,
            4 => Domain,
            5 => Compute,
            6 => Library,
            7 => RayGeneration,
            8 => Intersection,
            9 => AnyHit,
            10 => ClosestHit,
            11 => Miss,
            12 => Callable,
            13 => Mesh,
            14 => Amplification,
            15 => Node,
            other => Unknown(other),
        }
    }
}

#[derive(Debug, Clone)]
pub struct DxilProgram<'a> {
    pub kind: ShaderKind,
    pub shader_model: (u8, u8),
    pub dxil_version: u32,
    /// LLVM bitcode (starts with `BC C0 DE`).
    pub bitcode: &'a [u8],
    /// Bytes between the bitcode header and the bitcode. Empty on PC.
    pub prefix: &'a [u8],
    /// Bytes after the bitcode, including any padding to a dword boundary.
    pub trailer: &'a [u8],
}

/// Xbox additions recognised around the bitcode.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct XboxExtras {
    /// The prefix starts with an `XDAT` table.
    pub xdat: bool,
    /// The trailer contains an `xbop` blob (compiler options).
    pub xbop: bool,
}

impl<'a> DxilProgram<'a> {
    pub fn parse(part: &'a [u8]) -> Result<Self, ShaderError> {
        let version = u32_at(part, 0)?;
        let size = (u32_at(part, 4)? as usize)
            .checked_mul(4)
            .filter(|&s| s <= part.len() && s >= 24)
            .ok_or(ShaderError::Malformed(
                "program size disagrees with part size",
            ))?;
        let part = &part[..size];
        if part.get(8..12) != Some(b"DXIL") {
            return Err(ShaderError::Malformed("missing DXIL magic"));
        }
        let dxil_version = u32_at(part, 12)?;
        let offset = u32_at(part, 16)? as usize;
        let len = u32_at(part, 20)? as usize;
        let (header_and_prefix, payload) = part[8..]
            .split_at_checked(offset)
            .ok_or(ShaderError::Malformed("bitcode out of bounds"))?;
        let prefix = header_and_prefix
            .get(BITCODE_HEADER_SIZE..)
            .ok_or(ShaderError::Malformed("bitcode offset too small"))?;
        let (bitcode, trailer) = payload
            .split_at_checked(len)
            .ok_or(ShaderError::Malformed("bitcode out of bounds"))?;
        if !bitcode.starts_with(&LLVM_BITCODE_MAGIC) {
            return Err(ShaderError::Malformed("bitcode lacks LLVM magic"));
        }
        Ok(DxilProgram {
            kind: ShaderKind::from_raw((version >> 16) as u16),
            shader_model: (((version >> 4) & 0xF) as u8, (version & 0xF) as u8),
            dxil_version,
            bitcode,
            prefix,
            trailer,
        })
    }

    /// True for a program laid out exactly like PC DXIL.
    pub fn is_standard_layout(&self) -> bool {
        let padding = self.bitcode.len().next_multiple_of(4) - self.bitcode.len();
        self.prefix.is_empty() && self.trailer == &[0; 3][..padding]
    }

    pub fn xbox_extras(&self) -> XboxExtras {
        XboxExtras {
            xdat: self.prefix.starts_with(b"XDAT"),
            xbop: self.trailer.windows(4).any(|w| w == b"xbop"),
        }
    }
}

/// Builds a standard PC-layout program around `bitcode` (test helper).
#[cfg(test)]
pub fn build_standard(kind: u16, sm: (u8, u8), dxil_version: u32, bitcode: &[u8]) -> Vec<u8> {
    assert!(
        sm.0 < 16 && sm.1 < 16,
        "shader model fields must fit in a nibble"
    );
    let bitcode_size = u32::try_from(bitcode.len()).expect("bitcode size must fit in a DWORD");
    let mut out = Vec::new();
    let version = (u32::from(kind) << 16) | (u32::from(sm.0) << 4) | u32::from(sm.1);
    let size_dwords = 6 + bitcode_size.div_ceil(4);
    out.extend_from_slice(&version.to_le_bytes());
    out.extend_from_slice(&size_dwords.to_le_bytes());
    out.extend_from_slice(b"DXIL");
    out.extend_from_slice(&dxil_version.to_le_bytes());
    out.extend_from_slice(&(BITCODE_HEADER_SIZE as u32).to_le_bytes());
    out.extend_from_slice(&bitcode_size.to_le_bytes());
    out.extend_from_slice(bitcode);
    out.resize(out.len().next_multiple_of(4), 0);
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fake_bitcode() -> Vec<u8> {
        let mut b = LLVM_BITCODE_MAGIC.to_vec();
        b.extend_from_slice(&[0xAA; 12]);
        b
    }

    #[test]
    fn parses_standard_layout() {
        let bytes = build_standard(0, (6, 0), 0x100, &fake_bitcode());
        let p = DxilProgram::parse(&bytes).unwrap();
        assert_eq!(p.kind, ShaderKind::Pixel);
        assert_eq!(p.shader_model, (6, 0));
        assert!(p.is_standard_layout());
        assert_eq!(p.xbox_extras(), XboxExtras::default());
    }

    #[test]
    fn header_matches_wire_format() {
        let bytes = build_standard(0x1234, (6, 10), 0x10a, &LLVM_BITCODE_MAGIC);
        assert_eq!(
            bytes,
            [
                0x6a, 0, 0x34, 0x12, 7, 0, 0, 0, b'D', b'X', b'I', b'L', 0x0a, 1, 0, 0, 16, 0, 0,
                0, 4, 0, 0, 0, 0x42, 0x43, 0xc0, 0xde,
            ]
        );
        let program = DxilProgram::parse(&bytes).unwrap();
        assert_eq!(program.kind, ShaderKind::Unknown(0x1234));
        assert_eq!(program.shader_model, (6, 10));
        assert_eq!(program.dxil_version, 0x10a);
        assert_eq!(program.bitcode, LLVM_BITCODE_MAGIC);
    }

    #[test]
    fn round_trips_each_dword_padding_length() {
        let payload = fake_bitcode();
        for (len, padding) in [(4, 0), (5, 3), (6, 2), (7, 1)] {
            let mut bytes = build_standard(15, (6, 8), 0x108, &payload[..len]);
            bytes.extend_from_slice(b"xbop"); // Outside the declared program.
            let program = DxilProgram::parse(&bytes).unwrap();
            assert_eq!(program.kind, ShaderKind::Node);
            assert_eq!(program.bitcode, &payload[..len]);
            assert_eq!(program.trailer, &[0; 3][..padding]);
            assert!(program.is_standard_layout());
            assert_eq!(program.xbox_extras(), XboxExtras::default());
        }
    }

    #[test]
    fn distinguishes_extra_trailer_bytes_from_padding() {
        let mut bytes = build_standard(0, (6, 0), 0x100, &fake_bitcode()[..7]);
        bytes[31] = 1;
        assert!(!DxilProgram::parse(&bytes).unwrap().is_standard_layout());
        bytes[31] = 0;
        bytes.extend_from_slice(&[0; 4]);
        bytes[4..8].copy_from_slice(&9u32.to_le_bytes());
        assert!(!DxilProgram::parse(&bytes).unwrap().is_standard_layout());
    }

    #[test]
    fn rejects_truncation_and_invalid_header_fields() {
        let bytes = build_standard(0, (6, 0), 0x100, &LLVM_BITCODE_MAGIC);
        for len in 0..bytes.len() {
            assert!(DxilProgram::parse(&bytes[..len]).is_err(), "length {len}");
        }
        for (at, value) in [
            (4, 5),
            (4, 6),
            (4, u32::MAX), // Program length in dwords.
            (8, 0),        // DXIL magic.
            (16, 15),
            (16, 21),
            (16, u32::MAX), // Relative byte offset.
            (20, 0),
            (20, 3),
            (20, 5),
            (20, u32::MAX), // Bitcode byte length.
        ] {
            let mut invalid = bytes.clone();
            invalid[at..at + 4].copy_from_slice(&value.to_le_bytes());
            assert!(
                DxilProgram::parse(&invalid).is_err(),
                "field {at}, value {value}"
            );
        }
    }

    #[test]
    fn detects_xbox_layout() {
        // Xbox layout: XDAT table before the bitcode, xbop blob after it.
        let bitcode = fake_bitcode();
        let xdat = b"XDAT\x01\x00\x00\x00";
        let trailer = b"xbop\x02\x00\x00\x00";
        let mut bytes = Vec::new();
        let size = 24 + xdat.len() + bitcode.len() + trailer.len();
        bytes.extend_from_slice(&((5u32 << 16) | 0x65).to_le_bytes()); // cs_6_5
        bytes.extend_from_slice(&((size / 4) as u32).to_le_bytes());
        bytes.extend_from_slice(b"DXIL");
        bytes.extend_from_slice(&0x105u32.to_le_bytes());
        bytes.extend_from_slice(&(16 + xdat.len() as u32).to_le_bytes());
        bytes.extend_from_slice(&(bitcode.len() as u32).to_le_bytes());
        bytes.extend_from_slice(xdat);
        bytes.extend_from_slice(&bitcode);
        bytes.extend_from_slice(trailer);

        let p = DxilProgram::parse(&bytes).unwrap();
        assert_eq!(p.kind, ShaderKind::Compute);
        assert_eq!(p.shader_model, (6, 5));
        assert!(!p.is_standard_layout());
        assert_eq!(
            p.xbox_extras(),
            XboxExtras {
                xdat: true,
                xbop: true
            }
        );
        assert_eq!(p.prefix, xdat);
        assert_eq!(p.bitcode, &bitcode[..]);
        assert_eq!(p.trailer, trailer);
    }

    #[test]
    fn rejects_missing_bitcode_magic() {
        let bytes = build_standard(0, (6, 0), 0x100, &[0u8; 16]);
        assert!(DxilProgram::parse(&bytes).is_err());
    }
}
