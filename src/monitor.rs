//! 主監控循環模組
//!
//! 實現程式的核心邏輯：
//! - 定期輪詢 `/api/radar/rollcalls`
//! - 偵測到新的待簽到項目時，分派到對應的簽到模組
//! - Session 過期時自動重新認證
//! - 透過 `WebhookState` 接收 Line Bot 的控制指令（/stop、/start、/force）
//! - 更新 `MonitorStatus` 供 `/status` 指令查詢
//!
//! # 執行流程
//! ```
//! main()
//!  ├── init_config()
//!  ├── AuthClient::new()           ← CAS 登錄
//!  ├── ApiClient::from_config()    ← 建立 API 客戶端
//!  ├── LineBotClient::new()        ← 建立 Line Bot 客戶端
//!  ├── start_webhook_server()      ← 背景啟動 Webhook 伺服器
//!  └── run_monitor_loop()          ← 進入主監控循環（阻塞）
//!       ├── sleep(startup_delay)
//!       └── loop {
//!             wait_for_trigger()   ← 等待定時或強制觸發
//!             if !is_running { continue }
//!             get_rollcalls()
//!             process_rollcall_batch()
//!             update_status()
//!             on_failure: maybe_reauth()
//!           }
//! ```

use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use miette::{Result, WrapErr};
use tokio::sync::Mutex;
use tracing::{debug, error, info, instrument, warn};

use crate::adapters::line::types::MonitorStatus;
use crate::adapters::line::{LineBotClient, WebhookState};
use crate::api::{is_auth_error, ApiClient};
use crate::auth::AuthClient;
use crate::config::{AccountConfig, AppConfig};
use crate::rollcalls::{
    create_qrcode_channel, process_rollcall_batch, BatchSummary, QrCodeReceiver, QrCodeSender,
};

// ─── 監控器設定 ───────────────────────────────────────────────────────────────

/// 監控器執行時的完整上下文
pub struct MonitorContext {
    /// 全域設定
    pub global_config: Arc<AppConfig>,

    /// 單一帳號設定
    pub account: Arc<AccountConfig>,

    /// 已認證的 HTTP 客戶端（持有 session cookie）
    pub auth_client: AuthClient,

    /// Tronclass API 客戶端
    pub api_client: Arc<ApiClient>,

    /// Line Bot 客戶端（可選）
    pub line_bot: Option<Arc<LineBotClient>>,

    /// Webhook 伺服器狀態（用於接收控制指令）
    pub webhook_state: Option<WebhookState>,

    /// QR code 輸入通道接收端
    pub qr_rx: QrCodeReceiver,

    /// QR code 輸入通道傳送端（可複製給 Webhook handler）
    pub qr_tx: QrCodeSender,

    /// 共享的監控狀態
    pub status: Arc<Mutex<MonitorStatus>>,
}

impl MonitorContext {
    /// 建立監控器上下文
    ///
    /// 此函式會：
    /// 1. 執行 CAS 登錄（或注入手動 cookie）
    /// 2. 建立 API 客戶端
    /// 3. （可選）建立 Line Bot 客戶端
    /// 4. 建立 QR code 通道
    /// 5. 初始化監控狀態
    pub async fn new(
        global_config: Arc<AppConfig>,
        account: Arc<AccountConfig>,
        line_bot: Option<Arc<LineBotClient>>,
        interactive_line: bool,
    ) -> Result<Self> {
        // ── 登錄 ──────────────────────────────────────────────────────────────
        info!(account = %account.id, "正在登錄 Tronclass...");

        let (auth_client, session) = AuthClient::new(&account)
            .await
            .wrap_err("登錄失敗，請檢查帳號密碼或 cookie 設定")?;

        info!(account = %account.id, user_name = %session.user_name, "登錄成功！");

        // ── API 客戶端 ────────────────────────────────────────────────────────
        // 使用 auth_client 內的 client（帶 session cookie）
        // 注意：重新建立一個共享的 client（透過 Arc）
        let api_client = Arc::new(ApiClient::new(
            auth_client.client.clone(),
            account.base_url().to_string(),
        ));

        // ── QR code 通道 ──────────────────────────────────────────────────────
        let (qr_tx, qr_rx) = create_qrcode_channel(4);

        // ── 監控狀態 ──────────────────────────────────────────────────────────
        let started_at = current_unix_secs();
        let status = Arc::new(Mutex::new(MonitorStatus {
            is_running: true,
            user_name: format!("{} ({})", session.user_name, account.display_name()),
            last_poll_timestamp: None,
            last_success_course: None,
            consecutive_failures: 0,
            started_at,
        }));

        // ── Webhook 狀態 ──────────────────────────────────────────────────────
        let webhook_state = if interactive_line {
            line_bot
                .as_ref()
                .map(|bot| WebhookState::new(Arc::clone(bot), qr_tx.clone(), Arc::clone(&status)))
        } else {
            None
        };

        Ok(Self {
            global_config,
            account,
            auth_client,
            api_client,
            line_bot,
            webhook_state,
            qr_rx,
            qr_tx,
            status,
        })
    }
}

