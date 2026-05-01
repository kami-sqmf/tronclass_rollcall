# Changelog

All notable changes to this project will be documented in this file.

The format loosely follows [Keep a Changelog](https://keepachangelog.com/en/1.1.0/).

## [0.1.2] - 2026-05-01

### Added

- Added a Discord Bot adapter with slash commands, DM status/help/QR handling, admin dashboard notifications, and account request approval buttons.
- Added `[adapters.discord]` configuration for bot credentials, admin routing, scanner public URLs, slash command registration, and optional guild-scoped command registration.
- Added Discord user bindings to accounts, the SQLite account database, account list/show output, `account add --discord-user-id`, and `account bind`.
- Added adapter fan-out delivery so enabled Line and Discord adapters can both receive monitor, rollcall, QR Code, and result notifications.
- Added a standalone QR scanner HTTP server for deployments that use Discord without the Line webhook server.

### Changed

- Route account notifications through per-adapter account targets, using Discord bindings for Discord delivery and Line bindings for Line delivery.
- Allow `/reauth` requests to interrupt the monitor wait loop immediately instead of waiting for the next polling tick.
- Treat ambiguous number-rollcall API responses as transient failures unless the response body clearly confirms success or a wrong code.

### Fixed

- Migrated existing account databases by adding the missing `discord_user_id` column on startup.
- Kept Discord username binding by Tronclass username safe when the same username exists under multiple providers by reporting ambiguity unless a provider is supplied.

## [0.1.1] - 2026-04-24

### Added

- Added a shared local QR scanner flow for Line Bot deployments so one QR scan can fan out to every account waiting on the same provider and rollcall ID.
- Added `[adapters.line_bot].public_base_url` for externally reachable scanner callback URLs served by the existing webhook server.
- Added scanner routes and a browser scanner page with camera scanning and manual QR payload fallback.

### Fixed

- Added configurable runtime timezone support through `[time].timezone` so scheduled polling follows the intended local timezone instead of the host default.
- Fixed tracing log timestamps to use the configured local timezone and include the UTC offset, keeping log time consistent with monitor scheduling.
- Updated the sample configuration and documentation to show `timezone = "Asia/Taipei"` for local deployments.

## [0.1.1] - 2026-04-24

### Added

- Drafted project documentation covering installation, configuration, account management, Line Bot usage, provider extension, project structure, and security notes.
- Added this changelog.
- Allowed non-admin Line users with matching `line_user_id` bindings to query their own Tronclass account status with `/status`.
- Allowed Line users to send a sticker to receive the same current status response as `/status`.
- Ignored Line message and postback events from group or room sources so commands only run in one-on-one user conversations.
- Routed account-related Line push notifications to the bound account `line_user_id` first, falling back to the admin user only when no binding is configured.
- Added provider-level polling schedules with simple `HH:MM~HH:MM` time ranges and Sunday rest support; FJU now defaults to `07:10~17:30` with Sunday off.

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
