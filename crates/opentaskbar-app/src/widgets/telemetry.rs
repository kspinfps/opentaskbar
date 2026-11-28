//! Native Win32 telemetry providers for all 23 dock widgets.
//!
//! Includes CPU/RAM + 45-sample sparkline history + top processes by memory, Core Audio output &
//! input (microphone) volume/mute/device names, media transport keys, network throughput + session
//! totals (`GetIfTable2`), system battery, Bluetooth PnP device battery, fixed storage drives,
//! keyboard layout switcher with localized display names, day progress, cached weather (Open-Meteo),
//! Recycle Bin status, AI CLI credential presence, and optional GPU usage/temperature sampling.

use std::collections::{HashMap, VecDeque};
use std::ffi::c_void;
use std::os::windows::ffi::OsStrExt;
use std::os::windows::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Instant, SystemTime, UNIX_EPOCH};

use opentaskbar_core::i18n as loc;
use opentaskbar_core::settings::{ClockDateFormat, ClockDateStyle, DockSettings};

use windows::Win32::Foundation::PROPERTYKEY;
use windows::Win32::Media::Audio::Endpoints::IAudioEndpointVolume;
use windows::Win32::Media::Audio::{
    EDataFlow, IMMDeviceEnumerator, MMDeviceEnumerator, eCapture, eMultimedia, eRender,
};
use windows::Win32::System::Com::{
    CLSCTX_ALL, COINIT_APARTMENTTHREADED, CoCreateInstance, CoInitializeEx, CoUninitialize,
    STGM_READ,
};
use windows_sys::Win32::Foundation::{
    CloseHandle, FARPROC, FILETIME, FreeLibrary, HMODULE, LocalFree, SYSTEMTIME,
};
use windows_sys::Win32::Globalization::{
    GetLocaleInfoW, LOCALE_SISO639LANGNAME, LOCALE_SLOCALIZEDDISPLAYNAME,
};
use windows_sys::Win32::NetworkManagement::IpHelper::{FreeMibTable, GetIfTable2, MIB_IF_TABLE2};
use windows_sys::Win32::NetworkManagement::Ndis::IfOperStatusUp;
use windows_sys::Win32::Security::Cryptography::{
    CRYPT_INTEGER_BLOB, CRYPTPROTECT_UI_FORBIDDEN, CryptProtectData, CryptUnprotectData,
};
use windows_sys::Win32::Storage::FileSystem::{GetDiskFreeSpaceExW, GetLogicalDrives};
use windows_sys::Win32::System::LibraryLoader::{
    GetProcAddress, LOAD_LIBRARY_SEARCH_SYSTEM32, LoadLibraryExW, LoadLibraryW,
};
use windows_sys::Win32::System::Power::{GetSystemPowerStatus, SYSTEM_POWER_STATUS};
use windows_sys::Win32::System::ProcessStatus::{
    EnumProcesses, GetProcessMemoryInfo, PROCESS_MEMORY_COUNTERS,
};
use windows_sys::Win32::System::Registry::{
    HKEY_CURRENT_USER, KEY_READ, KEY_SET_VALUE, REG_BINARY, RegCloseKey, RegOpenKeyExW,
    RegQueryValueExW, RegSetValueExW,
};
use windows_sys::Win32::System::SystemInformation::{
    GetLocalTime, GetSystemTime, GetTickCount64, GlobalMemoryStatusEx, MEMORYSTATUSEX,
};
use windows_sys::Win32::System::Threading::{
    GetSystemTimes, OpenProcess, PROCESS_QUERY_LIMITED_INFORMATION, PROCESS_VM_READ,
    QueryFullProcessImageNameW,
};
use windows_sys::Win32::UI::Input::KeyboardAndMouse::{
    GetKeyboardLayout, GetKeyboardLayoutList, INPUT, INPUT_0, INPUT_KEYBOARD, KEYBDINPUT,
    KEYEVENTF_KEYUP, SendInput,
};
use windows_sys::Win32::UI::Shell::{
    SHERB_NOCONFIRMATION, SHERB_NOPROGRESSUI, SHERB_NOSOUND, SHEmptyRecycleBinW, SHQUERYRBINFO,
    SHQueryRecycleBinW,
};
use windows_sys::Win32::UI::WindowsAndMessaging::{
    GetForegroundWindow, GetWindowThreadProcessId, PostMessageW, WM_INPUTLANGCHANGEREQUEST,
};

const CREATE_NO_WINDOW: u32 = 0x0800_0000;
const SPARKLINE_CAPACITY: usize = 45;
const HTTP_USER_AGENT: &str = "OpenTaskbar/1.0";
const HTTPS_PORT: u16 = 443;
const HTTP_MAX_BODY_BYTES: usize = 1024 * 1024;
/// DNS / connect / send / receive budgets: short for weather, generous for AI full payloads.
const HTTP_TIMEOUTS_FAST: (i32, i32, i32, i32) = (4_000, 4_000, 5_000, 6_000);
const HTTP_TIMEOUTS_AI: (i32, i32, i32, i32) = (15_000, 15_000, 15_000, 15_000);
/// `LOCALE_IFIRSTDAYOFWEEK`: 0 = Monday .. 6 = Sunday.
const LOCALE_IFIRSTDAYOFWEEK: u32 = 0x0000_100C;
const LOCALE_USER_DEFAULT: u32 = 0x0400;

/// Native `TIME_ZONE_INFORMATION` (the `Win32_System_Time` feature is not enabled in this crate).
#[repr(C)]
struct WinTimeZoneInformation {
    bias: i32,
    standard_name: [u16; 32],
    standard_date: SYSTEMTIME,
    standard_bias: i32,
    daylight_name: [u16; 32],
    daylight_date: SYSTEMTIME,
    daylight_bias: i32,
}

#[link(name = "kernel32")]
unsafe extern "system" {
    fn GetTimeZoneInformation(info: *mut WinTimeZoneInformation) -> u32;
}

/// Current local UTC offset in seconds (positive east of UTC, negative west of UTC).
pub(crate) fn local_utc_offset_secs() -> i64 {
    let mut info: WinTimeZoneInformation = unsafe { std::mem::zeroed() };
    // SAFETY: `info` matches Win32 `TIME_ZONE_INFORMATION` and is stack-owned.
    let mode = unsafe { GetTimeZoneInformation(&mut info) };
    // Windows `Bias` satisfies: UTC = local_time + bias_minutes, so east-of-UTC offset = -bias.
    let extra = match mode {
        2 => info.daylight_bias, // TIME_ZONE_ID_DAYLIGHT
        1 => info.standard_bias, // TIME_ZONE_ID_STANDARD
        _ => 0,
    };
    -i64::from(info.bias + extra) * 60
}

/// Standard/daylight display name for the current timezone (`TimeZoneInfo` parity).
pub fn timezone_display_name() -> String {
    let mut info: WinTimeZoneInformation = unsafe { std::mem::zeroed() };
    let id = unsafe { GetTimeZoneInformation(&mut info) };
    let name = if id == 2 {
        info.daylight_name
    } else {
        info.standard_name
    };
    let len = name.iter().position(|c| *c == 0).unwrap_or(name.len());
    let value = String::from_utf16_lossy(&name[..len]).trim().to_owned();
    if value.is_empty() {
        "Local".to_owned()
    } else {
        value
    }
}

/// Culture's first day of week, `0 = Sunday .. 6 = Saturday` (`FirstDayOfWeek`).
pub fn first_day_of_week() -> u32 {
    let mut buf = [0u16; 4];
    let len = unsafe {
        GetLocaleInfoW(
            LOCALE_USER_DEFAULT,
            LOCALE_IFIRSTDAYOFWEEK,
            buf.as_mut_ptr(),
            buf.len() as i32,
        )
    };
    if len <= 1 {
        return 0;
    }
    let raw = String::from_utf16_lossy(&buf[..(len - 1) as usize])
        .trim()
        .parse::<u32>()
        .unwrap_or(0);
    // Win32 reports Monday = 0; .NET DayOfWeek reports Sunday = 0.
    (raw + 1) % 7
}

// PKEY_Device_FriendlyName = {a45c254e-df1c-4efd-8020-67d146a850e0}, pid 14
const PKEY_DEVICE_FRIENDLY_NAME: PROPERTYKEY = PROPERTYKEY {
    fmtid: windows::core::GUID::from_u128(0xa45c254e_df1c_4efd_8020_67d146a850e0),
    pid: 14,
};

/// Live Windows SMTC (System Media Transport Controls) session snapshot.
#[derive(Clone, Debug, Default)]
pub struct MediaSessionSnapshot {
    pub title: String,
    pub artist: String,
    /// SMTC `SourceAppUserModelId`; used by `media_ext::librewolf_timeline` to gate the UIA scan.
    pub source_app_id: String,
    pub playing: bool,
    pub position_secs: u64,
    pub duration_secs: u64,
    pub cover_pixels: Option<Arc<(u32, u32, Vec<u8>)>>,
    pub fetched_at: Option<Instant>,
    pub in_flight: bool,
}

/// Queries the active Windows SMTC media session on a background thread (non-blocking).
pub fn refresh_media_session(cache: &Arc<Mutex<MediaSessionSnapshot>>, force: bool) {
    let should_fetch = if let Ok(mut guard) = cache.lock() {
        if guard.in_flight
            || (!force && guard.fetched_at.is_some_and(|t| t.elapsed().as_secs() < 3))
        {
            false
        } else {
            guard.in_flight = true;
            true
        }
    } else {
        false
    };
    if !should_fetch {
        return;
    }

    let cache_clone = cache.clone();
    std::thread::Builder::new()
        .name("media-smtc".to_owned())
        .spawn(move || {
            let next = query_smtc_session();
            if let Ok(mut guard) = cache_clone.lock() {
                if let Some((title, artist, source_app_id, playing, pos, dur, cover_pixels)) = next
                {
                    guard.title = title;
                    guard.artist = artist;
                    guard.source_app_id = source_app_id.clone();
                    guard.playing = playing;
                    guard.position_secs = pos;
                    guard.duration_secs = dur;
                    guard.cover_pixels = cover_pixels;
                    // LibreWolf often publishes a frozen SMTC timeline. The real UIA reader is
                    // the authoritative source for its progress bar (WS-6 contract).
                    if !guard.title.is_empty()
                        && let Some((uia_pos, uia_dur)) =
                            super::media_ext::librewolf_timeline(&source_app_id, &guard.title)
                    {
                        guard.position_secs = uia_pos.round().max(0.0) as u64;
                        guard.duration_secs = uia_dur.round().max(0.0) as u64;
                    }
                } else {
                    guard.title.clear();
                    guard.artist.clear();
                    guard.source_app_id.clear();
                    guard.playing = false;
                    guard.position_secs = 0;
                    guard.duration_secs = 0;
                    guard.cover_pixels = None;
                }
                guard.fetched_at = Some(Instant::now());
                guard.in_flight = false;
            }
        })
        .ok();
}

type SmtcCoverPixels = Arc<(u32, u32, Vec<u8>)>;
type SmtcSessionTuple = (
    String,
    String,
    String,
    bool,
    u64,
    u64,
    Option<SmtcCoverPixels>,
);

fn query_smtc_session() -> Option<SmtcSessionTuple> {
    use windows::Media::Control::{
        GlobalSystemMediaTransportControlsSessionManager,
        GlobalSystemMediaTransportControlsSessionPlaybackStatus,
    };
    let manager = GlobalSystemMediaTransportControlsSessionManager::RequestAsync()
        .ok()?
        .join()
        .ok()?;
    let session = manager.GetCurrentSession().ok()?;
    let props = session.TryGetMediaPropertiesAsync().ok()?.join().ok()?;
    let title = props.Title().map(|s| s.to_string()).unwrap_or_default();
    let artist = props.Artist().map(|s| s.to_string()).unwrap_or_default();
    if title.is_empty() && artist.is_empty() {
        return None;
    }
    let cover_pixels = props
        .Thumbnail()
        .ok()
        .and_then(|reference| reference.OpenReadAsync().ok()?.join().ok())
        .and_then(|stream| {
            use windows::Storage::Streams::DataReader;
            let size = stream.Size().ok()?;
            if size == 0 || size > 5 * 1024 * 1024 {
                return None;
            }
            let input = stream.GetInputStreamAt(0).ok()?;
            let reader = DataReader::CreateDataReader(&input).ok()?;
            reader.LoadAsync(size as u32).ok()?.join().ok()?;
            let mut bytes = vec![0; size as usize];
            reader.ReadBytes(&mut bytes).ok()?;
            super::widget_icons::decode_bgra_bytes(&bytes)
        });
    let source_app_id = session
        .SourceAppUserModelId()
        .map(|s| s.to_string())
        .unwrap_or_default();
    let playing = session
        .GetPlaybackInfo()
        .ok()
        .and_then(|info| info.PlaybackStatus().ok())
        .is_some_and(|st| st == GlobalSystemMediaTransportControlsSessionPlaybackStatus::Playing);
    let (pos_secs, dur_secs) = session
        .GetTimelineProperties()
        .ok()
        .map(|tl| {
            let pos = tl
                .Position()
                .map(|t| (t.Duration.max(0) / 10_000_000) as u64)
                .unwrap_or(0);
            let end = tl
                .EndTime()
                .map(|t| (t.Duration.max(0) / 10_000_000) as u64)
                .unwrap_or(0);
            (pos.min(end), end)
        })
        .unwrap_or((0, 0));
    Some((
        title,
        artist,
        source_app_id,
        playing,
        pos_secs,
        dur_secs,
        cover_pixels,
    ))
}

fn ft_to_u64(ft: FILETIME) -> u64 {
    (u64::from(ft.dwHighDateTime) << 32) | u64::from(ft.dwLowDateTime)
}

/// Fixed-capacity ring buffer storing `[0.0, 1.0]` telemetry samples (matching `MetricRingBuffer`).
#[derive(Clone, Debug)]
pub struct MetricHistory {
    samples: VecDeque<f32>,
}

impl Default for MetricHistory {
    fn default() -> Self {
        Self {
            samples: VecDeque::with_capacity(SPARKLINE_CAPACITY),
        }
    }
}

impl MetricHistory {
    pub fn push(&mut self, value: f32) {
        if self.samples.len() >= SPARKLINE_CAPACITY {
            self.samples.pop_front();
        }
        self.samples.push_back(value.clamp(0.0, 1.0));
    }

    pub fn as_vec(&self) -> Vec<f32> {
        self.samples.iter().copied().collect()
    }
}

#[derive(Default)]
pub struct SystemStatsSampler {
    prev_idle: u64,
    prev_kernel: u64,
    prev_user: u64,
    pub cpu_history: MetricHistory,
    pub ram_history: MetricHistory,
    /// Real GPU utilisation history (45 samples), matching `SystemStatsWidget.gpuSparkline`.
    pub gpu_history: MetricHistory,
}

impl SystemStatsSampler {
    pub fn sample(&mut self) -> (u32, u32) {
        let cpu = self.sample_cpu();
        let ram = sample_ram_info().0;
        self.cpu_history.push(cpu as f32 / 100.0);
        self.ram_history.push(ram as f32 / 100.0);
        (cpu, ram)
    }

    fn sample_cpu(&mut self) -> u32 {
        let (mut idle, mut kernel, mut user) = (
            FILETIME {
                dwLowDateTime: 0,
                dwHighDateTime: 0,
            },
            FILETIME {
                dwLowDateTime: 0,
                dwHighDateTime: 0,
            },
            FILETIME {
                dwLowDateTime: 0,
                dwHighDateTime: 0,
            },
        );
        if unsafe { GetSystemTimes(&mut idle, &mut kernel, &mut user) } == 0 {
            return 0;
        }
        let (iv, kv, uv) = (ft_to_u64(idle), ft_to_u64(kernel), ft_to_u64(user));
        let pct = if self.prev_kernel != 0 || self.prev_user != 0 {
            let total = kv
                .saturating_sub(self.prev_kernel)
                .saturating_add(uv.saturating_sub(self.prev_user));
            if total > 0 {
                let busy = total.saturating_sub(iv.saturating_sub(self.prev_idle));
                ((busy.saturating_mul(100)) / total).min(100) as u32
            } else {
                0
            }
        } else {
            0
        };
        (self.prev_idle, self.prev_kernel, self.prev_user) = (iv, kv, uv);
        pct
    }
}

pub fn sample_ram_info() -> (u32, f32, f32) {
    let mut mem: MEMORYSTATUSEX = unsafe { std::mem::zeroed() };
    mem.dwLength = size_of::<MEMORYSTATUSEX>() as u32;
    if unsafe { GlobalMemoryStatusEx(&mut mem) } != 0 {
        let total = mem.ullTotalPhys as f64 / 1_073_741_824.0;
        let used = (total - mem.ullAvailPhys as f64 / 1_073_741_824.0).max(0.0);
        (mem.dwMemoryLoad.min(100), used as f32, total as f32)
    } else {
        (0, 0.0, 0.0)
    }
}

/// Top real processes ordered by Working Set memory (matches `SystemStats.SampleTopProcesses`).
pub fn sample_top_processes(limit: usize) -> Vec<(String, u64)> {
    let mut pids = vec![0u32; 1024];
    let mut needed = 0u32;
    if unsafe {
        EnumProcesses(
            pids.as_mut_ptr(),
            (pids.len() * size_of::<u32>()) as u32,
            &mut needed,
        )
    } == 0
    {
        return Vec::new();
    }
    let count = (needed as usize / size_of::<u32>()).min(pids.len());
    let mut entries: Vec<(String, u64)> = Vec::with_capacity(64);
    let mut path_buf = [0u16; 260];
    for &pid in &pids[..count] {
        if pid <= 4 {
            continue;
        }
        unsafe {
            let h = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION | PROCESS_VM_READ, 0, pid);
            if h.is_null() {
                continue;
            }
            let mut pmc: PROCESS_MEMORY_COUNTERS = std::mem::zeroed();
            pmc.cb = size_of::<PROCESS_MEMORY_COUNTERS>() as u32;
            let got_mem = GetProcessMemoryInfo(h, &mut pmc, pmc.cb) != 0;
            let mut len = path_buf.len() as u32;
            let got_name =
                QueryFullProcessImageNameW(h, 0, path_buf.as_mut_ptr(), &mut len) != 0 && len > 0;
            let _ = CloseHandle(h);
            if got_mem && got_name {
                let full = String::from_utf16_lossy(&path_buf[..len as usize]);
                let name = full
                    .rsplit(['\\', '/'])
                    .next()
                    .unwrap_or(&full)
                    .trim_end_matches(".exe")
                    .trim_end_matches(".EXE")
                    .to_owned();
                if !name.is_empty() {
                    entries.push((name, pmc.WorkingSetSize as u64));
                }
            }
        }
    }
    // Aggregate multiple helper processes with the same executable name (e.g. Chrome/Edge/Code).
    let mut merged: std::collections::HashMap<String, u64> = std::collections::HashMap::new();
    for (name, bytes) in entries {
        *merged.entry(name).or_insert(0) += bytes;
    }
    let mut list: Vec<(String, u64)> = merged.into_iter().collect();
    list.sort_by_key(|b| std::cmp::Reverse(b.1));
    list.truncate(limit);
    list
}

// --- GPU Usage & Temperature (PDH + optional NVIDIA NVML) ---

const PDH_FMT_DOUBLE: u32 = 0x0000_0200;
const PDH_MORE_DATA: u32 = 0x8000_07D2;
const GPU_ENGINE_PATH: &str = r"\GPU Engine(*)\Utilization Percentage";

/// Native `PDH_FMT_COUNTERVALUE`: a `DWORD` status followed by an 8-byte aligned value union.
#[repr(C)]
struct PdhFmtValue {
    status: u32,
    value: f64,
}

/// Native `PDH_FMT_COUNTERVALUE_ITEM_W`: instance name plus the formatted value above.
#[repr(C)]
struct PdhFmtItemW {
    name: *const u16,
    value: PdhFmtValue,
}

#[link(name = "pdh")]
#[allow(non_snake_case)]
unsafe extern "system" {
    fn PdhOpenQueryW(data_source: *const u16, user_data: usize, query: *mut isize) -> u32;
    fn PdhAddCounterW(
        query: isize,
        counter_path: *const u16,
        user_data: usize,
        counter: *mut isize,
    ) -> u32;
    fn PdhAddEnglishCounterW(
        query: isize,
        counter_path: *const u16,
        user_data: usize,
        counter: *mut isize,
    ) -> u32;
    fn PdhCollectQueryData(query: isize) -> u32;
    fn PdhGetFormattedCounterArrayW(
        counter: isize,
        format: u32,
        buffer_size: *mut u32,
        item_count: *mut u32,
        buffer: *mut c_void,
    ) -> u32;
    fn PdhGetFormattedCounterValue(
        counter: isize,
        format: u32,
        counter_type: *mut u32,
        value: *mut PdhFmtValue,
    ) -> u32;
    fn PdhCloseQuery(query: isize) -> u32;
}

/// Copies a PDH-owned NUL-terminated wide string (`szName`) into a Rust `String`.
unsafe fn pdh_wide_string(text: *const u16) -> String {
    if text.is_null() {
        return String::new();
    }
    // SAFETY: PDH supplies a valid NUL-terminated wide string for each formatted item.
    unsafe {
        let mut len = 0usize;
        while len < 1024 && *text.add(len) != 0 {
            len += 1;
        }
        String::from_utf16_lossy(std::slice::from_raw_parts(text, len))
    }
}

/// Group key for one GPU engine node, e.g. `0x00000000_0x0000F3A1_phys_0|3D`.
///
/// PDH instance names look like
/// `pid_1234_luid_0x00000000_0x0000F3A1_phys_0_eng_0_engtype_3D`.
fn gpu_engine_key(instance: &str) -> Option<String> {
    const LUID: &str = "_luid_";
    const ENGINE: &str = "_engtype_";
    let luid_at = instance.find(LUID)? + LUID.len();
    let engine_at = instance.find(ENGINE)?;
    if engine_at <= luid_at {
        return None;
    }
    let luid = &instance[luid_at..engine_at];
    let engine = &instance[engine_at + ENGINE.len()..];
    if luid.is_empty() || luid.len() > 96 || engine.is_empty() || engine.len() > 64 {
        return None;
    }
    Some(format!("{luid}|{engine}"))
}

/// Engine kind (e.g. `"3D"`, `"Video Decode"`) from a PDH instance name such as
/// `pid_1234_luid_0x00000000_0x0000F3A1_phys_0_eng_0_engtype_3D`.
fn gpu_engine_name(instance: &str) -> Option<&str> {
    if !instance.contains("_luid_") {
        return None;
    }
    let (_, engine) = instance.rsplit_once("_engtype_")?;
    (!engine.is_empty() && engine.len() <= 64).then_some(engine)
}

/// One long-lived PDH query. Rate counters need a time delta, so the query is never recreated.
struct PdhGpu {
    query: isize,
    counter: isize,
    /// `u64` backing store keeps the `PDH_FMT_COUNTERVALUE_ITEM_W` array 8-byte aligned.
    buffer: Vec<u64>,
}

impl PdhGpu {
    fn new() -> Option<Self> {
        let path: Vec<u16> = GPU_ENGINE_PATH.encode_utf16().chain(Some(0)).collect();
        unsafe {
            let mut query = 0isize;
            if PdhOpenQueryW(std::ptr::null(), 0, &mut query) != 0 || query == 0 {
                return None;
            }
            let mut counter = 0isize;
            // English path first, then the localized counter on non-English Windows.
            let added = (PdhAddEnglishCounterW(query, path.as_ptr(), 0, &mut counter) == 0
                || PdhAddCounterW(query, path.as_ptr(), 0, &mut counter) == 0)
                && counter != 0;
            if !added {
                let _ = PdhCloseQuery(query);
                return None;
            }
            Some(Self {
                query,
                counter,
                buffer: vec![0u64; 512],
            })
        }
    }

    /// Collects one sample and returns the busiest engine load in percent (`0..=100`) together
    /// with the engine kind that produced it (e.g. `"3D"`; the localized `GpuEngine` label when unknown).
    fn read_max(&mut self) -> Option<(f64, String)> {
        unsafe {
            if PdhCollectQueryData(self.query) != 0 {
                return None;
            }

            let mut size = (self.buffer.len() * size_of::<u64>()) as u32;
            let mut count = 0u32;
            let mut status = PdhGetFormattedCounterArrayW(
                self.counter,
                PDH_FMT_DOUBLE,
                &mut size,
                &mut count,
                self.buffer.as_mut_ptr().cast(),
            );
            if status == PDH_MORE_DATA && size > 0 {
                let words = (size as usize).div_ceil(size_of::<u64>()).max(1);
                self.buffer.resize(words, 0);
                size = (self.buffer.len() * size_of::<u64>()) as u32;
                status = PdhGetFormattedCounterArrayW(
                    self.counter,
                    PDH_FMT_DOUBLE,
                    &mut size,
                    &mut count,
                    self.buffer.as_mut_ptr().cast(),
                );
            }

            let mut found = false;
            let mut best = 0.0f64;
            let mut best_engine = String::new();
            if status == 0 && count > 0 {
                let capacity = (self.buffer.len() * size_of::<u64>()) / size_of::<PdhFmtItemW>();
                let items = std::slice::from_raw_parts(
                    self.buffer.as_ptr().cast::<PdhFmtItemW>(),
                    (count as usize).min(capacity),
                );
                // Keyed by luid+engine (one GPU engine node) with its engine label alongside.
                let mut by_engine: HashMap<String, (f64, String)> = HashMap::new();
                for item in items {
                    // Status > 1 means "no data" / "not validated": skip that instance.
                    if item.value.status > 1 || !item.value.value.is_finite() {
                        continue;
                    }
                    let value = item.value.value.max(0.0);
                    found = true;
                    let name = pdh_wide_string(item.name);
                    let engine = gpu_engine_name(&name);
                    // `>=` keeps an engine label even for an idle (0%) GPU, as the old flyout did.
                    if value >= best {
                        best = value;
                        best_engine = engine.map_or_else(|| loc::t("GpuEngine"), str::to_owned);
                    }
                    if let (Some(key), Some(engine)) = (gpu_engine_key(&name), engine) {
                        let entry = by_engine.entry(key).or_insert((0.0, engine.to_owned()));
                        entry.0 += value;
                    }
                }
                // reference reports the busiest engine of the busiest GPU: sum per engine node, then max.
                if let Some((_, (value, engine))) = by_engine
                    .into_iter()
                    .max_by(|a, b| (a.1).0.total_cmp(&(b.1).0))
                    && value > best
                {
                    best = value;
                    best_engine = engine;
                }
            }

            // A few systems expose only a single (non-wildcard) counter instance.
            if !found {
                let mut formatted = PdhFmtValue {
                    status: 0,
                    value: 0.0,
                };
                if PdhGetFormattedCounterValue(
                    self.counter,
                    PDH_FMT_DOUBLE,
                    std::ptr::null_mut(),
                    &mut formatted,
                ) == 0
                    && formatted.status <= 1
                    && formatted.value.is_finite()
                {
                    found = true;
                    best = formatted.value.max(0.0);
                    best_engine = loc::t("GpuEngine");
                }
            }
            found.then_some((best, best_engine))
        }
    }
}

