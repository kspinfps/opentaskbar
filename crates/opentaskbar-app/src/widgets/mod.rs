//! Unified 26-widget registry, runtime state, and reference parity `DockFlyout` builders.
//!
//! Categories (matching `DockWidgetService.cs`):
//! - `Time & Weather`    (5): `clock`, `weather`, `dayprogress`, `stopwatch`, `countdown`
//! - `System & Hardware` (5): `stats`, `netspeed`, `battery`, `bluetoothbattery`, `storage`
//! - `AI Quotas`         (8): `aiusage_claude`, `aiusage_codex`,
//!   `aiusage_opencode`, `aiusage_copilot`, `aiusage_deepseek`,
//!   `aiusage_openrouter`, `aiusage_cursor`, `agents`
//! - `Media & Controls`  (4): `media`, `audio`, `quicksettings`, `keyboard`
//! - `Productivity`      (4): `focus`, `clipboard`, `shelf`, `todo`

// Public API is re-exported for the dock and UI layers. Most of those re-exports are used by
// other crates / test harnesses only, so silence `dead_code` at the re-export sites below.

pub mod agents;
pub mod ai_flyout;
pub mod catalog_ops;
pub mod clipboard;
pub mod custom_widgets;
pub mod diagnostics;
pub mod focus_break;
pub mod hover;
pub mod lyrics;
pub mod media_ext;
pub mod registry;
pub mod shelf;
pub mod speedtest;
pub mod telemetry;
pub mod todo;
pub mod uia_raw;
pub mod widget_icons;
pub mod widget_snapshot;

use std::collections::{HashMap, HashSet, VecDeque};
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};
use std::sync::{Arc, LazyLock, Mutex};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use opentaskbar_core::i18n as loc;
use opentaskbar_core::settings::{
    AiUsageRing, AppTheme, AutoHideMode, ClockDateLayout, DockSettings, WidgetDensityMode,
};
use windows_sys::Win32::UI::Input::KeyboardAndMouse::{
    VK_MEDIA_NEXT_TRACK, VK_MEDIA_PLAY_PAUSE, VK_MEDIA_PREV_TRACK,
};

use crate::design::{self, Rgb};
use crate::dock::dcomp::{SystemStatColumn, WidgetChipSpec, WidgetIcon};
use crate::platform::{pins, toast, wireless_battery};
use crate::ui::flyout::{self, BarTone, FlyoutBlock, FlyoutSpec, LiveEvent, LiveOutcome};
use crate::ui::menu::BorderStyle;
use telemetry::{BluetoothBatteryDevice, BluetoothDeviceKind};

#[allow(unused_imports)]
pub use catalog_ops::{
    catalog, compact_id, installed_catalog_widget_ids, is_widget_installed, set_setting,
    setting_bool, setting_f64, setting_i64, setting_str, setting_string_list, setting_value,
    setting_value_from,
};
#[allow(unused_imports)]
pub use focus_break::{
    FocusEvent, FocusPhase, FocusSession, SharedFocusSession, format_mm_secs, play_break_chime,
};
#[allow(unused_imports)]
pub use hover::{HOVER_INTENT_MS, HoverChange, HoverPhase, SUPPRESS_MS, WidgetHoverCoordinator};
#[allow(unused_imports)]
pub use media_ext::{
    AudioAppSession, MediaDetails, MediaTimeline, TimelineSample, confirm_seek,
    format_time as format_media_time, get_details as get_media_details, playing_glyph, seek_ratio,
    volume_from_wheel, volume_from_wheel_step,
};
#[allow(unused_imports)]
pub use registry::{
    WIDGET_ALIASES, WIDGET_CATEGORIES, WidgetDensity, WidgetDescriptor, WidgetLayout, WidgetOption,
    active_widgets, bluetooth_width_dips, canonical_widget_id, catalog_widgets, category_i18n_key,
    clock_width_dips, descriptor_for, descriptor_has_settings, hardware_available,
    stats_width_dips, storage_width_dips, weather_width_dips, widget_descriptor, widget_has_flyout,
    widget_has_inline_settings, widget_layout, widget_name,
};
#[allow(unused_imports)]
pub use speedtest::{STREAM_COUNT, SpeedTestController, SpeedTestResult, SpeedTestState};
#[allow(unused_imports)]
pub use telemetry::{
    WeatherKind, active_keyboard_code, active_keyboard_hkl, adjust_audio_volume, day_progress_for,
    day_progress_metrics, empty_recycle_bin, get_audio_state, is_recycle_bin_non_empty,
    sample_battery, sample_drives, set_audio_volume, set_audio_volume_preserving_mute,
    toggle_audio_mute, weekday_short_name,
};
#[allow(unused_imports)]
pub use widget_snapshot::{POLL_TTL, WidgetPollSnapshot};

/// Locale-aware percentage formatting (`"{0}%"` in English, `"%{0}"` in Turkish, etc.).
pub fn format_percent(n: u32) -> String {
    loc::t_fmt("OpacityPercent", &[&n.to_string()])
}

/// Returns `true` when `weekday` is empty or already present inside `date` (e.g.
/// `"Fri, Nov 27"` + `"Fri"`), preventing duplicate weekday rendering on the Clock chip.
pub fn clock_weekday_is_redundant(date: &str, weekday: &str) -> bool {
    let wd = weekday.trim();
    if wd.is_empty() {
        return true;
    }
    date.to_lowercase().contains(&wd.to_lowercase())
}

/// Locale-aware percentage label for a `0.0..=1.0` day progress ratio.
pub fn day_progress_percent_text(ratio: f32) -> String {
    format_percent((ratio.clamp(0.0, 1.0) * 100.0).round() as u32)
}

/// CPU/RAM/GPU threshold coloring with a widget-specific base color.
fn threshold_color(pal: &design::Palette, pct: u32, base: Rgb) -> Rgb {
    if pct >= 90 {
        pal.danger
    } else if pct >= 75 {
        pal.warning
    } else {
        base
    }
}

pub fn ai_provider_color(id: &str) -> Rgb {
    ai_provider_color_for_theme(id, true)
}

/// Theme-aware brand color for AI usage chips: keeps vibrant hues in Dark mode and uses
/// high-contrast darkened shades in Light mode so logos never wash out on light glass.
pub fn ai_provider_color_for_theme(id: &str, is_dark: bool) -> Rgb {
    if is_dark {
        match id {
            "aiusage_claude" => (0xd9, 0x77, 0x57),
            "aiusage_codex" => (0x8d, 0x99, 0xff),
            "aiusage_opencode" => (0x58, 0xd8, 0xb2),
            "aiusage_copilot" => (0x9f, 0x9f, 0xf2),
            "aiusage_deepseek" => (0x4d, 0x6b, 0xfe),
            "aiusage_openrouter" => (0xb7, 0x9b, 0xf8),
            "aiusage_cursor" => (0xf3, 0xf4, 0xf6),
            _ => (0xaa, 0xb2, 0xc0),
        }
    } else {
        match id {
            "aiusage_claude" => (0xc2, 0x5a, 0x38),
            "aiusage_codex" => (0x4f, 0x46, 0xe5),
            "aiusage_opencode" => (0x05, 0x96, 0x69),
            "aiusage_copilot" => (0x4f, 0x46, 0xe5),
            "aiusage_deepseek" => (0x25, 0x63, 0xeb),
            "aiusage_openrouter" => (0x7c, 0x3a, 0xed),
            "aiusage_cursor" => (0x18, 0x18, 0x1b),
            _ => (0x47, 0x55, 0x69),
        }
    }
}

/// Visual rendering hints shared between `WidgetRuntime::chip_spec` and `dock/dcomp.rs`
/// so enhanced chip visuals (Media art/progress/playing, Focus ring/phase, Countdown bar,
/// Storage drive bars, Clock Below layout, OpenRouter credit bar) stay in sync with cache keys
/// without breaking the public `WidgetChipSpec` enum layout consumed by `ui/settings.rs`.
#[derive(Clone, Debug, Default, PartialEq)]
pub(crate) struct MediaChipHint {
    pub playing: bool,
    pub progress: f32,
    pub cover_path: Option<String>,
    pub cover_pixels: Option<Arc<(u32, u32, Vec<u8>)>>,
    pub has_media: bool,
}

static CHIP_CLOCK_BELOW: AtomicBool = AtomicBool::new(false);
static CHIP_FOCUS_IS_BREAK: AtomicBool = AtomicBool::new(false);
static CHIP_FOCUS_RATIO_BITS: AtomicU32 = AtomicU32::new(0);
static CHIP_COUNTDOWN_RATIO_BITS: AtomicU32 = AtomicU32::new(0);
static CHIP_OPENROUTER_RATIO_BITS: AtomicU32 = AtomicU32::new(u32::MAX);
static CHIP_MEDIA_HINT: LazyLock<Mutex<MediaChipHint>> =
    LazyLock::new(|| Mutex::new(MediaChipHint::default()));
static CHIP_STORAGE_RATIOS: LazyLock<Mutex<HashMap<String, f32>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));

pub(crate) fn set_chip_clock_below(below: bool) {
    CHIP_CLOCK_BELOW.store(below, Ordering::Relaxed);
}

pub(crate) fn chip_clock_below() -> bool {
    CHIP_CLOCK_BELOW.load(Ordering::Relaxed)
}

pub(crate) fn set_chip_focus_hint(is_break: bool, ratio: f32) {
    CHIP_FOCUS_IS_BREAK.store(is_break, Ordering::Relaxed);
    CHIP_FOCUS_RATIO_BITS.store(ratio.clamp(0.0, 1.0).to_bits(), Ordering::Relaxed);
}

pub(crate) fn chip_focus_hint() -> (bool, f32) {
    let is_break = CHIP_FOCUS_IS_BREAK.load(Ordering::Relaxed);
    let ratio = f32::from_bits(CHIP_FOCUS_RATIO_BITS.load(Ordering::Relaxed)).clamp(0.0, 1.0);
    (is_break, ratio)
}

pub(crate) fn set_chip_countdown_ratio(ratio: f32) {
    CHIP_COUNTDOWN_RATIO_BITS.store(ratio.clamp(0.0, 1.0).to_bits(), Ordering::Relaxed);
}

pub(crate) fn chip_countdown_ratio() -> f32 {
    f32::from_bits(CHIP_COUNTDOWN_RATIO_BITS.load(Ordering::Relaxed)).clamp(0.0, 1.0)
}

pub(crate) fn set_chip_openrouter_ratio(ratio: Option<f32>) {
    let bits = ratio.map_or(u32::MAX, |r| r.clamp(0.0, 1.0).to_bits());
    CHIP_OPENROUTER_RATIO_BITS.store(bits, Ordering::Relaxed);
}

pub(crate) fn chip_openrouter_ratio() -> Option<f32> {
    let bits = CHIP_OPENROUTER_RATIO_BITS.load(Ordering::Relaxed);
    if bits == u32::MAX {
        None
    } else {
        Some(f32::from_bits(bits).clamp(0.0, 1.0))
    }
}

pub(crate) fn set_chip_media_hint(hint: MediaChipHint) {
    if let Ok(mut guard) = CHIP_MEDIA_HINT.lock() {
        *guard = hint;
    }
}

pub(crate) fn chip_media_hint() -> MediaChipHint {
    CHIP_MEDIA_HINT
        .lock()
        .ok()
        .map(|g| g.clone())
        .unwrap_or_default()
}

pub(crate) fn set_chip_storage_ratios(ratios: HashMap<String, f32>) {
    if let Ok(mut guard) = CHIP_STORAGE_RATIOS.lock() {
        *guard = ratios;
    }
}

pub(crate) fn chip_storage_ratio(letter: &str) -> Option<f32> {
    CHIP_STORAGE_RATIOS
        .lock()
        .ok()
        .and_then(|g| g.get(letter).copied())
}

/// Result of a direct left-click on a dock widget.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DirectClickOutcome {
    Handled,
    SettingsChanged,
    OpenFlyout,
}

#[derive(Debug, Clone, PartialEq)]
pub struct IslandDetailRow {
    pub label: String,
    pub value: String,
    pub progress: Option<f32>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct IslandDetailAction {
    pub id: String,
    pub label: String,
    pub primary: bool,
}

#[derive(Debug, Clone, PartialEq)]
pub struct IslandDetailSpec {
    pub widget_id: String,
    pub title: String,
    pub headline: String,
    pub subtitle: String,
    pub rows: Vec<IslandDetailRow>,
    pub actions: Vec<IslandDetailAction>,
    pub has_full_flyout: bool,
}

struct AudioDetailCache {
    checked: Instant,
    out_name: Option<String>,
    mic: Option<(u32, bool)>,
}

struct KeyboardLayoutsCache {
    checked: Instant,
    layouts: Vec<(isize, String, String)>,
}

struct TooltipTextCache {
    id: &'static str,
    sub: Option<u8>,
    /// Settings that change tooltip text (city/unit/device names) must invalidate the cache
    /// immediately, not after the 500 ms TTL.
    settings_key: String,
    text: String,
    checked: Instant,
}

fn tooltip_settings_key(settings: &DockSettings) -> String {
    format!(
        "{}|{:?}|{}|{}",
        settings.weather_city,
        weather_unit(settings),
        setting_str(settings, "WeatherUnit").unwrap_or_default(),
        setting_str(settings, "AudioOutputDevice").unwrap_or_default()
    )
}

pub struct WidgetRuntime {
    stats: telemetry::SystemStatsSampler,
    net: telemetry::NetSpeedSampler,
    cpu_pct: u32,
    ram_pct: u32,
    stats_uptime: String,
    stats_top: Arc<Mutex<Vec<(String, u64)>>>,
    last_stats_top: Option<Instant>,
    /// Coalesces background top-process sampling: the live flyout may not spawn a new thread
    /// on every 1 Hz refresh tick.
    stats_top_in_flight: Arc<AtomicBool>,
    /// GPU utilization percent (`SystemStatsWidget` GPU meter).
    gpu_pct: u32,
    /// GPU die temperature in °C (reference GPU temperature meter).
    gpu_temp_c: Option<u32>,
    /// Engine name from the 1 Hz GPU probe, cached so flyouts never sample GPU counters.
    gpu_engine_name: String,
    /// Consecutive failed GPU probes; reference hides the GPU meters after 5 misses.
    gpu_misses: u32,
    /// Consecutive successful GPU probes; reference requires 2 samples before showing meters.
    gpu_samples: u32,
    /// Thermal state 0 = normal, 1 = warning (>=70 °C), 2 = critical (>=85 °C).
    gpu_thermal_state: u8,
    /// `SystemStatsWidget`: 5 samples to enter a hotter band, 2 to leave it.
    gpu_thermal_enter: u8,
    gpu_thermal_exit: u8,
    /// Number of active stat columns on the Dock chip (1..=4), synced on `tick` for dynamic width.
    stats_col_count: u8,
    /// Cached dynamic widths updated on `tick` so `widget_width_dips(id)` reflects live settings.
    clock_width: f32,
    weather_width: f32,
    storage_width: f32,
    bt_width: f32,
    /// Rolling 10-sample throughput history (bytes/sec) for the NetSpeed sparkline chip.
    net_down_history: VecDeque<u64>,
    net_up_history: VecDeque<u64>,
    audio_vol: u32,
    audio_muted: bool,
    /// Accumulated wheel delta for 120-delta = 5% volume steps.
    volume_wheel_remainder: i32,
    stopwatch_running: bool,
    stopwatch_secs: u64,
    /// Dedicated fractional accumulator for the stopwatch so centiseconds freeze while paused.
    stopwatch_debt_secs: f64,
    stopwatch_laps: Vec<(u64, u32)>,
    /// Centiseconds elapsed within the current second (mm:ss.cc display).
    stopwatch_cs: u32,
    countdown_running: bool,
    countdown_total_secs: u64,
    countdown_remaining_secs: u64,
    /// Shared focus/break session (chip, flyout and island see the same state).
    focus: SharedFocusSession,
    /// Legacy mirror kept in sync for callers that read these directly.
    focus_running: bool,
    focus_preset_mins: u64,
    focus_remaining_secs: u64,
    focus_completed: u32,
    weather: Arc<Mutex<telemetry::WeatherCache>>,
    weather_last_city: String,
    weather_fail_count: u32,
    weather_retry_at: Option<Instant>,
    bt_battery: Arc<Mutex<telemetry::BluetoothBatteryCache>>,
    /// Monotonic refresh generation; every worker result is tagged with the generation it
    /// started with so a late result never replaces a newer snapshot.
    wb_generation: Arc<AtomicU64>,
    /// Single atomic refresh queue holding in-flight state plus any pending `(settings, force)`
    /// follow-up request so mid-poll setting changes never run against a stale clone.
    wb_queue: Arc<Mutex<WirelessRefreshQueue>>,
    /// Active wakeup handle while the `bluetoothbattery` flyout is open, so background
    /// refresh completion updates the open flyout immediately via `LiveEvent::Update`.
    wb_live_update: Arc<Mutex<Option<flyout::LiveUpdateHandle>>>,
    last_wb_poll: Option<Instant>,
    notified_low_bt: HashSet<String>,
    ai_usage: Arc<Mutex<telemetry::AiUsageCache>>,
    /// Full `ai_fetch_full` payloads keyed by provider id. Never rendered as `default()`: the
    /// flyout reads the last successfully fetched spend / breakdown / reset / plan / limit data.
    ai_full: Arc<Mutex<HashMap<String, (Instant, telemetry::AiFullDetail)>>>,
    ai_full_in_flight: Arc<Mutex<HashSet<String>>>,
    media_session: Arc<Mutex<telemetry::MediaSessionSnapshot>>,
    /// Seek-anchored media clock; rejects stale SMTC echoes after a confirmed seek.
    media_timeline: MediaTimeline,
    /// Monotonic epoch backing every `MediaTimeline` timestamp.
    timeline_epoch: Instant,
    /// 1 s TTL cache for disk / keyboard reads made by chip renderers on every VSync.
    pub(crate) poll_snapshot: WidgetPollSnapshot,
    /// Multi-stream NetSpeed test controller.
    speedtest: SpeedTestController,
    speedtest_started_at: Option<Instant>,
    /// Shared hover intent coordinator (WS-D calls the pub API from its mouse handlers).
    pub hover: WidgetHoverCoordinator,
    night_light_on: bool,
    /// Real Wi-Fi / Bluetooth radio states (C4 `platform::radio`).
    wifi_on: bool,
    bluetooth_on: bool,
    radio_loading: bool,
    radio_unavailable: bool,
    /// Pending target of a radio toggle, reconciled against the real state on the next poll.
    wifi_target: Option<bool>,
    bluetooth_target: Option<bool>,
    radio_denied: bool,
    radio_failed: bool,
    battery_anim_start: Instant,
    battery_charging: bool,
    keyboard_override: Option<(String, Instant)>,
    /// Sticky external foreground target: our own dock/flyout can take focus between scroll
    /// events, but the layout change must still go to the app the user was typing in.
    keyboard_target_hwnd: Option<usize>,
    last_tick: Instant,
    /// Fractional seconds carried between ticks: a 16 ms animation tick must not consume
    /// the 1 s delta that the 2 s widget timer is about to deliver.
    tick_debt_secs: f64,
    /// Sensor polling is capped at 1 Hz; hover/click refreshes only rebuild chip specs.
    last_poll: Option<Instant>,
    last_night_light_check: Option<Instant>,
    tooltip_text: Option<TooltipTextCache>,
    /// 5 s TTL cache for `ai_provider_detected` so Dynamic Island detail frames never
    /// repeat PATH scans or DPAPI/filesystem credential probes at VSync frequency.
    ai_detected_cache: HashMap<&'static str, (Instant, bool)>,
    /// 1 s TTL cache for `(default_output_device_name, mic_state)` so `island_detail_spec("audio")`
    /// never initializes COM + `MMDeviceEnumerator` on every frame.
    audio_detail_cache: Option<AudioDetailCache>,
    /// 2 s TTL cache for `installed_keyboard_layouts` so `island_detail_spec("keyboard")`
    /// never repeats `GetLocaleInfoW` queries at VSync frequency.
    keyboard_layouts_cache: Option<KeyboardLayoutsCache>,
    /// Lazy N2 File Shelf state (`None` unless enabled or files added).
    shelf: Option<Box<shelf::FileShelfState>>,
    /// Lazy N3 Clipboard History store (`None` unless enabled).
    clipboard: Option<Box<clipboard::ClipboardStore>>,
    /// Lazy N8 To-Do & iCal state (`None` unless `todo` enabled or `.ics` URL configured).
    todo: Option<Box<todo::TodoState>>,
    /// Lazy N7 AI Coding Agent Monitor (`None` unless enabled; dropping stops its pipe thread).
    agents: Option<Box<agents::AgentMonitorState>>,
    /// Lazy N6 Synced Lyrics cache (`None` unless `media_lyrics_enabled` is true).
    lyrics: Option<Box<lyrics::LyricsCache>>,
}

impl Default for WidgetRuntime {
    fn default() -> Self {
        Self {
            stats: telemetry::SystemStatsSampler::default(),
            net: telemetry::NetSpeedSampler::default(),
            cpu_pct: 0,
            ram_pct: 0,
            stats_uptime: telemetry::system_uptime_string(),
            stats_top: Arc::new(Mutex::new(Vec::new())),
            last_stats_top: None,
            stats_top_in_flight: Arc::new(AtomicBool::new(false)),
            gpu_pct: 0,
            gpu_temp_c: None,
            gpu_engine_name: String::new(),
            gpu_misses: 0,
            gpu_samples: 0,
            gpu_thermal_state: 0,
            gpu_thermal_enter: 0,
            gpu_thermal_exit: 0,
            stats_col_count: 2,
            clock_width: 148.0,
            weather_width: 196.0,
            storage_width: 124.0,
            bt_width: 78.0,
            net_down_history: VecDeque::from([0u64; 10]),
            net_up_history: VecDeque::from([0u64; 10]),
            audio_vol: 50,
            audio_muted: false,
            volume_wheel_remainder: 0,
            stopwatch_running: false,
            stopwatch_secs: 0,
            stopwatch_debt_secs: 0.0,
            stopwatch_laps: Vec::new(),
            stopwatch_cs: 0,
            countdown_running: false,
            countdown_total_secs: 15 * 60,
            countdown_remaining_secs: 15 * 60,
            focus: SharedFocusSession::default(),
            focus_running: false,
            focus_preset_mins: 25,
            focus_remaining_secs: 25 * 60,
            focus_completed: 0,
            weather: Arc::new(Mutex::new(telemetry::WeatherCache::default())),
            weather_last_city: String::new(),
            weather_fail_count: 0,
            weather_retry_at: None,
            bt_battery: Arc::new(Mutex::new(telemetry::BluetoothBatteryCache::default())),
            wb_generation: Arc::new(AtomicU64::new(0)),
            wb_queue: Arc::new(Mutex::new(WirelessRefreshQueue::default())),
            wb_live_update: Arc::new(Mutex::new(None)),
            last_wb_poll: None,
            notified_low_bt: HashSet::new(),
            ai_usage: Arc::new(Mutex::new(telemetry::AiUsageCache::default())),
            ai_full: Arc::new(Mutex::new(HashMap::new())),
            ai_full_in_flight: Arc::new(Mutex::new(HashSet::new())),
            media_session: Arc::new(Mutex::new(telemetry::MediaSessionSnapshot::default())),
            media_timeline: MediaTimeline::default(),
            timeline_epoch: Instant::now(),
            poll_snapshot: WidgetPollSnapshot::default(),
            speedtest: SpeedTestController::new(),
            speedtest_started_at: None,
            hover: WidgetHoverCoordinator::new(),
            night_light_on: telemetry::read_night_light().unwrap_or(false),
            wifi_on: false,
            bluetooth_on: false,
            radio_loading: true,
            radio_unavailable: false,
            wifi_target: None,
            bluetooth_target: None,
            radio_denied: false,
            radio_failed: false,
            battery_anim_start: Instant::now(),
            battery_charging: false,
            keyboard_override: None,
            keyboard_target_hwnd: None,
            last_tick: Instant::now(),
            tick_debt_secs: 0.0,
            last_poll: None,
            last_night_light_check: None,
            tooltip_text: None,
            ai_detected_cache: HashMap::new(),
            audio_detail_cache: None,
            keyboard_layouts_cache: None,
            shelf: None,
            clipboard: None,
            todo: None,
            agents: None,
            lyrics: None,
        }
    }
}

/// `StopwatchWidget` centisecond display: `mm:ss.cc`.
fn format_mm_ss_cs(total_secs: u64, centis: u32) -> String {
    format!(
        "{:02}:{:02}.{:02}",
        total_secs / 60,
        total_secs % 60,
        centis % 100
    )
}

/// GPU availability hysteresis (`SystemStatsWidget`): 5 consecutive misses hide the meters
/// and 2 consecutive samples bring them back. Returns `None` while the meters must stay hidden.
/// `BluetoothBatteryWidget` low-alert threshold cycle (0 disables the alert).
fn bt_next_threshold(current: i64) -> i64 {
    const STEPS: [i64; 4] = [0, 10, 20, 30];
    STEPS
        .iter()
        .find(|&&value| value > current)
        .copied()
        .unwrap_or(STEPS[0])
}

/// The single wireless poll cadence (the same 15..=300 s range the settings validator and
/// the platform aggregator use).
fn wireless_poll_secs(settings: &DockSettings) -> u64 {
    setting_i64(
        settings,
        "WirelessBatteryPollSeconds",
        i64::from(settings.wireless_battery_poll_seconds),
    )
    .clamp(15, 300) as u64
}

/// `IgnoredWirelessDevices` match by stable physical id (or legacy plain name).
fn bt_is_ignored(ignored: &[String], device: &BluetoothBatteryDevice) -> bool {
    ignored
        .iter()
        .any(|entry| device.matches_ignore_entry(entry))
}

fn notify_wireless_flyout(live_update: &Mutex<Option<flyout::LiveUpdateHandle>>) {
    if let Ok(slot) = live_update.lock()
        && let Some(handle) = slot.as_ref()
    {
        handle.notify();
    }
}

/// Atomic queue coordinating the single background wireless battery worker.
///
/// Holding `in_flight` and `pending: Option<(DockSettings, bool)>` under one mutex prevents
/// both lost-wakeup races and stale `DockSettings` clones when Listen-Only, ignore, or alias
/// settings change mid-poll.
#[derive(Default)]
struct WirelessRefreshQueue {
    in_flight: bool,
    pending: Option<(DockSettings, bool)>,
}

/// Releases the wireless worker's `in_flight` claim and the cache `loading` flag on panic
/// unwind, and wakes any open `bluetoothbattery` live flyout when the worker exits.
struct WirelessRefreshGuard {
    queue: Arc<Mutex<WirelessRefreshQueue>>,
    cache: Arc<Mutex<telemetry::BluetoothBatteryCache>>,
    live_update: Arc<Mutex<Option<flyout::LiveUpdateHandle>>>,
    completed: bool,
}

impl Drop for WirelessRefreshGuard {
    fn drop(&mut self) {
        if !self.completed {
            let mut q = self
                .queue
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            q.in_flight = false;
            q.pending = None;
            if let Ok(mut cache) = self.cache.lock() {
                cache.loading = false;
            }
        }
        notify_wireless_flyout(&self.live_update);
    }
}

fn wireless_kind_to_bt(kind: wireless_battery::WirelessDeviceKind) -> BluetoothDeviceKind {
    match kind {
        wireless_battery::WirelessDeviceKind::Mouse => BluetoothDeviceKind::Mouse,
        wireless_battery::WirelessDeviceKind::Keyboard => BluetoothDeviceKind::Keyboard,
        wireless_battery::WirelessDeviceKind::Headphones => BluetoothDeviceKind::Headphones,
        wireless_battery::WirelessDeviceKind::Gamepad => BluetoothDeviceKind::Gamepad,
        wireless_battery::WirelessDeviceKind::Other => BluetoothDeviceKind::Generic,
    }
}

/// One aggregated reading -> one widget row, with level, spare level, sample time,
/// staleness and link state taken from the same sample. Unknown levels yield `None`: they
/// are never shown as 0%.
fn wireless_reading_to_row(
    reading: wireless_battery::WirelessReading,
) -> Option<BluetoothBatteryDevice> {
    let percent = u32::from(reading.level_percent_opt()?);
    Some(BluetoothBatteryDevice {
        kind: wireless_kind_to_bt(reading.device_kind),
        percent,
        source_label: reading.source.label().to_owned(),
        charging: reading.is_charging && reading.is_connected && !reading.stale,
        stale: reading.stale,
        connected: reading.is_connected && !reading.stale,
        sampled_at: Some(reading.sampled_at),
        secondary_level: reading
            .secondary_level
            .map(|spare| (spare * 100.0).round().clamp(0.0, 100.0) as u8),
        id: reading.id,
        name: reading.name,
    })
}

/// Returns `true` only when the PowerShell PnP fallback is permitted and needed: native
/// providers returned no visible rows, native Bluetooth did not observe candidates that were
/// merely hidden by `IgnoredWirelessDevices`, and `"bluetooth"` is not disabled.
fn should_run_powershell_fallback(
    settings: &DockSettings,
    rows_empty: bool,
    had_native_bluetooth: bool,
) -> bool {
    rows_empty && !had_native_bluetooth && wireless_battery::allows_bluetooth_fallback(settings)
}

/// Applies `IgnoredWirelessDevices` and `WirelessDeviceAliases` to PowerShell PnP fallback rows.
fn filter_powershell_fallback_rows(
    settings: &DockSettings,
    rows: Vec<BluetoothBatteryDevice>,
) -> Vec<BluetoothBatteryDevice> {
    let ignored = setting_string_list(settings, "IgnoredWirelessDevices");
    let agg_settings = wireless_battery::AggregatorSettings::from(settings);
    rows.into_iter()
        .filter(|d| {
            !bt_is_ignored(&ignored, d) && !agg_settings.is_ignored_id_or_name(&d.id, &d.name)
        })
        .map(|mut d| {
            d.name = agg_settings.display_name_id_or_name(&d.id, &d.name);
            d
        })
        .collect()
}

/// One refresh pass tagged with `generation`: native providers first, then the PowerShell PnP
/// fallback only when they reported nothing, native Bluetooth did not merely filter out ignored
/// devices, and Bluetooth is not disabled. A failed fallback keeps the previous rows but marks
/// them stale; nothing older than an applied generation can overwrite it.
fn run_wireless_refresh(
    settings: &DockSettings,
    force: bool,
    cache: &Arc<Mutex<telemetry::BluetoothBatteryCache>>,
    generation: u64,
) {
    let outcome = wireless_battery::read_all_with_settings_forced_outcome(settings, force);
    let rows: Vec<BluetoothBatteryDevice> = outcome
        .readings
        .into_iter()
        .filter_map(wireless_reading_to_row)
        .collect();
    let snapshot = if should_run_powershell_fallback(
        settings,
        rows.is_empty(),
        outcome.had_native_bluetooth,
    ) {
        telemetry::query_bluetooth_battery_powershell()
            .map(|ps_rows| filter_powershell_fallback_rows(settings, ps_rows))
    } else {
        Ok(rows)
    };
    if let Ok(mut guard) = cache.lock() {
        match snapshot {
            Ok(rows) => {
                telemetry::apply_bluetooth_snapshot(&mut guard, generation, rows);
            }
            Err(_) => {
                telemetry::apply_bluetooth_error(&mut guard, generation);
            }
        }
    }
}

/// `SystemStatsWidget` thermal bands: 70 °C warning, 85 °C critical.
fn thermal_band(temp_c: u32) -> u8 {
    if temp_c >= 85 {
        2
    } else if temp_c >= 70 {
        1
    } else {
        0
    }
}

fn gpu_metrics_visible(samples: u32, misses: u32) -> bool {
    misses < 5 && samples >= 2
}

fn action_row(id: u32, title: impl Into<String>, badge: Option<String>) -> FlyoutBlock {
    FlyoutBlock::ActionRow {
        id,
        title: title.into(),
        subtitle: None,
        badge,
        icon: None,
        destructive: false,
    }
}

/// Copies diagnostics text to the Windows clipboard without showing a console window.
fn copy_to_clipboard(text: &str) {
    use std::os::windows::process::CommandExt;
    let escaped = text.replace('\'', "''");
    let script = format!("Set-Clipboard -Value '{escaped}'");
    let _ = std::process::Command::new("powershell.exe")
        .args(["-NoProfile", "-NonInteractive", "-Command", &script])
        .creation_flags(0x0800_0000)
        .spawn();
}

/// `QuickSettingsWidget` radio state labels: `Loading`, `Unavailable`, `On`, `Off`.
fn radio_label(on: bool, loading: bool) -> String {
    if loading {
        loc::t("AiUsageStatusLoading")
    } else if on {
        loc::t("QuickOn")
    } else {
        loc::t("QuickOff")
    }
}

/// Applies a settings-controlled alias ID so rename/alias entries are resolved consistently.
fn resolved_widget_id(id: &str, settings: &DockSettings) -> String {
    let canonical = canonical_widget_id(id).to_owned();
    let aliases = setting_value(settings, "WidgetAliases");
    aliases
        .and_then(|v| v.as_object().cloned())
        .and_then(|m| {
            m.iter()
                .find(|(k, _)| k.eq_ignore_ascii_case(id))
                .and_then(|(_, v)| v.as_str())
                .map(str::to_owned)
        })
        .unwrap_or(canonical)
}

fn parse_ai_usage_ring(value: &serde_json::Value) -> Option<AiUsageRing> {
    if let Ok(ring) = serde_json::from_value::<AiUsageRing>(value.clone()) {
        return Some(ring);
    }
    let text = value.as_str()?.trim().to_ascii_lowercase();
    match text.as_str() {
        "highest" => Some(AiUsageRing::Highest),
        "session" | "5h" | "1h" => Some(AiUsageRing::Session),
        "weekly" | "7d" | "30d" | "monthly" => Some(AiUsageRing::Weekly),
        _ => None,
    }
}

/// Resolves the effective `AiUsageRing` for `provider_id`, checking optional per-provider
/// overrides in `settings.extra["AiUsageRingOverrides"][canonical_id]` before falling back
/// to `settings.ai_usage_ring`.
pub fn resolved_ai_usage_ring(settings: &DockSettings, provider_id: &str) -> AiUsageRing {
    let canonical_id = canonical_widget_id(provider_id);
    if let Some(overrides) = setting_value(settings, "AiUsageRingOverrides")
        && let Some(map) = overrides.as_object()
        && let Some(ring) = map
            .iter()
            .find(|(k, _)| {
                k.eq_ignore_ascii_case(canonical_id)
                    || canonical_widget_id(k).eq_ignore_ascii_case(canonical_id)
            })
            .and_then(|(_, v)| parse_ai_usage_ring(v))
    {
        return ring;
    }
    settings.ai_usage_ring
}

fn is_session_window_label(label: &str) -> bool {
    matches!(
        label.trim().to_ascii_lowercase().as_str(),
        "5h" | "1h" | "session"
    )
}

fn is_weekly_window_label(label: &str) -> bool {
    let lower = label.trim().to_ascii_lowercase();
    matches!(lower.as_str(), "7d" | "30d" | "weekly" | "monthly")
        || lower == loc::t("AiUsageMonthly").to_ascii_lowercase()
}

pub(crate) fn order_ai_windows(
    windows: &[telemetry::AiWindowData],
    ring: AiUsageRing,
) -> Vec<telemetry::AiWindowData> {
    let mut ordered = windows.to_vec();
    match ring {
        AiUsageRing::Highest => {
            ordered.sort_by(|a, b| b.used.total_cmp(&a.used));
        }
        AiUsageRing::Session => {
            ordered.sort_by_key(|w| usize::from(!is_session_window_label(&w.label)));
        }
        AiUsageRing::Weekly => {
            ordered.sort_by_key(|w| {
                if w.label.eq_ignore_ascii_case("7d") {
                    0usize
                } else if is_weekly_window_label(&w.label) {
                    1usize
                } else {
                    2usize
                }
            });
        }
    }
    ordered
}

/// The catalog's `WeatherUnit` override wins over the typed setting. The old flyout applied
/// this override while the chip/tooltip did not; both now read the same effective unit.
fn weather_unit(settings: &DockSettings) -> opentaskbar_core::settings::TemperatureUnit {
    use opentaskbar_core::settings::TemperatureUnit;
    match setting_str(settings, "WeatherUnit").as_deref() {
        Some(value)
            if value.eq_ignore_ascii_case("fahrenheit")
                || value.eq_ignore_ascii_case("f")
                || value == "°F" =>
        {
            TemperatureUnit::Fahrenheit
        }
        Some(_) => TemperatureUnit::Celsius,
        None => settings.weather_unit,
    }
}

/// One source for chip, flyout and tooltip temperature strings: `(temp, feels-like, today range)`.
fn weather_display(
    snap: &telemetry::WeatherCache,
    settings: &DockSettings,
) -> (String, String, String) {
    let unit = weather_unit(settings);
    let temp = snap.temp_c_val.map_or_else(
        || snap.temp_c.clone(),
        |c| telemetry::format_temperature(c, unit),
    );
    let feels = snap.feels_like_c_val.map_or_else(
        || snap.feels_like.clone(),
        |c| telemetry::format_temperature(c, unit),
    );
    let range = match (snap.today_hi_c, snap.today_lo_c) {
        (Some(hi), Some(lo)) => format!(
            "{} / {}",
            telemetry::format_temperature(hi, unit),
            telemetry::format_temperature(lo, unit)
        ),
        _ => snap.today_range.clone(),
    };
    (temp, feels, range)
}

/// Native Windows 10/11 `Segoe Fluent Icons` weather glyph.
fn weather_icon_glyph(condition: &str) -> u16 {
    match telemetry::weather_kind_from_condition(condition, false) {
        WeatherKind::Storm => 0xE945,  // LightningBolt
        WeatherKind::Snow => 0xE9CA,   // Frigid / Snowflake
        WeatherKind::Rain => 0xEB42,   // Drop / Rain
        WeatherKind::Cloudy => 0xE753, // Cloud
        WeatherKind::Clear => 0xE706,  // Brightness / Sun
    }
}

/// Semantic accent color for weather glyphs (warm amber sun, cool sky rain, frost snow,
/// slate cloud, amber storm) adapted for Dark and Light contrast.
fn weather_icon_color(condition: &str, is_dark: bool) -> Rgb {
    let pal = design::palette(is_dark);
    match telemetry::weather_kind_from_condition(condition, false) {
        WeatherKind::Storm => pal.warning,
        WeatherKind::Clear => {
            if is_dark {
                (0xf5, 0xb9, 0x52)
            } else {
                (0xd9, 0x77, 0x06)
            }
        }
        WeatherKind::Rain => {
            if is_dark {
                (0x60, 0xa5, 0xfa)
            } else {
                (0x25, 0x63, 0xeb)
            }
        }
        WeatherKind::Snow => {
            if is_dark {
                (0x93, 0xc5, 0xfd)
            } else {
                (0x02, 0x84, 0xc7)
            }
        }
        WeatherKind::Cloudy => {
            if is_dark {
                (0x94, 0xa3, 0xb8)
            } else {
                (0x47, 0x55, 0x69)
            }
        }
    }
}

/// `platform::core_audio::list_devices(false)` adapter. Assumed shape is `(id, display_name)`;
/// keeping the conversion in one place makes a signature change a one-line fix.
fn audio_output_devices() -> Vec<(String, String)> {
    crate::platform::core_audio::list_devices(false)
}

/// Capture endpoints for the Audio flyout's input device selector.
fn audio_input_devices() -> Vec<(String, String)> {
    crate::platform::core_audio::list_devices(true)
}

/// Maps a Quick Settings `IconBar` shortcut ID to its `ms-settings:` URI.
fn shortcut_uri(id: u32) -> &'static str {
    match id {
        100 => "ms-settings:network-wifi",
        101 => "ms-settings:bluetooth",
        102 => "ms-settings:nightlight",
        103 => "ms-settings:network-airplanemode",
        104 => "ms-settings:network-cellular",
        105 => "ms-settings:display",
        106 => "ms-settings:screenrotation",
        107 => "ms-settings:batterysaver",
        108 => "ms-settings:crossdevice",
        109 => "ms-settings:easeofaccess-closedcaptioning",
        110 => "ms-settings:quiethours",
        111 => "ms-settings:camera",
        112 => "ms-settings:easeofaccess-hearingaids",
        113 => "ms-settings:display-advanced",
        114 => "ms-settings:network-mobilehotspot",
        115 => "ms-settings:easeofaccess",
        116 => "ms-settings:taskbar",
        _ => "ms-settings:",
    }
}

