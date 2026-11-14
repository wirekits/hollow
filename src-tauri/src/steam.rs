//! Обогащение профиля игрока данными Steam Web API.
//!
//! Подтверждено документацией/источниками: эндпоинты и параметры Steam Web API,
//! форма ответов (players[], bans, friendslist, games, player_level, playerstats.stats[{name,value}]),
//! имена статистик Rust `kill_player`, `bullet_fired`, `deaths` (пример ответа Rust в сторонней доке).
//! НЕ подтверждено из надёжного источника: `headshot`, `bullet_hit*`, `death_player`.
//! Поэтому разбор статистики терпимый: перебирает кандидатов, и если ключа нет, поле остаётся null.
use serde_json::{json, Map, Value};
use std::collections::HashMap;
use std::time::Duration;
use tauri::{AppHandle, Manager};

pub mod stats;

const MAX_FRIENDS_LIST: usize = 6;
const RUST_APPID: u32 = 252490;
const BASE: &str = "https://api.steampowered.com";
const TIMEOUT: Duration = Duration::from_secs(10);
const MAX_FRIEND_NAMES: usize = 20;

// ---------------------------------------------------------------- чистые функции разбора

#[derive(Debug, Clone, PartialEq, Default)]
pub struct Summary {
    pub avatar: Option<String>,
    pub personaname: Option<String>,
    pub timecreated: Option<i64>,
    /// communityvisibilitystate: 3 = публичный, 1 = приватный, 2 = только друзья и т.д.
    pub visibility: Option<i64>,
}

impl Summary {
    pub fn is_public(&self) -> bool {
        self.visibility == Some(3)
    }
}

#[derive(Debug, Clone, PartialEq, Default)]
pub struct Bans {
    pub community_banned: bool,
    pub vac_banned: bool,
    pub vac_bans: i64,
    pub game_bans: i64,
    pub days_since_last_ban: Option<i64>,
    pub economy_ban: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Default)]
pub struct RustStats {
    pub kills: Option<i64>,
    pub deaths: Option<i64>,
    pub headshots: Option<i64>,
    pub fired: Option<i64>,
    pub hit: Option<i64>,
}

pub fn parse_summaries(v: &Value) -> HashMap<String, Summary> {
    let mut out = HashMap::new();
    let Some(arr) = v.pointer("/response/players").and_then(|p| p.as_array()) else {
        return out;
    };
    for p in arr {
        let Some(id) = p.get("steamid").and_then(|x| x.as_str()) else { continue };
        let s = |k: &str| p.get(k).and_then(|x| x.as_str()).filter(|x| !x.is_empty()).map(String::from);
        out.insert(
            id.to_string(),
            Summary {
                avatar: s("avatarfull").or_else(|| s("avatarmedium")).or_else(|| s("avatar")),
                personaname: s("personaname"),
                timecreated: p.get("timecreated").and_then(|x| x.as_i64()),
                visibility: p.get("communityvisibilitystate").and_then(|x| x.as_i64()),
            },
        );
    }
    out
}

pub fn parse_bans(v: &Value) -> Option<Bans> {
    let p = v.get("players")?.as_array()?.first()?;
    let b = |k: &str| p.get(k).and_then(|x| x.as_bool()).unwrap_or(false);
    let n = |k: &str| p.get(k).and_then(|x| x.as_i64()).unwrap_or(0);
    let econ = p
        .get("EconomyBan")
        .and_then(|x| x.as_str())
        .filter(|s| !s.is_empty() && *s != "none")
        .map(String::from);
    Some(Bans {
        community_banned: b("CommunityBanned"),
        vac_banned: b("VACBanned"),
        vac_bans: n("NumberOfVACBans"),
        game_bans: n("NumberOfGameBans"),
        days_since_last_ban: p.get("DaysSinceLastBan").and_then(|x| x.as_i64()),
        economy_ban: econ,
    })
}

pub fn parse_level(v: &Value) -> Option<i64> {
    v.pointer("/response/player_level").and_then(|x| x.as_i64())
}

/// Минуты наигранного в игре -> часы (округление). `None`, если список игр скрыт или игры нет.
pub fn parse_owned_hours(v: &Value, appid: u32) -> Option<i64> {
    let games = v.pointer("/response/games")?.as_array()?;
    let g = games.iter().find(|g| g.get("appid").and_then(|a| a.as_u64()) == Some(appid as u64))?;
    let min = g.get("playtime_forever")?.as_f64()?;
    Some((min / 60.0).round() as i64)
}

