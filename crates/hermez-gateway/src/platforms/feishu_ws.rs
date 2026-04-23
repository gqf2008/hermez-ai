//! Feishu/Lark WebSocket client (pure protocol layer).
//!
//! Implements the Feishu pbbp2 binary frame protocol over WebSocket.
//! Event dispatch is delegated to the caller via a callback so that
//! `FeishuAdapter` can apply dedup, policy, and normalization uniformly.
//!
//! Mirrors the Python `lark_oapi` WS client.
//!
//! Connection flow:
//! 1. POST {domain}/callback/ws/endpoint to get a WebSocket URL
//! 2. Connect via WebSocket upgrade
//! 3. Receive/send protobuf-encoded Frame messages
//! 4. Heartbeat: CONTROL ping every 120s
//! 5. Auto-reconnect on disconnect

use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use futures::{SinkExt, StreamExt};
use tokio::time::{interval, Duration};
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::{MaybeTlsStream, connect_async};
use tracing::{debug, error, info, warn};

use crate::platforms::feishu::FeishuConfig;

type WsWriteHalf = futures::stream::SplitSink<
    tokio_tungstenite::WebSocketStream<MaybeTlsStream<tokio::net::TcpStream>>,
    Message,
>;
#[allow(dead_code)]
type WsStream = tokio_tungstenite::WebSocketStream<MaybeTlsStream<tokio::net::TcpStream>>;

/// Feishu domain URLs.
const FEISHU_DOMAIN: &str = "https://open.feishu.cn";
const LARK_DOMAIN: &str = "https://open.larksuite.com";

/// Default ping interval in seconds.
const DEFAULT_PING_INTERVAL: u64 = 120;
/// Default reconnect interval in seconds.
const DEFAULT_RECONNECT_INTERVAL: u64 = 120;
/// Default reconnect jitter in seconds.
const DEFAULT_RECONNECT_NONCE: u64 = 30;

/// Frame method types.
const METHOD_CONTROL: i32 = 0;
const METHOD_DATA: i32 = 1;

/// Protobuf wire format encoder/decoder for pbbp2 Frame messages.
mod proto {
    /// Encode a varint (unsigned LEB128).
    pub fn encode_varint(mut value: u64, buf: &mut Vec<u8>) {
        loop {
            let byte = (value & 0x7F) as u8;
            value >>= 7;
            if value != 0 {
                buf.push(byte | 0x80);
            } else {
                buf.push(byte);
                break;
            }
        }
    }

    /// Decode a varint from a byte slice, returning (value, bytes_consumed).
    pub fn decode_varint(data: &[u8]) -> Option<(u64, usize)> {
        let mut value: u64 = 0;
        let mut shift: u32 = 0;
        for (i, &byte) in data.iter().enumerate() {
            value |= ((byte & 0x7F) as u64) << shift;
            shift += 7;
            if byte & 0x80 == 0 {
                return Some((value, i + 1));
            }
            if shift > 63 {
                return None;
            }
        }
        None
    }

    /// Encode a protobuf string field: tag (wire type 2) + length-prefixed bytes.
    pub fn encode_string_field(tag: u32, value: &str, buf: &mut Vec<u8>) {
        encode_tag(tag, 2, buf);
        let bytes = value.as_bytes();
        encode_varint(bytes.len() as u64, buf);
        buf.extend_from_slice(bytes);
    }

    /// Encode a protobuf bytes field: tag (wire type 2) + length-prefixed bytes.
    pub fn encode_bytes_field(tag: u32, value: &[u8], buf: &mut Vec<u8>) {
        encode_tag(tag, 2, buf);
        encode_varint(value.len() as u64, buf);
        buf.extend_from_slice(value);
    }

    /// Encode a protobuf uint64 field: tag (wire type 0) + varint.
    pub fn encode_uint64(tag: u32, value: u64, buf: &mut Vec<u8>) {
        encode_tag(tag, 0, buf);
        encode_varint(value, buf);
    }

