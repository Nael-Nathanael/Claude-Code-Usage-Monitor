//! One quota file per ccd profile, in the claude-limits-plasmoid format:
//! `%LOCALAPPDATA%\claude-limits\claude-N.json`. WSL writes them, from the
//! statusline and the claude-limits service. The widget only reads them, so it
//! never touches a login or the network, and never wakes WSL.
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use serde::Deserialize;
use windows::core::HSTRING;
use windows::Win32::Foundation::WAIT_OBJECT_0;
use windows::Win32::Storage::FileSystem::{
    FindFirstChangeNotificationW, FindNextChangeNotification, FILE_NOTIFY_CHANGE_FILE_NAME,
    FILE_NOTIFY_CHANGE_LAST_WRITE,
};
use windows::Win32::System::Threading::WaitForSingleObject;

use crate::accounts::{AccountProfile, ProviderAccounts};
use crate::diagnose;
use crate::models::{UsageData, UsageSection};
use crate::poller::PollError;

pub const ACCOUNT_PREFIX: &str = "ccd_";
const PROFILE_PREFIX: &str = ".claude-profile-";
/// The service renews a record within 10 minutes while WSL runs.
const OLD_AFTER_SECS: u64 = 900;
const AGE_CHECK_MS: u32 = 60_000;
/// A write lands as a temporary file then a rename: one poll covers both.
const SETTLE: Duration = Duration::from_millis(200);

#[derive(Clone, Debug, PartialEq, Deserialize)]
struct LimitWindow {
    used_percentage: f64,
    resets_at: Option<u64>,
}

#[derive(Clone, Debug, Default, PartialEq, Deserialize)]
#[serde(default)]
struct LimitFile {
    five_hour: Option<LimitWindow>,
    seven_day: Option<LimitWindow>,
    error: Option<String>,
    saved_at: u64,
    hidden: bool,
}

fn output_dir() -> Option<PathBuf> {
    Some(crate::known_folders::local_app_data_dir()?.join("claude-limits"))
}

fn file_name(number: u32) -> String {
    format!("claude-{number}.json")
}

fn file_number(name: &str) -> Option<u32> {
    name.strip_prefix("claude-")?
        .strip_suffix(".json")?
        .parse()
        .ok()
}

fn epoch_secs(time: SystemTime) -> u64 {
    time.duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}

fn read_file(path: &Path) -> Option<LimitFile> {
    serde_json::from_slice(&std::fs::read(path).ok()?).ok()
}

fn section(window: Option<&LimitWindow>) -> UsageSection {
    window
        .map(|window| UsageSection {
            available: true,
            percentage: window.used_percentage,
            resets_at: window
                .resets_at
                .map(|secs| UNIX_EPOCH + Duration::from_secs(secs)),
        })
        .unwrap_or_default()
}

fn is_old(state: &LimitFile, now: u64) -> bool {
    now.saturating_sub(state.saved_at) > OLD_AFTER_SECS
}

fn usage_from_file(state: &LimitFile, now: u64) -> Result<UsageData, PollError> {
    if state.hidden {
        return Err(PollError::NoCredentials);
    }
    if state.five_hour.is_none() && state.seven_day.is_none() {
        return Err(PollError::RequestFailed);
    }
    Ok(UsageData {
        session: section(state.five_hour.as_ref()),
        weekly: section(state.seven_day.as_ref()),
        stale: state.error.is_some() || is_old(state, now),
        ..Default::default()
    })
}

fn read_usage_at(path: &Path, now: u64) -> Result<UsageData, PollError> {
    let state = read_file(path).ok_or(PollError::RequestFailed)?;
    if let Some(error) = &state.error {
        diagnose::log(format!("{} reports: {error}", path.display()));
    }
    usage_from_file(&state, now)
}

/// The quota file behind a profile. `None` for a profile ccd did not make.
pub fn file_for(profile: &AccountProfile) -> Option<PathBuf> {
    let number = match profile.id.strip_prefix(ACCOUNT_PREFIX) {
        Some(number) => number.parse().ok()?,
        None if profile.id == "default" => 1,
        None => return None,
    };
    Some(output_dir()?.join(file_name(number)))
}

