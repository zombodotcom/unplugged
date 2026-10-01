//! Unplugged: a free, open-source DAW for recording at home.
//!
//! The GUI lives in `app`; `share` is a standalone social-media exporter that the
//! `unplugged-share` command-line tool also uses.

pub mod app;
pub mod audio;
pub mod dsp;
pub mod engine;
pub mod fx;
pub mod model;
pub mod plugins;
pub mod project;
pub mod share;
