//! In-process agent scheduling + delivery for harness-rs. Optional — nothing
//! else in the framework depends on it.

pub mod channel;
// Named for what it is, inside a module named for the crate it used to
// be. Renaming either would move a public path for no gain.
#[allow(clippy::module_inception)]
pub mod scheduler;
pub mod store;
pub mod tool;

pub use channel::*;
pub use scheduler::*;
pub use store::*;
pub use tool::*;
