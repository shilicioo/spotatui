use std::panic::{catch_unwind, AssertUnwindSafe};
use std::path::Path;
use std::rc::Rc;

use mlua::{Lua, LuaSerdeExt, Value};

use crate::core::app::App;
use crate::core::plugin_api;

use super::api::install_api;
use super::effects::{apply_effects, ScriptEffect};
use super::events::{diff_events, queue_uris, ScriptEvent};
use super::shared::{ScriptShared, COMMANDS_KEY, HANDLERS_KEY};

pub struct ScriptEngine {
  pub(super) lua: Lua,
  pub(crate) shared: Rc<ScriptShared>,
  /// Previous playback snapshot, for diffing on tick.
  last_playback: Option<crate::core::plugin_api::PlaybackState>,
  /// Previous queue item uris, for diffing on tick.
  last_queue: Option<Vec<String>>,
}

impl ScriptEngine {
  /// Build the Lua state and install the `spotatui` global table.
  pub fn new() -> mlua::Result<Self> {
    let lua = Lua::new();
    let shared = Rc::new(ScriptShared::new());

    // Registry handler table: { event_name = { {plugin=, callback=}, ... } }.
    let handlers = lua.create_table()?;
    lua.set_named_registry_value(HANDLERS_KEY, handlers)?;

    // Registry commands table: { command_name = { plugin=, callback= } }.
    let commands = lua.create_table()?;
    lua.set_named_registry_value(COMMANDS_KEY, commands)?;

    install_api(&lua, &shared)?;

    Ok(ScriptEngine {
      lua,
      shared,
      last_playback: None,
      last_queue: None,
    })
  }

  /// Load `init.lua` then `plugins/*.lua` (sorted by filename). Missing files/dir are fine.
  /// A failing file logs an error and queues a Notify effect but never aborts the others.
  /// Returns the number of files loaded successfully.
  pub fn load_user_scripts(&mut self, config_dir: &Path) -> usize {
    let mut loaded = 0;

    let init_path = config_dir.join("init.lua");
    if init_path.is_file() && self.load_file(&init_path, "init.lua") {
      loaded += 1;
    }

    let plugins_dir = config_dir.join("plugins");
    if plugins_dir.is_dir() {
      let mut files: Vec<_> = std::fs::read_dir(&plugins_dir)
        .into_iter()
        .flatten()
        .flatten()
        .map(|e| e.path())
        .filter(|p| p.extension().and_then(|e| e.to_str()) == Some("lua"))
        .collect();
      files.sort();
      for path in files {
        let name = path
          .file_name()
          .and_then(|n| n.to_str())
          .unwrap_or("plugin")
          .to_string();
        if self.load_file(&path, &name) {
          loaded += 1;
        }
      }
    }

    loaded
  }

  /// Read and load a single file. Returns true on success. On any failure logs and queues a
  /// Notify effect, returning false.
  fn load_file(&mut self, path: &Path, name: &str) -> bool {
    let source = match std::fs::read_to_string(path) {
      Ok(s) => s,
      Err(e) => {
        log::error!("[lua] failed to read {}: {}", path.display(), e);
        self
          .shared
          .effects
          .borrow_mut()
          .push(ScriptEffect::NotifyError(
            format!("plugin '{name}' failed to load: {e}"),
            6,
          ));
        return false;
      }
    };
    match self.load_source(name, &source) {
      Ok(()) => true,
      Err(e) => {
        let fl = first_line(&e.to_string());
        log::error!("[lua] failed to load plugin '{name}': {e}");
        self
          .shared
          .effects
          .borrow_mut()
          .push(ScriptEffect::NotifyError(
            format!("plugin '{name}' failed to load: {fl}"),
            6,
          ));
        false
      }
    }
  }

  /// Execute a Lua chunk under the given plugin name (used as the chunk name for tracebacks).
  /// Public for tests.
  pub fn load_source(&mut self, plugin_name: &str, source: &str) -> mlua::Result<()> {
    *self.shared.current_plugin.borrow_mut() = plugin_name.to_string();
    let result = self
      .lua
      .load(source)
      .set_name(plugin_name.to_string())
      .exec();
    self.shared.current_plugin.borrow_mut().clear();
    result
  }

  /// Refresh caches, emit Start, drain effects.
  pub fn on_start(&mut self, app: &mut App) {
    self.refresh_caches(app);
    self.emit(ScriptEvent::Start);
    self.drain_effects(app);
  }

  /// On tick: if there are no handlers at all, return cheaply. Otherwise refresh caches,
  /// diff against the previous snapshot, emit each derived event, then drain.
  pub fn on_tick(&mut self, app: &mut App) {
    if !self.has_any_handlers() {
      return;
    }

    let new_playback = plugin_api::playback_state(app);
    let new_queue = Some(queue_uris(app));

    let events = diff_events(
      &self.last_playback,
      &self.last_queue,
      &new_playback,
      &new_queue,
    );

    *self.shared.playback.borrow_mut() = new_playback.clone();
    *self.shared.devices.borrow_mut() = plugin_api::device_list(app);

    self.last_playback = new_playback;
    self.last_queue = new_queue;

    for ev in events {
      self.emit(ev);
    }
    self.drain_effects(app);
  }

  /// Emit Quit, drain effects.
  pub fn on_quit(&mut self, app: &mut App) {
    self.refresh_caches(app);
    self.emit(ScriptEvent::Quit);
    self.drain_effects(app);
  }

