//! Аккаунт-наблюдатель: отдельный Steam-аккаунт, в друзья которого добавлены цели.
//! Steam показывает друзьям игру и сервер, даже когда игровые данные скрыты в режиме «Только друзья»
//! (Web API по ключу отдаёт только публичный вид, поэтому нужен вход как настоящий клиент).
//! Протокол — крейт `steam-vent` (клиентское соединение с CM-серверами Steam).
//!
//! Вход: логин + пароль один раз, затем Steam Guard: код из почты/приложения вводится в окне hollow
//! или вход подтверждается в мобильном Steam. Дальше хранится только refresh-токен (`watcher.json`
//! в каталоге данных приложения, не в репозитории), пароль нигде не сохраняется.
//! Токен и пароль не логируются и не уходят во фронт.
//!
//! Данные: CMsgClientFriendsList (кто в друзьях) и CMsgClientPersonaState (статус, игра, сервер).
//! Наружу — `presence(steam_id)`: что видит наблюдатель про этого друга.

use serde::{Deserialize, Serialize};
use serde_json::json;
use std::collections::HashMap;
use std::net::Ipv4Addr;
use std::sync::Mutex;
use std::time::Duration;
use steam_vent::auth::{
    AuthConfirmationHandler, DeviceConfirmationHandler, FileGuardDataStore, UserProvidedAuthConfirmationHandler,
};
use steam_vent::{Connection, ConnectionError, ConnectionTrait, LoginError, ServerList};
use steam_vent_proto_steam::steammessages_clientserver_friends::{
    CMsgClientAddFriend, CMsgClientChangeStatus, CMsgClientFriendsList, CMsgClientPersonaState,
    CMsgClientRequestFriendData,
};
use tauri::{AppHandle, Emitter, Manager};
use tokio::io::{AsyncReadExt, AsyncWriteExt, DuplexStream};
use tokio_stream::StreamExt;

const RUST_APPID: u64 = 252490;
/// Статус, игра, имя, присутствие, last seen, доп. инфо об игре, rich presence.
const PERSONA_FLAGS: u32 = 1 | 2 | 16 | 64 | 256 | 4096;
/// EFriendRelationship::Friend
const REL_FRIEND: u32 = 3;
/// Мы отправили заявку, ждём ответа.
const REL_REQUEST_SENT: u32 = 4;
const REFRESH_EVERY: Duration = Duration::from_secs(60);

#[derive(Clone, Debug, Default, PartialEq, Serialize)]
pub struct FriendSeen {
    pub steam_on: bool,
    pub in_rust: bool,
    /// "ip:port" игрового сервера, если Steam его прислал.
    pub server: String,
    /// Текст rich presence (если игра его выставила), запасная подпись.
    pub status: String,
    pub name: String,
}

#[derive(Default)]
struct Shared {
    conn: Option<Connection>,
    online: bool,
    /// SteamID -> EFriendRelationship
    rel: HashMap<u64, u32>,
    seen: HashMap<u64, FriendSeen>,
    task: Option<tauri::async_runtime::JoinHandle<()>>,
    /// Куда писать код Steam Guard (конец duplex, второй читает обработчик steam-vent).
    code_tx: Option<DuplexStream>,
    account: String,
    state: String,
    msg: String,
    guard: String,
}

static S: Mutex<Option<Shared>> = Mutex::new(None);

fn with<R>(f: impl FnOnce(&mut Shared) -> R) -> R {
    let mut g = S.lock().unwrap_or_else(|e| e.into_inner());
    f(g.get_or_insert_with(Shared::default))
}

#[derive(Serialize, Deserialize, Default)]
struct Saved {
    account: String,
    token: String,
}

fn data_dir(app: &AppHandle) -> Option<std::path::PathBuf> {
    app.path().app_data_dir().ok()
}

fn load_saved(app: &AppHandle) -> Option<Saved> {
    let t = std::fs::read_to_string(data_dir(app)?.join("watcher.json")).ok()?;
    let s: Saved = serde_json::from_str(t.trim_start_matches('\u{feff}')).ok()?;
    (!s.account.is_empty() && !s.token.is_empty()).then_some(s)
}

fn store_saved(app: &AppHandle, s: Option<&Saved>) {
    let Some(dir) = data_dir(app) else { return };
    let p = dir.join("watcher.json");
    match s {
        Some(s) => {
            let _ = std::fs::create_dir_all(&dir);
            let tmp = p.with_extension("json.tmp");
            if std::fs::write(&tmp, serde_json::to_string(s).unwrap_or_default()).is_ok() {
                let _ = std::fs::rename(&tmp, &p);
            }
        }
        None => {
            let _ = std::fs::remove_file(p);
        }
    }
}

