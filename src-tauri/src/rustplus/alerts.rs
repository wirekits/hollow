//! Детекция алертов из потоков Rust+. Здесь только чистая логика без IO (тестируется отдельно);
//! отправка (`notify::dispatch`) и эмит событий делаются в mod.rs.

use crate::types::Alert;
use serde_json::{json, Value};
use std::collections::{BTreeMap, HashMap, VecDeque};

/// Тип сущности из AppEntityType (rustplus.proto): Switch=1, Alarm=2, StorageMonitor=3.
pub const ENTITY_ALARM: i32 = 2;

/// Рейд = столько срабатываний алярмов за окно.
pub const RAID_HITS: usize = 3;
pub const RAID_WINDOW_MS: i64 = 60_000;
pub const RAID_COOLDOWN_MS: i64 = 5 * 60_000;
/// Алярм считается «горячим» в списке столько мс после срабатывания.
pub const ALARM_HOT_MS: i64 = 5 * 60_000;
/// Пороги decay (сек до конца защиты шкафа).
pub const DECAY_MED_SECS: i64 = 6 * 3600;
pub const DECAY_HIGH_SECS: i64 = 3600;
pub const DECAY_THROTTLE_MS: i64 = 30 * 60_000;
const HISTORY_MAX: usize = 50;

fn alert(kind: &str, prio: &str, title: &str, text: String) -> Alert {
    Alert { kind: kind.into(), prio: prio.into(), title: title.into(), text }
}

// ---------- команда ----------

#[derive(Clone, Debug, PartialEq)]
pub struct MemberSnap {
    pub steam_id: u64,
    pub name: String,
    pub online: bool,
    pub alive: bool,
    pub x: f32,
    pub y: f32,
}

/// Дифф двух снимков команды. `prev == None` (первое состояние после подключения) алертов не даёт.
/// `grid` превращает мировые координаты в квадрат («{имя} погиб на {квадрат}»); пустая строка = неизвестно.
pub fn diff_team(prev: Option<&[MemberSnap]>, next: &[MemberSnap], grid: &dyn Fn(f32, f32) -> String) -> Vec<Alert> {
    let Some(prev) = prev else { return Vec::new() };
    let old: HashMap<u64, &MemberSnap> = prev.iter().map(|m| (m.steam_id, m)).collect();
    let mut out = Vec::new();
    for m in next {
        let Some(o) = old.get(&m.steam_id) else {
            out.push(alert("join", "low", "Новый тимейт", format!("{} теперь в команде", m.name)));
            continue;
        };
        if o.alive && !m.alive {
            // у мёртвого/оффлайн игрока координаты могут быть нулевыми: тогда берём последние известные
            let (x, y) = if m.x != 0.0 || m.y != 0.0 { (m.x, m.y) } else { (o.x, o.y) };
            let g = if x != 0.0 || y != 0.0 { grid(x, y) } else { String::new() };
            let text = if g.is_empty() { format!("{} погиб", m.name) } else { format!("{} погиб на {}", m.name, g) };
            out.push(alert("death", "med", "Смерть тимейта", text));
        }
        if o.online != m.online {
            if m.online {
                out.push(alert("online", "low", "Тимейт онлайн", format!("{} в сети", m.name)));
            } else {
                out.push(alert("offline", "low", "Тимейт оффлайн", format!("{} вышел", m.name)));
            }
        }
    }
    out
}

// ---------- рейд ----------

/// Окно рейда: N срабатываний за `window_ms`, после срабатывания пауза `cooldown_ms`.
pub struct RaidWindow {
    hits: VecDeque<i64>,
    n: usize,
    window_ms: i64,
    cooldown_ms: i64,
    quiet_until: i64,
}

impl RaidWindow {
    pub fn new(n: usize, window_ms: i64, cooldown_ms: i64) -> Self {
        Self { hits: VecDeque::new(), n, window_ms, cooldown_ms, quiet_until: i64::MIN }
    }

    /// Регистрирует срабатывание; true, если набралось достаточно для рейда (ровно один раз на cooldown).
    pub fn record(&mut self, now_ms: i64) -> bool {
        self.hits.push_back(now_ms);
        while let Some(&f) = self.hits.front() {
            if now_ms - f > self.window_ms {
                self.hits.pop_front();
            } else {
                break;
            }
        }
        if self.hits.len() >= self.n && now_ms >= self.quiet_until {
            self.hits.clear();
            self.quiet_until = now_ms + self.cooldown_ms;
            return true;
        }
        false
    }
}

impl Default for RaidWindow {
    fn default() -> Self {
        Self::new(RAID_HITS, RAID_WINDOW_MS, RAID_COOLDOWN_MS)
    }
}

/// Имя устройства из сопряжения, иначе «{prefix} #id».
pub fn label(names: &HashMap<u32, String>, id: u32, prefix: &str) -> String {
    match names.get(&id).map(|s| s.trim()).filter(|s| !s.is_empty()) {
        Some(n) => n.to_string(),
        None => format!("{prefix} #{id}"),
    }
}

pub fn alarm_alert(name: &str) -> Alert {
    alert("alarm", "med", "Аларм", format!("Сработал аларм: {name}"))
}

