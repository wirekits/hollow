//! FCM-регистрация «Android-устройства» Rust+ (по `@liamcottle/push-receiver`, src/android/fcm.js и src/gcm/index.js),
//! получение Expo push token и регистрация токена в Rust+ companion API (по `rustplus.js`, cli/index.js).
//! Все константы ниже взяты из этих исходников, не придуманы.
use base64::Engine;
use prost::Message;
use serde::{Deserialize, Serialize};
use std::time::Duration;

pub const API_KEY: &str = "AIzaSyB5y2y-Tzqb4-I4Qnlsh_9naYv_TD8pCvY";
pub const PROJECT_ID: &str = "rust-companion-app";
pub const GCM_SENDER_ID: &str = "976529667804";
pub const GMS_APP_ID: &str = "1:976529667804:android:d6f1ddeb4403b338fea619";
pub const ANDROID_PACKAGE: &str = "com.facepunch.rust.companion";
pub const ANDROID_CERT: &str = "E28D05345FB78A7A1A63D70F4A302DBF426CA5AD";
pub const EXPO_PROJECT_ID: &str = "49451aca-a822-41e6-ad59-955718d0ff9c";
pub const EXPO_URL: &str = "https://exp.host/--/api/v2/push/getExpoPushToken";
pub const RUSTPLUS_REGISTER_URL: &str = "https://companion-rust.facepunch.com:443/api/push/register";
pub const CHECKIN_URL: &str = "https://android.clients.google.com/checkin";
pub const REGISTER3_URL: &str = "https://android.clients.google.com/c2dm/register3";

const FIREBASE_CLIENT: &str = "android-min-sdk/23 fire-core/20.0.0 device-name/a21snnxx device-brand/samsung device-model/a21s android-installer/com.android.vending fire-android/30 fire-installations/17.0.0 fire-fcm/22.0.0 android-platform/ kotlin/1.9.23 android-target-sdk/34";
const FIREBASE_UA: &str = "Dalvik/2.1.0 (Linux; U; Android 11; SM-A217F Build/RP1A.200720.012)";

/// Учётные данные FCM/GCM (то, что нужно для MCS-логина).
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct FcmCreds {
    pub android_id: String,
    pub security_token: String,
    pub fcm_token: String,
}

// ---------- protobuf checkin (checkin.proto / android_checkin.proto из push-receiver) ----------

#[derive(Clone, PartialEq, Message)]
pub struct ChromeBuildProto {
    #[prost(int32, optional, tag = "1")]
    pub platform: Option<i32>,
    #[prost(string, optional, tag = "2")]
    pub chrome_version: Option<String>,
    #[prost(int32, optional, tag = "3")]
    pub channel: Option<i32>,
}

#[derive(Clone, PartialEq, Message)]
pub struct AndroidCheckinProto {
    #[prost(int32, optional, tag = "12")]
    pub r#type: Option<i32>,
    #[prost(message, optional, tag = "13")]
    pub chrome_build: Option<ChromeBuildProto>,
}

#[derive(Clone, PartialEq, Message)]
pub struct AndroidCheckinRequest {
    #[prost(int64, optional, tag = "2")]
    pub id: Option<i64>,
    #[prost(message, optional, tag = "4")]
    pub checkin: Option<AndroidCheckinProto>,
    #[prost(fixed64, optional, tag = "13")]
    pub security_token: Option<u64>,
    #[prost(int32, optional, tag = "14")]
    pub version: Option<i32>,
    #[prost(int32, optional, tag = "22")]
    pub user_serial_number: Option<i32>,
}

#[derive(Clone, PartialEq, Message)]
pub struct AndroidCheckinResponse {
    #[prost(bool, optional, tag = "1")]
    pub stats_ok: Option<bool>,
    #[prost(fixed64, optional, tag = "7")]
    pub android_id: Option<u64>,
    #[prost(fixed64, optional, tag = "8")]
    pub security_token: Option<u64>,
}

