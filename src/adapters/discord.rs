//! Discord adapter powered by serenity.

use std::{collections::HashMap, sync::Arc};

use async_trait::async_trait;
use miette::{IntoDiagnostic, Result, WrapErr};
use serenity::all::{
    ButtonStyle, ChannelId, Command, CommandDataOptionValue, CommandInteraction, CommandOptionType,
    ComponentInteraction, Context, CreateActionRow, CreateButton, CreateCommand,
    CreateCommandOption, CreateInteractionResponse, CreateInteractionResponseMessage,
    CreateMessage, EventHandler, GatewayIntents, GuildId, Http, Interaction, InteractionId,
    Message, Ready, UserId,
};
use serenity::Client;
use tokio::sync::Mutex;
use tracing::{debug, error, info, warn};
use uuid::Uuid;

use crate::account::RawAccountConfig;
use crate::adapters::events::{
    AdapterAccountTarget, AdapterMessenger, MonitorStatus, OutboundMessage, StatusMessage,
};
use crate::adapters::requests::{
    self, AdapterRequest, RequestCommand, RequestContent, RequestState,
};
use crate::config::DiscordBotConfig;
use crate::db::{AccountsDb, UsernameUpdateResult};

const REPLY_INTERACTION_PREFIX: &str = "discord_interaction:";
const COMPONENT_STATUS: &str = "trc:status";
const COMPONENT_FORCE: &str = "trc:force";
const COMPONENT_REAUTH: &str = "trc:reauth";
const COMPONENT_HELP: &str = "trc:help";
const COMPONENT_ACCOUNT_APPROVE_PREFIX: &str = "trc:account-approve:";
const COMPONENT_ACCOUNT_REJECT_PREFIX: &str = "trc:account-reject:";

#[derive(Clone)]
pub struct DiscordBotClient {
    http: Arc<Http>,
    bot_token: String,
    admin_user_id: String,
    admin_channel_id: String,
}

impl DiscordBotClient {
    pub fn new(config: &DiscordBotConfig) -> Result<Self> {
        let http = Arc::new(Http::new(&config.bot_token));
        Ok(Self {
            http,
            bot_token: config.bot_token.clone(),
            admin_user_id: config.admin_user_id.clone(),
            admin_channel_id: config.admin_channel_id.clone(),
        })
    }

    pub fn admin_channel_id(&self) -> Option<ChannelId> {
        parse_discord_id(&self.admin_channel_id).map(ChannelId::new)
    }

    async fn send_text_to_user(&self, user_id: &str, message: &OutboundMessage) -> Result<()> {
        let user_id = parse_discord_id(user_id)
            .ok_or_else(|| miette::miette!("Discord user id 無效：{user_id}"))?;
        let channel = UserId::new(user_id)
            .create_dm_channel(&self.http)
            .await
            .into_diagnostic()
            .wrap_err("建立 Discord DM channel 失敗")?;

        send_outbound_to_channel(&self.http, channel.id, message).await
    }

    async fn send_dashboard_copy(&self, message: &OutboundMessage) -> Result<()> {
        let Some(channel_id) = self.admin_channel_id() else {
            return Ok(());
        };
        send_outbound_to_channel(&self.http, channel_id, message).await
    }
}

#[async_trait]
impl AdapterMessenger for DiscordBotClient {
    fn adapter_name(&self) -> &'static str {
        "discord"
    }

    fn admin_user_id(&self) -> &str {
        &self.admin_user_id
    }

    async fn reply(&self, reply_token: &str, message: &OutboundMessage) -> Result<()> {
        if let Some((id, token)) = parse_interaction_reply(reply_token) {
            let response = CreateInteractionResponse::Message(
                CreateInteractionResponseMessage::new()
                    .content(render_discord_message(message))
                    .components(action_rows_for_message(message)),
            );
            self.http
                .create_interaction_response(InteractionId::new(id), token, &response, Vec::new())
                .await
                .into_diagnostic()
                .wrap_err("回覆 Discord interaction 失敗")?;
            return Ok(());
        }

        let channel_id = parse_discord_id(reply_token)
            .ok_or_else(|| miette::miette!("Discord reply target 無效：{reply_token}"))?;
        send_outbound_to_channel(&self.http, ChannelId::new(channel_id), message).await
    }

    async fn push(&self, to: &str, message: &OutboundMessage) -> Result<()> {
        self.send_text_to_user(to, message).await
    }

    async fn push_to_account_or_admin(
        &self,
        target: &AdapterAccountTarget,
        message: &OutboundMessage,
    ) -> Result<()> {
        if !target.discord_user_id.trim().is_empty() {
            if let Err(e) = self
                .send_text_to_user(&target.discord_user_id, message)
                .await
            {
                warn!(
                    account = %target.account_id,
                    user_id = %target.discord_user_id,
                    error = %e,
                    "Discord 帳號 DM 通知失敗"
                );
            }
        } else if !self.admin_user_id.trim().is_empty() {
            if let Err(e) = self.send_text_to_user(&self.admin_user_id, message).await {
                warn!(error = %e, "Discord admin DM fallback 失敗");
            }
        }

        self.send_dashboard_copy(message).await
    }
}

impl std::fmt::Debug for DiscordBotClient {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("DiscordBotClient")
            .field("bot_token", &"<redacted>")
            .field("admin_user_id", &self.admin_user_id)
            .field("admin_channel_id", &self.admin_channel_id)
            .finish()
    }
}

#[derive(Clone)]
pub struct DiscordRuntime {
    bot: Arc<DiscordBotClient>,
    requests: RequestState,
    accounts_db: AccountsDb,
    register_commands: bool,
    guild_ids: Vec<String>,
    pending_accounts: Arc<Mutex<HashMap<String, PendingAccountRequest>>>,
}