/// Number of reference Control Center shortcut tiles in the Quick Settings flyout.
pub const QUICK_SETTINGS_SHORTCUT_COUNT: u32 = 17;

impl WidgetRuntime {
    /// Monotonic seconds on the media timeline clock.
    fn now_secs(&self) -> f64 {
        self.timeline_epoch.elapsed().as_secs_f64()
    }

    /// SMTC snapshot with the extrapolated playback position applied.
    fn media_snapshot(&self) -> telemetry::MediaSessionSnapshot {
        let mut snap = self
            .media_session
            .lock()
            .ok()
            .map(|guard| guard.clone())
            .unwrap_or_default();
        snap.position_secs =
            media_ext::displayed_position(&self.media_timeline, &snap, self.now_secs());
        snap
    }

    /// Animation-only tick. The battery wave phase is derived from `battery_anim_start` inside
    /// `chip_spec`, so this advances no sensor, no timer and leaves the snapshot untouched.
    pub fn animation_tick(&mut self, _settings: &DockSettings) -> bool {
        false
    }

    /// Advances timers with fractional-second accumulation. The stopwatch owns its own
    /// accumulator so paused centiseconds stay frozen instead of drifting with background ticks;
    /// countdown completion plays the native chime and shows a localized toast.
    fn advance_timers(&mut self, settings: &DockSettings, elapsed_secs: f64) {
        let clamped = elapsed_secs.clamp(0.0, 60.0);
        if self.stopwatch_running {
            self.stopwatch_debt_secs += clamped;
            let sw_whole = self.stopwatch_debt_secs.trunc() as u64;
            self.stopwatch_debt_secs -= sw_whole as f64;
            self.stopwatch_secs = self.stopwatch_secs.saturating_add(sw_whole);
            self.stopwatch_cs = ((self.stopwatch_debt_secs * 100.0).floor() as u32).min(99);
        }
        self.tick_debt_secs += clamped;
        let whole_secs = self.tick_debt_secs.trunc() as u64;
        self.tick_debt_secs -= whole_secs as f64;
        if whole_secs == 0 {
            return;
        }
        if self.countdown_running {
            let was_positive = self.countdown_remaining_secs > 0;
            self.countdown_remaining_secs =
                self.countdown_remaining_secs.saturating_sub(whole_secs);
            if self.countdown_remaining_secs == 0 {
                self.countdown_running = false;
                if was_positive {
                    play_break_chime();
                    let _ = toast::show_info_toast(&loc::t("CountdownFinished"));
                }
            }
        }
        // Shared focus/break session: crossing a phase boundary plays the Asterisk chime.
        if settings.is_widget_enabled("focus") {
            let event = self
                .focus
                .with_mut(|s| s.tick(whole_secs))
                .unwrap_or(FocusEvent::None);
            match event {
                FocusEvent::PhaseEnded(FocusPhase::Focus) | FocusEvent::CycleCompleted => {
                    play_break_chime()
                }
                _ => {}
            }
            self.sync_focus_mirrors();
        }
    }

    fn sync_focus_mirrors(&mut self) {
        let s = self.focus.snapshot();
        self.focus_running = s.running;
        self.focus_preset_mins = s.focus_minutes;
        self.focus_remaining_secs = s.remaining_secs;
        self.focus_completed = s.sessions_completed as u32;
    }

    /// Lazy subsystem count (0 when all 5 optional subsystems are disabled).
    pub(crate) fn lazy_subsystems_allocated(&self) -> usize {
        usize::from(self.shelf.is_some())
            + usize::from(self.clipboard.is_some())
            + usize::from(self.todo.is_some())
            + usize::from(self.agents.is_some())
            + usize::from(self.lyrics.is_some())
    }

    /// Lazily instantiates or drops optional subsystems based on `settings`, keeping idle RAM
    /// and background thread count at zero while disabled.
    fn sync_lazy_subsystems(&mut self, settings: &DockSettings) {
        if !settings.shelf_enabled() && !settings.is_widget_enabled("shelf") {
            self.shelf = None;
        } else if settings.is_widget_enabled("shelf") && self.shelf.is_none() {
            self.shelf = Some(Box::new(shelf::FileShelfState::load_default()));
        }

        if settings.clipboard_history_enabled() || settings.is_widget_enabled("clipboard") {
            let store = self
                .clipboard
                .get_or_insert_with(|| Box::new(clipboard::ClipboardStore::new()));
            store.poll(settings);
        } else {
            self.clipboard = None;
        }

        if settings.is_widget_enabled("todo") || !settings.ical_feed_url().trim().is_empty() {
            let todo = self
                .todo
                .get_or_insert_with(|| Box::new(todo::TodoState::load_default()));
            let interval = Duration::from_secs(u64::from(settings.ical_poll_minutes()) * 60);
            todo.poll_ical_url(settings.ical_feed_url().trim(), interval);
        } else {
            self.todo = None;
        }

        if settings.agent_monitor_enabled() || settings.is_widget_enabled("agents") {
            let agents = self
                .agents
                .get_or_insert_with(|| Box::new(agents::AgentMonitorState::start()));
            agents.poll_events();
        } else {
            self.agents = None;
        }

        if settings.media_lyrics_enabled() {
            let _ = self
                .lyrics
                .get_or_insert_with(|| Box::new(lyrics::LyricsCache::new()));
        } else if let Some(mut cache) = self.lyrics.take() {
            cache.clear();
        }
    }

    /// Adds dropped file paths onto the lazy File Shelf (no-op when shelf is disabled).
    pub fn add_shelf_files(
        &mut self,
        paths: &[std::path::PathBuf],
        settings: &DockSettings,
    ) -> usize {
        if paths.is_empty() || (!settings.shelf_enabled() && !settings.is_widget_enabled("shelf")) {
            return 0;
        }
        let shelf = self
            .shelf
            .get_or_insert_with(|| Box::new(shelf::FileShelfState::load_default()));
        let added = shelf.add_files(paths, settings.shelf_max_items());
        if added > 0 {
            let _ = shelf.save_default();
        }
        added
    }

    /// Highest-priority pending AI coding agent approval request, if monitoring is active.
    pub fn pending_agent_approval(&self) -> Option<agents::AgentApprovalRequest> {
        self.agents
            .as_ref()
            .and_then(|agents| agents.pending_approval().cloned())
    }

    /// Dispatches an approval decision (`"allow"`, `"deny"`, `"always"`) to the agent monitor.
    pub fn respond_agent_approval(&mut self, request_id: &str, decision: &str) -> bool {
        self.agents
            .as_mut()
            .is_some_and(|agents| agents.respond_approval(request_id, decision))
    }

    /// Polls for any due or snooze-expired local to-do reminder without allocating when disabled.
    pub fn poll_todo_reminder(&mut self, settings: &DockSettings) -> Option<todo::TodoTask> {
        if !settings.todo_reminders_enabled() {
            return None;
        }
        let todo = self.todo.as_mut()?;
        let now_unix = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs();
        let info = telemetry::local_clock_info();
        let now_hhmm = format!("{:02}:{:02}", info.hour, info.minute);
        todo.poll_due_reminder(now_unix, &now_hhmm, true)
    }

    /// Live 4-bar audio peak waveform for the Dynamic Island media pill.
    pub fn media_waveform_bars(&self, settings: &DockSettings) -> [u8; 4] {
        media_ext::sample_audio_waveform_bars(
            settings.media_waveform_enabled() && self.is_media_playing(),
        )
    }

    /// Current synced lyric line at the active media timeline position, if enabled and cached.
    pub fn current_lyric_line(&mut self, settings: &DockSettings) -> Option<String> {
        if !settings.media_lyrics_enabled() {
            if let Some(mut cache) = self.lyrics.take() {
                cache.clear();
            }
            return None;
        }
        let snap = self.media_snapshot();
        if snap.title.trim().is_empty() {
            return None;
        }
        let pos_ms = if self.media_timeline.duration() > 0.0 {
            (self.media_timeline.position_at(self.now_secs()) * 1000.0)
                .round()
                .max(0.0) as u64
        } else {
            snap.position_secs.saturating_mul(1000)
        };
        let duration_secs = (snap.duration_secs > 0).then_some(snap.duration_secs);
        let cache = self
            .lyrics
            .get_or_insert_with(|| Box::new(lyrics::LyricsCache::new()));
        let synced = cache.poll_for_track(true, &snap.title, &snap.artist, duration_secs)?;
        synced.line_at(pos_ms).map(|(_, line)| line.text.clone())
    }

    /// Cached `(device_name, percent)` pairs for Dynamic Island Bluetooth HUDs. Retained
    /// last-known rows are labelled as cached instead of being shown as live.
    pub fn wireless_battery_summary_pairs(&self) -> Vec<(String, u8)> {
        self.bt_battery
            .lock()
            .ok()
            .map(|cache| {
                cache
                    .devices
                    .iter()
                    .map(|dev| {
                        let name = if dev.is_live() {
                            dev.name.clone()
                        } else {
                            loc::t_fmt("WirelessBatteryStale", &[&dev.name])
                        };
                        (name, dev.percent.min(100) as u8)
                    })
                    .collect()
            })
            .unwrap_or_default()
    }

    /// Cached `(volume_pct, muted)` snapshot for Dynamic Island Volume HUDs.
    pub fn audio_volume_snapshot(&self) -> Option<(u8, bool)> {
        Some((self.audio_vol.min(100) as u8, self.audio_muted))
    }

    /// Whether an SMTC media session is currently playing.
    pub fn is_media_playing(&self) -> bool {
        self.media_session
            .lock()
            .ok()
            .is_some_and(|snap| snap.playing && !snap.title.is_empty())
    }

    pub fn tick(&mut self, settings: &DockSettings) {
        let now = Instant::now();
        let elapsed = now.duration_since(self.last_tick).as_secs_f64();
        self.last_tick = now;
        self.advance_timers(settings, elapsed);
        self.sync_lazy_subsystems(settings);

        // Keep dynamic settings-driven chip widths current even between 1 Hz sensor polls.
        self.clock_width = clock_width_dips(settings);
        let weather_compact = setting_bool(settings, "WeatherCompactMode", false)
            || opentaskbar_core::settings::resolve_widget_density(settings, "weather")
                == WidgetDensityMode::Compact;
        self.weather_width = weather_width_dips(weather_compact);
        let storage_compact = setting_bool(settings, "StorageCompactMode", false)
            || opentaskbar_core::settings::resolve_widget_density(settings, "storage")
                == WidgetDensityMode::Compact;
        self.storage_width =
            storage_width_dips(self.poll_snapshot.fixed_drives().len(), storage_compact);
        if settings.is_widget_enabled("focus") {
            self.focus.with_mut(|s| s.sync_from_settings(settings));
            self.sync_focus_mirrors();
        }

        // `refresh_widgets` also runs on hover/click; those calls must not re-sample COM,
        // registry, network and disk state. The 2 s widget timer still drives polling.
        if self
            .last_poll
            .is_some_and(|t| now.duration_since(t) < POLL_TTL)
        {
            return;
        }
        self.last_poll = Some(now);
        // Single cached read of the expensive chip inputs. Every renderer below (including the
        // 144 Hz animation path) reads this snapshot instead of touching the disk / locale APIs.
        self.poll_snapshot.refresh();
        self.storage_width =
            storage_width_dips(self.poll_snapshot.fixed_drives().len(), storage_compact);

        // User widgets are polled on the same 1 Hz gate; each sidecar applies its
        // own `pollSeconds` throttle and does its I/O on a worker thread.
        for widget in registry::catalog_widgets() {
            if custom_widgets::is_custom(widget.id) && settings.is_widget_enabled(widget.id) {
                custom_widgets::poll_widget(widget.id, settings);
            }
        }

        if settings.is_widget_enabled("stats") {
            let show_cpu = settings.stats_meters & 1 != 0;
            let show_ram = settings.stats_meters & 2 != 0;
            let show_gpu = settings.stats_meters & 16 != 0;
            let show_gpu_temp = settings.stats_meters & 32 != 0;
            (self.cpu_pct, self.ram_pct) = self.stats.sample();
            self.stats_uptime = telemetry::system_uptime_string();
            self.sample_top_processes_async(now);
            if (show_gpu || show_gpu_temp)
                && let Some((pct, temp)) = telemetry::sample_gpu_temp()
            {
                self.gpu_pct = pct;
                self.gpu_temp_c = temp;
                self.gpu_engine_name = telemetry::sample_gpu_engine_name().unwrap_or_default();
                self.stats.gpu_history.push(pct as f32 / 100.0);
                self.gpu_samples = self.gpu_samples.saturating_add(1);
                self.gpu_misses = 0;
            } else if show_gpu || show_gpu_temp {
                self.gpu_misses = self.gpu_misses.saturating_add(1);
                if self.gpu_misses >= 5 {
                    self.gpu_samples = 0;
                    self.gpu_temp_c = None;
                    self.gpu_pct = 0;
                    self.stats.gpu_history.push(0.0);
                }
            }
            self.update_gpu_thermal(self.gpu_temp_c);
            let gpu_ready = gpu_metrics_visible(self.gpu_samples, self.gpu_misses);
            let cols = u8::from(show_cpu)
                + u8::from(show_ram)
                + u8::from(show_gpu && gpu_ready)
                + u8::from(show_gpu_temp && self.gpu_temp_c.is_some());
            self.stats_col_count = cols.max(1);
        }
        if settings.is_widget_enabled("audio")
            || settings.is_widget_enabled("media")
            || settings.is_widget_enabled("quicksettings")
        {
            (self.audio_vol, self.audio_muted) = get_audio_state();
        }
        if settings.is_widget_enabled("media") {
            telemetry::refresh_media_session(&self.media_session, false);
            // Re-anchor the extrapolated clock from the latest SMTC fetch. A repeated sample
            // leaves the anchor alone; a confirmed seek survives stale echoes (WS-6 contract).
            if let Ok(guard) = self.media_session.lock() {
                let now = self.now_secs();
                let sample = media_ext::timeline_from_snapshot(&guard, self.timeline_epoch);
                let rate = if guard.playing { 1.0 } else { 0.0 };
                self.media_timeline.update(sample, rate, now, now);
            }
            if settings.media_lyrics_enabled() {
                let _ = self.current_lyric_line(settings);
            }
        }
        if settings.is_widget_enabled("netspeed") {
            let (rx, tx) = self.net.sample();
            if self.net_down_history.len() >= 10 {
                self.net_down_history.pop_front();
            }
            self.net_down_history.push_back(rx);
            if self.net_up_history.len() >= 10 {
                self.net_up_history.pop_front();
            }
            self.net_up_history.push_back(tx);
            self.poll_speedtest(now);
        }
        if settings.is_widget_enabled("weather") {
            if !self
                .weather_last_city
                .eq_ignore_ascii_case(&settings.weather_city)
            {
                self.weather_last_city = settings.weather_city.clone();
                self.weather_retry_at = None;
            }
            let can_fetch = self.weather_retry_at.is_none_or(|at| now >= at);
            if can_fetch {
                let had_data = self
                    .weather
                    .lock()
                    .ok()
                    .is_some_and(|g| !g.temp_c.is_empty());
                telemetry::refresh_weather(&self.weather, &settings.weather_city, false);
                self.weather_retry_at = if had_data {
                    Some(now + std::time::Duration::from_secs(30 * 60))
                } else {
                    // 2-minute retry while there is no usable data.
                    Some(now + std::time::Duration::from_secs(120))
                };
            }
        }
        if settings.is_widget_enabled("bluetoothbattery") {
            // Wireless HID/GATT probes can take up to PROVIDER_TIMEOUT (1.2 s), so keep polling
            // strictly off the UI thread. This tick is the single refresh clock: the worker
            // always performs a real poll (the platform aggregate reuse window is bypassed).
            let poll_secs = wireless_poll_secs(settings);
            if self
                .last_wb_poll
                .is_none_or(|t| now.duration_since(t) >= Duration::from_secs(poll_secs))
            {
                self.last_wb_poll = Some(now);
                self.refresh_bluetooth_devices(settings);
            }
            let ignored = setting_string_list(settings, "IgnoredWirelessDevices");
            let visible_count = self.bt_battery.lock().ok().map_or(0, |cache| {
                cache
                    .devices
                    .iter()
                    .filter(|d| !bt_is_ignored(&ignored, d))
                    .count()
            });
            self.bt_width = bluetooth_width_dips(visible_count);

            let threshold = setting_i64(
                settings,
                "WirelessBatteryLowThreshold",
                i64::from(settings.wireless_battery_low_threshold.max(0)),
            )
            .max(0) as u32;
            if threshold == 0 {
                self.notified_low_bt.clear();
            } else if let Ok(cache) = self.bt_battery.lock() {
                for dev in &cache.devices {
                    // Retained last-known (stale / disconnected) rows never raise a toast.
                    if bt_is_ignored(&ignored, dev) || !dev.is_live() {
                        continue;
                    }
                    if dev.percent <= threshold {
                        if self.notified_low_bt.insert(dev.id.clone()) {
                            let msg = loc::t_fmt(
                                "WirelessBatteryLowToast",
                                &[&dev.name, &dev.percent.to_string()],
                            );
                            let _ = toast::show_info_toast(&msg);
                        }
                    } else if dev.percent > threshold.saturating_add(5) {
                        self.notified_low_bt.remove(&dev.id);
                    }
                }
            }
        }
        if settings.is_widget_enabled("quicksettings")
            && self
                .last_night_light_check
                .is_none_or(|t| now.duration_since(t) >= Duration::from_secs(10))
        {
            self.last_night_light_check = Some(now);
            self.night_light_on = telemetry::read_night_light().unwrap_or(self.night_light_on);
            // C4: platform::radio::{state,set} — WinRT Radio control.
            self.wifi_on = crate::platform::radio::state("wifi");
            self.bluetooth_on = crate::platform::radio::state("bluetooth");
            // Reconcile a previous toggle: access denied has no refresh at all, a failed write
            // leaves a refresh but the state does not match the requested target (reference ChangeResult).
            if let Some(target) = self.wifi_target.take()
                && self.wifi_on != target
            {
                self.radio_failed = true;
                self.radio_denied = crate::platform::radio::last_refresh().is_none();
            }
            if let Some(target) = self.bluetooth_target.take()
                && self.bluetooth_on != target
            {
                self.radio_failed = true;
                self.radio_denied = crate::platform::radio::last_refresh().is_none();
            }
            self.radio_loading = false;
        }
        if settings.is_widget_enabled("battery") {
            let b = sample_battery();
            self.battery_charging = b.has_battery && b.charging;
        } else {
            self.battery_charging = false;
        }
        let all_ai = settings.is_widget_enabled("aiusage_all");
        for ai in telemetry::AI_PROVIDERS {
            if all_ai || settings.is_widget_enabled(ai.id) {
                telemetry::refresh_ai_provider(&self.ai_usage, ai.id, false);
                if self.ai_full_detail(ai.id).is_none() {
                    self.refresh_ai_full(ai.id);
                }
            }
        }
    }

    /// Returns `true` when any dock widget needs a 15 FPS animation tick (e.g. reference Battery charging wave).
    pub fn needs_animation_tick(&self, settings: &DockSettings) -> bool {
        settings.animations_enabled
            && settings.is_widget_enabled("battery")
            && self.battery_charging
    }

    pub fn cycle_keyboard_layout(&mut self, forward: bool) {
        if let Some(code) = telemetry::cycle_keyboard_layout(forward, self.keyboard_target_hwnd) {
            self.keyboard_override = Some((code, Instant::now()));
        }
    }

    fn current_keyboard_code(&self) -> String {
        if let Some((ref code, at)) = self.keyboard_override
            && at.elapsed().as_millis() < 450
        {
            return code.clone();
        }
        // Cached by `tick` behind the 1 s poll gate; a freshly booted runtime falls back once.
        let cached = self.poll_snapshot.keyboard_code.clone();
        if cached.is_empty() {
            active_keyboard_code()
        } else {
            cached
        }
    }

    /// Returns the live dynamic width for a dock widget (Clock, Weather, Stats, Storage,
    /// and BluetoothBattery adapt to active settings, density, and connected counts).
    pub fn widget_width_dips(&self, id: &str) -> f32 {
        let canonical = if custom_widgets::is_custom(id) {
            id
        } else {
            canonical_widget_id(id)
        };
        match canonical {
            "clock" => self.clock_width,
            "weather" => self.weather_width,
            "stats" => stats_width_dips(self.stats_col_count),
            "storage" => self.storage_width,
            "bluetoothbattery" => self.bt_width,
            _ => widget_descriptor(canonical).map_or(96.0, |d| d.width_dips),
        }
    }

    /// Computes dynamic width directly from `settings` without waiting for the next `tick`.
    pub fn widget_width_dips_with_settings(&self, id: &str, settings: &DockSettings) -> f32 {
        let canonical = if custom_widgets::is_custom(id) {
            id
        } else {
            canonical_widget_id(id)
        };
        match canonical {
            "clock" => clock_width_dips(settings),
            "weather" => {
                let compact = setting_bool(settings, "WeatherCompactMode", false)
                    || opentaskbar_core::settings::resolve_widget_density(settings, "weather")
                        == WidgetDensityMode::Compact;
                weather_width_dips(compact)
            }
            "stats" => stats_width_dips(self.stats_col_count),
            "storage" => {
                let compact = setting_bool(settings, "StorageCompactMode", false)
                    || opentaskbar_core::settings::resolve_widget_density(settings, "storage")
                        == WidgetDensityMode::Compact;
                storage_width_dips(self.poll_snapshot.fixed_drives().len(), compact)
            }
            "bluetoothbattery" => {
                let ignored = setting_string_list(settings, "IgnoredWirelessDevices");
                let visible_count = self.bt_battery.lock().ok().map_or(0, |g| {
                    g.devices
                        .iter()
                        .filter(|d| !bt_is_ignored(&ignored, d))
                        .count()
                });
                bluetooth_width_dips(visible_count)
            }
            _ => widget_descriptor(canonical).map_or(96.0, |d| d.width_dips),
        }
    }

    /// `AudioWidget` wheel handler: 120 delta = ±5% with fractional remainder carried across
    /// events. Refreshes live audio state first, preserves mute when scrolling down, auto-unmutes
    /// when scrolling up, and invalidates the cached tooltip immediately.
    pub fn adjust_volume_wheel(&mut self, delta: i32) -> u32 {
        (self.audio_vol, self.audio_muted) = get_audio_state();
        let next = media_ext::volume_from_wheel_step(
            self.audio_vol,
            delta,
            &mut self.volume_wheel_remainder,
            5,
        );
        telemetry::set_audio_volume_preserving_mute(next, delta > 0);
        (self.audio_vol, self.audio_muted) = get_audio_state();
        self.tooltip_text = None;
        self.audio_vol
    }

    /// Records the last external foreground HWND so Keyboard Layout flyout picks and wheel
    /// scrolls always switch the application the user was typing in.
    pub fn set_keyboard_target_hwnd(&mut self, hwnd: usize) {
        if hwnd != 0 {
            self.keyboard_target_hwnd = Some(hwnd);
        }
    }

    /// Single radio toggle path for chip clicks and the Quick Settings flyout. A successful
    /// write clears the sticky "Unavailable" flag and records the pending target; a failed
    /// write opens the matching Settings page (reference `ChangeResult` reconciliation).
    fn toggle_radio(&mut self, kind: &'static str) -> bool {
        let (current, settings_uri) = match kind {
            "wifi" => (self.wifi_on, "ms-settings:network-wifi"),
            _ => (self.bluetooth_on, "ms-settings:bluetooth"),
        };
        let target = !current;
        self.radio_failed = false;
        self.radio_denied = false;
        if crate::platform::radio::set(kind, target) {
            if kind == "wifi" {
                self.wifi_on = target;
                self.wifi_target = Some(target);
            } else {
                self.bluetooth_on = target;
                self.bluetooth_target = Some(target);
            }
            self.radio_unavailable = false;
            true
        } else {
            self.radio_unavailable = true;
            self.radio_denied = true;
            self.radio_failed = true;
            pins::launch(settings_uri.to_owned());
            false
        }
    }

