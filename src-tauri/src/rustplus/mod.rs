//! Клиент Rust+ companion API, бинарные кадры protobuf. Подключение через ретранслятор Facepunch
//! (wss://companion-rust.facepunch.com/socket/<relay_id>, см. `relay_request`), если у сервера есть
//! relay_id, иначе старый прямой ws://ip:port. Текстовые кадры ретранслятора пропускаются.
//! playerToken и токен Rust+ нигде не логируются и не уходят во фронт.

mod alerts;
mod forms;
mod proto;

use crate::types::ServerCreds;
use base64::Engine;
use futures_util::{SinkExt, StreamExt};
use prost::Message as _;
use proto::*;
use serde::Serialize;
use std::collections::{BTreeMap, HashMap, HashSet};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use tauri::{AppHandle, Emitter, Manager};
use tokio::sync::mpsc::{unbounded_channel, UnboundedReceiver, UnboundedSender};
use tokio::task::JoinHandle;
use tokio_tungstenite::tungstenite::Message as WsMessage;

const TEAM_EVERY: Duration = Duration::from_secs(5);
const MARKERS_EVERY: Duration = Duration::from_secs(10);
const INFO_EVERY: Duration = Duration::from_secs(30);
const TIME_EVERY: Duration = Duration::from_secs(60);
const MAX_PENDING: usize = 20;

enum Cmd {
    SetDevice(u32, bool),
    Chat(String),
    /// Подписаться на устройство (getEntityInfo): без этого сервер не шлёт его изменения.
    Watch(u32),
}

/// Последний состав команды из Rust+: (когда, SteamID -> в сети). Нужен профилю игрока:
/// Steam не показывает «в Rust» при закрытых игровых данных, а Rust+ про тимейтов знает точно.
static TEAM_SEEN: Mutex<Option<(i64, HashMap<String, (bool, String)>)>> = Mutex::new(None);
/// Снимок команды старше этого не доверяем (опрос каждые 5 с, значит соединение пропало).
const TEAM_FRESH_MS: i64 = 60_000;

/// В сети ли тимейт по данным Rust+. None — не тимейт или свежих данных нет.
pub fn team_online(steam_id: &str) -> Option<bool> {
    let g = TEAM_SEEN.lock().ok()?;
    let (at, m) = g.as_ref()?;
    if now_ms() - at > TEAM_FRESH_MS {
        return None;
    }
    m.get(steam_id).map(|(on, _)| *on)
}

/// Квадрат игрока команды по последнему снимку Rust+ (у погибшего — место смерти). None — нет данных.
pub fn member_grid(steam_id: &str) -> Option<String> {
    let g = TEAM_SEEN.lock().ok()?;
    let (at, m) = g.as_ref()?;
    if now_ms() - at > TEAM_FRESH_MS {
        return None;
    }
    m.get(steam_id).map(|(_, grid)| grid.clone()).filter(|s| !s.is_empty())
}

const ENTITY_SWITCH: i32 = 1;
const ENTITY_STORAGE: i32 = 3;

struct Conn {
    handle: JoinHandle<()>,
    tx: UnboundedSender<Cmd>,
    connected: Arc<AtomicBool>,
}

#[derive(Default)]
pub struct RpState {
    conn: Mutex<Option<Conn>>,
}

#[derive(Clone, Copy)]
enum Kind {
    Info(Instant),
    Time,
    Map,
    Team,
    Markers,
    Device(u32, bool),
    EntityInfo(u32),
    Other,
}

fn emit<S: Serialize + Clone>(app: &AppHandle, name: &str, payload: S) {
    let _ = app.emit(name, payload);
}

fn status(app: &AppHandle, state: &str, msg: &str) {
    log::info!("rust+: {state}{}{msg}", if msg.is_empty() { "" } else { " · " });
    emit(app, "rp_status", serde_json::json!({ "state": state, "msg": msg }));
}

fn now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