  fn refresh_caches(&mut self, app: &App) {
    let pb = plugin_api::playback_state(app);
    *self.shared.playback.borrow_mut() = pb.clone();
    *self.shared.devices.borrow_mut() = plugin_api::device_list(app);
    self.last_playback = pb;
    self.last_queue = Some(queue_uris(app));
  }

  fn has_any_handlers(&self) -> bool {
    let handlers: mlua::Table = match self.lua.named_registry_value(HANDLERS_KEY) {
      Ok(t) => t,
      Err(_) => return false,
    };
    handlers
      .pairs::<String, mlua::Table>()
      .any(|p| p.map(|(_, list)| list.raw_len() > 0).unwrap_or(false))
  }

  /// Invoke every registered callback for `event`. Lua errors and caught panics disable the
  /// offending callback (one strike) and queue a Notify effect.
  pub(crate) fn emit(&mut self, event: ScriptEvent) {
    let handlers: mlua::Table = match self.lua.named_registry_value(HANDLERS_KEY) {
      Ok(t) => t,
      Err(_) => return,
    };
    let list: mlua::Table = match handlers.get(event.lua_name()) {
      Ok(t) => t,
      Err(_) => return,
    };

    let len = list.raw_len();
    if len == 0 {
      return;
    }

    let arg = if event.passes_playback_arg() {
      self.playback_value()
    } else {
      Value::Nil
    };

    // Indices to remove after the pass (descending so removal stays valid).
    let mut to_remove: Vec<usize> = Vec::new();

    for idx in 1..=len {
      let entry: mlua::Table = match list.get(idx) {
        Ok(t) => t,
        Err(_) => continue,
      };
      let plugin: String = entry.get("plugin").unwrap_or_default();
      let callback: mlua::Function = match entry.get("callback") {
        Ok(f) => f,
        Err(_) => continue,
      };

      let arg = arg.clone();
      *self.shared.current_plugin.borrow_mut() = plugin.clone();
      let call_result = catch_unwind(AssertUnwindSafe(|| callback.call::<()>(arg)));
      self.shared.current_plugin.borrow_mut().clear();

      let err_msg = match call_result {
        Ok(Ok(())) => None,
        Ok(Err(e)) => Some(first_line(&e.to_string())),
        Err(_) => Some("panic".to_string()),
      };

      if let Some(msg) = err_msg {
        log::error!(
          "[lua] plugin '{plugin}': error in on_{}: {msg}",
          event.lua_name()
        );
        self
          .shared
          .effects
          .borrow_mut()
          .push(ScriptEffect::NotifyError(
            format!("plugin '{plugin}': error in on_{}: {msg}", event.lua_name()),
            6,
          ));
        to_remove.push(idx);
      }
    }

    for idx in to_remove.into_iter().rev() {
      let _ = list.raw_remove(idx);
    }
  }

  /// Serialize the cached playback snapshot into a Lua value (table or nil).
  fn playback_value(&self) -> Value {
    let pb = self.shared.playback.borrow().clone();
    match pb {
      Some(state) => self.lua.to_value(&state).unwrap_or(Value::Nil),
      None => Value::Nil,
    }
  }

  /// Run any commands queued in `app.pending_plugin_commands`, then drain effects.
  pub fn run_pending_commands(&mut self, app: &mut App) {
    if app.pending_plugin_commands.is_empty() {
      return;
    }
    self.refresh_caches(app);
    let names: Vec<String> = app.pending_plugin_commands.drain(..).collect();
    let commands: mlua::Table = match self.lua.named_registry_value(COMMANDS_KEY) {
      Ok(t) => t,
      Err(_) => {
        self.drain_effects(app);
        return;
      }
    };
    for name in names {
      let entry: mlua::Table = match commands.get::<Option<mlua::Table>>(name.clone()) {
        Ok(Some(t)) => t,
        _ => {
          self
            .shared
            .effects
            .borrow_mut()
            .push(ScriptEffect::NotifyError(
              format!("no plugin command named '{name}'"),
              6,
            ));
          continue;
        }
      };
      let plugin: String = entry.get("plugin").unwrap_or_default();
      let callback: mlua::Function = match entry.get("callback") {
        Ok(f) => f,
        Err(_) => continue,
      };
      *self.shared.current_plugin.borrow_mut() = plugin.clone();
      let call_result = catch_unwind(AssertUnwindSafe(|| callback.call::<()>(())));
      self.shared.current_plugin.borrow_mut().clear();
      match call_result {
        Ok(Ok(())) => {}
        Ok(Err(e)) => {
          let msg = first_line(&e.to_string());
          log::error!("[lua] plugin '{plugin}': error in command '{name}': {msg}");
          self
            .shared
            .effects
            .borrow_mut()
            .push(ScriptEffect::NotifyError(
              format!("plugin '{plugin}': error in command '{name}': {msg}"),
              6,
            ));
        }
        Err(_) => {
          log::error!("[lua] plugin '{plugin}': panic in command '{name}'");
          self
            .shared
            .effects
            .borrow_mut()
            .push(ScriptEffect::NotifyError(
              format!("plugin '{plugin}': panic in command '{name}'"),
              6,
            ));
        }
      }
    }
    self.drain_effects(app);
  }

  /// Drain queued effects into the app while holding `&mut App`.
  pub(crate) fn drain_effects(&self, app: &mut App) {
    let effects: Vec<ScriptEffect> = self.shared.effects.borrow_mut().drain(..).collect();
    apply_effects(effects, app);
  }
}

/// First line of an error string (Lua tracebacks are multi-line).
fn first_line(s: &str) -> String {
  s.lines().next().unwrap_or(s).trim().to_string()
}
