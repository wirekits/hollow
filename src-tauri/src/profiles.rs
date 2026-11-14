use crate::types::{Profile, ServerCreds};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::path::PathBuf;
use std::sync::Mutex;
use tauri::{AppHandle, Emitter, Manager, State};

const MASK: &str = "•••";

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct ProfilesData {
    pub active: Option<String>,
    pub profiles: Vec<Profile>,
}

#[derive(Default)]
struct Inner {
    data: ProfilesData,
    path: Option<PathBuf>,
}

#[derive(Default)]
pub struct ProfilesState {
    inner: Mutex<Inner>,
}

#[derive(Serialize)]
pub struct ProfilesDto {
    pub active: Option<String>,
    pub profiles: Vec<Profile>,
}

fn new_profile(name: &str) -> Profile {
    Profile {
        id: uuid::Uuid::new_v4().to_string(),
        name: name.to_string(),
        server: None,
        bm_token: None,
        ..Default::default()
    }
}

fn save(inner: &Inner) -> Result<(), String> {
    let path = inner.path.as_ref().ok_or("хранилище профилей не инициализировано")?;
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir).map_err(|e| e.to_string())?;
    }
    let json = serde_json::to_vec_pretty(&inner.data).map_err(|e| e.to_string())?;
    let tmp = path.with_extension("json.tmp");
    std::fs::write(&tmp, json).map_err(|e| e.to_string())?;
    if let Err(e) = std::fs::rename(&tmp, path) {
        // на Windows rename поверх существующего может не сработать
        let _ = std::fs::remove_file(path);
        std::fs::rename(&tmp, path).map_err(|_| e.to_string())?;
    }
    Ok(())
}

impl ProfilesState {
    /// Загрузка profiles.json из каталога данных приложения; создаёт «Моя пачка» при первом запуске.
    pub fn load(&self, dir: &std::path::Path) {
        let path = dir.join("profiles.json");
        let mut g = self.inner.lock().unwrap();
        g.path = Some(path.clone());
        let mut data: ProfilesData = std::fs::read(&path)
            .ok()
            .and_then(|b| serde_json::from_slice(&b).ok())
            .unwrap_or_default();
        let mut dirty = false;
        if data.profiles.is_empty() {
            let p = new_profile("Моя пачка");
            data.active = Some(p.id.clone());
            data.profiles.push(p);
            dirty = true;
        }
        let ok = data
            .active
            .as_ref()
            .map(|a| data.profiles.iter().any(|p| &p.id == a))
            .unwrap_or(false);
        if !ok {
            data.active = Some(data.profiles[0].id.clone());
            dirty = true;
        }
        g.data = data;
        if dirty {
            let _ = save(&g);
        }
    }

    /// Полная (немаскированная) копия активной пачки. Только для бэкенда, во фронт не отдавать.
    pub fn active_profile(&self) -> Option<Profile> {
        let g = self.inner.lock().unwrap();
        let id = g.data.active.as_ref()?;
        g.data.profiles.iter().find(|p| &p.id == id).cloned()
    }

    /// Записать данные сервера активной пачки (pairing). Возвращает id пачки.
    pub fn set_active_server(&self, creds: ServerCreds) -> Result<String, String> {
        let mut g = self.inner.lock().unwrap();
        let id = g.data.active.clone().ok_or("нет активной пачки")?;
        let p = g.data.profiles.iter_mut().find(|p| p.id == id).ok_or("активная пачка не найдена")?;
        p.server = Some(creds);
        save(&g)?;
        Ok(id)
    }

    /// Добавить или обновить устройство (по id) в активной пачке (pairing).
    pub fn upsert_active_device(&self, dev: crate::types::PairedDevice) -> Result<(), String> {
        let mut g = self.inner.lock().unwrap();
        let id = g.data.active.clone().ok_or("нет активной пачки")?;
        let p = g.data.profiles.iter_mut().find(|p| p.id == id).ok_or("активная пачка не найдена")?;
        match p.devices.iter_mut().find(|d| d.id == dev.id) {
            Some(d) => *d = dev,
            None => p.devices.push(dev),
        }
        save(&g)
    }

    pub fn active_server(&self) -> Option<ServerCreds> {
        self.active_profile().and_then(|p| p.server)
    }

    pub fn active_bm_token(&self) -> Option<String> {
        self.active_profile().and_then(|p| p.bm_token)
    }
}

fn mask(mut p: Profile) -> Profile {
    if p.bm_token.as_deref().map(|t| !t.is_empty()).unwrap_or(false) {
        p.bm_token = Some(MASK.to_string());
    } else {
        p.bm_token = None;
    }
    if let Some(s) = p.server.as_mut() {
        if !s.player_token.is_empty() {
            s.player_token = MASK.to_string();
        }
    }
    mask_opt(&mut p.steam_key);
    mask_opt(&mut p.notify.discord_webhook);
    mask_opt(&mut p.notify.tg_token);
    p
}

fn mask_opt(o: &mut Option<String>) {
    if o.as_deref().map(|t| !t.is_empty()).unwrap_or(false) {
        *o = Some(MASK.to_string());
    } else {
        *o = None;
    }
}

