//! 簽到主邏輯模組
//!
//! 作為各種簽到類型的調度中心：
//! - 接收 rollcall 列表
//! - 根據 `is_number` / `is_radar` 判斷類型
//! - 分派到對應的子模組（number / radar / qrcode）執行
//! - 統一處理結果與錯誤回報

pub mod number;
pub mod qrcode;
pub mod radar;

use std::sync::Arc;
use std::time::Duration;

use tokio::sync::{mpsc, Mutex};
use tracing::{debug, error, info, instrument, warn};

use crate::account::AccountConfig;
use crate::adapters::line::LineBotClient;
use crate::api::{
    is_auth_error,
    rollcall::{AttendanceType, Rollcall},
    ApiClient,
};

use self::number::{brute_force_number_rollcall, BruteForceResult};
use self::qrcode::{attempt_qrcode_rollcall, QrCodeResult};
use self::radar::{attempt_radar_rollcall, RadarResult};

// ─── 簽到執行結果 ─────────────────────────────────────────────────────────────

/// 單次 rollcall 的最終執行結果
#[derive(Debug, Clone)]
pub struct RollcallOutcome {
    /// 對應的 rollcall
    pub rollcall: Rollcall,

    /// 簽到類型
    pub attendance_type: AttendanceType,

    /// 執行結果
    pub result: RollcallResult,

    /// 本次簽到花費的時間（毫秒）
    pub elapsed_ms: u64,
}

impl RollcallOutcome {
    pub fn is_success(&self) -> bool {
        self.result.is_success()
    }
}

impl std::fmt::Display for RollcallOutcome {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "[{}] {} → {} ({}ms)",
            self.rollcall.rollcall_id, self.attendance_type, self.result, self.elapsed_ms,
        )
    }
}

/// 簽到結果枚舉
#[derive(Debug, Clone)]
pub enum RollcallResult {
    /// 簽到成功
    Success { detail: String },

    /// 簽到失敗（非致命，下次仍可重試）
    Failed { reason: String },

    /// 等待外部輸入（QR code 需要使用者掃碼）
    WaitingForInput { prompt: String },

    /// 跳過（已簽到、已過期，或不需處理）
    Skipped { reason: String },

    /// 致命錯誤（session 過期，需重新認證）
    FatalError { reason: String },
}

impl RollcallResult {
    pub fn is_success(&self) -> bool {
        matches!(self, RollcallResult::Success { .. })
    }

    pub fn is_fatal(&self) -> bool {
        matches!(self, RollcallResult::FatalError { .. })
    }

    pub fn is_skipped(&self) -> bool {
        matches!(self, RollcallResult::Skipped { .. })
    }
}

impl std::fmt::Display for RollcallResult {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            RollcallResult::Success { detail } => write!(f, "✅ 成功：{detail}"),
            RollcallResult::Failed { reason } => write!(f, "❌ 失敗：{reason}"),
            RollcallResult::WaitingForInput { prompt } => write!(f, "⏳ 等待輸入：{prompt}"),
            RollcallResult::Skipped { reason } => write!(f, "⏭️  跳過：{reason}"),
            RollcallResult::FatalError { reason } => write!(f, "🔴 致命錯誤：{reason}"),
        }
    }
}

// ─── QR Code 通道 ─────────────────────────────────────────────────────────────

/// 用於在 Line Bot handler 和 rollcall 邏輯之間傳遞 QR code 資料的通道類型
pub type QrCodeSender = mpsc::Sender<String>;
pub type QrCodeReceiver = Arc<Mutex<mpsc::Receiver<String>>>;

/// 建立 QR code 通道
pub fn create_qrcode_channel(buffer: usize) -> (QrCodeSender, QrCodeReceiver) {
    let (tx, rx) = mpsc::channel(buffer);
    (tx, Arc::new(Mutex::new(rx)))
}

// ─── 主調度函式 ───────────────────────────────────────────────────────────────