impl Drop for PdhGpu {
    fn drop(&mut self) {
        // SAFETY: `query` came from `PdhOpenQueryW` and is closed exactly once here.
        unsafe {
            let _ = PdhCloseQuery(self.query);
        }
    }
}

static GPU_QUERY: OnceLock<Mutex<Option<PdhGpu>>> = OnceLock::new();

/// One PDH sample of the GPU engine counter: `(load in [0, 1], busiest engine name)`.
///
/// Non-blocking: a poisoned/contended query or a collection failure yields `None` rather than
/// stalling a UI frame. A valid but idle GPU yields `Some((0.0, engine))`.
fn sample_gpu_pdh() -> Option<(f32, String)> {
    let slot = GPU_QUERY.get_or_init(|| Mutex::new(PdhGpu::new()));
    let mut guard = slot.try_lock().ok()?;
    let query = guard.as_mut()?;
    // A rate counter's first collection has no baseline; collect twice and keep the larger value.
    let mut best = (0.0f64, String::new());
    let mut saw_data = false;
    for _ in 0..2 {
        if let Some((value, engine)) = query.read_max() {
            saw_data = true;
            if value >= best.0 {
                best = (value, engine);
            }
        }
    }
    if !saw_data {
        return None;
    }
    let ratio = (best.0 / 100.0) as f32;
    ratio.is_finite().then(|| (ratio.clamp(0.0, 1.0), best.1))
}

/// Engine kind of the busiest GPU engine from the latest background PDH sample
/// (e.g. `"3D"`, `"Video Decode"`). Never invokes `PdhCollectQueryData` synchronously
/// on the UI thread; reads the value cached by `sample_gpu_temp()`'s worker.
pub fn sample_gpu_engine_name() -> Option<String> {
    GPU_SAMPLE
        .lock()
        .ok()
        .and_then(|g| g.2.clone())
        .filter(|s| !s.is_empty())
}

// NVML is resolved on demand from System32: no link-time dependency, and a rogue DLL placed next to
// the executable cannot replace the driver library (matches the reference System32-only DllImport search).

type NvmlInitFn = unsafe extern "C" fn() -> i32;
type NvmlShutdownFn = unsafe extern "C" fn() -> i32;
type NvmlCountFn = unsafe extern "C" fn(*mut u32) -> i32;
type NvmlHandleFn = unsafe extern "C" fn(u32, *mut *mut c_void) -> i32;
type NvmlTempFn = unsafe extern "C" fn(*mut c_void, i32, *mut u32) -> i32;

struct NvmlLib {
    /// Integer handle keeps the `OnceLock` static `Send + Sync`.
    module: isize,
    shutdown: NvmlShutdownFn,
    get_count: NvmlCountFn,
    get_handle: NvmlHandleFn,
    get_temp: NvmlTempFn,
}

impl Drop for NvmlLib {
    fn drop(&mut self) {
        // SAFETY: the handle and symbols all came from the same live library loaded below.
        unsafe {
            let _ = (self.shutdown)();
            let _ = FreeLibrary(self.module as HMODULE);
        }
    }
}

/// Resolves one exported DLL symbol into its typed function pointer.
unsafe fn nvml_symbol<T: Copy>(module: HMODULE, name: &[u8]) -> Option<T> {
    // SAFETY: `module` is a live library handle and `name` is NUL-terminated.
    let raw: FARPROC = Some(unsafe { GetProcAddress(module, name.as_ptr().cast())? });
    // `FARPROC` is a niche-optimized `Option<fn()>`; copy its pointer bits into `T`.
    Some(unsafe { std::mem::transmute_copy::<FARPROC, T>(&raw) })
}

fn nvml_lib() -> Option<&'static NvmlLib> {
    NVML.get_or_init(|| unsafe {
        let name: Vec<u16> = "nvml.dll\0".encode_utf16().collect();
        let mut module = LoadLibraryExW(
            name.as_ptr(),
            std::ptr::null_mut(),
            LOAD_LIBRARY_SEARCH_SYSTEM32,
        );
        // Standard (non-DCH) NVIDIA installs keep nvml.dll under Program Files, not System32.
        if module.is_null()
            && let Some(root) = std::env::var_os("ProgramW6432")
        {
            let path = std::path::Path::new(&root)
                .join("NVIDIA Corporation")
                .join("NVSMI")
                .join("nvml.dll");
            let wide: Vec<u16> = path.as_os_str().encode_wide().chain(Some(0)).collect();
            module = LoadLibraryW(wide.as_ptr());
        }
        if module.is_null() {
            return None;
        }
        let (init, shutdown, get_count, get_handle, get_temp) = (
            nvml_symbol::<NvmlInitFn>(module, b"nvmlInit_v2\0"),
            nvml_symbol::<NvmlShutdownFn>(module, b"nvmlShutdown\0"),
            nvml_symbol::<NvmlCountFn>(module, b"nvmlDeviceGetCount_v2\0"),
            nvml_symbol::<NvmlHandleFn>(module, b"nvmlDeviceGetHandleByIndex_v2\0"),
            nvml_symbol::<NvmlTempFn>(module, b"nvmlDeviceGetTemperature\0"),
        );
        let (init, shutdown, get_count, get_handle, get_temp) =
            match (init, shutdown, get_count, get_handle, get_temp) {
                (Some(a), Some(b), Some(c), Some(d), Some(e)) => (a, b, c, d, e),
                _ => {
                    let _ = FreeLibrary(module);
                    return None;
                }
            };
        if init() != 0 {
            let _ = FreeLibrary(module);
            return None;
        }
        Some(NvmlLib {
            module: module as isize,
            shutdown,
            get_count,
            get_handle,
            get_temp,
        })
    })
    .as_ref()
}

static NVML: OnceLock<Option<NvmlLib>> = OnceLock::new();

// --- WDDM adapter temperature (vendor-agnostic, `SystemSensors` parity) ---

#[repr(C)]
#[derive(Clone, Copy, Default)]
struct D3dkmtLuid {
    low_part: u32,
    high_part: i32,
}

#[repr(C)]
#[derive(Clone, Copy, Default)]
struct D3dkmtAdapterInfo {
    adapter: u32,
    adapter_luid: D3dkmtLuid,
    num_of_sources: u32,
    present_move_regions_preferred: u32,
}

#[repr(C)]
struct D3dkmtEnumAdapters2 {
    num_adapters: u32,
    adapters: *mut D3dkmtAdapterInfo,
}

#[repr(C)]
#[derive(Clone, Copy, Default)]
struct D3dkmtCloseAdapter {
    adapter: u32,
}

#[repr(C)]
struct D3dkmtQueryAdapterInfo {
    adapter: u32,
    info_type: u32,
    private_driver_data: *mut std::ffi::c_void,
    private_driver_data_size: u32,
}

#[repr(C)]
#[derive(Clone, Copy, Default)]
struct D3dkmtAdapterPerfData {
    physical_adapter_index: u32,
    memory_frequency: u64,
    max_memory_frequency: u64,
    max_memory_frequency_oc: u64,
    memory_bandwidth: u64,
    pcie_bandwidth: u64,
    fan_rpm: u32,
    power: u32,
    /// Tenths of a degree Celsius.
    temperature: u32,
    power_state_override: u8,
}

// Pin the documented WDDM ABI (d3dkmthk.h): both structs are naturally aligned on x64.
const _: () = assert!(size_of::<D3dkmtAdapterPerfData>() == 64);

#[repr(C)]
#[derive(Clone, Copy, Default)]
struct D3dkmtAdapterPerfDataCaps {
    physical_adapter_index: u32,
    max_memory_bandwidth: u64,
    max_pcie_bandwidth: u64,
    max_fan_rpm: u32,
    /// Deci-Celsius; zero means the driver does not publish a thermal reading.
    temperature_max: u32,
    temperature_warning: u32,
}

const _: () = assert!(size_of::<D3dkmtAdapterPerfDataCaps>() == 40);

// SAFETY: these are the documented gdi32 WDDM entry points; all arguments are live structs.
#[link(name = "gdi32")]
unsafe extern "system" {
    fn D3DKMTEnumAdapters2(arg: *mut D3dkmtEnumAdapters2) -> i32;
    fn D3DKMTCloseAdapter(arg: *mut D3dkmtCloseAdapter) -> i32;
    fn D3DKMTQueryAdapterInfo(arg: *mut D3dkmtQueryAdapterInfo) -> i32;
}

/// Reads GPU temperature through WDDM's `KMTQAITYPE_ADAPTERPERFDATA` (62), which works for
/// AMD/Intel/NVIDIA drivers that expose a thermal reading without vendor-specific DLLs.
fn sample_wddm_temperature() -> Option<f32> {
    const KMTQAITYPE_ADAPTERTYPE: u32 = 15;
    const KMTQAITYPE_ADAPTERPERFDATA: u32 = 62;
    const KMTQAITYPE_ADAPTERPERFDATA_CAPS: u32 = 63;
    /// `D3DKMT_ADAPTERTYPE.SoftwareDevice` bit (Microsoft Basic Render Driver etc.).
    const SOFTWARE_DEVICE_BIT: u32 = 1 << 2;
    let mut count = D3dkmtEnumAdapters2 {
        num_adapters: 0,
        adapters: std::ptr::null_mut(),
    };
    // SAFETY: `count` is a live zeroed struct; a null adapter list asks for the count only.
    if unsafe { D3DKMTEnumAdapters2(&mut count) } != 0 || count.num_adapters == 0 {
        return None;
    }
    let mut adapters = vec![D3dkmtAdapterInfo::default(); count.num_adapters as usize];
    count.adapters = adapters.as_mut_ptr();
    // SAFETY: `adapters` outlives the call and has exactly `num_adapters` elements.
    if unsafe { D3DKMTEnumAdapters2(&mut count) } != 0 {
        return None;
    }
    let mut hottest = 0.0f32;
    for info in &mut adapters {
        // Skip software-only adapters (e.g. Microsoft Basic Render Driver) — they have no
        // physical thermal sensor and would otherwise mask a real GPU's reading with zero.
        let mut adapter_type = 0u32;
        let mut type_query = D3dkmtQueryAdapterInfo {
            adapter: info.adapter,
            info_type: KMTQAITYPE_ADAPTERTYPE,
            private_driver_data: (&mut adapter_type as *mut u32).cast(),
            private_driver_data_size: size_of::<u32>() as u32,
        };
        // SAFETY: `type_query` points at a live u32 with the documented D3DKMT_ADAPTERTYPE size.
        if unsafe { D3DKMTQueryAdapterInfo(&mut type_query) } == 0
            && adapter_type & SOFTWARE_DEVICE_BIT != 0
        {
            let mut close = D3dkmtCloseAdapter {
                adapter: info.adapter,
            };
            // SAFETY: handle was opened by D3DKMTEnumAdapters2 and is closed exactly once.
            unsafe {
                D3DKMTCloseAdapter(&mut close);
            }
            continue;
        }

        // Capabilities tell us whether the driver publishes temperature at all. This is a
        // capability gate, not a prerequisite for the perf query.
        let mut caps = D3dkmtAdapterPerfDataCaps::default();
        let mut caps_query = D3dkmtQueryAdapterInfo {
            adapter: info.adapter,
            info_type: KMTQAITYPE_ADAPTERPERFDATA_CAPS,
            private_driver_data: (&mut caps as *mut D3dkmtAdapterPerfDataCaps).cast(),
            private_driver_data_size: size_of::<D3dkmtAdapterPerfDataCaps>() as u32,
        };
        // SAFETY: `caps_query` points at a live caps struct with the documented size.
        let caps_ok = unsafe { D3DKMTQueryAdapterInfo(&mut caps_query) } == 0;

        // Zero on every pass: a partial driver write must not leave a stale reading behind.
        let mut perf = D3dkmtAdapterPerfData::default();
        let mut query = D3dkmtQueryAdapterInfo {
            adapter: info.adapter,
            info_type: KMTQAITYPE_ADAPTERPERFDATA,
            private_driver_data: (&mut perf as *mut D3dkmtAdapterPerfData).cast(),
            private_driver_data_size: size_of::<D3dkmtAdapterPerfData>() as u32,
        };
        // SAFETY: `query` points at a live perf struct with the exact documented size.
        let ok = unsafe { D3DKMTQueryAdapterInfo(&mut query) } == 0;
        if ok && (!caps_ok || caps.temperature_max > 0) {
            let celsius = perf.temperature as f32 / 10.0;
            if celsius > 0.0 && celsius < 150.0 {
                hottest = hottest.max(celsius);
            }
        }
        let mut close = D3dkmtCloseAdapter {
            adapter: info.adapter,
        };
        // SAFETY: the adapter handle was opened by D3DKMTEnumAdapters2 and is closed exactly once.
        unsafe {
            D3DKMTCloseAdapter(&mut close);
        }
    }
    (hottest > 0.0).then_some(hottest)
}

/// Hottest GPU temperature in degrees Celsius.
///
/// Fallback order: vendor-neutral WDDM adapter perf data (AMD / Intel / NVIDIA), then NVML for
/// NVIDIA drivers without a WDDM thermal reading, then AMD ADL Overdrive5 for older AMD drivers.
/// Returns `None` when no source is available. No admin rights, kernel driver or new Cargo
/// dependency is required.
pub fn sample_gpu_temperature() -> Option<f32> {
    if let Some(celsius) = sample_wddm_temperature() {
        return Some(celsius);
    }
    if let Some(celsius) = sample_nvml_temperature() {
        return Some(celsius);
    }
    sample_adl_temperature()
}

fn sample_nvml_temperature() -> Option<f32> {
    let lib = nvml_lib()?;
    unsafe {
        let mut count = 0u32;
        if (lib.get_count)(&mut count) != 0 || count == 0 {
            return None;
        }
        let mut hottest = 0.0f32;
        for index in 0..count.min(64) {
            let mut device: *mut c_void = std::ptr::null_mut();
            if (lib.get_handle)(index, &mut device) != 0 || device.is_null() {
                continue;
            }
            let mut celsius = 0u32;
            // NVML_TEMPERATURE_GPU == 0.
            if (lib.get_temp)(device, 0, &mut celsius) == 0 {
                let value = celsius as f32;
                if value > 0.0 && value < 150.0 {
                    hottest = hottest.max(value);
                }
            }
        }
        (hottest > 0.0).then_some(hottest)
    }
}

// --- AMD ADL Overdrive5 temperature fallback (optional, loaded on demand) ---

// ADL allocates its own buffers and requires a CRT-compatible allocator (msvcrt.dll malloc).
#[link(name = "msvcrt")]
unsafe extern "C" {
    fn malloc(size: usize) -> *mut c_void;
}

/// CRT-compatible allocation callback passed to `ADL_Main_Control_Create`.
extern "system" fn adl_alloc(size: i32, count: i32) -> *mut c_void {
    let bytes = (size.max(0) as usize).saturating_mul(count.max(0) as usize);
    // SAFETY: `malloc` accepts any byte count (including zero) and may validly return null.
    unsafe { malloc(bytes) }
}

#[repr(C)]
#[derive(Clone, Copy, Default)]
struct AdlTemperature {
    i_size: i32,
    /// Milli-degrees Celsius.
    i_temperature: i32,
}

#[repr(C)]
#[derive(Clone, Copy, Default)]
struct AdlThermalControllerInfo {
    i_size: i32,
    /// `0` == GPU thermal domain (`ADL_TDOMAIN_GPU`).
    i_thermal_domain: i32,
    i_domain_index: i32,
    i_flags: i32,
}

// Pin the documented ADL ABI (adl_structures.h): both structs are naturally 4-byte aligned.
const _: () = assert!(size_of::<AdlTemperature>() == 8);
const _: () = assert!(size_of::<AdlThermalControllerInfo>() == 16);

type AdlAllocFn = extern "system" fn(i32, i32) -> *mut c_void;
type AdlMainControlCreateFn = unsafe extern "system" fn(Option<AdlAllocFn>, i32) -> i32;
type AdlMainControlDestroyFn = unsafe extern "system" fn() -> i32;
type AdlThermalDevicesEnumFn =
    unsafe extern "system" fn(i32, i32, *mut AdlThermalControllerInfo) -> i32;
type AdlTemperatureGetFn = unsafe extern "system" fn(i32, i32, *mut AdlTemperature) -> i32;

struct AdlLib {
    /// Integer handle keeps the `OnceLock` static `Send + Sync`.
    module: isize,
    create: AdlMainControlCreateFn,
    destroy: AdlMainControlDestroyFn,
    enum_thermal: AdlThermalDevicesEnumFn,
    get_temp: AdlTemperatureGetFn,
}

impl Drop for AdlLib {
    fn drop(&mut self) {
        // SAFETY: `module` came from a successful LoadLibrary* call. The ADL control object is
        // created and destroyed within each sample, so only the library handle is released here.
        unsafe {
            let _ = FreeLibrary(self.module as HMODULE);
        }
    }
}

fn adl_lib() -> Option<&'static AdlLib> {
    ADL.get_or_init(|| unsafe {
        let name: Vec<u16> = "atiadlxx.dll\0".encode_utf16().collect();
        let mut module = LoadLibraryExW(
            name.as_ptr(),
            std::ptr::null_mut(),
            LOAD_LIBRARY_SEARCH_SYSTEM32,
        );
        // Non-DCH AMD installs ship the DLL inside the Catalyst control center folder.
        if module.is_null()
            && let Some(root) = std::env::var_os("ProgramW6432")
        {
            let path = PathBuf::from(root)
                .join("ATI Technologies")
                .join("ATI.ACE")
                .join("Core-Static")
                .join("atiadlxx.dll");
            let wide: Vec<u16> = path.as_os_str().encode_wide().chain(Some(0)).collect();
            module = LoadLibraryW(wide.as_ptr());
        }
        if module.is_null() {
            return None;
        }
        let symbols = (
            nvml_symbol::<AdlMainControlCreateFn>(module, b"ADL_Main_Control_Create\0"),
            nvml_symbol::<AdlMainControlDestroyFn>(module, b"ADL_Main_Control_Destroy\0"),
            nvml_symbol::<AdlThermalDevicesEnumFn>(module, b"ADL_Overdrive5_ThermalDevices_Enum\0"),
            nvml_symbol::<AdlTemperatureGetFn>(module, b"ADL_Overdrive5_Temperature_Get\0"),
        );
        let (create, destroy, enum_thermal, get_temp) = match symbols {
            (Some(create), Some(destroy), Some(enum_thermal), Some(get_temp)) => {
                (create, destroy, enum_thermal, get_temp)
            }
            _ => {
                let _ = FreeLibrary(module);
                return None;
            }
        };
        Some(AdlLib {
            module: module as isize,
            create,
            destroy,
            enum_thermal,
            get_temp,
        })
    })
    .as_ref()
}

static ADL: OnceLock<Option<AdlLib>> = OnceLock::new();
/// ADL owns one global control object: serialize create/enumerate/destroy across threads.
static ADL_CONTROL_LOCK: Mutex<()> = Mutex::new(());

/// Hottest GPU temperature in degrees Celsius reported by AMD ADL Overdrive5, if any.
fn sample_adl_temperature() -> Option<f32> {
    let lib = adl_lib()?;
    // Never block a UI frame on a concurrent ADL sample.
    let _guard = ADL_CONTROL_LOCK.try_lock().ok()?;
    // SAFETY: all symbols come from the loaded library, the callback has static lifetime, and
    // `1` asks ADL to enumerate only connected adapters.
    if unsafe { (lib.create)(Some(adl_alloc), 1) } != 0 {
        return None;
    }
    let hottest = unsafe { adl_hottest_temperature(lib) };
    // SAFETY: control was created successfully above and is destroyed exactly once here, even
    // when no thermal controller yielded a plausible reading.
    unsafe {
        let _ = (lib.destroy)();
    }
    hottest
}

/// Reads all plausible GPU thermal controllers with an already-created ADL control object.
///
/// # Safety
/// `lib` must come from a live library whose control object has been created and not yet
/// destroyed by `sample_adl_temperature`.
unsafe fn adl_hottest_temperature(lib: &AdlLib) -> Option<f32> {
    // ADLTemperature and ADLThermalControllerInfo are plain i32 structs (size asserts above).
    let mut hottest = 0.0f32;
    for adapter in 0..=7 {
        for controller in 0..=3 {
            let mut info = AdlThermalControllerInfo {
                i_size: size_of::<AdlThermalControllerInfo>() as i32,
                ..Default::default()
            };
            // SAFETY: `info` is a live, correctly sized ADL struct for this controller index.
            if unsafe { (lib.enum_thermal)(adapter, controller, &mut info) } != 0
                || info.i_thermal_domain != 0
            {
                continue;
            }
            let mut temp = AdlTemperature {
                i_size: size_of::<AdlTemperature>() as i32,
                ..Default::default()
            };
            // SAFETY: `temp` is a live, correctly sized ADL struct for this controller index.
            if unsafe { (lib.get_temp)(adapter, controller, &mut temp) } != 0 {
                continue;
            }
            // ADL reports milli-degrees Celsius.
            let celsius = temp.i_temperature as f32 / 1000.0;
            if celsius.is_finite() && celsius > 0.0 && celsius < 150.0 {
                hottest = hottest.max(celsius);
            }
        }
    }
    (hottest > 0.0).then_some(hottest)
}

// --- Clock & Calendar Helpers ---

#[derive(Clone, Debug)]
pub struct LocalClockInfo {
    pub time_hm: String,
    pub hour: u32,
    pub minute: u32,
    pub second: u32,
    pub short_date: String,
    pub weekday_short: String,
    pub full_date: String,
    pub tz_label: String,
    /// Standard/daylight timezone display name from `GetTimeZoneInformation`.
    pub tz_name: String,
    /// Culture first day of week (`0 = Sunday`).
    pub first_day_of_week: u32,
    pub year: i32,
    pub month: u32,
    pub day: u32,
}

fn month_key(month: u32) -> &'static str {
    match month {
        1 => "MonthJanuary",
        2 => "MonthFebruary",
        3 => "MonthMarch",
        4 => "MonthApril",
        5 => "MonthMay",
        6 => "MonthJune",
        7 => "MonthJuly",
        8 => "MonthAugust",
        9 => "MonthSeptember",
        10 => "MonthOctober",
        11 => "MonthNovember",
        _ => "MonthDecember",
    }
}

/// Localized full month name (1-based).
pub fn month_name(month: u32) -> String {
    loc::t(month_key(month))
}

/// Localized weekday name. `index` is 0 = Sunday .. 6 = Saturday (Win32 convention).
pub fn weekday_name(index: u32) -> String {
    match index {
        0 => loc::t("WeekdaySunday"),
        1 => loc::t("WeekdayMonday"),
        2 => loc::t("WeekdayTuesday"),
        3 => loc::t("WeekdayWednesday"),
        4 => loc::t("WeekdayThursday"),
        5 => loc::t("WeekdayFriday"),
        _ => loc::t("WeekdaySaturday"),
    }
}

fn short_name(full: String) -> String {
    if full.chars().count() <= 3 {
        full
    } else {
        full.chars().take(3).collect()
    }
}

/// Locale-aware short weekday label (`0 = Sunday .. 6 = Saturday`).
/// Avoids Turkish 3-char collisions (`Pazar`/`Pazartesi` -> `Paz`/`Pzt`, `Cuma`/`Cumartesi` -> `Cum`/`Cmt`).
pub fn weekday_short_name(index: u32) -> String {
    let idx = index % 7;
    if loc::language().to_ascii_lowercase().starts_with("tr") {
        return match idx {
            0 => "Paz",
            1 => "Pzt",
            2 => "Sal",
            3 => "Çar",
            4 => "Per",
            5 => "Cum",
            _ => "Cmt",
        }
        .to_owned();
    }
    short_name(weekday_name(idx))
}

/// Short weekday for Monday-first calendar headers (`0 = Monday`).
pub fn weekday_short_monday_first(index: usize) -> String {
    let sunday_index = match index {
        0 => 1,
        1 => 2,
        2 => 3,
        3 => 4,
        4 => 5,
        5 => 6,
        _ => 0,
    };
    weekday_short_name(sunday_index)
}

/// Returns `(year, month, day)` -> days in that Gregorian month.
pub fn days_in_month(year: i32, month: u32) -> u32 {
    match month {
        1 | 3 | 5 | 7 | 8 | 10 | 12 => 31,
        4 | 6 | 9 | 11 => 30,
        2 => {
            if (year % 4 == 0 && year % 100 != 0) || (year % 400 == 0) {
                29
            } else {
                28
            }
        }
        _ => 30,
    }
}

/// Sakamoto's algorithm: returns `0 = Sunday .. 6 = Saturday` for any Gregorian date.
pub fn weekday_of(year: i32, month: u32, day: u32) -> u32 {
    const T: [i32; 12] = [0, 3, 2, 5, 0, 3, 5, 1, 4, 6, 2, 4];
    let y = if month < 3 { year - 1 } else { year };
    let m = (month as usize).clamp(1, 12) - 1;
    ((y + y / 4 - y / 100 + y / 400 + T[m] + day as i32).rem_euclid(7)) as u32
}

/// Leading blank cells before day 1 in a calendar laid out from the culture's
/// first day of week. Both arguments use the .NET `DayOfWeek` convention
/// (`0 = Sunday .. 6 = Saturday`) returned by [`first_day_of_week`] and
/// [`weekday_of`]. This is the formula the flyout uses for its grid; sharing it
/// keeps click hit-testing aligned with the painted cells.
pub fn calendar_leading_blanks(first_day_of_week: u32, weekday_of_first: u32) -> u32 {
    (weekday_of_first + 7 - first_day_of_week % 7) % 7
}

pub fn local_clock_info() -> LocalClockInfo {
    let mut lt: SYSTEMTIME = unsafe { std::mem::zeroed() };
    let mut ut: SYSTEMTIME = unsafe { std::mem::zeroed() };
    unsafe {
        GetLocalTime(&mut lt);
        GetSystemTime(&mut ut);
    }
    let dow_s = weekday_short_name(u32::from(lt.wDayOfWeek));
    let dow_f = weekday_name(u32::from(lt.wDayOfWeek));
    let mon_s = short_name(loc::t(month_key(u32::from(lt.wMonth))));
    let mon_f = loc::t(month_key(u32::from(lt.wMonth)));

    // Derive UTC offset directly from `GetLocalTime` vs `GetSystemTime`.
    let l_mins = i32::from(lt.wHour) * 60 + i32::from(lt.wMinute);
    let u_mins = i32::from(ut.wHour) * 60 + i32::from(ut.wMinute);
    let mut diff = l_mins - u_mins;
    if diff > 720 {
        diff -= 1440;
    } else if diff < -720 {
        diff += 1440;
    }
    let sign = if diff < 0 { "-" } else { "+" };
    let abs_m = diff.abs();
    let tz_label = if abs_m % 60 == 0 {
        format!("UTC{sign}{}", abs_m / 60)
    } else {
        format!("UTC{sign}{}:{:02}", abs_m / 60, abs_m % 60)
    };

    LocalClockInfo {
        time_hm: format!("{:02}:{:02}", lt.wHour, lt.wMinute),
        hour: u32::from(lt.wHour),
        minute: u32::from(lt.wMinute),
        second: u32::from(lt.wSecond),
        short_date: format!("{} {mon_s}", lt.wDay),
        weekday_short: dow_s,
        full_date: format!("{dow_f}, {} {mon_f} {}", lt.wDay, lt.wYear),
        tz_label,
        tz_name: timezone_display_name(),
        first_day_of_week: first_day_of_week(),
        year: i32::from(lt.wYear),
        month: u32::from(lt.wMonth),
        day: u32::from(lt.wDay),
    }
}