pub fn parse_friend_ids(v: &Value) -> Option<Vec<String>> {
    let arr = v.pointer("/friendslist/friends")?.as_array()?;
    Some(arr.iter().filter_map(|f| f.get("steamid").and_then(|s| s.as_str()).map(String::from)).collect())
}

/// До MAX_FRIENDS_LIST друзей [{name, avatar}]; без профиля имя = steamid, аватар = null.
pub fn friends_list(ids: &[String], sums: &HashMap<String, Summary>) -> Vec<Value> {
    ids.iter()
        .take(MAX_FRIENDS_LIST)
        .map(|id| {
            let s = sums.get(id);
            json!({
                "name": s.and_then(|s| s.personaname.clone()).unwrap_or_else(|| id.clone()),
                "avatar": s.and_then(|s| s.avatar.clone()),
            })
        })
        .collect()
}

fn stat(map: &HashMap<String, i64>, keys: &[&str]) -> Option<i64> {
    keys.iter().find_map(|k| map.get(*k).copied())
}

pub fn parse_rust_stats(v: &Value) -> Option<RustStats> {
    let arr = v.pointer("/playerstats/stats")?.as_array()?;
    let map: HashMap<String, i64> = arr
        .iter()
        .filter_map(|s| {
            let n = s.get("name")?.as_str()?.to_string();
            let val = s.get("value")?.as_f64()?;
            Some((n, val as i64))
        })
        .collect();
    Some(RustStats {
        kills: stat(&map, &["kill_player"]),
        deaths: stat(&map, &["deaths", "death_player"]),
        headshots: stat(&map, &["headshot"]),
        fired: stat(&map, &["bullet_fired"]),
        hit: stat(&map, &["bullet_hit_player", "bullet_hit"]),
    })
}

/// K/D с двумя знаками. 0 смертей -> просто число убийств.
pub fn kdr(kills: i64, deaths: i64) -> f64 {
    let r = if deaths > 0 { kills as f64 / deaths as f64 } else { kills as f64 };
    (r * 100.0).round() / 100.0
}

/// Точность в процентах (1 знак). None, если нет выстрелов или попаданий больше выстрелов (статистика неверна).
pub fn accuracy(fired: i64, hit: i64) -> Option<f64> {
    if fired <= 0 || hit < 0 || hit > fired {
        return None;
    }
    Some((hit as f64 / fired as f64 * 1000.0).round() / 10.0)
}

/// Unix-время -> "YYYY-MM-DD" (UTC), без внешних зависимостей.
pub fn format_date(ts: i64) -> String {
    let days = ts.div_euclid(86_400);
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = if m <= 2 { y + 1 } else { y };
    format!("{y:04}-{m:02}-{d:02}")
}

// ---------------------------------------------------------------- риск

