//! Вкладка «Трекер»: бесплатная замена BattleMetrics (платный API).
//!
//! Два канала онлайна, цель «в сети», если сработал ЛЮБОЙ:
//!  1. Steam Web API (ключ пользователя из active_profile().steam_key): GetPlayerSummaries,
//!     «в Rust» = gameid == "252490" (нужен открытый профиль);
//!  2. A2S-опрос игровых серверов (crate::a2s, без ключей): ник цели совпадает с именем игрока
//!     на наблюдаемом сервере (сервер пачки по умолчанию, `watch_servers` пачки и серверы,
//!     которые Steam показал в gameserverip у открытых профилей). Работает и для закрытых профилей,
//!     но совпадение по нику, возможен тёзка.
//! Основной путь пользователя: вставил SteamID64/ссылку, ники (personaname + прошлые ники из
//! ajaxaliases) собираются автоматически. Цель «только по нику» необязательное дополнение.
//! Имя модуля осталось прежним ради lib.rs.
//!
//! Подтверждено по документации (partner.steamgames.com и wiki.teamfortress.com/wiki/WebAPI):
//!  * GetPlayerSummaries/v2: `steamids` до 100 через запятую; `gameid` строкой и только если
//!    игрок в игре; `gameserverip` отсутствует или "0.0.0.0:0", если сервера нет;
//!    `communityvisibilitystate`: 1 приватный, 2 только друзья, 3 «друзья друзей», при этом
//!    wiki пишет, что 3 сейчас выставляется и публичным профилям (5 тоже считаем публичным).
//!  * ResolveVanityURL: параметр `vanityurl`, ответ `response.success` (1 успех, 42 нет совпадения)
//!    и `response.steamid`. Официальная страница partner.steamgames.com ответ не описывает,
//!    поля взяты с wiki.teamfortress.com.
//! НЕ подтверждено документацией: `gameextrainfo` не используем; сопоставление `gameserverip`
//! с сервером пачки эвристическое (порт Rust+ обычно игровой порт + 67), проверить на живом профиле.
//! Подтверждено реальным запросом: ajaxaliases отдаёт JSON-массив
//! `[{"newname":"Robin","timechanged":"7 May, 2019 @ 11:04pm"}, ...]`, свежие первыми,
//! для скрытых/пустых историй `[]`. Дата локализована и без часового пояса (читаем как UTC).
//!
//! Ключ Steam не логируется и не попадает в тексты ошибок.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::Duration;

use futures_util::StreamExt;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use tauri::{AppHandle, Emitter, Listener, Manager};

use crate::a2s::A2sPlayer;

const BASE: &str = "https://api.steampowered.com";
const ALIASES_URL: &str = "https://steamcommunity.com/profiles";
const RUST_APPID: &str = "252490";
const POLL_SECS: u64 = 45;
const MAX_BACKOFF_SECS: u64 = 600;
const BATCH: usize = 100;
const MAX_SESSIONS: usize = 5000;
const TIMEOUT: Duration = Duration::from_secs(10);
/// Таймаут одного A2S-запроса и число серверов, опрашиваемых параллельно.
const A2S_TIMEOUT: Duration = Duration::from_secs(4);
const RESOLVE_TIMEOUT: Duration = Duration::from_secs(6);
const PROBE_OK_MS: i64 = 3_600_000;
const A2S_PAR: usize = 6;
const MAX_NAMES: usize = 30;
const MAX_EPHEMERAL: usize = 8;
const PROBE_RETRY_MS: i64 = 300_000;
const NOT_FOUND: &str = "ник не найден: укажи SteamID или ссылку на профиль";
const NO_KEY: &str = "нужен Steam API-ключ (Настройки)";

const MS_HOUR: i64 = 3_600_000;
const MS_DAY: i64 = 86_400_000;
/// Защита от аномальных сессий.
const MAX_SESSION_MS: i64 = 30 * MS_DAY;

// ---------------------------------------------------------------- состояние

/// Цель слежения. `label` = ровно то, что ввёл пользователь (по нему фронт сопоставляет цель).
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
struct TargetRec {
    label: String,
    /// None: цель «только по нику» (без Steam-канала).
    #[serde(default, rename = "steamId")]
    steam_id: Option<String>,
    /// Ники для матчинга на игровых серверах (personaname, прошлые ники, введённая строка).
    #[serde(default)]
    names: Vec<String>,
    /// Когда начали отслеживать (мс).
    #[serde(default, rename = "trackedSince")]
    tracked_since: i64,
}

impl TargetRec {
    /// Ключ целей в сессиях/заметках: SteamID или "n:<ник>".
    fn key(&self) -> String {
        match &self.steam_id {
            Some(id) => id.clone(),
            None => format!("n:{}", norm_name(&self.label)),
        }
    }
}

/// Локальная сессия игры в Rust. `stop = None`: открыта.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
struct Session {
    start: i64,
    stop: Option<i64>,
    #[serde(default)]
    srv: String,
}

#[derive(Default, Serialize, Deserialize)]
struct SessionsFile {
    /// Время последнего успешного опроса (мс): по нему закрываем сессии после простоя приложения.
    #[serde(default, rename = "lastPoll")]
    last_poll: i64,
    #[serde(default)]
    sessions: HashMap<String, Vec<Session>>,
}

#[derive(Default)]
struct Inner {
    profile_id: Option<String>,
    /// Растёт при reset/configure: опрос, начатый до смены пачки, результат не применяет.
    generation: u64,
    targets: Vec<TargetRec>,
    sessions: HashMap<String, Vec<Session>>,
    notes: HashMap<String, String>,
    /// Откуда видим онлайн: "steam" | "server" | "both" (только для целей «в сети»).
    src: HashMap<String, &'static str>,
    /// Кэш определения query-адреса A2S: "ключ" -> (адрес, когда проверяли, мс).
    probe: HashMap<String, (Option<SocketAddr>, i64)>,
    last_poll: i64,
    server_name: Option<String>,
    listening: bool,
    task: Option<tauri::async_runtime::JoinHandle<()>>,
}

#[derive(Default)]
pub struct BmState {
    inner: Mutex<Inner>,
}

// ---------------------------------------------------------------- tauri API

/// Ключ BattleMetrics больше не нужен: безобидный no-op (UI мог ещё вызывать).
#[tauri::command]
pub async fn set_bm_token(_app: AppHandle, _token: Option<String>) -> Result<(), String> {
    Ok(())
}

