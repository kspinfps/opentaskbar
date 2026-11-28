use std::fs;
use std::os::windows::ffi::OsStrExt;
use std::path::{Path, PathBuf};

use opentaskbar_core::log;
use windows::Win32::Foundation::HWND;
use windows::Win32::System::Com::CoTaskMemFree;
use windows::Win32::UI::Shell::PropertiesSystem::{IPropertyStore, SHGetPropertyStoreForWindow};
use windows::Win32::UI::Shell::{
    FOLDERID_CommonPrograms, FOLDERID_Programs, KF_FLAG_DEFAULT, SHGetKnownFolderPath,
};
use windows::core::w;
use windows_sys::Win32::Foundation::{ERROR_CANCELLED, GetLastError};
use windows_sys::Win32::UI::Shell::{SEE_MASK_NOASYNC, SHELLEXECUTEINFOW, ShellExecuteExW};
use windows_sys::Win32::UI::WindowsAndMessaging::SW_SHOWNORMAL;

use super::{
    APPS_FOLDER_PREFIX, EXPLORER_AUMID, PKEY_APPUSERMODEL_ID, apps_folder_id, is_apps_folder_app,
    is_packaged_aumid, read_prop_string, resolve_shortcut, resolve_windowsapps_aumid,
};

pub fn launch(target: String) {
    launch_impl(move || target, false, None);
}

pub fn launch_elevated(target: String) {
    launch_impl(move || target, true, None);
}

pub fn launch_with_args(target: String, args: String) {
    launch_impl(move || target, false, Some(args));
}

pub fn launch_app(source: String, window: Option<usize>, elevated: bool) {
    launch_impl(move || resolve_app_target(source, window), elevated, None);
}

fn resolve_app_target(source: String, window: Option<usize>) -> String {
    if let Some(id) = apps_folder_id(&source) {
        return format!("{APPS_FOLDER_PREFIX}{id}");
    }

    let path = Path::new(&source);
    let extension = path.extension().and_then(|ext| ext.to_str());
    let is_shortcut = extension.is_some_and(|ext| ext.eq_ignore_ascii_case("lnk"));
    let shortcut = is_shortcut.then(|| resolve_shortcut(&source));
    let executable = if let Some(meta) = &shortcut {
        meta.target_exe.as_deref()
    } else {
        extension
            .filter(|ext| ext.eq_ignore_ascii_case("exe"))
            .map(|_| source.as_str())
    };

    let packaged_id = shortcut
        .as_ref()
        .and_then(|meta| {
            meta.aumid.as_deref().filter(|id| {
                is_packaged_aumid(id)
                    || (!meta.has_arguments && id.eq_ignore_ascii_case(EXPLORER_AUMID))
            })
        })
        .map(str::to_owned)
        .or_else(|| {
            if is_shortcut {
                return None;
            }
            if !source.trim().is_empty() && executable.is_none() {
                return None;
            }
            let hwnd = HWND(window? as *mut _);
            // SAFETY: This worker has initialized COM and the shell validates the borrowed HWND.
            unsafe {
                let store: IPropertyStore = SHGetPropertyStoreForWindow(hwnd).ok()?;
                read_prop_string(&store, &PKEY_APPUSERMODEL_ID).filter(|id| is_apps_folder_app(id))
            }
        })
        .or_else(|| executable.and_then(resolve_windowsapps_aumid));
    if let Some(id) = packaged_id {
        return format!("{APPS_FOLDER_PREFIX}{id}");
    }

    // Argument-bearing shortcuts retain their own launch contract, including browser profiles.
    if !shortcut.as_ref().is_some_and(|meta| meta.has_arguments)
        && let Some(exe) = executable
        && is_helper(Path::new(exe))
        && let Some(target) = resolve_helper_target(Path::new(exe))
    {
        return target.to_string_lossy().into_owned();
    }
    source
}

