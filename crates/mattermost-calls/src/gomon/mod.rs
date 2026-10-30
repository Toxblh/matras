//! gomon (`ru.corp.comms`): calls through a Mattermost plugin and LiveKit.
//! The protocol is written up in `docs/GOMON.md`.

pub mod api;
mod proto;
mod session;

pub use session::{GomonJoin, GomonSession};
