//! A2S (Source Server Queries): A2S_INFO и A2S_PLAYER поверх UDP, без ключей.
//! Поддержаны challenge (0x41), разбитые пакеты (заголовок -2). Сжатые bz2 пакеты не
//! распаковываются: возвращается понятная ошибка.
use std::net::{IpAddr, SocketAddr};
use std::time::{Duration, Instant};

use futures_util::stream::{FuturesUnordered, StreamExt};
use tokio::net::UdpSocket;

/// Потолок числа игроков (счётчик в протоколе u8, но защищаемся от мусора).
const MAX_PLAYERS: usize = 512;
/// Максимум пакетов в одном ответе.
const MAX_FRAGMENTS: usize = 64;
/// Максимум собранного ответа.
const MAX_MESSAGE: usize = 256 * 1024;
const RECV_BUF: usize = 4096;

#[derive(Clone, Debug, Default)]
pub struct A2sPlayer {
    pub name: String,
    pub score: i32,
    pub duration_s: f32,
}

#[derive(Clone, Debug, Default)]
pub struct A2sInfo {
    pub name: String,
    pub map: String,
    pub players: u32,
    pub max_players: u32,
    pub game: String,
    pub bots: u32,
}

// ───────────────────────── чистый парсинг ─────────────────────────

struct Reader<'a> {
    b: &'a [u8],
    p: usize,
}

impl<'a> Reader<'a> {
    fn new(b: &'a [u8]) -> Self {
        Reader { b, p: 0 }
    }
    fn left(&self) -> usize {
        self.b.len() - self.p
    }
    fn u8(&mut self) -> Option<u8> {
        let v = *self.b.get(self.p)?;
        self.p += 1;
        Some(v)
    }
    fn take(&mut self, n: usize) -> Option<&'a [u8]> {
        if self.left() < n {
            return None;
        }
        let s = &self.b[self.p..self.p + n];
        self.p += n;
        Some(s)
    }
    fn u16(&mut self) -> Option<u16> {
        let s = self.take(2)?;
        Some(u16::from_le_bytes([s[0], s[1]]))
    }
    fn i32(&mut self) -> Option<i32> {
        let s = self.take(4)?;
        Some(i32::from_le_bytes([s[0], s[1], s[2], s[3]]))
    }
    fn f32(&mut self) -> Option<f32> {
        let s = self.take(4)?;
        Some(f32::from_le_bytes([s[0], s[1], s[2], s[3]]))
    }
    /// Строка до нуля; UTF-8 с заменой невалидного. Без нуля в конце: None.
    fn cstr(&mut self) -> Option<String> {
        let rest = &self.b[self.p..];
        let n = rest.iter().position(|&c| c == 0)?;
        let s = String::from_utf8_lossy(&rest[..n]).into_owned();
        self.p += n + 1;
        Some(s)
    }
}

pub fn build_info_request(challenge: Option<[u8; 4]>) -> Vec<u8> {
    let mut v = vec![0xFF, 0xFF, 0xFF, 0xFF, 0x54];
    v.extend_from_slice(b"Source Engine Query\0");
    if let Some(c) = challenge {
        v.extend_from_slice(&c);
    }
    v
}

pub fn build_player_request(challenge: [u8; 4]) -> Vec<u8> {
    let mut v = vec![0xFF, 0xFF, 0xFF, 0xFF, 0x55];
    v.extend_from_slice(&challenge);
    v
}

/// Разобранное содержимое сообщения (после заголовка -1).
#[derive(Debug, PartialEq)]
pub enum Reply<'a> {
    Challenge([u8; 4]),
    Info(&'a [u8]),
    Players(&'a [u8]),
    Other(u8),
}

pub fn classify(payload: &[u8]) -> Result<Reply<'_>, String> {
    let (&t, rest) = payload.split_first().ok_or("пустой ответ A2S")?;
    match t {
        0x41 => {
            if rest.len() < 4 {
                return Err("короткий challenge".into());
            }
            Ok(Reply::Challenge([rest[0], rest[1], rest[2], rest[3]]))
        }
        0x49 | 0x6D => Ok(Reply::Info(payload)),
        0x44 => Ok(Reply::Players(payload)),
        o => Ok(Reply::Other(o)),
    }
}

