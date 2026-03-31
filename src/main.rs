//! Tronclass Rollcall — 程式進入點
//!
//! 負責：
//! 1. 解析命令列參數（設定檔路徑）
//! 2. 初始化日誌（tracing）
//! 3. 載入並驗證設定（config.toml + accounts.toml）
//! 4. 建立監控器上下文（每帳號各自登錄 + 共用 Line Bot）
//! 5. 啟動 Webhook 伺服器（若 Line Bot 啟用）
//! 6. 進入主監控循環
//! 7. 優雅關閉（捕捉 Ctrl+C / SIGTERM）
//!
//! Credits:
//! [KrsMt-0113/XMU-Rollcall-Bot](https://github.com/KrsMt-0113/XMU-Rollcall-Bot)

mod api;
mod auth;
mod config;
mod line_bot;
mod monitor;
mod rollcall;

use std::path::PathBuf;
use std::sync::Arc;

use miette::Result;
use tracing::{error, info, warn};
use tracing_subscriber::{fmt, prelude::*, EnvFilter};

// ─── 命令列引數 ───────────────────────────────────────────────────────────────

/// CLI 子命令
enum Subcommand {
    /// 正常啟動（預設）
    Run { validate_only: bool },
    /// 初始化設定檔
    Init { force: bool },
}

/// 命令列引數結構
struct CliArgs {
    /// 設定檔路徑（預設：`./config.toml`）
    config_path: PathBuf,

    /// 帳號檔路徑（預設：`./accounts.toml`）
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
        let mut config_path = PathBuf::from("config.toml");
        let mut accounts_path = PathBuf::from("accounts.toml");
        let mut show_version = false;
        let mut show_help = false;
        let mut validate_only = false;
        let mut is_init = false;
        let mut init_force = false;

        let mut i = 1;
        while i < args.len() {
            match args[i].as_str() {
                "-v" | "--version" => show_version = true,
                "-h" | "--help" => show_help = true,
                "--validate" => validate_only = true,
                "init" => is_init = true,
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
                other => {
                    // 如果是第一個非 flag 引數，當作 config 路徑
                    if !other.starts_with('-') && config_path == PathBuf::from("config.toml") {
                        config_path = PathBuf::from(other);
                    } else {
                        eprintln!("未知引數：{other}");
                        std::process::exit(1);
                    }
                }
            }
            i += 1;
        }