#[derive(Debug, Clone, Default)]
pub struct RiskInput {
    pub vac_bans: i64,
    pub game_bans: i64,
    pub community_banned: bool,
    pub days_since_last_ban: Option<i64>,
    pub account_age_days: Option<i64>,
    pub rust_hours: Option<i64>,
    pub profile_private: bool,
    pub kills: Option<i64>,
    pub kdr: Option<f64>,
    pub acc: Option<f64>,
    pub level: Option<i64>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Risk {
    pub score: u32,
    pub level: &'static str,
    pub reasons: Vec<String>,
}

/// Чистая оценка риска 0..100. Уровни: <30 low, 30..59 mid, >=60 hi.
pub fn risk_score(i: &RiskInput) -> Risk {
    let mut s: u32 = 0;
    let mut r: Vec<String> = Vec::new();
    if i.vac_bans > 0 {
        s += 50;
        r.push(format!("VAC-банов: {}", i.vac_bans));
    }
    if i.game_bans > 0 {
        s += 40;
        r.push(format!("игровых банов: {}", i.game_bans));
    }
    if i.vac_bans > 0 || i.game_bans > 0 {
        if let Some(d) = i.days_since_last_ban {
            if d < 365 {
                s += 15;
                r.push(format!("последний бан {d} дн. назад"));
            }
        }
    }
    if i.community_banned {
        s += 10;
        r.push("бан сообщества".into());
    }
    if let Some(a) = i.account_age_days {
        if a < 30 {
            s += 25;
            r.push(format!("аккаунту {a} дн."));
        } else if a < 180 {
            s += 10;
            r.push(format!("свежий аккаунт ({a} дн.)"));
        }
    }
    if let Some(h) = i.rust_hours {
        if h < 10 {
            s += 20;
            r.push(format!("мало часов в Rust: {h}"));
        } else if h < 50 {
            s += 10;
            r.push(format!("мало часов в Rust: {h}"));
        }
    }
    if i.profile_private {
        s += 10;
        r.push("профиль скрыт".into());
    }
    let enough_kills = i.kills.map_or(false, |k| k >= 50);
    if enough_kills {
        if let Some(k) = i.kdr {
            if k > 8.0 {
                s += 15;
                r.push(format!("аномальный K/D: {k}"));
            }
        }
    }
    if let Some(a) = i.acc {
        if a > 70.0 {
            s += 15;
            r.push(format!("аномальная точность: {a}%"));
        }
    }
    if i.level == Some(0) {
        s += 5;
        r.push("уровень Steam 0".into());
    }
    let score = s.min(100);
    let level = if score >= 60 {
        "hi"
    } else if score >= 30 {
        "mid"
    } else {
        "low"
    };
    Risk { score, level, reasons: r }
}

fn risk_json(r: &Risk) -> Value {
    json!({ "score": r.score, "level": r.level, "reasons": r.reasons })
}

// ---------------------------------------------------------------- сеть

/// Убирает из текста ошибки всё, что может содержать ключ.
fn scrub(msg: &str, key: &str) -> String {
    if key.is_empty() {
        msg.to_string()
    } else {
        msg.replace(key, "***")
    }
}

async fn get_json(
    client: &reqwest::Client,
    key: &str,
    path: &str,
    params: &[(&str, String)],
) -> Result<Value, String> {
    let mut q: Vec<(&str, String)> = vec![("key", key.to_string())];
    q.extend(params.iter().cloned());
    let resp = client
        .get(format!("{BASE}/{path}"))
        .query(&q)
        .send()
        .await
        .map_err(|e| scrub(&e.without_url().to_string(), key))?;
    let status = resp.status();
    if !status.is_success() {
        return Err(match status.as_u16() {
            401 | 403 => "доступ запрещён (ключ неверный или данные скрыты)".to_string(),
            429 => "лимит запросов Steam".to_string(),
            c => format!("HTTP {c}"),
        });
    }
    resp.json::<Value>().await.map_err(|e| scrub(&e.without_url().to_string(), key))
}

fn active_key(app: &AppHandle) -> Option<String> {
    let st = app.try_state::<crate::profiles::ProfilesState>()?;
    st.active_profile()?.steam_key.filter(|k| !k.trim().is_empty() && k != "•••")
}

fn is_steam_id(s: &str) -> bool {
    s.len() == 17 && s.starts_with("7656119") && s.bytes().all(|b| b.is_ascii_digit())
}

pub async fn enrich(app: &AppHandle, steam_id: &str, base: Value) -> Value {
    let Some(key) = active_key(app) else {
        return with_note(base, "Ключ Steam API не задан: данные Steam недоступны");
    };
    if !is_steam_id(steam_id) {
        return with_note(base, "Некорректный SteamID: данные Steam недоступны");
    }
    enrich_with_key(&key, steam_id, base, now_secs()).await
}

fn now_secs() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

fn with_note(mut base: Value, note: &str) -> Value {
    if let Some(o) = base.as_object_mut() {
        o.insert("steamNote".into(), json!(note));
    }
    base
}

async fn enrich_with_key(key: &str, sid: &str, base: Value, now: i64) -> Value {
    let client = match reqwest::Client::builder().timeout(TIMEOUT).build() {
        Ok(c) => c,
        Err(_) => return with_note(base, "Не удалось создать HTTP-клиент для Steam"),
    };
    let sid_s = sid.to_string();
    let app_s = RUST_APPID.to_string();
    let p_sid1 = [("steamids", sid_s.clone())];
    let p_sid2 = [("steamid", sid_s.clone())];
    let p_owned = [
        ("steamid", sid_s.clone()),
        ("include_played_free_games", "1".to_string()),
        ("appids_filter[0]", app_s.clone()),
    ];
    let p_stats = [("steamid", sid_s.clone()), ("appid", app_s.clone())];
    let p_friends = [("steamid", sid_s.clone()), ("relationship", "friend".to_string())];
    let (sum, bans, lvl, owned, stats, friends) = tokio::join!(
        get_json(&client, key, "ISteamUser/GetPlayerSummaries/v2/", &p_sid1),
        get_json(&client, key, "ISteamUser/GetPlayerBans/v1/", &p_sid1),
        get_json(&client, key, "IPlayerService/GetSteamLevel/v1/", &p_sid2),
        get_json(&client, key, "IPlayerService/GetOwnedGames/v1/", &p_owned),
        get_json(&client, key, "ISteamUserStats/GetUserStatsForGame/v2/", &p_stats),
        get_json(&client, key, "ISteamUser/GetFriendList/v1/", &p_friends),
    );
    // имена друзей: второй шаг, одним батчем (до 20)
    let friend_ids = friends.as_ref().ok().and_then(parse_friend_ids);
    let friend_names: HashMap<String, Summary> = match &friend_ids {
        Some(ids) if !ids.is_empty() => {
            let batch = ids.iter().take(MAX_FRIEND_NAMES).cloned().collect::<Vec<_>>().join(",");
            match get_json(&client, key, "ISteamUser/GetPlayerSummaries/v2/", &[("steamids", batch)]).await {
                Ok(v) => parse_summaries(&v),
                Err(_) => HashMap::new(),
            }
        }
        _ => HashMap::new(),
    };

    let mut notes: Vec<String> = Vec::new();
    let summary = sum.as_ref().ok().and_then(|v| parse_summaries(v).remove(sid));
    if summary.is_none() {
        notes.push(match &sum {
            Err(e) => format!("профиль Steam недоступен ({e})"),
            Ok(_) => "профиль Steam не найден".to_string(),
        });
    }
    let bans_v = bans.as_ref().ok().and_then(parse_bans);
    if bans_v.is_none() {
        notes.push("баны Steam недоступны".into());
    }
    let level = lvl.as_ref().ok().and_then(parse_level);
    let hours = owned.as_ref().ok().and_then(|v| parse_owned_hours(v, RUST_APPID));
    let rust_stats = stats.as_ref().ok().and_then(parse_rust_stats);
    let private = summary.as_ref().map_or(false, |s| !s.is_public());
    if rust_stats.is_none() {
        notes.push("статистика Rust недоступна (профиль или статистика игры закрыты)".into());
    }
    if hours.is_none() && summary.is_some() {
        notes.push("часы в Rust скрыты".into());
    }
    if friend_ids.is_none() {
        notes.push("список друзей скрыт".into());
    }

    let mut out: Map<String, Value> = base.as_object().cloned().unwrap_or_default();
    let mut put = |k: &str, v: Value| {
        out.insert(k.to_string(), v);
    };

    let mut kdr_v = None;
    let mut acc_v = None;
    if let Some(s) = &rust_stats {
        if let (Some(k), Some(d)) = (s.kills, s.deaths) {
            kdr_v = Some(kdr(k, d));
        }
        if let (Some(f), Some(h)) = (s.fired, s.hit) {
            acc_v = accuracy(f, h);
        }
        put("kills", json!(s.kills));
        put("deaths", json!(s.deaths));
        put("hs", json!(s.headshots));
        if let Some(k) = kdr_v {
            put("kdr", json!(k));
        }
        if let Some(a) = acc_v {
            put("acc", json!(a));
        }
    } else {
        for k in ["kills", "deaths", "hs"] {
            put(k, Value::Null);
        }
    }
    if let Some(h) = hours {
        put("hours", json!(h));
    }
    if let Some(l) = level {
        put("lvl", json!(l));
    }
    put("avatar", json!(summary.as_ref().and_then(|s| s.avatar.clone())));
    put("personaname", json!(summary.as_ref().and_then(|s| s.personaname.clone())));
    let created_ts = summary.as_ref().and_then(|s| s.timecreated);
    put("created", json!(created_ts.map(format_date)));
    put("profilePublic", json!(summary.as_ref().map(|s| s.is_public())));

    if let Some(ids) = &friend_ids {
        let names: Vec<String> = ids
            .iter()
            .take(MAX_FRIEND_NAMES)
            .map(|id| {
                friend_names
                    .get(id)
                    .and_then(|s| s.personaname.clone())
                    .unwrap_or_else(|| id.clone())
            })
            .collect();
        put("friends", json!(names));
        put("friendsCount", json!(ids.len()));
        put("friendsList", json!(friends_list(ids, &friend_names)));
    }

    // сырая статистика и производные (наша оценка, см. steam/stats.rs)
    match stats.as_ref().ok().and_then(stats::raw_stats) {
        Some(raw) => {
            put("stats", stats::stats_json(&raw));
            put("derived", stats::derive(&raw));
        }
        None => {
            put("stats", Value::Null);
            put("derived", Value::Null);
        }
    }

    if let Some(b) = &bans_v {
        put(
            "steamBans",
            json!({
                "vac": b.vac_bans, "game": b.game_bans, "community": b.community_banned,
                "daysSinceLastBan": b.days_since_last_ban, "economy": b.economy_ban,
            }),
        );
        let mut list: Vec<Value> = out.get("bans").and_then(|v| v.as_array()).cloned().unwrap_or_default();
        if b.vac_bans > 0 {
            list.push(json!(format!("Steam: VAC-банов {}", b.vac_bans)));
        }
        if b.game_bans > 0 {
            list.push(json!(format!("Steam: игровых банов {}", b.game_bans)));
        }
        if b.community_banned {
            list.push(json!("Steam: бан сообщества"));
        }
        out.insert("bans".into(), Value::Array(list));
    }

    // риск считаем только если получили хоть что-то осмысленное
    if summary.is_some() || bans_v.is_some() {
        let rust_hours = hours.or_else(|| out.get("hours").and_then(|v| v.as_i64()));
        let input = RiskInput {
            vac_bans: bans_v.as_ref().map_or(0, |b| b.vac_bans),
            game_bans: bans_v.as_ref().map_or(0, |b| b.game_bans),
            community_banned: bans_v.as_ref().map_or(false, |b| b.community_banned),
            days_since_last_ban: bans_v.as_ref().and_then(|b| b.days_since_last_ban),
            account_age_days: created_ts.map(|t| ((now - t) / 86_400).max(0)),
            rust_hours,
            profile_private: private,
            kills: rust_stats.as_ref().and_then(|s| s.kills),
            kdr: kdr_v,
            acc: acc_v,
            level,
        };
        out.insert("risk".into(), risk_json(&risk_score(&input)));
    } else {
        out.insert("risk".into(), Value::Null);
    }

    if !notes.is_empty() {
        out.insert("steamNote".into(), json!(notes.join("; ")));
    }
    Value::Object(out)
}

// ---------------------------------------------------------------- тесты

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn summaries_parse() {
        let v = json!({"response":{"players":[{"steamid":"76561190000000001","communityvisibilitystate":3,
            "personaname":"Bob","avatarfull":"http://a/f.jpg","timecreated":1500000000}]}});
        let m = parse_summaries(&v);
        let s = m.get("76561190000000001").unwrap();
        assert_eq!(s.personaname.as_deref(), Some("Bob"));
        assert_eq!(s.avatar.as_deref(), Some("http://a/f.jpg"));
        assert_eq!(s.timecreated, Some(1500000000));
        assert!(s.is_public());
        assert!(parse_summaries(&json!({"response":{"players":[]}})).is_empty());
        assert!(parse_summaries(&json!({})).is_empty());
    }

