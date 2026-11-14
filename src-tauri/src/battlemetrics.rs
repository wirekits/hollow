//! Интеграция с BattleMetrics REST API (вкладка «Трекер»).
//! Токен хранится только в памяти и никогда не логируется и не эмитится.

use std::sync::Mutex;
use std::time::Duration;

use serde_json::{json, Value};
use tauri::{AppHandle, Emitter, Manager};

const API: &str = "https://api.battlemetrics.com";
const POLL_SECS: u64 = 60;
const MAX_BACKOFF_SECS: u64 = 600;

#[derive(Clone, Debug)]
struct Target {
    /// Ровно та строка, которую ввёл пользователь (по ней фронт сопоставляет цель).
    n: String,
    /// BattleMetrics player id.
    bm_id: String,
}

#[derive(Default)]
struct Inner {
    token: Option<String>,
    targets: Vec<Target>,
    task: Option<tauri::async_runtime::JoinHandle<()>>,
}

#[derive(Default)]
pub struct BmState {
    inner: Mutex<Inner>,
}

// ---------------------------------------------------------------- tauri API

#[tauri::command]
pub async fn set_bm_token(app: AppHandle, token: Option<String>) -> Result<(), String> {
    configure(&app, token).await;
    Ok(())
}

#[tauri::command]
pub async fn lookup_player(app: AppHandle, query: String) -> Result<(), String> {
    let query = query.trim().to_string();
    if query.is_empty() {
        return Err("Пустой запрос".into());
    }
    let token = get_token(&app)?;
    let id = resolve_player(&token, &query).await?;
    let mut profile = fetch_profile(&token, &id, &query).await?;
    let sid = profile.get("steamId").and_then(|v| v.as_str()).map(String::from);
    if let Some(sid) = sid {
        profile = crate::steam::enrich(&app, &sid, profile).await;
    }
    let _ = app.emit("player_profile", profile);
    Ok(())
}

#[tauri::command]
pub async fn track_add(app: AppHandle, query: String) -> Result<(), String> {
    let query = query.trim().to_string();
    if query.is_empty() {
        return Err("Пустой запрос".into());
    }
    let token = get_token(&app)?;
    let id = resolve_player(&token, &query).await?;
    {
        let st = app.state::<BmState>();
        let mut g = st.inner.lock().unwrap();
        if !g.targets.iter().any(|t| t.n.eq_ignore_ascii_case(&query)) {
            g.targets.push(Target { n: query, bm_id: id });
        }
    }
    ensure_task(&app);
    // сразу один опрос, чтобы фронт увидел статус без ожидания минуты
    let app2 = app.clone();
    tauri::async_runtime::spawn(async move {
        let _ = poll_once(&app2).await;
    });
    Ok(())
}

#[tauri::command]
pub async fn track_remove(app: AppHandle, query: String) -> Result<(), String> {
    let query = query.trim().to_string();
    let st = app.state::<BmState>();
    let mut g = st.inner.lock().unwrap();
    g.targets.retain(|t| !t.n.eq_ignore_ascii_case(&query));
    if g.targets.is_empty() {
        if let Some(h) = g.task.take() {
            h.abort();
        }
    }
    Ok(())
}

pub async fn reset(app: &AppHandle) {
    let st = app.state::<BmState>();
    let mut g = st.inner.lock().unwrap();
    if let Some(h) = g.task.take() {
        h.abort();
    }
    g.targets.clear();
}

pub async fn configure(app: &AppHandle, token: Option<String>) {
    let token = token.map(|t| t.trim().to_string()).filter(|t| !t.is_empty());
    let st = app.state::<BmState>();
    let mut g = st.inner.lock().unwrap();
    let off = token.is_none();
    g.token = token;
    if off {
        if let Some(h) = g.task.take() {
            h.abort();
        }
    }
}

fn get_token(app: &AppHandle) -> Result<String, String> {
    app.state::<BmState>()
        .inner
        .lock()
        .unwrap()
        .token
        .clone()
        .ok_or_else(|| "Токен BattleMetrics не задан (Настройки, профиль)".to_string())
}

