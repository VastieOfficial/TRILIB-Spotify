use std::{
    collections::HashMap, env,fs, io::{Read}, panic::AssertUnwindSafe, path::PathBuf, time::Duration
};
use anyhow::{anyhow, Result};

use librespot::{
    audio::{AudioDecrypt, AudioFile},
    core::{
        authentication::Credentials,
        config::SessionConfig,
        session::Session,
        spotify_id::{SpotifyId, SpotifyItemType},
    },
    metadata::audio::AudioFileFormat,
    playback::{config::PlayerConfig, player::PlayerTrackLoader},
};

use hyper::StatusCode;
use reqwest::Client;
use serde_json::Value;
use std::error::Error;
use tokio::time::timeout;
use urlencoding::encode;

use axum::{Json, Router, extract::DefaultBodyLimit, response::IntoResponse, routing::post};
use futures_util::future::FutureExt;
use once_cell::sync::Lazy;
use regex::Regex;
use serde::{Deserialize, Serialize};


fn get_extension(format: AudioFileFormat) -> &'static str {
    use AudioFileFormat::*;
    match format {
        FLAC_FLAC | FLAC_FLAC_24BIT => "flac",
        MP3_96 | MP3_160 | MP3_160_ENC | MP3_256 | MP3_320 => "mp3",
        AAC_24 | AAC_48 | AAC_160 | AAC_320 | XHE_AAC_24 | XHE_AAC_16 | XHE_AAC_12 => "aac",
        OGG_VORBIS_96 | OGG_VORBIS_160 | OGG_VORBIS_320 => "ogg",
        MP4_128 => "mp4",
        OTHER5 => "dat",
    }
}
#[derive(Debug)]
struct TrackError(&'static str);

impl std::fmt::Display for TrackError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.0)
    }
}

impl Error for TrackError {}

pub async fn save_best_medium_low(track: SpotifyId, hash: String, token: String)  -> Result<(), Box<dyn Error>> {
    let session_config = SessionConfig::default();
    let player_config = PlayerConfig::default();
    let session = Session::new(session_config, None);

    session
        .connect(Credentials::with_access_token(token), false)
        .await?;
    
    let player = PlayerTrackLoader::new(session, player_config);
    let decrypted_files: Vec<(AudioFileFormat, AudioDecrypt<AudioFile>)> =
        player.load_decrypted_files(track).await;

    let mut file_map: HashMap<AudioFileFormat, AudioDecrypt<AudioFile>> = HashMap::new();
    for (format, file) in decrypted_files {
        file_map.insert(format, file);
    }

    let best_priority = [
        AudioFileFormat::FLAC_FLAC_24BIT,
        AudioFileFormat::FLAC_FLAC,
        AudioFileFormat::MP3_320,
        AudioFileFormat::AAC_320,
        AudioFileFormat::MP3_256,
        AudioFileFormat::OGG_VORBIS_320,
    ];

    let medium_priority = [
        AudioFileFormat::MP3_160,
        AudioFileFormat::MP3_160_ENC,
        AudioFileFormat::AAC_48,
        AudioFileFormat::AAC_24,
        AudioFileFormat::XHE_AAC_24,
        AudioFileFormat::OGG_VORBIS_320,
        AudioFileFormat::MP4_128,
    ];

    let low_priority = [
        AudioFileFormat::AAC_24,
        AudioFileFormat::XHE_AAC_24,
        AudioFileFormat::XHE_AAC_16,
        AudioFileFormat::XHE_AAC_12,
        AudioFileFormat::OGG_VORBIS_160,
        AudioFileFormat::MP3_96,
        AudioFileFormat::OGG_VORBIS_96,
    ];

    let mut used_formats = std::collections::HashSet::new();

    let try_save = async
        |label: &str,
         tiers: &[&[AudioFileFormat]],
         file_map: &mut HashMap<AudioFileFormat, AudioDecrypt<AudioFile>>,
         used_formats: &mut std::collections::HashSet<AudioFileFormat>| {
            for tier in tiers {
                for format in *tier {
                    if used_formats.contains(format) {
                        continue;
                    }
                    if let Some(file) = file_map.remove(format) {
                        let format_clone = *format;
                        let label_owned = label.to_string();
                        let hash_owned = hash.clone();
                        let path_clone = (*CACHEDIR).clone();

                        let buffer = tokio::task::spawn_blocking(move || {
                            let mut buf = Vec::new();
                            let mut f = file;
                            f.read_to_end(&mut buf).unwrap();
                            buf
                        })
                        .await
                        .unwrap();

                        let ext = get_extension(format_clone);
                        let mut filepath = path_clone.clone();
                        filepath.push(&hash_owned);
                        filepath.push("spotify");
                        fs::create_dir_all(&filepath).unwrap();
                        filepath.push(format!("{}.{}", label_owned, ext));

                        fs::write(filepath.as_path(), &buffer).unwrap();
                        println!(
                            "Saved {} as {:?} -> {}",
                            label_owned,
                            format_clone,
                            filepath.to_str().unwrap()
                        );

                        used_formats.insert(*format);
                        return;
                    }
                }
            }
            println!("No available format found for {}", label);
        };

    try_save(
        "best",
        &[&best_priority, &medium_priority, &low_priority],
        &mut file_map,
        &mut used_formats,
    ).await;
    //if used_formats.len() == 0 {
        try_save(
            "medium",
            &[&medium_priority, &low_priority, &best_priority],
            &mut file_map,
            &mut used_formats,
        ).await;
    //}

    //if used_formats.len() == 0 {
        try_save(
            "low",
            &[&low_priority, &medium_priority, &best_priority],
            &mut file_map,
            &mut used_formats,
        ).await;
    //}

    if used_formats.len() == 0 {
        return Err(Box::new(TrackError("Track can't be saved: no files available")));
    }
    Ok(())
}

