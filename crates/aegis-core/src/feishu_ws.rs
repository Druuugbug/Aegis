//! Feishu (Lark) long-connection (WebSocket) protocol codec.
//!
//! The Feishu open platform offers a **long-connection mode**: instead of the
//! platform POSTing events to a public webhook, the app dials *out* to Feishu
//! over WebSocket and events arrive on that connection. This is purely
//! outbound — no public IP, no inbound port, NAT-friendly — the ideal fit for a
//! small 1c1g box (mirrors Discord Gateway / Slack Socket Mode).
//!
//! This module is the **pure protocol layer**: the protobuf `Frame` codec, the
//! endpoint bootstrap (`/callback/ws/endpoint`), ping/ack frame builders and a
//! split-packet reassembler. It has no WebSocket dependency — the connection
//! orchestration lives in the `aegis` binary's gateway (which already depends
//! on `tokio-tungstenite`). Protocol per `larksuite/oapi-sdk-go` (`ws/`).

use anyhow::{anyhow, Result};
use std::collections::HashMap;
use std::time::Instant;

/// Frame `method` value for a control frame (ping/pong).
pub const METHOD_CONTROL: i32 = 0;
/// Frame `method` value for a data frame (event/card).
pub const METHOD_DATA: i32 = 1;

// ── varint (LEB128) ──────────────────────────────────────────────────────────

fn put_varint(buf: &mut Vec<u8>, mut v: u64) {
    loop {
        let mut b = (v & 0x7f) as u8;
        v >>= 7;
        if v != 0 {
            b |= 0x80;
        }
        buf.push(b);
        if v == 0 {
            break;
        }
    }
}

/// Read a base-128 varint at `*pos`, advancing `pos`. `None` on truncation.
fn read_varint(data: &[u8], pos: &mut usize) -> Option<u64> {
    let mut result: u64 = 0;
    let mut shift = 0u32;
    loop {
        let b = *data.get(*pos)?;
        *pos += 1;
        result |= ((b & 0x7f) as u64) << shift;
        if b & 0x80 == 0 {
            return Some(result);
        }
        shift += 7;
        if shift >= 64 {
            return None; // malformed: varint too long
        }
    }
}

fn put_tag(buf: &mut Vec<u8>, field: u32, wire: u8) {
    put_varint(buf, ((field as u64) << 3) | wire as u64);
}

fn put_len_delim(buf: &mut Vec<u8>, bytes: &[u8]) {
    put_varint(buf, bytes.len() as u64);
    buf.extend_from_slice(bytes);
}

// ── Header (proto2: 1=key string, 2=value string) ────────────────────────────

fn encode_header(key: &str, value: &str) -> Vec<u8> {
    let mut b = Vec::new();
    put_tag(&mut b, 1, 2);
    put_len_delim(&mut b, key.as_bytes());
    put_tag(&mut b, 2, 2);
    put_len_delim(&mut b, value.as_bytes());
    b
}

fn decode_header(data: &[u8]) -> Result<(String, String)> {
    let mut key = String::new();
    let mut value = String::new();
    let mut pos = 0;
    while pos < data.len() {
        let tag = read_varint(data, &mut pos).ok_or_else(|| anyhow!("header: bad tag"))?;
        let field = tag >> 3;
        let wire = (tag & 0x7) as u8;
        if wire != 2 {
            return Err(anyhow!("header: unexpected wire {wire}"));
        }
        let len = read_varint(data, &mut pos).ok_or_else(|| anyhow!("header: bad len"))? as usize;
        let end = pos.checked_add(len).filter(|e| *e <= data.len());
        let end = end.ok_or_else(|| anyhow!("header: len overflow"))?;
        let bytes = &data[pos..end];
        pos = end;
        match field {
            1 => key = String::from_utf8_lossy(bytes).into_owned(),
            2 => value = String::from_utf8_lossy(bytes).into_owned(),
            _ => {}
        }
    }
    Ok((key, value))
}

// ── Frame ────────────────────────────────────────────────────────────────────

/// A decoded/encodable Feishu long-connection frame.
#[derive(Debug, Clone, Default)]
pub struct Frame {
    /// Field 1: split-packet sequence id.
    pub seq_id: u64,
    /// Field 2: log id.
    pub log_id: u64,
    /// Field 3: service id (echoed from the connection URL's `service_id`).
    pub service: i32,
    /// Field 4: 0 = control (ping/pong), 1 = data (event/card).
    pub method: i32,
    /// Field 5: repeated key/value headers (`type`, `message_id`, `sum`, ...).
    pub headers: Vec<(String, String)>,
    /// Field 8: payload bytes (event JSON downstream / response JSON upstream).
    pub payload: Vec<u8>,
}

