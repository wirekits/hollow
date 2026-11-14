//! MCS (Mobile Connection Server) клиент: mtalk.google.com:5228 по TLS, protobuf (mcs.proto из push-receiver).
//! Логин, разбор потока, DataMessageStanza. Константы/поля взяты из `@liamcottle/push-receiver`
//! (src/client.js, src/parser.js, src/constants.js, src/mcs.proto).
use prost::Message;
use std::sync::Arc;
use tokio::io::{AsyncReadExt, AsyncWriteExt};

pub const HOST: &str = "mtalk.google.com";
pub const PORT: u16 = 5228;
pub const MCS_VERSION: u8 = 41;

pub const TAG_HEARTBEAT_PING: u8 = 0;
pub const TAG_HEARTBEAT_ACK: u8 = 1;
pub const TAG_LOGIN_REQUEST: u8 = 2;
pub const TAG_LOGIN_RESPONSE: u8 = 3;
pub const TAG_CLOSE: u8 = 4;
pub const TAG_DATA_MESSAGE: u8 = 8;
pub const TAG_STREAM_ERROR: u8 = 10;

#[derive(Clone, PartialEq, Message)]
pub struct Setting {
    #[prost(string, tag = "1")]
    pub name: String,
    #[prost(string, tag = "2")]
    pub value: String,
}

#[derive(Clone, PartialEq, Message)]
pub struct ErrorInfo {
    #[prost(int32, tag = "1")]
    pub code: i32,
    #[prost(string, optional, tag = "2")]
    pub message: Option<String>,
    #[prost(string, optional, tag = "3")]
    pub r#type: Option<String>,
}

#[derive(Clone, PartialEq, Message)]
pub struct HeartbeatPing {
    #[prost(int32, optional, tag = "1")]
    pub stream_id: Option<i32>,
    #[prost(int32, optional, tag = "2")]
    pub last_stream_id_received: Option<i32>,
    #[prost(int64, optional, tag = "3")]
    pub status: Option<i64>,
}

#[derive(Clone, PartialEq, Message)]
pub struct HeartbeatAck {
    #[prost(int32, optional, tag = "1")]
    pub stream_id: Option<i32>,
    #[prost(int32, optional, tag = "2")]
    pub last_stream_id_received: Option<i32>,
    #[prost(int64, optional, tag = "3")]
    pub status: Option<i64>,
}

#[derive(Clone, PartialEq, Message)]
pub struct LoginRequest {
    #[prost(string, tag = "1")]
    pub id: String,
    #[prost(string, tag = "2")]
    pub domain: String,
    #[prost(string, tag = "3")]
    pub user: String,
    #[prost(string, tag = "4")]
    pub resource: String,
    #[prost(string, tag = "5")]
    pub auth_token: String,
    #[prost(string, optional, tag = "6")]
    pub device_id: Option<String>,
    #[prost(message, repeated, tag = "8")]
    pub setting: Vec<Setting>,
    #[prost(string, repeated, tag = "10")]
    pub received_persistent_id: Vec<String>,
    #[prost(bool, optional, tag = "12")]
    pub adaptive_heartbeat: Option<bool>,
    #[prost(bool, optional, tag = "14")]
    pub use_rmq2: Option<bool>,
    #[prost(int32, optional, tag = "16")]
    pub auth_service: Option<i32>,
    #[prost(int32, optional, tag = "17")]
    pub network_type: Option<i32>,
}

#[derive(Clone, PartialEq, Message)]
pub struct LoginResponse {
    #[prost(string, tag = "1")]
    pub id: String,
    #[prost(string, optional, tag = "2")]
    pub jid: Option<String>,
    #[prost(message, optional, tag = "3")]
    pub error: Option<ErrorInfo>,
}

#[derive(Clone, PartialEq, Message)]
pub struct AppData {
    #[prost(string, tag = "1")]
    pub key: String,
    #[prost(string, tag = "2")]
    pub value: String,
}

