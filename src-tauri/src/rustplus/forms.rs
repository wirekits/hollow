//! Преобразование данных Rust+ в формы payload'ов, которые ждёт фронт (hollow_plus_panel_improved.html).
//! Всё здесь чистые функции без IO, чтобы их можно было тестировать.

use super::proto::*;
use serde_json::{json, Value};
use std::collections::HashMap;

/// Размер клетки сетки в фронте (CELL в HTML).
const CELL: f32 = 146.2857;

pub const ITEM_WOOD: i32 = -151838493;
pub const ITEM_STONE: i32 = -2099697608;
pub const ITEM_METAL: i32 = 69511070;
pub const ITEM_SCRAP: i32 = -932201673;

/// Нормализация мировых координат в 0..1 как ждёт фронт: u слева направо, v сверху вниз (в Rust y растёт вверх).
pub fn uv(x: f32, y: f32, map_size: f32) -> (f32, f32) {
    if map_size <= 0.0 {
        return (0.0, 0.0);
    }
    (x / map_size, 1.0 - y / map_size)
}

fn letters(mut n: i64) -> String {
    let mut s = String::new();
    n += 1;
    while n > 0 {
        let r = ((n - 1) % 26) as u8;
        s.insert(0, (65 + r) as char);
        n = (n - 1) / 26;
    }
    s
}

/// Тот же расчёт, что gridOf(u, v) во фронте.
pub fn grid_of(u: f32, v: f32, map_size: f32) -> String {
    let cu = (u * map_size / CELL).floor() as i64;
    let cv = (v * map_size / CELL).floor() as i64;
    format!("{}{}", letters(cu.max(0)), cv.max(0))
}

pub fn item_name(id: i32) -> String {
    match id {
        ITEM_WOOD => "Wood".into(),
        ITEM_STONE => "Stones".into(),
        ITEM_METAL => "Metal Fragments".into(),
        ITEM_SCRAP => "скрап".into(),
        _ => format!("item #{id}"),
    }
}

fn prettify_monument(token: &str) -> String {
    let t = token.strip_suffix("_display_name").unwrap_or(token);
    t.replace('_', " ")
}

/// setServer: { name, online, max, ping, seed, mapSize, wipeStart(мс), wipeEnd(мс) }.
/// wipeEnd в Rust+ нет, поэтому не шлём (фронт покажет «—»).
pub fn server_info_form(info: &AppInfo, ping_ms: Option<u64>) -> Value {
    let mut v = json!({
        "name": info.name,
        "online": info.players,
        "max": info.max_players,
        "seed": info.seed,
        "mapSize": info.map_size,
    });
    if info.wipe_time > 0 {
        v["wipeStart"] = json!(info.wipe_time as u64 * 1000);
    }
    if let Some(p) = ping_ms {
        v["ping"] = json!(p);
    }
    v
}

/// setTeam: [{ n, u, v, alive, online, srv, since }].
/// since: момент (мс), когда мы впервые увидели текущее online-состояние игрока.
/// У оффлайн игроков координат нет (u,v = null).
pub fn team_form(
    info: &AppTeamInfo,
    map_size: f32,
    presence: &mut HashMap<u64, (bool, i64)>,
    now_ms: i64,
) -> Value {
    let list: Vec<Value> = info
        .members
        .iter()
        .map(|m| {
            let since = match presence.get(&m.steam_id) {
                Some((on, t)) if *on == m.is_online => *t,
                _ => {
                    presence.insert(m.steam_id, (m.is_online, now_ms));
                    now_ms
                }
            };
            let (u, v) = if m.is_online && map_size > 0.0 {
                let (u, v) = uv(m.x, m.y, map_size);
                (json!(u), json!(v))
            } else {
                (Value::Null, Value::Null)
            };
            json!({
                "n": m.name, "u": u, "v": v,
                "alive": m.is_alive, "online": m.is_online,
                "srv": "", "since": since,
            })
        })
        .collect();
    Value::Array(list)
}