/// The widget's read path. `None` means this source is not a quota file.
pub fn read_usage(path: &Path) -> Option<Result<UsageData, PollError>> {
    (path.parent()? == output_dir()?).then(|| read_usage_at(path, epoch_secs(SystemTime::now())))
}

/// Profiles with a quota file, except those the service marked as logged out.
fn visible_numbers(dir: &Path) -> Vec<u32> {
    let entries = std::fs::read_dir(dir).into_iter().flatten().flatten();
    let mut numbers: Vec<u32> = entries
        .filter_map(|entry| {
            let number = file_number(entry.file_name().to_str()?)?;
            (!read_file(&entry.path())?.hidden).then_some(number)
        })
        .collect();
    numbers.sort_unstable();
    numbers
}

fn any_old(dir: &Path, now: u64) -> bool {
    visible_numbers(dir).into_iter().any(|number| {
        read_file(&dir.join(file_name(number))).is_some_and(|state| is_old(&state, now))
    })
}

fn sync_profiles(accounts: &mut ProviderAccounts, found: &[u32], wsl_home: &Path) {
    let id = |number: u32| format!("{ACCOUNT_PREFIX}{number}");
    accounts.profiles.retain(|profile| {
        !profile.id.starts_with(ACCOUNT_PREFIX)
            || found.iter().any(|number| id(*number) == profile.id)
    });
    for number in found.iter().copied().filter(|number| *number > 1) {
        if !accounts
            .profiles
            .iter()
            .any(|profile| profile.id == id(number))
        {
            accounts.profiles.push(AccountProfile {
                id: id(number),
                name: format!("ccd {number}"),
                config_dir: wsl_home
                    .join(format!("{PROFILE_PREFIX}{number}"))
                    .to_string_lossy()
                    .into_owned(),
                enabled: true,
                ..Default::default()
            });
        }
    }
    accounts.normalize();
}

/// Keep one account per ccd profile. A profile the user disabled stays disabled.
pub fn sync_accounts(accounts: &mut ProviderAccounts) {
    if let Some(dir) = output_dir() {
        sync_profiles(accounts, &visible_numbers(&dir), &crate::poller::wsl_home());
    }
}

