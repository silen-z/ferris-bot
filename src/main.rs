mod discord_commands;
mod queue_manager;

use async_trait::async_trait;
use chrono::{DateTime, Duration, Utc};
use log::{debug, trace, LevelFilter};
use queue_manager::QueueManager;
use serde::{Deserialize, Serialize};
use serenity::http::Http;
use serenity::model::id::ChannelId;
use simple_logger::SimpleLogger;
use std::fs::File;
use std::io::Write;
use std::process::{Command, Stdio};
use std::sync::{Arc, Mutex};
use std::{fs, io, str};
use structopt::StructOpt;
use twitch_irc::login::{RefreshingLoginCredentials, TokenStorage, UserAccessToken};
use twitch_irc::message::{PrivmsgMessage, ServerMessage, TwitchUserBasics};
use twitch_irc::{ClientConfig, TCPTransport, TwitchIRCClient};

#[derive(Debug)]
struct CustomTokenStorage {
    token_checkpoint_file: String,
}

#[async_trait]
impl TokenStorage for CustomTokenStorage {
    type LoadError = std::io::Error; // or some other error
    type UpdateError = std::io::Error;

    async fn load_token(&mut self) -> Result<UserAccessToken, Self::LoadError> {
        debug!("load_token called");
        let token = fs::read_to_string(&self.token_checkpoint_file).unwrap();
        let token: UserAccessToken = serde_json::from_str(&token).unwrap();
        Ok(token)
    }

    async fn update_token(&mut self, token: &UserAccessToken) -> Result<(), Self::UpdateError> {
        debug!("update_token called");
        let serialized = serde_json::to_string(&token).unwrap();
        let _ = File::create(&self.token_checkpoint_file);
        fs::write(&self.token_checkpoint_file, serialized)
            .expect("Twitch IRC: Unable to write token to checkpoint file");
        Ok(())
    }
}

#[derive(Deserialize)]
struct FerrisBotConfig {
    twitch: TwitchConfig,
    discord: DiscordConfig,
}

#[derive(Deserialize)]
struct TwitchConfig {
    token_filepath: String,
    login_name: String,
    channel_name: String,
    client_id: String,
    secret: String,
}

#[derive(Deserialize)]
struct DiscordConfig {
    auth_token: String,
    channel_id: u64,
}

#[derive(Deserialize)]
struct FirstToken {
    access_token: String,
    expires_in: i64,
    refresh_token: String,
}

// Command-line arguments for the tool.
#[derive(StructOpt)]
struct Cli {
    /// Log level
    #[structopt(short, long, case_insensitive = true, default_value = "INFO")]
    log_level: LevelFilter,

    /// Twitch credential files.
    #[structopt(short, long, default_value = "ferrisbot.toml")]
    config_file: String,

    /// Generates the curl command to obtain the first token and exits.
    #[structopt(short, long)]
    generate_curl_first_token_request: bool,

    /// Auth code to be used when obtaining first token.
    #[structopt(long, default_value = "")]
    auth_code: String,

    /// Show the authentication URL and exits.
    #[structopt(short, long)]
    show_auth_url: bool,

