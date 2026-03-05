use std::path::PathBuf;
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::Result;
use self_update::cargo_crate_version;
use serde::{Deserialize, Serialize};

/// Information about an available update
#[allow(dead_code)]
pub struct UpdateInfo {
  pub current_version: String,
  pub latest_version: String,
}

/// Parse a human-readable delay string into seconds.
/// Accepts: "0", "30s", "10m", "2h", "7d"
/// Returns 0 for unrecognized/empty input (= no delay).
pub fn parse_delay_secs(s: &str) -> Result<u64, String> {
  let s = s.trim();
  if s == "0" || s.is_empty() {
    return Ok(0);
  }
  if let Some(n) = s.strip_suffix('d') {
    return n
      .trim()
      .parse::<u64>()
      .map(|v| v * 86400)
      .map_err(|_| format!("Invalid days value"));
  }
  if let Some(n) = s.strip_suffix('h') {
    return n
      .trim()
      .parse::<u64>()
      .map(|v| v * 3600)
      .map_err(|_| format!("Invalid hours value"));
  }
  if let Some(n) = s.strip_suffix('m') {
    return n
      .trim()
      .parse::<u64>()
      .map(|v| v * 60)
      .map_err(|_| format!("Invalid minutes value"));
  }
  if let Some(n) = s.strip_suffix('s') {
    return n
      .trim()
      .parse::<u64>()
      .map_err(|_| format!("Invalid seconds value"));
  }
  // bare number treated as seconds
  s.parse::<u64>()
    .map_err(|_| format!("Invalid numeric value or unknown suffix"))
}

#[derive(Debug, Serialize, Deserialize)]
struct UpdatePendingState {
  pending_version: String,
  detected_at_secs: u64,
}

fn update_state_file_path() -> Option<PathBuf> {
  use crate::core::user_config::UserConfig;
  UserConfig::get_app_config_dir().map(|dir| dir.join("update_pending.json"))
}

pub enum UpdateOutcome {
  /// Update was installed; contains the new version string
  Installed(String),
  /// Update is available but waiting for the delay to elapse
  Pending {
    version: String,
    secs_remaining: u64,
  },
  /// Already up to date
  UpToDate,
}

/// Check for updates in the background (non-blocking)
/// Returns Some(UpdateInfo) if an update is available, None if up to date
#[allow(dead_code)]
pub fn check_for_update_silent() -> Option<UpdateInfo> {
  // ============ TESTING: Uncomment below to simulate update ============
  // return Some(UpdateInfo {
  //   current_version: env!("CARGO_PKG_VERSION").to_string(),
  //   latest_version: "99.0.0".to_string(),
  // });
  // =====================================================================

  let current_version = cargo_crate_version!();

  let status = self_update::backends::github::Update::configure()
    .repo_owner("LargeModGames")
    .repo_name("spotatui")
    .bin_name("spotatui")
    .current_version(current_version)
    .build()
    .ok()?;

  let latest = status.get_latest_release().ok()?;
  let latest_version = latest.version.trim_start_matches('v').to_string();

  if latest_version != current_version {
    Some(UpdateInfo {
      current_version: current_version.to_string(),
      latest_version,
    })
  } else {
    None
  }
}

/// Convert self_update::Status to UpdateOutcome
fn status_to_outcome(status: self_update::Status) -> UpdateOutcome {
  match status {
    self_update::Status::UpToDate(_) => UpdateOutcome::UpToDate,
    self_update::Status::Updated(v) => UpdateOutcome::Installed(v),
  }
}