/// Состояние для фронта: off | connecting | guard | online | error.
fn emit_status(app: &AppHandle) {
    let v = with(|s| {
        let friends = s.rel.values().filter(|r| **r == REL_FRIEND).count();
        let in_rust = s.seen.iter().filter(|(id, f)| f.in_rust && s.rel.get(id) == Some(&REL_FRIEND)).count();
        json!({"state": if s.state.is_empty() { "off" } else { s.state.as_str() },
               "account": s.account, "msg": s.msg, "guard": s.guard,
               "friends": friends, "inRust": in_rust})
    });
    let _ = app.emit("watcher_status", v);
}

fn set_state(app: &AppHandle, state: &str, msg: &str) {
    log::info!("наблюдатель: {state}{}{msg}", if msg.is_empty() { "" } else { " · " });
    with(|s| {
        s.state = state.into();
        s.msg = msg.into();
        if state != "guard" {
            s.guard.clear();
        }
        if state != "online" {
            s.online = false;
        }
    });
    emit_status(app);
}

/// Что наблюдатель видит про этого игрока. None — не друг, нет соединения или данных ещё нет.
pub fn presence(steam_id: &str) -> Option<FriendSeen> {
    let id: u64 = steam_id.parse().ok()?;
    with(|s| {
        if !s.online || s.rel.get(&id) != Some(&REL_FRIEND) {
            return None;
        }
        s.seen.get(&id).cloned()
    })
}

/// Отношение наблюдателя к игроку для профиля: friend | pending | none | off.
pub fn relation(steam_id: &str) -> &'static str {
    let Ok(id) = steam_id.parse::<u64>() else { return "none" };
    with(|s| {
        if !s.online {
            return "off";
        }
        match s.rel.get(&id) {
            Some(&REL_FRIEND) => "friend",
            Some(&REL_REQUEST_SENT) => "pending",
            _ => "none",
        }
    })
}

/// Разбор одного друга из CMsgClientPersonaState (чистая функция, покрыта тестами).
fn seen_from(
    persona_state: u32,
    app_id: u32,
    gameid: u64,
    ip: u32,
    port: u32,
    name: &str,
    status: &str,
) -> FriendSeen {
    let in_rust = app_id as u64 == RUST_APPID || gameid == RUST_APPID;
    let server = if in_rust && ip != 0 && port != 0 { format!("{}:{}", Ipv4Addr::from(ip), port) } else { String::new() };
    FriendSeen {
        steam_on: persona_state > 0 || in_rust,
        in_rust,
        server,
        status: if in_rust { status.trim().to_string() } else { String::new() },
        name: name.to_string(),
    }
}

fn on_persona(app: &AppHandle, p: CMsgClientPersonaState) {
    with(|s| {
        for f in &p.friends {
            let id = f.friendid();
            let prev = s.seen.get(&id).cloned().unwrap_or_default();
            // в инкрементальных обновлениях Steam шлёт только изменённые поля: недостающее берём из прошлого
            let name = if f.has_player_name() { f.player_name().to_string() } else { prev.name.clone() };
            let status = f
                .rich_presence
                .iter()
                .find(|kv| kv.key() == "status" || kv.key() == "steam_display")
                .map(|kv| kv.value().to_string())
                .unwrap_or_default();
            let ps = if f.has_persona_state() { f.persona_state() } else { prev.steam_on as u32 };
            let seen = seen_from(ps, f.game_played_app_id(), f.gameid(), f.game_server_ip(), f.game_server_port(), &name, &status);
            s.seen.insert(id, seen);
        }
    });
    emit_status(app);
}

fn on_friends(app: &AppHandle, l: CMsgClientFriendsList) -> Vec<u64> {
    let ids = with(|s| {
        if !l.bincremental() {
            s.rel.clear();
        }
        for f in &l.friends {
            let r = f.efriendrelationship();
            if r == 0 {
                s.rel.remove(&f.ulfriendid());
            } else {
                s.rel.insert(f.ulfriendid(), r);
            }
        }
        s.rel.iter().filter(|(_, r)| **r == REL_FRIEND).map(|(id, _)| *id).collect::<Vec<_>>()
    });
    emit_status(app);
    ids
}

async fn request_data(conn: &Connection, ids: Vec<u64>) -> Result<(), String> {
    if ids.is_empty() {
        return Ok(());
    }
    conn.send(CMsgClientRequestFriendData { persona_state_requested: Some(PERSONA_FLAGS), friends: ids, ..Default::default() })
        .await
        .map_err(|e| format!("Steam: {e}"))
}

