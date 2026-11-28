//! Win32 running-window enumeration with DWM cloaked filtering and activation helpers.

use std::sync::mpsc;

use windows::Win32::Foundation::{HWND as WinHwnd, PROPERTYKEY, RPC_E_CHANGED_MODE};
use windows::Win32::System::Com::StructuredStorage::{PropVariantClear, PropVariantToStringAlloc};
use windows::Win32::System::Com::{
    CLSCTX_ALL, COINIT_APARTMENTTHREADED, CoCreateInstance, CoInitializeEx, CoTaskMemFree,
    CoUninitialize,
};
use windows::Win32::UI::Shell::IVirtualDesktopManager;
use windows::Win32::UI::Shell::PropertiesSystem::{IPropertyStore, SHGetPropertyStoreForWindow};
use windows::core::GUID;
use windows_sys::Win32::Foundation::{CloseHandle, HANDLE, HWND, LPARAM};
use windows_sys::Win32::Graphics::Dwm::{DWMWA_CLOAKED, DwmGetWindowAttribute};
use windows_sys::Win32::System::Threading::{
    OpenProcess, PROCESS_QUERY_LIMITED_INFORMATION, PROCESS_TERMINATE, QueryFullProcessImageNameW,
    TerminateProcess,
};
use windows_sys::Win32::UI::WindowsAndMessaging::{
    EnumChildWindows, EnumWindows, GW_OWNER, GWL_EXSTYLE, GetClassNameW, GetForegroundWindow,
    GetWindow, GetWindowLongPtrW, GetWindowTextLengthW, GetWindowTextW, GetWindowThreadProcessId,
    IsIconic, IsWindow, IsWindowVisible, PostMessageW, SW_MINIMIZE, SW_RESTORE,
    SetForegroundWindow, ShowWindow, WM_CLOSE, WS_EX_APPWINDOW, WS_EX_NOACTIVATE, WS_EX_TOOLWINDOW,
};
use windows_sys::core::BOOL;

// `GetApplicationUserModelId` lives in `Win32_Storage_Packaging_Appx`, which is
// not enabled for this crate's windows-sys feature set. Declaring the kernel32
// entry point here keeps the AUMID lookup self-contained without widening the
// workspace dependency surface.
const ERROR_INSUFFICIENT_BUFFER: u32 = 122;
const MAX_AUMID_UTF16_UNITS: u32 = 4096;

#[link(name = "kernel32")]
unsafe extern "system" {
    /// With a null/short buffer this returns ERROR_INSUFFICIENT_BUFFER and
    /// writes the required UTF-16 length (including the terminator).
    fn GetApplicationUserModelId(
        hprocess: HANDLE,
        applicationusermodelidlength: *mut u32,
        applicationusermodelid: *mut u16,
    ) -> u32;
}

thread_local! {
    /// `IVirtualDesktopManager` is apartment-bound, so it is cached per dock UI thread.
    static VIRTUAL_DESKTOP_MANAGER: std::cell::OnceCell<Option<IVirtualDesktopManager>> =
        const { std::cell::OnceCell::new() };
}

/// True when `hwnd` belongs to the active virtual desktop. Reader-friendly fallback:
/// if the COM manager is unavailable (older Windows / no COM), keep the window visible.
pub fn is_on_current_desktop(hwnd: usize) -> bool {
    VIRTUAL_DESKTOP_MANAGER.with(|cell| {
        let manager = cell.get_or_init(|| {
            // CLSID_VirtualDesktopManager (not exported by the windows crate).
            let clsid = GUID::from_u128(0xaa509086_5ca9_4c25_8f95_589d3c07b48a);
            // SAFETY: COM is initialized by the dock before window enumeration.
            unsafe { CoCreateInstance::<_, IVirtualDesktopManager>(&clsid, None, CLSCTX_ALL) }.ok()
        });
        let Some(manager) = manager else {
            return true;
        };
        // SAFETY: `hwnd` is a live top-level window handle from EnumWindows.
        unsafe { manager.IsWindowOnCurrentVirtualDesktop(WinHwnd(hwnd as *mut core::ffi::c_void)) }
            .map(|on_current| on_current.as_bool())
            .unwrap_or(true)
    })
}