    #[test]
    fn private_summary_has_no_timecreated() {
        let v = json!({"response":{"players":[{"steamid":"1","communityvisibilitystate":1,"personaname":"x"}]}});
        let s = parse_summaries(&v).remove("1").unwrap();
        assert!(!s.is_public());
        assert_eq!(s.timecreated, None);
    }

    #[test]
    fn bans_parse() {
        let v = json!({"players":[{"SteamId":"1","CommunityBanned":false,"VACBanned":true,"NumberOfVACBans":2,
            "DaysSinceLastBan":100,"NumberOfGameBans":1,"EconomyBan":"none"}]});
        let b = parse_bans(&v).unwrap();
        assert!(b.vac_banned);
        assert_eq!((b.vac_bans, b.game_bans), (2, 1));
        assert_eq!(b.days_since_last_ban, Some(100));
        assert_eq!(b.economy_ban, None);
        assert!(parse_bans(&json!({"players":[]})).is_none());
    }

    #[test]
    fn level_and_hours() {
        assert_eq!(parse_level(&json!({"response":{"player_level":23}})), Some(23));
        assert_eq!(parse_level(&json!({"response":{}})), None);
        let g = json!({"response":{"game_count":2,"games":[{"appid":10,"playtime_forever":5},
            {"appid":252490,"playtime_forever":6030}]}});
        assert_eq!(parse_owned_hours(&g, 252490), Some(101));
        assert_eq!(parse_owned_hours(&g, 99), None);
        assert_eq!(parse_owned_hours(&json!({"response":{}}), 252490), None);
    }