pub fn raid_alert(hits: usize) -> Alert {
    alert("raid", "high", "Рейд", format!("{hits} срабатываний алярма за минуту: возможно, рейд базы"))
}

// ---------- алярмы для фронта ----------

struct AlarmRec {
    last_val: Option<bool>,
    last_on: Option<i64>,
}

/// Известные алярмы (появляются, только когда мы увидели их broadcast) и история срабатываний.
#[derive(Default)]
pub struct AlarmTracker {
    alarms: BTreeMap<u32, AlarmRec>,
    history: VecDeque<(i64, String)>,
    /// имена из сопряжённых устройств пачки (id -> имя из игры)
    pub names: HashMap<u32, String>,
}

impl AlarmTracker {
    /// Новое значение алярма; true, если это срабатывание (переход в true).
    pub fn on_value(&mut self, id: u32, val: bool, now_ms: i64) -> bool {
        let rec = self.alarms.entry(id).or_insert(AlarmRec { last_val: None, last_on: None });
        let trig = val && rec.last_val != Some(true);
        rec.last_val = Some(val);
        if trig {
            rec.last_on = Some(now_ms);
            let name = label(&self.names, id, "Аларм");
            self.push_history(now_ms, format!("{name}: сработал"));
        }
        trig
    }

    pub fn push_history(&mut self, now_ms: i64, text: String) {
        self.history.push_front((now_ms, text));
        self.history.truncate(HISTORY_MAX);
    }

    /// setAlarms: [{ name, hot, t (минут назад), prio }].
    pub fn alarms_form(&self, now_ms: i64) -> Value {
        let list: Vec<Value> = self
            .alarms
            .iter()
            .map(|(id, r)| {
                let ago = r.last_on.map(|t| (now_ms - t).max(0));
                let hot = ago.map(|a| a <= ALARM_HOT_MS).unwrap_or(false);
                json!({ "name": label(&self.names, *id, "Аларм"), "hot": hot, "t": ago.map(|a| a / 60_000).unwrap_or(0), "prio": "med" })
            })
            .collect();
        Value::Array(list)
    }

    /// setAlarmHist: [{ t: "ЧЧ:ММ", text }], новые сверху.
    pub fn history_form(&self, fmt_time: &dyn Fn(i64) -> String) -> Value {
        Value::Array(self.history.iter().map(|(t, s)| json!({ "t": fmt_time(*t), "text": s })).collect())
    }
}

// ---------- decay шкафа ----------

fn prio_rank(p: &str) -> u8 {
    match p {
        "high" => 2,
        "med" => 1,
        _ => 0,
    }
}

/// Приоритет алерта по оставшимся секундам защиты: <1 ч high, <6 ч med, иначе нет алерта.
pub fn decay_prio(left_secs: i64) -> Option<&'static str> {
    if left_secs < DECAY_HIGH_SECS {
        Some("high")
    } else if left_secs < DECAY_MED_SECS {
        Some("med")
    } else {
        None
    }
}

/// Уровень для setDecay: "ok" | "warn" | "bad".
pub fn decay_level(left_secs: i64) -> &'static str {
    match decay_prio(left_secs) {
        Some("high") => "bad",
        Some(_) => "warn",
        None => "ok",
    }
}

pub fn decay_alert(name: &str, left_secs: i64) -> Option<Alert> {
    let prio = decay_prio(left_secs)?;
    let l = left_secs.max(0);
    let text = format!("{name}: защита закончится через {} ч {} мин", l / 3600, (l % 3600) / 60);
    Some(alert("decay", prio, "Шкаф распадается", text))
}

/// Не чаще раза в 30 мин на шкаф; повышение приоритета (med -> high) проходит сразу.
#[derive(Default)]
pub struct DecayGate {
    last: HashMap<u32, (i64, u8)>,
}

impl DecayGate {
    pub fn allow(&mut self, id: u32, prio: &str, now_ms: i64) -> bool {
        let r = prio_rank(prio);
        let ok = match self.last.get(&id) {
            None => true,
            Some(&(t, pr)) => now_ms - t >= DECAY_THROTTLE_MS || r > pr,
        };
        if ok {
            self.last.insert(id, (now_ms, r));
        }
        ok
    }
}