/// 處理單個 rollcall 簽到
///
/// 根據 rollcall 的類型（number / radar / qrcode）分派到對應的處理函式。
///
/// # 參數
/// - `api`：已認證的 API 客戶端（Arc 包裹，可跨任務共享）
/// - `rollcall`：要處理的 rollcall
/// - `config`：應用程式設定
/// - `line_bot`：Line Bot 客戶端（用於發送通知和接收 QR code）
/// - `qr_rx`：QR code 輸入通道接收端（等待使用者透過 Line Bot 傳送 QR code）
#[instrument(
    skip(api, config, line_bot, qr_rx),
    fields(
        rollcall_id = rollcall.rollcall_id,
        course = %rollcall.course_title,
        r#type = %rollcall.attendance_type(),
    )
)]
pub async fn process_rollcall(
    api: Arc<ApiClient>,
    rollcall: Rollcall,
    config: &AccountConfig,
    account_label: &str,
    line_bot: Option<&LineBotClient>,
    qr_rx: Option<QrCodeReceiver>,
) -> RollcallOutcome {
    let start = std::time::Instant::now();
    let attendance_type = rollcall.attendance_type();

    // ── 前置檢查 ──────────────────────────────────────────────────────────────

    // 已簽到，跳過
    if rollcall.is_attended() {
        debug!(rollcall_id = rollcall.rollcall_id, "已簽到，跳過");
        return RollcallOutcome {
            attendance_type,
            result: RollcallResult::Skipped {
                reason: "已簽到（on_call_fine）".to_string(),
            },
            elapsed_ms: start.elapsed().as_millis() as u64,
            rollcall,
        };
    }

    // 已過期，跳過
    if rollcall.is_expired {
        debug!(rollcall_id = rollcall.rollcall_id, "已過期，跳過");
        return RollcallOutcome {
            attendance_type,
            result: RollcallResult::Skipped {
                reason: "簽到已過期".to_string(),
            },
            elapsed_ms: start.elapsed().as_millis() as u64,
            rollcall,
        };
    }

    // 狀態不是 absent，跳過
    if !rollcall.needs_attendance() {
        debug!(
            rollcall_id = rollcall.rollcall_id,
            status = %rollcall.status,
            "狀態非 absent，跳過"
        );
        return RollcallOutcome {
            attendance_type,
            result: RollcallResult::Skipped {
                reason: format!("狀態為 `{}`，不需要簽到", rollcall.status),
            },
            elapsed_ms: start.elapsed().as_millis() as u64,
            rollcall,
        };
    }

    info!(
        rollcall_id = rollcall.rollcall_id,
        course = %rollcall.course_title,
        teacher = %rollcall.created_by_name,
        r#type = %attendance_type,
        "開始處理簽到"
    );

    // ── 發送開始通知 ──────────────────────────────────────────────────────────
    if let Some(bot) = line_bot {
        let msg = format!(
            "📋 偵測到新簽到\n帳號：{}\n課程：{}\n教師：{}\n類型：{}\n開始自動簽到...",
            account_label, rollcall.course_title, rollcall.created_by_name, attendance_type,
        );
        if let Err(e) = bot.push_message_to_admin(&msg).await {
            warn!(error = %e, "發送 Line 開始通知失敗");
        }
    }

    // ── 根據類型分派 ──────────────────────────────────────────────────────────
    let result = match attendance_type {
        AttendanceType::Number => handle_number_rollcall(Arc::clone(&api), &rollcall, config).await,
        AttendanceType::Radar => handle_radar_rollcall(Arc::clone(&api), &rollcall, config).await,
        AttendanceType::QrCode => {
            handle_qrcode_rollcall(
                Arc::clone(&api),
                &rollcall,
                config,
                account_label,
                line_bot,
                qr_rx,
            )
            .await
        }
    };

    let elapsed_ms = start.elapsed().as_millis() as u64;

    // ── 發送結果通知 ──────────────────────────────────────────────────────────
    if let Some(bot) = line_bot {
        let emoji = if result.is_success() { "✅" } else { "❌" };
        let msg = format!(
            "{emoji} 簽到結果\n帳號：{}\n課程：{}\n結果：{}\n耗時：{}ms",
            account_label, rollcall.course_title, result, elapsed_ms,
        );
        if let Err(e) = bot.push_message_to_admin(&msg).await {
            warn!(error = %e, "發送 Line 結果通知失敗");
        }
    }

    let outcome = RollcallOutcome {
        rollcall,
        attendance_type,
        result,
        elapsed_ms,
    };

    if outcome.is_success() {
        info!(outcome = %outcome, "簽到完成");
    } else {
        warn!(outcome = %outcome, "簽到未成功");
    }

    outcome
}

// ─── 各類型處理函式 ───────────────────────────────────────────────────────────