#[derive(Clone, PartialEq, Message)]
pub struct DataMessageStanza {
    #[prost(string, optional, tag = "2")]
    pub id: Option<String>,
    #[prost(string, tag = "3")]
    pub from: String,
    #[prost(string, optional, tag = "4")]
    pub to: Option<String>,
    #[prost(string, tag = "5")]
    pub category: String,
    #[prost(string, optional, tag = "6")]
    pub token: Option<String>,
    #[prost(message, repeated, tag = "7")]
    pub app_data: Vec<AppData>,
    #[prost(string, optional, tag = "9")]
    pub persistent_id: Option<String>,
    #[prost(int32, optional, tag = "10")]
    pub stream_id: Option<i32>,
    #[prost(bytes = "vec", optional, tag = "21")]
    pub raw_data: Option<Vec<u8>>,
}

/// Буфер логина: [version, LoginRequest tag] + length-delimited LoginRequest (как `_loginBuffer`).
pub fn login_buffer(android_id: &str, security_token: &str, persistent_ids: &[String]) -> Result<Vec<u8>, String> {
    let id: i64 = android_id.parse().map_err(|_| "некорректный androidId".to_string())?;
    let hex = format!("{:x}", id);
    let req = LoginRequest {
        id: "chrome-63.0.3234.0".into(),
        domain: "mcs.android.com".into(),
        user: android_id.into(),
        resource: android_id.into(),
        auth_token: security_token.into(),
        device_id: Some(format!("android-{hex}")),
        setting: vec![Setting { name: "new_vc".into(), value: "1".into() }],
        received_persistent_id: persistent_ids.to_vec(),
        adaptive_heartbeat: Some(false),
        use_rmq2: Some(true),
        auth_service: Some(2),
        network_type: Some(1),
    };
    let mut out = vec![MCS_VERSION, TAG_LOGIN_REQUEST];
    out.extend_from_slice(&req.encode_length_delimited_to_vec());
    Ok(out)
}

/// Пустой HeartbeatAck (тег 1, размер 0). В референсе ответа на ping нет; формат берётся из mcs.proto
/// (все поля опциональны), поведение как у Chromium: отвечать на ping ack'ом.
pub fn heartbeat_ack_frame() -> Vec<u8> {
    vec![TAG_HEARTBEAT_ACK, 0]
}

/// Потоковый разбор MCS: версия (один раз), затем [tag][varint size][body]...
#[derive(Default)]
pub struct McsParser {
    buf: Vec<u8>,
    got_version: bool,
}

impl McsParser {
    pub fn new() -> Self {
        Self::default()
    }

    /// Добавляет байты, возвращает все полные сообщения (tag, body).
    pub fn push(&mut self, data: &[u8]) -> Result<Vec<(u8, Vec<u8>)>, String> {
        self.buf.extend_from_slice(data);
        let mut out = Vec::new();
        loop {
            let mut pos = 0usize;
            if !self.got_version {
                if self.buf.is_empty() {
                    break;
                }
                let v = self.buf[0] as i8 as i32;
                if v < MCS_VERSION as i32 && v != 38 {
                    return Err(format!("неподдерживаемая версия MCS: {v}"));
                }
                pos = 1;
            }
            if self.buf.len() < pos + 1 {
                break;
            }
            let tag = self.buf[pos];
            pos += 1;
            // varint размера (до 5 байт)
            let mut size: u64 = 0;
            let mut shift = 0u32;
            let mut complete = false;
            let mut n = 0;
            while pos + n < self.buf.len() {
                let b = self.buf[pos + n];
                size |= ((b & 0x7F) as u64) << shift;
                n += 1;
                if b & 0x80 == 0 {
                    complete = true;
                    break;
                }
                shift += 7;
                if n >= 5 {
                    return Err("некорректный размер сообщения MCS".into());
                }
            }
            if !complete {
                break;
            }
            let size = size as usize;
            if size > 8 * 1024 * 1024 {
                return Err("слишком большое сообщение MCS".into());
            }
            let body_start = pos + n;
            if self.buf.len() < body_start + size {
                break;
            }
            let body = self.buf[body_start..body_start + size].to_vec();
            self.buf.drain(..body_start + size);
            self.got_version = true;
            out.push((tag, body));
        }
        Ok(out)
    }
}

/// Событие сессии MCS наружу.
#[derive(Debug)]
pub enum McsEvent {
    LoggedIn,
    Data { persistent_id: Option<String>, app_data: Vec<(String, String)>, encrypted: bool },
}