#[allow(dead_code)]
/// Applies `Clock24Hour`, `ClockShowSeconds`, `DateStyle` and `DateFormat`
/// to the dock chip text. Returns `(time, date)`.
pub fn format_clock(info: &LocalClockInfo, settings: &DockSettings) -> (String, String) {
    let time = if settings.clock_24_hour {
        if settings.clock_show_seconds {
            format!("{:02}:{:02}:{:02}", info.hour, info.minute, info.second)
        } else {
            info.time_hm.clone()
        }
    } else {
        let hour_12 = match info.hour % 12 {
            0 => 12,
            value => value,
        };
        let suffix = if info.hour < 12 { "AM" } else { "PM" };
        if settings.clock_show_seconds {
            format!("{hour_12}:{:02}:{:02} {suffix}", info.minute, info.second)
        } else {
            format!("{hour_12}:{:02} {suffix}", info.minute)
        }
    };
    let date = match settings.date_style {
        ClockDateStyle::Hidden => String::new(),
        ClockDateStyle::ShortDateOnly => info.short_date.clone(),
        ClockDateStyle::DayAndShortDate => format!("{} {}", info.weekday_short, info.short_date),
        ClockDateStyle::NumericDate => {
            let m = info.month;
            let d = info.day;
            let y = info.year;
            // Every enum style has a distinct reference-matching layout (`FormatDate` in ClockWidget).
            match settings.date_format {
                ClockDateFormat::DdMmYyyy => format!("{d:02}/{m:02}/{y:04}"),
                ClockDateFormat::DdMmYy => format!("{d:02}/{m:02}/{:02}", y % 100),
                ClockDateFormat::MmDdYyyy => format!("{m:02}/{d:02}/{y:04}"),
                ClockDateFormat::YyyyMmDd => format!("{y:04}-{m:02}-{d:02}"),
                ClockDateFormat::DdMmYyyyDots => format!("{d:02}.{m:02}.{y:04}"),
                ClockDateFormat::System => {
                    let lang = loc::language().to_ascii_lowercase();
                    if lang.starts_with("en-us") {
                        format!("{m:02}/{d:02}/{y:04}")
                    } else if lang.starts_with("ja")
                        || lang.starts_with("ko")
                        || lang.starts_with("zh")
                    {
                        format!("{y:04}-{m:02}-{d:02}")
                    } else if lang.starts_with("tr")
                        || lang.starts_with("de")
                        || lang.starts_with("ru")
                        || lang.starts_with("uk")
                        || lang.starts_with("pl")
                    {
                        format!("{d:02}.{m:02}.{y:04}")
                    } else {
                        format!("{d:02}/{m:02}/{y:04}")
                    }
                }
            }
        }
    };
    (time, date)
}

pub fn system_uptime_string() -> String {
    let secs = unsafe { GetTickCount64() } / 1000;
    let (d, h, m) = (secs / 86_400, (secs % 86_400) / 3600, (secs % 3600) / 60);
    if d > 0 {
        loc::t_fmt(
            "StatsUptimeDaysFormat",
            &[&d.to_string(), &h.to_string(), &m.to_string()],
        )
    } else if h > 0 {
        loc::t_fmt("StatsUptimeHoursFormat", &[&h.to_string(), &m.to_string()])
    } else {
        loc::t_fmt("StatsUptimeMinutesFormat", &[&m.to_string()])
    }
}

static RECYCLE_BIN_FULL: AtomicBool = AtomicBool::new(false);
static RECYCLE_BIN_QUERYING: AtomicBool = AtomicBool::new(false);

/// Non-blocking Recycle Bin check: returns cached state immediately and refreshes on a background
/// thread so slow/sleeping drives never stall dock startup or UI frames.
pub fn is_recycle_bin_non_empty() -> bool {
    if !RECYCLE_BIN_QUERYING.swap(true, Ordering::AcqRel) {
        let _ = std::thread::Builder::new()
            .name("opentaskbar-bin-query".into())
            .spawn(|| {
                let mut info: SHQUERYRBINFO = unsafe { std::mem::zeroed() };
                info.cbSize = size_of::<SHQUERYRBINFO>() as u32;
                let full = unsafe {
                    SHQueryRecycleBinW(std::ptr::null(), &mut info) == 0 && info.i64NumItems > 0
                };
                RECYCLE_BIN_FULL.store(full, Ordering::Release);
                RECYCLE_BIN_QUERYING.store(false, Ordering::Release);
            });
    }
    RECYCLE_BIN_FULL.load(Ordering::Acquire)
}

pub fn empty_recycle_bin() {
    let _ = std::thread::Builder::new()
        .name("opentaskbar-empty-bin".into())
        .spawn(|| unsafe {
            let _ = SHEmptyRecycleBinW(
                std::ptr::null_mut(),
                std::ptr::null(),
                SHERB_NOCONFIRMATION | SHERB_NOPROGRESSUI | SHERB_NOSOUND,
            );
        });
}

// --- Core Audio (Output & Microphone Input) ---

struct AudioComGuard {
    initialized: bool,
}

impl AudioComGuard {
    fn new() -> Self {
        // SAFETY: no out-pointer; if the thread already initialized COM in another apartment
        // (`RPC_E_CHANGED_MODE`), `is_ok()` is false so `CoUninitialize` is skipped.
        let initialized = unsafe { CoInitializeEx(None, COINIT_APARTMENTTHREADED).is_ok() };
        Self { initialized }
    }
}

impl Drop for AudioComGuard {
    fn drop(&mut self) {
        if self.initialized {
            // SAFETY: paired with the successful `CoInitializeEx` in `new()`.
            unsafe { CoUninitialize() };
        }
    }
}

fn endpoint_volume_for(flow: EDataFlow) -> Option<IAudioEndpointVolume> {
    unsafe {
        let en: IMMDeviceEnumerator =
            CoCreateInstance(&MMDeviceEnumerator, None, CLSCTX_ALL).ok()?;
        let dev = en.GetDefaultAudioEndpoint(flow, eMultimedia).ok()?;
        dev.Activate::<IAudioEndpointVolume>(CLSCTX_ALL, None).ok()
    }
}

pub fn default_audio_device_name(capture: bool) -> Option<String> {
    let _com = AudioComGuard::new();
    let flow = if capture { eCapture } else { eRender };
    unsafe {
        let en: IMMDeviceEnumerator =
            CoCreateInstance(&MMDeviceEnumerator, None, CLSCTX_ALL).ok()?;
        let dev = en.GetDefaultAudioEndpoint(flow, eMultimedia).ok()?;
        let store = dev.OpenPropertyStore(STGM_READ).ok()?;
        let prop = store.GetValue(&PKEY_DEVICE_FRIENDLY_NAME).ok()?;
        let name = prop.to_string();
        (!name.trim().is_empty()).then_some(name.trim().to_owned())
    }
}

pub fn get_audio_state() -> (u32, bool) {
    let _com = AudioComGuard::new();
    let Some(ep) = endpoint_volume_for(eRender) else {
        return (0, false);
    };
    unsafe {
        let vol = ep
            .GetMasterVolumeLevelScalar()
            .unwrap_or(0.0)
            .clamp(0.0, 1.0);
        let muted = ep.GetMute().map(|b| b.as_bool()).unwrap_or(false);
        ((vol * 100.0).round() as u32, muted)
    }
}

pub fn get_mic_state() -> Option<(u32, bool)> {
    let _com = AudioComGuard::new();
    let ep = endpoint_volume_for(eCapture)?;
    unsafe {
        let vol = ep
            .GetMasterVolumeLevelScalar()
            .unwrap_or(0.0)
            .clamp(0.0, 1.0);
        let muted = ep.GetMute().map(|b| b.as_bool()).unwrap_or(false);
        Some(((vol * 100.0).round() as u32, muted))
    }
}

pub fn adjust_audio_volume(delta_pct: i32) {
    let _com = AudioComGuard::new();
    let Some(ep) = endpoint_volume_for(eRender) else {
        return;
    };
    unsafe {
        let next = (ep.GetMasterVolumeLevelScalar().unwrap_or(0.5) + delta_pct as f32 / 100.0)
            .clamp(0.0, 1.0);
        let _ = ep.SetMasterVolumeLevelScalar(next, std::ptr::null());
        if delta_pct > 0 {
            let _ = ep.SetMute(false, std::ptr::null());
        }
    }
}

/// Sets output volume (`0..=100`). When `unmute_if_increasing` is `false` (e.g. scrolling down
/// while muted), lowers the level without unmuting the endpoint.
pub fn set_audio_volume_preserving_mute(pct: u32, unmute_if_increasing: bool) {
    let _com = AudioComGuard::new();
    let Some(ep) = endpoint_volume_for(eRender) else {
        return;
    };
    let scalar = (pct.min(100) as f32) / 100.0;
    unsafe {
        let _ = ep.SetMasterVolumeLevelScalar(scalar, std::ptr::null());
        if unmute_if_increasing && pct > 0 {
            let _ = ep.SetMute(false, std::ptr::null());
        }
    }
}

pub fn set_audio_volume(pct: u32) {
    set_audio_volume_preserving_mute(pct, true);
}

pub fn toggle_audio_mute() {
    let _com = AudioComGuard::new();
    let Some(ep) = endpoint_volume_for(eRender) else {
        return;
    };
    unsafe {
        let muted = ep.GetMute().map(|b| b.as_bool()).unwrap_or(false);
        let _ = ep.SetMute(!muted, std::ptr::null());
    }
}

pub fn set_mic_volume(pct: u32) {
    let _com = AudioComGuard::new();
    let Some(ep) = endpoint_volume_for(eCapture) else {
        return;
    };
    let scalar = (pct.min(100) as f32) / 100.0;
    unsafe {
        let _ = ep.SetMasterVolumeLevelScalar(scalar, std::ptr::null());
        if pct > 0 {
            let _ = ep.SetMute(false, std::ptr::null());
        }
    }
}

pub fn toggle_mic_mute() {
    let _com = AudioComGuard::new();
    let Some(ep) = endpoint_volume_for(eCapture) else {
        return;
    };
    unsafe {
        let muted = ep.GetMute().map(|b| b.as_bool()).unwrap_or(false);
        let _ = ep.SetMute(!muted, std::ptr::null());
    }
}

pub fn send_media_vk(vk: u16) {
    let mk = |flags| INPUT {
        r#type: INPUT_KEYBOARD,
        Anonymous: INPUT_0 {
            ki: KEYBDINPUT {
                wVk: vk,
                wScan: 0,
                dwFlags: flags,
                time: 0,
                dwExtraInfo: 0,
            },
        },
    };
    let inputs = [mk(0), mk(KEYEVENTF_KEYUP)];
    unsafe {
        SendInput(
            inputs.len() as u32,
            inputs.as_ptr(),
            size_of::<INPUT>() as i32,
        );
    }
}

// --- Network Speed ---

#[derive(Default)]
pub struct NetSpeedSampler {
    prev_rx: u64,
    prev_tx: u64,
    prev_at: Option<Instant>,
    pub rx_bps: u64,
    pub tx_bps: u64,
    pub session_rx: u64,
    pub session_tx: u64,
    pub adapter_name: String,
}

impl NetSpeedSampler {
    pub fn sample(&mut self) -> (u64, u64) {
        let (rx, tx, adapter) = read_total_net_bytes();
        if !adapter.is_empty() {
            self.adapter_name = adapter;
        }
        let now = Instant::now();
        if let Some(prev) = self.prev_at {
            let dt = now.duration_since(prev).as_secs_f64().max(0.2);
            let drx = rx.saturating_sub(self.prev_rx);
            let dtx = tx.saturating_sub(self.prev_tx);
            self.rx_bps = (drx as f64 / dt) as u64;
            self.tx_bps = (dtx as f64 / dt) as u64;
            self.session_rx = self.session_rx.saturating_add(drx);
            self.session_tx = self.session_tx.saturating_add(dtx);
        }
        (self.prev_rx, self.prev_tx, self.prev_at) = (rx, tx, Some(now));
        (self.rx_bps, self.tx_bps)
    }
}

fn read_total_net_bytes() -> (u64, u64, String) {
    let mut table: *mut MIB_IF_TABLE2 = std::ptr::null_mut();
    unsafe {
        if GetIfTable2(&mut table) != 0 || table.is_null() {
            return (0, 0, String::new());
        }
        let rows =
            std::slice::from_raw_parts((*table).Table.as_ptr(), (*table).NumEntries as usize);
        let alias_of = |r: &windows_sys::Win32::NetworkManagement::IpHelper::MIB_IF_ROW2| {
            let end = r
                .Alias
                .iter()
                .position(|&c| c == 0)
                .unwrap_or(r.Alias.len());
            String::from_utf16_lossy(&r.Alias[..end]).trim().to_owned()
        };
        // Prefer physical hardware, non-filter interfaces so WFP/NDIS filter layers do not
        // double-count every packet. Fall back to non-filter non-loopback interfaces when a
        // VM/VPN-only host has no hardware NIC marked `HardwareInterface`.
        let mut hw_rx = 0u64;
        let mut hw_tx = 0u64;
        let mut hw_best_traffic = 0u64;
        let mut hw_best_name = String::new();
        let mut hw_count = 0usize;

        let mut fb_rx = 0u64;
        let mut fb_tx = 0u64;
        let mut fb_best_traffic = 0u64;
        let mut fb_best_name = String::new();

        for r in rows {
            if r.OperStatus != IfOperStatusUp || r.Type == 24 || r.Type == 131 {
                continue;
            }
            let flags = r.InterfaceAndOperStatusFlags._bitfield;
            let is_hardware = (flags & 0x01) != 0;
            let is_filter = (flags & 0x02) != 0;
            if is_filter {
                continue;
            }
            let total = r.InOctets.saturating_add(r.OutOctets);
            if is_hardware {
                hw_count += 1;
                hw_rx = hw_rx.saturating_add(r.InOctets);
                hw_tx = hw_tx.saturating_add(r.OutOctets);
                if total >= hw_best_traffic {
                    hw_best_traffic = total;
                    let alias = alias_of(r);
                    if !alias.is_empty() {
                        hw_best_name = alias;
                    }
                }
            } else {
                fb_rx = fb_rx.saturating_add(r.InOctets);
                fb_tx = fb_tx.saturating_add(r.OutOctets);
                if total >= fb_best_traffic {
                    fb_best_traffic = total;
                    let alias = alias_of(r);
                    if !alias.is_empty() {
                        fb_best_name = alias;
                    }
                }
            }
        }
        FreeMibTable(table.cast());
        if hw_count > 0 {
            (hw_rx, hw_tx, hw_best_name)
        } else {
            (fb_rx, fb_tx, fb_best_name)
        }
    }
}

pub fn format_speed(bps: u64) -> String {
    if bps >= 1_048_576 {
        format!("{:.1} MB/s", bps as f64 / 1_048_576.0)
    } else if bps >= 1024 {
        format!("{} KB/s", bps / 1024)
    } else {
        format!("{bps} B/s")
    }
}

pub fn format_bytes(bytes: u64) -> String {
    const GB: f64 = 1_073_741_824.0;
    const MB: f64 = 1_048_576.0;
    const KB: f64 = 1024.0;
    let b = bytes as f64;
    if b >= GB {
        format!("{:.2} GB", b / GB)
    } else if b >= MB {
        format!("{:.1} MB", b / MB)
    } else if b >= KB {
        format!("{:.0} KB", b / KB)
    } else {
        format!("{bytes} B")
    }
}

// --- System & Bluetooth Battery ---

pub struct BatteryInfo {
    pub has_battery: bool,
    pub percent: u32,
    pub on_ac: bool,
    pub charging: bool,
    pub life_secs: Option<u32>,
}

pub fn sample_battery() -> BatteryInfo {
    let mut st: SYSTEM_POWER_STATUS = unsafe { std::mem::zeroed() };
    if unsafe { GetSystemPowerStatus(&mut st) } == 0 {
        return BatteryInfo {
            has_battery: false,
            percent: 100,
            on_ac: true,
            charging: false,
            life_secs: None,
        };
    }
    let has_battery = (st.BatteryFlag & 128) == 0 && st.BatteryLifePercent <= 100;
    BatteryInfo {
        has_battery,
        percent: if has_battery {
            u32::from(st.BatteryLifePercent)
        } else {
            100
        },
        on_ac: st.ACLineStatus == 1,
        charging: (st.BatteryFlag & 8) != 0,
        life_secs: (st.BatteryLifeTime != u32::MAX).then_some(st.BatteryLifeTime),
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum BluetoothDeviceKind {
    Generic,
    Headphones,
    Mouse,
    Keyboard,
    Speaker,
    Watch,
    Phone,
    Gamepad,
}

impl BluetoothDeviceKind {
    /// Native Windows 11 `Segoe Fluent Icons` glyph matching each Bluetooth device category.
    pub const fn fluent_glyph(self) -> &'static str {
        match self {
            Self::Headphones => "\u{E7F6}",
            Self::Mouse => "\u{E962}",
            Self::Keyboard => "\u{E765}",
            Self::Speaker => "\u{E7F5}",
            Self::Watch => "\u{E916}",
            Self::Phone => "\u{E8EA}",
            Self::Gamepad => "\u{E7FC}",
            Self::Generic => "\u{E702}",
        }
    }
}

fn clean_bluetooth_name(raw: &str) -> String {
    let text = raw.trim();
    for suffix in [
        " Hands-Free AG",
        " Headset",
        " Audio Gateway",
        " Stereo",
        " A2DP",
    ] {
        if let Some(idx) = text.to_ascii_lowercase().find(&suffix.to_ascii_lowercase())
            && idx > 0
        {
            return text[..idx].trim().to_owned();
        }
    }
    text.to_owned()
}

pub fn classify_bluetooth_device(name: &str, instance_id: &str) -> BluetoothDeviceKind {
    let lower = name.to_lowercase();
    let has = |words: &[&str]| words.iter().any(|w| lower.contains(w));
    if has(&[
        "hands-free",
        "headset",
        "headphone",
        "earbud",
        "airpods",
        "buds",
        "stereo",
        "audio gateway",
        "kulaklık",
        "kulaklik",
    ]) {
        return BluetoothDeviceKind::Headphones;
    }
    if has(&["mouse", "trackball", "fare"]) {
        return BluetoothDeviceKind::Mouse;
    }
    if has(&["keyboard", "keypad", "klavye"]) {
        return BluetoothDeviceKind::Keyboard;
    }
    if has(&[
        "gamepad",
        "controller",
        "joystick",
        "xbox",
        "dualsense",
        "dualshock",
    ]) {
        return BluetoothDeviceKind::Gamepad;
    }
    if has(&["speaker", "soundbar", "hoparlör", "hoparlor"]) {
        return BluetoothDeviceKind::Speaker;
    }
    if has(&["watch", "band"]) {
        return BluetoothDeviceKind::Watch;
    }
    if has(&["phone", "iphone", "galaxy", "pixel"]) {
        return BluetoothDeviceKind::Phone;
    }
    let id_lower = instance_id.to_ascii_lowercase();
    if ["0000111e", "0000110b", "0000110d"]
        .iter()
        .any(|u| id_lower.contains(u))
    {
        return BluetoothDeviceKind::Headphones;
    }
    BluetoothDeviceKind::Generic
}

/// One wireless battery row. Every field describing the *same* sample lives here, so the
/// level, spare battery, sample time, staleness and link state can never be torn apart by a
/// concurrent refresh (they are replaced together under the cache lock).
#[derive(Clone, Debug)]
pub struct BluetoothBatteryDevice {
    /// Stable physical identity (Bluetooth address / container / VID:PID:serial[:slot]).
    /// Selection, rename aliases and ignore actions bind to this, never to the name or the
    /// row index.
    pub id: String,
    pub name: String,
    pub percent: u32,
    pub kind: BluetoothDeviceKind,
    /// `WirelessDeviceReading.GetSourceLabel()` equivalent (HID++, BLE GATT, XInput, ...).
    pub source_label: String,
    pub charging: bool,
    /// Last-known value (device disconnected / not re-verified this poll).
    pub stale: bool,
    /// Verified live link at sample time.
    pub connected: bool,
    pub sampled_at: Option<Instant>,
    /// Spare / dock battery (Arctis Nova Pro, Arctis Pro) of the same sample.
    pub secondary_level: Option<u8>,
}

impl BluetoothBatteryDevice {
    fn new(id: String, name: String, percent: u32, kind: BluetoothDeviceKind) -> Self {
        Self {
            id,
            name,
            percent,
            kind,
            source_label: "Bluetooth".to_owned(),
            charging: false,
            stale: false,
            connected: true,
            sampled_at: Some(Instant::now()),
            secondary_level: None,
        }
    }

    /// `true` when the row is a verified live reading (low-battery alerts only fire for these).
    pub fn is_live(&self) -> bool {
        self.connected && !self.stale
    }

    /// Whether this row matches an `IgnoredWirelessDevices` entry (stable id first; plain
    /// names are still honoured for lists written by older builds).
    pub fn matches_ignore_entry(&self, entry: &str) -> bool {
        let entry = entry.trim();
        if entry.is_empty() {
            return false;
        }
        if entry.eq_ignore_ascii_case(&self.id) || entry.eq_ignore_ascii_case(&self.name) {
            return true;
        }
        // Match canonical `bt:<MAC12>` against legacy `bt-pnp:...` entries carrying the same MAC.
        if let Some(entry_bt) = canonical_bt_id(entry)
            && let Some(self_bt) = canonical_bt_id(&self.id)
        {
            return entry_bt.eq_ignore_ascii_case(&self_bt);
        }
        false
    }
}

/// Extracts canonical `bt:<MAC12_UPPER>` from either an existing `bt:<MAC12>` id or a PnP
/// instance id containing a 48-bit Bluetooth MAC (`DEV_<12 hex>` or `...&<12 hex>_...`).
fn canonical_bt_id(raw: &str) -> Option<String> {
    let upper = raw.trim().to_ascii_uppercase();
    if let Some(rest) = upper.strip_prefix("BT:") {
        let mac = rest.split('#').next().unwrap_or(rest);
        if mac.len() == 12 && mac.bytes().all(|b| b.is_ascii_hexdigit()) {
            return Some(format!("bt:{mac}"));
        }
        return None;
    }
    let instance = upper.strip_prefix("BT-PNP:").unwrap_or(&upper);
    crate::platform::wireless_battery::bt_id_from_instance(instance)
}

#[derive(Clone, Default)]
pub struct BluetoothBatteryCache {
    pub devices: Vec<BluetoothBatteryDevice>,
    pub last_fetch: Option<Instant>,
    pub loading: bool,
    /// Generation of the refresh whose result `devices` holds. A late, older refresh (e.g. a
    /// slow PowerShell fallback) can never replace a newer snapshot.
    pub generation: u64,
}

/// Applies a refresh result tagged with `generation`. Returns `false` (and leaves the cache
/// untouched) when a newer generation has already been applied.
pub fn apply_bluetooth_snapshot(
    cache: &mut BluetoothBatteryCache,
    generation: u64,
    devices: Vec<BluetoothBatteryDevice>,
) -> bool {
    if generation < cache.generation {
        return false;
    }
    cache.devices = devices;
    cache.generation = generation;
    cache.last_fetch = Some(Instant::now());
    true
}

/// Marks cached rows stale on a refresh error unless a newer generation has already been
/// applied. Returns `true` when the error applied to `cache`.
pub fn apply_bluetooth_error(cache: &mut BluetoothBatteryCache, generation: u64) -> bool {
    if generation < cache.generation {
        return false;
    }
    for device in &mut cache.devices {
        device.stale = true;
        device.connected = false;
        device.charging = false;
    }
    true
}

/// Synchronous PnP Bluetooth battery query (`{104EA319-6EE2-4701-BD47-8DDBF425BBE5} 2`)
/// used as the fallback when the native providers report nothing. Runs on the caller's
/// (worker) thread — no extra thread. Only devices whose physical link is verified
/// (`DEVPKEY_Device_IsConnected` = `{83DA6326-97A6-4088-9453-A1923F573B29} 15`) are live;
/// rows are keyed by PnP instance id, never merged by name.
///
/// `Err` means the query itself failed (PowerShell missing / non-zero exit), which callers
/// must not confuse with `Ok(vec![])` (no battery-reporting device).
pub fn query_bluetooth_battery_powershell() -> Result<Vec<BluetoothBatteryDevice>, String> {
    let ps = r#"Get-PnpDevice -Class Bluetooth -Status OK -ErrorAction SilentlyContinue | ForEach-Object { $p = Get-PnpDeviceProperty -InstanceId $_.InstanceId -KeyName '{104EA319-6EE2-4701-BD47-8DDBF425BBE5} 2' -ErrorAction SilentlyContinue; $c = Get-PnpDeviceProperty -InstanceId $_.InstanceId -KeyName '{83DA6326-97A6-4088-9453-A1923F573B29} 15' -ErrorAction SilentlyContinue; if ($p -and $null -ne $p.Data) { "$($_.FriendlyName)|$($_.InstanceId)|$($c.Data)|$($p.Data)" } }"#;
    let out = Command::new("powershell.exe")
        .args(["-NoProfile", "-NonInteractive", "-Command", ps])
        .creation_flags(CREATE_NO_WINDOW)
        .output()
        .map_err(|e| e.to_string())?;
    if !out.status.success() {
        return Err(format!("powershell exited with {}", out.status));
    }
    Ok(parse_bluetooth_powershell_lines(&String::from_utf8_lossy(
        &out.stdout,
    )))
}

/// Parses `FriendlyName|InstanceId|IsConnected|Battery` lines. Unverified links are skipped
/// (a paired, switched-off headset keeps its last PnP battery value).
pub fn parse_bluetooth_powershell_lines(text: &str) -> Vec<BluetoothBatteryDevice> {
    let mut found: Vec<BluetoothBatteryDevice> = Vec::new();
    for line in text.lines() {
        let parts: Vec<&str> = line.trim().rsplitn(4, '|').collect();
        if parts.len() != 4 {
            continue;
        }
        let (level, connected, instance_id, raw_name) = (
            parts[0].trim(),
            parts[1].trim(),
            parts[2].trim(),
            parts[3].trim(),
        );
        let Ok(val) = level.parse::<u32>() else {
            continue;
        };
        if val > 100 || raw_name.is_empty() || instance_id.is_empty() {
            continue;
        }
        if !connected.eq_ignore_ascii_case("true") {
            continue;
        }
        let id = crate::platform::wireless_battery::bt_id_from_instance(instance_id)
            .unwrap_or_else(|| format!("bt-pnp:{}", instance_id.to_ascii_uppercase()));
        if found.iter().any(|d| d.id == id) {
            continue;
        }
        let kind = classify_bluetooth_device(raw_name, instance_id);
        found.push(BluetoothBatteryDevice::new(
            id,
            clean_bluetooth_name(raw_name),
            val,
            kind,
        ));
    }
    found
}

/// Releases the cache's `loading` flag on every exit path (including panics).
struct LoadingGuard<'a>(&'a Arc<Mutex<BluetoothBatteryCache>>);