        let subcommand = if is_init {
            Subcommand::Init { force: init_force }
        } else {
            Subcommand::Run { validate_only }
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
    println!("  fju_ghost [選項] [設定檔路徑]");
    println!("  fju_ghost init [--force] [-c <PATH>] [-a <PATH>]");
    println!();
    println!("子命令：");
    println!("  init                 產生預設 config.toml 與 accounts.toml");
    println!("    --force, -f        覆蓋已存在的檔案");
    println!();
    println!("選項：");
    println!("  -c, --config <PATH>  指定設定檔路徑（預設：./config.toml）");
    println!("  -a, --accounts <PATH> 指定帳號檔路徑（預設：./accounts.toml）");
    println!("  --validate           只驗證設定檔，不啟動程式");
    println!("  -v, --version        顯示版本資訊");
    println!("  -h, --help           顯示此說明");
    println!();
    println!("環境變數：");
    println!("  FJU_GHOST__API__BASE_URL      覆蓋設定檔中的 API base URL");
    println!("  RUST_LOG                      控制日誌等級（例如：info, debug）");
    println!();
    println!("更多資訊：");
    println!("  https://github.com/your-username/fju_ghost");
}

// ─── 設定初始化 ───────────────────────────────────────────────────────────────

const CONFIG_EXAMPLE: &str = include_str!("../config.toml.example");
const ACCOUNTS_EXAMPLE: &str = include_str!("../accounts.toml.example");

fn run_init(config_path: &PathBuf, accounts_path: &PathBuf, force: bool) -> std::io::Result<()> {
    write_init_file(config_path, CONFIG_EXAMPLE, force)?;
    write_init_file(accounts_path, ACCOUNTS_EXAMPLE, force)?;
    Ok(())
}

fn write_init_file(path: &PathBuf, content: &str, force: bool) -> std::io::Result<()> {
    if path.exists() && !force {
        eprintln!("⚠️  已存在：{}（使用 --force 強制覆蓋）", path.display());
        return Ok(());
    }
    std::fs::write(path, content)?;
    println!("✅ 已建立：{}", path.display());
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
    // 若 RUST_LOG 已設定，優先使用
    let filter = if std::env::var("RUST_LOG").is_ok() {
        EnvFilter::from_default_env()
    } else {
        EnvFilter::try_new(log_level).unwrap_or_else(|_| EnvFilter::new("info"))
    };

    tracing_subscriber::registry()
        .with(
            fmt::layer()
                .with_target(true)
                .with_thread_ids(false)
                .with_file(false)
                .with_line_number(false)
                // 在 terminal 輸出彩色格式
                .with_ansi(supports_ansi()),
        )
        .with(filter)
        .init();
}

/// 判斷終端是否支援 ANSI 彩色輸出
fn supports_ansi() -> bool {
    // 若輸出不是 TTY（例如重定向到檔案），不使用顏色
    #[cfg(unix)]
    {
        use std::os::unix::io::AsRawFd;
        libc_isatty(std::io::stderr().as_raw_fd())
    }
    #[cfg(not(unix))]
    {
        true // Windows 現代終端通常支援 ANSI
    }
}

#[cfg(unix)]
fn libc_isatty(fd: i32) -> bool {
    extern "C" {
        fn isatty(fd: i32) -> i32;
    }
    unsafe { isatty(fd) != 0 }
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

    // ── init 子命令 ────────────────────────────────────────────────────────────
    if let Subcommand::Init { force } = args.subcommand {
        run_init(&args.config_path, &args.accounts_path, force)
            .map_err(|e| miette::miette!("init 失敗：{e}"))?;
        return Ok(());
    }

    eprintln!("🎓 FJU Ghost Student v{VERSION} 啟動中...");

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
            eprintln!("  - 可以複製 config.toml.example 為 config.toml 並填入設定");
            eprintln!("  - 再複製 accounts.toml.example 為 accounts.toml 並填入帳號");
            return Err(e);
        }
    };

    let accounts_file = match config::AccountsFile::load(&args.accounts_path) {
        Ok(accounts) => {
            info!(
                accounts_path = %args.accounts_path.display(),
                "帳號檔載入成功"
            );
            accounts
        }
        Err(e) => {
            error!(
                error = %e,
                accounts_path = %args.accounts_path.display(),
                "帳號檔載入失敗"
            );
            eprintln!("\n❌ 帳號檔載入失敗：{e}");
            return Err(e);
        }
    };

    // ── 4. 驗證設定 ────────────────────────────────────────────────────────────
    if let Err(e) = config.validate() {
        error!(error = %e, "設定驗證失敗");
        eprintln!("\n❌ 設定驗證失敗：{e}");
        return Err(e);
    }

    let accounts = match accounts_file.resolve(&config) {
        Ok(accounts) => accounts,
        Err(e) => {
            error!(error = %e, "帳號設定驗證失敗");
            eprintln!("\n❌ 帳號設定驗證失敗：{e}");
            return Err(e);
        }
    };

    info!(account_count = accounts.len(), "設定驗證通過");

    // 若只是驗證，到此結束
    let validate_only = matches!(
        args.subcommand,
        Subcommand::Run {
            validate_only: true
        }
    );
    if validate_only {
        println!("✅ 設定檔驗證通過：{}", args.config_path.display());
        println!("✅ 帳號檔驗證通過：{}", args.accounts_path.display());
        println!("啟用帳號數：{}", accounts.len());
        println!("{config}");
        return Ok(());
    }

    // config 載入後才初始化 tracing，確保使用設定檔中的等級
    init_tracing(&config.logging.level);

    info!(
        version = VERSION,
        config = %args.config_path.display(),
        accounts = %args.accounts_path.display(),
        log_level = %config.logging.level,
        "設定載入完成"
    );

    let config = Arc::new(config);
    let line_bot = if config.line_bot.enabled {
        match line_bot::LineBotClient::new(&config.line_bot) {
            Ok(bot) => Some(Arc::new(bot)),
            Err(e) => {
                warn!(error = %e, "Line Bot 初始化失敗，將在無 Line Bot 模式下運行");
                None
            }
        }
    } else {
        None
    };

    // ── 5. 建立監控器上下文 ────────────────────────────────────────────────────
    info!(account_count = accounts.len(), "初始化監控器...");
    let interactive_line = line_bot.is_some() && accounts.len() == 1;
    if line_bot.is_some() && accounts.len() > 1 {
        warn!("多帳號模式下暫不啟用 Webhook 控制與 QR Code 回傳；保留推播通知");
    }

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

    // ── 6. 啟動 Webhook 伺服器（背景任務） ───────────────────────────────────
    let webhook_handle = if interactive_line {
        if let Some(webhook_state) = contexts.get_mut(0).and_then(|ctx| ctx.webhook_state.take()) {
            let port = config.line_bot.webhook_port;
            let webhook_path = config.line_bot.webhook_path.clone();

            info!(port = port, path = %webhook_path, "啟動 Line Bot Webhook 伺服器...");

            let handle = tokio::spawn(async move {
                if let Err(e) =
                    line_bot::start_webhook_server(webhook_state, port, &webhook_path).await
                {
                    error!(error = %e, "Webhook 伺服器異常退出");
                }
            });

            tokio::time::sleep(std::time::Duration::from_millis(100)).await;
            info!(port = port, "✅ Webhook 伺服器已在 port {} 啟動", port);

            Some(handle)
        } else {
            warn!("Line Bot 啟用但 Webhook 狀態未初始化，跳過 Webhook 伺服器");
            None
        }
    } else {
        info!("目前模式未啟用 Webhook 伺服器");
        None
    };

    // ── 7. 設定優雅關閉 ───────────────────────────────────────────────────────
    // 在背景監聽 Ctrl+C（SIGINT）和 SIGTERM
    let shutdown_notify = Arc::new(tokio::sync::Notify::new());
    let shutdown_notify_clone = Arc::clone(&shutdown_notify);

    tokio::spawn(async move {
        wait_for_shutdown_signal().await;
        info!("收到關閉信號，準備優雅關閉...");
        shutdown_notify_clone.notify_one();
    });

    // ── 8. 進入主監控循環（帶優雅關閉） ──────────────────────────────────────
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

    // ── 9. 清理資源 ───────────────────────────────────────────────────────────
    info!("清理資源...");

    monitor_tasks.abort_all();

    // 取消 Webhook 伺服器
    if let Some(handle) = webhook_handle {
        handle.abort();
        info!("Webhook 伺服器已關閉");
    }

    info!("👋 FJU Rollcall 已關閉，再見！");

    Ok(())
}

