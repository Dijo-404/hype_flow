pub mod audio;
pub mod backend;
pub mod cleanup;
pub mod config;
pub mod controller;
pub mod feedback;
pub mod hook;
mod itn;
pub mod output;
pub mod process;
pub mod recovery;
mod restarts;
pub mod segments;
pub mod setup;
pub mod shortcuts;
mod text_diff;
pub mod text_processing;
pub mod vocabulary;

#[cfg(feature = "desktop")]
pub mod app;
#[cfg(feature = "desktop")]
pub mod ui;
