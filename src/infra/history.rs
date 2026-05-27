use crate::core::app::App;
use crate::infra::media_metadata::{
  current_playback_snapshot, PlaybackItemKind, PlaybackSnapshot, PlaybackSource,
};
use anyhow::{anyhow, Result};
use chrono::{DateTime, Datelike, Duration, Timelike, Utc};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::fs::{self, OpenOptions};
use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Instant;
use tokio::sync::Mutex;

const HISTORY_SUBDIR: &str = "history";
const LISTENS_FILE_NAME: &str = "listens.jsonl";
const CLOUD_SYNC_URL: &str = "https://spotatui.com/api/sync";
const NOW_PLAYING_SYNC_URL: &str = "https://spotatui.com/api/sync/now-playing";
/// Heartbeat interval: must be well under the 5-minute online threshold used by the website.
const NOW_PLAYING_HEARTBEAT_SECS: u64 = 60;
const MAX_INTERVAL_MS: u64 = 5_000;
const REPLAY_RESET_THRESHOLD_MS: u128 = 15_000;
const REPLAY_PREVIOUS_PROGRESS_FLOOR_MS: u128 = 30_000;

#[derive(Clone, Copy, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum HistoryPlaybackSource {
  NativeContext,
  NativeRawList,
  ExternalDevice,
}

#[derive(Clone, Copy, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum HistoryItemKind {
  Track,
  Episode,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
pub struct ListenRecord {
  pub started_at: DateTime<Utc>,
  pub ended_at: DateTime<Utc>,
  pub listened_ms: u64,
  pub duration_ms: u32,
  pub qualified: bool,
  pub title: String,
  pub artists: Vec<String>,
  pub album: String,
  pub item_kind: HistoryItemKind,
  pub item_id: Option<String>,
  pub item_uri: Option<String>,
  pub context_uri: Option<String>,
  pub source: HistoryPlaybackSource,
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct SessionIdentity {
  title: String,
  artists: Vec<String>,
  album: String,
  item_id: Option<String>,
  item_uri: Option<String>,
  context_uri: Option<String>,
  source: HistoryPlaybackSource,
}

#[derive(Clone, Debug)]
struct ActiveListenSession {
  started_at: DateTime<Utc>,
  identity: SessionIdentity,
  title: String,
  artists: Vec<String>,
  album: String,
  item_kind: HistoryItemKind,
  item_id: Option<String>,
  item_uri: Option<String>,
  context_uri: Option<String>,
  source: HistoryPlaybackSource,
  duration_ms: u32,
  listened_ms: u64,
  last_progress_ms: u128,
  last_is_playing: bool,
}

#[derive(Default)]
struct HistoryCollector {
  current: Option<ActiveListenSession>,
  last_observed_at: Option<Instant>,
}

#[derive(Serialize)]
struct NowPlayingPayload<'a> {
  title: &'a str,
  artists: &'a [String],
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RecapPeriod {
  SevenDays,
  ThirtyDays,
  Month,
  Year,
  All,
}

pub fn spawn_history_collector(app: Arc<Mutex<App>>) {
  tokio::spawn(async move {
    let mut collector = HistoryCollector::default();
    let mut interval = tokio::time::interval(tokio::time::Duration::from_secs(1));
    let mut last_auto_check = Instant::now();
    let http_client = reqwest::Client::new();

    // Check on startup
    perform_auto_recap_check(&app);

    // Sync history to cloud on startup
    let sync_token_opt: Option<String> = if let Ok(app_guard) = app.try_lock() {
      app_guard.user_config.behavior.sync_token.clone()
    } else {
      None
    };

    if let Some(ref token) = sync_token_opt {
      let token = token.clone();
      let client = http_client.clone();
      tokio::spawn(async move {
        if let Err(e) = sync_history_to_cloud_with_client(&client, &token).await {
          log::warn!("failed to run startup history cloud sync: {}", e);
        }
      });
    }

    // Now-playing tracking state
    let mut last_now_playing: Option<(String, Vec<String>)> = None;
    let mut last_heartbeat: Option<Instant> = None;

    loop {
      interval.tick().await;

      if last_auto_check.elapsed().as_secs() >= 3600 {
        last_auto_check = Instant::now();
        perform_auto_recap_check(&app);
      }

      let snapshot = if let Ok(app) = app.try_lock() {
        current_playback_snapshot(&app)
      } else {
        continue;
      };

      // Now-playing sync: update on track change and heartbeat while playing
      if let Some(ref token) = sync_token_opt {
        let snap_for_np = snapshot
          .as_ref()
          .filter(|s| s.item_kind == PlaybackItemKind::Track);
        match snap_for_np {
          Some(snap) => {
            let current_id = (snap.metadata.title.clone(), snap.metadata.artists.clone());
            let track_changed = last_now_playing.as_ref() != Some(&current_id);
            let heartbeat_due = snap.is_playing
              && last_heartbeat
                .map(|t| t.elapsed().as_secs() >= NOW_PLAYING_HEARTBEAT_SECS)
                .unwrap_or(false);

            if track_changed || heartbeat_due {
              last_now_playing = Some(current_id.clone());
              last_heartbeat = Some(Instant::now());
              let token_clone = token.clone();
              let client = http_client.clone();
              let (title, artists) = current_id;
              tokio::spawn(async move {
                if let Err(e) =
                  sync_now_playing_to_cloud(&client, &token_clone, &title, &artists).await
                {
                  log::warn!("failed to sync now-playing: {}", e);
                }
              });
            }
          }
          None => {
            last_now_playing = None;
          }
        }
      }

      if let Err(error) = collector.observe(snapshot) {
        log::warn!("listening history collector failed: {}", error);
      }
    }
  });
}

fn last_recap_file_path() -> Result<PathBuf> {
  let home = dirs::home_dir().ok_or_else(|| anyhow!("No $HOME directory found for history"))?;
  Ok(
    home
      .join(".config")
      .join("spotatui")
      .join(HISTORY_SUBDIR)
      .join("last_recap_at.txt"),
  )
}

fn perform_auto_recap_check(app: &Arc<Mutex<App>>) {
  let path = match last_recap_file_path() {
    Ok(p) => p,
    Err(e) => {
      log::warn!("failed to get last recap file path: {}", e);
      return;
    }
  };

  let now = Utc::now();
  let should_generate = match fs::read_to_string(&path) {
    Ok(content) => {
      if let Ok(ts) = content.trim().parse::<i64>() {
        let diff_secs = now.timestamp() - ts;
        diff_secs >= 30 * 24 * 3600
      } else {
        true
      }
    }
    Err(_) => true,
  };

  if should_generate {
    let home = match dirs::home_dir() {
      Some(h) => h,
      None => return,
    };
    let output_path = home
      .join(".config")
      .join("spotatui")
      .join("spotatui-recap.html");

    match export_history_recap(RecapPeriod::ThirtyDays, &output_path) {
      Ok(count) => {
        if count > 0 {
          if let Err(e) = fs::write(&path, now.timestamp().to_string()) {
            log::warn!("failed to write last recap timestamp: {}", e);
          }

          if let Ok(mut app_guard) = app.try_lock() {
            app_guard.set_status_message(
              format!(
                "30-day listening recap generated at ~/.config/spotatui/spotatui-recap.html ({} listens)",
                count
              ),
              10,
            );
          }
        }
      }
      Err(e) => {
        log::warn!("failed to automatically generate recap: {}", e);
      }
    }
  }
}

pub fn export_history_recap(period: RecapPeriod, output_path: &Path) -> Result<usize> {
  let listens = load_listens()?;
  let filtered = filter_listens_for_period(&listens, period);
  let html = render_history_recap_html(period, &filtered);

  if let Some(parent) = output_path.parent() {
    fs::create_dir_all(parent)?;
  }
  fs::write(output_path, html)?;

  Ok(filtered.len())
}

pub fn parse_recap_period(value: &str) -> Result<RecapPeriod> {
  match value {
    "7d" => Ok(RecapPeriod::SevenDays),
    "30d" => Ok(RecapPeriod::ThirtyDays),
    "month" => Ok(RecapPeriod::Month),
    "year" => Ok(RecapPeriod::Year),
    "all" => Ok(RecapPeriod::All),
    _ => Err(anyhow!("unsupported recap period '{}'", value)),
  }
}

pub fn load_listens() -> Result<Vec<ListenRecord>> {
  let path = listens_file_path()?;
  if !path.exists() {
    return Ok(Vec::new());
  }

  let file = fs::File::open(path)?;
  let reader = BufReader::new(file);
  let mut listens = Vec::new();
  for line in reader.lines() {
    let line = line?;
    if line.trim().is_empty() {
      continue;
    }

    match serde_json::from_str::<ListenRecord>(&line) {
      Ok(record) => listens.push(record),
      Err(error) => {
        log::warn!("skipping malformed history line: {}", error);
      }
    }
  }

  Ok(listens)
}

impl HistoryCollector {
  fn observe(&mut self, snapshot: Option<PlaybackSnapshot>) -> Result<()> {
    let now_utc = Utc::now();
    let now_instant = Instant::now();

    if let Some(current) = &mut self.current {
      if let Some(last_observed_at) = self.last_observed_at {
        let elapsed_ms = now_instant
          .saturating_duration_since(last_observed_at)
          .as_millis()
          .min(MAX_INTERVAL_MS as u128) as u64;
        if current.last_is_playing {
          current.listened_ms = current.listened_ms.saturating_add(elapsed_ms);
        }
      }
    }
    self.last_observed_at = Some(now_instant);

    let snapshot = snapshot.filter(|snapshot| snapshot.item_kind == PlaybackItemKind::Track);
    match snapshot {
      Some(snapshot) => {
        let identity = SessionIdentity::from_snapshot(&snapshot);
        let should_roll = self.current.as_ref().is_some_and(|current| {
          current.identity != identity
            || (current.last_progress_ms > REPLAY_PREVIOUS_PROGRESS_FLOOR_MS
              && snapshot.progress_ms + REPLAY_RESET_THRESHOLD_MS < current.last_progress_ms)
        });

        if should_roll {
          self.finalize_current(now_utc)?;
        }

        if self.current.is_none() {
          self.current = Some(ActiveListenSession::from_snapshot(snapshot, now_utc));
          return Ok(());
        }

        if let Some(current) = &mut self.current {
          current.duration_ms = snapshot.metadata.duration_ms;
          current.last_progress_ms = snapshot.progress_ms;
          current.last_is_playing = snapshot.is_playing;
        }
      }
      None => {
        self.finalize_current(now_utc)?;
      }
    }

    Ok(())
  }