    /// If present, parse the access token from the file passed as argument.
    #[structopt(long, default_value = "")]
    first_token_file: String,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct MyUserAccessToken {
    access_token: String,
    refresh_token: String,
    created_at: DateTime<Utc>,
    expires_at: Option<DateTime<Utc>>,
}

#[tokio::main]
pub async fn main() {
    let args = Cli::from_args();
    SimpleLogger::new()
        .with_level(args.log_level)
        .init()
        .unwrap();

    let config = fs::read_to_string(args.config_file).unwrap();
    let config: FerrisBotConfig = toml::from_str(&config).unwrap();

    if args.show_auth_url {
        println!("https://id.twitch.tv/oauth2/authorize?client_id={}&redirect_uri=http://localhost&response_type=code&scope=chat:read%20chat:edit", config.twitch.client_id);
        std::process::exit(0);
    }

    if args.generate_curl_first_token_request {
        if args.auth_code.is_empty() {
            println!("Please set --auth_code. Aborting.");
            std::process::exit(1);
        }
        println!("curl -X POST 'https://id.twitch.tv/oauth2/token?client_id={}&client_secret={}&code={}&grant_type=authorization_code&redirect_uri=http://localhost' > /tmp/firsttoken.json",
            config.twitch.client_id,
            config.twitch.secret,
            args.auth_code);
        std::process::exit(0);
    }

    let mut storage = CustomTokenStorage {
        token_checkpoint_file: config.twitch.token_filepath.clone(),
    };

    if !args.first_token_file.is_empty() {
        let first_token = fs::read_to_string(args.first_token_file).unwrap();
        let first_token: FirstToken = serde_json::from_str(&first_token).unwrap();
        let created_at = Utc::now();
        let expires_at = created_at + Duration::seconds(first_token.expires_in);
        let user_access_token = MyUserAccessToken {
            access_token: first_token.access_token,
            refresh_token: first_token.refresh_token,
            created_at,
            expires_at: Some(expires_at),
        };
        let serialized = serde_json::to_string(&user_access_token).unwrap();
        let user_access_token: UserAccessToken = serde_json::from_str(&serialized).unwrap();
        storage.update_token(&user_access_token).await.unwrap();
    }

    // Discord credentials.
    let discord_http = Http::new_with_token(&config.discord.auth_token);
    discord_commands::init_discord_bot(&discord_http, &config.discord.auth_token).await;

    let irc_config = ClientConfig::new_simple(RefreshingLoginCredentials::new(
        config.twitch.login_name.clone(),
        config.twitch.client_id.clone(),
        config.twitch.secret.clone(),
        storage,
    ));

    let (mut incoming_messages, twitch_client) =
        TwitchIRCClient::<TCPTransport, _>::new(irc_config);

    let mut context = Context {
        queue_manager: Arc::new(Mutex::new(QueueManager::new())),
        twitch_client,
        discord_http,
    };

    // join a channel
    context
        .twitch_client
        .join(config.twitch.channel_name.to_owned());

    context
        .twitch_client
        .say(
            config.twitch.channel_name.to_owned(),
            "Hello! I am the Stuck-Bot, How may I unstick you?".to_owned(),
        )
        .await
        .unwrap();

    let join_handle = tokio::spawn(async move {
        while let Some(message) = incoming_messages.recv().await {
            trace!("{:?}", message);
            match message {
                ServerMessage::Privmsg(msg) => {
                    if let Some(cmd) = TwitchCommand::parse_msg(&msg) {
                        cmd.handle(msg, &config, &mut context).await;
                    }
                }
                _ => continue,
            }
        }
    });

    // keep the tokio executor alive.
    // If you return instead of waiting the background task will exit.
    join_handle.await.unwrap();
}

struct Context {
    twitch_client: TwitchIRCClient<TCPTransport, RefreshingLoginCredentials<CustomTokenStorage>>,
    queue_manager: Arc<Mutex<QueueManager>>,
    discord_http: Http,
}

#[derive(Debug, PartialEq)]
enum TwitchCommand {
    Join,
    Queue,
    ReplyWith(&'static str),
    Broadcast(&'static str),
    Nothing,
    DiscordSnippet(String),
}

impl TwitchCommand {
    async fn handle(self, msg: PrivmsgMessage, config: &FerrisBotConfig, ctx: &mut Context) {
        match self {
            TwitchCommand::Join => {
                ctx.twitch_client
                    .say(
                        msg.channel_login,
                        format!("@{}: Join requested", &msg.sender.login),
                    )
                    .await
                    .unwrap();

                ctx.queue_manager
                    .lock()
                    .unwrap()
                    .join(msg.sender.login, queue_manager::UserType::Default)
                    .unwrap();
            }

            TwitchCommand::Queue => {
                let reply = {
                    let queue_manager = ctx.queue_manager.lock().unwrap();
                    queue_manager.queue().join(", ")
                };
                ctx.twitch_client
                    .say(
                        msg.channel_login,
                        format!("@{}: Current queue: {}", msg.sender.login, reply),
                    )
                    .await
                    .unwrap();
            }

            TwitchCommand::ReplyWith(reply) => {
                ctx.twitch_client
                    .say(msg.channel_login, format!("@{}: {}", msg.sender.login, reply))
                    .await
                    .unwrap();
            }

            TwitchCommand::Broadcast(message) => {
                ctx.twitch_client
                    .say(msg.channel_login, message.to_owned())
                    .await
                    .unwrap();
            }

            TwitchCommand::Nothing => {
                debug!("nothing received");
                let _ = ChannelId(config.discord.channel_id)
                    .say(&ctx.discord_http, "This does nothing")
                    .await;
            }

            TwitchCommand::DiscordSnippet(snippet) => {
                let formatted = format_snippet(&snippet).unwrap_or(snippet);
                let code_block = format!("```rs\n{}\n```", formatted);

                let _ = ChannelId(config.discord.channel_id)
                    .say(&ctx.discord_http, code_block)
                    .await;
            }
        }
    }

    fn parse_msg(msg: &PrivmsgMessage) -> Option<TwitchCommand> {
        if !msg.message_text.starts_with('!') {
            return None;
        }

        let args: Vec<&str> = msg.message_text.split_whitespace().collect();

        match args.as_slice() {
            ["!join", ..] => Some(TwitchCommand::Join),
            ["!queue", ..] => Some(TwitchCommand::Queue),
            ["!pythonsucks", ..] => Some(TwitchCommand::ReplyWith("This must be Lord")),
            ["!stonk", ..] => Some(TwitchCommand::ReplyWith("yOu shOULd Buy AMC sTOnKS")),
            ["!c++", ..] => Some(TwitchCommand::ReplyWith("segmentation fault")),
            ["!dave", ..] => Some(TwitchCommand::Broadcast(include_str!("../assets/dave.txt"))),
            ["!bazylia", ..] => Some(TwitchCommand::Broadcast(include_str!(
                "../assets/bazylia.txt"
            ))),
            ["!zoya", ..] => Some(TwitchCommand::Broadcast(include_str!("../assets/zoya.txt"))),
            ["!discord", ..] => Some(TwitchCommand::Broadcast("https://discord.gg/UyrsFX7N")),
            ["!nothing", ..] => Some(TwitchCommand::Nothing),
            ["!code", ..] => Some(TwitchCommand::DiscordSnippet(
                msg.message_text.trim_start_matches("!code ").into(),
            )),
            _ => None,
        }
    }
}

fn format_snippet(snippet: &str) -> Result<String, io::Error> {
    let mut rustfmt = Command::new("rustfmt")
        .args(&["--config", "newline_style=Unix"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()?;

    let input = rustfmt.stdin.as_mut().expect("msg");
    input.write_all(snippet.as_bytes())?;
    let output = rustfmt.wait_with_output()?;
    if output.status.success() {
        String::from_utf8(output.stdout).map_err(|e| io::Error::new(io::ErrorKind::Other, e))
    } else {
        Err(io::Error::new(
            io::ErrorKind::Other,
            String::from_utf8_lossy(&output.stdout),
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parsing_commands() {
        assert!(TwitchCommand::parse_msg(&test_msg("regular message text")).is_none());
        assert_eq!(
            TwitchCommand::parse_msg(&test_msg("!join")),
            Some(TwitchCommand::Join)
        );
        assert_eq!(
            TwitchCommand::parse_msg(&test_msg("!code snippet")),
            Some(TwitchCommand::DiscordSnippet("snippet".into()))
        );
    }

    #[test]
    fn formatting_snippets() {
        assert!(matches!(
            format_snippet(r#"fn main() { println!("hello world"); }"#).as_deref(),
            Ok("fn main() {\n    println!(\"hello world\");\n}\n")
        ));
    }

    fn test_msg(message_text: &str) -> PrivmsgMessage {
        use twitch_irc::message::IRCMessage;
        use twitch_irc::message::IRCTags;

        PrivmsgMessage {
            channel_login: "channel_login".to_owned(),
            channel_id: "channel_id".to_owned(),
            message_text: message_text.to_owned(),
            is_action: false,
            sender: TwitchUserBasics {
                id: "12345678".to_owned(),
                login: "login".to_owned(),
                name: "name".to_owned(),
            },
            badge_info: vec![],
            badges: vec![],
            bits: None,
            name_color: None,
            emotes: vec![],
            server_timestamp: Utc::now(),
            message_id: "1094e782-a8fc-4d95-a589-ad53e7c13d25".to_owned(),
            source: IRCMessage {
                tags: IRCTags::default(),
                prefix: None,
                command: String::new(),
                params: vec![],
            },
        }
    }
}