// ─── 主監控循環 ───────────────────────────────────────────────────────────────

/// 執行主監控循環（阻塞直到程式終止）
///
/// # 流程
/// 1. 等待 `startup_delay_secs` 後開始第一次輪詢
/// 2. 每隔 `poll_interval_secs` 輪詢一次
/// 3. 若收到強制觸發（`force_poll_tx`），立即執行一次
/// 4. 若監控暫停（`is_running = false`），跳過輪詢但繼續等待
/// 5. 若連續失敗超過 `max_failures_before_reauth`，嘗試重新認證
/// 6. 收到 `reauth_tx` 通知時，立即重新認證
///
/// # 參數
/// - `ctx`：監控器上下文（mut，因為重新認證時需要更新 auth_client）
pub async fn run_monitor_loop(mut ctx: MonitorContext) -> Result<()> {
    let config = Arc::clone(&ctx.global_config);
    let account_label = ctx.account.display_name().to_string();
    let poll_interval = Duration::from_secs(config.api.poll_interval_secs);
    let startup_delay = Duration::from_secs(config.monitor.startup_delay_secs);

    // ── 啟動延遲 ──────────────────────────────────────────────────────────────
    if startup_delay > Duration::ZERO {
        info!(
            delay_secs = config.monitor.startup_delay_secs,
            "等待 {} 秒後開始第一次輪詢...", config.monitor.startup_delay_secs
        );
        tokio::time::sleep(startup_delay).await;
    }

    // 取得控制訊號的參考（從 webhook_state）
    let force_poll_notify = ctx
        .webhook_state
        .as_ref()
        .map(|s| Arc::clone(&s.force_poll_tx));
    let reauth_notify = ctx.webhook_state.as_ref().map(|s| Arc::clone(&s.reauth_tx));
    let is_running_lock = ctx
        .webhook_state
        .as_ref()
        .map(|s| Arc::clone(&s.is_running));

    info!(account = %account_label, "🚀 開始主監控循環");

    // 傳送啟動通知給管理員
    if let Some(bot) = &ctx.line_bot {
        let user_name = {
            let s = ctx.status.lock().await;
            s.user_name.clone()
        };
        let startup_msg = format!(
            "🚀 FJU Ghost Student 已啟動\n\
             帳號：{} / {user_name}\n\
             輪詢間隔：{} 秒\n\
             Line Bot：已啟用\n\n\
             輸入 /help 查看可用指令",
            account_label, config.api.poll_interval_secs,
        );
        if let Err(e) = bot.push_message_to_admin(&startup_msg).await {
            warn!(error = %e, "發送啟動通知失敗");
        }
    }

    let mut consecutive_failures: u32 = 0;
    let max_failures = config.monitor.max_failures_before_reauth;
    let retry_interval = Duration::from_secs(config.monitor.retry_interval_secs);

    loop {
        // ── 等待觸發（定時 or 強制） ──────────────────────────────────────────
        let triggered_by_force =
            wait_for_trigger(poll_interval, force_poll_notify.as_deref()).await;

        if triggered_by_force {
            debug!("由強制觸發啟動本次輪詢");
        } else {
            debug!("由定時觸發啟動本次輪詢");
        }

        // ── 檢查是否暫停 ──────────────────────────────────────────────────────
        if let Some(ref lock) = is_running_lock {
            let is_running = *lock.lock().await;
            if !is_running {
                debug!("監控已暫停，跳過本次輪詢");
                continue;
            }
        }

        // ── 檢查重新認證請求 ──────────────────────────────────────────────────
        if let Some(ref notify) = reauth_notify {
            // 非阻塞地嘗試接收通知
            if let Ok(()) = tokio::time::timeout(Duration::from_millis(1), notify.notified()).await
            {
                info!("收到重新認證請求");
                if let Err(e) = do_reauth(&mut ctx).await {
                    error!(error = %e, "重新認證失敗");
                    if let Some(bot) = &ctx.line_bot {
                        let _ = bot
                            .push_message_to_admin(&format!(
                                "❌ [{}] 重新認證失敗：{e}",
                                account_label
                            ))
                            .await;
                    }
                } else {
                    consecutive_failures = 0;
                    if let Some(bot) = &ctx.line_bot {
                        let _ = bot
                            .push_message_to_admin(&format!(
                                "✅ [{}] 重新認證成功！",
                                account_label
                            ))
                            .await;
                    }
                }
                continue;
            }
        }

        // ── 更新輪詢時間 ──────────────────────────────────────────────────────
        let poll_timestamp = current_unix_secs();
        {
            let mut s = ctx.status.lock().await;
            s.last_poll_timestamp = Some(poll_timestamp);
        }

        // ── 執行輪詢 ──────────────────────────────────────────────────────────
        info!("🔄 開始輪詢簽到列表...");

        match do_poll_and_attend(&mut ctx).await {
            Ok(summary) => {
                // 成功輪詢
                consecutive_failures = 0;

                if summary.total > 0 {
                    info!(summary = %summary, "本次輪詢完成");

                    // 更新最後成功簽到課程
                    // （實際上 summary 沒有包含課程名，這裡只是示意）
                }

                // 更新狀態中的連續失敗計數
                {
                    let mut s = ctx.status.lock().await;
                    s.consecutive_failures = 0;
                    s.is_running = true;
                }
            }

            Err(e) => {
                consecutive_failures += 1;
                error!(
                    error = %e,
                    consecutive_failures = consecutive_failures,
                    "輪詢失敗（第 {} 次連續失敗）",
                    consecutive_failures
                );

                // 更新狀態
                {
                    let mut s = ctx.status.lock().await;
                    s.consecutive_failures = consecutive_failures;
                }

                // 判斷是否需要重新認證
                if is_auth_error(&e.to_string()) || consecutive_failures >= max_failures {
                    if consecutive_failures >= max_failures {
                        warn!(
                            consecutive_failures = consecutive_failures,
                            max = max_failures,
                            "連續失敗次數達到上限，嘗試重新認證"
                        );
                    } else {
                        warn!("偵測到認證錯誤，嘗試重新認證");
                    }

                    if let Some(bot) = &ctx.line_bot {
                        let _ = bot
                            .push_message_to_admin(&format!(
                                "⚠️ [{}] 簽到監控遇到錯誤，嘗試重新認證...\n錯誤：{e}",
                                account_label
                            ))
                            .await;
                    }

                    match do_reauth(&mut ctx).await {
                        Ok(()) => {
                            info!("重新認證成功，重置失敗計數");
                            consecutive_failures = 0;
                            {
                                let mut s = ctx.status.lock().await;
                                s.consecutive_failures = 0;
                            }
                            if let Some(bot) = &ctx.line_bot {
                                let _ = bot
                                    .push_message_to_admin(&format!(
                                        "✅ [{}] 自動重新認證成功！",
                                        account_label
                                    ))
                                    .await;
                            }
                        }
                        Err(reauth_err) => {
                            error!(error = %reauth_err, "重新認證失敗！");
                            if let Some(bot) = &ctx.line_bot {
                                let _ = bot
                                    .push_message_to_admin(&format!(
                                        "❌ [{}] 自動重新認證失敗：{reauth_err}\n\n\
                                         請手動更新 accounts.toml 中對應帳號的 manual_cookie，\n\
                                         或輸入 /reauth 再次嘗試。",
                                        account_label
                                    ))
                                    .await;
                            }

                            // 重新認證失敗後等待更長的時間再重試
                            warn!(
                                retry_secs = config.monitor.retry_interval_secs,
                                "重新認證失敗，等待 {} 秒後重試",
                                config.monitor.retry_interval_secs
                            );
                            tokio::time::sleep(retry_interval).await;
                        }
                    }
                } else {
                    // 普通失敗，等待 retry_interval 後繼續
                    warn!(
                        retry_secs = config.monitor.retry_interval_secs,
                        "等待 {} 秒後重試...", config.monitor.retry_interval_secs
                    );
                    tokio::time::sleep(retry_interval).await;
                }
            }
        }
    }
}

