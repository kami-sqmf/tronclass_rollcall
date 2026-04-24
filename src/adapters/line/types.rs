//! Line Bot API 型別定義
//!
//! 包含 Line Messaging API 的所有資料結構：
//! - Webhook 事件（`WebhookPayload`、`Event`、`Message` 等）
//! - API 請求 Body（發送訊息用）
//! - API 回應
//!
//! 參考：https://developers.line.biz/en/reference/messaging-api/

use serde::{Deserialize, Serialize};

// ─── Webhook Payload ──────────────────────────────────────────────────────────

/// Line Webhook 的頂層 Payload
///
/// Line 伺服器以 POST 請求將事件推送到 Webhook URL，
/// Body 為此結構的 JSON 序列化。
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct WebhookPayload {
    /// 發送此 Webhook 的 Channel 的 destination（Channel ID）
    pub destination: String,

    /// 事件列表（一次可能包含多個事件）
    pub events: Vec<Event>,
}

// ─── Event ────────────────────────────────────────────────────────────────────

/// Line Webhook 事件
#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(tag = "type", rename_all = "camelCase")]
pub enum Event {
    /// 使用者傳送訊息
    Message(MessageEvent),

    /// 使用者追蹤（加入好友）
    Follow(FollowEvent),

    /// 使用者取消追蹤（封鎖或刪除）
    Unfollow(UnfollowEvent),

    /// 加入群組或多人聊天室
    Join(JoinEvent),

    /// 離開群組或多人聊天室
    Leave(LeaveEvent),

    /// Postback 事件（使用者點擊 Postback 按鈕）
    Postback(PostbackEvent),

    /// Beacon 事件
    Beacon(BeaconEvent),

    /// 成員加入群組
    MemberJoined(MemberJoinedEvent),

    /// 成員離開群組
    MemberLeft(MemberLeftEvent),

    /// 其他未知事件
    #[serde(other)]
    Unknown,
}

impl Event {
    /// 取得事件的共用欄位（reply token、timestamp、source）
    pub fn common(&self) -> Option<&EventCommon> {
        match self {
            Event::Message(e) => Some(&e.common),
            Event::Follow(e) => Some(&e.common),
            Event::Unfollow(e) => Some(&e.common),
            Event::Join(e) => Some(&e.common),
            Event::Leave(e) => Some(&e.common),
            Event::Postback(e) => Some(&e.common),
            Event::Beacon(e) => Some(&e.common),
            Event::MemberJoined(e) => Some(&e.common),
            Event::MemberLeft(e) => Some(&e.common),
            Event::Unknown => None,
        }
    }

    /// 取得 reply token（只有部分事件有）
    pub fn reply_token(&self) -> Option<&str> {
        match self {
            Event::Message(e) => e.common.reply_token.as_deref(),
            Event::Follow(e) => e.common.reply_token.as_deref(),
            Event::Join(e) => e.common.reply_token.as_deref(),
            Event::Postback(e) => e.common.reply_token.as_deref(),
            Event::Beacon(e) => e.common.reply_token.as_deref(),
            Event::MemberJoined(e) => e.common.reply_token.as_deref(),
            _ => None,
        }
    }

    /// 取得發送者的 User ID（若有）
    pub fn user_id(&self) -> Option<&str> {
        self.common().and_then(|c| c.source.user_id())
    }

    /// 是否來自指定使用者
    pub fn is_from_user(&self, user_id: &str) -> bool {
        self.user_id().map(|id| id == user_id).unwrap_or(false)
    }

    /// 若為訊息事件，取得訊息內容
    pub fn as_message_event(&self) -> Option<&MessageEvent> {
        match self {
            Event::Message(e) => Some(e),
            _ => None,
        }
    }
}

// ─── EventCommon ──────────────────────────────────────────────────────────────