    /// `KeyboardLayoutWidget`: send `WM_INPUTLANGCHANGEREQUEST` to the last *external*
    /// foreground window (never our own dock/island/flyout windows).
    pub fn activate_keyboard_on_last_external_window(&mut self) {
        use windows_sys::Win32::UI::WindowsAndMessaging::{
            GetForegroundWindow, GetWindowThreadProcessId,
        };
        unsafe {
            let hwnd = GetForegroundWindow();
            if hwnd.is_null() {
                return;
            }
            let mut pid = 0u32;
            GetWindowThreadProcessId(hwnd, &mut pid);
            if pid == std::process::id() {
                return;
            }
            // Remember the last external target so a later wheel event that lands while our own
            // window has focus still switches the app the user was actually typing in.
            self.keyboard_target_hwnd = Some(hwnd as usize);
            let layouts = telemetry::installed_keyboard_layouts();
            if layouts.len() < 2 {
                return;
            }
            let active_hkl = active_keyboard_hkl(Some(hwnd as usize));
            let active_code = active_keyboard_code();
            let idx = layouts
                .iter()
                .position(|(hkl, _, _)| active_hkl != 0 && *hkl == active_hkl)
                .or_else(|| layouts.iter().position(|(_, code, _)| code == &active_code))
                .unwrap_or(0);
            let (hkl, ref next_code, _) = layouts[(idx + 1) % layouts.len()];
            telemetry::activate_keyboard_layout_for(hwnd as usize, hkl);
            self.keyboard_override = Some((next_code.clone(), Instant::now()));
        }
    }

    /// Exposes the shared focus session to WS-D (island expanded details) and flyouts.
    pub fn focus_session(&self) -> SharedFocusSession {
        self.focus.clone()
    }

    /// Exposes the NetSpeed speed-test controller to the live flyout.
    pub fn speedtest(&mut self) -> &mut SpeedTestController {
        &mut self.speedtest
    }

    /// Off-thread wireless battery read used by the poll tick. Honours the persisted
    /// ignored-device / alias / listen-only settings in the platform provider, so a refresh
    /// never wakes a device that the user asked to listen to only.
    pub fn refresh_bluetooth_devices(&self, settings: &DockSettings) {
        self.spawn_wireless_refresh(settings, false);
    }

    /// Manual "Refresh": always performs a fresh poll. If a poll is already running, the
    /// forced request is queued and that same worker polls again when it finishes (no
    /// second thread).
    pub fn refresh_bluetooth_devices_forced(&self, settings: &DockSettings) {
        self.spawn_wireless_refresh(settings, true);
    }

    fn spawn_wireless_refresh(&self, settings: &DockSettings, force: bool) {
        {
            let mut q = self
                .wb_queue
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            if q.in_flight {
                let prev_force = q.pending.as_ref().is_some_and(|(_, f)| *f);
                q.pending = Some((settings.clone(), prev_force || force));
                return;
            }
            q.in_flight = true;
            if let Ok(mut cache) = self.bt_battery.lock() {
                cache.loading = true;
            }
        }
        let initial_settings = settings.clone();
        let bt_cache = Arc::clone(&self.bt_battery);
        let queue = Arc::clone(&self.wb_queue);
        let generation = Arc::clone(&self.wb_generation);
        let live_update = Arc::clone(&self.wb_live_update);
        let spawned = std::thread::Builder::new()
            .name("opentaskbar-wb".to_owned())
            .spawn(move || {
                // Releases `in_flight` and `loading` on panic unwind and wakes any open flyout.
                let mut guard = WirelessRefreshGuard {
                    queue: Arc::clone(&queue),
                    cache: Arc::clone(&bt_cache),
                    live_update: Arc::clone(&live_update),
                    completed: false,
                };
                let mut cur_settings = initial_settings;
                let mut cur_force = force;
                let mut first_pass = true;
                loop {
                    let tag = generation.fetch_add(1, Ordering::AcqRel) + 1;
                    run_wireless_refresh(&cur_settings, cur_force || first_pass, &bt_cache, tag);
                    first_pass = false;
                    let next = {
                        let mut q = queue
                            .lock()
                            .unwrap_or_else(|poisoned| poisoned.into_inner());
                        if let Some(next) = q.pending.take() {
                            Some(next)
                        } else {
                            if let Ok(mut cache) = bt_cache.lock() {
                                cache.loading = false;
                            }
                            q.in_flight = false;
                            guard.completed = true;
                            None
                        }
                    };
                    match next {
                        Some((next_settings, next_force)) => {
                            cur_settings = next_settings;
                            cur_force = next_force;
                            notify_wireless_flyout(&live_update);
                        }
                        None => break,
                    }
                }
            })
            .is_ok();
        if !spawned {
            {
                let mut q = self
                    .wb_queue
                    .lock()
                    .unwrap_or_else(|poisoned| poisoned.into_inner());
                q.in_flight = false;
                q.pending = None;
                if let Ok(mut cache) = self.bt_battery.lock() {
                    cache.loading = false;
                }
            }
            notify_wireless_flyout(&self.wb_live_update);
        }
    }

    /// Requests a background top-process refresh at most once per 5 s / while one is running.
    fn sample_top_processes_async(&mut self, now: Instant) {
        if self.stats_top_in_flight.swap(true, Ordering::AcqRel)
            || self
                .last_stats_top
                .is_some_and(|t| now.duration_since(t) < Duration::from_secs(5))
        {
            return;
        }
        self.last_stats_top = Some(now);
        let shared = Arc::clone(&self.stats_top);
        let in_flight = Arc::clone(&self.stats_top_in_flight);
        let spawned = std::thread::Builder::new()
            .name("opentaskbar-stats".to_owned())
            .spawn(move || {
                let top = telemetry::sample_top_processes(5);
                if let Ok(mut guard) = shared.lock() {
                    *guard = top;
                }
                in_flight.store(false, Ordering::Release);
            })
            .is_ok();
        if !spawned {
            self.stats_top_in_flight.store(false, Ordering::Release);
        }
    }

    /// Explicit invalidation used only by the Stats flyout Refresh button. Live ticks must
    /// never reset the cache — that used to spawn a new thread every second.
    pub fn invalidate_top_processes(&mut self) {
        self.last_stats_top = None;
        self.sample_top_processes_async(Instant::now());
    }

    /// Polls the speed test from the tick, the live flyout and the flyout buttons alike so
    /// `speedtest_started_at` is always cleared when the controller reports completion.
    fn poll_speedtest(&mut self, now: Instant) {
        if self.speedtest.poll(now, self.speedtest_started_at) {
            self.speedtest_started_at = None;
        }
    }

    /// Rebuilds a flyout spec from the *current* widget state. Called by the live flyout WM_TIMER
    /// update path (WS-7 `LiveEvent::Update`) so Clock / Stats / Focus / Countdown / Stopwatch /
    /// NetSpeed show live data instead of a frozen snapshot.
    pub fn live_spec(&mut self, id: &str, settings: &DockSettings, is_dark: bool) -> FlyoutSpec {
        if let Some(spec) = custom_widgets::flyout(id) {
            return spec;
        }
        match id {
            "clock" => {
                let info = telemetry::local_clock_info();
                Self::build_calendar_spec(
                    info.year,
                    info.month,
                    Some((info.year, info.month, info.day)),
                )
            }
            "stats" => {
                (self.cpu_pct, self.ram_pct) = self.stats.sample();
                self.stats_uptime = telemetry::system_uptime_string();
                // Keep the cached top-process list: live ticks used to reset it every second.
                self.build_stats_spec(settings, is_dark)
            }
            "focus" => focus_break::build_focus_flyout(&self.focus.snapshot(), is_dark),
            "stopwatch" => self.build_stopwatch_spec(),
            "countdown" => self.build_countdown_spec(),
            "netspeed" => {
                self.poll_speedtest(Instant::now());
                let mut spec = speedtest::build_speedtest_flyout(
                    self.speedtest.state(),
                    self.speedtest.result(),
                );
                spec.blocks.insert(
                    0,
                    FlyoutBlock::MetricTiles(vec![
                        (
                            loc::t("NetworkDownload"),
                            telemetry::format_speed(self.net.rx_bps),
                        ),
                        (
                            loc::t("NetworkUpload"),
                            telemetry::format_speed(self.net.tx_bps),
                        ),
                    ]),
                );
                spec
            }
            _ => FlyoutSpec {
                title: String::new(),
                subtitle: None,
                width_dips: 280.0,
                blocks: vec![FlyoutBlock::Note(String::new())],
            },
        }
    }

    /// Applies 5-sample enter / 2-sample exit hysteresis to the GPU temperature badge so a single
    /// driver spike cannot flip the colour band.
    fn update_gpu_thermal(&mut self, temp_c: Option<u32>) -> u8 {
        let target = temp_c.map_or(0, thermal_band);
        if target == self.gpu_thermal_state {
            self.gpu_thermal_enter = 0;
            self.gpu_thermal_exit = 0;
            return self.gpu_thermal_state;
        }
        if target > self.gpu_thermal_state {
            self.gpu_thermal_exit = 0;
            self.gpu_thermal_enter = self.gpu_thermal_enter.saturating_add(1);
            if self.gpu_thermal_enter >= 5 {
                self.gpu_thermal_state = target;
                self.gpu_thermal_enter = 0;
            }
        } else {
            self.gpu_thermal_enter = 0;
            self.gpu_thermal_exit = self.gpu_thermal_exit.saturating_add(1);
            if self.gpu_thermal_exit >= 2 {
                self.gpu_thermal_state = target;
                self.gpu_thermal_exit = 0;
            }
        }
        self.gpu_thermal_state
    }

    /// Last successfully fetched full AI detail. The flyout never substitutes an empty default
    /// when a cached payload exists.
    pub fn ai_full_detail(&self, id: &str) -> Option<telemetry::AiFullDetail> {
        self.ai_full.lock().ok()?.get(id).and_then(|(at, detail)| {
            (at.elapsed() < Duration::from_secs(600)).then(|| detail.clone())
        })
    }

    /// Fetches `ai_fetch_full` off the UI thread and writes the result into the shared cache.
    /// Concurrent refreshes for the same provider are coalesced.
    pub fn refresh_ai_full(&self, id: &'static str) {
        let claimed = match self.ai_full_in_flight.lock() {
            Ok(mut guard) => guard.insert(id.to_owned()),
            Err(_) => false,
        };
        if !claimed {
            return;
        }
        let cache = Arc::clone(&self.ai_full);
        let in_flight = Arc::clone(&self.ai_full_in_flight);
        let worker_in_flight = Arc::clone(&in_flight);
        let spawned = std::thread::Builder::new()
            .name(format!("otb-ai-full-{id}"))
            .spawn(move || {
                let detail = telemetry::ai_fetch_full(id);
                if detail.error.is_none()
                    && let Ok(mut guard) = cache.lock()
                {
                    guard.insert(id.to_owned(), (Instant::now(), detail));
                }
                if let Ok(mut guard) = worker_in_flight.lock() {
                    guard.remove(id);
                }
            })
            .is_ok();
        if !spawned && let Ok(mut guard) = in_flight.lock() {
            guard.remove(id);
        }
    }

    /// Builds flyout detail from the cache, falling back to a severity-only snapshot while the
    /// first full fetch is still in flight.
    fn ai_flyout_detail(
        &self,
        id: &str,
        snap: &telemetry::AiUsageSnapshot,
    ) -> ai_flyout::AiFullDetail {
        if let Some(full) = self.ai_full_detail(id) {
            return ai_flyout::from_telemetry(&full);
        }
        let peak = snap.windows.iter().map(|w| w.used).fold(0.0f32, f32::max);
        // Route through the same adapter as a fetched payload so severity and formatting
        // have a single source of truth.
        ai_flyout::from_telemetry(&telemetry::AiFullDetail {
            provider_id: id.to_owned(),
            used: peak,
            ..Default::default()
        })
    }

    /// Exposes the hover coordinator's public API (WS-D already reaches it via the pub field).
    pub fn hover(&self) -> &WidgetHoverCoordinator {
        &self.hover
    }

    /// Builds the dynamic-island expanded details payload for the media widget.
    pub fn island_media_details(&self) -> MediaDetails {
        let snap = self.media_snapshot();
        media_ext::get_details(&snap, self.audio_vol, self.audio_muted)
    }

    fn island_detail_row(
        label: impl Into<String>,
        value: impl Into<String>,
        progress: Option<f32>,
    ) -> IslandDetailRow {
        IslandDetailRow {
            label: label.into(),
            value: value.into(),
            progress: progress
                .filter(|p| p.is_finite())
                .map(|p| p.clamp(0.0, 1.0)),
        }
    }

    fn cached_ai_provider_detected(&mut self, provider_id: &'static str) -> bool {
        const AI_DETECT_TTL: Duration = Duration::from_secs(5);
        if let Some(&(checked, detected)) = self.ai_detected_cache.get(provider_id)
            && checked.elapsed() < AI_DETECT_TTL
        {
            return detected;
        }
        let detected = telemetry::ai_provider_detected(provider_id);
        self.ai_detected_cache
            .insert(provider_id, (Instant::now(), detected));
        detected
    }

    fn cached_audio_detail(&mut self) -> (Option<String>, Option<(u32, bool)>) {
        if let Some(ref cache) = self.audio_detail_cache
            && cache.checked.elapsed() < POLL_TTL
        {
            return (cache.out_name.clone(), cache.mic);
        }
        let out_name = telemetry::default_audio_device_name(false);
        let mic = telemetry::get_mic_state();
        self.audio_detail_cache = Some(AudioDetailCache {
            checked: Instant::now(),
            out_name: out_name.clone(),
            mic,
        });
        (out_name, mic)
    }

    fn cached_keyboard_layouts(&mut self) -> Vec<(isize, String, String)> {
        const KB_LAYOUTS_TTL: Duration = Duration::from_secs(2);
        if let Some(ref cache) = self.keyboard_layouts_cache
            && cache.checked.elapsed() < KB_LAYOUTS_TTL
        {
            return cache.layouts.clone();
        }
        let layouts = telemetry::installed_keyboard_layouts();
        self.keyboard_layouts_cache = Some(KeyboardLayoutsCache {
            checked: Instant::now(),
            layouts: layouts.clone(),
        });
        layouts
    }

    fn island_detail_action(
        id: impl Into<String>,
        label: impl Into<String>,
        primary: bool,
    ) -> IslandDetailAction {
        IslandDetailAction {
            id: id.into(),
            label: label.into(),
            primary,
        }
    }

    fn build_ai_island_detail(
        &mut self,
        ai: &telemetry::AiProviderMeta,
        settings: &DockSettings,
    ) -> IslandDetailSpec {
        let snap = self
            .ai_usage
            .lock()
            .ok()
            .and_then(|g| g.providers.get(ai.id).cloned())
            .unwrap_or_default();
        let full = self.ai_full_detail(ai.id);
        let ring = resolved_ai_usage_ring(settings, ai.id);
        let ordered = order_ai_windows(&snap.windows, ring);

        let detected = snap.has_cred || self.cached_ai_provider_detected(ai.id);
        let headline = if let Some(bal) = &snap.balance_text {
            bal.clone()
        } else if let Some(w) = ordered.first() {
            format!(
                "{} · {}",
                w.label,
                format_percent((w.used * 100.0).round() as u32)
            )
        } else if detected {
            loc::t("AiUsageConfigured")
        } else {
            loc::t("AiUsageStatusSignIn")
        };

        let subtitle = full
            .as_ref()
            .and_then(|f| f.plan.clone())
            .or_else(|| snap.plan.clone())
            .unwrap_or_else(|| ai.name.to_owned());

        let mut rows: Vec<IslandDetailRow> = ordered
            .iter()
            .take(4)
            .map(|w| {
                let pct = format_percent((w.used * 100.0).round() as u32);
                let val = if w.reset_text.trim().is_empty() || w.reset_text == "—" {
                    pct
                } else {
                    format!("{pct} · {}", w.reset_text)
                };
                Self::island_detail_row(w.label.clone(), val, Some(w.used))
            })
            .collect();

        if rows.is_empty()
            && let Some(bal) = &snap.balance_text
        {
            rows.push(Self::island_detail_row(
                loc::t("AiUsageBalance"),
                bal.clone(),
                snap.credit_ratio,
            ));
        }
        if rows.is_empty() {
            rows.push(Self::island_detail_row(
                loc::t("WidgetDetailStatus"),
                if detected {
                    loc::t("AiUsageConfigured")
                } else {
                    loc::t("AiUsageStatusSignIn")
                },
                None,
            ));
        }
        rows.truncate(4);

        IslandDetailSpec {
            widget_id: ai.id.to_owned(),
            title: widget_name(ai.id),
            headline,
            subtitle,
            rows,
            actions: vec![
                Self::island_detail_action("refresh", loc::t("AiUsageRefresh"), true),
                Self::island_detail_action("open_dashboard", loc::t("AiUsageOpenDashboard"), false),
            ],
            has_full_flyout: widget_has_flyout(ai.id),
        }
    }

    /// Universal Dynamic Island expanded detail spec for every built-in and custom widget.
    pub fn island_detail_spec(
        &mut self,
        widget_id: &str,
        settings: &DockSettings,
    ) -> IslandDetailSpec {
        let canonical = if custom_widgets::is_custom(widget_id) {
            widget_id.to_owned()
        } else {
            canonical_widget_id(widget_id).to_owned()
        };
        let id = canonical.as_str();
        let title = widget_name(id);
        let has_full_flyout = widget_has_flyout(id);
        fn row(l: impl Into<String>, v: impl Into<String>, r: Option<f32>) -> IslandDetailRow {
            WidgetRuntime::island_detail_row(l, v, r)
        }
        fn act(
            id: impl Into<String>,
            label: impl Into<String>,
            primary: bool,
        ) -> IslandDetailAction {
            WidgetRuntime::island_detail_action(id, label, primary)
        }
        let dash_or = |s: &str| {
            if s.is_empty() {
                "—".to_owned()
            } else {
                s.to_owned()
            }
        };
        let run_label = |running: bool| {
            loc::t(if running {
                "WidgetDetailRunning"
            } else {
                "WidgetDetailPaused"
            })
        };
        let vol_label = |vol: u32, muted: bool| {
            if muted {
                loc::t("AudioMuted")
            } else {
                format_percent(vol)
            }
        };
        let make_spec = |headline, subtitle, rows, actions| IslandDetailSpec {
            widget_id: canonical.clone(),
            title: title.clone(),
            headline,
            subtitle,
            rows,
            actions,
            has_full_flyout,
        };

        if custom_widgets::is_custom(id) {
            let chip = custom_widgets::chip(id);
            let details = custom_widgets::details(id);
            let headline = chip
                .as_ref()
                .map_or_else(|| "…".to_owned(), |c| c.value.clone());
            let subtitle = chip.as_ref().map_or_else(
                || custom_widgets::error(id).unwrap_or_else(|| loc::t("AiUsageStatusLoading")),
                |c| c.label.clone(),
            );
            let mut rows: Vec<IslandDetailRow> = details
                .into_iter()
                .take(4)
                .map(|(k, v)| row(k, v, None))
                .collect();
            if rows.is_empty() {
                rows.push(row(
                    loc::t("WidgetDetailStatus"),
                    headline.clone(),
                    chip.and_then(|c| c.ratio),
                ));
            }
            return make_spec(
                headline,
                subtitle,
                rows,
                vec![act("refresh", loc::t("AiUsageRefresh"), false)],
            );
        }

        if let Some(ai) = telemetry::ai_provider_meta(id) {
            return self.build_ai_island_detail(ai, settings);
        }

        let mut spec = match id {
            "clock" => {
                let info = telemetry::local_clock_info();
                let (time, date) = telemetry::format_clock(&info, settings);
                let (day, _, _, left) = day_progress_metrics();
                let date_val = if date.is_empty() {
                    info.short_date.clone()
                } else {
                    date
                };
                make_spec(
                    time,
                    info.full_date,
                    vec![
                        row(loc::t("Today"), date_val, None),
                        row(
                            loc::t("WidgetDetailDay"),
                            format!("{} · {}", info.tz_name, info.tz_label),
                            None,
                        ),
                        row(
                            loc::t("DayProgressDay"),
                            format!(
                                "{} · {}",
                                day_progress_percent_text(day),
                                loc::t_fmt("DayProgressRemaining", &[&left.to_string()])
                            ),
                            Some(day),
                        ),
                    ],
                    vec![act("open_settings", loc::t("LauncherDateTime"), false)],
                )
            }
            "weather" => {
                let snap = self
                    .weather
                    .lock()
                    .ok()
                    .map(|g| g.clone())
                    .unwrap_or_default();
                let (temp, feels, range) = weather_display(&snap, settings);
                let city = if snap.city.is_empty() {
                    settings.weather_city.clone()
                } else {
                    snap.city.clone()
                };
                let cond = if snap.condition.is_empty() {
                    loc::t("WeatherUnavailable")
                } else {
                    snap.condition.clone()
                };
                let wind = snap.formatted_wind(weather_unit(settings));
                make_spec(
                    if snap.temp_c.is_empty() {
                        "—".to_owned()
                    } else {
                        temp
                    },
                    if city.is_empty() {
                        cond
                    } else {
                        format!("{city} · {cond}")
                    },
                    vec![
                        row(
                            loc::t("FeelsLikeLabel"),
                            if snap.feels_like.is_empty() {
                                "—".to_owned()
                            } else {
                                feels
                            },
                            None,
                        ),
                        row(loc::t("WeatherHumidity"), dash_or(&snap.humidity), None),
                        row(loc::t("WeatherWind"), dash_or(&wind), None),
                        row(loc::t("WeatherToday"), dash_or(&range), None),
                    ],
                    vec![act("refresh", loc::t("AiUsageRefresh"), true)],
                )
            }
            "dayprogress" => {
                let (day, week, year, left) = day_progress_metrics();
                make_spec(
                    day_progress_percent_text(day),
                    loc::t_fmt("DayProgressRemaining", &[&left.to_string()]),
                    vec![
                        row(
                            loc::t("DayProgressDay"),
                            day_progress_percent_text(day),
                            Some(day),
                        ),
                        row(
                            loc::t("DayProgressWeek"),
                            day_progress_percent_text(week),
                            Some(week),
                        ),
                        row(
                            loc::t("DayProgressYear"),
                            day_progress_percent_text(year),
                            Some(year),
                        ),
                    ],
                    Vec::new(),
                )
            }
            "stopwatch" => {
                let state = run_label(self.stopwatch_running);
                let mut rows = vec![row(loc::t("WidgetDetailState"), state.clone(), None)];
                if self.stopwatch_laps.is_empty() {
                    rows.push(row(loc::t("StopwatchLap"), loc::t("StopwatchNoLaps"), None));
                } else {
                    for (i, &(secs, cs)) in self.stopwatch_laps.iter().rev().take(3).enumerate() {
                        let num = self.stopwatch_laps.len() - i;
                        rows.push(row(
                            format!("{} #{num:02}", loc::t("StopwatchLap")),
                            format_mm_ss_cs(secs, cs),
                            None,
                        ));
                    }
                }
                make_spec(
                    format_mm_ss_cs(self.stopwatch_secs, self.stopwatch_cs),
                    state,
                    rows,
                    vec![
                        act(
                            "toggle",
                            loc::t(if self.stopwatch_running {
                                "TimedWidgetPause"
                            } else {
                                "TimedWidgetStart"
                            }),
                            true,
                        ),
                        act("lap", loc::t("StopwatchLap"), false),
                        act("reset", loc::t("Reset"), false),
                    ],
                )
            }
            "countdown" => {
                let ratio = if self.countdown_total_secs > 0 {
                    (self.countdown_remaining_secs as f32 / self.countdown_total_secs as f32)
                        .clamp(0.0, 1.0)
                } else {
                    0.0
                };
                let state = run_label(self.countdown_running);
                make_spec(
                    format_mm_secs(self.countdown_remaining_secs),
                    state.clone(),
                    vec![
                        row(
                            loc::t("WidgetCountdown"),
                            format!(
                                "{} / {}",
                                format_mm_secs(self.countdown_remaining_secs),
                                format_mm_secs(self.countdown_total_secs)
                            ),
                            Some(ratio),
                        ),
                        row(loc::t("WidgetDetailState"), state, None),
                    ],
                    vec![
                        act(
                            "toggle",
                            loc::t(if self.countdown_running {
                                "TimedWidgetPause"
                            } else {
                                "TimedWidgetStart"
                            }),
                            true,
                        ),
                        act("add_1m", "+1m", false),
                        act("reset", loc::t("Reset"), false),
                    ],
                )
            }
            "focus" => {
                self.focus.with_mut(|s| s.sync_from_settings(settings));
                self.sync_focus_mirrors();
                let snap = self.focus.snapshot();
                make_spec(
                    format_mm_secs(snap.remaining_secs),
                    snap.status_text(),
                    vec![
                        row(
                            snap.phase.label(),
                            format_mm_secs(snap.remaining_secs),
                            Some(snap.progress_ratio()),
                        ),
                        row(
                            loc::t("FocusSessionsLabel"),
                            format!("{}/{}", snap.session_number, snap.sessions_target),
                            Some(snap.session_number as f32 / snap.sessions_target.max(1) as f32),
                        ),
                        row(loc::t("WidgetDetailState"), run_label(snap.running), None),
                    ],
                    vec![
                        act(
                            "toggle",
                            loc::t(if snap.running {
                                "FocusPause"
                            } else {
                                "FocusStart"
                            }),
                            true,
                        ),
                        act("skip", loc::t("FocusSkip"), false),
                        act("reset", loc::t("FocusReset"), false),
                    ],
                )
            }
            "stats" => {
                let (_, used_gib, total_gib) = telemetry::sample_ram_info();
                let mut rows = vec![
                    row(
                        loc::t("Cpu"),
                        format_percent(self.cpu_pct),
                        Some(self.cpu_pct as f32 / 100.0),
                    ),
                    row(
                        loc::t("Memory"),
                        format!(
                            "{} ({used_gib:.1}/{total_gib:.0} GB)",
                            format_percent(self.ram_pct)
                        ),
                        Some(self.ram_pct as f32 / 100.0),
                    ),
                ];
                if gpu_metrics_visible(self.gpu_samples, self.gpu_misses) {
                    rows.push(row(
                        loc::t("Gpu"),
                        format_percent(self.gpu_pct),
                        Some(self.gpu_pct as f32 / 100.0),
                    ));
                }
                if let Some(c) = self.gpu_temp_c {
                    rows.push(row(
                        loc::t("GpuTemperature"),
                        telemetry::format_temperature(f64::from(c), weather_unit(settings)),
                        Some(c as f32 / 100.0),
                    ));
                }
                make_spec(
                    format!(
                        "CPU {} · RAM {}",
                        format_percent(self.cpu_pct),
                        format_percent(self.ram_pct)
                    ),
                    format!("{} · {}", loc::t("StatsUptime"), self.stats_uptime),
                    rows,
                    vec![
                        act("refresh", loc::t("AiUsageRefresh"), false),
                        act("taskmgr", loc::t("TaskManager"), false),
                    ],
                )
            }
            "netspeed" => {
                let running = matches!(
                    self.speedtest.state(),
                    SpeedTestState::MeasuringPing | SpeedTestState::Downloading
                );
                let down = telemetry::format_speed(self.net.rx_bps);
                let up = telemetry::format_speed(self.net.tx_bps);
                make_spec(
                    format!("↓ {down} · ↑ {up}"),
                    if self.net.adapter_name.is_empty() {
                        self.speedtest.state().label()
                    } else {
                        self.net.adapter_name.clone()
                    },
                    vec![
                        row(loc::t("NetworkDownload"), down, None),
                        row(loc::t("NetworkUpload"), up, None),
                        row(
                            loc::t("NetworkSessionReceived"),
                            telemetry::format_bytes(self.net.session_rx),
                            None,
                        ),
                        row(
                            loc::t("NetworkSessionSent"),
                            telemetry::format_bytes(self.net.session_tx),
                            None,
                        ),
                    ],
                    vec![
                        act(
                            "speedtest",
                            loc::t(if running {
                                "SpeedTestCancel"
                            } else {
                                "SpeedTestStart"
                            }),
                            true,
                        ),
                        act("network_settings", loc::t("NetworkSettings"), false),
                    ],
                )
            }
            "battery" => {
                let b = sample_battery();
                let status = if !b.has_battery {
                    loc::t("WidgetDetailNoBattery")
                } else if b.charging {
                    loc::t("Charging")
                } else if b.on_ac {
                    loc::t_fmt("BatteryPluggedIn", &[&b.percent.to_string()])
                } else {
                    loc::t("WidgetDetailOnBattery")
                };
                let mut rows = vec![
                    row(
                        loc::t("WidgetDetailCharge"),
                        if b.has_battery {
                            format_percent(b.percent)
                        } else {
                            "—".to_owned()
                        },
                        b.has_battery.then_some(b.percent as f32 / 100.0),
                    ),
                    row(loc::t("WidgetDetailStatus"), status.clone(), None),
                ];
                if let Some(secs) = b
                    .life_secs
                    .filter(|&s| b.has_battery && !b.on_ac && s >= 60)
                {
                    rows.push(row(
                        loc::t("Battery"),
                        format!("{}h {}m", secs / 3600, (secs % 3600) / 60),
                        None,
                    ));
                }
                make_spec(
                    if b.has_battery {
                        format_percent(b.percent)
                    } else {
                        loc::t("BatteryUnavailable")
                    },
                    status,
                    rows,
                    vec![act("power_settings", loc::t("BatterySaver"), false)],
                )
            }
            "bluetoothbattery" => {
                let ignored = setting_string_list(settings, "IgnoredWirelessDevices");
                let poll_secs = wireless_poll_secs(settings);
                let low = setting_i64(
                    settings,
                    "WirelessBatteryLowThreshold",
                    i64::from(settings.wireless_battery_low_threshold.max(0)),
                )
                .max(0);
                let devices: Vec<BluetoothBatteryDevice> = self
                    .bt_battery
                    .lock()
                    .ok()
                    .map(|g| {
                        g.devices
                            .iter()
                            .filter(|d| !bt_is_ignored(&ignored, d))
                            .take(4)
                            .cloned()
                            .collect()
                    })
                    .unwrap_or_default();
                // Last-known rows are labelled "(cached)" instead of looking live.
                let shown_percent = |d: &BluetoothBatteryDevice| {
                    if d.is_live() {
                        format_percent(d.percent)
                    } else {
                        loc::t_fmt("WirelessBatteryStale", &[&format_percent(d.percent)])
                    }
                };
                let headline = devices.first().map_or_else(
                    || loc::t("BluetoothNoDevices"),
                    |d| format!("{} · {}", d.name, shown_percent(d)),
                );
                let mut rows: Vec<IslandDetailRow> = devices
                    .iter()
                    .map(|d| {
                        row(
                            d.name.clone(),
                            shown_percent(d),
                            Some(d.percent as f32 / 100.0),
                        )
                    })
                    .collect();
                if rows.is_empty() {
                    rows.push(row(
                        loc::t("BluetoothDevices"),
                        loc::t("WirelessBatteryNoDevices"),
                        None,
                    ));
                }
                make_spec(
                    headline,
                    loc::t_fmt(
                        "WirelessBatterySubtitle",
                        &[&poll_secs.to_string(), &low.to_string()],
                    ),
                    rows,
                    vec![
                        act("refresh", loc::t("WirelessBatteryRefresh"), true),
                        act(
                            "bluetooth_settings",
                            loc::t("QuickBluetoothSettings"),
                            false,
                        ),
                    ],
                )
            }
            "storage" => {
                let drives = self.poll_snapshot.fixed_drives();
                let headline = drives.first().map_or_else(
                    || loc::t("StorageNoFixedDrives"),
                    |d| format!("{}: {}", d.letter, d.format_capacity_pair()),
                );
                let mut rows: Vec<IslandDetailRow> = drives
                    .iter()
                    .take(4)
                    .map(|d| {
                        let used = (d.total_gb - d.free_gb).max(0.0);
                        row(
                            format!("{}:", d.letter),
                            d.format_capacity_pair(),
                            Some((used / d.total_gb.max(1.0)) as f32),
                        )
                    })
                    .collect();
                if rows.is_empty() {
                    rows.push(row(
                        loc::t("WidgetStorage"),
                        loc::t("StorageNoFixedDrives"),
                        None,
                    ));
                }
                make_spec(
                    headline,
                    loc::t("WidgetStorage"),
                    rows,
                    vec![act("explorer", loc::t("OpenFileExplorer"), false)],
                )
            }
            "media" => {
                let snap = self.media_snapshot();
                let has_media = !snap.title.is_empty();
                let ratio = (snap.duration_secs > 0).then(|| {
                    (snap.position_secs as f32 / snap.duration_secs as f32).clamp(0.0, 1.0)
                });
                let timeline_val = if snap.duration_secs > 0 {
                    format!(
                        "{} / {}",
                        format_mm_secs(snap.position_secs),
                        format_mm_secs(snap.duration_secs)
                    )
                } else {
                    "—".to_owned()
                };
                make_spec(
                    if has_media {
                        snap.title
                    } else {
                        loc::t("MediaNothingPlaying")
                    },
                    if has_media && !snap.artist.is_empty() {
                        snap.artist
                    } else {
                        loc::t("MediaStartPlaybackHint")
                    },
                    vec![
                        row(loc::t("SeekLabel"), timeline_val, ratio),
                        row(
                            loc::t("Volume"),
                            vol_label(self.audio_vol, self.audio_muted),
                            Some(self.audio_vol as f32 / 100.0),
                        ),
                    ],
                    vec![
                        act("prev", loc::t("MediaPrevious"), false),
                        act("play_pause", loc::t("MediaPlayPause"), true),
                        act("next", loc::t("MediaNext"), false),
                    ],
                )
            }
            "audio" => {
                let (cached_out_name, cached_mic) = self.cached_audio_detail();
                let out_name = cached_out_name.unwrap_or_else(|| loc::t("Speakers"));
                let vol_text = vol_label(self.audio_vol, self.audio_muted);
                let mut rows = vec![
                    row(
                        loc::t("AudioOutput"),
                        vol_text.clone(),
                        Some(self.audio_vol as f32 / 100.0),
                    ),
                    row(loc::t("WidgetDetailDevice"), out_name.clone(), None),
                ];
                if let Some((in_vol, in_muted)) = cached_mic {
                    rows.push(row(
                        loc::t("AudioInput"),
                        vol_label(in_vol, in_muted),
                        Some(in_vol as f32 / 100.0),
                    ));
                }
                make_spec(
                    vol_text,
                    out_name,
                    rows,
                    vec![
                        act(
                            "mute",
                            loc::t(if self.audio_muted {
                                "AudioUnmute"
                            } else {
                                "AudioMute"
                            }),
                            true,
                        ),
                        act("vol_down", "-5%", false),
                        act("vol_up", "+5%", false),
                    ],
                )
            }
            "quicksettings" => make_spec(
                title.clone(),
                format!(
                    "{}: {}",
                    loc::t("WiFi"),
                    radio_label(self.wifi_on, self.radio_loading)
                ),
                vec![
                    row(
                        loc::t("WiFi"),
                        radio_label(self.wifi_on, self.radio_loading),
                        None,
                    ),
                    row(
                        loc::t("Bluetooth"),
                        radio_label(self.bluetooth_on, self.radio_loading),
                        None,
                    ),
                    row(
                        loc::t("NightLight"),
                        loc::t(if self.night_light_on {
                            "QuickOn"
                        } else {
                            "QuickOff"
                        }),
                        None,
                    ),
                    row(
                        loc::t("Volume"),
                        vol_label(self.audio_vol, self.audio_muted),
                        Some(self.audio_vol as f32 / 100.0),
                    ),
                ],
                vec![
                    act("wifi", loc::t("WiFi"), self.wifi_on),
                    act("bluetooth", loc::t("Bluetooth"), self.bluetooth_on),
                    act("nightlight", loc::t("NightLight"), self.night_light_on),
                ],
            ),
            "keyboard" => {
                let code = self.current_keyboard_code();
                let layouts = self.cached_keyboard_layouts();
                let active_name = layouts
                    .iter()
                    .find(|(_, c, _)| c == &code)
                    .map_or_else(|| code.clone(), |(_, _, d)| d.clone());
                let mut rows: Vec<IslandDetailRow> = layouts
                    .iter()
                    .take(4)
                    .map(|(_, c, d)| row(c.clone(), d.clone(), None))
                    .collect();
                if rows.is_empty() {
                    rows.push(row(code.clone(), active_name.clone(), None));
                }
                make_spec(
                    code,
                    active_name,
                    rows,
                    vec![act("next_layout", loc::t("WidgetKeyboardLayout"), true)],
                )
            }
            "aiusage_all" => {
                let guard_map = self
                    .ai_usage
                    .lock()
                    .ok()
                    .map(|g| g.providers.clone())
                    .unwrap_or_default();
                let mut active = 0usize;
                let mut rows = Vec::new();
                for ai in telemetry::AI_PROVIDERS {
                    let snap = guard_map.get(ai.id).cloned().unwrap_or_default();
                    if snap.has_cred || snap.balance_text.is_some() || !snap.windows.is_empty() {
                        active += 1;
                    }
                    let ring = resolved_ai_usage_ring(settings, ai.id);
                    let ordered = order_ai_windows(&snap.windows, ring);
                    if let Some(w) = ordered.first() {
                        rows.push(row(
                            format!("{} ({})", ai.short, w.label),
                            format_percent((w.used * 100.0).round() as u32),
                            Some(w.used),
                        ));
                    } else if let Some(bal) = snap.balance_text {
                        rows.push(row(ai.short, bal, snap.credit_ratio));
                    }
                }
                if rows.is_empty() {
                    rows.push(row(
                        loc::t("WidgetCategoryAi"),
                        loc::t_fmt(
                            "AiUsageReadyCount",
                            &[
                                &active.to_string(),
                                &telemetry::AI_PROVIDERS.len().to_string(),
                            ],
                        ),
                        None,
                    ));
                }
                rows.truncate(4);
                make_spec(
                    loc::t_fmt("AiUsageActiveCount", &[&active.to_string()]),
                    loc::t("AiUsageLiveUsageNote"),
                    rows,
                    vec![act("refresh", loc::t("AiUsageRefreshAll"), true)],
                )
            }
            "stacks" => {
                let mut rows: Vec<IslandDetailRow> = settings
                    .stack_folders
                    .iter()
                    .take(4)
                    .map(|path| {
                        let name = std::path::Path::new(path)
                            .file_name()
                            .and_then(|s| s.to_str())
                            .unwrap_or(path);
                        row(loc::t("FolderLabel"), name, None)
                    })
                    .collect();
                if rows.is_empty() {
                    rows.push(row(
                        loc::t("FolderStacks"),
                        loc::t("WidgetStacksHint"),
                        None,
                    ));
                }
                make_spec(
                    loc::t_fmt("FolderCount", &[&settings.stack_folders.len().to_string()]),
                    loc::t("FolderStacks"),
                    rows,
                    Vec::new(),
                )
            }
            "groups" => {
                let mut rows: Vec<IslandDetailRow> = settings
                    .app_groups
                    .iter()
                    .take(4)
                    .map(|g| {
                        row(
                            g.name.clone(),
                            loc::t_fmt(
                                "AppGroupAppsCount",
                                &[&g.executable_paths.len().to_string()],
                            ),
                            None,
                        )
                    })
                    .collect();
                if rows.is_empty() {
                    rows.push(row(loc::t("AppGroups"), loc::t("WidgetGroupsHint"), None));
                }
                make_spec(
                    loc::t_fmt("GroupCount", &[&settings.app_groups.len().to_string()]),
                    loc::t("AppGroups"),
                    rows,
                    Vec::new(),
                )
            }
            "clipboard" => {
                let mut spec = self.clipboard.as_ref().map_or_else(
                    || {
                        make_spec(
                            "No History".to_owned(),
                            if settings.clipboard_history_enabled() {
                                "Copy text to capture locally".to_owned()
                            } else {
                                "Clipboard history paused".to_owned()
                            },
                            vec![row("Ready", "Waiting for text copies", None)],
                            Vec::new(),
                        )
                    },
                    |store| store.build_island_detail(),
                );
                spec.widget_id = canonical;
                spec.title = title;
                spec.has_full_flyout = has_full_flyout;
                spec
            }
            "shelf" => {
                let mut spec = self.shelf.as_ref().map_or_else(
                    || {
                        make_spec(
                            "Shelf Empty".to_owned(),
                            "Drop files or notes here".to_owned(),
                            vec![row("Tip", "Drag files onto the Island or paste text", None)],
                            vec![Self::island_detail_action(
                                "paste_text",
                                "Add Clipboard",
                                true,
                            )],
                        )
                    },
                    |shelf| shelf.build_island_detail(),
                );
                spec.widget_id = canonical;
                spec.title = title;
                spec.has_full_flyout = has_full_flyout;
                spec
            }
            "todo" => {
                let now_unix = SystemTime::now()
                    .duration_since(UNIX_EPOCH)
                    .unwrap_or_default()
                    .as_secs();
                let mut spec = self.todo.as_ref().map_or_else(
                    || {
                        make_spec(
                            "All Clear".to_owned(),
                            "No pending tasks or events".to_owned(),
                            vec![row(
                                "Tip",
                                "Type \"15:30 Standup\" in the flyout to schedule",
                                None,
                            )],
                            Vec::new(),
                        )
                    },
                    |todo| todo.build_island_detail(now_unix),
                );
                spec.widget_id = canonical;
                spec.title = title;
                spec.has_full_flyout = has_full_flyout;
                spec
            }
            "agents" => {
                let mut spec = self.agents.as_ref().map_or_else(
                    || {
                        make_spec(
                            "Monitor Off".to_owned(),
                            "Enable AI agent monitor to track sessions".to_owned(),
                            vec![row("Status", "Idle (0 threads)", None)],
                            vec![Self::island_detail_action(
                                "install_hook",
                                "Install Claude Hook",
                                false,
                            )],
                        )
                    },
                    |agents| agents.build_island_detail(),
                );
                spec.widget_id = canonical;
                spec.title = title;
                spec.has_full_flyout = has_full_flyout;
                spec
            }
            _ => make_spec(
                title.clone(),
                loc::t("WidgetDetails"),
                vec![row(loc::t("WidgetDetailStatus"), title.clone(), None)],
                Vec::new(),
            ),
        };
        spec.rows.truncate(4);
        spec.actions.truncate(3);
        spec
    }

