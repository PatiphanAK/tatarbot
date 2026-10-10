//! `VoicePort` on Songbird: one voice connection (`Call`) and built-in track
//! queue per guild. Track metadata (`TrackInfo`) rides along as the track's
//! user data, so the queue can be listed without a second data structure.
//!
//! Sources are opened lazily when a track starts (Songbird `Compose`), so a
//! queued link is not fetched until its turn.

use std::num::NonZeroU64;
use std::sync::Arc;

use async_trait::async_trait;
use songbird::events::{Event, EventContext, EventHandler, TrackEvent};
use songbird::input::{HttpRequest, Input};
use songbird::tracks::Track as SongbirdTrack;
use songbird::{Call, Songbird};
use tokio::sync::Mutex;

use crate::application::ports::{VoiceError, VoicePort};
use crate::domain::music::{GuildId, Track, TrackInfo, TrackSource, VoiceChannelId};

pub struct SongbirdVoice {
    manager: Arc<Songbird>,
    http: reqwest_audio::Client,
}

impl SongbirdVoice {
    pub fn new(manager: Arc<Songbird>, http: reqwest_audio::Client) -> Self {
        Self { manager, http }
    }

    fn call(&self, guild: GuildId) -> Option<Arc<Mutex<Call>>> {
        self.manager.get(songbird_guild(guild)?)
    }

    fn input(&self, source: &TrackSource) -> Input {
        match source {
            TrackSource::Http(url) => HttpRequest::new(self.http.clone(), url.to_string()).into(),
        }
    }
}

/// Disconnects from every guild; used on shutdown so the bot doesn't linger
/// in voice channels until Discord times it out.
pub async fn leave_all(manager: &Songbird) {
    let guilds: Vec<_> = manager.iter().map(|(guild, _)| guild).collect();
    for guild in guilds {
        if let Err(err) = manager.remove(guild).await {
            tracing::warn!(guild = %guild.0, %err, "failed to leave voice channel");
        }
    }
}

fn songbird_guild(guild: GuildId) -> Option<songbird::id::GuildId> {
    NonZeroU64::new(guild.0).map(Into::into)
}

#[async_trait]
impl VoicePort for SongbirdVoice {
    async fn current_channel(&self, guild: GuildId) -> Option<VoiceChannelId> {
        let call = self.call(guild)?;
        let channel = call.lock().await.current_channel()?;
        Some(VoiceChannelId(channel.0.get()))
    }

    async fn join(&self, guild: GuildId, channel: VoiceChannelId) -> Result<(), VoiceError> {
        let (Some(guild_id), Some(channel_id)) =
            (songbird_guild(guild), NonZeroU64::new(channel.0))
        else {
            return Err(VoiceError::Join("invalid id".into()));
        };
        let first_join = self.manager.get(guild_id).is_none();
        let call = self
            .manager
            .join(guild_id, songbird::id::ChannelId::from(channel_id))
            .await
            .map_err(|e| VoiceError::Join(e.to_string()))?;
        let mut call = call.lock().await;
        // The bot never listens; deafening tells Discord not to send audio.
        if let Err(err) = call.deafen(true).await {
            tracing::debug!(%err, "could not self-deafen");
        }
        if first_join {
            call.add_global_event(Event::Track(TrackEvent::Error), LogTrackErrors { guild });
        }
        tracing::info!(guild = guild.0, channel = channel.0, "joined voice channel");
        Ok(())
    }

    async fn enqueue(&self, guild: GuildId, track: Track) -> Result<usize, VoiceError> {
        let call = self.call(guild).ok_or(VoiceError::NotConnected)?;
        let input = self.input(&track.source);
        let mut call = call.lock().await;
        call.enqueue(SongbirdTrack::new_with_data(input, Arc::new(track.info)))
            .await;
        Ok(call.queue().len().saturating_sub(1))
    }

    async fn queue(&self, guild: GuildId) -> Vec<TrackInfo> {
        let Some(call) = self.call(guild) else {
            return Vec::new();
        };
        let handles = call.lock().await.queue().current_queue();
        handles
            .iter()
            .map(|handle| handle.data::<TrackInfo>().as_ref().clone())
            .collect()
    }

    async fn skip(&self, guild: GuildId) -> Result<(), VoiceError> {
        let call = self.call(guild).ok_or(VoiceError::NotConnected)?;
        let call = call.lock().await;
        call.queue()
            .skip()
            .map_err(|e| VoiceError::Other(e.to_string()))
    }

    async fn stop(&self, guild: GuildId) -> Result<(), VoiceError> {
        let call = self.call(guild).ok_or(VoiceError::NotConnected)?;
        call.lock().await.queue().stop();
        Ok(())
    }

    async fn leave(&self, guild: GuildId) -> Result<(), VoiceError> {
        let guild_id = songbird_guild(guild).ok_or(VoiceError::NotConnected)?;
        if let Some(call) = self.manager.get(guild_id) {
            call.lock().await.queue().stop();
        }
        self.manager
            .remove(guild_id)
            .await
            .map_err(|e| VoiceError::Other(e.to_string()))?;
        tracing::info!(guild = guild.0, "left voice channel");
        Ok(())
    }
}

/// A stream that fails to open or decode is skipped by the queue; log it so
/// operators can see bad links or dead stations.
struct LogTrackErrors {
    guild: GuildId,
}

#[async_trait]
impl EventHandler for LogTrackErrors {
    async fn act(&self, ctx: &EventContext<'_>) -> Option<Event> {
        if let EventContext::Track(tracks) = ctx {
            for (state, handle) in *tracks {
                let info = handle.data::<TrackInfo>();
                tracing::warn!(
                    guild = self.guild.0,
                    title = %info.title,
                    state = ?state.playing,
                    "track failed to play"
                );
            }
        }
        None
    }
}

#[cfg(test)]
mod tests {
    use songbird::input::LiveInput;
    use songbird::input::codecs::{get_codec_registry, get_probe};
    use url::Url;

    use super::*;
    use crate::adapters::outbound::audio_http;
    use crate::domain::music::StationList;

    /// Network test: every default station opens through the audio client and
    /// decodes, exactly as the voice adapter would play it.
    /// Run with `cargo test -- --ignored stations_decode`.
    #[tokio::test]
    #[ignore = "needs internet access"]
    async fn stations_decode() {
        let manager = Songbird::serenity();
        let voice = SongbirdVoice::new(manager, audio_http::client().unwrap());
        let stations = StationList::from_toml(include_str!("../../../stations.toml")).unwrap();
        for station in stations.iter() {
            let input = voice.input(&TrackSource::Http(station.url.clone()));
            let input = input
                .make_playable_async(get_codec_registry(), get_probe())
                .await
                .unwrap_or_else(|e| panic!("{}: {e}", station.name));
            let Input::Live(LiveInput::Parsed(mut parsed), _) = input else {
                panic!("{}: not parsed", station.name);
            };
            for _ in 0..20 {
                let packet = parsed.format.next_packet().expect("packet");
                parsed.decoder.decode(&packet).expect("decodable audio");
            }
        }
    }

    /// The SSRF guard also applies inside Songbird's fetch.
    #[tokio::test]
    async fn private_hosts_are_refused_at_play_time() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = Url::parse(&format!(
            "http://localhost:{}/x.mp3",
            listener.local_addr().unwrap().port()
        ))
        .unwrap();
        let voice = SongbirdVoice::new(Songbird::serenity(), audio_http::client().unwrap());

        let result = voice
            .input(&TrackSource::Http(url))
            .make_playable_async(get_codec_registry(), get_probe())
            .await;

        assert!(result.is_err(), "fetching localhost must fail");
    }
}