#[tauri::command]
pub async fn lookup_player(app: AppHandle, query: String) -> Result<(), String> {
    let query = query.trim().to_string();
    if query.is_empty() {
        return Err("Пустой запрос".into());
    }
    let key = active_key(&app).ok_or_else(|| NO_KEY.to_string())?;
    let id = resolve_query(&key, &query).await?;
    let sums = fetch_summaries(&key, &[id.clone()]).await.map_err(human)?;
    let player = sums.get(&id).ok_or_else(|| "профиль Steam не найден".to_string())?;
    let nick = player.get("personaname").and_then(|v| v.as_str()).unwrap_or(&query).to_string();
    let presence = parse_presence(player);

    let base = json!({
        // name = то, что ввёл пользователь: фронт сверяет его с запросом
        "name": query,
        "nick": nick,
        "steamId": id,
        "kdr": null, "acc": null, "hours": null, "lvl": null,
        "aliases": [], "friends": [], "bans": [], "sessions": [],
    });
    let mut profile = crate::steam::enrich(&app, &id, base).await;

    let now = now_ms();
    let alias_list = fetch_aliases(&id).await.unwrap_or_default();
    let (names, detailed) = aliases_from(&alias_list, &nick, now);

    let pack = pack_server(&app);
    let (sessions, rec, note, src, server_name) = {
        let st = app.state::<BmState>();
        let g = st.inner.lock().unwrap();
        let rec = g.targets.iter().find(|t| t.steam_id.as_deref() == Some(id.as_str())).cloned();
        (
            g.sessions.get(&id).cloned().unwrap_or_default(),
            rec,
            g.notes.get(&id).cloned(),
            g.src.get(&id).copied(),
            g.server_name.clone(),
        )
    };
    let srv = label_server(&presence.srv, pack.as_ref().map(|(i, p)| (i.as_str(), *p)), server_name.as_deref());
    let online = presence.in_rust || src.is_some();
    if let Some(o) = profile.as_object_mut() {
        o.insert("aliases".into(), json!(names));
        o.insert("aliasesDetailed".into(), json!(detailed));
        o.insert("sessions".into(), json!(session_lines(&sessions)));
        o.insert("activity".into(), build_activity(&sessions, now).unwrap_or(Value::Null));
        o.insert("onlineNow".into(), json!(online));
        let open = sessions.last().filter(|s| s.stop.is_none());
        let servers = if online {
            let name = open.map(|s| s.srv.clone()).filter(|s| !s.is_empty()).unwrap_or(srv);
            json!([{"name": if name.is_empty() { "Rust".to_string() } else { name },
                    "online": true, "since": open.map_or(now, |s| s.start)}])
        } else {
            json!([])
        };
        o.insert("servers".into(), servers);
        o.insert("tracked".into(), json!(rec.is_some()));
        o.insert("trackedSince".into(), json!(rec.as_ref().map(|t| t.tracked_since)));
        o.insert("trackSrc".into(), json!(src));
        // для отслеживаемой цели заметка считается опросом (учтены серверы), иначе только по профилю
        let note = note.or_else(|| {
            let ni = NoteIn {
                has_steam: true,
                public: Some(presence.public),
                tracked_since: now,
                has_sessions: !sessions.is_empty(),
                now,
                servers_total: 0,
                servers_ok: 0,
                src: None,
            };
            if rec.is_none() && !presence.public { track_note(&ni) } else { None }
        });
        o.insert("trackNote".into(), note.map_or(Value::Null, |n| json!(n)));
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
    let key = active_key(&app);
    let is_link = query.to_ascii_lowercase().contains("steamcommunity.com/");
    let mut typed_is_nick = !is_link;
    // Steam не умеет искать по нику: если не разрешилось, цель только по нику (не ошибка).
    let steam_id: Option<String> = match parse_query(&query) {
        Ok(Query::Id(id)) => {
            typed_is_nick = false;
            if key.is_none() {
                return Err(NO_KEY.into());
            }
            Some(id)
        }
        Ok(Query::Vanity(v)) => match &key {
            Some(k) => match resolve_vanity(k, &v).await? {
                Some(id) => Some(id),
                None if is_link => return Err(NOT_FOUND.into()),
                None => None,
            },
            None if is_link => return Err(NO_KEY.into()),
            None => None,
        },
        Err(_) if is_link => return Err(NOT_FOUND.into()),
        Err(_) => None,
    };
    let mut names: Vec<String> = Vec::new();
    if typed_is_nick {
        merge_names(&mut names, [query.clone()]);
    }
    if let (Some(id), Some(k)) = (&steam_id, &key) {
        // personaname публичен и у закрытых профилей; сбой сети не фатален (опрос доберёт)
        if let Ok(sums) = fetch_summaries(k, &[id.clone()]).await {
            if let Some(n) = sums.get(id).and_then(|p| p.get("personaname")).and_then(|v| v.as_str()) {
                merge_names(&mut names, [n.to_string()]);
            }
        }
        if let Some(list) = fetch_aliases(id).await {
            merge_names(&mut names, list.into_iter().map(|(n, _)| n));
        }
    }
    let pid = active_profile_id(&app).ok_or("нет активной пачки")?;
    {
        let st = app.state::<BmState>();
        let mut g = st.inner.lock().unwrap();
        if g.profile_id.as_deref() != Some(pid.as_str()) {
            g.profile_id = Some(pid);
        }
        let dup = g
            .targets
            .iter()
            .any(|t| t.label.eq_ignore_ascii_case(&query) || (steam_id.is_some() && t.steam_id == steam_id));
        if !dup {
            g.targets.push(TargetRec { label: query, steam_id, names, tracked_since: now_ms() });
        }
    }
    save_targets(&app);
    emit_targets(&app);
    ensure_task(&app);
    // сразу один опрос, чтобы фронт увидел статус без ожидания
    let app2 = app.clone();
    tauri::async_runtime::spawn(async move {
        let _ = poll_once(&app2).await;
    });
    Ok(())
}

#[tauri::command]
pub async fn track_remove(app: AppHandle, query: String) -> Result<(), String> {
    let query = query.trim().to_string();
    {
        let st = app.state::<BmState>();
        let mut g = st.inner.lock().unwrap();
        // фронт удаляет по SteamID, если он известен, а label — то, что вводили (ссылка/ник)
        g.targets.retain(|t| !t.label.eq_ignore_ascii_case(&query) && t.steam_id.as_deref() != Some(query.as_str()));
        if g.targets.is_empty() {
            if let Some(h) = g.task.take() {
                h.abort();
            }
        }
    }
    save_targets(&app);
    emit_targets(&app);
    Ok(())
}

/// Смена пачки: остановить слежение, очистить цели в памяти (файлы пачки остаются на диске).
pub async fn reset(app: &AppHandle) {
    let st = app.state::<BmState>();
    let mut g = st.inner.lock().unwrap();
    if let Some(h) = g.task.take() {
        h.abort();
    }
    g.generation += 1;
    g.profile_id = None;
    g.targets.clear();
    g.sessions.clear();
    g.notes.clear();
    g.src.clear();
    g.probe.clear();
    g.last_poll = 0;
    g.server_name = None;
}

/// Вызывается при смене пачки/запуске. Параметр (бывший токен BM) игнорируется:
/// ключ Steam берётся из активной пачки. Восстанавливает цели и сессии с диска.
pub async fn configure(app: &AppHandle, _token: Option<String>) {
    let Some(pid) = active_profile_id(app) else { return };
    let dir = tracker_dir(app);
    let (targets, file) = match &dir {
        Some(d) => (
            read_json::<Vec<TargetRec>>(&d.join(format!("{}.json", safe_id(&pid)))).unwrap_or_default(),
            read_json::<SessionsFile>(&d.join(format!("{}-sessions.json", safe_id(&pid)))).unwrap_or_default(),
        ),
        None => (Vec::new(), SessionsFile::default()),
    };
    let need_listen;
    let has_targets;
    {
        let st = app.state::<BmState>();
        let mut g = st.inner.lock().unwrap();
        if let Some(h) = g.task.take() {
            h.abort();
        }
        g.generation += 1;
        g.profile_id = Some(pid);
        g.targets = targets;
        g.sessions = file.sessions;
        g.last_poll = file.last_poll;
        g.notes.clear();
        g.src.clear();
        g.probe.clear();
        has_targets = !g.targets.is_empty();
        need_listen = !g.listening;
        g.listening = true;
    }
    if need_listen {
        // имя сервера пачки берём из события rustplus `server_info` ({name,...})
        let a = app.clone();
        app.listen("server_info", move |ev| {
            if let Ok(v) = serde_json::from_str::<Value>(ev.payload()) {
                let name = v.get("name").and_then(|n| n.as_str()).map(|s| s.trim().to_string());
                a.state::<BmState>().inner.lock().unwrap().server_name = name.filter(|s| !s.is_empty());
            }
        });
    }
    emit_targets(app);
    if has_targets {
        ensure_task(app);
        let app2 = app.clone();
        tauri::async_runtime::spawn(async move {
            let _ = poll_once(&app2).await;
        });
    }
}

// ---------------------------------------------------------------- пачка, ключ, диск

fn active_key(app: &AppHandle) -> Option<String> {
    let st = app.try_state::<crate::profiles::ProfilesState>()?;
    st.active_profile()?.steam_key.filter(|k| !k.trim().is_empty() && k != "•••")
}

fn active_profile_id(app: &AppHandle) -> Option<String> {
    Some(app.try_state::<crate::profiles::ProfilesState>()?.active_profile()?.id)
}

/// (ip, порт Rust+) сервера активной пачки.
fn pack_server(app: &AppHandle) -> Option<(String, u16)> {
    let c = app.try_state::<crate::profiles::ProfilesState>()?.active_server()?;
    if c.ip.is_empty() {
        None
    } else {
        Some((c.ip, c.port))
    }
}

/// Ручные серверы активной пачки: (addr, label).
fn watch_servers(app: &AppHandle) -> Vec<(String, String)> {
    app.try_state::<crate::profiles::ProfilesState>()
        .and_then(|s| s.active_profile())
        .map(|p| p.watch_servers.into_iter().map(|w| (w.addr, w.label)).collect())
        .unwrap_or_default()
}

fn safe_id(id: &str) -> String {
    id.chars().filter(|c| c.is_ascii_alphanumeric() || *c == '-' || *c == '_').collect()
}

fn tracker_dir(app: &AppHandle) -> Option<PathBuf> {
    app.path().app_data_dir().ok().map(|d| d.join("tracker"))
}

fn read_json<T: serde::de::DeserializeOwned>(p: &Path) -> Option<T> {
    serde_json::from_slice(&std::fs::read(p).ok()?).ok()
}

/// Атомарная запись: tmp + rename. UTF-8 без BOM (serde_json его не пишет).
fn write_atomic(path: &Path, bytes: &[u8]) -> std::io::Result<()> {
    if let Some(d) = path.parent() {
        std::fs::create_dir_all(d)?;
    }
    let mut tmp = path.as_os_str().to_owned();
    tmp.push(".tmp");
    let tmp = PathBuf::from(tmp);
    std::fs::write(&tmp, bytes)?;
    std::fs::rename(&tmp, path)
}

fn save_targets(app: &AppHandle) {
    let (pid, targets) = {
        let g = app.state::<BmState>();
        let g = g.inner.lock().unwrap();
        (g.profile_id.clone(), g.targets.clone())
    };
    let (Some(pid), Some(dir)) = (pid, tracker_dir(app)) else { return };
    if let Ok(b) = serde_json::to_vec_pretty(&targets) {
        let _ = write_atomic(&dir.join(format!("{}.json", safe_id(&pid))), &b);
    }
}

fn save_sessions(app: &AppHandle) {
    let (pid, file) = {
        let g = app.state::<BmState>();
        let g = g.inner.lock().unwrap();
        (g.profile_id.clone(), SessionsFile { last_poll: g.last_poll, sessions: g.sessions.clone() })
    };
    let (Some(pid), Some(dir)) = (pid, tracker_dir(app)) else { return };
    if let Ok(b) = serde_json::to_vec(&file) {
        let _ = write_atomic(&dir.join(format!("{}-sessions.json", safe_id(&pid))), &b);
    }
}

// ---------------------------------------------------------------- опрос

enum PollErr {
    RateLimited(Option<u64>),
    Other,
}

fn emit_targets(app: &AppHandle) {
    let payload = {
        let st = app.state::<BmState>();
        let g = st.inner.lock().unwrap();
        targets_payload(&g.targets, &g.sessions, &g.notes, &g.src)
    };
    let _ = app.emit("targets", payload);
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
            backoff = match poll_once(&app2).await {
                Ok(()) => 0,
                Err(PollErr::RateLimited(ra)) => next_backoff(backoff, ra),
                Err(PollErr::Other) => next_backoff(backoff, None),
            };
        }
    }));
}

