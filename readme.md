# Tronclass Rollcall
你還在為了點名煩惱嗎？ Tronclass Rollcall 是一個可以利人利己的點名工具，支援 *數字/雷達/QrCode 點名*，可透過網頁以及 SNS Webhook 進行管理，以及多帳號同時使用。

> [!CAUTION]
> 本專案 **僅供教學/研究用途** 使用，請遵守學校校規和當地法律！維護者與貢獻者 不對任何濫用、法律風險、帳號處置等後果負責，使用者需自行負責！

## Features 功能
尚在製作中⋯⋯

## Installation 安裝指南
### 方法一 使用 Bundled 套件
很可惜，目前還尚未支援！

### 方法二 Docker
很可惜，目前還尚未支援！

### 方法三 Build from Source
在編譯本專案前，請確認 [Rust](https://rust-lang.org/tools/install/) 已經安裝在搭建環境。
#### 1. Clone the repository and navigate into the directory:
```sh
git clone https://github.com/kami-sqmf/tronclass_rollcall.git
cd tronclass_rollcall
```

#### 2. Run:
> 備註
> 編譯過程可能會花上不少時間，請耐心等候。
```sh
cargo run --release
```

## Guide 使用指南
尚在製作中⋯⋯

## 專案結構 Project Structure
```
config/
├── config.toml           # 系統設定
└── accounts.toml         # 帳號設定
src/
├── main.rs               # 主程式進入點
├── config.rs             # 設定管理模組
├── monitor.rs            # 主監控循環模組
├── api/
│   ├── mod.rs            # API 交流模組
│   ├── profile.rs        # 資訊 API
│   └── rollcall.rs       # 點名 API
├── auth/
│   ├── mod.rs            # 認證主邏輯模組
│   └── providers/        
│       ├── mod.rs        # 各校認證組態
│       └── <school>.rs   # 各校登入處理模組
├── adapters/
│   └── line/             # Line Messaging API 模組
│       ├── mod.rs          # Line Messaging API 收發模組
│       └── types.rs        # Line Messaging API 物件類型
└── rollcalls/
    ├── mod.rs            # 簽到主邏輯模組
    ├── number.rs         # 數字點名爆力破解模組
    ├── qrcode.rs         # QR Code 簽到解析與簽到邏輯模組
    └── radar.rs          # 雷達簽到模組
```

# Contributing 貢獻

歡迎提交 Issues, PR 及 Fork!
1. 新增學校 — 你可以提交 PR 到 /src/auth/providers/<your-school>.rs (mod.rs 也要記得修改！)
2. 回報問題 — 在 Github Issues 中提出你的問題
3. 新增 Adapters — 將你想要使用的 Webhook 提交上來

# License
[![MIT licensed](https://img.shields.io/badge/license-MIT-blue.svg)](https://github.com/kami-sqmf/tronclass_rollcall/blob/master/LICENSE)

歡迎自由使用、修改這份程式碼，也歡迎基於它進行開發！若您將其用於商業專案，若能標註來源，我將非常感激。謝謝！🙏

# Credits
[seven-317/Tronclass-API](https://github.com/seven-317/Tronclass-API)

[KrsMt-0113/XMU-Rollcall-Bot](https://github.com/KrsMt-0113/XMU-Rollcall-Bot)
