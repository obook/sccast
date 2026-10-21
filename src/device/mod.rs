//! device/
//! Everything on the TV side: find the Cast devices (discovery.rs), start and close their
//! receiver app (receiver.rs), and play a stream on it (playback.rs, media.rs).
//!
//! Author: O. Booklage
//! Date: October 2026
//! Licence: MIT

pub mod discovery;
mod media;
mod playback;
mod receiver;

pub use discovery::CastDevice;
pub use media::Report;
pub use playback::{Drops, RECONNECT_DELAY};
pub use receiver::{launch, stop, Receiver, DEFAULT_MEDIA_RECEIVER, SCCAST_RECEIVER};