/// payload начинается с байта типа (0x49 Source или 0x6D GoldSrc).
pub fn parse_info(payload: &[u8]) -> Result<A2sInfo, String> {
    let mut r = Reader::new(payload);
    let t = r.u8().ok_or("пустой info")?;
    let bad = || "обрезанный ответ A2S_INFO".to_string();
    match t {
        0x49 => {
            let _proto = r.u8().ok_or_else(bad)?;
            let name = r.cstr().ok_or_else(bad)?;
            let map = r.cstr().ok_or_else(bad)?;
            let _folder = r.cstr().ok_or_else(bad)?;
            let game = r.cstr().ok_or_else(bad)?;
            let _id = r.u16().ok_or_else(bad)?;
            let players = r.u8().ok_or_else(bad)? as u32;
            let max_players = r.u8().ok_or_else(bad)? as u32;
            let bots = r.u8().unwrap_or(0) as u32;
            Ok(A2sInfo { name, map, players, max_players, game, bots })
        }
        0x6D => {
            let _addr = r.cstr().ok_or_else(bad)?;
            let name = r.cstr().ok_or_else(bad)?;
            let map = r.cstr().ok_or_else(bad)?;
            let _folder = r.cstr().ok_or_else(bad)?;
            let game = r.cstr().ok_or_else(bad)?;
            let players = r.u8().ok_or_else(bad)? as u32;
            let max_players = r.u8().ok_or_else(bad)? as u32;
            Ok(A2sInfo { name, map, players, max_players, game, bots: 0 })
        }
        o => Err(format!("не A2S_INFO (тип 0x{o:02X})")),
    }
}

/// payload начинается с байта 0x44. Обрезанный хвост не ошибка: берём разобранное.
pub fn parse_players(payload: &[u8]) -> Result<Vec<A2sPlayer>, String> {
    let mut r = Reader::new(payload);
    if r.u8() != Some(0x44) {
        return Err("не A2S_PLAYER".into());
    }
    let count = r.u8().ok_or("обрезанный ответ A2S_PLAYER")? as usize;
    let mut out = Vec::new();
    for _ in 0..count.min(MAX_PLAYERS) {
        if r.u8().is_none() {
            break; // index
        }
        let Some(name) = r.cstr() else { break };
        let (Some(score), Some(dur)) = (r.i32(), r.f32()) else { break };
        out.push(A2sPlayer { name, score, duration_s: if dur.is_finite() { dur } else { 0.0 } });
    }
    Ok(out)
}

/// Сборщик разбитых пакетов. feed возвращает Some(payload без заголовка -1), когда сообщение готово.
#[derive(Default)]
pub struct Assembler {
    id: Option<u32>,
    total: usize,
    parts: Vec<Option<Vec<u8>>>,
    compressed: bool,
    size: usize,
}

impl Assembler {
    pub fn feed(&mut self, d: &[u8]) -> Result<Option<Vec<u8>>, String> {
        if d.len() < 5 {
            return Err("слишком короткий пакет".into());
        }
        let head = i32::from_le_bytes([d[0], d[1], d[2], d[3]]);
        if head == -1 {
            return Ok(Some(d[4..].to_vec()));
        }
        if head != -2 {
            return Err("неизвестный заголовок пакета".into());
        }
        if d.len() < 12 {
            return Err("обрезанный заголовок split-пакета".into());
        }
        let id = u32::from_le_bytes([d[4], d[5], d[6], d[7]]);
        let total = d[8] as usize;
        let num = d[9] as usize;
        if total == 0 || total > MAX_FRAGMENTS || num >= total {
            return Err("некорректные total/number в split-пакете".into());
        }
        let comp = id & 0x8000_0000 != 0;
        match self.id {
            None => {
                self.id = Some(id);
                self.total = total;
                self.parts = vec![None; total];
                self.compressed = comp;
            }
            Some(cur) if cur != id || self.total != total => return Ok(None), // чужой ответ
            _ => {}
        }
        // d[10..12] size (Source); у сжатого пакета №0 далее u32 размер и u32 crc
        let mut body = &d[12..];
        if comp && num == 0 {
            if body.len() < 8 {
                return Err("обрезанный заголовок сжатого пакета".into());
            }
            body = &body[8..];
        }
        self.size += body.len();
        if self.size > MAX_MESSAGE {
            return Err("ответ A2S слишком большой".into());
        }
        self.parts[num] = Some(body.to_vec());
        if self.parts.iter().all(|p| p.is_some()) {
            if self.compressed {
                return Err("ответ A2S сжат bz2: распаковка не поддерживается".into());
            }
            let mut all = Vec::with_capacity(self.size);
            for p in self.parts.iter().flatten() {
                all.extend_from_slice(p);
            }
            // собранное сообщение снова начинается с заголовка -1
            if all.len() >= 4 && all[..4] == [0xFF, 0xFF, 0xFF, 0xFF] {
                all.drain(..4);
            }
            return Ok(Some(all));
        }
        Ok(None)
    }
}

