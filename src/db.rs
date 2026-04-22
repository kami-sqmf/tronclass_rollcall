//! 帳號資料庫模組
//!
//! 以 SQLite 取代 `accounts.toml`，提供帳號的 CRUD 操作。
//! 資料庫檔案預設為 `accounts.db`，可透過 `-a/--accounts` 指定路徑。

use std::path::Path;

use miette::{IntoDiagnostic, Result, WrapErr};
use sqlx::{sqlite::SqlitePoolOptions, FromRow, SqlitePool};

use crate::config::{AccountConfig, AccountsFile, AppConfig, RawAccountConfig};

// ─── 資料庫列映射 ─────────────────────────────────────────────────────────────

#[derive(Debug, FromRow)]
struct AccountRow {
    id: String,
    provider: String,
    username: String,
    password: String,
    captcha: String,
    manual_cookie: String,
    enabled: bool,
    line_user_id: String,
}

impl From<AccountRow> for RawAccountConfig {
    fn from(row: AccountRow) -> Self {
        RawAccountConfig {
            id: row.id,
            provider: row.provider,
            username: row.username,
            password: row.password,
            captcha: row.captcha,
            manual_cookie: row.manual_cookie,
            enabled: row.enabled,
            line_user_id: row.line_user_id,
        }
    }
}

// ─── AccountsDb ───────────────────────────────────────────────────────────────

pub struct AccountsDb {
    pool: SqlitePool,
}

impl AccountsDb {
    /// 開啟（或建立）帳號資料庫，並確保 schema 存在。
    pub async fn open(path: impl AsRef<Path>) -> Result<Self> {
        let path = path.as_ref();
        let url = format!("sqlite://{}?mode=rwc", path.display());

        let pool = SqlitePoolOptions::new()
            .max_connections(5)
            .connect(&url)
            .await
            .into_diagnostic()
            .wrap_err_with(|| format!("無法開啟資料庫：`{}`", path.display()))?;

        sqlx::query(
            "CREATE TABLE IF NOT EXISTS accounts (
                id           TEXT PRIMARY KEY NOT NULL,
                provider     TEXT NOT NULL DEFAULT '',
                username     TEXT NOT NULL DEFAULT '',
                password     TEXT NOT NULL DEFAULT '',
                captcha      TEXT NOT NULL DEFAULT '',
                manual_cookie TEXT NOT NULL DEFAULT '',
                enabled      INTEGER NOT NULL DEFAULT 1,
                line_user_id TEXT NOT NULL DEFAULT ''
            )",
        )
        .execute(&pool)
        .await
        .into_diagnostic()
        .wrap_err("初始化 accounts 資料表失敗")?;

        Ok(Self { pool })
    }

    // ── 查詢 ──────────────────────────────────────────────────────────────────

    /// 取得所有帳號（依 id 排序）。
    pub async fn list(&self) -> Result<Vec<RawAccountConfig>> {
        let rows = sqlx::query_as::<_, AccountRow>(
            "SELECT id, provider, username, password, captcha, manual_cookie, enabled, line_user_id
             FROM accounts ORDER BY id",
        )
        .fetch_all(&self.pool)
        .await
        .into_diagnostic()
        .wrap_err("列出帳號失敗")?;

        Ok(rows.into_iter().map(Into::into).collect())
    }

    /// 以 id 取得單一帳號。
    pub async fn get(&self, id: &str) -> Result<Option<RawAccountConfig>> {
        let row = sqlx::query_as::<_, AccountRow>(
            "SELECT id, provider, username, password, captcha, manual_cookie, enabled, line_user_id
             FROM accounts WHERE id = ?",
        )
        .bind(id)
        .fetch_optional(&self.pool)
        .await
        .into_diagnostic()
        .wrap_err_with(|| format!("取得帳號 `{id}` 失敗"))?;

        Ok(row.map(Into::into))
    }

    // ── 寫入 ──────────────────────────────────────────────────────────────────

    /// 新增帳號；若 id 已存在則回傳錯誤。
    pub async fn insert(&self, account: &RawAccountConfig) -> Result<()> {
        sqlx::query(
            "INSERT INTO accounts (id, provider, username, password, captcha, manual_cookie, enabled, line_user_id)
             VALUES (?, ?, ?, ?, ?, ?, ?, ?)",
        )
        .bind(&account.id)
        .bind(&account.provider)
        .bind(&account.username)
        .bind(&account.password)
        .bind(&account.captcha)
        .bind(&account.manual_cookie)
        .bind(account.enabled)
        .bind(&account.line_user_id)
        .execute(&self.pool)
        .await
        .into_diagnostic()
        .wrap_err_with(|| format!("新增帳號 `{}` 失敗", account.id))?;
        Ok(())
    }

    /// 新增或更新帳號（upsert）。
    pub async fn upsert(&self, account: &RawAccountConfig) -> Result<()> {
        sqlx::query(
            "INSERT INTO accounts (id, provider, username, password, captcha, manual_cookie, enabled, line_user_id)
             VALUES (?, ?, ?, ?, ?, ?, ?, ?)
             ON CONFLICT(id) DO UPDATE SET
               provider      = excluded.provider,
               username      = excluded.username,
               password      = excluded.password,
               captcha       = excluded.captcha,
               manual_cookie = excluded.manual_cookie,
               enabled       = excluded.enabled,
               line_user_id  = excluded.line_user_id",
        )
        .bind(&account.id)
        .bind(&account.provider)
        .bind(&account.username)
        .bind(&account.password)
        .bind(&account.captcha)
        .bind(&account.manual_cookie)
        .bind(account.enabled)
        .bind(&account.line_user_id)
        .execute(&self.pool)
        .await
        .into_diagnostic()
        .wrap_err_with(|| format!("Upsert 帳號 `{}` 失敗", account.id))?;
        Ok(())
    }

    /// 刪除帳號，回傳是否找到並刪除。
    pub async fn delete(&self, id: &str) -> Result<bool> {
        let result = sqlx::query("DELETE FROM accounts WHERE id = ?")
            .bind(id)
            .execute(&self.pool)
            .await
            .into_diagnostic()
            .wrap_err_with(|| format!("刪除帳號 `{id}` 失敗"))?;

        Ok(result.rows_affected() > 0)
    }

    /// 設定帳號啟用狀態，回傳是否找到並更新。
    pub async fn set_enabled(&self, id: &str, enabled: bool) -> Result<bool> {
        let result = sqlx::query("UPDATE accounts SET enabled = ? WHERE id = ?")
            .bind(enabled)
            .bind(id)
            .execute(&self.pool)
            .await
            .into_diagnostic()
            .wrap_err_with(|| format!("更新帳號 `{id}` 啟用狀態失敗"))?;

        Ok(result.rows_affected() > 0)
    }

    // ── 解析 ──────────────────────────────────────────────────────────────────

    /// 從資料庫載入所有帳號並解析成可執行的 `AccountConfig` 清單。
    pub async fn resolve(&self, app: &AppConfig) -> Result<Vec<AccountConfig>> {
        let raw = self.list().await?;
        AccountsFile { accounts: raw }.resolve(app)
    }
}