impl Frame {
    /// Get a header value by key.
    pub fn header(&self, key: &str) -> Option<&str> {
        self.headers
            .iter()
            .find(|(k, _)| k == key)
            .map(|(_, v)| v.as_str())
    }

    /// Get a header value parsed as an integer (0 when absent/unparseable).
    pub fn header_int(&self, key: &str) -> i64 {
        self.header(key).and_then(|v| v.parse().ok()).unwrap_or(0)
    }

    /// Encode to the wire format. Required fields (1–4) are always written
    /// (proto2 required semantics); optional payload (8) only when non-empty.
    pub fn encode(&self) -> Vec<u8> {
        let mut b = Vec::new();
        put_tag(&mut b, 1, 0);
        put_varint(&mut b, self.seq_id);
        put_tag(&mut b, 2, 0);
        put_varint(&mut b, self.log_id);
        put_tag(&mut b, 3, 0);
        put_varint(&mut b, self.service as u64);
        put_tag(&mut b, 4, 0);
        put_varint(&mut b, self.method as u64);
        for (k, v) in &self.headers {
            put_tag(&mut b, 5, 2);
            put_len_delim(&mut b, &encode_header(k, v));
        }
        if !self.payload.is_empty() {
            put_tag(&mut b, 8, 2);
            put_len_delim(&mut b, &self.payload);
        }
        b
    }

    /// Decode from the wire format. Unknown/optional fields (6,7,9) are skipped.
    pub fn decode(data: &[u8]) -> Result<Frame> {
        let mut f = Frame::default();
        let mut pos = 0;
        while pos < data.len() {
            let tag = read_varint(data, &mut pos).ok_or_else(|| anyhow!("frame: bad tag"))?;
            let field = tag >> 3;
            let wire = (tag & 0x7) as u8;
            match wire {
                0 => {
                    let v =
                        read_varint(data, &mut pos).ok_or_else(|| anyhow!("frame: bad varint"))?;
                    match field {
                        1 => f.seq_id = v,
                        2 => f.log_id = v,
                        3 => f.service = v as i32,
                        4 => f.method = v as i32,
                        _ => {}
                    }
                }
                2 => {
                    let len = read_varint(data, &mut pos)
                        .ok_or_else(|| anyhow!("frame: bad len"))?
                        as usize;
                    let end = pos
                        .checked_add(len)
                        .filter(|e| *e <= data.len())
                        .ok_or_else(|| anyhow!("frame: len overflow"))?;
                    let bytes = &data[pos..end];
                    pos = end;
                    match field {
                        5 => f.headers.push(decode_header(bytes)?),
                        8 => f.payload = bytes.to_vec(),
                        _ => {} // 6 payload_encoding, 7 payload_type, 9 LogIDNew
                    }
                }
                1 => pos += 8, // fixed64 (not expected) — skip
                5 => pos += 4, // fixed32 (not expected) — skip
                _ => return Err(anyhow!("frame: unknown wire type {wire}")),
            }
        }
        Ok(f)
    }
}

/// Build a ping control frame for the keep-alive loop.
pub fn build_ping_frame(service_id: i32) -> Vec<u8> {
    Frame {
        method: METHOD_CONTROL,
        service: service_id,
        headers: vec![("type".to_string(), "ping".to_string())],
        ..Default::default()
    }
    .encode()
}

/// Build the ack frame to return after handling a data frame: echo the inbound
/// frame's ids/method/headers (plus `biz_rt`) with a `{"code":200}` payload.
/// Feishu redelivers events that are not acked.
pub fn build_ack_frame(recv: &Frame, biz_rt_ms: i64) -> Vec<u8> {
    let mut headers = recv.headers.clone();
    headers.push(("biz_rt".to_string(), biz_rt_ms.to_string()));
    Frame {
        seq_id: recv.seq_id,
        log_id: recv.log_id,
        service: recv.service,
        method: recv.method,
        headers,
        payload: br#"{"code":200,"headers":null,"data":null}"#.to_vec(),
    }
    .encode()
}

// ── Endpoint bootstrap ────────────────────────────────────────────────────────

/// Parsed `/callback/ws/endpoint` result: where + how to connect.
#[derive(Debug, Clone)]
pub struct Endpoint {
    /// `wss://...` connection URL (carries `service_id`/`device_id` in query).
    pub url: String,
    /// `service_id` from the URL query — echoed in the `service` frame field.
    pub service_id: i32,
    /// `device_id` from the URL query (for logging/diagnostics).
    pub device_id: String,
    /// Server-advised ping interval in seconds (default 120 when unset).
    pub ping_interval_secs: u64,
}