    /// Dispatches a quick action emitted from a Dynamic Island expanded detail card.
    pub fn handle_island_detail_action(
        &mut self,
        widget_id: &str,
        action_id: &str,
        settings: &DockSettings,
    ) -> bool {
        let id = if custom_widgets::is_custom(widget_id) {
            widget_id
        } else {
            canonical_widget_id(widget_id)
        };
        if custom_widgets::is_custom(id) {
            if action_id == "refresh" {
                custom_widgets::poll_widget(id, settings);
                return true;
            }
            return false;
        }
        if let Some(ai) = telemetry::ai_provider_meta(id) {
            return match action_id {
                "refresh" => {
                    telemetry::refresh_ai_provider(&self.ai_usage, ai.id, true);
                    self.refresh_ai_full(ai.id);
                    true
                }
                "open_dashboard" => {
                    pins::launch(ai.dashboard_url.to_owned());
                    true
                }
                _ => false,
            };
        }
        match (id, action_id) {
            ("clock", "open_settings") => {
                pins::launch("ms-settings:dateandtime".to_owned());
                true
            }
            ("weather", "refresh") => {
                telemetry::refresh_weather(&self.weather, &settings.weather_city, true);
                self.weather_retry_at = Some(Instant::now() + Duration::from_secs(30 * 60));
                true
            }
            ("stopwatch", "toggle") => {
                self.stopwatch_running = !self.stopwatch_running;
                true
            }
            ("stopwatch", "lap") => {
                self.stopwatch_laps
                    .push((self.stopwatch_secs, self.stopwatch_cs));
                true
            }
            ("stopwatch", "reset") => {
                self.stopwatch_running = false;
                self.stopwatch_secs = 0;
                self.stopwatch_cs = 0;
                self.stopwatch_debt_secs = 0.0;
                self.stopwatch_laps.clear();
                true
            }
            ("countdown", "toggle") => {
                if self.countdown_remaining_secs == 0 {
                    self.countdown_remaining_secs = self.countdown_total_secs;
                }
                self.countdown_running = !self.countdown_running;
                true
            }
            ("countdown", "add_1m") => {
                self.countdown_total_secs =
                    self.countdown_total_secs.saturating_add(60).min(1440 * 60);
                self.countdown_remaining_secs = self
                    .countdown_remaining_secs
                    .saturating_add(60)
                    .min(1440 * 60);
                true
            }
            ("countdown", "add_5m") => {
                self.countdown_total_secs =
                    self.countdown_total_secs.saturating_add(300).min(1440 * 60);
                self.countdown_remaining_secs = self
                    .countdown_remaining_secs
                    .saturating_add(300)
                    .min(1440 * 60);
                true
            }
            ("countdown", "reset") => {
                self.countdown_running = false;
                self.countdown_remaining_secs = self.countdown_total_secs;
                true
            }
            ("focus", "toggle") => {
                self.focus.with_mut(|s| s.toggle());
                self.sync_focus_mirrors();
                true
            }
            ("focus", "skip") => {
                let event = self.focus.with_mut(|s| {
                    s.skip_phase();
                    FocusEvent::PhaseEnded(s.phase)
                });
                if matches!(event, Some(FocusEvent::PhaseEnded(_))) {
                    play_break_chime();
                }
                self.sync_focus_mirrors();
                true
            }
            ("focus", "reset") => {
                self.focus.with_mut(|s| s.reset());
                self.sync_focus_mirrors();
                true
            }
            ("stats", "refresh") => {
                (self.cpu_pct, self.ram_pct) = self.stats.sample();
                self.stats_uptime = telemetry::system_uptime_string();
                self.invalidate_top_processes();
                true
            }
            ("stats", "taskmgr") => {
                pins::launch("taskmgr.exe".to_owned());
                true
            }
            ("netspeed", "speedtest") => {
                if matches!(
                    self.speedtest.state(),
                    SpeedTestState::MeasuringPing | SpeedTestState::Downloading
                ) {
                    self.speedtest.cancel();
                    self.speedtest_started_at = None;
                } else {
                    self.speedtest.start();
                    self.speedtest_started_at = Some(Instant::now());
                    self.poll_speedtest(Instant::now());
                }
                true
            }
            ("netspeed", "network_settings") => {
                pins::launch("ms-settings:network".to_owned());
                true
            }
            ("battery", "power_settings") => {
                pins::launch("ms-settings:batterysaver".to_owned());
                true
            }
            ("bluetoothbattery", "refresh") => {
                self.refresh_bluetooth_devices_forced(settings);
                true
            }
            ("bluetoothbattery", "bluetooth_settings") => {
                pins::launch("ms-settings:bluetooth".to_owned());
                true
            }
            ("storage", "explorer") => {
                pins::launch("explorer.exe".to_owned());
                true
            }
            ("media", "prev") => {
                telemetry::send_media_vk(VK_MEDIA_PREV_TRACK);
                telemetry::refresh_media_session(&self.media_session, true);
                true
            }
            ("media", "play_pause") => {
                telemetry::send_media_vk(VK_MEDIA_PLAY_PAUSE);
                telemetry::refresh_media_session(&self.media_session, true);
                true
            }
            ("media", "next") => {
                telemetry::send_media_vk(VK_MEDIA_NEXT_TRACK);
                telemetry::refresh_media_session(&self.media_session, true);
                true
            }
            ("audio", "mute") => {
                toggle_audio_mute();
                (self.audio_vol, self.audio_muted) = get_audio_state();
                self.tooltip_text = None;
                true
            }
            ("audio", "vol_down") => {
                adjust_audio_volume(-5);
                (self.audio_vol, self.audio_muted) = get_audio_state();
                self.tooltip_text = None;
                true
            }
            ("audio", "vol_up") => {
                adjust_audio_volume(5);
                (self.audio_vol, self.audio_muted) = get_audio_state();
                self.tooltip_text = None;
                true
            }
            ("audio", "sound_settings") => {
                pins::launch("ms-settings:sound".to_owned());
                true
            }
            ("quicksettings", "wifi") => {
                self.toggle_radio("wifi");
                true
            }
            ("quicksettings", "bluetooth") => {
                self.toggle_radio("bluetooth");
                true
            }
            ("quicksettings", "nightlight") => {
                if telemetry::toggle_night_light() {
                    self.night_light_on = !self.night_light_on;
                } else {
                    pins::launch("ms-settings:nightlight".to_owned());
                }
                true
            }
            ("keyboard", "next_layout") => {
                self.cycle_keyboard_layout(true);
                true
            }
            ("aiusage_all", "refresh") => {
                for ai in telemetry::AI_PROVIDERS {
                    telemetry::refresh_ai_provider(&self.ai_usage, ai.id, true);
                    self.refresh_ai_full(ai.id);
                }
                true
            }
            ("clipboard", _) => {
                if self.clipboard.is_none()
                    && (settings.clipboard_history_enabled()
                        || settings.is_widget_enabled("clipboard"))
                {
                    self.clipboard = Some(Box::new(clipboard::ClipboardStore::new()));
                }
                self.clipboard
                    .as_mut()
                    .is_some_and(|store| store.handle_flyout_action(action_id))
            }
            ("shelf", _) => {
                let shelf = self
                    .shelf
                    .get_or_insert_with(|| Box::new(shelf::FileShelfState::load_default()));
                let changed = shelf.handle_flyout_action(action_id);
                if changed {
                    let _ = shelf.save_default();
                }
                changed
            }
            ("todo", _) => {
                let todo = self
                    .todo
                    .get_or_insert_with(|| Box::new(todo::TodoState::load_default()));
                let changed = todo.handle_flyout_action(action_id, None);
                if changed {
                    let _ = todo.save_default();
                }
                changed
            }
            ("agents", _) => {
                if let Some(agents) = self.agents.as_mut() {
                    agents.handle_flyout_action(action_id)
                } else if action_id == "install_hook" {
                    agents::install_claude_code_hook().is_ok()
                } else {
                    false
                }
            }
            _ => false,
        }
    }

    /// Builds the exact reference 1-to-1 `WidgetChipSpec` rendered onto the dock pill.
    pub fn chip_spec(&self, id: &str, settings: &DockSettings, is_dark: bool) -> WidgetChipSpec {
        let pal = design::palette(is_dark);
        if custom_widgets::is_custom(id) {
            let details = custom_widgets::details(id);
            let fallback_icon =
                custom_widgets::icon_for(&custom_widgets::CustomChip::default(), id, settings);
            if !custom_widgets::is_approved(id, settings) {
                return WidgetChipSpec::Custom {
                    label: "Approval required".to_owned(),
                    value: "!".to_owned(),
                    ratio: None,
                    icon: fallback_icon,
                    color: pal.warning,
                    details,
                };
            }
            return match custom_widgets::chip(id) {
                Some(chip) => {
                    let icon = custom_widgets::icon_for(&chip, id, settings);
                    WidgetChipSpec::Custom {
                        label: chip.label,
                        value: chip.value,
                        ratio: chip.ratio,
                        icon,
                        color: pal.widget_accent,
                        details,
                    }
                }
                None => WidgetChipSpec::Custom {
                    label: custom_widgets::error(id)
                        .unwrap_or_else(|| loc::t("AiUsageStatusLoading")),
                    value: "…".to_owned(),
                    ratio: None,
                    icon: fallback_icon,
                    color: pal.danger,
                    details,
                },
            };
        }
        let id = canonical_widget_id(id);
        if let Some(ai) = telemetry::ai_provider_meta(id) {
            let snap = self
                .ai_usage
                .lock()
                .ok()
                .and_then(|g| g.providers.get(ai.id).cloned())
                .unwrap_or_default();
            let ring = resolved_ai_usage_ring(settings, ai.id);
            let ordered = order_ai_windows(&snap.windows, ring);
            let bar_accent = |used: f32| -> Rgb { design::meter_color(&pal, used) };
            return match ai.id {
                "aiusage_claude" | "aiusage_codex" => {
                    let s_win = snap.windows.iter().find(|w| w.label == "5h");
                    let w_win = snap.windows.iter().find(|w| w.label == "7d");
                    let s_used = s_win.map_or(0.0, |w| w.used);
                    let w_used = w_win.map_or(0.0, |w| w.used);
                    let s_row = (
                        "5h".to_owned(),
                        s_used,
                        bar_accent(s_used),
                        s_win.map_or_else(|| "—".to_owned(), |w| w.reset_text.clone()),
                    );
                    let w_row = (
                        "7d".to_owned(),
                        w_used,
                        bar_accent(w_used),
                        w_win.map_or_else(|| "—".to_owned(), |w| w.reset_text.clone()),
                    );
                    let rows = match ring {
                        AiUsageRing::Session => vec![s_row, w_row],
                        AiUsageRing::Weekly => vec![w_row, s_row],
                        AiUsageRing::Highest => {
                            if w_used > s_used {
                                vec![w_row, s_row]
                            } else {
                                vec![s_row, w_row]
                            }
                        }
                    };
                    WidgetChipSpec::AiDual {
                        provider_id: ai.id,
                        brand_color: Some(ai_provider_color_for_theme(ai.id, is_dark)),
                        rows,
                    }
                }
                "aiusage_cursor" => {
                    // Exact reference AiUsageWidget.cs: Cursor shows a single long window row (`30d` / `7d`).
                    let win = ordered.first();
                    let used = win.map_or(0.0, |w| w.used);
                    let default_lbl = match ring {
                        AiUsageRing::Session => "5h",
                        AiUsageRing::Weekly => "7d",
                        AiUsageRing::Highest => "30d",
                    };
                    WidgetChipSpec::AiDual {
                        provider_id: ai.id,
                        brand_color: Some(ai_provider_color_for_theme(ai.id, is_dark)),
                        rows: vec![(
                            win.map_or_else(|| default_lbl.to_owned(), |w| w.label.clone()),
                            used,
                            bar_accent(used),
                            win.map_or_else(|| "—".to_owned(), |w| w.reset_text.clone()),
                        )],
                    }
                }
                "aiusage_opencode" => {
                    let order_labels = match ring {
                        AiUsageRing::Weekly => ["7d", "30d", "1h"],
                        AiUsageRing::Session | AiUsageRing::Highest => ["1h", "7d", "30d"],
                    };
                    let mut cols: Vec<(String, String, f32, Rgb, String)> = order_labels
                        .into_iter()
                        .map(|lbl| {
                            if let Some(w) = snap.windows.iter().find(|w| w.label == lbl) {
                                let col = if w.used >= 0.75 {
                                    bar_accent(w.used)
                                } else {
                                    pal.success
                                };
                                (
                                    lbl.to_owned(),
                                    format_percent((w.used * 100.0).round() as u32),
                                    w.used,
                                    col,
                                    w.reset_text.clone(),
                                )
                            } else {
                                (
                                    lbl.to_owned(),
                                    "—".to_owned(),
                                    0.0,
                                    pal.success,
                                    "—".to_owned(),
                                )
                            }
                        })
                        .collect();
                    if ring == AiUsageRing::Highest {
                        cols.sort_by(|a, b| b.2.total_cmp(&a.2));
                    }
                    WidgetChipSpec::AiOpenCode { columns: cols }
                }
                "aiusage_copilot" => {
                    let brand = ai_provider_color_for_theme("aiusage_copilot", is_dark);
                    if let Some(w) = ordered.first() {
                        let col = if w.used >= 0.75 {
                            bar_accent(w.used)
                        } else {
                            brand
                        };
                        WidgetChipSpec::AiCopilot {
                            value_text: format_percent((w.used * 100.0).round() as u32),
                            meta_text: w.reset_text.clone(),
                            used: w.used,
                            bar_color: col,
                        }
                    } else {
                        WidgetChipSpec::AiCopilot {
                            value_text: "—".to_owned(),
                            meta_text: if snap.has_cred {
                                loc::t("AiUsageMonthly")
                            } else {
                                loc::t("AiUsageStatusSignIn")
                            },
                            used: 0.0,
                            bar_color: brand,
                        }
                    }
                }
                "aiusage_deepseek" | "aiusage_openrouter" => {
                    if ai.id == "aiusage_openrouter" {
                        set_chip_openrouter_ratio(snap.credit_ratio);
                    }
                    WidgetChipSpec::AiBalance {
                        provider_id: ai.id,
                        name: ai.short,
                        brand_color: ai_provider_color_for_theme(ai.id, is_dark),
                        value_text: snap.balance_text.unwrap_or_else(|| "—".to_owned()),
                    }
                }
                _ => WidgetChipSpec::AiDual {
                    provider_id: ai.id,
                    brand_color: Some(ai_provider_color_for_theme(ai.id, is_dark)),
                    rows: vec![("5h".to_owned(), 0.0, pal.widget_accent, "—".to_owned())],
                },
            };
        }

        match id {
            "clock" => {
                let info = telemetry::local_clock_info();
                let (time, date) = telemetry::format_clock(&info, settings);
                set_chip_clock_below(settings.clock_date_layout == ClockDateLayout::Below);
                let weekday = if clock_weekday_is_redundant(&date, &info.weekday_short) {
                    String::new()
                } else {
                    info.weekday_short
                };
                WidgetChipSpec::Clock {
                    time,
                    date,
                    weekday,
                }
            }
            "weather" => {
                let snap = self
                    .weather
                    .lock()
                    .ok()
                    .map(|g| g.clone())
                    .unwrap_or_default();
                let available = !snap.temp_c.is_empty();
                let (formatted_temp, _, formatted_range) = weather_display(&snap, settings);
                let place = if snap.city.is_empty() {
                    settings.weather_city.clone()
                } else {
                    snap.city
                };
                let condition = if available {
                    snap.condition
                } else if snap.loading {
                    loc::t("WeatherLoading").to_owned()
                } else {
                    loc::t("WeatherUnavailable").to_owned()
                };
                let icon_color = weather_icon_color(&condition, is_dark);
                WidgetChipSpec::Weather {
                    available,
                    icon_color,
                    is_rain: snap.is_rain,
                    temp: formatted_temp,
                    place,
                    condition,
                    today_range: (!formatted_range.is_empty()).then_some(formatted_range),
                }
            }
            "dayprogress" => {
                let (day, _, _, left) = day_progress_metrics();
                let left_suffix = if loc::language().starts_with("tr") {
                    format!("{left}s")
                } else {
                    format!("{left}h")
                };
                WidgetChipSpec::DayProgress {
                    ratio: day,
                    percent: day_progress_percent_text(day),
                    unit: format!("{} · {left_suffix}", loc::t("DayProgressUnit")),
                }
            }
            "stopwatch" => WidgetChipSpec::Timed {
                is_focus: false,
                running: self.stopwatch_running,
                text: format_mm_ss_cs(self.stopwatch_secs, self.stopwatch_cs),
            },
            "countdown" => {
                let ratio = if self.countdown_total_secs > 0 {
                    (self.countdown_remaining_secs as f32 / self.countdown_total_secs as f32)
                        .clamp(0.0, 1.0)
                } else {
                    0.0
                };
                set_chip_countdown_ratio(ratio);
                WidgetChipSpec::Timed {
                    is_focus: false,
                    running: self.countdown_running,
                    text: format_mm_secs(self.countdown_remaining_secs),
                }
            }
            "focus" => {
                let snapshot = self.focus.snapshot();
                let is_break = snapshot.phase == FocusPhase::Break;
                let total = if is_break {
                    snapshot.break_minutes.max(1) * 60
                } else {
                    snapshot.focus_minutes.max(1) * 60
                };
                let ratio = (snapshot.remaining_secs as f32 / total as f32).clamp(0.0, 1.0);
                set_chip_focus_hint(is_break, ratio);
                // Chip text stays tabular; phase/session are exposed in the flyout/island.
                WidgetChipSpec::Timed {
                    is_focus: true,
                    running: snapshot.running,
                    text: format_mm_secs(snapshot.remaining_secs),
                }
            }
            "stats" => {
                let show_cpu = settings.stats_meters & 1 != 0;
                let show_ram = settings.stats_meters & 2 != 0;
                let show_gpu = settings.stats_meters & 16 != 0;
                let show_gpu_temp = settings.stats_meters & 32 != 0;
                let gpu_ready = gpu_metrics_visible(self.gpu_samples, self.gpu_misses);
                let mut columns = Vec::with_capacity(4);
                if show_cpu {
                    columns.push(SystemStatColumn {
                        label: "CPU".to_owned(),
                        value_text: format_percent(self.cpu_pct),
                        ratio: (self.cpu_pct as f32 / 100.0).clamp(0.0, 1.0),
                        bar_color: threshold_color(&pal, self.cpu_pct, pal.widget_blue),
                        warm_text: self.cpu_pct >= 85,
                    });
                }
                if show_ram {
                    columns.push(SystemStatColumn {
                        label: "RAM".to_owned(),
                        value_text: format_percent(self.ram_pct),
                        ratio: (self.ram_pct as f32 / 100.0).clamp(0.0, 1.0),
                        bar_color: threshold_color(&pal, self.ram_pct, pal.widget_accent),
                        warm_text: self.ram_pct >= 85,
                    });
                }
                if show_gpu && gpu_ready {
                    columns.push(SystemStatColumn {
                        label: "GPU".to_owned(),
                        value_text: format_percent(self.gpu_pct),
                        ratio: (self.gpu_pct as f32 / 100.0).clamp(0.0, 1.0),
                        bar_color: threshold_color(&pal, self.gpu_pct, pal.success),
                        warm_text: self.gpu_pct >= 85,
                    });
                }
                if show_gpu_temp && let Some(c) = self.gpu_temp_c {
                    let unit = weather_unit(settings);
                    let unit_lbl = match unit {
                        opentaskbar_core::settings::TemperatureUnit::Celsius => {
                            loc::t("GpuCelsius")
                        }
                        opentaskbar_core::settings::TemperatureUnit::Fahrenheit => {
                            loc::t("GpuFahrenheit")
                        }
                    };
                    let temp_bar_color = match self.gpu_thermal_state {
                        2 => pal.danger,
                        1 => pal.warning,
                        _ => pal.success,
                    };
                    columns.push(SystemStatColumn {
                        label: unit_lbl,
                        value_text: telemetry::format_temperature(f64::from(c), unit),
                        ratio: (c as f32 / 100.0).clamp(0.0, 1.0),
                        bar_color: temp_bar_color,
                        warm_text: self.gpu_thermal_state >= 1,
                    });
                }
                WidgetChipSpec::SystemStats { columns }
            }
            "netspeed" => {
                let down = if self.speedtest.state() == SpeedTestState::Downloading {
                    speedtest::format_throughput(self.net.rx_bps)
                } else {
                    telemetry::format_speed(self.net.rx_bps)
                };
                let peak_down = self
                    .net_down_history
                    .iter()
                    .copied()
                    .max()
                    .unwrap_or(0)
                    .max(64 * 1024) as f32;
                let peak_up = self
                    .net_up_history
                    .iter()
                    .copied()
                    .max()
                    .unwrap_or(0)
                    .max(64 * 1024) as f32;
                let down_history: Vec<f32> = self
                    .net_down_history
                    .iter()
                    .map(|&v| (v as f32 / peak_down).clamp(0.04, 1.0))
                    .collect();
                let up_history: Vec<f32> = self
                    .net_up_history
                    .iter()
                    .map(|&v| (v as f32 / peak_up).clamp(0.04, 1.0))
                    .collect();
                WidgetChipSpec::NetSpeed {
                    down,
                    up: telemetry::format_speed(self.net.tx_bps),
                    down_history,
                    up_history,
                }
            }
            "battery" => {
                let b = sample_battery();
                let wave_phase = if b.has_battery && b.charging && settings.animations_enabled {
                    (((self.battery_anim_start.elapsed().as_millis() % 1600) / 67) as f32) / 24.0
                } else {
                    0.0
                };
                WidgetChipSpec::BatteryLiquid {
                    present: b.has_battery,
                    percent: b.percent,
                    charging: b.charging,
                    wave_phase,
                }
            }
            "bluetoothbattery" => {
                let ignored = setting_string_list(settings, "IgnoredWirelessDevices");
                let low_threshold = setting_i64(
                    settings,
                    "WirelessBatteryLowThreshold",
                    i64::from(settings.wireless_battery_low_threshold.max(0)),
                )
                .max(0) as u32;
                let devices: Vec<(telemetry::BluetoothDeviceKind, u32, bool, bool)> = self
                    .bt_battery
                    .lock()
                    .ok()
                    .map(|g| {
                        g.devices
                            .iter()
                            .filter(|d| !bt_is_ignored(&ignored, d))
                            .take(3)
                            .map(|d| {
                                // A retained last-known level never shows the live low-battery
                                // or charging accent.
                                let live = d.is_live();
                                (
                                    d.kind,
                                    d.percent,
                                    live && low_threshold > 0 && d.percent <= low_threshold,
                                    live && d.charging,
                                )
                            })
                            .collect()
                    })
                    .unwrap_or_default();
                WidgetChipSpec::Bluetooth { devices }
            }
            "storage" => {
                let compact = setting_bool(settings, "StorageCompactMode", false)
                    || opentaskbar_core::settings::resolve_widget_density(settings, "storage")
                        == WidgetDensityMode::Compact;
                let mut ratios = HashMap::new();
                let drives: Vec<(String, String)> = self
                    .poll_snapshot
                    .fixed_drives()
                    .iter()
                    .take(if compact { 1 } else { 3 })
                    .map(|d| {
                        let key = format!("{}:", d.letter);
                        let used = (d.total_gb - d.free_gb).max(0.0);
                        let used_ratio = (used / d.total_gb.max(1.0)).clamp(0.0, 1.0) as f32;
                        ratios.insert(key.clone(), used_ratio);
                        let pair = d.format_capacity_pair();
                        (
                            key,
                            if compact {
                                pair.split(" / ").next().unwrap_or(&pair).to_owned()
                            } else {
                                pair
                            },
                        )
                    })
                    .collect();
                set_chip_storage_ratios(ratios);
                WidgetChipSpec::Storage { drives }
            }
            "media" => {
                let snap = self.media_snapshot();
                let has_media = !snap.title.is_empty();
                let progress = if snap.duration_secs > 0 {
                    (snap.position_secs as f32 / snap.duration_secs as f32).clamp(0.0, 1.0)
                } else {
                    0.0
                };
                set_chip_media_hint(MediaChipHint {
                    playing: snap.playing,
                    progress,
                    cover_path: if has_media {
                        media_ext::find_cover_art(&snap.title, &snap.artist)
                    } else {
                        None
                    },
                    cover_pixels: snap.cover_pixels,
                    has_media,
                });
                let title = if has_media {
                    snap.title
                } else {
                    loc::t("MediaNothingPlaying")
                };
                WidgetChipSpec::Media {
                    title,
                    artist: snap.artist,
                }
            }
            "audio" => WidgetChipSpec::Audio {
                volume: self.audio_vol,
                muted: self.audio_muted,
            },
            "quicksettings" => {
                // reference QuickSettingsWidget.xaml: Wi-Fi, Bluetooth, Night Light — real radio states.
                let (wifi, bt) = if self.radio_loading {
                    (false, false)
                } else {
                    (self.wifi_on, self.bluetooth_on)
                };
                WidgetChipSpec::QuickSettings {
                    pills: [
                        ("\u{E701}".to_owned(), wifi),
                        ("\u{E702}".to_owned(), bt),
                        ("\u{E708}".to_owned(), self.night_light_on),
                    ],
                }
            }
            "keyboard" => WidgetChipSpec::Keyboard {
                code: self.current_keyboard_code(),
            },
            "aiusage_all" => {
                let mut active_count = 0usize;
                let mut peak_label: Option<(f32, String)> = None;
                let mut first_bal: Option<String> = None;
                if let Ok(guard) = self.ai_usage.lock() {
                    for ai in telemetry::AI_PROVIDERS {
                        if let Some(snap) = guard.providers.get(ai.id) {
                            if snap.has_cred
                                || snap.balance_text.is_some()
                                || !snap.windows.is_empty()
                            {
                                active_count += 1;
                            }
                            if let Some(w) = order_ai_windows(
                                &snap.windows,
                                resolved_ai_usage_ring(settings, ai.id),
                            )
                            .first()
                            {
                                let replace =
                                    peak_label.as_ref().is_none_or(|(prev, _)| w.used > *prev);
                                if replace {
                                    peak_label = Some((
                                        w.used,
                                        format!(
                                            "{} {}",
                                            ai.name,
                                            format_percent((w.used * 100.0).round() as u32)
                                        ),
                                    ));
                                }
                            } else if first_bal.is_none()
                                && let Some(bal) = &snap.balance_text
                            {
                                first_bal = Some(format!("{} {bal}", ai.name));
                            }
                        }
                    }
                }
                let bottom = peak_label.map(|(_, s)| s).or(first_bal).unwrap_or_else(|| {
                    loc::t_fmt(
                        "AiUsageReadyCount",
                        &[
                            &active_count.to_string(),
                            &telemetry::AI_PROVIDERS.len().to_string(),
                        ],
                    )
                });
                WidgetChipSpec::Simple {
                    // AppIcon::Layers
                    icon: widget_icons::resolve_widget_icon(
                        id,
                        settings,
                        WidgetIcon::Glyph(0xE81E),
                    ),
                    icon_color: pal.widget_accent,
                    top: loc::t_fmt("AiUsageActiveCount", &[&active_count.to_string()]),
                    bottom,
                }
            }
            "stacks" => WidgetChipSpec::Simple {
                // AppIcon::Folder
                icon: widget_icons::resolve_widget_icon(id, settings, WidgetIcon::Glyph(0xE8B7)),
                icon_color: pal.warning,
                top: loc::t("StacksShort"),
                bottom: loc::t_fmt("FolderCount", &[&settings.stack_folders.len().to_string()]),
            },
            "groups" => WidgetChipSpec::Simple {
                // AppIcon::LayoutGrid
                icon: widget_icons::resolve_widget_icon(id, settings, WidgetIcon::Glyph(0xF0E2)),
                icon_color: pal.accent,
                top: loc::t("GroupsShort"),
                bottom: loc::t_fmt("GroupCount", &[&settings.app_groups.len().to_string()]),
            },
            "clipboard" => {
                let count = self.clipboard.as_ref().map_or(0, |c| c.entries().len());
                let bottom = match count {
                    0 => "Empty".to_owned(),
                    1 => "1 clip".to_owned(),
                    n => format!("{n} clips"),
                };
                WidgetChipSpec::Simple {
                    icon: widget_icons::resolve_widget_icon(
                        id,
                        settings,
                        WidgetIcon::Glyph(0xE77F),
                    ),
                    icon_color: pal.widget_blue,
                    top: loc::t("WidgetClipboard"),
                    bottom,
                }
            }
            "shelf" => {
                let count = self.shelf.as_ref().map_or(0, |s| s.count());
                let bottom = match count {
                    0 => "Empty".to_owned(),
                    1 => "1 item".to_owned(),
                    n => format!("{n} items"),
                };
                WidgetChipSpec::Simple {
                    icon: widget_icons::resolve_widget_icon(
                        id,
                        settings,
                        WidgetIcon::Glyph(0xE8B7),
                    ),
                    icon_color: pal.widget_accent,
                    top: loc::t("WidgetFileShelf"),
                    bottom,
                }
            }
            "todo" => {
                let now_unix = SystemTime::now()
                    .duration_since(UNIX_EPOCH)
                    .unwrap_or_default()
                    .as_secs();
                let bottom = self.todo.as_ref().map_or_else(
                    || "All Clear".to_owned(),
                    |todo| {
                        if let Some(ev) = todo.next_event(now_unix) {
                            format!(
                                "{} · {}",
                                todo::format_countdown(ev.start_unix, ev.end_unix, now_unix),
                                ev.summary
                            )
                        } else {
                            let open = todo.tasks().iter().filter(|t| !t.done).count();
                            match open {
                                0 => "All Clear".to_owned(),
                                1 => "1 task".to_owned(),
                                n => format!("{n} tasks"),
                            }
                        }
                    },
                );
                WidgetChipSpec::Simple {
                    icon: widget_icons::resolve_widget_icon(
                        id,
                        settings,
                        WidgetIcon::Glyph(0xE73A),
                    ),
                    icon_color: pal.widget_accent,
                    top: loc::t("WidgetTodo"),
                    bottom,
                }
            }
            "agents" => {
                let has_pending = self
                    .agents
                    .as_ref()
                    .is_some_and(|a| a.pending_approval().is_some());
                let bottom = self
                    .agents
                    .as_ref()
                    .map_or_else(|| "Idle".to_owned(), |a| a.summary_badge());
                WidgetChipSpec::Simple {
                    icon: widget_icons::resolve_widget_icon(
                        id,
                        settings,
                        WidgetIcon::Glyph(0xE99A),
                    ),
                    icon_color: if has_pending {
                        pal.warning
                    } else {
                        pal.widget_accent
                    },
                    top: loc::t("WidgetAiAgents"),
                    bottom,
                }
            }
            _ => WidgetChipSpec::Keyboard {
                code: id.chars().take(3).collect::<String>().to_uppercase(),
            },
        }
    }

