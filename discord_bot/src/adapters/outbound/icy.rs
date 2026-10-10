//! Live radio streams with ICY (SHOUTcast/Icecast) now-playing metadata.
//!
//! Asking with `Icy-MetaData: 1` makes the server interleave a metadata block
//! after every `icy-metaint` audio bytes: one length byte (x16), then
//! `StreamTitle='Artist - Title';...` padded with NULs (length 0 = unchanged).
//! Those bytes must never reach the decoder, so `IcyParser` strips them and
//! records the title in a `NowPlaying` the queue can read.
//!
//! A live stream that ends has dropped; reconnecting is decided per track in
//! `songbird_voice` (a fresh connection and decoder, not a byte splice).

use std::io;
use std::pin::Pin;
use std::sync::{Arc, RwLock};
use std::task::{Context, Poll};

use async_trait::async_trait;
use bytes::{Buf, Bytes};
use futures::StreamExt;
use songbird::input::{
    AsyncAdapterStream, AsyncMediaSource, AudioStream, AudioStreamError, Compose,
};
use symphonia::core::io::MediaSource;
use tokio::io::{AsyncRead, AsyncSeek, ReadBuf};
use tokio_util::io::StreamReader;

const MAX_TITLE_CHARS: usize = 100;
/// Buffer between the network task and the decoder.
const ADAPTER_BUFFER: usize = 64 * 1024;

/// The current `StreamTitle` of a live track, shared with the queue view.
#[derive(Debug, Default)]
pub struct NowPlaying(RwLock<Option<String>>);

impl NowPlaying {
    pub fn get(&self) -> Option<String> {
        self.0.read().ok().and_then(|t| t.clone())
    }

    fn set(&self, title: String) {
        if let Ok(mut current) = self.0.write()
            && current.as_deref() != Some(title.as_str())
        {
            tracing::debug!(%title, "now playing");
            *current = Some(title);
        }
    }
}

/// Splits an ICY byte stream into audio and titles, across arbitrary chunk
/// boundaries.
#[derive(Debug)]
pub struct IcyParser {
    metaint: usize,
    state: State,
}

#[derive(Debug)]
enum State {
    Audio { left: usize },
    Length,
    Meta { left: usize, buf: Vec<u8> },
}

impl IcyParser {
    pub fn new(metaint: usize) -> Self {
        Self {
            metaint,
            state: State::Audio { left: metaint },
        }
    }

    /// Returns the audio bytes in `chunk`, and the last title completed in it.
    pub fn feed(&mut self, mut chunk: Bytes) -> (Vec<u8>, Option<String>) {
        let mut audio = Vec::with_capacity(chunk.len());
        let mut title = None;
        while chunk.has_remaining() {
            match &mut self.state {
                State::Audio { left } => {
                    let n = (*left).min(chunk.len());
                    audio.extend_from_slice(&chunk[..n]);
                    chunk.advance(n);
                    *left -= n;
                    if *left == 0 {
                        self.state = State::Length;
                    }
                }
                State::Length => {
                    let len = usize::from(chunk[0]) * 16;
                    chunk.advance(1);
                    self.state = if len == 0 {
                        State::Audio { left: self.metaint }
                    } else {
                        State::Meta {
                            left: len,
                            buf: Vec::with_capacity(len),
                        }
                    };
                }
                State::Meta { left, buf } => {
                    let n = (*left).min(chunk.len());
                    buf.extend_from_slice(&chunk[..n]);
                    chunk.advance(n);
                    *left -= n;
                    if *left == 0 {
                        if let Some(t) = parse_stream_title(buf) {
                            title = Some(t);
                        }
                        self.state = State::Audio { left: self.metaint };
                    }
                }
            }
        }
        (audio, title)
    }
}

