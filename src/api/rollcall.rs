//! 簽到相關的 Payload、Response 資料結構，以及 API 呼叫實作

use miette::{IntoDiagnostic, Result, WrapErr};
use reqwest::StatusCode;
use serde::{Deserialize, Serialize};
use thiserror::Error;
use tracing::{debug, instrument, warn};
use uuid::Uuid;

use super::ApiError;

// ─── 簽到錯誤類型 ─────────────────────────────────────────────────────────────

#[derive(Debug, Error)]
pub enum RollcallError {
    #[error("雷達簽到距離不足：需要移動 {distance:.2} 公尺")]
    RadarDistanceTooFar { distance: f64 },

    #[error("數字簽到爆破未找到正確答案（已嘗試 0000~9999）")]
    BruteForceExhausted,

    #[error("QR code 簽到失敗：{reason}")]
    QrCodeFailed { reason: String },
}

// ─── API 回應資料結構 ──────────────────────────────────────────────────────────

/// `/api/radar/rollcalls` 回應的外層結構
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct RollcallsResponse {
    /// 簽到列表
    #[serde(default)]
    pub rollcalls: Vec<Rollcall>,
}

/// 單一簽到項目
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct Rollcall {
    /// 簽到 ID
    pub rollcall_id: u64,

    /// 課程名稱
    #[serde(default)]
    pub course_title: String,

    /// 教師姓名
    #[serde(default)]
    pub created_by_name: String,

    /// 學系名稱
    #[serde(default)]
    pub department_name: String,

    /// 是否已過期
    #[serde(default)]
    pub is_expired: bool,

    /// 是否為數字簽到
    #[serde(default)]
    pub is_number: bool,

    /// 是否為雷達簽到
    #[serde(default)]
    pub is_radar: bool,

    /// 簽到狀態（`"absent"` / `"on_call_fine"` / 其他）
    #[serde(default)]
    pub status: String,

    /// 簽到結果狀態
    #[serde(default)]
    pub rollcall_status: String,

    /// 是否已得分
    #[serde(default)]
    pub scored: bool,
}

impl Rollcall {
    /// 是否需要簽到（狀態為 absent 且未過期）
    pub fn needs_attendance(&self) -> bool {
        self.status == "absent" && !self.is_expired
    }

    /// 是否已簽到成功
    pub fn is_attended(&self) -> bool {
        self.status == "on_call_fine"
    }

    /// 判斷簽到類型
    pub fn attendance_type(&self) -> AttendanceType {
        if self.is_number && !self.is_radar {
            AttendanceType::Number
        } else if self.is_radar {
            AttendanceType::Radar
        } else {
            AttendanceType::QrCode
        }
    }

    /// 課程與教師的顯示字串
    pub fn display(&self) -> String {
        format!(
            "[{}] {} (by {})",
            self.rollcall_id, self.course_title, self.created_by_name
        )
    }
}

/// 簽到類型枚舉
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AttendanceType {
    /// 數字簽到（爆破 0000~9999）
    Number,
    /// 雷達簽到（地理位置）
    Radar,
    /// QR Code 簽到
    QrCode,
}

impl std::fmt::Display for AttendanceType {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            AttendanceType::Number => write!(f, "數字簽到"),
            AttendanceType::Radar => write!(f, "雷達簽到"),
            AttendanceType::QrCode => write!(f, "QR Code 簽到"),
        }
    }
}

// ─── 請求 Body 結構 ───────────────────────────────────────────────────────────

/// 數字簽到請求 Body
#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct NumberRollcallBody {
    /// 裝置唯一識別碼
    pub device_id: Uuid,
    /// 4 位數字驗證碼（"0000" ~ "9999"）
    pub number_code: String,
}

impl NumberRollcallBody {
    pub fn new(number_code: impl Into<String>) -> Self {
        Self {
            device_id: Uuid::new_v4(),
            number_code: number_code.into(),
        }
    }
}

/// 雷達簽到請求 Body
#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct RadarRollcallBody {
    /// 位置精確度（公尺）
    pub accuracy: u32,
    /// 高度（公尺）
    pub altitude: i32,
    /// 高度精確度（通常為 null）
    pub altitude_accuracy: Option<f64>,
    /// 裝置唯一識別碼
    pub device_id: Uuid,
    /// 方向（通常為 null）
    pub heading: Option<f64>,
    /// 緯度
    pub latitude: f64,
    /// 經度
    pub longitude: f64,
    /// 速度（通常為 null）
    pub speed: Option<f64>,
}

