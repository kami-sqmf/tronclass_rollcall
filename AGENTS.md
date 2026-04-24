# AGENTS.md

本檔提供給 Codex 或其他 coding agent 作為本專案的工作指南。請優先遵守使用者當下要求；若沒有更具體指示，依照本檔行事。

## 專案概覽

`tronclass_rollcall` 是 Rust 2021 單一 binary 專案，用於 Tronclass 自動簽到與多帳號監控。主要能力包含：

- Tronclass provider 設定與帳號資料庫管理。
- CAS/Tronclass 登入、API 輪詢與 session 重新認證。
- 數字、雷達、QR Code 簽到流程。
- LINE Bot webhook、狀態查詢、控制指令與 QR Code 回傳。

入口點是 `src/main.rs`，設定範例在 `config/config.toml.example`，使用說明在 `readme.md`。

## 常用命令

開發與驗證：

```sh
cargo fmt
cargo check
cargo test
```

執行與 CLI：

```sh
cargo run -- --help
cargo run -- init
cargo run -- --validate
cargo run -- account list
cargo run --release
```

建議在修改 Rust 程式碼後至少跑 `cargo fmt` 與 `cargo test`。如果變更範圍很小且測試耗時或需要網路，至少跑 `cargo check`，並在回覆中說明未跑完整測試的原因。

## 目錄與模組責任

- `src/main.rs`：CLI 解析、設定載入、帳號初始化、adapter 啟動與監控任務 orchestration。
- `src/config.rs`：TOML 設定結構、預設值、驗證與時區/排程解析。
- `src/account.rs`、`src/db.rs`：帳號設定與 SQLite 儲存。
- `src/auth/`：登入、session/cookie 與 provider-specific auth。
- `src/api/`：Tronclass API client、rollcall/profile 型別與 API 錯誤判斷。
- `src/rollcalls/`：簽到策略；`number.rs`、`radar.rs`、`qrcode.rs` 各自處理不同簽到類型。
- `src/monitor.rs`：主輪詢循環、排程判斷、失敗重試、重新認證與 adapter 通知。
- `src/adapters/events.rs`：adapter-neutral 的事件與訊息模型。
- `src/adapters/requests.rs`：從 adapter 回來的使用者請求、控制指令、QR Code 等待通道。
- `src/adapters/line/`：LINE client、webhook、payload types 與 LINE-specific 轉換。

修改時盡量維持這些邊界：API 型別不要滲入 adapter UI，LINE payload 不要滲入 `monitor` 或 `rollcalls`，簽到策略不要直接處理 CLI。

## 編碼慣例

- 使用現有 Rust 風格與模組組織；優先延伸既有型別與 helper。
- 錯誤處理使用專案既有的 `miette`, `thiserror`, `WrapErr` 模式。
- 非同步流程使用 `tokio`；共享狀態目前多以 `Arc`、`Mutex` 與 notify/channel 管理。
- 日誌使用 `tracing`，避免 `println!` 進入長期執行路徑。CLI 使用者輸出例外。
- 中文註解與說明已是專案慣例；新增註解要精簡，只解釋不易立即看懂的流程。
- 不要引入大型新依賴，除非它明確降低複雜度且符合現有架構。

## 設定、資料與安全

- 不要提交真實的 `config/config.toml`、`config/accounts.db`、session cookie、LINE token、channel secret、學生帳密或個資。
- 更新設定欄位時，同步檢查 `config/config.toml.example`、`readme.md` 與相關 validation/tests。
- `accounts.db` 是本機 SQLite 狀態，測試請使用暫存檔或 `tempfile`，不要依賴使用者本機資料。
- 涉及自動簽到、爆破數字、定位座標或帳號控制的變更要保守，避免提高請求頻率或放寬安全檢查，除非使用者明確要求。

## 測試指引

本專案已有大量 inline unit tests，修改對應模組時請補齊或更新附近的 `mod tests`。

建議測試重點：

- `config.rs`：預設值、TOML parse、排程與 weekday/timezone 行為。
- `api/`：URL、payload、API 錯誤分類與 response parsing。
- `rollcalls/`：數字簽到結果分類、暫時性錯誤冷卻、QR Code URL/`p` 參數解析、雷達座標推估。
- `monitor.rs`：排程允許/休息日判斷、狀態更新與 reauth 觸發條件。
- `adapters/line/` 與 `adapters/requests.rs`：signature 驗證、webhook event 轉換、權限與 QR Code 請求路由。

若測試需要外部 Tronclass 或 LINE 服務，請改用 mock/fake client 或純函式測試；不要讓 unit tests 依賴真實網路或真實帳號。

## Agent 工作流程

1. 先看 `git status --short`，確認是否有使用者既有變更。不要回復或覆蓋不屬於本次工作的變更。
2. 使用 `rg` / `rg --files` 找入口與引用。
3. 小幅、聚焦地修改檔案；不要順手大重構或格式化無關檔案。
4. 修改程式碼後跑適合的 `cargo fmt`、`cargo check`、`cargo test`。
5. 最終回覆列出改了什麼、跑了哪些驗證；若沒有驗證，說明原因。

## PR / Commit 注意事項

- commit message 建議使用簡潔英文或中文祈使句，例如 `Add Line webhook request routing tests`。
- PR 描述請包含使用者可觀察行為、設定變更、測試結果與相容性風險。
- 若變更 CLI 或設定格式，請在 README 中同步更新範例。
