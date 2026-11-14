//! Разбор push-уведомлений Rust+. Формат подтверждён по коду `alexemanuelol/rustplusplus` (src/util/FcmListener.js):
//! `appData` = список {key,value}; ключи `channelId`, `title`, `message`, `body` (JSON-строка).
//! channelId=="pairing": body.type=="server" (ip, port, playerId, playerToken, name/desc/img/url/id)
//! либо body.type=="entity" (entityId, entityName: "Smart Switch" | "Smart Alarm" | "Storage Monitor").
//! Другие каналы (alarm, player, team) для pairing не нужны и игнорируются.
//!
//! **ip/port бывают пустыми.** С обновления Rust 2026-11-05 (переход на ретранслятор Facepunch) уведомление
//! о сопряжении сервера может не нести ip/port — только `id`, id сервера в ретрансляторе
//! (wss://companion-rust.facepunch.com/socket/<id>). Так же делает rustplusplus (src/util/rustRelay.js,
//! `pairingAddress`). Поэтому ip/port не обязательны: пустые/битые дают `""`/`0`, `id` идёт в `relay_id`.
//! Ошибка, только если нет ни адреса, ни id — тогда подключаться не к чему.
use serde_json::Value;

#[derive(Debug, PartialEq, Clone)]
pub struct ServerPair {
    pub name: String,
    pub ip: String,
    pub port: u16,
    pub player_id: String,
    pub player_token: String,
    /// id сервера в ретрансляторе Facepunch (`body.id`), может быть пустым у старых серверов.
    pub relay_id: String,
}

#[derive(Debug, PartialEq, Clone)]
pub struct EntityPair {
    pub id: String,
    pub name: String,
    pub kind: String, // switch | alarm | storage
    pub ip: Option<String>,
    pub port: Option<u16>,
    /// id сервера в ретрансляторе (`body.id`), по нему сверяем сервер, когда ip нет.
    pub relay_id: Option<String>,
}

/// Смерть игрока, к которому привязан Rust+ (push приходит только самому убитому).
/// Формат по rustplusplus (FcmListener.js + getPlayerDeathEmbed): channelId "player", body.type "death",
/// текст в `title` («You were killed by <ник>»), `body.targetId` — SteamID убийцы (пусто у NPC/животных),
/// `body.name` — имя сервера.
#[derive(Debug, PartialEq, Clone)]
pub struct Death {
    pub title: String,
    pub killer_id: String,
    pub server: String,
}

#[derive(Debug, PartialEq, Clone)]
pub enum Notif {
    Server(ServerPair),
    Entity(EntityPair),
    Death(Death),
    Ignored(String),
}