/// Разбор DataMessageStanza в событие (чистая функция).
pub fn data_event(body: &[u8]) -> Result<McsEvent, String> {
    let d = DataMessageStanza::decode(body).map_err(|e| format!("DataMessageStanza: {e}"))?;
    let encrypted = d.app_data.iter().any(|a| a.key == "crypto-key");
    Ok(McsEvent::Data {
        persistent_id: d.persistent_id,
        app_data: d.app_data.into_iter().map(|a| (a.key, a.value)).collect(),
        encrypted,
    })
}

fn tls_config() -> Result<Arc<rustls::ClientConfig>, String> {
    let mut roots = rustls::RootCertStore::empty();
    roots.extend(webpki_roots::TLS_SERVER_ROOTS.iter().cloned());
    let cfg = rustls::ClientConfig::builder_with_provider(Arc::new(rustls::crypto::ring::default_provider()))
        .with_safe_default_protocol_versions()
        .map_err(|e| format!("TLS: {e}"))?
        .with_root_certificates(roots)
        .with_no_client_auth();
    Ok(Arc::new(cfg))
}

/// Одна MCS-сессия: подключение, логин, чтение до обрыва. `on_event` вызывается синхронно.
/// Возвращает Err при обрыве/ошибке (вызывающий переподключается).
pub async fn session<F>(
    android_id: &str,
    security_token: &str,
    persistent_ids: &[String],
    mut on_event: F,
) -> Result<(), String>
where
    F: FnMut(McsEvent),
{
    let login = login_buffer(android_id, security_token, persistent_ids)?;
    let tcp = tokio::time::timeout(std::time::Duration::from_secs(20), tokio::net::TcpStream::connect((HOST, PORT)))
        .await
        .map_err(|_| "MCS: таймаут подключения".to_string())?
        .map_err(|e| format!("MCS: подключение не удалось: {e}"))?;
    let _ = tcp.set_nodelay(true);
    let connector = tokio_rustls::TlsConnector::from(tls_config()?);
    let name = rustls::pki_types::ServerName::try_from(HOST).map_err(|e| format!("TLS: {e}"))?;
    let mut tls = connector.connect(name, tcp).await.map_err(|e| format!("MCS: TLS не удался: {e}"))?;
    tls.write_all(&login).await.map_err(|e| format!("MCS: {e}"))?;

    let mut parser = McsParser::new();
    let mut buf = vec![0u8; 16 * 1024];
    loop {
        let n = tls.read(&mut buf).await.map_err(|e| format!("MCS: чтение: {e}"))?;
        if n == 0 {
            return Err("MCS: соединение закрыто сервером".into());
        }
        for (tag, body) in parser.push(&buf[..n])? {
            match tag {
                TAG_LOGIN_RESPONSE => {
                    let r = LoginResponse::decode(body.as_slice()).map_err(|e| format!("LoginResponse: {e}"))?;
                    if let Some(e) = r.error {
                        return Err(format!("MCS: логин отклонён (код {})", e.code));
                    }
                    on_event(McsEvent::LoggedIn);
                }
                TAG_HEARTBEAT_PING => {
                    tls.write_all(&heartbeat_ack_frame()).await.map_err(|e| format!("MCS: {e}"))?;
                }
                TAG_DATA_MESSAGE => on_event(data_event(&body)?),
                TAG_CLOSE => return Err("MCS: сервер прислал Close".into()),
                TAG_STREAM_ERROR => return Err("MCS: StreamError от сервера".into()),
                _ => {} // IqStanza и прочее, как в референсе, игнорируется
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn frame(tag: u8, body: &[u8], with_version: bool) -> Vec<u8> {
        let mut v = Vec::new();
        if with_version {
            v.push(MCS_VERSION);
        }
        v.push(tag);
        let mut n = body.len();
        loop {
            let b = (n & 0x7F) as u8;
            n >>= 7;
            if n == 0 {
                v.push(b);
                break;
            }
            v.push(b | 0x80);
        }
        v.extend_from_slice(body);
        v
    }

    #[test]
    fn login_buffer_roundtrip() {
        let b = login_buffer("4611686018427387905", "123456", &["pid1".into()]).unwrap();
        assert_eq!(b[0], 41);
        assert_eq!(b[1], 2);
        let r = LoginRequest::decode_length_delimited(&b[2..]).unwrap();
        assert_eq!(r.id, "chrome-63.0.3234.0");
        assert_eq!(r.domain, "mcs.android.com");
        assert_eq!(r.user, "4611686018427387905");
        assert_eq!(r.resource, "4611686018427387905");
        assert_eq!(r.auth_token, "123456");
        assert_eq!(r.device_id.as_deref(), Some("android-4000000000000001"));
        assert_eq!(r.setting[0].name, "new_vc");
        assert_eq!(r.received_persistent_id, vec!["pid1".to_string()]);
        assert_eq!(r.adaptive_heartbeat, Some(false));
        assert_eq!(r.use_rmq2, Some(true));
        assert_eq!(r.auth_service, Some(2));
        assert_eq!(r.network_type, Some(1));
        assert!(login_buffer("x", "1", &[]).is_err());
    }

    #[test]
    fn parser_chunked_and_multi() {
        let lr = LoginResponse { id: "x".into(), jid: None, error: None }.encode_to_vec();
        let mut all = frame(TAG_LOGIN_RESPONSE, &lr, true);
        all.extend(frame(TAG_HEARTBEAT_PING, &[], false));
        // сообщение с размером > 127 (двухбайтовый varint)
        let big = vec![7u8; 300];
        all.extend(frame(TAG_DATA_MESSAGE, &big, false));
        let mut p = McsParser::new();
        let mut got = Vec::new();
        for ch in all.chunks(3) {
            got.extend(p.push(ch).unwrap());
        }
        assert_eq!(got.len(), 3);
        assert_eq!(got[0].0, TAG_LOGIN_RESPONSE);
        assert_eq!(got[0].1, lr);
        assert_eq!(got[1], (TAG_HEARTBEAT_PING, vec![]));
        assert_eq!(got[2].0, TAG_DATA_MESSAGE);
        assert_eq!(got[2].1.len(), 300);
    }

    #[test]
    fn parser_version_checks() {
        let mut p = McsParser::new();
        assert!(p.push(&[40, 3, 0]).is_err());
        let mut p = McsParser::new();
        assert_eq!(p.push(&[38, 3, 0]).unwrap().len(), 1);
        let mut p = McsParser::new();
        assert_eq!(p.push(&[45, 3, 0]).unwrap().len(), 1);
        let mut p = McsParser::new();
        assert!(p.push(&[]).unwrap().is_empty());
        // версия не должна читаться повторно
        assert_eq!(p.push(&[41, 3, 0]).unwrap().len(), 1);
        assert_eq!(p.push(&[3, 0]).unwrap().len(), 1);
    }

    #[test]
    fn data_message_decode() {
        let d = DataMessageStanza {
            id: None,
            from: "g".into(),
            to: None,
            category: "c".into(),
            token: None,
            app_data: vec![
                AppData { key: "channelId".into(), value: "pairing".into() },
                AppData { key: "body".into(), value: "{}".into() },
            ],
            persistent_id: Some("0:1234".into()),
            stream_id: None,
            raw_data: None,
        };
        match data_event(&d.encode_to_vec()).unwrap() {
            McsEvent::Data { persistent_id, app_data, encrypted } => {
                assert_eq!(persistent_id.as_deref(), Some("0:1234"));
                assert_eq!(app_data.len(), 2);
                assert!(!encrypted);
            }
            _ => panic!(),
        }
        let mut d2 = d.clone();
        d2.app_data.push(AppData { key: "crypto-key".into(), value: "dh=...".into() });
        match data_event(&d2.encode_to_vec()).unwrap() {
            McsEvent::Data { encrypted, .. } => assert!(encrypted),
            _ => panic!(),
        }
    }

    #[test]
    fn ack_frame_parses() {
        let mut p = McsParser::new();
        let r = p.push(&[MCS_VERSION, 1, 0]).unwrap();
        assert_eq!(r[0].0, TAG_HEARTBEAT_ACK);
        assert_eq!(heartbeat_ack_frame(), vec![1, 0]);
    }

    #[test]
    fn tls_config_builds() {
        assert!(tls_config().is_ok());
    }
}