/// Следующая пауза при ошибке: Retry-After, иначе удвоение; в пределах [POLL, MAX].
fn next_backoff(prev: u64, retry_after: Option<u64>) -> u64 {
    retry_after.unwrap_or_else(|| prev.max(POLL_SECS) * 2).clamp(POLL_SECS, MAX_BACKOFF_SECS)
}

#[derive(Clone, Debug, PartialEq)]
struct WatchAddr {
    addr: SocketAddr,
    label: String,
}

/// Список серверов для A2S без дублей по адресу. Приоритет подписи: ручной список, пачка, эфемерные.
fn plan_servers(
    watch: &[(SocketAddr, String)],
    pack: Option<(SocketAddr, String)>,
    ephemeral: &[(SocketAddr, String)],
) -> Vec<WatchAddr> {
    let mut seen: HashSet<SocketAddr> = HashSet::new();
    let mut out = Vec::new();
    for (a, l) in watch.iter().cloned().chain(pack).chain(ephemeral.iter().cloned()) {
        if seen.insert(a) {
            out.push(WatchAddr { addr: a, label: l });
        }
    }
    out
}

/// Что резолвим в query-адрес A2S.
enum ResolveKind<'a> {
    /// Сервер пачки: ip, порт Rust+, имя (если известно).
    Pack(&'a str, u16, Option<&'a str>),
    /// gameserverip из Steam ("ip:игровой порт").
    Game(&'a str),
}

/// query-адрес A2S с кэшем: найденный ~1 ч, неудача ~5 мин (чтобы не дёргать master list каждый опрос).
/// Ключ Steam только передаётся в a2s (там он не логируется).
async fn cached_resolve(app: &AppHandle, cache_key: &str, kind: ResolveKind<'_>) -> Option<SocketAddr> {
    let now = now_ms();
    {
        let st = app.state::<BmState>();
        let g = st.inner.lock().unwrap();
        if let Some((addr, at)) = g.probe.get(cache_key) {
            let ttl = if addr.is_some() { PROBE_OK_MS } else { PROBE_RETRY_MS };
            if now - *at < ttl {
                return *addr;
            }
        }
    }
    let key = active_key(app);
    let found = match kind {
        ResolveKind::Pack(ip, port, name) => {
            crate::a2s::resolve_query_addr(key.as_deref(), ip, port, name, RESOLVE_TIMEOUT).await
        }
        ResolveKind::Game(raw) => crate::a2s::resolve_game_addr(key.as_deref(), raw, RESOLVE_TIMEOUT).await,
    };
    app.state::<BmState>().inner.lock().unwrap().probe.insert(cache_key.to_string(), (found, now));
    found
}

fn parse_sock(s: &str) -> Option<SocketAddr> {
    s.trim().parse::<SocketAddr>().ok()
}

/// Собирает серверы для опроса. Возвращает (список, сколько серверов настроено всего:
/// включая те, чей query-адрес определить не удалось).
async fn collect_servers(
    app: &AppHandle,
    server_name: Option<&str>,
    ephemeral_raw: &[String],
) -> (Vec<WatchAddr>, usize) {
    let mut total = 0usize;
    let mut watch = Vec::new();
    for (addr, label) in watch_servers(app) {
        total += 1;
        if let Some(a) = parse_sock(&addr) {
            let l = if label.trim().is_empty() { addr.trim().to_string() } else { label.trim().to_string() };
            watch.push((a, l));
        }
    }
    let mut pack_entry = None;
    if let Some((ip, port)) = pack_server(app) {
        total += 1;
        let ck = format!("pack:{ip}:{port}");
        if let Some(a) = cached_resolve(app, &ck, ResolveKind::Pack(&ip, port, server_name)).await {
            let l = server_name.map(String::from).unwrap_or_else(|| a.to_string());
            pack_entry = Some((a, l));
        }
    }
    let mut eph = Vec::new();
    for raw in ephemeral_raw.iter().take(MAX_EPHEMERAL) {
        total += 1;
        if let Some(a) = cached_resolve(app, &format!("eph:{raw}"), ResolveKind::Game(raw)).await {
            eph.push((a, raw.clone()));
        }
    }
    (plan_servers(&watch, pack_entry, &eph), total)
}

async fn query_all(servers: &[WatchAddr]) -> (Vec<(String, Vec<A2sPlayer>)>, Vec<String>) {
    let results: Vec<(String, Result<Vec<A2sPlayer>, String>)> = futures_util::stream::iter(servers.iter().cloned())
        .map(|s| async move {
            let r = crate::a2s::query_players(s.addr, A2S_TIMEOUT).await;
            (s.label, r)
        })
        .buffer_unordered(A2S_PAR)
        .collect()
        .await;
    let mut ok = Vec::new();
    let mut failed = Vec::new();
    for (l, r) in results {
        match r {
            Ok(p) => ok.push((l, p)),
            Err(_) => failed.push(l),
        }
    }
    (ok, failed)
}

async fn poll_once(app: &AppHandle) -> Result<(), PollErr> {
    let (gen, targets, server_name) = {
        let st = app.state::<BmState>();
        let g = st.inner.lock().unwrap();
        (g.generation, g.targets.clone(), g.server_name.clone())
    };
    if targets.is_empty() {
        return Ok(());
    }
    // --- канал Steam
    let mut steam_err: Option<PollErr> = None;
    let mut sums: Option<HashMap<String, Value>> = None;
    let mut ids: Vec<String> = targets.iter().filter_map(|t| t.steam_id.clone()).collect();
    ids.sort();
    ids.dedup();
    if let (Some(key), false) = (active_key(app), ids.is_empty()) {
        match fetch_summaries(&key, &ids).await {
            Ok(m) => sums = Some(m),
            Err(HttpErr::RateLimited(ra)) => steam_err = Some(PollErr::RateLimited(ra)),
            Err(HttpErr::Other(_)) => steam_err = Some(PollErr::Other),
        }
    }
    let pack = pack_server(app);
    let presence_of = |t: &TargetRec| -> Option<Presence> {
        let id = t.steam_id.as_ref()?;
        sums.as_ref()?.get(id).map(parse_presence)
    };
    // --- эфемерные серверы: где Steam видит открытые профили (кроме сервера пачки)
    let mut ephemeral: Vec<String> = Vec::new();
    for t in &targets {
        if let Some(p) = presence_of(t) {
            let is_pack = label_server(&p.srv, pack.as_ref().map(|(i, p)| (i.as_str(), *p)), Some("x")) == "x";
            if p.in_rust && !p.srv.is_empty() && !is_pack && !ephemeral.contains(&p.srv) {
                ephemeral.push(p.srv);
            }
        }
    }
    // --- канал серверов (A2S)
    let (servers, servers_total) = collect_servers(app, server_name.as_deref(), &ephemeral).await;
    let (ok_servers, failed) = query_all(&servers).await;
    let servers_ok = ok_servers.len();

    let now = now_ms();
    let (names_changed, ()) = {
        let st = app.state::<BmState>();
        let mut guard = st.inner.lock().unwrap();
        if guard.generation != gen {
            return Ok(()); // пачку сменили, пока шёл опрос
        }
        let g = &mut *guard;
        let last_poll = g.last_poll;
        let mut names_changed = false;
        for t in &targets {
            let k = t.key();
            let pr = presence_of(t);
            // свежий personaname (в т.ч. после смены ника) добавляем в ники для матчинга
            let mut names = t.names.clone();
            if let (Some(id), Some(s)) = (&t.steam_id, sums.as_ref()) {
                if let Some(n) = s.get(id).and_then(|p| p.get("personaname")).and_then(|v| v.as_str()) {
                    let before = names.len();
                    merge_names(&mut names, [n.to_string()]);
                    if names.len() != before {
                        if let Some(rec) = g.targets.iter_mut().find(|x| x.key() == k) {
                            rec.names = names.clone();
                            names_changed = true;
                        }
                    }
                }
            }
            let steam_obs = pr.as_ref().map(|p| {
                let srv = label_server(&p.srv, pack.as_ref().map(|(i, p)| (i.as_str(), *p)), server_name.as_deref());
                (p.in_rust, srv)
            });
            let hit = find_match(&names, &ok_servers, now);
            let ss = g.sessions.entry(k.clone()).or_default();
            let open_srv = ss.last().filter(|s| s.stop.is_none()).map(|s| s.srv.clone());
            let dec = decide(
                t.steam_id.is_some(),
                steam_obs.as_ref().map(|(a, b)| (*a, b.as_str())),
                hit.as_ref(),
                servers_ok > 0,
                open_srv.as_deref(),
                &failed,
            );
            if let Some(d) = dec {
                apply_presence(ss, d.on, &d.srv, now, last_poll, d.since_hint);
                match d.src {
                    Some(s) if d.on => g.src.insert(k.clone(), s),
                    _ => g.src.remove(&k),
                };
            }
            let has_sessions = g.sessions.get(&k).map_or(false, |s| !s.is_empty());
            let ni = NoteIn {
                has_steam: t.steam_id.is_some(),
                public: pr.as_ref().map(|p| p.public),
                tracked_since: t.tracked_since,
                has_sessions,
                now,
                servers_total,
                servers_ok,
                src: g.src.get(&k).copied(),
            };
            match track_note(&ni) {
                Some(n) => g.notes.insert(k, n),
                None => g.notes.remove(&k),
            };
        }
        g.last_poll = now;
        (names_changed, ())
    };
    if names_changed {
        save_targets(app);
    }
    // lastPoll пишем каждый раз: по нему закрываются сессии после простоя приложения
    save_sessions(app);
    emit_targets(app);
    match steam_err {
        Some(e) => Err(e),
        None => Ok(()),
    }
}