/// Работа одного соединения до его обрыва.
async fn session(app: &AppHandle, conn: Connection) -> Result<(), String> {
    let mut personas = conn.on::<CMsgClientPersonaState>();
    let mut friends = conn.on::<CMsgClientFriendsList>();
    // «в сети» — иначе Steam не присылает статусы друзей
    conn.send(CMsgClientChangeStatus { persona_state: Some(1), ..Default::default() })
        .await
        .map_err(|e| format!("Steam: {e}"))?;
    with(|s| {
        s.conn = Some(conn.clone());
        s.online = true;
    });
    set_state(app, "online", "");
    let mut tick = tokio::time::interval(REFRESH_EVERY);
    tick.tick().await;
    loop {
        tokio::select! {
            p = personas.next() => match p {
                Some(Ok(p)) => on_persona(app, p),
                Some(Err(_)) => {}
                None => return Err("Steam закрыл соединение".into()),
            },
            f = friends.next() => match f {
                Some(Ok(l)) => { let ids = on_friends(app, l); request_data(&conn, ids).await?; }
                Some(Err(_)) => {}
                None => return Err("Steam закрыл соединение".into()),
            },
            _ = tick.tick() => {
                let ids = with(|s| s.rel.iter().filter(|(_, r)| **r == REL_FRIEND).map(|(id, _)| *id).collect::<Vec<_>>());
                request_data(&conn, ids).await?;
            }
        }
    }
}

fn human(e: &ConnectionError) -> String {
    match e {
        ConnectionError::LoginError(LoginError::InvalidCredentials) => "неверный логин или пароль".into(),
        ConnectionError::LoginError(LoginError::RateLimited) => "Steam ограничил попытки входа, подожди и попробуй позже".into(),
        ConnectionError::LoginError(LoginError::SteamGuardRequired) => "нужен код Steam Guard".into(),
        ConnectionError::Aborted => "вход отменён".into(),
        ConnectionError::UnsupportedConfirmationAction(_) => "этот способ подтверждения входа не поддерживается".into(),
        ConnectionError::Network(_) => "нет связи со Steam".into(),
        _ => "Steam отказал во входе".into(),
    }
}

/// Сессия по сохранённому токену с переподключением. Токен отозван — просим войти заново.
async fn supervise(app: AppHandle, saved: Saved) {
    let mut backoff = 10u64;
    loop {
        set_state(&app, "connecting", "");
        let r = match ServerList::discover().await {
            Ok(list) => Connection::access(&list, &saved.account, &saved.token).await,
            Err(_) => {
                set_state(&app, "error", "нет связи со Steam, повтор");
                tokio::time::sleep(Duration::from_secs(backoff)).await;
                backoff = (backoff * 2).min(300);
                continue;
            }
        };
        match r {
            Ok(conn) => {
                backoff = 10;
                let e = session(&app, conn).await.err().unwrap_or_default();
                with(|s| {
                    s.conn = None;
                    s.online = false;
                });
                set_state(&app, "connecting", &e);
            }
            Err(ConnectionError::LoginError(_)) | Err(ConnectionError::AccessToken(_)) => {
                store_saved(&app, None);
                set_state(&app, "error", "сессия Steam истекла, войди в аккаунт-наблюдатель заново");
                return;
            }
            Err(e) => {
                set_state(&app, "error", &format!("{}, повтор", human(&e)));
            }
        }
        tokio::time::sleep(Duration::from_secs(backoff)).await;
        backoff = (backoff * 2).min(300);
    }
}

fn stop_task() {
    with(|s| {
        if let Some(t) = s.task.take() {
            t.abort();
        }
        s.conn = None;
        s.online = false;
        s.code_tx = None;
        s.rel.clear();
        s.seen.clear();
    });
}

/// Запуск при старте приложения, если вход уже был.
pub fn start(app: &AppHandle) {
    let Some(saved) = load_saved(app) else { return };
    stop_task();
    with(|s| s.account = saved.account.clone());
    let a = app.clone();
    let t = tauri::async_runtime::spawn(async move { supervise(a, saved).await });
    with(|s| s.task = Some(t));
}