    /// Encode a protobuf int32 field: tag (wire type 0) + varint (zigzag for negative).
    pub fn encode_int32(tag: u32, value: i32, buf: &mut Vec<u8>) {
        encode_tag(tag, 0, buf);
        // Zigzag encode for negative values
        let unsigned = value as u64; // Two's complement for negative, identity for positive
        encode_varint(unsigned, buf);
    }

    /// Encode a tag: (field_number << 3) | wire_type.
    fn encode_tag(field_number: u32, wire_type: u8, buf: &mut Vec<u8>) {
        buf.push(((field_number << 3) | wire_type as u32) as u8);
    }

    /// Decode a tag, returning (field_number, wire_type, bytes_consumed).
    pub fn decode_tag(data: &[u8]) -> Option<(u32, u8, usize)> {
        let (value, consumed) = decode_varint(data)?;
        let field_number = (value >> 3) as u32;
        let wire_type = (value & 0x07) as u8;
        Some((field_number, wire_type, consumed))
    }
}

/// A pbbp2 Frame header entry (key-value pair).
#[derive(Debug, Clone, Default)]
struct FrameHeader {
    key: String,
    value: String,
}

/// A pbbp2 Frame.
#[derive(Debug, Clone, Default)]
struct Frame {
    seq_id: u64,
    log_id: u64,
    service: i32,
    method: i32, // 0=CONTROL, 1=DATA
    headers: Vec<FrameHeader>,
    payload_encoding: Option<String>,
    payload_type: Option<String>,
    payload: Option<Vec<u8>>,
    log_id_new: Option<String>,
}

impl Frame {
    /// Encode the frame to protobuf bytes.
    fn encode(&self) -> Vec<u8> {
        let mut buf = Vec::new();
        proto::encode_uint64(1, self.seq_id, &mut buf);
        proto::encode_uint64(2, self.log_id, &mut buf);
        proto::encode_int32(3, self.service, &mut buf);
        proto::encode_int32(4, self.method, &mut buf);

        for header in &self.headers {
            // Encode Header message (fields 1=key, 2=value)
            let mut header_buf = Vec::new();
            proto::encode_string_field(1, &header.key, &mut header_buf);
            proto::encode_string_field(2, &header.value, &mut header_buf);
            // Wrap as field 5 (Header repeated)
            proto::encode_bytes_field(5, &header_buf, &mut buf);
        }

        if let Some(ref enc) = self.payload_encoding {
            proto::encode_string_field(6, enc, &mut buf);
        }
        if let Some(ref ptype) = self.payload_type {
            proto::encode_string_field(7, ptype, &mut buf);
        }
        if let Some(ref payload) = self.payload {
            proto::encode_bytes_field(8, payload, &mut buf);
        }
        if let Some(ref log_id_new) = self.log_id_new {
            proto::encode_string_field(9, log_id_new, &mut buf);
        }

        buf
    }