/// `StreamTitle='...';` from a metadata block. Titles may contain `'`, so the
/// value ends at the last `';` before the next field. UTF-8, else Latin-1.
pub fn parse_stream_title(block: &[u8]) -> Option<String> {
    let block = block.split(|&b| b == 0).next().unwrap_or_default();
    let text = match std::str::from_utf8(block) {
        Ok(s) => s.to_owned(),
        Err(_) => block.iter().map(|&b| char::from(b)).collect(),
    };
    let start = text.find("StreamTitle='")? + "StreamTitle='".len();
    let rest = &text[start..];
    let end = rest
        .find("';StreamUrl=")
        .or_else(|| rest.rfind("';"))
        .unwrap_or(rest.len());
    let title: String = rest[..end]
        .chars()
        .filter(|c| !c.is_control())
        .collect::<String>()
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ");
    (!title.is_empty()).then(|| title.chars().take(MAX_TITLE_CHARS).collect())
}

type Reader = Box<dyn AsyncRead + Send + Sync + Unpin>;

/// The response body as a decoder input, ICY blocks removed (they are
/// stripped as the chunks arrive, before the bytes reach the reader). Not
/// seekable; the end of the body is the end of the track.
struct IcySource {
    reader: Reader,
}

impl IcySource {
    fn new(
        response: reqwest_audio::Response,
        metaint: Option<usize>,
        now: Arc<NowPlaying>,
    ) -> Self {
        let mut parser = metaint.map(IcyParser::new);
        let body = response.bytes_stream().map(move |chunk| {
            let chunk = chunk.map_err(|e| io::Error::other(e.without_url()))?;
            Ok::<_, io::Error>(match &mut parser {
                None => chunk,
                Some(parser) => {
                    let (audio, title) = parser.feed(chunk);
                    if let Some(title) = title {
                        now.set(title);
                    }
                    Bytes::from(audio)
                }
            })
        });
        Self {
            reader: Box::new(StreamReader::new(body)),
        }
    }
}

impl AsyncRead for IcySource {
    fn poll_read(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        out: &mut ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        Pin::new(&mut self.reader).poll_read(cx, out)
    }
}

impl AsyncSeek for IcySource {
    fn start_seek(self: Pin<&mut Self>, _: io::SeekFrom) -> io::Result<()> {
        Err(io::ErrorKind::Unsupported.into())
    }

    fn poll_complete(self: Pin<&mut Self>, _: &mut Context<'_>) -> Poll<io::Result<u64>> {
        Poll::Ready(Err(io::ErrorKind::Unsupported.into()))
    }
}

#[async_trait]
impl AsyncMediaSource for IcySource {
    fn is_seekable(&self) -> bool {
        false
    }

    async fn byte_len(&self) -> Option<u64> {
        None
    }
}

/// Songbird input for a live station: connects when the track starts.
pub struct LiveStream {
    pub client: reqwest_audio::Client,
    pub url: String,
    pub now_playing: Arc<NowPlaying>,
}

#[async_trait]
impl Compose for LiveStream {
    fn create(&mut self) -> Result<AudioStream<Box<dyn MediaSource>>, AudioStreamError> {
        Err(AudioStreamError::Unsupported)
    }

    async fn create_async(
        &mut self,
    ) -> Result<AudioStream<Box<dyn MediaSource>>, AudioStreamError> {
        let fail = |e: String| AudioStreamError::Fail(e.into());
        let response = self
            .client
            .get(&self.url)
            .header("Icy-MetaData", "1")
            .send()
            .await
            .and_then(|r| r.error_for_status())
            .map_err(|e| fail(e.without_url().to_string()))?;
        let metaint = response
            .headers()
            .get("icy-metaint")
            .and_then(|v| v.to_str().ok())
            .and_then(|v| v.trim().parse::<usize>().ok())
            .filter(|&n| n > 0);
        let source = IcySource::new(response, metaint, self.now_playing.clone());
        Ok(AudioStream {
            input: Box::new(AsyncAdapterStream::new(Box::new(source), ADAPTER_BUFFER)),
        })
    }