    #[test]
    fn friends_parse() {
        let v = json!({"friendslist":{"friends":[{"steamid":"1","relationship":"friend"},{"steamid":"2"}]}});
        assert_eq!(parse_friend_ids(&v), Some(vec!["1".to_string(), "2".to_string()]));
        assert_eq!(parse_friend_ids(&json!({})), None);
    }

    #[test]
    fn stats_parse() {
        let v = json!({"playerstats":{"steamID":"1","gameName":"Rust","stats":[
            {"name":"kill_player","value":67},{"name":"deaths","value":121},
            {"name":"bullet_fired","value":6473},{"name":"headshot","value":20},
            {"name":"bullet_hit_player","value":1000}]}});
        let s = parse_rust_stats(&v).unwrap();
        assert_eq!(s.kills, Some(67));
        assert_eq!(s.deaths, Some(121));
        assert_eq!(s.fired, Some(6473));
        assert_eq!(s.headshots, Some(20));
        assert_eq!(s.hit, Some(1000));
        let partial = parse_rust_stats(&json!({"playerstats":{"stats":[{"name":"kill_player","value":3}]}})).unwrap();
        assert_eq!(partial.deaths, None);
        assert!(parse_rust_stats(&json!({"playerstats":{"error":"Profile is not public"}})).is_none());
    }