/// setMarkers: [{ type: "monument"|"shop"|"cargo"|"other", name, u, v, rot }].
/// rot в радианах (Rust+ отдаёт градусы).
pub fn markers_form(markers: &[AppMarker], monuments: &[Monument], map_size: f32) -> Value {
    let mut out: Vec<Value> = Vec::new();
    for mo in monuments {
        let (u, v) = uv(mo.x, mo.y, map_size);
        out.push(json!({ "type": "monument", "name": prettify_monument(&mo.token), "u": u, "v": v, "rot": 0.0 }));
    }
    for m in markers {
        let (ty, name) = match m.r#type {
            marker_type::VENDING_MACHINE => ("shop", m.name.clone().unwrap_or_default()),
            marker_type::CARGO_SHIP => ("cargo", "Cargo Ship".to_string()),
            marker_type::CH47 => ("other", "Chinook".to_string()),
            marker_type::PATROL_HELICOPTER => ("other", "Patrol Helicopter".to_string()),
            marker_type::CRATE => ("other", "Crate".to_string()),
            _ => continue, // игроки, взрывы, радиусы не рисуем
        };
        let (u, v) = uv(m.x, m.y, map_size);
        out.push(json!({ "type": ty, "name": name, "u": u, "v": v, "rot": m.rotation.to_radians() }));
    }
    Value::Array(out)
}

/// setVendings: [{ item, grid, side: "sell"|"buy", price, qty }].
/// sell: что продаёт автомат, price = "<cost> <валюта>", qty = quantity.
/// buy: автомат принимает валюту, item = валюта, price = "<quantity> <товар>", qty = cost.
pub fn vendings_form(markers: &[AppMarker], map_size: f32) -> Value {
    let mut out: Vec<Value> = Vec::new();
    for m in markers.iter().filter(|m| m.r#type == marker_type::VENDING_MACHINE) {
        let (u, v) = uv(m.x, m.y, map_size);
        let grid = grid_of(u, v, map_size);
        for o in m.sell_orders.iter().filter(|o| o.amount_in_stock > 0) {
            out.push(json!({
                "item": item_name(o.item_id), "grid": grid, "side": "sell",
                "price": format!("{} {}", o.cost_per_item, item_name(o.currency_id)), "qty": o.quantity,
            }));
            out.push(json!({
                "item": item_name(o.currency_id), "grid": grid, "side": "buy",
                "price": format!("{} {}", o.quantity, item_name(o.item_id)), "qty": o.cost_per_item,
            }));
        }
    }
    Value::Array(out)
}

/// Оценочное время жизни события на карте (сек). Rust+ его не отдаёт, это ОЦЕНКА для таймера.
fn event_ttl(key: &str) -> i64 {
    match key {
        "evCargo" => 3600,
        "evHeli" => 1800,
        _ => 1200,
    }
}