/// Call `on_change` when a quota file changes, or when a record turns old with
/// no write at all, as happens once WSL stops.
// shortcut: each call polls every enabled provider, fine while only Claude is shown.
pub fn watch(on_change: impl Fn() + Send + 'static) {
    std::thread::spawn(move || {
        let Some(dir) = output_dir() else {
            return;
        };
        if let Err(error) = std::fs::create_dir_all(&dir) {
            diagnose::log_error("unable to create the claude-limits folder", error);
            return;
        }
        let handle = match unsafe {
            FindFirstChangeNotificationW(
                &HSTRING::from(dir.as_os_str()),
                false,
                FILE_NOTIFY_CHANGE_FILE_NAME | FILE_NOTIFY_CHANGE_LAST_WRITE,
            )
        } {
            Ok(handle) => handle,
            Err(error) => {
                diagnose::log_error("unable to watch the claude-limits folder", error);
                return;
            }
        };
        let mut was_old = false;
        loop {
            let changed = unsafe { WaitForSingleObject(handle, AGE_CHECK_MS) } == WAIT_OBJECT_0;
            if changed {
                std::thread::sleep(SETTLE);
                if let Err(error) = unsafe { FindNextChangeNotification(handle) } {
                    diagnose::log_error("claude-limits watch stopped", error);
                    return;
                }
            }
            let old = any_old(&dir, epoch_secs(SystemTime::now()));
            if changed || old != was_old {
                on_change();
            }
            was_old = old;
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    const NOW: u64 = 1_800_000_000;

    fn write(dir: &Path, number: u32, json: &str) {
        std::fs::write(dir.join(file_name(number)), json).unwrap();
    }

    fn state(saved_at: u64, error: Option<&str>) -> LimitFile {
        LimitFile {
            five_hour: Some(LimitWindow {
                used_percentage: 40.0,
                resets_at: None,
            }),
            saved_at,
            error: error.map(str::to_string),
            ..Default::default()
        }
    }

    #[test]
    fn profiles_map_to_their_file() {
        let profile = |id: &str| AccountProfile {
            id: id.into(),
            ..Default::default()
        };
        let name =
            |id: &str| file_for(&profile(id)).map(|path| path.file_name().unwrap().to_owned());
        assert_eq!(name("default").unwrap(), "claude-1.json");
        assert_eq!(name("ccd_4").unwrap(), "claude-4.json");
        assert_eq!(name("ccd_x"), None);
        assert_eq!(name("work"), None);
    }

    #[test]
    fn a_recent_record_is_current_and_an_old_one_is_marked() {
        let reading = |saved_at| usage_from_file(&state(saved_at, None), NOW).unwrap();
        assert!(!reading(NOW - OLD_AFTER_SECS).stale);
        assert!(reading(NOW - OLD_AFTER_SECS - 1).stale);
        assert_eq!(reading(NOW).session.percentage, 40.0);
    }

    #[test]
    fn a_reported_error_keeps_the_figures_and_marks_them() {
        let reading = usage_from_file(&state(NOW, Some("HTTP 429")), NOW).unwrap();
        assert_eq!(reading.session.percentage, 40.0);
        assert!(reading.stale);
    }

    #[test]
    fn missing_figures_and_logged_out_files_are_errors() {
        assert_eq!(
            usage_from_file(&LimitFile::default(), NOW),
            Err(PollError::RequestFailed)
        );
        let hidden = LimitFile {
            hidden: true,
            ..Default::default()
        };
        assert_eq!(usage_from_file(&hidden, NOW), Err(PollError::NoCredentials));
        let root = tempfile::tempdir().unwrap();
        assert_eq!(
            read_usage_at(&root.path().join("claude-2.json"), NOW),
            Err(PollError::RequestFailed)
        );
    }

    #[test]
    fn statusline_files_without_service_fields_are_readable() {
        let state: LimitFile = serde_json::from_str(
            r#"{"five_hour":{"used_percentage":12,"resets_at":1792042200},
            "seven_day":null,"error":null,"saved_at":1792026690}"#,
        )
        .unwrap();
        let reading = usage_from_file(&state, 1792026690).unwrap();
        assert_eq!(reading.session.percentage, 12.0);
        assert!(!reading.weekly.available);
    }

    #[test]
    fn logged_out_and_unrelated_files_are_not_profiles() {
        let root = tempfile::tempdir().unwrap();
        let fresh = format!(r#"{{"seven_day":null,"saved_at":{NOW}}}"#);
        write(root.path(), 10, &fresh);
        write(root.path(), 2, &fresh);
        write(root.path(), 5, r#"{"hidden":true}"#);
        std::fs::write(root.path().join("codex.json"), "{}").unwrap();
        std::fs::write(root.path().join("claude-3.json.77.tmp"), "{}").unwrap();
        assert_eq!(visible_numbers(root.path()), [2, 10]);
    }

    #[test]
    fn only_a_visible_old_record_counts_as_old() {
        let root = tempfile::tempdir().unwrap();
        write(root.path(), 2, &format!(r#"{{"saved_at":{NOW}}}"#));
        write(root.path(), 5, r#"{"hidden":true,"saved_at":0}"#);
        assert!(!any_old(root.path(), NOW));
        assert!(any_old(root.path(), NOW + OLD_AFTER_SECS + 1));
    }

    #[test]
    fn accounts_follow_files_and_keep_user_choices() {
        let home = Path::new("C:\\wsl-home");
        let mut accounts = ProviderAccounts::default();
        sync_profiles(&mut accounts, &[1, 2], home);
        let ids: Vec<_> = accounts.profiles.iter().map(|p| p.id.as_str()).collect();
        assert_eq!(ids, ["default", "ccd_2"]);
        assert!(accounts.profiles[1]
            .config_dir
            .ends_with(".claude-profile-2"));
        accounts.profiles[1].enabled = false;
        sync_profiles(&mut accounts, &[1, 2], home);
        assert!(!accounts.profiles[1].enabled);
        sync_profiles(&mut accounts, &[1], home);
        assert_eq!(accounts.profiles.len(), 1);
    }
}
