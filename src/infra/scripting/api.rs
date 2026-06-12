use std::rc::Rc;

use mlua::{Lua, LuaSerdeExt, Value};
use tokio::sync::mpsc::UnboundedSender;

use crate::core::plugin_api::{self, PluginPopup, PopupLine};
use crate::core::user_config::parse_theme_item;

use super::effects::ScriptEffect;
use super::events::VALID_EVENT_NAMES;
use super::shared::{
  HttpResponseData, HttpResult, ScriptShared, COMMANDS_KEY, HANDLERS_KEY, HTTP_CALLBACKS_KEY,
};

/// Build the `spotatui` global table and its functions.
pub(super) fn install_api(
  lua: &Lua,
  shared: &Rc<ScriptShared>,
  http_tx: UnboundedSender<HttpResult>,
  http_client: reqwest::Client,
  rt_handle: Option<tokio::runtime::Handle>,
) -> mlua::Result<()> {
  let tbl = lua.create_table()?;

  tbl.set("api_version", plugin_api::API_VERSION)?;

  // spotatui.require_api(n): assert this build is new enough for the plugin.
  {
    let require_api = lua.create_function(move |_, n: i64| {
      if n < 1 {
        return Err(mlua::Error::RuntimeError(format!(
          "spotatui.require_api: version must be a positive integer, got {n}"
        )));
      }
      let n = n as u32;
      if n > plugin_api::API_VERSION {
        return Err(mlua::Error::RuntimeError(format!(
          "requires spotatui scripting API v{n} (this build provides v{}); update spotatui to use this plugin",
          plugin_api::API_VERSION
        )));
      }
      Ok(())
    })?;
    tbl.set("require_api", require_api)?;
  }

  // spotatui.on(event, fn)
  {
    let lua_inner = lua.clone();
    let shared = shared.clone();
    let on = lua.create_function(move |_, (event, callback): (String, mlua::Function)| {
      if !VALID_EVENT_NAMES.contains(&event.as_str()) {
        return Err(mlua::Error::RuntimeError(format!(
          "spotatui.on: unknown event '{event}'; valid events: {}",
          VALID_EVENT_NAMES.join(", ")
        )));
      }
      let handlers: mlua::Table = lua_inner.named_registry_value(HANDLERS_KEY)?;
      let list: mlua::Table = match handlers.get::<Option<mlua::Table>>(event.clone())? {
        Some(t) => t,
        None => {
          let t = lua_inner.create_table()?;
          handlers.set(event.clone(), t.clone())?;
          t
        }
      };
      let entry = lua_inner.create_table()?;
      entry.set("plugin", shared.current_plugin.borrow().clone())?;
      entry.set("callback", callback)?;
      list.push(entry)?;
      Ok(())
    })?;
    tbl.set("on", on)?;
  }

  // Reads: spotatui.playback() / current_track() / devices()
  {
    let shared_pb = shared.clone();
    let playback = lua.create_function(move |lua, ()| {
      let pb = shared_pb.playback.borrow().clone();
      match pb {
        Some(state) => lua.to_value(&state),
        None => Ok(Value::Nil),
      }
    })?;
    tbl.set("playback", playback)?;

    let shared_ct = shared.clone();
    let current_track = lua.create_function(move |lua, ()| {
      let pb = shared_ct.playback.borrow().clone();
      match pb.and_then(|s| s.track) {
        Some(track) => lua.to_value(&track),
        None => Ok(Value::Nil),
      }
    })?;
    tbl.set("current_track", current_track)?;

    let shared_dev = shared.clone();
    let devices = lua.create_function(move |lua, ()| {
      let devices = shared_dev.devices.borrow().clone();
      lua.to_value(&devices)
    })?;
    tbl.set("devices", devices)?;
  }

  // Actions: queue effects.
  install_action(lua, &tbl, shared, "play", || ScriptEffect::Play)?;
  install_action(lua, &tbl, shared, "pause", || ScriptEffect::Pause)?;
  install_action(lua, &tbl, shared, "next", || ScriptEffect::Next)?;
  install_action(lua, &tbl, shared, "previous", || ScriptEffect::Previous)?;

  {
    let shared = shared.clone();
    let seek = lua.create_function(move |_, ms: u32| {
      shared.effects.borrow_mut().push(ScriptEffect::Seek(ms));
      Ok(())
    })?;
    tbl.set("seek", seek)?;
  }

  {
    let shared = shared.clone();
    let set_volume = lua.create_function(move |_, pct: i64| {
      let clamped = pct.clamp(0, 100) as u8;
      shared
        .effects
        .borrow_mut()
        .push(ScriptEffect::SetVolume(clamped));
      Ok(())
    })?;
    tbl.set("set_volume", set_volume)?;
  }

  {
    let shared = shared.clone();
    let shuffle = lua.create_function(move |_, on: bool| {
      shared
        .effects
        .borrow_mut()
        .push(ScriptEffect::SetShuffle(on));
      Ok(())
    })?;
    tbl.set("shuffle", shuffle)?;
  }

  {
    let shared = shared.clone();
    let search = lua.create_function(move |_, query: String| {
      shared
        .effects
        .borrow_mut()
        .push(ScriptEffect::Search(query));
      Ok(())
    })?;
    tbl.set("search", search)?;
  }

  {
    let shared = shared.clone();
    let notify = lua.create_function(move |_, (msg, ttl): (String, Option<u64>)| {
      shared
        .effects
        .borrow_mut()
        .push(ScriptEffect::Notify(msg, ttl.unwrap_or(4)));
      Ok(())
    })?;
    tbl.set("notify", notify)?;
  }

  {
    let log = lua.create_function(move |_, msg: String| {
      log::info!("[lua] {msg}");
      Ok(())
    })?;
    tbl.set("log", log)?;
  }

  {
    let json_decode = lua.create_function(move |lua, json: String| {
      let value: serde_json::Value = serde_json::from_str(&json).map_err(mlua::Error::external)?;
      lua.to_value(&value)
    })?;
    tbl.set("json_decode", json_decode)?;

    let json_encode = lua.create_function(move |lua, value: Value| {
      let value: serde_json::Value = lua.from_value(value)?;
      serde_json::to_string(&value).map_err(mlua::Error::external)
    })?;
    tbl.set("json_encode", json_encode)?;
  }

  // Async HTTP: request tasks send results back to the engine, which owns the Lua state.
  {
    let lua_inner = lua.clone();
    let shared = shared.clone();
    let tx = http_tx.clone();
    let client = http_client.clone();
    let handle = rt_handle.clone();
    let http_get = lua.create_function(move |_, (url, callback): (String, mlua::Function)| {
      validate_http_url("spotatui.http_get", &url)?;
      let handle = handle.clone().ok_or_else(|| {
        mlua::Error::RuntimeError("spotatui.http_get: no tokio runtime available".to_string())
      })?;
      let token = register_http_callback(&lua_inner, &shared, callback)?;
      let client = client.clone();
      let tx = tx.clone();
      handle.spawn(async move {
        let result = run_http_get(client, url).await;
        let _ = tx.send((token, result));
      });
      Ok(())
    })?;
    tbl.set("http_get", http_get)?;
  }

  {
    let lua_inner = lua.clone();
    let shared = shared.clone();
    let tx = http_tx.clone();
    let client = http_client.clone();
    let handle = rt_handle.clone();
    let http_post = lua.create_function(
      move |_,
            (url, body, headers, callback): (
        String,
        String,
        Option<mlua::Table>,
        mlua::Function,
      )| {
        validate_http_url("spotatui.http_post", &url)?;
        let handle = handle.clone().ok_or_else(|| {
          mlua::Error::RuntimeError("spotatui.http_post: no tokio runtime available".to_string())
        })?;
        let headers = collect_headers(headers)?;
        let token = register_http_callback(&lua_inner, &shared, callback)?;
        let client = client.clone();
        let tx = tx.clone();
        handle.spawn(async move {
          let result = run_http_post(client, url, body, headers).await;
          let _ = tx.send((token, result));
        });
        Ok(())
      },
    )?;
    tbl.set("http_post", http_post)?;
  }

  // spotatui.register_command(name, fn)
  {
    let lua_inner = lua.clone();
    let shared = shared.clone();
    let register_command =
      lua.create_function(move |_, (name, callback): (String, mlua::Function)| {
        if name.is_empty() || name.contains(char::is_whitespace) {
          return Err(mlua::Error::RuntimeError(
            "spotatui.register_command: name must be a non-empty string without whitespace"
              .to_string(),
          ));
        }
        let commands: mlua::Table = lua_inner.named_registry_value(COMMANDS_KEY)?;
        if commands.get::<Option<mlua::Table>>(name.clone())?.is_some() {
          return Err(mlua::Error::RuntimeError(format!(
            "spotatui.register_command: command '{name}' is already registered"
          )));
        }
        let entry = lua_inner.create_table()?;
        entry.set("plugin", shared.current_plugin.borrow().clone())?;
        entry.set("callback", callback)?;
        commands.set(name, entry)?;
        Ok(())
      })?;
    tbl.set("register_command", register_command)?;
  }

  // spotatui.set_playbar(text_or_nil)
  {
    let shared = shared.clone();
    let set_playbar = lua.create_function(move |_, text: Option<String>| {
      let plugin = shared.current_plugin.borrow().clone();
      shared
        .effects
        .borrow_mut()
        .push(ScriptEffect::SetPlaybarSegment { plugin, text });
      Ok(())
    })?;
    tbl.set("set_playbar", set_playbar)?;
  }

  // spotatui.popup(title, lines)
  {
    let shared = shared.clone();
    let popup = lua.create_function(move |_, (title, lines_val): (String, mlua::Value)| {
      let lines = parse_popup_lines(lines_val)?;
      shared
        .effects
        .borrow_mut()
        .push(ScriptEffect::ShowPopup(PluginPopup { title, lines }));
      Ok(())
    })?;
    tbl.set("popup", popup)?;
  }

  // spotatui.set_theme(tbl)
  {
    let shared = shared.clone();
    let set_theme = lua.create_function(move |_, tbl: mlua::Table| {
      let mut pairs: Vec<(String, ratatui::style::Color)> = Vec::new();
      for pair in tbl.pairs::<String, String>() {
        let (field, color_str) = pair?;
        // Validate field name
        const VALID_FIELDS: &[&str] = &[
          "active",
          "banner",
          "error_border",
          "error_text",
          "hint",
          "hovered",
          "inactive",
          "playbar_background",
          "playbar_progress",
          "playbar_progress_text",
          "playbar_text",
          "selected",
          "text",
          "background",
          "header",
          "highlighted_lyrics",
          "analysis_bar",
          "analysis_bar_text",
        ];
        if !VALID_FIELDS.contains(&field.as_str()) {
          return Err(mlua::Error::RuntimeError(format!(
            "spotatui.set_theme: unknown theme field '{field}'"
          )));
        }
        let color = parse_theme_item(&color_str).map_err(|e| {
          mlua::Error::RuntimeError(format!(
            "spotatui.set_theme: invalid color for field '{field}': {e}"
          ))
        })?;
        pairs.push((field, color));
      }
      shared
        .effects
        .borrow_mut()
        .push(ScriptEffect::SetTheme(pairs));
      Ok(())
    })?;
    tbl.set("set_theme", set_theme)?;
  }

  lua.globals().set("spotatui", tbl)?;
  Ok(())
}

