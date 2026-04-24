//! Tronclass Rollcall — 程式進入點
//!
//! 負責：
//! 1. 解析命令列參數（設定檔路徑）
//! 2. 初始化日誌（tracing）
//! 3. 載入並驗證設定（config.toml + accounts.db）
//! 4. 建立監控器上下文（每帳號各自登錄 + 共用 Line Bot）
//! 5. 啟動 Webhook 伺服器（若 Line Bot 啟用）
//! 6. 進入主監控循環
//! 7. 優雅關閉（捕捉 Ctrl+C / SIGTERM）
//!
//! Credits:
//! [KrsMt-0113/XMU-Rollcall-Bot](https://github.com/KrsMt-0113/XMU-Rollcall-Bot)

mod account;
mod adapters;
mod api;
mod auth;
mod config;
mod db;
mod monitor;
mod rollcalls;

use std::path::PathBuf;
use std::sync::Arc;

use miette::Result;
use tracing::{error, info, warn};
use tracing_subscriber::{
    fmt::{self, format::Writer, time::FormatTime},
    prelude::*,
    EnvFilter,
};

// ─── 帳號管理子命令 ───────────────────────────────────────────────────────────

enum AccountCmd {
    List,
    Show {
        id: String,
    },
    Add {
        id: String,
        provider: String,
        username: String,
        password: String,
        line_user_id: String,
        enabled: bool,
    },
    Remove {
        id: String,
    },
    Enable {
        id: String,
    },
    Disable {
        id: String,
    },
}

impl AccountCmd {
    fn parse_from(args: &[String], i: &mut usize) -> Self {
        let subcmd = args.get(*i).map(String::as_str).unwrap_or("");
        match subcmd {
            "list" => Self::List,
            "show" => {
                *i += 1;
                let id = cli_require_positional(args, *i, "show", "<id>");
                Self::Show { id }
            }
            "add" => {
                *i += 1;
                let id = cli_require_positional(args, *i, "add", "<id>");
                let mut provider = String::new();
                let mut username = String::new();
                let mut password = String::new();
                let mut line_user_id = String::new();
                let mut enabled = true;
                *i += 1;
                while *i < args.len() {
                    match args[*i].as_str() {
                        "-p" | "--provider" => {
                            *i += 1;
                            provider = cli_require_value(args, *i, "--provider");
                        }
                        "-u" | "--username" => {
                            *i += 1;
                            username = cli_require_value(args, *i, "--username");
                        }
                        "-P" | "--password" => {
                            *i += 1;
                            password = cli_require_value(args, *i, "--password");
                        }
                        "--line-user-id" => {
                            *i += 1;
                            line_user_id = cli_require_value(args, *i, "--line-user-id");
                        }
                        "--disabled" => enabled = false,
                        other => {
                            eprintln!("未知引數：{other}");
                            std::process::exit(1);
                        }
                    }
                    *i += 1;
                }
                if provider.is_empty() {
                    eprintln!("錯誤：account add 需要 -p/--provider");
                    std::process::exit(1);
                }
                if username.is_empty() {
                    eprintln!("錯誤：account add 需要 -u/--username");
                    std::process::exit(1);
                }
                if password.is_empty() {
                    eprintln!("錯誤：account add 需要 -P/--password");
                    std::process::exit(1);
                }
                Self::Add {
                    id,
                    provider,
                    username,
                    password,
                    line_user_id,
                    enabled,
                }
            }
            "remove" => {
                *i += 1;
                let id = cli_require_positional(args, *i, "remove", "<id>");
                Self::Remove { id }
            }
            "enable" => {
                *i += 1;
                let id = cli_require_positional(args, *i, "enable", "<id>");
                Self::Enable { id }
            }
            "disable" => {
                *i += 1;
                let id = cli_require_positional(args, *i, "disable", "<id>");
                Self::Disable { id }
            }
            "" => {
                eprintln!(
                    "錯誤：account 需要子命令\n用法：account <list|show|add|remove|enable|disable>"
                );
                std::process::exit(1);
            }
            other => {
                eprintln!("未知的 account 子命令：{other}");
                std::process::exit(1);
            }
        }
    }
}

