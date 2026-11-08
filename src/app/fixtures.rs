//! The snapshots the window's tests draw and check.

use crate::app::model::{DeviceStatus, LayoutState, LinkStatus, ModeChange, Snapshot};
use crate::app::updates::UpdateStatus;
use crate::platform::{DisplayInfo, DisplayMode};

pub const STATE: &str = r#"{"config":{"devices":[{"node":"Debian","x":0,"y":0,"monitors":[{"name":"DP-3 (240Hz Native)","x":0,"y":360,"w":1920,"h":1080},{"name":"DP-2 (360Hz Native)","x":1920,"y":0,"w":2560,"h":1440}]},{"node":"Windows","x":1920,"y":1440,"monitors":[{"name":"Display 1","x":0,"y":0,"w":2560,"h":1600}]}],"attach_side":"Bottom","edge_resistance":55,"auto_game_lock":true,"manual_game_lock":false},"telemetry":{"active_node":"Debian","peer_online":true}}"#;

pub fn device(node: &str, tx_source: Option<u32>) -> DeviceStatus {
    let mut d: DeviceStatus = serde_json::from_str(&format!(
        r#"{{"node":"{node}","hostname":"{node}-host","ip":"192.168.8.2","peer_online":true,"active_node":"Debian","cursor":[10,20]}}"#
    ))
    .unwrap();
    d.sharing = Some(Default::default());
    d.audio.output = if tx_source.is_some() { "speakers".into() } else { String::new() };
    d.audio.outputs = vec![crate::app::model::AudioDevice { id: "speakers".into(), name: "Speakers".into() }];
    d.audio.tx_volume = 100;
    d.audio.rx_volume = 100;
    d.audio.tx_peak = 60;
    d.audio.play_only_on_other = Some(true);
    d
}

pub fn linked() -> Snapshot {
    Snapshot {
        local: Some(device("debian", Some(0))),
        peer: Some(device("windows", None)),
        layout: LayoutState::parse(STATE),
        ..Default::default()
    }
}

pub fn states() -> Vec<Snapshot> {
    let mut no_peer = linked();
    no_peer.peer = None;
    let laptop_alone = Snapshot { local: Some(device("windows", None)), layout: LayoutState::parse(STATE), ..Default::default() };
    let no_layout = Snapshot { local: Some(device("windows", None)), ..Default::default() };
    let mut split = linked();
    split.peer.as_mut().unwrap().main_device = 1;
    split.local.as_mut().unwrap().main_conflict = true;
    let mut older = linked();
    older.peer.as_mut().unwrap().sharing = None;
    older.peer.as_mut().unwrap().audio.play_only_on_other = None;
    let mut with_modes = linked();
    with_modes.local_displays = vec![display("DP-3"), display("DP-2")];
    with_modes.peer_displays = vec![display("Display 1")];
    let mut changing = with_modes.clone();
    changing.local.as_mut().unwrap().display_change.change = Some(waiting_change());
    let mut all = vec![Snapshot::default(), linked(), no_peer, laptop_alone, no_layout, split, older, with_modes, changing];
    all.extend(update_states());
    all
}

pub fn update(state: &str, ready: bool) -> UpdateStatus {
    UpdateStatus { current: "0.7.0".into(), available: Some("0.7.1".into()), ready, state: state.into(), progress: Some(45), ..Default::default() }
}

/// The window in each state the update line can be in.
pub fn update_states() -> Vec<Snapshot> {
    let with = |here: UpdateStatus, there: Option<UpdateStatus>| {
        let mut snapshot = linked();
        snapshot.local.as_mut().unwrap().update = Some(here);
        match there {
            Some(there) => snapshot.peer.as_mut().unwrap().update = Some(there),
            None => snapshot.peer = None,
        }
        snapshot
    };
    let ready = || update("ready", true);
    let mut failed = update("failed", true);
    failed.note = Some("A very long way of saying that SACBOOKPRO didn't come back on version 0.7.1 within 5 minutes, so this device stayed as it is.".into());
    vec![
        with(ready(), Some(ready())),
        with(ready(), None),
        with(ready(), Some(update("downloading", false))),
        with(update("updating_peer", true), Some(ready())),
        with(update("waiting_for_peer", true), None),
        with(update("installing", true), Some(ready())),
        with(update("view_only", false), Some(ready())),
        with(failed, Some(ready())),
    ]
}

pub fn mode(width: i32, height: i32, millihertz: u32) -> DisplayMode {
    DisplayMode { width, height, millihertz, id: String::new() }
}

pub fn display(id: &str) -> DisplayInfo {
    let modes = vec![mode(2560, 1440, 144_000), mode(1920, 1080, 60_000)];
    DisplayInfo { id: id.into(), name: format!("{id} model"), current: Some(modes[0].clone()), modes, can_change: true, ..Default::default() }
}

pub fn waiting_change() -> ModeChange {
    ModeChange {
        id: 7,
        output: "DP-2".into(),
        name: "Big".into(),
        from: Some(mode(2560, 1440, 144_000)),
        to: Some(mode(1920, 1080, 60_000)),
        stage: "waiting".into(),
        ms_left: 12_000,
    }
}

pub fn unpaired() -> Snapshot {
    let mut local = device("windows", None);
    local.paired = Some(false);
    local.ip = "192.168.8.241".into();
    Snapshot { local: Some(local), layout: LayoutState::parse(STATE), ..Default::default() }
}

pub fn secure(mut snapshot: Snapshot, secure: &str) -> Snapshot {
    snapshot.local.as_mut().unwrap().secure = secure.into();
    snapshot
}

/// The linked snapshot with the link measured at these figures.
pub fn link(mut snapshot: Snapshot, round_trip_ms: u32, loss: f32) -> Snapshot {
    snapshot.local.as_mut().unwrap().link = Some(LinkStatus {
        measured: true,
        round_trip_ms: Some(round_trip_ms),
        loss_in: Some(loss),
        loss_out: Some(loss),
        ..Default::default()
    });
    snapshot
}

/// The snapshot with the other device out of reach of the link.
pub fn link_down(mut snapshot: Snapshot) -> Snapshot {
    let local = snapshot.local.as_mut().unwrap();
    local.peer_online = false;
    local.link = Some(LinkStatus::default());
    if let Some(layout) = snapshot.layout.as_mut() {
        layout.telemetry.peer_online = false;
    }
    snapshot
}

/// The snapshot with audio arriving from the other device.
pub fn audio_live(mut snapshot: Snapshot, rx_loss: f32, rx_buffer_ms: u32) -> Snapshot {
    let audio = &mut snapshot.local.as_mut().unwrap().audio;
    audio.rx_live = true;
    audio.rx_loss = rx_loss;
    audio.rx_buffer_ms = rx_buffer_ms;
    snapshot
}