    /// Concise reference-style tooltip above each hovered widget.
    /// Per-frame accessor. The expensive path (COM device name, disk queries, file checks)
    /// runs at most twice per second for the same sub-button instead of on every VSync.
    pub fn hover_tooltip(
        &mut self,
        id: &'static str,
        sub: Option<u8>,
        settings: &DockSettings,
    ) -> &str {
        let now = Instant::now();
        let settings_key = tooltip_settings_key(settings);
        let cached = self.tooltip_text.as_ref().is_some_and(|c| {
            c.id == id
                && c.sub == sub
                && c.settings_key == settings_key
                && now.duration_since(c.checked) < Duration::from_millis(500)
        });
        if !cached {
            let text = self.build_hover_tooltip(id, sub, settings);
            self.tooltip_text = Some(TooltipTextCache {
                id,
                sub,
                settings_key,
                text,
                checked: now,
            });
        }
        &self.tooltip_text.as_ref().unwrap().text
    }

    fn build_hover_tooltip(&self, id: &str, sub: Option<u8>, settings: &DockSettings) -> String {
        if let Some(ai) = telemetry::ai_provider_meta(id) {
            let status = if telemetry::ai_provider_detected(ai.id) {
                loc::t("AiUsageAccountConnected")
            } else {
                loc::t("AiUsageStatusSignIn")
            };
            return format!("{} · {status}", ai.name);
        }
        match id {
            "clock" => telemetry::local_clock_info().full_date,
            "media" => {
                let has_media = !self.media_snapshot().title.is_empty();
                if !has_media {
                    match sub {
                        Some(2 | 3) => loc::t("MediaPlay").to_owned(),
                        _ => loc::t("DynamicIslandShortMedia").to_owned(),
                    }
                } else {
                    match sub {
                        Some(0) => loc::t("MediaPrevious").to_owned(),
                        Some(1) => loc::t("MediaPlayPause").to_owned(),
                        Some(2) => loc::t("MediaNext").to_owned(),
                        _ => loc::t("DynamicIslandShortMedia").to_owned(),
                    }
                }
            }
            "quicksettings" => match sub {
                Some(0) => loc::t_fmt(
                    "RadioWifiStatus",
                    &[&radio_label(self.wifi_on, self.radio_loading)],
                ),
                Some(1) => loc::t_fmt(
                    "RadioBluetoothStatus",
                    &[&radio_label(self.bluetooth_on, self.radio_loading)],
                ),
                Some(2) => loc::t_fmt(
                    "NightLightStatus",
                    &[&if self.night_light_on {
                        loc::t("QuickOn")
                    } else {
                        loc::t("QuickOff")
                    }],
                ),
                Some(3) => loc::t("QuickSettingsPanel"),
                _ => loc::t("WidgetQuickSettings").to_owned(),
            },
            "audio" => {
                let dev = telemetry::default_audio_device_name(false)
                    .unwrap_or_else(|| loc::t("AudioOutput").to_owned());
                if self.audio_muted {
                    loc::t_fmt("AudioDeviceMuted", &[&dev])
                } else {
                    loc::t_fmt("AudioDeviceVolume", &[&dev, &self.audio_vol.to_string()])
                }
            }
            "weather" => {
                let snap = self
                    .weather
                    .lock()
                    .ok()
                    .map(|g| g.clone())
                    .unwrap_or_default();
                if !snap.temp_c.is_empty() {
                    let (temp, _, _) = weather_display(&snap, settings);
                    format!("{} · {} · {}", snap.city, temp, snap.condition)
                } else {
                    loc::t_fmt("WeatherTooltip", &[&settings.weather_city])
                }
            }
            "stats" => {
                let history = self.stats.cpu_history.as_vec();
                let avg = if history.is_empty() {
                    0.0
                } else {
                    history.iter().sum::<f32>() / history.len() as f32
                };
                let gpu = if gpu_metrics_visible(self.gpu_samples, self.gpu_misses) {
                    match self.gpu_temp_c {
                        Some(t) => loc::t_fmt(
                            "StatsGpuTooltipWithTemp",
                            &[&self.gpu_pct.to_string(), &t.to_string()],
                        ),
                        None => loc::t_fmt("StatsGpuTooltip", &[&self.gpu_pct.to_string()]),
                    }
                } else {
                    String::new()
                };
                loc::t_fmt(
                    "StatsTooltip",
                    &[
                        &self.cpu_pct.to_string(),
                        &format!("{avg:.0}"),
                        &self.ram_pct.to_string(),
                        &gpu,
                        &loc::t("StatsUptime"),
                        &self.stats_uptime,
                    ],
                )
            }
            "dayprogress" => {
                let (day, _, _, left) = day_progress_metrics();
                loc::t_fmt(
                    "WidgetDayProgressTooltip",
                    &[&format!("{:.0}", day * 100.0), &left.to_string()],
                )
            }
            "stopwatch" => {
                if sub == Some(1) {
                    if self.stopwatch_running {
                        loc::t("TimedWidgetPause")
                    } else {
                        loc::t("TimedWidgetStart")
                    }
                    .to_owned()
                } else {
                    format!(
                        "{} · {}",
                        loc::t("WidgetStopwatch"),
                        format_mm_secs(self.stopwatch_secs)
                    )
                }
            }
            "countdown" => {
                if sub == Some(1) {
                    if self.countdown_running {
                        loc::t("TimedWidgetPause")
                    } else {
                        loc::t("TimedWidgetStart")
                    }
                    .to_owned()
                } else {
                    format!(
                        "{} · {}",
                        loc::t("WidgetCountdown"),
                        format_mm_secs(self.countdown_remaining_secs)
                    )
                }
            }
            "focus" => {
                if sub == Some(1) {
                    if self.focus_running {
                        loc::t("TimedWidgetPause")
                    } else {
                        loc::t("TimedWidgetStart")
                    }
                    .to_owned()
                } else {
                    format!(
                        "{} · {}",
                        loc::t("WidgetFocus"),
                        format_mm_secs(self.focus_remaining_secs)
                    )
                }
            }
            "keyboard" => loc::t_fmt("KeyboardLayoutTooltip", &[&self.current_keyboard_code()]),
            "storage" => self
                .poll_snapshot
                .fixed_drives()
                .iter()
                .take(3)
                .map(|d| format!("{}: {}", d.letter, d.format_capacity_pair()))
                .collect::<Vec<_>>()
                .join("  |  "),
            "netspeed" => format!(
                "↓ {} · ↑ {}",
                telemetry::format_speed(self.net.rx_bps),
                telemetry::format_speed(self.net.tx_bps)
            ),
            "battery" => {
                let b = sample_battery();
                if !b.has_battery {
                    loc::t("BatteryUnavailable").to_owned()
                } else if b.charging {
                    loc::t_fmt("BatteryChargingPercent", &[&b.percent.to_string()])
                } else if b.on_ac {
                    loc::t_fmt("BatteryPluggedIn", &[&b.percent.to_string()])
                } else if let Some(secs) = b.life_secs.filter(|&s| s >= 60) {
                    let h = secs / 3600;
                    let m = (secs % 3600) / 60;
                    if h > 0 {
                        loc::t_fmt(
                            "BatteryRemainingHours",
                            &[&b.percent.to_string(), &h.to_string(), &m.to_string()],
                        )
                    } else {
                        loc::t_fmt(
                            "BatteryRemainingMinutes",
                            &[&b.percent.to_string(), &m.to_string()],
                        )
                    }
                } else {
                    loc::t_fmt("BatteryPercent", &[&b.percent.to_string()])
                }
            }
            "bluetoothbattery" => loc::t("BluetoothBattery").to_owned(),
            _ => widget_name(id),
        }
    }

    /// Left-click dispatch: inline transport/toggle sub-buttons execute directly; clicking
    /// any flyout-enabled widget body opens its flyout so every flyout is reachable in Click mode.
    pub fn handle_direct_click(
        &mut self,
        id: &str,
        rel_x_dips: f32,
        width_dips: f32,
        _settings: &mut DockSettings,
    ) -> DirectClickOutcome {
        if custom_widgets::is_custom(id) {
            return DirectClickOutcome::OpenFlyout;
        }
        if telemetry::ai_provider_meta(id).is_some() {
            return DirectClickOutcome::OpenFlyout;
        }
        match id {
            "media" => {
                let has_media = !self.media_snapshot().title.is_empty();
                if !has_media {
                    let play_l = (width_dips - 38.0).max(0.0);
                    if rel_x_dips >= play_l {
                        telemetry::send_media_vk(VK_MEDIA_PLAY_PAUSE);
                    }
                    DirectClickOutcome::Handled
                } else {
                    let controls_l = (width_dips - 88.0).max(0.0);
                    if rel_x_dips >= controls_l && rel_x_dips < controls_l + 26.0 {
                        telemetry::send_media_vk(VK_MEDIA_PREV_TRACK);
                    } else if rel_x_dips >= controls_l + 27.0 && rel_x_dips < controls_l + 55.0 {
                        telemetry::send_media_vk(VK_MEDIA_PLAY_PAUSE);
                    } else if rel_x_dips >= controls_l + 56.0 && rel_x_dips < controls_l + 82.0 {
                        telemetry::send_media_vk(VK_MEDIA_NEXT_TRACK);
                    }
                    DirectClickOutcome::Handled
                }
            }
            "quicksettings" => {
                // 4 interactive zones: Wi-Fi, Bluetooth, Night Light, and Control Center chevron.
                let fourth = (width_dips / 4.0).max(1.0);
                if rel_x_dips < fourth {
                    self.toggle_radio("wifi");
                    DirectClickOutcome::Handled
                } else if rel_x_dips < fourth * 2.0 {
                    self.toggle_radio("bluetooth");
                    DirectClickOutcome::Handled
                } else if rel_x_dips < fourth * 3.0 {
                    if telemetry::toggle_night_light() {
                        self.night_light_on = !self.night_light_on;
                    } else {
                        pins::launch("ms-settings:nightlight".to_owned());
                    }
                    DirectClickOutcome::Handled
                } else {
                    DirectClickOutcome::OpenFlyout
                }
            }
            "stopwatch" => {
                if rel_x_dips >= width_dips - 36.0 {
                    self.stopwatch_running = !self.stopwatch_running;
                    DirectClickOutcome::Handled
                } else {
                    DirectClickOutcome::OpenFlyout
                }
            }
            "countdown" => {
                if rel_x_dips >= width_dips - 36.0 {
                    if self.countdown_remaining_secs == 0 {
                        self.countdown_remaining_secs = self.countdown_total_secs;
                    }
                    self.countdown_running = !self.countdown_running;
                    DirectClickOutcome::Handled
                } else {
                    DirectClickOutcome::OpenFlyout
                }
            }
            "focus" => {
                if rel_x_dips >= width_dips - 36.0 {
                    self.focus.with_mut(|s| s.toggle());
                    self.sync_focus_mirrors();
                    DirectClickOutcome::Handled
                } else {
                    DirectClickOutcome::OpenFlyout
                }
            }
            "battery" => {
                // Exact reference BatteryWidget.OpenPowerSettingsAction (no flyout).
                pins::launch("ms-settings:batterysaver".to_owned());
                DirectClickOutcome::Handled
            }
            "clock" | "audio" | "weather" | "dayprogress" | "stats" | "netspeed"
            | "bluetoothbattery" | "storage" | "keyboard" | "aiusage_all" | "stacks" | "groups"
            | "clipboard" | "shelf" | "todo" | "agents" => DirectClickOutcome::OpenFlyout,
            _ => DirectClickOutcome::Handled,
        }
    }

    /// Middle-click quick actions on dock widget chips (mute, play/pause, lap/reset, force refresh,
    /// or jump directly to the system tool). Returns `true` when the widget handled the click.
    pub fn handle_middle_click(&mut self, widget_id: &str, settings: &DockSettings) -> bool {
        let id = canonical_widget_id(widget_id);
        if let Some(ai) = telemetry::ai_provider_meta(id) {
            telemetry::refresh_ai_provider(&self.ai_usage, ai.id, true);
            self.refresh_ai_full(ai.id);
            return true;
        }
        match id {
            "audio" => {
                toggle_audio_mute();
                (self.audio_vol, self.audio_muted) = get_audio_state();
                self.tooltip_text = None;
                true
            }
            "media" => {
                telemetry::send_media_vk(VK_MEDIA_PLAY_PAUSE);
                true
            }
            "stopwatch" => {
                if self.stopwatch_running {
                    self.stopwatch_laps
                        .push((self.stopwatch_secs, self.stopwatch_cs));
                } else {
                    self.stopwatch_secs = 0;
                    self.stopwatch_cs = 0;
                    self.stopwatch_debt_secs = 0.0;
                    self.stopwatch_laps.clear();
                }
                true
            }
            "countdown" => {
                self.countdown_running = false;
                self.countdown_remaining_secs = self.countdown_total_secs;
                true
            }
            "focus" => {
                self.focus.with_mut(|s| s.reset());
                self.sync_focus_mirrors();
                true
            }
            "stats" => {
                pins::launch("taskmgr.exe".to_owned());
                true
            }
            "netspeed" => {
                pins::launch("ms-settings:network".to_owned());
                true
            }
            "storage" => {
                pins::launch("explorer.exe".to_owned());
                true
            }
            "weather" => {
                telemetry::refresh_weather(&self.weather, &settings.weather_city, true);
                self.weather_retry_at =
                    Some(Instant::now() + std::time::Duration::from_secs(30 * 60));
                true
            }
            "aiusage_all" => {
                for ai in telemetry::AI_PROVIDERS {
                    telemetry::refresh_ai_provider(&self.ai_usage, ai.id, true);
                    self.refresh_ai_full(ai.id);
                }
                true
            }
            _ => false,
        }
    }

    /// Quick secondary (right-click) shortcuts matching reference widget context menus.
    pub fn handle_secondary_click(
        &mut self,
        id: &str,
        settings: &mut DockSettings,
    ) -> Option<DirectClickOutcome> {
        match id {
            "stopwatch" => {
                self.stopwatch_running = false;
                self.stopwatch_secs = 0;
                self.stopwatch_cs = 0;
                self.stopwatch_debt_secs = 0.0;
                self.stopwatch_laps.clear();
                Some(DirectClickOutcome::Handled)
            }
            "countdown" => {
                self.countdown_running = false;
                self.countdown_remaining_secs = self.countdown_total_secs;
                Some(DirectClickOutcome::Handled)
            }
            "focus" => {
                // Reset the shared session, then mirror its values back. Resetting only the
                // legacy mirror let the next tick restore the still-running shared session.
                self.focus.with_mut(|s| s.reset());
                self.sync_focus_mirrors();
                Some(DirectClickOutcome::Handled)
            }
            "audio" => {
                pins::launch("ms-settings:sound".to_owned());
                Some(DirectClickOutcome::Handled)
            }
            "quicksettings" => {
                settings.focus_mode = !settings.focus_mode;
                Some(DirectClickOutcome::SettingsChanged)
            }
            _ => None,
        }
    }

    /// Maps a clicked calendar grid cell (`day_id_base + cell`, `0..42`) to a date.
    /// The leading blanks come from the culture's first day of week, matching the
    /// flyout grid; a hard-coded Monday offset silently shifted every click in
    /// Sunday-first (en-US) and Saturday-first (Arabic) cultures.
    pub(crate) fn calendar_cell_date(
        year: i32,
        month: u32,
        cell: i32,
        first_day_of_week: u32,
    ) -> (i32, u32, u32) {
        let first_wd = telemetry::weekday_of(year, month, 1);
        let leading = telemetry::calendar_leading_blanks(first_day_of_week, first_wd) as i32;
        let prev_m = if month == 1 { 12 } else { month - 1 };
        let prev_y = if month == 1 { year - 1 } else { year };
        let prev_days = telemetry::days_in_month(prev_y, prev_m) as i32;
        let cur_days = telemetry::days_in_month(year, month) as i32;
        let next_m = if month == 12 { 1 } else { month + 1 };
        let next_y = if month == 12 { year + 1 } else { year };
        let d_idx = cell - leading + 1;
        if d_idx < 1 {
            (prev_y, prev_m, (prev_days + d_idx) as u32)
        } else if d_idx > cur_days {
            (next_y, next_m, (d_idx - cur_days) as u32)
        } else {
            (year, month, d_idx as u32)
        }
    }

    fn compact_tz_badge(tz_name: &str, tz_label: &str) -> String {
        let offset = tz_label.trim();
        if !offset.is_empty() {
            return offset.to_owned();
        }
        let name = tz_name.trim();
        if !name.is_empty() && name.chars().count() <= 10 {
            name.to_owned()
        } else {
            "UTC".to_owned()
        }
    }

    fn friendly_app_session_name(app_name: &str) -> String {
        let trimmed = app_name.trim();
        if trimmed.is_empty()
            || trimmed.eq_ignore_ascii_case("SystemSounds")
            || trimmed.eq_ignore_ascii_case("System Sounds")
        {
            loc::t("SystemSounds")
        } else {
            trimmed.to_owned()
        }
    }

    fn build_calendar_spec(
        shown_y: i32,
        shown_m: u32,
        selected: Option<(i32, u32, u32)>,
    ) -> FlyoutSpec {
        let info = telemetry::local_clock_info();
        let today = (info.year, info.month, info.day);
        FlyoutSpec {
            title: String::new(),
            subtitle: None,
            width_dips: 316.0,
            blocks: vec![
                FlyoutBlock::Hero {
                    value: info.time_hm,
                    caption: Some(info.full_date.clone()),
                    centered: false,
                    large: false,
                    right_meta: Some(Self::compact_tz_badge(&info.tz_name, &info.tz_label)),
                },
                FlyoutBlock::Separator,
                FlyoutBlock::CalendarGrid {
                    year: shown_y,
                    month: shown_m,
                    today,
                    selected,
                    prev_year_id: 1,
                    prev_month_id: 2,
                    title_id: 3,
                    next_month_id: 4,
                    next_year_id: 5,
                    day_id_base: 100,
                },
                FlyoutBlock::Separator,
                FlyoutBlock::StatRow {
                    label: loc::t_fmt("TodayDate", &[&info.full_date]),
                    value: String::new(),
                },
                FlyoutBlock::ButtonBar(vec![
                    (10, loc::t("GoToToday"), false),
                    (11, loc::t("LauncherDateTime"), false),
                ]),
            ],
        }
    }

    fn build_audio_mixer_spec(&self) -> FlyoutSpec {
        let (out_vol, out_muted) = get_audio_state();
        let out_name =
            telemetry::default_audio_device_name(false).unwrap_or_else(|| loc::t("Speakers"));
        let mic = telemetry::get_mic_state();
        let (in_vol, in_muted) = mic.unwrap_or((0, true));
        let in_name =
            telemetry::default_audio_device_name(true).unwrap_or_else(|| loc::t("NoMicDevice"));

        let mut blocks = vec![
            FlyoutBlock::AudioSliderSection {
                label: loc::t("AudioOutput"),
                device_name: out_name.clone(),
                value_text: if out_muted {
                    format_percent(0)
                } else {
                    format_percent(out_vol)
                },
                ratio: out_vol as f32 / 100.0,
                muted: out_muted,
                mute_id: 1,
                slider_id: 2,
            },
            FlyoutBlock::Separator,
            FlyoutBlock::AudioSliderSection {
                label: loc::t("AudioInput"),
                device_name: in_name.clone(),
                value_text: if mic.is_none() {
                    "-".to_owned()
                } else if in_muted {
                    format_percent(0)
                } else {
                    format_percent(in_vol)
                },
                ratio: if mic.is_some() {
                    in_vol as f32 / 100.0
                } else {
                    0.0
                },
                muted: in_muted || mic.is_none(),
                mute_id: 3,
                slider_id: 4,
            },
        ];
        // Per-app rows populate `device_name` with the friendly app/session name (never raw session IDs or empty box).
        let sessions = media_ext::list_app_sessions();
        if !sessions.is_empty() {
            blocks.push(FlyoutBlock::Separator);
            for (index, session) in sessions.iter().take(6).enumerate() {
                blocks.push(FlyoutBlock::AudioSliderSection {
                    label: if index == 0 {
                        loc::t("AudioApps")
                    } else {
                        String::new()
                    },
                    device_name: Self::friendly_app_session_name(&session.app_name),
                    value_text: if session.muted {
                        format_percent(0)
                    } else {
                        format_percent(session.volume)
                    },
                    ratio: session.volume as f32 / 100.0,
                    muted: session.muted,
                    mute_id: 100 + index as u32 * 2,
                    slider_id: 101 + index as u32 * 2,
                });
            }
        }
        let append_devices = |blocks: &mut Vec<FlyoutBlock>,
                              devices: &[(String, String)],
                              id_base: u32,
                              glyph: &str,
                              active_name: &str,
                              heading_key: &str| {
            if devices.is_empty() {
                return;
            }
            blocks.push(FlyoutBlock::StatRow {
                label: loc::t(heading_key),
                value: String::new(),
            });
            blocks.extend(devices.iter().enumerate().take(6).map(|(i, (_, name))| {
                FlyoutBlock::BadgeRow {
                    id: id_base + i as u32,
                    code: glyph.to_owned(),
                    title: name.clone(),
                    active: name.eq_ignore_ascii_case(active_name),
                }
            }));
            blocks.push(FlyoutBlock::Separator);
        };
        append_devices(
            &mut blocks,
            &audio_output_devices(),
            200,
            "\u{E7F5}",
            &out_name,
            "AudioOutputDevices",
        );
        append_devices(
            &mut blocks,
            &audio_input_devices(),
            300,
            "\u{E720}",
            &in_name,
            "AudioInputDevices",
        );
        blocks.push(FlyoutBlock::ButtonBar(vec![(
            5,
            loc::t_fmt("RadioSettings", &[&loc::t("LauncherSound")]),
            false,
        )]));
        FlyoutSpec {
            title: loc::t("WidgetAudio"),
            subtitle: None,
            width_dips: 314.0,
            blocks,
        }
    }

    fn build_stopwatch_spec(&self) -> FlyoutSpec {
        let mut blocks = vec![
            FlyoutBlock::Hero {
                value: format_mm_ss_cs(self.stopwatch_secs, self.stopwatch_cs),
                caption: Some(loc::t("StopwatchQuickAction")),
                centered: true,
                large: true,
                right_meta: None,
            },
            FlyoutBlock::ButtonBar(vec![(3, loc::t("StopwatchLap"), false)]),
        ];
        if self.stopwatch_laps.is_empty() {
            blocks.push(FlyoutBlock::Note(loc::t("StopwatchNoLaps")));
        } else {
            for (i, &(lap_secs, lap_cs)) in self.stopwatch_laps.iter().rev().take(6).enumerate() {
                let num = self.stopwatch_laps.len() - i;
                blocks.push(FlyoutBlock::StatRow {
                    label: format!("{num:02}"),
                    value: format_mm_ss_cs(lap_secs, lap_cs),
                });
            }
        }
        blocks.push(FlyoutBlock::Separator);
        blocks.push(FlyoutBlock::ButtonBar(vec![
            (
                1,
                if self.stopwatch_running {
                    loc::t("TimedWidgetPause")
                } else {
                    loc::t("TimedWidgetStart")
                },
                true,
            ),
            (2, loc::t("Reset"), false),
        ]));
        FlyoutSpec {
            title: loc::t("WidgetStopwatch"),
            subtitle: None,
            width_dips: 300.0,
            blocks,
        }
    }

    fn build_countdown_spec(&self) -> FlyoutSpec {
        let ratio = if self.countdown_total_secs > 0 {
            (self.countdown_remaining_secs as f32 / self.countdown_total_secs as f32)
                .clamp(0.0, 1.0)
        } else {
            0.0
        };
        let mins = self.countdown_total_secs / 60;
        FlyoutSpec {
            title: loc::t("WidgetCountdown"),
            subtitle: None,
            width_dips: 300.0,
            blocks: vec![
                FlyoutBlock::Hero {
                    value: format_mm_secs(self.countdown_remaining_secs),
                    caption: None,
                    centered: true,
                    large: true,
                    right_meta: None,
                },
                FlyoutBlock::ProgressBar {
                    label: String::new(),
                    value_text: String::new(),
                    ratio,
                    tone: BarTone::Accent,
                },
                FlyoutBlock::ButtonBar(vec![
                    (5, loc::t_fmt("CountdownMinutesShort", &["5"]), mins == 5),
                    (10, loc::t_fmt("CountdownMinutesShort", &["10"]), mins == 10),
                ]),
                FlyoutBlock::ButtonBar(vec![
                    (15, loc::t_fmt("CountdownMinutesShort", &["15"]), mins == 15),
                    (30, loc::t_fmt("CountdownMinutesShort", &["30"]), mins == 30),
                ]),
                // `TimedWidgetPanel` custom duration (C2 TextInput, validated 1-1440 minutes).
                FlyoutBlock::TextInput {
                    id: 50,
                    label: loc::t("CountdownCustomMinutes"),
                    value: format!("{mins}"),
                    numeric: true,
                    min: 1.0,
                    max: 1440.0,
                },
                FlyoutBlock::Separator,
                FlyoutBlock::ButtonBar(vec![
                    (
                        1,
                        if self.countdown_running {
                            loc::t("TimedWidgetPause")
                        } else {
                            loc::t("TimedWidgetStart")
                        },
                        true,
                    ),
                    (2, loc::t("Reset"), false),
                ]),
            ],
        }
    }