/// Extract `service_id` and `device_id` from a connection URL's query string.
pub fn parse_conn_url(url: &str) -> (i32, String) {
    let mut service_id = 0i32;
    let mut device_id = String::new();
    if let Some(q) = url.split('?').nth(1) {
        for pair in q.split('&') {
            let mut it = pair.splitn(2, '=');
            match (it.next(), it.next()) {
                (Some("service_id"), Some(v)) => service_id = v.parse().unwrap_or(0),
                (Some("device_id"), Some(v)) => device_id = v.to_string(),
                _ => {}
            }
        }
    }
    (service_id, device_id)
}

/// Parse an endpoint bootstrap response body into an [`Endpoint`].
/// Separated from the HTTP call so it can be unit-tested without a network.
pub fn parse_endpoint_response(data: &serde_json::Value) -> Result<Endpoint> {
    let code = data["code"].as_i64().unwrap_or(-1);
    if code != 0 {
        let msg = data["msg"].as_str().unwrap_or("");
        return Err(anyhow!("feishu ws endpoint error code {code}: {msg}"));
    }
    let url = data["data"]["URL"]
        .as_str()
        .ok_or_else(|| anyhow!("feishu ws endpoint: missing data.URL"))?
        .to_string();
    let ping_interval_secs = data["data"]["ClientConfig"]["PingInterval"]
        .as_u64()
        .filter(|v| *v > 0)
        .unwrap_or(120);
    let (service_id, device_id) = parse_conn_url(&url);
    Ok(Endpoint {
        url,
        service_id,
        device_id,
        ping_interval_secs,
    })
}

/// Call `POST {base}/callback/ws/endpoint` to obtain the WebSocket URL.
/// `base` is the open-platform base (e.g. `https://open.feishu.cn`).
pub async fn get_endpoint(base: &str, app_id: &str, app_secret: &str) -> Result<Endpoint> {
    let url = format!("{}/callback/ws/endpoint", base.trim_end_matches('/'));
    let body = serde_json::json!({
        "AppID": app_id,
        "AppSecret": app_secret,
        "ClientAssertion": "",
    });
    let client = reqwest::Client::new();
    let resp = client
        .post(&url)
        .header("locale", "zh")
        .header("Content-Type", "application/json")
        .json(&body)
        .send()
        .await?;
    let status = resp.status();
    let data: serde_json::Value = resp.json().await?;
    if !status.is_success() {
        return Err(anyhow!("feishu ws endpoint HTTP {status}: {data}"));
    }
    parse_endpoint_response(&data)
}

// ── Split-packet reassembly ───────────────────────────────────────────────────

struct Pending {
    parts: Vec<Option<Vec<u8>>>,
    inserted: Instant,
}

/// Reassembles frames split across multiple packets (`sum > 1`), keyed by
/// `message_id`. Buffers expire after 5s (matching the Go SDK).
#[derive(Default)]
pub struct Reassembler {
    buf: HashMap<String, Pending>,
}

impl Reassembler {
    /// Create an empty reassembler.
    pub fn new() -> Self {
        Self::default()
    }

    /// Insert one packet. Returns `Some(full_payload)` once all `sum` packets
    /// of `message_id` have arrived, else `None`. Call only when `sum > 1`.
    pub fn push(
        &mut self,
        message_id: &str,
        sum: usize,
        seq: usize,
        payload: &[u8],
    ) -> Option<Vec<u8>> {
        self.evict_expired();
        if sum == 0 || seq >= sum {
            return None; // malformed
        }
        let entry = self
            .buf
            .entry(message_id.to_string())
            .or_insert_with(|| Pending {
                parts: vec![None; sum],
                inserted: Instant::now(),
            });
        // A late/duplicate packet with a mismatched sum: reset the buffer.
        if entry.parts.len() != sum {
            entry.parts = vec![None; sum];
            entry.inserted = Instant::now();
        }
        entry.parts[seq] = Some(payload.to_vec());
        if entry.parts.iter().all(|p| p.is_some()) {
            let entry = self.buf.remove(message_id)?;
            let mut full = Vec::new();
            for p in entry.parts.into_iter().flatten() {
                full.extend_from_slice(&p);
            }
            Some(full)
        } else {
            None
        }
    }