/// Ник убийцы из заголовка «You were killed by X» (по-русски клиент не локализует push, но на всякий случай
/// понимаем и «Вас убил X»). Не распознали — None, тогда показываем заголовок как есть.
pub fn killer_from_title(title: &str) -> Option<String> {
    let t = title.trim();
    for p in ["You were killed by ", "you were killed by ", "Вас убил ", "Тебя убил "] {
        if let Some(rest) = t.strip_prefix(p) {
            let n = rest.trim().trim_end_matches(['.', '!']).trim();
            if !n.is_empty() {
                return Some(n.to_string());
            }
        }
    }
    None
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
    let title = get(app_data, "title").unwrap_or("");
    if channel == "player" {
        let Ok(body) = serde_json::from_str::<Value>(body_raw) else {
            return Ok(Notif::Ignored("player: body не JSON".into()));
        };
        if body.get("type").and_then(|t| t.as_str()) != Some("death") {
            return Ok(Notif::Ignored("player/не death".into()));
        }
        return Ok(Notif::Death(Death {
            title: title.trim().to_string(),
            killer_id: s(&body, "targetId").unwrap_or_default(),
            server: s(&body, "name").unwrap_or_default(),
        }));
    }
    if channel != "pairing" {
        return Ok(Notif::Ignored(format!("канал «{channel}»")));
    }
    let body: Value = serde_json::from_str(body_raw).map_err(|_| "body уведомления не JSON".to_string())?;
    match body.get("type").and_then(|t| t.as_str()) {
        Some("server") => {
            // ip/port не обязательны (см. комментарий модуля) — пустая строка/0 вместо ошибки разбора.
            let ip = s(&body, "ip").unwrap_or_default();
            let port: u16 = s(&body, "port").and_then(|p| p.parse().ok()).unwrap_or(0);
            let relay_id = s(&body, "id").unwrap_or_default();
            if relay_id.is_empty() && (ip.is_empty() || port == 0) {
                return Err("в уведомлении сервера нет ни адреса (ip/port), ни id сервера".into());
            }
            let player_id = s(&body, "playerId").ok_or("в уведомлении сервера нет playerId")?;
            let player_token = s(&body, "playerToken").ok_or("в уведомлении сервера нет playerToken")?;
            if player_token.parse::<i32>().is_err() {
                return Err("playerToken не число (ожидается i32)".into());
            }
            let name = s(&body, "name").unwrap_or_else(|| {
                if !title.is_empty() { title.to_string() } else if !ip.is_empty() { ip.clone() } else { String::new() }
            });
            Ok(Notif::Server(ServerPair { name, ip, port, player_id, player_token, relay_id }))
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
                relay_id: s(&body, "id"),
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
                player_token: "-1234567".into(),
                relay_id: "uuid".into()
            })
        );
    }

    #[test]
    fn server_errors() {
        // playerToken не число — единственное, что реально ломает сопряжение, валим с ошибкой.
        let b = r#"{"type":"server","ip":"1.2.3.4","port":"1","playerId":"1","playerToken":"x"}"#;
        assert!(parse(&ad("pairing", b)).is_err());
        let b = r#"{"type":"server","id":"r","port":"1","playerId":"1"}"#; // нет playerToken вообще
        assert!(parse(&ad("pairing", b)).is_err());
        let b = r#"{"type":"server","id":"r","ip":"1.2.3.4","port":"1","playerToken":"2"}"#; // нет playerId
        assert!(parse(&ad("pairing", b)).is_err());
        assert!(parse(&ad("pairing", "not json")).is_err());
    }

    /// Живой случай после обновления Rust 2026-11-05: `type:"server"` без ip/port, только `id` ретранслятора.
    #[test]
    fn server_pairing_relay_only() {
        let b = r#"{"type":"server","id":"8c1f0e2a-relay","name":"My Server","playerId":"76561198000000000","playerToken":"-1234567"}"#;
        let n = parse(&ad("pairing", b)).unwrap();
        assert_eq!(
            n,
            Notif::Server(ServerPair {
                name: "My Server".into(),
                ip: "".into(),
                port: 0,
                player_id: "76561198000000000".into(),
                player_token: "-1234567".into(),
                relay_id: "8c1f0e2a-relay".into()
            })
        );
        // битый port при наличии id — тоже не ошибка.
        let b = r#"{"type":"server","id":"r","port":"abc","playerId":"1","playerToken":"2"}"#;
        assert!(matches!(parse(&ad("pairing", b)).unwrap(), Notif::Server(ServerPair { port: 0, .. })));
    }

    /// Нет ни адреса, ни id — подключаться не к чему, это ошибка.
    #[test]
    fn server_pairing_without_address_and_id_is_error() {
        let b = r#"{"type":"server","name":"My Server","playerId":"76561198000000000","playerToken":"-1234567"}"#;
        assert!(parse(&ad("pairing", b)).is_err());
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
    fn death_push() {
        let mut d = ad("player", r#"{"type":"death","targetId":"76561198444816488","name":"Blood Rust - Black","img":""}"#);
        d[1].1 = "You were killed by John Silver".into();
        assert_eq!(
            parse(&d).unwrap(),
            Notif::Death(Death { title: "You were killed by John Silver".into(), killer_id: "76561198444816488".into(), server: "Blood Rust - Black".into() })
        );
        // NPC: targetId пустой
        let n = parse(&ad("player", r#"{"type":"death","targetId":""}"#)).unwrap();
        assert!(matches!(n, Notif::Death(Death { ref killer_id, .. }) if killer_id.is_empty()));
        // другие типы канала player игнорируются
        assert!(matches!(parse(&ad("player", r#"{"type":"login"}"#)).unwrap(), Notif::Ignored(_)));
        assert_eq!(killer_from_title("You were killed by John Silver"), Some("John Silver".into()));
        assert_eq!(killer_from_title("You died"), None);
    }

    #[test]
    fn other_channels_ignored() {
        assert!(matches!(parse(&ad("alarm", "{}")).unwrap(), Notif::Ignored(_)));
        assert!(matches!(parse(&ad("player", "bad")).unwrap(), Notif::Ignored(_)));
        assert!(matches!(parse(&[]).unwrap(), Notif::Ignored(_)));
    }
}
