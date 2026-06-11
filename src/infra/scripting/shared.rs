use std::cell::{Cell, RefCell};

use crate::core::plugin_api::{DeviceInfo, PlaybackState};

use super::effects::ScriptEffect;

/// Registry key for the table mapping event name -> array of `{ plugin, callback }`.
pub(super) const HANDLERS_KEY: &str = "spotatui.handlers";

/// Registry key for the table mapping command name -> `{ plugin, callback }`.
pub(super) const COMMANDS_KEY: &str = "spotatui.commands";

/// Registry key for the table mapping HTTP token -> `{ plugin, callback }`.
pub(super) const HTTP_CALLBACKS_KEY: &str = "spotatui.http_callbacks";

pub(super) type HttpResult = (u64, Result<HttpResponseData, String>);

pub(super) struct HttpResponseData {
  pub(super) status: u16,
  pub(super) body: String,
}

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
  pub(super) next_http_token: Cell<u64>,
}

impl ScriptShared {
  pub(super) fn new() -> Self {
    ScriptShared {
      playback: RefCell::new(None),
      devices: RefCell::new(Vec::new()),
      effects: RefCell::new(Vec::new()),
      current_plugin: RefCell::new(String::new()),
      next_http_token: Cell::new(0),
    }
  }
}