/// 處理數字簽到（爆破 0000~9999）
async fn handle_number_rollcall(
    api: Arc<ApiClient>,
    rollcall: &Rollcall,
    config: &AccountConfig,
) -> RollcallResult {
    info!(
        rollcall_id = rollcall.rollcall_id,
        concurrency = config.provider_config.brute_force.concurrency,
        "開始數字爆破簽到"
    );

    let result = brute_force_number_rollcall(
        api,
        rollcall.rollcall_id,
        &config.provider_config.brute_force,
    )
    .await;

    match result {
        BruteForceResult::Found { code, attempts } => RollcallResult::Success {
            detail: format!("數字代碼 `{code}` 正確（嘗試 {attempts} 次）"),
        },
        BruteForceResult::NotFound => RollcallResult::Failed {
            reason: "爆破失敗：0000~9999 全部嘗試完畢，無正確代碼".to_string(),
        },
        BruteForceResult::Error(e) => {
            if is_auth_error(&e) {
                RollcallResult::FatalError { reason: e }
            } else {
                RollcallResult::Failed { reason: e }
            }
        }
    }
}

/// 處理雷達簽到（地理位置）
async fn handle_radar_rollcall(
    api: Arc<ApiClient>,
    rollcall: &Rollcall,
    config: &AccountConfig,
) -> RollcallResult {
    info!(
        rollcall_id = rollcall.rollcall_id,
        default_coords = config.provider_config.radar.default_coords.len(),
        "開始雷達簽到"
    );

    let result =
        attempt_radar_rollcall(api, rollcall.rollcall_id, &config.provider_config.radar).await;

    match result {
        RadarResult::Success { coord } => RollcallResult::Success {
            detail: format!("座標 ({:.6}, {:.6})", coord.latitude, coord.longitude),
        },
        RadarResult::Failed {
            last_distance,
            tried_coords,
        } => {
            let dist_msg = last_distance
                .map(|d| format!("，最後距離 {d:.2}m"))
                .unwrap_or_default();
            RollcallResult::Failed {
                reason: format!(
                    "所有座標均失敗（嘗試 {} 個座標{}）",
                    tried_coords.len(),
                    dist_msg
                ),
            }
        }
        RadarResult::Error(e) => {
            if is_auth_error(&e) {
                RollcallResult::FatalError { reason: e }
            } else {
                RollcallResult::Failed { reason: e }
            }
        }
    }
}

/// 處理 QR Code 簽到
///
/// 流程：
/// 1. 透過 Line Bot 發送掃碼請求給管理員
/// 2. 等待管理員透過 Line Bot 回傳 QR code URL（或 p 參數）
/// 3. 解析並呼叫 API 完成簽到
/// 4. 若超時（`config.provider_config.qrcode.scan_timeout_secs`），返回失敗
async fn handle_qrcode_rollcall(
    api: Arc<ApiClient>,
    rollcall: &Rollcall,
    config: &AccountConfig,
    account_label: &str,
    line_bot: Option<&LineBotClient>,
    qr_rx: Option<QrCodeReceiver>,
) -> RollcallResult {
    info!(rollcall_id = rollcall.rollcall_id, "開始 QR Code 簽到流程");

    // 構建掃碼頁面 URL（讓使用者點擊）
    let scan_url = format!(
        "{}?rollcall_id={}",
        config.provider_config.qrcode.scanner_base_url, rollcall.rollcall_id
    );

    // 發送 Line 通知，要求使用者提供 QR code
    if let Some(bot) = line_bot {
        let msg = format!(
            "📷 需要 QR Code 簽到\n\
             課程：{}\n\
             帳號：{}\n\
             教師：{}\n\
             \n\
             請到以下連結掃描 QR Code，\n\
             然後將掃描結果（URL）傳送到此對話：\n\
             {}\n\
             \n\
             ⏰ 請在 {} 秒內回覆，否則簽到逾時",
            rollcall.course_title,
            account_label,
            rollcall.created_by_name,
            scan_url,
            config.provider_config.qrcode.scan_timeout_secs,
        );

        if let Err(e) = bot.push_message_to_admin(&msg).await {
            warn!(error = %e, "發送 QR Code 請求通知失敗");
        }
    } else {
        // 沒有 Line Bot，只能記錄日誌
        warn!(
            rollcall_id = rollcall.rollcall_id,
            scan_url = %scan_url,
            "QR Code 簽到需要 Line Bot 支援，但 Line Bot 未啟用！"
        );
        return RollcallResult::Failed {
            reason: "QR Code 簽到需要 Line Bot，請在 config.toml 中啟用 line_bot".to_string(),
        };
    }

    // 等待 QR code 輸入（帶逾時）
    let Some(rx) = qr_rx else {
        error!("QR code 接收通道未初始化");
        return RollcallResult::Failed {
            reason: "此部署模式未啟用 QR Code 回傳通道".to_string(),
        };
    };

    let timeout = Duration::from_secs(config.provider_config.qrcode.scan_timeout_secs);
    info!(
        rollcall_id = rollcall.rollcall_id,
        timeout_secs = config.provider_config.qrcode.scan_timeout_secs,
        "等待 QR code 輸入..."
    );

    let qr_input = {
        let mut rx_guard = rx.lock().await;
        match tokio::time::timeout(timeout, rx_guard.recv()).await {
            Ok(Some(input)) => {
                info!(input_len = input.len(), "收到 QR code 輸入");
                input
            }
            Ok(None) => {
                // 通道已關閉
                return RollcallResult::FatalError {
                    reason: "QR code 輸入通道已關閉".to_string(),
                };
            }
            Err(_) => {
                warn!(
                    rollcall_id = rollcall.rollcall_id,
                    "QR code 等待逾時（{}秒）", config.provider_config.qrcode.scan_timeout_secs
                );
                return RollcallResult::Failed {
                    reason: format!(
                        "QR code 輸入逾時（{}秒）",
                        config.provider_config.qrcode.scan_timeout_secs
                    ),
                };
            }
        }
    };

    // 執行 QR Code 簽到
    let result = attempt_qrcode_rollcall(api, rollcall.rollcall_id, &qr_input, false).await;

    match result {
        QrCodeResult::Success { data } => RollcallResult::Success {
            detail: format!("QR code data 簽到成功（data 長度：{}）", data.len()),
        },
        QrCodeResult::Failed { reason } => RollcallResult::Failed { reason },
        QrCodeResult::ParseError { reason } => RollcallResult::Failed {
            reason: format!("QR code 解析失敗：{reason}"),
        },
        QrCodeResult::Error(e) => {
            if is_auth_error(&e) {
                RollcallResult::FatalError { reason: e }
            } else {
                RollcallResult::Failed { reason: e }
            }
        }
    }
}

