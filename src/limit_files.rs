//! One quota file per ccd profile, in the claude-limits-plasmoid format:
//! `%LOCALAPPDATA%\claude-limits\claude-N.json`. The updater writes them and
//! the widget reads them, so the read side never touches a token. Files hold
//! figures only. Any other tool may read them, or write them as a statusline does.
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};

use crate::accounts::{AccountProfile, ProviderAccounts};
use crate::diagnose;
use crate::models::{CreditsSection, UsageData, UsageLimit, UsageSection};
use crate::poller::PollError;

const MAX_AGE_SECS: u64 = 300;
pub const UPDATE_INTERVAL: Duration = Duration::from_secs(60);
const ACCOUNT_PREFIX: &str = "ccd_";
const PROFILE_PREFIX: &str = ".claude-profile-";
const CREDENTIALS: &str = ".credentials.json";
const EXPIRED: &str = "token expired";

/// The widget poll and the updater thread must not fetch the same file twice.
static UPDATE_LOCK: Mutex<()> = Mutex::new(());

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
struct LimitWindow {
    used_percentage: f64,
    resets_at: Option<u64>,
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
struct LimitFile {
    five_hour: Option<LimitWindow>,
    seven_day: Option<LimitWindow>,
    tier: String,
    error: Option<String>,
    saved_at: u64,
    checked_at: u64,
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    hidden: bool,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    limits: Vec<UsageLimit>,
    #[serde(skip_serializing_if = "Option::is_none")]
    credits: Option<CreditsSection>,
}

#[derive(Clone, Debug, PartialEq)]
struct Profile {
    number: u32,
    dir: PathBuf,
}

struct Login {
    token: String,
    expired: bool,
    tier: String,
}

fn output_dir() -> Option<PathBuf> {
    Some(crate::known_folders::local_app_data_dir()?.join("claude-limits"))
}

fn file_name(number: u32) -> String {
    format!("claude-{number}.json")
}

fn epoch_secs(time: SystemTime) -> u64 {
    time.duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}

/// Profile 1 is `~/.claude`; profile N is ccd's `~/.claude-profile-N`.
fn discover(home: &Path) -> Vec<Profile> {
    let mut profiles = vec![Profile {
        number: 1,
        dir: home.join(".claude"),
    }];
    let entries = std::fs::read_dir(home).into_iter().flatten().flatten();
    profiles.extend(entries.filter_map(|entry| {
        let name = entry.file_name();
        let number = name.to_str()?.strip_prefix(PROFILE_PREFIX)?.parse().ok()?;
        (number > 1 && entry.path().is_dir()).then(|| Profile {
            number,
            dir: entry.path(),
        })
    }));
    profiles.sort_by_key(|profile| profile.number);
    profiles
}

/// Profiles from every home. A number found in two homes goes to the one with
/// a login, the Windows home on a tie.
fn discover_all(homes: &[PathBuf]) -> Vec<Profile> {
    let mut profiles: Vec<Profile> = homes.iter().flat_map(|home| discover(home)).collect();
    profiles.sort_by_key(|profile| (profile.number, !profile.dir.join(CREDENTIALS).exists()));
    profiles.dedup_by_key(|profile| profile.number);
    profiles
}

fn profile_number(home: &Path, credentials: &Path) -> Option<u32> {
    let dir = credentials.parent()?;
    if credentials.file_name()? != CREDENTIALS || dir.parent()? != home {
        return None;
    }
    let name = dir.file_name()?.to_str()?;
    if name == ".claude" {
        return Some(1);
    }
    name.strip_prefix(PROFILE_PREFIX)?
        .parse()
        .ok()
        .filter(|number| *number > 1)
}

fn read_login(path: &Path, now: u64) -> Option<Login> {
    let json: serde_json::Value = serde_json::from_slice(&std::fs::read(path).ok()?).ok()?;
    let oauth = json.get("claudeAiOauth")?;
    let token = oauth.get("accessToken")?.as_str()?.trim();
    let tier = oauth
        .get("rateLimitTier")
        .and_then(|tier| tier.as_str())
        .unwrap_or_default()
        .trim_start_matches("default_claude_")
        .replace('_', " ");
    // Logins keep the old tier ids after Anthropic renamed the plans.
    let tier = match tier.as_str() {
        "max 20x" => "ultra".to_string(),
        "max 5x" => "max".to_string(),
        _ => tier,
    };
    (!token.is_empty()).then(|| Login {
        token: token.to_string(),
        expired: oauth
            .get("expiresAt")
            .and_then(|value| value.as_u64())
            .is_some_and(|expires_at| expires_at / 1000 <= now),
        tier,
    })
}

fn read_file(path: &Path) -> Option<LimitFile> {
    serde_json::from_slice(&std::fs::read(path).ok()?).ok()
}

fn write_file(path: &Path, state: &LimitFile) -> std::io::Result<()> {
    let temporary = path.with_extension(format!("{}.tmp", std::process::id()));
    std::fs::write(&temporary, serde_json::to_vec(state)?)?;
    std::fs::rename(&temporary, path).inspect_err(|_| {
        let _ = std::fs::remove_file(&temporary);
    })
}

fn modified_secs(path: &Path) -> Option<u64> {
    Some(epoch_secs(std::fs::metadata(path).ok()?.modified().ok()?))
}

fn window(section: &UsageSection) -> Option<LimitWindow> {
    section.available.then(|| LimitWindow {
        used_percentage: section.percentage,
        resets_at: section.resets_at.map(epoch_secs),
    })
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

fn error_text(error: PollError) -> String {
    match error {
        PollError::HttpStatus(code) => format!("HTTP {code}"),
        other => format!("{other:?}"),
    }
}

/// Tokens are never refreshed here: Claude Code owns them, and its refresh path
/// sends a real prompt. An idle profile keeps its last figures beside an error.
fn update_profile(
    out: &Path,
    profile: &Profile,
    force: bool,
    now: u64,
    fetch: impl FnOnce(&str) -> Result<Option<UsageData>, PollError>,
) -> std::io::Result<()> {
    let path = out.join(file_name(profile.number));
    let previous = read_file(&path).unwrap_or_default();
    let credentials = profile.dir.join(CREDENTIALS);
    let relogin = modified_secs(&credentials).is_some_and(|time| time > previous.checked_at);
    let age = now.saturating_sub(previous.saved_at.max(previous.checked_at));
    if !force && !relogin && age < MAX_AGE_SECS {
        return Ok(());
    }
    let mut state = if previous.hidden {
        LimitFile::default()
    } else {
        previous
    };
    state.checked_at = now;
    match read_login(&credentials, now) {
        None => {
            state = LimitFile {
                hidden: true,
                checked_at: now,
                ..Default::default()
            }
        }
        Some(login) if login.expired => {
            state.tier = login.tier;
            state.error = Some(format!("{EXPIRED}, run ccd {} to refresh", profile.number));
        }
        Some(login) => {
            state.tier = login.tier;
            match fetch(&login.token) {
                Ok(Some(usage)) => {
                    state.five_hour = window(&usage.session);
                    state.seven_day = window(&usage.weekly);
                    state.limits = usage.limits;
                    state.credits = usage.credits;
                    state.error = None;
                    state.saved_at = now;
                }
                Ok(None) => state.error = Some("usage endpoint unavailable".into()),
                Err(error) => state.error = Some(error_text(error)),
            }
        }
    }
    write_file(&path, &state)
}

/// Fetch every profile whose file is missing, old, or older than its login.
pub fn update_all(force: bool) {
    let _guard = UPDATE_LOCK
        .lock()
        .unwrap_or_else(|error| error.into_inner());
    let (Some(home), Some(out)) = (crate::known_folders::home_dir(), output_dir()) else {
        return;
    };
    if let Err(error) = std::fs::create_dir_all(&out) {
        diagnose::log_error("unable to create the claude-limits folder", error);
        return;
    }
    let now = epoch_secs(SystemTime::now());
    // Listing a WSL home over `\\wsl.localhost` would start a stopped distro.
    let wsl_up = crate::poller::wsl_running();
    let mut homes = vec![home];
    if wsl_up {
        homes.push(crate::poller::wsl_home());
    }
    update_homes(
        &out,
        &homes,
        wsl_up,
        force,
        now,
        crate::poller::fetch_claude_usage,
    );
}

fn update_homes(
    out: &Path,
    homes: &[PathBuf],
    wsl_up: bool,
    force: bool,
    now: u64,
    fetch: impl Fn(&str) -> Result<Option<UsageData>, PollError>,
) {
    for profile in discover_all(homes) {
        // With the distro stopped, a missing login proves nothing: its profiles
        // are just out of sight, and `hidden` would erase their last figures.
        if !wsl_up && !profile.dir.join(CREDENTIALS).exists() {
            continue;
        }
        if let Err(error) = update_profile(out, &profile, force, now, &fetch) {
            diagnose::log_error("unable to write a claude-limits file", error);
        }
    }
}

fn usage_from_file(state: &LimitFile, login_changed: bool) -> Result<UsageData, PollError> {
    if state.five_hour.is_none() && state.seven_day.is_none() {
        let expired = state
            .error
            .as_deref()
            .is_some_and(|error| error.starts_with(EXPIRED));
        // A new login outdates the file. Reporting expiry then would pause the
        // account on the new login before the updater has fetched it.
        return Err(if expired && !login_changed {
            PollError::TokenExpired
        } else {
            PollError::RequestFailed
        });
    }
    Ok(UsageData {
        session: section(state.five_hour.as_ref()),
        weekly: section(state.seven_day.as_ref()),
        limits: state.limits.clone(),
        credits: state.credits.clone(),
        stale: state.error.is_some(),
        ..Default::default()
    })
}

fn read_usage_in(
    out: &Path,
    homes: &[PathBuf],
    credentials: &Path,
    asleep: bool,
) -> Option<Result<UsageData, PollError>> {
    let number = homes
        .iter()
        .find_map(|home| profile_number(home, credentials))?;
    // Without a CLI login the account may still have a desktop app login.
    // A stopped distro is not touched: its last figures stand.
    if !asleep && !credentials.exists() {
        return None;
    }
    let Some(state) = read_file(&out.join(file_name(number))) else {
        return Some(Err(PollError::RequestFailed));
    };
    if state.hidden {
        return None;
    }
    if let Some(error) = &state.error {
        diagnose::log(format!("claude-{number}.json reports: {error}"));
    }
    let login_changed =
        !asleep && modified_secs(credentials).is_some_and(|time| time > state.checked_at);
    let mut reading = usage_from_file(&state, login_changed);
    if let (true, Ok(usage)) = (asleep, &mut reading) {
        usage.stale = true;
    }
    Some(reading)
}

/// The widget's read path. `None` means this login is not a ccd profile.
pub fn read_usage(credentials: &Path) -> Option<Result<UsageData, PollError>> {
    read_usage_in(
        &output_dir()?,
        &[crate::known_folders::home_dir()?, crate::poller::wsl_home()],
        credentials,
        crate::poller::wsl_path_asleep(credentials),
    )
}

fn sync_profiles(accounts: &mut ProviderAccounts, found: &[Profile], wsl_up: bool) {
    let id = |number: u32| format!("{ACCOUNT_PREFIX}{number}");
    accounts.profiles.retain(|profile| {
        !profile.id.starts_with(ACCOUNT_PREFIX)
            || found.iter().any(|found| id(found.number) == profile.id)
            // A stopped distro's profiles are not listed, not gone.
            || (!wsl_up && crate::poller::is_wsl_unc(Path::new(&profile.config_dir)))
    });
    // API-key profiles have no login file, so they have no quota to show.
    for found in found
        .iter()
        .filter(|found| found.number > 1 && found.dir.join(CREDENTIALS).exists())
    {
        if !accounts
            .profiles
            .iter()
            .any(|profile| profile.id == id(found.number))
        {
            accounts.profiles.push(AccountProfile {
                id: id(found.number),
                name: format!("ccd {}", found.number),
                config_dir: found.dir.to_string_lossy().into_owned(),
                enabled: true,
                ..Default::default()
            });
        }
    }
    accounts.normalize();
}

/// Keep one account per ccd profile. A profile the user disabled stays disabled.
pub fn sync_accounts(accounts: &mut ProviderAccounts) {
    if let Some(home) = crate::known_folders::home_dir() {
        let wsl_up = crate::poller::wsl_running();
        let mut homes = vec![home];
        if wsl_up {
            homes.push(crate::poller::wsl_home());
        }
        sync_profiles(accounts, &discover_all(&homes), wsl_up);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const NOW: u64 = 1_800_000_000;

    fn login(dir: &Path, expires_at: u64) {
        std::fs::create_dir_all(dir).unwrap();
        std::fs::write(
            dir.join(CREDENTIALS),
            format!(
                r#"{{"claudeAiOauth":{{"accessToken":"fixture","expiresAt":{expires_at},
                "rateLimitTier":"default_claude_max_20x"}}}}"#
            ),
        )
        .unwrap();
    }

    fn usage(percentage: f64) -> UsageData {
        UsageData {
            session: UsageSection {
                available: true,
                percentage,
                resets_at: Some(UNIX_EPOCH + Duration::from_secs(NOW + 60)),
            },
            ..Default::default()
        }
    }

    fn profile(root: &Path) -> Profile {
        Profile {
            number: 2,
            dir: root.join(".claude-profile-2"),
        }
    }

    #[test]
    fn discovery_follows_the_ccd_layout() {
        let root = tempfile::tempdir().unwrap();
        for name in [
            ".claude-profile-10",
            ".claude-profile-2",
            ".claude-profile-1",
            ".claude-profile-x",
            ".claude-work",
        ] {
            std::fs::create_dir(root.path().join(name)).unwrap();
        }
        std::fs::write(root.path().join(".claude-profile-3"), "a file").unwrap();
        let numbers: Vec<_> = discover(root.path()).iter().map(|p| p.number).collect();
        assert_eq!(numbers, [1, 2, 10]);
    }

    #[test]
    fn only_ccd_login_paths_map_to_a_file() {
        let home = Path::new("C:\\Users\\Test");
        for (path, expected) in [
            ("C:\\Users\\Test\\.claude\\.credentials.json", Some(1)),
            (
                "C:\\Users\\Test\\.claude-profile-4\\.credentials.json",
                Some(4),
            ),
            (
                "C:\\Users\\Test\\.claude-profile-1\\.credentials.json",
                None,
            ),
            ("C:\\Users\\Test\\.claude-work\\.credentials.json", None),
            ("C:\\elsewhere\\.claude\\.credentials.json", None),
            ("C:\\Users\\Test\\.claude\\export.json", None),
        ] {
            assert_eq!(profile_number(home, Path::new(path)), expected, "{path}");
        }
    }

    #[test]
    fn file_keeps_the_plasmoid_shape_and_no_token() {
        let root = tempfile::tempdir().unwrap();
        let profile = profile(root.path());
        login(&profile.dir, (NOW + 3600) * 1000);
        update_profile(root.path(), &profile, true, NOW, |token| {
            assert_eq!(token, "fixture");
            Ok(Some(usage(12.0)))
        })
        .unwrap();
        let text = std::fs::read_to_string(root.path().join("claude-2.json")).unwrap();
        assert!(!text.contains("fixture"));
        let json: serde_json::Value = serde_json::from_str(&text).unwrap();
        assert_eq!(json["five_hour"]["used_percentage"], 12.0);
        assert_eq!(json["five_hour"]["resets_at"], NOW + 60);
        assert!(json["seven_day"].is_null());
        assert_eq!(json["tier"], "ultra");
        assert!(json["error"].is_null());
        assert_eq!(json["saved_at"], NOW);
    }

    #[test]
    fn fresh_files_make_no_request_until_old_or_forced() {
        let root = tempfile::tempdir().unwrap();
        let profile = profile(root.path());
        let now = epoch_secs(SystemTime::now()) + 10;
        login(&profile.dir, (now + 3600) * 1000);
        update_profile(root.path(), &profile, false, now, |_| Ok(Some(usage(1.0)))).unwrap();
        update_profile(root.path(), &profile, false, now + 299, |_| {
            panic!("a fresh file must not be fetched")
        })
        .unwrap();
        for (at, force) in [(now + 1, true), (now + 301, false)] {
            let mut fetched = false;
            update_profile(root.path(), &profile, force, at, |_| {
                fetched = true;
                Ok(Some(usage(2.0)))
            })
            .unwrap();
            assert!(fetched);
        }
    }

    #[test]
    fn expired_token_keeps_figures_and_is_never_sent() {
        let root = tempfile::tempdir().unwrap();
        let profile = profile(root.path());
        login(&profile.dir, (NOW + 3600) * 1000);
        update_profile(root.path(), &profile, true, NOW, |_| Ok(Some(usage(40.0)))).unwrap();
        login(&profile.dir, (NOW - 1) * 1000);
        update_profile(root.path(), &profile, true, NOW, |_| {
            panic!("an expired token must not be sent")
        })
        .unwrap();
        let state = read_file(&root.path().join("claude-2.json")).unwrap();
        assert!(state.error.as_deref().unwrap().starts_with(EXPIRED));
        let reading = usage_from_file(&state, false).unwrap();
        assert_eq!(reading.session.percentage, 40.0);
        assert!(reading.stale);
    }

    #[test]
    fn failed_fetch_keeps_figures_beside_the_error() {
        let root = tempfile::tempdir().unwrap();
        let profile = profile(root.path());
        login(&profile.dir, (NOW + 3600) * 1000);
        update_profile(root.path(), &profile, true, NOW, |_| Ok(Some(usage(40.0)))).unwrap();
        update_profile(root.path(), &profile, true, NOW + 1, |_| {
            Err(PollError::HttpStatus(429))
        })
        .unwrap();
        let state = read_file(&root.path().join("claude-2.json")).unwrap();
        assert_eq!(state.error.as_deref(), Some("HTTP 429"));
        assert_eq!(state.saved_at, NOW);
        assert_eq!(state.checked_at, NOW + 1);
        assert_eq!(state.five_hour.unwrap().used_percentage, 40.0);
    }

    #[test]
    fn logged_out_profile_is_hidden() {
        let root = tempfile::tempdir().unwrap();
        let profile = profile(root.path());
        std::fs::create_dir(&profile.dir).unwrap();
        update_profile(root.path(), &profile, true, NOW, |_| {
            panic!("no login, no request")
        })
        .unwrap();
        assert!(
            read_file(&root.path().join("claude-2.json"))
                .unwrap()
                .hidden
        );
    }

    #[test]
    fn new_login_is_not_paused_by_an_old_expiry_error() {
        let state = LimitFile {
            error: Some(format!("{EXPIRED}, run ccd 2 to refresh")),
            ..Default::default()
        };
        assert_eq!(usage_from_file(&state, false), Err(PollError::TokenExpired));
        assert_eq!(usage_from_file(&state, true), Err(PollError::RequestFailed));
    }

    #[test]
    fn statusline_files_without_service_fields_are_readable() {
        let state: LimitFile = serde_json::from_str(
            r#"{"five_hour":{"used_percentage":12,"resets_at":1792042200},
            "seven_day":null,"error":null,"saved_at":1792026690}"#,
        )
        .unwrap();
        let reading = usage_from_file(&state, false).unwrap();
        assert_eq!(reading.session.percentage, 12.0);
        assert!(!reading.weekly.available);
    }

    #[test]
    fn missing_login_falls_through_to_the_desktop_app_path() {
        let root = tempfile::tempdir().unwrap();
        let credentials = root.path().join(".claude").join(CREDENTIALS);
        let homes = [root.path().to_path_buf()];
        assert!(read_usage_in(root.path(), &homes, &credentials, false).is_none());
        let other = root.path().join("elsewhere").join(CREDENTIALS);
        assert!(read_usage_in(root.path(), &homes, &other, false).is_none());
    }

    #[test]
    fn stopped_wsl_keeps_files_and_writes_no_hidden() {
        let root = tempfile::tempdir().unwrap();
        let home = root.path().join("home");
        std::fs::create_dir_all(home.join(".claude")).unwrap();
        let out = root.path().join("out");
        std::fs::create_dir(&out).unwrap();
        let saved = LimitFile {
            five_hour: Some(LimitWindow {
                used_percentage: 40.0,
                resets_at: None,
            }),
            saved_at: NOW,
            checked_at: NOW,
            ..Default::default()
        };
        write_file(&out.join("claude-1.json"), &saved).unwrap();
        let homes = [home];
        let no_fetch = |_: &str| -> Result<Option<UsageData>, PollError> { panic!("no login") };
        update_homes(&out, &homes, false, true, NOW + 1, no_fetch);
        assert_eq!(read_file(&out.join("claude-1.json")).unwrap(), saved);
        update_homes(&out, &homes, true, true, NOW + 1, no_fetch);
        assert!(read_file(&out.join("claude-1.json")).unwrap().hidden);
    }

    #[test]
    fn stopped_wsl_reads_last_figures_without_touching_the_login() {
        let root = tempfile::tempdir().unwrap();
        let wsl_home = root.path().join("wsl");
        let credentials = wsl_home.join(".claude-profile-2").join(CREDENTIALS);
        let saved = LimitFile {
            five_hour: Some(LimitWindow {
                used_percentage: 40.0,
                resets_at: None,
            }),
            ..Default::default()
        };
        write_file(&root.path().join("claude-2.json"), &saved).unwrap();
        let homes = [root.path().join("win"), wsl_home];
        assert!(read_usage_in(root.path(), &homes, &credentials, false).is_none());
        let reading = read_usage_in(root.path(), &homes, &credentials, true)
            .unwrap()
            .unwrap();
        assert_eq!(reading.session.percentage, 40.0);
        assert!(reading.stale);
    }

    #[test]
    fn a_number_in_two_homes_goes_to_the_one_with_a_login() {
        let root = tempfile::tempdir().unwrap();
        let (win, wsl) = (root.path().join("win"), root.path().join("wsl"));
        std::fs::create_dir_all(win.join(".claude")).unwrap();
        login(&wsl.join(".claude"), 0);
        let found = discover_all(&[win, wsl.clone()]);
        assert_eq!(found.len(), 1);
        assert_eq!(found[0].dir, wsl.join(".claude"));
    }

    #[test]
    fn accounts_follow_profiles_and_keep_user_choices() {
        let root = tempfile::tempdir().unwrap();
        let two = profile(root.path());
        let three = Profile {
            number: 3,
            dir: root.path().join(".claude-profile-3"),
        };
        login(&two.dir, 0);
        std::fs::create_dir(&three.dir).unwrap();
        let mut accounts = ProviderAccounts::default();
        sync_profiles(&mut accounts, &[two.clone(), three], true);
        let ids: Vec<_> = accounts.profiles.iter().map(|p| p.id.as_str()).collect();
        assert_eq!(ids, ["default", "ccd_2"]);
        accounts.profiles[1].enabled = false;
        sync_profiles(&mut accounts, std::slice::from_ref(&two), true);
        assert!(!accounts.profiles[1].enabled);
        sync_profiles(&mut accounts, &[], true);
        assert_eq!(accounts.profiles.len(), 1);
    }
}