fn validate_http_url(function_name: &str, url: &str) -> mlua::Result<()> {
  let parsed = reqwest::Url::parse(url)
    .map_err(|e| mlua::Error::RuntimeError(format!("{function_name}: invalid URL '{url}': {e}")))?;
  match parsed.scheme() {
    "http" | "https" => Ok(()),
    scheme => Err(mlua::Error::RuntimeError(format!(
      "{function_name}: unsupported URL scheme '{scheme}'"
    ))),
  }
}

fn register_http_callback(
  lua: &Lua,
  shared: &Rc<ScriptShared>,
  callback: mlua::Function,
) -> mlua::Result<u64> {
  let token = shared
    .next_http_token
    .get()
    .checked_add(1)
    .ok_or_else(|| mlua::Error::RuntimeError("spotatui.http: token overflow".to_string()))?;
  let key = i64::try_from(token)
    .map_err(|_| mlua::Error::RuntimeError("spotatui.http: token overflow".to_string()))?;
  shared.next_http_token.set(token);

  let callbacks: mlua::Table = lua.named_registry_value(HTTP_CALLBACKS_KEY)?;
  let entry = lua.create_table()?;
  entry.set("plugin", shared.current_plugin.borrow().clone())?;
  entry.set("callback", callback)?;
  callbacks.raw_set(key, entry)?;
  Ok(token)
}