    #[test]
    fn friends_list_limits_and_fallback() {
        let ids: Vec<String> = (1..=9).map(|i| i.to_string()).collect();
        let mut m = HashMap::new();
        m.insert("1".to_string(), Summary { personaname: Some("Al".into()), avatar: Some("u".into()), ..Default::default() });
        let l = friends_list(&ids, &m);
        assert_eq!(l.len(), 6);
        assert_eq!(l[0], json!({"name":"Al","avatar":"u"}));
        assert_eq!(l[1], json!({"name":"2","avatar":null}));
    }

    #[test]
    fn math_helpers() {
        assert_eq!(kdr(67, 121), 0.55);
        assert_eq!(kdr(10, 0), 10.0);
        assert_eq!(accuracy(200, 50), Some(25.0));
        assert_eq!(accuracy(0, 0), None);
        assert_eq!(accuracy(10, 20), None);
    }

    #[test]
    fn date_format() {
        assert_eq!(format_date(0), "1970-01-01");
        assert_eq!(format_date(1_500_000_000), "2017-07-14");
        assert_eq!(format_date(951_782_400), "2000-02-29");
    }

    #[test]
    fn risk_clean_is_low() {
        let r = risk_score(&RiskInput {
            account_age_days: Some(2000),
            rust_hours: Some(1500),
            kills: Some(1000),
            kdr: Some(1.2),
            acc: Some(20.0),
            level: Some(30),
            ..Default::default()
        });
        assert_eq!(r.score, 0);
        assert_eq!(r.level, "low");
        assert!(r.reasons.is_empty());
    }

    #[test]
    fn risk_banned_new_account_is_high() {
        let r = risk_score(&RiskInput {
            vac_bans: 1,
            days_since_last_ban: Some(30),
            account_age_days: Some(5),
            rust_hours: Some(3),
            ..Default::default()
        });
        assert_eq!(r.score, 100); // 50+15+25+20 = 110 -> 100
        assert_eq!(r.level, "hi");
        assert!(r.reasons.len() >= 4);
    }

    #[test]
    fn risk_mid_private_low_hours() {
        let r = risk_score(&RiskInput {
            profile_private: true,
            rust_hours: Some(30),
            account_age_days: Some(100),
            ..Default::default()
        });
        assert_eq!(r.score, 30); // 10+10+10
        assert_eq!(r.level, "mid");
    }

    #[test]
    fn risk_odd_stats_need_enough_kills() {
        let few = risk_score(&RiskInput { kills: Some(10), kdr: Some(20.0), ..Default::default() });
        assert_eq!(few.score, 0);
        let many = risk_score(&RiskInput { kills: Some(100), kdr: Some(20.0), acc: Some(80.0), ..Default::default() });
        assert_eq!(many.score, 30);
        assert_eq!(many.level, "mid");
    }

    #[test]
    fn scrub_hides_key() {
        assert_eq!(scrub("error for url ?key=SECRET&x", "SECRET"), "error for url ?key=***&x");
        assert_eq!(scrub("abc", ""), "abc");
    }

    #[test]
    fn steam_id_check() {
        assert!(is_steam_id("76561198000000001"));
        assert!(!is_steam_id("123"));
        assert!(!is_steam_id("7656119800000000x"));
    }

    #[test]
    fn note_added() {
        let v = with_note(json!({"a":1}), "нет ключа");
        assert_eq!(v["steamNote"], "нет ключа");
        assert_eq!(v["a"], 1);
    }
}