#[derive(Clone)]
struct PendingAccountRequest {
    provider: String,
    username: String,
    password: String,
    discord_user_id: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct CreatedAccount {
    provider: String,
    username: String,
    account_id: String,
    discord_user_id: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum AccountReviewAction {
    Approve,
    Reject,
}

impl DiscordRuntime {
    pub fn new(
        bot: Arc<DiscordBotClient>,
        requests: RequestState,
        accounts_db: AccountsDb,
        config: &DiscordBotConfig,
    ) -> Self {
        Self {
            bot,
            requests,
            accounts_db,
            register_commands: config.register_commands,
            guild_ids: config.guild_ids.clone(),
            pending_accounts: Arc::new(Mutex::new(HashMap::new())),
        }
    }
}

pub async fn start_discord_bot(runtime: DiscordRuntime) -> Result<()> {
    let intents =
        GatewayIntents::GUILDS | GatewayIntents::DIRECT_MESSAGES | GatewayIntents::GUILD_MESSAGES;
    let token = runtime.bot.bot_token.clone();
    let mut client = Client::builder(&token, intents)
        .event_handler(runtime)
        .await
        .into_diagnostic()
        .wrap_err("建立 Discord client 失敗")?;

    client
        .start()
        .await
        .into_diagnostic()
        .wrap_err("Discord client 已停止")
}

#[async_trait]
impl EventHandler for DiscordRuntime {
    async fn ready(&self, ctx: Context, ready: Ready) {
        info!(user = %ready.user.name, "Discord bot 已連線");
        if !self.register_commands {
            return;
        }

        if let Err(e) = register_commands(&ctx, &self.guild_ids).await {
            error!(error = %e, "註冊 Discord slash commands 失敗");
        }
    }

    async fn message(&self, _ctx: Context, msg: Message) {
        if msg.author.bot {
            return;
        }
        if msg.content.trim().is_empty() {
            return;
        }

        let user_id = msg.author.id.to_string();
        let is_dm = msg.guild_id.is_none();
        let is_admin_channel = self
            .bot
            .admin_channel_id()
            .is_some_and(|channel_id| channel_id == msg.channel_id);

        if !is_dm && !is_admin_channel {
            debug!("忽略非 DM / 非管理 channel 的 Discord 文字訊息");
            return;
        }

        if is_admin_channel && user_id != self.bot.admin_user_id {
            let _ = msg
                .channel_id
                .say(
                    &self.bot.http,
                    render_discord_message(&OutboundMessage::NotAuthorized),
                )
                .await;
            return;
        }

        let request = AdapterRequest::new(
            user_id,
            Some(msg.channel_id.to_string()),
            true,
            RequestContent::Text(msg.content.clone()),
        );

        if let Err(e) = requests::handle_request(request, &self.requests).await {
            error!(error = %e, "處理 Discord 文字請求失敗");
        }
    }

    async fn interaction_create(&self, ctx: Context, interaction: Interaction) {
        let result = match interaction {
            Interaction::Command(command) => self.handle_command(&ctx, command).await,
            Interaction::Component(component) => self.handle_component(&ctx, component).await,
            _ => Ok(()),
        };

        if let Err(e) = result {
            error!(error = %e, "處理 Discord interaction 失敗");
        }
    }
}

impl DiscordRuntime {
    async fn handle_command(&self, ctx: &Context, command: CommandInteraction) -> Result<()> {
        let user_id = command.user.id.to_string();
        let reply_token = interaction_reply_token(command.id, &command.token);
        match command.data.name.as_str() {
            "add-account" => {
                self.ensure_admin_interaction(ctx, &command).await?;
                let provider = string_option(&command, "provider").unwrap_or_default();
                let username = string_option(&command, "username").unwrap_or_default();
                let password = string_option(&command, "password").unwrap_or_default();
                let discord_user_id = user_option(&command, "user").unwrap_or_default();
                let account_id = string_option(&command, "id").unwrap_or_default();
                let message = self
                    .add_account(
                        &provider,
                        &username,
                        &password,
                        &discord_user_id,
                        &account_id,
                    )
                    .await;
                reply_interaction(ctx, &command, message, true).await
            }
            "request-account" => {
                if command.guild_id.is_some() {
                    reply_interaction(
                        ctx,
                        &command,
                        OutboundMessage::Text(
                            "請在 bot DM 使用 /request-account，避免帳密出現在公共 channel。"
                                .to_string(),
                        ),
                        true,
                    )
                    .await?;
                    return Ok(());
                }

                let provider = string_option(&command, "provider").unwrap_or_default();
                let username = string_option(&command, "username").unwrap_or_default();
                let password = string_option(&command, "password").unwrap_or_default();
                let message = self
                    .request_account(&provider, &username, &password, &user_id)
                    .await;
                reply_interaction(ctx, &command, message, true).await
            }
            "bind-account" => {
                self.ensure_admin_interaction(ctx, &command).await?;
                let username = string_option(&command, "username").unwrap_or_default();
                let provider = string_option(&command, "provider");
                let discord_user_id = user_option(&command, "user").unwrap_or_default();
                let message = self
                    .bind_account_by_username(&username, provider.as_deref(), &discord_user_id)
                    .await;
                reply_interaction(ctx, &command, message, true).await
            }
            "unbind-account" => {
                self.ensure_admin_interaction(ctx, &command).await?;
                let username = string_option(&command, "username").unwrap_or_default();
                let provider = string_option(&command, "provider");
                let message = self
                    .bind_account_by_username(&username, provider.as_deref(), "")
                    .await;
                reply_interaction(ctx, &command, message, true).await
            }
            "bindings" => {
                self.ensure_admin_interaction(ctx, &command).await?;
                let message = OutboundMessage::Text(self.render_bindings().await);
                reply_interaction(ctx, &command, message, true).await
            }
            "qr" => {
                let qr_data = string_option(&command, "data").unwrap_or_default();
                requests::execute_command(
                    RequestCommand::QrCode(qr_data),
                    &reply_token,
                    &self.requests,
                    &user_id,
                )
                .await
            }
            "status" => {
                requests::execute_command(
                    RequestCommand::GetStatus,
                    &reply_token,
                    &self.requests,
                    &user_id,
                )
                .await
            }
            "start" => {
                self.execute_admin_command(
                    ctx,
                    &command,
                    RequestCommand::Start,
                    &reply_token,
                    &user_id,
                )
                .await
            }
            "stop" => {
                self.execute_admin_command(
                    ctx,
                    &command,
                    RequestCommand::Stop,
                    &reply_token,
                    &user_id,
                )
                .await
            }
            "force" => {
                self.execute_admin_command(
                    ctx,
                    &command,
                    RequestCommand::ForceAttend,
                    &reply_token,
                    &user_id,
                )
                .await
            }
            "reauth" => {
                self.execute_admin_command(
                    ctx,
                    &command,
                    RequestCommand::ReAuth,
                    &reply_token,
                    &user_id,
                )
                .await
            }
            "help" => {
                requests::execute_command(
                    RequestCommand::Help,
                    &reply_token,
                    &self.requests,
                    &user_id,
                )
                .await
            }
            _ => {
                reply_interaction(
                    ctx,
                    &command,
                    OutboundMessage::UnknownCommand {
                        text: command.data.name.clone(),
                    },
                    true,
                )
                .await
            }
        }
    }

    async fn handle_component(&self, ctx: &Context, component: ComponentInteraction) -> Result<()> {
        let user_id = component.user.id.to_string();
        let reply_token = interaction_reply_token(component.id, &component.token);

        if let Some((action, token)) = parse_account_review_custom_id(&component.data.custom_id) {
            if user_id != self.bot.admin_user_id {
                reply_component(ctx, &component, OutboundMessage::NotAuthorized, true).await?;
                return Ok(());
            }

            let message = match action {
                AccountReviewAction::Approve => self.approve_pending_account(token).await,
                AccountReviewAction::Reject => self.reject_pending_account(token).await,
            };
            reply_component(ctx, &component, message, true).await?;
            return Ok(());
        }

        let cmd = match component.data.custom_id.as_str() {
            COMPONENT_STATUS => RequestCommand::GetStatus,
            COMPONENT_FORCE => RequestCommand::ForceAttend,
            COMPONENT_REAUTH => RequestCommand::ReAuth,
            COMPONENT_HELP => RequestCommand::Help,
            other => {
                reply_component(
                    ctx,
                    &component,
                    OutboundMessage::UnknownCommand {
                        text: other.to_string(),
                    },
                    true,
                )
                .await?;
                return Ok(());
            }
        };

        if matches!(cmd, RequestCommand::ForceAttend | RequestCommand::ReAuth)
            && user_id != self.bot.admin_user_id
        {
            reply_component(ctx, &component, OutboundMessage::NotAuthorized, true).await?;
            return Ok(());
        }

        requests::execute_command(cmd, &reply_token, &self.requests, &user_id).await
    }

    async fn execute_admin_command(
        &self,
        ctx: &Context,
        command: &CommandInteraction,
        cmd: RequestCommand,
        reply_token: &str,
        user_id: &str,
    ) -> Result<()> {
        if user_id != self.bot.admin_user_id {
            reply_interaction(ctx, command, OutboundMessage::NotAuthorized, true).await?;
            return Ok(());
        }

        requests::execute_command(cmd, reply_token, &self.requests, user_id).await
    }

    async fn ensure_admin_interaction(
        &self,
        ctx: &Context,
        command: &CommandInteraction,
    ) -> Result<()> {
        if command.user.id.to_string() == self.bot.admin_user_id {
            return Ok(());
        }

        reply_interaction(ctx, command, OutboundMessage::NotAuthorized, true).await?;
        Err(miette::miette!("Discord 使用者未授權執行管理指令"))
    }

    async fn add_account(
        &self,
        provider: &str,
        username: &str,
        password: &str,
        discord_user_id: &str,
        account_id: &str,
    ) -> OutboundMessage {
        match self
            .create_account(provider, username, password, discord_user_id, account_id)
            .await
        {
            Ok(account) => {
                self.notify_created_account(&account).await;
                OutboundMessage::Text(format!(
                    "已新增 Tronclass 帳號 `{}`（provider: `{}`，id: `{}`）。新帳號會在服務重新啟動後開始監控。",
                    account.username, account.provider, account.account_id
                ))
            }
            Err(e) => OutboundMessage::Text(e),
        }
    }

    async fn create_account(
        &self,
        provider: &str,
        username: &str,
        password: &str,
        discord_user_id: &str,
        account_id: &str,
    ) -> std::result::Result<CreatedAccount, String> {
        let provider = provider.trim();
        let username = username.trim();
        let password = password.trim();
        let discord_user_id = discord_user_id.trim();
        let account_id = if account_id.trim().is_empty() {
            username
        } else {
            account_id.trim()
        };

        if provider.is_empty() || username.is_empty() || password.is_empty() {
            return Err("新增失敗：provider、username、password 都必須填寫。".to_string());
        }
        if account_id.is_empty() {
            return Err("新增失敗：無法建立空白帳號 ID。".to_string());
        }

        let account = RawAccountConfig {
            id: account_id.to_string(),
            provider: provider.to_string(),
            username: username.to_string(),
            password: password.to_string(),
            enabled: true,
            line_user_id: String::new(),
            discord_user_id: discord_user_id.to_string(),
        };

        self.accounts_db
            .insert(&account)
            .await
            .map_err(|e| format!("新增帳號失敗：{e}"))?;

        Ok(CreatedAccount {
            provider: provider.to_string(),
            username: username.to_string(),
            account_id: account_id.to_string(),
            discord_user_id: discord_user_id.to_string(),
        })
    }

    async fn notify_created_account(&self, account: &CreatedAccount) {
        if account.discord_user_id.is_empty() {
            return;
        }

        let welcome = OutboundMessage::Text(format!(
            "你的 Discord 已綁定 Tronclass username `{}`。新帳號會在服務重新啟動後開始監控。",
            account.username
        ));
        let _ = self.bot.push(&account.discord_user_id, &welcome).await;
    }

    async fn request_account(
        &self,
        provider: &str,
        username: &str,
        password: &str,
        discord_user_id: &str,
    ) -> OutboundMessage {
        let provider = provider.trim();
        let username = username.trim();
        let password = password.trim();
        let discord_user_id = discord_user_id.trim();

        if provider.is_empty() || username.is_empty() || password.is_empty() {
            return OutboundMessage::Text(
                "申請失敗：provider、username、password 都必須填寫。".to_string(),
            );
        }

        match self
            .accounts_db
            .find_by_username(username, Some(provider))
            .await
        {
            Ok(accounts) if !accounts.is_empty() => {
                return OutboundMessage::Text(format!(
                    "Tronclass username `{username}`（provider: `{provider}`）已存在，請管理員改用綁定。"
                ));
            }
            Err(e) => {
                return OutboundMessage::Text(format!("檢查既有帳號失敗：{e}"));
            }
            _ => {}
        }

        match self.accounts_db.get(username).await {
            Ok(Some(_)) => {
                return OutboundMessage::Text(format!(
                    "帳號 ID `{username}` 已存在，請管理員手動新增或綁定。"
                ));
            }
            Err(e) => {
                return OutboundMessage::Text(format!("檢查既有帳號 ID 失敗：{e}"));
            }
            _ => {}
        }

        let Some(channel_id) = self.bot.admin_channel_id() else {
            return OutboundMessage::Text("申請失敗：Discord 管理 channel 尚未設定。".to_string());
        };

        let pending = PendingAccountRequest {
            provider: provider.to_string(),
            username: username.to_string(),
            password: password.to_string(),
            discord_user_id: discord_user_id.to_string(),
        };
        let token = account_review_token();

        {
            let mut pending_accounts = self.pending_accounts.lock().await;
            if pending_accounts.values().any(|request| {
                request.provider == pending.provider
                    && request.username == pending.username
                    && request.discord_user_id == pending.discord_user_id
            }) {
                return OutboundMessage::Text(
                    "你已送出同一個 Tronclass 帳號申請，請等待管理員核准。".to_string(),
                );
            }
            pending_accounts.insert(token.clone(), pending.clone());
        }

        let send_result = channel_id
            .send_message(
                &self.bot.http,
                CreateMessage::new()
                    .content(render_pending_account_request(&pending))
                    .components(account_review_action_rows(&token)),
            )
            .await;

        if let Err(e) = send_result {
            self.pending_accounts.lock().await.remove(&token);
            return OutboundMessage::Text(format!("申請建立失敗：無法通知管理員：{e}"));
        }

        OutboundMessage::Text(format!(
            "已送出 Tronclass username `{username}` 的新增申請，請等待管理員核准。"
        ))
    }

    async fn approve_pending_account(&self, token: &str) -> OutboundMessage {
        let Some(request) = self.pending_accounts.lock().await.remove(token) else {
            return OutboundMessage::Text("這筆帳號申請不存在或已處理。".to_string());
        };

        match self
            .create_account(
                &request.provider,
                &request.username,
                &request.password,
                &request.discord_user_id,
                "",
            )
            .await
        {
            Ok(account) => {
                self.notify_created_account(&account).await;
                OutboundMessage::Text(format!(
                    "已核准並新增 Tronclass username `{}`（provider: `{}`）。新帳號會在服務重新啟動後開始監控。",
                    account.username, account.provider
                ))
            }
            Err(e) => {
                if !request.discord_user_id.is_empty() {
                    let _ = self
                        .bot
                        .push(
                            &request.discord_user_id,
                            &OutboundMessage::Text(format!(
                                "你的 Tronclass username `{}` 申請在核准時新增失敗：{e}",
                                request.username
                            )),
                        )
                        .await;
                }
                OutboundMessage::Text(format!("核准失敗：{e}"))
            }
        }
    }

    async fn reject_pending_account(&self, token: &str) -> OutboundMessage {
        let Some(request) = self.pending_accounts.lock().await.remove(token) else {
            return OutboundMessage::Text("這筆帳號申請不存在或已處理。".to_string());
        };

        if !request.discord_user_id.is_empty() {
            let _ = self
                .bot
                .push(
                    &request.discord_user_id,
                    &OutboundMessage::Text(format!(
                        "你的 Tronclass username `{}` 新增申請已被管理員拒絕。",
                        request.username
                    )),
                )
                .await;
        }

        OutboundMessage::Text(format!(
            "已拒絕 Tronclass username `{}` 的新增申請。",
            request.username
        ))
    }

    async fn bind_account_by_username(
        &self,
        username: &str,
        provider: Option<&str>,
        discord_user_id: &str,
    ) -> OutboundMessage {
        if username.trim().is_empty() {
            return OutboundMessage::Text("缺少 username 參數".to_string());
        }

        match self
            .accounts_db
            .set_discord_user_id_by_username(username.trim(), provider, discord_user_id.trim())
            .await
        {
            Ok(UsernameUpdateResult::Updated { account_id }) => {
                let updated = self
                    .requests
                    .set_discord_user_id(&account_id, discord_user_id.trim())
                    .await;
                if !updated {
                    warn!(username = %username, account = %account_id, "DB 已更新，但 runtime 找不到帳號");
                }

                if discord_user_id.trim().is_empty() {
                    OutboundMessage::Text(format!(
                        "已解除 Tronclass username `{}` 的 Discord 綁定。",
                        username.trim()
                    ))
                } else {
                    let welcome = OutboundMessage::Text(format!(
                        "你的 Discord 已綁定 Tronclass username `{}`。可用 /status 查詢狀態。",
                        username.trim()
                    ));
                    let _ = self.bot.push(discord_user_id.trim(), &welcome).await;
                    OutboundMessage::Text(format!(
                        "已將 Tronclass username `{}` 綁定到 <@{}>。",
                        username.trim(),
                        discord_user_id.trim()
                    ))
                }
            }
            Ok(UsernameUpdateResult::NotFound) => {
                OutboundMessage::Text(format!("找不到 Tronclass username `{}`。", username.trim()))
            }
            Ok(UsernameUpdateResult::Ambiguous { account_ids }) => OutboundMessage::Text(format!(
                "Tronclass username `{}` 對應多個帳號：{}。請加上 provider 參數消除歧義。",
                username.trim(),
                account_ids.join(", ")
            )),
            Err(e) => OutboundMessage::Text(format!("更新綁定失敗：{e}")),
        }
    }

    async fn render_bindings(&self) -> String {
        let bindings = self.requests.discord_bindings().await;
        if bindings.is_empty() {
            return "目前沒有可列出的帳號。".to_string();
        }

        let mut lines = vec!["Discord 綁定清單".to_string()];
        for (username, discord_user_id) in bindings {
            let target = if discord_user_id.is_empty() {
                "(未綁定)".to_string()
            } else {
                format!("<@{discord_user_id}>")
            };
            lines.push(format!("- `{username}` -> {target}"));
        }
        lines.join("\n")
    }
}

async fn register_commands(ctx: &Context, guild_ids: &[String]) -> Result<()> {
    let commands = discord_commands();
    if guild_ids.is_empty() {
        Command::set_global_commands(&ctx.http, commands)
            .await
            .into_diagnostic()
            .wrap_err("註冊 Discord global commands 失敗")?;
        return Ok(());
    }

    for guild_id in guild_ids {
        let guild_id = parse_discord_id(guild_id)
            .ok_or_else(|| miette::miette!("Discord guild id 無效：{guild_id}"))?;
        GuildId::new(guild_id)
            .set_commands(&ctx.http, commands.clone())
            .await
            .into_diagnostic()
            .wrap_err_with(|| format!("註冊 Discord guild commands 失敗：{guild_id}"))?;
    }

    Ok(())
}

fn discord_commands() -> Vec<CreateCommand> {
    vec![
        CreateCommand::new("status").description("查看目前監控狀態"),
        CreateCommand::new("start").description("啟動簽到監控"),
        CreateCommand::new("stop").description("暫停簽到監控"),
        CreateCommand::new("force").description("立即觸發一次簽到檢查"),
        CreateCommand::new("reauth").description("重新登入"),
        CreateCommand::new("help").description("顯示說明"),
        CreateCommand::new("qr")
            .description("回傳 QR Code 掃描資料")
            .add_option(
                CreateCommandOption::new(CommandOptionType::String, "data", "QR Code URL 或資料")
                    .required(true),
            ),
        CreateCommand::new("request-account")
            .description("使用者：申請新增 Tronclass 帳號，需管理員核准")
            .add_option(
                CreateCommandOption::new(CommandOptionType::String, "provider", "Provider 名稱")
                    .required(true),
            )
            .add_option(
                CreateCommandOption::new(
                    CommandOptionType::String,
                    "username",
                    "Tronclass username",
                )
                .required(true),
            )
            .add_option(
                CreateCommandOption::new(CommandOptionType::String, "password", "Tronclass 密碼")
                    .required(true),
            ),
        CreateCommand::new("add-account")
            .description("管理員：新增 Tronclass 帳號並可立即綁定 Discord 使用者")
            .add_option(
                CreateCommandOption::new(CommandOptionType::String, "provider", "Provider 名稱")
                    .required(true),
            )
            .add_option(
                CreateCommandOption::new(
                    CommandOptionType::String,
                    "username",
                    "Tronclass username",
                )
                .required(true),
            )
            .add_option(
                CreateCommandOption::new(CommandOptionType::String, "password", "Tronclass 密碼")
                    .required(true),
            )
            .add_option(
                CreateCommandOption::new(
                    CommandOptionType::User,
                    "user",
                    "要綁定的 Discord 使用者",
                )
                .required(false),
            )
            .add_option(
                CreateCommandOption::new(
                    CommandOptionType::String,
                    "id",
                    "DB account id；留空時使用 Tronclass username",
                )
                .required(false),
            ),
        CreateCommand::new("bind-account")
            .description("管理員：用 Tronclass username 綁定 Discord 使用者")
            .add_option(
                CreateCommandOption::new(
                    CommandOptionType::String,
                    "username",
                    "Tronclass username",
                )
                .required(true),
            )
            .add_option(
                CreateCommandOption::new(CommandOptionType::User, "user", "Discord 使用者")
                    .required(true),
            )
            .add_option(
                CreateCommandOption::new(
                    CommandOptionType::String,
                    "provider",
                    "Provider 名稱；同 username 多帳號時用來消除歧義",
                )
                .required(false),
            ),
        CreateCommand::new("unbind-account")
            .description("管理員：用 Tronclass username 解除 Discord 綁定")
            .add_option(
                CreateCommandOption::new(
                    CommandOptionType::String,
                    "username",
                    "Tronclass username",
                )
                .required(true),
            )
            .add_option(
                CreateCommandOption::new(
                    CommandOptionType::String,
                    "provider",
                    "Provider 名稱；同 username 多帳號時用來消除歧義",
                )
                .required(false),
            ),
        CreateCommand::new("bindings").description("管理員：列出 Discord 綁定"),
    ]
}

#[cfg(test)]
fn discord_command_names() -> Vec<&'static str> {
    vec![
        "status",
        "start",
        "stop",
        "force",
        "reauth",
        "help",
        "qr",
        "request-account",
        "add-account",
        "bind-account",
        "unbind-account",
        "bindings",
    ]
}

async fn send_outbound_to_channel(
    http: &Arc<Http>,
    channel_id: ChannelId,
    message: &OutboundMessage,
) -> Result<()> {
    channel_id
        .send_message(
            http,
            CreateMessage::new()
                .content(render_discord_message(message))
                .components(action_rows_for_message(message)),
        )
        .await
        .into_diagnostic()
        .wrap_err("發送 Discord 訊息失敗")?;
    Ok(())
}

async fn reply_interaction(
    ctx: &Context,
    command: &CommandInteraction,
    message: OutboundMessage,
    ephemeral: bool,
) -> Result<()> {
    command
        .create_response(
            &ctx.http,
            CreateInteractionResponse::Message(
                CreateInteractionResponseMessage::new()
                    .content(render_discord_message(&message))
                    .ephemeral(ephemeral)
                    .components(action_rows_for_message(&message)),
            ),
        )
        .await
        .into_diagnostic()
        .wrap_err("回覆 Discord slash command 失敗")
}

async fn reply_component(
    ctx: &Context,
    component: &ComponentInteraction,
    message: OutboundMessage,
    ephemeral: bool,
) -> Result<()> {
    component
        .create_response(
            &ctx.http,
            CreateInteractionResponse::Message(
                CreateInteractionResponseMessage::new()
                    .content(render_discord_message(&message))
                    .ephemeral(ephemeral)
                    .components(action_rows_for_message(&message)),
            ),
        )
        .await
        .into_diagnostic()
        .wrap_err("回覆 Discord component 失敗")
}

fn action_rows_for_message(message: &OutboundMessage) -> Vec<CreateActionRow> {
    match message {
        OutboundMessage::SystemStarted(_)
        | OutboundMessage::RollcallDetected(_)
        | OutboundMessage::RollcallFinished(_)
        | OutboundMessage::Status(_)
        | OutboundMessage::Help
        | OutboundMessage::Welcome => vec![CreateActionRow::Buttons(vec![
            CreateButton::new(COMPONENT_STATUS)
                .label("Status")
                .style(ButtonStyle::Primary),
            CreateButton::new(COMPONENT_FORCE)
                .label("Force")
                .style(ButtonStyle::Secondary),
            CreateButton::new(COMPONENT_REAUTH)
                .label("Reauth")
                .style(ButtonStyle::Secondary),
            CreateButton::new(COMPONENT_HELP)
                .label("Help")
                .style(ButtonStyle::Secondary),
        ])],
        OutboundMessage::QrCodeRequested(request) => vec![
            CreateActionRow::Buttons(vec![
                CreateButton::new_link(&request.scan_url).label("Open scanner")
            ]),
            CreateActionRow::Buttons(vec![CreateButton::new(COMPONENT_STATUS)
                .label("Status")
                .style(ButtonStyle::Primary)]),
        ],
        _ => vec![],
    }
}

fn account_review_token() -> String {
    Uuid::new_v4().simple().to_string()
}

fn account_review_custom_id(action: AccountReviewAction, token: &str) -> String {
    let prefix = match action {
        AccountReviewAction::Approve => COMPONENT_ACCOUNT_APPROVE_PREFIX,
        AccountReviewAction::Reject => COMPONENT_ACCOUNT_REJECT_PREFIX,
    };
    format!("{prefix}{token}")
}

fn parse_account_review_custom_id(value: &str) -> Option<(AccountReviewAction, &str)> {
    if let Some(token) = value.strip_prefix(COMPONENT_ACCOUNT_APPROVE_PREFIX) {
        return Some((AccountReviewAction::Approve, token));
    }
    if let Some(token) = value.strip_prefix(COMPONENT_ACCOUNT_REJECT_PREFIX) {
        return Some((AccountReviewAction::Reject, token));
    }
    None
}

fn account_review_action_rows(token: &str) -> Vec<CreateActionRow> {
    vec![CreateActionRow::Buttons(vec![
        CreateButton::new(account_review_custom_id(
            AccountReviewAction::Approve,
            token,
        ))
        .label("核准")
        .style(ButtonStyle::Success),
        CreateButton::new(account_review_custom_id(AccountReviewAction::Reject, token))
            .label("拒絕")
            .style(ButtonStyle::Danger),
    ])]
}

fn render_pending_account_request(request: &PendingAccountRequest) -> String {
    format!(
        "待審核 Tronclass 帳號新增\n申請人：<@{}>\nProvider：`{}`\nUsername：`{}`\n\n密碼不會顯示在控制台；核准後會寫入 accounts.db 並綁定申請人。新帳號會在服務重新啟動後開始監控。",
        request.discord_user_id, request.provider, request.username
    )
}

fn render_discord_message(message: &OutboundMessage) -> String {
    match message {
        OutboundMessage::Text(text) => text.clone(),
        OutboundMessage::SystemStarted(event) => format!(
            "Tronclass Rollcall 已啟動\n帳號：{} / {}\n輪詢間隔：{} 秒\nAdapter：{}",
            event.account, event.user_name, event.poll_interval_secs, event.adapter_name
        ),
        OutboundMessage::RollcallDetected(event) => format!(
            "偵測到新簽到\n帳號：{}\n課程：{}\n教師：{}\n類型：{}\n點名 ID：{}",
            event.account,
            event.course_name,
            event.teacher_name,
            event.attendance_type,
            event.rollcall_id
        ),
        OutboundMessage::QrCodeRequested(request) => format!(
            "需要 QR Code 簽到\n課程：{}\n帳號：{}\n教師：{}\n點名 ID：{}\n請在 {} 秒內開啟 scanner 或貼回 QR Code 資料：\n{}",
            request.course_name,
            request.account,
            request.teacher_name,
            request.rollcall_id,
            request.timeout_secs,
            request.scan_url
        ),
        OutboundMessage::RollcallFinished(event) => format!(
            "簽到結果：{}\n帳號：{}\n課程：{}\n類型：{}\n點名 ID：{}\n結果：{}\n耗時：{} ms",
            if event.success { "成功" } else { "未成功" },
            event.account,
            event.course_name,
            event.attendance_type,
            event.rollcall_id,
            event.result,
            event.elapsed_ms
        ),
        OutboundMessage::Help => help_text().to_string(),
        OutboundMessage::Welcome => format!("歡迎使用 Tronclass Rollcall\n\n{}", help_text()),
        OutboundMessage::UnsupportedMedia => "不支援媒體訊息，請傳送文字指令。".to_string(),
        OutboundMessage::LocationReceived {
            latitude,
            longitude,
        } => format!("收到位置訊息：{latitude:.6}, {longitude:.6}（位置功能尚未實作）"),
        OutboundMessage::Status(status) => render_status_message(status),
        OutboundMessage::NotAuthorized => {
            "你可以查詢自己的帳號狀態，但此操作僅限管理員使用。".to_string()
        }
        OutboundMessage::MonitorPaused => "簽到監控已暫停。".to_string(),
        OutboundMessage::MonitorResumed => "簽到監控已恢復，立即執行一次簽到檢查。".to_string(),
        OutboundMessage::ForcePollTriggered => "已觸發立即簽到檢查。".to_string(),
        OutboundMessage::ReauthTriggered => "已觸發重新認證。".to_string(),
        OutboundMessage::QrAccepted => "已收到 QR Code，正在嘗試簽到。".to_string(),
        OutboundMessage::QrAmbiguousTarget => {
            "多帳號模式下 QR Code 目標不明，請由綁定該帳號的使用者傳送 QR Code。"
                .to_string()
        }
        OutboundMessage::QrNoBoundAccount => {
            "找不到與你的 Discord 綁定的 Tronclass 帳號。".to_string()
        }
        OutboundMessage::QrNoPendingRequest => {
            "目前沒有等待 QR Code 的簽到，或簽到已逾時。".to_string()
        }
        OutboundMessage::UnknownCommand { text } => {
            format!("不認識的指令：{}\n\n{}", text.chars().take(80).collect::<String>(), help_text())
        }
    }
}

fn render_status_message(message: &StatusMessage) -> String {
    match message {
        StatusMessage::NoAccounts => "目前沒有可查詢的 Tronclass 帳號。".to_string(),
        StatusMessage::Single(status) => render_monitor_status(status),
        StatusMessage::UserAccount { account_id, status } => {
            format!(
                "你的 Tronclass 帳號狀態\n帳號 ID：{account_id}\n{}",
                render_monitor_status(status)
            )
        }
        StatusMessage::AdminAccounts(accounts) => {
            let mut lines = vec![format!("系統狀態（{} 個帳號）", accounts.len())];
            for account in accounts {
                let status = &account.status;
                let running = if status.is_running {
                    "運行中"
                } else {
                    "已暫停"
                };
                let last_success = status.last_success_course.as_deref().unwrap_or("無");
                lines.push(format!(
                    "\n帳號：{}\n使用者：{}\n狀態：{}\n最後成功：{}\n連續失敗：{} 次",
                    account.account_id,
                    status.user_name,
                    running,
                    last_success,
                    status.consecutive_failures
                ));
            }
            lines.join("\n")
        }
    }
}

fn render_monitor_status(status: &MonitorStatus) -> String {
    let running = if status.is_running {
        "運行中"
    } else {
        "已暫停"
    };
    let last_success = status.last_success_course.as_deref().unwrap_or("無");
    format!(
        "狀態：{running}\n帳號：{}\n最後成功：{last_success}\n連續失敗：{} 次",
        status.user_name, status.consecutive_failures
    )
}

fn help_text() -> &'static str {
    "可用指令：\n/status - 查看狀態\n/help - 顯示說明\n/qr data:<資料> - 回傳 QR Code\n/request-account provider:<name> username:<Tronclass username> password:<password> - 在 DM 申請新增帳號\n\n管理員可在控制台使用 /start、/stop、/force、/reauth、/add-account、/bind-account、/unbind-account、/bindings。"
}