#[derive(Clone, Debug)]
pub struct RunningWindow {
    pub hwnd: usize,
    pub pid: u32,
    pub title: String,
    /// Image of the hosted app, or of the owner for an ordinary window.
    pub exe_path: String,
    /// Shell or process AppUserModelID, empty when neither provides an identity.
    pub aumid: String,
    pub is_foreground: bool,
}

/// Raw `DWMWA_CLOAKED` value (0 when the query failed). Kept separate from the
/// boolean helper so the `CLOAKED_SHELL` virtual-desktop exception survives.
fn cloak_state(hwnd: HWND) -> u32 {
    let mut cloaked: u32 = 0;
    // SAFETY: querying DWMWA_CLOAKED into a local u32.
    let hr = unsafe {
        DwmGetWindowAttribute(
            hwnd,
            DWMWA_CLOAKED as u32,
            (&mut cloaked as *mut u32).cast(),
            size_of::<u32>() as u32,
        )
    };
    if hr == 0 { cloaked } else { 0 }
}

fn is_cloaked(hwnd: HWND) -> bool {
    cloak_state(hwnd) != 0
}

fn has_app_window_style(hwnd: HWND) -> bool {
    // SAFETY: The HWND is supplied by EnumWindows on the caller's thread.
    let (style, owner) = unsafe {
        (
            GetWindowLongPtrW(hwnd, GWL_EXSTYLE) as u32,
            GetWindow(hwnd, GW_OWNER),
        )
    };
    style & WS_EX_NOACTIVATE == 0
        && ((style & WS_EX_TOOLWINDOW == 0 && owner.is_null()) || style & WS_EX_APPWINDOW != 0)
}

/// AppUserModelID of a packaged process via `GetApplicationUserModelId`.
/// Classic Win32 processes (and handles we cannot query) return an empty
/// string, matching the `GetProcessPackagedAppId` fallback behaviour.
fn process_aumid(handle: HANDLE) -> String {
    // SAFETY: null buffer with a length out-parameter is the documented
    // "query required size" call; it writes length and returns
    // ERROR_INSUFFICIENT_BUFFER.
    let mut length = 0u32;
    let status = unsafe { GetApplicationUserModelId(handle, &mut length, std::ptr::null_mut()) };
    if status != ERROR_INSUFFICIENT_BUFFER || length == 0 || length > MAX_AUMID_UTF16_UNITS {
        return String::new();
    }
    let mut buffer = vec![0u16; length as usize];
    // SAFETY: writable buffer of exactly the reported length, owned for the call.
    let status = unsafe { GetApplicationUserModelId(handle, &mut length, buffer.as_mut_ptr()) };
    if status != 0 || length == 0 {
        return String::new();
    }
    let end = (length as usize).min(buffer.len());
    let mut chars = &buffer[..end];
    while chars.last() == Some(&0) {
        chars = &chars[..chars.len() - 1];
    }
    String::from_utf16_lossy(chars)
}

#[derive(Default)]
struct ProcessInfo {
    exe_path: String,
    aumid: String,
}

/// Opens one limited-query handle and reads both the image path and the
/// packaged AUMID, so enumeration pays for only one `OpenProcess` per window.
fn process_info(pid: u32) -> Option<ProcessInfo> {
    // SAFETY: limited query handle closed before returning.
    unsafe {
        let handle = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, pid);
        if handle.is_null() {
            return None;
        }
        let mut buf = [0u16; 1024];
        let mut len = buf.len() as u32;
        let ok = QueryFullProcessImageNameW(handle, 0, buf.as_mut_ptr(), &mut len);
        let aumid = process_aumid(handle);
        CloseHandle(handle);
        Some(ProcessInfo {
            exe_path: if ok != 0 {
                String::from_utf16_lossy(&buf[..len as usize])
            } else {
                String::new()
            },
            aumid,
        })
    }
}

fn process_image_path(pid: u32) -> Option<String> {
    process_info(pid)
        .map(|info| info.exe_path)
        .filter(|path| !path.is_empty())
}

