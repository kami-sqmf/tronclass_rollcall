//! 帳號資料庫模組
//!
//! 資料庫檔案預設為 `accounts.db`，可透過 `-a/--accounts` 指定路徑。

use std::path::Path;

use miette::{IntoDiagnostic, Result, WrapErr};
use sqlx::{sqlite::SqlitePoolOptions, FromRow, SqlitePool};

use crate::account::{AccountConfig, AccountsFile, RawAccountConfig};
use crate::config::AppConfig;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum UsernameUpdateResult {
    Updated { account_id: String },
    NotFound,
    Ambiguous { account_ids: Vec<String> },
}

// ─── 資料庫列映射 ─────────────────────────────────────────────────────────────

#[derive(Debug, FromRow)]
struct AccountRow {
    id: String,
    provider: String,
    username: String,
    password: String,
    enabled: bool,
    line_user_id: String,
    discord_user_id: String,
}

impl From<AccountRow> for RawAccountConfig {
    fn from(row: AccountRow) -> Self {
        RawAccountConfig {
            id: row.id,
            provider: row.provider,
            username: row.username,
            password: row.password,
            enabled: row.enabled,
            line_user_id: row.line_user_id,
            discord_user_id: row.discord_user_id,
        }
    }
}

// ─── AccountsDb ───────────────────────────────────────────────────────────────

