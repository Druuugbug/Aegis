//! # Encoder
//!
//! Encoding strategies for serializing IBLT data for network transmission.
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum EncodingFormat {
    Raw,
    Varint,
    #[default]
    Framed,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FrameHeader {
    pub magic: u32,
    pub version: u8,
    pub num_cells: u32,
    pub num_hashes: u8,
    pub payload_len: u32,
}
impl FrameHeader {
    pub const MAGIC: u32 = 0x4942_4C54;
    pub const VERSION: u8 = 1;
    pub const SIZE: usize = 14;
}

pub struct IbltEncoder {
    format: EncodingFormat,
}
impl IbltEncoder {
    pub fn new(format: EncodingFormat) -> Self {
        Self { format }
    }
    pub fn encode_cell(&self, key_hash: u64, value_hash: u64, count: i32) -> Vec<u8> {
        let mut buf = Vec::with_capacity(20);
        buf.extend_from_slice(&key_hash.to_le_bytes());
        buf.extend_from_slice(&value_hash.to_le_bytes());
        buf.extend_from_slice(&count.to_le_bytes());
        buf
    }
    pub fn format(&self) -> EncodingFormat {
        self.format
    }
}
impl Default for IbltEncoder {
    fn default() -> Self {
        Self::new(EncodingFormat::default())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn test_encoder_cell() {
        let e = IbltEncoder::default();
        let cell = e.encode_cell(0xDEADBEEF, 0xCAFEBABE, 42);
        assert_eq!(cell.len(), 20);
    }
}