fn cli_require_positional(args: &[String], i: usize, cmd: &str, name: &str) -> String {
    match args.get(i) {
        Some(v) if !v.starts_with('-') => v.clone(),
        _ => {
            eprintln!("錯誤：account {cmd} 需要 {name}");
            std::process::exit(1);
        }
    }
}

fn cli_require_value(args: &[String], i: usize, flag: &str) -> String {
    match args.get(i) {
        Some(v) => v.clone(),
        None => {
            eprintln!("錯誤：{flag} 需要一個值");
            std::process::exit(1);
        }
    }
}

// ─── 命令列引數 ───────────────────────────────────────────────────────────────

/// CLI 子命令
enum Subcommand {
    /// 正常啟動（預設）
    Run { validate_only: bool },
    /// 初始化設定檔
    Init { force: bool },
    /// 帳號管理
    Account(AccountCmd),
}

/// 命令列引數結構
struct CliArgs {
    /// 設定檔路徑（預設：`./config/config.toml`）
    config_path: PathBuf,

    /// 帳號資料庫路徑（預設：`./config/accounts.db`）
    accounts_path: PathBuf,

    /// 是否印出版本資訊後退出
    show_version: bool,

    /// 是否印出說明後退出
    show_help: bool,

    /// 子命令
    subcommand: Subcommand,
}

impl CliArgs {
    fn parse() -> Self {
        let args: Vec<String> = std::env::args().collect();
        let mut config_path = PathBuf::from("config/config.toml");
        let mut accounts_path = PathBuf::from("config/accounts.db");
        let mut show_version = false;
        let mut show_help = false;
        let mut validate_only = false;
        let mut init_force = false;
        let mut subcommand_tag: Option<&'static str> = None;
        let mut account_cmd: Option<AccountCmd> = None;

        let mut i = 1usize;
        while i < args.len() {
            match args[i].as_str() {
                "-v" | "--version" => show_version = true,
                "-h" | "--help" => show_help = true,
                "--validate" => validate_only = true,
                "--force" | "-f" => init_force = true,
                "-c" | "--config" => {
                    i += 1;
                    if i < args.len() {
                        config_path = PathBuf::from(&args[i]);
                    } else {
                        eprintln!("錯誤：-c/--config 需要指定路徑");
                        std::process::exit(1);
                    }
                }
                "-a" | "--accounts" => {
                    i += 1;
                    if i < args.len() {
                        accounts_path = PathBuf::from(&args[i]);
                    } else {
                        eprintln!("錯誤：-a/--accounts 需要指定路徑");
                        std::process::exit(1);
                    }
                }
                "init" => subcommand_tag = Some("init"),
                "account" => {
                    subcommand_tag = Some("account");
                    i += 1;
                    account_cmd = Some(AccountCmd::parse_from(&args, &mut i));
                    break;
                }
                other if !other.starts_with('-') && subcommand_tag.is_none() => {
                    config_path = PathBuf::from(other);
                }
                other => {
                    eprintln!("未知引數：{other}");
                    std::process::exit(1);
                }
            }
            i += 1;
        }

        let subcommand = match subcommand_tag {
            Some("init") => Subcommand::Init { force: init_force },
            Some("account") => Subcommand::Account(account_cmd.unwrap()),
            _ => Subcommand::Run { validate_only },
        };

        Self {
            config_path,
            accounts_path,
            show_version,
            show_help,
            subcommand,
        }
    }
}

// ─── 版本資訊 ─────────────────────────────────────────────────────────────────

const VERSION: &str = env!("CARGO_PKG_VERSION");
const PKG_NAME: &str = env!("CARGO_PKG_NAME");
const DESCRIPTION: &str = env!("CARGO_PKG_DESCRIPTION");

fn print_version() {
    println!("{PKG_NAME} v{VERSION}");
    println!("{DESCRIPTION}");
}