// ─── 關閉信號 ─────────────────────────────────────────────────────────────────

/// 等待關閉信號（Ctrl+C 或 SIGTERM）
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
        // Windows 只支援 Ctrl+C
        tokio::signal::ctrl_c()
            .await
            .expect("Failed to register Ctrl+C handler");
        info!("收到 Ctrl+C 信號");
    }
}

// ─── 啟動橫幅 ─────────────────────────────────────────────────────────────────

/// 印出啟動橫幅（ASCII art）
#[allow(dead_code)]
fn print_banner() {
    println!(
        r#"
  _____ _ _   _    _  ____  _               _
 |  ___| | | | |  | |/ ___|| |__   ___  ___| |_
 | |_  | | | | |  | | |  _ | '_ \ / _ \/ __| __|
 |  _| | | |_| | _| | |_| || | | | (_) \__ \ |_
 |_|   | |\___/|_|\_\\____||_| |_|\___/|___/\__|
       |_|
  ____  _             _            _
 / ___|| |_ _   _  __| | ___ _ __ | |_
 \___ \| __| | | |/ _` |/ _ \ '_ \| __|
  ___) | |_| |_| | (_| |  __/ | | | |_
 |____/ \__|\__,_|\__,_|\___|_| |_|\__|

  v{VERSION} — 輔仁大學 Tronclass 自動簽到系統
"#
    );
}