/// Тело checkin-запроса ровно как в `getCheckinRequest` push-receiver:
/// userSerialNumber 0, checkin{type 3, chromeBuild{platform 2, chromeVersion '63.0.3234.0', channel 1}}, version 3.
/// Первичный checkin: id и securityToken не заданы.
pub fn build_checkin_request(android_id: Option<&str>, security_token: Option<&str>) -> Result<Vec<u8>, String> {
    let id = match android_id {
        Some(s) if !s.is_empty() => Some(s.parse::<i64>().map_err(|_| "некорректный androidId".to_string())?),
        _ => None,
    };
    let st = match security_token {
        Some(s) if !s.is_empty() => Some(s.parse::<u64>().map_err(|_| "некорректный securityToken".to_string())?),
        _ => None,
    };
    let req = AndroidCheckinRequest {
        id,
        checkin: Some(AndroidCheckinProto {
            r#type: Some(3),
            chrome_build: Some(ChromeBuildProto {
                platform: Some(2),
                chrome_version: Some("63.0.3234.0".into()),
                channel: Some(1),
            }),
        }),
        security_token: st,
        version: Some(3),
        user_serial_number: Some(0),
    };
    Ok(req.encode_to_vec())
}

/// Разбор ответа checkin: (androidId, securityToken) десятичными строками (как `longs: String` в JS).
pub fn parse_checkin_response(body: &[u8]) -> Result<(String, String), String> {
    let r = AndroidCheckinResponse::decode(body).map_err(|e| format!("не удалось разобрать ответ checkin: {e}"))?;
    match (r.android_id, r.security_token) {
        (Some(a), Some(s)) if a != 0 && s != 0 => Ok((a.to_string(), s.to_string())),
        _ => Err("checkin не вернул androidId/securityToken".into()),
    }
}

// ---------- чистые помощники ----------

/// Firebase Installation ID: 17 случайных байт, первые 4 бита = 0b0111, base64 без «=» (generateFirebaseFID).
pub fn fid_from_bytes(mut b: [u8; 17]) -> String {
    b[0] = 0b0111_0000 | (b[0] & 0b0000_1111);
    base64::engine::general_purpose::STANDARD.encode(b).replace('=', "")
}

fn random_fid() -> String {
    let a = uuid::Uuid::new_v4();
    let c = uuid::Uuid::new_v4();
    let mut b = [0u8; 17];
    b[..16].copy_from_slice(a.as_bytes());
    b[16] = c.as_bytes()[0];
    fid_from_bytes(b)
}

/// Ответ register3: `token=...` или `Error=...`. Как в источнике: Error -> повтор, токен = текст после первого «=».
pub fn parse_register3(resp: &str) -> Result<String, ()> {
    if resp.contains("Error") {
        return Err(());
    }
    match resp.split('=').nth(1) {
        Some(t) if !t.trim().is_empty() => Ok(t.trim().to_string()),
        _ => Err(()),
    }
}

/// Убирает из текста ошибки всё, что похоже на токен (на случай, если reqwest положит URL в сообщение).
pub fn scrub(s: &str) -> String {
    s.split_whitespace()
        .map(|w| if w.len() > 40 && !w.contains('/') { "***" } else { w })
        .collect::<Vec<_>>()
        .join(" ")
}

fn client() -> Result<reqwest::Client, String> {
    reqwest::Client::builder()
        .timeout(Duration::from_secs(20))
        .build()
        .map_err(|e| scrub(&e.to_string()))
}

// ---------- сетевые шаги ----------

async fn installation_auth_token(c: &reqwest::Client) -> Result<String, String> {
    let url = format!("https://firebaseinstallations.googleapis.com/v1/projects/{PROJECT_ID}/installations");
    let body = serde_json::json!({
        "fid": random_fid(),
        "appId": GMS_APP_ID,
        "authVersion": "FIS_v2",
        "sdkVersion": "a:17.0.0",
    });
    let resp = c
        .post(url)
        .header("Accept", "application/json")
        .header("X-Android-Package", ANDROID_PACKAGE)
        .header("X-Android-Cert", ANDROID_CERT)
        .header("x-firebase-client", FIREBASE_CLIENT)
        .header("x-firebase-client-log-type", "3")
        .header("x-goog-api-key", API_KEY)
        .header("User-Agent", FIREBASE_UA)
        .json(&body)
        .send()
        .await
        .map_err(|e| format!("Firebase Installations: {}", scrub(&e.to_string())))?;
    let status = resp.status();
    let v: serde_json::Value = resp
        .json()
        .await
        .map_err(|_| format!("Firebase Installations: нечитаемый ответ (HTTP {status})"))?;
    v.pointer("/authToken/token")
        .and_then(|t| t.as_str())
        .map(|s| s.to_string())
        .ok_or_else(|| format!("Firebase Installations не вернул authToken (HTTP {status})"))
}