    /// Decode a frame from protobuf bytes.
    fn decode(data: &[u8]) -> Option<Self> {
        let mut frame = Frame::default();
        let mut pos = 0;

        while pos < data.len() {
            let (field_number, wire_type, consumed) = proto::decode_tag(&data[pos..])?;
            pos += consumed;

            match (field_number, wire_type) {
                (1, 0) => {
                    let (val, c) = proto::decode_varint(&data[pos..])?;
                    frame.seq_id = val;
                    pos += c;
                }
                (2, 0) => {
                    let (val, c) = proto::decode_varint(&data[pos..])?;
                    frame.log_id = val;
                    pos += c;
                }
                (3, 0) => {
                    let (val, c) = proto::decode_varint(&data[pos..])?;
                    frame.service = val as i32;
                    pos += c;
                }
                (4, 0) => {
                    let (val, c) = proto::decode_varint(&data[pos..])?;
                    frame.method = val as i32;
                    pos += c;
                }
                (5, 2) => {
                    // Header message (length-delimited)
                    let (len, c) = proto::decode_varint(&data[pos..])?;
                    pos += c;
                    let header_data = &data[pos..pos + len as usize];
                    if let Some(header) = Self::decode_header(header_data) {
                        frame.headers.push(header);
                    }
                    pos += len as usize;
                }
                (6, 2) => {
                    let (len, c) = proto::decode_varint(&data[pos..])?;
                    pos += c;
                    frame.payload_encoding = Some(
                        String::from_utf8_lossy(&data[pos..pos + len as usize]).to_string(),
                    );
                    pos += len as usize;
                }
                (7, 2) => {
                    let (len, c) = proto::decode_varint(&data[pos..])?;
                    pos += c;
                    frame.payload_type = Some(
                        String::from_utf8_lossy(&data[pos..pos + len as usize]).to_string(),
                    );
                    pos += len as usize;
                }
                (8, 2) => {
                    let (len, c) = proto::decode_varint(&data[pos..])?;
                    pos += c;
                    frame.payload = Some(data[pos..pos + len as usize].to_vec());
                    pos += len as usize;
                }
                (9, 2) => {
                    let (len, c) = proto::decode_varint(&data[pos..])?;
                    pos += c;
                    frame.log_id_new = Some(
                        String::from_utf8_lossy(&data[pos..pos + len as usize]).to_string(),
                    );
                    pos += len as usize;
                }
                _ => {
                    // Skip unknown field
                    let (len, c) = proto::decode_varint(&data[pos..])?;
                    pos += c;
                    pos += len as usize;
                }
            }
        }

        Some(frame)
    }

    /// Decode a Header sub-message.
    fn decode_header(data: &[u8]) -> Option<FrameHeader> {
        let mut header = FrameHeader::default();
        let mut pos = 0;

        while pos < data.len() {
            let (field_number, wire_type, consumed) = proto::decode_tag(&data[pos..])?;
            pos += consumed;

            match (field_number, wire_type) {
                (1, 2) => {
                    let (len, c) = proto::decode_varint(&data[pos..])?;
                    pos += c;
                    header.key = String::from_utf8_lossy(&data[pos..pos + len as usize]).to_string();
                    pos += len as usize;
                }
                (2, 2) => {
                    let (len, c) = proto::decode_varint(&data[pos..])?;
                    pos += c;
                    header.value =
                        String::from_utf8_lossy(&data[pos..pos + len as usize]).to_string();
                    pos += len as usize;
                }
                _ => {
                    let (len, c) = proto::decode_varint(&data[pos..])?;
                    pos += c;
                    pos += len as usize;
                }
            }
        }

        Some(header)
    }

    /// Get a header value by key.
    fn get_header(&self, key: &str) -> Option<&str> {
        self.headers
            .iter()
            .find(|h| h.key == key)
            .map(|h| h.value.as_str())
    }

    /// Add a header key-value pair.
    fn add_header(&mut self, key: &str, value: &str) {
        self.headers.push(FrameHeader {
            key: key.to_string(),
            value: value.to_string(),
        });
    }
}

/// Client configuration from the endpoint response.
#[derive(Debug, Clone)]
struct WsClientConfig {
    reconnect_count: i32,
    reconnect_interval: u64,
    reconnect_nonce: u64,
    ping_interval: u64,
}

impl Default for WsClientConfig {
    fn default() -> Self {
        Self {
            reconnect_count: -1,
            reconnect_interval: DEFAULT_RECONNECT_INTERVAL,
            reconnect_nonce: DEFAULT_RECONNECT_NONCE,
            ping_interval: DEFAULT_PING_INTERVAL,
        }
    }
}

/// Fragment cache entry for reassembling fragmented messages.
struct FragmentEntry {
    fragments: HashMap<u32, String>,
    total: u32,
}