#[derive(Clone)]
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
                enabled      INTEGER NOT NULL DEFAULT 1,
                line_user_id TEXT NOT NULL DEFAULT '',
                discord_user_id TEXT NOT NULL DEFAULT ''
            )",
        )
        .execute(&pool)
        .await
        .into_diagnostic()
        .wrap_err("初始化 accounts 資料表失敗")?;

        ensure_column(&pool, "discord_user_id", "TEXT NOT NULL DEFAULT ''").await?;

        Ok(Self { pool })
    }

    // ── 查詢 ──────────────────────────────────────────────────────────────────

    /// 取得所有帳號（依 id 排序）。
    pub async fn list(&self) -> Result<Vec<RawAccountConfig>> {
        let rows = sqlx::query_as::<_, AccountRow>(
            "SELECT id, provider, username, password, enabled, line_user_id, discord_user_id
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
            "SELECT id, provider, username, password, enabled, line_user_id, discord_user_id
             FROM accounts WHERE id = ?",
        )
        .bind(id)
        .fetch_optional(&self.pool)
        .await
        .into_diagnostic()
        .wrap_err_with(|| format!("取得帳號 `{id}` 失敗"))?;

        Ok(row.map(Into::into))
    }

    /// 以 Tronclass username 查詢帳號；provider 可用來消除同 username 多 provider 的歧義。
    pub async fn find_by_username(
        &self,
        username: &str,
        provider: Option<&str>,
    ) -> Result<Vec<RawAccountConfig>> {
        let rows = if let Some(provider) = provider.filter(|v| !v.trim().is_empty()) {
            sqlx::query_as::<_, AccountRow>(
                "SELECT id, provider, username, password, enabled, line_user_id, discord_user_id
                 FROM accounts WHERE username = ? AND provider = ? ORDER BY id",
            )
            .bind(username)
            .bind(provider)
            .fetch_all(&self.pool)
            .await
            .into_diagnostic()
            .wrap_err_with(|| format!("以 username `{username}` 查詢帳號失敗"))?
        } else {
            sqlx::query_as::<_, AccountRow>(
                "SELECT id, provider, username, password, enabled, line_user_id, discord_user_id
                 FROM accounts WHERE username = ? ORDER BY id",
            )
            .bind(username)
            .fetch_all(&self.pool)
            .await
            .into_diagnostic()
            .wrap_err_with(|| format!("以 username `{username}` 查詢帳號失敗"))?
        };

        Ok(rows.into_iter().map(Into::into).collect())
    }

    // ── 寫入 ──────────────────────────────────────────────────────────────────

    /// 新增帳號；若 id 已存在則回傳錯誤。
    pub async fn insert(&self, account: &RawAccountConfig) -> Result<()> {
        sqlx::query(
            "INSERT INTO accounts (id, provider, username, password, enabled, line_user_id, discord_user_id)
             VALUES (?, ?, ?, ?, ?, ?, ?)",
        )
        .bind(&account.id)
        .bind(&account.provider)
        .bind(&account.username)
        .bind(&account.password)
        .bind(account.enabled)
        .bind(&account.line_user_id)
        .bind(&account.discord_user_id)
        .execute(&self.pool)
        .await
        .into_diagnostic()
        .wrap_err_with(|| format!("新增帳號 `{}` 失敗", account.id))?;
        Ok(())
    }

    /// 新增或更新帳號（upsert）。
    pub async fn upsert(&self, account: &RawAccountConfig) -> Result<()> {
        sqlx::query(
            "INSERT INTO accounts (id, provider, username, password, enabled, line_user_id, discord_user_id)
             VALUES (?, ?, ?, ?, ?, ?, ?)
             ON CONFLICT(id) DO UPDATE SET
               provider      = excluded.provider,
               username      = excluded.username,
               password      = excluded.password,
               enabled       = excluded.enabled,
               line_user_id  = excluded.line_user_id,
               discord_user_id = excluded.discord_user_id",
        )
        .bind(&account.id)
        .bind(&account.provider)
        .bind(&account.username)
        .bind(&account.password)
        .bind(account.enabled)
        .bind(&account.line_user_id)
        .bind(&account.discord_user_id)
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

    /// 更新帳號綁定的 Line User ID，回傳是否找到並更新。
    pub async fn set_line_user_id(&self, id: &str, line_user_id: &str) -> Result<bool> {
        let result = sqlx::query("UPDATE accounts SET line_user_id = ? WHERE id = ?")
            .bind(line_user_id)
            .bind(id)
            .execute(&self.pool)
            .await
            .into_diagnostic()
            .wrap_err_with(|| format!("更新帳號 `{id}` Line User ID 失敗"))?;

        Ok(result.rows_affected() > 0)
    }

    /// 更新帳號綁定的 Discord User ID，回傳是否找到並更新。
    pub async fn set_discord_user_id(&self, id: &str, discord_user_id: &str) -> Result<bool> {
        let result = sqlx::query("UPDATE accounts SET discord_user_id = ? WHERE id = ?")
            .bind(discord_user_id)
            .bind(id)
            .execute(&self.pool)
            .await
            .into_diagnostic()
            .wrap_err_with(|| format!("更新帳號 `{id}` Discord User ID 失敗"))?;

        Ok(result.rows_affected() > 0)
    }

    /// 以 Tronclass username 更新 Discord User ID。
    pub async fn set_discord_user_id_by_username(
        &self,
        username: &str,
        provider: Option<&str>,
        discord_user_id: &str,
    ) -> Result<UsernameUpdateResult> {
        let matches = self.find_by_username(username, provider).await?;
        match matches.as_slice() {
            [] => Ok(UsernameUpdateResult::NotFound),
            [account] => {
                self.set_discord_user_id(&account.id, discord_user_id)
                    .await?;
                Ok(UsernameUpdateResult::Updated {
                    account_id: account.id.clone(),
                })
            }
            accounts => Ok(UsernameUpdateResult::Ambiguous {
                account_ids: accounts.iter().map(|account| account.id.clone()).collect(),
            }),
        }
    }

    // ── 解析 ──────────────────────────────────────────────────────────────────

    /// 從資料庫載入所有帳號並解析成可執行的 `AccountConfig` 清單。
    pub async fn resolve(&self, app: &AppConfig) -> Result<Vec<AccountConfig>> {
        let raw = self.list().await?;
        AccountsFile { accounts: raw }.resolve(app)
    }
}