/// Состояние одной сессии (сбрасывается при переподключении).
struct Session {
    player_id: u64,
    token: i32,
    seq: u32,
    pending: HashMap<u32, Kind>,
    map_size: f32,
    map_loaded: bool,
    monuments: Vec<Monument>,
    markers: Vec<AppMarker>,
    have_markers: bool,
    last_team: Option<AppTeamInfo>,
    presence: HashMap<u64, (bool, i64)>,
    first_seen: HashMap<&'static str, i64>,
    devices: BTreeMap<u32, bool>,
    last_info: Option<(AppInfo, Option<u64>)>,
    // алерты
    team_snap: Option<Vec<alerts::MemberSnap>>,
    entity_types: HashMap<u32, i32>,
    info_requested: HashSet<u32>,
    want_info: Vec<u32>,
    pending_vals: HashMap<u32, bool>,
    alarm: alerts::AlarmTracker,
    raid: alerts::RaidWindow,
    decay_gate: alerts::DecayGate,
    decay: BTreeMap<u32, i64>,
    last_alarms: Option<serde_json::Value>,
    shops: alerts::ShopWatch,
}

fn forms_decay(m: &BTreeMap<u32, i64>, names: &HashMap<u32, String>) -> serde_json::Value {
    alerts::decay_form(m, now_ms() / 1000, names)
}

fn fmt_hhmm(ms: i64) -> String {
    use chrono::TimeZone;
    chrono::Local
        .timestamp_millis_opt(ms)
        .single()
        .map(|d| d.format("%H:%M").to_string())
        .unwrap_or_default()
}

/// Не блокирует цикл Rust+: отправка уходит в фоновую задачу.
fn fire(app: &AppHandle, alert: crate::types::Alert) {
    let app = app.clone();
    tauri::async_runtime::spawn(async move {
        crate::notify::dispatch(&app, alert).await;
    });
}

impl Session {
    fn new(player_id: u64, token: i32) -> Self {
        Self {
            player_id,
            token,
            seq: 0,
            pending: HashMap::new(),
            map_size: 0.0,
            map_loaded: false,
            monuments: Vec::new(),
            markers: Vec::new(),
            have_markers: false,
            last_team: None,
            presence: HashMap::new(),
            first_seen: HashMap::new(),
            devices: BTreeMap::new(),
            last_info: None,
            team_snap: None,
            entity_types: HashMap::new(),
            info_requested: HashSet::new(),
            want_info: Vec::new(),
            pending_vals: HashMap::new(),
            alarm: alerts::AlarmTracker::default(),
            raid: alerts::RaidWindow::default(),
            decay_gate: alerts::DecayGate::default(),
            decay: BTreeMap::new(),
            last_alarms: None,
            shops: alerts::ShopWatch::default(),
        }
    }

    /// Новый снимок команды: дифф с предыдущим (первый снимок молча запоминается), затем эмит.
    fn set_team(&mut self, app: &AppHandle, t: AppTeamInfo) {
        let next: Vec<alerts::MemberSnap> = t
            .members
            .iter()
            .map(|m| alerts::MemberSnap { steam_id: m.steam_id, name: m.name.clone(), online: m.is_online, alive: m.is_alive, x: m.x, y: m.y })
            .collect();
        let ms = self.map_size;
        let grid = move |x: f32, y: f32| {
            if ms <= 0.0 {
                return String::new();
            }
            let (u, v) = forms::uv(x, y, ms);
            forms::grid_of(u, v, ms)
        };
        for a in alerts::diff_team(self.team_snap.as_deref(), &next, &grid) {
            fire(app, a);
        }
        if let Ok(mut g) = TEAM_SEEN.lock() {
            *g = Some((now_ms(), t.members.iter().map(|m| (m.steam_id.to_string(), (m.is_online, grid(m.x, m.y)))).collect()));
        }
        self.team_snap = Some(next);
        self.last_team = Some(t);
        self.emit_team(app);
    }

    /// Значение неизвестной сущности: тип узнаём через getEntityInfo, алярм (тип 2) даёт срабатывание.
    fn on_unknown_value(&mut self, app: &AppHandle, id: u32, on: bool) {
        match self.entity_types.get(&id) {
            Some(&t) if t == alerts::ENTITY_ALARM => self.on_alarm_value(app, id, on),
            Some(_) => {}
            None => {
                self.pending_vals.insert(id, on);
                if self.info_requested.insert(id) {
                    self.want_info.push(id);
                }
            }
        }
    }

