//! Ручное описание подмножества rustplus.proto (liamcottle/rustplus.js) через prost derive.
//! Номера полей сверены с rustplus.proto из репозитория liamcottle/rustplus.js (master).
//! Секретный playerToken живёт только в AppRequest и никуда не логируется.

use prost::Message;

#[derive(Clone, PartialEq, Message)]
pub struct Vector4 {
    #[prost(float, tag = "1")]
    pub x: f32,
    #[prost(float, tag = "2")]
    pub y: f32,
    #[prost(float, tag = "3")]
    pub z: f32,
    #[prost(float, tag = "4")]
    pub w: f32,
}

// ---------- запросы ----------

#[derive(Clone, PartialEq, Message)]
pub struct AppEmpty {}

#[derive(Clone, PartialEq, Message)]
pub struct AppSendMessage {
    #[prost(string, tag = "1")]
    pub message: String,
}

#[derive(Clone, PartialEq, Message)]
pub struct AppSetEntityValue {
    #[prost(bool, tag = "1")]
    pub value: bool,
}

#[derive(Clone, PartialEq, Message)]
pub struct AppFlag {
    #[prost(bool, tag = "1")]
    pub value: bool,
}

#[derive(Clone, PartialEq, Message)]
pub struct AppRequest {
    #[prost(uint32, tag = "1")]
    pub seq: u32,
    #[prost(uint64, tag = "2")]
    pub player_id: u64,
    #[prost(int32, tag = "3")]
    pub player_token: i32,
    #[prost(uint32, optional, tag = "4")]
    pub entity_id: Option<u32>,
    #[prost(message, optional, tag = "8")]
    pub get_info: Option<AppEmpty>,
    #[prost(message, optional, tag = "9")]
    pub get_time: Option<AppEmpty>,
    #[prost(message, optional, tag = "10")]
    pub get_map: Option<AppEmpty>,
    #[prost(message, optional, tag = "11")]
    pub get_team_info: Option<AppEmpty>,
    #[prost(message, optional, tag = "12")]
    pub get_team_chat: Option<AppEmpty>,
    #[prost(message, optional, tag = "13")]
    pub send_team_message: Option<AppSendMessage>,
    #[prost(message, optional, tag = "14")]
    pub get_entity_info: Option<AppEmpty>,
    #[prost(message, optional, tag = "15")]
    pub set_entity_value: Option<AppSetEntityValue>,
    #[prost(message, optional, tag = "16")]
    pub check_subscription: Option<AppEmpty>,
    #[prost(message, optional, tag = "17")]
    pub set_subscription: Option<AppFlag>,
    #[prost(message, optional, tag = "18")]
    pub get_map_markers: Option<AppEmpty>,
}

// ---------- ответы ----------

#[derive(Clone, PartialEq, Message)]
pub struct AppSuccess {}

#[derive(Clone, PartialEq, Message)]
pub struct AppError {
    #[prost(string, tag = "1")]
    pub error: String,
}

#[derive(Clone, PartialEq, Message)]
pub struct AppInfo {
    #[prost(string, tag = "1")]
    pub name: String,
    #[prost(string, tag = "2")]
    pub header_image: String,
    #[prost(string, tag = "3")]
    pub url: String,
    #[prost(string, tag = "4")]
    pub map: String,
    #[prost(uint32, tag = "5")]
    pub map_size: u32,
    #[prost(uint32, tag = "6")]
    pub wipe_time: u32,
    #[prost(uint32, tag = "7")]
    pub players: u32,
    #[prost(uint32, tag = "8")]
    pub max_players: u32,
    #[prost(uint32, tag = "9")]
    pub queued_players: u32,
    #[prost(uint32, optional, tag = "10")]
    pub seed: Option<u32>,
    #[prost(uint32, optional, tag = "11")]
    pub salt: Option<u32>,
}

#[derive(Clone, PartialEq, Message)]
pub struct AppTime {
    #[prost(float, tag = "1")]
    pub day_length_minutes: f32,
    #[prost(float, tag = "2")]
    pub time_scale: f32,
    #[prost(float, tag = "3")]
    pub sunrise: f32,
    #[prost(float, tag = "4")]
    pub sunset: f32,
    #[prost(float, tag = "5")]
    pub time: f32,
}

