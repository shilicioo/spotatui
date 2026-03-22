use crate::core::app::App;
use crate::tui::event::Key;

pub fn handler(key: Key, app: &mut App) {
  match key {
    Key::Char('s') => {
      super::playbar::toggle_like_currently_playing_item(app);
    }
    k if k == app.user_config.keys.back => {
      app.pop_navigation_stack();
    }
    _ => {}
  }
}