    /// Unified `SystemStatsWidget` flyout: localized metrics, hysteresis-gated GPU meters,
    /// `Chart`/`Sparkline` series and cached top processes (sampled off the UI thread).
    fn build_stats_spec(&self, settings: &DockSettings, is_dark: bool) -> FlyoutSpec {
        let pal = design::palette(is_dark);
        let top = self
            .stats_top
            .lock()
            .map(|guard| guard.clone())
            .unwrap_or_default();
        let (_, used_gib, total_gib) = telemetry::sample_ram_info();
        let cores = std::thread::available_parallelism()
            .map(|n| n.get() as u64)
            .unwrap_or(1);
        let cpu_col = threshold_color(&pal, self.cpu_pct, pal.widget_blue);
        let ram_col = threshold_color(&pal, self.ram_pct, pal.widget_accent);
        let mut blocks = vec![
            FlyoutBlock::Hero {
                value: self.stats_uptime.clone(),
                caption: Some(loc::t("StatsUptime")),
                centered: false,
                large: false,
                right_meta: None,
            },
            FlyoutBlock::StatRow {
                label: "CPU".to_owned(),
                value: format!(
                    "{} · {}",
                    format_percent(self.cpu_pct),
                    loc::t_fmt("StatsLogicalCores", &[&cores.to_string()])
                ),
            },
            FlyoutBlock::StatRow {
                label: "RAM".to_owned(),
                value: format!(
                    "{} · {} · {}",
                    format_percent(self.ram_pct),
                    loc::t_fmt(
                        "StatsMemoryUsed",
                        &[&telemetry::format_bytes(
                            (used_gib * 1_073_741_824.0) as u64
                        )]
                    ),
                    loc::t_fmt(
                        "StatsMemoryAvailable",
                        &[&telemetry::format_bytes(
                            ((total_gib - used_gib).max(0.0) * 1_073_741_824.0) as u64
                        )]
                    )
                ),
            },
            // C2 `Chart` blocks with named CPU / RAM series (WS-C contract).
            FlyoutBlock::Chart {
                label: "CPU".to_owned(),
                series: vec![("CPU".to_owned(), self.stats.cpu_history.as_vec())],
                accent: cpu_col,
            },
            FlyoutBlock::Chart {
                label: loc::t_fmt(
                    "StatsMemoryChart",
                    &[&format!("{used_gib:.1}"), &format!("{total_gib:.0}")],
                ),
                series: vec![("RAM".to_owned(), self.stats.ram_history.as_vec())],
                accent: ram_col,
            },
        ];
        let temp_unit = weather_unit(settings);
        if gpu_metrics_visible(self.gpu_samples, self.gpu_misses) {
            let gpu_history = if self.stats.gpu_history.as_vec().is_empty() {
                vec![self.gpu_pct as f32 / 100.0; 45]
            } else {
                self.stats.gpu_history.as_vec()
            };
            blocks.push(FlyoutBlock::Chart {
                label: "GPU".to_owned(),
                series: vec![("GPU".to_owned(), gpu_history)],
                accent: threshold_color(&pal, self.gpu_pct, pal.success),
            });
            if let Some(temp) = self.gpu_temp_c {
                // Hysteresis-gated band: warning turns the bar amber, critical red.
                let tone = match self.gpu_thermal_state {
                    2 => BarTone::Usage,
                    1 => BarTone::Accent,
                    _ => BarTone::Level,
                };
                blocks.push(FlyoutBlock::ProgressBar {
                    label: loc::t("GpuTemperature"),
                    value_text: telemetry::format_temperature(f64::from(temp), temp_unit),
                    ratio: (temp as f32 / 100.0).clamp(0.0, 1.0),
                    tone,
                });
            }
        }
        // Flyout values come from the 1 Hz cache sampled by `tick`, not a second UI-thread
        // probe — chip and flyout now always agree.
        if settings.stats_meters & 16 != 0 && gpu_metrics_visible(self.gpu_samples, self.gpu_misses)
        {
            let value = if let Some(temp) = self.gpu_temp_c {
                format!(
                    "{} · {} · {}",
                    format_percent(self.gpu_pct),
                    telemetry::format_temperature(f64::from(temp), temp_unit),
                    self.gpu_engine_name
                )
            } else {
                format!(
                    "{} · {}",
                    format_percent(self.gpu_pct),
                    self.gpu_engine_name
                )
            };
            blocks.insert(
                3,
                FlyoutBlock::StatRow {
                    label: loc::t("Gpu"),
                    value,
                },
            );
        } else if settings.stats_meters & 32 != 0
            && let Some(temp) = self.gpu_temp_c
        {
            blocks.insert(
                3,
                FlyoutBlock::StatRow {
                    label: loc::t("GpuTemperature"),
                    value: telemetry::format_temperature(f64::from(temp), temp_unit),
                },
            );
        }
        blocks.push(FlyoutBlock::Separator);
        if top.is_empty() {
            // Wait for the background sampler; never do disk/process I/O on the UI thread.
            let sampling = self.stats_top_in_flight.load(Ordering::Relaxed)
                || self
                    .last_stats_top
                    .is_some_and(|t| t.elapsed() < Duration::from_secs(10));
            blocks.push(FlyoutBlock::Note(loc::t(if sampling {
                "StatsSamplingProcesses"
            } else {
                "StatsTopProcessesEmpty"
            })));
        } else {
            blocks.push(FlyoutBlock::StatRow {
                label: loc::t("StatsTopProcesses"),
                value: String::new(),
            });
            for (name, bytes) in top {
                blocks.push(FlyoutBlock::GlyphStatRow {
                    glyph: 0xE7FC,
                    glyph_color: pal.widget_accent,
                    label: name,
                    detail: None,
                    value: telemetry::format_bytes(bytes),
                });
            }
        }
        blocks.push(FlyoutBlock::Separator);
        blocks.push(FlyoutBlock::ButtonBar(vec![
            (1, loc::t("TaskManager"), false),
            (2, loc::t("ResourceMonitor"), false),
            (3, loc::t("AiUsageRefresh"), false),
        ]));
        blocks.push(FlyoutBlock::Note(loc::t("StatsAvailabilityNote")));
        FlyoutSpec {
            title: loc::t("WidgetSystemStats"),
            subtitle: None,
            width_dips: 320.0,
            blocks,
        }
    }

    /// WS-1: `open_widget_flyout`'s return value means "persist settings and reload
    /// them live". Only a real `LiveEvent::Click` / `TextCommit` that actually
    /// changed a setting may return `true`; opening a flyout on hover must never
    /// do so (on the dock UI thread that chain blocks the WH_MOUSE_LL hook owner).
    fn flyout_settings_changed(id: &str, live_change: bool) -> bool {
        live_change && matches!(id, "quicksettings" | "bluetoothbattery")
    }

    pub fn open_widget_flyout(
        &mut self,
        id: &str,
        owner_hwnd: usize,
        is_dark: bool,
        anchor_x: i32,
        anchor_y: i32,
        settings: &mut DockSettings,
    ) -> bool {
        self.open_widget_flyout_on_edge(
            id,
            owner_hwnd,
            is_dark,
            anchor_x,
            anchor_y,
            flyout::FlyoutEdge::Bottom,
            settings,
        )
    }