struct EnumerationApartment(bool);

impl Drop for EnumerationApartment {
    fn drop(&mut self) {
        if self.0 {
            // SAFETY: balances this thread's successful CoInitializeEx, including S_FALSE.
            unsafe { CoUninitialize() };
        }
    }
}

fn window_aumid(hwnd: HWND) -> Option<String> {
    const KEY: PROPERTYKEY = PROPERTYKEY {
        fmtid: GUID::from_u128(0x9f4c2855_9f79_4b39_a8d0_e1d42de1d5f3),
        pid: 5,
    };
    // SAFETY: the enumeration thread has a COM apartment. Both allocations are
    // released here, including when string conversion fails.
    unsafe {
        let store: IPropertyStore = SHGetPropertyStoreForWindow(WinHwnd(hwnd)).ok()?;
        let mut value = store.GetValue(&KEY).ok()?;
        let text = PropVariantToStringAlloc(&value).ok().and_then(|text| {
            let string = text.to_string().ok();
            CoTaskMemFree(Some(text.0.cast()));
            string
        });
        let _ = PropVariantClear(&mut value);
        text.map(|id| id.trim().to_owned())
            .filter(|id| !id.is_empty())
    }
}

fn window_has_class(hwnd: HWND, expected: &str) -> bool {
    let mut buf = [0u16; 64];
    // SAFETY: bounded buffer and a borrowed window handle.
    let len = unsafe { GetClassNameW(hwnd, buf.as_mut_ptr(), buf.len() as i32) };
    len > 0
        && buf[..len as usize]
            .iter()
            .copied()
            .eq(expected.encode_utf16())
}

fn hosted_process_info(
    hwnd: HWND,
    frame_pid: u32,
    has_com: bool,
    window_id: Option<&str>,
) -> Option<ProcessInfo> {
    struct Children {
        frame_pid: u32,
        entries: Vec<(bool, u32, HWND)>,
    }

    unsafe extern "system" fn visit(hwnd: HWND, lparam: LPARAM) -> BOOL {
        // SAFETY: EnumChildWindows calls this synchronously with a live Children.
        unsafe {
            let children = &mut *(lparam as *mut Children);
            let mut pid = 0;
            GetWindowThreadProcessId(hwnd, &mut pid);
            if pid != 0 && pid != children.frame_pid {
                children.entries.push((
                    window_has_class(hwnd, "Windows.UI.Core.CoreWindow"),
                    pid,
                    hwnd,
                ));
            }
        }
        1
    }

    let mut children = Children {
        frame_pid,
        entries: Vec::new(),
    };
    // SAFETY: the callback state lives until enumeration returns.
    unsafe {
        EnumChildWindows(
            hwnd,
            Some(visit),
            (&mut children as *mut Children) as LPARAM,
        );
    }
    children.entries.sort_by_key(|&(is_core, _, _)| !is_core);
    let mut seen = std::collections::HashSet::new();
    let mut best = None;
    for (is_core, pid, child_hwnd) in children.entries {
        if !seen.insert(pid) {
            continue;
        }
        let mut info = process_info(pid).unwrap_or_default();
        if info.aumid.is_empty()
            && has_com
            && let Some(child_id) = window_aumid(child_hwnd)
        {
            info.aumid = child_id;
        }
        let has_aumid = !info.aumid.is_empty();
        if !has_aumid && info.exe_path.is_empty() {
            continue;
        }
        let matches_window = window_id.is_some_and(|id| id.eq_ignore_ascii_case(&info.aumid));
        let rank = (matches_window, has_aumid, is_core);
        if best.as_ref().is_none_or(|(previous, _)| rank > *previous) {
            best = Some((rank, info));
        }
    }
    best.map(|(_, info)| info)
}

fn window_title(hwnd: HWND) -> String {
    // SAFETY: reading window title into buffer.
    unsafe {
        let len = GetWindowTextLengthW(hwnd);
        if len <= 0 {
            return String::new();
        }
        let mut buf = vec![0u16; (len + 1) as usize];
        let copied = GetWindowTextW(hwnd, buf.as_mut_ptr(), buf.len() as i32);
        if copied <= 0 {
            String::new()
        } else {
            String::from_utf16_lossy(&buf[..copied as usize])
        }
    }
}

