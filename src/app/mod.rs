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
mod theme;
// Nothing draws from the view model until the window is rebuilt on it.
#[allow(dead_code)]
mod view;
pub mod tray;
pub mod ui;
pub mod updates;
mod widgets;