/// Silently check for, download, and install an update, respecting an optional delay.
/// Returns Ok(UpdateOutcome::Installed(v)) if updated, Ok(UpdateOutcome::Pending{..}) if
/// a new version was detected but the delay hasn't elapsed yet, Ok(UpdateOutcome::UpToDate)
/// if already current, or Err on failure.
pub fn install_update_silent(delay_secs: u64) -> Result<UpdateOutcome> {
  let current_version = cargo_crate_version!();

  let status = self_update::backends::github::Update::configure()
    .repo_owner("LargeModGames")
    .repo_name("spotatui")
    .bin_name("spotatui")
    .show_download_progress(false)
    .no_confirm(true)
    .current_version(current_version)
    .build()?;

  let latest = status.get_latest_release()?;
  let latest_version = latest.version.trim_start_matches('v').to_string();

  if latest_version == current_version {
    // Up to date — clear any stale pending state
    if let Some(path) = update_state_file_path() {
      let _ = std::fs::remove_file(path);
    }
    return Ok(UpdateOutcome::UpToDate);
  }

  // New version available — apply delay logic
  if delay_secs == 0 {
    // No delay: install immediately
    let result = status.update()?;
    return Ok(status_to_outcome(result));
  }

  // Delay > 0: check state file
  let now_secs = SystemTime::now()
    .duration_since(UNIX_EPOCH)
    .unwrap_or_default()
    .as_secs();

  let state_path = update_state_file_path();
  let mut pending = state_path.as_ref().and_then(|p| {
    std::fs::read_to_string(p)
      .ok()
      .and_then(|s| serde_json::from_str::<UpdatePendingState>(&s).ok())
  });

  // If pending is for a different version (newer release arrived), reset the timer
  if let Some(ref p) = pending {
    if p.pending_version != latest_version {
      pending = None;
    }
  }

  let state = match pending {
    Some(p) => p,
    None => {
      // First detection: write state file and notify
      let s = UpdatePendingState {
        pending_version: latest_version.clone(),
        detected_at_secs: now_secs,
      };
      if let Some(ref path) = state_path {
        if let Some(parent) = path.parent() {
          let _ = std::fs::create_dir_all(parent);
        }
        let _ = std::fs::write(
          path,
          serde_json::to_string(&s).expect("failed to serialize update state"),
        );
      }
      s
    }
  };

  let elapsed = now_secs.saturating_sub(state.detected_at_secs);
  if elapsed >= delay_secs {
    // Delay elapsed — install
    let result = status.update()?;
    // Clean up state file
    if let Some(ref path) = state_path {
      let _ = std::fs::remove_file(path);
    }
    Ok(status_to_outcome(result))
  } else {
    let secs_remaining = delay_secs - elapsed;
    Ok(UpdateOutcome::Pending {
      version: latest_version,
      secs_remaining,
    })
  }
}

/// Check for updates and optionally install the latest version
pub fn check_for_update(do_update: bool) -> Result<()> {
  let current_version = cargo_crate_version!();

  println!("Current version: v{}", current_version);
  println!("Checking for updates...");

  let status = self_update::backends::github::Update::configure()
    .repo_owner("LargeModGames")
    .repo_name("spotatui")
    .bin_name("spotatui")
    .show_download_progress(true)
    .current_version(current_version)
    .no_confirm(false)
    .build()?;

  let latest = status.get_latest_release()?;

  // Remove 'v' prefix if present for comparison
  let latest_version = latest.version.trim_start_matches('v');

  if latest_version == current_version {
    println!("✓ You are already running the latest version!");
    return Ok(());
  }

  println!("New version available: v{}", latest_version);

  if do_update {
    println!("\nDownloading and installing update...");

    let result = status.update()?;
    match result {
      self_update::Status::UpToDate(_) => {
        println!("✓ Already up to date!");
      }
      self_update::Status::Updated(v) => {
        println!("✓ Successfully updated to v{}!", v);
        println!("\nPlease restart spotatui to use the new version.");
      }
    }
  } else {
    println!("\nRun `spotatui update --install` to install the update.");
  }

  Ok(())
}

#[cfg(test)]
mod tests {
  use super::parse_delay_secs;

  #[test]
  fn test_parse_delay_secs() {
    assert_eq!(parse_delay_secs("0"), Ok(0));
    assert_eq!(parse_delay_secs(""), Ok(0));
    assert_eq!(parse_delay_secs("7d"), Ok(7 * 86400));
    assert_eq!(parse_delay_secs("2h"), Ok(2 * 3600));
    assert_eq!(parse_delay_secs("10m"), Ok(10 * 60));
    assert_eq!(parse_delay_secs("30s"), Ok(30));
    assert_eq!(parse_delay_secs("120"), Ok(120));
    assert!(parse_delay_secs("bogus").is_err());
  }
}