fn is_helper(exe: &Path) -> bool {
    let file = exe
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or_default()
        .to_ascii_lowercase();
    ["helper", "cef", "subprocess", "crashhandler"]
        .iter()
        .any(|marker| file.contains(marker))
        || exe
            .ancestors()
            .skip(1)
            .filter_map(Path::file_name)
            .any(|dir| {
                dir.to_str().is_some_and(|dir| {
                    let dir = dir.to_ascii_lowercase();
                    dir == "bin" || dir == "cef" || dir.starts_with("cef.")
                })
            })
}

fn resolve_helper_target(exe: &Path) -> Option<PathBuf> {
    let ancestors: Vec<_> = exe
        .ancestors()
        .skip(1)
        .take(6)
        .take_while(|dir| dir.file_name().is_some())
        .collect();
    let launcher = ancestors.iter().find_map(|dir| {
        let mut name = dir.file_name()?.to_os_string();
        name.push(".exe");
        let candidate = dir.join(name);
        (!same_path(&candidate, exe) && candidate.is_file()).then_some(candidate)
    });

    let mut best = None;
    for folder in [FOLDERID_CommonPrograms, FOLDERID_Programs] {
        // SAFETY: The shell allocates this path with the COM task allocator.
        let path = unsafe {
            SHGetKnownFolderPath(&folder, KF_FLAG_DEFAULT, None)
                .ok()
                .and_then(|path| {
                    let value = path.to_string().ok();
                    CoTaskMemFree(Some(path.0.cast()));
                    value
                })
        };
        if let Some(path) = path {
            find_launcher_shortcut(Path::new(&path), &ancestors, launcher.as_deref(), &mut best);
        }
    }
    best.map(|(_, path)| path).or(launcher)
}

fn same_path(left: &Path, right: &Path) -> bool {
    left.to_str()
        .zip(right.to_str())
        .is_some_and(|(left, right)| left.eq_ignore_ascii_case(right))
}

fn find_launcher_shortcut(
    directory: &Path,
    ancestors: &[&Path],
    launcher: Option<&Path>,
    best: &mut Option<((usize, bool), PathBuf)>,
) {
    let Ok(entries) = fs::read_dir(directory) else {
        return;
    };
    for entry in entries.flatten() {
        let Ok(file_type) = entry.file_type() else {
            continue;
        };
        let path = entry.path();
        if file_type.is_dir() {
            find_launcher_shortcut(&path, ancestors, launcher, best);
            continue;
        }
        if !file_type.is_file()
            || !path
                .extension()
                .is_some_and(|ext| ext.eq_ignore_ascii_case("lnk"))
        {
            continue;
        }
        let Some(shortcut_path) = path.to_str() else {
            continue;
        };
        let meta = resolve_shortcut(shortcut_path);
        let Some(target) = meta.target_exe.as_deref().map(Path::new) else {
            continue;
        };
        let Some(rank) = shortcut_rank(target, ancestors, launcher, meta.has_arguments) else {
            continue;
        };
        if best.as_ref().is_none_or(|(current, _)| rank < *current) {
            *best = Some((rank, path));
        }
    }
}

fn shortcut_rank(
    target: &Path,
    ancestors: &[&Path],
    launcher: Option<&Path>,
    has_arguments: bool,
) -> Option<(usize, bool)> {
    if let Some(launcher) = launcher {
        return same_path(target, launcher).then_some((0, has_arguments));
    }
    if is_helper(target)
        || !target
            .extension()
            .is_some_and(|ext| ext.eq_ignore_ascii_case("exe"))
    {
        return None;
    }
    let parent = target.parent()?;
    let distance = ancestors
        .iter()
        .skip(1)
        .position(|dir| same_path(dir, parent))?;
    Some((distance, has_arguments))
}

fn launch_impl(
    target: impl FnOnce() -> String + Send + 'static,
    elevated: bool,
    args: Option<String>,
) {
    let worker = std::thread::Builder::new()
        .name("opentaskbar-launch".into())
        .spawn(move || {
            // This dedicated thread owns the COM apartment; the guard releases
            // it after shell execution (no-op if the thread was already in one).
            let Some(_com) = crate::platform::com::ComApartment::sta() else {
                log::warn("App launch COM initialization failed");
                return;
            };
            if let Err(error) = execute_shell_target(&target(), elevated, args.as_deref())
                && error != ERROR_CANCELLED
            {
                log::warn(format!(
                    "Windows Shell could not launch the target (Win32 {error})"
                ));
            }
        });
    if let Err(error) = worker {
        log::warn(format!(
            "App launch worker could not start ({:?})",
            error.kind()
        ));
    }
}