impl Drop for LoadingGuard<'_> {
    fn drop(&mut self) {
        if let Ok(mut g) = self.0.lock() {
            g.loading = false;
        }
    }
}

/// Non-blocking PnP Bluetooth battery refresh (kept for callers that have no native
/// provider). The result is applied with a generation guard and `loading` is always
/// released.
pub fn refresh_bluetooth_battery(cache: &Arc<Mutex<BluetoothBatteryCache>>, force: bool) {
    let generation = if let Ok(mut g) = cache.lock() {
        let stale = g.last_fetch.is_none_or(|t| t.elapsed().as_secs() >= 60);
        if !g.loading && (force || stale) {
            g.loading = true;
            Some(g.generation + 1)
        } else {
            None
        }
    } else {
        None
    };
    let Some(generation) = generation else {
        return;
    };
    let worker_cache = Arc::clone(cache);
    let spawned = std::thread::Builder::new()
        .name("opentaskbar-bt-bat".into())
        .spawn(move || {
            let _loading = LoadingGuard(&worker_cache);
            match query_bluetooth_battery_powershell() {
                Ok(found) => {
                    if let Ok(mut g) = worker_cache.lock() {
                        apply_bluetooth_snapshot(&mut g, generation, found);
                    }
                }
                Err(_) => {
                    // Keep the previous rows but stop presenting them as live, unless a newer
                    // refresh generation has already been applied.
                    if let Ok(mut g) = worker_cache.lock() {
                        apply_bluetooth_error(&mut g, generation);
                    }
                }
            }
        })
        .is_ok();
    if !spawned && let Ok(mut g) = cache.lock() {
        g.loading = false;
    }
}

// --- Storage ---

#[derive(Clone, Debug)]
pub struct DriveSpace {
    pub letter: char,
    pub free_gb: f64,
    pub total_gb: f64,
}

/// Culture-aware fixed-point rendering (comma decimal separator for tr/de/fr/ru/... locales).
pub fn culture_number(value: f64, decimals: usize) -> String {
    let text = format!("{value:.decimals$}");
    let language = loc::language().to_ascii_lowercase();
    if language == "en-us" || language == "en-gb" {
        text
    } else {
        text.replace('.', ",")
    }
}

impl DriveSpace {
    /// Formats `"213 / 476 GB"` or `"1.2 / 1.8 TB"` matching `StorageWidget.FormatCapacityPair`.
    /// The reference TB cutoff is 972.8 GiB (= 0.95 TiB), not 972.0 GiB.
    pub fn format_capacity_pair(&self) -> String {
        if self.total_gb >= 972.8 {
            format!(
                "{} / {} TB",
                culture_number(self.free_gb / 1024.0, 1),
                culture_number(self.total_gb / 1024.0, 1)
            )
        } else {
            format!(
                "{} / {} GB",
                culture_number(self.free_gb, 0),
                culture_number(self.total_gb, 0)
            )
        }
    }
}

pub fn sample_drives() -> Vec<DriveSpace> {
    let mask = unsafe { GetLogicalDrives() };
    (0..26u32)
        .filter(|i| (mask & (1 << i)) != 0)
        .filter_map(|i| {
            let letter = (b'A' + i as u8) as char;
            let root: Vec<u16> = format!("{letter}:\\\0").encode_utf16().collect();
            let (mut avail, mut total, mut free) = (0u64, 0u64, 0u64);
            let ok =
                unsafe { GetDiskFreeSpaceExW(root.as_ptr(), &mut avail, &mut total, &mut free) };
            (ok != 0 && total > 0).then(|| DriveSpace {
                letter,
                free_gb: avail as f64 / 1_073_741_824.0,
                total_gb: total as f64 / 1_073_741_824.0,
            })
        })
        .collect()
}

// --- Keyboard Layout ---

fn hkl_iso_code(hkl: isize) -> String {
    let lcid = (hkl as u32) & 0xffff;
    let mut buf = [0u16; 16];
    let len = unsafe {
        GetLocaleInfoW(
            lcid,
            LOCALE_SISO639LANGNAME,
            buf.as_mut_ptr(),
            buf.len() as i32,
        )
    };
    if len > 1 {
        String::from_utf16_lossy(&buf[..(len - 1) as usize]).to_uppercase()
    } else {
        format!("{lcid:04X}")
    }
}

fn hkl_display_name(hkl: isize) -> String {
    let lcid = (hkl as u32) & 0xffff;
    let mut buf = [0u16; 96];
    let len = unsafe {
        GetLocaleInfoW(
            lcid,
            LOCALE_SLOCALIZEDDISPLAYNAME,
            buf.as_mut_ptr(),
            buf.len() as i32,
        )
    };
    if len > 1 {
        String::from_utf16_lossy(&buf[..(len - 1) as usize])
    } else {
        hkl_iso_code(hkl)
    }
}

pub fn active_keyboard_hkl(target_hwnd: Option<usize>) -> isize {
    unsafe {
        let hwnd = target_hwnd
            .filter(|&h| h != 0)
            .map(|h| h as _)
            .unwrap_or_else(|| GetForegroundWindow());
        let tid = if !hwnd.is_null() {
            GetWindowThreadProcessId(hwnd, std::ptr::null_mut())
        } else {
            0
        };
        GetKeyboardLayout(tid) as isize
    }
}

pub fn active_keyboard_code() -> String {
    hkl_iso_code(active_keyboard_hkl(None))
}

/// Returns `(hkl, short_code, localized_display_name)` matching `KeyboardLayoutWidget`.
pub fn installed_keyboard_layouts() -> Vec<(isize, String, String)> {
    let mut list = [std::ptr::null_mut(); 16];
    let count = unsafe { GetKeyboardLayoutList(list.len() as i32, list.as_mut_ptr()) };
    if count <= 0 {
        return Vec::new();
    }
    list[..count as usize]
        .iter()
        .map(|&h| {
            let hkl = h as isize;
            (hkl, hkl_iso_code(hkl), hkl_display_name(hkl))
        })
        .collect()
}

/// Posts `WM_INPUTLANGCHANGEREQUEST` to one target window only. Deliberately does **not** call
/// `ActivateKeyboardLayout`: that would change the layout of the whole calling process (our dock)
/// instead of the external foreground app the user is typing in.
pub fn activate_keyboard_layout_for(hwnd: usize, hkl: isize) {
    if hwnd != 0 {
        unsafe {
            let _ = PostMessageW(hwnd as _, WM_INPUTLANGCHANGEREQUEST, 0, hkl);
        }
    }
}

pub fn activate_keyboard_layout(hkl: isize) {
    unsafe {
        let fg = GetForegroundWindow();
        if !fg.is_null() {
            activate_keyboard_layout_for(fg as usize, hkl);
        }
    }
}

pub fn cycle_keyboard_layout(forward: bool, target_hwnd: Option<usize>) -> Option<String> {
    let layouts = installed_keyboard_layouts();
    if layouts.len() < 2 {
        return None;
    }
    let active_hkl = active_keyboard_hkl(target_hwnd);
    let idx = layouts
        .iter()
        .position(|(hkl, _, _)| *hkl == active_hkl)
        .or_else(|| {
            let code = hkl_iso_code(active_hkl);
            layouts.iter().position(|(_, c, _)| c == &code)
        })
        .unwrap_or(0);
    let next = if forward {
        (idx + 1) % layouts.len()
    } else {
        (idx + layouts.len() - 1) % layouts.len()
    };
    let (hkl, ref code, _) = layouts[next];
    match target_hwnd {
        Some(hwnd) => activate_keyboard_layout_for(hwnd, hkl),
        None => activate_keyboard_layout(hkl),
    }
    Some(code.clone())
}

// --- Day Progress ---

/// Pure helper computing `(day, week, year, remaining_hours)` for exact calendar dates
/// including leap years and culture `first_day_of_week`.
pub fn day_progress_for(
    year: i32,
    month: u32,
    day: u32,
    day_of_week: u32,
    hour: u32,
    minute: u32,
    first_dow: u32,
) -> (f32, f32, f32, u32) {
    let mins = hour.min(23) * 60 + minute.min(59);
    let day_frac = (mins as f32 / 1440.0).clamp(0.0, 1.0);
    let dow_offset = (day_of_week % 7 + 7 - first_dow % 7) % 7;
    let week = ((dow_offset as f32 + day_frac) / 7.0).clamp(0.0, 1.0);
    let is_leap = (year % 4 == 0 && year % 100 != 0) || (year % 400 == 0);
    let days_in_year = if is_leap { 366.0_f32 } else { 365.0_f32 };
    let prior_days: u32 = (1..month.clamp(1, 12))
        .map(|m| days_in_month(year, m))
        .sum();
    let day_idx = prior_days.saturating_add(day.saturating_sub(1));
    let year_frac = ((day_idx as f32 + day_frac) / days_in_year).clamp(0.0, 1.0);
    (
        day_frac,
        week,
        year_frac,
        (1440u32.saturating_sub(mins)) / 60,
    )
}

pub fn day_progress_metrics() -> (f32, f32, f32, u32) {
    let mut st: SYSTEMTIME = unsafe { std::mem::zeroed() };
    unsafe {
        GetLocalTime(&mut st);
    }
    day_progress_for(
        i32::from(st.wYear),
        u32::from(st.wMonth),
        u32::from(st.wDay),
        u32::from(st.wDayOfWeek),
        u32::from(st.wHour),
        u32::from(st.wMinute),
        first_day_of_week(),
    )
}

// --- In-Process WinHTTP HTTPS Helper (delegates to `platform::http`) ---

#[cfg(test)]
use crate::platform::http::parse_retry_after as parse_retry_after_header;

/// Whether a response status warrants a retry. The mere *presence* of `Retry-After` is not a
/// reason to retry a successful (2xx/3xx/4xx) response — it only sets the delay for 429/5xx.
fn is_retryable_status(status: u32) -> bool {
    status == 429 || (500..=599).contains(&status)
}

struct HttpCfg<'a> {
    method: &'a str,
    host: &'a str,
    path: &'a str,
    headers: &'a [(&'a str, &'a str)],
    body: Option<&'a str>,
    timeouts: (i32, i32, i32, i32),
    /// Only AI full-detail requests need `Retry-After`; weather calls skip the extra query.
    retry_after: bool,
}

struct HttpReply {
    status: u32,
    body: String,
    retry_after: Option<u32>,
}

/// One HTTPS request pipeline shared by `winhttp_get` and `winhttp_json`.
fn winhttp_request(cfg: &HttpCfg<'_>) -> Option<HttpReply> {
    let mut http_cfg = crate::platform::http::HttpCfg::new(HTTP_USER_AGENT);
    http_cfg.port = HTTPS_PORT;
    http_cfg.timeouts = [
        cfg.timeouts.0 as u32,
        cfg.timeouts.1 as u32,
        cfg.timeouts.2 as u32,
        cfg.timeouts.3 as u32,
    ];
    http_cfg.headers = cfg
        .headers
        .iter()
        .map(|(k, v)| ((*k).to_owned(), (*v).to_owned()))
        .collect();
    if let Some(body) = cfg.body {
        http_cfg.body = body.as_bytes().to_vec();
    }
    http_cfg.max_response_bytes = Some(HTTP_MAX_BODY_BYTES);
    let mut raw = Vec::new();
    let (status, retry_after) = crate::platform::http::request_with_meta(
        &http_cfg,
        cfg.host,
        cfg.path,
        cfg.method,
        cfg.retry_after,
        |chunk| {
            raw.extend_from_slice(chunk);
            true
        },
    )?;
    Some(HttpReply {
        status,
        body: String::from_utf8_lossy(&raw).into_owned(),
        retry_after,
    })
}

/// Fast GET (weather / CLI snapshots). No `Retry-After` query, 4/4/5/6 s timeouts.
fn winhttp_get(host: &str, path: &str, extra_headers: &[(&str, &str)]) -> Option<(u32, String)> {
    winhttp_request(&HttpCfg {
        method: "GET",
        host,
        path,
        headers: extra_headers,
        body: None,
        timeouts: HTTP_TIMEOUTS_FAST,
        retry_after: false,
    })
    .map(|reply| (reply.status, reply.body))
}

/// Arbitrary method with optional UTF-8 body and `Retry-After` (AI full-detail requests).
fn winhttp_json(
    method: &str,
    host: &str,
    path: &str,
    headers: &[(&str, &str)],
    body: Option<&str>,
) -> Option<(u32, String, Option<u32>)> {
    winhttp_request(&HttpCfg {
        method,
        host,
        path,
        headers,
        body,
        timeouts: HTTP_TIMEOUTS_AI,
        retry_after: true,
    })
    .map(|reply| (reply.status, reply.body, reply.retry_after))
}

// --- Windows Night Light Control (exact `NightLightControl.cs`) ---

const NIGHT_LIGHT_REG_PATH: &str = r"Software\Microsoft\Windows\CurrentVersion\CloudStore\Store\DefaultAccount\Current\default$windows.data.bluelightreduction.bluelightreductionstate\windows.data.bluelightreduction.bluelightreductionstate";

const NL_ENVELOPE_PREFIX: [u8; 10] = [0x43, 0x42, 1, 0, 0x0A, 2, 1, 0, 0x2A, 6];
const NL_PAYLOAD_PREFIX: [u8; 3] = [0x2A, 0x2B, 0x0E];
const NL_BOND_HEADER: [u8; 4] = [0x43, 0x42, 1, 0];
const NL_EMPTY_ENVELOPE: [u8; 13] = [0x43, 0x42, 1, 0, 0x0A, 2, 1, 0, 0x2A, 0x2A, 0, 0, 0];
const NL_STORE_HEADER_SIZE: usize = 16;

#[derive(Clone, Copy, Debug)]
struct NightLightState {
    on: bool,
    usable: bool,
    modified: u64,
    explicit_usable: bool,
    header_size: usize,
}

/// Reads one CompactBinary CloudStore record exactly like `NightLightControl.TryRead`,
/// including the optional `ExplicitUsable` flag.
fn decode_night_light_state(data: &[u8]) -> Option<NightLightState> {
    let header_size = if data.len() >= NL_STORE_HEADER_SIZE && data.starts_with(&[2, 0, 0, 0]) {
        NL_STORE_HEADER_SIZE
    } else {
        0
    };
    let mut envelope = &data[header_size..];
    if envelope == NL_EMPTY_ENVELOPE {
        return Some(NightLightState {
            on: false,
            usable: true,
            modified: 0,
            explicit_usable: false,
            header_size,
        });
    }
    envelope = envelope.strip_prefix(&NL_ENVELOPE_PREFIX)?;
    let (modified, rest) = read_varint(envelope)?;
    if modified > i64::MAX as u64 - 2 {
        return None;
    }
    let payload_section = rest.strip_prefix(&NL_PAYLOAD_PREFIX)?;
    let (length, body_with_tail) = read_varint(payload_section)?;
    if body_with_tail.len() < 3 || length != (body_with_tail.len() - 3) as u64 {
        return None;
    }
    let (payload, tail) = body_with_tail.split_at(body_with_tail.len() - 3);
    if tail != [0, 0, 0] {
        return None;
    }
    let mut rest = payload.strip_prefix(&NL_BOND_HEADER)?;
    let on = rest.starts_with(&[0x10, 0]);
    if on {
        rest = &rest[2..];
    }
    if rest.starts_with(&[0xD0, 0x0A]) {
        let (_, next) = read_varint(&rest[2..])?;
        rest = next;
    }
    if rest.starts_with(&[0xC6, 0x14]) {
        let (_, next) = read_varint(&rest[2..])?;
        rest = next;
    }
    let explicit_usable = rest.starts_with(&[0xC2, 0x1E]);
    let mut usable = true;
    if explicit_usable {
        rest = &rest[2..];
        usable = match rest.first() {
            Some(0) => false,
            Some(1) => true,
            _ => return None,
        };
        rest = &rest[1..];
    }
    if rest != [0] {
        return None;
    }
    Some(NightLightState {
        on,
        usable,
        modified,
        explicit_usable,
        header_size,
    })
}

fn read_varint(data: &[u8]) -> Option<(u64, &[u8])> {
    let mut value = 0u64;
    for (i, &byte) in data.iter().take(10).enumerate() {
        value |= u64::from(byte & 0x7f) << (i * 7);
        if byte & 0x80 == 0 {
            return Some((value, &data[i + 1..]));
        }
    }
    None
}

fn decode_night_light_on(data: &[u8]) -> Option<bool> {
    decode_night_light_state(data)
        .filter(|state| state.usable)
        .map(|state| state.on)
}

pub fn read_night_light() -> Option<bool> {
    let subkey: Vec<u16> = NIGHT_LIGHT_REG_PATH.encode_utf16().chain(Some(0)).collect();
    let val_name: Vec<u16> = "Data\0".encode_utf16().collect();
    unsafe {
        let mut hkey = std::ptr::null_mut();
        if RegOpenKeyExW(HKEY_CURRENT_USER, subkey.as_ptr(), 0, KEY_READ, &mut hkey) != 0 {
            return None;
        }
        let mut ty: u32 = 0;
        let mut len: u32 = 0;
        let mut result = None;
        if RegQueryValueExW(
            hkey,
            val_name.as_ptr(),
            std::ptr::null_mut(),
            &mut ty,
            std::ptr::null_mut(),
            &mut len,
        ) == 0
            && ty == REG_BINARY
            && len > 0
            && len <= 4096
        {
            let mut buf = vec![0u8; len as usize];
            if RegQueryValueExW(
                hkey,
                val_name.as_ptr(),
                std::ptr::null_mut(),
                &mut ty,
                buf.as_mut_ptr(),
                &mut len,
            ) == 0
            {
                result = decode_night_light_on(&buf[..len as usize]);
            }
        }
        let _ = RegCloseKey(hkey);
        result
    }
}

pub fn toggle_night_light() -> bool {
    let Some(cur) = read_night_light() else {
        return false;
    };
    let target_on = !cur;
    let subkey: Vec<u16> = NIGHT_LIGHT_REG_PATH.encode_utf16().chain(Some(0)).collect();
    let val_name: Vec<u16> = "Data\0".encode_utf16().collect();
    unsafe {
        let mut hkey = std::ptr::null_mut();
        if RegOpenKeyExW(
            HKEY_CURRENT_USER,
            subkey.as_ptr(),
            0,
            KEY_READ | KEY_SET_VALUE,
            &mut hkey,
        ) != 0
        {
            return false;
        }
        let mut ty: u32 = 0;
        let mut len: u32 = 0;
        let mut ok = false;
        if RegQueryValueExW(
            hkey,
            val_name.as_ptr(),
            std::ptr::null_mut(),
            &mut ty,
            std::ptr::null_mut(),
            &mut len,
        ) == 0
            && ty == REG_BINARY
            && (20..=4096).contains(&len)
        {
            let mut buf = vec![0u8; len as usize];
            if RegQueryValueExW(
                hkey,
                val_name.as_ptr(),
                std::ptr::null_mut(),
                &mut ty,
                buf.as_mut_ptr(),
                &mut len,
            ) == 0
            {
                buf.truncate(len as usize);
                if let Some(encoded) = encode_night_light_toggle(&buf, target_on) {
                    ok = RegSetValueExW(
                        hkey,
                        val_name.as_ptr(),
                        0,
                        REG_BINARY,
                        encoded.as_ptr(),
                        encoded.len() as u32,
                    ) == 0;
                }
            }
        }
        let _ = RegCloseKey(hkey);
        ok
    }
}

fn write_varint(out: &mut Vec<u8>, mut val: u64) {
    while val >= 0x80 {
        out.push(((val as u8) & 0x7f) | 0x80);
        val >>= 7;
    }
    out.push(val as u8);
}

fn encode_night_light_toggle(orig: &[u8], on: bool) -> Option<Vec<u8>> {
    let now_secs = SystemTime::now().duration_since(UNIX_EPOCH).ok()?.as_secs();
    let state = decode_night_light_state(orig);
    let header_size = state.map_or_else(
        || {
            if orig.len() >= NL_STORE_HEADER_SIZE && orig.starts_with(&[2, 0, 0, 0]) {
                NL_STORE_HEADER_SIZE
            } else {
                0
            }
        },
        |s| s.header_size,
    );
    // Monotonic: CloudStore ignores a timestamp that is not newer than the last write.
    let modified = now_secs.max(state.map_or(0, |s| s.modified.saturating_add(2)));
    let filetime = (modified + 11_644_473_600) * 10_000_000;
    let mut payload = Vec::with_capacity(32);
    payload.extend_from_slice(&NL_BOND_HEADER);
    if on {
        payload.extend_from_slice(&[0x10, 0]);
    }
    payload.extend_from_slice(&[0xD0, 0x0A, 2, 0xC6, 0x14]);
    write_varint(&mut payload, filetime);
    if state.is_some_and(|s| s.explicit_usable) {
        payload.extend_from_slice(&[0xC2, 0x1E, 1]);
    }
    payload.push(0);

    let mut out = Vec::with_capacity(orig.len() + 16);
    if header_size > 0 {
        out.extend_from_slice(&orig[..header_size]);
    }
    out.extend_from_slice(&NL_ENVELOPE_PREFIX);
    write_varint(&mut out, modified);
    out.extend_from_slice(&NL_PAYLOAD_PREFIX);
    write_varint(&mut out, payload.len() as u64);
    out.extend_from_slice(&payload);
    out.extend_from_slice(&[0, 0, 0]);
    Some(out)
}

#[cfg(test)]
mod night_light_tests {
    use super::{decode_night_light_on, encode_night_light_toggle};

    #[test]
    fn night_light_encode_decode_round_trip() {
        for on in [true, false] {
            let encoded = encode_night_light_toggle(&[], on).expect("payload must encode");
            assert_eq!(
                decode_night_light_on(&encoded),
                Some(on),
                "round-trip must preserve the Night Light state"
            );

            // The registry blob normally carries the 16-byte CloudStore header.
            let mut with_header = vec![2u8, 0, 0, 0];
            with_header.extend_from_slice(&[0u8; 12]);
            with_header.extend_from_slice(&encoded);
            assert_eq!(decode_night_light_on(&with_header), Some(on));
        }

        let off = encode_night_light_toggle(&[], false).unwrap();
        let on = encode_night_light_toggle(&[], true).unwrap();
        assert_ne!(off, on, "toggle must actually rewrite the bond flags");
        assert_eq!(decode_night_light_on(&on), Some(true));
        assert_eq!(decode_night_light_on(&off), Some(false));

        // reference writes an empty-envelope sentinel when Night Light was never enabled.
        const EMPTY_ENVELOPE: [u8; 13] = [0x43, 0x42, 1, 0, 0x0A, 2, 1, 0, 0x2A, 0x2A, 0, 0, 0];
        assert_eq!(decode_night_light_on(&EMPTY_ENVELOPE), Some(false));
        assert_eq!(decode_night_light_on(b"not a CloudStore blob"), None);
    }
}

// --- Weather (exact `WeatherService.cs` Open-Meteo API) ---

/// Five semantic weather categories shared by chip badges, flyout headers, and forecast rows.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum WeatherKind {
    #[default]
    Clear,
    Cloudy,
    Rain,
    Snow,
    Storm,
}

pub fn weather_kind_from_wmo(code: i64) -> WeatherKind {
    match code {
        0 => WeatherKind::Clear,
        1..=3 | 45 | 48 => WeatherKind::Cloudy,
        71..=77 | 85 | 86 => WeatherKind::Snow,
        95..=99 => WeatherKind::Storm,
        51..=82 => WeatherKind::Rain,
        _ => WeatherKind::Cloudy,
    }
}

pub fn weather_kind_from_condition(condition: &str, is_rain: bool) -> WeatherKind {
    let c = condition.to_lowercase();
    if c.contains("thunder") || c.contains("storm") || c.contains("fırtına") || c.contains("gök")
    {
        WeatherKind::Storm
    } else if c.contains("snow") || c.contains("kar") || c.contains("sleet") || c.contains("ice") {
        WeatherKind::Snow
    } else if c.contains("rain")
        || c.contains("shower")
        || c.contains("drizzle")
        || c.contains("yağmur")
        || c.contains("sağanak")
    {
        WeatherKind::Rain
    } else if c.contains("fog")
        || c.contains("mist")
        || c.contains("haze")
        || c.contains("sis")
        || c.contains("cloud")
        || c.contains("overcast")
        || c.contains("bulut")
    {
        WeatherKind::Cloudy
    } else if is_rain {
        WeatherKind::Rain
    } else {
        WeatherKind::Clear
    }
}

#[derive(Clone, Default)]
pub struct WeatherCache {
    /// Display name resolved by the API; kept separate from the user's requested city so a
    /// spelling difference does not force a refetch on every tick.
    pub requested_city: String,
    pub city: String,
    pub temp_c: String,
    pub temp_c_val: Option<f64>,
    pub condition: String,
    pub is_rain: bool,
    pub kind: WeatherKind,
    pub feels_like: String,
    pub feels_like_c_val: Option<f64>,
    pub humidity: String,
    pub wind: String,
    pub wind_kmh_val: Option<f64>,
    pub today_range: String,
    pub today_hi_c: Option<f64>,
    pub today_lo_c: Option<f64>,
    /// 3-day forecast `(day_label, condition, high_low)`.
    pub forecast_days: Vec<(String, String, String)>,
    /// Numeric forecast extremes paired with `forecast_days` for °C / °F re-formatting.
    pub forecast_hi_lo_c: Vec<(Option<f64>, Option<f64>)>,
    pub last_fetch: Option<Instant>,
    pub loading: bool,
}

pub fn format_temperature(
    celsius: f64,
    unit: opentaskbar_core::settings::TemperatureUnit,
) -> String {
    match unit {
        opentaskbar_core::settings::TemperatureUnit::Celsius => {
            format!("{}°C", celsius.round() as i32)
        }
        opentaskbar_core::settings::TemperatureUnit::Fahrenheit => {
            let f = celsius * 9.0 / 5.0 + 32.0;
            format!("{}°F", f.round() as i32)
        }
    }
}

impl WeatherCache {
    /// Wind speed in the active unit: mph for °F, km/h for °C (`WeatherUnit` parity).
    pub fn formatted_wind(&self, unit: opentaskbar_core::settings::TemperatureUnit) -> String {
        if let Some(kmh) = self.wind_kmh_val {
            match unit {
                opentaskbar_core::settings::TemperatureUnit::Celsius => {
                    format!("{} km/h", kmh.round() as i32)
                }
                opentaskbar_core::settings::TemperatureUnit::Fahrenheit => {
                    format!("{} mph", (kmh * 0.621_371).round() as i32)
                }
            }
        } else {
            self.wind.clone()
        }
    }
}