    fn on_alarm_value(&mut self, app: &AppHandle, id: u32, on: bool) {
        let now = now_ms();
        if self.alarm.on_value(id, on, now) {
            fire(app, alerts::alarm_alert(&alerts::label(&self.alarm.names, id, "Аларм")));
            if self.raid.record(now) {
                self.alarm.push_history(now, "Возможный рейд: серия срабатываний алярмов".into());
                fire(app, alerts::raid_alert(alerts::RAID_HITS));
            }
        }
        self.emit_alarms(app);
    }

    fn emit_alarms(&mut self, app: &AppHandle) {
        let now = now_ms();
        let cur = self.alarm.alarms_form(now);
        if self.last_alarms.as_ref() != Some(&cur) {
            emit(app, "alarms", cur.clone());
            emit(app, "alarm_history", self.alarm.history_form(&fmt_hhmm));
            self.last_alarms = Some(cur);
        }
    }

    /// Монитор шкафа: protectionExpiry (unix сек) -> decay-алерты и событие `decay`.
    fn on_protection(&mut self, app: &AppHandle, id: u32, p: &AppEntityPayload) {
        if !p.has_protection {
            if self.decay.remove(&id).is_some() {
                emit(app, "decay", forms_decay(&self.decay, &self.alarm.names));
            }
            return;
        }
        // защита есть, но expiry 0 — ресурсов на апкип нет, шкаф уже гниёт: это самый срочный случай,
        // а не «нет данных» (раньше такой шкаф молча пропадал из списка)
        let now_s = now_ms() / 1000;
        let exp = if p.protection_expiry == 0 { now_s } else { p.protection_expiry as i64 };
        self.decay.insert(id, exp);
        emit(app, "decay", forms_decay(&self.decay, &self.alarm.names));
        let left = exp - now_s;
        if let Some(a) = alerts::decay_alert(&alerts::label(&self.alarm.names, id, "Шкаф"), left) {
            if self.decay_gate.allow(id, &a.prio, now_ms()) {
                fire(app, a);
            }
        }
    }

    fn request(&mut self, kind: Kind, fill: impl FnOnce(&mut AppRequest)) -> WsMessage {
        self.seq = self.seq.wrapping_add(1).max(1);
        let mut req = AppRequest {
            seq: self.seq,
            player_id: self.player_id,
            player_token: self.token,
            ..Default::default()
        };
        fill(&mut req);
        self.pending.insert(self.seq, kind);
        WsMessage::Binary(req.encode_to_vec().into())
    }

    fn emit_team(&mut self, app: &AppHandle) {
        if self.map_size <= 0.0 {
            return;
        }
        if let Some(t) = self.last_team.clone() {
            let v = forms::team_form(&t, self.map_size, &mut self.presence, now_ms());
            emit(app, "team_info", v);
        }
    }

    /// Алерты по магазинам: новые предложения с предметами из настроек (notify.shop_watch).
    fn check_shops(&mut self, app: &AppHandle) {
        if self.map_size <= 0.0 {
            return; // без размера карты нет квадратов; не запоминаем снимок, дождёмся карты
        }
        let watch = app
            .try_state::<crate::profiles::ProfilesState>()
            .and_then(|s| s.active_profile())
            .map(|p| p.notify.shop_watch)
            .unwrap_or_default();
        let offers = forms::shop_offers(&self.markers, self.map_size);
        for a in self.shops.diff(&offers, &watch) {
            fire(app, a);
        }
    }

    fn emit_markers(&mut self, app: &AppHandle) {
        if self.map_size <= 0.0 || !self.have_markers {
            return;
        }
        emit(app, "markers", forms::markers_form(&self.markers, &self.monuments, self.map_size));
        emit(app, "vendings", forms::vendings_form(&self.markers, self.map_size));
        emit(app, "events", forms::events_form(&self.markers, &mut self.first_seen, now_ms()));
    }