impl RadarRollcallBody {
    pub fn new(latitude: f64, longitude: f64, accuracy: u32, altitude: i32) -> Self {
        Self {
            accuracy,
            altitude,
            altitude_accuracy: None,
            device_id: Uuid::new_v4(),
            heading: None,
            latitude,
            longitude,
            speed: None,
        }
    }
}

/// QR Code 簽到請求 Body
#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct QrCodeRollcallBody {
    /// QR Code 解析出的 data 字串
    pub data: String,
    /// 裝置唯一識別碼
    pub device_id: Uuid,
}

impl QrCodeRollcallBody {
    pub fn new(data: impl Into<String>) -> Self {
        Self {
            data: data.into(),
            device_id: Uuid::new_v4(),
        }
    }
}

// ─── 回應結果結構 ─────────────────────────────────────────────────────────────

/// 簽到 API 的通用回應
#[derive(Debug, Clone, Deserialize)]
pub struct AttendanceResponse {
    /// 是否成功（部分 API 有此欄位）
    #[serde(default)]
    pub success: Option<bool>,

    /// 錯誤訊息（失敗時）
    #[serde(default)]
    pub message: Option<String>,

    /// 雷達簽到失敗時返回的距離（公尺）
    #[serde(default)]
    pub distance: Option<f64>,

    /// 其他欄位
    #[serde(flatten)]
    pub extra: serde_json::Map<String, serde_json::Value>,
}

impl AttendanceResponse {
    /// 是否為雷達距離不足的錯誤
    pub fn is_radar_distance_error(&self) -> bool {
        self.distance.is_some()
    }
}

/// 簽到操作結果
#[derive(Debug, Clone)]
pub enum AttendanceResult {
    /// 簽到成功
    Success,
    /// 雷達距離不足，附帶從原點到教室的距離
    RadarTooFar { distance: f64 },
    /// 其他失敗（含錯誤訊息）
    Failed { reason: String },
}

impl AttendanceResult {
    pub fn is_success(&self) -> bool {
        matches!(self, AttendanceResult::Success)
    }
}

// ─── ApiClient 簽到方法 ───────────────────────────────────────────────────────

impl super::ApiClient {
    /// `GET /api/radar/rollcalls` — 輪詢獲取簽到列表
    #[instrument(skip(self))]
    pub async fn get_rollcalls(&self) -> Result<Vec<Rollcall>> {
        let url = format!("{}/api/radar/rollcalls", self.base_url);
        debug!(url = %url, "GET /api/radar/rollcalls");

        let resp = self
            .client
            .get(&url)
            .send()
            .await
            .into_diagnostic()
            .wrap_err("GET /api/radar/rollcalls failed")?;

        let rollcalls_resp: RollcallsResponse = self.handle_response(resp).await?;
        Ok(rollcalls_resp.rollcalls)
    }

    /// `PUT /api/rollcall/{id}/answer_number_rollcall` — 數字簽到
    #[instrument(skip(self), fields(rollcall_id = rollcall_id, number_code = %number_code))]
    pub async fn answer_number_rollcall(
        &self,
        rollcall_id: u64,
        number_code: &str,
    ) -> Result<AttendanceResult> {
        let url = format!(
            "{}/api/rollcall/{}/answer_number_rollcall",
            self.base_url, rollcall_id
        );
        let body = NumberRollcallBody::new(number_code);
        debug!(url = %url, code = %number_code, "PUT answer_number_rollcall");

        let resp = self
            .client
            .put(&url)
            .json(&body)
            .send()
            .await
            .into_diagnostic()
            .wrap_err_with(|| {
                format!("PUT /api/rollcall/{rollcall_id}/answer_number_rollcall failed")
            })?;

        let status = resp.status();
        match status {
            StatusCode::OK | StatusCode::CREATED | StatusCode::NO_CONTENT => {
                debug!(code = %number_code, "數字簽到成功");
                Ok(AttendanceResult::Success)
            }
            StatusCode::UNAUTHORIZED | StatusCode::FORBIDDEN => {
                Err(miette::miette!(ApiError::Unauthorized))
            }
            StatusCode::BAD_REQUEST | StatusCode::UNPROCESSABLE_ENTITY => {
                let body_text = resp.text().await.unwrap_or_default();
                debug!(code = %number_code, status = %status, body = %body_text, "數字代碼錯誤");
                Ok(AttendanceResult::Failed {
                    reason: format!("code {number_code} 不正確"),
                })
            }
            other => {
                let body_text = resp.text().await.unwrap_or_default();
                warn!(status = %other, body = %body_text, "數字簽到收到非預期狀態");
                Ok(AttendanceResult::Failed {
                    reason: format!("HTTP {other}: {body_text}"),
                })
            }
        }
    }

