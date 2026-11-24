//! Windows glue: who holds the microphone (the privacy registry behind the mic icon in the
//! taskbar) and which meeting windows are open (top-level window titles).
#![cfg(target_os = "windows")]

use super::platform::{self, AppKind, Platform};
use super::tracker::Observation;

const MIC_STORE: &str = r"Software\Microsoft\Windows\CurrentVersion\CapabilityAccessManager\ConsentStore\microphone";
const BROWSERS: &[&str] = &["chrome.exe", "msedge.exe", "brave.exe", "opera.exe", "firefox.exe", "vivaldi.exe", "comet.exe"];
const TEAMS: &[&str] = &["ms-teams.exe", "teams.exe"];
const ZOOM: &str = "zoom.exe";
/// Window views that only exist during a Teams call (the same names `meeting_title` skips).
const TEAMS_CALL_VIEWS: &[&str] = &["Visualizzazione compatta delle riunioni", "Meeting compact view", "Compact meeting view"];

/// App kind of a ConsentStore entry: a packaged app (new Teams is `MSTeams_8wekyb3d8bbwe`) or a
/// NonPackaged exe path written with `#` for `\`.
fn kind_for_entry(entry: &str) -> Option<AppKind> {
    let lower = entry.to_ascii_lowercase();
    let exe = lower.rsplit('#').next().unwrap_or(&lower);
    if lower.starts_with("msteams_") || TEAMS.contains(&exe) {
        Some(AppKind::Teams)
    } else if exe == ZOOM {
        Some(AppKind::Zoom)
    } else {
        BROWSERS.iter().find(|b| exe == **b).map(|b| AppKind::Browser(b))
    }
}

/// App kinds currently capturing the mic: Windows keeps LastUsedTimeStop at 0 while in use.
fn mic_users() -> Vec<AppKind> {
    use winreg::{enums::HKEY_CURRENT_USER, RegKey};
    let in_use = |k: &RegKey| {
        k.get_value::<u64, _>("LastUsedTimeStop").ok() == Some(0)
            && k.get_value::<u64, _>("LastUsedTimeStart").is_ok_and(|t| t > 0)
    };
    let Ok(store) = RegKey::predef(HKEY_CURRENT_USER).open_subkey(MIC_STORE) else { return Vec::new() };
    let mut kinds = Vec::new();
    let mut check = |parent: &RegKey, name: &str| {
        if let (Some(kind), Ok(key)) = (kind_for_entry(name), parent.open_subkey(name)) {
            if in_use(&key) && !kinds.contains(&kind) {
                kinds.push(kind);
            }
        }
    };
    for name in store.enum_keys().flatten() {
        if name == "NonPackaged" {
            if let Ok(np) = store.open_subkey(&name) {
                for exe in np.enum_keys().flatten() {
                    check(&np, &exe);
                }
            }
        } else {
            check(&store, &name);
        }
    }
    kinds
}

/// (exe name in lowercase, title) of every visible top-level window with a title.
fn windows() -> Vec<(String, String)> {
    use windows::core::{BOOL, PWSTR};
    use windows::Win32::Foundation::{CloseHandle, HWND, LPARAM, MAX_PATH};
    use windows::Win32::System::Threading::{
        OpenProcess, QueryFullProcessImageNameW, PROCESS_NAME_WIN32, PROCESS_QUERY_LIMITED_INFORMATION,
    };
    use windows::Win32::UI::WindowsAndMessaging::{EnumWindows, GetWindowTextW, GetWindowThreadProcessId, IsWindowVisible};

    unsafe extern "system" fn collect(hwnd: HWND, out: LPARAM) -> BOOL {
        let out = &mut *(out.0 as *mut Vec<(HWND, String)>);
        if IsWindowVisible(hwnd).as_bool() {
            let mut buf = [0u16; 512];
            let n = GetWindowTextW(hwnd, &mut buf);
            if n > 0 {
                out.push((hwnd, String::from_utf16_lossy(&buf[..n as usize])));
            }
        }
        BOOL(1)
    }

    let mut found: Vec<(HWND, String)> = Vec::new();
    unsafe {
        let _ = EnumWindows(Some(collect), LPARAM(&mut found as *mut _ as isize));
    }
    found
        .into_iter()
        .filter_map(|(hwnd, title)| unsafe {
            let mut pid = 0u32;
            GetWindowThreadProcessId(hwnd, Some(&mut pid as *mut u32));
            let process = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, false, pid).ok()?;
            let mut buf = [0u16; MAX_PATH as usize];
            let mut len = buf.len() as u32;
            let ok = QueryFullProcessImageNameW(process, PROCESS_NAME_WIN32, PWSTR(buf.as_mut_ptr()), &mut len);
            let _ = CloseHandle(process);
            ok.ok()?;
            let path = String::from_utf16_lossy(&buf[..len as usize]).to_ascii_lowercase();
            let exe = path.rsplit('\\').next().unwrap_or(&path).to_string();
            Some((exe, title))
        })
        .collect()
}