    fn emit_info(&self, app: &AppHandle) {
        if let Some((info, ping)) = &self.last_info {
            emit(app, "server_info", forms::server_info_form(info, *ping));
        }
    }

    fn handle_response(&mut self, app: &AppHandle, r: AppResponse) {
        let kind = self.pending.remove(&r.seq).unwrap_or(Kind::Other);
        if let Some(e) = &r.error {
            log::warn!("rust+ error response: {}", e.error);
            if let Kind::Map = kind {
                self.map_loaded = false;
            }
            return;
        }
        if let Some(info) = r.info {
            let ping = match kind {
                Kind::Info(t) => Some(t.elapsed().as_millis() as u64),
                _ => None,
            };
            if info.map_size > 0 {
                self.map_size = info.map_size as f32;
            }
            self.last_info = Some((info, ping));
            self.emit_info(app);
            self.emit_team(app);
            self.emit_markers(app);
        }
        if let Some(map) = r.map {
            self.map_loaded = true;
            if !map.jpg_image.is_empty() {
                // картинка включает океанские поля (oceanMargin пикселей с каждой стороны): мир 0..mapSize
                // занимает только середину. Без поправки маркеры съезжают к центру — фронт обрезает поля.
                let b64 = base64::engine::general_purpose::STANDARD.encode(&map.jpg_image);
                emit(
                    app,
                    "map_image",
                    serde_json::json!({
                        "src": format!("data:image/jpeg;base64,{b64}"),
                        "w": map.width, "h": map.height, "margin": map.ocean_margin.max(0),
                    }),
                );
            }
            // размер мира берём только из getInfo: width/oceanMargin карты в пикселях, не в метрах
            self.monuments = map.monuments;
            self.emit_team(app);
            self.emit_markers(app);
        }
        if let Some(t) = r.team_info {
            self.set_team(app, t);
        }
        if let (Kind::EntityInfo(id), Some(ei)) = (kind, r.entity_info) {
            self.entity_types.insert(id, ei.r#type);
            let pend = self.pending_vals.remove(&id);
            match ei.r#type {
                ENTITY_SWITCH => {
                    // начальное состояние выключателя: дальше его меняют broadcast entityChanged
                    let on = ei.payload.as_ref().and_then(|p| p.value).or(pend).unwrap_or(false);
                    self.devices.insert(id, on);
                    emit(app, "devices", forms::devices_form(&self.devices));
                }
                ENTITY_STORAGE => {
                    if let Some(p) = ei.payload.as_ref() {
                        if let Some(v) = forms::storage_update_form(id, p) {
                            emit(app, "storage_update", v);
                        }
                        self.on_protection(app, id, p);
                    }
                }
                t if t == alerts::ENTITY_ALARM => {
                    if let Some(on) = pend {
                        self.on_alarm_value(app, id, on);
                    }
                }
                _ => {}
            }
        }
        if let Some(m) = r.map_markers {
            self.markers = m.markers;
            self.have_markers = true;
            self.emit_markers(app);
            self.check_shops(app);
        }
        if r.success.is_some() {
            if let Kind::Device(id, on) = kind {
                self.devices.insert(id, on);
                emit(app, "devices", forms::devices_form(&self.devices));
            }
        }
    }

    fn handle_broadcast(&mut self, app: &AppHandle, b: AppBroadcast) {
        if let Some(tc) = b.team_changed {
            if let Some(t) = tc.team_info {
                self.set_team(app, t);
            }
        }
        if let Some(tm) = b.team_message.and_then(|m| m.message) {
            emit(app, "team_chat", serde_json::json!({ "author": tm.name, "text": tm.message }));
        }
        if let Some(ec) = b.entity_changed {
            if let Some(p) = ec.payload {
                if let Some(v) = forms::storage_update_form(ec.entity_id, &p) {
                    emit(app, "storage_update", v);
                    self.on_protection(app, ec.entity_id, &p);
                } else if let Some(on) = p.value {
                    if let Some(s) = self.devices.get_mut(&ec.entity_id) {
                        *s = on;
                        emit(app, "devices", forms::devices_form(&self.devices));
                    } else {
                        self.on_unknown_value(app, ec.entity_id, on);
                    }
                }
            }
        }
    }
}

