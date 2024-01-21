use anyhow::Context as _;
use futures_util::stream::TryStreamExt;
use itertools::Itertools;
use regex::Regex;
use rspotify::clients::BaseClient;
use serenity::builder::{
    CreateAllowedMentions, CreateInteractionResponse,
    CreateInteractionResponseMessage, EditInteractionResponse, EditMessage,
};
use serenity::model::prelude::CommandInteraction;
use serenity::model::prelude::{ChannelId, Message};
use serenity::prelude::RwLock;
use serenity::{async_trait, prelude::Context};
use serenity_command::{BotCommand, CommandResponse};
use serenity_command_derive::Command;
use std::collections::HashMap;
use std::sync::Arc;

use serenity_command_handler::modules::polls; // serenity-command-handler, for hooking

use serenity_command_handler::{
    CommandStore, CompletionStore, Handler, HandlerBuilder, Module, ModuleMap,
};

#[derive(Debug)]
pub struct TrackInfo {
    pub number: u32,
    pub name: String,
    pub uri: Option<String>,
    pub duration: chrono::Duration,
}

#[derive(Debug)]
pub struct AlbumInfo {
    pub artist: String,
    pub name: String,
    pub uri: Option<String>,
    pub tracks: Vec<TrackInfo>,
}

#[derive(Debug)]
pub struct LPInfo {
    pub playlist: AlbumInfo,
    pub started: Option<chrono::DateTime<chrono::Utc>>,
}

enum PlayState<'a> {
    NotStarted,
    Finished(chrono::Duration), // how long ago
    Playing {
        track: &'a TrackInfo,
        position: chrono::Duration,
    },
}

impl LPInfo {
    fn now_playing(&self) -> PlayState {
        let started = match self.started {
            None => {
                return PlayState::NotStarted;
            }
            Some(started) => started,
        };
        let now = chrono::offset::Utc::now();
        if started > now {
            eprintln!(
                "LPInfo: Started timestamp in the future! started={} > now={}",
                started, now
            );
            return PlayState::NotStarted;
        }
        let mut remain = now - started;
        for track in self.playlist.tracks.iter() {
            if track.duration > remain {
                return PlayState::Playing {
                    track: &track,
                    position: remain,
                };
            } else {
                remain = remain - track.duration;
            }
        }
        // We passed all the tracks
        // remain = now - started - sum(track duration)
        // How long ago the playlist finished
        PlayState::Finished(remain)
    }
}

// Format Duration as hh:mm:ss
fn display_duration(duration: &chrono::Duration) -> String {
    let allsecs = duration.num_seconds();
    let seconds = allsecs % 60;
    let minutes = allsecs / 60 % 60;
    let hours = allsecs / 3600;
    if hours > 0 {
        format!("{}:{:0>2}:{:0>2}", hours, minutes, seconds)
    } else {
        format!("{:0>2}:{:0>2}", minutes, seconds)
    }
}

// Regex to identity spotify album URIs and extract album id
const SPOTIFY_ALBUM_RE: &str =
    "\\bhttps://open.spotify.com/album/([a-zA-Z0-9]+)(?:\\?[a-zA-Z?=&]*)\\b";

async fn fetch_album_info<C: BaseClient>(
    client: &C,
    album_id_str: &str,
) -> anyhow::Result<AlbumInfo> {
    let album_id = rspotify::model::AlbumId::from_id(album_id_str)
        .context("trying to parse album ID")?;

    let album = client
        .album(album_id.clone(), None)
        .await
        .context("fetching album")?;
    let artists = album
        .artists
        .iter()
        .map(|a| a.name.as_ref())
        .collect::<Vec<_>>()
        .join(", ");
    eprintln!("Album pinged: {} - {} ", &artists, &album.name); // Debug
    let tracks = client
        .album_track(album_id, None)
        .map_ok(|track| TrackInfo {
            number: track.track_number,
            name: track.name.to_string(),
            duration: track.duration.clone(),
            uri: track.external_urls.get("spotify").map(|s| s.to_owned()),
        })
        .try_collect::<Vec<TrackInfo>>()
        .await?;
    Ok(AlbumInfo {
        artist: artists.clone(),
        name: album.name.to_string(),
        uri: album.external_urls.get("spotify").map(|s| s.to_owned()),
        tracks,
    })
}

#[derive(Command, Debug)]
#[cmd(name = "lp", desc = "Check if listening party is going")]
pub struct CurrentLP {}