/// Feishu WebSocket client.
pub struct FeishuWsClient {
    config: FeishuConfig,
    client: reqwest::Client,
    /// Running flag.
    running: Arc<std::sync::atomic::AtomicBool>,
    /// Current service_id (from WebSocket URL query param).
    service_id: Arc<std::sync::atomic::AtomicI32>,
    /// Fragment cache: message_id -> FragmentEntry.
    fragment_cache: Arc<parking_lot::Mutex<HashMap<String, FragmentEntry>>>,
    /// Monotonically increasing seq counter.
    #[allow(dead_code)]
    seq_counter: AtomicU64,
}

/// Type alias for the raw JSON event callback.
pub type WsEventCallback = Arc<dyn Fn(serde_json::Value) + Send + Sync>;

impl FeishuWsClient {
    pub fn new(config: FeishuConfig) -> Self {
        Self {
            client: reqwest::Client::builder()
                .timeout(std::time::Duration::from_secs(30))
                .build()
                .unwrap_or_else(|e| {
                    tracing::warn!("Failed to build HTTP client: {e}");
                    reqwest::Client::new()
                }),
            running: Arc::new(std::sync::atomic::AtomicBool::new(true)),
            service_id: Arc::new(std::sync::atomic::AtomicI32::new(0)),
            fragment_cache: Arc::new(parking_lot::Mutex::new(HashMap::new())),
            seq_counter: AtomicU64::new(0),
            config,
        }
    }

    /// Get the domain base URL.
    fn domain_url(&self) -> String {
        let domain = std::env::var("FEISHU_DOMAIN").ok();
        match domain.as_deref() {
            Some("lark") | Some("larksuite") => LARK_DOMAIN.to_string(),
            Some(d) => d.to_string(),
            None => FEISHU_DOMAIN.to_string(),
        }
    }

    /// Fetch WebSocket endpoint URL via HTTP POST.
    async fn fetch_ws_endpoint(&self) -> Result<(String, WsClientConfig), String> {
        let resp = self
            .client
            .post(format!("{}/callback/ws/endpoint", self.domain_url()))
            .header("locale", "zh")
            .json(&serde_json::json!({
                "AppID": &self.config.app_id,
                "AppSecret": &self.config.app_secret,
            }))
            .send()
            .await
            .map_err(|e| format!("Failed to fetch WS endpoint: {e}"))?;

        let body: serde_json::Value = resp
            .json()
            .await
            .map_err(|e| format!("Failed to parse endpoint response: {e}"))?;

        let code = body.get("code").and_then(|v| v.as_i64()).unwrap_or(-1);
        if code != 0 {
            return Err(format!(
                "Endpoint error: code={code}, msg={}",
                body.get("msg").and_then(|v| v.as_str()).unwrap_or("unknown")
            ));
        }

        let ws_url = body
            .get("data")
            .and_then(|d| d.get("URL"))
            .and_then(|v| v.as_str())
            .ok_or("Missing URL in endpoint response")?
            .to_string();

        // Parse client config
        let mut client_config = WsClientConfig::default();
        if let Some(cfg) = body.get("data").and_then(|d| d.get("ClientConfig")) {
            if let Some(v) = cfg.get("ReconnectCount").and_then(|v| v.as_i64()) {
                client_config.reconnect_count = v as i32;
            }
            if let Some(v) = cfg.get("ReconnectInterval").and_then(|v| v.as_i64()) {
                client_config.reconnect_interval = v as u64;
            }
            if let Some(v) = cfg.get("ReconnectNonce").and_then(|v| v.as_i64()) {
                client_config.reconnect_nonce = v as u64;
            }
            if let Some(v) = cfg.get("PingInterval").and_then(|v| v.as_i64()) {
                client_config.ping_interval = v as u64;
            }
        }

        // Extract service_id from URL query string
        if let Ok(parsed) = reqwest::Url::parse(&ws_url) {
            for (key, value) in parsed.query_pairs() {
                if key == "service_id" {
                    if let Ok(sid) = value.parse::<i32>() {
                        self.service_id.store(sid, Ordering::SeqCst);
                    }
                    break;
                }
            }
        }

        Ok((ws_url, client_config))
    }