fn print_help() {
    println!("{PKG_NAME} v{VERSION}");
    println!("{DESCRIPTION}");
    println!();
    println!("用法：");
    println!("  {PKG_NAME} [選項] [設定檔路徑]");
    println!("  {PKG_NAME} init [--force] [-c <PATH>]");
    println!("  {PKG_NAME} account <子命令> [-a <PATH>]");
    println!();
    println!("子命令：");
    println!("  init                       產生預設 config.toml");
    println!("    --force, -f              覆蓋已存在的檔案");
    println!();
    println!("  account list               列出所有帳號");
    println!("  account show <id>          顯示帳號詳情");
    println!("  account add <id>           新增帳號");
    println!("    -p, --provider <name>      Provider 名稱（必填）");
    println!("    -u, --username <user>      使用者名稱（必填）");
    println!("    -P, --password <pass>      密碼（必填）");
    println!("    --line-user-id <uid>       Line User ID（選填）");
    println!("    --disabled                 新增時停用帳號");
    println!("  account remove <id>        刪除帳號");
    println!("  account enable <id>        啟用帳號");
    println!("  account disable <id>       停用帳號");
    println!();
    println!("選項：");
    println!("  -c, --config <PATH>    指定設定檔路徑（預設：config/config.toml）");
    println!("  -a, --accounts <PATH>  指定帳號資料庫路徑（預設：config/accounts.db）");
    println!("  --validate             只驗證設定檔，不啟動程式");
    println!("  -v, --version          顯示版本資訊");
    println!("  -h, --help             顯示此說明");
    println!();
    println!("環境變數：");
    println!("  TRONCLASS_ROLLCALL__PROVIDERS__<name>__BASE_URL  覆蓋設定中的 Base URL");
    println!("  RUST_LOG                                          控制日誌等級（info, debug…）");
}

// ─── 設定初始化 ───────────────────────────────────────────────────────────────

const CONFIG_EXAMPLE: &str = include_str!("../config/config.toml.example");

fn run_init(config_path: &PathBuf, force: bool) -> std::io::Result<()> {
    write_init_file(config_path, CONFIG_EXAMPLE, force)?;
    println!("💡 帳號資料庫將在首次啟動時自動建立（預設：config/accounts.db）");
    println!("💡 使用 `{PKG_NAME} account add` 新增帳號");
    Ok(())
}

fn write_init_file(path: &PathBuf, content: &str, force: bool) -> std::io::Result<()> {
    if path.exists() && !force {
        eprintln!("⚠️  已存在：{}（使用 --force 強制覆蓋）", path.display());
        return Ok(());
    }
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    std::fs::write(path, content)?;
    println!("✅ 已建立：{}", path.display());
    Ok(())
}

// ─── 帳號管理指令執行 ─────────────────────────────────────────────────────────

async fn run_account_cmd(db: db::AccountsDb, cmd: AccountCmd) -> Result<()> {
    use account::RawAccountConfig;

    match cmd {
        AccountCmd::List => {
            let accounts = db.list().await?;
            if accounts.is_empty() {
                println!("（沒有帳號）");
            } else {
                println!(
                    "{:<20} {:<12} {:<30} {:<8} {}",
                    "ID", "Provider", "Username", "Enabled", "Line User ID"
                );
                println!("{}", "─".repeat(88));
                for a in &accounts {
                    println!(
                        "{:<20} {:<12} {:<30} {:<8} {}",
                        a.id,
                        a.provider,
                        a.username,
                        if a.enabled { "✓" } else { "✗" },
                        a.line_user_id,
                    );
                }
                println!();
                println!("共 {} 個帳號", accounts.len());
            }
        }
        AccountCmd::Show { id } => match db.get(&id).await? {
            None => {
                eprintln!("找不到帳號：{id}");
                std::process::exit(1);
            }
            Some(a) => {
                println!("ID:           {}", a.id);
                println!("Provider:     {}", a.provider);
                println!("Username:     {}", a.username);
                println!("Password:     {}", "*".repeat(a.password.len().min(16)));
                println!("Enabled:      {}", if a.enabled { "是" } else { "否" });
                println!(
                    "Line User ID: {}",
                    if a.line_user_id.is_empty() {
                        "(未設定)"
                    } else {
                        &a.line_user_id
                    }
                );
            }
        },
        AccountCmd::Add {
            id,
            provider,
            username,
            password,
            line_user_id,
            enabled,
        } => {
            let account = RawAccountConfig {
                id: id.clone(),
                provider,
                username,
                password,
                enabled,
                line_user_id,
            };
            db.insert(&account).await?;
            println!("✅ 已新增帳號：{id}");
        }
        AccountCmd::Remove { id } => {
            if db.delete(&id).await? {
                println!("✅ 已刪除帳號：{id}");
            } else {
                eprintln!("找不到帳號：{id}");
                std::process::exit(1);
            }
        }
        AccountCmd::Enable { id } => {
            if db.set_enabled(&id, true).await? {
                println!("✅ 已啟用帳號：{id}");
            } else {
                eprintln!("找不到帳號：{id}");
                std::process::exit(1);
            }
        }
        AccountCmd::Disable { id } => {
            if db.set_enabled(&id, false).await? {
                println!("✅ 已停用帳號：{id}");
            } else {
                eprintln!("找不到帳號：{id}");
                std::process::exit(1);
            }
        }
    }

    Ok(())
}

