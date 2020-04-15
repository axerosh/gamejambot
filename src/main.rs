use std::env;
use std::fs::File;
use std::io::prelude::*;
use std::sync::Mutex;
use std::collections::HashMap;
use std::path::PathBuf;

use tokio::stream::StreamExt;
use serde_derive::{Serialize, Deserialize};
use anyhow::Context;
use lazy_static::lazy_static;
use serde_json;
use regex::Regex;

use twilight::{
    cache::{
        twilight_cache_inmemory::config::{InMemoryConfigBuilder, EventType},
        InMemoryCache,
    },
    gateway::cluster::{config::ShardScheme, Cluster, ClusterConfig},
    gateway::shard::Event,
    http::Client as HttpClient,
    model::{
        gateway::GatewayIntents,
        user::CurrentUser,
        channel::{Message, Channel, ChannelType, GuildChannel},
        id::{ChannelId, UserId, GuildId},
    },
};

type Result<T> = std::result::Result<T, anyhow::Error>;

enum SubmissionResult {
    Done,
    AlreadySubmitted,
}

const FILENAME: &'static str = "themes.json";

#[derive(Serialize, Deserialize)]
struct ThemeIdeas {
    content: HashMap<UserId, String>,
}

impl ThemeIdeas {
    fn load() -> Result<Self> {
        if PathBuf::from(FILENAME).exists() {
            let mut file = File::open(FILENAME)?;
            let mut content = String::new();
            file.read_to_string(&mut content)?;
            Ok(serde_json::from_str(&content)?)
        }
        else {
            Ok(Self {content: HashMap::new()})
        }
    }

    pub fn instance() -> &'static Mutex<Self> {
        lazy_static! {
            static ref INSTANCE: Mutex<ThemeIdeas> = Mutex::new(
                ThemeIdeas::load().unwrap()
            );
        }
        &INSTANCE
    }

    pub fn try_add(&mut self, user: UserId, idea: &str) -> Result<SubmissionResult> {
        if self.content.contains_key(&user) {
            self.content.insert(user, idea.into());
            self.save().context("Failed to write current themes")?;
            Ok(SubmissionResult::AlreadySubmitted)
        }
        else {
            self.content.insert(user, idea.into());
            self.save().context("Failed to write current themes")?;
            Ok(SubmissionResult::Done)
        }
    }

    pub fn save(&self) -> Result<()> {
        let mut file = File::create(FILENAME)
            .with_context(|| format!("failed to open {} for writing", FILENAME))?;
        file.write_all(serde_json::to_string(&self)?.as_bytes())
            .with_context(|| format!("failed to write to {}", FILENAME))?;
        Ok(())
    }
}

#[tokio::main]
async fn main() -> Result<()> {
    dotenv::dotenv().ok();
    let token = env::var("DISCORD_TOKEN")?;

    // This is also the default.
    let scheme = ShardScheme::Auto;

    let config = ClusterConfig::builder(&token)
        .shard_scheme(scheme)
        // Use intents to only listen to GUILD_MESSAGES events
        .intents(Some(
            GatewayIntents::GUILD_MESSAGES | GatewayIntents::DIRECT_MESSAGES,
        ))
        .build();

    // Start up the cluster
    let cluster = Cluster::new(config);
    cluster.up().await?;

    // The http client is seperate from the gateway,
    // so startup a new one
    let http = HttpClient::new(&token);

    // Since we only care about messages, make the cache only
    // cache message related events
    let cache_config = InMemoryConfigBuilder::new()
        .event_types(
            EventType::MESSAGE_CREATE
                | EventType::MESSAGE_DELETE
                | EventType::MESSAGE_DELETE_BULK
                | EventType::MESSAGE_UPDATE,
        )
        .build();
    let cache = InMemoryCache::from(cache_config);


    let mut events = cluster.events().await;

    let current_user = http.current_user().await?;
    // Startup an event loop for each event in the event stream
    while let Some(event) = events.next().await {
        // Update the cache
        cache.update(&event.1).await.expect("Cache failed, OhNoe");

        // Spawn a new task to handle the event
        handle_event(event, http.clone(), &current_user).await?;
    }

    Ok(())
}