// ───────────────────────── сеть ─────────────────────────

async fn open(addr: SocketAddr) -> Result<UdpSocket, String> {
    let bind: SocketAddr = if addr.is_ipv4() { "0.0.0.0:0" } else { "[::]:0" }.parse().unwrap();
    let s = UdpSocket::bind(bind).await.map_err(|e| format!("udp bind: {e}"))?;
    s.connect(addr).await.map_err(|e| format!("udp connect: {e}"))?;
    Ok(s)
}

/// Отправить запрос и дождаться полного сообщения (с учётом split-пакетов) до deadline.
async fn exchange(s: &UdpSocket, req: &[u8], deadline: Instant) -> Result<Vec<u8>, String> {
    s.send(req).await.map_err(|e| format!("udp send: {e}"))?;
    let mut asm = Assembler::default();
    let mut buf = vec![0u8; RECV_BUF];
    loop {
        let left = deadline.saturating_duration_since(Instant::now());
        if left.is_zero() {
            return Err("таймаут A2S".into());
        }
        let n = match tokio::time::timeout(left, s.recv(&mut buf)).await {
            Err(_) => return Err("таймаут A2S".into()),
            Ok(Err(e)) => return Err(format!("udp recv: {e}")),
            Ok(Ok(n)) => n,
        };
        if let Some(m) = asm.feed(&buf[..n])? {
            return Ok(m);
        }
    }
}

pub async fn query_info(addr: SocketAddr, timeout: Duration) -> Result<A2sInfo, String> {
    let deadline = Instant::now() + timeout;
    let s = open(addr).await?;
    let mut challenge = None;
    for _ in 0..3 {
        let m = exchange(&s, &build_info_request(challenge), deadline).await?;
        match classify(&m)? {
            Reply::Challenge(c) => challenge = Some(c),
            Reply::Info(p) => return parse_info(p),
            _ => return Err("неожиданный ответ на A2S_INFO".into()),
        }
    }
    Err("сервер не принял challenge A2S_INFO".into())
}

/// Список игроков сервера (A2S_PLAYER, с challenge и сборкой разбитых пакетов).
pub async fn query_players(addr: SocketAddr, timeout: Duration) -> Result<Vec<A2sPlayer>, String> {
    let deadline = Instant::now() + timeout;
    let s = open(addr).await?;
    let mut challenge = [0xFF; 4];
    for _ in 0..3 {
        let m = exchange(&s, &build_player_request(challenge), deadline).await?;
        match classify(&m)? {
            Reply::Challenge(c) => challenge = c,
            Reply::Players(p) => return parse_players(p),
            _ => return Err("неожиданный ответ на A2S_PLAYER".into()),
        }
    }
    Err("сервер не принял challenge A2S_PLAYER".into())
}

fn norm(s: &str) -> String {
    s.chars().filter(|c| !c.is_whitespace()).flat_map(|c| c.to_lowercase()).collect()
}

/// Подходит ли ответ A2S_INFO: имя совпадает (без регистра/пробелов) либо, без ожидаемого имени, game содержит "Rust".
pub fn info_matches(info: &A2sInfo, expect_name: Option<&str>) -> bool {
    match expect_name.map(norm).filter(|e| !e.is_empty()) {
        Some(e) => {
            let n = norm(&info.name);
            n == e || n.contains(&e) || e.contains(&n) && !n.is_empty() && n.len() * 2 >= e.len()
        }
        None => info.game.to_lowercase().contains("rust"),
    }
}