// ---------------------------------------------------------------- HTTP

enum HttpErr {
    RateLimited(Option<u64>),
    Other(String),
}

fn human(e: HttpErr) -> String {
    match e {
        HttpErr::RateLimited(_) => "лимит запросов Steam, повторите позже".into(),
        HttpErr::Other(s) => s,
    }
}

fn client() -> Result<reqwest::Client, HttpErr> {
    static C: std::sync::OnceLock<Option<reqwest::Client>> = std::sync::OnceLock::new();
    C.get_or_init(|| reqwest::Client::builder().timeout(TIMEOUT).user_agent("hollow-plus").build().ok())
        .clone()
        .ok_or_else(|| HttpErr::Other("Не удалось создать HTTP-клиент".into()))
}

fn scrub(msg: &str, key: &str) -> String {
    if key.is_empty() {
        msg.to_string()
    } else {
        msg.replace(key, "***")
    }
}

async fn send_json(req: reqwest::RequestBuilder, key: &str) -> Result<Value, HttpErr> {
    let resp = req
        .send()
        .await
        .map_err(|e| HttpErr::Other(format!("Нет связи со Steam: {}", scrub(&e.without_url().to_string(), key))))?;
    let status = resp.status();
    if status.as_u16() == 429 {
        let ra = resp
            .headers()
            .get("retry-after")
            .and_then(|v| v.to_str().ok())
            .and_then(|s| s.trim().parse::<u64>().ok());
        return Err(HttpErr::RateLimited(ra));
    }
    if matches!(status.as_u16(), 401 | 403) {
        return Err(HttpErr::Other("Steam отклонил запрос (ключ неверный или данные скрыты)".into()));
    }
    if !status.is_success() {
        return Err(HttpErr::Other(format!("Steam вернул HTTP {}", status.as_u16())));
    }
    resp.json::<Value>()
        .await
        .map_err(|e| HttpErr::Other(format!("Некорректный ответ Steam: {}", scrub(&e.without_url().to_string(), key))))
}

/// Ключ идёт через .query(), а не в строке URL; из ошибок URL вычищается.
async fn steam_get(key: &str, path: &str, params: &[(&str, String)]) -> Result<Value, HttpErr> {
    let mut q: Vec<(&str, String)> = vec![("key", key.to_string())];
    q.extend(params.iter().cloned());
    send_json(client()?.get(format!("{BASE}/{path}")).query(&q), key).await
}

/// Делит id на запросы по BATCH штук (Valve: до 100 steamids).
fn chunk_ids(ids: &[String]) -> Vec<String> {
    ids.chunks(BATCH).map(|c| c.join(",")).collect()
}

/// GetPlayerSummaries пачками по 100: steamid -> объект игрока.
async fn fetch_summaries(key: &str, ids: &[String]) -> Result<HashMap<String, Value>, HttpErr> {
    let mut out = HashMap::new();
    for (i, chunk) in chunk_ids(ids).into_iter().enumerate() {
        if i > 0 {
            tokio::time::sleep(Duration::from_millis(300)).await;
        }
        let v = steam_get(key, "ISteamUser/GetPlayerSummaries/v2/", &[("steamids", chunk)]).await?;
        if let Some(arr) = v.pointer("/response/players").and_then(|p| p.as_array()) {
            for p in arr {
                if let Some(id) = p.get("steamid").and_then(|s| s.as_str()) {
                    out.insert(id.to_string(), p.clone());
                }
            }
        }
    }
    Ok(out)
}

/// Vanity -> SteamID64. Ok(None): нет совпадения (success 42).
async fn resolve_vanity(key: &str, v: &str) -> Result<Option<String>, String> {
    let r = steam_get(key, "ISteamUser/ResolveVanityURL/v1/", &[("vanityurl", v.to_string())])
        .await
        .map_err(human)?;
    Ok(parse_vanity(&r))
}

async fn resolve_query(key: &str, query: &str) -> Result<String, String> {
    match parse_query(query)? {
        Query::Id(id) => Ok(id),
        Query::Vanity(v) => resolve_vanity(key, &v).await?.ok_or_else(|| NOT_FOUND.to_string()),
    }
}

/// Публичная страница ajaxaliases (без ключа). None при сбое; `[]` -> Some(пусто).
async fn fetch_aliases(id: &str) -> Option<Vec<(String, String)>> {
    let req = client().ok()?.get(format!("{ALIASES_URL}/{id}/ajaxaliases")).header("Accept-Language", "en");
    let v = send_json(req, "").await.ok()?;
    parse_aliases(&v)
}

// ---------------------------------------------------------------- чистый разбор

fn is_steam_id(s: &str) -> bool {
    s.len() == 17 && s.starts_with("7656119") && s.bytes().all(|b| b.is_ascii_digit())
}

#[derive(Debug, PartialEq)]
enum Query {
    Id(String),
    Vanity(String),
}

fn valid_vanity(s: &str) -> bool {
    (2..=32).contains(&s.len()) && s.chars().all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-')
}

/// SteamID64, ссылка /profiles/<id>, ссылка /id/<vanity> или просто vanity.
/// Поиска по произвольному нику у Steam нет: всё остальное даёт ошибку (track_add превращает её
/// в цель «только по нику», lookup_player показывает понятное сообщение).
fn parse_query(q: &str) -> Result<Query, String> {
    let t = q.trim();
    if is_steam_id(t) {
        return Ok(Query::Id(t.to_string()));
    }
    const HOST: &str = "steamcommunity.com/";
    let lower = t.to_ascii_lowercase();
    if let Some(pos) = lower.find(HOST) {
        let rest = t[pos + HOST.len()..].split(|c| c == '?' || c == '#').next().unwrap_or("");
        let mut it = rest.split('/').filter(|s| !s.is_empty());
        let kind = it.next().map(|s| s.to_ascii_lowercase());
        return match (kind.as_deref(), it.next()) {
            (Some("profiles"), Some(v)) if is_steam_id(v) => Ok(Query::Id(v.to_string())),
            (Some("id"), Some(v)) if valid_vanity(v) => Ok(Query::Vanity(v.to_string())),
            _ => Err(NOT_FOUND.to_string()),
        };
    }
    if valid_vanity(t) {
        Ok(Query::Vanity(t.to_string()))
    } else {
        Err(NOT_FOUND.to_string())
    }
}

/// {"response":{"success":1,"steamid":"..."}}; success 42 = нет совпадения.
fn parse_vanity(v: &Value) -> Option<String> {
    let r = v.get("response")?;
    if r.get("success").and_then(|s| s.as_i64()) != Some(1) {
        return None;
    }
    r.get("steamid").and_then(|s| s.as_str()).filter(|s| is_steam_id(s)).map(String::from)
}

#[derive(Debug, PartialEq, Default)]
struct Presence {
    in_rust: bool,
    /// Сырое gameserverip ("" если нет или "0.0.0.0:0").
    srv: String,
    public: bool,
}

/// Из объекта игрока GetPlayerSummaries. «В Rust» = gameid == "252490".
fn parse_presence(p: &Value) -> Presence {
    let in_rust = match p.get("gameid") {
        Some(Value::String(s)) => s == RUST_APPID,
        Some(Value::Number(n)) => n.to_string() == RUST_APPID,
        _ => false,
    };
    let srv = p
        .get("gameserverip")
        .and_then(|s| s.as_str())
        .map(str::trim)
        .filter(|s| !s.is_empty() && *s != "0.0.0.0:0")
        .unwrap_or("")
        .to_string();
    let public = matches!(p.get("communityvisibilitystate").and_then(|v| v.as_i64()), Some(3) | Some(5));
    Presence { in_rust, srv: if in_rust { srv } else { String::new() }, public }
}

/// Подпись сервера: если ip совпадает с сервером пачки (порт Rust+ = игровой или игровой + 67,
/// эвристика) и имя известно, то имя; иначе ip:port.
fn label_server(raw: &str, pack: Option<(&str, u16)>, name: Option<&str>) -> String {
    if raw.is_empty() {
        return String::new();
    }
    if let (Some((pip, pport)), Some(n)) = (pack, name) {
        if let Some((ip, port)) = raw.rsplit_once(':') {
            if let Ok(port) = port.parse::<u32>() {
                if ip == pip && (port == pport as u32 || port + 67 == pport as u32) {
                    return n.to_string();
                }
            }
        }
    }
    raw.to_string()
}

// --- ники и A2S-матчинг

/// Нормализация ника: крайние пробелы и регистр (Unicode). Префиксы клановых тегов не угадываем.
fn norm_name(s: &str) -> String {
    s.trim().to_lowercase()
}

/// Добавляет ники без дублей (по нормализованному виду), пустые пропускает, не больше MAX_NAMES.
fn merge_names(dst: &mut Vec<String>, new: impl IntoIterator<Item = String>) {
    for n in new {
        let t = n.trim();
        if t.is_empty() || dst.len() >= MAX_NAMES {
            continue;
        }
        let k = norm_name(t);
        if !dst.iter().any(|x| norm_name(x) == k) {
            dst.push(t.to_string());
        }
    }
}

#[derive(Clone, Debug, PartialEq)]
struct A2sHit {
    srv: String,
    /// Когда игрок зашёл на сервер: now - duration_s.
    since: i64,
}

