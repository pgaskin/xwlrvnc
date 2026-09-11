//! The X11↔Wayland bridge: the fake X server, the Wayland client, and the shared
//! state they exchange.
//!
//! Everything that actually translates between the two protocols lives here; the
//! crate root is just the runtime harness (CLI, logging, process management,
//! X-display socket binding, and starting the Wayland connection).

pub mod capture;
pub mod clipboard;
pub mod clipjobs;
pub mod cursor;
pub mod damage;
pub mod dynres;
pub mod event;
pub mod input;
pub mod keymap;
pub mod profile;
mod server;
pub mod wayland;
pub mod x11;

/// The shared state both protocol sides exchange (see [`server`]).
pub(crate) use server::Server;