async fn checkin(c: &reqwest::Client) -> Result<(String, String), String> {
    let body = build_checkin_request(None, None)?;
    let resp = c
        .post(CHECKIN_URL)
        .header("Content-Type", "application/x-protobuf")
        .body(body)
        .send()
        .await
        .map_err(|e| format!("checkin: {}", scrub(&e.to_string())))?;
    let status = resp.status();
    let bytes = resp.bytes().await.map_err(|e| format!("checkin: {}", scrub(&e.to_string())))?;
    if !status.is_success() {
        return Err(format!("checkin: HTTP {status}"));
    }
    parse_checkin_response(&bytes)
}

async fn register3(
    c: &reqwest::Client,
    android_id: &str,
    security_token: &str,
    install_token: &str,
) -> Result<String, String> {
    let form: Vec<(&str, &str)> = vec![
        ("device", android_id),
        ("app", ANDROID_PACKAGE),
        ("cert", ANDROID_CERT),
        ("app_ver", "1"),
        ("X-subtype", GCM_SENDER_ID),
        ("X-app_ver", "1"),
        ("X-osv", "29"),
        ("X-cliv", "fiid-21.1.1"),
        ("X-gmsv", "220217001"),
        ("X-scope", "*"),
        ("X-Goog-Firebase-Installations-Auth", install_token),
        ("X-gms_app_id", GMS_APP_ID),
        ("X-Firebase-Client", FIREBASE_CLIENT),
        ("X-Firebase-Client-Log-Type", "1"),
        ("X-app_ver_name", "1"),
        ("target_ver", "31"),
        ("sender", GCM_SENDER_ID),
    ];
    for attempt in 0..=5 {
        let resp = c
            .post(REGISTER3_URL)
            .header("Authorization", format!("AidLogin {android_id}:{security_token}"))
            .form(&form)
            .send()
            .await
            .map_err(|e| format!("GCM register: {}", scrub(&e.to_string())))?;
        let text = resp.text().await.map_err(|e| format!("GCM register: {}", scrub(&e.to_string())))?;
        if let Ok(t) = parse_register3(&text) {
            return Ok(t);
        }
        if attempt < 5 {
            tokio::time::sleep(Duration::from_secs(1)).await;
        }
    }
    Err("GCM register не удался (сервер отвечает ошибкой)".into())
}

/// Полная FCM-регистрация. Порядок как в `AndroidFCM.register`: installation -> checkin -> register3.
pub async fn fcm_register() -> Result<FcmCreds, String> {
    let c = client()?;
    let install = installation_auth_token(&c).await?;
    let (android_id, security_token) = checkin(&c).await?;
    let fcm_token = register3(&c, &android_id, &security_token, &install).await?;
    Ok(FcmCreds { android_id, security_token, fcm_token })
}

/// Expo push token (rustplus.js getExpoPushToken): ответ `data.data.expoPushToken`.
pub async fn expo_token(fcm_token: &str) -> Result<String, String> {
    let c = client()?;
    let body = serde_json::json!({
        "type": "fcm",
        "deviceId": uuid::Uuid::new_v4().to_string(),
        "development": false,
        "appId": ANDROID_PACKAGE,
        "deviceToken": fcm_token,
        "projectId": EXPO_PROJECT_ID,
    });
    let resp = c
        .post(EXPO_URL)
        .json(&body)
        .send()
        .await
        .map_err(|e| format!("Expo: {}", scrub(&e.to_string())))?;
    let status = resp.status();
    let v: serde_json::Value = resp.json().await.map_err(|_| format!("Expo: нечитаемый ответ (HTTP {status})"))?;
    v.pointer("/data/expoPushToken")
        .and_then(|t| t.as_str())
        .map(|s| s.to_string())
        .ok_or_else(|| format!("Expo не вернул expoPushToken (HTTP {status})"))
}