/// Секретное поле из патча: None = не менять, Some(None) = стереть, Some(Some(v)) = новое значение.
fn secret_patch(v: Option<&Value>) -> Result<Option<Option<String>>, String> {
    match v {
        None => Ok(None),
        Some(Value::Null) => Ok(Some(None)),
        Some(Value::String(t)) => {
            let t = t.trim();
            if t == MASK {
                Ok(None)
            } else if t.is_empty() {
                Ok(Some(None))
            } else {
                Ok(Some(Some(t.to_string())))
            }
        }
        Some(_) => Err("ожидалась строка или null".into()),
    }
}

fn plain_opt(v: Option<&Value>) -> Option<Option<String>> {
    match v {
        None => None,
        Some(Value::String(t)) if !t.trim().is_empty() => Some(Some(t.trim().to_string())),
        _ => Some(None),
    }
}

fn apply_notify(n: &mut crate::types::NotifyCfg, v: &Value) -> Result<(), String> {
    if let Some(x) = secret_patch(v.get("discord_webhook")).map_err(|e| format!("discord_webhook: {e}"))? {
        if let Some(u) = &x {
            if !u.starts_with("https://") {
                return Err("discord_webhook должен начинаться с https://".into());
            }
        }
        n.discord_webhook = x;
    }
    if let Some(x) = secret_patch(v.get("tg_token")).map_err(|e| format!("tg_token: {e}"))? {
        n.tg_token = x;
    }
    if let Some(x) = plain_opt(v.get("tg_chat")) {
        n.tg_chat = x;
    }
    if let Some(Value::String(p)) = v.get("min_prio") {
        if !matches!(p.as_str(), "low" | "med" | "high") {
            return Err("min_prio: low, med или high".into());
        }
        n.min_prio = p.clone();
    }
    for (k, slot) in [
        ("dest_app", &mut n.dest_app),
        ("dest_chat", &mut n.dest_chat),
        ("dest_discord", &mut n.dest_discord),
        ("dest_tg", &mut n.dest_tg),
    ] {
        if let Some(Value::Bool(b)) = v.get(k) {
            *slot = *b;
        }
    }
    Ok(())
}

#[tauri::command]
pub fn profiles_list(state: State<'_, ProfilesState>) -> ProfilesDto {
    let g = state.inner.lock().unwrap();
    ProfilesDto {
        active: g.data.active.clone(),
        profiles: g.data.profiles.iter().cloned().map(mask).collect(),
    }
}

#[tauri::command]
pub fn profile_create(state: State<'_, ProfilesState>, name: String) -> Result<Profile, String> {
    let name = name.trim();
    if name.is_empty() {
        return Err("имя пачки не может быть пустым".into());
    }
    let mut g = state.inner.lock().unwrap();
    let p = new_profile(name);
    g.data.profiles.push(p.clone());
    if g.data.active.is_none() {
        g.data.active = Some(p.id.clone());
    }
    save(&g)?;
    Ok(mask(p))
}

fn str_field(v: &Value, k: &str) -> Option<String> {
    match v.get(k) {
        Some(Value::String(s)) => Some(s.trim().to_string()),
        Some(Value::Number(n)) => Some(n.to_string()),
        _ => None,
    }
}

fn apply_server(old: Option<&ServerCreds>, v: &Value) -> Result<ServerCreds, String> {
    let mut s = old.cloned().unwrap_or_default();
    if let Some(ip) = str_field(v, "ip") {
        s.ip = ip;
    }
    if let Some(port) = v.get("port") {
        let n = match port {
            Value::Number(n) => n.as_i64(),
            Value::String(t) => t.trim().parse::<i64>().ok(),
            _ => None,
        }
        .ok_or("некорректный порт")?;
        if !(1..=65535).contains(&n) {
            return Err("порт должен быть в диапазоне 1..65535".into());
        }
        s.port = n as u16;
    }
    if let Some(pid) = str_field(v, "player_id") {
        if pid.is_empty() || !pid.chars().all(|c| c.is_ascii_digit()) {
            return Err("player_id должен состоять только из цифр".into());
        }
        s.player_id = pid;
    }
    if let Some(t) = str_field(v, "player_token") {
        // «•••» из profiles_list и пустая строка при уже сохранённом токене: оставляем как было
        if t != MASK && !(t.is_empty() && !s.player_token.is_empty()) {
            if !t.is_empty() && t.parse::<i64>().is_err() {
                return Err("player_token должен быть числом (может быть отрицательным)".into());
            }
            s.player_token = t;
        }
    }
    if s.player_token.is_empty() {
        return Err("player_token не задан".into());
    }
    // сервер через ретранслятор Facepunch (relay_id из сопряжения) живёт без ip/порта
    if s.relay_id.is_empty() {
        if s.ip.is_empty() {
            return Err("ip сервера не задан".into());
        }
        if s.port == 0 {
            return Err("порт должен быть в диапазоне 1..65535".into());
        }
    }
    if s.player_id.is_empty() {
        return Err("player_id должен состоять только из цифр".into());
    }
    Ok(s)
}