#[derive(Clone, PartialEq, Message)]
pub struct Monument {
    #[prost(string, tag = "1")]
    pub token: String,
    #[prost(float, tag = "2")]
    pub x: f32,
    #[prost(float, tag = "3")]
    pub y: f32,
}

#[derive(Clone, PartialEq, Message)]
pub struct AppMap {
    #[prost(uint32, tag = "1")]
    pub width: u32,
    #[prost(uint32, tag = "2")]
    pub height: u32,
    #[prost(bytes = "vec", tag = "3")]
    pub jpg_image: Vec<u8>,
    #[prost(int32, tag = "4")]
    pub ocean_margin: i32,
    #[prost(message, repeated, tag = "5")]
    pub monuments: Vec<Monument>,
    #[prost(string, optional, tag = "6")]
    pub background: Option<String>,
}

#[derive(Clone, PartialEq, Message)]
pub struct TeamMember {
    #[prost(uint64, tag = "1")]
    pub steam_id: u64,
    #[prost(string, tag = "2")]
    pub name: String,
    #[prost(float, tag = "3")]
    pub x: f32,
    #[prost(float, tag = "4")]
    pub y: f32,
    #[prost(bool, tag = "5")]
    pub is_online: bool,
    #[prost(uint32, tag = "6")]
    pub spawn_time: u32,
    #[prost(bool, tag = "7")]
    pub is_alive: bool,
    #[prost(uint32, tag = "8")]
    pub death_time: u32,
}

#[derive(Clone, PartialEq, Message)]
pub struct TeamNote {
    #[prost(int32, tag = "2")]
    pub r#type: i32,
    #[prost(float, tag = "3")]
    pub x: f32,
    #[prost(float, tag = "4")]
    pub y: f32,
}

#[derive(Clone, PartialEq, Message)]
pub struct AppTeamInfo {
    #[prost(uint64, tag = "1")]
    pub leader_steam_id: u64,
    #[prost(message, repeated, tag = "2")]
    pub members: Vec<TeamMember>,
    #[prost(message, repeated, tag = "3")]
    pub map_notes: Vec<TeamNote>,
    #[prost(message, repeated, tag = "4")]
    pub leader_map_notes: Vec<TeamNote>,
}

#[derive(Clone, PartialEq, Message)]
pub struct AppTeamMessage {
    #[prost(uint64, tag = "1")]
    pub steam_id: u64,
    #[prost(string, tag = "2")]
    pub name: String,
    #[prost(string, tag = "3")]
    pub message: String,
    #[prost(string, tag = "4")]
    pub color: String,
    #[prost(uint32, tag = "5")]
    pub time: u32,
}

#[derive(Clone, PartialEq, Message)]
pub struct AppTeamChat {
    #[prost(message, repeated, tag = "1")]
    pub messages: Vec<AppTeamMessage>,
}

#[derive(Clone, PartialEq, Message)]
pub struct EntityItem {
    #[prost(int32, tag = "1")]
    pub item_id: i32,
    #[prost(int32, tag = "2")]
    pub quantity: i32,
    #[prost(bool, tag = "3")]
    pub item_is_blueprint: bool,
}

#[derive(Clone, PartialEq, Message)]
pub struct AppEntityPayload {
    #[prost(bool, optional, tag = "1")]
    pub value: Option<bool>,
    #[prost(message, repeated, tag = "2")]
    pub items: Vec<EntityItem>,
    #[prost(int32, tag = "3")]
    pub capacity: i32,
    #[prost(bool, tag = "4")]
    pub has_protection: bool,
    #[prost(uint32, tag = "5")]
    pub protection_expiry: u32,
}

#[derive(Clone, PartialEq, Message)]
pub struct AppEntityInfo {
    #[prost(int32, tag = "1")]
    pub r#type: i32,
    #[prost(message, optional, tag = "3")]
    pub payload: Option<AppEntityPayload>,
}

#[derive(Clone, PartialEq, Message)]
pub struct SellOrder {
    #[prost(int32, tag = "1")]
    pub item_id: i32,
    #[prost(int32, tag = "2")]
    pub quantity: i32,
    #[prost(int32, tag = "3")]
    pub currency_id: i32,
    #[prost(int32, tag = "4")]
    pub cost_per_item: i32,
    #[prost(int32, tag = "5")]
    pub amount_in_stock: i32,
    #[prost(bool, tag = "6")]
    pub item_is_blueprint: bool,
    #[prost(bool, tag = "7")]
    pub currency_is_blueprint: bool,
    #[prost(float, tag = "8")]
    pub item_condition: f32,
    #[prost(float, tag = "9")]
    pub item_condition_max: f32,
}