// ─── 批次處理 ─────────────────────────────────────────────────────────────────

/// 批次處理多個 rollcall
///
/// 對傳入的 rollcall 列表，依序（或並發）執行簽到。
/// 返回每個 rollcall 的執行結果。
///
/// # 注意
/// QR code 類型的簽到需要串行（等待使用者輸入），因此整體以串行模式執行。
/// 數字和雷達簽到的內部並發在各自的子模組中處理。
pub async fn process_rollcall_batch(
    api: Arc<ApiClient>,
    rollcalls: Vec<Rollcall>,
    config: &AccountConfig,
    account_label: &str,
    line_bot: Option<&LineBotClient>,
    qr_rx: Option<QrCodeReceiver>,
) -> Vec<RollcallOutcome> {
    let total = rollcalls.len();
    let mut outcomes = Vec::with_capacity(total);

    if total == 0 {
        debug!("沒有需要處理的 rollcall");
        return outcomes;
    }

    // 篩選出需要簽到的 rollcall
    let pending: Vec<Rollcall> = rollcalls
        .into_iter()
        .filter(|rc| rc.needs_attendance())
        .collect();

    if pending.is_empty() {
        debug!(
            total = total,
            "全部 rollcall 均不需要簽到（已簽到或已過期）"
        );
        return outcomes;
    }

    info!(
        pending = pending.len(),
        total = total,
        "開始批次處理 {} 個待簽到 rollcall",
        pending.len()
    );

    for (i, rollcall) in pending.into_iter().enumerate() {
        let rollcall_id = rollcall.rollcall_id;
        debug!(
            idx = i + 1,
            rollcall_id = rollcall_id,
            "處理第 {} 個 rollcall",
            i + 1
        );

        let outcome = process_rollcall(
            Arc::clone(&api),
            rollcall,
            config,
            account_label,
            line_bot,
            qr_rx.clone(),
        )
        .await;

        // 若發生致命錯誤（session 過期），立即停止批次
        if outcome.result.is_fatal() {
            error!(
                rollcall_id = rollcall_id,
                "批次處理遇到致命錯誤，停止處理剩餘 rollcall"
            );
            outcomes.push(outcome);
            break;
        }

        outcomes.push(outcome);
    }

    // 統計結果
    let success_count = outcomes.iter().filter(|o| o.is_success()).count();
    let failed_count = outcomes
        .iter()
        .filter(|o| matches!(o.result, RollcallResult::Failed { .. }))
        .count();
    let skipped_count = outcomes.iter().filter(|o| o.result.is_skipped()).count();

    info!(
        total = outcomes.len(),
        success = success_count,
        failed = failed_count,
        skipped = skipped_count,
        "批次處理完成"
    );

    outcomes
}