async fn is_pm(http: &HttpClient, channel_id: ChannelId) -> Result<bool> {
    match http.channel(channel_id).await?.unwrap() {
        Channel::Private(_) => Ok(true),
        _ => Ok(false)
    }
}

async fn handle_event(
    event: (u64, Event),
    http: HttpClient,
    current_user: &CurrentUser
) -> Result<()> {
    match event {
        (_, Event::MessageCreate(msg)) => {
            // Don't send replies to yourself
            if msg.author.id != current_user.id {
                if is_pm(&http, msg.channel_id).await? {
                    // Check if the message is a single word
                    if msg.content.split_ascii_whitespace().count() != 1 {
                        http.create_message(msg.channel_id)
                            .content("Themes ideas should only be a single word")
                            .await?;
                    }
                    else {
                        let has_old_theme = ThemeIdeas::instance().lock().unwrap()
                            .try_add(msg.author.id, &msg.content)
                            .context("failed to save theme")?;

                        match has_old_theme {
                            SubmissionResult::Done => {
                                // Check if the message is a PM
                                http.create_message(msg.channel_id)
                                    .content("Theme idea registered, thanks!")
                                    .await?;
                            }
                            SubmissionResult::AlreadySubmitted => {
                                // Check if the message is a PM
                                http.create_message(msg.channel_id)
                                    .content("You can only send one idea. We replaced your old submission")
                                    .await?;
                            }
                        }
                    }
                }
                else {
                    handle_potential_command(&msg, http, current_user)
                        .await?;
                }
            }
        }
        (id, Event::ShardConnected(_)) => {
            println!("Connected on shard {}", id);
        }
        _ => {}
    }

    Ok(())
}

async fn handle_potential_command(
    msg: &Message,
    http: HttpClient,
    current_user: &CurrentUser
) -> Result<()> {
    let mut words = msg.content.split_ascii_whitespace();
    match words.next() {
        Some("~help") => {
            send_help_message(msg.channel_id, http).await?;
        }
        Some("~create_channel") => {
            handle_create_channel(
                &words.collect::<Vec<_>>(),
                msg.channel_id,
                msg.guild_id.expect("Tried to create channel in non-guild"),
                http
            ).await?;
        },
        Some("~role") => {
            handle_give_role(
                &words.collect::<Vec<_>>(),
                msg.channel_id,
                msg.guild_id.expect("Tried to create channel in non-guild"),
                msg.author.id,
                http
            ).await?;
        },
        Some("~leave") => {
            handle_remove_role(
                &words.collect::<Vec<_>>(),
                msg.channel_id,
                msg.guild_id.expect("Tried to create channel in non-guild"),
                msg.author.id,
                http
            ).await?;
        },
        Some(s) if s.chars().next() == Some('~') => {
            http.create_message(msg.channel_id)
                .content("Unrecognised command")
                .await?;
            send_help_message(msg.channel_id, http).await?;
        }
        // Not a command and probably not for us
        Some(_) => {
            // Check if we were mentioned
            if msg.mentions.contains_key(&current_user.id) {
                send_help_message(msg.channel_id, http).await?;
            }
        }
        None => {}
    }
    Ok(())
}

async fn send_help_message(
    channel_id: ChannelId,
    http: HttpClient,
) -> Result<()> {
    http.create_message(channel_id)
        .content("Talk to me in a PM to submit theme ideas.\n\nYou can also ask for a voice channel by sending `~create_channel <channel name>`\n\nGet a new role with `~role <role name>`\nand leave a role with `~leave <role name>`")
        .await?;
    Ok(())
}