/// Первый вход: логин + пароль, затем Steam Guard (код в hollow или подтверждение в мобильном Steam).
#[tauri::command]
pub async fn watcher_login(app: AppHandle, account: String, password: String) -> Result<(), String> {
    let account = account.trim().to_string();
    if account.is_empty() || password.is_empty() {
        return Err("введи логин и пароль аккаунта-наблюдателя".into());
    }
    stop_task();
    with(|s| s.account = account.clone());
    let guard_path = data_dir(&app).ok_or("нет каталога данных приложения")?.join("watcher_guard.json");
    // обработчик steam-vent читает код строкой из `code_rx` и пишет, что за код нужен, в `prompt_tx`
    let (code_tx, code_rx) = tokio::io::duplex(256);
    let (prompt_tx, mut prompt_rx) = tokio::io::duplex(1024);
    with(|s| s.code_tx = Some(code_tx));
    let a = app.clone();
    let prompt_task = tauri::async_runtime::spawn(async move {
        let mut buf = vec![0u8; 1024];
        if let Ok(n) = prompt_rx.read(&mut buf).await {
            let text = String::from_utf8_lossy(&buf[..n]).to_lowercase();
            let kind = if text.starts_with("email") { "email" } else { "device" };
            with(|s| s.guard = kind.into());
            with(|s| s.state = "guard".into());
            emit_status(&a);
        }
    });
    set_state(&app, "connecting", "");
    let handler = UserProvidedAuthConfirmationHandler::new(code_rx, prompt_tx).or(DeviceConfirmationHandler);
    let list = ServerList::discover().await.map_err(|_| {
        set_state(&app, "error", "нет связи со Steam");
        "нет связи со Steam".to_string()
    })?;
    let r = Connection::login(&list, &account, &password, FileGuardDataStore::new(guard_path), handler).await;
    prompt_task.abort();
    with(|s| s.code_tx = None);
    let conn = match r {
        Ok(c) => c,
        Err(e) => {
            let m = human(&e);
            set_state(&app, "error", &m);
            return Err(m);
        }
    };
    let token = conn.access_token().unwrap_or_default().to_string();
    if !token.is_empty() {
        store_saved(&app, Some(&Saved { account: account.clone(), token: token.clone() }));
    }
    // эта сессия уже открыта — работаем в ней, при обрыве supervise переподключится по токену
    let a = app.clone();
    let t = tauri::async_runtime::spawn(async move {
        let e = session(&a, conn).await.err().unwrap_or_default();
        with(|s| {
            s.conn = None;
            s.online = false;
        });
        set_state(&a, "connecting", &e);
        if !token.is_empty() {
            supervise(a, Saved { account, token }).await;
        }
    });
    with(|s| s.task = Some(t));
    Ok(())
}

/// Код Steam Guard из почты или мобильного приложения.
#[tauri::command]
pub async fn watcher_code(code: String) -> Result<(), String> {
    let code = code.trim().to_uppercase();
    if code.is_empty() {
        return Err("пустой код".into());
    }
    let tx = with(|s| s.code_tx.take()).ok_or("код сейчас не запрашивается")?;
    let mut tx = tx;
    tx.write_all(format!("{code}\n").as_bytes()).await.map_err(|_| "не удалось передать код".to_string())?;
    with(|s| s.code_tx = Some(tx));
    Ok(())
}

#[tauri::command]
pub async fn watcher_logout(app: AppHandle) -> Result<(), String> {
    stop_task();
    store_saved(&app, None);
    with(|s| s.account.clear());
    set_state(&app, "off", "");
    Ok(())
}

#[tauri::command]
pub async fn watcher_status(app: AppHandle) -> Result<(), String> {
    emit_status(&app);
    Ok(())
}

/// Заявка в друзья от наблюдателя (цель должна принять).
#[tauri::command]
pub async fn watcher_add_friend(app: AppHandle, steam_id: String) -> Result<(), String> {
    let id: u64 = steam_id.trim().parse().map_err(|_| "нужен SteamID64".to_string())?;
    let conn = with(|s| s.conn.clone()).ok_or("аккаунт-наблюдатель не подключён (Настройки)")?;
    conn.send(CMsgClientAddFriend { steamid_to_add: Some(id), ..Default::default() })
        .await
        .map_err(|e| format!("Steam: {e}"))?;
    with(|s| {
        s.rel.entry(id).or_insert(REL_REQUEST_SENT);
    });
    emit_status(&app);
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rust_with_server() {
        // 185.207.214.233 = 0xB9CFD6E9
        let f = seen_from(1, 252490, 0, 0xB9CF_D6E9, 36000, "John", "");
        assert!(f.in_rust && f.steam_on);
        assert_eq!(f.server, "185.207.214.233:36000");
    }

    #[test]
    fn rust_by_gameid_without_server() {
        let f = seen_from(1, 0, 252490, 0, 0, "x", "Playing");
        assert!(f.in_rust);
        assert_eq!(f.server, "");
        assert_eq!(f.status, "Playing");
    }

    #[test]
    fn other_game_and_offline() {
        let f = seen_from(1, 730, 730, 1, 1, "x", "CS");
        assert!(f.steam_on && !f.in_rust && f.server.is_empty() && f.status.is_empty());
        let off = seen_from(0, 0, 0, 0, 0, "x", "");
        assert!(!off.steam_on && !off.in_rust);
    }
}