fn emit_empty_lists(app: &AppHandle) {
    for n in ["devices", "storage", "decay", "cameras", "shared_devices", "alarms", "alarm_history", "vendings", "events"] {
        emit(app, n, serde_json::json!([]));
    }
    emit(app, "merchant", serde_json::Value::Null);
}

/// Одна сессия на живом сокете. Возвращает причину завершения.
async fn run_session(
    app: &AppHandle,
    ws: tokio_tungstenite::WebSocketStream<tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>>,
    rx: &mut UnboundedReceiver<Cmd>,
    player_id: u64,
    token: i32,
) -> String {
    let (mut sink, mut stream) = ws.split();
    let mut s = Session::new(player_id, token);
    // сопряжённые устройства пачки: подписываемся на каждое (getEntityInfo), мониторы хранилищ — в «фарм»
    let paired = app
        .try_state::<crate::profiles::ProfilesState>()
        .and_then(|st| st.active_profile())
        .map(|p| p.devices)
        .unwrap_or_default();
    let boxes: Vec<serde_json::Value> = paired
        .iter()
        .filter(|d| d.kind == "storage")
        .map(|d| serde_json::json!({ "id": d.id, "name": d.name }))
        .collect();
    emit(app, "storage_boxes", boxes);
    s.alarm.names = paired
        .iter()
        .filter_map(|d| d.id.trim().parse::<u32>().ok().map(|id| (id, d.name.clone())))
        .collect();
    for d in &paired {
        if let Ok(id) = d.id.trim().parse::<u32>() {
            if s.info_requested.insert(id) {
                s.want_info.push(id);
            }
        }
    }
    let mk = |d| {
        let mut i = tokio::time::interval(d);
        i.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        i
    };
    let (mut t_team, mut t_markers, mut t_info, mut t_time) = (mk(TEAM_EVERY), mk(MARKERS_EVERY), mk(INFO_EVERY), mk(TIME_EVERY));

    loop {
        let out: Option<WsMessage> = tokio::select! {
            incoming = stream.next() => {
                match incoming {
                    None => return "соединение закрыто".into(),
                    Some(Err(e)) => return format!("ошибка сокета: {e}"),
                    Some(Ok(WsMessage::Binary(b))) => {
                        match AppMessage::decode(&b[..]) {
                            Ok(m) => {
                                if let Some(r) = m.response { s.handle_response(app, r); }
                                if let Some(b) = m.broadcast { s.handle_broadcast(app, b); }
                            }
                            Err(e) => log::warn!("rust+ decode error: {e}"),
                        }
                        None
                    }
                    Some(Ok(WsMessage::Close(_))) => return "сервер закрыл соединение".into(),
                    Some(Ok(_)) => None,
                }
            }
            cmd = rx.recv() => match cmd {
                None => return "остановлено".into(),
                Some(Cmd::SetDevice(id, on)) => Some(s.request(Kind::Device(id, on), |r| {
                    r.entity_id = Some(id);
                    r.set_entity_value = Some(AppSetEntityValue { value: on });
                })),
                Some(Cmd::Chat(text)) => Some(s.request(Kind::Other, |r| {
                    r.send_team_message = Some(AppSendMessage { message: text });
                })),
                Some(Cmd::Watch(id)) => {
                    s.info_requested.insert(id);
                    Some(s.request(Kind::EntityInfo(id), |r| {
                        r.entity_id = Some(id);
                        r.get_entity_info = Some(AppEmpty {});
                    }))
                }
            },
            _ = t_team.tick() => Some(s.request(Kind::Team, |r| r.get_team_info = Some(AppEmpty {}))),
            _ = t_markers.tick() => Some(s.request(Kind::Markers, |r| r.get_map_markers = Some(AppEmpty {}))),
            _ = t_time.tick() => Some(s.request(Kind::Time, |r| r.get_time = Some(AppEmpty {}))),
            _ = t_info.tick() => {
                // карту грузим один раз; если не удалось, повторяем на следующем тике
                if !s.map_loaded && !s.pending.values().any(|k| matches!(k, Kind::Map)) {
                    let m = s.request(Kind::Map, |r| r.get_map = Some(AppEmpty {}));
                    if sink.send(m).await.is_err() { return "ошибка записи".into(); }
                }
                Some(s.request(Kind::Info(Instant::now()), |r| r.get_info = Some(AppEmpty {})))
            }
        };
        if let Some(m) = out {
            if sink.send(m).await.is_err() {
                return "ошибка записи".into();
            }
        }
        // по 4 запроса за проход: у Rust+ лимит запросов, а пачка из 20+ устройств разом
        // упирается и в MAX_PENDING (тогда сессия рвётся с «сервер не отвечает»)
        let n = s.want_info.len().min(4);
        let batch: Vec<u32> = s.want_info.drain(..n).collect();
        for id in batch {
            let m = s.request(Kind::EntityInfo(id), |r| {
                r.entity_id = Some(id);
                r.get_entity_info = Some(AppEmpty {});
            });
            if sink.send(m).await.is_err() {
                return "ошибка записи".into();
            }
        }
        s.emit_alarms(app); // обновляет «N мин назад», эмитит только при изменении
        if s.pending.len() > MAX_PENDING {
            return "сервер не отвечает".into();
        }
    }
}

