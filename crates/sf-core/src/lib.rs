//! Pure game logic — no I/O, no async, no rendering.
//! Compiled into both client and server so rules can never drift.

pub mod board;
pub mod combat;
pub mod data;
pub mod game;
pub mod geometry;
pub mod maneuver;
pub mod rules;
pub mod ship;
pub mod templates;