/// Первый сервер, где имя игрока равно одному из ников цели (после нормализации).
fn find_match(names: &[String], servers: &[(String, Vec<A2sPlayer>)], now: i64) -> Option<A2sHit> {
    let wanted: HashSet<String> = names.iter().map(|n| norm_name(n)).filter(|n| !n.is_empty()).collect();
    if wanted.is_empty() {
        return None;
    }
    for (label, players) in servers {
        if let Some(p) = players.iter().find(|p| wanted.contains(&norm_name(&p.name))) {
            let d = if p.duration_s.is_finite() && p.duration_s > 0.0 { (p.duration_s as f64 * 1000.0) as i64 } else { 0 };
            return Some(A2sHit { srv: label.clone(), since: (now - d).max(0) });
        }
    }
    None
}

#[derive(Debug, PartialEq)]
struct Decision {
    on: bool,
    srv: String,
    src: Option<&'static str>,
    since_hint: Option<i64>,
}

/// Объединяет каналы. None = состояние неизвестно, сессии не трогаем:
///  * у цели есть SteamID, но данных Steam в этом опросе нет, и по нику её не нашли;
///  * у цели только ник и ни один сервер не ответил;
///  * открытая сессия шла на сервере, который сейчас не ответил.
fn decide(
    has_steam_id: bool,
    steam: Option<(bool, &str)>,
    hit: Option<&A2sHit>,
    a2s_ok: bool,
    open_srv: Option<&str>,
    failed: &[String],
) -> Option<Decision> {
    let steam_in = steam.map_or(false, |s| s.0);
    match (steam_in, hit) {
        (true, Some(h)) => Some(Decision { on: true, srv: h.srv.clone(), src: Some("both"), since_hint: Some(h.since) }),
        (true, None) => Some(Decision {
            on: true,
            srv: steam.map(|s| s.1.to_string()).unwrap_or_default(),
            src: Some("steam"),
            since_hint: None,
        }),
        (false, Some(h)) => Some(Decision { on: true, srv: h.srv.clone(), src: Some("server"), since_hint: Some(h.since) }),
        (false, None) => {
            let known = if has_steam_id { steam.is_some() } else { a2s_ok };
            let open_failed = open_srv.map_or(false, |s| failed.iter().any(|f| f == s));
            if known && !open_failed {
                Some(Decision { on: false, srv: String::new(), src: None, since_hint: None })
            } else {
                None
            }
        }
    }
}

struct NoteIn {
    has_steam: bool,
    /// None: данных профиля Steam в этом опросе нет (нет ключа, сбой, профиль не найден).
    public: Option<bool>,
    tracked_since: i64,
    has_sessions: bool,
    now: i64,
    servers_total: usize,
    servers_ok: usize,
    src: Option<&'static str>,
}

/// Честная заметка о том, что ограничивает слежение. Закрытый профиль и скрытая игровая активность
/// неотличимы от «не играет» в Steam; для второго случая это эвристика (открыт, 7+ дней без сессий).
fn track_note(i: &NoteIn) -> Option<String> {
    let servers_note = |prefix: &str| -> String {
        if i.servers_total == 0 {
            format!("{prefix}, но серверов для опроса нет (подключи сервер пачки или добавь сервер в Настройках)")
        } else if i.servers_ok == 0 {
            format!("{prefix}, но серверы не отвечают на A2S-запросы")
        } else {
            format!("{prefix}: совпадение по нику, возможен тёзка")
        }
    };
    if !i.has_steam {
        return Some(servers_note("слежение только по нику на серверах"));
    }
    match i.public {
        Some(false) => Some(if i.servers_total == 0 {
            "профиль закрыт и серверов для опроса нет: слежение невозможно (подключи сервер пачки или добавь сервер в Настройках)".into()
        } else if i.servers_ok == 0 {
            "профиль закрыт, серверы не отвечают: слежение невозможно".into()
        } else {
            "профиль закрыт: онлайн определяется только по нику на серверах (совпадение по нику, возможен тёзка)".into()
        }),
        None => Some(if i.servers_ok > 0 {
            "данных Steam нет (ключ не задан или профиль не найден): онлайн только по нику на серверах (возможен тёзка)".into()
        } else {
            "данных Steam нет (ключ не задан или профиль не найден) и серверы не опрашиваются: слежение невозможно".into()
        }),
        Some(true) => {
            if i.src == Some("server") {
                Some("онлайн определён по нику на сервере: возможен тёзка".into())
            } else if !i.has_sessions && i.servers_ok == 0 && i.now - i.tracked_since > 7 * MS_DAY {
                Some("игровая активность скрыта (или игрок не заходил в Rust)".into())
            } else {
                None
            }
        }
    }
}

/// Применяет текущее состояние к списку сессий (хронологический). Возвращает true, если что-то изменилось.
/// Открытая сессия при «не в сети» закрывается временем прошлого опроса (`last_poll`; когда он
/// неизвестен или раньше старта, временем старта): после простоя приложения не накручиваем часы.
/// Одна непрерывная сессия на цель, источник не важен: пока открыта, новые каналы её продолжают.
/// `start_hint`: реальное начало (A2S: now - duration), не раньше конца предыдущей сессии.
fn apply_presence(ss: &mut Vec<Session>, on: bool, srv: &str, now: i64, last_poll: i64, start_hint: Option<i64>) -> bool {
    let prev_stop = ss.last().and_then(|s| s.stop);
    let open = ss.last_mut().filter(|s| s.stop.is_none());
    match (open, on) {
        (Some(s), true) => {
            if s.srv.is_empty() && !srv.is_empty() {
                s.srv = srv.to_string();
                true
            } else {
                false
            }
        }
        (Some(s), false) => {
            let end = if last_poll > s.start { last_poll } else { s.start };
            s.stop = Some(end.min(now));
            true
        }
        (None, true) => {
            let start = start_hint.map_or(now, |h| h.min(now)).max(prev_stop.unwrap_or(0));
            ss.push(Session { start, stop: None, srv: srv.to_string() });
            if ss.len() > MAX_SESSIONS {
                let extra = ss.len() - MAX_SESSIONS;
                ss.drain(..extra);
            }
            true
        }
        (None, false) => false,
    }
}

/// (on, srv, since_ms) по последней сессии. since = 0, если неизвестно (фронт подставит текущее).
fn target_status(ss: &[Session]) -> (bool, String, i64) {
    match ss.last() {
        Some(s) if s.stop.is_none() => (true, s.srv.clone(), s.start),
        Some(s) => (false, s.srv.clone(), s.stop.unwrap_or(s.start)),
        None => (false, String::new(), 0),
    }
}

/// `targets`: [{n,on,srv,since}] + необязательные src/note/steamId/trackedSince.
fn targets_payload(
    targets: &[TargetRec],
    sessions: &HashMap<String, Vec<Session>>,
    notes: &HashMap<String, String>,
    srcs: &HashMap<String, &'static str>,
) -> Value {
    let list: Vec<Value> = targets
        .iter()
        .map(|t| {
            let k = t.key();
            let (on, srv, since) = target_status(sessions.get(&k).map_or(&[][..], |v| v.as_slice()));
            json!({
                "n": t.label, "on": on, "srv": srv, "since": since,
                "src": if on { srcs.get(&k).copied() } else { None },
                "steamId": t.steam_id,
                "trackedSince": t.tracked_since,
                "note": notes.get(&k),
            })
        })
        .collect();
    Value::Array(list)
}

/// ajaxaliases: [{"newname","timechanged"}] -> [(имя, строка даты)].
fn parse_aliases(v: &Value) -> Option<Vec<(String, String)>> {
    let arr = v.as_array()?;
    Some(
        arr.iter()
            .filter_map(|a| {
                let n = a.get("newname")?.as_str()?.trim();
                if n.is_empty() {
                    return None;
                }
                let t = a.get("timechanged").and_then(|t| t.as_str()).unwrap_or("");
                Some((n.to_string(), t.to_string()))
            })
            .collect(),
    )
}

/// ("names", "aliasesDetailed"): без текущего ника и дублей, свежие первыми.
fn aliases_from(list: &[(String, String)], nick: &str, now: i64) -> (Vec<String>, Vec<Value>) {
    let year = civil_from_days(now.div_euclid(MS_DAY)).0;
    let mut names: Vec<String> = Vec::new();
    let mut detailed = Vec::new();
    for (n, t) in list {
        if n.eq_ignore_ascii_case(nick) || names.iter().any(|x| x == n) {
            continue;
        }
        names.push(n.clone());
        detailed.push(json!({"name": n, "lastSeen": parse_alias_time(t, year)}));
    }
    (names, detailed)
}

/// "7 May, 2019 @ 11:04pm" (год может отсутствовать у свежих записей) -> мс. Пояс неизвестен: UTC.
fn parse_alias_time(s: &str, default_year: i64) -> Option<i64> {
    let (date, time) = s.split_once('@')?;
    let cleaned = date.replace(',', " ");
    let mut it = cleaned.split_whitespace();
    let day: i64 = it.next()?.parse().ok()?;
    let mon = it.next()?.get(..3)?.to_ascii_lowercase();
    let month = ["jan", "feb", "mar", "apr", "may", "jun", "jul", "aug", "sep", "oct", "nov", "dec"]
        .iter()
        .position(|m| *m == mon)? as i64
        + 1;
    let year = match it.next() {
        Some(y) => y.parse().ok()?,
        None => default_year,
    };
    let time = time.trim().to_ascii_lowercase();
    let pm = time.ends_with("pm");
    let hm = time.trim_end_matches("am").trim_end_matches("pm");
    let (h, m) = hm.split_once(':')?;
    let (mut h, m): (i64, i64) = (h.trim().parse().ok()?, m.trim().parse().ok()?);
    if h > 12 || m > 59 || !(1..=31).contains(&day) {
        return None;
    }
    h %= 12;
    if pm {
        h += 12;
    }
    Some(((days_from_civil(year, month, day) * 24 + h) * 60 + m) * 60_000)
}