// ─── 單次輪詢與簽到 ───────────────────────────────────────────────────────────

/// 執行一次完整的輪詢與簽到流程
///
/// 1. 呼叫 `GET /api/radar/rollcalls`
/// 2. 篩選出需要簽到的項目
/// 3. 分派到 `process_rollcall_batch`
/// 4. 返回批次統計摘要
#[instrument(skip(ctx))]
async fn do_poll_and_attend(ctx: &mut MonitorContext) -> Result<BatchSummary> {
    // ── 取得簽到列表 ──────────────────────────────────────────────────────────
    let rollcalls = ctx
        .api_client
        .get_rollcalls()
        .await
        .wrap_err("取得簽到列表失敗")?;

    let total = rollcalls.len();
    let pending_count = rollcalls.iter().filter(|rc| rc.needs_attendance()).count();

    debug!(
        total = total,
        pending = pending_count,
        "取得簽到列表：共 {total} 個，{pending_count} 個需要簽到"
    );

    if pending_count == 0 {
        if total > 0 {
            debug!("所有 {} 個簽到均已完成或已過期", total);
        } else {
            debug!("目前沒有任何簽到");
        }
        return Ok(BatchSummary::default());
    }

    info!(
        pending = pending_count,
        "偵測到 {} 個待簽到項目，開始自動簽到...", pending_count
    );

    // 記錄每個待簽到項目
    for rc in rollcalls.iter().filter(|rc| rc.needs_attendance()) {
        info!(
            rollcall_id = rc.rollcall_id,
            course = %rc.course_title,
            teacher = %rc.created_by_name,
            r#type = %rc.attendance_type(),
            "待簽到：{}",
            rc.display()
        );
    }

    // ── 執行批次簽到 ──────────────────────────────────────────────────────────
    let line_bot_ref = ctx.line_bot.as_ref().map(|b| b.as_ref());

    let outcomes = process_rollcall_batch(
        Arc::clone(&ctx.api_client),
        rollcalls,
        &ctx.global_config,
        ctx.account.display_name(),
        line_bot_ref,
        ctx.webhook_state.as_ref().map(|_| Arc::clone(&ctx.qr_rx)),
    )
    .await;

    let summary = BatchSummary::from_outcomes(&outcomes);

    // ── 記錄結果 ──────────────────────────────────────────────────────────────
    info!(summary = %summary, "批次簽到完成");

    // 記錄每個結果
    for outcome in &outcomes {
        if outcome.is_success() {
            info!(
                rollcall_id = outcome.rollcall.rollcall_id,
                course = %outcome.rollcall.course_title,
                result = %outcome.result,
                elapsed_ms = outcome.elapsed_ms,
                "✅ 簽到成功"
            );

            // 更新最後成功課程
            {
                let mut s = ctx.status.lock().await;
                s.last_success_course = Some(outcome.rollcall.course_title.clone());
            }
        } else if outcome.result.is_skipped() {
            debug!(
                rollcall_id = outcome.rollcall.rollcall_id,
                result = %outcome.result,
                "⏭️ 跳過"
            );
        } else {
            warn!(
                rollcall_id = outcome.rollcall.rollcall_id,
                course = %outcome.rollcall.course_title,
                result = %outcome.result,
                elapsed_ms = outcome.elapsed_ms,
                "❌ 簽到失敗"
            );
        }
    }

    // 若批次中有致命錯誤，傳播為 Err
    if summary.has_fatal() {
        return Err(miette::miette!(
            "批次簽到中發生致命錯誤（session 可能已過期）"
        ));
    }

    Ok(summary)
}