const RELAY_HOST: &str = "companion-rust.facepunch.com";
const RELAY_ORIGIN: &str = "https://app.playrust.com";
const RELAY_SUBPROTOCOL: &str = "rustplus.auth.v1";

/// base64url без паддинга (вторая подпротокольная строка — токен Rust+ в таком виде).
fn b64url(s: &str) -> String {
    base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(s.as_bytes())
}

/// Запрос рукопожатия к ретранслятору Facepunch (с обновления Rust 2026-11-05 сервера не принимают
/// прямой ws://ip:port). Формат взят из rustplusplus src/util/rustRelay.js:
/// wss://companion-rust.facepunch.com/socket/<relay_id>?v=<ms>, подпротоколы
/// ["rustplus.auth.v1", base64url(токен Rust+)], Origin https://app.playrust.com.
fn relay_request(relay_id: &str, auth_token: &str) -> Result<tokio_tungstenite::tungstenite::handshake::client::Request, String> {
    use tokio_tungstenite::tungstenite::client::IntoClientRequest;
    use tokio_tungstenite::tungstenite::http::HeaderValue;
    let ms = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_millis()).unwrap_or(0);
    let url = format!("wss://{RELAY_HOST}/socket/{relay_id}?v={ms}");
    let mut req = url.into_client_request().map_err(|e| format!("адрес ретранслятора: {e}"))?;
    let proto = format!("{RELAY_SUBPROTOCOL}, {}", b64url(auth_token));
    let h = req.headers_mut();
    h.insert("Sec-WebSocket-Protocol", HeaderValue::from_str(&proto).map_err(|_| "токен Rust+ содержит недопустимые символы".to_string())?);
    h.insert("Origin", HeaderValue::from_static(RELAY_ORIGIN));
    Ok(req)
}

/// Понятная причина для отказа ретранслятора: 401/403 — почти всегда протухший токен Rust+.
fn relay_reason(e: &str) -> String {
    if e.contains("401") || e.contains("403") {
        "ретранслятор Facepunch отклонил токен Rust+ (истёк?): нажми «Подключить через Steam» ещё раз".into()
    } else {
        format!("не удалось подключиться через ретранслятор: {e}")
    }
}