#[derive(Clone, PartialEq, Message)]
pub struct AppMarker {
    #[prost(uint32, tag = "1")]
    pub id: u32,
    #[prost(int32, tag = "2")]
    pub r#type: i32,
    #[prost(float, tag = "3")]
    pub x: f32,
    #[prost(float, tag = "4")]
    pub y: f32,
    #[prost(uint64, tag = "5")]
    pub steam_id: u64,
    #[prost(float, tag = "6")]
    pub rotation: f32,
    #[prost(float, tag = "7")]
    pub radius: f32,
    #[prost(message, optional, tag = "8")]
    pub color1: Option<Vector4>,
    #[prost(message, optional, tag = "9")]
    pub color2: Option<Vector4>,
    #[prost(float, tag = "10")]
    pub alpha: f32,
    #[prost(string, optional, tag = "11")]
    pub name: Option<String>,
    #[prost(bool, tag = "12")]
    pub out_of_stock: bool,
    #[prost(message, repeated, tag = "13")]
    pub sell_orders: Vec<SellOrder>,
}

#[derive(Clone, PartialEq, Message)]
pub struct AppMapMarkers {
    #[prost(message, repeated, tag = "1")]
    pub markers: Vec<AppMarker>,
}

/// AppMarkerType из rustplus.proto.
pub mod marker_type {
    pub const VENDING_MACHINE: i32 = 3;
    pub const CH47: i32 = 4;
    pub const CARGO_SHIP: i32 = 5;
    pub const CRATE: i32 = 6;
    pub const PATROL_HELICOPTER: i32 = 8;
}

#[derive(Clone, PartialEq, Message)]
pub struct AppResponse {
    #[prost(uint32, tag = "1")]
    pub seq: u32,
    #[prost(message, optional, tag = "4")]
    pub success: Option<AppSuccess>,
    #[prost(message, optional, tag = "5")]
    pub error: Option<AppError>,
    #[prost(message, optional, tag = "6")]
    pub info: Option<AppInfo>,
    #[prost(message, optional, tag = "7")]
    pub time: Option<AppTime>,
    #[prost(message, optional, tag = "8")]
    pub map: Option<AppMap>,
    #[prost(message, optional, tag = "9")]
    pub team_info: Option<AppTeamInfo>,
    #[prost(message, optional, tag = "10")]
    pub team_chat: Option<AppTeamChat>,
    #[prost(message, optional, tag = "11")]
    pub entity_info: Option<AppEntityInfo>,
    #[prost(message, optional, tag = "12")]
    pub flag: Option<AppFlag>,
    #[prost(message, optional, tag = "13")]
    pub map_markers: Option<AppMapMarkers>,
}

// ---------- broadcast ----------

#[derive(Clone, PartialEq, Message)]
pub struct AppTeamChanged {
    #[prost(uint64, tag = "1")]
    pub player_id: u64,
    #[prost(message, optional, tag = "2")]
    pub team_info: Option<AppTeamInfo>,
}

#[derive(Clone, PartialEq, Message)]
pub struct AppNewTeamMessage {
    #[prost(message, optional, tag = "1")]
    pub message: Option<AppTeamMessage>,
}

#[derive(Clone, PartialEq, Message)]
pub struct AppEntityChanged {
    #[prost(uint32, tag = "1")]
    pub entity_id: u32,
    #[prost(message, optional, tag = "2")]
    pub payload: Option<AppEntityPayload>,
}

#[derive(Clone, PartialEq, Message)]
pub struct AppBroadcast {
    #[prost(message, optional, tag = "4")]
    pub team_changed: Option<AppTeamChanged>,
    #[prost(message, optional, tag = "5")]
    pub team_message: Option<AppNewTeamMessage>,
    #[prost(message, optional, tag = "6")]
    pub entity_changed: Option<AppEntityChanged>,
}

#[derive(Clone, PartialEq, Message)]
pub struct AppMessage {
    #[prost(message, optional, tag = "1")]
    pub response: Option<AppResponse>,
    #[prost(message, optional, tag = "2")]
    pub broadcast: Option<AppBroadcast>,
}