fn session_lines(ss: &[Session]) -> Vec<String> {
    ss.iter()
        .rev()
        .take(5)
        .map(|s| {
            let srv = if s.srv.is_empty() { "Rust" } else { &s.srv };
            match s.stop {
                Some(e) => format!("{srv} · {} · {} мин", fmt_date(s.start), ((e - s.start).max(0) / 60000)),
                None => format!("{srv} · {} · онлайн", fmt_date(s.start)),
            }
        })
        .collect()
}

// ---------------------------------------------------------------- активность

fn now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

/// Раскладывает [start,end) по часовым слотам: f(абсолютный_час_от_эпохи, мс_в_слоте).
fn slice_hours(start: i64, end: i64, mut f: impl FnMut(i64, i64)) {
    let end = end.min(start + MAX_SESSION_MS);
    let mut t = start;
    while t < end {
        let h = t.div_euclid(MS_HOUR);
        let e = end.min((h + 1) * MS_HOUR);
        f(h, e - t);
        t = e;
    }
}

/// День недели по номеру дня от эпохи: 0 = понедельник (1970-01-01 был четвергом).
fn weekday_of_day(day: i64) -> usize {
    (day + 3).rem_euclid(7) as usize
}

fn session_end(s: &Session, now: i64) -> i64 {
    s.stop.unwrap_or(now).max(s.start)
}

/// Матрица 7x24 (мс), мс по дням от эпохи. Время всегда UTC.
fn bucket_sessions(sessions: &[Session], now: i64) -> ([[i64; 24]; 7], BTreeMap<i64, i64>) {
    let mut m = [[0i64; 24]; 7];
    let mut days = BTreeMap::new();
    for s in sessions {
        slice_hours(s.start, session_end(s, now), |h, ms| {
            let day = h.div_euclid(24);
            m[weekday_of_day(day)][h.rem_euclid(24) as usize] += ms;
            *days.entry(day).or_insert(0) += ms;
        });
    }
    (m, days)
}

fn to_min(ms: i64) -> i64 {
    (ms + 30_000) / 60_000
}

/// Пик: слот с максимумом; при равенстве первый по (день недели, час). None, если игры нет.
fn peak_of(m: &[[i64; 24]; 7]) -> Option<Value> {
    let mut best: Option<(usize, usize, i64)> = None;
    for (d, row) in m.iter().enumerate() {
        for (h, &v) in row.iter().enumerate() {
            if v > 0 && best.map_or(true, |b| v > b.2) {
                best = Some((d, h, v));
            }
        }
    }
    best.map(|(d, h, v)| json!({"weekday": d, "hour": h, "minutes": to_min(v)}))
}

fn days_to_date(day: i64) -> String {
    let (y, m, d) = civil_from_days(day);
    format!("{y:04}-{m:02}-{d:02}")
}