    /// Send a Frame as a binary WebSocket message.
    async fn send_frame(ws: &mut WsWriteHalf, frame: &Frame) -> Result<(), String> {
        let bytes = frame.encode();
        ws.send(Message::Binary(bytes.into()))
            .await
            .map_err(|e| format!("Failed to send frame: {e}"))
    }

    /// Send a CONTROL ping frame.
    async fn send_ping(ws: &mut WsWriteHalf, service_id: i32) -> Result<(), String> {
        let mut frame = Frame {
            method: METHOD_CONTROL,
            service: service_id,
            seq_id: 0,
            log_id: 0,
            ..Default::default()
        };
        frame.add_header("type", "ping");
        Self::send_frame(ws, &frame).await
    }

    /// Send a response Frame for a DATA event.
    async fn send_event_response(
        ws: &mut WsWriteHalf,
        original: &Frame,
        biz_rt_ms: u64,
    ) -> Result<(), String> {
        let mut frame = Frame {
            seq_id: original.seq_id,
            log_id: original.log_id,
            service: original.service,
            method: original.method,
            headers: original.headers.clone(),
            ..Default::default()
        };
        frame.add_header("biz_rt", &biz_rt_ms.to_string());

        // Response payload
        let response = serde_json::json!({
            "code": 200,
            "headers": {},
            "data": "",
        });
        frame.payload = Some(response.to_string().into_bytes());

        Self::send_frame(ws, &frame).await
    }

    /// Reassemble fragmented messages. Returns None if not all fragments received.
    fn try_reassemble(&self, message_id: &str, seq: u32, data: String, total: u32) -> Option<String> {
        if total <= 1 {
            return Some(data);
        }

        let mut cache = self.fragment_cache.lock();
        let entry = cache
            .entry(message_id.to_string())
            .or_insert_with(|| FragmentEntry {
                fragments: HashMap::new(),
                total,
            });

        entry.fragments.insert(seq, data);

        if entry.fragments.len() == entry.total as usize {
            let parts: Vec<String> = (0..entry.total)
                .filter_map(|i| entry.fragments.remove(&i))
                .collect();
            cache.remove(message_id);
            Some(parts.join(""))
        } else {
            // Clean up old entries (5 second TTL)
            None
        }
    }

    /// Process a DATA frame payload (JSON event).
    async fn process_data_frame(
        &self,
        frame: &Frame,
    ) -> Option<serde_json::Value> {
        let payload_bytes = frame.payload.as_ref()?;
        let payload_str = String::from_utf8_lossy(payload_bytes);

        // Check for fragmentation
        let total = frame
            .get_header("sum")
            .and_then(|v| v.parse::<u32>().ok())
            .unwrap_or(1);
        let seq = frame
            .get_header("seq")
            .and_then(|v| v.parse::<u32>().ok())
            .unwrap_or(0);
        let message_id = frame.get_header("message_id").unwrap_or("");

        let full_payload = self.try_reassemble(message_id, seq, payload_str.to_string(), total)?;

        // Parse JSON event
        match serde_json::from_str::<serde_json::Value>(&full_payload) {
            Ok(event) => Some(event),
            Err(e) => {
                warn!("[Feishu WS] Failed to parse event JSON: {e}");
                None
            }
        }
    }