// ─── 統計摘要 ─────────────────────────────────────────────────────────────────

/// 批次處理結果的統計摘要
#[derive(Debug, Clone, Default)]
pub struct BatchSummary {
    pub total: usize,
    pub success: usize,
    pub failed: usize,
    pub skipped: usize,
    pub fatal: usize,
    pub waiting: usize,
}

impl BatchSummary {
    pub fn from_outcomes(outcomes: &[RollcallOutcome]) -> Self {
        let mut s = BatchSummary::default();
        s.total = outcomes.len();
        for o in outcomes {
            match &o.result {
                RollcallResult::Success { .. } => s.success += 1,
                RollcallResult::Failed { .. } => s.failed += 1,
                RollcallResult::Skipped { .. } => s.skipped += 1,
                RollcallResult::FatalError { .. } => s.fatal += 1,
                RollcallResult::WaitingForInput { .. } => s.waiting += 1,
            }
        }
        s
    }

    pub fn has_fatal(&self) -> bool {
        self.fatal > 0
    }

    pub fn all_success(&self) -> bool {
        self.success == self.total
    }
}

impl std::fmt::Display for BatchSummary {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "共 {} 個 | ✅ {} 成功 | ❌ {} 失敗 | ⏭️ {} 跳過 | 🔴 {} 致命",
            self.total, self.success, self.failed, self.skipped, self.fatal,
        )
    }
}