fn ensure_task(app: &AppHandle) {
    let st = app.state::<BmState>();
    let mut g = st.inner.lock().unwrap();
    if g.task.as_ref().map_or(false, |h| !h.inner().is_finished()) {
        return;
    }
    let app2 = app.clone();
    g.task = Some(tauri::async_runtime::spawn(async move {
        let mut backoff = 0u64;
        loop {
            let wait = POLL_SECS.max(backoff);
            tokio::time::sleep(Duration::from_secs(wait)).await;
            match poll_once(&app2).await {
                Ok(()) => backoff = 0,
                Err(PollErr::RateLimited(ra)) => {
                    backoff = ra
                        .unwrap_or_else(|| (backoff.max(POLL_SECS)) * 2)
                        .clamp(POLL_SECS, MAX_BACKOFF_SECS);
                }
                Err(PollErr::Other) => {}
            }
        }
    }));
}

enum PollErr {
    RateLimited(Option<u64>),
    Other,
}

async fn poll_once(app: &AppHandle) -> Result<(), PollErr> {
    let (token, targets) = {
        let st = app.state::<BmState>();
        let g = st.inner.lock().unwrap();
        (g.token.clone(), g.targets.clone())
    };
    let Some(token) = token else { return Ok(()) };
    if targets.is_empty() {
        return Ok(());
    }
    let mut out = Vec::new();
    let mut rate_limited: Option<Option<u64>> = None;
    for t in &targets {
        let url = format!(
            "{API}/players/{}/relationships/sessions?include=server&page%5Bsize%5D=10",
            t.bm_id
        );
        match http_get(&token, &url, &[]).await {
            Ok(v) => {
                let (on, srv, since) = target_status(&v);
                out.push(json!({"n": t.n, "on": on, "srv": srv, "since": since}));
            }
            Err(HttpErr::RateLimited(ra)) => {
                rate_limited = Some(ra);
                break;
            }
            Err(HttpErr::Other(_)) => {}
        }
        tokio::time::sleep(Duration::from_millis(400)).await;
    }
    if !out.is_empty() {
        let _ = app.emit("targets", out);
    }
    match rate_limited {
        Some(ra) => Err(PollErr::RateLimited(ra)),
        None => Ok(()),
    }
}

// ---------------------------------------------------------------- HTTP

enum HttpErr {
    RateLimited(Option<u64>),
    Other(String),
}

fn client() -> Result<reqwest::Client, String> {
    reqwest::Client::builder()
        .timeout(Duration::from_secs(20))
        .user_agent("hollow-plus")
        .build()
        .map_err(|_| "Не удалось создать HTTP-клиент".to_string())
}

async fn send(token: &str, req: reqwest::RequestBuilder) -> Result<Value, HttpErr> {
    let resp = req
        .header("Authorization", format!("Bearer {token}"))
        .header("Accept", "application/json")
        .send()
        .await
        .map_err(|_| HttpErr::Other("Нет связи с BattleMetrics".into()))?;
    let status = resp.status();
    if status.as_u16() == 429 {
        let ra = resp
            .headers()
            .get("retry-after")
            .and_then(|v| v.to_str().ok())
            .and_then(|s| s.trim().parse::<u64>().ok());
        return Err(HttpErr::RateLimited(ra));
    }
    if status.as_u16() == 401 || status.as_u16() == 403 {
        return Err(HttpErr::Other("BattleMetrics отклонил токен".into()));
    }
    if !status.is_success() {
        return Err(HttpErr::Other(format!("BattleMetrics вернул HTTP {}", status.as_u16())));
    }
    resp.json::<Value>()
        .await
        .map_err(|_| HttpErr::Other("Некорректный ответ BattleMetrics".into()))
}

async fn http_get(token: &str, url: &str, query: &[(&str, &str)]) -> Result<Value, HttpErr> {
    let c = client().map_err(HttpErr::Other)?;
    send(token, c.get(url).query(query)).await
}