fn is_shell_noise(exe_path: &str) -> bool {
    let lower = exe_path.to_lowercase();
    lower.ends_with(r"\textinputhost.exe")
        || lower.ends_with(r"\shellexperiencehost.exe")
        || lower.ends_with(r"\searchhost.exe")
        || lower.ends_with(r"\startmenuexperiencehost.exe")
        || lower.ends_with(r"\lockapp.exe")
}

struct EnumCtx {
    self_pid: u32,
    has_com: bool,
    foreground: HWND,
    out: Vec<RunningWindow>,
}

unsafe extern "system" fn enum_proc(hwnd: HWND, lparam: LPARAM) -> BOOL {
    // SAFETY: `lparam` points to `EnumCtx` alive for the duration of `EnumWindows`.
    unsafe {
        let ctx = &mut *(lparam as *mut EnumCtx);
        if IsWindowVisible(hwnd) == 0 {
            return 1;
        }
        // One authoritative shell-eligibility check: WS_EX owner/tool/noactivate
        // rules, ITaskbarList_Deleted, hidden DWM cloak (with the shell-cloak
        // exception) and hidden immersive frames.
        if !crate::window_rules::has_taskbar_button_for_hwnd(hwnd as usize, cloak_state(hwnd)) {
            return 1;
        }
        let title = window_title(hwnd);
        if title.is_empty() {
            return 1;
        }
        let mut pid = 0u32;
        GetWindowThreadProcessId(hwnd, &mut pid);
        if pid == 0 || pid == ctx.self_pid {
            return 1;
        }
        let property_id = ctx.has_com.then(|| window_aumid(hwnd)).flatten();
        let owner = process_info(pid).unwrap_or_default();
        if is_shell_noise(&owner.exe_path) {
            return 1;
        }
        let is_frame = window_has_class(hwnd, "ApplicationFrameWindow")
            || std::path::Path::new(&owner.exe_path)
                .file_name()
                .and_then(|name| name.to_str())
                .is_some_and(|name| name.eq_ignore_ascii_case("ApplicationFrameHost.exe"));
        let info = if is_frame {
            hosted_process_info(hwnd, pid, ctx.has_com, property_id.as_deref()).unwrap_or_default()
        } else {
            owner
        };
        if is_shell_noise(&info.exe_path) {
            return 1;
        }
        ctx.out.push(RunningWindow {
            hwnd: hwnd as usize,
            pid,
            title,
            exe_path: info.exe_path,
            aumid: property_id.unwrap_or(info.aumid),
            is_foreground: hwnd == ctx.foreground,
        });
        1
    }
}

pub fn enumerate_windows() -> Vec<RunningWindow> {
    // SAFETY: COM initialization is per-thread. A caller's existing MTA can also use COM.
    let hr = unsafe { CoInitializeEx(None, COINIT_APARTMENTTHREADED) };
    let _apartment = EnumerationApartment(hr.is_ok());
    // SAFETY: `EnumWindows` invokes `enum_proc` synchronously on this thread.
    let foreground = unsafe { GetForegroundWindow() };
    let mut ctx = EnumCtx {
        self_pid: std::process::id(),
        has_com: hr.is_ok() || hr == RPC_E_CHANGED_MODE,
        foreground,
        out: Vec::with_capacity(24),
    };
    unsafe {
        EnumWindows(Some(enum_proc), (&mut ctx as *mut EnumCtx) as LPARAM);
    }
    ctx.out
}

/// Non-blocking snapshot: runs the synchronous enumeration on a worker thread
/// and delivers the result through `tx`. Callers (e.g. the dock's 800 ms
/// refresh) must never block the UI thread on `OpenProcess`/DWM/COM calls.
/// The synchronous [`enumerate_windows`] API is kept for callers that already
/// run off the UI thread.
pub fn enumerate_windows_async(tx: mpsc::Sender<Vec<RunningWindow>>) {
    let result = std::thread::Builder::new()
        .name("opentaskbar-window-enum".into())
        .spawn(move || {
            let windows = enumerate_windows();
            let _ = tx.send(windows);
        });
    if let Err(err) = result {
        opentaskbar_core::log::warn(format!("Window enumeration worker failed: {err}"));
    }
}