/// 所有事件共用的欄位
#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct EventCommon {
    /// 事件的 webhook 事件 ID
    #[serde(default)]
    pub webhook_event_id: String,

    /// Reply token（用於回覆，只有在 isRedelivery=false 時有效）
    #[serde(default)]
    pub reply_token: Option<String>,

    /// 事件發生時間（Unix 毫秒時間戳）
    pub timestamp: i64,

    /// 事件來源
    pub source: EventSource,

    /// 是否為重新投遞的事件
    #[serde(default)]
    pub delivery_context: DeliveryContext,
}

/// 事件投遞上下文
#[derive(Debug, Clone, Default, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct DeliveryContext {
    /// 是否為重新投遞（若為 true，reply_token 無效）
    #[serde(default)]
    pub is_redelivery: bool,
}

// ─── EventSource ──────────────────────────────────────────────────────────────

/// 事件來源（使用者、群組或多人聊天室）
#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(tag = "type", rename_all = "camelCase")]
pub enum EventSource {
    /// 來自個人對話
    User {
        #[serde(rename = "userId")]
        user_id: String,
    },

    /// 來自群組
    Group {
        #[serde(rename = "groupId")]
        group_id: String,
        #[serde(rename = "userId", default)]
        user_id: Option<String>,
    },

    /// 來自多人聊天室
    Room {
        #[serde(rename = "roomId")]
        room_id: String,
        #[serde(rename = "userId", default)]
        user_id: Option<String>,
    },
}

impl EventSource {
    /// 取得 User ID（若存在）
    pub fn user_id(&self) -> Option<&str> {
        match self {
            EventSource::User { user_id } => Some(user_id),
            EventSource::Group {
                user_id: Some(uid), ..
            } => Some(uid),
            EventSource::Room {
                user_id: Some(uid), ..
            } => Some(uid),
            _ => None,
        }
    }

    /// 取得聊天室 ID（用於推送訊息的目標）
    ///
    /// - 個人對話：返回 user_id
    /// - 群組：返回 group_id
    /// - 多人聊天室：返回 room_id
    pub fn chat_id(&self) -> &str {
        match self {
            EventSource::User { user_id } => user_id,
            EventSource::Group { group_id, .. } => group_id,
            EventSource::Room { room_id, .. } => room_id,
        }
    }

    /// 是否為個人對話
    pub fn is_user(&self) -> bool {
        matches!(self, EventSource::User { .. })
    }

    /// 是否為群組
    pub fn is_group(&self) -> bool {
        matches!(self, EventSource::Group { .. })
    }
}

// ─── 各種事件型別 ─────────────────────────────────────────────────────────────

/// 訊息事件
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct MessageEvent {
    #[serde(flatten)]
    pub common: EventCommon,

    /// 訊息內容
    pub message: LineMessage,
}

/// 追蹤事件
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct FollowEvent {
    #[serde(flatten)]
    pub common: EventCommon,
}

/// 取消追蹤事件
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct UnfollowEvent {
    #[serde(flatten)]
    pub common: EventCommon,
}

/// 加入群組事件
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct JoinEvent {
    #[serde(flatten)]
    pub common: EventCommon,
}

/// 離開群組事件
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct LeaveEvent {
    #[serde(flatten)]
    pub common: EventCommon,
}

/// Postback 事件
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct PostbackEvent {
    #[serde(flatten)]
    pub common: EventCommon,

    /// Postback 資料
    pub postback: PostbackData,
}

/// Postback 資料
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct PostbackData {
    /// Postback 字串（按鈕設定的 data 欄位）
    pub data: String,

    /// 使用者填寫的文字（若 action 為 datetimepicker 或 timepicker）
    #[serde(default)]
    pub params: Option<serde_json::Value>,
}

/// Beacon 事件
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct BeaconEvent {
    #[serde(flatten)]
    pub common: EventCommon,

    pub beacon: BeaconData,
}

/// Beacon 資料
#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct BeaconData {
    pub hwid: String,
    pub r#type: String,
    #[serde(default)]
    pub dm: Option<String>,
}

/// 成員加入事件
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct MemberJoinedEvent {
    #[serde(flatten)]
    pub common: EventCommon,

    pub joined: MemberList,
}

