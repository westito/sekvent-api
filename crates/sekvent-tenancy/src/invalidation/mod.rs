//! The cross-process invalidation channel: messages, the sender, the listener and its resync.

mod listener;
mod message;
mod sender;

pub use listener::{Listener, Sink, listen};
pub use message::{ChangeKind, Channel, Message, Topic};
pub use sender::Sender;