// ─── 日誌初始化 ───────────────────────────────────────────────────────────────

/// 初始化 tracing 日誌系統
///
/// 優先順序：
/// 1. `RUST_LOG` 環境變數
/// 2. `config.logging.level` 設定
/// 3. 預設 `info`
fn init_tracing(log_level: &str) {
    let filter = if std::env::var("RUST_LOG").is_ok() {
        EnvFilter::from_default_env()
    } else {
        EnvFilter::try_new(log_level).unwrap_or_else(|_| EnvFilter::new("info"))
    };

    tracing_subscriber::registry()
        .with(
            fmt::layer()
                .with_timer(LocalTimestamp)
                .with_target(true)
                .with_thread_ids(false)
                .with_file(false)
                .with_line_number(false)
                .with_ansi(supports_ansi()),
        )
        .with(filter)
        .init();
}

#[derive(Debug, Clone, Copy, Default)]
struct LocalTimestamp;

impl FormatTime for LocalTimestamp {
    fn format_time(&self, w: &mut Writer<'_>) -> std::fmt::Result {
        write!(w, "{}", chrono::Local::now().format("%Y-%m-%dT%H:%M:%S%:z"))
    }
}

fn apply_runtime_timezone(timezone: &str) -> Result<()> {
    let timezone = timezone.trim();
    if timezone.is_empty() {
        return Err(miette::miette!("time.timezone 不可為空"));
    }

    let use_system_timezone = timezone.eq_ignore_ascii_case("local");
    if use_system_timezone {
        std::env::remove_var("TZ");
    } else {
        std::env::set_var("TZ", timezone);
    }

    #[cfg(unix)]
    unsafe {
        tzset();
    }

    Ok(())
}

fn supports_ansi() -> bool {
    #[cfg(unix)]
    {
        use std::os::unix::io::AsRawFd;
        libc_isatty(std::io::stderr().as_raw_fd())
    }
    #[cfg(not(unix))]
    {
        true
    }
}

#[cfg(unix)]
fn libc_isatty(fd: i32) -> bool {
    extern "C" {
        fn isatty(fd: i32) -> i32;
    }
    unsafe { isatty(fd) != 0 }
}

#[cfg(unix)]
unsafe extern "C" {
    fn tzset();
}

// ─── 主函式 ───────────────────────────────────────────────────────────────────