/// Небольшой набор кандидатов query-порта. Rust+ app.port по умолчанию 28082 = game 28015 + 67.
/// Query-порт Rust по умолчанию: game+1 или rcon+1 (что больше), типично 28017 (game 28015, rcon 28016).
pub fn candidate_ports(app_port: u16) -> Vec<u16> {
    let g = app_port.saturating_sub(67);
    let mut v: Vec<u16> = Vec::new();
    let mut add = |p: i32| {
        if (1024..=65535).contains(&p) && !v.contains(&(p as u16)) {
            v.push(p as u16);
        }
    };
    let g = g as i32;
    for d in [2, 1, 0, 3, -1, 4] {
        add(g + d);
    }
    let a = app_port as i32;
    for d in [-1, 1, 2, 0] {
        add(a + d);
    }
    for p in [27015, 27016, 27017, 28017] {
        add(p);
    }
    v
}

/// Угадать query-порт по адресу Rust+ (app port) и подтвердить по имени сервера. Возвращает рабочий адрес.
pub async fn probe_query_addr(ip: &str, app_port: u16, expect_name: Option<&str>, timeout: Duration) -> Option<SocketAddr> {
    let ip: IpAddr = ip.trim().parse().ok()?;
    let deadline = Instant::now() + timeout;
    let mut futs = FuturesUnordered::new();
    for p in candidate_ports(app_port) {
        let addr = SocketAddr::new(ip, p);
        futs.push(async move {
            let left = deadline.saturating_duration_since(Instant::now());
            match query_info(addr, left).await {
                Ok(i) if info_matches(&i, expect_name) => Some(addr),
                _ => None,
            }
        });
    }
    let run = async {
        while let Some(r) = futs.next().await {
            if r.is_some() {
                return r;
            }
        }
        None
    };
    tokio::time::timeout(timeout, run).await.ok().flatten()
}

