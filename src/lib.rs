//! Unplugged: play electric guitar DI and hear it as an acoustic.
//!
//! The GUI lives in `app`; `share` is a standalone social-media exporter that the
//! `unplugged-share` command-line tool also uses.

pub mod app;
pub mod audio;
pub mod dsp;
pub mod engine;
pub mod model;
pub mod project;
pub mod share;