async fn http_post(token: &str, url: &str, body: &Value) -> Result<Value, HttpErr> {
    let c = client().map_err(HttpErr::Other)?;
    send(token, c.post(url).json(body)).await
}

fn human(e: HttpErr) -> String {
    match e {
        HttpErr::RateLimited(_) => "Лимит запросов BattleMetrics, повторите позже".into(),
        HttpErr::Other(s) => s,
    }
}

// ---------------------------------------------------------------- сценарии

fn is_steam_id(s: &str) -> bool {
    s.len() == 17 && s.starts_with("7656119") && s.bytes().all(|b| b.is_ascii_digit())
}

async fn resolve_player(token: &str, query: &str) -> Result<String, String> {
    if is_steam_id(query) {
        let body = json!({"data": [{"type": "identifier",
            "attributes": {"type": "steamID", "identifier": query}}]});
        if let Ok(v) = http_post(token, &format!("{API}/players/match"), &body).await {
            if let Some(id) = parse_match(&v, query) {
                return Ok(id);
            }
        }
    }
    let v = http_get(
        token,
        &format!("{API}/players"),
        &[("filter[search]", query), ("page[size]", "10")],
    )
    .await
    .map_err(human)?;
    parse_player_search(&v, query).ok_or_else(|| "Игрок не найден в BattleMetrics".to_string())
}

async fn fetch_profile(token: &str, id: &str, query: &str) -> Result<Value, String> {
    let pv = http_get(
        token,
        &format!("{API}/players/{id}"),
        &[("include", "identifier,server")],
    )
    .await
    .map_err(human)?;
    // баны и сессии необязательны: их сбой не должен ронять карточку
    let bans = http_get(token, &format!("{API}/bans"), &[("filter[player]", id), ("page[size]", "10")])
        .await
        .ok()
        .map(|v| parse_bans(&v))
        .unwrap_or_default();
    let sessions = http_get(
        token,
        &format!("{API}/players/{id}/relationships/sessions"),
        &[("include", "server"), ("page[size]", "5")],
    )
    .await
    .ok()
    .map(|v| session_lines(&v))
    .unwrap_or_default();
    Ok(build_profile(&pv, query, bans, sessions))
}

// ---------------------------------------------------------------- чистые разборщики

fn str_at<'a>(v: &'a Value, path: &[&str]) -> Option<&'a str> {
    let mut cur = v;
    for p in path {
        cur = cur.get(*p)?;
    }
    cur.as_str()
}

/// POST /players/match: data[] с attributes.identifier и relationships.player.data.id.
fn parse_match(v: &Value, steam_id: &str) -> Option<String> {
    let arr = v.get("data")?.as_array()?;
    let pick = |it: &Value| str_at(it, &["relationships", "player", "data", "id"]).map(String::from);
    arr.iter()
        .find(|it| str_at(it, &["attributes", "identifier"]) == Some(steam_id))
        .and_then(pick)
        .or_else(|| arr.first().and_then(pick))
}

/// GET /players?filter[search]=: data[] типа player, attributes.name. Точное имя приоритетнее.
fn parse_player_search(v: &Value, query: &str) -> Option<String> {
    let arr = v.get("data")?.as_array()?;
    let id_of = |it: &Value| str_at(it, &["id"]).map(String::from);
    arr.iter()
        .find(|it| {
            str_at(it, &["attributes", "name"]).map_or(false, |n| n.eq_ignore_ascii_case(query))
        })
        .and_then(id_of)
        .or_else(|| arr.first().and_then(id_of))
}

fn included<'a>(v: &'a Value, ty: &'a str) -> impl Iterator<Item = &'a Value> + 'a {
    v.get("included")
        .and_then(|i| i.as_array())
        .into_iter()
        .flatten()
        .filter(move |it| str_at(it, &["type"]) == Some(ty))
}