// ───────────────────────── тесты ─────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    fn info_bytes() -> Vec<u8> {
        let mut v = vec![0x49, 17];
        v.extend_from_slice(b"My Rust \xD0\x9F\xFF\0");
        v.extend_from_slice(b"Procedural Map\0rust\0Rust\0");
        v.extend_from_slice(&[0, 0, 42, 100, 3, b'd', b'w', 1, 1]);
        v
    }

    #[test]
    fn info_parse() {
        let i = parse_info(&info_bytes()).unwrap();
        assert!(i.name.starts_with("My Rust П"));
        assert!(i.name.contains('\u{FFFD}'));
        assert_eq!((i.players, i.max_players, i.bots), (42, 100, 3));
        assert_eq!(i.game, "Rust");
        assert!(parse_info(&[0x49, 17, b'a']).is_err());
    }

    #[test]
    fn challenge_classify() {
        assert_eq!(classify(&[0x41, 1, 2, 3, 4]).unwrap(), Reply::Challenge([1, 2, 3, 4]));
        assert!(classify(&[0x41, 1]).is_err());
        assert_eq!(build_info_request(Some([9, 9, 9, 9])).last(), Some(&9));
        assert_eq!(build_player_request([0xFF; 4]), vec![0xFF, 0xFF, 0xFF, 0xFF, 0x55, 0xFF, 0xFF, 0xFF, 0xFF]);
    }

    fn players_bytes(n: u8) -> Vec<u8> {
        let mut v = vec![0x44, n];
        for i in 0..n {
            v.push(i);
            v.extend_from_slice(format!("pl{i}\0").as_bytes());
            v.extend_from_slice(&(i as i32).to_le_bytes());
            v.extend_from_slice(&123.5f32.to_le_bytes());
        }
        v
    }

    #[test]
    fn players_parse_and_truncate() {
        let p = parse_players(&players_bytes(3)).unwrap();
        assert_eq!(p.len(), 3);
        assert_eq!(p[2].name, "pl2");
        assert_eq!(p[1].score, 1);
        assert_eq!(p[0].duration_s, 123.5);
        let mut b = players_bytes(3);
        b.truncate(b.len() - 3);
        assert_eq!(parse_players(&b).unwrap().len(), 2);
        assert!(parse_players(&[0x45]).is_err());
    }

    fn frag(id: u32, total: u8, num: u8, body: &[u8], with_head: bool) -> Vec<u8> {
        let mut v = (-2i32).to_le_bytes().to_vec();
        v.extend_from_slice(&id.to_le_bytes());
        v.push(total);
        v.push(num);
        v.extend_from_slice(&1248u16.to_le_bytes());
        if with_head {
            v.extend_from_slice(&[0xFF; 4]);
        }
        v.extend_from_slice(body);
        v
    }

    #[test]
    fn split_reassembly_out_of_order() {
        let msg = players_bytes(40);
        let (a, rest) = msg.split_at(100);
        let (b, c) = rest.split_at(150);
        let mut asm = Assembler::default();
        assert!(asm.feed(&frag(7, 3, 2, c, false)).unwrap().is_none());
        assert!(asm.feed(&frag(7, 3, 0, a, true)).unwrap().is_none());
        let out = asm.feed(&frag(7, 3, 1, b, false)).unwrap().unwrap();
        assert_eq!(out, msg);
        assert_eq!(parse_players(&out).unwrap().len(), 40);
    }

    #[test]
    fn single_and_bad_packets() {
        let mut asm = Assembler::default();
        let mut d = vec![0xFF; 4];
        d.extend_from_slice(&[0x41, 1, 2, 3, 4]);
        assert_eq!(asm.feed(&d).unwrap().unwrap(), vec![0x41, 1, 2, 3, 4]);
        assert!(Assembler::default().feed(&[1, 2, 3]).is_err());
        assert!(Assembler::default().feed(&frag(1, 0, 0, b"x", false)).is_err());
        assert!(Assembler::default().feed(&frag(1, 2, 5, b"x", false)).is_err());
    }

    #[test]
    fn compressed_is_error() {
        let id = 0x8000_0001u32;
        let mut asm = Assembler::default();
        let mut p0 = vec![0u8; 8];
        p0.extend_from_slice(b"BZh");
        assert!(asm.feed(&frag(id, 2, 0, &p0, false)).unwrap().is_none());
        let e = asm.feed(&frag(id, 2, 1, b"zz", false)).unwrap_err();
        assert!(e.contains("bz2"));
    }

    /// Живая проверка вручную: A2S_LIVE=ip:app_port cargo test --lib a2s_live -- --ignored --nocapture
    #[tokio::test]
    #[ignore]
    async fn a2s_live() {
        let Ok(v) = std::env::var("A2S_LIVE") else { return };
        let (ip, port) = v.rsplit_once(':').unwrap();
        let t = Duration::from_secs(5);
        let addr = probe_query_addr(ip, port.parse().unwrap(), None, t).await;
        println!("probe: {addr:?}");
        if let Some(a) = addr {
            println!("info: {:?}", query_info(a, t).await);
            let p = query_players(a, t).await;
            match p {
                Ok(p) => println!("players: {} first: {:?}", p.len(), p.iter().take(5).collect::<Vec<_>>()),
                Err(e) => println!("players err: {e}"),
            }
        }
    }

    #[test]
    fn matching_and_candidates() {
        let i = A2sInfo { name: "My  Rust Server".into(), game: "Rust".into(), ..Default::default() };
        assert!(info_matches(&i, Some("my rust server")));
        assert!(!info_matches(&i, Some("totally different")));
        assert!(info_matches(&i, None));
        let c = candidate_ports(28082);
        assert_eq!(c[0], 28017);
        assert!(c.contains(&28015) && c.contains(&27015));
        assert!(c.len() < 20);
    }
}


// ---------- автоопределение query-порта через Steam master list ----------
// Нужен бесплатный Steam Web API-ключ. IGameServersService/GetServerList отдаёт для каждого сервера
// `addr` = ip:QUERY-порт и `gameport` (проверено живым запросом: addr 80.232.242.125:28017, gameport 28015).
// Фильтры `\appid\252490\addr\<ip>` и `\appid\252490\gameaddr\<ip:gameport>` подтверждены живым запросом.

#[derive(Debug, Clone, PartialEq)]
pub struct MasterServer {
    pub addr: String,
    pub game_port: u16,
    pub name: String,
}