    /// Run the WebSocket connection loop with auto-reconnect.
    ///
    /// Connects to Feishu via WebSocket, processes inbound events,
    /// and dispatches them to the callback.
    pub async fn run(
        &self,
        callback: WsEventCallback,
    ) {
        const BACKOFF: &[u64] = &[2, 5, 10, 30, 60];
        let mut backoff_idx = 0;

        while self.running.load(Ordering::SeqCst) {
            match self.connect_and_run(&callback).await {
                Ok(()) => {
                    backoff_idx = 0; // Clean disconnect
                }
                Err(e) => {
                    if !self.running.load(Ordering::SeqCst) {
                        break;
                    }
                    error!("[Feishu WS] Connection error: {e}");

                    let delay = BACKOFF[backoff_idx.min(BACKOFF.len() - 1)];
                    info!("[Feishu WS] Reconnecting in {delay}s...");
                    tokio::time::sleep(Duration::from_secs(delay)).await;
                    backoff_idx = (backoff_idx + 1).min(BACKOFF.len() - 1);
                }
            }
        }

        info!("[Feishu WS] WebSocket loop stopped");
    }

    /// Connect and run the message loop. Returns Ok(()) on clean disconnect.
    async fn connect_and_run(
        &self,
        callback: &WsEventCallback,
    ) -> Result<(), String> {
        let (ws_url, client_config) = self.fetch_ws_endpoint().await?;
        info!("[Feishu WS] Connecting to {ws_url}...");

        let uri: tokio_tungstenite::tungstenite::http::Uri = ws_url
            .parse()
            .map_err(|e| format!("Invalid WebSocket URL: {e}"))?;

        let (ws_stream, _response) = connect_async(uri)
            .await
            .map_err(|e| format!("WebSocket connect failed: {e}"))?;

        info!("[Feishu WS] Connected");

        let (mut write_half, mut read_half) = ws_stream.split();
        let mut heartbeat = interval(Duration::from_secs(client_config.ping_interval));
        heartbeat.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);

