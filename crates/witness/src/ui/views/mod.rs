//! The five views plus the transcript's player bar and the Settings →
//! Updates section. Each view is a struct remounted when the view key
//! changes (`ui::View::key`); its loads are view-owned `Task`s and its
//! actions go through `Tasks::spawn_op`.

pub mod ai_settings;
pub mod ai_summary;
pub mod meetings;
pub mod people;
pub mod player_bar;
pub mod search;
pub mod settings;
pub mod transcript;
pub mod updates;