#[tokio::main]
async fn main() -> Result<()> {
    // ── 1. 解析命令列引數 ──────────────────────────────────────────────────────
    let args = CliArgs::parse();

    if args.show_version {
        print_version();
        return Ok(());
    }

    if args.show_help {
        print_help();
        return Ok(());
    }

    // ── 子命令分發 ─────────────────────────────────────────────────────────────
    let validate_only = match args.subcommand {
        Subcommand::Init { force } => {
            run_init(&args.config_path, force).map_err(|e| miette::miette!("init 失敗：{e}"))?;
            return Ok(());
        }
        // account 子命令不需要 config.toml
        Subcommand::Account(cmd) => {
            let db = db::AccountsDb::open(&args.accounts_path)
                .await
                .map_err(|e| miette::miette!("無法開啟帳號資料庫：{e}"))?;
            return run_account_cmd(db, cmd).await;
        }
        Subcommand::Run { validate_only } => validate_only,
    };

    eprintln!("🎓 {PKG_NAME} v{VERSION} 啟動中...");

    // ── 2. 載入設定 ────────────────────────────────────────────────────────────
    let config = match config::AppConfig::load(&args.config_path) {
        Ok(cfg) => cfg,
        Err(e) => {
            error!(
                error = %e,
                config_path = %args.config_path.display(),
                "設定檔載入失敗"
            );
            eprintln!("\n❌ 設定檔載入失敗：{e}");
            eprintln!("\n💡 提示：");
            eprintln!("  - 請確認設定檔路徑正確：{}", args.config_path.display());
            eprintln!("  - 執行 `{PKG_NAME} init` 產生預設設定檔");
            return Err(e);
        }
    };

    let accounts_db = match db::AccountsDb::open(&args.accounts_path).await {
        Ok(db) => {
            info!(
                accounts_path = %args.accounts_path.display(),
                "帳號資料庫開啟成功"
            );
            db
        }
        Err(e) => {
            error!(
                error = %e,
                accounts_path = %args.accounts_path.display(),
                "帳號資料庫開啟失敗"
            );
            eprintln!("\n❌ 帳號資料庫開啟失敗：{e}");
            eprintln!("  - 執行 `{PKG_NAME} account add` 新增帳號");
            return Err(e);
        }
    };

    // ── 3. 驗證設定 ────────────────────────────────────────────────────────────
    if let Err(e) = config.validate() {
        error!(error = %e, "設定驗證失敗");
        eprintln!("\n❌ 設定驗證失敗：{e}");
        return Err(e);
    }

    let accounts = match accounts_db.resolve(&config).await {
        Ok(accounts) => accounts,
        Err(e) => {
            error!(error = %e, "帳號設定驗證失敗");
            eprintln!("\n❌ 帳號設定驗證失敗：{e}");
            return Err(e);
        }
    };

    info!(account_count = accounts.len(), "設定驗證通過");

    if validate_only {
        println!("✅ 設定檔驗證通過：{}", args.config_path.display());
        println!("✅ 帳號資料庫驗證通過：{}", args.accounts_path.display());
        println!("啟用帳號數：{}", accounts.len());
        println!("{config}");
        return Ok(());
    }

    apply_runtime_timezone(&config.time.timezone)?;

    // config 載入後才初始化 tracing，確保使用設定檔中的等級
    init_tracing(&config.logging.level);

    info!(
        version = VERSION,
        config = %args.config_path.display(),
        accounts_db = %args.accounts_path.display(),
        timezone = %config.time.timezone,
        log_level = %config.logging.level,
        "設定載入完成"
    );

    let config = Arc::new(config);
    let line_bot = if config.adapters.line_bot.enabled {
        match adapters::line::LineBotClient::new(&config.adapters.line_bot) {
            Ok(bot) => Some(Arc::new(bot)),
            Err(e) => {
                warn!(error = %e, "Line Bot 初始化失敗，將在無 Line Bot 模式下運行");
                None
            }
        }
    } else {
        None
    };

    // ── 4. 建立監控器上下文 ────────────────────────────────────────────────────
    info!(account_count = accounts.len(), "初始化監控器...");
    let interactive_line = line_bot.is_some();

    let mut contexts = Vec::with_capacity(accounts.len());
    for account in accounts {
        let account = Arc::new(account);
        let ctx = match monitor::MonitorContext::new(
            Arc::clone(&config),
            Arc::clone(&account),
            line_bot.as_ref().map(Arc::clone),
            interactive_line,
        )
        .await
        {
            Ok(ctx) => ctx,
            Err(e) => {
                error!(account = %account.id, error = %e, "監控器初始化失敗");
                eprintln!("\n❌ 帳號 `{}` 初始化失敗：{e}", account.id);
                return Err(e);
            }
        };
        contexts.push(ctx);
    }

    // ── 5. 啟動 Webhook 伺服器（背景任務） ───────────────────────────────────
    let webhook_handle = if interactive_line {
        if let Some(bot) = line_bot.as_ref() {
            let webhook_accounts = contexts
                .iter()
                .filter_map(|ctx| ctx.webhook_account.clone())
                .collect::<Vec<_>>();
            let webhook_state =
                adapters::line::WebhookState::new(Arc::clone(bot), webhook_accounts);
            let port = config.adapters.line_bot.webhook_port;
            let webhook_path = config.adapters.line_bot.webhook_path.clone();

            info!(port = port, path = %webhook_path, "啟動 Line Bot Webhook 伺服器...");

            let handle = tokio::spawn(async move {
                if let Err(e) =
                    adapters::line::start_webhook_server(webhook_state, port, &webhook_path).await
                {
                    error!(error = %e, "Webhook 伺服器異常退出");
                }
            });

            tokio::time::sleep(std::time::Duration::from_millis(100)).await;
            info!(port = port, "✅ Webhook 伺服器已在 port {} 啟動", port);

            Some(handle)
        } else {
            warn!("Line Bot 啟用但客戶端未初始化，跳過 Webhook 伺服器");
            None
        }
    } else {
        info!("目前模式未啟用 Webhook 伺服器");
        None
    };

    // ── 6. 設定優雅關閉 ───────────────────────────────────────────────────────
    let shutdown_notify = Arc::new(tokio::sync::Notify::new());
    let shutdown_notify_clone = Arc::clone(&shutdown_notify);

    tokio::spawn(async move {
        wait_for_shutdown_signal().await;
        info!("收到關閉信號，準備優雅關閉...");
        shutdown_notify_clone.notify_one();
    });

    // ── 7. 進入主監控循環（帶優雅關閉） ──────────────────────────────────────
    info!(account_count = contexts.len(), "🚀 開始監控簽到...");

    let mut monitor_tasks = tokio::task::JoinSet::new();
    for ctx in contexts {
        let account_id = ctx.account.id.clone();
        monitor_tasks.spawn(async move {
            match monitor::run_monitor_loop(ctx).await {
                Ok(()) => Ok(account_id),
                Err(e) => Err((account_id, e.to_string())),
            }
        });
    }

    let monitor_result = tokio::select! {
        joined = monitor_tasks.join_next() => {
            Some(joined)
        }
        _ = shutdown_notify.notified() => {
            info!("收到關閉通知，開始優雅關閉");
            None
        }
    };

    if let Some(joined) = monitor_result {
        match joined {
            Some(Ok(Ok(account_id))) => {
                warn!(account = %account_id, "監控循環提前正常結束");
            }
            Some(Ok(Err((account_id, err)))) => {
                error!(account = %account_id, error = %err, "監控循環異常結束");
                monitor_tasks.abort_all();
                return Err(miette::miette!(
                    "account `{}` monitor failed: {}",
                    account_id,
                    err
                ));
            }
            Some(Err(join_err)) => {
                monitor_tasks.abort_all();
                return Err(miette::miette!("monitor task join failed: {}", join_err));
            }
            None => {}
        }
    }

    // ── 8. 清理資源 ───────────────────────────────────────────────────────────
    info!("清理資源...");

    monitor_tasks.abort_all();

    if let Some(handle) = webhook_handle {
        handle.abort();
        info!("Webhook 伺服器已關閉");
    }

    info!("👋 {} 已關閉，再見！", PKG_NAME);

    Ok(())
}

// ─── 關閉信號 ─────────────────────────────────────────────────────────────────

async fn wait_for_shutdown_signal() {
    #[cfg(unix)]
    {
        use tokio::signal::unix::{signal, SignalKind};

        let mut sigterm =
            signal(SignalKind::terminate()).expect("Failed to register SIGTERM handler");
        let mut sigint =
            signal(SignalKind::interrupt()).expect("Failed to register SIGINT handler");

        tokio::select! {
            _ = sigterm.recv() => {
                info!("收到 SIGTERM 信號");
            }
            _ = sigint.recv() => {
                info!("收到 SIGINT (Ctrl+C) 信號");
            }
        }
    }

    #[cfg(not(unix))]
    {
        tokio::signal::ctrl_c()
            .await
            .expect("Failed to register Ctrl+C handler");
        info!("收到 Ctrl+C 信號");
    }
}