    /// `PUT /api/rollcall/{id}/answer` — 雷達簽到
    #[instrument(skip(self), fields(rollcall_id = rollcall_id, lat = latitude, lon = longitude))]
    pub async fn answer_radar_rollcall(
        &self,
        rollcall_id: u64,
        latitude: f64,
        longitude: f64,
        accuracy: u32,
        altitude: i32,
    ) -> Result<AttendanceResult> {
        let url = format!("{}/api/rollcall/{}/answer", self.base_url, rollcall_id);
        let body = RadarRollcallBody::new(latitude, longitude, accuracy, altitude);
        debug!(url = %url, lat = latitude, lon = longitude, "PUT answer (radar rollcall)");

        let resp = self
            .client
            .put(&url)
            .json(&body)
            .send()
            .await
            .into_diagnostic()
            .wrap_err_with(|| format!("PUT /api/rollcall/{rollcall_id}/answer failed"))?;

        let status = resp.status();
        match status {
            StatusCode::OK | StatusCode::CREATED | StatusCode::NO_CONTENT => {
                debug!(lat = latitude, lon = longitude, "雷達簽到成功");
                Ok(AttendanceResult::Success)
            }
            StatusCode::UNAUTHORIZED | StatusCode::FORBIDDEN => {
                Err(miette::miette!(ApiError::Unauthorized))
            }
            StatusCode::BAD_REQUEST
            | StatusCode::UNPROCESSABLE_ENTITY
            | StatusCode::NOT_ACCEPTABLE => {
                let body_text = resp.text().await.unwrap_or_default();
                debug!(status = %status, body = %body_text, "雷達簽到失敗，嘗試解析 distance");
                if let Ok(err_resp) = serde_json::from_str::<AttendanceResponse>(&body_text) {
                    if let Some(dist) = err_resp.distance {
                        return Ok(AttendanceResult::RadarTooFar { distance: dist });
                    }
                }
                Ok(AttendanceResult::Failed {
                    reason: format!("HTTP {status}: {body_text}"),
                })
            }
            other => {
                let body_text = resp.text().await.unwrap_or_default();
                warn!(status = %other, body = %body_text, "雷達簽到收到非預期狀態");
                if let Ok(err_resp) = serde_json::from_str::<AttendanceResponse>(&body_text) {
                    if let Some(dist) = err_resp.distance {
                        return Ok(AttendanceResult::RadarTooFar { distance: dist });
                    }
                }
                Ok(AttendanceResult::Failed {
                    reason: format!("HTTP {other}: {body_text}"),
                })
            }
        }
    }

    /// `PUT /api/rollcall/{id}/answer_qr_rollcall` — QR Code 簽到
    #[instrument(skip(self, data), fields(rollcall_id = rollcall_id))]
    pub async fn answer_qr_rollcall(
        &self,
        rollcall_id: u64,
        data: impl Into<String>,
    ) -> Result<AttendanceResult> {
        let url = format!(
            "{}/api/rollcall/{}/answer_qr_rollcall",
            self.base_url, rollcall_id
        );
        let body = QrCodeRollcallBody::new(data);
        debug!(url = %url, "PUT answer_qr_rollcall");

        let resp = self
            .client
            .put(&url)
            .json(&body)
            .send()
            .await
            .into_diagnostic()
            .wrap_err_with(|| {
                format!("PUT /api/rollcall/{rollcall_id}/answer_qr_rollcall failed")
            })?;

        let status = resp.status();
        match status {
            StatusCode::OK | StatusCode::CREATED | StatusCode::NO_CONTENT => {
                debug!("QR Code 簽到成功");
                Ok(AttendanceResult::Success)
            }
            StatusCode::UNAUTHORIZED | StatusCode::FORBIDDEN => {
                Err(miette::miette!(ApiError::Unauthorized))
            }
            other => {
                let body_text = resp.text().await.unwrap_or_default();
                warn!(status = %other, body = %body_text, "QR Code 簽到失敗");
                Ok(AttendanceResult::Failed {
                    reason: format!("HTTP {other}: {body_text}"),
                })
            }
        }
    }
}