// ─── 重新認證 ─────────────────────────────────────────────────────────────────

/// 執行重新認證流程
///
/// 1. 呼叫 `auth_client.re_authenticate()`
/// 2. 用新的 client 重建 `api_client`
/// 3. 更新 `ctx.status` 中的 user_name
#[instrument(skip(ctx))]
async fn do_reauth(ctx: &mut MonitorContext) -> Result<()> {
    info!("開始重新認證...");

    let session = ctx
        .auth_client
        .re_authenticate(&ctx.account)
        .await
        .wrap_err("重新認證失敗")?;

    info!(user_name = %session.user_name, "重新認證成功");

    // 重建 API 客戶端（使用新的 session cookie）
    ctx.api_client = Arc::new(ApiClient::from_config(
        ctx.auth_client.client.clone(),
        &ctx.global_config,
    ));

    // 更新狀態中的 user_name
    {
        let mut s = ctx.status.lock().await;
        s.user_name = format!("{} ({})", session.user_name, ctx.account.display_name());
        s.consecutive_failures = 0;
    }

    Ok(())
}

// ─── 觸發等待 ─────────────────────────────────────────────────────────────────

/// 等待輪詢觸發
///
/// 等待以下其中之一：
/// 1. 定時器到期（`poll_interval`）
/// 2. 強制觸發通知（`force_poll_notify`）
///
/// # 返回
/// `true` 如果是強制觸發，`false` 如果是定時觸發。
async fn wait_for_trigger(
    poll_interval: Duration,
    force_poll_notify: Option<&tokio::sync::Notify>,
) -> bool {
    match force_poll_notify {
        Some(notify) => {
            // 同時等待定時器和強制通知
            tokio::select! {
                _ = tokio::time::sleep(poll_interval) => false,
                _ = notify.notified() => true,
            }
        }
        None => {
            // 沒有 Line Bot，只等定時器
            tokio::time::sleep(poll_interval).await;
            false
        }
    }
}

