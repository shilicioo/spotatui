use std::cell::RefCell;

use crate::core::plugin_api::{DeviceInfo, PlaybackState};

use super::effects::ScriptEffect;

/// Registry key for the table mapping event name -> array of `{ plugin, callback }`.
pub(super) const HANDLERS_KEY: &str = "spotatui.handlers";

/// Registry key for the table mapping command name -> `{ plugin, callback }`.
pub(super) const COMMANDS_KEY: &str = "spotatui.commands";

/// State shared between the engine and the Lua closures via `Rc`.
///
/// `mlua` is built without the `send` feature, so `Rc`/`RefCell` are fine here: everything
/// runs on the single UI task.
pub(crate) struct ScriptShared {
  /// Playback snapshot, refreshed by the runner before callbacks run.
  pub(crate) playback: RefCell<Option<PlaybackState>>,
  pub(super) devices: RefCell<Vec<DeviceInfo>>,
  pub(crate) effects: RefCell<Vec<ScriptEffect>>,
  /// Plugin name currently being loaded, so `spotatui.on` can tag its callbacks.
  pub(super) current_plugin: RefCell<String>,
}

impl ScriptShared {
  pub(super) fn new() -> Self {
    ScriptShared {
      playback: RefCell::new(None),
      devices: RefCell::new(Vec::new()),
      effects: RefCell::new(Vec::new()),
      current_plugin: RefCell::new(String::new()),
    }
  }
}
