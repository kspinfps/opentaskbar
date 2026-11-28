//! Reads Windows taskbar pinned shortcuts (`.lnk`), mirroring OpenTaskbar the reference //! `WindowsTaskbarPinImporter`: resolves `PKEY_AppUserModel_ID`,
//! `PKEY_Link_TargetParsingPath` (`::{4234d49b-0245-4df3-b780-3893943456e1}\<AUMID>`),
//! `IShellLinkW::GetIconLocation`, and `IShellLinkW::GetPath`.

use std::collections::HashSet;
use std::fs;
use std::path::{Path, PathBuf};

use windows::Win32::Foundation::PROPERTYKEY;
use windows::Win32::System::Com::StructuredStorage::{PropVariantClear, PropVariantToStringAlloc};
use windows::Win32::System::Com::{
    CLSCTX_INPROC_SERVER, CoCreateInstance, CoTaskMemFree, IPersistFile, STGM_READ,
};
use windows::Win32::UI::Shell::PropertiesSystem::IPropertyStore;
use windows::Win32::UI::Shell::{IShellLinkW, SLGP_RAWPATH, ShellLink};
use windows::core::{GUID, HSTRING, Interface};
use windows_sys::Win32::System::Registry::{
    HKEY_CURRENT_USER, KEY_READ, REG_BINARY, RegCloseKey, RegOpenKeyExW, RegQueryValueExW,
};

use super::taskbar_pin_monitor::extract_aumids;

mod launch;
pub use launch::{launch, launch_app, launch_elevated, launch_with_args};

/// `System.AppUserModel.ID` (`{9F4C2855-9F79-4B39-A8D0-E1D42DE1D5F3}, 5`).
const PKEY_APPUSERMODEL_ID: PROPERTYKEY = PROPERTYKEY {
    fmtid: GUID::from_u128(0x9F4C_2855_9F79_4B39_A8D0_E1D4_2DE1_D5F3),
    pid: 5,
};

/// `System.Link.TargetParsingPath` (`{B9B4B3FC-2B51-4A42-B5D8-324146AFCF25}, 2`).
const PKEY_LINK_TARGET_PARSING_PATH: PROPERTYKEY = PROPERTYKEY {
    fmtid: GUID::from_u128(0xB9B4_B3FC_2B51_4A42_B5D8_3241_46AF_CF25),
    pid: 2,
};

const APPS_FOLDER_CLSID_PREFIX: &str = "::{4234d49b-0245-4df3-b780-3893943456e1}\\";
const APPS_FOLDER_PREFIX: &str = "shell:AppsFolder\\";
const EXPLORER_AUMID: &str = "Microsoft.Windows.Explorer";

fn is_packaged_aumid(id: &str) -> bool {
    let Some((family, app)) = id.split_once('!') else {
        return false;
    };
    let Some((name, publisher)) = family.rsplit_once('_') else {
        return false;
    };
    !name.is_empty()
        && name
            .bytes()
            .all(|c| c.is_ascii_alphanumeric() || b".-".contains(&c))
        && publisher.len() == 13
        && publisher.bytes().all(|c| c.is_ascii_alphanumeric())
        && !app.is_empty()
        && app
            .bytes()
            .all(|c| c.is_ascii_alphanumeric() || b".-_".contains(&c))
}

fn apps_folder_aumid<'a>(value: &'a str, prefix: &str) -> Option<&'a str> {
    if !value.get(..prefix.len())?.eq_ignore_ascii_case(prefix) {
        return None;
    }
    let id = value[prefix.len()..].trim();
    id.bytes()
        .all(|c| c.is_ascii_alphanumeric() || b".-_!{}".contains(&c))
        .then_some(id)
        .filter(|id| !id.is_empty())
}

#[derive(Clone, Debug, serde::Deserialize, serde::Serialize)]
pub struct PinnedApp {
    pub name: String,
    /// Shortcut, executable, or stable AppsFolder launch target.
    pub launch_path: String,
    /// Resolved executable path when available (used to match running windows).
    pub target_exe: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub aumid: Option<String>,
    /// Resolved icon source (`shell:AppsFolder\<AUMID>`, custom icon path, resolved `.exe`, or `.lnk`).
    pub icon_source: String,
}

fn pins_dir() -> PathBuf {
    std::env::var_os("APPDATA")
        .map(PathBuf::from)
        .unwrap_or_default()
        .join(r"Microsoft\Internet Explorer\Quick Launch\User Pinned\TaskBar")
}