pub fn parse_master_list(v: &serde_json::Value) -> Vec<MasterServer> {
    v.get("response")
        .and_then(|r| r.get("servers"))
        .and_then(|s| s.as_array())
        .map(|arr| {
            arr.iter()
                .filter_map(|s| {
                    let addr = s.get("addr")?.as_str()?.to_string();
                    let game_port = s.get("gameport").and_then(|p| p.as_u64()).unwrap_or(0).min(65535) as u16;
                    let name = s.get("name").and_then(|n| n.as_str()).unwrap_or("").to_string();
                    Some(MasterServer { addr, game_port, name })
                })
                .collect()
        })
        .unwrap_or_default()
}

/// Выбор сервера из ответа: по игровому порту, иначе по имени, иначе единственный кандидат.
pub fn pick_master_server(list: &[MasterServer], game_port: Option<u16>, expect_name: Option<&str>) -> Option<SocketAddr> {
    let norm = |s: &str| s.chars().filter(|c| !c.is_whitespace()).collect::<String>().to_lowercase();
    let by_port = game_port.and_then(|gp| list.iter().find(|s| s.game_port == gp));
    let by_name = expect_name.and_then(|n| {
        let n = norm(n);
        if n.is_empty() {
            return None;
        }
        list.iter().find(|s| {
            let m = norm(&s.name);
            !m.is_empty() && (m.contains(&n) || n.contains(&m))
        })
    });
    let chosen = by_port.or(by_name).or_else(|| if list.len() == 1 { list.first() } else { None })?;
    chosen.addr.parse().ok()
}

pub async fn master_lookup(api_key: &str, filter: &str, game_port: Option<u16>, expect_name: Option<&str>, timeout: Duration) -> Option<SocketAddr> {
    let client = reqwest::Client::builder().timeout(timeout).build().ok()?;
    // ключ передаётся через .query(); ошибки не возвращаем наружу, чтобы URL с ключом не попал в текст
    let resp = client
        .get("https://api.steampowered.com/IGameServersService/GetServerList/v1/")
        .query(&[("key", api_key), ("filter", filter), ("limit", "100")])
        .send()
        .await
        .ok()?;
    if !resp.status().is_success() {
        return None;
    }
    let v: serde_json::Value = resp.json().await.ok()?;
    pick_master_server(&parse_master_list(&v), game_port, expect_name)
}

fn usable_key(k: Option<&str>) -> Option<&str> {
    k.map(|s| s.trim()).filter(|s| !s.is_empty() && *s != "•••")
}

/// Query-адрес сервера пачки по данным Rust+ (ip и app port): сначала master list по ключу, затем перебор портов.
pub async fn resolve_query_addr(api_key: Option<&str>, ip: &str, app_port: u16, expect_name: Option<&str>, timeout: Duration) -> Option<SocketAddr> {
    let game_port = app_port.checked_sub(67);
    if let Some(k) = usable_key(api_key) {
        let filter = format!("\\appid\\252490\\addr\\{ip}");
        if let Some(a) = master_lookup(k, &filter, game_port, expect_name, timeout).await {
            return Some(a);
        }
    }
    probe_query_addr(ip, app_port, expect_name, timeout).await
}

/// Фильтр master list по имени сервера. `\` разделяет поля фильтра, поэтому заменяем его на `*` (любой текст).
pub fn name_filter(name: &str) -> String {
    format!("\\appid\\252490\\name_match\\{}", name.trim().replace('\\', "*"))
}

/// Из результатов поиска по имени: точное совпадение имени (без пробелов/регистра), иначе единственный.
pub fn pick_by_exact_name(list: &[MasterServer], name: &str) -> Option<SocketAddr> {
    let norm = |s: &str| s.chars().filter(|c| !c.is_whitespace()).collect::<String>().to_lowercase();
    let n = norm(name);
    if n.is_empty() {
        return None;
    }
    let exact: Vec<&MasterServer> = list.iter().filter(|s| norm(&s.name) == n).collect();
    let chosen = match exact.as_slice() {
        [one] => *one,
        [] if list.len() == 1 => &list[0],
        _ => return None, // несколько одноимённых — не угадываем
    };
    chosen.addr.parse().ok()
}

