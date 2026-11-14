use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct ServerCreds {
    pub ip: String,
    pub port: u16,
    pub player_id: String,     // SteamID64 строкой
    pub player_token: String,  // i32 со знаком, строкой
}

/// Куда и что слать. Секреты (webhook, tg_token) наружу только маской «•••».
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(default)]
pub struct NotifyCfg {
    pub discord_webhook: Option<String>,
    pub tg_token: Option<String>,
    pub tg_chat: Option<String>,
    pub min_prio: String,      // "low" | "med" | "high"; пусто = "low"
    pub dest_app: bool,        // уведомление внутри программы и нативное
    pub dest_chat: bool,       // в тим-чат игры
    pub dest_discord: bool,
    pub dest_tg: bool,
}

impl Default for NotifyCfg {
    fn default() -> Self {
        Self {
            discord_webhook: None,
            tg_token: None,
            tg_chat: None,
            min_prio: "low".into(),
            dest_app: true,
            dest_chat: false,
            dest_discord: false,
            dest_tg: false,
        }
    }
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct DeviceGroup {
    pub id: String,
    pub name: String,
    pub device_ids: Vec<String>,   // id устройств Rust+ строками
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct AutomationCfg {
    pub night_off: bool,
    pub night_from: String,        // "22:00" локальное время
    pub night_to: String,          // "07:00"
    pub night_groups: Vec<String>, // id групп, которые гасим на ночь
    pub groups: Vec<DeviceGroup>,
}

/// Игровой сервер под наблюдением (опрос A2S): ищем в списке игроков ники целей.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct WatchServer {
    pub addr: String,    // "ip:port" (query-порт или игровой порт, см. модуль a2s)
    pub label: String,   // имя для показа (можно пустое)
}

/// Устройство, полученное через pairing (имя из игры).
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct PairedDevice {
    pub id: String,     // entityId строкой
    pub name: String,   // имя из игры
    pub kind: String,   // "switch" | "alarm" | "storage"
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct Profile {
    #[serde(default)]
    pub devices: Vec<PairedDevice>,
    #[serde(default)]
    pub watch_servers: Vec<WatchServer>,
    pub id: String,                       // uuid v4
    pub name: String,                     // «пачка»
    pub server: Option<ServerCreds>,
    pub bm_token: Option<String>,         // BattleMetrics token
    #[serde(default)]
    pub steam_key: Option<String>,        // Steam Web API key
    #[serde(default)]
    pub notify: NotifyCfg,
    #[serde(default)]
    pub automation: AutomationCfg,
}

/// Уведомление. kind: raid|alarm|decay|death|online|offline|join|info; prio: low|med|high.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct Alert {
    pub kind: String,
    pub prio: String,
    pub title: String,
    pub text: String,
}