/// GET /bans: attributes.reason (+ expires).
fn parse_bans(v: &Value) -> Vec<String> {
    v.get("data")
        .and_then(|d| d.as_array())
        .into_iter()
        .flatten()
        .filter_map(|b| {
            let reason = str_at(b, &["attributes", "reason"]).unwrap_or("").trim();
            let reason = if reason.is_empty() { "Бан" } else { reason };
            Some(match str_at(b, &["attributes", "expires"]) {
                Some(e) if parse_iso_ms(e).is_some() => format!("{reason} (до {})", fmt_date(parse_iso_ms(e)?)),
                _ => reason.to_string(),
            })
        })
        .collect()
}

/// Профиль в форме, которую ждёт фронт. Чего BM не отдаёт, то null.
fn build_profile(pv: &Value, query: &str, bans: Vec<String>, sessions: Vec<String>) -> Value {
    let cur_name = str_at(pv, &["data", "attributes", "name"]).unwrap_or(query);
    let mut steam_id: Option<String> = None;
    let mut aliases: Vec<String> = Vec::new();
    for it in included(pv, "identifier") {
        let ty = str_at(it, &["attributes", "type"]).unwrap_or("");
        let val = str_at(it, &["attributes", "identifier"]).unwrap_or("");
        if val.is_empty() {
            continue;
        }
        match ty {
            "steamID" => {
                if steam_id.is_none() {
                    steam_id = Some(val.to_string());
                }
            }
            "name" => {
                if !val.eq_ignore_ascii_case(cur_name) && !aliases.iter().any(|a| a == val) {
                    aliases.push(val.to_string());
                }
            }
            _ => {}
        }
    }
    if steam_id.is_none() && is_steam_id(query) {
        steam_id = Some(query.to_string());
    }
    // часы: сумма meta.timePlayed (секунды) по серверам, которые отслеживает BM
    let secs: f64 = included(pv, "server")
        .filter_map(|s| s.get("meta").and_then(|m| m.get("timePlayed")).and_then(|t| t.as_f64()))
        .sum();
    let has_time = included(pv, "server").any(|s| s.get("meta").and_then(|m| m.get("timePlayed")).is_some());
    let hours = if has_time { json!((secs / 3600.0).round() as i64) } else { Value::Null };
    json!({
        // name = то, что ввёл пользователь: фронт сверяет его с запросом
        "name": query,
        "nick": cur_name,
        "steamId": steam_id,
        "kdr": null,
        "acc": null,
        "hours": hours,
        "lvl": null,
        "aliases": aliases,
        "friends": [],
        "bans": bans,
        "sessions": sessions,
    })
}

struct Session {
    start: i64,
    stop: Option<i64>,
    server: String,
}

fn parse_sessions(v: &Value) -> Vec<Session> {
    let servers: std::collections::HashMap<&str, &str> = included(v, "server")
        .filter_map(|s| Some((str_at(s, &["id"])?, str_at(s, &["attributes", "name"])?)))
        .collect();
    let mut out: Vec<Session> = v
        .get("data")
        .and_then(|d| d.as_array())
        .into_iter()
        .flatten()
        .filter_map(|s| {
            let start = parse_iso_ms(str_at(s, &["attributes", "start"])?)?;
            let stop = str_at(s, &["attributes", "stop"]).and_then(parse_iso_ms);
            let sid = str_at(s, &["relationships", "server", "data", "id"]).unwrap_or("");
            let server = servers.get(sid).copied().unwrap_or("").to_string();
            Some(Session { start, stop, server })
        })
        .collect();
    out.sort_by(|a, b| b.start.cmp(&a.start));
    out
}

fn session_lines(v: &Value) -> Vec<String> {
    parse_sessions(v)
        .iter()
        .map(|s| {
            let srv = if s.server.is_empty() { "?" } else { &s.server };
            match s.stop {
                Some(e) => format!("{srv} · {} · {} мин", fmt_date(s.start), ((e - s.start).max(0) / 60000)),
                None => format!("{srv} · {} · онлайн", fmt_date(s.start)),
            }
        })
        .collect()
}