async fn handle_create_channel<'a>(
    rest_command: &[&'a str],
    original_channel: ChannelId,
    guild: GuildId,
    http: HttpClient
) -> Result<()> {
    lazy_static! {
        static ref VALID_REGEX: Regex = Regex::new("[_A-z0-9]+").unwrap();
    }
    println!("got request for channel with name {:?}", rest_command);
    let reply = if rest_command.len() == 0 {
        "You need to specify a team name".to_string()
    }
    else if rest_command.len() > 1 {
        "Channel names can not contain whitespace".into()
    }
    // Check if the name is valid
    else if !VALID_REGEX.find_iter(rest_command[0])
            // The regex crate does not have a function to check if the whole string
            // matches the regex. Instead we check if any of the matches
            // are the same as the whole string being searched
            .any(|m| m.as_str() == rest_command[0])
    {
        "Channel names can only contain A-z, _ and digits".into()
    }
    else {
        let request = http.create_guild_channel(guild, rest_command[0])
            .kind(ChannelType::GuildVoice)
            .nsfw(true);
        match request.await {
            Ok(GuildChannel::Voice(_)) => {
                "Channel created 🎊".into()
            }
            Ok(_) => {
                "A channel was created but it wasn't a voice channel 🤔. Blame discord".into()
            }
            Err(e) => {
                println!(
                    "Failed to create channel {}. Error: {:?}",
                    rest_command[0],
                    e
                );
                "Channel creation failed, check logs for details".into()
            }
        }
    };

    http.create_message(original_channel)
        .content(&reply)
        .await?;

    Ok(())
}

async fn handle_give_role<'a>(
    rest_command: &[&'a str],
    original_channel: ChannelId,
    guild: GuildId,
    msg_author_id: UserId,
    http: HttpClient
) -> Result<()> {
    let mut message = "You need to to specify a valid role.\nAvailable roles are:```Programmer\n2D Artist\n3D Artist\nSound Designer\nMusician\nBoard Games```";

    let reply : String = if rest_command.len() == 0 {
        message.into()
    }
    else {
        let guild_roles = http.roles(guild).await?;

        for role in guild_roles {
            if role.name.to_lowercase() == rest_command.join(" ").to_lowercase() {
                let request = http.add_guild_member_role(guild, msg_author_id, role.id);

                match request.await {
                    Ok(_) => {
                        message = "New role assigned.";
                        println!("New role {} assigned to {}", role.name, msg_author_id);
                    }
                    Err(e) => {
                        message = "Something went wrong.";
                        println!("Couldn't assign role {} to {}\n{}", role.name, msg_author_id, e);
                    }
                }
            }
        }
        message.into()
    };

    http.create_message(original_channel)
        .content(&reply)
        .await?;

    Ok(())
}

async fn handle_remove_role<'a>(
    rest_command: &[&'a str],
    original_channel: ChannelId,
    guild: GuildId,
    msg_author_id: UserId,
    http: HttpClient
) -> Result<()> {
    let mut message = "You need to to specify a valid role.\nAvailable roles are:```Programmer\n2D Artist\n3D Artist\nSound Designer\nMusician\nBoard Games```";

    let reply : String = if rest_command.len() == 0 {
        message.into()
    }
    else {
        let guild_roles = http.roles(guild).await?;

        for role in guild_roles {
            if role.name.to_lowercase() == rest_command.join(" ").to_lowercase() {
                let request = http.remove_guild_member_role(guild, msg_author_id, role.id);

                match request.await {
                    Ok(_) => {
                        message = "Role removed.";
                        println!("{} left the role {}", msg_author_id, role.name);
                    }
                    Err(e) => {
                        message = "Something went wrong.";
                        println!("Couldn't remove role {} from {}\n{}", role.name, msg_author_id, e);
                    }
                }
            }
        }
        message.into()
    };

    http.create_message(original_channel)
        .content(&reply)
        .await?;

    Ok(())
}