static CACHEDIR: Lazy<PathBuf> = Lazy::new(|| {
    env::var("TRI_CACHE")
        .map(PathBuf::from)
        .unwrap_or_else(|_| {
            let mut path = env::current_dir().unwrap();
            path.push("TRICACHE");
            path
        })
});
static PORT: Lazy<u16> = Lazy::new(|| {
    env::var("TRI_SPOTIFY_PORT")
        .ok()
        .and_then(|s| s.parse::<u16>().ok())
        .unwrap_or(3500)
});
static REGEX: Lazy<Regex> = Lazy::new(|| {
    Regex::new(r"(?:spotify:track:|https://open\.spotify\.com/track/)([a-zA-Z0-9]+)").unwrap()
});

fn extract_spotify_id(payload_url: &str) -> String {
    return REGEX
        .captures(payload_url)
        .and_then(|captures| captures.get(1)) // Get the first capture group
        .map(|cap| cap.as_str())
        .unwrap()
        .to_string(); // Convert to SpotifyId (handling errors)
}

async fn search_song_id(query: &str, access_token: &str) -> Result<String, Box<dyn Error>> {
    let client = Client::new();

    let uri = format!(
        "https://api.spotify.com/v1/search?q={}&type=track&limit=1",
        encode(query)
    );

    let res = client
        .get(&uri)
        .header("Authorization", format!("Bearer {}", access_token))
        .send()
        .await?;

    if !res.status().is_success() {
        return Err(format!("Spotify API error: {}", res.status()).into());
    }
    let x: String = (res.text().await?);
    let json: Value = serde_json::from_str(&x)?;
    println!("{}", x);

    if let Some(id) = json["tracks"]["items"][0]["id"].as_str() {
        Ok(id.to_string())
    } else {
        Err("No track found".into())
    }
}


async fn download(
    Json(payload): Json<DownloadSpotify>,
) -> impl IntoResponse {
    let result = timeout(Duration::from_secs(300), async move {
        let run = AssertUnwindSafe(async move {
            // get track id
            let mut track = if payload.url.is_empty() {
                let id = search_song_id(&payload.title, &payload.token)
                    .await
                    .map_err(|e| anyhow!("search_song_id failed: {}", e))?;

                SpotifyId::from_base62(&id)
                    .map_err(|_| anyhow!("invalid SpotifyId"))?
            } else {
                let extracted = extract_spotify_id(&payload.url);
                SpotifyId::from_base62(extracted.as_str())
                    .map_err(|_| anyhow!("invalid SpotifyId"))?
            };

            track.item_type = SpotifyItemType::Track;

            // assume save_best_medium_low returns Result<(), E>
            save_best_medium_low(track, payload.hash, payload.token)
                .await
                .map_err(|e| anyhow!("save_best_medium_low failed: {}", e))?;

            Ok::<(), anyhow::Error>(())
        })
        .catch_unwind()
        .await;

        match run {
            Ok(inner) => inner,
            Err(panic) => Err(anyhow!("panic: {:?}", panic)),
        }
    })
    .await;

    match result {
        Ok(Ok(())) => (
            StatusCode::OK,
            axum::Json(IsOK { ok: true, error: "".to_string() }),
        ),
        Ok(Err(err)) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            axum::Json(IsOK { ok: false, error: err.to_string() }),
        ),
        Err(timeout_err) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            axum::Json(IsOK { ok: false, error: format!("timeout: {timeout_err:?}") }),
        ),
    }
}

#[derive(Deserialize)]
struct DownloadSpotify {
    url: String,
    title: String,
    hash: String,
    token: String,
}

#[derive(Serialize)]
struct IsOK {
    ok: bool,
    error: String,
}

#[tokio::main]
async fn main() {
    tracing_subscriber::fmt::init();
    let app = Router::new()
        .route("/dl", post(download))
        .layer(DefaultBodyLimit::max(1024 * 1024));
    let listener = tokio::net::TcpListener::bind(format!("0.0.0.0:{}", *PORT))
        .await
        .unwrap();
    axum::serve(listener, app).await.unwrap();
}