/// (on, srv, since_ms). since = 0, если неизвестно (фронт подставит текущее время).
fn target_status(v: &Value) -> (bool, String, i64) {
    match parse_sessions(v).first() {
        Some(s) if s.stop.is_none() => (true, s.server.clone(), s.start),
        Some(s) => (false, s.server.clone(), s.stop.unwrap_or(s.start)),
        None => (false, String::new(), 0),
    }
}

// ---------------------------------------------------------------- время (без chrono)

fn days_from_civil(y: i64, m: i64, d: i64) -> i64 {
    let y = if m <= 2 { y - 1 } else { y };
    let era = if y >= 0 { y } else { y - 399 } / 400;
    let yoe = y - era * 400;
    let doy = (153 * (if m > 2 { m - 3 } else { m + 9 }) + 2) / 5 + d - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146097 + doe - 719468
}

fn civil_from_days(z: i64) -> (i64, i64, i64) {
    let z = z + 719468;
    let era = if z >= 0 { z } else { z - 146096 } / 146097;
    let doe = z - era * 146097;
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    (yoe + era * 400 + if m <= 2 { 1 } else { 0 }, m, d)
}

/// ISO 8601 UTC вида 2024-05-01T12:34:56.789Z -> миллисекунды от эпохи.
fn parse_iso_ms(s: &str) -> Option<i64> {
    let b = s.as_bytes();
    if b.len() < 19 || b[4] != b'-' || b[7] != b'-' || (b[10] != b'T' && b[10] != b' ') {
        return None;
    }
    let n = |r: std::ops::Range<usize>| s.get(r)?.parse::<i64>().ok();
    let (y, mo, d) = (n(0..4)?, n(5..7)?, n(8..10)?);
    let (h, mi, se) = (n(11..13)?, n(14..16)?, n(17..19)?);
    let mut ms = 0;
    if b.len() > 20 && b[19] == b'.' {
        let frac: String = s[20..].chars().take_while(|c| c.is_ascii_digit()).take(3).collect();
        ms = format!("{frac:0<3}").parse::<i64>().ok()?;
    }
    Some(((days_from_civil(y, mo, d) * 24 + h) * 60 + mi) * 60_000 + se * 1000 + ms)
}

fn fmt_date(ms: i64) -> String {
    let secs = ms.div_euclid(1000);
    let (y, m, d) = civil_from_days(secs.div_euclid(86400));
    let rem = secs.rem_euclid(86400);
    format!("{y:04}-{m:02}-{d:02} {:02}:{:02}", rem / 3600, rem % 3600 / 60)
}

// ---------------------------------------------------------------- тесты

#[cfg(test)]
mod tests {
    use super::*;