#[tauri::command]
pub fn profile_update(
    state: State<'_, ProfilesState>,
    id: String,
    patch: Value,
) -> Result<Profile, String> {
    let mut g = state.inner.lock().unwrap();
    let idx = g
        .data
        .profiles
        .iter()
        .position(|p| p.id == id)
        .ok_or("пачка не найдена")?;
    let mut p = g.data.profiles[idx].clone();

    if let Some(n) = patch.get("name") {
        let n = n.as_str().unwrap_or("").trim();
        if n.is_empty() {
            return Err("имя пачки не может быть пустым".into());
        }
        p.name = n.to_string();
    }
    match patch.get("server") {
        Some(Value::Null) => p.server = None,
        Some(v @ Value::Object(_)) => p.server = Some(apply_server(p.server.as_ref(), v)?),
        Some(_) => return Err("server: ожидался объект или null".into()),
        None => {}
    }
    match patch.get("bm_token") {
        Some(Value::Null) => p.bm_token = None,
        Some(Value::String(t)) => {
            let t = t.trim();
            if t == MASK {
                // маска из списка: токен не менять
            } else if t.is_empty() {
                p.bm_token = None;
            } else {
                p.bm_token = Some(t.to_string());
            }
        }
        Some(_) => return Err("bm_token: ожидалась строка или null".into()),
        None => {}
    }
    if let Some(x) = secret_patch(patch.get("steam_key")).map_err(|e| format!("steam_key: {e}"))? {
        p.steam_key = x;
    }
    if let Some(v @ Value::Object(_)) = patch.get("notify") {
        apply_notify(&mut p.notify, v)?;
    }
    if let Some(Value::Array(arr)) = patch.get("watch_servers") {
        let mut out: Vec<crate::types::WatchServer> = Vec::new();
        for it in arr.iter().take(50) {
            let addr = it.get("addr").and_then(|v| v.as_str()).unwrap_or("").trim().to_string();
            let label = it.get("label").and_then(|v| v.as_str()).unwrap_or("").trim().to_string();
            let ok = addr.rsplit_once(':').map(|(h, p)| !h.is_empty() && p.parse::<u16>().map(|n| n > 0).unwrap_or(false)).unwrap_or(false);
            if !ok {
                return Err(format!("watch_servers: некорректный адрес «{addr}», нужен ip:порт"));
            }
            if !out.iter().any(|s| s.addr == addr) {
                out.push(crate::types::WatchServer { addr, label });
            }
        }
        p.watch_servers = out;
    }
    if let Some(v @ Value::Object(_)) = patch.get("automation") {
        p.automation = serde_json::from_value(v.clone()).map_err(|e| format!("automation: {e}"))?;
    }
    g.data.profiles[idx] = p.clone();
    save(&g)?;
    Ok(mask(p))
}

#[tauri::command]
pub async fn profile_delete(app: AppHandle, id: String) -> Result<(), String> {
    let (was_active, next) = {
        let st = app.state::<ProfilesState>();
        let mut g = st.inner.lock().unwrap();
        if !g.data.profiles.iter().any(|p| p.id == id) {
            return Err("пачка не найдена".into());
        }
        if g.data.profiles.len() <= 1 {
            return Err("нельзя удалить последнюю пачку".into());
        }
        let was_active = g.data.active.as_deref() == Some(id.as_str());
        g.data.profiles.retain(|p| p.id != id);
        let next = g.data.profiles[0].id.clone();
        if was_active {
            // active временно указывает на удалённую пачку до profile_switch; сбрасываем
            g.data.active = None;
        }
        save(&g)?;
        (was_active, next)
    };
    if was_active {
        profile_switch(app, next).await?;
    }
    Ok(())
}

#[tauri::command]
pub async fn profile_switch(app: AppHandle, id: String) -> Result<(), String> {
    {
        let st = app.state::<ProfilesState>();
        let g = st.inner.lock().unwrap();
        if !g.data.profiles.iter().any(|p| p.id == id) {
            return Err("пачка не найдена".into());
        }
    }
    crate::automation::stop(&app).await;
    crate::rustplus::disconnect(&app).await;
    crate::battlemetrics::reset(&app).await;
    let (server, token) = {
        let st = app.state::<ProfilesState>();
        let mut g = st.inner.lock().unwrap();
        g.data.active = Some(id.clone());
        save(&g)?;
        let p = g.data.profiles.iter().find(|p| p.id == id).cloned();
        (
            p.as_ref().and_then(|p| p.server.clone()),
            p.and_then(|p| p.bm_token),
        )
    };
    crate::battlemetrics::configure(&app, token).await;
    if let Some(creds) = server {
        // ошибки подключения приходят фронту событием rp_status
        let _ = crate::rustplus::connect(app.clone(), creds).await;
    }
    crate::automation::start(&app).await;
    app.emit("profile_changed", serde_json::json!({ "id": id }))
        .map_err(|e| e.to_string())?;
    Ok(())
}
