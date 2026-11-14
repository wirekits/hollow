//! Разбор push-уведомлений Rust+. Формат подтверждён по коду `alexemanuelol/rustplusplus` (src/util/FcmListener.js):
//! `appData` = список {key,value}; ключи `channelId`, `title`, `message`, `body` (JSON-строка).
//! channelId=="pairing": body.type=="server" (ip, port, playerId, playerToken, name/desc/img/url/id)
//! либо body.type=="entity" (entityId, entityName: "Smart Switch" | "Smart Alarm" | "Storage Monitor").
//! Другие каналы (alarm, player, team) для pairing не нужны и игнорируются.
use serde_json::Value;

#[derive(Debug, PartialEq, Clone)]
pub struct ServerPair {
    pub name: String,
    pub ip: String,
    pub port: u16,
    pub player_id: String,
    pub player_token: String,
}

#[derive(Debug, PartialEq, Clone)]
pub struct EntityPair {
    pub id: String,
    pub name: String,
    pub kind: String, // switch | alarm | storage
    pub ip: Option<String>,
    pub port: Option<u16>,
}

#[derive(Debug, PartialEq, Clone)]
pub enum Notif {
    Server(ServerPair),
    Entity(EntityPair),
    Ignored(String),
}

fn get<'a>(d: &'a [(String, String)], k: &str) -> Option<&'a str> {
    d.iter().find(|(key, _)| key == k).map(|(_, v)| v.as_str())
}

/// Значение JSON как строка: "123" и 123 равнозначны (в push все поля приходят строками, но не рискуем).
fn s(v: &Value, k: &str) -> Option<String> {
    match v.get(k)? {
        Value::String(x) if !x.is_empty() => Some(x.clone()),
        Value::Number(n) => Some(n.to_string()),
        _ => None,
    }
}

pub fn kind_from_entity_name(name: &str) -> Option<&'static str> {
    match name {
        "Smart Switch" => Some("switch"),
        "Smart Alarm" => Some("alarm"),
        "Storage Monitor" => Some("storage"),
        _ => None,
    }
}

pub fn parse(app_data: &[(String, String)]) -> Result<Notif, String> {
    let channel = get(app_data, "channelId").unwrap_or("");
    let Some(body_raw) = get(app_data, "body") else {
        return Ok(Notif::Ignored("нет поля body".into()));
    };
    if channel != "pairing" {
        return Ok(Notif::Ignored(format!("канал «{channel}»")));
    }
    let body: Value = serde_json::from_str(body_raw).map_err(|_| "body уведомления не JSON".to_string())?;
    let title = get(app_data, "title").unwrap_or("");
    match body.get("type").and_then(|t| t.as_str()) {
        Some("server") => {
            let ip = s(&body, "ip").ok_or("в уведомлении сервера нет ip")?;
            let port: u16 = s(&body, "port")
                .and_then(|p| p.parse().ok())
                .ok_or("в уведомлении сервера нет корректного port")?;
            let player_id = s(&body, "playerId").ok_or("в уведомлении сервера нет playerId")?;
            let player_token = s(&body, "playerToken").ok_or("в уведомлении сервера нет playerToken")?;
            if player_token.parse::<i32>().is_err() {
                return Err("playerToken не число (ожидается i32)".into());
            }
            let name = s(&body, "name").unwrap_or_else(|| if title.is_empty() { ip.clone() } else { title.to_string() });
            Ok(Notif::Server(ServerPair { name, ip, port, player_id, player_token }))
        }
        Some("entity") => {
            let id = s(&body, "entityId").ok_or("в уведомлении устройства нет entityId")?;
            let ename = s(&body, "entityName").unwrap_or_default();
            let Some(kind) = kind_from_entity_name(&ename) else {
                return Ok(Notif::Ignored(format!("неизвестный тип устройства «{ename}»")));
            };
            Ok(Notif::Entity(EntityPair {
                name: format!("{ename} {id}"),
                id,
                kind: kind.into(),
                ip: s(&body, "ip"),
                port: s(&body, "port").and_then(|p| p.parse().ok()),
            }))
        }
        other => Ok(Notif::Ignored(format!("pairing/{}", other.unwrap_or("?")))),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ad(channel: &str, body: &str) -> Vec<(String, String)> {
        vec![
            ("channelId".into(), channel.into()),
            ("title".into(), "Заголовок".into()),
            ("message".into(), "m".into()),
            ("body".into(), body.into()),
        ]
    }

    #[test]
    fn server_pairing() {
        let b = r#"{"img":"","port":"28017","ip":"1.2.3.4","name":"My Server","id":"uuid","type":"server","url":"","desc":"d","playerId":"76561198000000000","playerToken":"-1234567"}"#;
        let n = parse(&ad("pairing", b)).unwrap();
        assert_eq!(
            n,
            Notif::Server(ServerPair {
                name: "My Server".into(),
                ip: "1.2.3.4".into(),
                port: 28017,
                player_id: "76561198000000000".into(),
                player_token: "-1234567".into()
            })
        );
    }

    #[test]
    fn server_errors() {
        let b = r#"{"type":"server","ip":"1.2.3.4","port":"abc","playerId":"1","playerToken":"2"}"#;
        assert!(parse(&ad("pairing", b)).is_err());
        let b = r#"{"type":"server","ip":"1.2.3.4","port":"1","playerId":"1","playerToken":"x"}"#;
        assert!(parse(&ad("pairing", b)).is_err());
        let b = r#"{"type":"server","port":"1","playerId":"1","playerToken":"2"}"#;
        assert!(parse(&ad("pairing", b)).is_err());
        assert!(parse(&ad("pairing", "not json")).is_err());
    }

    #[test]
    fn entity_pairing() {
        let b = r#"{"type":"entity","entityId":"12345","entityType":"1","entityName":"Smart Switch","ip":"1.2.3.4","port":"28017"}"#;
        match parse(&ad("pairing", b)).unwrap() {
            Notif::Entity(e) => {
                assert_eq!(e.id, "12345");
                assert_eq!(e.kind, "switch");
                assert_eq!(e.port, Some(28017));
                assert_eq!(e.name, "Smart Switch 12345");
            }
            x => panic!("{x:?}"),
        }
        let b = r#"{"type":"entity","entityId":7,"entityName":"Storage Monitor"}"#;
        match parse(&ad("pairing", b)).unwrap() {
            Notif::Entity(e) => assert_eq!((e.id.as_str(), e.kind.as_str()), ("7", "storage")),
            x => panic!("{x:?}"),
        }
        let b = r#"{"type":"entity","entityId":"9","entityName":"Smart Alarm"}"#;
        assert!(matches!(parse(&ad("pairing", b)).unwrap(), Notif::Entity(e) if e.kind == "alarm"));
        let b = r#"{"type":"entity","entityId":"9","entityName":"Something Else"}"#;
        assert!(matches!(parse(&ad("pairing", b)).unwrap(), Notif::Ignored(_)));
    }

    #[test]
    fn other_channels_ignored() {
        assert!(matches!(parse(&ad("alarm", "{}")).unwrap(), Notif::Ignored(_)));
        assert!(matches!(parse(&ad("player", "bad")).unwrap(), Notif::Ignored(_)));
        assert!(matches!(parse(&[]).unwrap(), Notif::Ignored(_)));
    }
}