/// Finds a visible or hidden application window for an already verified process.
pub fn find_app_window(pid: u32) -> Option<usize> {
    struct Search {
        pid: u32,
        hwnd: Option<usize>,
    }

    unsafe extern "system" fn visit(hwnd: HWND, lparam: LPARAM) -> BOOL {
        // SAFETY: EnumWindows invokes this callback synchronously while Search is alive.
        unsafe {
            let search = &mut *(lparam as *mut Search);
            let mut owner_pid = 0;
            GetWindowThreadProcessId(hwnd, &mut owner_pid);
            if owner_pid != search.pid
                || GetWindowTextLengthW(hwnd) == 0
                || !has_app_window_style(hwnd)
                || is_cloaked(hwnd)
            {
                return 1;
            }
            if IsWindowVisible(hwnd) != 0 {
                search.hwnd = Some(hwnd as usize);
                return 0;
            }
            search.hwnd.get_or_insert(hwnd as usize);
            1
        }
    }

    let mut search = Search { pid, hwnd: None };
    // SAFETY: The local Search outlives every synchronous callback.
    unsafe { EnumWindows(Some(visit), (&mut search as *mut Search) as LPARAM) };
    search.hwnd
}

/// Activates and restores a specific window.
pub fn activate_window(hwnd_val: usize) {
    if hwnd_val == 0 {
        return;
    }
    let hwnd = hwnd_val as HWND;
    // SAFETY: standard Win32 activation calls on a live window handle.
    unsafe {
        if IsWindow(hwnd) == 0 {
            return;
        }
        if IsIconic(hwnd) != 0 {
            ShowWindow(hwnd, SW_RESTORE);
        }
        SetForegroundWindow(hwnd);
    }
}

/// Verifies that an HWND still belongs to the process captured in the snapshot. Without this,
/// a handle recycled between the 800 ms tracker refresh and the user's click can target an
/// unrelated application.
fn identity_matches(hwnd_val: usize, pid: u32) -> bool {
    if hwnd_val == 0 || pid == 0 || pid == std::process::id() {
        return false;
    }
    let mut current = 0u32;
    // SAFETY: querying the PID of an arbitrary handle is safe; the result is only compared.
    unsafe { GetWindowThreadProcessId(hwnd_val as HWND, &mut current) };
    current == pid
}

/// Requests a graceful close only when the HWND still belongs to the captured process.
pub fn close_window_verified(hwnd_val: usize, pid: u32) {
    if identity_matches(hwnd_val, pid) {
        close_window(hwnd_val);
    }
}

/// Terminates the captured process only when the HWND still belongs to it and its image path
/// still matches the app represented by the dock button.
pub fn terminate_window_process_verified(hwnd_val: usize, pid: u32, expected_exe: &str) {
    if !identity_matches(hwnd_val, pid) {
        return;
    }
    if expected_exe.is_empty()
        || !process_image_path(pid).is_some_and(|path| path.eq_ignore_ascii_case(expected_exe))
    {
        return;
    }
    // SAFETY: verified PID; only this process handle is opened and closed.
    unsafe {
        let handle = OpenProcess(PROCESS_TERMINATE, 0, pid);
        if !handle.is_null() {
            let _ = TerminateProcess(handle, 1);
            CloseHandle(handle);
        }
    }
}

/// Requests a graceful close (`WM_CLOSE`) for a specific window.
pub fn close_window(hwnd_val: usize) {
    if hwnd_val == 0 {
        return;
    }
    let hwnd = hwnd_val as HWND;
    // SAFETY: asynchronous `WM_CLOSE` message to a live window.
    unsafe {
        if IsWindow(hwnd) != 0 {
            PostMessageW(hwnd, WM_CLOSE, 0, 0);
        }
    }
}

