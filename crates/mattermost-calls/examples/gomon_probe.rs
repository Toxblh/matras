//! Starts (or joins) a gomon call, publishes silence for a few seconds and
//! leaves, printing what it sees. Protocol check without the UI:
//!
//! MM_URL=https://mm.example MM_TOKEN=… MM_CHANNEL=<channel id> \
//!     cargo run -p mattermost-calls --example gomon_probe

use std::time::Duration;

use mattermost_calls::gomon::{api, GomonJoin, GomonSession};
use mattermost_calls::{CallUpdate, Sample};

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    tracing_subscriber_init();
    let var = |k: &str| std::env::var(k).map_err(|_| format!("{k} is not set"));
    let client = mattermost_api::Client::new(&var("MM_URL")?)?;
    client.set_token(var("MM_TOKEN")?);
    let channel_id = var("MM_CHANNEL")?;

    println!("gomon available: {}", api::available(&client).await?);
    let started = api::start(&client, &channel_id, false).await?;
    println!("call {} (reused: {})", started.call_id, started.reused);

    let session = GomonSession::join(GomonJoin {
        channel_id,
        join_url: started.join_url,
        ..Default::default()
    })
    .await?;
    let mut updates = session.subscribe();
    tokio::spawn(async move {
        while let Ok(update) = updates.recv().await {
            match update {
                CallUpdate::State(state) => println!(
                    "roster: {:?}",
                    state
                        .sessions
                        .iter()
                        .map(|s| (&s.session_id, s.unmuted))
                        .collect::<Vec<_>>()
                ),
                CallUpdate::RemoteTrack {
                    session_id,
                    track_type,
                    ..
                } => {
                    println!("remote {track_type} from {session_id}")
                }
                other => println!("{other:?}"),
            }
        }
    });

    let track = session.unmute().await?;
    println!("microphone published");
    // Opus silence frame, 20 ms.
    for _ in 0..250 {
        track
            .write_sample(&Sample {
                data: vec![0xf8, 0xff, 0xfe].into(),
                duration: Duration::from_millis(20),
                ..Default::default()
            })
            .await?;
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    session.leave().await?;
    println!("left");
    tokio::time::sleep(Duration::from_millis(500)).await;
    Ok(())
}

fn tracing_subscriber_init() {
    tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::from_default_env())
        .init();
}
