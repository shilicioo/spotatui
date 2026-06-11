//! Plugin-facing domain facade.
//!
//! Serde-serializable snapshots with string IDs/URIs only. rspotify types must never leak
//! through this boundary — that is the compatibility contract for the future scripting API
//! and multi-source refactor. All conversions from rspotify types happen in the mapping
//! functions below; callers receive only the plain structs defined here.

// Nothing in the main binary calls this API yet; Phase 1 will wire it up.
#![allow(dead_code)]

use crate::core::app::App;
use crate::infra::media_metadata::current_playback_snapshot;
use rspotify::model::RepeatState;
use serde::{Deserialize, Serialize};

pub const API_VERSION: u32 = 2;

/// A popup dialog produced by a plugin.
#[derive(Debug, Clone, PartialEq)]
pub struct PluginPopup {
  pub title: String,
  pub lines: Vec<PopupLine>,
}

/// A single line in a [`PluginPopup`].
#[derive(Debug, Clone, PartialEq)]
pub struct PopupLine {
  pub text: String,
  pub fg: Option<ratatui::style::Color>,
  pub bold: bool,
  pub italic: bool,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct TrackInfo {
  pub uri: Option<String>,
  pub name: String,
  pub artists: Vec<String>,
  pub album: String,
  pub duration_ms: u64,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PlaybackState {
  pub track: Option<TrackInfo>,
  pub is_playing: bool,
  pub progress_ms: u64,
  pub shuffle: bool,
  /// One of `"off"`, `"track"`, or `"context"`.
  pub repeat: String,
  pub volume_percent: Option<u8>,
  pub device: Option<DeviceInfo>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct DeviceInfo {
  pub id: Option<String>,
  pub name: String,
  /// Lowercased DeviceType name, e.g. `"computer"`, `"smartphone"`, `"speaker"`.
  pub kind: String,
  pub is_active: bool,
  pub volume_percent: Option<u8>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PlaylistInfo {
  pub uri: String,
  pub name: String,
  pub owner: String,
  pub track_count: u32,
}

// ---------------------------------------------------------------------------
// Mapping helpers
// ---------------------------------------------------------------------------

impl PlaybackState {
  pub fn repeat_from(state: RepeatState) -> String {
    match state {
      RepeatState::Off => "off".to_string(),
      RepeatState::Track => "track".to_string(),
      RepeatState::Context => "context".to_string(),
    }
  }
}

impl DeviceInfo {
  pub fn from_rspotify(device: &rspotify::model::Device) -> Self {
    DeviceInfo {
      id: device.id.clone(),
      name: device.name.clone(),
      kind: format!("{:?}", device._type).to_lowercase(),
      is_active: device.is_active,
      volume_percent: device.volume_percent.map(|v| v.min(100) as u8),
    }
  }
}

impl PlaylistInfo {
  pub fn from_simplified(p: &rspotify::model::SimplifiedPlaylist) -> Self {
    use rspotify::prelude::Id;
    let owner = p
      .owner
      .display_name
      .clone()
      .unwrap_or_else(|| p.owner.id.id().to_string());
    PlaylistInfo {
      uri: p.id.uri(),
      name: p.name.clone(),
      owner,
      track_count: p.items.total,
    }
  }
}

/// Build a [`PlaybackState`] from the current [`App`] state.
///
/// Returns `None` only when there is no playback context at all (both
/// `current_playback_snapshot` and `app.current_playback_context` are absent).
pub fn playback_state(app: &App) -> Option<PlaybackState> {
  let snapshot = current_playback_snapshot(app);
  let context = app.current_playback_context.as_ref();

  if snapshot.is_none() && context.is_none() {
    return None;
  }

  let track = snapshot.as_ref().map(|s| TrackInfo {
    uri: s.item_uri.clone(),
    name: s.metadata.title.clone(),
    artists: s.metadata.artists.clone(),
    album: s.metadata.album.clone(),
    duration_ms: s.metadata.duration_ms as u64,
  });

  let (is_playing, shuffle, repeat, device) = if let Some(s) = &snapshot {
    let repeat_str = s
      .repeat
      .map(PlaybackState::repeat_from)
      .unwrap_or_else(|| "off".to_string());
    let device = context.map(|ctx| DeviceInfo::from_rspotify(&ctx.device));
    (s.is_playing, s.shuffle, repeat_str, device)
  } else {
    // snapshot is None but context is Some — build from context only
    let ctx = context.unwrap();
    let repeat_str = PlaybackState::repeat_from(ctx.repeat_state);
    let device = Some(DeviceInfo::from_rspotify(&ctx.device));
    (ctx.is_playing, ctx.shuffle_state, repeat_str, device)
  };

  let volume_percent = device.as_ref().and_then(|d| d.volume_percent);

  let progress_ms = snapshot.as_ref().map(|s| s.progress_ms as u64).unwrap_or(0);

  Some(PlaybackState {
    track,
    is_playing,
    progress_ms,
    shuffle,
    repeat,
    volume_percent,
    device,
  })
}

/// Return a list of available devices from [`App`]'s cached device payload.
pub fn device_list(app: &App) -> Vec<DeviceInfo> {
  app
    .devices
    .as_ref()
    .map(|payload| {
      payload
        .devices
        .iter()
        .map(DeviceInfo::from_rspotify)
        .collect()
    })
    .unwrap_or_default()
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
  use super::*;
  use chrono::Utc;
  use rspotify::model::{
    context::{Actions, CurrentPlaybackContext},
    CurrentlyPlayingType, Device, DeviceType, RepeatState,
  };
  use std::{sync::mpsc::channel, time::SystemTime};

  fn make_app() -> App {
    let (tx, _rx) = channel();
    App::new(
      tx,
      crate::core::user_config::UserConfig::new(),
      SystemTime::now(),
    )
  }

  #[allow(deprecated)]
  fn make_device(volume: u32) -> Device {
    Device {
      id: Some("dev-abc".to_string()),
      is_active: true,
      is_private_session: false,
      is_restricted: false,
      name: "Living Room TV".to_string(),
      _type: DeviceType::Tv,
      volume_percent: Some(volume),
    }
  }

  #[allow(deprecated)]
  fn make_playback_context_no_item(
    is_playing: bool,
    shuffle: bool,
    repeat: RepeatState,
    device: Device,
  ) -> CurrentPlaybackContext {
    CurrentPlaybackContext {
      device,
      repeat_state: repeat,
      shuffle_state: shuffle,
      context: None,
      timestamp: Utc::now(),
      progress: None,
      is_playing,
      item: None,
      currently_playing_type: CurrentlyPlayingType::Unknown,
      actions: Actions::default(),
    }
  }

  // --- DeviceInfo::from_rspotify ---

  #[test]
  fn device_info_maps_all_fields_and_lowercases_kind() {
    let d = make_device(75);
    let info = DeviceInfo::from_rspotify(&d);
    assert_eq!(info.id.as_deref(), Some("dev-abc"));
    assert_eq!(info.name, "Living Room TV");
    assert_eq!(info.kind, "tv");
    assert!(info.is_active);
    assert_eq!(info.volume_percent, Some(75));
  }

  #[test]
  fn device_info_computer_kind() {
    #[allow(deprecated)]
    let d = Device {
      id: None,
      is_active: false,
      is_private_session: false,
      is_restricted: false,
      name: "Laptop".to_string(),
      _type: DeviceType::Computer,
      volume_percent: Some(50),
    };
    let info = DeviceInfo::from_rspotify(&d);
    assert_eq!(info.kind, "computer");
    assert_eq!(info.volume_percent, Some(50));
    assert!(info.id.is_none());
    assert!(!info.is_active);
  }

  #[test]
  fn device_info_volume_clamped_to_u8() {
    // volume_percent is u32; values > 255 should not cause panic (min(100) ensures <= 100).
    #[allow(deprecated)]
    let d = Device {
      id: None,
      is_active: false,
      is_private_session: false,
      is_restricted: false,
      name: "X".to_string(),
      _type: DeviceType::Smartphone,
      volume_percent: Some(100),
    };
    let info = DeviceInfo::from_rspotify(&d);
    assert_eq!(info.volume_percent, Some(100));
  }

  // --- repeat_from ---

  #[test]
  fn repeat_off_maps_to_string() {
    assert_eq!(PlaybackState::repeat_from(RepeatState::Off), "off");
  }

  #[test]
  fn repeat_track_maps_to_string() {
    assert_eq!(PlaybackState::repeat_from(RepeatState::Track), "track");
  }

  #[test]
  fn repeat_context_maps_to_string() {
    assert_eq!(PlaybackState::repeat_from(RepeatState::Context), "context");
  }

  // --- playback_state ---

  #[test]
  fn playback_state_returns_none_on_default_app() {
    let app = make_app();
    assert!(playback_state(&app).is_none());
  }

  #[test]
  fn playback_state_with_context_no_item_returns_some_with_track_none() {
    let mut app = make_app();
    let device = make_device(60);
    app.current_playback_context = Some(make_playback_context_no_item(
      true,
      true,
      RepeatState::Context,
      device,
    ));

    let state = playback_state(&app).expect("should be Some");
    assert!(state.track.is_none());
    assert!(state.is_playing);
    assert!(state.shuffle);
    assert_eq!(state.repeat, "context");
    assert_eq!(state.volume_percent, Some(60));
    let dev = state.device.as_ref().expect("device should be present");
    assert_eq!(dev.id.as_deref(), Some("dev-abc"));
    assert_eq!(dev.name, "Living Room TV");
    assert_eq!(dev.kind, "tv");
  }

  // --- PlaylistInfo::from_simplified ---

  #[test]
  fn playlist_info_maps_all_fields() {
    let playlist = crate::core::test_helpers::simplified_playlist(
      "37i9dQZF1DXcBWIGoYBM5M",
      "Today's Top Hits",
      "spotify",
      false,
    );
    let info = PlaylistInfo::from_simplified(&playlist);
    assert_eq!(info.uri, "spotify:playlist:37i9dQZF1DXcBWIGoYBM5M");
    assert_eq!(info.name, "Today's Top Hits");
    // test_helpers::simplified_playlist sets owner display_name = owner_id
    assert_eq!(info.owner, "spotify");
    assert_eq!(info.track_count, 5);
  }

  #[test]
  fn playlist_info_falls_back_to_owner_id_when_no_display_name() {
    use rspotify::model::{
      idtypes::{PlaylistId, UserId},
      playlist::PlaylistTracksRef,
      user::PublicUser,
    };
    use std::collections::HashMap;

    #[allow(deprecated)]
    let playlist = rspotify::model::SimplifiedPlaylist {
      collaborative: false,
      external_urls: HashMap::new(),
      href: "https://api.spotify.com/v1/playlists/abc".to_string(),
      id: PlaylistId::from_id("37i9dQZF1DXcBWIGoYBM5M")
        .unwrap()
        .into_static(),
      images: Vec::new(),
      name: "Chill Vibes".to_string(),
      owner: PublicUser {
        display_name: None,
        external_urls: HashMap::new(),
        followers: None,
        href: "https://api.spotify.com/v1/users/spotifyuser".to_string(),
        id: UserId::from_id("spotifyuser").unwrap().into_static(),
        images: Vec::new(),
      },
      public: None,
      snapshot_id: "snap".to_string(),
      tracks: PlaylistTracksRef {
        href: "https://api.spotify.com/v1/playlists/abc/tracks".to_string(),
        total: 10,
      },
      items: PlaylistTracksRef {
        href: "https://api.spotify.com/v1/playlists/abc/tracks".to_string(),
        total: 10,
      },
    };
    let info = PlaylistInfo::from_simplified(&playlist);
    assert_eq!(info.owner, "spotifyuser");
    assert_eq!(info.track_count, 10);
  }
}