// ─── 測試 ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use crate::api::rollcall::Rollcall;

    // ── RollcallResult ────────────────────────────────────────────────────────

    #[test]
    fn test_rollcall_result_is_success() {
        assert!(RollcallResult::Success {
            detail: "ok".into()
        }
        .is_success());
        assert!(!RollcallResult::Failed {
            reason: "err".into()
        }
        .is_success());
        assert!(!RollcallResult::Skipped {
            reason: "skip".into()
        }
        .is_success());
        assert!(!RollcallResult::FatalError {
            reason: "fatal".into()
        }
        .is_success());
        assert!(!RollcallResult::WaitingForInput {
            prompt: "wait".into()
        }
        .is_success());
    }

    #[test]
    fn test_rollcall_result_is_fatal() {
        assert!(RollcallResult::FatalError {
            reason: "fatal".into()
        }
        .is_fatal());
        assert!(!RollcallResult::Success {
            detail: "ok".into()
        }
        .is_fatal());
        assert!(!RollcallResult::Failed {
            reason: "err".into()
        }
        .is_fatal());
    }

    #[test]
    fn test_rollcall_result_is_skipped() {
        assert!(RollcallResult::Skipped {
            reason: "skip".into()
        }
        .is_skipped());
        assert!(!RollcallResult::Success {
            detail: "ok".into()
        }
        .is_skipped());
    }

    #[test]
    fn test_rollcall_result_display() {
        let s = RollcallResult::Success {
            detail: "code=0042".into(),
        };
        assert!(s.to_string().contains("成功") && s.to_string().contains("0042"));

        let f = RollcallResult::Failed {
            reason: "距離不足".into(),
        };
        assert!(f.to_string().contains("失敗") && f.to_string().contains("距離不足"));

        let sk = RollcallResult::Skipped {
            reason: "已簽到".into(),
        };
        assert!(sk.to_string().contains("跳過") && sk.to_string().contains("已簽到"));

        let fe = RollcallResult::FatalError {
            reason: "session 過期".into(),
        };
        assert!(fe.to_string().contains("致命") && fe.to_string().contains("session"));

        let w = RollcallResult::WaitingForInput {
            prompt: "請掃碼".into(),
        };
        assert!(w.to_string().contains("等待") && w.to_string().contains("掃碼"));
    }

    // ── RollcallOutcome ───────────────────────────────────────────────────────

    #[test]
    fn test_rollcall_outcome_is_success() {
        let outcome = make_outcome(RollcallResult::Success {
            detail: "ok".into(),
        });
        assert!(outcome.is_success());
    }

    #[test]
    fn test_rollcall_outcome_display() {
        let outcome = make_outcome(RollcallResult::Success {
            detail: "ok".into(),
        });
        let s = outcome.to_string();
        assert!(s.contains("✅"));
        assert!(s.contains("ms"));
    }

    // ── BatchSummary ──────────────────────────────────────────────────────────

    #[test]
    fn test_batch_summary_from_outcomes() {
        let outcomes = vec![
            make_outcome(RollcallResult::Success {
                detail: "ok".into(),
            }),
            make_outcome(RollcallResult::Failed {
                reason: "err".into(),
            }),
            make_outcome(RollcallResult::Skipped {
                reason: "skip".into(),
            }),
            make_outcome(RollcallResult::FatalError {
                reason: "fatal".into(),
            }),
            make_outcome(RollcallResult::WaitingForInput {
                prompt: "wait".into(),
            }),
        ];

        let summary = BatchSummary::from_outcomes(&outcomes);
        assert_eq!(summary.total, 5);
        assert_eq!(summary.success, 1);
        assert_eq!(summary.failed, 1);
        assert_eq!(summary.skipped, 1);
        assert_eq!(summary.fatal, 1);
        assert_eq!(summary.waiting, 1);
    }

    #[test]
    fn test_batch_summary_has_fatal() {
        let outcomes = vec![make_outcome(RollcallResult::FatalError {
            reason: "fatal".into(),
        })];
        let summary = BatchSummary::from_outcomes(&outcomes);
        assert!(summary.has_fatal());
    }

    #[test]
    fn test_batch_summary_all_success() {
        let outcomes = vec![
            make_outcome(RollcallResult::Success {
                detail: "ok1".into(),
            }),
            make_outcome(RollcallResult::Success {
                detail: "ok2".into(),
            }),
        ];
        let summary = BatchSummary::from_outcomes(&outcomes);
        assert!(summary.all_success());
    }

    #[test]
    fn test_batch_summary_not_all_success() {
        let outcomes = vec![
            make_outcome(RollcallResult::Success {
                detail: "ok".into(),
            }),
            make_outcome(RollcallResult::Failed {
                reason: "err".into(),
            }),
        ];
        let summary = BatchSummary::from_outcomes(&outcomes);
        assert!(!summary.all_success());
    }

    #[test]
    fn test_batch_summary_display() {
        let outcomes: Vec<RollcallOutcome> = vec![];
        let summary = BatchSummary::from_outcomes(&outcomes);
        let s = summary.to_string();
        assert!(s.contains("共"));
        assert!(s.contains("成功"));
        assert!(s.contains("失敗"));
    }

    #[test]
    fn test_batch_summary_empty() {
        let summary = BatchSummary::from_outcomes(&[]);
        assert_eq!(summary.total, 0);
        assert!(!summary.has_fatal());
        assert!(summary.all_success()); // 0 == 0，視為全成功
    }

    // ── is_auth_error ─────────────────────────────────────────────────────────

    #[test]
    fn test_is_auth_error_true() {
        assert!(is_auth_error("Session expired"));
        assert!(is_auth_error("HTTP 401 Unauthorized"));
        assert!(is_auth_error("403 Forbidden"));
        assert!(is_auth_error("session cookie invalid"));
        assert!(is_auth_error("需要重新登錄"));
        assert!(is_auth_error("認證失敗"));
    }

    #[test]
    fn test_is_auth_error_false() {
        assert!(!is_auth_error("Network timeout"));
        assert!(!is_auth_error("距離不足 42m"));
        assert!(!is_auth_error("Code 0042 incorrect"));
        assert!(!is_auth_error("JSON parse error"));
    }

    // ── QR code channel ───────────────────────────────────────────────────────

    #[tokio::test]
    async fn test_qrcode_channel_send_recv() {
        let (tx, rx) = create_qrcode_channel(1);
        tx.send("test_qr_data".to_string()).await.unwrap();

        let mut rx_guard = rx.lock().await;
        let received = rx_guard.recv().await;
        assert_eq!(received, Some("test_qr_data".to_string()));
    }

    #[tokio::test]
    async fn test_qrcode_channel_timeout() {
        let (_tx, rx) = create_qrcode_channel(1);
        let mut rx_guard = rx.lock().await;
        let result = tokio::time::timeout(Duration::from_millis(50), rx_guard.recv()).await;
        assert!(result.is_err(), "應該逾時");
    }

    // ── helpers ───────────────────────────────────────────────────────────────

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

    fn make_outcome(result: RollcallResult) -> RollcallOutcome {
        RollcallOutcome {
            rollcall: make_test_rollcall(),
            attendance_type: AttendanceType::QrCode,
            result,
            elapsed_ms: 100,
        }
    }
}