unsafe fn read_prop_string(store: &IPropertyStore, key: &PROPERTYKEY) -> Option<String> {
    // SAFETY: caller ensures COM STA is initialized and `store` is valid.
    unsafe {
        // windows-rs 0.61 PROPVARIANT has no Drop impl: the caller must clear the variant itself.
        let mut val = store.GetValue(key).ok()?;
        let text = PropVariantToStringAlloc(&val).ok().and_then(|pwstr| {
            let text = pwstr.to_string().ok();
            CoTaskMemFree(Some(pwstr.0.cast()));
            text
        });
        let _ = PropVariantClear(&mut val);
        text.map(|s| s.trim().to_owned()).filter(|s| !s.is_empty())
    }
}

#[derive(Default)]
struct ShortcutMeta {
    target_exe: Option<String>,
    aumid: Option<String>,
    icon_location: Option<String>,
    has_arguments: bool,
}

fn resolve_shortcut(lnk_path: &str) -> ShortcutMeta {
    // SAFETY: COM STA calls to inspect `.lnk` metadata.
    unsafe {
        let Ok(link) = CoCreateInstance::<_, IShellLinkW>(&ShellLink, None, CLSCTX_INPROC_SERVER)
        else {
            return ShortcutMeta::default();
        };
        let Ok(persist) = link.cast::<IPersistFile>() else {
            return ShortcutMeta::default();
        };
        if persist.Load(&HSTRING::from(lnk_path), STGM_READ).is_err() {
            return ShortcutMeta::default();
        }

        let mut target_buf = [0u16; 1024];
        let target_exe = if link
            .GetPath(&mut target_buf, std::ptr::null_mut(), SLGP_RAWPATH.0 as u32)
            .is_ok()
        {
            let len = target_buf
                .iter()
                .position(|&c| c == 0)
                .unwrap_or(target_buf.len());
            let raw = String::from_utf16_lossy(&target_buf[..len]);
            let expanded = expand_env_vars(&raw);
            let is_windowsapps = expanded.to_lowercase().contains(r"\windowsapps\");
            (!expanded.is_empty() && (is_windowsapps || PathBuf::from(&expanded).exists()))
                .then_some(expanded)
        } else {
            None
        };

        let mut icon_buf = [0u16; 1024];
        let mut icon_idx = 0i32;
        let icon_location = if link.GetIconLocation(&mut icon_buf, &mut icon_idx).is_ok() {
            let len = icon_buf
                .iter()
                .position(|&c| c == 0)
                .unwrap_or(icon_buf.len());
            let raw = String::from_utf16_lossy(&icon_buf[..len]);
            let expanded = expand_env_vars(raw.trim());
            (!expanded.is_empty() && PathBuf::from(&expanded).exists()).then(|| {
                if icon_idx != 0 {
                    format!("{expanded},{icon_idx}")
                } else {
                    expanded
                }
            })
        } else {
            None
        };

        let mut arguments_buf = [0u16; 2048];
        let arguments = if link.GetArguments(&mut arguments_buf).is_ok() {
            String::from_utf16_lossy(arguments_buf.split(|&c| c == 0).next().unwrap_or_default())
        } else {
            String::new()
        };
        let aumid = link
            .cast::<IPropertyStore>()
            .ok()
            .and_then(|store| {
                let direct = read_prop_string(&store, &PKEY_APPUSERMODEL_ID);
                direct.or_else(|| {
                    let parsing_path = read_prop_string(&store, &PKEY_LINK_TARGET_PARSING_PATH)?;
                    apps_folder_id(&parsing_path).map(str::to_owned)
                })
            })
            .or_else(|| {
                let exe = Path::new(target_exe.as_deref()?).file_name()?.to_str()?;
                if !exe.eq_ignore_ascii_case("explorer.exe") {
                    return None;
                }
                explorer_arguments_aumid(&arguments).map(str::to_owned)
            });

        ShortcutMeta {
            target_exe,
            aumid,
            icon_location,
            has_arguments: !arguments.trim().is_empty(),
        }
    }
}

fn explorer_arguments_aumid(arguments: &str) -> Option<&str> {
    let arguments_lower = arguments.to_ascii_lowercase();
    let start = arguments_lower.find("shell:appsfolder\\")?;
    let target = arguments[start..].split([' ', '"', '\t']).next()?;
    apps_folder_aumid(target, APPS_FOLDER_PREFIX).filter(|id| is_apps_folder_app(id))
}

fn is_apps_folder_app(id: &str) -> bool {
    is_packaged_aumid(id) || id.eq_ignore_ascii_case(EXPLORER_AUMID)
}

pub(super) fn apps_folder_id(spec: &str) -> Option<&str> {
    apps_folder_aumid(spec.trim(), APPS_FOLDER_PREFIX)
        .or_else(|| apps_folder_aumid(spec.trim(), APPS_FOLDER_CLSID_PREFIX))
}

fn is_frame_host_spec(spec: &str) -> bool {
    let file = spec.rsplit(['\\', '/']).next().unwrap_or_default();
    let file = file.split_once(',').map_or(file, |(name, _)| name.trim());
    file.eq_ignore_ascii_case("ApplicationFrameHost.exe")
}

/// Resolves app artwork in AppsFolder, shortcut, executable order.
pub fn icon_candidates_for_app(
    aumid: Option<&str>,
    exe_path: &str,
    shortcut_path: Option<&str>,
) -> Vec<String> {
    let mut specs = Vec::<String>::with_capacity(3);
    let app_id: Option<String> = aumid
        .map(str::trim)
        .map(|raw| apps_folder_id(raw).unwrap_or(raw))
        .filter(|id| is_apps_folder_app(id))
        .map(str::to_owned)
        .or_else(|| {
            shortcut_path
                .and_then(apps_folder_id)
                .filter(|id| is_apps_folder_app(id))
                .map(str::to_owned)
        })
        .or_else(|| {
            apps_folder_id(exe_path)
                .filter(|id| is_apps_folder_app(id))
                .map(str::to_owned)
        })
        .or_else(|| {
            exe_path
                .to_ascii_lowercase()
                .contains(r"\windowsapps\")
                .then(|| resolve_windowsapps_aumid(exe_path))
                .flatten()
        });
    if let Some(id) = app_id {
        specs.push(format!("{APPS_FOLDER_PREFIX}{id}"));
    }
    for spec in shortcut_path.into_iter().chain([exe_path]).map(str::trim) {
        let normalized = apps_folder_id(spec).map(|id| format!("{APPS_FOLDER_PREFIX}{id}"));
        let spec = normalized.as_deref().unwrap_or(spec);
        if !spec.is_empty()
            && !is_frame_host_spec(spec)
            && !specs
                .iter()
                .any(|existing| existing.eq_ignore_ascii_case(spec))
        {
            specs.push(spec.to_owned());
        }
    }
    specs
}

fn expand_env_vars(input: &str) -> String {
    let mut out = input.to_owned();
    for var in [
        "LOCALAPPDATA",
        "APPDATA",
        "ProgramFiles",
        "ProgramFiles(x86)",
        "SystemRoot",
        "WINDIR",
        "USERPROFILE",
    ] {
        let token = format!("%{var}%");
        let tok_lower = token.to_lowercase();
        while let Some(pos) = out.to_ascii_lowercase().find(&tok_lower) {
            let Ok(val) = std::env::var(var) else {
                break;
            };
            out.replace_range(pos..pos + token.len(), &val);
        }
    }
    out
}

/// Resolves a `WindowsApps\<PackageFullName>\...` executable path to a packaged app ID.
/// Mirrors `ApplicationIconProvider.TryResolvePackagedAppUserModelId`.
pub fn resolve_windowsapps_aumid(exe_path: &str) -> Option<String> {
    let exe = Path::new(exe_path);
    let package_root = exe.ancestors().skip(1).find(|dir| {
        dir.parent()
            .and_then(Path::file_name)
            .and_then(|name| name.to_str())
            .is_some_and(|name| name.eq_ignore_ascii_case("WindowsApps"))
    })?;
    let [name, _, _, _, publisher] = package_root
        .file_name()?
        .to_str()?
        .split('_')
        .collect::<Vec<_>>()[..]
    else {
        return None;
    };
    let xml = fs::read_to_string(package_root.join("AppxManifest.xml")).ok()?;
    let relative_exe = exe.strip_prefix(package_root).ok()?.to_str()?;
    let app = manifest_app_id(&xml, relative_exe)?;
    let id = format!("{name}_{publisher}!{app}");
    is_packaged_aumid(&id).then_some(id)
}

fn manifest_app_id(xml: &str, relative_exe: &str) -> Option<String> {
    let manifest = roxmltree::Document::parse(xml).ok()?;
    let apps = manifest
        .root_element()
        .children()
        .find(|node| node.tag_name().name() == "Applications")?;
    let entries = apps
        .children()
        .filter(|node| node.tag_name().name() == "Application")
        .filter_map(|node| Some((node.attribute("Id")?, node.attribute("Executable"))));
    let relative_exe = relative_exe.replace('/', "\\");
    let matching_id = entries.clone().find_map(|(id, exe)| {
        exe.filter(|exe| exe.replace('/', "\\").eq_ignore_ascii_case(&relative_exe))
            .map(|_| id)
    });
    matching_id
        .or_else(|| {
            let mut entries = entries;
            let (only_id, _) = entries.next()?;
            entries.next().is_none().then_some(only_id)
        })
        .map(str::to_owned)
}

// ---------------------------------------------------------------------------
// Windows 11 packaged pins (Explorer Taskband\Favorites, reference parity)
// ---------------------------------------------------------------------------

const TASKBAND_KEY: &str = r"Software\Microsoft\Windows\CurrentVersion\Explorer\Taskband";
const FAVORITES_VALUE: &str = "Favorites";

use crate::platform::win32_helpers::wide;

/// Raw `HKCU\...\Taskband\Favorites` blob; absent keys simply mean no packaged pins.
fn taskband_favorites() -> Option<Vec<u8>> {
    let key_path = wide(TASKBAND_KEY);
    let value = wide(FAVORITES_VALUE);
    // SAFETY: read-only registry access with NUL-terminated UTF-16 names.
    unsafe {
        let mut key = std::ptr::null_mut();
        if RegOpenKeyExW(HKEY_CURRENT_USER, key_path.as_ptr(), 0, KEY_READ, &mut key) != 0 {
            return None;
        }
        let mut size = 0u32;
        let status = RegQueryValueExW(
            key,
            value.as_ptr(),
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            &mut size,
        );
        if status != 0 || size < 4 {
            let _ = RegCloseKey(key);
            return None;
        }
        let mut buffer = vec![0u8; size as usize];
        let mut kind = 0u32;
        let status = RegQueryValueExW(
            key,
            value.as_ptr(),
            std::ptr::null_mut(),
            &mut kind,
            buffer.as_mut_ptr(),
            &mut size,
        );
        let _ = RegCloseKey(key);
        (status == 0 && kind == REG_BINARY).then_some(buffer)
    }
}

/// `(display name, relative executable)` for an `<Application Id="...">` entry.
fn manifest_application(xml: &str, app_id: &str) -> Option<(Option<String>, Option<String>)> {
    let manifest = roxmltree::Document::parse(xml).ok()?;
    let mut display = None;
    let mut executable = None;
    let mut found = false;
    for node in manifest
        .descendants()
        .filter(|n| n.tag_name().name() == "Application")
    {
        if !node
            .attribute("Id")
            .is_some_and(|id| id.eq_ignore_ascii_case(app_id))
        {
            continue;
        }
        let relative = node
            .attribute("Executable")
            .map(str::trim)
            .filter(|exe| !exe.is_empty() && !exe.contains('$') && exe.ends_with(".exe"));
        executable = relative.map(str::to_owned);
        display = node
            .children()
            .find(|n| n.tag_name().name() == "VisualElements")
            .and_then(|n| n.attribute("DisplayName"))
            .map(str::to_owned);
        found = true;
        break;
    }
    if !found {
        return None;
    }
    if display.is_none() {
        display = manifest
            .descendants()
            .find(|n| n.tag_name().name() == "DisplayName")
            .and_then(|n| n.text())
            .map(str::to_owned);
    }
    Some((display, executable))
}

fn is_resource_reference(value: &str) -> bool {
    value
        .trim_start()
        .to_ascii_lowercase()
        .starts_with("ms-resource:")
}

#[link(name = "kernel32")]
unsafe extern "system" {
    fn GetPackagesByPackageFamily(
        family: *const u16,
        count: *mut u32,
        names: *mut *mut u16,
        length: *mut u32,
        buffer: *mut u16,
    ) -> i32;
    fn GetPackagePathByFullName(name: *const u16, length: *mut u32, path: *mut u16) -> i32;
}

fn registered_package_roots(family: &str) -> Vec<PathBuf> {
    use std::os::windows::ffi::OsStringExt;
    use std::ptr::null_mut;

    const INSUFFICIENT_BUFFER: i32 = 122;
    let family = wide(family);
    let (mut count, mut length) = (0, 0);
    // SAFETY: query required capacities, then provide both output buffers.
    unsafe {
        if GetPackagesByPackageFamily(
            family.as_ptr(),
            &mut count,
            null_mut(),
            &mut length,
            null_mut(),
        ) != INSUFFICIENT_BUFFER
            || count > 1024
            || length > 262_144
        {
            return Vec::new();
        }
        let mut names = vec![null_mut(); count as usize];
        let mut buffer = vec![0; length as usize];
        if GetPackagesByPackageFamily(
            family.as_ptr(),
            &mut count,
            names.as_mut_ptr(),
            &mut length,
            buffer.as_mut_ptr(),
        ) != 0
        {
            return Vec::new();
        }
        names
            .into_iter()
            .take(count as usize)
            .filter_map(|name| {
                // The package name points into the still-live buffer above.
                let mut length = 0;
                if name.is_null()
                    || GetPackagePathByFullName(name, &mut length, null_mut())
                        != INSUFFICIENT_BUFFER
                    || length == 0
                    || length > 32_768
                {
                    return None;
                }
                let mut path = vec![0; length as usize];
                if GetPackagePathByFullName(name, &mut length, path.as_mut_ptr()) != 0 {
                    return None;
                }
                let end = path.iter().position(|&c| c == 0).unwrap_or(path.len());
                Some(PathBuf::from(std::ffi::OsString::from_wide(&path[..end])))
            })
            .collect()
    }
}

/// Registered paths first, then inbox and installation-directory fallbacks.
pub(super) fn package_install_roots(family: &str) -> impl Iterator<Item = PathBuf> + use<> {
    let registered = registered_package_roots(family);
    let identity = family
        .rsplit_once('_')
        .map(|(package, publisher)| (package.to_ascii_lowercase(), publisher.to_ascii_lowercase()));
    let inbox = identity
        .as_ref()
        .filter(|(package, publisher)| {
            package == "windows.immersivecontrolpanel" && publisher == "cw5n1h2txyewy"
        })
        .and_then(|_| std::env::var_os("SystemRoot"))
        .map(|root| PathBuf::from(root).join("ImmersiveControlPanel"));

    registered.into_iter().chain(inbox).chain(
        std::iter::once_with(move || {
            let root = std::env::var_os("ProgramFiles")
                .map(PathBuf::from)
                .unwrap_or_else(|| PathBuf::from(r"C:\Program Files"))
                .join("WindowsApps");
            identity
                .and_then(|(package, publisher)| {
                    let prefix = format!("{package}_");
                    let suffix = format!("_{publisher}");
                    let entries = fs::read_dir(root).ok()?;
                    Some(entries.flatten().filter_map(move |entry| {
                        let name = entry.file_name().to_str()?.to_ascii_lowercase();
                        (name.starts_with(&prefix) && name.ends_with(&suffix)).then(|| entry.path())
                    }))
                })
                .into_iter()
                .flatten()
        })
        .flatten(),
    )
}

fn fallback_packaged_name(family: &str, app: &str) -> String {
    let package = family.rsplit_once('_').map_or(family, |(name, _)| name);
    if package.eq_ignore_ascii_case("windows.immersivecontrolpanel") {
        return "Settings".to_owned();
    }
    if !app.is_empty() && !app.eq_ignore_ascii_case("app") && !app.contains('.') {
        return app.to_owned();
    }
    package
        .rsplit('.')
        .next()
        .filter(|segment| !segment.is_empty())
        .unwrap_or(app)
        .to_owned()
}

/// Resolves a packaged or inbox AUMID to a durable AppsFolder pin.
pub fn resolve_packaged_app(aumid: &str) -> Option<PinnedApp> {
    let launch_path = format!("{APPS_FOLDER_PREFIX}{aumid}");
    if aumid.eq_ignore_ascii_case(EXPLORER_AUMID) {
        return Some(PinnedApp {
            name: "File Explorer".to_owned(),
            launch_path: launch_path.clone(),
            target_exe: std::env::var_os("SystemRoot").map(|root| {
                PathBuf::from(root)
                    .join("explorer.exe")
                    .to_string_lossy()
                    .into_owned()
            }),
            aumid: Some(EXPLORER_AUMID.to_owned()),
            icon_source: launch_path,
        });
    }
    if !is_packaged_aumid(aumid) {
        return None;
    }
    let (family, app) = aumid.split_once('!')?;
    let mut name = fallback_packaged_name(family, app);
    let mut target_exe = None;
    for root in package_install_roots(family) {
        let Ok(xml) = fs::read_to_string(root.join("AppxManifest.xml")) else {
            continue;
        };
        let Some((display, executable)) = manifest_application(&xml, app) else {
            continue;
        };
        if let Some(display) = display.filter(|s| !is_resource_reference(s)) {
            name = display;
        }
        target_exe = executable.map(|exe| {
            root.join(exe.replace('/', "\\"))
                .to_string_lossy()
                .into_owned()
        });
        break;
    }
    Some(PinnedApp {
        name,
        launch_path: launch_path.clone(),
        target_exe,
        aumid: Some(aumid.to_owned()),
        icon_source: launch_path,
    })
}

/// Imports packaged taskbar pins from the Explorer Taskband registry blob.
/// Windows 11 no longer creates `.lnk` files for Store apps, so this is the
/// only source for their AUMIDs. Missing/unreadable keys yield an empty list.
pub fn import_packaged_pins() -> Vec<PinnedApp> {
    let Some(blob) = taskband_favorites() else {
        return Vec::new();
    };
    let mut seen = HashSet::new();
    let mut pins = Vec::new();
    for aumid in extract_aumids(&blob) {
        if !seen.insert(aumid.to_ascii_lowercase()) {
            continue;
        }
        if let Some(pin) = resolve_packaged_app(&aumid) {
            pins.push(pin);
        }
    }
    pins.sort_by_key(|a| a.name.to_lowercase());
    pins
}

pub fn load_pins() -> Vec<PinnedApp> {
    let dir = pins_dir();
    let entries = fs::read_dir(&dir).ok();
    // Resolve `.lnk` shortcuts on this thread's STA; the shared guard balances
    // the initialization (or tolerates a thread already in an apartment).
    let _com = super::com::ComApartment::sta();

    let items: Vec<PinnedApp> = entries
        .into_iter()
        .flatten()
        .flatten()
        .filter_map(|entry| {
            let path = entry.path();
            let is_lnk = path
                .extension()
                .and_then(|e| e.to_str())
                .is_some_and(|e| e.eq_ignore_ascii_case("lnk"));
            if !is_lnk {
                return None;
            }
            let name = path.file_stem()?.to_string_lossy().into_owned();
            let launch_path = path.to_string_lossy().into_owned();
            Some(pin_from_shortcut(name, &launch_path))
        })
        .collect();

    // Windows 11 stores packaged pins only in the Taskband blob; merge them
    // with `.lnk` pins and keep one row per launch target or normalized AUMID.
    let mut deduped: Vec<PinnedApp> = Vec::with_capacity(items.len());
    for pin in items.into_iter().chain(import_packaged_pins()) {
        if !deduped
            .iter()
            .any(|existing| pins_share_identity(existing, &pin))
        {
            deduped.push(pin);
        }
    }
    deduped.sort_by_key(|a| a.name.to_lowercase());
    deduped
}

/// Normalized lowercase AUMID for a pin, from `pin.aumid` or an AppsFolder `launch_path`.
pub fn pin_normalized_aumid(pin: &PinnedApp) -> Option<String> {
    pin.aumid
        .as_deref()
        .map(str::trim)
        .filter(|id| !id.is_empty())
        .map(|id| apps_folder_id(id).unwrap_or(id).to_ascii_lowercase())
        .or_else(|| apps_folder_id(&pin.launch_path).map(str::to_ascii_lowercase))
}

/// True when two pins represent the same app by either normalized `launch_path`
/// or normalized `aumid` (including `shell:AppsFolder\<aumid>`).
pub fn pins_share_identity(a: &PinnedApp, b: &PinnedApp) -> bool {
    let a_path = a.launch_path.trim();
    let b_path = b.launch_path.trim();
    if !a_path.is_empty() && a_path.eq_ignore_ascii_case(b_path) {
        return true;
    }
    if let (Some(a_id), Some(b_id)) = (pin_normalized_aumid(a), pin_normalized_aumid(b))
        && a_id == b_id
    {
        return true;
    }
    false
}

/// True when a saved pin/unpin spec (`launch_path`, `shell:AppsFolder\<aumid>`, or
/// legacy `WindowsApps\...\App.exe`) identifies the same app as `pin`.
pub fn spec_matches_pin(spec: &str, pin: &PinnedApp) -> bool {
    let spec = spec.trim();
    if spec.is_empty() {
        return false;
    }
    if pin.launch_path.trim().eq_ignore_ascii_case(spec) {
        return true;
    }
    if let Some(pin_aumid) = pin_normalized_aumid(pin) {
        if let Some(spec_aumid) = apps_folder_id(spec)
            && spec_aumid.eq_ignore_ascii_case(&pin_aumid)
        {
            return true;
        }
        if let Some(target_exe) = pin.target_exe.as_deref().map(str::trim)
            && !target_exe.is_empty()
            && !is_frame_host_spec(target_exe)
            && target_exe.eq_ignore_ascii_case(spec)
        {
            return true;
        }
        if spec.to_ascii_lowercase().contains(r"\windowsapps\")
            && resolve_windowsapps_aumid(spec)
                .is_some_and(|resolved| resolved.eq_ignore_ascii_case(&pin_aumid))
        {
            return true;
        }
    }
    false
}

fn pin_from_shortcut(name: String, launch_path: &str) -> PinnedApp {
    let meta = resolve_shortcut(launch_path);
    let aumid = meta.aumid.or_else(|| {
        meta.target_exe
            .as_deref()
            .and_then(resolve_windowsapps_aumid)
    });
    let icon_source = if let Some(id) = aumid.as_deref().filter(|id| is_apps_folder_app(id)) {
        format!("{APPS_FOLDER_PREFIX}{id}")
    } else {
        meta.icon_location
            .filter(|loc| !is_frame_host_spec(loc))
            .or_else(|| {
                meta.target_exe
                    .clone()
                    .filter(|exe| !is_frame_host_spec(exe))
            })
            .unwrap_or_else(|| launch_path.to_owned())
    };
    PinnedApp {
        name,
        launch_path: launch_path.to_owned(),
        target_exe: meta.target_exe,
        aumid,
        icon_source,
    }
}

/// Builds a pin from a filesystem item or an AppsFolder target.
pub fn pin_from_path(raw_path: &str) -> Option<PinnedApp> {
    if let Some(id) = apps_folder_id(raw_path) {
        return resolve_packaged_app(id).or_else(|| {
            let launch_path = format!("{APPS_FOLDER_PREFIX}{id}");
            Some(PinnedApp {
                name: id.to_owned(),
                launch_path: launch_path.clone(),
                target_exe: None,
                aumid: Some(id.to_owned()),
                icon_source: launch_path,
            })
        });
    }
    let path = Path::new(raw_path);
    let name = path.file_stem()?.to_string_lossy().into_owned();
    if let Some(id) = resolve_windowsapps_aumid(raw_path) {
        return resolve_packaged_app(&id).map(|mut pin| {
            if pin.target_exe.is_none() {
                pin.target_exe = Some(raw_path.to_owned());
            }
            pin
        });
    }
    if !path.exists() {
        return None;
    }
    if path
        .extension()
        .is_some_and(|ext| ext.eq_ignore_ascii_case("lnk"))
    {
        let _com = super::com::ComApartment::sta();
        return Some(pin_from_shortcut(name, raw_path));
    }
    Some(PinnedApp {
        name,
        launch_path: raw_path.to_owned(),
        target_exe: Some(raw_path.to_owned()),
        aumid: None,
        icon_source: raw_path.to_owned(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn legacy_pin_json_defaults_aumid_and_omits_it_when_serializing() {
        let json =
            r#"{"name":"App","launch_path":"app.exe","target_exe":null,"icon_source":"app.exe"}"#;
        let mut pin: PinnedApp = serde_json::from_str(json).unwrap();
        assert!(pin.aumid.is_none());
        assert!(serde_json::to_value(&pin).unwrap().get("aumid").is_none());
        pin.aumid = Some(EXPLORER_AUMID.to_owned());
        let decoded: PinnedApp =
            serde_json::from_value(serde_json::to_value(&pin).unwrap()).unwrap();
        assert_eq!(decoded.aumid.as_deref(), Some(EXPLORER_AUMID));
    }

    #[test]
    fn icon_candidates_prefer_appsfolder_and_exclude_frame_host() {
        assert_eq!(
            icon_candidates_for_app(
                Some(EXPLORER_AUMID),
                r"C:\Windows\explorer.exe",
                Some("Explorer.lnk"),
            ),
            [
                r"shell:AppsFolder\Microsoft.Windows.Explorer",
                "Explorer.lnk",
                r"C:\Windows\explorer.exe",
            ]
        );
        assert_eq!(
            icon_candidates_for_app(
                Some("Example.App_0123456789abc!App"),
                r"C:\Windows\System32\ApplicationFrameHost.exe",
                None,
            ),
            [r"shell:AppsFolder\Example.App_0123456789abc!App"]
        );
        assert_eq!(
            icon_candidates_for_app(
                Some("Microsoft.VisualStudioCode"),
                r"C:\Apps\Code\Code.exe",
                Some(r"C:\Links\Code.lnk"),
            ),
            [r"C:\Links\Code.lnk", r"C:\Apps\Code\Code.exe"]
        );
    }

    #[test]
    fn appsfolder_pins_bypass_filesystem_exists_checks() {
        let explorer = pin_from_path(r"shell:appsfolder\Microsoft.Windows.Explorer").unwrap();
        assert_eq!(explorer.aumid.as_deref(), Some(EXPLORER_AUMID));
        assert_eq!(
            explorer.launch_path,
            r"shell:AppsFolder\Microsoft.Windows.Explorer"
        );

        let settings = pin_from_path(
            r"shell:AppsFolder\windows.immersivecontrolpanel_cw5n1h2txyewy!microsoft.windows.immersivecontrolpanel",
        )
        .unwrap();
        assert_eq!(settings.name, "Settings");
        assert_eq!(
            settings.aumid.as_deref(),
            Some(
                "windows.immersivecontrolpanel_cw5n1h2txyewy!microsoft.windows.immersivecontrolpanel"
            )
        );
        assert_eq!(
            settings.icon_source,
            r"shell:AppsFolder\windows.immersivecontrolpanel_cw5n1h2txyewy!microsoft.windows.immersivecontrolpanel"
        );
    }

    #[test]
    fn explorer_store_shortcut_preserves_non_default_application_id() {
        let args = r#"/separate,"SHELL:AppsFolder\Example.Product_0123456789abc!Client""#;
        assert_eq!(
            explorer_arguments_aumid(args),
            Some("Example.Product_0123456789abc!Client")
        );
        assert_eq!(
            explorer_arguments_aumid(r"shell:AppsFolder\Chrome.Profile"),
            None
        );
    }

    #[test]
    fn package_manifest_selects_the_application_for_the_executable() {
        let xml = r#"<Package xmlns="urn:test">
            <Applications>
                <Application Id="Main" Executable="Main.exe"/>
                <Application Executable="tools/Second.exe" Id="Second"/>
            </Applications>
        </Package>"#;
        assert_eq!(
            manifest_app_id(xml, r"tools\SECOND.EXE").as_deref(),
            Some("Second")
        );
        assert_eq!(manifest_app_id(xml, "Unknown.exe"), None);
        assert_eq!(manifest_app_id("<invalid>", "Unknown.exe"), None);
    }

    #[test]
    fn manifest_application_resolves_display_name_and_executable() {
        let xml = r#"<Package xmlns="urn:test">
            <Properties><DisplayName>Example App</DisplayName></Properties>
            <Applications>
                <Application Id="Client" Executable="Client.exe">
                    <VisualElements DisplayName="Example Client"/>
                </Application>
            </Applications>
        </Package>"#;
        assert_eq!(
            manifest_application(xml, "client"),
            Some((
                Some("Example Client".to_owned()),
                Some("Client.exe".to_owned())
            ))
        );
        assert_eq!(manifest_application(xml, "Missing"), None);
    }

    #[test]
    fn packaged_pin_import_degrades_gracefully_without_taskband() {
        assert!(resolve_packaged_app("not-an-aumid").is_none());
        // No Explorer Taskband on CI: must return an empty list, never panic.
        let _ = import_packaged_pins();
    }

    #[test]
    fn pin_identity_deduplicates_matching_launch_paths_and_aumids() {
        let aumid = "Microsoft.WindowsTerminal_8wekyb3d8bbwe!App";
        let apps_folder = format!("{APPS_FOLDER_PREFIX}{aumid}");
        let taskband_pin = PinnedApp {
            name: "Terminal".into(),
            launch_path: apps_folder.clone(),
            target_exe: Some(
                r"C:\Program Files\WindowsApps\Microsoft.WindowsTerminal_1.0_x64__8wekyb3d8bbwe\WindowsTerminal.exe"
                    .into(),
            ),
            aumid: Some(aumid.into()),
            icon_source: apps_folder.clone(),
        };
        let legacy_shortcut_pin = PinnedApp {
            name: "Windows Terminal".into(),
            launch_path: r"C:\Users\Test\Terminal.lnk".into(),
            target_exe: Some(
                r"C:\Program Files\WindowsApps\Microsoft.WindowsTerminal_1.0_x64__8wekyb3d8bbwe\WindowsTerminal.exe"
                    .into(),
            ),
            aumid: Some(aumid.to_ascii_lowercase()),
            icon_source: apps_folder.clone(),
        };
        let apps_folder_no_aumid_field = PinnedApp {
            name: "Terminal".into(),
            launch_path: apps_folder.to_ascii_lowercase(),
            target_exe: None,
            aumid: None,
            icon_source: apps_folder,
        };
        let other_pin = PinnedApp {
            name: "Notepad".into(),
            launch_path: r"C:\Windows\System32\notepad.exe".into(),
            target_exe: Some(r"C:\Windows\System32\notepad.exe".into()),
            aumid: None,
            icon_source: r"C:\Windows\System32\notepad.exe".into(),
        };

        assert!(pins_share_identity(&taskband_pin, &legacy_shortcut_pin));
        assert!(pins_share_identity(
            &taskband_pin,
            &apps_folder_no_aumid_field
        ));
        assert!(pins_share_identity(
            &legacy_shortcut_pin,
            &apps_folder_no_aumid_field
        ));
        assert!(!pins_share_identity(&taskband_pin, &other_pin));
        assert!(spec_matches_pin(
            r"C:\Program Files\WindowsApps\Microsoft.WindowsTerminal_1.0_x64__8wekyb3d8bbwe\WindowsTerminal.exe",
            &taskband_pin
        ));
        assert!(spec_matches_pin(
            r"SHELL:AppsFolder\microsoft.windowsterminal_8wekyb3d8bbwe!app",
            &legacy_shortcut_pin
        ));
    }
}