/// setEvents: [{ key, status, left, total }] по присутствию маркеров. first_seen: key -> мс первого появления.
pub fn events_form(markers: &[AppMarker], first_seen: &mut HashMap<&'static str, i64>, now_ms: i64) -> Value {
    let present = |t: i32| markers.iter().any(|m| m.r#type == t);
    let mut out = Vec::new();
    for (key, ty) in [
        ("evCargo", marker_type::CARGO_SHIP),
        ("evHeli", marker_type::PATROL_HELICOPTER),
        ("evChinook", marker_type::CH47),
    ] {
        if present(ty) {
            let t0 = *first_seen.entry(key).or_insert(now_ms);
            let total = event_ttl(key);
            let left = (total - (now_ms - t0) / 1000).max(1);
            out.push(json!({ "key": key, "status": "evOnMap", "left": left, "total": total }));
        } else {
            first_seen.remove(key);
        }
    }
    Value::Array(out)
}

/// farmPoll(id, items): items = { wood, stone, metal } (сколько сейчас в боксе).
/// None, если у payload нет ни предметов, ни ёмкости (это не монитор хранилища).
pub fn storage_update_form(entity_id: u32, p: &AppEntityPayload) -> Option<Value> {
    if p.items.is_empty() && p.capacity <= 0 {
        return None;
    }
    let sum = |id: i32| -> i64 { p.items.iter().filter(|i| i.item_id == id).map(|i| i.quantity.max(0) as i64).sum() };
    Some(json!({
        "id": entity_id.to_string(),
        "items": { "wood": sum(ITEM_WOOD), "stone": sum(ITEM_STONE), "metal": sum(ITEM_METAL) },
    }))
}

/// setDevices для известных нам переключателей (id -> состояние).
pub fn devices_form(known: &std::collections::BTreeMap<u32, bool>) -> Value {
    if known.is_empty() {
        return json!([]);
    }
    let items: Vec<Value> = known
        .iter()
        .map(|(id, on)| json!({ "id": id.to_string(), "k": "", "name": format!("#{id}"), "on": on }))
        .collect();
    json!([{ "id": "switches", "key": "g_switches", "night": false, "nightFrom": "", "items": items }])
}

#[cfg(test)]
mod tests {
    use super::*;
    use prost::Message;

    fn marker(ty: i32, x: f32, y: f32) -> AppMarker {
        AppMarker { id: 1, r#type: ty, x, y, ..Default::default() }
    }

    #[test]
    fn request_roundtrip_and_tags() {
        let req = AppRequest {
            seq: 7,
            player_id: 76561198000000000,
            player_token: -123456,
            get_info: Some(AppEmpty {}),
            ..Default::default()
        };
        let bytes = req.encode_to_vec();
        // seq (поле 1, varint) = 0x08 0x07; getInfo (поле 8, len-delimited) = 0x42 0x00 в конце
        assert_eq!(&bytes[..2], &[0x08, 0x07]);
        assert_eq!(&bytes[bytes.len() - 2..], &[0x42, 0x00]);
        let back = AppRequest::decode(&bytes[..]).unwrap();
        assert_eq!(back, req);
    }

    #[test]
    fn send_message_tag() {
        let req = AppRequest {
            seq: 1,
            send_team_message: Some(AppSendMessage { message: "hi".into() }),
            ..Default::default()
        };
        let b = req.encode_to_vec();
        // поле 13, wire type 2 -> (13<<3)|2 = 0x6A
        assert!(b.contains(&0x6A));
        assert_eq!(AppRequest::decode(&b[..]).unwrap(), req);
    }

    #[test]
    fn response_decode() {
        let msg = AppMessage {
            response: Some(AppResponse {
                seq: 3,
                info: Some(AppInfo { name: "srv".into(), map_size: 4000, players: 10, max_players: 100, seed: Some(5), ..Default::default() }),
                ..Default::default()
            }),
            broadcast: None,
        };
        let back = AppMessage::decode(&msg.encode_to_vec()[..]).unwrap();
        assert_eq!(back.response.unwrap().info.unwrap().map_size, 4000);
    }

    #[test]
    fn broadcast_decode() {
        let msg = AppMessage {
            response: None,
            broadcast: Some(AppBroadcast {
                team_message: Some(AppNewTeamMessage {
                    message: Some(AppTeamMessage { name: "a".into(), message: "yo".into(), ..Default::default() }),
                }),
                ..Default::default()
            }),
        };
        let b = AppMessage::decode(&msg.encode_to_vec()[..]).unwrap().broadcast.unwrap();
        assert_eq!(b.team_message.unwrap().message.unwrap().message, "yo");
    }

    #[test]
    fn uv_flips_y() {
        let (u, v) = uv(1000.0, 3000.0, 4000.0);
        assert!((u - 0.25).abs() < 1e-6 && (v - 0.25).abs() < 1e-6);
    }

    #[test]
    fn grid_matches_frontend() {
        // клетка 146.2857: x=0,y=top -> A0
        assert_eq!(grid_of(0.0, 0.0, 4000.0), "A0");
        assert_eq!(letters(26), "AA");
    }

    #[test]
    fn markers_to_form() {
        let mut cargo = marker(marker_type::CARGO_SHIP, 2000.0, 2000.0);
        cargo.rotation = 180.0;
        let mut shop = marker(marker_type::VENDING_MACHINE, 0.0, 4000.0);
        shop.name = Some("Shop".into());
        let ms = vec![cargo, shop, marker(1, 1.0, 1.0)];
        let mon = vec![Monument { token: "airfield_display_name".into(), x: 4000.0, y: 0.0 }];
        let v = markers_form(&ms, &mon, 4000.0);
        let a = v.as_array().unwrap();
        assert_eq!(a.len(), 3); // игрок пропущен
        assert_eq!(a[0]["type"], "monument");
        assert_eq!(a[0]["name"], "airfield");
        assert_eq!(a[0]["u"], 1.0);
        assert_eq!(a[0]["v"], 1.0);
        assert_eq!(a[1]["type"], "cargo");
        assert!((a[1]["rot"].as_f64().unwrap() - std::f64::consts::PI).abs() < 1e-4);
        assert_eq!(a[2]["type"], "shop");
        assert_eq!(a[2]["v"], 0.0);
    }

    #[test]
    fn vendings_to_form() {
        let mut m = marker(marker_type::VENDING_MACHINE, 100.0, 3900.0);
        m.sell_orders = vec![
            SellOrder { item_id: ITEM_WOOD, quantity: 1000, currency_id: ITEM_SCRAP, cost_per_item: 50, amount_in_stock: 3, ..Default::default() },
            SellOrder { item_id: 5, quantity: 1, currency_id: ITEM_SCRAP, cost_per_item: 9, amount_in_stock: 0, ..Default::default() },
        ];
        let v = vendings_form(&[m], 4000.0);
        let a = v.as_array().unwrap();
        assert_eq!(a.len(), 2);
        assert_eq!(a[0]["side"], "sell");
        assert_eq!(a[0]["price"], "50 скрап");
        assert_eq!(a[0]["qty"], 1000);
        assert_eq!(a[0]["grid"], "A0");
        assert_eq!(a[1]["side"], "buy");
    }

    #[test]
    fn team_to_form() {
        let info = AppTeamInfo {
            members: vec![
                TeamMember { steam_id: 1, name: "A".into(), x: 2000.0, y: 2000.0, is_online: true, is_alive: true, ..Default::default() },
                TeamMember { steam_id: 2, name: "B".into(), is_online: false, is_alive: true, ..Default::default() },
            ],
            ..Default::default()
        };
        let mut pres = HashMap::new();
        let v = team_form(&info, 4000.0, &mut pres, 1000);
        let a = v.as_array().unwrap();
        assert_eq!(a[0]["u"], 0.5);
        assert!(a[1]["u"].is_null());
        assert_eq!(a[0]["since"], 1000);
        let v2 = team_form(&info, 4000.0, &mut pres, 5000);
        assert_eq!(v2[0]["since"], 1000); // состояние не менялось
    }

    #[test]
    fn events_and_storage() {
        let ms = vec![marker(marker_type::CH47, 1.0, 1.0)];
        let mut fs = HashMap::new();
        let e = events_form(&ms, &mut fs, 0);
        assert_eq!(e[0]["key"], "evChinook");
        assert_eq!(e[0]["status"], "evOnMap");
        assert!(events_form(&[], &mut fs, 10).as_array().unwrap().is_empty());

        let p = AppEntityPayload {
            items: vec![EntityItem { item_id: ITEM_WOOD, quantity: 500, item_is_blueprint: false }],
            capacity: 48,
            ..Default::default()
        };
        let s = storage_update_form(9, &p).unwrap();
        assert_eq!(s["id"], "9");
        assert_eq!(s["items"]["wood"], 500);
        assert_eq!(s["items"]["metal"], 0);
        assert!(storage_update_form(9, &AppEntityPayload { value: Some(true), ..Default::default() }).is_none());
    }
}