/// setDecay: [{ name, level, left (часов) }] по известным шкафам (id -> unix-секунда конца защиты).
pub fn decay_form(expiries: &BTreeMap<u32, i64>, now_secs: i64, names: &HashMap<u32, String>) -> Value {
    Value::Array(
        expiries
            .iter()
            .map(|(id, exp)| {
                let left = exp - now_secs;
                json!({ "name": label(names, *id, "Шкаф"), "level": decay_level(left), "left": (left.max(0)) / 3600 })
            })
            .collect(),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn m(id: u64, name: &str, online: bool, alive: bool) -> MemberSnap {
        MemberSnap { steam_id: id, name: name.into(), online, alive, x: 100.0, y: 200.0 }
    }
    fn g(_: f32, _: f32) -> String {
        "B3".into()
    }

    #[test]
    fn first_snapshot_is_silent() {
        assert!(diff_team(None, &[m(1, "a", true, true)], &g).is_empty());
    }

    #[test]
    fn death_online_offline_join() {
        let prev = vec![m(1, "a", true, true), m(2, "b", false, true)];
        let next = vec![m(1, "a", true, false), m(2, "b", true, true), m(3, "c", true, true)];
        let al = diff_team(Some(&prev), &next, &g);
        assert_eq!(al.len(), 3);
        assert_eq!((al[0].kind.as_str(), al[0].prio.as_str()), ("death", "med"));
        assert_eq!(al[0].text, "a погиб на B3");
        assert_eq!(al[1].kind, "online");
        assert_eq!(al[2].kind, "join");
        let back = diff_team(Some(&next), &[m(1, "a", false, false)], &g);
        assert_eq!(back.len(), 1);
        assert_eq!(back[0].kind, "offline");
        // без изменений
        assert!(diff_team(Some(&next), &next, &g).is_empty());
    }

    #[test]
    fn death_uses_last_pos_when_zero() {
        let prev = vec![m(1, "a", true, true)];
        let mut d = m(1, "a", true, false);
        d.x = 0.0;
        d.y = 0.0;
        let al = diff_team(Some(&prev), &[d], &|x, _| if x == 100.0 { "C1".into() } else { String::new() });
        assert_eq!(al[0].text, "a погиб на C1");
    }

    #[test]
    fn raid_window() {
        let mut w = RaidWindow::new(3, 60_000, 300_000);
        assert!(!w.record(0));
        assert!(!w.record(10_000));
        assert!(w.record(20_000));
        // пауза: ещё три срабатывания сразу не дают второй рейд
        assert!(!w.record(21_000) && !w.record(22_000) && !w.record(23_000));
        // после паузы снова работает
        let t = 400_000;
        assert!(!w.record(t) && !w.record(t + 1000));
        assert!(w.record(t + 2000));
    }

    #[test]
    fn raid_window_expires() {
        let mut w = RaidWindow::new(3, 60_000, 0);
        assert!(!w.record(0));
        assert!(!w.record(30_000));
        assert!(!w.record(70_000)); // первое вышло из окна: в окне 2
        assert!(w.record(80_000));
    }

    #[test]
    fn decay_thresholds() {
        assert_eq!(decay_prio(7 * 3600), None);
        assert_eq!(decay_prio(5 * 3600), Some("med"));
        assert_eq!(decay_prio(3599), Some("high"));
        assert_eq!(decay_prio(-5), Some("high"));
        assert_eq!(decay_level(10 * 3600), "ok");
        assert_eq!(decay_level(2 * 3600), "warn");
        assert_eq!(decay_level(60), "bad");
        let a = decay_alert("Шкаф #7", 5400 + 6 * 3600 - 6 * 3600).unwrap();
        assert_eq!((a.kind.as_str(), a.prio.as_str()), ("decay", "med"));
        assert!(a.text.contains("1 ч 30 мин"));
        assert!(decay_alert("Шкаф #7", 9 * 3600).is_none());
    }

    #[test]
    fn decay_throttle() {
        let mut gate = DecayGate::default();
        assert!(gate.allow(1, "med", 0));
        assert!(!gate.allow(1, "med", 10 * 60_000));
        assert!(gate.allow(1, "high", 11 * 60_000)); // эскалация
        assert!(!gate.allow(1, "high", 20 * 60_000));
        assert!(gate.allow(1, "high", 42 * 60_000));
        assert!(gate.allow(2, "med", 0)); // другой шкаф
    }

    #[test]
    fn alarm_tracker_forms() {
        let mut t = AlarmTracker::default();
        assert!(t.on_value(5, true, 1_000));
        assert!(!t.on_value(5, true, 2_000)); // повтор true не срабатывание
        assert!(!t.on_value(5, false, 3_000));
        assert!(t.on_value(5, true, 4_000));
        let f = t.alarms_form(4_000 + 2 * 60_000);
        assert_eq!(f[0]["hot"], true);
        assert_eq!(f[0]["t"], 2);
        let f2 = t.alarms_form(4_000 + 10 * 60_000);
        assert_eq!(f2[0]["hot"], false);
        assert_eq!(f[0]["name"], "Аларм #5");
        t.names.insert(5, "Дверь ядро".into());
        assert_eq!(t.alarms_form(0)[0]["name"], "Дверь ядро");
        let h = t.history_form(&|ms| format!("t{ms}"));
        assert_eq!(h.as_array().unwrap().len(), 2);
        assert_eq!(h[0]["t"], "t4000");
    }

    #[test]
    fn decay_form_shape() {
        let mut e = BTreeMap::new();
        e.insert(3u32, 1000 + 2 * 3600);
        let f = decay_form(&e, 1000, &HashMap::new());
        assert_eq!(f[0]["level"], "warn");
        assert_eq!(f[0]["left"], 2);
        assert_eq!(f[0]["name"], "Шкаф #3");
        let names: HashMap<u32, String> = [(3u32, "TC main".to_string())].into_iter().collect();
        assert_eq!(decay_form(&e, 1000, &names)[0]["name"], "TC main");
    }
}