/// 成員離開事件
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct MemberLeftEvent {
    #[serde(flatten)]
    pub common: EventCommon,

    pub left: MemberList,
}

/// 成員列表
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct MemberList {
    pub members: Vec<MemberSource>,
}

/// 成員來源
#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(tag = "type", rename_all = "camelCase")]
pub enum MemberSource {
    User {
        #[serde(rename = "userId")]
        user_id: String,
    },
}

// ─── LineMessage ──────────────────────────────────────────────────────────────

/// Line 訊息內容
#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(tag = "type", rename_all = "camelCase")]
pub enum LineMessage {
    /// 文字訊息
    Text(TextMessage),

    /// 圖片訊息
    Image(MediaMessage),

    /// 影片訊息
    Video(MediaMessage),

    /// 音訊訊息
    Audio(AudioMessage),

    /// 檔案訊息
    File(FileMessage),

    /// 位置訊息
    Location(LocationMessage),

    /// 貼圖訊息
    Sticker(StickerMessage),

    /// 其他未知類型
    #[serde(other)]
    Unknown,
}

impl LineMessage {
    /// 取得訊息 ID
    pub fn id(&self) -> Option<&str> {
        match self {
            LineMessage::Text(m) => Some(&m.id),
            LineMessage::Image(m) => Some(&m.id),
            LineMessage::Video(m) => Some(&m.id),
            LineMessage::Audio(m) => Some(&m.id),
            LineMessage::File(m) => Some(&m.id),
            LineMessage::Location(m) => Some(&m.id),
            LineMessage::Sticker(m) => Some(&m.id),
            LineMessage::Unknown => None,
        }
    }

    /// 若為文字訊息，取得文字內容
    pub fn as_text(&self) -> Option<&str> {
        match self {
            LineMessage::Text(m) => Some(&m.text),
            _ => None,
        }
    }

    /// 是否為文字訊息
    pub fn is_text(&self) -> bool {
        matches!(self, LineMessage::Text(_))
    }
}

/// 文字訊息
#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct TextMessage {
    /// 訊息 ID
    pub id: String,

    /// 文字內容
    pub text: String,

    /// 表情符號列表
    #[serde(default)]
    pub emojis: Vec<EmojiInfo>,

    /// 提及（mention）資訊
    #[serde(default)]
    pub mention: Option<MentionInfo>,

    /// 引用的訊息
    #[serde(default)]
    pub quoted_message_id: Option<String>,
}

/// 表情符號資訊
#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct EmojiInfo {
    pub index: u32,
    pub length: u32,
    pub product_id: String,
    pub emoji_id: String,
}

/// Mention 資訊
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct MentionInfo {
    pub mentionees: Vec<Mentionee>,
}

/// 被提及的對象
#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Mentionee {
    pub index: u32,
    pub length: u32,
    #[serde(rename = "userId", default)]
    pub user_id: Option<String>,
    #[serde(rename = "type", default)]
    pub mentionee_type: String,
}

/// 媒體訊息（圖片、影片）
#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct MediaMessage {
    pub id: String,
    #[serde(default)]
    pub content_provider: Option<ContentProvider>,
}

/// 音訊訊息
#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct AudioMessage {
    pub id: String,
    pub duration: Option<u64>,
    #[serde(default)]
    pub content_provider: Option<ContentProvider>,
}

/// 檔案訊息
#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct FileMessage {
    pub id: String,
    pub file_name: String,
    pub file_size: u64,
}

/// 位置訊息
#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct LocationMessage {
    pub id: String,
    #[serde(default)]
    pub title: Option<String>,
    #[serde(default)]
    pub address: Option<String>,
    pub latitude: f64,
    pub longitude: f64,
}

/// 貼圖訊息
#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct StickerMessage {
    pub id: String,
    pub package_id: String,
    pub sticker_id: String,
    #[serde(default)]
    pub sticker_resource_type: Option<String>,
}

