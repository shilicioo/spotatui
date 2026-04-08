use crate::core::user_config::BehaviorConfig;
use ratatui::layout::{Constraint, Layout, Rect};

/// Returns horizontal constraints for the [sidebar, content] split based on config.
/// When sidebar_width_percent is 0, the sidebar is hidden (zero length).
/// When sidebar_width_percent is 100, content is hidden.
pub fn sidebar_constraints(behavior: &BehaviorConfig) -> [Constraint; 2] {
  let sidebar = behavior.sidebar_width_percent.min(100) as u16;
  let content = 100u16.saturating_sub(sidebar);
  [
    Constraint::Percentage(sidebar),
    Constraint::Percentage(content),
  ]
}

/// Returns the playbar height constraint based on config.
/// When playbar_height_rows is 0, the playbar is hidden.
pub fn playbar_constraint(behavior: &BehaviorConfig) -> Constraint {
  Constraint::Length(behavior.playbar_height_rows)
}

/// Returns vertical constraints for the [library, playlists] split within the sidebar.
pub fn library_constraints(behavior: &BehaviorConfig) -> [Constraint; 2] {
  let library = behavior.library_height_percent.min(100) as u16;
  let playlists = 100u16.saturating_sub(library);
  [
    Constraint::Percentage(library),
    Constraint::Percentage(playlists),
  ]
}

/// Returns the fullscreen content/playbar split used by lyrics and cover-art views.
///
/// When `playbar_height_rows` is 0, the playbar is hidden and the content area fills the screen.
pub fn fullscreen_view_layout(behavior: &BehaviorConfig, area: Rect) -> (Rect, Option<Rect>) {
  if behavior.playbar_height_rows == 0 {
    return (area, None);
  }

  let chunks = Layout::vertical([
    Constraint::Min(0),
    Constraint::Length(behavior.playbar_height_rows),
  ])
  .split(area);
  let content_area = chunks[0];
  let playbar_area = chunks[1];

  (content_area, Some(playbar_area))
}

#[cfg(test)]
mod tests {
  use super::*;
  use crate::core::user_config::UserConfig;

  fn make_behavior_with(sidebar_pct: u8, playbar_rows: u16) -> BehaviorConfig {
    let mut cfg = UserConfig::new();
    cfg.behavior.sidebar_width_percent = sidebar_pct;
    cfg.behavior.playbar_height_rows = playbar_rows;
    cfg.behavior
  }

  fn make_behavior_with_library(library_pct: u8) -> BehaviorConfig {
    let mut cfg = UserConfig::new();
    cfg.behavior.library_height_percent = library_pct;
    cfg.behavior
  }

  fn make_behavior(sidebar_pct: u8) -> BehaviorConfig {
    make_behavior_with(sidebar_pct, 6)
  }

  #[test]
  fn default_sidebar_is_20_percent() {
    let b = make_behavior(20);
    let [sidebar, content] = sidebar_constraints(&b);
    assert_eq!(sidebar, Constraint::Percentage(20));
    assert_eq!(content, Constraint::Percentage(80));
  }

  #[test]
  fn hidden_sidebar_gives_zero_percent() {
    let b = make_behavior(0);
    let [sidebar, content] = sidebar_constraints(&b);
    assert_eq!(sidebar, Constraint::Percentage(0));
    assert_eq!(content, Constraint::Percentage(100));
  }

  #[test]
  fn full_sidebar_hides_content() {
    let b = make_behavior(100);
    let [sidebar, content] = sidebar_constraints(&b);
    assert_eq!(sidebar, Constraint::Percentage(100));
    assert_eq!(content, Constraint::Percentage(0));
  }

  #[test]
  fn over_100_percent_is_clamped() {
    let mut b = make_behavior(20);
    b.sidebar_width_percent = 255;
    let [sidebar, content] = sidebar_constraints(&b);
    assert_eq!(sidebar, Constraint::Percentage(100));
    assert_eq!(content, Constraint::Percentage(0));
  }

  #[test]
  fn default_playbar_is_6_rows() {
    let b = make_behavior_with(20, 6);
    assert_eq!(playbar_constraint(&b), Constraint::Length(6));
  }

  #[test]
  fn hidden_playbar_is_zero_rows() {
    let b = make_behavior_with(20, 0);
    assert_eq!(playbar_constraint(&b), Constraint::Length(0));
  }

  #[test]
  fn default_library_is_30_percent() {
    let b = make_behavior_with_library(30);
    let [lib, playlists] = library_constraints(&b);
    assert_eq!(lib, Constraint::Percentage(30));
    assert_eq!(playlists, Constraint::Percentage(70));
  }

  #[test]
  fn hidden_library_gives_zero_percent() {
    let b = make_behavior_with_library(0);
    let [lib, playlists] = library_constraints(&b);
    assert_eq!(lib, Constraint::Percentage(0));
    assert_eq!(playlists, Constraint::Percentage(100));
  }

  #[test]
  fn library_over_100_percent_is_clamped() {
    let mut b = make_behavior_with_library(30);
    b.library_height_percent = 255;
    let [lib, playlists] = library_constraints(&b);
    assert_eq!(lib, Constraint::Percentage(100));
    assert_eq!(playlists, Constraint::Percentage(0));
  }

  #[test]
  fn fullscreen_layout_hides_playbar_when_height_is_zero() {
    let b = make_behavior_with(20, 0);
    let area = Rect::new(2, 4, 80, 24);

    let (content, playbar) = fullscreen_view_layout(&b, area);

    assert_eq!(content, area);
    assert!(playbar.is_none());
  }

  #[test]
  fn fullscreen_layout_splits_content_and_playbar_when_height_is_set() {
    let b = make_behavior_with(20, 6);
    let area = Rect::new(2, 4, 80, 24);

    let (content, playbar) = fullscreen_view_layout(&b, area);

    assert_eq!(content, Rect::new(2, 4, 80, 18));
    assert_eq!(playbar, Some(Rect::new(2, 22, 80, 6)));
  }
}
