//! Reading what Claude Code leaves on disk. Read-only: nothing here writes into a config dir
//! (the vault does that, explicitly).

pub mod config_json;
pub mod credentials;
pub mod files;
pub mod iterm2;
pub mod kitty;
pub mod pane;
pub mod proc_env;
pub mod procs;
pub mod project;
pub mod registry;
pub mod seats;
pub mod tmux;
pub mod transcript;
pub mod wezterm;
pub mod zellij;