/// Forcefully terminates the process owning `hwnd_val`.
pub fn terminate_window_process(hwnd_val: usize) {
    if hwnd_val == 0 {
        return;
    }
    // SAFETY: queries PID and terminates only the target window's process.
    unsafe {
        let mut pid = 0u32;
        GetWindowThreadProcessId(hwnd_val as HWND, &mut pid);
        if pid == 0 || pid == std::process::id() {
            return;
        }
        let handle = OpenProcess(PROCESS_TERMINATE, 0, pid);
        if !handle.is_null() {
            let _ = TerminateProcess(handle, 1);
            CloseHandle(handle);
        }
    }
}

/// Cycles forward or backward through an app's open windows (e.g. mouse wheel on dock icon).
pub fn cycle_windows(hwnds: &[usize], forward: bool) {
    if hwnds.is_empty() {
        return;
    }
    let fg = unsafe { GetForegroundWindow() as usize };
    let next = match hwnds.iter().position(|&h| h == fg) {
        Some(pos) => {
            if forward {
                hwnds[(pos + 1) % hwnds.len()]
            } else {
                hwnds[(pos + hwnds.len() - 1) % hwnds.len()]
            }
        }
        None => hwnds[0],
    };
    activate_window(next);
}

/// Cycles or toggles activation for a group of windows belonging to one app.
/// Returns `true` if a live window was activated/toggled, or `false` if all HWNDs were stale
/// (so the caller can launch a fresh instance instead of doing nothing, fixing issue #173).
pub fn activate_or_toggle(hwnds: &[usize]) -> bool {
    let live: Vec<usize> = hwnds
        .iter()
        .copied()
        .filter(|&h| {
            let hwnd = h as HWND;
            unsafe { h != 0 && IsWindow(hwnd) != 0 && IsWindowVisible(hwnd) != 0 }
        })
        .collect();
    if live.is_empty() {
        return false;
    }
    // SAFETY: standard Win32 activation calls on verified live windows.
    unsafe {
        let fg = GetForegroundWindow() as usize;
        if live.len() == 1 {
            let hwnd = live[0] as HWND;
            if fg == live[0] && IsIconic(hwnd) == 0 {
                ShowWindow(hwnd, SW_MINIMIZE);
            } else {
                activate_window(live[0]);
            }
            return true;
        }
        cycle_windows(&live, true);
        true
    }
}

/// Same as [`activate_or_toggle`], but drops windows whose HWND was recycled into a different
/// process since the snapshot was captured.
pub fn activate_or_toggle_verified(windows: &[(usize, u32)]) -> bool {
    let live: Vec<usize> = windows
        .iter()
        .filter(|(hwnd, pid)| {
            let handle = *hwnd as HWND;
            identity_matches(*hwnd, *pid)
                && unsafe { IsWindow(handle) != 0 && IsWindowVisible(handle) != 0 }
        })
        .map(|(hwnd, _)| *hwnd)
        .collect();
    if live.is_empty() {
        return false;
    }
    let fg = unsafe { GetForegroundWindow() as usize };
    if live.len() == 1 {
        let hwnd = live[0] as HWND;
        if fg == live[0] && unsafe { IsIconic(hwnd) } == 0 {
            unsafe { ShowWindow(hwnd, SW_MINIMIZE) };
        } else {
            activate_window(live[0]);
        }
        return true;
    }
    cycle_windows(&live, true);
    true
}

#[cfg(test)]
mod tests {
    use super::*;
    use windows_sys::Win32::UI::WindowsAndMessaging::{
        CreateWindowExW, DestroyWindow, WS_OVERLAPPEDWINDOW,
    };
    use windows_sys::core::PCWSTR;
    use windows_sys::w;

    struct HiddenWindow(HWND);

    impl HiddenWindow {
        fn new(style: u32, title: PCWSTR) -> Self {
            // SAFETY: STATIC is a system window class. No window is shown or activated.
            let hwnd = unsafe {
                CreateWindowExW(
                    style,
                    w!("STATIC"),
                    title,
                    WS_OVERLAPPEDWINDOW,
                    0,
                    0,
                    200,
                    100,
                    std::ptr::null_mut(),
                    std::ptr::null_mut(),
                    std::ptr::null_mut(),
                    std::ptr::null(),
                )
            };
            assert!(!hwnd.is_null());
            Self(hwnd)
        }
    }

