pub mod client;
mod icon;
pub mod install;
pub mod launch;
pub mod model;
mod pairing;
pub mod single;
mod canvas;
#[cfg(test)]
mod fixtures;
mod panels;
// The settings table (Task 7) draws these rows; until then only tests read them.
#[allow(dead_code)]
mod rows;
mod sparkline;
mod theme;
mod top;
mod view;
pub mod tray;
pub mod ui;
pub mod updates;
mod widgets;