/// 媒體內容提供者資訊
#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ContentProvider {
    pub r#type: String,
    #[serde(default)]
    pub original_content_url: Option<String>,
    #[serde(default)]
    pub preview_image_url: Option<String>,
}

// ─── 發送訊息 API ─────────────────────────────────────────────────────────────

/// Reply Message API 請求 Body
///
/// `POST https://api.line.me/v2/bot/message/reply`
#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ReplyMessageRequest {
    /// Reply token（從事件中取得）
    pub reply_token: String,

    /// 要回覆的訊息列表（最多 5 則）
    pub messages: Vec<SendMessage>,

    /// 是否為通知訊息（不計入聊天記錄）
    #[serde(skip_serializing_if = "Option::is_none")]
    pub notification_disabled: Option<bool>,
}

impl ReplyMessageRequest {
    /// 建立單則文字回覆
    #[allow(dead_code)]
    pub fn text(reply_token: impl Into<String>, text: impl Into<String>) -> Self {
        Self {
            reply_token: reply_token.into(),
            messages: vec![SendMessage::text(text)],
            notification_disabled: None,
        }
    }

    /// 建立多則訊息回覆
    pub fn messages(reply_token: impl Into<String>, messages: Vec<SendMessage>) -> Self {
        Self {
            reply_token: reply_token.into(),
            messages,
            notification_disabled: None,
        }
    }
}

/// Push Message API 請求 Body
///
/// `POST https://api.line.me/v2/bot/message/push`
#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct PushMessageRequest {
    /// 目標 ID（User ID / Group ID / Room ID）
    pub to: String,

    /// 要發送的訊息列表（最多 5 則）
    pub messages: Vec<SendMessage>,

    /// 是否為通知訊息
    #[serde(skip_serializing_if = "Option::is_none")]
    pub notification_disabled: Option<bool>,

    /// 自訂聚合單位（用於統計）
    #[serde(skip_serializing_if = "Option::is_none")]
    pub custom_aggregation_units: Option<Vec<String>>,
}

impl PushMessageRequest {
    /// 建立單則文字推送
    #[allow(dead_code)]
    pub fn text(to: impl Into<String>, text: impl Into<String>) -> Self {
        Self {
            to: to.into(),
            messages: vec![SendMessage::text(text)],
            notification_disabled: None,
            custom_aggregation_units: None,
        }
    }

    /// 建立多則訊息推送
    pub fn messages(to: impl Into<String>, messages: Vec<SendMessage>) -> Self {
        Self {
            to: to.into(),
            messages,
            notification_disabled: None,
            custom_aggregation_units: None,
        }
    }
}

/// Broadcast Message API 請求 Body
///
/// `POST https://api.line.me/v2/bot/message/broadcast`
#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct BroadcastMessageRequest {
    pub messages: Vec<SendMessage>,

    #[serde(skip_serializing_if = "Option::is_none")]
    pub notification_disabled: Option<bool>,
}

/// 要發送的訊息（可序列化為 Line API 格式）
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "camelCase")]
pub enum SendMessage {
    /// 文字訊息
    Text(SendTextMessage),

    /// 圖片訊息
    Image(SendImageMessage),

    /// 影片訊息
    Video(SendVideoMessage),

    /// 音訊訊息
    Audio(SendAudioMessage),

    /// 位置訊息
    Location(SendLocationMessage),

    /// 貼圖訊息
    Sticker(SendStickerMessage),

    /// Flex 訊息（富文字）
    Flex(SendFlexMessage),

    /// Template 訊息
    Template(SendTemplateMessage),
}

impl SendMessage {
    /// 建立文字訊息
    pub fn text(text: impl Into<String>) -> Self {
        SendMessage::Text(SendTextMessage {
            text: text.into(),
            emojis: None,
            quote_token: None,
        })
    }

    /// 建立圖片訊息
    pub fn image(
        original_content_url: impl Into<String>,
        preview_image_url: impl Into<String>,
    ) -> Self {
        SendMessage::Image(SendImageMessage {
            original_content_url: original_content_url.into(),
            preview_image_url: preview_image_url.into(),
        })
    }