    impl Drop for HiddenWindow {
        fn drop(&mut self) {
            // SAFETY: The same test thread owns the fixture window.
            unsafe { DestroyWindow(self.0) };
        }
    }

    #[test]
    fn background_app_lookup_skips_utility_windows_and_finds_a_hidden_app_window() {
        let pid = std::process::id();
        let _utility = HiddenWindow::new(WS_EX_TOOLWINDOW, w!("Utility"));
        let _untitled = HiddenWindow::new(0, w!(""));
        let _no_activate = HiddenWindow::new(WS_EX_NOACTIVATE, w!("Notification"));
        assert_eq!(find_app_window(pid), None);

        let app = HiddenWindow::new(0, w!("Application window"));
        assert_eq!(find_app_window(pid), Some(app.0 as usize));
    }

    #[test]
    fn running_window_carries_the_packaged_aumid() {
        let window = RunningWindow {
            hwnd: 42,
            pid: 7,
            title: "WhatsApp".into(),
            exe_path: r"C:\WhatsApp.Root.exe".into(),
            aumid: "5319275A.WhatsAppDesktop_cv1g1gvanyjgm!App".into(),
            is_foreground: false,
        };
        let cloned = window.clone();
        assert_eq!(cloned.aumid, "5319275A.WhatsAppDesktop_cv1g1gvanyjgm!App");
        assert_eq!(cloned.exe_path, r"C:\WhatsApp.Root.exe");
        assert_eq!(cloned.hwnd, 42);

        let classic = RunningWindow {
            aumid: String::new(),
            ..window
        };
        assert!(classic.aumid.is_empty());
    }

    #[test]
    fn process_info_rejects_an_invalid_pid_and_aumid_rejects_a_null_handle() {
        assert!(process_info(u32::MAX).is_none());
        assert!(process_image_path(u32::MAX).is_none());
        // A null process handle is never a packaged app.
        assert!(process_aumid(std::ptr::null_mut()).is_empty());
    }

    #[test]
    fn async_snapshot_delivers_a_vector_without_blocking() {
        let (tx, rx) = mpsc::channel();
        enumerate_windows_async(tx);
        let snapshot = rx
            .recv_timeout(std::time::Duration::from_secs(10))
            .expect("worker must deliver a snapshot");
        // In a headless CI run there may be no eligible windows; elevated or
        // restricted windows may legitimately have an empty `exe_path`, so only
        // require valid top-level identity fields.
        assert!(
            snapshot
                .iter()
                .all(|window| window.hwnd != 0 && window.pid != 0 && !window.title.is_empty())
        );
    }

    #[test]
    fn enumeration_apartment_balances_com_init_and_tolerates_changed_mode() {
        use windows::Win32::System::Com::COINIT_MULTITHREADED;

        std::thread::spawn(|| {
            // SAFETY: fresh thread initializes STA, then verifies RAII drop balances it.
            let hr = unsafe { CoInitializeEx(None, COINIT_APARTMENTTHREADED) };
            assert!(hr.is_ok());
            let apartment = EnumerationApartment(hr.is_ok());
            drop(apartment);

            // Switch thread to MTA, then verify STA init returns RPC_E_CHANGED_MODE
            // without owning the foreign apartment.
            let mta = unsafe { CoInitializeEx(None, COINIT_MULTITHREADED) };
            assert!(mta.is_ok());
            let mta_guard = EnumerationApartment(mta.is_ok());
            let changed = unsafe { CoInitializeEx(None, COINIT_APARTMENTTHREADED) };
            assert_eq!(changed, RPC_E_CHANGED_MODE);
            let nested = EnumerationApartment(changed.is_ok());
            assert!(!nested.0);
            drop(nested);
            drop(mta_guard);
        })
        .join()
        .expect("COM apartment guard must not fault");
    }
}
