use crate::core::app::App;
use crate::core::plugin_api::{self, PluginPopup};
use crate::infra::network::IoEvent;

/// An action queued by a plugin, drained by the runner while holding `&mut App`.
///
/// Each variant routes through the same `App` methods as the equivalent keybinding,
/// so native-streaming fast paths and throttling/coalescing are automatically honoured.
/// Tests inspect effects via pattern matching (no `derive` needed).
pub(crate) enum ScriptEffect {
  Play,
  Pause,
  Next,
  Previous,
  Seek(u32),
  SetVolume(u8),
  SetShuffle(bool),
  /// Resolved at drain time (country lookup needs `App`).
  Search(String),
  /// message, ttl_secs
  Notify(String, u64),
  /// Error message, ttl_secs -- always shown; blocks normal message overwrites until it expires.
  NotifyError(String, u64),
  /// Set or clear a playbar segment for a plugin (keyed by plugin name).
  SetPlaybarSegment {
    plugin: String,
    text: Option<String>,
  },
  /// Show a plugin popup dialog.
  ShowPopup(PluginPopup),
  /// Apply theme color overrides at runtime (field name -> color).
  SetTheme(Vec<(String, ratatui::style::Color)>),
}

/// Returns `true` when the current playback state indicates active playback.
pub(super) fn effective_is_playing(app: &App) -> bool {
  plugin_api::playback_state(app)
    .map(|p| p.is_playing)
    .unwrap_or(false)
}

/// Drain queued effects into the app while holding `&mut App`.
pub(super) fn apply_effects(effects: Vec<ScriptEffect>, app: &mut App) {
  for effect in effects {
    match effect {
      ScriptEffect::Play => {
        if !effective_is_playing(app) {
          app.toggle_playback();
        }
      }
      ScriptEffect::Pause => {
        if effective_is_playing(app) {
          app.toggle_playback();
        }
      }
      ScriptEffect::Next => app.next_track(),
      ScriptEffect::Previous => app.previous_track(),
      ScriptEffect::Seek(ms) => app.seek_to(ms),
      ScriptEffect::SetVolume(v) => app.set_volume_percent(v),
      ScriptEffect::SetShuffle(desired) => {
        let current = plugin_api::playback_state(app)
          .map(|p| p.shuffle)
          .unwrap_or(false);
        if current != desired {
          app.shuffle();
        }
      }
      ScriptEffect::Search(query) => {
        let country = app.get_user_country();
        app.dispatch(IoEvent::GetSearchResults(query, country));
      }
      ScriptEffect::Notify(msg, ttl) => app.set_status_message(msg, ttl),
      ScriptEffect::NotifyError(msg, ttl) => app.set_error_status_message(msg, ttl),
      ScriptEffect::SetPlaybarSegment { plugin, text } => match text {
        Some(t) => {
          app.plugin_playbar_segments.insert(plugin, t);
        }
        None => {
          app.plugin_playbar_segments.remove(&plugin);
        }
      },
      ScriptEffect::ShowPopup(popup) => {
        app.plugin_popup = Some(popup);
        app.plugin_popup_scroll = 0;
      }
      ScriptEffect::SetTheme(pairs) => {
        for (field, color) in pairs {
          match field.as_str() {
            "active" => app.user_config.theme.active = color,
            "banner" => app.user_config.theme.banner = color,
            "error_border" => app.user_config.theme.error_border = color,
            "error_text" => app.user_config.theme.error_text = color,
            "hint" => app.user_config.theme.hint = color,
            "hovered" => app.user_config.theme.hovered = color,
            "inactive" => app.user_config.theme.inactive = color,
            "playbar_background" => app.user_config.theme.playbar_background = color,
            "playbar_progress" => app.user_config.theme.playbar_progress = color,
            "playbar_progress_text" => app.user_config.theme.playbar_progress_text = color,
            "playbar_text" => app.user_config.theme.playbar_text = color,
            "selected" => app.user_config.theme.selected = color,
            "text" => app.user_config.theme.text = color,
            "background" => app.user_config.theme.background = color,
            "header" => app.user_config.theme.header = color,
            "highlighted_lyrics" => app.user_config.theme.highlighted_lyrics = color,
            "analysis_bar" => app.user_config.theme.analysis_bar = color,
            "analysis_bar_text" => app.user_config.theme.analysis_bar_text = color,
            _ => {} // unknown fields were rejected at the API layer
          }
        }
      }
    }
  }
}