fn collect_headers(headers: Option<mlua::Table>) -> mlua::Result<Vec<(String, String)>> {
  let Some(headers) = headers else {
    return Ok(Vec::new());
  };
  let mut out = Vec::new();
  for pair in headers.pairs::<String, String>() {
    out.push(pair?);
  }
  Ok(out)
}

async fn run_http_get(client: reqwest::Client, url: String) -> Result<HttpResponseData, String> {
  let response = client.get(url).send().await.map_err(|e| e.to_string())?;
  response_data(response).await
}

async fn run_http_post(
  client: reqwest::Client,
  url: String,
  body: String,
  headers: Vec<(String, String)>,
) -> Result<HttpResponseData, String> {
  let mut request = client.post(url).body(body);
  for (key, value) in headers {
    request = request.header(key, value);
  }
  let response = request.send().await.map_err(|e| e.to_string())?;
  response_data(response).await
}

async fn response_data(response: reqwest::Response) -> Result<HttpResponseData, String> {
  let status = response.status().as_u16();
  let bytes = response.bytes().await.map_err(|e| e.to_string())?;
  let body = String::from_utf8_lossy(&bytes).into_owned();
  Ok(HttpResponseData { status, body })
}

/// Parse the `lines` argument for `spotatui.popup`.
///
/// Accepts: a single string, or an array whose items are each a string or a table
/// `{ text, fg?, bold?, italic? }`.
fn parse_popup_lines(val: mlua::Value) -> mlua::Result<Vec<PopupLine>> {
  match val {
    mlua::Value::String(s) => Ok(vec![PopupLine {
      text: s.to_str()?.to_string(),
      fg: None,
      bold: false,
      italic: false,
    }]),
    mlua::Value::Table(tbl) => {
      let mut lines = Vec::new();
      for item in tbl.sequence_values::<mlua::Value>() {
        let item = item?;
        match item {
          mlua::Value::String(s) => lines.push(PopupLine {
            text: s.to_str()?.to_string(),
            fg: None,
            bold: false,
            italic: false,
          }),
          mlua::Value::Table(t) => {
            let text: Option<String> = t.get("text")?;
            let text = text.ok_or_else(|| {
              mlua::Error::RuntimeError(
                "spotatui.popup: each line table must have a 'text' field".to_string(),
              )
            })?;
            let fg_str: Option<String> = t.get("fg")?;
            let fg = fg_str
              .map(|s| {
                parse_theme_item(&s).map_err(|e| {
                  mlua::Error::RuntimeError(format!("spotatui.popup: invalid color '{}': {}", s, e))
                })
              })
              .transpose()?;
            let bold: bool = t.get::<Option<bool>>("bold")?.unwrap_or(false);
            let italic: bool = t.get::<Option<bool>>("italic")?.unwrap_or(false);
            lines.push(PopupLine {
              text,
              fg,
              bold,
              italic,
            });
          }
          other => {
            return Err(mlua::Error::RuntimeError(format!(
              "spotatui.popup: each line must be a string or table, got {}",
              other.type_name()
            )));
          }
        }
      }
      Ok(lines)
    }
    other => Err(mlua::Error::RuntimeError(format!(
      "spotatui.popup: lines must be a string or array, got {}",
      other.type_name()
    ))),
  }
}

/// Install a no-argument action that pushes a fixed effect.
pub(super) fn install_action(
  lua: &Lua,
  tbl: &mlua::Table,
  shared: &Rc<ScriptShared>,
  name: &str,
  make: fn() -> ScriptEffect,
) -> mlua::Result<()> {
  let shared = shared.clone();
  let f = lua.create_function(move |_, ()| {
    shared.effects.borrow_mut().push(make());
    Ok(())
  })?;
  tbl.set(name, f)?;
  Ok(())
}