    /// 建立位置訊息
    pub fn location(
        title: impl Into<String>,
        address: impl Into<String>,
        latitude: f64,
        longitude: f64,
    ) -> Self {
        SendMessage::Location(SendLocationMessage {
            title: title.into(),
            address: address.into(),
            latitude,
            longitude,
        })
    }
}

/// 要發送的文字訊息
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SendTextMessage {
    /// 文字內容（最多 5000 字元）
    pub text: String,

    /// 自訂表情符號
    #[serde(skip_serializing_if = "Option::is_none")]
    pub emojis: Option<Vec<SendEmoji>>,

    /// 要引用的訊息 token
    #[serde(skip_serializing_if = "Option::is_none")]
    pub quote_token: Option<String>,
}

/// 要發送的自訂表情符號
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SendEmoji {
    pub index: u32,
    pub product_id: String,
    pub emoji_id: String,
}

/// 要發送的圖片訊息
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SendImageMessage {
    /// 原始圖片 URL（HTTPS，最大 10MB）
    pub original_content_url: String,

    /// 預覽圖片 URL（HTTPS，最大 1MB）
    pub preview_image_url: String,
}

/// 要發送的影片訊息
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SendVideoMessage {
    pub original_content_url: String,
    pub preview_image_url: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tracking_id: Option<String>,
}

/// 要發送的音訊訊息
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SendAudioMessage {
    pub original_content_url: String,
    pub duration: u64,
}

/// 要發送的位置訊息
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SendLocationMessage {
    pub title: String,
    pub address: String,
    pub latitude: f64,
    pub longitude: f64,
}

/// 要發送的貼圖訊息
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SendStickerMessage {
    pub package_id: String,
    pub sticker_id: String,
}

/// 要發送的 Flex 訊息
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SendFlexMessage {
    pub alt_text: String,
    pub contents: serde_json::Value,
}

/// 要發送的 Template 訊息
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SendTemplateMessage {
    pub alt_text: String,
    pub template: serde_json::Value,
}

// ─── API 回應 ─────────────────────────────────────────────────────────────────

/// Line API 的標準成功回應（空 JSON 物件）
#[derive(Debug, Deserialize)]
pub struct LineApiResponse {
    #[serde(default)]
    pub message: Option<String>,

    #[serde(default)]
    pub details: Option<Vec<LineApiErrorDetail>>,
}

/// Line API 錯誤詳情
#[derive(Debug, Deserialize)]
pub struct LineApiErrorDetail {
    pub message: String,
    pub property: Option<String>,
}

/// Line API 錯誤回應
#[derive(Debug, Deserialize)]
pub struct LineApiError {
    pub message: String,
    #[serde(default)]
    pub details: Vec<LineApiErrorDetail>,
}

impl std::fmt::Display for LineApiError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "Line API Error: {}", self.message)?;
        for detail in &self.details {
            if let Some(prop) = &detail.property {
                write!(f, " [{prop}: {}]", detail.message)?;
            } else {
                write!(f, " [{}]", detail.message)?;
            }
        }
        Ok(())
    }
}