#[async_trait]
impl BotCommand for CurrentLP {
    type Data = Handler;
    async fn run(
        self,
        data: &Handler,
        ctx: &Context,
        interaction: &CommandInteraction,
    ) -> anyhow::Result<CommandResponse> {
        let channel = interaction.channel_id;
        let lpmod = data.module::<LP>().unwrap();
        let lps = lpmod.last_pinged.read().await;
        let lp = lps.get(&channel);
        let msg = match lp {
            None => {
                "There is no listening party at the moment.".to_string()
            }
            Some(lpinfo) => {
                let playlist_duration: chrono::Duration =
                    lpinfo.playlist.tracks.iter().map(|t| t.duration).sum();
                let album_uri_str = match &lpinfo.playlist.uri {
                    None => "No album link available".to_string(),
                    // Use angle brackets to suppress link preview
                    Some(uri) => format!("Album: <{}>", &uri),
                };
                let now_playing = match lpinfo.now_playing() {
                    PlayState::NotStarted => "Not yet started.".to_string(),
                    PlayState::Finished(end) => {
                        format!("LP ended {} ago", display_duration(&end))
                    }
                    PlayState::Playing { track, position } => format!(
                        "Playing Track {}: `{}` at **{}** / {}",
                        &track.number,
                        &track.name,
                        display_duration(&position),
                        display_duration(&track.duration)
                    ),
                };
                format!(
                    "Ongoing Listening Party:\n {} - {} ({}) \n {} \n {}",
                    &lpinfo.playlist.artist,
                    &lpinfo.playlist.name,
                    display_duration(&playlist_duration),
                    &now_playing,
                    album_uri_str,
                )
            }
        };

        let http = &ctx.http;
        interaction
            .create_response(
                http,
                CreateInteractionResponse::Message(
                    CreateInteractionResponseMessage::new()
                        .content(msg)
                        .allowed_mentions(
                            CreateAllowedMentions::new().empty_users(),
                        ),
                ),
            )
            .await
            .context("error creating response")?;
        Ok(CommandResponse::None)
    }
}

pub type PingedMap = Arc<RwLock<HashMap<ChannelId, LPInfo>>>;

pub struct LP {
    last_pinged: PingedMap,
}

impl Clone for LP {
    fn clone(&self) -> Self {
        LP {
            last_pinged: self.last_pinged.clone(),
        }
    }
}

#[async_trait]
impl polls::ModPollReadyHandler for LP {
    async fn ready(&self, channelid: &ChannelId) {
        self.start_lp(channelid).await;
    }
}

// Roles used for pinging listening parties
const LP_ROLES: &'static [u64] = &[
    1198354637137391709, // @Listening Party in test guild
];

impl LP {
    pub fn new() -> Self {
        LP {
            last_pinged: Default::default(),
        }
    }

    // Handle messages to remember the last pinged album
    pub async fn handle_message<C: BaseClient>(
        &self,
        client: &C,
        msg: &Message,
    ) {
        let msg_txt: &str = &msg.content;

        // Check if the specified roles
        if msg
            .mention_roles
            .iter()
            .any(|&role| LP_ROLES.iter().contains(&role.get()))
        {
            let album = match Regex::new(&SPOTIFY_ALBUM_RE)
                .unwrap()
                .captures(&msg_txt)
            {
                None => return,
                Some(caps) => match fetch_album_info(client, &caps[1]).await {
                    Err(e) => {
                        eprintln!("Error resolving ping: {}", e);
                        return;
                    }
                    Ok(album) => album,
                },
            };
            let mut channels = self.last_pinged.write().await;

            (*channels).insert(
                msg.channel_id,
                LPInfo {
                    playlist: album,
                    started: None,
                },
            );
            eprintln!("Found pinged LP!");
            ()
        };
    }

    pub async fn start_lp(&self, channel: &ChannelId) {
        let now = chrono::offset::Utc::now();
        let mut channels = self.last_pinged.write().await;
        channels
            .entry(*channel)
            .and_modify(|lp_info| lp_info.started = Some(now));
        ()
    }
}

#[async_trait]
impl Module for LP {
    async fn add_dependencies(
        builder: HandlerBuilder,
    ) -> anyhow::Result<HandlerBuilder> {
        Ok(builder)
    }
    fn register_commands(
        &self,
        store: &mut CommandStore,
        _completions: &mut CompletionStore,
    ) {
        eprintln!("Created LP module");
        store.register::<CurrentLP>();
    }

    async fn init(m: &ModuleMap) -> anyhow::Result<Self> {
        Ok(LP {
            last_pinged: Default::default(),
        })
    }
}