// ─── 輔助函式 ─────────────────────────────────────────────────────────────────

/// 取得當前 Unix 時間戳（秒）
pub fn current_unix_secs() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs() as i64
}

// ─── 測試 ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicBool, Ordering};

    // ── current_unix_secs ─────────────────────────────────────────────────────

    #[test]
    fn test_current_unix_secs_positive() {
        let ts = current_unix_secs();
        assert!(ts > 0, "Unix 時間戳應大於 0");
    }

    #[test]
    fn test_current_unix_secs_reasonable() {
        let ts = current_unix_secs();
        // 2024-01-01 00:00:00 UTC = 1704067200
        assert!(ts > 1_704_067_200, "時間戳應在 2024 年之後");
        // 2100-01-01 00:00:00 UTC = 4102444800
        assert!(ts < 4_102_444_800, "時間戳應在 2100 年之前");
    }

    #[test]
    fn test_current_unix_secs_monotonic() {
        let t1 = current_unix_secs();
        std::thread::sleep(std::time::Duration::from_millis(10));
        let t2 = current_unix_secs();
        assert!(t2 >= t1, "時間戳應單調遞增");
    }

    // ── is_auth_error ─────────────────────────────────────────────────────────

    #[test]
    fn test_is_auth_error_true_cases() {
        assert!(is_auth_error("Unauthorized access"));
        assert!(is_auth_error("HTTP 401 error"));
        assert!(is_auth_error("Response status: 403 Forbidden"));
        assert!(is_auth_error("Session expired, please re-login"));
        assert!(is_auth_error("session cookie invalid"));
        assert!(is_auth_error("session 可能已過期"));
        assert!(is_auth_error("批次簽到中發生致命錯誤"));
    }

    #[test]
    fn test_is_auth_error_false_cases() {
        assert!(!is_auth_error("Network timeout"));
        assert!(!is_auth_error("HTTP 500 Internal Server Error"));
        assert!(!is_auth_error("JSON parse error"));
        assert!(!is_auth_error("距離不足 50 公尺"));
        assert!(!is_auth_error("爆破失敗：無正確代碼"));
        assert!(!is_auth_error(""));
    }

    #[test]
    fn test_is_auth_error_edge_cases() {
        // 包含 "4" 和 "01" 但不是 "401"
        assert!(!is_auth_error("Error code 4: timeout"));
        // 包含 "Session" 的正常訊息
        assert!(is_auth_error("Session management failed"));
    }

    // ── wait_for_trigger ──────────────────────────────────────────────────────

    #[tokio::test]
    async fn test_wait_for_trigger_timeout() {
        let interval = Duration::from_millis(50);
        let result = wait_for_trigger(interval, None).await;
        assert!(!result, "定時觸發應返回 false");
    }

    #[tokio::test]
    async fn test_wait_for_trigger_force() {
        let interval = Duration::from_secs(60); // 很長，確保不會先到期
        let notify = Arc::new(tokio::sync::Notify::new());
        let notify_clone = Arc::clone(&notify);

        // 在背景觸發通知
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(20)).await;
            notify_clone.notify_one();
        });

        let result = wait_for_trigger(interval, Some(&notify)).await;
        assert!(result, "強制觸發應返回 true");
    }

    #[tokio::test]
    async fn test_wait_for_trigger_prefers_force_over_timer() {
        // 設定很短的 interval，但強制通知更早到達
        let interval = Duration::from_millis(100);
        let notify = Arc::new(tokio::sync::Notify::new());
        let notify_clone = Arc::clone(&notify);

        // 立即觸發通知
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(1)).await;
            notify_clone.notify_one();
        });

        let start = std::time::Instant::now();
        let result = wait_for_trigger(interval, Some(&notify)).await;
        let elapsed = start.elapsed();

        // 應該在 50ms 內返回（因為通知在 1ms 後觸發）
        assert!(result, "應為強制觸發");
        assert!(
            elapsed < Duration::from_millis(50),
            "應在 50ms 內返回，實際：{elapsed:?}"
        );
    }

    #[tokio::test]
    async fn test_wait_for_trigger_no_force_respects_interval() {
        let interval = Duration::from_millis(100);
        let start = std::time::Instant::now();
        let result = wait_for_trigger(interval, None).await;
        let elapsed = start.elapsed();

        assert!(!result, "應為定時觸發");
        assert!(
            elapsed >= Duration::from_millis(90),
            "應等待至少 90ms，實際：{elapsed:?}"
        );
    }

    // ── MonitorStatus 相關 ────────────────────────────────────────────────────

    #[test]
    fn test_monitor_status_default_display() {
        let status = MonitorStatus {
            is_running: true,
            user_name: "test_user".to_string(),
            last_poll_timestamp: Some(1_700_000_000),
            last_success_course: Some("計算機概論".to_string()),
            consecutive_failures: 2,
            started_at: 1_699_900_000,
        };

        let msg = status.to_line_message();

        // 驗證包含關鍵資訊
        assert!(msg.contains("test_user"), "應包含使用者名稱");
        assert!(msg.contains("計算機概論"), "應包含最後成功課程");
        assert!(msg.contains('2'), "應包含失敗次數");
    }

    #[test]
    fn test_monitor_status_no_poll_yet() {
        let status = MonitorStatus {
            is_running: true,
            user_name: "user".to_string(),
            last_poll_timestamp: None,
            last_success_course: None,
            consecutive_failures: 0,
            started_at: 0,
        };

        let msg = status.to_line_message();
        assert!(msg.contains("尚未輪詢"));
        assert!(msg.contains("無"));
    }

    // ── BatchSummary 整合 ─────────────────────────────────────────────────────

    #[test]
    fn test_batch_summary_default_is_empty() {
        let summary = BatchSummary::default();
        assert_eq!(summary.total, 0);
        assert_eq!(summary.success, 0);
        assert_eq!(summary.failed, 0);
        assert_eq!(summary.skipped, 0);
        assert_eq!(summary.fatal, 0);
        assert!(!summary.has_fatal());
        assert!(summary.all_success()); // 0 == 0
    }

    // ── 並發安全性測試 ────────────────────────────────────────────────────────

    #[tokio::test]
    async fn test_status_concurrent_access() {
        let status = Arc::new(Mutex::new(MonitorStatus {
            is_running: true,
            user_name: "initial".to_string(),
            last_poll_timestamp: None,
            last_success_course: None,
            consecutive_failures: 0,
            started_at: 0,
        }));

        let mut handles = Vec::new();

        // 多個並發任務同時更新狀態
        for i in 0..10 {
            let status_clone = Arc::clone(&status);
            let handle = tokio::spawn(async move {
                let mut s = status_clone.lock().await;
                s.consecutive_failures = i;
                s.last_poll_timestamp = Some(i as i64);
                drop(s);

                // 小延遲讓其他任務有機會執行
                tokio::time::sleep(Duration::from_millis(1)).await;

                let s = status_clone.lock().await;
                // 確認狀態可以正常讀取
                assert!(s.consecutive_failures <= 10);
            });
            handles.push(handle);
        }

        for handle in handles {
            handle.await.unwrap();
        }

        // 最終狀態應該是某個任務設定的值
        let final_status = status.lock().await;
        assert!(final_status.consecutive_failures <= 9);
    }

    #[tokio::test]
    async fn test_notify_multiple_waiters() {
        let notify = Arc::new(tokio::sync::Notify::new());
        let ready = Arc::new(AtomicBool::new(false));

        let notify_clone = Arc::clone(&notify);
        let ready_clone = Arc::clone(&ready);

        // 等待通知的任務
        let waiter = tokio::spawn(async move {
            notify_clone.notified().await;
            ready_clone.store(true, Ordering::SeqCst);
        });

        // 發送通知
        tokio::time::sleep(Duration::from_millis(10)).await;
        notify.notify_one();

        tokio::time::sleep(Duration::from_millis(10)).await;
        assert!(ready.load(Ordering::SeqCst), "等待者應在通知後被喚醒");

        waiter.await.unwrap();
    }

    // ── QR code 通道整合測試 ──────────────────────────────────────────────────

    #[tokio::test]
    async fn test_qrcode_channel_integration() {
        use crate::rollcalls::create_qrcode_channel;

        let (tx, rx) = create_qrcode_channel(2);

        // 傳送兩個 QR code
        tx.send("qr_1".to_string()).await.unwrap();
        tx.send("qr_2".to_string()).await.unwrap();

        let mut rx_guard = rx.lock().await;
        assert_eq!(rx_guard.recv().await, Some("qr_1".to_string()));
        assert_eq!(rx_guard.recv().await, Some("qr_2".to_string()));
    }

    #[tokio::test]
    async fn test_qrcode_channel_overflow() {
        use crate::rollcalls::create_qrcode_channel;

        let (tx, _rx) = create_qrcode_channel(1); // 容量只有 1

        // 第一個應該成功
        tx.send("qr_1".to_string()).await.unwrap();

        // 第二個在 channel 滿時應該失敗（因為 _rx 不讀取）
        let result = tx.try_send("qr_2".to_string());
        assert!(result.is_err(), "Channel 滿時 try_send 應失敗");
    }

    // ── 時間相關邊界測試 ──────────────────────────────────────────────────────

    #[test]
    fn test_duration_from_secs_zero() {
        let d = Duration::from_secs(0);
        assert_eq!(d, Duration::ZERO);
        // startup_delay = 0 時不應等待
        assert!(d.is_zero());
    }

    #[test]
    fn test_duration_comparison() {
        let short = Duration::from_millis(100);
        let long = Duration::from_secs(60);
        assert!(short < long);
        assert!(long > short);
    }
}