/// "activity" для профиля; None, если сессий нет. Всё в UTC (tzNote = "utc").
fn build_activity(sessions: &[Session], now: i64) -> Option<Value> {
    if sessions.is_empty() {
        return None;
    }
    let (m, days) = bucket_sessions(sessions, now);
    let total_ms: i64 = m.iter().flatten().sum();
    let min_start = sessions.iter().map(|s| s.start).min()?;
    let max_end = sessions.iter().map(|s| session_end(s, now)).max()?.max(min_start);
    let span_days = ((max_end - min_start + MS_DAY - 1) / MS_DAY).max(1);
    let today = now.div_euclid(MS_DAY);
    let by_day: Vec<Value> = days
        .iter()
        .filter(|(d, v)| **d > today - 365 && **d <= today && **v > 0)
        .map(|(d, v)| json!({"date": days_to_date(*d), "minutes": to_min(*v)}))
        .collect();
    let by_week_hour: Vec<Vec<i64>> = m.iter().map(|r| r.iter().map(|&v| to_min(v)).collect()).collect();
    Some(json!({
        "byWeekHour": by_week_hour,
        "peak": peak_of(&m),
        "days": span_days,
        "sessions": sessions.len(),
        "totalHours": (total_ms as f64 / MS_HOUR as f64 * 10.0).round() / 10.0,
        "byDay": by_day,
        "tzNote": "utc",
    }))
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

    fn j(s: &str) -> Value {
        serde_json::from_str(s).unwrap()
    }

    /// UTC-время в мс для тестов.
    fn t(y: i64, mo: i64, d: i64, h: i64, mi: i64) -> i64 {
        ((days_from_civil(y, mo, d) * 24 + h) * 60 + mi) * 60_000
    }

    fn ss(a: i64, b: Option<i64>) -> Session {
        Session { start: a, stop: b, srv: "S".into() }
    }

    fn pl(name: &str, dur: f32) -> A2sPlayer {
        A2sPlayer { name: name.into(), score: 0, duration_s: dur }
    }

    fn sa(s: &str) -> SocketAddr {
        s.parse().unwrap()
    }

    fn names(v: &[&str]) -> Vec<String> {
        v.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn query_parsing() {
        let id = "76561197960287930";
        assert_eq!(parse_query(id), Ok(Query::Id(id.into())));
        assert_eq!(parse_query(&format!(" https://steamcommunity.com/profiles/{id}/ ")), Ok(Query::Id(id.into())));
        assert_eq!(parse_query(&format!("HTTP://SteamCommunity.com/profiles/{id}?xml=1")), Ok(Query::Id(id.into())));
        assert_eq!(parse_query("https://steamcommunity.com/id/gabelogannewell/"), Ok(Query::Vanity("gabelogannewell".into())));
        assert_eq!(parse_query("steamcommunity.com/id/abc#x"), Ok(Query::Vanity("abc".into())));
        assert_eq!(parse_query("gabelogannewell"), Ok(Query::Vanity("gabelogannewell".into())));
        assert_eq!(parse_query("Some Nick"), Err(NOT_FOUND.to_string()));
        assert_eq!(parse_query("https://steamcommunity.com/groups/x"), Err(NOT_FOUND.to_string()));
        assert_eq!(parse_query("https://steamcommunity.com/profiles/123"), Err(NOT_FOUND.to_string()));
        assert!(!is_steam_id("7656119000000001") && !is_steam_id("12345678901234567"));
    }

    #[test]
    fn vanity_response() {
        assert_eq!(
            parse_vanity(&j(r#"{"response":{"steamid":"76561197960287930","success":1}}"#)).as_deref(),
            Some("76561197960287930")
        );
        assert_eq!(parse_vanity(&j(r#"{"response":{"success":42,"message":"No match"}}"#)), None);
        assert_eq!(parse_vanity(&j(r#"{}"#)), None);
    }

    #[test]
    fn presence_detection() {
        let p = j(r#"{"steamid":"1","communityvisibilitystate":3,"personastate":1,
            "gameid":"252490","gameextrainfo":"Rust","gameserverip":"1.2.3.4:28015"}"#);
        assert_eq!(parse_presence(&p), Presence { in_rust: true, srv: "1.2.3.4:28015".into(), public: true });
        let other = j(r#"{"communityvisibilitystate":3,"gameid":"730","gameserverip":"5.5.5.5:27015"}"#);
        assert_eq!(parse_presence(&other), Presence { in_rust: false, srv: "".into(), public: true });
        let empty = j(r#"{"communityvisibilitystate":3,"gameserverip":"0.0.0.0:0"}"#);
        assert_eq!(parse_presence(&empty), Presence { in_rust: false, srv: "".into(), public: true });
        let rust_no_srv = j(r#"{"communityvisibilitystate":3,"gameid":"252490","gameserverip":"0.0.0.0:0"}"#);
        let r = parse_presence(&rust_no_srv);
        assert!(r.in_rust && r.srv.is_empty());
        let closed = j(r#"{"communityvisibilitystate":1,"personastate":0}"#);
        assert!(!parse_presence(&closed).public);
        assert!(parse_presence(&j(r#"{"communityvisibilitystate":5}"#)).public);
        assert!(parse_presence(&j(r#"{"gameid":252490,"communityvisibilitystate":3}"#)).in_rust);
    }

    #[test]
    fn server_label() {
        assert_eq!(label_server("", Some(("1.2.3.4", 28082)), Some("My")), "");
        assert_eq!(label_server("1.2.3.4:28015", Some(("1.2.3.4", 28082)), Some("My")), "My");
        assert_eq!(label_server("1.2.3.4:28082", Some(("1.2.3.4", 28082)), Some("My")), "My");
        assert_eq!(label_server("1.2.3.4:28015", Some(("1.2.3.4", 28082)), None), "1.2.3.4:28015");
        assert_eq!(label_server("9.9.9.9:28015", Some(("1.2.3.4", 28082)), Some("My")), "9.9.9.9:28015");
        assert_eq!(label_server("1.2.3.4:28015", None, Some("My")), "1.2.3.4:28015");
    }

    #[test]
    fn nick_matching() {
        let now = 1_000_000_000;
        let servers = vec![("A".to_string(), vec![pl("  HoLLow  ", 120.0), pl("Другой", 5.0)])];
        // регистр и крайние пробелы
        let h = find_match(&names(&["hollow"]), &servers, now).unwrap();
        assert_eq!(h, A2sHit { srv: "A".into(), since: now - 120_000 });
        // Unicode (кириллица, разный регистр)
        assert!(find_match(&names(&["ДРУГОЙ"]), &servers, now).is_some());
        // точное равенство: префикс/тег и подстрока не совпадают
        assert!(find_match(&names(&["[TAG] Hollow"]), &servers, now).is_none());
        assert!(find_match(&names(&["Holl"]), &servers, now).is_none());
        // пустые ники и пустое имя игрока (подключается) не матчатся
        let conn = vec![("A".to_string(), vec![pl("", 1.0)])];
        assert!(find_match(&names(&["", "  "]), &conn, now).is_none());
        // любой из ников цели
        assert!(find_match(&names(&["old", "Hollow"]), &servers, now).is_some());
        // мусорная длительность
        let bad = vec![("A".to_string(), vec![pl("x", f32::NAN)])];
        assert_eq!(find_match(&names(&["x"]), &bad, now).unwrap().since, now);
    }

    #[test]
    fn names_merge_dedup() {
        let mut v = names(&["Hollow"]);
        merge_names(&mut v, ["hollow ".to_string(), "  ".to_string(), "Old".to_string(), "OLD".to_string()]);
        assert_eq!(v, names(&["Hollow", "Old"]));
        let many = (0..100).map(|i| format!("n{i}"));
        merge_names(&mut v, many);
        assert_eq!(v.len(), MAX_NAMES);
    }

    #[test]
    fn channels_union() {
        let hit = A2sHit { srv: "Srv".into(), since: 500 };
        // Steam и сервер
        let d = decide(true, Some((true, "1.1.1.1:1")), Some(&hit), true, None, &[]).unwrap();
        assert_eq!((d.on, d.src, d.srv.as_str(), d.since_hint), (true, Some("both"), "Srv", Some(500)));
        // только Steam
        let d = decide(true, Some((true, "1.1.1.1:1")), None, false, None, &[]).unwrap();
        assert_eq!((d.on, d.src, d.srv.as_str(), d.since_hint), (true, Some("steam"), "1.1.1.1:1", None));
        // только сервер (закрытый профиль: Steam говорит «не в игре»)
        let d = decide(true, Some((false, "")), Some(&hit), true, None, &[]).unwrap();
        assert_eq!((d.on, d.src), (true, Some("server")));
        // ни там ни там, оба канала известны: офлайн
        let d = decide(true, Some((false, "")), None, true, Some("Srv"), &[]).unwrap();
        assert!(!d.on);
        // данных Steam нет и по нику не нашли: неизвестно, сессию не трогаем
        assert_eq!(decide(true, None, None, true, Some("Srv"), &[]), None);
        // цель только по нику: серверы не ответили -> неизвестно; ответили -> офлайн
        assert_eq!(decide(false, None, None, false, None, &[]), None);
        assert!(!decide(false, None, None, true, None, &[]).unwrap().on);
        // открытая сессия на сервере, который не ответил: неизвестно
        assert_eq!(decide(false, None, None, true, Some("Srv"), &["Srv".to_string()]), None);
        // а если не ответил другой сервер: офлайн
        assert!(decide(false, None, None, true, Some("Srv"), &["Other".to_string()]).is_some());
    }

    #[test]
    fn server_list_dedup() {
        let w = vec![(sa("1.2.3.4:28015"), "Мой".to_string()), (sa("5.6.7.8:28015"), "B".to_string())];
        let pack = Some((sa("1.2.3.4:28015"), "Пачка".to_string()));
        let eph = vec![(sa("5.6.7.8:28015"), "5.6.7.8:28015".to_string()), (sa("9.9.9.9:1"), "9.9.9.9:1".to_string())];
        let l = plan_servers(&w, pack, &eph);
        assert_eq!(l.len(), 3);
        // подпись ручного списка приоритетнее
        assert_eq!(l[0], WatchAddr { addr: sa("1.2.3.4:28015"), label: "Мой".into() });
        assert_eq!(l[2].addr, sa("9.9.9.9:1"));
        // только пачка без ручного списка
        assert_eq!(plan_servers(&[], Some((sa("1.1.1.1:1"), "P".into())), &[]).len(), 1);
        assert!(plan_servers(&[], None, &[]).is_empty());
        assert!(parse_sock("bad").is_none() && parse_sock(" 1.2.3.4:5 ").is_some());
    }

    fn note_in() -> NoteIn {
        NoteIn {
            has_steam: true,
            public: Some(true),
            tracked_since: 0,
            has_sessions: false,
            now: 100 * MS_DAY,
            servers_total: 1,
            servers_ok: 1,
            src: None,
        }
    }

    #[test]
    fn track_notes() {
        let mut n = note_in();
        assert_eq!(track_note(&n), None);
        // закрытый профиль: честно называем, что ограничивает
        n.public = Some(false);
        assert!(track_note(&n).unwrap().contains("только по нику"));
        n.servers_ok = 0;
        assert!(track_note(&n).unwrap().contains("серверы не отвечают"));
        n.servers_total = 0;
        assert!(track_note(&n).unwrap().contains("серверов для опроса нет"));
        // только по нику
        let mut m = note_in();
        m.has_steam = false;
        m.public = None;
        assert!(track_note(&m).unwrap().contains("тёзка"));
        m.servers_ok = 0;
        assert!(track_note(&m).unwrap().contains("не отвечают"));
        // найден только по нику у открытого профиля
        let mut s = note_in();
        s.src = Some("server");
        assert!(track_note(&s).unwrap().contains("тёзка"));
        // скрытая активность (эвристика): только без серверного канала
        let mut h = note_in();
        h.tracked_since = 90 * MS_DAY;
        h.servers_ok = 0;
        assert!(track_note(&h).unwrap().contains("скрыта"));
        h.servers_ok = 1;
        assert_eq!(track_note(&h), None);
        // Steam недоступен
        let mut k = note_in();
        k.public = None;
        assert!(track_note(&k).unwrap().contains("данных Steam нет"));
    }

    #[test]
    fn session_transitions() {
        let mut v: Vec<Session> = vec![];
        assert!(!apply_presence(&mut v, false, "", 1000, 0, None));
        assert!(apply_presence(&mut v, true, "", 2000, 1000, None)); // вошёл
        assert_eq!(v, vec![Session { start: 2000, stop: None, srv: "".into() }]);
        assert!(apply_presence(&mut v, true, "1.1.1.1:1", 3000, 2000, None)); // уточнили сервер
        assert!(!apply_presence(&mut v, true, "1.1.1.1:1", 4000, 3000, None));
        assert!(apply_presence(&mut v, false, "", 5000, 4000, None)); // вышел: стоп = прошлый опрос
        assert_eq!(v[0].stop, Some(4000));
        assert_eq!(target_status(&v), (false, "1.1.1.1:1".into(), 4000));
        assert!(apply_presence(&mut v, true, "2.2.2.2:2", 6000, 5000, None));
        assert_eq!(v.len(), 2);
        assert_eq!(target_status(&v), (true, "2.2.2.2:2".into(), 6000));
        assert_eq!(target_status(&[]), (false, String::new(), 0));
    }

    #[test]
    fn sessions_merge_across_sources() {
        // A2S увидел раньше (since_hint), потом подключился Steam: сессия одна и не дробится
        let mut v: Vec<Session> = vec![];
        apply_presence(&mut v, true, "Srv", 10_000, 0, Some(4_000));
        assert_eq!(v[0].start, 4_000);
        assert!(!apply_presence(&mut v, true, "Srv", 20_000, 10_000, None));
        assert!(!apply_presence(&mut v, true, "Srv", 30_000, 20_000, Some(1)));
        assert_eq!(v.len(), 1);
        // start_hint из будущего не опережает now, а новая сессия не заходит на предыдущую
        apply_presence(&mut v, false, "", 40_000, 30_000, None);
        assert_eq!(v[0].stop, Some(30_000));
        apply_presence(&mut v, true, "Srv", 50_000, 40_000, Some(10_000));
        assert_eq!(v[1].start, 30_000);
        let mut w: Vec<Session> = vec![];
        apply_presence(&mut w, true, "", 100, 0, Some(999));
        assert_eq!(w[0].start, 100);
    }

    #[test]
    fn unfinished_session_closed_after_restart() {
        // приложение было закрыто: последний успешный опрос в 10:30, сейчас 15:00, игрок не в Rust
        let mut v = vec![ss(t(2024, 5, 1, 10, 0), None)];
        let last = t(2024, 5, 1, 10, 30);
        assert!(apply_presence(&mut v, false, "", t(2024, 5, 1, 15, 0), last, None));
        assert_eq!(v[0].stop, Some(last));
        // lastPoll раньше старта (или 0): длительность 0, не отрицательная
        let mut w = vec![ss(5000, None)];
        apply_presence(&mut w, false, "", 9000, 0, None);
        assert_eq!(w[0].stop, Some(5000));
        // игрок всё ещё в Rust: сессия продолжается без изменений
        let mut x = vec![Session { start: 1, stop: None, srv: "s".into() }];
        assert!(!apply_presence(&mut x, true, "s", 99, 50, None));
        assert_eq!(x[0].stop, None);
    }

    #[test]
    fn sessions_capped() {
        let mut v: Vec<Session> = (0..MAX_SESSIONS as i64).map(|i| ss(i * 10, Some(i * 10 + 5))).collect();
        apply_presence(&mut v, true, "", 999_999, 0, None);
        assert_eq!(v.len(), MAX_SESSIONS);
        assert_eq!(v[0].start, 10);
        assert!(v.last().unwrap().stop.is_none());
    }

    #[test]
    fn batching_by_100() {
        let ids: Vec<String> = (0..250).map(|i| format!("7656119{:010}", i)).collect();
        let c = chunk_ids(&ids);
        assert_eq!(c.len(), 3);
        assert_eq!(c[0].split(',').count(), 100);
        assert_eq!(c[1].split(',').count(), 100);
        assert_eq!(c[2].split(',').count(), 50);
        assert_eq!(chunk_ids(&ids[..100]).len(), 1);
        assert!(chunk_ids(&[]).is_empty());
    }

    #[test]
    fn backoff() {
        assert_eq!(next_backoff(0, None), POLL_SECS * 2);
        assert_eq!(next_backoff(POLL_SECS * 2, None), POLL_SECS * 4);
        assert_eq!(next_backoff(500, None), MAX_BACKOFF_SECS);
        assert_eq!(next_backoff(0, Some(5)), POLL_SECS);
        assert_eq!(next_backoff(0, Some(300)), 300);
    }

    #[test]
    fn targets_payload_shape() {
        let steam = TargetRec { label: "Abc".into(), steam_id: Some("7656119000000000A".into()), names: vec![], tracked_since: 5 };
        let nick = TargetRec { label: "Вася ".into(), steam_id: None, names: names(&["Вася"]), tracked_since: 6 };
        assert_eq!(nick.key(), "n:вася");
        let mut s = HashMap::new();
        s.insert(steam.key(), vec![ss(100, None)]);
        s.insert(nick.key(), vec![ss(10, Some(20))]);
        let mut n = HashMap::new();
        n.insert(steam.key(), "x".to_string());
        let mut src = HashMap::new();
        src.insert(steam.key(), "both");
        let p = targets_payload(&[steam.clone(), nick], &s, &n, &src);
        assert_eq!(p[0]["n"], "Abc");
        assert_eq!(p[0]["on"], true);
        assert_eq!(p[0]["srv"], "S");
        assert_eq!(p[0]["since"], 100);
        assert_eq!(p[0]["note"], "x");
        assert_eq!(p[0]["src"], "both");
        assert_eq!(p[1]["on"], false);
        assert!(p[1]["src"].is_null() && p[1]["steamId"].is_null());
        let p2 = targets_payload(&[steam], &HashMap::new(), &HashMap::new(), &HashMap::new());
        assert_eq!(p2[0]["on"], false);
        assert!(p2[0]["note"].is_null());
    }

    #[test]
    fn persistence_roundtrip() {
        let tr = vec![TargetRec { label: "L".into(), steam_id: Some("1".into()), names: names(&["a"]), tracked_since: 7 }];
        let s = serde_json::to_string(&tr).unwrap();
        assert!(s.contains("\"steamId\""));
        let back: Vec<TargetRec> = serde_json::from_str(&s).unwrap();
        assert_eq!(back, tr);
        // старый файл (без trackedSince/names) и цель без steamId читаются
        let old: Vec<TargetRec> = serde_json::from_str(r#"[{"label":"a","steamId":"2"},{"label":"b"}]"#).unwrap();
        assert_eq!(old[0].tracked_since, 0);
        assert!(old[1].steam_id.is_none() && old[1].names.is_empty());
        let f: SessionsFile = serde_json::from_str(r#"{"sessions":{"1":[{"start":1,"stop":null}]}}"#).unwrap();
        assert_eq!(f.sessions["1"][0].srv, "");
    }

    #[test]
    fn atomic_write_no_bom() {
        let dir = std::env::temp_dir().join(format!("hollow-bm-test-{}", now_ms()));
        let p = dir.join("sub").join("x.json");
        write_atomic(&p, b"[1]").unwrap();
        write_atomic(&p, "[\"я\"]".as_bytes()).unwrap();
        let b = std::fs::read(&p).unwrap();
        assert_ne!(&b[..3.min(b.len())], &[0xEF, 0xBB, 0xBF]);
        assert_eq!(read_json::<Vec<String>>(&p).unwrap(), vec!["я".to_string()]);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn aliases_real_format() {
        // формат подтверждён реальным запросом к публичному профилю
        let v = j(r#"[{"newname":"Robin","timechanged":"7 May, 2019 @ 11:04pm"},
            {"newname":"Sekiro","timechanged":"7 May, 2019 @ 9:13pm"},
            {"newname":"Sekiro","timechanged":"1 Jan, 2010 @ 12:00am"},
            {"newname":"New","timechanged":"3 Mar @ 12:30pm"}]"#);
        let l = parse_aliases(&v).unwrap();
        assert_eq!(l.len(), 4);
        assert_eq!(parse_aliases(&j("[]")).unwrap().len(), 0);
        assert!(parse_aliases(&j(r#"{"x":1}"#)).is_none());
        let (names, det) = aliases_from(&l, "robin", t(2024, 6, 1, 0, 0));
        assert_eq!(names, vec!["Sekiro", "New"]);
        assert_eq!(det[0]["lastSeen"], t(2019, 5, 7, 21, 13));
        assert_eq!(det[1]["lastSeen"], t(2024, 3, 3, 12, 30));
    }

    #[test]
    fn alias_time_parse() {
        assert_eq!(parse_alias_time("7 May, 2019 @ 11:04pm", 2000), Some(t(2019, 5, 7, 23, 4)));
        assert_eq!(parse_alias_time("1 Jan, 2010 @ 12:00am", 2000), Some(t(2010, 1, 1, 0, 0)));
        assert_eq!(parse_alias_time("26 Dec, 2013 @ 9:38pm", 2000), Some(t(2013, 12, 26, 21, 38)));
        assert_eq!(parse_alias_time("мусор", 2000), None);
        assert_eq!(parse_alias_time("7 Foo, 2019 @ 1:00pm", 2000), None);
    }

    #[test]
    fn weekday_index() {
        assert_eq!(weekday_of_day(0), 3); // четверг
        assert_eq!(weekday_of_day(t(2024, 5, 6, 0, 0) / MS_DAY), 0); // пн
        assert_eq!(weekday_of_day(t(2024, 5, 12, 0, 0) / MS_DAY), 6); // вс
    }

    #[test]
    fn slice_across_hours() {
        let mut v = vec![];
        slice_hours(t(2024, 5, 1, 10, 30), t(2024, 5, 1, 12, 15), |h, ms| v.push((h.rem_euclid(24), ms)));
        assert_eq!(v, vec![(10, 30 * 60_000), (11, MS_HOUR), (12, 15 * 60_000)]);
    }

    #[test]
    fn session_over_midnight_and_week_boundary() {
        // вс 2024-05-12 23:30 -> пн 2024-05-13 00:45
        let s = [ss(t(2024, 5, 12, 23, 30), Some(t(2024, 5, 13, 0, 45)))];
        let (m, days) = bucket_sessions(&s, 0);
        assert_eq!(to_min(m[6][23]), 30);
        assert_eq!(to_min(m[0][0]), 45);
        assert_eq!(days.len(), 2);
        let a = build_activity(&s, t(2024, 5, 14, 0, 0)).unwrap();
        assert_eq!(a["byWeekHour"][6][23], 30);
        assert_eq!(a["byWeekHour"][0][0], 45);
        assert_eq!(a["peak"], json!({"weekday": 0, "hour": 0, "minutes": 45}));
        assert_eq!(a["byDay"], json!([{"date":"2024-05-12","minutes":30},{"date":"2024-05-13","minutes":45}]));
        assert_eq!(a["sessions"], 1);
        assert_eq!(a["totalHours"], 1.3);
        assert_eq!(a["tzNote"], "utc");
        assert_eq!(a["days"], 1);
    }

    #[test]
    fn multi_day_session_and_empty() {
        let s = [ss(t(2024, 5, 1, 22, 0), Some(t(2024, 5, 3, 2, 0)))];
        let (m, days) = bucket_sessions(&s, 0);
        let total: i64 = m.iter().flatten().sum();
        assert_eq!(total, 28 * MS_HOUR);
        assert_eq!(days.len(), 3);
        assert!(build_activity(&[], 0).is_none());
        assert!(peak_of(&[[0; 24]; 7]).is_none());
    }

    #[test]
    fn open_session_counts_until_now() {
        let now = t(2024, 5, 2, 12, 0);
        let s = [ss(t(2024, 5, 2, 10, 0), None)];
        let a = build_activity(&s, now).unwrap();
        assert_eq!(a["totalHours"], 2.0);
    }

    #[test]
    fn by_day_window_is_365_days() {
        let now = t(2024, 5, 2, 12, 0);
        let s = [
            ss(t(2023, 1, 1, 10, 0), Some(t(2023, 1, 1, 11, 0))),
            ss(t(2024, 5, 1, 10, 0), Some(t(2024, 5, 1, 11, 0))),
        ];
        let a = build_activity(&s, now).unwrap();
        assert_eq!(a["byDay"], json!([{"date":"2024-05-01","minutes":60}]));
    }

    #[test]
    fn session_lines_latest_first() {
        let s = [ss(t(2024, 5, 1, 10, 0), Some(t(2024, 5, 1, 11, 0))), ss(t(2024, 5, 2, 10, 0), None)];
        let l = session_lines(&s);
        assert_eq!(l[0], "S · 2024-05-02 10:00 · онлайн");
        assert_eq!(l[1], "S · 2024-05-01 10:00 · 60 мин");
    }

    #[test]
    fn time_roundtrip() {
        assert_eq!(t(2000, 3, 1, 0, 0), 951868800000);
        assert_eq!(fmt_date(951868800000), "2000-03-01 00:00");
    }
}