async fn ensure_column(pool: &SqlitePool, column: &str, definition: &str) -> Result<()> {
    let rows = sqlx::query_as::<_, (String,)>("SELECT name FROM pragma_table_info('accounts')")
        .fetch_all(pool)
        .await
        .into_diagnostic()
        .wrap_err("檢查 accounts schema 失敗")?;

    if rows.iter().any(|(name,)| name == column) {
        return Ok(());
    }

    sqlx::query(&format!(
        "ALTER TABLE accounts ADD COLUMN {column} {definition}"
    ))
    .execute(pool)
    .await
    .into_diagnostic()
    .wrap_err_with(|| format!("新增 accounts.{column} 欄位失敗"))?;

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::NamedTempFile;

    #[tokio::test]
    async fn discord_user_id_is_inserted_and_updated() {
        let file = NamedTempFile::new().unwrap();
        let db = AccountsDb::open(file.path()).await.unwrap();
        let account = RawAccountConfig {
            id: "acc1".to_string(),
            provider: "fju".to_string(),
            username: "student".to_string(),
            password: "secret".to_string(),
            enabled: true,
            line_user_id: "Uline".to_string(),
            discord_user_id: "100".to_string(),
        };

        db.insert(&account).await.unwrap();
        let loaded = db.get("acc1").await.unwrap().unwrap();
        assert_eq!(loaded.discord_user_id, "100");

        assert!(db.set_discord_user_id("acc1", "200").await.unwrap());
        let loaded = db.get("acc1").await.unwrap().unwrap();
        assert_eq!(loaded.discord_user_id, "200");
    }

    #[tokio::test]
    async fn discord_user_id_can_update_by_tronclass_username() {
        let file = NamedTempFile::new().unwrap();
        let db = AccountsDb::open(file.path()).await.unwrap();
        db.insert(&RawAccountConfig {
            id: "acc1".to_string(),
            provider: "fju".to_string(),
            username: "student001".to_string(),
            password: "secret".to_string(),
            enabled: true,
            line_user_id: String::new(),
            discord_user_id: String::new(),
        })
        .await
        .unwrap();

        let result = db
            .set_discord_user_id_by_username("student001", None, "123")
            .await
            .unwrap();
        assert_eq!(
            result,
            UsernameUpdateResult::Updated {
                account_id: "acc1".to_string()
            }
        );
        let loaded = db.get("acc1").await.unwrap().unwrap();
        assert_eq!(loaded.discord_user_id, "123");
    }

    #[tokio::test]
    async fn username_binding_reports_ambiguous_matches() {
        let file = NamedTempFile::new().unwrap();
        let db = AccountsDb::open(file.path()).await.unwrap();
        for (id, provider) in [("acc1", "fju"), ("acc2", "tku")] {
            db.insert(&RawAccountConfig {
                id: id.to_string(),
                provider: provider.to_string(),
                username: "student001".to_string(),
                password: "secret".to_string(),
                enabled: true,
                line_user_id: String::new(),
                discord_user_id: String::new(),
            })
            .await
            .unwrap();
        }

        let result = db
            .set_discord_user_id_by_username("student001", None, "123")
            .await
            .unwrap();
        assert_eq!(
            result,
            UsernameUpdateResult::Ambiguous {
                account_ids: vec!["acc1".to_string(), "acc2".to_string()]
            }
        );

        let result = db
            .set_discord_user_id_by_username("student001", Some("fju"), "123")
            .await
            .unwrap();
        assert_eq!(
            result,
            UsernameUpdateResult::Updated {
                account_id: "acc1".to_string()
            }
        );
    }

    #[tokio::test]
    async fn open_migrates_existing_accounts_table() {
        let file = NamedTempFile::new().unwrap();
        let url = format!("sqlite://{}?mode=rwc", file.path().display());
        let pool = SqlitePoolOptions::new()
            .max_connections(1)
            .connect(&url)
            .await
            .unwrap();
        sqlx::query(
            "CREATE TABLE accounts (
                id TEXT PRIMARY KEY NOT NULL,
                provider TEXT NOT NULL DEFAULT '',
                username TEXT NOT NULL DEFAULT '',
                password TEXT NOT NULL DEFAULT '',
                enabled INTEGER NOT NULL DEFAULT 1,
                line_user_id TEXT NOT NULL DEFAULT ''
            )",
        )
        .execute(&pool)
        .await
        .unwrap();
        drop(pool);

        let db = AccountsDb::open(file.path()).await.unwrap();
        assert!(db.set_discord_user_id("missing", "1").await.unwrap() == false);
    }
}