    #[allow(clippy::too_many_arguments)]
    pub fn open_widget_flyout_on_edge(
        &mut self,
        id: &str,
        owner_hwnd: usize,
        is_dark: bool,
        anchor_x: i32,
        anchor_y: i32,
        edge: flyout::FlyoutEdge,
        settings: &mut DockSettings,
    ) -> bool {
        if !widget_has_flyout(id) {
            return false;
        }
        let border = BorderStyle::from_widget_settings(is_dark, settings);
        let options = flyout::FlyoutOptions::default().with_edge(edge);
        let show_primary = |spec: FlyoutSpec| {
            flyout::show_primary_with_options(
                owner_hwnd, is_dark, anchor_x, anchor_y, spec, border, options,
            )
        };
        macro_rules! show_live {
            ($wid:expr, $init:expr, $handler:expr) => {
                flyout::show_flyout_live_with_options(
                    $wid, owner_hwnd, is_dark, anchor_x, anchor_y, $init, border, options, $handler,
                )
            };
        }
        let toggle_app_mute = |idx: usize| {
            if let Some(session) = media_ext::list_app_sessions().get(idx) {
                let _ = media_ext::set_app_mute(&session.session_id, !session.muted);
            }
        };
        let set_app_vol = |idx: usize, ratio: f32| {
            if let Some(session) = media_ext::list_app_sessions().get(idx) {
                let _ =
                    media_ext::set_app_volume(&session.session_id, (ratio * 100.0).round() as u32);
            }
        };

        if custom_widgets::is_custom(id) {
            let init = custom_widgets::flyout(id).unwrap_or_else(|| FlyoutSpec {
                title: widget_name(id),
                subtitle: None,
                width_dips: 280.0,
                blocks: vec![FlyoutBlock::Note(loc::t("AiUsageStatusLoading"))],
            });
            show_live!(id, init.clone(), |ev| match ev {
                LiveEvent::Click(action_id) => {
                    custom_widgets::click_widget(id, action_id, settings);
                    LiveOutcome::Close
                }
                _ =>
                    LiveOutcome::Update(custom_widgets::flyout(id).unwrap_or_else(|| init.clone()),),
            });
            return false;
        }

        if id == "aiusage_all" {
            let mut blocks = Vec::new();
            let guard_map = self
                .ai_usage
                .lock()
                .ok()
                .map(|g| g.providers.clone())
                .unwrap_or_default();
            for ai in telemetry::AI_PROVIDERS {
                let snap = guard_map.get(ai.id).cloned().unwrap_or_default();
                if let Some(w) = snap.windows.first() {
                    blocks.push(FlyoutBlock::ProgressBar {
                        label: format!("{} ({})", ai.name, w.label),
                        value_text: format!(
                            "{} · {}",
                            format_percent((w.used * 100.0).round() as u32),
                            w.reset_text
                        ),
                        ratio: w.used,
                        tone: BarTone::Usage,
                    });
                } else if let Some(bal) = snap.balance_text {
                    blocks.push(FlyoutBlock::StatRow {
                        label: ai.name.to_owned(),
                        value: bal,
                    });
                } else {
                    blocks.push(FlyoutBlock::StatRow {
                        label: ai.name.to_owned(),
                        value: loc::t(if snap.has_cred {
                            "AiUsageConfigured"
                        } else {
                            "AiUsageStatusSignIn"
                        }),
                    });
                }
            }
            blocks.push(FlyoutBlock::Separator);
            blocks.push(FlyoutBlock::ButtonBar(vec![
                (1, loc::t("AiUsageRefreshAll"), true),
                (2, loc::t("AiUsageFullRefreshAll"), false),
            ]));
            let spec = FlyoutSpec {
                title: loc::t("WidgetCategoryAi"),
                subtitle: Some(loc::t("AiUsageLiveUsageNote")),
                width_dips: 316.0,
                blocks,
            };
            match show_primary(spec) {
                Some(1) => {
                    for ai in telemetry::AI_PROVIDERS {
                        telemetry::refresh_ai_provider(&self.ai_usage, ai.id, true);
                    }
                }
                Some(2) => {
                    for ai in telemetry::AI_PROVIDERS {
                        self.refresh_ai_full(ai.id);
                    }
                }
                _ => {}
            }
        }
        // Full `AiUsageWidget.BuildFlyout` — spend / breakdown / reset / plan chip /
        // severity dot / refresh. Full detail is read from the shared AI fetch cache.
        else if let Some(ai) = telemetry::ai_provider_meta(id) {
            let snap = self
                .ai_usage
                .lock()
                .ok()
                .and_then(|g| g.providers.get(ai.id).cloned())
                .unwrap_or_default();
            // Kick off a full fetch when there is no cached payload yet; the returned flyout uses
            // the cache as soon as the background fetch lands (no `AiFullDetail::default()`).
            if self.ai_full_detail(ai.id).is_none() {
                self.refresh_ai_full(ai.id);
            }
            let detail = self.ai_flyout_detail(ai.id, &snap);
            let spec = ai_flyout::build_ai_flyout(ai.id, &snap, &detail, settings, is_dark);
            match show_primary(spec) {
                Some(1) => pins::launch(ai.dashboard_url.to_owned()),
                Some(2) => telemetry::refresh_ai_provider(&self.ai_usage, ai.id, true),
                Some(3) => self.refresh_ai_full(ai.id),
                _ => {}
            }
        }

        match id {
            // System stats flyout: uptime, memory, GPU, charts and top processes (union).
            "stats" => {
                let init = self.build_stats_spec(settings, is_dark);
                show_live!("stats", init, |ev| match ev {
                    LiveEvent::Click(1) => {
                        pins::launch("taskmgr.exe".to_owned());
                        LiveOutcome::Close
                    }
                    LiveEvent::Click(2) => {
                        pins::launch("perfmon.exe".to_owned());
                        LiveOutcome::Close
                    }
                    LiveEvent::Click(3) => {
                        (self.cpu_pct, self.ram_pct) = self.stats.sample();
                        self.stats_uptime = telemetry::system_uptime_string();
                        self.invalidate_top_processes();
                        LiveOutcome::Update(self.build_stats_spec(settings, is_dark))
                    }
                    // WM_TIMER-driven LiveEvent::Update (WS-7) refreshes live values.
                    _ => LiveOutcome::Update(self.live_spec("stats", settings, is_dark)),
                });
            }
            // Exact `ClockWidget.BuildCalendar` (live interactive month/year nav & day selection)
            "clock" => {
                let info = telemetry::local_clock_info();
                let (mut shown_y, mut shown_m) = (info.year, info.month);
                let mut selected: Option<(i32, u32, u32)> = None;
                let init = Self::build_calendar_spec(shown_y, shown_m, selected);
                show_live!("clock", init, |ev| match ev {
                    LiveEvent::Click(11) => {
                        pins::launch("ms-settings:dateandtime".to_owned());
                        LiveOutcome::Close
                    }
                    LiveEvent::Click(btn @ (1..=5 | 10 | 100..=141)) => {
                        match btn {
                            1 => shown_y = (shown_y - 1).max(1970),
                            2 if shown_m == 1 => {
                                shown_m = 12;
                                shown_y = (shown_y - 1).max(1970);
                            }
                            2 => shown_m -= 1,
                            3 | 10 => {
                                let now = telemetry::local_clock_info();
                                shown_y = now.year;
                                shown_m = now.month;
                                selected = Some((now.year, now.month, now.day));
                            }
                            4 if shown_m == 12 => {
                                shown_m = 1;
                                shown_y = (shown_y + 1).min(2100);
                            }
                            4 => shown_m += 1,
                            5 => shown_y = (shown_y + 1).min(2100),
                            cell_id => {
                                let picked = Self::calendar_cell_date(
                                    shown_y,
                                    shown_m,
                                    (cell_id - 100) as i32,
                                    telemetry::first_day_of_week(),
                                );
                                selected = Some(picked);
                                shown_y = picked.0;
                                shown_m = picked.1;
                            }
                        }
                        LiveOutcome::Update(Self::build_calendar_spec(shown_y, shown_m, selected))
                    }
                    // Live tick: keep the clock/time header current.
                    _ => LiveOutcome::Update(self.live_spec("clock", settings, is_dark)),
                });
            }
            // Exact `AudioWidget.PopulateFlyout` (live interactive Output + Input mixer!)
            "audio" => {
                let init = self.build_audio_mixer_spec();
                show_live!("audio", init, |ev| match ev {
                    LiveEvent::Click(1) => {
                        toggle_audio_mute();
                        (self.audio_vol, self.audio_muted) = get_audio_state();
                        LiveOutcome::Update(self.build_audio_mixer_spec())
                    }
                    LiveEvent::Slider { id: 2, ratio } => {
                        set_audio_volume((ratio * 100.0).round() as u32);
                        (self.audio_vol, self.audio_muted) = get_audio_state();
                        LiveOutcome::Update(self.build_audio_mixer_spec())
                    }
                    LiveEvent::Click(3) => {
                        telemetry::toggle_mic_mute();
                        LiveOutcome::Update(self.build_audio_mixer_spec())
                    }
                    LiveEvent::Slider { id: 4, ratio } => {
                        telemetry::set_mic_volume((ratio * 100.0).round() as u32);
                        LiveOutcome::Update(self.build_audio_mixer_spec())
                    }
                    LiveEvent::Click(5) => {
                        pins::launch("ms-settings:sound".to_owned());
                        LiveOutcome::Close
                    }
                    // Per-app mixers from WS-G diagnostics (ids 100..199).
                    LiveEvent::Click(id) if (100..200).contains(&id) && id % 2 == 0 => {
                        toggle_app_mute(((id - 100) / 2) as usize);
                        LiveOutcome::Update(self.build_audio_mixer_spec())
                    }
                    LiveEvent::Slider { id, ratio } if (101..200).contains(&id) && id % 2 == 1 => {
                        set_app_vol(((id - 101) / 2) as usize, ratio);
                        LiveOutcome::Update(self.build_audio_mixer_spec())
                    }
                    // C4 `platform::core_audio::set_default_device` — output (200..=205) & input (300..=305).
                    LiveEvent::Click(id @ (200..=205 | 300..=305)) => {
                        let (devices, base) = if id < 300 {
                            (audio_output_devices(), 200)
                        } else {
                            (audio_input_devices(), 300)
                        };
                        if let Some((device_id, _)) = devices.get((id - base) as usize) {
                            crate::platform::core_audio::set_default_device(device_id);
                        }
                        LiveOutcome::Update(self.build_audio_mixer_spec())
                    }
                    _ => LiveOutcome::Close,
                });
                (self.audio_vol, self.audio_muted) = get_audio_state();
            }
            // Exact `WeatherWidget.BuildFlyoutContent`
            "weather" => {
                let snap = self.weather.lock().map(|g| g.clone()).unwrap_or_default();
                let dash_or = |s: &str| {
                    if s.is_empty() {
                        "—".to_owned()
                    } else {
                        s.to_owned()
                    }
                };
                let city = if snap.city.is_empty() {
                    settings.weather_city.clone()
                } else {
                    snap.city.clone()
                };
                // Chip, flyout and tooltip all use the same effective unit / formatting.
                let (temp, feels, today_range) = weather_display(&snap, settings);
                let unit = weather_unit(settings);
                let wind = snap.formatted_wind(unit);
                let mut blocks = vec![
                    FlyoutBlock::HeroHeader {
                        glyph: weather_icon_glyph(&snap.condition),
                        glyph_color: weather_icon_color(&snap.condition, is_dark),
                        title: if city.is_empty() {
                            loc::t("WidgetWeather")
                        } else {
                            city.clone()
                        },
                        subtitle: Some(if snap.condition.is_empty() {
                            loc::t("WidgetWeather")
                        } else {
                            snap.condition.clone()
                        }),
                        right_value: Some(if snap.temp_c.is_empty() {
                            "—".to_owned()
                        } else {
                            temp
                        }),
                    },
                    // Balanced 2×2 metric grid (Feels Like, Humidity, Wind, Today Hi/Lo).
                    FlyoutBlock::MetricTiles(vec![
                        (
                            loc::t("FeelsLikeLabel"),
                            if snap.feels_like.is_empty() {
                                "—".to_owned()
                            } else {
                                feels
                            },
                        ),
                        (loc::t("WeatherHumidity"), dash_or(&snap.humidity)),
                        (loc::t("WeatherWind"), dash_or(&wind)),
                        (loc::t("WeatherToday"), dash_or(&today_range)),
                    ]),
                    FlyoutBlock::StatRow {
                        label: loc::t("WeatherSource"),
                        value: "Open-Meteo".to_owned(),
                    },
                ];
                if !snap.forecast_days.is_empty() {
                    blocks.push(FlyoutBlock::Separator);
                    blocks.push(FlyoutBlock::StatRow {
                        label: loc::t("WeatherForecast3Days"),
                        value: String::new(),
                    });
                    for (i, (day, condition, high_low)) in
                        snap.forecast_days.iter().take(3).enumerate()
                    {
                        let value = if let Some((Some(hi), Some(lo))) = snap.forecast_hi_lo_c.get(i)
                        {
                            format!(
                                "{} / {}",
                                telemetry::format_temperature(*hi, unit),
                                telemetry::format_temperature(*lo, unit)
                            )
                        } else {
                            high_low.clone()
                        };
                        blocks.push(FlyoutBlock::GlyphStatRow {
                            glyph: weather_icon_glyph(condition),
                            glyph_color: weather_icon_color(condition, is_dark),
                            label: day.clone(),
                            detail: Some(condition.clone()),
                            value,
                        });
                    }
                }
                blocks.push(FlyoutBlock::Separator);
                blocks.push(FlyoutBlock::ButtonBar(vec![(
                    1,
                    loc::t("AiUsageRefresh"),
                    false,
                )]));
                let spec = FlyoutSpec {
                    title: String::new(),
                    subtitle: None,
                    width_dips: 312.0,
                    blocks,
                };
                if show_primary(spec) == Some(1) {
                    telemetry::refresh_weather(&self.weather, &settings.weather_city, true);
                    self.weather_retry_at =
                        Some(Instant::now() + std::time::Duration::from_secs(30 * 60));
                }
            }
            // `NetSpeedWidget`: live throughput + 6-stream speed test with cancel/retry/restart.
            "netspeed" => {
                let adapter = if self.net.adapter_name.is_empty() {
                    None
                } else {
                    Some(loc::t_fmt("NetworkAdapterLabel", &[&self.net.adapter_name]))
                };
                let build = |runtime: &Self| {
                    let mut spec = speedtest::build_speedtest_flyout(
                        runtime.speedtest.state(),
                        runtime.speedtest.result(),
                    );
                    spec.subtitle = adapter.clone();
                    // Live throughput + session totals above the test grid.
                    let mut blocks = vec![
                        FlyoutBlock::MetricTiles(vec![
                            (
                                loc::t_fmt("NetworkDirectionDown", &[&loc::t("NetworkDownload")]),
                                telemetry::format_speed(runtime.net.rx_bps),
                            ),
                            (
                                loc::t_fmt("NetworkDirectionUp", &[&loc::t("NetworkUpload")]),
                                telemetry::format_speed(runtime.net.tx_bps),
                            ),
                            (
                                loc::t("NetworkSessionReceived"),
                                telemetry::format_bytes(runtime.net.session_rx),
                            ),
                            (
                                loc::t("NetworkSessionSent"),
                                telemetry::format_bytes(runtime.net.session_tx),
                            ),
                        ]),
                        FlyoutBlock::Separator,
                    ];
                    blocks.append(&mut spec.blocks);
                    blocks.push(FlyoutBlock::ButtonBar(vec![(
                        5,
                        loc::t("NetworkSettings"),
                        false,
                    )]));
                    spec.blocks = blocks;
                    spec
                };
                let init = build(self);
                show_live!("netspeed", init, |ev| match ev {
                    LiveEvent::Click(btn @ (1 | 3 | 4)) => {
                        match btn {
                            1 => self.speedtest.start(),
                            3 => self.speedtest.retry(),
                            _ => self.speedtest.restart(),
                        }
                        let now = Instant::now();
                        self.speedtest_started_at = Some(now);
                        self.poll_speedtest(now);
                        LiveOutcome::Update(build(self))
                    }
                    LiveEvent::Click(2) => {
                        self.speedtest.cancel();
                        self.speedtest_started_at = None;
                        LiveOutcome::Update(build(self))
                    }
                    LiveEvent::Click(5) => {
                        pins::launch("ms-settings:network".to_owned());
                        LiveOutcome::Close
                    }
                    _ => {
                        self.poll_speedtest(Instant::now());
                        LiveOutcome::Update(build(self))
                    }
                });
            }
            // `BluetoothBatteryWidget.PopulateHoverFlyoutPanel` + device management.
            "bluetoothbattery" => {
                // WS-1: only a real live-event click/commit may request a settings save.
                // Hover-generated flyouts must leave this `false`.
                let mut settings_changed = false;
                // Selection, rename and hide bind to the stable physical device id: a list
                // that re-sorts between clicks never retargets the action to another device.
                let mut selected_id: Option<String> = None;
                let mut feedback: Option<String> = None;
                // Snapshot of rendered ActionRow ids (100 + idx) so a background re-sort
                // between render and click still resolves the exact row the user clicked.
                let rendered_row_ids: std::cell::RefCell<Vec<String>> =
                    std::cell::RefCell::new(Vec::new());
                let visible = |cfg: &DockSettings| -> Vec<BluetoothBatteryDevice> {
                    let ignored = setting_string_list(cfg, "IgnoredWirelessDevices");
                    self.bt_battery
                        .lock()
                        .map(|g| {
                            g.devices
                                .iter()
                                .filter(|d| !bt_is_ignored(&ignored, d))
                                .cloned()
                                .collect()
                        })
                        .unwrap_or_default()
                };
                let selected_device = |cfg: &DockSettings, id: Option<&str>| {
                    let devices = visible(cfg);
                    id.and_then(|id| devices.iter().find(|d| d.id == id).cloned())
                        .or_else(|| devices.first().cloned())
                };
                // Aliases are keyed by device id (legacy name keys are still honoured).
                let alias_for = |cfg: &DockSettings, d: &BluetoothBatteryDevice| -> String {
                    setting_value(cfg, "WirelessDeviceAliases")
                        .and_then(|v| {
                            let map = v.as_object()?.clone();
                            map.iter()
                                .find(|(key, _)| key.eq_ignore_ascii_case(&d.id))
                                .or_else(|| map.iter().find(|(key, _)| key.as_str() == d.name))
                                .and_then(|(_, alias)| alias.as_str().map(str::to_owned))
                        })
                        .unwrap_or_else(|| d.name.clone())
                };
                let prefill_for = |cfg: &DockSettings, id: Option<&str>| -> String {
                    selected_device(cfg, id)
                        .map(|d| alias_for(cfg, &d))
                        .unwrap_or_default()
                };
                let build = |cfg: &DockSettings,
                             selected: Option<&str>,
                             rename_value: String,
                             feedback: Option<&str>| {
                    let devices = visible(cfg);
                    *rendered_row_ids.borrow_mut() = devices.iter().map(|d| d.id.clone()).collect();
                    let low = setting_i64(cfg, "WirelessBatteryLowThreshold", 20) as u32;
                    let interval = wireless_poll_secs(cfg);
                    let listen_only = setting_bool(cfg, "WirelessBatteryListenOnly", false);
                    let loading = self.bt_battery.lock().ok().is_some_and(|g| g.loading);
                    let mut blocks: Vec<FlyoutBlock> = devices
                        .iter()
                        .enumerate()
                        .map(|(i, d)| {
                            let shown = alias_for(cfg, d);
                            let stale = !d.is_live()
                                || d.sampled_at
                                    .is_none_or(|t| t.elapsed().as_secs() > interval * 2);
                            let charging = d.charging && !stale;
                            let pct_str = format_percent(d.percent);
                            let mut subtitle = if stale {
                                loc::t_fmt("WirelessBatteryStale", &[&pct_str])
                            } else if charging {
                                loc::t("WirelessBatteryCharging")
                            } else {
                                format!("{} · {pct_str}", d.source_label)
                            };
                            if let Some(spare) = d.secondary_level {
                                let spare_text =
                                    loc::t_fmt("WirelessBatterySpareDock", &[&spare.to_string()]);
                                subtitle = format!("{subtitle} · {spare_text}");
                            }
                            FlyoutBlock::ActionRow {
                                id: 100 + i as u32,
                                title: format!("{shown}{}", if charging { " ⚡" } else { "" }),
                                subtitle: Some(subtitle),
                                badge: Some(if stale {
                                    loc::t_fmt("WirelessBatteryStale", &[&pct_str])
                                } else {
                                    format!("{pct_str}{}", if charging { " ⚡" } else { "" })
                                }),
                                icon: None,
                                destructive: false,
                            }
                        })
                        .collect();
                    if blocks.is_empty() {
                        blocks.push(FlyoutBlock::Note(loc::t("WirelessBatteryNoDevices")));
                    }
                    let current = selected
                        .and_then(|id| devices.iter().find(|d| d.id == id))
                        .or_else(|| devices.first());
                    if let Some(d) = current {
                        blocks.push(FlyoutBlock::Separator);
                        blocks.push(FlyoutBlock::StatRow {
                            label: loc::t("WirelessBatterySource"),
                            value: d.source_label.clone(),
                        });
                    }
                    if let Some(note) = feedback {
                        blocks.push(FlyoutBlock::Note(note.to_owned()));
                    } else if loading {
                        blocks.push(FlyoutBlock::Note(loc::t("WirelessBatteryRefresh")));
                    }
                    // C2 TextInput — device rename (1-64 chars).
                    blocks.push(FlyoutBlock::TextInput {
                        id: 50,
                        label: loc::t("WirelessBatteryRename"),
                        value: rename_value,
                        numeric: false,
                        min: 0.0,
                        max: 0.0,
                    });
                    blocks.push(FlyoutBlock::StatRow {
                        label: loc::t("WirelessBatteryPollIntervalLabel"),
                        value: format!("{interval}s"),
                    });
                    blocks.push(FlyoutBlock::StatRow {
                        label: loc::t("WirelessBatteryLowThresholdLabel"),
                        value: format_percent(low),
                    });
                    blocks.push(FlyoutBlock::ToggleGrid(vec![(
                        7,
                        loc::t("WirelessBatteryListenOnlyLabel"),
                        loc::t(if listen_only {
                            "WirelessBatteryListenOnlyOn"
                        } else {
                            "WirelessBatteryListenOnlyOff"
                        }),
                        listen_only,
                    )]));
                    blocks.push(FlyoutBlock::ButtonBar(vec![
                        (1, loc::t("WirelessBatteryRefresh"), false),
                        (2, loc::t("WirelessBatteryHideDevice"), false),
                        (3, loc::t("WirelessBatteryRestoreIgnored"), false),
                    ]));
                    blocks.push(FlyoutBlock::ButtonBar(vec![
                        (4, loc::t("WirelessBatteryIntervalButton"), false),
                        (5, loc::t("WirelessBatteryThresholdButton"), false),
                    ]));
                    blocks.push(FlyoutBlock::ButtonBar(vec![
                        (6, loc::t("WirelessBatteryCopyDiagnostics"), false),
                        (8, loc::t("QuickBluetoothSettings"), false),
                    ]));
                    FlyoutSpec {
                        title: loc::t("BluetoothBattery"),
                        subtitle: Some(loc::t_fmt(
                            "WirelessBatterySubtitle",
                            &[&interval.to_string(), &low.to_string()],
                        )),
                        width_dips: 304.0,
                        blocks,
                    }
                };
                let init = build(settings, None, prefill_for(settings, None), None);
                let live_handle = flyout::LiveUpdateHandle::new();
                if let Ok(mut slot) = self.wb_live_update.lock() {
                    *slot = Some(live_handle.clone());
                }
                flyout::set_next_live_update_handle(Some(live_handle.clone()));
                show_live!("bluetoothbattery", init, |ev| match ev {
                    // Device row selection: resolve via the rendered row snapshot so live
                    // re-sorts never select the wrong device or abruptly close the flyout.
                    LiveEvent::Click(id @ 100..=199) => {
                        let idx = (id - 100) as usize;
                        let clicked_id = rendered_row_ids.borrow().get(idx).cloned();
                        if let Some(target_id) = clicked_id
                            && let Some(d) =
                                visible(settings).into_iter().find(|d| d.id == target_id)
                        {
                            selected_id = Some(d.id.clone());
                            feedback = None;
                            LiveOutcome::Update(build(
                                settings,
                                selected_id.as_deref(),
                                alias_for(settings, &d),
                                feedback.as_deref(),
                            ))
                        } else {
                            LiveOutcome::Update(build(
                                settings,
                                selected_id.as_deref(),
                                prefill_for(settings, selected_id.as_deref()),
                                feedback.as_deref(),
                            ))
                        }
                    }
                    LiveEvent::Click(1) => {
                        // Forced: bypasses the poll-interval reuse; queued if a poll is running.
                        // Listen-only devices are still refreshed passively by the provider.
                        self.refresh_bluetooth_devices_forced(settings);
                        feedback = Some(loc::t("WirelessBatteryRefresh"));
                        LiveOutcome::Update(build(
                            settings,
                            selected_id.as_deref(),
                            prefill_for(settings, selected_id.as_deref()),
                            feedback.as_deref(),
                        ))
                    }
                    LiveEvent::Click(2) => {
                        // Hide the *selected* device by its stable id (not the first row and
                        // not every device that happens to share its name).
                        if let Some(d) = selected_device(settings, selected_id.as_deref()) {
                            let mut list = setting_string_list(settings, "IgnoredWirelessDevices");
                            if !list.iter().any(|n| n.eq_ignore_ascii_case(&d.id)) {
                                list.push(d.id.clone());
                                set_setting(
                                    settings,
                                    "IgnoredWirelessDevices",
                                    serde_json::json!(list),
                                );
                                settings_changed = true;
                            }
                            selected_id = None;
                        }
                        LiveOutcome::Update(build(
                            settings,
                            selected_id.as_deref(),
                            prefill_for(settings, selected_id.as_deref()),
                            feedback.as_deref(),
                        ))
                    }
                    LiveEvent::Click(3) => {
                        set_setting(settings, "IgnoredWirelessDevices", serde_json::json!([]));
                        settings_changed = true;
                        selected_id = None;
                        LiveOutcome::Update(build(
                            settings,
                            None,
                            prefill_for(settings, None),
                            feedback.as_deref(),
                        ))
                    }
                    LiveEvent::Click(4) => {
                        const STEPS: [i64; 5] = [15, 30, 60, 120, 300];
                        let cur = setting_i64(settings, "WirelessBatteryPollSeconds", 60);
                        let next = STEPS
                            .iter()
                            .find(|&&s| s > cur)
                            .copied()
                            .unwrap_or(STEPS[0]);
                        set_setting(
                            settings,
                            "WirelessBatteryPollSeconds",
                            serde_json::json!(next),
                        );
                        settings_changed = true;
                        LiveOutcome::Update(build(
                            settings,
                            selected_id.as_deref(),
                            prefill_for(settings, selected_id.as_deref()),
                            feedback.as_deref(),
                        ))
                    }
                    LiveEvent::Click(5) => {
                        // reference cycles 0 / 10 / 20 / 30 percent (0 disables the low alert).
                        let cur = setting_i64(settings, "WirelessBatteryLowThreshold", 20);
                        set_setting(
                            settings,
                            "WirelessBatteryLowThreshold",
                            serde_json::json!(bt_next_threshold(cur)),
                        );
                        settings_changed = true;
                        LiveOutcome::Update(build(
                            settings,
                            selected_id.as_deref(),
                            prefill_for(settings, selected_id.as_deref()),
                            feedback.as_deref(),
                        ))
                    }
                    LiveEvent::Click(6) => {
                        let diag = self
                            .bt_battery
                            .lock()
                            .map(|g| {
                                g.devices
                                    .iter()
                                    .map(|d| {
                                        format!(
                                            "{}|{}|{}%|{}|{}|{}",
                                            d.name,
                                            d.id,
                                            d.percent,
                                            d.source_label,
                                            if d.charging {
                                                "charging"
                                            } else {
                                                "discharging"
                                            },
                                            if d.is_live() { "live" } else { "cached" }
                                        )
                                    })
                                    .collect::<Vec<_>>()
                                    .join("\n")
                            })
                            .unwrap_or_default();
                        copy_to_clipboard(&diag);
                        // Visible feedback instead of a silent clipboard write.
                        feedback = Some(loc::t("WirelessBatteryDiagnosticsCopied"));
                        LiveOutcome::Update(build(
                            settings,
                            selected_id.as_deref(),
                            prefill_for(settings, selected_id.as_deref()),
                            feedback.as_deref(),
                        ))
                    }
                    LiveEvent::Click(7) => {
                        let next = !setting_bool(settings, "WirelessBatteryListenOnly", false);
                        set_setting(
                            settings,
                            "WirelessBatteryListenOnly",
                            serde_json::json!(next),
                        );
                        settings_changed = true;
                        self.refresh_bluetooth_devices_forced(settings);
                        LiveOutcome::Update(build(
                            settings,
                            selected_id.as_deref(),
                            prefill_for(settings, selected_id.as_deref()),
                            feedback.as_deref(),
                        ))
                    }
                    LiveEvent::Click(8) => {
                        pins::launch("ms-settings:bluetooth".to_owned());
                        LiveOutcome::Close
                    }
                    LiveEvent::TextCommit { id: 50, text } => {
                        if (1..=64).contains(&text.len())
                            && let Some(d) = selected_device(settings, selected_id.as_deref())
                        {
                            let mut map = setting_value(settings, "WirelessDeviceAliases")
                                .and_then(|v| v.as_object().cloned())
                                .unwrap_or_default();
                            // Re-key a legacy name alias onto the stable id.
                            map.remove(&d.name);
                            map.insert(d.id.clone(), serde_json::json!(text));
                            set_setting(settings, "WirelessDeviceAliases", serde_json::json!(map));
                            settings_changed = true;
                            selected_id = Some(d.id.clone());
                        }
                        LiveOutcome::Update(build(
                            settings,
                            selected_id.as_deref(),
                            text,
                            feedback.as_deref(),
                        ))
                    }
                    LiveEvent::Update => {
                        let still_loading = self.bt_battery.lock().ok().is_some_and(|g| g.loading);
                        if !still_loading
                            && feedback.as_deref()
                                == Some(loc::t("WirelessBatteryRefresh").as_str())
                        {
                            feedback = None;
                        }
                        LiveOutcome::Update(build(
                            settings,
                            selected_id.as_deref(),
                            prefill_for(settings, selected_id.as_deref()),
                            feedback.as_deref(),
                        ))
                    }
                    _ => LiveOutcome::Close,
                });
                live_handle.detach();
                if let Ok(mut slot) = self.wb_live_update.lock() {
                    *slot = None;
                }
                // WS-1: no unconditional `true` — a hover-opened flyout must not
                // trigger settings save + live reload on the hook-owning UI thread.
                return Self::flyout_settings_changed("bluetoothbattery", settings_changed);
            }
            // `StorageWidget` expanded drives flyout (DriveType.Fixed && IsReady).
            "storage" => {
                let drives = self.poll_snapshot.fixed_drives();
                let mut blocks: Vec<FlyoutBlock> = drives
                    .iter()
                    .take(6)
                    .map(|d| {
                        let used = (d.total_gb - d.free_gb).max(0.0);
                        FlyoutBlock::ProgressBar {
                            label: format!("{}:", d.letter),
                            value_text: d.format_capacity_pair(),
                            ratio: (used / d.total_gb.max(1.0)) as f32,
                            tone: BarTone::Usage,
                        }
                    })
                    .collect();
                if blocks.is_empty() {
                    blocks.push(FlyoutBlock::Note(loc::t("StorageNoFixedDrives")));
                }
                blocks.push(FlyoutBlock::Separator);
                blocks.push(FlyoutBlock::ButtonBar(vec![(
                    1,
                    loc::t("OpenFileExplorer"),
                    false,
                )]));
                let spec = FlyoutSpec {
                    title: loc::t("WidgetStorage"),
                    subtitle: None,
                    width_dips: 286.0,
                    blocks,
                };
                if show_primary(spec) == Some(1) {
                    pins::launch("explorer.exe".to_owned());
                }
            }
            // Exact `KeyboardLayoutWidget.BuildFlyoutContent` (`BadgeRow` with code pill + full name)
            "keyboard" => {
                let layouts = telemetry::installed_keyboard_layouts();
                let active_hkl = active_keyboard_hkl(self.keyboard_target_hwnd);
                let active_code = active_keyboard_code();
                let blocks: Vec<FlyoutBlock> = layouts
                    .iter()
                    .enumerate()
                    .map(|(i, (hkl, code, display))| FlyoutBlock::BadgeRow {
                        id: 100 + i as u32,
                        code: code.clone(),
                        title: display.clone(),
                        active: if active_hkl != 0 {
                            *hkl == active_hkl
                        } else {
                            code == &active_code
                        },
                    })
                    .collect();
                let spec = FlyoutSpec {
                    title: loc::t("WidgetKeyboardLayout"),
                    subtitle: None,
                    width_dips: 268.0,
                    blocks,
                };
                if let Some(act) = show_primary(spec)
                    && act >= 100
                    && let Some((hkl, code, _)) = layouts.get((act - 100) as usize)
                {
                    if let Some(target) = self.keyboard_target_hwnd {
                        telemetry::activate_keyboard_layout_for(target, *hkl);
                    } else {
                        telemetry::activate_keyboard_layout(*hkl);
                    }
                    self.keyboard_override = Some((code.clone(), Instant::now()));
                }
            }
            "dayprogress" => {
                let (day, week, year, left) = day_progress_metrics();
                let spec = FlyoutSpec {
                    title: loc::t("WidgetDayProgress"),
                    subtitle: None,
                    width_dips: 272.0,
                    blocks: vec![
                        FlyoutBlock::Hero {
                            value: day_progress_percent_text(day),
                            caption: Some(loc::t_fmt("DayProgressRemaining", &[&left.to_string()])),
                            centered: false,
                            large: false,
                            right_meta: None,
                        },
                        FlyoutBlock::ProgressBar {
                            label: loc::t("DayProgressDay"),
                            value_text: day_progress_percent_text(day),
                            ratio: day,
                            tone: BarTone::Accent,
                        },
                        FlyoutBlock::ProgressBar {
                            label: loc::t("DayProgressWeek"),
                            value_text: day_progress_percent_text(week),
                            ratio: week,
                            tone: BarTone::Accent,
                        },
                        FlyoutBlock::ProgressBar {
                            label: loc::t("DayProgressYear"),
                            value_text: day_progress_percent_text(year),
                            ratio: year,
                            tone: BarTone::Accent,
                        },
                    ],
                };
                let _ = show_primary(spec);
            }
            // `FocusWidget.BuildFocusFlyout` — Break phase + session number + pulse.
            "focus" => {
                self.focus.with_mut(|s| s.sync_from_settings(settings));
                self.sync_focus_mirrors();
                let build = |runtime: &Self| {
                    focus_break::build_focus_flyout(&runtime.focus.snapshot(), is_dark)
                };
                let init = build(self);
                show_live!("focus", init, |ev| match ev {
                    LiveEvent::Click(1) => {
                        self.focus.with_mut(|s| s.toggle());
                        self.sync_focus_mirrors();
                        LiveOutcome::Update(build(self))
                    }
                    LiveEvent::Click(2) => {
                        self.focus.with_mut(|s| s.reset());
                        self.sync_focus_mirrors();
                        LiveOutcome::Update(build(self))
                    }
                    LiveEvent::Click(4) => {
                        let event = self.focus.with_mut(|s| {
                            s.skip_phase();
                            FocusEvent::PhaseEnded(s.phase)
                        });
                        if matches!(event, Some(FocusEvent::PhaseEnded(_))) {
                            play_break_chime();
                        }
                        self.sync_focus_mirrors();
                        LiveOutcome::Update(build(self))
                    }
                    LiveEvent::Click(mins @ (15 | 25 | 50)) => {
                        self.focus
                            .with_mut(|s| s.set_focus_minutes(u64::from(mins)));
                        self.sync_focus_mirrors();
                        LiveOutcome::Update(build(self))
                    }
                    _ => LiveOutcome::Update(build(self)),
                });
            }
            // Exact `TimedWidgetPanel.xaml` Stopwatch (live Lap / Start / Pause / Reset)
            "stopwatch" => {
                let init = self.build_stopwatch_spec();
                show_live!("stopwatch", init, |ev| match ev {
                    LiveEvent::Click(1) => {
                        self.stopwatch_running = !self.stopwatch_running;
                        LiveOutcome::Update(self.build_stopwatch_spec())
                    }
                    LiveEvent::Click(2) => {
                        self.stopwatch_running = false;
                        self.stopwatch_secs = 0;
                        self.stopwatch_cs = 0;
                        self.stopwatch_debt_secs = 0.0;
                        self.stopwatch_laps.clear();
                        LiveOutcome::Update(self.build_stopwatch_spec())
                    }
                    LiveEvent::Click(3) => {
                        self.stopwatch_laps
                            .push((self.stopwatch_secs, self.stopwatch_cs));
                        LiveOutcome::Update(self.build_stopwatch_spec())
                    }
                    _ => LiveOutcome::Update(self.live_spec("stopwatch", settings, is_dark)),
                });
            }
            // Exact `TimedWidgetPanel.xaml` Countdown (live 5/10/15/30 presets + Start/Pause/Reset)
            "countdown" => {
                let init = self.build_countdown_spec();
                show_live!("countdown", init, |ev| match ev {
                    LiveEvent::Click(1) => {
                        if self.countdown_remaining_secs == 0 {
                            self.countdown_remaining_secs = self.countdown_total_secs;
                        }
                        self.countdown_running = !self.countdown_running;
                        LiveOutcome::Update(self.build_countdown_spec())
                    }
                    LiveEvent::Click(2) => {
                        self.countdown_running = false;
                        self.countdown_remaining_secs = self.countdown_total_secs;
                        LiveOutcome::Update(self.build_countdown_spec())
                    }
                    LiveEvent::Click(mins @ (5 | 10 | 15 | 30)) => {
                        self.countdown_total_secs = u64::from(mins) * 60;
                        self.countdown_remaining_secs = self.countdown_total_secs;
                        self.countdown_running = false;
                        LiveOutcome::Update(self.build_countdown_spec())
                    }
                    // C2 TextCommit — custom duration with 1-1440 minute validation.
                    LiveEvent::TextCommit { id: 50, text } => {
                        if let Ok(mins) = text.trim().parse::<u64>()
                            && (1..=1440).contains(&mins)
                        {
                            self.countdown_total_secs = mins * 60;
                            self.countdown_remaining_secs = self.countdown_total_secs;
                            self.countdown_running = false;
                        }
                        LiveOutcome::Update(self.build_countdown_spec())
                    }
                    _ => LiveOutcome::Update(self.live_spec("countdown", settings, is_dark)),
                });
            }
            // `MediaWidget` live flyout: seek anchor + per-app mixer + N6 lyrics & source switch.
            "media" => {
                telemetry::refresh_media_session(&self.media_session, true);
                let build = |runtime: &mut Self| {
                    let snap = runtime.media_snapshot();
                    let details =
                        media_ext::get_details(&snap, runtime.audio_vol, runtime.audio_muted);
                    let sessions = media_ext::list_app_sessions();
                    let mut spec = media_ext::build_media_flyout(&details, &sessions, settings);
                    if settings.media_lyrics_enabled() && !snap.title.trim().is_empty() {
                        let pos_ms = if runtime.media_timeline.duration() > 0.0 {
                            (runtime.media_timeline.position_at(runtime.now_secs()) * 1000.0)
                                .round()
                                .max(0.0) as u64
                        } else {
                            snap.position_secs.saturating_mul(1000)
                        };
                        let dur = (snap.duration_secs > 0).then_some(snap.duration_secs);
                        let cache = runtime
                            .lyrics
                            .get_or_insert_with(|| Box::new(lyrics::LyricsCache::new()));
                        if let Some(synced) =
                            cache.poll_for_track(true, &snap.title, &snap.artist, dur)
                        {
                            let window = synced.window_around(pos_ms, 1, 1);
                            if !window.is_empty() {
                                let pal = design::palette(is_dark);
                                spec.blocks.push(FlyoutBlock::Separator);
                                for (line_idx, line, is_current) in window {
                                    spec.blocks.push(FlyoutBlock::GlyphActionRow {
                                        id: 1000 + line_idx as u32,
                                        glyph: if is_current { 0xE768 } else { 0xE8D2 },
                                        glyph_color: if is_current {
                                            pal.widget_accent
                                        } else {
                                            pal.text_secondary
                                        },
                                        title: line.text.clone(),
                                        subtitle: None,
                                        badge: Some(media_ext::format_time(line.time_ms / 1000)),
                                    });
                                }
                            }
                        }
                    }
                    spec.blocks.push(FlyoutBlock::Separator);
                    spec.blocks.push(FlyoutBlock::ButtonBar(vec![
                        (5, "Switch Source".to_owned(), false),
                        (
                            4,
                            loc::t_fmt("RadioSettings", &[&loc::t("LauncherSound")]),
                            false,
                        ),
                    ]));
                    spec
                };
                let init = build(self);
                show_live!("media", init, |ev| match ev {
                    LiveEvent::Click(btn @ 1..=3) => {
                        let vk = match btn {
                            1 => VK_MEDIA_PREV_TRACK,
                            2 => VK_MEDIA_PLAY_PAUSE,
                            _ => VK_MEDIA_NEXT_TRACK,
                        };
                        telemetry::send_media_vk(vk);
                        telemetry::refresh_media_session(&self.media_session, true);
                        LiveOutcome::Update(build(self))
                    }
                    LiveEvent::Click(4) => {
                        pins::launch("ms-settings:sound".to_owned());
                        LiveOutcome::Close
                    }
                    LiveEvent::Click(5) => {
                        media_ext::cycle_media_session();
                        telemetry::refresh_media_session(&self.media_session, true);
                        LiveOutcome::Update(build(self))
                    }
                    LiveEvent::Click(60) => {
                        toggle_audio_mute();
                        (self.audio_vol, self.audio_muted) = get_audio_state();
                        LiveOutcome::Update(build(self))
                    }
                    LiveEvent::Click(id) if id >= 1000 => {
                        let snap = self.media_snapshot();
                        let dur = (snap.duration_secs > 0).then_some(snap.duration_secs);
                        let line_idx = (id - 1000) as usize;
                        let target_ms = self.lyrics.as_mut().and_then(|cache| {
                            cache
                                .poll_for_track(true, &snap.title, &snap.artist, dur)
                                .and_then(|synced| synced.lines.get(line_idx))
                                .map(|line| line.time_ms)
                        });
                        if let Some(ms) = target_ms {
                            let target_secs = ms / 1000;
                            if telemetry::media_seek(target_secs) {
                                let now = self.now_secs();
                                confirm_seek(&mut self.media_timeline, target_secs as f64, now);
                            }
                            telemetry::refresh_media_session(&self.media_session, true);
                        }
                        LiveOutcome::Update(build(self))
                    }
                    // C2 LiveEvent::Seek — C4 `telemetry::media_seek(secs)`.
                    LiveEvent::Seek { id: 50, ratio } => {
                        let dur = self
                            .media_session
                            .lock()
                            .ok()
                            .map_or(0, |g| g.duration_secs);
                        let target = media_ext::seek_position(ratio, dur);
                        if telemetry::media_seek(target) {
                            // Anchor locally at once; a stale SMTC echo of the old position
                            // must not rewind the slider until the provider catches up.
                            let now = self.now_secs();
                            confirm_seek(&mut self.media_timeline, target as f64, now);
                        }
                        telemetry::refresh_media_session(&self.media_session, true);
                        LiveOutcome::Update(build(self))
                    }
                    LiveEvent::Slider { id: 51, ratio } => {
                        set_audio_volume((ratio * 100.0).round() as u32);
                        (self.audio_vol, self.audio_muted) = get_audio_state();
                        LiveOutcome::Update(build(self))
                    }
                    // Per-app mixer rows (ids 120..=131) from `media_ext::list_app_sessions`.
                    LiveEvent::Click(id) if (120..132).contains(&id) && id % 2 == 0 => {
                        toggle_app_mute(((id - 120) / 2) as usize);
                        LiveOutcome::Update(build(self))
                    }
                    LiveEvent::Slider { id, ratio } if (121..132).contains(&id) && id % 2 == 1 => {
                        set_app_vol(((id - 121) / 2) as usize, ratio);
                        LiveOutcome::Update(build(self))
                    }
                    LiveEvent::Update => LiveOutcome::Update(build(self)),
                    _ => LiveOutcome::Close,
                });
            }
            "quicksettings" => {
                let mut settings_changed = false;
                let build_qs_spec = |runtime: &Self, settings: &DockSettings| {
                    let change_error = runtime.radio_failed.then(|| {
                        loc::t(if runtime.radio_denied {
                            "QuickPermissionDenied"
                        } else {
                            "QuickChangeFailed"
                        })
                    });
                    let radio_state = |on: bool| {
                        if runtime.radio_unavailable {
                            loc::t("QuickUnavailable")
                        } else {
                            change_error
                                .clone()
                                .unwrap_or_else(|| radio_label(on, runtime.radio_loading))
                        }
                    };
                    let (vol, muted) = get_audio_state();
                    let out_name = telemetry::default_audio_device_name(false)
                        .unwrap_or_else(|| loc::t("Speakers"));
                    let hide_label = match settings.auto_hide {
                        AutoHideMode::Disabled => loc::t("QuickOff"),
                        AutoHideMode::Smart => loc::t("AutoHideSmart"),
                        AutoHideMode::Always => loc::t("AutoHideAlways"),
                    };
                    let theme_label = match settings.theme {
                        AppTheme::System => loc::t("ThemeSystem"),
                        AppTheme::Dark => loc::t("Dark"),
                        AppTheme::Light => loc::t("Light"),
                    };
                    let on_off = |on: bool| loc::t(if on { "QuickOn" } else { "QuickOff" });
                    let mut spec = FlyoutSpec {
                        title: loc::t("WidgetQuickSettings"),
                        subtitle: runtime.radio_loading.then(|| loc::t("QuickLoading")),
                        width_dips: 312.0,
                        blocks: vec![
                            FlyoutBlock::ToggleGrid(vec![
                                (
                                    5,
                                    loc::t("WiFi"),
                                    radio_state(runtime.wifi_on),
                                    runtime.wifi_on,
                                ),
                                (
                                    6,
                                    loc::t("Bluetooth"),
                                    radio_state(runtime.bluetooth_on),
                                    runtime.bluetooth_on,
                                ),
                                (
                                    1,
                                    loc::t("FocusMode"),
                                    on_off(settings.focus_mode),
                                    settings.focus_mode,
                                ),
                                (
                                    2,
                                    loc::t("AutoHide"),
                                    hide_label,
                                    settings.auto_hide != AutoHideMode::Disabled,
                                ),
                                (
                                    3,
                                    loc::t("Theme"),
                                    theme_label,
                                    settings.theme != AppTheme::System,
                                ),
                                (
                                    7,
                                    loc::t("NightLight"),
                                    on_off(runtime.night_light_on),
                                    runtime.night_light_on,
                                ),
                            ]),
                            FlyoutBlock::Separator,
                            FlyoutBlock::AudioSliderSection {
                                label: loc::t("Volume"),
                                device_name: out_name,
                                value_text: if muted {
                                    loc::t("AudioMuted")
                                } else {
                                    format_percent(vol)
                                },
                                ratio: vol as f32 / 100.0,
                                muted,
                                mute_id: 4,
                                slider_id: 10,
                            },
                            FlyoutBlock::Separator,
                            // 8 crisp Control Center shortcuts (2 neat rows of 4): only live stateful radios highlight ON.
                            FlyoutBlock::IconBar {
                                buttons: vec![
                                    (100, loc::t("WiFi"), 0xE701, runtime.wifi_on),
                                    (101, loc::t("Bluetooth"), 0xE702, runtime.bluetooth_on),
                                    (102, loc::t("NightLight"), 0xE708, runtime.night_light_on),
                                    (103, loc::t("QuickAirplaneMode"), 0xF71D, false),
                                    (105, loc::t("QuickBrightness"), 0xE7F3, false),
                                    (107, loc::t("Battery"), 0xE7EF, false),
                                    (113, loc::t("QuickProject"), 0xEC15, false),
                                    (110, loc::t("Focus"), 0xE7ED, false),
                                ],
                            },
                            FlyoutBlock::ButtonBar(vec![
                                (8, loc::t("Settings"), true),
                                (9, loc::t("WidgetShowDesktop"), false),
                            ]),
                        ],
                    };
                    if settings.quick_power_actions_enabled() {
                        spec.blocks.push(FlyoutBlock::Separator);
                        spec.blocks.push(FlyoutBlock::ButtonBar(vec![
                            (20, loc::t("QuickActionLock"), false),
                            (21, loc::t("QuickActionSleep"), false),
                        ]));
                        let mut capture_buttons = Vec::new();
                        if settings.quick_screenshot_enabled() {
                            capture_buttons.push((22, loc::t("QuickActionScreenshot"), false));
                        }
                        if settings.smart_ocr_enabled() {
                            capture_buttons.push((23, loc::t("QuickActionOcr"), false));
                        }
                        if !capture_buttons.is_empty() {
                            spec.blocks.push(FlyoutBlock::ButtonBar(capture_buttons));
                        }
                    }
                    spec
                };
                let init = build_qs_spec(self, settings);
                show_live!("quicksettings", init, |ev| match ev {
                    LiveEvent::Click(1) => {
                        settings.focus_mode = !settings.focus_mode;
                        settings_changed = true;
                        LiveOutcome::Update(build_qs_spec(self, settings))
                    }
                    LiveEvent::Click(2) => {
                        settings.auto_hide = match settings.auto_hide {
                            AutoHideMode::Disabled => AutoHideMode::Smart,
                            AutoHideMode::Smart => AutoHideMode::Always,
                            AutoHideMode::Always => AutoHideMode::Disabled,
                        };
                        settings_changed = true;
                        LiveOutcome::Update(build_qs_spec(self, settings))
                    }
                    LiveEvent::Click(3) => {
                        settings.theme = match settings.theme {
                            AppTheme::System => AppTheme::Dark,
                            AppTheme::Dark => AppTheme::Light,
                            AppTheme::Light => AppTheme::System,
                        };
                        settings_changed = true;
                        LiveOutcome::Update(build_qs_spec(self, settings))
                    }
                    LiveEvent::Click(4) => {
                        toggle_audio_mute();
                        (self.audio_vol, self.audio_muted) = get_audio_state();
                        LiveOutcome::Update(build_qs_spec(self, settings))
                    }
                    LiveEvent::Slider { id: 10, ratio } => {
                        set_audio_volume((ratio * 100.0).round() as u32);
                        (self.audio_vol, self.audio_muted) = get_audio_state();
                        LiveOutcome::Update(build_qs_spec(self, settings))
                    }
                    LiveEvent::Click(5) => {
                        // C4 `platform::radio::set` — real WinRT Wi-Fi toggle.
                        self.toggle_radio("wifi");
                        LiveOutcome::Update(build_qs_spec(self, settings))
                    }
                    LiveEvent::Click(6) => {
                        // C4 `platform::radio::set` — real WinRT Bluetooth toggle.
                        self.toggle_radio("bluetooth");
                        LiveOutcome::Update(build_qs_spec(self, settings))
                    }
                    LiveEvent::Click(id @ 100..=116) => {
                        pins::launch(shortcut_uri(id).to_owned());
                        LiveOutcome::Close
                    }
                    LiveEvent::Click(7) => {
                        if telemetry::toggle_night_light() {
                            self.night_light_on = !self.night_light_on;
                        } else {
                            pins::launch("ms-settings:nightlight".to_owned());
                        }
                        LiveOutcome::Update(build_qs_spec(self, settings))
                    }
                    LiveEvent::Click(8) => {
                        crate::ui::settings::spawn_settings_window();
                        LiveOutcome::Close
                    }
                    LiveEvent::Click(9) => {
                        opentaskbar_shell::taskbar::show_desktop();
                        LiveOutcome::Close
                    }
                    LiveEvent::Click(20) => {
                        let _ = crate::platform::privacy_dots::lock_workstation();
                        LiveOutcome::Close
                    }
                    LiveEvent::Click(21) => {
                        let _ = crate::platform::privacy_dots::sleep_system();
                        LiveOutcome::Close
                    }
                    LiveEvent::Click(22) => {
                        crate::platform::smart_tools::capture_region_screenshot_async(
                            None,
                            |res| {
                                if let Ok(done) = res {
                                    let _ = toast::show_info_toast(&done.summary);
                                }
                            },
                        );
                        LiveOutcome::Close
                    }
                    LiveEvent::Click(23) => {
                        crate::platform::smart_tools::capture_and_ocr_async(|res| {
                            if let Ok(text) = res
                                && !text.trim().is_empty()
                            {
                                let _ = toast::show_info_toast(&format!(
                                    "OCR copied ({} chars)",
                                    text.chars().count()
                                ));
                            }
                        });
                        LiveOutcome::Close
                    }
                    _ => LiveOutcome::Close,
                });
                return Self::flyout_settings_changed("quicksettings", settings_changed);
            }
            "stacks" => {
                let mut blocks: Vec<FlyoutBlock> = settings
                    .stack_folders
                    .iter()
                    .enumerate()
                    .map(|(i, path)| {
                        let name = std::path::Path::new(path)
                            .file_name()
                            .and_then(|s| s.to_str())
                            .unwrap_or(path);
                        action_row(100 + i as u32, name, Some(loc::t("FolderLabel")))
                    })
                    .collect();
                if blocks.is_empty() {
                    blocks.push(FlyoutBlock::Note(loc::t("WidgetStacksHint")));
                }
                let spec = FlyoutSpec {
                    title: loc::t("FolderStacks"),
                    subtitle: None,
                    width_dips: 276.0,
                    blocks,
                };
                if let Some(act) = show_primary(spec)
                    && act >= 100
                    && let Some(path) = settings.stack_folders.get((act - 100) as usize)
                {
                    pins::launch(path.clone());
                }
            }
            "groups" => {
                let mut blocks: Vec<FlyoutBlock> = settings
                    .app_groups
                    .iter()
                    .enumerate()
                    .map(|(i, g)| {
                        action_row(
                            100 + i as u32,
                            g.name.clone(),
                            Some(loc::t_fmt(
                                "AppGroupAppsCount",
                                &[&g.executable_paths.len().to_string()],
                            )),
                        )
                    })
                    .collect();
                if blocks.is_empty() {
                    blocks.push(FlyoutBlock::Note(loc::t("WidgetGroupsHint")));
                }
                let spec = FlyoutSpec {
                    title: loc::t("AppGroups"),
                    subtitle: None,
                    width_dips: 276.0,
                    blocks,
                };
                if let Some(act) = show_primary(spec)
                    && act >= 100
                    && let Some(group) = settings.app_groups.get((act - 100) as usize)
                {
                    for member in &group.executable_paths {
                        pins::launch(member.clone());
                    }
                }
            }
            "clipboard" => {
                let store = self
                    .clipboard
                    .get_or_insert_with(|| Box::new(clipboard::ClipboardStore::new()));
                if settings.clipboard_history_enabled() || settings.is_widget_enabled("clipboard") {
                    store.poll(settings);
                }
                let init = store.build_flyout(is_dark);
                flyout::show_flyout_live_with_options(
                    "clipboard",
                    owner_hwnd,
                    is_dark,
                    anchor_x,
                    anchor_y,
                    init,
                    border,
                    options,
                    |ev| match ev {
                        LiveEvent::Click(click_id) => {
                            if let Some(store) = self.clipboard.as_mut()
                                && let Some(action) = store.flyout_action_id_for_click(click_id)
                            {
                                let _ = store.handle_flyout_action(&action);
                            }
                            let spec = self.clipboard.as_ref().map_or_else(
                                || clipboard::ClipboardStore::new().build_flyout(is_dark),
                                |store| store.build_flyout(is_dark),
                            );
                            LiveOutcome::Update(spec)
                        }
                        _ => LiveOutcome::Close,
                    },
                );
            }
            "shelf" => {
                let shelf = self
                    .shelf
                    .get_or_insert_with(|| Box::new(shelf::FileShelfState::load_default()));
                let init = shelf.build_flyout(is_dark);
                flyout::show_flyout_live_with_options(
                    "shelf",
                    owner_hwnd,
                    is_dark,
                    anchor_x,
                    anchor_y,
                    init,
                    border,
                    options,
                    |ev| match ev {
                        LiveEvent::Click(click_id) => {
                            if let Some(shelf) = self.shelf.as_mut()
                                && let Some(action) = shelf.flyout_action_id_for_click(click_id)
                                && shelf.handle_flyout_action(&action)
                            {
                                let _ = shelf.save_default();
                            }
                            let spec = self.shelf.as_ref().map_or_else(
                                || shelf::FileShelfState::default().build_flyout(is_dark),
                                |shelf| shelf.build_flyout(is_dark),
                            );
                            LiveOutcome::Update(spec)
                        }
                        _ => LiveOutcome::Close,
                    },
                );
            }
            "todo" => {
                let now_unix = || {
                    SystemTime::now()
                        .duration_since(UNIX_EPOCH)
                        .unwrap_or_default()
                        .as_secs()
                };
                let todo = self
                    .todo
                    .get_or_insert_with(|| Box::new(todo::TodoState::load_default()));
                let init = todo.build_flyout(now_unix(), is_dark);
                flyout::show_flyout_live_with_options(
                    "todo",
                    owner_hwnd,
                    is_dark,
                    anchor_x,
                    anchor_y,
                    init,
                    border,
                    options,
                    |ev| match ev {
                        LiveEvent::Click(click_id) => {
                            if let Some(todo) = self.todo.as_mut()
                                && let Some(action) = todo.flyout_action_id_for_click(click_id)
                                && todo.handle_flyout_action(&action, None)
                            {
                                let _ = todo.save_default();
                            }
                            let spec = self.todo.as_ref().map_or_else(
                                || todo::TodoState::default().build_flyout(now_unix(), is_dark),
                                |todo| todo.build_flyout(now_unix(), is_dark),
                            );
                            LiveOutcome::Update(spec)
                        }
                        LiveEvent::TextCommit {
                            id: todo::FLYOUT_ADD_INPUT_ID,
                            text,
                        } => {
                            if let Some(todo) = self.todo.as_mut()
                                && todo.handle_flyout_action("add", Some(&text))
                            {
                                let _ = todo.save_default();
                            }
                            let spec = self.todo.as_ref().map_or_else(
                                || todo::TodoState::default().build_flyout(now_unix(), is_dark),
                                |todo| todo.build_flyout(now_unix(), is_dark),
                            );
                            LiveOutcome::Update(spec)
                        }
                        _ => LiveOutcome::Close,
                    },
                );
            }
            "agents" => {
                let agents = self
                    .agents
                    .get_or_insert_with(|| Box::new(agents::AgentMonitorState::start()));
                agents.poll_events();
                let init = agents.build_flyout(is_dark);
                flyout::show_flyout_live_with_options(
                    "agents",
                    owner_hwnd,
                    is_dark,
                    anchor_x,
                    anchor_y,
                    init,
                    border,
                    options,
                    |ev| match ev {
                        LiveEvent::Click(click_id) => {
                            if let Some(agents) = self.agents.as_mut() {
                                agents.poll_events();
                                let _ = agents.handle_flyout_click(click_id);
                            }
                            let spec = self.agents.as_ref().map_or_else(
                                || agents::AgentMonitorState::disabled().build_flyout(is_dark),
                                |agents| agents.build_flyout(is_dark),
                            );
                            LiveOutcome::Update(spec)
                        }
                        _ => LiveOutcome::Close,
                    },
                );
            }
            _ => {}
        }
        false
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn all_22_widgets_registered_with_valid_categories() {
        assert_eq!(
            catalog().len(),
            opentaskbar_core::settings::CATALOG_WIDGET_COUNT
        );
        let mut seen = std::collections::HashSet::new();
        for w in catalog() {
            assert!(seen.insert(w.id), "duplicate widget id {}", w.id);
            assert!(
                WIDGET_CATEGORIES.contains(&w.category),
                "unknown category {} for {}",
                w.category,
                w.id
            );
            assert!(w.width_dips >= 48.0);
        }
        assert!(widget_descriptor("clock").is_some());
        assert!(widget_descriptor("aiusage_claude").is_some());
        assert!(widget_descriptor("aiusage_cursor").is_some());
        assert!(widget_descriptor("bluetoothbattery").is_some());
        assert!(widget_descriptor("dayprogress").is_some());
        assert!(widget_descriptor("keyboard").is_some());
    }

    #[test]
    fn flyout_gate_covers_every_flyout_widget() {
        // Derived from the descriptor table so a newly added flyout widget is covered
        // automatically; AI providers and custom widgets have their own dispatch branches.
        for w in registry::BUILTINS {
            assert_eq!(
                widget_has_flyout(w.id),
                w.has_flyout,
                "descriptor/gate mismatch for {}",
                w.id
            );
            assert_eq!(widget_has_flyout(&w.id.to_uppercase()), w.has_flyout);
        }
        for ai in telemetry::AI_PROVIDERS {
            assert!(
                !widget_has_flyout(ai.id),
                "{} should not have a flyout",
                ai.id
            );
        }
        assert!(widget_has_flyout("CUSTOM:demo"));
        for disabled in ["battery", "unknown"] {
            assert!(
                !widget_has_flyout(disabled),
                "{disabled} should not have a flyout"
            );
        }
    }

    #[test]
    fn calendar_cell_mapping_follows_culture_first_day() {
        // 2024-01-01 was a Monday (.NET weekday 1).
        // en-US: Sunday-first -> one leading blank.
        let en_us = 0;
        assert_eq!(telemetry::calendar_leading_blanks(en_us, 1), 1);
        assert_eq!(
            WidgetRuntime::calendar_cell_date(2024, 1, 0, en_us),
            (2023, 12, 31)
        );
        assert_eq!(
            WidgetRuntime::calendar_cell_date(2024, 1, 1, en_us),
            (2024, 1, 1)
        );
        // Arabic: Saturday-first -> two leading blanks.
        let ar_sa = 6;
        assert_eq!(telemetry::calendar_leading_blanks(ar_sa, 1), 2);
        assert_eq!(
            WidgetRuntime::calendar_cell_date(2024, 1, 0, ar_sa),
            (2023, 12, 30)
        );
        assert_eq!(
            WidgetRuntime::calendar_cell_date(2024, 1, 2, ar_sa),
            (2024, 1, 1)
        );
        // Monday-first (de-DE) keeps the historical no-blank mapping for Monday.
        assert_eq!(telemetry::calendar_leading_blanks(1, 1), 0);
        assert_eq!(
            WidgetRuntime::calendar_cell_date(2024, 1, 0, 1),
            (2024, 1, 1)
        );
    }

    #[test]
    fn calendar_cell_mapping_handles_sunday_first_months_and_month_edges() {
        // 2023-10-01 was a Sunday: no blanks in en-US, six in Monday-first.
        assert_eq!(telemetry::weekday_of(2023, 10, 1), 0);
        assert_eq!(telemetry::calendar_leading_blanks(0, 0), 0);
        assert_eq!(telemetry::calendar_leading_blanks(1, 0), 6);
        assert_eq!(
            WidgetRuntime::calendar_cell_date(2023, 10, 0, 0),
            (2023, 10, 1)
        );
        assert_eq!(
            WidgetRuntime::calendar_cell_date(2023, 10, 0, 1),
            (2023, 9, 25)
        );
        // Trailing cells still roll into the next month.
        assert_eq!(
            WidgetRuntime::calendar_cell_date(2024, 1, 32, 0),
            (2024, 2, 1)
        );
        assert!(telemetry::first_day_of_week() <= 6);
    }

    #[test]
    fn hover_does_not_request_settings_reload() {
        // WS-1 regression: opening a flyout on hover (no live user event) must
        // never report "settings changed" to the dock, even for the
        // bluetoothbattery widget that used to return `true` unconditionally.
        assert!(!WidgetRuntime::flyout_settings_changed(
            "bluetoothbattery",
            false
        ));
        assert!(!WidgetRuntime::flyout_settings_changed(
            "quicksettings",
            false
        ));
        assert!(!WidgetRuntime::flyout_settings_changed("stats", false));
        // Real click/commit paths keep the previous save + live-reload behaviour.
        assert!(WidgetRuntime::flyout_settings_changed(
            "bluetoothbattery",
            true
        ));
        assert!(WidgetRuntime::flyout_settings_changed(
            "quicksettings",
            true
        ));
        // Widgets without a click-driven settings mutation never request a reload.
        assert!(!WidgetRuntime::flyout_settings_changed("stats", true));
    }

    #[test]
    fn widget_poll_snapshot_has_one_second_ttl() {
        assert_eq!(POLL_TTL, std::time::Duration::from_secs(1));
        let now = Instant::now();
        let mut snap = WidgetPollSnapshot::with_samples(Vec::new(), "EN".to_owned(), now);
        assert!(snap.is_fresh(now + std::time::Duration::from_millis(900)));
        assert!(!snap.is_fresh(now + POLL_TTL));
        // A subsequent refresh produces an empty drive list without touching the cache timestamp.
        snap.drives.clear();
        assert!(snap.keyboard_code == "EN");
    }

    #[test]
    fn animation_tick_does_not_change_poll_snapshot() {
        let mut runtime = WidgetRuntime::default();
        let settings = DockSettings::default();
        let before = runtime.poll_snapshot.sampled_at();
        runtime.poll_snapshot.refresh();
        let after = runtime.poll_snapshot.sampled_at();
        assert!(!runtime.animation_tick(&settings));
        assert_eq!(runtime.poll_snapshot.sampled_at(), after);
        assert_ne!(before, after);
    }

    #[test]
    fn fast_ticks_accumulate_into_whole_seconds_with_centisecond_carry() {
        let mut runtime = WidgetRuntime::default();
        let settings = DockSettings::default();
        runtime.stopwatch_running = true;
        runtime.countdown_running = true;
        runtime.countdown_remaining_secs = 10;
        for _ in 0..60 {
            runtime.advance_timers(&settings, 0.016);
        }
        assert_eq!(
            runtime.stopwatch_secs, 0,
            "0.96 s must not become a second yet"
        );
        assert_eq!(runtime.stopwatch_cs, 96);
        assert_eq!(runtime.countdown_remaining_secs, 10);
        runtime.advance_timers(&settings, 0.05);
        assert_eq!(runtime.stopwatch_secs, 1);
        assert_eq!(runtime.stopwatch_cs, 1);
        assert_eq!(runtime.countdown_remaining_secs, 9);
        let before = (runtime.stopwatch_secs, runtime.stopwatch_cs);
        assert!(!runtime.animation_tick(&settings));
        assert_eq!((runtime.stopwatch_secs, runtime.stopwatch_cs), before);
    }

    #[test]
    fn focus_secondary_click_resets_the_shared_session() {
        let mut runtime = WidgetRuntime::default();
        runtime.focus.with_mut(|s| {
            s.running = true;
            s.remaining_secs = 1;
        });
        runtime.focus_running = true;
        runtime.focus_remaining_secs = 1;
        let mut settings = DockSettings::default();
        assert!(matches!(
            runtime.handle_secondary_click("focus", &mut settings),
            Some(DirectClickOutcome::Handled)
        ));
        let snap = runtime.focus.snapshot();
        assert!(!snap.running);
        assert_eq!(snap.remaining_secs, snap.focus_minutes * 60);
        assert_eq!(runtime.focus_remaining_secs, snap.remaining_secs);
        assert_eq!(runtime.focus_running, snap.running);
    }

    #[test]
    fn weather_override_is_shared_by_chip_flyout_and_tooltip() {
        let mut settings = DockSettings::default();
        settings
            .extra
            .insert("WeatherUnit".into(), serde_json::json!("Fahrenheit"));
        let snap = telemetry::WeatherCache {
            temp_c_val: Some(0.0),
            feels_like_c_val: Some(10.0),
            today_hi_c: Some(5.0),
            today_lo_c: Some(-5.0),
            city: "Izmir".to_owned(),
            ..Default::default()
        };
        let (temp, feels, range) = weather_display(&snap, &settings);
        assert_eq!(temp, "32°F");
        assert_eq!(feels, "50°F");
        assert_eq!(range, "41°F / 23°F");
        let runtime = WidgetRuntime::default();
        *runtime.weather.lock().unwrap() = snap;
        let chip = runtime.chip_spec("weather", &settings, true);
        let WidgetChipSpec::Weather {
            temp, today_range, ..
        } = chip
        else {
            panic!("expected weather chip");
        };
        assert_eq!(temp, "32°F");
        assert_eq!(today_range.as_deref(), Some("41°F / 23°F"));
    }

    #[test]
    fn tooltip_cache_invalidates_on_settings_change() {
        let mut runtime = WidgetRuntime::default();
        let mut settings = DockSettings {
            weather_city: "Izmir".to_owned(),
            ..Default::default()
        };
        let _ = runtime.hover_tooltip("weather", None, &settings);
        let first_key = runtime.tooltip_text.as_ref().unwrap().settings_key.clone();
        settings.weather_city = "Ankara".to_owned();
        let _ = runtime.hover_tooltip("weather", None, &settings);
        let second_key = runtime.tooltip_text.as_ref().unwrap().settings_key.clone();
        assert_ne!(first_key, second_key);
        assert!(second_key.contains("Ankara"));
    }

    #[test]
    fn live_stats_tick_keeps_the_cached_top_process_list() {
        let mut runtime = WidgetRuntime::default();
        let settings = DockSettings::default();
        let stamp = Instant::now();
        runtime.last_stats_top = Some(stamp);
        let _ = runtime.live_spec("stats", &settings, true);
        assert_eq!(runtime.last_stats_top, Some(stamp));
    }

    #[test]
    fn media_runtime_exposes_timeline_position_without_rebuilding_state() {
        let mut runtime = WidgetRuntime::default();
        let snapshot = telemetry::MediaSessionSnapshot {
            title: "Song".into(),
            artist: "Artist".into(),
            playing: true,
            position_secs: 10,
            duration_secs: 100,
            ..Default::default()
        };
        {
            let mut guard = runtime.media_session.lock().unwrap();
            *guard = snapshot;
        }
        let sample = media_ext::timeline_from_snapshot(
            &runtime.media_session.lock().unwrap(),
            runtime.timeline_epoch,
        );
        runtime
            .media_timeline
            .update(sample, 1.0, runtime.now_secs(), runtime.now_secs());
        let shown = runtime.media_snapshot().position_secs;
        assert!(
            shown >= 10,
            "extrapolated position must not rewind the SMTC sample"
        );
    }

    fn wireless_sample(id: &str, level_01: f32, stale: bool) -> wireless_battery::WirelessReading {
        wireless_battery::WirelessReading {
            id: id.to_owned(),
            name: "Arctis Nova Pro Wireless".to_owned(),
            level_01,
            sampled_at: Instant::now(),
            is_connected: !stale,
            is_charging: true,
            source: wireless_battery::BatterySource::SteelSeriesHid,
            stale,
            device_kind: wireless_battery::WirelessDeviceKind::Headphones,
            secondary_level: Some(0.5),
        }
    }

    #[test]
    fn wireless_rows_carry_every_sample_field_together() {
        let row = wireless_reading_to_row(wireless_sample("hid:1038:12E0:A", 0.75, false))
            .expect("known level");
        assert_eq!(row.percent, 75);
        assert_eq!(row.secondary_level, Some(50));
        assert!(row.sampled_at.is_some());
        assert!(row.is_live() && row.charging);
        assert_eq!(row.kind, BluetoothDeviceKind::Headphones);

        // A retained last-known row is not live and never "charging".
        let stale = wireless_reading_to_row(wireless_sample("hid:1038:12E0:B", 0.75, true))
            .expect("known level");
        assert!(!stale.is_live() && !stale.charging);

        // Unknown charge is dropped rather than rendered as 0%.
        assert!(wireless_reading_to_row(wireless_sample("hid:1038:12E0:C", -1.0, false)).is_none());
    }

    #[test]
    fn wireless_ignore_binds_to_the_stable_id() {
        let a = wireless_reading_to_row(wireless_sample("hid:1038:12E0:A", 0.5, false)).unwrap();
        let b = wireless_reading_to_row(wireless_sample("hid:1038:12E0:B", 0.5, false)).unwrap();
        let ignored = vec![a.id.clone()];
        assert!(bt_is_ignored(&ignored, &a));
        assert!(
            !bt_is_ignored(&ignored, &b),
            "same-name device must stay visible"
        );
    }

    #[test]
    fn wireless_powershell_fallback_respects_disabled_vendors_and_ignores() {
        let mut settings = DockSettings::default();
        assert!(should_run_powershell_fallback(&settings, true, false));
        // Native Bluetooth saw a device (e.g. hidden via ignore list): do not spawn PowerShell.
        assert!(!should_run_powershell_fallback(&settings, true, true));
        // Explicitly disabling Bluetooth must also suppress the PowerShell fallback.
        settings.set_wireless_disabled_vendors(vec!["bluetooth".to_owned()]);
        assert!(!should_run_powershell_fallback(&settings, true, false));

        let mut policy_settings = DockSettings::default();
        policy_settings
            .ignored_wireless_devices
            .push("bt:AABBCCDDEEFF".to_owned());
        policy_settings
            .wireless_device_aliases
            .insert("bt:112233445566".to_owned(), "Work Buds".to_owned());
        let parsed = telemetry::parse_bluetooth_powershell_lines(
            "Hidden|BTHLE\\DEV_AABBCCDDEEFF\\7&1&0|True|70\n\
             Buds|BTHLE\\DEV_112233445566\\7&1&0|True|85",
        );
        let filtered = filter_powershell_fallback_rows(&policy_settings, parsed);
        assert_eq!(filtered.len(), 1);
        assert_eq!(filtered[0].id, "bt:112233445566");
        assert_eq!(filtered[0].name, "Work Buds");
    }

    #[test]
    fn wireless_stale_label_formats_percent_and_name_without_double_percent() {
        let pct = format_percent(75);
        let stale_pct = loc::t_fmt("WirelessBatteryStale", &[&pct]);
        assert!(stale_pct.contains("75"));
        assert!(stale_pct.contains('%'));
        assert!(!stale_pct.contains("%%"));

        let stale_name = loc::t_fmt("WirelessBatteryStale", &["AirPods Pro"]);
        assert!(stale_name.contains("AirPods Pro"));
        assert!(!stale_name.contains('%'));
    }

    #[test]
    fn wireless_refresh_guard_clears_queue_and_loading_on_unwind() {
        let queue = Arc::new(Mutex::new(WirelessRefreshQueue {
            in_flight: true,
            pending: Some((DockSettings::default(), true)),
        }));
        let cache = Arc::new(Mutex::new(telemetry::BluetoothBatteryCache {
            loading: true,
            ..Default::default()
        }));
        let live_update = Arc::new(Mutex::new(None));
        let q2 = Arc::clone(&queue);
        let c2 = Arc::clone(&cache);
        let l2 = Arc::clone(&live_update);
        let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(move || {
            let _guard = WirelessRefreshGuard {
                queue: q2,
                cache: c2,
                live_update: l2,
                completed: false,
            };
            panic!("simulated worker panic");
        }));
        let q = queue.lock().unwrap();
        assert!(!q.in_flight);
        assert!(q.pending.is_none());
        assert!(!cache.lock().unwrap().loading);
    }