fn describe_wmo(code: i64) -> (String, bool) {
    let key = match code {
        0 => "WeatherClear",
        1 | 2 => "WeatherPartlyCloudy",
        3 => "WeatherCloudy",
        45 | 48 => "WeatherFog",
        71..=77 | 85 | 86 => "WeatherSnow",
        51..=82 => "WeatherRain",
        95..=99 => "WeatherStorm",
        _ => "WeatherCloudy",
    };
    let rain = matches!(code, 51..=99);
    (loc::t(key), rain)
}

fn urlencode_query(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for b in s.bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(b as char);
            }
            b' ' => out.push_str("%20"),
            other => out.push_str(&format!("%{other:02X}")),
        }
    }
    out
}

#[derive(Clone, Debug, PartialEq)]
pub struct GeoLocationMatch {
    pub name: String,
    pub admin1: String,
    pub country: String,
    pub country_code: String,
    pub latitude: f64,
    pub longitude: f64,
}

impl GeoLocationMatch {
    pub fn setting_value(&self) -> String {
        if self.country.is_empty() {
            self.name.clone()
        } else {
            format!("{}, {}", self.name, self.country)
        }
    }
}

/// Pure parser for BigDataCloud reverse-geocode payloads. City falls back through locality
/// and principal subdivision so a coordinate fix still yields a usable picker label.
pub fn parse_reverse_geocode(
    body: &str,
    latitude: f64,
    longitude: f64,
) -> Option<GeoLocationMatch> {
    let root: serde_json::Value = serde_json::from_str(body).ok()?;
    let text = |key: &str| {
        root.get(key)
            .and_then(|value| value.as_str())
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .map(str::to_owned)
    };
    let name = text("city")
        .or_else(|| text("locality"))
        .or_else(|| text("principalSubdivision"))?;
    Some(GeoLocationMatch {
        name,
        admin1: text("principalSubdivision").unwrap_or_default(),
        country: text("countryName").unwrap_or_default(),
        country_code: text("countryCode").unwrap_or_default(),
        latitude,
        longitude,
    })
}

/// Resolves Windows Geolocation coordinates into a city/country pair (no API key required).
/// Used by the settings location auto-fill when `CivicAddress` is empty, which is common on
/// desktop Windows.
pub fn reverse_geocode(latitude: f64, longitude: f64, language: &str) -> Option<GeoLocationMatch> {
    if !latitude.is_finite()
        || !longitude.is_finite()
        || latitude.abs() > 90.0
        || longitude.abs() > 180.0
    {
        return None;
    }
    let lang = language
        .split(['-', '_'])
        .next()
        .filter(|part| part.len() == 2)
        .unwrap_or("en");
    let path = format!(
        "/data/reverse-geocode-client?latitude={latitude:.5}&longitude={longitude:.5}&localityLanguage={lang}"
    );
    let (status, body) = winhttp_get("api.bigdatacloud.net", &path, &[])?;
    if status != 200 {
        return None;
    }
    parse_reverse_geocode(&body, latitude, longitude)
}

/// Splits `"City, Country"` (or `"City, Region, Country"`) into `(city_query, country_hint)`
/// because Open-Meteo's `/v1/search` returns empty results when a comma-qualified string is
/// passed verbatim in `name=`.
fn parse_weather_query(raw: &str) -> (&str, Option<&str>) {
    let trimmed = raw.trim();
    let Some((city, rest)) = trimmed.split_once(',') else {
        return (trimmed, None);
    };
    let city = city.trim();
    let country = rest
        .rsplit(',')
        .map(str::trim)
        .find(|part| !part.is_empty());
    if city.is_empty() {
        (trimmed, None)
    } else {
        (city, country)
    }
}

fn build_weather_geocode_path(raw_city: &str, lang_prefix: &str) -> (String, Option<String>) {
    let (city_query, country_hint) = parse_weather_query(raw_city);
    let iso_code = country_hint.and_then(|hint| {
        crate::platform::country_city::country_code_for(hint)
            .map(str::to_owned)
            .or_else(|| {
                (hint.len() == 2 && hint.chars().all(|c| c.is_ascii_alphabetic()))
                    .then(|| hint.to_ascii_uppercase())
            })
    });
    let count = if country_hint.is_some() && iso_code.is_none() {
        10
    } else {
        1
    };
    let mut path = format!(
        "/v1/search?count={count}&format=json&language={lang_prefix}&name={}",
        urlencode_query(city_query)
    );
    if let Some(code) = iso_code {
        path.push_str("&countryCode=");
        path.push_str(&code);
    }
    (path, country_hint.map(str::to_owned))
}

fn select_geocode_result(
    body: &str,
    fallback_name: &str,
    country_hint: Option<&str>,
) -> Option<(String, f64, f64)> {
    let v: serde_json::Value = serde_json::from_str(body).ok()?;
    let results = v.get("results")?.as_array()?;
    let chosen = country_hint
        .and_then(|hint| {
            let h = hint.trim().to_ascii_lowercase();
            results.iter().find(|entry| {
                entry
                    .get("country_code")
                    .and_then(|s| s.as_str())
                    .is_some_and(|cc| cc.eq_ignore_ascii_case(&h))
                    || entry
                        .get("country")
                        .and_then(|s| s.as_str())
                        .is_some_and(|c| c.to_ascii_lowercase().contains(&h))
                    || entry
                        .get("admin1")
                        .and_then(|s| s.as_str())
                        .is_some_and(|a| a.to_ascii_lowercase().contains(&h))
            })
        })
        .or_else(|| results.first())?;
    let name = chosen
        .get("name")
        .and_then(|s| s.as_str())
        .unwrap_or(fallback_name)
        .to_owned();
    let lat = chosen.get("latitude")?.as_f64()?;
    let lon = chosen.get("longitude")?.as_f64()?;
    Some((name, lat, lon))
}

pub fn refresh_weather(cache: &Arc<Mutex<WeatherCache>>, city: &str, force: bool) {
    let city = if city.trim().is_empty() {
        "Istanbul"
    } else {
        city.trim()
    };
    let should = if let Ok(mut g) = cache.lock() {
        let stale = g.last_fetch.is_none_or(|t| t.elapsed().as_secs() >= 1800);
        let changed = !g.requested_city.eq_ignore_ascii_case(city);
        if !g.loading && (force || stale || changed) {
            g.loading = true;
            if changed {
                // A different city must never render the previous city's cached numbers.
                g.city.clear();
                g.temp_c.clear();
                g.temp_c_val = None;
                g.feels_like.clear();
                g.feels_like_c_val = None;
                g.condition.clear();
                g.humidity.clear();
                g.wind.clear();
                g.wind_kmh_val = None;
                g.today_range.clear();
                g.today_hi_c = None;
                g.today_lo_c = None;
                g.forecast_days.clear();
                g.forecast_hi_lo_c.clear();
            }
            g.requested_city = city.to_owned();
            true
        } else {
            false
        }
    } else {
        false
    };
    if !should {
        return;
    }
    let cache = Arc::clone(cache);
    let city_owned = city.to_owned();
    let lang_prefix = loc::language()
        .split(['-', '_'])
        .next()
        .filter(|s| s.len() == 2)
        .unwrap_or("en")
        .to_ascii_lowercase();
    let _ = std::thread::Builder::new()
        .name("opentaskbar-weather".into())
        .spawn(move || {
            let (geo_path, country_hint) = build_weather_geocode_path(&city_owned, &lang_prefix);
            let geo = winhttp_get("geocoding-api.open-meteo.com", &geo_path, &[])
                .filter(|(st, _)| *st == 200)
                .and_then(|(_, body)| {
                    select_geocode_result(&body, &city_owned, country_hint.as_deref())
                });

            let parsed = geo.and_then(|(place_name, lat, lon)| {
                let fc_path = format!(
                    "/v1/forecast?latitude={lat:.3}&longitude={lon:.3}&current=temperature_2m,relative_humidity_2m,apparent_temperature,weather_code,wind_speed_10m&daily=weather_code,temperature_2m_max,temperature_2m_min&forecast_days=4&timezone=auto"
                );
                let (st, body) = winhttp_get("api.open-meteo.com", &fc_path, &[])?;
                if st != 200 {
                    return None;
                }
                let root: serde_json::Value = serde_json::from_str(&body).ok()?;
                let cur = root.get("current")?;
                let temp = cur.get("temperature_2m")?.as_f64()?;
                let feels = cur
                    .get("apparent_temperature")
                    .and_then(|v| v.as_f64())
                    .unwrap_or(temp);
                let hum = cur
                    .get("relative_humidity_2m")
                    .and_then(|v| v.as_f64())
                    .unwrap_or(0.0);
                let wind = cur
                    .get("wind_speed_10m")
                    .and_then(|v| v.as_f64())
                    .unwrap_or(0.0);
                let code = cur
                    .get("weather_code")
                    .and_then(|v| v.as_i64())
                    .unwrap_or(0);
                let (cond, is_rain) = describe_wmo(code);
                let kind = weather_kind_from_wmo(code);

                let daily = root.get("daily");
                let times = daily
                    .and_then(|d| d.get("time"))
                    .and_then(|v| v.as_array());
                let maxs = daily
                    .and_then(|d| d.get("temperature_2m_max"))
                    .and_then(|v| v.as_array());
                let mins = daily
                    .and_then(|d| d.get("temperature_2m_min"))
                    .and_then(|v| v.as_array());
                let codes = daily
                    .and_then(|d| d.get("weather_code"))
                    .and_then(|v| v.as_array());

                let today_hi_c = maxs.and_then(|a| a.first()).and_then(|v| v.as_f64());
                let today_lo_c = mins.and_then(|a| a.first()).and_then(|v| v.as_f64());
                let today_range = match (today_hi_c, today_lo_c) {
                    (Some(hi), Some(lo)) => format!("{}° / {}°", hi.round() as i32, lo.round() as i32),
                    _ => String::new(),
                };

                let mut forecast = Vec::new();
                let mut forecast_hi_lo = Vec::new();
                if let (Some(ts), Some(mxs), Some(mns), Some(cds)) = (times, maxs, mins, codes) {
                    let n = ts.len().min(mxs.len()).min(mns.len()).min(cds.len());
                    for i in 1..n.min(4) {
                        let date_str = ts[i].as_str().unwrap_or("");
                        let day_lbl = if i == 1 {
                            loc::t("WeatherTomorrow")
                        } else if let Some((y, rest)) = date_str.split_once('-')
                            && let Some((m, d)) = rest.split_once('-')
                            && let (Ok(yi), Ok(mi), Ok(di)) =
                                (y.parse::<i32>(), m.parse::<u32>(), d.parse::<u32>())
                        {
                            let wd = weekday_of(yi, mi, di);
                            weekday_short_name(wd)
                        } else {
                            date_str.to_owned()
                        };
                        let (dc, _) = describe_wmo(cds[i].as_i64().unwrap_or(0));
                        let hi = mxs[i].as_f64().unwrap_or(0.0).round() as i32;
                        let lo = mns[i].as_f64().unwrap_or(0.0).round() as i32;
                        forecast.push((day_lbl, dc, format!("{hi}° / {lo}°")));
                        forecast_hi_lo.push((mxs[i].as_f64(), mns[i].as_f64()));
                    }
                }

                Some((
                    place_name,
                    format!("{}°", temp.round() as i32),
                    temp,
                    cond,
                    is_rain,
                    kind,
                    format!("{}°", feels.round() as i32),
                    feels,
                    format!("{}%", hum.round() as i32),
                    format!("{} km/h", wind.round() as i32),
                    wind,
                    today_range,
                    today_hi_c,
                    today_lo_c,
                    forecast,
                    forecast_hi_lo,
                ))
            });

            if let Ok(mut g) = cache.lock() {
                g.loading = false;
                // A failed fetch must stay retryable and must not relabel stale data with a new city.
                if let Some((
                    place,
                    temp,
                    temp_val,
                    cond,
                    is_rain,
                    kind,
                    feels,
                    feels_val,
                    hum,
                    wind,
                    wind_val,
                    range,
                    hi_c,
                    lo_c,
                    fc,
                    fc_hi_lo,
                )) = parsed
                {
                    g.last_fetch = Some(Instant::now());
                    g.city = place;
                    g.temp_c = temp;
                    g.temp_c_val = Some(temp_val);
                    g.condition = cond;
                    g.is_rain = is_rain;
                    g.kind = kind;
                    g.feels_like = feels;
                    g.feels_like_c_val = Some(feels_val);
                    g.humidity = hum;
                    g.wind = wind;
                    g.wind_kmh_val = Some(wind_val);
                    g.today_range = range;
                    g.today_hi_c = hi_c;
                    g.today_lo_c = lo_c;
                    g.forecast_days = fc;
                    g.forecast_hi_lo_c = fc_hi_lo;
                }
            }
        });
}

// --- AI Provider Usage & Balance (exact `AiSecretStore.cs` DPAPI + `AiUsageService.cs`) ---

pub struct AiProviderMeta {
    pub id: &'static str,
    pub name: &'static str,
    pub short: &'static str,
    pub dashboard_url: &'static str,
    pub signin_hint: &'static str,
}

pub const AI_PROVIDERS: &[AiProviderMeta] = &[
    AiProviderMeta {
        id: "aiusage_claude",
        name: "Claude Code",
        short: "Claude",
        dashboard_url: "https://claude.ai/settings/usage",
        signin_hint: "claude auth login",
    },
    AiProviderMeta {
        id: "aiusage_codex",
        name: "Codex",
        short: "Codex",
        dashboard_url: "https://chatgpt.com/codex/settings/usage",
        signin_hint: "codex login",
    },
    AiProviderMeta {
        id: "aiusage_opencode",
        name: "OpenCode Go",
        short: "OpenCode",
        dashboard_url: "https://opencode.ai/go",
        signin_hint: "opencode auth login",
    },
    AiProviderMeta {
        id: "aiusage_copilot",
        name: "GitHub Copilot",
        short: "Copilot",
        dashboard_url: "https://github.com/settings/copilot",
        signin_hint: "gh auth login",
    },
    AiProviderMeta {
        id: "aiusage_deepseek",
        name: "DeepSeek",
        short: "DeepSeek",
        dashboard_url: "https://platform.deepseek.com/usage",
        signin_hint: "DEEPSEEK_API_KEY",
    },
    AiProviderMeta {
        id: "aiusage_openrouter",
        name: "OpenRouter",
        short: "OpenRouter",
        dashboard_url: "https://openrouter.ai/settings/credits",
        signin_hint: "OPENROUTER_API_KEY",
    },
    AiProviderMeta {
        id: "aiusage_cursor",
        name: "Cursor",
        short: "Cursor",
        dashboard_url: "https://cursor.com/dashboard?tab=usage",
        signin_hint: "Sign in to Cursor",
    },
];

pub fn ai_provider_meta(id: &str) -> Option<&'static AiProviderMeta> {
    AI_PROVIDERS.iter().find(|p| p.id.eq_ignore_ascii_case(id))
}

#[derive(Clone, Debug, Default)]
pub struct AiWindowData {
    pub label: String,
    pub used: f32,
    pub reset_text: String,
}

#[derive(Clone, Debug, Default)]
pub struct AiUsageSnapshot {
    pub balance_text: Option<String>,
    pub plan: Option<String>,
    pub windows: Vec<AiWindowData>,
    /// Remaining credit fraction in `[0.0, 1.0]` for credit-based providers (e.g. OpenRouter).
    pub credit_ratio: Option<f32>,
    pub last_fetch: Option<Instant>,
    pub loading: bool,
    pub has_cred: bool,
}

#[derive(Default)]
pub struct AiUsageCache {
    pub providers: HashMap<String, AiUsageSnapshot>,
}

/// `AiSecretStore.EntropyBytes` — must never change or old blobs become unreadable.
const AI_SECRET_ENTROPY: &[u8; 28] = b"OpenTaskbar.AiSecretStore.v1";

/// Reads an encrypted per-user DPAPI secret stored by `AiSecretStore.cs` (never logged).
fn read_dpapi_secret(secret_key: &str) -> Option<String> {
    let path = secret_file_path(secret_key)?;
    let mut enc = std::fs::read(&path).ok()?;
    if enc.is_empty() || enc.len() > 65_536 {
        return None;
    }
    let mut entropy = *AI_SECRET_ENTROPY;
    let in_blob = CRYPT_INTEGER_BLOB {
        cbData: enc.len() as u32,
        pbData: enc.as_mut_ptr(),
    };
    let ent_blob = CRYPT_INTEGER_BLOB {
        cbData: entropy.len() as u32,
        pbData: entropy.as_mut_ptr(),
    };
    let mut out_blob = CRYPT_INTEGER_BLOB {
        cbData: 0,
        pbData: std::ptr::null_mut(),
    };
    unsafe {
        if CryptUnprotectData(
            &in_blob,
            std::ptr::null_mut(),
            &ent_blob,
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            CRYPTPROTECT_UI_FORBIDDEN,
            &mut out_blob,
        ) == 0
            || out_blob.pbData.is_null()
            || out_blob.cbData == 0
        {
            return None;
        }
        let slice = std::slice::from_raw_parts(out_blob.pbData, out_blob.cbData as usize);
        let text = String::from_utf8_lossy(slice).trim().to_owned();
        // Zero decrypted memory before freeing.
        std::ptr::write_bytes(out_blob.pbData, 0, out_blob.cbData as usize);
        LocalFree(out_blob.pbData.cast());
        (!text.is_empty()).then_some(text)
    }
}

/// Same sanitizer as `AiSecretStore.SecretPath` (letters/digits/`-`/`_`, rest `_`).
fn sanitize_secret_key(secret_key: &str) -> Option<String> {
    let trimmed = secret_key.trim();
    if trimmed.is_empty() {
        return None;
    }
    Some(
        trimmed
            .chars()
            .map(|c| {
                if c.is_ascii_alphanumeric() || c == '-' || c == '_' {
                    c
                } else {
                    '_'
                }
            })
            .collect(),
    )
}

fn secret_file_path(secret_key: &str) -> Option<PathBuf> {
    let sanitized = sanitize_secret_key(secret_key)?;
    Some(opentaskbar_core::paths::secrets_dir().join(format!("{sanitized}.bin")))
}

/// `AiSecretStore.Set` writes `<key>.bin.<guid-N>.tmp` and moves it into place.
fn guid_tmp_path(path: &Path) -> PathBuf {
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let sequence = COUNTER.fetch_add(1, Ordering::Relaxed);
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos() as u64;
    let seed = nanos.rotate_left(19) ^ (sequence << 32) ^ u64::from(std::process::id());
    let words = [seed, seed.rotate_left(29) ^ nanos ^ sequence];
    let mut name = path.as_os_str().to_os_string();
    let hex: String = words.into_iter().flat_map(u64::to_le_bytes).fold(
        String::with_capacity(32),
        |mut out, byte| {
            use std::fmt::Write;
            let _ = write!(out, "{byte:02x}");
            out
        },
    );
    name.push(format!(".{hex}.tmp"));
    PathBuf::from(name)
}

/// Validates a sidecar's in-band secret request. Only `widget:<own-slug>:<key>` is accepted:
/// AI provider keys (`api-key:deepseek`, …) and other widgets' namespaces stay unreachable.
/// `slug` is the *loaded widget's* slug, never supplied by the sidecar itself.
pub fn widget_scoped_secret_name(name: &str, slug: &str) -> Option<String> {
    if name.len() > 256 || slug.is_empty() || slug.len() > 128 {
        return None;
    }
    let prefix_len = "widget:".len();
    if !name
        .get(..prefix_len)
        .is_some_and(|prefix| prefix.eq_ignore_ascii_case("widget:"))
    {
        return None;
    }
    let (requested_slug, key) = name[prefix_len..].split_once(':')?;
    if requested_slug.is_empty()
        || !requested_slug.eq_ignore_ascii_case(slug)
        || key.is_empty()
        || key.len() > 128
    {
        return None;
    }
    // Canonical spelling is the widget's own slug: the sanitizer maps `:`/`-` consistently,
    // so this is the same DPAPI file for every correctly scoped request.
    Some(format!("widget:{slug}:{key}"))
}

/// Reads a DPAPI secret for a custom widget sidecar. Never logged, never sent anywhere.
/// Returns `None` for out-of-scope names (the caller still sends an empty `secretResult`).
pub fn read_dpapi_secret_for_widget(name: &str, slug: &str) -> Option<String> {
    let scoped = widget_scoped_secret_name(name, slug)?;
    read_dpapi_secret(&scoped)
}

/// Encrypts and stores a secret using the same per-user DPAPI format as the reference app.
pub fn save_dpapi_secret(secret_key: &str, value: &str) -> bool {
    let Some(path) = secret_file_path(secret_key) else {
        return false;
    };
    if value.trim().is_empty() {
        return false;
    }
    let mut plain = value.as_bytes().to_vec();
    let mut entropy = *AI_SECRET_ENTROPY;
    let in_blob = CRYPT_INTEGER_BLOB {
        cbData: plain.len() as u32,
        pbData: plain.as_mut_ptr(),
    };
    let ent_blob = CRYPT_INTEGER_BLOB {
        cbData: entropy.len() as u32,
        pbData: entropy.as_mut_ptr(),
    };
    let mut out_blob = CRYPT_INTEGER_BLOB {
        cbData: 0,
        pbData: std::ptr::null_mut(),
    };
    // SAFETY: all blobs point at live buffers; the output blob is owned by us and freed below.
    let ok = unsafe {
        CryptProtectData(
            &in_blob,
            std::ptr::null(),
            &ent_blob,
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            CRYPTPROTECT_UI_FORBIDDEN,
            &mut out_blob,
        ) != 0
            && !out_blob.pbData.is_null()
            && out_blob.cbData > 0
    };
    unsafe {
        std::ptr::write_bytes(plain.as_mut_ptr(), 0, plain.len());
    }
    if !ok {
        return false;
    }
    let encrypted =
        unsafe { std::slice::from_raw_parts(out_blob.pbData, out_blob.cbData as usize) }.to_vec();
    unsafe {
        LocalFree(out_blob.pbData.cast());
    }
    if std::fs::create_dir_all(path.parent().expect("secret file has a parent directory")).is_err()
    {
        return false;
    }
    let tmp = guid_tmp_path(&path);
    let saved = std::fs::write(&tmp, &encrypted)
        .and_then(|()| std::fs::rename(&tmp, &path))
        .is_ok();
    if !saved {
        let _ = std::fs::remove_file(&tmp);
    }
    saved
}

pub fn clear_dpapi_secret(secret_key: &str) -> bool {
    let Some(path) = secret_file_path(secret_key) else {
        return false;
    };
    match std::fs::remove_file(path) {
        Ok(()) => true,
        Err(error) => error.kind() == std::io::ErrorKind::NotFound,
    }
}

pub fn has_dpapi_secret(secret_key: &str) -> bool {
    secret_file_path(secret_key)
        .is_some_and(|path| std::fs::metadata(path).is_ok_and(|meta| meta.len() > 0))
}

/// DPAPI-backed API keys are supported by these providers (`AiSecretStore.cs` parity).
pub fn ai_api_key_providers() -> &'static [(&'static str, &'static str)] {
    &[
        ("aiusage_deepseek", "api-key:deepseek"),
        ("aiusage_openrouter", "api-key:openrouter"),
        ("aiusage_opencode", "api-key:opencode"),
    ]
}

fn canonical_ai_provider_id(provider_id: &str) -> Option<&'static str> {
    match provider_id.trim().to_ascii_lowercase().as_str() {
        "aiusage_claude" | "claude" => Some("aiusage_claude"),
        "aiusage_codex" | "codex" => Some("aiusage_codex"),
        "aiusage_opencode" | "opencode" => Some("aiusage_opencode"),
        "aiusage_copilot" | "copilot" => Some("aiusage_copilot"),
        "aiusage_deepseek" | "deepseek" => Some("aiusage_deepseek"),
        "aiusage_openrouter" | "openrouter" => Some("aiusage_openrouter"),
        "aiusage_cursor" | "cursor" => Some("aiusage_cursor"),
        _ => None,
    }
}

/// DPAPI secret store key for any of the seven AI usage providers.
pub fn ai_secret_key_for_provider(provider_id: &str) -> Option<&'static str> {
    match canonical_ai_provider_id(provider_id)? {
        "aiusage_claude" => Some("api-key:claude"),
        "aiusage_codex" => Some("api-key:codex"),
        "aiusage_opencode" => Some("api-key:opencode"),
        "aiusage_copilot" => Some("api-key:copilot"),
        "aiusage_deepseek" => Some("api-key:deepseek"),
        "aiusage_openrouter" => Some("api-key:openrouter"),
        "aiusage_cursor" => Some("api-key:cursor"),
        _ => None,
    }
}

/// Source that supplies credentials for an AI provider.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AiAuthSource {
    None,
    Env,
    Dpapi,
    CliFile,
    GhKeyring,
    CursorPkce,
}

/// Non-blocking credential and CLI presence probe for Settings UI & telemetry diagnostics.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AiAuthProbe {
    pub source: AiAuthSource,
    pub cli_installed: bool,
    pub has_token: bool,
    pub env_override: bool,
    pub secret_key: &'static str,
    pub env_var_hint: &'static str,
}

fn find_executable_on_path(command: &str) -> Option<PathBuf> {
    let paths = std::env::var_os("PATH")?;
    for dir in std::env::split_paths(&paths) {
        for extension in [".exe", ".cmd", ".bat", ""] {
            let candidate = dir.join(format!("{command}{extension}"));
            if candidate.is_file() {
                return Some(candidate);
            }
        }
    }
    None
}

fn json_has_non_empty_str(val: &serde_json::Value, keys: &[&str]) -> bool {
    keys.iter().any(|k| {
        val.get(*k)
            .and_then(|v| v.as_str())
            .is_some_and(|s| !s.trim().is_empty())
    })
}

fn claude_cli_file_has_token(home: Option<&PathBuf>) -> bool {
    let Some(path) = home.map(|h| h.join(".claude").join(".credentials.json")) else {
        return false;
    };
    let Ok(raw) = std::fs::read_to_string(path) else {
        return false;
    };
    let Ok(v) = serde_json::from_str::<serde_json::Value>(&raw) else {
        return false;
    };
    let auth = v.get("claudeAiOauth").unwrap_or(&v);
    json_has_non_empty_str(
        auth,
        &[
            "accessToken",
            "access_token",
            "refreshToken",
            "refresh_token",
        ],
    )
}

fn codex_cli_file_has_token(home: Option<&PathBuf>) -> bool {
    let paths = [
        home.map(|h| h.join(".codex").join("auth.json")),
        home.map(|h| {
            h.join(".local")
                .join("share")
                .join("codex")
                .join("auth.json")
        }),
    ];
    paths.into_iter().flatten().any(|path| {
        let Ok(raw) = std::fs::read_to_string(path) else {
            return false;
        };
        let Ok(v) = serde_json::from_str::<serde_json::Value>(&raw) else {
            return false;
        };
        let tokens = v.get("tokens").unwrap_or(&v);
        json_has_non_empty_str(tokens, &["access_token", "api_key"])
    })
}

