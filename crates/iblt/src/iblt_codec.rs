//! # IBLT Codec
//!
//! Wire encoding/decoding of IBLT tables with frame headers and checksums.
use crate::compact::CompactIblt;
use crate::encoder::{EncodingFormat, FrameHeader, IbltEncoder};
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CodecConfig {
    pub format: EncodingFormat,
    pub include_checksum: bool,
}
impl Default for CodecConfig {
    fn default() -> Self {
        Self {
            format: EncodingFormat::Framed,
            include_checksum: true,
        }
    }
}

pub struct IbltCodecImpl {
    config: CodecConfig,
    encoder: IbltEncoder,
}
impl IbltCodecImpl {
    pub fn new(config: CodecConfig) -> Self {
        let encoder = IbltEncoder::new(config.format);
        Self { config, encoder }
    }
    pub fn encode_table(&self, iblt: &CompactIblt) -> Vec<u8> {
        let cells = iblt.cells();
        let mut payload = Vec::new();
        for cell in cells {
            payload.extend_from_slice(&self.encoder.encode_cell(
                cell.key_hash,
                cell.value_hash,
                cell.count,
            ));
        }
        let header = FrameHeader {
            magic: FrameHeader::MAGIC,
            version: FrameHeader::VERSION,
            num_cells: cells.len() as u32,
            num_hashes: 3,
            payload_len: payload.len() as u32,
        };
        let mut result = Vec::new();
        result.extend_from_slice(&header.magic.to_le_bytes());
        result.push(header.version);
        result.extend_from_slice(&header.num_cells.to_le_bytes());
        result.push(header.num_hashes);
        result.extend_from_slice(&header.payload_len.to_le_bytes());
        result.extend_from_slice(&payload);
        if self.config.include_checksum {
            let crc = compute_crc32(&result);
            result.extend_from_slice(&crc.to_le_bytes());
        }
        result
    }
}
impl Default for IbltCodecImpl {
    fn default() -> Self {
        Self::new(CodecConfig::default())
    }
}

fn compute_crc32(data: &[u8]) -> u32 {
    let mut c: u32 = 0xFFFFFFFF;
    for &b in data {
        c ^= b as u32;
        for _ in 0..8 {
            if c & 1 != 0 {
                c = (c >> 1) ^ 0xEDB88320;
            } else {
                c >>= 1;
            }
        }
    }
    !c
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn test_encode_table() {
        let codec = IbltCodecImpl::default();
        let iblt = CompactIblt::new(16);
        let encoded = codec.encode_table(&iblt);
        assert!(encoded.len() > 14);
    }
    #[test]
    fn test_crc32() {
        let c1 = compute_crc32(b"hello");
        let c2 = compute_crc32(b"hello");
        assert_eq!(c1, c2);
        assert_ne!(c1, 0);
    }
}
