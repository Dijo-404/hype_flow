pub mod audio;
pub mod backend;
pub mod cleanup;
pub mod config;
pub mod controller;
pub mod feedback;
pub mod hook;
pub mod output;
pub mod process;
pub mod segments;
pub mod setup;
pub mod shortcuts;
pub mod vocabulary;

#[cfg(feature = "desktop")]
pub mod app;
#[cfg(feature = "desktop")]
pub mod ui;