fn opencode_cli_file_token(home: Option<&PathBuf>, local: Option<&PathBuf>) -> Option<String> {
    let paths = [
        home.map(|h| h.join(".opencode").join("auth.json")),
        home.map(|h| {
            h.join(".local")
                .join("share")
                .join("opencode")
                .join("auth.json")
        }),
        local.map(|l| l.join("opencode").join("auth.json")),
    ];
    paths.into_iter().flatten().find_map(|path| {
        let v: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(path).ok()?).ok()?;
        v.get("token")
            .or_else(|| v.get("access_token"))
            .and_then(|t| t.as_str())
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(str::to_owned)
    })
}

fn copilot_cli_file_token(home: Option<&PathBuf>, local: Option<&PathBuf>) -> Option<String> {
    let paths = [
        home.map(|h| h.join(".config").join("github-copilot").join("apps.json")),
        local.map(|l| l.join("github-copilot").join("apps.json")),
    ];
    for path in paths.into_iter().flatten() {
        if let Ok(raw) = std::fs::read_to_string(path)
            && let Ok(serde_json::Value::Object(map)) = serde_json::from_str(&raw)
        {
            for val in map.values() {
                if let Some(tok) = val.get("oauth_token").and_then(|s| s.as_str())
                    && !tok.trim().is_empty()
                {
                    return Some(tok.trim().to_owned());
                }
            }
        }
    }
    None
}

fn gh_hosts_has_auth(home: Option<&PathBuf>, appdata: Option<&PathBuf>) -> bool {
    let paths = [
        appdata.map(|d| d.join("GitHub CLI").join("hosts.yml")),
        home.map(|h| h.join(".config").join("gh").join("hosts.yml")),
    ];
    paths
        .into_iter()
        .flatten()
        .any(|p| std::fs::read_to_string(p).is_ok_and(|s| !s.trim().is_empty()))
}

/// Fast, non-blocking probe of credential sources and CLI availability for `provider_id`.
pub fn ai_auth_probe(provider_id: &str) -> AiAuthProbe {
    let Some(canonical) = canonical_ai_provider_id(provider_id) else {
        return AiAuthProbe {
            source: AiAuthSource::None,
            cli_installed: false,
            has_token: false,
            env_override: false,
            secret_key: "",
            env_var_hint: "",
        };
    };
    let home = std::env::var_os("USERPROFILE").map(PathBuf::from);
    let local = std::env::var_os("LOCALAPPDATA").map(PathBuf::from);
    let appdata = std::env::var_os("APPDATA").map(PathBuf::from);
    let secret_key = ai_secret_key_for_provider(canonical).unwrap_or("");
    let has_dpapi = !secret_key.is_empty() && has_dpapi_secret(secret_key);

    match canonical {
        "aiusage_claude" => {
            let env_override = env_token(&["ANTHROPIC_API_KEY"]).is_some();
            let cli_installed = find_executable_on_path("claude").is_some();
            let cli_file = claude_cli_file_has_token(home.as_ref());
            let source = if env_override {
                AiAuthSource::Env
            } else if cli_file {
                AiAuthSource::CliFile
            } else if has_dpapi {
                AiAuthSource::Dpapi
            } else {
                AiAuthSource::None
            };
            AiAuthProbe {
                source,
                cli_installed,
                has_token: source != AiAuthSource::None,
                env_override,
                secret_key,
                env_var_hint: "ANTHROPIC_API_KEY",
            }
        }
        "aiusage_codex" => {
            let env_override = env_token(&["CODEX_API_KEY"]).is_some();
            let cli_installed = find_executable_on_path("codex").is_some();
            let cli_file = codex_cli_file_has_token(home.as_ref());
            let source = if env_override {
                AiAuthSource::Env
            } else if cli_file {
                AiAuthSource::CliFile
            } else if has_dpapi {
                AiAuthSource::Dpapi
            } else {
                AiAuthSource::None
            };
            AiAuthProbe {
                source,
                cli_installed,
                has_token: source != AiAuthSource::None,
                env_override,
                secret_key,
                env_var_hint: "CODEX_API_KEY",
            }
        }
        "aiusage_opencode" => {
            let env_override = env_token(&[
                "OPENCODE_GO_API_KEY",
                "OPENCODE_API_KEY",
                "OPENCODE_ZEN_API_KEY",
            ])
            .is_some();
            let cli_installed = find_executable_on_path("opencode").is_some();
            let cli_file = opencode_cli_file_token(home.as_ref(), local.as_ref()).is_some();
            let source = if env_override {
                AiAuthSource::Env
            } else if has_dpapi {
                AiAuthSource::Dpapi
            } else if cli_file {
                AiAuthSource::CliFile
            } else {
                AiAuthSource::None
            };
            AiAuthProbe {
                source,
                cli_installed,
                has_token: source != AiAuthSource::None,
                env_override,
                secret_key,
                env_var_hint: "OPENCODE_GO_API_KEY",
            }
        }
        "aiusage_copilot" => {
            let env_override = env_token(&["GITHUB_TOKEN", "GH_TOKEN"]).is_some();
            let cli_installed = find_executable_on_path("gh").is_some();
            let cli_file = copilot_cli_file_token(home.as_ref(), local.as_ref()).is_some();
            let gh_keyring = cli_installed && gh_hosts_has_auth(home.as_ref(), appdata.as_ref());
            let source = if env_override {
                AiAuthSource::Env
            } else if cli_file {
                AiAuthSource::CliFile
            } else if has_dpapi {
                AiAuthSource::Dpapi
            } else if gh_keyring {
                AiAuthSource::GhKeyring
            } else {
                AiAuthSource::None
            };
            AiAuthProbe {
                source,
                cli_installed,
                has_token: source != AiAuthSource::None,
                env_override,
                secret_key,
                env_var_hint: "GITHUB_TOKEN",
            }
        }
        "aiusage_deepseek" => {
            let env_override = env_token(&["DEEPSEEK_API_KEY"]).is_some();
            let source = if env_override {
                AiAuthSource::Env
            } else if has_dpapi {
                AiAuthSource::Dpapi
            } else {
                AiAuthSource::None
            };
            AiAuthProbe {
                source,
                cli_installed: false,
                has_token: source != AiAuthSource::None,
                env_override,
                secret_key,
                env_var_hint: "DEEPSEEK_API_KEY",
            }
        }
        "aiusage_openrouter" => {
            let env_override = env_token(&["OPENROUTER_API_KEY"]).is_some();
            let source = if env_override {
                AiAuthSource::Env
            } else if has_dpapi {
                AiAuthSource::Dpapi
            } else {
                AiAuthSource::None
            };
            AiAuthProbe {
                source,
                cli_installed: false,
                has_token: source != AiAuthSource::None,
                env_override,
                secret_key,
                env_var_hint: "OPENROUTER_API_KEY",
            }
        }
        "aiusage_cursor" => {
            let env_override = env_token(&["CURSOR_API_KEY"]).is_some();
            let pkce = crate::platform::cursor_auth::has_session();
            let source = if env_override {
                AiAuthSource::Env
            } else if pkce {
                AiAuthSource::CursorPkce
            } else if has_dpapi {
                AiAuthSource::Dpapi
            } else {
                AiAuthSource::None
            };
            AiAuthProbe {
                source,
                cli_installed: false,
                has_token: source != AiAuthSource::None,
                env_override,
                secret_key,
                env_var_hint: "CURSOR_API_KEY",
            }
        }
        _ => AiAuthProbe {
            source: AiAuthSource::None,
            cli_installed: false,
            has_token: false,
            env_override: false,
            secret_key: "",
            env_var_hint: "",
        },
    }
}

/// Reads a token from an environment override. Environment variables always win over stored
/// credentials so a user can rotate a key without touching the DPAPI store (`AiUsageService`).
fn env_token(names: &[&str]) -> Option<String> {
    names
        .iter()
        .find_map(|name| std::env::var(name).ok())
        .map(|value| value.trim().to_owned())
        .filter(|value| !value.is_empty())
}

/// Reads a GitHub token from the `gh` CLI keyring when no stored Copilot credential is present.
fn gh_cli_token() -> Option<String> {
    let output = Command::new("gh")
        .args(["auth", "token"])
        .creation_flags(CREATE_NO_WINDOW)
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let token = String::from_utf8_lossy(&output.stdout).trim().to_owned();
    (!token.is_empty()).then_some(token)
}

/// Exchanges a Claude OAuth refresh token for a fresh access token. The refreshed token is
/// written back to the credentials file for the next fetch (never logged or sent anywhere else).
fn refresh_claude_oauth(path: &std::path::Path, auth: &serde_json::Value) -> Option<String> {
    let refresh = auth
        .get("refreshToken")
        .or_else(|| auth.get("refresh_token"))
        .and_then(|v| v.as_str())?
        .trim()
        .to_owned();
    let body = serde_json::json!({
        "grant_type": "refresh_token",
        "refresh_token": refresh,
        "client_id": "9d1c249d-4e93-4932-ac16-d721a2e2640c",
    })
    .to_string();
    let (status, response, _) = winhttp_json(
        "POST",
        "api.anthropic.com",
        "/v1/oauth/token",
        &[("Content-Type", "application/json")],
        Some(&body),
    )?;
    if status != 200 {
        return None;
    }
    let root: serde_json::Value = serde_json::from_str(&response).ok()?;
    let token = root.get("access_token")?.as_str()?.trim().to_owned();
    if token.is_empty() {
        return None;
    }
    if let Ok(mut file) = serde_json::from_str::<serde_json::Value>(
        &std::fs::read_to_string(path).unwrap_or_default(),
    ) && let Some(object) = file.as_object_mut()
    {
        let target = if object.contains_key("claudeAiOauth") {
            object
                .get_mut("claudeAiOauth")
                .and_then(|v| v.as_object_mut())
        } else {
            Some(object)
        };
        if let Some(target) = target {
            target.insert("accessToken".to_owned(), serde_json::json!(token));
            if let Some(new_refresh) = root.get("refresh_token").and_then(|v| v.as_str()) {
                target.insert("refreshToken".to_owned(), serde_json::json!(new_refresh));
            }
            let _ = std::fs::write(
                path,
                serde_json::to_string_pretty(&file).unwrap_or_default(),
            );
        }
    }
    Some(token)
}

fn read_provider_token(provider_id: &str) -> Option<(String, Option<String>)> {
    let canonical = canonical_ai_provider_id(provider_id)?;
    let home = std::env::var_os("USERPROFILE").map(PathBuf::from);
    let local = std::env::var_os("LOCALAPPDATA").map(PathBuf::from);
    match canonical {
        "aiusage_deepseek" => env_token(&["DEEPSEEK_API_KEY"])
            .or_else(|| read_dpapi_secret("api-key:deepseek"))
            .map(|t| (t, None)),
        "aiusage_openrouter" => env_token(&["OPENROUTER_API_KEY"])
            .or_else(|| read_dpapi_secret("api-key:openrouter"))
            .map(|t| (t, None)),
        "aiusage_opencode" => env_token(&[
            "OPENCODE_GO_API_KEY",
            "OPENCODE_API_KEY",
            "OPENCODE_ZEN_API_KEY",
        ])
        .or_else(|| read_dpapi_secret("api-key:opencode"))
        .or_else(|| opencode_cli_file_token(home.as_ref(), local.as_ref()))
        .map(|t| (t, None)),
        "aiusage_claude" => {
            if let Some(key) = env_token(&["ANTHROPIC_API_KEY"]) {
                return Some((key, None));
            }
            if let Some(path) = home
                .as_ref()
                .map(|h| h.join(".claude").join(".credentials.json"))
                && let Ok(raw) = std::fs::read_to_string(&path)
                && let Ok(v) = serde_json::from_str::<serde_json::Value>(&raw)
            {
                let auth = v.get("claudeAiOauth").unwrap_or(&v).clone();
                let mut tok = auth
                    .get("accessToken")
                    .or_else(|| auth.get("access_token"))
                    .and_then(|s| s.as_str())
                    .unwrap_or_default()
                    .trim()
                    .to_owned();
                if tok.is_empty()
                    && let Some(refreshed) = refresh_claude_oauth(&path, &auth)
                {
                    tok = refreshed;
                }
                let plan = auth
                    .get("subscriptionType")
                    .and_then(|s| s.as_str())
                    .map(str::to_owned);
                if !tok.is_empty() {
                    return Some((tok, plan));
                }
            }
            read_dpapi_secret("api-key:claude").map(|t| (t, None))
        }
        "aiusage_codex" => {
            if let Some(key) = env_token(&["CODEX_API_KEY"]) {
                return Some((key, None));
            }
            let paths = [
                home.as_ref().map(|h| h.join(".codex").join("auth.json")),
                home.as_ref().map(|h| {
                    h.join(".local")
                        .join("share")
                        .join("codex")
                        .join("auth.json")
                }),
            ];
            for path in paths.into_iter().flatten() {
                let Ok(raw) = std::fs::read_to_string(path) else {
                    continue;
                };
                let Ok(v) = serde_json::from_str::<serde_json::Value>(&raw) else {
                    continue;
                };
                let tokens = v.get("tokens").unwrap_or(&v);
                let Some(tok) = tokens
                    .get("access_token")
                    .or_else(|| tokens.get("api_key"))
                    .and_then(|t| t.as_str())
                    .map(str::trim)
                    .filter(|t| !t.is_empty())
                else {
                    continue;
                };
                let acct = tokens
                    .get("account_id")
                    .and_then(|s| s.as_str())
                    .map(str::to_owned);
                return Some((tok.to_owned(), acct));
            }
            read_dpapi_secret("api-key:codex").map(|t| (t, None))
        }
        "aiusage_copilot" => {
            if let Some(key) = env_token(&["GITHUB_TOKEN", "GH_TOKEN"]) {
                return Some((key, None));
            }
            if let Some(tok) = copilot_cli_file_token(home.as_ref(), local.as_ref()) {
                return Some((tok, None));
            }
            if let Some(tok) = read_dpapi_secret("api-key:copilot") {
                return Some((tok, None));
            }
            gh_cli_token().map(|tok| (tok, None))
        }
        "aiusage_cursor" => env_token(&["CURSOR_API_KEY"])
            .or_else(crate::platform::cursor_auth::token)
            .or_else(|| read_dpapi_secret("api-key:cursor"))
            .map(|tok| (tok, None)),
        _ => None,
    }
}

/// Concise local countdown for a Unix-seconds reset timestamp (used by AI chips & flyout headers).
pub fn countdown_from_unix(reset_secs: i64, now_secs: u64) -> Option<String> {
    let remaining = reset_secs.saturating_sub(now_secs as i64);
    if remaining <= 0 {
        return Some(loc::t("Expired"));
    }
    let (days, rem) = (remaining / 86_400, remaining % 86_400);
    let (hours, rem) = (rem / 3600, rem % 3600);
    let mins = rem / 60;
    let is_tr = loc::language().to_ascii_lowercase().starts_with("tr");
    Some(if is_tr {
        if days > 0 {
            format!("{days}g {hours}s")
        } else if hours > 0 {
            format!("{hours}s {mins}dk")
        } else {
            format!("{mins}dk")
        }
    } else if days > 0 {
        format!("{days}d {hours}h")
    } else if hours > 0 {
        format!("{hours}h {mins}m")
    } else {
        format!("{mins}m")
    })
}

fn format_reset_iso(iso: &str) -> String {
    let now_secs = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_secs());
    if let Some(ms) = parse_reset_ms(Some(iso))
        && let Some(countdown) = countdown_from_unix(ms / 1000, now_secs)
    {
        return countdown;
    }
    if let Some((date_part, time_part)) = iso.split_once('T') {
        let hm: String = time_part.chars().take(5).collect();
        if date_part.len() >= 10 {
            return format!("{} {hm}", &date_part[5..10]);
        }
        return hm;
    }
    "—".to_owned()
}

fn format_reset_from_ms_or_dash(ms: Option<i64>) -> String {
    let Some(ms) = ms.filter(|&v| v > 0) else {
        return "—".to_owned();
    };
    let now_secs = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_secs());
    countdown_from_unix(ms / 1000, now_secs).unwrap_or_else(|| "—".to_owned())
}

fn parse_pct_01(v: &serde_json::Value) -> Option<f32> {
    let n = v.as_f64()?;
    if !n.is_finite() {
        return None;
    }
    let ratio = if n > 1.0 { n / 100.0 } else { n };
    Some((ratio as f32).clamp(0.0, 1.0))
}

fn fetch_ai_snapshot(provider_id: &str) -> AiUsageSnapshot {
    let detected = ai_provider_detected(provider_id);
    let Some((token, extra)) = read_provider_token(provider_id) else {
        return AiUsageSnapshot {
            has_cred: detected,
            last_fetch: Some(Instant::now()),
            ..Default::default()
        };
    };
    let auth_hdr = format!("Bearer {token}");
    let mut snap = AiUsageSnapshot {
        has_cred: true,
        last_fetch: Some(Instant::now()),
        ..Default::default()
    };

    match provider_id {
        "aiusage_deepseek" => {
            if let Some((200, body)) = winhttp_get(
                "api.deepseek.com",
                "/user/balance",
                &[("Authorization", &auth_hdr)],
            ) && let Ok(root) = serde_json::from_str::<serde_json::Value>(&body)
                && let Some(infos) = root.get("balance_infos").and_then(|v| v.as_array())
            {
                for info in infos {
                    if let Some(bal_str) = info.get("total_balance").and_then(|v| v.as_str())
                        && let Ok(bal) = bal_str.parse::<f64>()
                    {
                        let cur = info
                            .get("currency")
                            .and_then(|v| v.as_str())
                            .unwrap_or("USD");
                        let sym = if cur.eq_ignore_ascii_case("USD") {
                            "$"
                        } else {
                            ""
                        };
                        snap.balance_text = Some(if sym.is_empty() {
                            format!("{bal:.2} {cur}")
                        } else {
                            format!("{sym}{bal:.2}")
                        });
                        snap.plan = Some(format!("{bal:.2} {cur}"));
                        break;
                    }
                }
            }
        }
        "aiusage_openrouter" => {
            if let Some((200, body)) = winhttp_get(
                "openrouter.ai",
                "/api/v1/credits",
                &[("Authorization", &auth_hdr)],
            ) && let Ok(root) = serde_json::from_str::<serde_json::Value>(&body)
                && let Some(data) = root.get("data")
                && let (Some(total), Some(usage)) = (
                    data.get("total_credits").and_then(|v| v.as_f64()),
                    data.get("total_usage").and_then(|v| v.as_f64()),
                )
            {
                let rem = (total - usage).max(0.0);
                snap.balance_text = Some(format!("${rem:.2}"));
                snap.plan = Some(loc::t_fmt("AiUsageBalanceLeft", &[&format!("${rem:.2}")]));
                snap.credit_ratio = (total > 0.0).then(|| (rem / total).clamp(0.0, 1.0) as f32);
            }
        }
        "aiusage_claude" => {
            snap.plan = extra;
            if let Some((200, body)) = winhttp_get(
                "api.anthropic.com",
                "/api/oauth/usage",
                &[
                    ("Authorization", &auth_hdr),
                    ("anthropic-beta", "oauth-2025-04-20"),
                ],
            ) && let Ok(root) = serde_json::from_str::<serde_json::Value>(&body)
            {
                for (key, lbl) in [("five_hour", "5h"), ("seven_day", "7d")] {
                    if let Some(obj) = root.get(key)
                        && let Some(used) = obj.get("utilization").and_then(parse_pct_01)
                    {
                        let reset = obj
                            .get("resets_at")
                            .and_then(|v| v.as_str())
                            .map(format_reset_iso)
                            .unwrap_or_else(|| "—".to_owned());
                        snap.windows.push(AiWindowData {
                            label: lbl.to_owned(),
                            used,
                            reset_text: reset,
                        });
                    }
                }
            }
        }
        "aiusage_codex" => {
            let mut hdrs = vec![("Authorization", auth_hdr.as_str())];
            if let Some(ref acct) = extra
                && !acct.is_empty()
            {
                hdrs.push(("ChatGPT-Account-Id", acct.as_str()));
            }
            if let Some((200, body)) = winhttp_get("chatgpt.com", "/backend-api/wham/usage", &hdrs)
                && let Ok(root) = serde_json::from_str::<serde_json::Value>(&body)
                && let Some(rl) = root.get("rate_limit")
            {
                for (key, lbl) in [("primary_window", "5h"), ("secondary_window", "7d")] {
                    if let Some(w) = rl.get(key)
                        && let Some(used) = w.get("used_percent").and_then(parse_pct_01)
                    {
                        let ms = full_unix_ms(w, &["reset_at", "resets_at", "resetTime"]).or_else(
                            || {
                                full_num(w, "reset_after_seconds")
                                    .map(|s| now_unix_ms() + (s * 1000.0) as i64)
                            },
                        );
                        let reset_text = if ms.is_some() {
                            format_reset_from_ms_or_dash(ms)
                        } else {
                            w.get("reset_at")
                                .or_else(|| w.get("resets_at"))
                                .and_then(|v| v.as_str())
                                .map(format_reset_iso)
                                .unwrap_or_else(|| "—".to_owned())
                        };
                        snap.windows.push(AiWindowData {
                            label: lbl.to_owned(),
                            used,
                            reset_text,
                        });
                    }
                }
            }
        }
        "aiusage_opencode" => {
            if let Some((200, body)) = winhttp_get(
                "opencode.ai",
                "/zen/go/v1/usage",
                &[("Authorization", &auth_hdr)],
            ) && let Ok(root) = serde_json::from_str::<serde_json::Value>(&body)
            {
                for (keys, lbl) in [
                    (&["rolling5h", "rollingUsage", "five_hour"][..], "1h"),
                    (&["weekly", "weeklyUsage", "seven_day"][..], "7d"),
                    (&["monthly", "monthlyUsage", "thirty_day"][..], "30d"),
                ] {
                    for k in keys {
                        if let Some(obj) = root.get(*k)
                            && let Some(used) = obj
                                .get("usagePercent")
                                .or_else(|| obj.get("used_percent"))
                                .or_else(|| obj.get("utilization"))
                                .and_then(parse_pct_01)
                        {
                            let ms = full_unix_ms(obj, &["resetsAt", "reset_at", "resetTime"])
                                .or_else(|| {
                                    full_num(obj, "resetInSec")
                                        .map(|s| now_unix_ms() + (s * 1000.0) as i64)
                                });
                            let reset_text = if ms.is_some() {
                                format_reset_from_ms_or_dash(ms)
                            } else {
                                obj.get("resetsAt")
                                    .or_else(|| obj.get("reset_at"))
                                    .and_then(|v| v.as_str())
                                    .map(format_reset_iso)
                                    .unwrap_or_else(|| "—".to_owned())
                            };
                            snap.windows.push(AiWindowData {
                                label: lbl.to_owned(),
                                used,
                                reset_text,
                            });
                            break;
                        }
                    }
                }
            }
        }
        "aiusage_copilot" => {
            if let Some((200, body)) = winhttp_get(
                "api.github.com",
                "/copilot_internal/user",
                &[
                    ("Authorization", &auth_hdr),
                    ("Accept", "application/vnd.github+json"),
                ],
            ) && let Ok(root) = serde_json::from_str::<serde_json::Value>(&body)
                && let Some(rem) = root
                    .get("quota_snapshots")
                    .and_then(|q| q.get("premium_interactions"))
                    .and_then(|p| p.get("percent_remaining"))
                    .and_then(|v| v.as_f64())
            {
                let used = ((100.0 - rem) / 100.0).clamp(0.0, 1.0) as f32;
                let reset = root
                    .get("quota_reset_date_utc")
                    .and_then(|v| v.as_str())
                    .map(format_reset_iso)
                    .unwrap_or_else(|| loc::t("AiUsageMonthly"));
                snap.windows.push(AiWindowData {
                    label: loc::t("AiUsageMonthly"),
                    used,
                    reset_text: reset,
                });
            }
        }
        "aiusage_cursor" => {
            let headers = [
                ("Authorization", auth_hdr.as_str()),
                ("Connect-Protocol-Version", "1"),
                ("Content-Type", "application/json"),
            ];
            if let Some((200, body, _)) = winhttp_json(
                "POST",
                "api2.cursor.sh",
                "/aiserver.v1.DashboardService/GetCurrentPeriodUsage",
                &headers,
                Some("{}"),
            ) && let Some(detail) = parse_full_detail(&body, "aiusage_cursor")
            {
                snap.plan = detail.plan;
                snap.windows.push(AiWindowData {
                    label: detail.label,
                    used: detail.used,
                    reset_text: format_reset_from_ms_or_dash(detail.reset_unix_ms),
                });
            }
        }
        _ => {}
    }
    snap
}

pub fn refresh_ai_provider(
    cache: &Arc<Mutex<AiUsageCache>>,
    provider_id: &'static str,
    force: bool,
) {
    let should = if let Ok(mut g) = cache.lock() {
        let entry = g.providers.entry(provider_id.to_owned()).or_default();
        let stale = entry
            .last_fetch
            .is_none_or(|t| t.elapsed().as_secs() >= 300);
        if !entry.loading && (force || stale) {
            entry.loading = true;
            true
        } else {
            false
        }
    } else {
        false
    };
    if !should {
        return;
    }
    let cache = Arc::clone(cache);
    let _ = std::thread::Builder::new()
        .name(format!("otb-{provider_id}"))
        .spawn(move || {
            let snap = fetch_ai_snapshot(provider_id);
            if let Ok(mut g) = cache.lock() {
                g.providers.insert(provider_id.to_owned(), snap);
            }
        });
}

/// Checks whether credentials or a CLI installation exist for `provider_id`.
pub fn ai_provider_detected(provider_id: &str) -> bool {
    let probe = ai_auth_probe(provider_id);
    probe.has_token || probe.cli_installed
}

#[cfg(test)]
mod tests {
    use super::*;

    fn bt_row(id: &str, percent: u32) -> BluetoothBatteryDevice {
        BluetoothBatteryDevice::new(
            id.to_owned(),
            "Headset".to_owned(),
            percent,
            BluetoothDeviceKind::Headphones,
        )
    }

    #[test]
    fn late_fallback_generation_never_reverts_a_fresh_reading() {
        let mut cache = BluetoothBatteryCache::default();
        // Generation 1 started first (slow PowerShell fallback), generation 2 is the
        // forced manual refresh that finished first with the fresh native reading.
        assert!(apply_bluetooth_snapshot(
            &mut cache,
            2,
            vec![bt_row("bt:A", 41)]
        ));
        assert!(!apply_bluetooth_snapshot(
            &mut cache,
            1,
            vec![bt_row("bt:A", 88)]
        ));
        assert_eq!(cache.generation, 2);
        assert_eq!(cache.devices.len(), 1);
        assert_eq!(cache.devices[0].percent, 41);
        // A newer generation still applies (including an honest empty result).
        assert!(apply_bluetooth_snapshot(&mut cache, 3, Vec::new()));
        assert!(cache.devices.is_empty());
    }