/// Browser window titles end with " - Google Chrome", " - Profilo 1 - Microsoft Edge"…: drop
/// those tails so a Meet title is just the meeting.
fn strip_browser_suffix(title: &str) -> &str {
    // ponytail: known browser names and profile tails only; "and 2 more pages" stays in.
    const TAILS: &[&str] = &["chrome", "edge", "brave", "opera", "firefox", "vivaldi", "comet", "profil", "persona"];
    let mut t = title.trim();
    while let Some((head, tail)) = t.rsplit_once(" - ").or_else(|| t.rsplit_once(" — ")) {
        let tail = tail.to_lowercase();
        if !TAILS.iter().any(|n| tail.contains(n)) {
            break;
        }
        t = head.trim();
    }
    t
}

fn titles_of<'a>(wins: &'a [(String, String)], exes: &'a [&str]) -> impl Iterator<Item = &'a str> {
    wins.iter().filter(|(e, _)| exes.contains(&e.as_str())).map(|(_, t)| t.as_str())
}

fn meet_title(wins: &[(String, String)]) -> Option<&str> {
    titles_of(wins, BROWSERS).map(strip_browser_suffix).find(|t| platform::is_meet_window(t))
}

/// The Teams compact meeting view: unambiguous, unlike the main window (it names chats, channels).
fn teams_call_title(wins: &[(String, String)]) -> Option<&str> {
    titles_of(wins, TEAMS).find(|t| TEAMS_CALL_VIEWS.iter().any(|v| t.starts_with(v)))
}

/// Title of the call on screen, only from unambiguous sources: used to tell one call from the next.
pub fn call_title(platform: Platform) -> Option<String> {
    let wins = windows();
    match platform {
        Platform::Teams => teams_call_title(&wins).and_then(|t| platform::meeting_title(Platform::Teams, t)),
        Platform::Meet => meet_title(&wins).and_then(|t| platform::meeting_title(Platform::Meet, t)),
        Platform::Zoom => None,
    }
}

/// Whole title of the call window (Teams compact view, Meet tab without the browser tail), when it
/// names a meeting: what the identity check compares with `platform::same_call`.
pub fn call_window_title(platform: Platform) -> Option<String> {
    let wins = windows();
    let title = match platform {
        Platform::Teams => teams_call_title(&wins),
        Platform::Meet => meet_title(&wins),
        Platform::Zoom => None,
    }?;
    platform::meeting_title(platform, title).is_some().then(|| title.to_string())
}

/// Meeting title for the platform, read when a session starts.
pub fn title_for(platform: Platform) -> Option<String> {
    match platform {
        Platform::Teams => call_title(Platform::Teams).or_else(|| {
            titles_of(&windows(), TEAMS).find_map(|t| platform::meeting_title(Platform::Teams, t))
        }),
        other => call_title(other),
    }
}

/// Same contract as the macOS probe; without Accessibility, `call_ended` is never asserted and
/// the stop comes from the tracker's mic/window absence threshold.
pub fn observe(session: Option<Platform>) -> Observation {
    let users = mic_users();
    let wins = windows();
    let meet_window = meet_title(&wins).is_some();
    let mic = platform::mic_platform(&users, meet_window, session);
    let window = if mic.is_some() {
        mic
    } else if titles_of(&wins, &[ZOOM]).any(platform::is_zoom_meeting_window) {
        Some(Platform::Zoom)
    } else if meet_window {
        Some(Platform::Meet)
    } else if session == Some(Platform::Teams) && teams_call_title(&wins).is_some() {
        Some(Platform::Teams)
    } else {
        None
    };
    Observation { mic, window, call_ended: false }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn classifies_consent_entries() {
        assert_eq!(kind_for_entry("MSTeams_8wekyb3d8bbwe"), Some(AppKind::Teams));
        assert_eq!(kind_for_entry(r"C:#Users#me#AppData#Local#Microsoft#Teams#current#Teams.exe"), Some(AppKind::Teams));
        assert_eq!(kind_for_entry(r"C:#Users#me#AppData#Roaming#Zoom#bin#Zoom.exe"), Some(AppKind::Zoom));
        assert_eq!(
            kind_for_entry(r"C:#Program Files#Google#Chrome#Application#chrome.exe"),
            Some(AppKind::Browser("chrome.exe"))
        );
        assert_eq!(kind_for_entry(r"C:#Users#me#AppData#Local#Dixit#Dixit.exe"), None);
        assert_eq!(kind_for_entry("Microsoft.WindowsSoundRecorder_8wekyb3d8bbwe"), None);
    }

    #[test]
    fn strips_browser_tails() {
        assert_eq!(strip_browser_suffix("Meet - abc-defg-hij - Google Chrome"), "Meet - abc-defg-hij");
        assert_eq!(strip_browser_suffix("Meet – Daily - Profilo 1 - Microsoft\u{200b} Edge"), "Meet – Daily");
        assert_eq!(strip_browser_suffix("Meet - Sprint - review"), "Meet - Sprint - review");
        assert_eq!(
            platform::meeting_title(Platform::Meet, strip_browser_suffix("Meet - abc-defg-hij - Google Chrome")).as_deref(),
            Some("abc-defg-hij")
        );
    }

    /// Live check on this PC: `cargo test --lib probe_windows_smoke -- --ignored --nocapture`.
    #[test]
    #[ignore]
    fn probe_windows_smoke() {
        println!("mic_users={:?}", mic_users());
        for (exe, title) in windows() {
            println!("{exe}: {title}");
        }
        println!("observation={:?}", observe(None));
    }
}