async fn supervisor(app: AppHandle, creds: ServerCreds, player_id: u64, token: i32, mut rx: UnboundedReceiver<Cmd>, connected: Arc<AtomicBool>) {
    let relay = !creds.relay_id.trim().is_empty();
    let direct_url = format!("ws://{}:{}", creds.ip, creds.port);
    let mut attempt: u32 = 0;
    loop {
        status(&app, "connecting", "");
        let res = if relay {
            // токен перечитываем на каждой попытке: после повторного входа через Steam он новый
            let Some(auth) = crate::pairing::auth_token(&app) else {
                status(&app, "error", "нет токена Rust+: нажми «Подключить через Steam»");
                tokio::time::sleep(Duration::from_secs(10)).await;
                continue;
            };
            let req = match relay_request(creds.relay_id.trim(), &auth) {
                Ok(r) => r,
                Err(e) => {
                    status(&app, "error", &e);
                    return;
                }
            };
            tokio::time::timeout(Duration::from_secs(15), tokio_tungstenite::connect_async(req)).await
        } else {
            tokio::time::timeout(Duration::from_secs(10), tokio_tungstenite::connect_async(direct_url.as_str())).await
        };
        let reason = match res {
            Ok(Ok((ws, _))) => {
                attempt = 0;
                connected.store(true, Ordering::SeqCst);
                status(&app, "connected", "");
                emit_empty_lists(&app);
                let r = run_session(&app, ws, &mut rx, player_id, token).await;
                connected.store(false, Ordering::SeqCst);
                if r == "остановлено" {
                    return;
                }
                r
            }
            Ok(Err(e)) if relay => relay_reason(&e.to_string()),
            Ok(Err(e)) => format!("не удалось подключиться: {e}"),
            Err(_) => "таймаут подключения".to_string(),
        };
        status(&app, "error", &reason);
        let delay = (1u64 << attempt.min(5)).min(30);
        attempt = attempt.saturating_add(1);
        tokio::time::sleep(Duration::from_secs(delay)).await;
    }
}

pub async fn connect(app: AppHandle, creds: ServerCreds) -> Result<(), String> {
    disconnect(&app).await;
    let player_id: u64 = creds.player_id.trim().parse().map_err(|_| "некорректный SteamID".to_string())?;
    let token: i32 = creds.player_token.trim().parse().map_err(|_| "некорректный playerToken".to_string())?;
    if creds.relay_id.trim().is_empty() && (creds.ip.trim().is_empty() || creds.port == 0) {
        return Err("не заданы ip/порт сервера".into());
    }
    let (tx, rx) = unbounded_channel();
    let connected = Arc::new(AtomicBool::new(false));
    let handle = tokio::spawn(supervisor(app.clone(), creds, player_id, token, rx, connected.clone()));
    let st = app.state::<RpState>();
    *st.conn.lock().unwrap() = Some(Conn { handle, tx, connected });
    Ok(())
}

pub async fn disconnect(app: &AppHandle) {
    let old = app.state::<RpState>().conn.lock().unwrap().take();
    if let Some(c) = old {
        c.handle.abort();
        status(app, "closed", "");
    }
}

fn send_cmd(app: &AppHandle, cmd: Cmd) -> Result<(), String> {
    let st = app.state::<RpState>();
    let g = st.conn.lock().unwrap();
    match g.as_ref() {
        Some(c) if c.connected.load(Ordering::SeqCst) => c.tx.send(cmd).map_err(|_| "нет соединения".to_string()),
        _ => Err("нет соединения с сервером".into()),
    }
}

/// Подписка на только что сопряжённое устройство (из pairing). Без соединения — тихо ничего:
/// при следующем подключении сессия сама подпишется на все устройства пачки.
pub fn watch_entity(app: &AppHandle, id: &str) {
    if let Ok(id) = id.trim().parse::<u32>() {
        let _ = send_cmd(app, Cmd::Watch(id));
    }
}

#[tauri::command]
pub async fn rp_connect(app: AppHandle) -> Result<(), String> {
    let creds = app
        .state::<crate::profiles::ProfilesState>()
        .active_server()
        .ok_or_else(|| "у активной пачки не задан сервер".to_string())?;
    connect(app, creds).await
}

#[tauri::command]
pub async fn rp_disconnect(app: AppHandle) -> Result<(), String> {
    disconnect(&app).await;
    Ok(())
}

#[tauri::command]
pub async fn set_device(app: AppHandle, id: String, on: bool) -> Result<(), String> {
    let id: u32 = id.trim().parse().map_err(|_| "некорректный id устройства".to_string())?;
    send_cmd(&app, Cmd::SetDevice(id, on))
}