  fn finalize_current(&mut self, ended_at: DateTime<Utc>) -> Result<()> {
    let Some(current) = self.current.take() else {
      return Ok(());
    };

    if current.listened_ms == 0 {
      return Ok(());
    }

    append_listen_record(ListenRecord::from_active_session(current, ended_at))
  }
}

impl SessionIdentity {
  fn from_snapshot(snapshot: &PlaybackSnapshot) -> Self {
    Self {
      title: snapshot.metadata.title.clone(),
      artists: snapshot.metadata.artists.clone(),
      album: snapshot.metadata.album.clone(),
      item_id: snapshot.item_id.clone(),
      item_uri: snapshot.item_uri.clone(),
      context_uri: snapshot.context_uri.clone(),
      source: history_source_from_snapshot(snapshot),
    }
  }
}

impl ActiveListenSession {
  fn from_snapshot(snapshot: PlaybackSnapshot, started_at: DateTime<Utc>) -> Self {
    let source = history_source_from_snapshot(&snapshot);
    let identity = SessionIdentity::from_snapshot(&snapshot);
    let PlaybackSnapshot {
      metadata,
      item_id,
      item_uri,
      context_uri,
      source: _,
      progress_ms,
      is_playing,
      ..
    } = snapshot;
    let title = metadata.title;
    let artists = metadata.artists;
    let album = metadata.album;
    let duration_ms = metadata.duration_ms;
    Self {
      started_at,
      identity,
      title,
      artists,
      album,
      item_kind: HistoryItemKind::Track,
      item_id,
      item_uri,
      context_uri,
      source,
      duration_ms,
      listened_ms: 0,
      last_progress_ms: progress_ms,
      last_is_playing: is_playing,
    }
  }
}

impl ListenRecord {
  fn from_active_session(session: ActiveListenSession, ended_at: DateTime<Utc>) -> Self {
    let qualified = qualifies_listen(session.duration_ms, session.listened_ms);
    Self {
      started_at: session.started_at,
      ended_at,
      listened_ms: session.listened_ms,
      duration_ms: session.duration_ms,
      qualified,
      title: session.title,
      artists: session.artists,
      album: session.album,
      item_kind: session.item_kind,
      item_id: session.item_id,
      item_uri: session.item_uri,
      context_uri: session.context_uri,
      source: session.source,
    }
  }
}

fn listens_file_path() -> Result<PathBuf> {
  let home = dirs::home_dir().ok_or_else(|| anyhow!("No $HOME directory found for history"))?;
  Ok(
    home
      .join(".config")
      .join("spotatui")
      .join(HISTORY_SUBDIR)
      .join(LISTENS_FILE_NAME),
  )
}

fn append_listen_record(record: ListenRecord) -> Result<()> {
  let path = listens_file_path()?;
  if let Some(parent) = path.parent() {
    fs::create_dir_all(parent)?;
  }

  let mut file = OpenOptions::new().create(true).append(true).open(path)?;
  serde_json::to_writer(&mut file, &record)?;
  writeln!(file)?;
  Ok(())
}

fn qualifies_listen(duration_ms: u32, listened_ms: u64) -> bool {
  if duration_ms <= 30_000 {
    return false;
  }

  let threshold_ms = u64::from(duration_ms / 2).min(240_000);
  listened_ms >= threshold_ms
}

fn history_source_from_snapshot(snapshot: &PlaybackSnapshot) -> HistoryPlaybackSource {
  match snapshot.source {
    PlaybackSource::NativeContext => HistoryPlaybackSource::NativeContext,
    PlaybackSource::NativeRawList => HistoryPlaybackSource::NativeRawList,
    PlaybackSource::ExternalDevice => HistoryPlaybackSource::ExternalDevice,
  }
}

fn filter_listens_for_period(listens: &[ListenRecord], period: RecapPeriod) -> Vec<ListenRecord> {
  let now = Utc::now();
  listens
    .iter()
    .filter(|record| record.qualified)
    .filter(|record| match period {
      RecapPeriod::SevenDays => record.ended_at >= now - Duration::days(7),
      RecapPeriod::ThirtyDays => record.ended_at >= now - Duration::days(30),
      RecapPeriod::Month => {
        record.ended_at.year() == now.year() && record.ended_at.month() == now.month()
      }
      RecapPeriod::Year => record.ended_at.year() == now.year(),
      RecapPeriod::All => true,
    })
    .cloned()
    .collect()
}

fn render_history_recap_html(period: RecapPeriod, listens: &[ListenRecord]) -> String {
  let total_listening_ms = listens.iter().map(|record| record.listened_ms).sum::<u64>();
  let top_tracks = aggregate_top_tracks(listens);
  let top_artists = aggregate_top_artists(listens);
  let top_albums = aggregate_top_albums(listens);
  let listening_days = aggregate_days(listens);
  let listening_hours = aggregate_hours(listens);

  let top_album_title = escape_html(
    top_albums
      .first()
      .map(|entry| entry.display.as_str())
      .unwrap_or("No data"),
  );

  let top_track_raw = top_tracks
    .first()
    .map(|entry| entry.display.as_str())
    .unwrap_or("No data");
  let top_track_title_clean = if top_track_raw == "No data" {
    "No data".to_string()
  } else {
    top_track_raw
      .split(" - ")
      .next()
      .unwrap_or(top_track_raw)
      .to_string()
  };
  let top_track_title_clean = escape_html(&top_track_title_clean);

  let top_track_artist = if top_track_raw == "No data" {
    "No data".to_string()
  } else {
    top_track_raw.split(" - ").nth(1).unwrap_or("").to_string()
  };
  let top_track_artist = escape_html(&top_track_artist);

  format!(
    r#"<!DOCTYPE html>
<html lang="en">
<head>
  <meta charset="utf-8">
  <meta name="viewport" content="width=device-width, initial-scale=1">
  <title>spotatui recap</title>
  <link rel="preconnect" href="https://fonts.googleapis.com">
  <link rel="preconnect" href="https://fonts.gstatic.com" crossorigin>
  <link href="https://fonts.googleapis.com/css2?family=Plus+Jakarta+Sans:wght@300;400;500;600;700;800&family=JetBrains+Mono:wght@400;700&display=swap" rel="stylesheet">
  <style>
    :root {{
      --bg: #05080c;
      --accent: #1db954;
      --accent-glow: rgba(29, 185, 84, 0.15);
      --card-gradient: linear-gradient(135deg, #0b1a11 0%, #050a0f 100%);
      --text: #f1f5f9;
      --text-muted: #94a3b8;
      --border: rgba(255, 255, 255, 0.05);
      --font-sans: "Plus Jakarta Sans", -apple-system, BlinkMacSystemFont, "Segoe UI", sans-serif;
      --font-mono: "JetBrains Mono", monospace;
    }}
    * {{
      box-sizing: border-box;
      margin: 0;
      padding: 0;
    }}
    body {{
      min-height: 100vh;
      background: radial-gradient(circle at 50% 0%, #0e2417 0%, #05080c 70%);
      color: var(--text);
      font-family: var(--font-sans);
      padding: 40px 20px 60px;
    }}
    .app-container {{
      max-width: 1200px;
      margin: 0 auto;
      width: 100%;
    }}
    .app-header {{
      text-align: center;
      margin-bottom: 40px;
    }}
    .app-header h1 {{
      font-size: 2.2rem;
      font-weight: 800;
      letter-spacing: -0.03em;
      font-family: var(--font-sans);
      margin-bottom: 6px;
    }}
    .brand-logo {{
      color: var(--text);
    }}
    .brand-accent {{
      color: var(--accent);
      text-shadow: 0 0 10px rgba(29, 185, 84, 0.4);
    }}
    .app-header p {{
      color: var(--text-muted);
      font-size: 0.9rem;
      letter-spacing: 0.12em;
      text-transform: uppercase;
      font-weight: 600;
    }}
    .app-desc {{
      color: var(--text-muted);
      font-size: 0.95rem;
      max-width: 580px;
      margin: 12px auto 0;
      line-height: 1.5;
      text-transform: none !important;
      letter-spacing: normal !important;
      font-weight: 400 !important;
    }}
    .dashboard-grid {{
      display: grid;
      grid-template-columns: 440px 1fr;
      gap: 40px;
      align-items: start;
    }}
    @media (max-width: 1024px) {{
      .dashboard-grid {{
        grid-template-columns: 1fr;
        gap: 32px;
      }}
      .share-column {{
        display: flex;
        flex-direction: column;
        align-items: center;
      }}
    }}
    .share-column {{
      position: sticky;
      top: 40px;
    }}
    .card-wrapper {{
      width: 440px;
      height: 660px;
      display: flex;
      justify-content: center;
      align-items: center;
      border-radius: 28px;
    }}
    @media (max-width: 480px) {{
      .card-wrapper {{
        transform: scale(0.8);
        height: 528px;
        margin-bottom: -40px;
      }}
    }}
    @media (max-width: 380px) {{
      .card-wrapper {{
        transform: scale(0.68);
        height: 448px;
        margin-bottom: -80px;
      }}
    }}
    .share-card {{
      width: 440px;
      height: 660px;
      background: var(--card-gradient);
      border: 1px solid rgba(29, 185, 84, 0.22);
      border-radius: 28px;
      padding: 32px;
      display: flex;
      flex-direction: column;
      justify-content: space-between;
      position: relative;
      overflow: hidden;
      box-shadow: 0 25px 50px -12px rgba(0,0,0,0.8), 0 0 40px rgba(29, 185, 84, 0.12);
      box-sizing: border-box;
    }}
    .card-glow {{
      position: absolute;
      top: -150px;
      left: 50%;
      transform: translateX(-50%);
      width: 300px;
      height: 300px;
      background: radial-gradient(circle, rgba(29, 185, 84, 0.18) 0%, transparent 70%);
      pointer-events: none;
      z-index: 0;
    }}
    .card-header {{
      position: relative;
      height: 32px;
      z-index: 1;
      width: 100%;
      margin-bottom: 10px;
    }}
    .card-brand {{
      position: absolute;
      left: 0;
      top: 0;
      font-family: var(--font-sans);
      font-weight: 800;
      font-size: 1.25rem;
      letter-spacing: -0.02em;
    }}
    .period-badge {{
      position: absolute;
      right: 0;
      top: 0;
      font-size: 0.72rem;
      font-weight: 700;
      color: var(--accent);
      background: rgba(29, 185, 84, 0.12);
      border: 1px solid rgba(29, 185, 84, 0.3);
      padding: 5px 12px;
      border-radius: 99px;
      text-transform: uppercase;
      letter-spacing: 0.05em;
    }}
    .cover-container {{
      position: relative;
      width: 174px;
      height: 174px;
      margin: 0 auto;
      border-radius: 20px;
      overflow: hidden;
      border: 1px solid rgba(255, 255, 255, 0.08);
      box-shadow: 0 15px 35px rgba(0, 0, 0, 0.5), 0 0 30px rgba(29, 185, 84, 0.15);
      z-index: 1;
    }}
    .cover-placeholder {{
      position: absolute;
      top: 0; left: 0; right: 0; bottom: 0;
      background: linear-gradient(135deg, #1b2820 0%, #05080c 100%);
      display: flex;
      justify-content: center;
      align-items: center;
      z-index: 1;
    }}
    .cover-img {{
      position: absolute;
      top: 0; left: 0; right: 0; bottom: 0;
      width: 100%;
      height: 100%;
      object-fit: cover;
      z-index: 2;
      display: none;
    }}
    .top-track-card {{
      background: rgba(255, 255, 255, 0.02);
      border: 1px solid rgba(255, 255, 255, 0.04);
      border-radius: 18px;
      padding: 16px 20px;
      text-align: center;
      backdrop-filter: blur(10px);
      z-index: 1;
      width: 100%;
    }}
    .rank-badge {{
      font-family: var(--font-mono);
      font-size: 0.68rem;
      color: var(--accent);
      background: rgba(29, 185, 84, 0.1);
      padding: 3px 8px;
      border-radius: 6px;
      letter-spacing: 0.08em;
      font-weight: 700;
      display: inline-block;
      margin-bottom: 8px;
      border: 1px solid rgba(29, 185, 84, 0.18);
    }}
    .track-title {{
      font-size: 1.15rem;
      font-weight: 800;
      color: #ffffff;
      margin-bottom: 4px;
      white-space: nowrap;
      overflow: hidden;
      text-overflow: ellipsis;
    }}
    .track-artist {{
      font-size: 0.88rem;
      color: var(--text-muted);
      white-space: nowrap;
      overflow: hidden;
      text-overflow: ellipsis;
    }}
    .card-stats-grid {{
      display: flex;
      flex-direction: column;
      gap: 12px;
      z-index: 1;
      width: 100%;
    }}
    .stats-row {{
      display: flex;
      gap: 12px;
      width: 100%;
    }}
    .stat-pill {{
      background: rgba(255, 255, 255, 0.02);
      border: 1px solid rgba(255, 255, 255, 0.03);
      border-radius: 14px;
      padding: 10px 14px;
      display: flex;
      flex-direction: column;
      flex: 1;
      min-width: 0;
      box-sizing: border-box;
    }}
    .stat-pill.full-width {{
      flex: none;
      width: 100%;
    }}
    .stat-label {{
      font-size: 0.7rem;
      color: var(--text-muted);
      text-transform: uppercase;
      letter-spacing: 0.04em;
      margin-bottom: 3px;
      font-weight: 500;
    }}
    .stat-value {{
      font-size: 0.92rem;
      font-weight: 700;
      color: #ffffff;
      white-space: nowrap;
      overflow: hidden;
      text-overflow: ellipsis;
    }}
    .card-footer {{
      border-top: 1px solid rgba(255, 255, 255, 0.04);
      padding-top: 14px;
      position: relative;
      height: 30px;
      z-index: 1;
      width: 100%;
    }}
    .card-footer-left {{
      position: absolute;
      left: 0;
      top: 14px;
      font-size: 0.72rem;
      color: var(--text-muted);
    }}
    .card-footer-right {{
      position: absolute;
      right: 0;
      top: 14px;
      font-size: 0.72rem;
      font-family: var(--font-mono);
      color: var(--accent);
    }}
    .download-container {{
      margin-top: 20px;
      width: 100%;
      max-width: 440px;
      display: flex;
      justify-content: center;
    }}
    .download-btn {{
      background: linear-gradient(90deg, var(--accent) 0%, #10b981 100%);
      border: none;
      border-radius: 14px;
      color: #ffffff;
      font-size: 0.95rem;
      font-weight: 700;
      padding: 14px 28px;
      cursor: pointer;
      display: flex;
      align-items: center;
      gap: 10px;
      box-shadow: 0 10px 20px rgba(29, 185, 84, 0.25);
      transition: all 0.3s cubic-bezier(0.4, 0, 0.2, 1);
      width: 100%;
      justify-content: center;
      font-family: var(--font-sans);
    }}
    .download-btn:hover {{
      transform: translateY(-2px);
      box-shadow: 0 15px 30px rgba(29, 185, 84, 0.35);
    }}
    .download-btn:active {{
      transform: translateY(0);
    }}
    .animate-spin {{
      animation: spin 1s linear infinite;
    }}
    .details-column {{
      min-width: 0;
    }}
    .details-section-title {{
      font-size: 1.25rem;
      font-weight: 800;
      margin-bottom: 20px;
      letter-spacing: 0.05em;
      text-transform: uppercase;
      color: #38bdf8;
      border-left: 3px solid var(--accent);
      padding-left: 12px;
      line-height: 1;
    }}
    .grid {{
      display: grid;
      grid-template-columns: repeat(auto-fit, minmax(320px, 1fr));
      gap: 20px;
      margin-bottom: 24px;
    }}
    .panel {{
      background: rgba(10, 18, 30, 0.5);
      border: 1px solid var(--border);
      border-radius: 20px;
      padding: 24px;
      backdrop-filter: blur(20px);
    }}
    .panel h2 {{
      margin: 0 0 16px 0;
      font-size: 1.1rem;
      font-weight: 700;
      color: #f8fafc;
    }}
    .ranked-list {{
      list-style: none;
      padding: 0;
      margin: 0;
    }}
    .ranked-list li {{
      display: flex;
      align-items: center;
      gap: 14px;
      padding: 10px 14px;
      border-radius: 12px;
      background: rgba(255, 255, 255, 0.01);
      border: 1px solid rgba(255, 255, 255, 0.02);
      margin-bottom: 8px;
      transition: all 0.2s ease;
    }}
    .ranked-list li:hover {{
      background: rgba(29, 185, 84, 0.04);
      border-color: rgba(29, 185, 84, 0.15);
      transform: translateX(4px);
    }}
    .rank {{
      font-family: var(--font-mono);
      font-size: 0.9rem;
      font-weight: 800;
      color: var(--accent);
      width: 20px;
    }}
    .recent-icon {{
      color: var(--accent);
      display: flex;
      align-items: center;
      justify-content: center;
      width: 20px;
    }}
    .entry-details {{
      flex: 1;
      min-width: 0;
    }}
    .entry-details strong {{
      display: block;
      font-size: 0.92rem;
      font-weight: 600;
      color: #f1f5f9;
      white-space: nowrap;
      overflow: hidden;
      text-overflow: ellipsis;
    }}
    .entry-artist {{
      font-size: 0.82rem;
      color: var(--accent);
      margin-top: 2px;
      font-weight: 500;
    }}
    .subtle {{
      font-size: 0.8rem;
      color: var(--text-muted);
      margin-top: 2px;
    }}
    .bars {{
      display: grid;
      gap: 12px;
    }}
    .bar-row {{
      display: grid;
      gap: 6px;
    }}
    .bar-label {{
      display: flex;
      justify-content: space-between;
      font-family: var(--font-mono);
      font-size: 0.78rem;
      color: var(--text-muted);
    }}
    .bar {{
      height: 8px;
      border-radius: 99px;
      background: rgba(255, 255, 255, 0.03);
      overflow: hidden;
      position: relative;
    }}
    .bar > span {{
      display: block;
      height: 100%;
      border-radius: 99px;
      background: linear-gradient(90deg, var(--accent) 0%, #10b981 100%);
      box-shadow: 0 0 10px rgba(29, 185, 84, 0.3);
    }}
    footer {{
      margin-top: 48px;
      text-align: center;
      font-size: 0.8rem;
      color: #64748b;
      line-height: 1.6;
      max-width: 600px;
      margin-left: auto;
      margin-right: auto;
      border-top: 1px solid var(--border);
      padding-top: 24px;
    }}
  </style>
</head>
<body>
  <div class="app-container">
    <header class="app-header">
      <h1><span class="brand-logo">spota</span><span class="brand-accent">tui</span></h1>
      <p>Listening History Recap</p>
      <p class="app-desc">{summary}</p>
    </header>

    <div class="dashboard-grid">
      <!-- Left Column: The Share Card Area -->
      <div class="share-column">
        <div class="card-wrapper">
          <div id="share-card" class="share-card">
            <!-- Ambient glowing node inside the card -->
            <div class="card-glow"></div>
            
            <div class="card-header">
              <span class="card-brand">spota<span style="color:var(--accent)">tui</span></span>
              <span class="period-badge">{title}</span>
            </div>
            
            <div class="cover-container">
              <div id="cover-art-placeholder" class="cover-placeholder">
                <svg xmlns="http://www.w3.org/2000/svg" width="48" height="48" viewBox="0 0 24 24" fill="none" stroke="rgba(255,255,255,0.2)" stroke-width="1.5" stroke-linecap="round" stroke-linejoin="round"><path d="M9 18V5l12-2v13"></path><circle cx="6" cy="18" r="3"></circle><circle cx="18" cy="16" r="3"></circle></svg>
              </div>
              <img id="cover-art-img" class="cover-img" alt="Cover Art" crossorigin="anonymous">
            </div>
            
            <div class="top-track-card">
              <span class="rank-badge">NO. 1 TRACK</span>
              <div class="track-title">{top_track_title}</div>
              <div class="track-artist">{top_track_artist}</div>
            </div>
            
            <div class="card-stats-grid">
              <div class="stats-row">
                <div class="stat-pill">
                  <span class="stat-label">Qualified Plays</span>
                  <span class="stat-value">{total_plays}</span>
                </div>
                <div class="stat-pill">
                  <span class="stat-label">Listening Time</span>
                  <span class="stat-value">{total_time}</span>
                </div>
              </div>
              <div class="stat-pill full-width">
                <span class="stat-label">Top Artist</span>
                <span class="stat-value">{top_artist_name}</span>
              </div>
              <div class="stat-pill full-width">
                <span class="stat-label">Top Album</span>
                <span class="stat-value">{top_album_title}</span>
              </div>
            </div>
            
            <div class="card-footer">
              <span class="card-footer-left">generated by <strong>spotatui</strong></span>
              <span class="card-footer-right">github.com/LargeModGames/spotatui</span>
            </div>
          </div>
        </div>
        
        <div class="download-container">
          <button class="download-btn" onclick="downloadCard()">
            <svg xmlns="http://www.w3.org/2000/svg" width="16" height="16" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2.5" stroke-linecap="round" stroke-linejoin="round"><path d="M21 15v4a2 2 0 0 1-2 2H5a2 2 0 0 1-2-2v-4"></path><polyline points="7 10 12 15 17 10"></polyline><line x1="12" y1="15" x2="12" y2="3"></line></svg>
            Download Share Card
          </button>
        </div>
      </div>
      
      <!-- Right Column: Detailed Insights Dashboard -->
      <div class="details-column">
        <div class="details-section-title">Detailed Analytics</div>
        
        <div class="grid">
          <article class="panel">
            <h2>Top Tracks</h2>
            {top_tracks_html}
          </article>
          <article class="panel">
            <h2>Top Artists</h2>
            {top_artists_html}
          </article>
          <article class="panel">
            <h2>Top Albums</h2>
            {top_albums_html}
          </article>
          <article class="panel">
            <h2>Recent Plays</h2>
            {recent_html}
          </article>
        </div>
        
        <div class="grid">
          <article class="panel">
            <h2>Listening by Day</h2>
            {days_html}
          </article>
          <article class="panel">
            <h2>Listening by Hour</h2>
            {hours_html}
          </article>
        </div>
      </div>
    </div>

    <footer>
      History is generated from spotatui local listening records and starts when the feature is enabled. Short or skipped plays are stored but excluded from headline recap totals.
    </footer>
  </div>

  <script src="https://cdnjs.cloudflare.com/ajax/libs/html2canvas/1.4.1/html2canvas.min.js"></script>
  <script>
    document.addEventListener('DOMContentLoaded', () => {{
      const trackTitle = "{top_track_title}";
      const trackArtist = "{top_artist_name}";
      
      if (trackTitle && trackTitle !== "No data") {{
        // Clean parentheses or bracket noise for high search precision
        const cleanTitle = trackTitle.replace(/\([^)]*\)/g, '').replace(/\[[^\]]*\]/g, '').trim();
        const query = encodeURIComponent(`${{cleanTitle}} ${{trackArtist}}`);
        const url = `https://itunes.apple.com/search?term=${{query}}&limit=1&entity=song`;
        
        fetch(url)
          .then(res => res.json())
          .then(data => {{
            if (data.results && data.results.length > 0) {{
              const artworkUrl = data.results[0].artworkUrl100.replace('100x100bb', '600x600bb');
              const img = document.getElementById('cover-art-img');
              img.src = artworkUrl;
              img.onload = () => {{
                img.style.display = 'block';
                document.getElementById('cover-art-placeholder').style.display = 'none';
              }};
            }}
          }})
          .catch(err => console.error('Error fetching cover art:', err));
      }}
    }});

    function downloadCard() {{
      const card = document.getElementById('share-card');
      const btn = document.querySelector('.download-btn');
      const wrapper = document.querySelector('.card-wrapper');
      const originalText = btn.innerHTML;
      
      btn.disabled = true;
      btn.innerHTML = `
        <svg class="animate-spin" xmlns="http://www.w3.org/2000/svg" width="16" height="16" fill="none" viewBox="0 0 24 24" stroke="currentColor" stroke-width="2.5"><path stroke-linecap="round" stroke-linejoin="round" d="M4 4v5h.582m15.356 2A8.001 8.001 0 1 1 21.2 8H18" /></svg>
        Generating...
      `;
      btn.style.opacity = '0.7';

      const originalTransform = wrapper.style.transform;
      const originalMargin = wrapper.style.marginBottom;
      const originalHeight = wrapper.style.height;
      const originalBoxShadow = card.style.boxShadow;

      wrapper.style.transform = 'none';
      wrapper.style.marginBottom = '0';
      wrapper.style.height = 'auto';
      card.style.boxShadow = 'none';

      document.fonts.ready.then(() => {{
        setTimeout(() => {{
          html2canvas(card, {{
            scale: 3,
            useCORS: true,
            backgroundColor: '#05080c',
            logging: false,
            width: 440,
            height: 660,
            windowWidth: 440,
            windowHeight: 660
          }}).then(canvas => {{
            wrapper.style.transform = originalTransform;
            wrapper.style.marginBottom = originalMargin;
            wrapper.style.height = originalHeight;
            card.style.boxShadow = originalBoxShadow;

            const link = document.createElement('a');
            const dateStr = new Date().toISOString().slice(0, 10);
            link.download = `spotatui-recap-${{dateStr}}.png`;
            link.href = canvas.toDataURL('image/png');
            link.click();
            
            btn.disabled = false;
            btn.innerHTML = originalText;
            btn.style.opacity = '1';
          }}).catch(err => {{
            console.error(err);
            wrapper.style.transform = originalTransform;
            wrapper.style.marginBottom = originalMargin;
            wrapper.style.height = originalHeight;
            card.style.boxShadow = originalBoxShadow;

            alert('Could not render image. Please try again!');
            btn.disabled = false;
            btn.innerHTML = originalText;
            btn.style.opacity = '1';
          }});
        }}, 300);
      }});
    }}
  </script>
</body>
</html>
"#,
    title = escape_html(period_label(period)),
    summary = escape_html(if listens.is_empty() {
      "No qualified local listening history was recorded for this window yet."
    } else {
      "A shareable HTML snapshot of your qualified local listening history, generated directly from spotatui."
    }),
    total_plays = listens.len(),
    total_time = format_duration(total_listening_ms),
    top_track_title = top_track_title_clean,
    top_track_artist = top_track_artist,
    top_artist_name = escape_html(
      top_artists
        .first()
        .map(|entry| entry.display.as_str())
        .unwrap_or("No data"),
    ),
    top_album_title = top_album_title,
    top_tracks_html = render_ranked_entries(&top_tracks, "No tracks yet."),
    top_artists_html = render_ranked_entries(&top_artists, "No artists yet."),
    top_albums_html = render_ranked_entries(&top_albums, "No albums yet."),
    recent_html = render_recent_entries(listens),
    days_html = render_bar_entries(&listening_days),
    hours_html = render_bar_entries(&listening_hours),
  )
}

#[derive(Clone)]
struct RankedEntry {
  display: String,
  detail: String,
  value: u64,
}

fn aggregate_top_tracks(listens: &[ListenRecord]) -> Vec<RankedEntry> {
  let mut totals: BTreeMap<String, (String, u64, u64)> = BTreeMap::new();
  for record in listens {
    let key = record
      .item_id
      .clone()
      .or_else(|| record.item_uri.clone())
      .unwrap_or_else(|| format!("{}::{}", record.title, record.artists.join(", ")));
    let entry = totals.entry(key).or_insert_with(|| {
      (
        format!("{} - {}", record.title, record.artists.join(", ")),
        0,
        0,
      )
    });
    entry.1 += record.listened_ms;
    entry.2 += 1;
  }

  sort_ranked_entries(
    totals
      .into_values()
      .map(|(display, listened_ms, plays)| RankedEntry {
        display,
        detail: format!("{} plays · {}", plays, format_duration(listened_ms)),
        value: listened_ms,
      })
      .collect(),
  )
}

fn split_artists(combo: &str) -> Vec<String> {
  let normalized = combo.replace(" and ", ", ").replace(" & ", ", ");
  normalized
    .split(',')
    .map(|s| s.trim().to_string())
    .filter(|s| !s.is_empty())
    .collect()
}

fn aggregate_top_artists(listens: &[ListenRecord]) -> Vec<RankedEntry> {
  let mut totals: BTreeMap<String, (u64, u64)> = BTreeMap::new();
  for record in listens {
    for artist_combo in &record.artists {
      let individual_artists = split_artists(artist_combo);
      for artist in individual_artists {
        let entry = totals.entry(artist).or_insert((0, 0));
        entry.0 += record.listened_ms;
        entry.1 += 1;
      }
    }
  }

  sort_ranked_entries(
    totals
      .into_iter()
      .map(|(artist, (listened_ms, plays))| RankedEntry {
        display: artist,
        detail: format!("{} track hits · {}", plays, format_duration(listened_ms)),
        value: listened_ms,
      })
      .collect(),
  )
}

fn aggregate_top_albums(listens: &[ListenRecord]) -> Vec<RankedEntry> {
  let mut totals: BTreeMap<String, (u64, u64)> = BTreeMap::new();
  for record in listens {
    if record.album.trim().is_empty() {
      continue;
    }
    let entry = totals.entry(record.album.clone()).or_insert((0, 0));
    entry.0 += record.listened_ms;
    entry.1 += 1;
  }

  sort_ranked_entries(
    totals
      .into_iter()
      .map(|(album, (listened_ms, plays))| RankedEntry {
        display: album,
        detail: format!("{} plays · {}", plays, format_duration(listened_ms)),
        value: listened_ms,
      })
      .collect(),
  )
}

fn aggregate_days(listens: &[ListenRecord]) -> Vec<RankedEntry> {
  let mut totals: BTreeMap<String, u64> = BTreeMap::new();
  for record in listens {
    let label = record.ended_at.format("%Y-%m-%d").to_string();
    *totals.entry(label).or_default() += record.listened_ms;
  }

  totals
    .into_iter()
    .rev()
    .take(10)
    .map(|(label, listened_ms)| RankedEntry {
      display: label,
      detail: format_duration(listened_ms),
      value: listened_ms,
    })
    .collect::<Vec<_>>()
    .into_iter()
    .rev()
    .collect()
}

fn aggregate_hours(listens: &[ListenRecord]) -> Vec<RankedEntry> {
  let mut totals: BTreeMap<u32, u64> = BTreeMap::new();
  for record in listens {
    let local_hour = record.ended_at.with_timezone(&chrono::Local).hour();
    *totals.entry(local_hour).or_default() += record.listened_ms;
  }

  (0..24)
    .map(|hour| RankedEntry {
      display: format!("{hour:02}:00"),
      detail: format_duration(*totals.get(&hour).unwrap_or(&0)),
      value: *totals.get(&hour).unwrap_or(&0),
    })
    .collect()
}

fn sort_ranked_entries(mut entries: Vec<RankedEntry>) -> Vec<RankedEntry> {
  entries.sort_by(|left, right| {
    right
      .value
      .cmp(&left.value)
      .then_with(|| left.display.cmp(&right.display))
  });
  entries.truncate(5);
  entries
}

fn render_ranked_entries(entries: &[RankedEntry], empty_label: &str) -> String {
  if entries.is_empty() {
    return format!(r#"<p class="subtle">{}</p>"#, escape_html(empty_label));
  }

  let items = entries
    .iter()
    .enumerate()
    .map(|(i, entry)| {
      let parts: Vec<&str> = entry.display.split(" - ").collect();
      let (title, subtitle) = if parts.len() == 2 {
        (parts[0], format!("<div class=\"entry-artist\">{}</div>", escape_html(parts[1])))
      } else {
        (entry.display.as_str(), "".to_string())
      };

      let detail_html = format!(r#"<div class="subtle">{}</div>"#, escape_html(&entry.detail));
      format!(
        "<li><span class=\"rank\">#{}</span><div class=\"entry-details\"><strong>{}</strong>{}{}</div></li>",
        i + 1,
        escape_html(title),
        subtitle,
        detail_html
      )
    })
    .collect::<Vec<_>>()
    .join("");
  format!("<ul class=\"ranked-list\">{items}</ul>")
}

fn render_recent_entries(listens: &[ListenRecord]) -> String {
  let recent_records = listens.iter().rev().take(5).collect::<Vec<_>>();

  if recent_records.is_empty() {
    return r#"<p class="subtle">No recent plays.</p>"#.to_string();
  }

  let items = recent_records
    .iter()
    .map(|record| {
      let local_time = record.ended_at.with_timezone(&chrono::Local);
      let time_str = local_time.format("%b %d, %H:%M").to_string();
      format!(
        "<li><span class=\"recent-icon\"><svg xmlns=\"http://www.w3.org/2000/svg\" width=\"14\" height=\"14\" viewBox=\"0 0 24 24\" fill=\"none\" stroke=\"currentColor\" stroke-width=\"2.5\" stroke-linecap=\"round\" stroke-linejoin=\"round\"><circle cx=\"12\" cy=\"12\" r=\"10\"></circle><polyline points=\"12 6 12 12 16 14\"></polyline></svg></span><div class=\"entry-details\"><strong>{}</strong><div class=\"entry-artist\">{}</div><div class=\"subtle\">listened at {}</div></div></li>",
        escape_html(&record.title),
        escape_html(&record.artists.join(", ")),
        escape_html(&time_str)
      )
    })
    .collect::<Vec<_>>()
    .join("");
  format!("<ul class=\"ranked-list\">{items}</ul>")
}

fn render_bar_entries(entries: &[RankedEntry]) -> String {
  if entries.is_empty() {
    return r#"<p class="subtle">No data yet.</p>"#.to_string();
  }

  let max_value = entries
    .iter()
    .map(|entry| entry.value)
    .max()
    .unwrap_or(1)
    .max(1);
  let rows = entries
    .iter()
    .map(|entry| {
      let width = (entry.value as f64 / max_value as f64) * 100.0;
      format!(
        r#"<div class="bar-row">
  <div class="bar-label"><span>{}</span><span>{}</span></div>
  <div class="bar"><span style="width:{:.2}%"></span></div>
</div>"#,
        escape_html(&entry.display),
        escape_html(&entry.detail),
        width
      )
    })
    .collect::<Vec<_>>()
    .join("");
  format!(r#"<div class="bars">{rows}</div>"#)
}

fn period_label(period: RecapPeriod) -> &'static str {
  match period {
    RecapPeriod::SevenDays => "Last 7 Days",
    RecapPeriod::ThirtyDays => "Last 30 Days",
    RecapPeriod::Month => "This Month",
    RecapPeriod::Year => "This Year",
    RecapPeriod::All => "All Time",
  }
}

fn format_duration(total_ms: u64) -> String {
  let total_minutes = total_ms / 1000 / 60;
  let hours = total_minutes / 60;
  let minutes = total_minutes % 60;
  if hours == 0 {
    format!("{minutes}m")
  } else {
    format!("{hours}h {minutes}m")
  }
}

fn escape_html(input: &str) -> String {
  input
    .replace('&', "&amp;")
    .replace('<', "&lt;")
    .replace('>', "&gt;")
    .replace('"', "&quot;")
    .replace('\'', "&#39;")
}

fn last_synced_file_path() -> Result<PathBuf> {
  let home = dirs::home_dir().ok_or_else(|| anyhow!("No $HOME directory found for history"))?;
  Ok(
    home
      .join(".config")
      .join("spotatui")
      .join(HISTORY_SUBDIR)
      .join("last_synced_at.txt"),
  )
}

/// Post the current now-playing track to the cloud dashboard.
/// Uses a shared HTTP client to reuse the connection pool across frequent calls.
async fn sync_now_playing_to_cloud(
  client: &reqwest::Client,
  sync_token: &str,
  title: &str,
  artists: &[String],
) -> Result<()> {
  let payload = NowPlayingPayload { title, artists };
  let response = client
    .post(NOW_PLAYING_SYNC_URL)
    .header("Authorization", format!("Bearer {}", sync_token))
    .json(&payload)
    .send()
    .await?;

  if !response.status().is_success() {
    let status = response.status();
    let body = response.text().await.unwrap_or_default();
    return Err(anyhow!("now-playing sync failed ({}): {}", status, body));
  }
  Ok(())
}

/// Clear the now-playing status when the TUI exits.
pub async fn clear_now_playing_from_cloud(sync_token: &str) -> Result<()> {
  let client = reqwest::Client::new();
  let response = client
    .delete(NOW_PLAYING_SYNC_URL)
    .header("Authorization", format!("Bearer {}", sync_token))
    .send()
    .await?;

  if !response.status().is_success() {
    let status = response.status();
    let body = response.text().await.unwrap_or_default();
    return Err(anyhow!("clear now-playing failed ({}): {}", status, body));
  }
  Ok(())
}

/// Internal helper used by the history collector's shared HTTP client.
async fn sync_history_to_cloud_with_client(
  client: &reqwest::Client,
  sync_token: &str,
) -> Result<()> {
  let path = last_synced_file_path()?;

  use chrono::TimeZone;
  let last_synced_at = match fs::read_to_string(&path) {
    Ok(content) => DateTime::parse_from_rfc3339(content.trim())
      .map(|dt| dt.with_timezone(&Utc))
      .unwrap_or_else(|_| Utc.timestamp_opt(0, 0).unwrap()),
    Err(_) => Utc.timestamp_opt(0, 0).unwrap(),
  };

  let listens = load_listens()?;
  let new_listens: Vec<&ListenRecord> = listens
    .iter()
    .filter(|record| record.ended_at > last_synced_at)
    .collect();

  if new_listens.is_empty() {
    log::info!("no new listening history records to sync");
    return Ok(());
  }

  let response = client
    .post(CLOUD_SYNC_URL)
    .header("Authorization", format!("Bearer {}", sync_token))
    .json(&new_listens)
    .send()
    .await?;

  if response.status().is_success() {
    if let Some(last_record) = new_listens.last() {
      fs::write(&path, last_record.ended_at.to_rfc3339())?;
    }
    log::info!(
      "successfully synchronized listening history to cloud ({} tracks)",
      new_listens.len()
    );
  } else {
    let status = response.status();
    let err_body = response.text().await.unwrap_or_default();
    log::warn!(
      "failed to synchronize history: {} (status {})",
      err_body,
      status
    );
    return Err(anyhow!("Sync failed: {}", err_body));
  }

  Ok(())
}

pub async fn sync_history_to_cloud(sync_token: &str) -> Result<()> {
  let path = last_synced_file_path()?;

  use chrono::TimeZone;
  let last_synced_at = match fs::read_to_string(&path) {
    Ok(content) => DateTime::parse_from_rfc3339(content.trim())
      .map(|dt| dt.with_timezone(&Utc))
      .unwrap_or_else(|_| Utc.timestamp_opt(0, 0).unwrap()),
    Err(_) => Utc.timestamp_opt(0, 0).unwrap(),
  };

  let listens = load_listens()?;
  let new_listens: Vec<&ListenRecord> = listens
    .iter()
    .filter(|record| record.ended_at > last_synced_at)
    .collect();

  if new_listens.is_empty() {
    log::info!("no new listening history records to sync");
    return Ok(());
  }

  let client = reqwest::Client::new();

  let response = client
    .post(CLOUD_SYNC_URL)
    .header("Authorization", format!("Bearer {}", sync_token))
    .json(&new_listens)
    .send()
    .await?;

  if response.status().is_success() {
    if let Some(last_record) = new_listens.last() {
      fs::write(&path, last_record.ended_at.to_rfc3339())?;
    }
    log::info!(
      "successfully synchronized listening history to cloud ({} tracks)",
      new_listens.len()
    );
  } else {
    let status = response.status();
    let err_body = response.text().await.unwrap_or_default();
    log::warn!(
      "failed to synchronize history: {} (status {})",
      err_body,
      status
    );
    return Err(anyhow!("Sync failed: {}", err_body));
  }

  Ok(())
}

#[cfg(test)]
mod tests {
  use super::*;
  use chrono::TimeZone;

  fn record_at(day: u32, listened_ms: u64, qualified: bool) -> ListenRecord {
    let timestamp = Utc.with_ymd_and_hms(2026, 5, day, 12, 0, 0).unwrap();
    ListenRecord {
      started_at: timestamp,
      ended_at: timestamp,
      listened_ms,
      duration_ms: 180_000,
      qualified,
      title: format!("Track {day}"),
      artists: vec!["Artist".to_string()],
      album: "Album".to_string(),
      item_kind: HistoryItemKind::Track,
      item_id: Some(format!("id-{day}")),
      item_uri: Some(format!("spotify:track:id-{day}")),
      context_uri: None,
      source: HistoryPlaybackSource::NativeContext,
    }
  }

  #[test]
  fn qualification_requires_minimum_duration_and_progress() {
    assert!(!qualifies_listen(30_000, 15_000));
    assert!(!qualifies_listen(180_000, 80_000));
    assert!(qualifies_listen(180_000, 90_000));
    assert!(qualifies_listen(800_000, 240_000));
  }

  #[test]
  fn all_time_filter_keeps_only_qualified_records() {
    let records = vec![record_at(20, 100_000, false), record_at(21, 120_000, true)];
    let filtered = filter_listens_for_period(&records, RecapPeriod::All);
    assert_eq!(filtered.len(), 1);
    assert_eq!(filtered[0].title, "Track 21");
  }

  #[test]
  fn cloud_sync_uses_public_spotatui_domain() {
    assert_eq!(CLOUD_SYNC_URL, "https://spotatui.com/api/sync");
  }

  #[test]
  fn now_playing_uses_public_spotatui_domain() {
    assert_eq!(NOW_PLAYING_SYNC_URL, "https://spotatui.com/api/sync/now-playing");
  }
}