fn string_option(command: &CommandInteraction, name: &str) -> Option<String> {
    command
        .data
        .options
        .iter()
        .find(|option| option.name == name)
        .and_then(|option| option.value.as_str())
        .map(ToOwned::to_owned)
}

fn user_option(command: &CommandInteraction, name: &str) -> Option<String> {
    command
        .data
        .options
        .iter()
        .find(|option| option.name == name)
        .and_then(|option| match option.value {
            CommandDataOptionValue::User(id) => Some(id.to_string()),
            _ => None,
        })
}

fn interaction_reply_token(id: InteractionId, token: &str) -> String {
    format!("{REPLY_INTERACTION_PREFIX}{}:{token}", id.get())
}

fn parse_interaction_reply(value: &str) -> Option<(u64, &str)> {
    let value = value.strip_prefix(REPLY_INTERACTION_PREFIX)?;
    let (id, token) = value.split_once(':')?;
    Some((id.parse().ok()?, token))
}

fn parse_discord_id(value: &str) -> Option<u64> {
    value.trim().parse::<u64>().ok()
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::NamedTempFile;

    fn test_bot() -> Arc<DiscordBotClient> {
        Arc::new(DiscordBotClient {
            http: Arc::new(Http::new("secret-token")),
            bot_token: "secret-token".to_string(),
            admin_user_id: "1".to_string(),
            admin_channel_id: "2".to_string(),
        })
    }

    async fn test_runtime(db: AccountsDb) -> DiscordRuntime {
        let bot = test_bot();
        let messenger = Arc::clone(&bot) as Arc<dyn AdapterMessenger>;
        DiscordRuntime {
            bot,
            requests: RequestState::new_with_binding(
                messenger,
                vec![],
                requests::AdapterBindingKind::Discord,
            ),
            accounts_db: db,
            register_commands: false,
            guild_ids: vec![],
            pending_accounts: Arc::new(Mutex::new(HashMap::new())),
        }
    }

    #[test]
    fn render_qr_request_contains_scanner_url() {
        let msg = OutboundMessage::QrCodeRequested(crate::adapters::events::QrCodeRequest {
            rollcall_id: 42,
            account: "acc1".to_string(),
            course_name: "資料結構".to_string(),
            teacher_name: "王老師".to_string(),
            scan_url: "https://example.test/scanner".to_string(),
            timeout_secs: 60,
        });

        let text = render_discord_message(&msg);
        assert!(text.contains("資料結構"));
        assert!(text.contains("https://example.test/scanner"));
        assert!(!action_rows_for_message(&msg).is_empty());
    }

    #[test]
    fn interaction_reply_token_roundtrips() {
        let token = interaction_reply_token(InteractionId::new(123), "abc");
        assert_eq!(parse_interaction_reply(&token), Some((123, "abc")));
    }

    #[test]
    fn discord_bot_debug_redacts_token() {
        let bot = test_bot();
        let text = format!("{bot:?}");
        assert!(text.contains("<redacted>"));
        assert!(!text.contains("secret-token"));
    }

    #[test]
    fn commands_include_binding_console_commands() {
        let names = discord_command_names();

        assert!(names.contains(&"request-account"));
        assert!(names.contains(&"bind-account"));
        assert!(names.contains(&"unbind-account"));
        assert!(names.contains(&"bindings"));
        assert!(names.contains(&"status"));
        assert_eq!(discord_commands().len(), names.len());
    }

    #[test]
    fn account_review_custom_id_roundtrips() {
        let approve = account_review_custom_id(AccountReviewAction::Approve, "token");
        let reject = account_review_custom_id(AccountReviewAction::Reject, "token");

        assert_eq!(
            parse_account_review_custom_id(&approve),
            Some((AccountReviewAction::Approve, "token"))
        );
        assert_eq!(
            parse_account_review_custom_id(&reject),
            Some((AccountReviewAction::Reject, "token"))
        );
        assert_eq!(parse_account_review_custom_id("trc:status"), None);
    }

    #[test]
    fn pending_account_admin_message_redacts_password() {
        let request = PendingAccountRequest {
            provider: "fju".to_string(),
            username: "student001".to_string(),
            password: "super-secret".to_string(),
            discord_user_id: "123".to_string(),
        };

        let text = render_pending_account_request(&request);
        assert!(text.contains("student001"));
        assert!(text.contains("<@123>"));
        assert!(!text.contains("super-secret"));
    }

    #[tokio::test]
    async fn add_account_inserts_tronclass_username_account() {
        let file = NamedTempFile::new().unwrap();
        let db = AccountsDb::open(file.path()).await.unwrap();
        let runtime = test_runtime(db.clone()).await;

        let message = runtime
            .add_account("fju", "student001", "secret", "", "")
            .await;
        assert!(render_discord_message(&message).contains("student001"));

        let account = db.get("student001").await.unwrap().unwrap();
        assert_eq!(account.provider, "fju");
        assert_eq!(account.username, "student001");
        assert_eq!(account.password, "secret");
    }

    #[tokio::test]
    async fn approve_pending_account_inserts_requested_account() {
        let file = NamedTempFile::new().unwrap();
        let db = AccountsDb::open(file.path()).await.unwrap();
        let runtime = test_runtime(db.clone()).await;

        runtime.pending_accounts.lock().await.insert(
            "token".to_string(),
            PendingAccountRequest {
                provider: "fju".to_string(),
                username: "student002".to_string(),
                password: "secret".to_string(),
                discord_user_id: String::new(),
            },
        );

        let message = runtime.approve_pending_account("token").await;
        assert!(render_discord_message(&message).contains("已核准"));

        let account = db.get("student002").await.unwrap().unwrap();
        assert_eq!(account.provider, "fju");
        assert_eq!(account.username, "student002");
        assert_eq!(account.password, "secret");
        assert!(runtime.pending_accounts.lock().await.is_empty());
    }

    #[tokio::test]
    async fn reject_pending_account_removes_request_without_insert() {
        let file = NamedTempFile::new().unwrap();
        let db = AccountsDb::open(file.path()).await.unwrap();
        let runtime = test_runtime(db.clone()).await;

        runtime.pending_accounts.lock().await.insert(
            "token".to_string(),
            PendingAccountRequest {
                provider: "fju".to_string(),
                username: "student003".to_string(),
                password: "secret".to_string(),
                discord_user_id: String::new(),
            },
        );

        let message = runtime.reject_pending_account("token").await;
        assert!(render_discord_message(&message).contains("已拒絕"));
        assert!(db.get("student003").await.unwrap().is_none());
        assert!(runtime.pending_accounts.lock().await.is_empty());
    }

    #[tokio::test]
    async fn bind_account_uses_tronclass_username() {
        let file = NamedTempFile::new().unwrap();
        let db = AccountsDb::open(file.path()).await.unwrap();
        db.insert(&RawAccountConfig {
            id: "db-id".to_string(),
            provider: "fju".to_string(),
            username: "student001".to_string(),
            password: "secret".to_string(),
            enabled: true,
            line_user_id: String::new(),
            discord_user_id: "old".to_string(),
        })
        .await
        .unwrap();
        let runtime = test_runtime(db.clone()).await;

        let message = runtime
            .bind_account_by_username("student001", None, "")
            .await;
        assert!(render_discord_message(&message).contains("student001"));

        let account = db.get("db-id").await.unwrap().unwrap();
        assert!(account.discord_user_id.is_empty());
    }
}