/// Query-адрес сервера пачки по одному имени: при подключении через ретранслятор Facepunch
/// ip сервера неизвестен, а имя Rust+ отдаёт (AppInfo.name совпадает с именем в master list).
pub async fn resolve_by_name(api_key: Option<&str>, name: &str, timeout: Duration) -> Option<SocketAddr> {
    let k = usable_key(api_key)?;
    let client = reqwest::Client::builder().timeout(timeout).build().ok()?;
    let resp = client
        .get("https://api.steampowered.com/IGameServersService/GetServerList/v1/")
        .query(&[("key", k), ("filter", name_filter(name).as_str()), ("limit", "20")])
        .send()
        .await
        .ok()?;
    if !resp.status().is_success() {
        return None;
    }
    let v: serde_json::Value = resp.json().await.ok()?;
    pick_by_exact_name(&parse_master_list(&v), name)
}

/// Query-адрес по игровому адресу `ip:gameport` (его показывает Steam в gameserverip у игрока).
pub async fn resolve_game_addr(api_key: Option<&str>, game_ip_port: &str, timeout: Duration) -> Option<SocketAddr> {
    let (ip, port) = game_ip_port.rsplit_once(':')?;
    let gp: u16 = port.parse().ok()?;
    if let Some(k) = usable_key(api_key) {
        let filter = format!("\\appid\\252490\\gameaddr\\{game_ip_port}");
        if let Some(a) = master_lookup(k, &filter, Some(gp), None, timeout).await {
            return Some(a);
        }
    }
    // без ключа: игровой порт известен точно, query-порт обычно game+2 (см. probe), пробуем кандидатов от app-порта = game+67
    probe_query_addr(ip, gp.checked_add(67)?, None, timeout).await
}

#[cfg(test)]
mod master_tests {
    use super::*;

    const SAMPLE: &str = r#"{"response":{"servers":[
      {"addr":"80.232.242.125:28017","gameport":28015,"name":"[EU] BLACKTIDE x1000 | Custom Map","appid":252490},
      {"addr":"80.232.242.125:28117","gameport":28115,"name":"Second Box","appid":252490}]}}"#;

    #[test]
    fn by_name() {
        let v: serde_json::Value = serde_json::from_str(SAMPLE).unwrap();
        let l = parse_master_list(&v);
        assert_eq!(pick_by_exact_name(&l, "second  box").map(|a| a.port()), Some(28117));
        assert_eq!(pick_by_exact_name(&l, "Second"), None); // частичное при нескольких — нет
        assert_eq!(pick_by_exact_name(&l[..1], "что угодно").map(|a| a.port()), Some(28017));
        assert_eq!(name_filter(" Blood Rust - Black [nolimit, x2] "), "\\appid\\252490\\name_match\\Blood Rust - Black [nolimit, x2]");
        assert_eq!(name_filter("a\\b"), "\\appid\\252490\\name_match\\a*b");
    }

    #[test]
    fn parses_real_shape() {
        let v: serde_json::Value = serde_json::from_str(SAMPLE).unwrap();
        let l = parse_master_list(&v);
        assert_eq!(l.len(), 2);
        assert_eq!(l[0].addr, "80.232.242.125:28017");
        assert_eq!(l[0].game_port, 28015);
    }

    #[test]
    fn picks_by_game_port_then_name_then_single() {
        let v: serde_json::Value = serde_json::from_str(SAMPLE).unwrap();
        let l = parse_master_list(&v);
        assert_eq!(pick_master_server(&l, Some(28115), None).unwrap().port(), 28117);
        assert_eq!(pick_master_server(&l, Some(1), Some("second box")).unwrap().port(), 28117);
        assert!(pick_master_server(&l, None, None).is_none()); // двусмысленно
        assert_eq!(pick_master_server(&l[..1], None, None).unwrap().port(), 28017);
    }

    #[test]
    fn empty_or_bad_json_is_none() {
        let v: serde_json::Value = serde_json::from_str("{}").unwrap();
        assert!(parse_master_list(&v).is_empty());
        assert!(pick_master_server(&[], Some(28015), None).is_none());
    }

    #[test]
    fn masked_key_is_unusable() {
        assert!(usable_key(Some("•••")).is_none());
        assert!(usable_key(Some("  ")).is_none());
        assert_eq!(usable_key(Some(" abc ")), Some("abc"));
    }
}