    fn should_create_async(&self) -> bool {
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// An ICY stream: `audio` with a block after every `metaint` bytes.
    fn icy_stream(audio: &[u8], metaint: usize, titles: &[Option<&str>]) -> Vec<u8> {
        let mut out = Vec::new();
        let mut titles = titles.iter();
        for chunk in audio.chunks(metaint) {
            out.extend_from_slice(chunk);
            if chunk.len() < metaint {
                break;
            }
            match titles.next().copied().flatten() {
                None => out.push(0),
                Some(t) => {
                    let meta = format!("StreamTitle='{t}';StreamUrl='https://x';");
                    let len = meta.len().div_ceil(16);
                    out.push(len as u8);
                    out.extend_from_slice(meta.as_bytes());
                    out.resize(out.len() + len * 16 - meta.len(), 0);
                }
            }
        }
        out
    }

    #[test]
    fn strips_metadata_at_every_chunk_boundary() {
        let audio: Vec<u8> = (0..500u32).map(|i| (i * 7 % 251) as u8).collect();
        let titles = [
            Some("A - One"),
            None,
            Some("B - Two's \"song\""),
            None,
            None,
        ];
        let stream = icy_stream(&audio, 64, &titles);
        for chunk_size in 1..=70 {
            let mut parser = IcyParser::new(64);
            let mut out = Vec::new();
            let mut seen = Vec::new();
            for chunk in stream.chunks(chunk_size) {
                let (a, t) = parser.feed(Bytes::copy_from_slice(chunk));
                out.extend(a);
                seen.extend(t);
            }
            assert_eq!(out, audio, "chunk size {chunk_size}");
            assert_eq!(
                seen,
                ["A - One", "B - Two's \"song\""],
                "chunk size {chunk_size}"
            );
        }
    }

    #[test]
    fn parses_stream_titles() {
        let parse = |s: &[u8]| parse_stream_title(s);
        assert_eq!(
            parse(b"StreamTitle='Sango - Never-Ending';StreamUrl='https://somafm.com/x.jpg';\0\0"),
            Some("Sango - Never-Ending".into())
        );
        assert_eq!(
            parse(b"StreamTitle='Rock 'n' Roll';"),
            Some("Rock 'n' Roll".into())
        );
        assert_eq!(parse(b"StreamTitle='';"), None);
        assert_eq!(parse(b"StreamUrl='x';"), None);
        // Latin-1 fallback for non-UTF-8 servers.
        assert_eq!(
            parse(b"StreamTitle='Caf\xe9 \t Song';"),
            Some("Café Song".into())
        );
        let long = format!("StreamTitle='{}';", "x".repeat(300));
        assert_eq!(parse(long.as_bytes()).unwrap().len(), MAX_TITLE_CHARS);
    }

    /// Network: the lofi station's title arrives and its audio still decodes.
    /// Run with `cargo test -- --ignored icy_live`.
    // Multi-threaded, decoding on a blocking thread: `next_packet` blocks
    // while Songbird's adapter task refills its buffer from the network.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    #[ignore = "needs internet access"]
    async fn icy_live_station_reports_now_playing() {
        use songbird::input::codecs::{get_codec_registry, get_probe};
        use songbird::input::{Input, LiveInput};

        crate::tls::install_crypto_provider().unwrap();
        let now_playing = Arc::new(NowPlaying::default());
        let input = Input::Lazy(Box::new(LiveStream {
            client: super::super::audio_http::client().unwrap(),
            url: "https://ice2.somafm.com/fluid-128-mp3".into(),
            now_playing: now_playing.clone(),
        }));
        let Input::Live(LiveInput::Parsed(mut parsed), _) = input
            .make_playable_async(get_codec_registry(), get_probe())
            .await
            .unwrap()
        else {
            panic!("not parsed");
        };
        // 45,000 bytes of 128 kb/s audio is ~120 MP3 frames.
        tokio::task::spawn_blocking(move || {
            for _ in 0..300 {
                let packet = parsed.format.next_packet().unwrap();
                parsed.decoder.decode(&packet).unwrap();
            }
        })
        .await
        .unwrap();
        let title = now_playing.get().expect("a StreamTitle within 300 frames");
        assert!(title.contains(" - "), "{title}");
    }
}