    fn evict_expired(&mut self) {
        let now = Instant::now();
        self.buf
            .retain(|_, p| now.duration_since(p.inserted).as_secs() < 5);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn varint_roundtrip() {
        for v in [0u64, 1, 127, 128, 300, 16384, u32::MAX as u64, u64::MAX] {
            let mut b = Vec::new();
            put_varint(&mut b, v);
            let mut pos = 0;
            assert_eq!(read_varint(&b, &mut pos), Some(v));
            assert_eq!(pos, b.len());
        }
    }

    #[test]
    fn read_varint_truncated_is_none() {
        // 0x80 means "more bytes follow" but there are none.
        let mut pos = 0;
        assert_eq!(read_varint(&[0x80], &mut pos), None);
    }

    #[test]
    fn header_roundtrip() {
        let enc = encode_header("type", "event");
        let (k, v) = decode_header(&enc).unwrap();
        assert_eq!(k, "type");
        assert_eq!(v, "event");
    }

    #[test]
    fn frame_roundtrip() {
        let f = Frame {
            seq_id: 7,
            log_id: 42,
            service: 100,
            method: METHOD_DATA,
            headers: vec![
                ("type".to_string(), "event".to_string()),
                ("message_id".to_string(), "m-1".to_string()),
                ("sum".to_string(), "1".to_string()),
            ],
            payload: br#"{"hello":"world"}"#.to_vec(),
        };
        let bytes = f.encode();
        let d = Frame::decode(&bytes).unwrap();
        assert_eq!(d.seq_id, 7);
        assert_eq!(d.log_id, 42);
        assert_eq!(d.service, 100);
        assert_eq!(d.method, METHOD_DATA);
        assert_eq!(d.header("type"), Some("event"));
        assert_eq!(d.header("message_id"), Some("m-1"));
        assert_eq!(d.header_int("sum"), 1);
        assert_eq!(d.payload, br#"{"hello":"world"}"#);
    }

    #[test]
    fn ping_frame_decodes_as_control_ping() {
        let bytes = build_ping_frame(555);
        let f = Frame::decode(&bytes).unwrap();
        assert_eq!(f.method, METHOD_CONTROL);
        assert_eq!(f.service, 555);
        assert_eq!(f.header("type"), Some("ping"));
        assert!(f.payload.is_empty());
    }

    #[test]
    fn ack_frame_echoes_ids_and_sets_response() {
        let recv = Frame {
            seq_id: 3,
            log_id: 9,
            service: 100,
            method: METHOD_DATA,
            headers: vec![("message_id".to_string(), "m-7".to_string())],
            payload: b"event-json".to_vec(),
        };
        let bytes = build_ack_frame(&recv, 12);
        let f = Frame::decode(&bytes).unwrap();
        assert_eq!(f.seq_id, 3);
        assert_eq!(f.log_id, 9);
        assert_eq!(f.service, 100);
        assert_eq!(f.method, METHOD_DATA);
        assert_eq!(f.header("message_id"), Some("m-7"));
        assert_eq!(f.header("biz_rt"), Some("12"));
        let body: serde_json::Value = serde_json::from_slice(&f.payload).unwrap();
        assert_eq!(body["code"], 200);
    }

    #[test]
    fn parse_conn_url_extracts_ids() {
        let (sid, did) = parse_conn_url("wss://host/path?device_id=dev-abc&service_id=100&foo=bar");
        assert_eq!(sid, 100);
        assert_eq!(did, "dev-abc");
    }

    #[test]
    fn parse_endpoint_response_ok() {
        let v = serde_json::json!({
            "code": 0,
            "msg": "",
            "data": {
                "URL": "wss://host/ws?device_id=d1&service_id=77",
                "ClientConfig": { "PingInterval": 90 }
            }
        });
        let ep = parse_endpoint_response(&v).unwrap();
        assert_eq!(ep.service_id, 77);
        assert_eq!(ep.device_id, "d1");
        assert_eq!(ep.ping_interval_secs, 90);
        assert!(ep.url.starts_with("wss://"));
    }

    #[test]
    fn parse_endpoint_response_default_ping() {
        let v = serde_json::json!({
            "code": 0,
            "data": { "URL": "wss://h/ws?service_id=1" }
        });
        let ep = parse_endpoint_response(&v).unwrap();
        assert_eq!(ep.ping_interval_secs, 120);
    }

    #[test]
    fn parse_endpoint_response_error_code() {
        let v = serde_json::json!({ "code": 1000040343, "msg": "internal error" });
        assert!(parse_endpoint_response(&v).is_err());
    }

    #[test]
    fn reassembler_combines_out_of_order() {
        let mut r = Reassembler::new();
        // seq 1 arrives first, then seq 0 → combined in order.
        assert_eq!(r.push("m1", 2, 1, b"world"), None);
        let full = r.push("m1", 2, 0, b"hello ").unwrap();
        assert_eq!(full, b"hello world");
    }

    #[test]
    fn reassembler_single_missing_returns_none() {
        let mut r = Reassembler::new();
        assert_eq!(r.push("m2", 3, 0, b"a"), None);
        assert_eq!(r.push("m2", 3, 2, b"c"), None);
        // seq 1 still missing
        assert!(r.buf.contains_key("m2"));
    }

    #[test]
    fn reassembler_rejects_malformed() {
        let mut r = Reassembler::new();
        assert_eq!(r.push("m3", 0, 0, b"x"), None);
        assert_eq!(r.push("m3", 2, 5, b"x"), None);
    }
}