    // Примеры составлены по формату JSON:API, который использует BattleMetrics.
    const SEARCH: &str = r#"{"data":[
      {"type":"player","id":"111","attributes":{"name":"Other"}},
      {"type":"player","id":"222","attributes":{"name":"Hollow"}}]}"#;

    const MATCH: &str = r#"{"data":[{"type":"identifier","id":"9",
      "attributes":{"type":"steamID","identifier":"76561190000000001"},
      "relationships":{"player":{"data":{"type":"player","id":"333"}}}}]}"#;

    const PLAYER: &str = r#"{"data":{"type":"player","id":"222","attributes":{"name":"Hollow"}},
      "included":[
       {"type":"identifier","id":"1","attributes":{"type":"steamID","identifier":"76561190000000001"}},
       {"type":"identifier","id":"2","attributes":{"type":"name","identifier":"Hollow"}},
       {"type":"identifier","id":"3","attributes":{"type":"name","identifier":"OldNick"}},
       {"type":"server","id":"10","attributes":{"name":"S1"},"meta":{"timePlayed":7200}},
       {"type":"server","id":"11","attributes":{"name":"S2"},"meta":{"timePlayed":3600}}]}"#;

    const SESSIONS: &str = r#"{"data":[
      {"type":"session","id":"a","attributes":{"start":"2024-05-01T10:00:00.000Z","stop":"2024-05-01T11:00:00.000Z"},
       "relationships":{"server":{"data":{"type":"server","id":"10"}}}},
      {"type":"session","id":"b","attributes":{"start":"2024-05-02T10:00:00.000Z","stop":null},
       "relationships":{"server":{"data":{"type":"server","id":"11"}}}}],
      "included":[{"type":"server","id":"10","attributes":{"name":"S1"}},
                  {"type":"server","id":"11","attributes":{"name":"S2"}}]}"#;

    const BANS: &str = r#"{"data":[{"type":"ban","id":"1","attributes":{"reason":"cheating","expires":null}},
      {"type":"ban","id":"2","attributes":{"reason":"toxic","expires":"2030-01-02T03:04:05.000Z"}}]}"#;

    fn j(s: &str) -> Value {
        serde_json::from_str(s).unwrap()
    }

    #[test]
    fn steam_id_check() {
        assert!(is_steam_id("76561190000000001"));
        assert!(!is_steam_id("7656119000000001"));
        assert!(!is_steam_id("Hollow"));
    }

    #[test]
    fn search_prefers_exact_name() {
        assert_eq!(parse_player_search(&j(SEARCH), "hollow").as_deref(), Some("222"));
        assert_eq!(parse_player_search(&j(SEARCH), "zzz").as_deref(), Some("111"));
        assert_eq!(parse_player_search(&j(r#"{"data":[]}"#), "x"), None);
    }

    #[test]
    fn match_gives_player_id() {
        assert_eq!(parse_match(&j(MATCH), "76561190000000001").as_deref(), Some("333"));
    }

    #[test]
    fn profile_shape() {
        let p = build_profile(&j(PLAYER), "Hollow", vec![], vec!["x".into()]);
        assert_eq!(p["name"], "Hollow");
        assert_eq!(p["steamId"], "76561190000000001");
        assert_eq!(p["hours"], 3);
        assert_eq!(p["aliases"], json!(["OldNick"]));
        assert!(p["kdr"].is_null() && p["acc"].is_null() && p["lvl"].is_null());
    }

    #[test]
    fn profile_without_server_time_has_null_hours() {
        let p = build_profile(&j(r#"{"data":{"attributes":{"name":"A"}}}"#), "A", vec![], vec![]);
        assert!(p["hours"].is_null());
    }

    #[test]
    fn sessions_and_status() {
        let v = j(SESSIONS);
        let lines = session_lines(&v);
        assert_eq!(lines[0], "S2 · 2024-05-02 10:00 · онлайн");
        assert_eq!(lines[1], "S1 · 2024-05-01 10:00 · 60 мин");
        let (on, srv, since) = target_status(&v);
        assert!(on);
        assert_eq!(srv, "S2");
        assert_eq!(since, parse_iso_ms("2024-05-02T10:00:00.000Z").unwrap());
    }

    #[test]
    fn offline_status_uses_last_stop() {
        let v = j(r#"{"data":[{"attributes":{"start":"2024-05-01T10:00:00Z","stop":"2024-05-01T11:00:00Z"},
          "relationships":{"server":{"data":{"id":"10"}}}}],"included":[{"type":"server","id":"10","attributes":{"name":"S1"}}]}"#);
        let (on, srv, since) = target_status(&v);
        assert!(!on);
        assert_eq!(srv, "S1");
        assert_eq!(since, parse_iso_ms("2024-05-01T11:00:00Z").unwrap());
        assert_eq!(target_status(&j(r#"{"data":[]}"#)), (false, String::new(), 0));
    }

    #[test]
    fn bans_parse() {
        let b = parse_bans(&j(BANS));
        assert_eq!(b[0], "cheating");
        assert_eq!(b[1], "toxic (до 2030-01-02 03:04)");
    }

    #[test]
    fn time_roundtrip() {
        assert_eq!(parse_iso_ms("1970-01-01T00:00:01.500Z"), Some(1500));
        assert_eq!(parse_iso_ms("2000-03-01T00:00:00Z"), Some(951868800000));
        assert_eq!(fmt_date(951868800000), "2000-03-01 00:00");
        assert_eq!(parse_iso_ms("garbage"), None);
    }
}
