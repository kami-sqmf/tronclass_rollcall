# Changelog

All notable changes to this project will be documented in this file.

The format loosely follows [Keep a Changelog](https://keepachangelog.com/en/1.1.0/), and this project currently uses pre-1.0 versioning.

## [Unreleased]

### Added

- Drafted project documentation covering installation, configuration, account management, Line Bot usage, provider extension, project structure, and security notes.
- Added this changelog.
- Allowed non-admin Line users with matching `line_user_id` bindings to query their own Tronclass account status with `/status`.
- Allowed Line users to send a sticker to receive the same current status response as `/status`.
- Ignored Line message and postback events from group or room sources so commands only run in one-on-one user conversations.

## [0.1.0] - 2026-04-23

### Added

- Added Rust CLI entrypoint with `init`, `account`, `--validate`, `--help`, and `--version` support.
- Added TOML-based application configuration with provider-specific API, radar, brute-force, QR Code, Line Bot, logging, and monitor settings.
- Added SQLite-backed account storage with `account list`, `show`, `add`, `remove`, `enable`, and `disable` commands.
- Added multi-account monitor startup with per-account login and polling loop.
- Added Tronclass API client for profile, rollcall list, number rollcall, radar rollcall, and QR Code rollcall endpoints.
- Added provider-based authentication flow infrastructure, including FJU provider support.
- Added number rollcall handling for `0000` through `9999`.
- Added dynamic failure control for number rollcall:
  - Separate normal wrong-code responses from transient failures.
  - Count `429`, `408`, `5xx`, timeout/connect errors, and unexpected number-rollcall responses as transient failures.
  - Cool down and reduce concurrency when thresholds are exceeded.
  - Stop after the configured maximum cooldown count.
- Added radar rollcall handling with coordinate attempts and distance-based candidate calculation.
- Added QR Code parsing and QR Code rollcall submission.
- Added Line Bot webhook integration:
  - Signature verification.
  - Push notifications.
  - `/status`, `/start`, `/stop`, `/force`, `/reauth`, and `/help` commands.
  - QR Code URL or `p` parameter forwarding.
- Added unit tests across config, API models, Line adapter, monitor, number rollcall, QR Code parsing, radar utilities, and rollcall orchestration.

### Changed

- Switched account configuration from static TOML examples toward CLI-managed SQLite account storage.
- Updated config examples to provider-scoped sections such as `[providers.fju.api]`, `[providers.fju.brute_force]`, and `[adapters.line_bot]`.
- Extended `AttendanceResult` with `TransientFailure` so number rollcall can distinguish abnormal server/network behavior from normal wrong-code attempts.

### Security

- Documented that `config/config.toml`, `config/accounts.db`, Line credentials, and account passwords should not be committed.
- Added safer defaults for number rollcall failure handling through cooldown and minimum-concurrency controls.