fn execute_shell_target(target: &str, elevated: bool, args: Option<&str>) -> Result<(), u32> {
    let file: Vec<u16> = target.encode_utf16().chain(Some(0)).collect();
    let params = args.map(|args| args.encode_utf16().chain(Some(0)).collect::<Vec<_>>());
    let path = Path::new(target);
    let directory = path
        .parent()
        .filter(|_| {
            path.is_absolute()
                && path
                    .extension()
                    .is_some_and(|ext| ext.eq_ignore_ascii_case("exe"))
        })
        .map(|parent| {
            parent
                .as_os_str()
                .encode_wide()
                .chain(Some(0))
                .collect::<Vec<_>>()
        });
    let mut info = SHELLEXECUTEINFOW {
        cbSize: size_of::<SHELLEXECUTEINFOW>() as u32,
        // The worker has no message loop and exits as soon as the shell hands off the launch.
        fMask: SEE_MASK_NOASYNC,
        lpVerb: if elevated {
            w!("runas").as_ptr()
        } else {
            std::ptr::null()
        },
        lpFile: file.as_ptr(),
        lpParameters: params
            .as_ref()
            .map_or(std::ptr::null(), |value| value.as_ptr()),
        lpDirectory: directory
            .as_ref()
            .map_or(std::ptr::null(), |value| value.as_ptr()),
        nShow: SW_SHOWNORMAL,
        ..Default::default()
    };
    // SAFETY: All pointers refer to live, null-terminated strings. No process handle is requested.
    unsafe {
        if ShellExecuteExW(&mut info) == 0 {
            return Err(GetLastError());
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn steam_helper_prefers_the_launcher_shortcut_without_special_arguments() {
        let helper = Path::new(r"C:\Apps\Steam\bin\cef\steamwebhelper.exe");
        let launcher = Path::new(r"C:\Apps\Steam\steam.exe");
        let ancestors: Vec<_> = helper.ancestors().skip(1).take(4).collect();
        assert!(is_helper(helper));
        assert_eq!(
            shortcut_rank(launcher, &ancestors, Some(launcher), false),
            Some((0, false))
        );
        assert_eq!(
            shortcut_rank(launcher, &ancestors, Some(launcher), true),
            Some((0, true))
        );
        assert_eq!(
            shortcut_rank(helper, &ancestors, Some(launcher), false),
            None
        );
        assert!(shortcut_rank(launcher, &ancestors, None, false).is_some());
        assert_eq!(
            shortcut_rank(
                Path::new(r"C:\Apps\Other\Other.exe"),
                &ancestors,
                None,
                false
            ),
            None
        );
    }

    #[test]
    fn dock_launch_preserves_internet_shortcuts_and_normalizes_store_targets() {
        let internet_shortcut = r"C:\Shortcuts\Steam game.url";
        assert_eq!(
            resolve_app_target(internet_shortcut.to_owned(), None),
            internet_shortcut
        );
        assert_eq!(
            resolve_app_target(
                r"  shell:appsfolder\Example.Product_0123456789abc!Client  ".to_owned(),
                None
            ),
            r"shell:AppsFolder\Example.Product_0123456789abc!Client"
        );
        assert_eq!(
            resolve_app_target(
                r"::{4234d49b-0245-4df3-b780-3893943456e1}\Microsoft.Windows.Explorer".to_owned(),
                None
            ),
            r"shell:AppsFolder\Microsoft.Windows.Explorer"
        );
        assert_eq!(
            resolve_app_target(
                r"shell:AppsFolder\windows.immersivecontrolpanel_cw5n1h2txyewy!microsoft.windows.immersivecontrolpanel".to_owned(),
                None
            ),
            r"shell:AppsFolder\windows.immersivecontrolpanel_cw5n1h2txyewy!microsoft.windows.immersivecontrolpanel"
        );
    }
}