pub fn rustplus_register_body(auth_token: &str, expo: &str) -> serde_json::Value {
    serde_json::json!({
        "AuthToken": auth_token,
        "DeviceId": "rustplus.js",
        "PushKind": 3,
        "PushToken": expo,
    })
}

/// Регистрация Expo-токена в Rust+ companion API. Успех = любой 2xx (тело в источнике не читается).
pub async fn rustplus_register(auth_token: &str, expo: &str) -> Result<(), String> {
    let c = client()?;
    let resp = c
        .post(RUSTPLUS_REGISTER_URL)
        .json(&rustplus_register_body(auth_token, expo))
        .send()
        .await
        .map_err(|e| format!("Rust+ register: {}", scrub(&e.to_string())))?;
    let status = resp.status();
    if status.is_success() {
        Ok(())
    } else {
        Err(format!("Rust+ register отклонён: HTTP {status}"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fid_format() {
        let f = fid_from_bytes([0xFF; 17]);
        assert_eq!(f.len(), 23);
        assert!(!f.contains('='));
        let raw = base64::engine::general_purpose::STANDARD_NO_PAD.decode(&f).unwrap();
        assert_eq!(raw[0], 0x7F);
        let f2 = fid_from_bytes([0x00; 17]);
        let raw2 = base64::engine::general_purpose::STANDARD_NO_PAD.decode(&f2).unwrap();
        assert_eq!(raw2[0], 0x70);
        assert_eq!(random_fid().len(), 23);
    }

    #[test]
    fn checkin_request_bytes() {
        let b = build_checkin_request(None, None).unwrap();
        let r = AndroidCheckinRequest::decode(b.as_slice()).unwrap();
        assert_eq!(r.id, None);
        assert_eq!(r.security_token, None);
        assert_eq!(r.version, Some(3));
        assert_eq!(r.user_serial_number, Some(0));
        let c = r.checkin.unwrap();
        assert_eq!(c.r#type, Some(3));
        let cb = c.chrome_build.unwrap();
        assert_eq!((cb.platform, cb.channel), (Some(2), Some(1)));
        assert_eq!(cb.chrome_version.as_deref(), Some("63.0.3234.0"));
        // повторный checkin: id знаковый, token беззнаковый fixed64
        let b = build_checkin_request(Some("4611686018427387905"), Some("18446744073709551615")).unwrap();
        let r = AndroidCheckinRequest::decode(b.as_slice()).unwrap();
        assert_eq!(r.id, Some(4611686018427387905));
        assert_eq!(r.security_token, Some(u64::MAX));
        assert!(build_checkin_request(Some("abc"), None).is_err());
    }

    #[test]
    fn checkin_response_parse() {
        let r = AndroidCheckinResponse { stats_ok: Some(true), android_id: Some(5555), security_token: Some(u64::MAX) };
        let (a, s) = parse_checkin_response(&r.encode_to_vec()).unwrap();
        assert_eq!(a, "5555");
        assert_eq!(s, "18446744073709551615");
        let empty = AndroidCheckinResponse { stats_ok: Some(true), android_id: None, security_token: None };
        assert!(parse_checkin_response(&empty.encode_to_vec()).is_err());
        assert!(parse_checkin_response(&[0xFF, 0xFF]).is_err());
    }

    #[test]
    fn register3_parse() {
        assert_eq!(parse_register3("token=abc:def-ghi").unwrap(), "abc:def-ghi");
        assert!(parse_register3("Error=PHONE_REGISTRATION_ERROR").is_err());
        assert!(parse_register3("").is_err());
        assert!(parse_register3("token=").is_err());
    }

    #[test]
    fn rustplus_body_shape() {
        let v = rustplus_register_body("AT", "ExponentPushToken[x]");
        assert_eq!(v["AuthToken"], "AT");
        assert_eq!(v["DeviceId"], "rustplus.js");
        assert_eq!(v["PushKind"], 3);
        assert_eq!(v["PushToken"], "ExponentPushToken[x]");
    }

    #[test]
    fn scrub_hides_long_words() {
        let long = "a".repeat(60);
        assert_eq!(scrub(&format!("error {long} end")), "error *** end");
        assert_eq!(scrub("connection refused"), "connection refused");
    }
}