// ─── 測試 ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use uuid::Uuid;

    #[test]
    fn test_rollcall_needs_attendance() {
        let mut rc = make_test_rollcall();
        rc.status = "absent".to_string();
        rc.is_expired = false;
        assert!(rc.needs_attendance());

        rc.is_expired = true;
        assert!(!rc.needs_attendance());

        rc.is_expired = false;
        rc.status = "on_call_fine".to_string();
        assert!(!rc.needs_attendance());
    }

    #[test]
    fn test_rollcall_is_attended() {
        let mut rc = make_test_rollcall();
        rc.status = "on_call_fine".to_string();
        assert!(rc.is_attended());

        rc.status = "absent".to_string();
        assert!(!rc.is_attended());
    }

    #[test]
    fn test_attendance_type_number() {
        let mut rc = make_test_rollcall();
        rc.is_number = true;
        rc.is_radar = false;
        assert_eq!(rc.attendance_type(), AttendanceType::Number);
    }

    #[test]
    fn test_attendance_type_radar() {
        let mut rc = make_test_rollcall();
        rc.is_radar = true;
        // is_radar 優先
        rc.is_number = true;
        assert_eq!(rc.attendance_type(), AttendanceType::Radar);
    }

    #[test]
    fn test_attendance_type_qrcode() {
        let mut rc = make_test_rollcall();
        rc.is_number = false;
        rc.is_radar = false;
        assert_eq!(rc.attendance_type(), AttendanceType::QrCode);
    }

    #[test]
    fn test_number_rollcall_body_format() {
        let body = NumberRollcallBody::new("0042");
        let json = serde_json::to_value(&body).unwrap();
        assert_eq!(json["numberCode"], "0042");
        assert!(json["deviceId"].is_string());
        // deviceId 應為合法的 UUID 格式
        let device_id_str = json["deviceId"].as_str().unwrap();
        assert!(Uuid::parse_str(device_id_str).is_ok());
    }

    #[test]
    fn test_radar_rollcall_body_null_fields() {
        let body = RadarRollcallBody::new(24.3, 118.0, 35, 0);
        let json = serde_json::to_value(&body).unwrap();
        assert_eq!(json["latitude"], 24.3);
        assert_eq!(json["longitude"], 118.0);
        assert_eq!(json["accuracy"], 35);
        assert_eq!(json["altitude"], 0);
        assert!(json["altitudeAccuracy"].is_null());
        assert!(json["heading"].is_null());
        assert!(json["speed"].is_null());
    }

    #[test]
    fn test_qrcode_rollcall_body() {
        let body = QrCodeRollcallBody::new("test-data-string");
        let json = serde_json::to_value(&body).unwrap();
        assert_eq!(json["data"], "test-data-string");
        assert!(json["deviceId"].is_string());
    }

    #[test]
    fn test_rollcalls_response_deserialize() {
        let raw = r#"{
            "rollcalls": [
                {
                    "rollcall_id": 12345,
                    "course_title": "計算機網路",
                    "created_by_name": "陳教授",
                    "department_name": "資訊工程系",
                    "is_expired": false,
                    "is_number": false,
                    "is_radar": true,
                    "status": "absent",
                    "rollcall_status": "ongoing",
                    "scored": false
                }
            ]
        }"#;

        let resp: RollcallsResponse = serde_json::from_str(raw).unwrap();
        assert_eq!(resp.rollcalls.len(), 1);
        let rc = &resp.rollcalls[0];
        assert_eq!(rc.rollcall_id, 12345);
        assert_eq!(rc.course_title, "計算機網路");
        assert!(rc.is_radar);
        assert!(!rc.is_number);
        assert!(rc.needs_attendance());
        assert_eq!(rc.attendance_type(), AttendanceType::Radar);
    }

    #[test]
    fn test_attendance_response_radar_distance() {
        let raw = r#"{"distance": 42.5}"#;
        let resp: AttendanceResponse = serde_json::from_str(raw).unwrap();
        assert!(resp.is_radar_distance_error());
        assert_eq!(resp.distance, Some(42.5));
    }

    #[test]
    fn test_attendance_result_is_success() {
        assert!(AttendanceResult::Success.is_success());
        assert!(!AttendanceResult::RadarTooFar { distance: 10.0 }.is_success());
        assert!(!AttendanceResult::Failed {
            reason: "err".into()
        }
        .is_success());
    }

    #[test]
    fn test_attendance_type_display() {
        assert_eq!(AttendanceType::Number.to_string(), "數字簽到");
        assert_eq!(AttendanceType::Radar.to_string(), "雷達簽到");
        assert_eq!(AttendanceType::QrCode.to_string(), "QR Code 簽到");
    }

    // ── 輔助函式 ──────────────────────────────────────────────────────────────

    fn make_test_rollcall() -> Rollcall {
        Rollcall {
            rollcall_id: 1,
            course_title: "測試課程".to_string(),
            created_by_name: "測試教授".to_string(),
            department_name: "測試系".to_string(),
            is_expired: false,
            is_number: false,
            is_radar: false,
            status: "absent".to_string(),
            rollcall_status: "ongoing".to_string(),
            scored: false,
        }
    }
}