#[tauri::command]
pub async fn send_chat(app: AppHandle, text: String) -> Result<(), String> {
    let text = text.trim().to_string();
    if text.is_empty() {
        return Ok(());
    }
    send_cmd(&app, Cmd::Chat(text))
}

#[cfg(test)]
mod relay_tests {
    use super::*;

    #[test]
    fn relay_handshake_shape() {
        let req = relay_request("8c1f0e2a-relay", "eyJzdGVhbUlkIjoiMSJ9.sig+/=").unwrap();
        let uri = req.uri().to_string();
        assert!(uri.starts_with("wss://companion-rust.facepunch.com/socket/8c1f0e2a-relay?v="), "{uri}");
        let proto = req.headers().get("Sec-WebSocket-Protocol").unwrap().to_str().unwrap();
        assert_eq!(proto, format!("rustplus.auth.v1, {}", b64url("eyJzdGVhbUlkIjoiMSJ9.sig+/=")));
        assert!(!proto.contains('+') && !proto.contains('/') && !proto.contains('='));
        assert_eq!(req.headers().get("Origin").unwrap(), "https://app.playrust.com");
    }

    #[test]
    fn relay_reason_hints_token() {
        assert!(relay_reason("HTTP error: 401 Unauthorized").contains("Подключить через Steam"));
        assert!(relay_reason("io error").starts_with("не удалось подключиться через ретранслятор"));
    }
}
#[cfg(test)]
mod live_probe {
    use super::*;

    /// Разовая живая проверка маркеров: HOLLOW_PROBE=1 cargo test --lib live_markers -- --ignored --nocapture
    #[tokio::test]
    #[ignore]
    async fn live_markers() {
        if std::env::var("HOLLOW_PROBE").is_err() { return; }
        let _ = rustls::crypto::ring::default_provider().install_default();
        let dir = std::path::PathBuf::from(std::env::var("APPDATA").unwrap()).join("com.hollow.panel");
        let p: serde_json::Value = serde_json::from_slice(&std::fs::read(dir.join("profiles.json")).unwrap()).unwrap();
        let act = p["active"].as_str().unwrap_or("");
        let prof = p["profiles"].as_array().unwrap().iter().find(|x| x["id"].as_str() == Some(act)).unwrap();
        let s = &prof["server"];
        let pair: serde_json::Value = serde_json::from_slice(&std::fs::read(dir.join("pairing.json")).unwrap()).unwrap();
        let auth = pair["auth_token"].as_str().unwrap().trim().to_string();
        let req = relay_request(s["relay_id"].as_str().unwrap().trim(), &auth).unwrap();
        let (mut ws, _) = tokio_tungstenite::connect_async(req).await.unwrap();
        let pid: u64 = s["player_id"].as_str().unwrap().parse().unwrap();
        let tok: i32 = s["player_token"].as_str().unwrap().parse().unwrap();
        for (seq, info) in [(1u32, true), (2, false)] {
            let mut r = AppRequest { seq, player_id: pid, player_token: tok, ..Default::default() };
            if info { r.get_info = Some(AppEmpty {}); } else { r.get_map_markers = Some(AppEmpty {}); }
            ws.send(WsMessage::Binary(r.encode_to_vec().into())).await.unwrap();
        }
        let mut got = 0;
        while got < 2 {
            let Some(Ok(WsMessage::Binary(b))) = tokio::time::timeout(Duration::from_secs(15), ws.next()).await.unwrap() else { continue };
            let m = AppMessage::decode(&b[..]).unwrap();
            let Some(r) = m.response else { continue };
            got += 1;
            if let Some(e) = r.error { println!("error: {}", e.error); }
            if let Some(i) = r.info { println!("map_size={}", i.map_size); }
            if let Some(mm) = r.map_markers {
                let mut h = std::collections::BTreeMap::new();
                for x in &mm.markers { *h.entry(x.r#type).or_insert(0) += 1; }
                println!("marker types: {h:?}");
                for x in mm.markers.iter().filter(|x| x.r#type != 3 && x.r#type != 1) {
                    println!("  type={} x={:.0} y={:.0} name={:?} radius={}", x.r#type, x.x, x.y, x.name, x.radius);
                }
            }
        }
    }
}