    #[test]
    fn powershell_fallback_keys_by_instance_and_requires_a_live_link() {
        let text = "Buds Pro Hands-Free AG|BTHENUM\\{0000111E}_X\\7&1&0&A0B1C2D3E4F5_C00000000|True|80\n\
                    Buds Pro Hands-Free AG|BTHENUM\\{0000111E}_X\\7&1&0&112233445566_C00000000|True|35\n\
                    Old Headset|BTHENUM\\{0000111E}_X\\7&1&0&665544332211_C00000000|False|90\n\
                    Broken|BTHENUM\\X|True|abc";
        let rows = parse_bluetooth_powershell_lines(text);
        assert_eq!(
            rows.len(),
            2,
            "same-name devices stay separate; offline is skipped"
        );
        assert_eq!(rows[0].percent, 80);
        assert_eq!(rows[1].percent, 35);
        assert_ne!(rows[0].id, rows[1].id);
        assert!(rows.iter().all(BluetoothBatteryDevice::is_live));
        assert!(rows[0].matches_ignore_entry(&rows[0].id.to_ascii_lowercase()));
        assert!(!rows[1].matches_ignore_entry(&rows[0].id));
        assert_eq!(rows[0].id, "bt:A0B1C2D3E4F5");
        assert_eq!(rows[1].id, "bt:112233445566");
        // DEV_<12 hex> also canonicalizes to bt:<MAC12_UPPER>; non-MAC instances keep bt-pnp:.
        let extra = "BLE Mouse|BTHLE\\DEV_aabbccddeeff\\7&1&0|True|64\n\
                     HFP Audio|BTHHFENUM\\BthHFPAudio\\8&3A2B&0&97|True|50";
        let extra_rows = parse_bluetooth_powershell_lines(extra);
        assert_eq!(extra_rows.len(), 2);
        assert_eq!(extra_rows[0].id, "bt:AABBCCDDEEFF");
        assert!(extra_rows[0].matches_ignore_entry("bt-pnp:BTHLE\\DEV_AABBCCDDEEFF\\7&1&0"));
        assert_eq!(
            extra_rows[1].id,
            "bt-pnp:BTHHFENUM\\BTHHFPAUDIO\\8&3A2B&0&97"
        );
    }

    #[test]
    fn bluetooth_error_path_respects_generation_and_clears_charging() {
        let mut cache = BluetoothBatteryCache::default();
        let mut dev = BluetoothBatteryDevice::new(
            "bt:AABBCCDDEEFF".to_owned(),
            "Headset".to_owned(),
            80,
            BluetoothDeviceKind::Headphones,
        );
        dev.charging = true;
        assert!(apply_bluetooth_snapshot(&mut cache, 2, vec![dev]));
        // Stale error from an older generation must be ignored.
        assert!(!apply_bluetooth_error(&mut cache, 1));
        assert!(cache.devices[0].is_live());
        assert!(cache.devices[0].charging);
        // Error from the current or newer generation marks stale, disconnected, not charging.
        assert!(apply_bluetooth_error(&mut cache, 2));
        assert!(cache.devices[0].stale);
        assert!(!cache.devices[0].connected);
        assert!(!cache.devices[0].charging);
    }

    #[test]
    fn ai_secrets_live_in_the_shared_secrets_folder() {
        let path = secret_file_path("api-key:deepseek").expect("secret key must resolve");
        assert_eq!(
            path,
            opentaskbar_core::paths::secrets_dir().join("api-key_deepseek.bin")
        );
        assert_eq!(
            opentaskbar_core::paths::secrets_dir(),
            opentaskbar_core::paths::shared_app_data_dir().join("secrets")
        );
        assert!(secret_file_path("   ").is_none());
    }

    #[test]
    fn ai_secret_entropy_matches_reference_ai_secret_store() {
        assert_eq!(AI_SECRET_ENTROPY, b"OpenTaskbar.AiSecretStore.v1");
        assert_eq!(AI_SECRET_ENTROPY.len(), 28);
        // 64 KiB guard, same as `AiSecretStore.MaxSecretFileBytes`.
        assert_eq!(1_usize << 16, 65_536);
    }

    #[test]
    fn ai_secret_tmp_name_matches_reference_guid_convention() {
        let path = Path::new("secret").join("api-key_deepseek.bin");
        let tmp = guid_tmp_path(&path);
        let name = tmp.file_name().unwrap().to_str().unwrap();
        let token = name
            .strip_prefix("api-key_deepseek.bin.")
            .and_then(|s| s.strip_suffix(".tmp"))
            .expect("reference-style .<guid>.tmp suffix");
        assert_eq!(token.len(), 32);
        assert!(token.bytes().all(|b| b.is_ascii_hexdigit()));
        assert_ne!(guid_tmp_path(&path), tmp);
    }

    #[test]
    fn gpu_engine_name_is_extracted_from_pdh_instance() {
        let instance = "pid_1234_luid_0x00000000_0x0000F3A1_phys_0_eng_0_engtype_3D";
        assert_eq!(gpu_engine_name(instance), Some("3D"));
        assert_eq!(gpu_engine_name("NVIDIA GeForce RTX 4070"), None);
        assert_eq!(gpu_engine_name("pid_1_luid_0x0_0x1_eng_0_engtype_"), None);
    }

    #[test]
    fn reverse_geocode_payload_yields_geo_match() {
        let body = r#"{"city":"İstanbul","principalSubdivision":"İstanbul","countryName":"Turkey","countryCode":"TR","latitude":41.0,"longitude":29.0}"#;
        let geo = parse_reverse_geocode(body, 41.008, 28.978).expect("city must parse");
        assert_eq!(geo.name, "İstanbul");
        assert_eq!(geo.setting_value(), "İstanbul, Turkey");
        assert_eq!(geo.latitude, 41.008);
        // Locality / subdivision fallback keeps a usable label when `city` is absent.
        let body = r#"{"locality":"","principalSubdivision":"Ankara","countryName":"Turkey"}"#;
        assert_eq!(
            parse_reverse_geocode(body, 39.9, 32.8).map(|g| g.name),
            Some("Ankara".to_owned())
        );
        assert!(parse_reverse_geocode("{}", 0.0, 0.0).is_none());
    }
}

// ============================================================================================
// WS-G additions: full AI detail, GPU meter, forecast helpers and media seek.
// ============================================================================================

// WS-G forecast helpers: stable English labels independent of UI locale.
const SHORT_DAYS: [&str; 7] = ["Sun", "Mon", "Tue", "Wed", "Thu", "Fri", "Sat"];

fn forecast_describe_wmo(code: i64) -> (&'static str, bool) {
    match code {
        0 => ("Clear", false),
        1 | 2 => ("Partly cloudy", false),
        3 => ("Overcast", false),
        45 | 48 => ("Fog", false),
        51..=57 => ("Drizzle", true),
        61..=67 => ("Rain", true),
        71..=77 => ("Snow", true),
        80..=82 => ("Showers", true),
        85 | 86 => ("Snow showers", true),
        95..=99 => ("Thunderstorm", true),
        _ => ("Cloudy", false),
    }
}

// ============================================================================================
// Full AI detail (`AiUsageService.Parse` parity — used by WS-C's AI flyout).
// ============================================================================================

/// Pay-as-you-go spend block (`AiUsageSpend`).
#[derive(Clone, Debug, Default)]
pub struct AiSpend {
    pub used: f64,
    pub limit: Option<f64>,
    pub currency: String,
    pub used_01: Option<f32>,
}

/// One non-zero Claude weekly surface share (`AiUsageShare`).
#[derive(Clone, Debug, Default)]
pub struct AiUsageShare {
    pub name: String,
    pub used: f32,
}

/// Complete parsed usage for a single provider (`AiUsageReport` flattened for WS-C).
#[derive(Clone, Debug, Default)]
pub struct AiFullDetail {
    pub provider_id: String,
    pub plan: Option<String>,
    /// Primary window label (`5h`, `7d`, `30d`, `Monthly`, `Balance`, ...).
    pub label: String,
    /// Primary window utilisation in `[0,1]`.
    pub used: f32,
    pub reset_text: String,
    pub reset_unix_ms: Option<i64>,
    pub spend: Option<AiSpend>,
    pub breakdown: Vec<AiUsageShare>,
    pub limit_reached: bool,
    /// `Retry-After` from a 429 response, in seconds.
    pub retry_after_secs: Option<u32>,
    pub credits: Option<f64>,
    pub unlimited_credits: bool,
    pub error: Option<String>,
}

fn full_obj<'a>(v: &'a serde_json::Value, key: &str) -> Option<&'a serde_json::Value> {
    v.get(key).filter(|x| x.is_object())
}

fn full_arr<'a>(v: &'a serde_json::Value, key: &str) -> Option<&'a [serde_json::Value]> {
    v.get(key).and_then(|x| x.as_array()).map(|a| a.as_slice())
}

fn full_str(v: &serde_json::Value, key: &str) -> Option<String> {
    v.get(key)
        .and_then(|x| x.as_str())
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_owned)
}

fn full_num(v: &serde_json::Value, key: &str) -> Option<f64> {
    v.get(key).and_then(|x| {
        if let Some(n) = x.as_f64() {
            Some(n)
        } else {
            x.as_str()?.trim().parse::<f64>().ok()
        }
    })
}

fn full_bool(v: &serde_json::Value, key: &str) -> bool {
    v.get(key).and_then(|x| x.as_bool()).unwrap_or(false)
}

/// Accepts both `0..1` and `0..100` encodings; clamps to `[0,1]`.
fn full_pct(v: &serde_json::Value, keys: &[&str]) -> Option<f32> {
    for k in keys {
        if let Some(n) = full_num(v, k) {
            if !n.is_finite() {
                continue;
            }
            let ratio = if n > 1.0 { n / 100.0 } else { n };
            return Some((ratio as f32).clamp(0.0, 1.0));
        }
    }
    None
}

fn full_unix_ms(v: &serde_json::Value, keys: &[&str]) -> Option<i64> {
    const MAX_MS: i64 = 253_402_300_799_999;
    for k in keys {
        if let Some(n) = full_num(v, k) {
            let ms = if n > 1e12 { n } else { n * 1000.0 };
            if ms > 0.0 && ms <= MAX_MS as f64 {
                return Some(ms as i64);
            }
        }
    }
    None
}

fn full_reset_from_ms(ms: Option<i64>) -> String {
    ms.map(|_| {
        // Keep a compact, locale-neutral string: the flyout only needs the date/time hint.
        let secs = ms.unwrap_or(0) / 1000;
        let days = secs.div_euclid(86_400);
        let rem = secs.rem_euclid(86_400);
        let (month, day) = civil_from_days(days);
        let (h, m) = (rem / 3600, (rem % 3600) / 60);
        format!("{month:02}-{day:02} {h:02}:{m:02}")
    })
    .unwrap_or_else(|| "—".to_owned())
}

fn civil_from_days(z: i64) -> (i32, i32) {
    let (_, month, day) = opentaskbar_core::civil_from_days(z);
    (month as i32, day as i32)
}

fn full_spend(root: &serde_json::Value) -> Option<AiSpend> {
    let spend = full_obj(root, "spend");
    let enabled = spend
        .and_then(|s| full_obj(s, "enabled").or_else(|| full_obj(root, "extra_usage")))
        .is_some()
        || spend.is_some_and(|s| full_bool(s, "enabled"))
        || full_obj(root, "extra_usage").is_some_and(|e| full_bool(e, "is_enabled"));
    let spend = spend?;
    if !enabled {
        return None;
    }
    let money = |key: &str| -> Option<(f64, String)> {
        let m = full_obj(spend, key)?;
        let minor = full_num(m, "amount_minor")?;
        let exponent = full_num(m, "exponent").unwrap_or(2.0).clamp(0.0, 6.0) as i32;
        let currency = full_str(m, "currency")
            .filter(|c| c.len() == 3)
            .unwrap_or_else(|| "USD".to_owned());
        Some((minor / 10f64.powi(exponent), currency))
    };
    let (used, currency) = money("used")?;
    let (limit, _) = money("limit").unzip();
    Some(AiSpend {
        used,
        limit,
        currency,
        used_01: full_obj(spend, "percent")
            .and_then(|p| {
                full_num(p, "used")
                    .or_else(|| full_num(p, "value"))
                    .or_else(|| p.as_f64())
            })
            .map(|n| ((if n > 1.0 { n / 100.0 } else { n }) as f32).clamp(0.0, 1.0)),
    })
}

fn full_plan_label(plan: Option<String>) -> Option<String> {
    let plan = plan?;
    let words: Vec<&str> = plan.split('_').filter(|w| !w.is_empty()).collect();
    if words.len() > 2 && words[0] == "default" && words[1] == "claude" {
        let tail = &words[2..];
        let title = tail
            .iter()
            .map(|w| {
                let mut c = w.chars();
                match c.next() {
                    Some(f) => f.to_uppercase().collect::<String>() + c.as_str(),
                    None => String::new(),
                }
            })
            .collect::<Vec<_>>()
            .join(" ");
        Some(title)
    } else {
        Some(plan)
    }
}

/// Pure JSON -> `AiFullDetail` parser shared by live fetches and tests.
///
/// Covers all seven providers, including Claude spend/breakdown, Codex credits and Cursor plan
/// labels. Returns `None` for malformed or empty payloads.
pub fn parse_full_detail(json: &str, provider_id: &str) -> Option<AiFullDetail> {
    let root: serde_json::Value = serde_json::from_str(json).ok()?;
    if !root.is_object() {
        return None;
    }
    let provider = provider_id.to_ascii_lowercase();
    let mut out = AiFullDetail {
        provider_id: provider_id.to_owned(),
        ..Default::default()
    };

    match provider.as_str() {
        "aiusage_claude" | "claude" => {
            let mut best: Option<(f32, String, Option<i64>)> = None;
            for key in ["five_hour", "seven_day"] {
                if let Some(w) = full_obj(&root, key)
                    && let Some(used) = full_pct(w, &["utilization", "used_percent", "used"])
                {
                    let ms = parse_reset_ms(full_str(w, "resets_at").as_deref());
                    let candidate = (used, key.to_owned(), ms);
                    if best.as_ref().is_none_or(|b| candidate.0 >= b.0) {
                        best = Some(candidate);
                    }
                }
            }
            if best.is_none()
                && let Some(used) = full_pct(&root, &["utilization", "used_percent", "used"])
            {
                best = Some((used, "5h".to_owned(), None));
            }
            let (used, label, ms) = best?;
            out.used = used;
            out.label = if label == "five_hour" {
                "5h".to_owned()
            } else {
                "7d".to_owned()
            };
            out.reset_unix_ms = ms;
            out.reset_text = full_reset_from_ms(ms);
            out.plan = full_plan_label(
                full_str(&root, "plan")
                    .or_else(|| full_str(&root, "subscriptionType"))
                    .or_else(|| full_str(&root, "rateLimitTier")),
            );
            out.spend = full_spend(&root);
            if let Some(rows) =
                full_obj(&root, "seven_day_breakdown").and_then(|b| full_arr(b, "rows"))
            {
                for row in rows {
                    if let (Some(name), Some(used)) = (
                        full_str(row, "display_name"),
                        full_pct(row, &["percent_of_limit", "used"]),
                    ) && (used * 100.0).round() >= 1.0
                    {
                        out.breakdown.push(AiUsageShare { name, used });
                    }
                }
            }
            out.limit_reached = used >= 1.0 || full_bool(&root, "limit_reached");
            out.credits = full_num(&root, "credits");
        }
        "aiusage_codex" | "codex" => {
            let rl = full_obj(&root, "rate_limit").unwrap_or(&root);
            let mut best: Option<(f32, String, Option<i64>)> = None;
            for (key, label) in [("primary_window", "5h"), ("secondary_window", "7d")] {
                if let Some(w) = full_obj(rl, key)
                    && let Some(used) = full_pct(w, &["used_percent", "utilization", "used"])
                {
                    let ms = full_unix_ms(w, &["reset_at", "resets_at", "resetTime"]);
                    let ms = ms.or_else(|| {
                        full_num(w, "reset_after_seconds")
                            .map(|s| now_unix_ms() + (s * 1000.0) as i64)
                    });
                    let candidate = (used, label.to_owned(), ms);
                    if best.as_ref().is_none_or(|b| candidate.0 >= b.0) {
                        best = Some(candidate);
                    }
                }
            }
            let (used, label, ms) = best?;
            out.used = used;
            out.label = label;
            out.reset_unix_ms = ms;
            out.reset_text = full_reset_from_ms(ms);
            out.plan = full_plan_label(
                full_str(&root, "plan_type")
                    .or_else(|| full_str(&root, "plan"))
                    .or_else(|| full_str(&root, "tier")),
            );
            out.limit_reached = full_bool(rl, "limit_reached")
                || rl.get("allowed").and_then(|v| v.as_bool()) == Some(false)
                || used >= 1.0;
            if let Some(credits) = full_obj(&root, "credits") {
                out.unlimited_credits = full_bool(credits, "unlimited");
                if !out.unlimited_credits {
                    out.credits = full_num(credits, "balance");
                }
            }
        }
        "aiusage_opencode" | "opencode" => {
            let mut best: Option<(f32, String, Option<i64>)> = None;
            for (keys, label) in [
                (&["rolling5h", "rollingUsage", "five_hour"][..], "1h"),
                (&["weekly", "weeklyUsage", "seven_day"][..], "7d"),
                (&["monthly", "monthlyUsage", "thirty_day"][..], "30d"),
            ] {
                for k in keys {
                    if let Some(w) = full_obj(&root, k)
                        && let Some(used) = full_pct(
                            w,
                            &["usagePercent", "used_percent", "utilization", "percent"],
                        )
                    {
                        let ms =
                            full_unix_ms(w, &["resetsAt", "reset_at", "resetTime"]).or_else(|| {
                                full_num(w, "resetInSec")
                                    .map(|s| now_unix_ms() + (s * 1000.0) as i64)
                            });
                        let candidate = (used, label.to_owned(), ms);
                        if best.as_ref().is_none_or(|b| candidate.0 >= b.0) {
                            best = Some(candidate);
                        }
                        break;
                    }
                }
            }
            let (used, label, ms) = best?;
            out.used = used;
            out.label = label;
            out.reset_unix_ms = ms;
            out.reset_text = full_reset_from_ms(ms);
            out.plan = full_plan_label(
                full_str(&root, "plan")
                    .or_else(|| full_str(&root, "plan_type"))
                    .or_else(|| full_str(&root, "tier")),
            );
            out.limit_reached = full_bool(&root, "limit_reached")
                || full_bool(&root, "limitReached")
                || used >= 1.0;
        }
        "aiusage_copilot" | "copilot" => {
            let premium = full_obj(&root, "quota_snapshots")
                .and_then(|q| full_obj(q, "premium_interactions"));
            let used = premium
                .and_then(|p| full_num(p, "percent_remaining"))
                .map(|rem| ((100.0 - rem) / 100.0).clamp(0.0, 1.0) as f32)?;
            out.used = used;
            out.label = loc::t("AiUsageMonthly");
            let ms = parse_reset_ms(full_str(&root, "quota_reset_date_utc").as_deref());
            out.reset_unix_ms = ms;
            out.reset_text = full_reset_from_ms(ms);
            out.plan = full_plan_label(full_str(&root, "copilot_plan"));
            out.limit_reached = used >= 1.0;
        }
        "aiusage_deepseek" | "deepseek" => {
            let infos = full_arr(&root, "balance_infos")?;
            let info = infos.first()?;
            let balance = full_num(info, "total_balance")
                .or_else(|| full_str(info, "total_balance")?.parse::<f64>().ok())?;
            let currency = full_str(info, "currency").unwrap_or_else(|| "USD".to_owned());
            out.used = if balance <= 0.0 { 1.0 } else { 0.0 };
            out.label = loc::t("AiUsageBalance");
            out.credits = Some(balance);
            out.plan = Some(format!("{balance:.2} {currency}"));
            out.reset_text = "—".to_owned();
            out.limit_reached = !full_bool(&root, "is_available") || balance <= 0.0;
        }
        "aiusage_openrouter" | "openrouter" => {
            let data = full_obj(&root, "data")?;
            let total = full_num(data, "total_credits")?;
            let used_credits = full_num(data, "total_usage").unwrap_or(0.0);
            let remaining = (total - used_credits).max(0.0);
            out.used = if total > 0.0 {
                (used_credits / total).clamp(0.0, 1.0) as f32
            } else {
                0.0
            };
            out.label = loc::t("AiUsageCreditsLabel");
            out.credits = Some(remaining);
            out.plan = Some(format!("${remaining:.2}"));
            out.reset_text = "—".to_owned();
            out.limit_reached = total > 0.0 && remaining <= 0.0;
        }
        "aiusage_cursor" | "cursor" => {
            let usage = full_obj(&root, "planUsage").unwrap_or(&root);
            let total = full_pct(usage, &["totalPercentUsed", "used_percent", "used"])?;
            out.used = total;
            out.label = "30d".to_owned();
            let start = full_unix_ms(&root, &["billingCycleStart"]);
            let end = full_unix_ms(&root, &["billingCycleEnd"]);
            out.reset_unix_ms = end;
            out.reset_text = full_reset_from_ms(end);
            let seconds = match (start, end) {
                (Some(s), Some(e)) if e > s => Some((e - s) / 1000),
                _ => None,
            };
            let period_hint = full_str(&root, "includedUsagePeriod").or_else(|| {
                full_obj(&root, "planInfo").and_then(|p| full_str(p, "includedUsagePeriod"))
            });
            let weekly = period_hint.as_deref() == Some("INCLUDED_USAGE_PERIOD_WEEKLY")
                || (period_hint.as_deref() != Some("INCLUDED_USAGE_PERIOD_MONTHLY")
                    && seconds.is_some_and(|s| (5 * 86_400..=9 * 86_400).contains(&s)));
            if weekly {
                out.label = "7d".to_owned();
            }
            let auto = full_num(usage, "autoPercentUsed");
            let api = full_num(usage, "apiPercentUsed");
            if api.is_some_and(|v| v > 0.05)
                || auto.is_some_and(|a| (a - (total as f64 * 100.0)).abs() >= 0.5)
            {
                if let Some(a) = auto {
                    out.breakdown.push(AiUsageShare {
                        name: "Auto".to_owned(),
                        used: (a / 100.0).clamp(0.0, 1.0) as f32,
                    });
                }
                if let Some(a) = api {
                    out.breakdown.push(AiUsageShare {
                        name: "API".to_owned(),
                        used: (a / 100.0).clamp(0.0, 1.0) as f32,
                    });
                }
            }
            out.plan = full_plan_label(full_str(&root, "planName"));
            out.limit_reached = total >= 1.0 || full_bool(&root, "limitReached");
        }
        _ => {
            let used = full_pct(
                &root,
                &["used", "usedPercent", "utilization", "used_percent"],
            )?;
            out.used = used;
            out.label = "5h".to_owned();
            out.limit_reached = used >= 1.0 || full_bool(&root, "limit_reached");
        }
    }
    Some(out)
}

fn now_unix_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

fn parse_reset_ms(text: Option<&str>) -> Option<i64> {
    let text = text?;
    if let Ok(ms) = text.trim().parse::<i64>() {
        return full_unix_ms(&serde_json::json!({ "v": ms }), &["v"]);
    }
    // ISO-8601 (`2026-11-25T16:30:00Z`) -> unix milliseconds (seconds precision is enough for UI).
    let date = text.get(0..10)?;
    let mut parts = date.split('-');
    let y: i64 = parts.next()?.parse().ok()?;
    let m: i64 = parts.next()?.parse().ok()?;
    let d: i64 = parts.next()?.parse().ok()?;
    let time = text.get(11..16).unwrap_or("00:00");
    let mut tp = time.split(':');
    let hh: i64 = tp.next()?.parse().ok()?;
    let mm: i64 = tp.next()?.parse().ok()?;
    let days = opentaskbar_core::days_from_civil(y, m, d);
    Some((days * 86_400 + hh * 3600 + mm * 60) * 1000)
}

/// Retries a full-detail request with exponential backoff and honours `Retry-After` on 429/5xx.
/// Network errors and 5xx responses are retried up to three times; the final response (including
/// its `Retry-After`) is returned so the flyout can show the rate-limit note.
fn winhttp_json_with_retry(
    method: &str,
    host: &str,
    path: &str,
    headers: &[(&str, &str)],
    body: Option<&str>,
) -> Option<(u32, String, Option<u32>)> {
    let mut last = None;
    for attempt in 0..3u32 {
        last = winhttp_json(method, host, path, headers, body);
        // `Retry-After` only *sets the delay*; it never turns a successful response into a retry.
        let retryable = last
            .as_ref()
            .is_none_or(|(status, _, _)| is_retryable_status(*status));
        if !retryable || attempt == 2 {
            break;
        }
        let delay = last
            .as_ref()
            .and_then(|(_, _, retry)| *retry)
            .map_or(500u64 * (1u64 << attempt), |secs| u64::from(secs) * 1000)
            .min(15_000);
        std::thread::sleep(std::time::Duration::from_millis(delay));
    }
    last
}

/// Synchronous full-detail fetch for one AI provider (WS-C `ai_fetch_full(id)`).
///
/// Reads credentials from the same DPAPI/CLI stores as `fetch_ai_snapshot`, performs the
/// provider request and returns a fully parsed `AiFullDetail` (never panics; errors are
/// carried in `AiFullDetail::error`).
pub fn ai_fetch_full(id: &str) -> AiFullDetail {
    let provider = id.to_ascii_lowercase();
    let mut out = AiFullDetail {
        provider_id: id.to_owned(),
        label: "5h".to_owned(),
        reset_text: "—".to_owned(),
        ..Default::default()
    };

    if provider == "aiusage_cursor" || provider == "cursor" {
        let Some(token) = read_provider_token("aiusage_cursor").map(|(t, _)| t) else {
            out.error = Some("sign_in".to_owned());
            return out;
        };
        let auth = format!("Bearer {token}");
        let headers = [
            ("Authorization", auth.as_str()),
            ("Connect-Protocol-Version", "1"),
            ("Content-Type", "application/json"),
        ];
        let Some((status, body, retry)) = winhttp_json_with_retry(
            "POST",
            "api2.cursor.sh",
            "/aiserver.v1.DashboardService/GetCurrentPeriodUsage",
            &headers,
            Some("{}"),
        ) else {
            out.error = Some("load".to_owned());
            return out;
        };
        out.retry_after_secs = retry;
        if status == 401 || status == 403 {
            out.error = Some("sign_in".to_owned());
            return out;
        }
        if status == 429 {
            out.error = Some("rate_limited".to_owned());
            return out;
        }
        if status != 200 {
            out.error = Some("load".to_owned());
            return out;
        }
        let plan_body = winhttp_json(
            "POST",
            "api2.cursor.sh",
            "/aiserver.v1.DashboardService/GetPlanInfo",
            &headers,
            Some("{}"),
        )
        .filter(|(st, _, _)| *st == 200)
        .map(|(_, b, _)| b);
        let merged = if let Some(plan) = plan_body {
            let mut root: serde_json::Value = serde_json::from_str(&body).unwrap_or_default();
            if let Ok(pv) = serde_json::from_str::<serde_json::Value>(&plan)
                && let Some(obj) = root.as_object_mut()
            {
                for (k, v) in pv.as_object().cloned().unwrap_or_default() {
                    obj.entry(k).or_insert(v);
                }
            }
            serde_json::to_string(&root).unwrap_or(body)
        } else {
            body
        };
        return parse_full_detail(&merged, "aiusage_cursor").unwrap_or(AiFullDetail {
            provider_id: id.to_owned(),
            error: Some("load".to_owned()),
            ..out
        });
    }

    let (host, path, extra_headers): (&str, &str, Vec<(&str, String)>) = match provider.as_str() {
        "aiusage_claude" | "claude" => (
            "api.anthropic.com",
            "/api/oauth/usage",
            vec![("anthropic-beta", "oauth-2025-04-20".to_owned())],
        ),
        "aiusage_codex" | "codex" => ("chatgpt.com", "/backend-api/wham/usage", Vec::new()),
        "aiusage_opencode" | "opencode" => ("opencode.ai", "/zen/go/v1/usage", Vec::new()),
        "aiusage_copilot" | "copilot" => (
            "api.github.com",
            "/copilot_internal/user",
            vec![("Accept", "application/vnd.github+json".to_owned())],
        ),
        "aiusage_deepseek" | "deepseek" => ("api.deepseek.com", "/user/balance", Vec::new()),
        "aiusage_openrouter" | "openrouter" => ("openrouter.ai", "/api/v1/credits", Vec::new()),
        _ => {
            out.error = Some("unknown_provider".to_owned());
            return out;
        }
    };
    let Some((token, account)) = read_provider_token(&provider) else {
        out.error = Some("sign_in".to_owned());
        return out;
    };
    let auth = format!("Bearer {token}");
    let account = account.filter(|a| !a.is_empty());
    let mut headers: Vec<(&str, &str)> = vec![("Authorization", &auth)];
    for (k, v) in &extra_headers {
        headers.push((k, v.as_str()));
    }
    if let Some(acct) = &account {
        headers.push(("ChatGPT-Account-Id", acct.as_str()));
    }
    let Some((status, body, retry)) = winhttp_json_with_retry("GET", host, path, &headers, None)
    else {
        out.error = Some("load".to_owned());
        return out;
    };
    out.retry_after_secs = retry;
    if status == 401 || status == 403 {
        out.error = Some("sign_in".to_owned());
        return out;
    }
    if status == 429 {
        out.error = Some("rate_limited".to_owned());
        return out;
    }
    if status != 200 {
        out.error = Some("load".to_owned());
        return out;
    }
    parse_full_detail(&body, &provider).unwrap_or(AiFullDetail {
        provider_id: id.to_owned(),
        error: Some("load".to_owned()),
        ..out
    })
}

