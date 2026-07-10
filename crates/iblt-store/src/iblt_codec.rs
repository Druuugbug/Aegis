//! # IBLT Store - Codec Layer
//!
//! Store-level codec with tier metadata, versioned headers, and checksums.
use iblt::compact::CompactIblt;
use iblt::traits::IbltCodec;
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[repr(u8)]
pub enum StorageTierCode { Hot = 1, Warm = 2, Cold = 3 }
impl StorageTierCode { pub fn from_u8(v: u8) -> Option<Self> { match v { 1 => Some(Self::Hot), 2 => Some(Self::Warm), 3 => Some(Self::Cold), _ => None } } }

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StoreCodecConfig { pub schema_version: u16, pub include_checksum: bool }
impl Default for StoreCodecConfig { fn default() -> Self { Self { schema_version: 1, include_checksum: true } } }

pub struct StoreCodec { config: StoreCodecConfig }
impl StoreCodec {
    pub fn new(config: StoreCodecConfig) -> Self { Self { config } }
    pub fn encode(&self, iblt: &CompactIblt, tier: StorageTierCode) -> Vec<u8> {
        let data = iblt.encode();
        let mut r = Vec::new();
        r.push(0x53); r.push(0x49); r.push(self.config.schema_version as u8); r.push(tier as u8);
        r.extend_from_slice(&(data.len() as u32).to_le_bytes());
        if self.config.include_checksum { let crc = simple_checksum(&data); r.extend_from_slice(&crc.to_le_bytes()); }
        r.extend_from_slice(&data);
        r
    }
    pub fn decode(&self, data: &[u8]) -> anyhow::Result<(CompactIblt, StorageTierCode, u16)> {
        if data.len() < 8 { anyhow::bail!("too short"); }
        if data[0] != 0x53 || data[1] != 0x49 { anyhow::bail!("bad marker"); }
        let sv = data[2] as u16;
        let tier = StorageTierCode::from_u8(data[3]).ok_or_else(|| anyhow::anyhow!("bad tier"))?;
        let plen = u32::from_le_bytes(data[4..8].try_into().unwrap()) as usize;
        let cs = if self.config.include_checksum { 4 } else { 0 };
        let ps = 8 + cs;
        if ps + plen > data.len() { anyhow::bail!("truncated"); }
        let payload = &data[ps..ps + plen];
        if self.config.include_checksum {
            let stored = u32::from_le_bytes(data[8..12].try_into().unwrap());
            if stored != simple_checksum(payload) { anyhow::bail!("checksum mismatch"); }
        }
        Ok((CompactIblt::decode_from(payload)?, tier, sv))
    }
}
impl Default for StoreCodec { fn default() -> Self { Self::new(StoreCodecConfig::default()) } }

fn simple_checksum(data: &[u8]) -> u32 { let mut h: u32 = 0x811c_9dc5; for &b in data { h ^= b as u32; h = h.wrapping_mul(0x0100_0193); } h }

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn test_store_codec_roundtrip() { let codec = StoreCodec::default(); let mut iblt = CompactIblt::new(64); iblt.insert(b"t", b"v"); let enc = codec.encode(&iblt, StorageTierCode::Hot); let (dec, tier, sv) = codec.decode(&enc).unwrap(); assert_eq!(tier, StorageTierCode::Hot); assert_eq!(sv, 1); }
    #[test]
    fn test_tier_codes() { assert_eq!(StorageTierCode::from_u8(1), Some(StorageTierCode::Hot)); assert_eq!(StorageTierCode::from_u8(0), None); }
}