    #[test]
    fn wireless_poll_cadence_uses_the_validated_range() {
        let mut settings = DockSettings {
            wireless_battery_poll_seconds: 5,
            ..Default::default()
        };
        assert_eq!(wireless_poll_secs(&settings), 15);
        settings.wireless_battery_poll_seconds = 45;
        assert_eq!(wireless_poll_secs(&settings), 45);
    }

    #[test]
    fn bluetooth_threshold_cycles_zero_ten_twenty_thirty() {
        assert_eq!(bt_next_threshold(20), 30);
        assert_eq!(bt_next_threshold(30), 0);
        assert_eq!(bt_next_threshold(0), 10);
        assert_eq!(bt_next_threshold(-1), 0);
    }

    #[test]
    fn gpu_thermal_hysteresis_uses_five_entries_and_two_exits() {
        let mut runtime = WidgetRuntime::default();
        assert_eq!(thermal_band(69), 0);
        assert_eq!(thermal_band(70), 1);
        assert_eq!(thermal_band(85), 2);
        for _ in 0..4 {
            assert_eq!(runtime.update_gpu_thermal(Some(72)), 0);
        }
        assert_eq!(runtime.update_gpu_thermal(Some(72)), 1);
        assert_eq!(runtime.update_gpu_thermal(Some(64)), 1);
        assert_eq!(runtime.update_gpu_thermal(Some(64)), 0);
    }

    #[test]
    fn live_spec_rebuilds_clock_and_stats_from_current_state() {
        let mut runtime = WidgetRuntime::default();
        let settings = DockSettings::default();
        let clock = runtime.live_spec("clock", &settings, true);
        assert!(
            clock
                .blocks
                .iter()
                .any(|b| matches!(b, FlyoutBlock::Hero { .. }))
        );
        assert!(
            clock
                .blocks
                .iter()
                .any(|b| matches!(b, FlyoutBlock::CalendarGrid { .. }))
        );
        let stats = runtime.live_spec("stats", &settings, true);
        assert!(
            stats
                .blocks
                .iter()
                .any(|b| matches!(b, FlyoutBlock::Chart { .. }))
        );
        let stopwatch = runtime.live_spec("stopwatch", &settings, true);
        assert!(!stopwatch.blocks.is_empty());
        let countdown = runtime.live_spec("countdown", &settings, true);
        assert!(!countdown.blocks.is_empty());
    }

    #[test]
    fn quick_settings_registers_seventeen_distinct_shortcuts() {
        assert_eq!(QUICK_SETTINGS_SHORTCUT_COUNT, 17);
        let mut uris = std::collections::HashSet::new();
        for id in 100..=116 {
            let uri = shortcut_uri(id);
            assert_ne!(uri, "ms-settings:", "shortcut {id} must have a URI");
            assert!(uris.insert(uri), "duplicate shortcut URI for {id}");
        }
    }

    #[test]
    fn catalog_and_ai_flyout_use_the_seven_remaining_providers() {
        assert!(
            !catalog()
                .iter()
                .any(|w| w.id.eq_ignore_ascii_case("aiusage_ag"))
        );
        assert_eq!(
            catalog()
                .iter()
                .filter(|w| w.id.starts_with("aiusage_"))
                .count(),
            7
        );
        assert_eq!(
            catalog()
                .iter()
                .filter(|w| w.category == "AI Quotas")
                .count(),
            8
        );
        assert!(canonical_widget_id("btbattery") == "bluetoothbattery");
    }

    #[test]
    fn cached_ai_full_detail_is_rendered_instead_of_default() {
        let runtime = WidgetRuntime::default();
        let full = telemetry::AiFullDetail {
            provider_id: "aiusage_claude".to_owned(),
            plan: Some("Pro".to_owned()),
            used: 0.5,
            ..Default::default()
        };
        runtime
            .ai_full
            .lock()
            .unwrap()
            .insert("aiusage_claude".to_owned(), (Instant::now(), full));
        let snap = telemetry::AiUsageSnapshot::default();
        let detail = runtime.ai_flyout_detail("aiusage_claude", &snap);
        assert_eq!(detail.plan.as_deref(), Some("Pro"));
        assert!(runtime.ai_full_detail("aiusage_claude").is_some());
        assert!(runtime.ai_full_detail("aiusage_codex").is_none());
    }

    #[test]
    fn stopped_stopwatch_centiseconds_do_not_drift() {
        let mut runtime = WidgetRuntime::default();
        let settings = DockSettings::default();
        runtime.stopwatch_running = false;
        runtime.stopwatch_secs = 12;
        runtime.stopwatch_cs = 34;
        for _ in 0..40 {
            runtime.advance_timers(&settings, 0.05);
        }
        assert_eq!(runtime.stopwatch_secs, 12);
        assert_eq!(runtime.stopwatch_cs, 34);
    }

    #[test]
    fn direct_clicks_open_flyout_for_all_flyout_widgets() {
        let mut runtime = WidgetRuntime::default();
        let mut settings = DockSettings::default();
        for id in [
            "clock",
            "weather",
            "dayprogress",
            "stats",
            "netspeed",
            "bluetoothbattery",
            "storage",
            "audio",
            "keyboard",
            "aiusage_claude",
            "aiusage_cursor",
            "aiusage_all",
        ] {
            assert_eq!(
                runtime.handle_direct_click(id, 20.0, 140.0, &mut settings),
                DirectClickOutcome::OpenFlyout,
                "{id} should open its flyout on direct click"
            );
        }
        // Media body is inert; QuickSettings 4th zone opens Control Center flyout.
        assert_eq!(
            runtime.handle_direct_click("media", 20.0, 244.0, &mut settings),
            DirectClickOutcome::Handled
        );
        assert_eq!(
            runtime.handle_direct_click("quicksettings", 120.0, 138.0, &mut settings),
            DirectClickOutcome::OpenFlyout
        );
    }

    #[test]
    fn middle_click_records_lap_when_running_and_resets_when_stopped() {
        let mut runtime = WidgetRuntime::default();
        let settings = DockSettings::default();
        runtime.stopwatch_running = true;
        runtime.stopwatch_secs = 7;
        runtime.stopwatch_cs = 42;
        assert!(runtime.handle_middle_click("stopwatch", &settings));
        assert_eq!(runtime.stopwatch_laps, vec![(7, 42)]);
        runtime.stopwatch_running = false;
        assert!(runtime.handle_middle_click("stopwatch", &settings));
        assert_eq!(runtime.stopwatch_secs, 0);
        assert_eq!(runtime.stopwatch_cs, 0);
        assert!(runtime.stopwatch_laps.is_empty());
        assert!(!runtime.handle_middle_click("unknown_widget", &settings));
    }

    #[test]
    fn clock_weekday_redundancy_and_percent_helpers() {
        assert!(clock_weekday_is_redundant("Fri, Nov 27", "Fri"));
        assert!(clock_weekday_is_redundant("Cum, 27 Kas", "cum"));
        assert!(clock_weekday_is_redundant("27.11.2026", ""));
        assert!(!clock_weekday_is_redundant("27.11.2026", "Cum"));
        let pct = format_percent(42);
        assert!(pct.contains("42"));
        assert!(pct.contains('%'));
        let dp = day_progress_percent_text(0.5);
        assert!(dp.contains("50"));
    }

    #[test]
    fn wheel_step_uses_five_percent_per_detent() {
        let mut rem = 0;
        assert_eq!(media_ext::volume_from_wheel_step(50, 120, &mut rem, 5), 55);
        assert_eq!(rem, 0);
        assert_eq!(media_ext::volume_from_wheel_step(55, -120, &mut rem, 5), 50);
        assert_eq!(rem, 0);
        // Fractional high-res scroll carries across calls.
        assert_eq!(media_ext::volume_from_wheel_step(50, 60, &mut rem, 5), 50);
        assert_eq!(rem, 60);
        assert_eq!(media_ext::volume_from_wheel_step(50, 60, &mut rem, 5), 55);
        assert_eq!(rem, 0);
    }

    #[test]
    fn ai_usage_ring_changes_ai_chip_spec_and_honors_per_provider_override() {
        let runtime = WidgetRuntime::default();
        {
            let mut guard = runtime.ai_usage.lock().unwrap();
            guard.providers.insert(
                "aiusage_claude".to_owned(),
                telemetry::AiUsageSnapshot {
                    windows: vec![
                        telemetry::AiWindowData {
                            label: "5h".to_owned(),
                            used: 0.20,
                            reset_text: "2h 0m".to_owned(),
                        },
                        telemetry::AiWindowData {
                            label: "7d".to_owned(),
                            used: 0.80,
                            reset_text: "3d 0h".to_owned(),
                        },
                    ],
                    has_cred: true,
                    ..Default::default()
                },
            );
            guard.providers.insert(
                "aiusage_opencode".to_owned(),
                telemetry::AiUsageSnapshot {
                    windows: vec![
                        telemetry::AiWindowData {
                            label: "1h".to_owned(),
                            used: 0.15,
                            reset_text: "30m".to_owned(),
                        },
                        telemetry::AiWindowData {
                            label: "7d".to_owned(),
                            used: 0.45,
                            reset_text: "4d".to_owned(),
                        },
                        telemetry::AiWindowData {
                            label: "30d".to_owned(),
                            used: 0.90,
                            reset_text: "12d".to_owned(),
                        },
                    ],
                    has_cred: true,
                    ..Default::default()
                },
            );
        }

        let mut settings = DockSettings {
            ai_usage_ring: AiUsageRing::Session,
            ..Default::default()
        };
        let WidgetChipSpec::AiDual {
            rows: session_rows, ..
        } = runtime.chip_spec("aiusage_claude", &settings, true)
        else {
            panic!("expected AiDual");
        };
        assert_eq!(session_rows[0].0, "5h");

        settings.ai_usage_ring = AiUsageRing::Weekly;
        let WidgetChipSpec::AiDual {
            rows: weekly_rows, ..
        } = runtime.chip_spec("aiusage_claude", &settings, true)
        else {
            panic!("expected AiDual");
        };
        assert_eq!(weekly_rows[0].0, "7d");

        settings.ai_usage_ring = AiUsageRing::Highest;
        let WidgetChipSpec::AiOpenCode {
            columns: highest_cols,
        } = runtime.chip_spec("aiusage_opencode", &settings, true)
        else {
            panic!("expected AiOpenCode");
        };
        assert_eq!(highest_cols[0].0, "30d");

        // Per-provider override in settings.extra["AiUsageRingOverrides"] wins over global.
        settings.extra.insert(
            "AiUsageRingOverrides".to_owned(),
            serde_json::json!({ "aiusage_claude": "Session" }),
        );
        assert_eq!(
            resolved_ai_usage_ring(&settings, "aiusage_claude"),
            AiUsageRing::Session
        );
        let WidgetChipSpec::AiDual {
            rows: overridden_rows,
            ..
        } = runtime.chip_spec("aiusage_claude", &settings, true)
        else {
            panic!("expected AiDual");
        };
        assert_eq!(overridden_rows[0].0, "5h");
    }

    #[test]
    fn per_widget_density_drives_weather_and_storage_compact_widths() {
        let runtime = WidgetRuntime {
            poll_snapshot: WidgetPollSnapshot::with_samples(
                vec![
                    telemetry::DriveSpace {
                        letter: 'C',
                        free_gb: 100.0,
                        total_gb: 500.0,
                    },
                    telemetry::DriveSpace {
                        letter: 'D',
                        free_gb: 200.0,
                        total_gb: 1000.0,
                    },
                ],
                "EN".to_owned(),
                Instant::now(),
            ),
            ..Default::default()
        };
        let mut settings = DockSettings::default();
        assert_eq!(
            runtime.widget_width_dips_with_settings("weather", &settings),
            196.0
        );
        let full_storage = runtime.widget_width_dips_with_settings("storage", &settings);
        settings
            .widget_density_overrides
            .insert("weather".to_owned(), WidgetDensityMode::Compact);
        settings
            .widget_density_overrides
            .insert("storage".to_owned(), WidgetDensityMode::Compact);
        assert_eq!(
            runtime.widget_width_dips_with_settings("weather", &settings),
            152.0
        );
        assert!(runtime.widget_width_dips_with_settings("storage", &settings) < full_storage);
        let WidgetChipSpec::Storage { drives } = runtime.chip_spec("storage", &settings, true)
        else {
            panic!("expected Storage chip");
        };
        assert_eq!(drives.len(), 1);
    }

    #[test]
    fn island_detail_spec_covers_all_22_builtin_widgets_with_localized_titles() {
        let mut runtime = WidgetRuntime::default();
        let settings = DockSettings::default();
        let builtins: Vec<&str> = catalog().iter().map(|w| w.id).collect();
        assert_eq!(
            builtins.len(),
            opentaskbar_core::settings::CATALOG_WIDGET_COUNT
        );
        for id in builtins {
            let spec = runtime.island_detail_spec(id, &settings);
            assert_eq!(spec.widget_id, id);
            assert!(!spec.title.trim().is_empty(), "empty title for {id}");
            assert!(
                !spec.title.starts_with("aiusage_"),
                "title for {id} must be localized, got {}",
                spec.title
            );
            assert!(!spec.rows.is_empty(), "rows must not be empty for {id}");
            assert!(spec.rows.len() <= 4, "too many rows for {id}");
            assert!(spec.actions.len() <= 3, "too many actions for {id}");
            assert_eq!(spec.has_full_flyout, widget_has_flyout(id));
        }

        // Verify quick actions mutate timer state cleanly.
        assert!(runtime.handle_island_detail_action("stopwatch", "toggle", &settings));
        assert!(runtime.stopwatch_running);
        assert!(runtime.handle_island_detail_action("stopwatch", "lap", &settings));
        assert_eq!(runtime.stopwatch_laps.len(), 1);
        assert!(runtime.handle_island_detail_action("stopwatch", "reset", &settings));
        assert!(!runtime.stopwatch_running);
        assert!(runtime.stopwatch_laps.is_empty());

        let prev_cd = runtime.countdown_remaining_secs;
        assert!(runtime.handle_island_detail_action("countdown", "add_1m", &settings));
        assert_eq!(runtime.countdown_remaining_secs, prev_cd + 60);
    }

    #[test]
    fn lazy_subsystems_stay_unallocated_until_enabled_and_drop_when_disabled() {
        let mut runtime = WidgetRuntime::default();
        let mut settings = DockSettings::default();
        assert_eq!(runtime.lazy_subsystems_allocated(), 0);
        runtime.tick(&settings);
        assert_eq!(runtime.lazy_subsystems_allocated(), 0);

        settings.set_clipboard_history_enabled(true);
        runtime.tick(&settings);
        assert_eq!(runtime.lazy_subsystems_allocated(), 1);
        settings.set_clipboard_history_enabled(false);
        runtime.tick(&settings);
        assert_eq!(runtime.lazy_subsystems_allocated(), 0);

        settings.set_media_lyrics_enabled(true);
        runtime.tick(&settings);
        assert_eq!(runtime.lazy_subsystems_allocated(), 1);
        settings.set_media_lyrics_enabled(false);
        runtime.tick(&settings);
        assert_eq!(runtime.lazy_subsystems_allocated(), 0);
    }
}