/// Runs a live connection test for `provider_id`, returning `(true, summary)` on success or
/// `(false, error_reason)` on failure.
pub fn test_ai_connection(provider_id: &str) -> (bool, String) {
    let Some(canonical) = canonical_ai_provider_id(provider_id) else {
        return (false, "Unknown AI provider".to_owned());
    };
    let meta = ai_provider_meta(canonical);
    let probe = ai_auth_probe(canonical);
    let detail = ai_fetch_full(canonical);
    if let Some(err) = &detail.error {
        let reason = match err.as_str() {
            "sign_in" => {
                if !probe.has_token {
                    let hint = meta.map_or(probe.env_var_hint, |m| m.signin_hint);
                    format!("{} ({hint})", loc::t("AiUsageAccountSignInRequired"))
                } else {
                    format!("{} (401/403)", loc::t("AiUsageAccountSignInRequired"))
                }
            }
            "rate_limited" => match detail.retry_after_secs {
                Some(secs) => format!(
                    "{} ({})",
                    loc::t("ErrorAiUsageRateLimited"),
                    loc::t_fmt("AiUsageRetryAfter", &[&secs.to_string()])
                ),
                None => loc::t("ErrorAiUsageRateLimited"),
            },
            "unknown_provider" => "Unknown AI provider".to_owned(),
            _ => loc::t("ErrorAiUsageLoad"),
        };
        return (false, reason);
    }

    let pct = (detail.used * 100.0).round() as u32;
    let pct_str = loc::t_fmt("OpacityPercent", &[&pct.to_string()]);
    let mut parts = vec![loc::t("AiUsageAccountConnected")];
    if let Some(plan) = detail.plan.as_deref().filter(|p| !p.is_empty()) {
        parts.push(plan.to_owned());
    }
    if matches!(canonical, "aiusage_deepseek" | "aiusage_openrouter") {
        if detail.plan.is_none()
            && let Some(credits) = detail.credits
        {
            parts.push(format!("${credits:.2}"));
        }
    } else if !detail.label.is_empty() {
        parts.push(format!("{} {pct_str}", detail.label));
    } else {
        parts.push(pct_str);
    }
    (true, parts.join(" · "))
}

// ============================================================================================
// GPU meter (`SystemSensors` GPU Engine counter + WMI thermal zone).
// ============================================================================================

/// Parses the numeric value from a PowerShell `Get-Counter` `\GPU Engine(*)\Utilization Percentage`
/// sample (multiple instances are summed and clamped).
pub fn parse_gpu_usage_counter(output: &str) -> Option<u32> {
    let mut total = 0.0f64;
    let mut saw = false;
    for line in output.lines() {
        let line = line.trim();
        if !line.to_ascii_lowercase().contains("utilization percentage") {
            continue;
        }
        let token = line
            .split_whitespace()
            .rev()
            .find(|t| t.parse::<f64>().is_ok())?;
        total += token.parse::<f64>().ok()?;
        saw = true;
    }
    if !saw && output.trim().parse::<f64>().is_ok() {
        total = output.trim().parse::<f64>().ok()?;
        saw = true;
    }
    saw.then(|| (total.round() as u32).min(100))
}

/// Parses a WMI `MSAcpi_ThermalZoneTemperature` `CurrentTemperature` value (tenths of Kelvin)
/// into Celsius.
pub fn parse_gpu_temperature_kelvin(raw: &str) -> Option<u32> {
    let tenths = raw.trim().parse::<f64>().ok()?;
    let c = tenths / 10.0 - 273.15;
    (c.is_finite() && (0.0..=125.0).contains(&c)).then(|| c.round() as u32)
}

/// Cached `(utilisation %, optional temperature °C)`, its timestamp, and the busiest GPU engine name.
type GpuSample = (Option<(u32, Option<u32>)>, Option<Instant>, Option<String>);

static GPU_SAMPLE: std::sync::Mutex<GpuSample> = std::sync::Mutex::new((None, None, None));
static GPU_SAMPLE_IN_FLIGHT: std::sync::atomic::AtomicBool =
    std::sync::atomic::AtomicBool::new(false);

/// Non-blocking sample of GPU utilisation (%) and temperature (°C) using native PDH + WDDM/NVML/ADL.
///
/// Returns the most recent cached sample immediately so dock startup and UI ticks never stall on
/// performance-counter or driver queries; refreshes happen on a background thread at most every 2 s.
pub fn sample_gpu_temp() -> Option<(u32, Option<u32>)> {
    let (cached, is_stale) = match GPU_SAMPLE.lock() {
        Ok(g) => (g.0, g.1.is_none_or(|at| at.elapsed().as_secs() >= 2)),
        Err(_) => (None, false),
    };
    if is_stale && !GPU_SAMPLE_IN_FLIGHT.swap(true, std::sync::atomic::Ordering::AcqRel) {
        let spawned = std::thread::Builder::new()
            .name("opentaskbar-gpu".to_owned())
            .spawn(|| {
                let pdh = sample_gpu_pdh();
                let usage = pdh
                    .as_ref()
                    .map(|(r, _)| (r * 100.0).round().clamp(0.0, 100.0) as u32);
                let engine = pdh.map(|(_, name)| name).filter(|name| !name.is_empty());
                let temp = sample_gpu_temperature()
                    .filter(|c| c.is_finite() && *c > 0.0 && *c < 150.0)
                    .map(|c| c.round() as u32);
                let result = if usage.is_some() || temp.is_some() {
                    Some((usage.unwrap_or(0), temp))
                } else {
                    None
                };
                if let Ok(mut g) = GPU_SAMPLE.lock() {
                    *g = (result, Some(Instant::now()), engine.or_else(|| g.2.clone()));
                }
                GPU_SAMPLE_IN_FLIGHT.store(false, std::sync::atomic::Ordering::Release);
            })
            .is_ok();
        if !spawned {
            GPU_SAMPLE_IN_FLIGHT.store(false, std::sync::atomic::Ordering::Release);
        }
    }
    cached
}

// ============================================================================================
// Weather forecast & media seek (WS-C `weather_forecast()` / `media_seek(secs)`).
// ============================================================================================

/// One forecast day: `(day label, condition, high/low)` — matches `WeatherCache::forecast_days`.
pub type WeatherDay = (String, String, String);

/// Parses Open-Meteo `daily` arrays into `(weekday, condition, "Hi° / Lo°")` rows.
pub fn parse_weather_forecast(root: &serde_json::Value) -> Vec<WeatherDay> {
    let mut rows = Vec::new();
    let Some(daily) = root.get("daily") else {
        return rows;
    };
    let times = daily.get("time").and_then(|v| v.as_array());
    let maxs = daily.get("temperature_2m_max").and_then(|v| v.as_array());
    let mins = daily.get("temperature_2m_min").and_then(|v| v.as_array());
    let codes = daily.get("weather_code").and_then(|v| v.as_array());
    if let (Some(ts), Some(mxs), Some(mns), Some(cds)) = (times, maxs, mins, codes) {
        let n = ts.len().min(mxs.len()).min(mns.len()).min(cds.len());
        for i in 1..n.min(4) {
            let date_str = ts[i].as_str().unwrap_or("");
            let label = if i == 1 {
                loc::t("WeatherTomorrow")
            } else if let Some((y, rest)) = date_str.split_once('-')
                && let Some((m, d)) = rest.split_once('-')
                && let (Ok(yi), Ok(mi), Ok(di)) =
                    (y.parse::<i32>(), m.parse::<u32>(), d.parse::<u32>())
            {
                let wd = weekday_of(yi, mi, di) as usize;
                SHORT_DAYS.get(wd).copied().unwrap_or(date_str).to_owned()
            } else {
                date_str.to_owned()
            };
            let (cond, _) = forecast_describe_wmo(cds[i].as_i64().unwrap_or(0));
            let hi = mxs[i].as_f64().unwrap_or(0.0).round() as i32;
            let lo = mns[i].as_f64().unwrap_or(0.0).round() as i32;
            rows.push((label, cond.to_owned(), format!("{hi}° / {lo}°")));
        }
    }
    rows.truncate(3);
    rows
}

/// Seeks the active SMTC session to `secs` from the start of the current track.
pub fn media_seek(secs: u64) -> bool {
    use windows::Media::Control::GlobalSystemMediaTransportControlsSessionManager;
    let manager = match GlobalSystemMediaTransportControlsSessionManager::RequestAsync()
        .and_then(|op| op.join())
    {
        Ok(m) => m,
        Err(_) => return false,
    };
    let session = match manager.GetCurrentSession() {
        Ok(s) => s,
        Err(_) => return false,
    };
    let ticks = (secs.min(u32::MAX as u64) as i64).saturating_mul(10_000_000);
    session
        .TryChangePlaybackPositionAsync(ticks)
        .and_then(|op| op.join())
        .unwrap_or(false)
}

#[cfg(test)]
mod full_detail_tests {
    use super::*;

    #[test]
    fn claude_full_detail_has_spend_breakdown_and_reset() {
        let json = r#"{
          "five_hour":{"utilization":0.42,"resets_at":"2026-11-26T16:30:00Z"},
          "seven_day":{"utilization":81.0,"resets_at":"2026-12-01T00:00:00Z"},
          "seven_day_breakdown":{"rows":[{"display_name":"Opus","percent_of_limit":0.31},{"display_name":"Haiku","percent_of_limit":0.004}]},
          "spend":{"enabled":true,"used":{"amount_minor":1234,"exponent":2,"currency":"USD"},"limit":{"amount_minor":5000,"exponent":2,"currency":"USD"},"percent":{"value":0.25}}
        }"#;
        let d = parse_full_detail(json, "aiusage_claude").unwrap();
        assert_eq!(d.used, 0.81);
        assert_eq!(d.label, "7d");
        assert!(d.reset_unix_ms.is_some());
        assert_eq!(d.breakdown.len(), 1);
        assert_eq!(d.breakdown[0].name, "Opus");
        let s = d.spend.unwrap();
        assert_eq!(s.used, 12.34);
        assert_eq!(s.limit, Some(50.0));
    }

    #[test]
    fn cursor_full_detail_limit_and_breakdown() {
        let json = r#"{
          "planUsage":{"totalPercentUsed":100.0,"autoPercentUsed":82.0,"apiPercentUsed":18.0},
          "billingCycleStart":1764000000000,"billingCycleEnd":1766582400000,
          "planName":"pro_plan","includedUsagePeriod":"INCLUDED_USAGE_PERIOD_WEEKLY"
        }"#;
        let d = parse_full_detail(json, "aiusage_cursor").unwrap();
        assert_eq!(d.used, 1.0);
        assert!(d.limit_reached);
        assert_eq!(d.label, "7d");
        assert_eq!(d.breakdown.len(), 2);
        assert_eq!(d.plan.as_deref(), Some("pro_plan"));
    }

    #[test]
    fn unknown_provider_is_rejected_and_codex_pool_parsing() {
        assert!(parse_full_detail(r#"{"used_percent":10}"#, "unknown_provider").is_some());
        let cx = r#"{"rate_limit":{"primary_window":{"used_percent":37.5,"reset_at":1764180000}},"credits":{"unlimited":true}}"#;
        let d = parse_full_detail(cx, "aiusage_codex").unwrap();
        assert!((d.used - 0.375).abs() < 0.001);
        assert!(d.unlimited_credits);
    }

    #[test]
    fn retry_after_is_parsed_from_http_header_value() {
        // `Retry-After` is surfaced through winhttp_json; the parser must tolerate numeric strings.
        let root = serde_json::json!({"retry_after":"12"});
        let n = full_num(&root, "retry_after").unwrap() as u32;
        assert_eq!(n, 12);
        assert_eq!(parse_retry_after_header(" 12 "), Some(12));
        assert_eq!(parse_retry_after_header("0"), None);
        assert_eq!(
            parse_retry_after_header(" Wed, 21 Oct 2015 07:28:00 GMT "),
            None
        );
    }

    #[test]
    fn only_rate_limit_and_server_errors_are_retryable() {
        for status in [200, 204, 301, 400, 401, 403, 404, 418] {
            assert!(!is_retryable_status(status), "{status} must not retry");
        }
        for status in [429, 500, 502, 503, 504, 599] {
            assert!(is_retryable_status(status), "{status} must retry");
        }
    }

    #[test]
    fn widget_secrets_are_scoped_to_the_owning_slug() {
        assert_eq!(
            widget_scoped_secret_name("widget:usd-try:token", "usd-try").as_deref(),
            Some("widget:usd-try:token")
        );
        assert_eq!(
            widget_scoped_secret_name("WIDGET:USD-TRY:token", "usd-try").as_deref(),
            Some("widget:usd-try:token")
        );
        for rejected in [
            "api-key:deepseek",
            "widget:other-widget:token",
            "widget:usd-try",
            "widget:usd-try:",
            "xwidget:usd-try:token",
            "widget::token",
        ] {
            assert_eq!(
                widget_scoped_secret_name(rejected, "usd-try"),
                None,
                "{rejected} must be rejected"
            );
        }
        assert_eq!(
            widget_scoped_secret_name("widget:x:key", &"k".repeat(129)),
            None
        );
        assert_eq!(
            widget_scoped_secret_name(&format!("widget:x:{}", "k".repeat(129)), "x"),
            None
        );
    }

    #[test]
    fn gpu_counter_and_temperature_parsers() {
        let out = "GPU Engine(pid123_0)\\(utilization percentage) : 12.5\nGPU Engine(pid123_1)\\(utilization percentage) : 7.5\n";
        // Counter names use `Utilization Percentage` capitalization in Windows; test both.
        assert!(parse_gpu_usage_counter(out).is_some() || parse_gpu_usage_counter("21").is_some());
        assert_eq!(parse_gpu_usage_counter("21"), Some(21));
        assert_eq!(parse_gpu_temperature_kelvin(" 3231.5 "), Some(50));
        assert_eq!(parse_gpu_temperature_kelvin("0"), None);
    }

    #[test]
    fn weather_forecast_parser_returns_three_days() {
        let root = serde_json::json!({
          "daily": {
            "time": ["2026-11-26","2026-11-27","2026-11-28","2026-11-29"],
            "temperature_2m_max": [10.0,11.0,12.0,13.0],
            "temperature_2m_min": [4.0,5.0,6.0,7.0],
            "weather_code": [0,61,3,95]
          }
        });
        let days = parse_weather_forecast(&root);
        assert_eq!(days.len(), 3);
        assert_eq!(days[0].1, "Rain");
        assert!(days[0].2.contains("11°"));
    }

    #[test]
    fn plan_label_and_reset_helpers() {
        assert_eq!(
            full_plan_label(Some("default_claude_max_20x".to_owned())).as_deref(),
            Some("Max 20x")
        );
        assert_eq!(civil_from_days(0), (1, 1));
        assert!(parse_reset_ms(Some("2026-11-26T16:30:00Z")).is_some());
    }
}

#[cfg(test)]
mod sweep_parity_tests {
    use super::*;
    use opentaskbar_core::settings::{ClockDateFormat, ClockDateStyle, TemperatureUnit};

    fn clock() -> LocalClockInfo {
        LocalClockInfo {
            time_hm: "09:05".to_owned(),
            hour: 9,
            minute: 5,
            second: 7,
            short_date: "26 Nov".to_owned(),
            weekday_short: "Thu".to_owned(),
            full_date: "Thursday, 26 November".to_owned(),
            tz_label: "UTC+3".to_owned(),
            tz_name: "Turkey Standard Time".to_owned(),
            first_day_of_week: 1,
            year: 2026,
            month: 11,
            day: 26,
        }
    }

    #[test]
    fn clock_formats_every_date_style_and_layout() {
        let mut settings = DockSettings {
            clock_24_hour: true,
            clock_show_seconds: true,
            date_style: ClockDateStyle::NumericDate,
            ..Default::default()
        };
        for (format, expected) in [
            (ClockDateFormat::DdMmYyyy, "26/11/2026"),
            (ClockDateFormat::DdMmYy, "26/11/26"),
            (ClockDateFormat::MmDdYyyy, "11/26/2026"),
            (ClockDateFormat::YyyyMmDd, "2026-11-26"),
            (ClockDateFormat::DdMmYyyyDots, "26.11.2026"),
        ] {
            settings.date_format = format;
            let (time, date) = format_clock(&clock(), &settings);
            assert_eq!(time, "09:05:07");
            assert_eq!(date, expected);
        }
        settings.clock_24_hour = false;
        settings.clock_show_seconds = false;
        assert_eq!(format_clock(&clock(), &settings).0, "9:05 AM");
        settings.date_style = ClockDateStyle::Hidden;
        assert!(format_clock(&clock(), &settings).1.is_empty());
    }

    #[test]
    fn weather_forecast_labels_tomorrow_and_formats_fahrenheit() {
        let root = serde_json::json!({
            "daily": {
                "time": ["2026-11-26","2026-11-27","2026-11-28","2026-11-29"],
                "temperature_2m_max": [10.0,11.0,12.0,13.0],
                "temperature_2m_min": [4.0,5.0,6.0,7.0],
                "weather_code": [0,0,3,0]
            }
        });
        let days = parse_weather_forecast(&root);
        assert_eq!(days[0].0, loc::t("WeatherTomorrow"));
        assert_eq!(
            format_temperature(10.0, TemperatureUnit::Fahrenheit),
            "50°F"
        );
        let cache = WeatherCache {
            wind_kmh_val: Some(10.0),
            ..Default::default()
        };
        assert!(
            cache
                .formatted_wind(TemperatureUnit::Fahrenheit)
                .contains("mph")
        );
        assert!(
            cache
                .formatted_wind(TemperatureUnit::Celsius)
                .contains("km/h")
        );
    }

    #[test]
    fn storage_uses_972_8_gib_tb_cutoff() {
        let drive = DriveSpace {
            letter: 'C',
            free_gb: 500.0,
            total_gb: 972.7,
        };
        assert!(drive.format_capacity_pair().contains("GB"));
        let drive = DriveSpace {
            letter: 'D',
            free_gb: 500.0,
            total_gb: 972.8,
        };
        assert!(drive.format_capacity_pair().contains("TB"));
    }

    #[test]
    fn night_light_round_trip_preserves_explicit_usable() {
        let base = encode_night_light_toggle(&[], true).unwrap();
        let state = decode_night_light_state(&base).unwrap();
        assert!(state.on && state.usable);
        let modified = state.modified;
        let next = encode_night_light_toggle(&base, false).unwrap();
        let next_state = decode_night_light_state(&next).unwrap();
        assert!(!next_state.on);
        assert!(
            next_state.modified > modified,
            "timestamp must advance monotonically"
        );
        // An unrecognized blob is never treated as Night Light state.
        assert_eq!(decode_night_light_on(b"not a CloudStore blob"), None);
    }

    #[test]
    fn ai_reset_countdown_handles_epoch_seconds() {
        assert_eq!(countdown_from_unix(0, 100), Some(loc::t("Expired")));
        assert_eq!(countdown_from_unix(3700, 100).as_deref(), Some("1h 0m"));
    }

    #[test]
    fn day_progress_respects_leap_year_and_first_day_of_week() {
        // 2024 is a leap year (366 days). Dec 31 noon = (365 + 0.5) / 366.
        let (day, _, year_leap, left) = day_progress_for(2024, 12, 31, 2, 12, 0, 1);
        assert!((day - 0.5).abs() < 0.001);
        assert!((year_leap - 365.5 / 366.0).abs() < 0.001);
        assert_eq!(left, 12);
        // 2025 is non-leap (365 days). Mar 1 midnight = (31 + 28) / 365.
        let (_, _, year_non_leap, _) = day_progress_for(2025, 3, 1, 6, 0, 0, 1);
        assert!((year_non_leap - 59.0 / 365.0).abs() < 0.001);
        // Sunday (0) on a Sunday-first locale is day 0 of week; on Monday-first it is day 6.
        let (_, week_sun, _, _) = day_progress_for(2026, 11, 29, 0, 0, 0, 0);
        let (_, week_mon, _, _) = day_progress_for(2026, 11, 29, 0, 0, 0, 1);
        assert!((week_sun - 0.0).abs() < 0.001);
        assert!((week_mon - 6.0 / 7.0).abs() < 0.001);
    }

    #[test]
    fn numeric_system_date_is_numeric_not_text_month() {
        let settings = DockSettings {
            date_style: ClockDateStyle::NumericDate,
            date_format: ClockDateFormat::System,
            ..Default::default()
        };
        let (_, date) = format_clock(&clock(), &settings);
        assert!(
            date.contains("2026"),
            "system numeric date must contain year: {date}"
        );
        assert!(
            !date.contains("Nov"),
            "system numeric date must not use text month: {date}"
        );
    }

    #[test]
    fn weather_kind_classifies_five_states() {
        assert_eq!(weather_kind_from_wmo(0), WeatherKind::Clear);
        assert_eq!(weather_kind_from_wmo(2), WeatherKind::Cloudy);
        assert_eq!(weather_kind_from_wmo(61), WeatherKind::Rain);
        assert_eq!(weather_kind_from_wmo(73), WeatherKind::Snow);
        assert_eq!(weather_kind_from_wmo(95), WeatherKind::Storm);
    }

    #[test]
    fn weather_geocode_strips_country_and_attaches_country_code() {
        let (tr_path, tr_hint) = build_weather_geocode_path("İstanbul, Turkey", "tr");
        assert!(tr_path.contains("name=%C4%B0stanbul"));
        assert!(!tr_path.contains("Turkey"));
        assert!(tr_path.ends_with("&countryCode=TR"));
        assert_eq!(tr_hint.as_deref(), Some("Turkey"));

        let (us_path, _) = build_weather_geocode_path("Paris, Texas, United States", "en");
        assert!(us_path.contains("name=Paris"));
        assert!(us_path.ends_with("&countryCode=US"));

        let (iso_path, _) = build_weather_geocode_path("Vienna, AT", "de");
        assert!(iso_path.contains("name=Vienna"));
        assert!(iso_path.ends_with("&countryCode=AT"));

        let (other_path, other_hint) = build_weather_geocode_path("Paris, Austria", "en");
        assert!(other_path.contains("count=10"));
        assert!(!other_path.contains("countryCode="));
        assert_eq!(other_hint.as_deref(), Some("Austria"));

        let (plain_path, plain_hint) = build_weather_geocode_path("Istanbul", "tr");
        assert!(plain_path.contains("count=1"));
        assert!(!plain_path.contains("countryCode="));
        assert_eq!(plain_hint, None);
    }

    #[test]
    fn weather_geocode_selection_prefers_matching_country_hint() {
        let payload = r#"{"results":[
            {"name":"Paris","latitude":48.85,"longitude":2.35,"country_code":"FR","country":"France"},
            {"name":"Paris","latitude":33.66,"longitude":-95.55,"country_code":"US","country":"United States","admin1":"Texas"}
        ]}"#;
        let (name, lat, lon) =
            select_geocode_result(payload, "Paris", Some("United States")).unwrap();
        assert_eq!(name, "Paris");
        assert!((lat - 33.66).abs() < 0.01);
        assert!((lon - (-95.55)).abs() < 0.01);

        let (fallback_name, fb_lat, _) =
            select_geocode_result(payload, "Paris", Some("Unknown")).unwrap();
        assert_eq!(fallback_name, "Paris");
        assert!((fb_lat - 48.85).abs() < 0.01);
    }

    #[test]
    fn ai_auth_probe_covers_all_seven_providers_with_secret_keys_and_env_hints() {
        let expected = [
            ("aiusage_claude", "api-key:claude", "ANTHROPIC_API_KEY"),
            ("aiusage_codex", "api-key:codex", "CODEX_API_KEY"),
            (
                "aiusage_opencode",
                "api-key:opencode",
                "OPENCODE_GO_API_KEY",
            ),
            ("aiusage_copilot", "api-key:copilot", "GITHUB_TOKEN"),
            ("aiusage_deepseek", "api-key:deepseek", "DEEPSEEK_API_KEY"),
            (
                "aiusage_openrouter",
                "api-key:openrouter",
                "OPENROUTER_API_KEY",
            ),
            ("aiusage_cursor", "api-key:cursor", "CURSOR_API_KEY"),
        ];
        for (id, secret_key, env_hint) in expected {
            let probe = ai_auth_probe(id);
            assert_eq!(probe.secret_key, secret_key, "secret key mismatch for {id}");
            assert_eq!(probe.env_var_hint, env_hint, "env hint mismatch for {id}");
            assert_eq!(ai_secret_key_for_provider(id), Some(secret_key));
            assert_eq!(
                ai_provider_detected(id),
                probe.has_token || probe.cli_installed,
                "ai_provider_detected must delegate to ai_auth_probe for {id}"
            );
        }
        let unknown = ai_auth_probe("unknown_ai_provider");
        assert_eq!(unknown.source, AiAuthSource::None);
        assert!(!unknown.has_token);
        assert!(!unknown.cli_installed);
        assert!(unknown.secret_key.is_empty());
        assert!(ai_secret_key_for_provider("unknown_ai_provider").is_none());
    }
}
