//! Joins a real call, unmutes, and logs what the wire actually carries.
//!
//! ```sh
//! MM_INSECURE_TLS=1 RUST_LOG=debug \
//!   cargo run -p matras --example callprobe -- [channel_id] [seconds] [camera]
//! ```
//!
//! A third argument of `camera` also turns the webcam on for the run.
//!
//! With no channel id it calls into the DM with yourself, so the "started a
//! call" post lands where nobody else sees it. Credentials come from the
//! session the app saved, or from `MM_URL` / `MM_TOKEN`.
//!
//! The audio pipeline is the app's own module, included verbatim rather than
//! reimplemented — a probe that tests different code tests nothing.

use std::time::Duration;

use mattermost_api::ws::WebSocket;
use mattermost_api::Client;
use mattermost_calls::{CallSession, CallUpdate, JoinOptions};

#[path = "../src/audio.rs"]
mod audio;
#[path = "../src/runtime.rs"]
#[allow(dead_code)] // the example only needs the Tokio handle
mod runtime;
#[path = "../src/video.rs"]
#[allow(dead_code, unused_imports)] // the window half of it needs a GTK main loop
mod video;

fn main() -> Result<(), Box<dyn std::error::Error>> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "callprobe=debug,mattermost_calls=debug".into()),
        )
        .init();

    let mut args = std::env::args().skip(1);
    // An empty first argument means "use the DM with myself".
    let channel_arg = args.next().filter(|s| !s.is_empty());
    let seconds: u64 = args.next().and_then(|s| s.parse().ok()).unwrap_or(30);
    // Comma-separated extras: `camera`, `record`.
    let extras = args.next().unwrap_or_default();
    let camera = extras.contains("camera");
    let record = extras.contains("record");

    let (server, token) = credentials()?;
    let client = Client::new(&server)?;
    client.set_token(token.clone());

    let rt = runtime::runtime();
    let (session, audio, _camera) = rt.block_on(async {
        let channel_id = match channel_arg {
            Some(id) => id,
            None => {
                let me = client.me().await?;
                let dm = client.create_direct_channel(&me.id, &me.id).await?;
                tracing::info!(channel = %dm.id, "calling into the DM with myself");
                dm.id
            }
        };

        let discovery = mattermost_calls::discover(&client).await?;
        tracing::info!(
            version = %discovery.version.version,
            ice = discovery.ice_servers.len(),
            dc_signaling = discovery.config.dc_signaling_allowed(),
            dc_locking = discovery.dc_locking,
            "discovery"
        );

        let ws = WebSocket::connect(client.websocket_url(), token);
        let opts = JoinOptions {
            channel_id,
            ..Default::default()
        };
        let session = std::sync::Arc::new(mattermost_calls::AnyCall::Calls(
            CallSession::join(&client, ws, opts, &discovery).await?,
        ));
        tracing::info!(
            session = %session.session_id(),
            call_id = %session.call_id(),
            "joined"
        );

        if record {
            mattermost_calls::set_recording(&client, session.channel_id(), true).await?;
            tracing::info!("recording started");
        }

        let audio = audio::AudioIo::start(session.clone()).map_err(std::io::Error::other)?;
        let track = session.unmute().await?;
        audio.set_track(track);
        tracing::info!("unmuted; microphone is live");

        let cam = if camera {
            let track = session.start_video().await?;
            let sender = video::VideoSender::camera(track).map_err(std::io::Error::other)?;
            tracing::info!("camera is live");
            Some(sender)
        } else {
            None
        };

        Ok::<_, Box<dyn std::error::Error>>((session, audio, cam))
    })?;

    // Everything below stays on this thread: `AudioIo` owns the cpal streams,
    // which are not `Send`.
    rt.block_on(async {
        let mut updates = session.subscribe();
        let deadline = tokio::time::Instant::now() + Duration::from_secs(seconds);
        loop {
            tokio::select! {
                _ = tokio::time::sleep_until(deadline) => break,
                update = updates.recv() => match update {
                    Ok(CallUpdate::RemoteTrack { session_id, track_type, track }) => {
                        tracing::info!(%session_id, %track_type, "remote track");
                        if track_type == mattermost_calls::protocol::track_type::VOICE {
                            audio.play(session_id, track);
                        }
                    }
                    Ok(CallUpdate::State(state)) => {
                        tracing::info!(
                            participants = state.participant_count(),
                            call_id = %state.id,
                            recording = ?state.recording,
                            "roster"
                        );
                    }
                    Ok(other) => tracing::info!(?other, "update"),
                    Err(tokio::sync::broadcast::error::RecvError::Lagged(n)) => {
                        tracing::warn!(skipped = n, "lagged");
                    }
                    Err(_) => break,
                },
            }
        }
    });

    if record {
        rt.block_on(async {
            match mattermost_calls::set_recording(&client, session.channel_id(), false).await {
                Ok(()) => tracing::info!("recording stopped"),
                Err(e) => tracing::warn!(error = %e, "could not stop the recording"),
            }
        });
    }
    rt.block_on(session.leave())?;
    tracing::info!("left the call");
    Ok(())
}

fn credentials() -> Result<(String, String), Box<dyn std::error::Error>> {
    if let (Ok(url), Ok(token)) = (std::env::var("MM_URL"), std::env::var("MM_TOKEN")) {
        return Ok((url, token));
    }
    let path = format!(
        "{}/.config/{}/session.json",
        std::env::var("HOME")?,
        matras_app_id()
    );
    let saved: serde_json::Value = serde_json::from_str(&std::fs::read_to_string(path)?)?;
    Ok((
        saved["server"]
            .as_str()
            .ok_or("no server in session.json")?
            .to_string(),
        saved["token"]
            .as_str()
            .ok_or("no token in session.json")?
            .to_string(),
    ))
}

fn matras_app_id() -> &'static str {
    "ru.toxblh.Matras"
}