        loop {
            tokio::select! {
                // Read inbound frames
                result = read_half.next() => {
                    match result {
                        Some(Ok(Message::Binary(data))) => {
                            if let Some(frame) = Frame::decode(&data) {
                                self.handle_frame(
                                    &frame,
                                    &mut write_half,
                                    callback,
                                ).await;
                            }
                        }
                        Some(Ok(Message::Close(_))) => {
                            info!("[Feishu WS] Closed by server");
                            return Err("WebSocket closed by server".to_string());
                        }
                        Some(Ok(Message::Ping(ping))) => {
                            debug!("[Feishu WS] Ping: {ping:?}");
                        }
                        Some(Ok(_)) => {
                            // Text, Pong: ignore
                        }
                        Some(Err(e)) => {
                            return Err(format!("WebSocket read error: {e}"));
                        }
                        None => {
                            return Err("WebSocket stream ended".to_string());
                        }
                    }
                }
                // Heartbeat
                _ = heartbeat.tick() => {
                    let sid = self.service_id.load(Ordering::SeqCst);
                    if let Err(e) = Self::send_ping(&mut write_half, sid).await {
                        warn!("[Feishu WS] Heartbeat failed: {e}");
                    }
                }
                // Check running flag
                _ = tokio::time::sleep(Duration::from_millis(200)) => {
                    if !self.running.load(Ordering::SeqCst) {
                        return Ok(());
                    }
                }
            }
        }
    }

    /// Handle a decoded Frame.
    async fn handle_frame(
        &self,
        frame: &Frame,
        write_half: &mut WsWriteHalf,
        callback: &WsEventCallback,
    ) {
        match frame.method {
            METHOD_CONTROL => {
                let msg_type = frame.get_header("type").unwrap_or("");
                match msg_type {
                    "pong" => {
                        debug!("[Feishu WS] Pong received");
                    }
                    "ping" => {
                        // Server-initiated ping — no response needed
                        debug!("[Feishu WS] Server ping received (no response)");
                    }
                    _ => {
                        debug!("[Feishu WS] Unknown control: {msg_type}");
                    }
                }
            }
            METHOD_DATA => {
                let msg_type = frame.get_header("type").unwrap_or("");
                let start = std::time::Instant::now();

                match msg_type {
                    "event" => {
                        if let Some(event) = self.process_data_frame(frame).await {
                            self.dispatch_event(event, callback).await;
                        }
                    }
                    "card" => {
                        debug!("[Feishu WS] Card action ignored");
                    }
                    _ => {
                        debug!("[Feishu WS] Unknown data type: {msg_type}");
                    }
                }

                // Send response with biz_rt
                let biz_rt = start.elapsed().as_millis() as u64;
                let _ = Self::send_event_response(write_half, frame, biz_rt).await;
            }
            _ => {
                warn!("[Feishu WS] Unknown method: {}", frame.method);
            }
        }
    }

    /// Dispatch a Feishu event to the callback.
    async fn dispatch_event(
        &self,
        event: serde_json::Value,
        callback: &WsEventCallback,
    ) {
        let event_type = event
            .get("header")
            .and_then(|h| h.get("event_type"))
            .and_then(|v| v.as_str())
            .unwrap_or("");

        match event_type {
            "im.message.receive_v1" => {
                callback(event);
            }
            "card.action.trigger" => {
                callback(event);
            }
            "im.chat.member.bot.added_v1" => {
                let chat_id = event
                    .get("event")
                    .and_then(|e| e.get("chat_id"))
                    .and_then(|v| v.as_str())
                    .unwrap_or("unknown");
                info!("[Feishu WS] Bot added to chat: {chat_id}");
            }
            "im.chat.member.bot.deleted_v1" => {
                let chat_id = event
                    .get("event")
                    .and_then(|e| e.get("chat_id"))
                    .and_then(|v| v.as_str())
                    .unwrap_or("unknown");
                info!("[Feishu WS] Bot removed from chat: {chat_id}");
            }
            "im.message.reaction.created_v1" | "im.message.reaction.deleted_v1" => {
                callback(event);
            }
            "im.message.message_read_v1" => {
                debug!("[Feishu WS] Message read event");
            }
            _ => {
                debug!("[Feishu WS] Unknown event: {event_type}");
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_proto_varint_encode_decode() {
        let mut buf = Vec::new();
        proto::encode_varint(42, &mut buf);
        assert_eq!(buf, vec![42]);

        let (val, consumed) = proto::decode_varint(&buf).unwrap();
        assert_eq!(val, 42);
        assert_eq!(consumed, 1);

        let mut buf2 = Vec::new();
        proto::encode_varint(300, &mut buf2);
        let (val, consumed) = proto::decode_varint(&buf2).unwrap();
        assert_eq!(val, 300);
        assert_eq!(consumed, 2);
    }

    #[test]
    fn test_frame_encode_decode() {
        let mut frame = Frame::default();
        frame.seq_id = 1;
        frame.log_id = 12345;
        frame.service = 10;
        frame.method = METHOD_DATA;
        frame.add_header("type", "event");
        frame.add_header("message_id", "msg_001");
        frame.payload = Some(b"hello".to_vec());

        let encoded = frame.encode();
        let decoded = Frame::decode(&encoded).unwrap();

        assert_eq!(decoded.seq_id, 1);
        assert_eq!(decoded.log_id, 12345);
        assert_eq!(decoded.service, 10);
        assert_eq!(decoded.method, METHOD_DATA);
        assert_eq!(decoded.get_header("type"), Some("event"));
        assert_eq!(decoded.get_header("message_id"), Some("msg_001"));
        assert_eq!(decoded.payload, Some(b"hello".to_vec()));
    }

    #[test]
    fn test_frame_empty() {
        let frame = Frame::default();
        let encoded = frame.encode();
        let decoded = Frame::decode(&encoded).unwrap();
        assert_eq!(decoded.seq_id, 0);
        assert_eq!(decoded.method, 0);
    }

    #[test]
    fn test_fragment_single() {
        let config = FeishuConfig::from_env();
        let client = FeishuWsClient::new(config);
        let result = client.try_reassemble("msg_1", 0, "hello".to_string(), 1);
        assert_eq!(result, Some("hello".to_string()));
    }
}