// ─── 測試 ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    // ── WebhookPayload 反序列化 ───────────────────────────────────────────────

    #[test]
    fn test_deserialize_text_message_event() {
        let json = r#"{
            "destination": "Uxxxxxxxxxx",
            "events": [
                {
                    "type": "message",
                    "webhookEventId": "01FZ74A0TDDPYRVKNK77XKC3ZR",
                    "replyToken": "nHuyWiB7yP5Zw52FIkcQobQuGDXCTA",
                    "timestamp": 1462629479859,
                    "deliveryContext": { "isRedelivery": false },
                    "source": {
                        "type": "user",
                        "userId": "U4af4980629..."
                    },
                    "message": {
                        "type": "text",
                        "id": "444573844083572737",
                        "text": "Hello, world!"
                    }
                }
            ]
        }"#;

        let payload: WebhookPayload = serde_json::from_str(json).unwrap();
        assert_eq!(payload.destination, "Uxxxxxxxxxx");
        assert_eq!(payload.events.len(), 1);

        let event = &payload.events[0];
        assert!(matches!(event, Event::Message(_)));

        if let Event::Message(msg_event) = event {
            assert_eq!(
                msg_event.common.reply_token.as_deref(),
                Some("nHuyWiB7yP5Zw52FIkcQobQuGDXCTA")
            );
            assert!(msg_event.message.is_text());
            assert_eq!(msg_event.message.as_text(), Some("Hello, world!"));
        }
    }

    #[test]
    fn test_deserialize_follow_event() {
        let json = r#"{
            "destination": "Uxx",
            "events": [{
                "type": "follow",
                "timestamp": 1462629479859,
                "replyToken": "token123",
                "deliveryContext": { "isRedelivery": false },
                "source": { "type": "user", "userId": "Uabc123" }
            }]
        }"#;

        let payload: WebhookPayload = serde_json::from_str(json).unwrap();
        assert!(matches!(payload.events[0], Event::Follow(_)));
    }

    #[test]
    fn test_deserialize_unknown_event() {
        let json = r#"{
            "destination": "Uxx",
            "events": [{
                "type": "some_future_event_type",
                "timestamp": 1462629479859,
                "source": { "type": "user", "userId": "U123" }
            }]
        }"#;

        let payload: WebhookPayload = serde_json::from_str(json).unwrap();
        assert!(matches!(payload.events[0], Event::Unknown));
    }

    // ── EventSource ───────────────────────────────────────────────────────────

    #[test]
    fn test_event_source_user_id() {
        let src = EventSource::User {
            user_id: "U123".to_string(),
        };
        assert_eq!(src.user_id(), Some("U123"));
        assert_eq!(src.chat_id(), "U123");
        assert!(src.is_user());
        assert!(!src.is_group());
    }

    #[test]
    fn test_event_source_group() {
        let src = EventSource::Group {
            group_id: "G456".to_string(),
            user_id: Some("U789".to_string()),
        };
        assert_eq!(src.user_id(), Some("U789"));
        assert_eq!(src.chat_id(), "G456");
        assert!(!src.is_user());
        assert!(src.is_group());
    }

    #[test]
    fn test_event_source_group_no_user() {
        let src = EventSource::Group {
            group_id: "G456".to_string(),
            user_id: None,
        };
        assert_eq!(src.user_id(), None);
        assert_eq!(src.chat_id(), "G456");
    }

    // ── Event helpers ─────────────────────────────────────────────────────────

    #[test]
    fn test_event_is_from_user() {
        let json = r#"{
            "destination": "Uxx",
            "events": [{
                "type": "message",
                "timestamp": 1462629479859,
                "replyToken": "token",
                "deliveryContext": { "isRedelivery": false },
                "source": { "type": "user", "userId": "Uadmin123" },
                "message": { "type": "text", "id": "1", "text": "hello" }
            }]
        }"#;

        let payload: WebhookPayload = serde_json::from_str(json).unwrap();
        let event = &payload.events[0];

        assert!(event.is_from_user("Uadmin123"));
        assert!(!event.is_from_user("Uother"));
    }

    #[test]
    fn test_event_reply_token() {
        let json = r#"{
            "destination": "Uxx",
            "events": [{
                "type": "message",
                "timestamp": 1462629479859,
                "replyToken": "myreplytoken",
                "deliveryContext": { "isRedelivery": false },
                "source": { "type": "user", "userId": "U123" },
                "message": { "type": "text", "id": "1", "text": "hi" }
            }]
        }"#;

        let payload: WebhookPayload = serde_json::from_str(json).unwrap();
        assert_eq!(payload.events[0].reply_token(), Some("myreplytoken"));
    }

    // ── LineMessage ───────────────────────────────────────────────────────────

    #[test]
    fn test_line_message_as_text() {
        let msg = LineMessage::Text(TextMessage {
            id: "1".to_string(),
            text: "hello".to_string(),
            emojis: vec![],
            mention: None,
            quoted_message_id: None,
        });
        assert!(msg.is_text());
        assert_eq!(msg.as_text(), Some("hello"));
        assert_eq!(msg.id(), Some("1"));
    }

    #[test]
    fn test_line_message_not_text() {
        let msg = LineMessage::Unknown;
        assert!(!msg.is_text());
        assert_eq!(msg.as_text(), None);
        assert_eq!(msg.id(), None);
    }

    // ── SendMessage ───────────────────────────────────────────────────────────

    #[test]
    fn test_send_message_text_serialize() {
        let msg = SendMessage::text("Hello, Line!");
        let json = serde_json::to_value(&msg).unwrap();
        assert_eq!(json["type"], "text");
        assert_eq!(json["text"], "Hello, Line!");
    }

    #[test]
    fn test_push_message_request_serialize() {
        let req = PushMessageRequest::text("Uadmin123", "Test message");
        let json = serde_json::to_value(&req).unwrap();
        assert_eq!(json["to"], "Uadmin123");
        assert_eq!(json["messages"][0]["type"], "text");
        assert_eq!(json["messages"][0]["text"], "Test message");
    }

    #[test]
    fn test_reply_message_request_serialize() {
        let req = ReplyMessageRequest::text("reply_token_abc", "Reply!");
        let json = serde_json::to_value(&req).unwrap();
        assert_eq!(json["replyToken"], "reply_token_abc");
        assert_eq!(json["messages"][0]["text"], "Reply!");
    }

    // ── LineApiError display ──────────────────────────────────────────────────

    #[test]
    fn test_line_api_error_display() {
        let err = LineApiError {
            message: "The request body has 1 error(s)".to_string(),
            details: vec![LineApiErrorDetail {
                message: "May not be empty".to_string(),
                property: Some("messages[0].text".to_string()),
            }],
        };
        let s = err.to_string();
        assert!(s.contains("Line API Error"));
        assert!(s.contains("May not be empty"));
        assert!(s.contains("messages[0].text"));
    }

    #[test]
    fn test_line_api_error_no_details() {
        let err = LineApiError {
            message: "Unauthorized".to_string(),
            details: vec![],
        };
        let s = err.to_string();
        assert!(s.contains("Unauthorized"));
    }

    // ── PostbackEvent ─────────────────────────────────────────────────────────

    #[test]
    fn test_deserialize_postback_event() {
        let json = r#"{
            "destination": "Uxx",
            "events": [{
                "type": "postback",
                "timestamp": 1462629479859,
                "replyToken": "token123",
                "deliveryContext": { "isRedelivery": false },
                "source": { "type": "user", "userId": "U123" },
                "postback": { "data": "action=force_attend" }
            }]
        }"#;

        let payload: WebhookPayload = serde_json::from_str(json).unwrap();
        if let Event::Postback(e) = &payload.events[0] {
            assert_eq!(e.postback.data, "action=force_attend");
        } else {
            panic!("Expected PostbackEvent");
        }
    }

    // ── 多個事件 ──────────────────────────────────────────────────────────────

    #[test]
    fn test_deserialize_multiple_events() {
        let json = r#"{
            "destination": "Uxx",
            "events": [
                {
                    "type": "follow",
                    "timestamp": 1000,
                    "deliveryContext": { "isRedelivery": false },
                    "source": { "type": "user", "userId": "U1" }
                },
                {
                    "type": "message",
                    "timestamp": 2000,
                    "replyToken": "t2",
                    "deliveryContext": { "isRedelivery": false },
                    "source": { "type": "user", "userId": "U2" },
                    "message": { "type": "text", "id": "2", "text": "Hi" }
                }
            ]
        }"#;

        let payload: WebhookPayload = serde_json::from_str(json).unwrap();
        assert_eq!(payload.events.len(), 2);
        assert!(matches!(payload.events[0], Event::Follow(_)));
        assert!(matches!(payload.events[1], Event::Message(_)));
    }
}
