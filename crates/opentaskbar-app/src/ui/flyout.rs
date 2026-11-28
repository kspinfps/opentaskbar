//! Shared reference parity `DockFlyout` window rendered with Slate Neutral + Aurora design tokens.
//!
//! Used across Background Apps (System Tray), Folder Stacks, App Groups, and all 23 Dock Widgets.
//! Supports:
//! - Exact `DockFlyout.cs` visuals (`#252B37` dark / `#FFFFFF` light, `14px` padding, 190ms ease-out)
//! - In-place live interactive updates (`show_flyout_live`) for Calendar month/year navigation,
//!   Audio Output/Input continuous slider drag & mute, and Focus/Stopwatch/Countdown timers
//! - Full reference widget blocks: `CalendarGrid`, `Slider`, `Sparkline`, `MetricTiles`, `BadgeRow`,
//!   `StatusHeader`, `Hero`, `ToggleGrid`, `ProgressBar`, `StatRow`, `ActionRow`, `AppRow`, `ButtonBar`
//! - Mouse-wheel vertical scrolling + thin scrollbar thumb + toggle-close suppression.

use crate::design::d2d::{
    D2dIconBitmapCache, DcD2d, TextAlign, d2d_color as d2d_rgba, d2d_rect, paint_dib_d2d,
};
use crate::platform::win32_helpers::{pt_in_rect, set_dwm_u32, set_window_long_ptr};
use opentaskbar_core::i18n as loc;

use std::cell::RefCell;
use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant};

use windows::Win32::Graphics::Direct2D::Common::D2D1_COLOR_F;
use windows::Win32::Graphics::Direct2D::D2D1_ANTIALIAS_MODE_PER_PRIMITIVE;
use windows::Win32::Graphics::DirectWrite::{
    DWRITE_FONT_WEIGHT_BOLD, DWRITE_FONT_WEIGHT_MEDIUM, DWRITE_FONT_WEIGHT_NORMAL,
    DWRITE_FONT_WEIGHT_SEMI_BOLD,
};
use windows_sys::Win32::Foundation::{HWND, LPARAM, LRESULT, POINT, RECT, WPARAM};
use windows_sys::Win32::Graphics::Dwm::{
    DWMWA_BORDER_COLOR, DWMWA_SYSTEMBACKDROP_TYPE, DWMWA_USE_IMMERSIVE_DARK_MODE,
    DWMWA_WINDOW_CORNER_PREFERENCE,
};
#[cfg(test)]
use windows_sys::Win32::Graphics::Gdi::{
    DT_END_ELLIPSIS, DT_LEFT, DT_SINGLELINE, DT_VCENTER, DrawTextW, HDC,
};
use windows_sys::Win32::Graphics::Gdi::{
    GetMonitorInfoW, InvalidateRect, MONITOR_DEFAULTTONEAREST, MONITORINFO, MonitorFromPoint,
};
use windows_sys::Win32::System::LibraryLoader::GetModuleHandleW;
use windows_sys::Win32::UI::HiDpi::{GetDpiForMonitor, MDT_EFFECTIVE_DPI};
use windows_sys::Win32::UI::Input::KeyboardAndMouse::{
    GetAsyncKeyState, GetKeyState, ReleaseCapture, SetCapture, TME_LEAVE, TRACKMOUSEEVENT,
    TrackMouseEvent, VK_BACK, VK_DELETE, VK_ESCAPE, VK_LBUTTON, VK_MBUTTON, VK_RBUTTON, VK_RETURN,
    VK_SHIFT, VK_SPACE, VK_TAB,
};
use windows_sys::Win32::UI::WindowsAndMessaging::{
    CallNextHookEx, CreateWindowExW, DefWindowProcW, DestroyWindow, DispatchMessageW,
    GWLP_USERDATA, GetClassNameW, GetClientRect, GetCursorPos, GetForegroundWindow, GetMessageW,
    GetWindowLongPtrW, GetWindowRect, HC_ACTION, HHOOK, HWND_TOPMOST, IDC_ARROW, IDC_HAND,
    IsWindow, KillTimer, LWA_ALPHA, LoadCursorW, MSG, MSLLHOOKSTRUCT, PostMessageW,
    RegisterClassExW, SW_SHOW, SWP_NOACTIVATE, SWP_NOSIZE, SWP_NOZORDER, SWP_SHOWWINDOW, SetCursor,
    SetForegroundWindow, SetLayeredWindowAttributes, SetTimer, SetWindowPos, SetWindowsHookExW,
    ShowWindow, TranslateMessage, UnhookWindowsHookEx, WH_MOUSE_LL, WM_ACTIVATE, WM_CHAR, WM_CLOSE,
    WM_ERASEBKGND, WM_KEYDOWN, WM_KILLFOCUS, WM_LBUTTONDOWN, WM_LBUTTONUP, WM_MOUSEMOVE,
    WM_MOUSEWHEEL, WM_PAINT, WM_RBUTTONDOWN, WM_RBUTTONUP, WM_SETCURSOR, WM_TIMER, WNDCLASSEXW,
    WS_EX_LAYERED, WS_EX_NOREDIRECTIONBITMAP, WS_EX_TOOLWINDOW, WS_EX_TOPMOST, WS_POPUP,
    WindowFromPoint,
};

use super::menu::BorderStyle;
use crate::design::spatial_glass::{
    DCompForeground, SpatialGlassHost, configure_host_backdrop_window,
};
use crate::design::{self, Rgb};
use crate::platform::win32;
use crate::ui::text_input::{MAX_TEXT_CHARS, normalize_numeric, push_char};
use crate::widgets::telemetry;

const WM_MOUSELEAVE: u32 = 0x02A3;
const DWMWCP_ROUND: u32 = 2;
const DWMWA_COLOR_NONE: u32 = 0xFFFF_FFFE;
const MAX_FLYOUT_H_DIPS: f32 = 500.0;
const FLYOUT_PAD_DIPS: f32 = 16.0;
const TIMER_ANIM: usize = 1;
const TIMER_DISMISS: usize = 2;
/// Live widget tick. Deliberately distinct from animation/dismiss so a data
/// refresh can never start (or cancel) the close animation.
const TIMER_LIVE: usize = 3;
/// One second matches the clock's second hand, the stopwatch/countdown display,
/// stats sampling and the network speed-test progress (no sub-second repaint).
const LIVE_TICK_MS: u32 = 1000;
const DISMISS_POLL_MS: u32 = 45;
const HOVER_LEAVE_GRACE: Duration = Duration::from_millis(240);
/// A flyout that just closed must not immediately reopen from the same click that
/// dismissed it. 350 ms covers the release side of a double-click and the reference
/// `DockFlyout` toggle-close window without swallowing a genuine later click.
const REOPEN_SUPPRESSION_WINDOW: Duration = Duration::from_millis(350);
const OPEN_ANIM_MS: f32 = 175.0;
const CLOSE_ANIM_MS: f32 = 95.0;
const SLIDE_OFFSET_DIPS: f32 = 7.0;
/// Private message posted by the WH_MOUSE_LL hook for every button click. The
/// hook itself must never call `WindowFromPoint`/`GetClassNameW`: a hung foreign
/// window would block the low-level hook and freeze the whole system's input.
/// Coordinates travel in (wparam, lparam); the UI-thread wndproc performs the
/// #32768 class check and keeps the flyout alive while that menu is open.
// WM_APP+2 is the dock's WM_APP_ICONS message; use the private WM_APP+9 slot
// so a cross-window post can never land in the dock's icon handler.
const WM_APP_MENU_CANDIDATE: u32 = 0x8009;
/// Async worker -> live flyout wakeup message (`LiveUpdateHandle::notify`).
const WM_APP_LIVE_UPDATE: u32 = 0x800A;

/// Tracks the native `WM_COMMAND`/`#32768` context menu opened from a flyout.
static EXTERNAL_MENU_ACTIVE: AtomicBool = AtomicBool::new(false);
static HOOK_TARGET: AtomicUsize = AtomicUsize::new(0);
static FLYOUT_ANIMATIONS_ENABLED: AtomicBool = AtomicBool::new(true);
/// Target FPS (`0` = auto / monitor refresh rate).
static FLYOUT_TARGET_FPS: AtomicU32 = AtomicU32::new(0);

/// Synchronizes flyout open/close motion with `DockSettings.animations_enabled` and `animation_fps`.
pub fn set_motion_prefs(animations_enabled: bool, target_fps: u32) {
    FLYOUT_ANIMATIONS_ENABLED.store(animations_enabled, Ordering::Relaxed);
    FLYOUT_TARGET_FPS.store(target_fps, Ordering::Relaxed);
}

/// Pure timer interval for a given target FPS. `0` means auto: ~60 Hz instead
/// of the old 125 Hz, halving the SetWindowPos + SetLayeredWindowAttributes
/// load on the dock UI thread (which also hosts the low-level input hooks).
#[must_use]
fn anim_interval_for_fps(fps: u32) -> u32 {
    if fps == 0 {
        16
    } else {
        (1000 / fps.max(15)).max(4)
    }
}

fn flyout_anim_interval_ms() -> u32 {
    anim_interval_for_fps(FLYOUT_TARGET_FPS.load(Ordering::Relaxed))
}

pub fn external_context_menu_active() -> bool {
    EXTERNAL_MENU_ACTIVE.load(Ordering::Acquire)
}

/// Physical work area (`rcWork`) of the monitor nearest to `(x, y)`, plus that
/// monitor's effective DPI scale when it can be read. The work area excludes the
/// Windows taskbar and other app bars, so popups sized and clamped against it never
/// hide their bottom controls behind them or past a screen edge.
pub(crate) fn monitor_work_area_at(x: i32, y: i32) -> (RECT, Option<f32>) {
    // SAFETY: plain monitor queries into stack-owned out-parameters.
    unsafe {
        let hmon = MonitorFromPoint(POINT { x, y }, MONITOR_DEFAULTTONEAREST);
        let mut info = MONITORINFO {
            cbSize: size_of::<MONITORINFO>() as u32,
            ..Default::default()
        };
        let work = if !hmon.is_null() && GetMonitorInfoW(hmon, &mut info) != 0 {
            info.rcWork
        } else {
            let (w, h) = win32::primary_screen_px();
            RECT {
                right: w,
                bottom: h,
                ..Default::default()
            }
        };
        let (mut dpi_x, mut dpi_y) = (0u32, 0u32);
        let dpi = (!hmon.is_null()
            && GetDpiForMonitor(hmon, MDT_EFFECTIVE_DPI, &mut dpi_x, &mut dpi_y) == 0
            && dpi_x > 0)
            .then_some(dpi_x as f32 / 96.0);
        (work, dpi)
    }
}

/// Dock edge the flyout is anchored to. `Bottom` docks open upward, `Top` docks
/// (Dynamic Island) open downward directly below the anchor, side docks sideways.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum FlyoutEdge {
    #[default]
    Bottom,
    Top,
    Left,
    Right,
}

/// Optional `DockFlyout` lifecycle / placement behavior. Existing constructors keep their
/// defaults so widget call sites are unaffected.
#[derive(Clone, Copy, Debug)]
pub struct FlyoutOptions {
    pub stays_open: bool,
    /// Set by [`FlyoutOptions::with_edge`]: the explicit dock/island anchor wins over a
    /// stale journaled position, and the popup only uses the room on its side of it.
    use_anchor_position: bool,
    pub retain_for_external_context_menu: bool,
    pub edge: FlyoutEdge,
    /// Soft-shadow preference kept for call-site compatibility. The popup never
    /// uses `CS_DROPSHADOW` (a square shadow behind rounded corners); its shadow
    /// comes from the DWM rounded-corner frame.
    pub shadow_rings: u8,
}

/// Window class of every dock flyout.
const FLYOUT_CLASS_NAME: &str = "OpenTaskBarFlyout";
/// Class style of every dock flyout: deliberately without `CS_DROPSHADOW`.
const FLYOUT_CLASS_STYLE: u32 = 0;

impl Default for FlyoutOptions {
    fn default() -> Self {
        Self {
            stays_open: false,
            use_anchor_position: false,
            retain_for_external_context_menu: false,
            edge: FlyoutEdge::Bottom,
            shadow_rings: 4,
        }
    }
}

impl FlyoutOptions {
    /// Tray / background-app flyout: each `AppRow` may open the target app's own
    /// native context menu, so arm the #32768 retention hook (WS-6 `dock/tray.rs`).
    #[must_use]
    pub fn tray() -> Self {
        Self {
            retain_for_external_context_menu: true,
            ..Self::default()
        }
    }

    /// Folder stack / app group flyout: clicking outside or re-clicking the
    /// dock item dismisses the popup cleanly like any other dock flyout.
    #[must_use]
    pub fn stack() -> Self {
        Self {
            stays_open: false,
            ..Self::default()
        }
    }

    /// Fluent builder: anchor the popup to the invoking dock `edge`. The explicit
    /// anchor then takes precedence over any journaled position for this tag.
    #[must_use]
    pub const fn with_edge(mut self, edge: FlyoutEdge) -> Self {
        self.edge = edge;
        self.use_anchor_position = true;
        self
    }

    /// Whether the explicit anchor wins over a journaled position.
    #[must_use]
    pub const fn uses_anchor_position(&self) -> bool {
        self.use_anchor_position
    }
}

/// Largest flyout height (physical px) that keeps the whole popup, including its
/// bottom controls, inside the work area `work` (inset by `margin`). An anchored
/// popup only gets the room on its own side of the anchor, but never less than a
/// usable minimum: below that it is better to slide over the anchor than to shrink.
fn flyout_available_height(
    edge: FlyoutEdge,
    anchored: bool,
    work: &RECT,
    anchor_y: i32,
    margin: i32,
    min_h: i32,
) -> i32 {
    let work_h = (work.bottom - work.top - 2 * margin).max(1);
    let room = match edge {
        FlyoutEdge::Top => work.bottom - margin - anchor_y,
        FlyoutEdge::Bottom if anchored => anchor_y - work.top - margin,
        _ => work_h,
    };
    room.max(min_h.min(work_h)).clamp(1, work_h)
}

/// Top-left corner (physical px) of a `width` x `height` flyout anchored at
/// `(anchor_x, anchor_y)` on `edge`, clamped inside the work area `work`.
fn flyout_origin(
    edge: FlyoutEdge,
    work: &RECT,
    anchor_x: i32,
    anchor_y: i32,
    width: i32,
    height: i32,
    margin: i32,
) -> (i32, i32) {
    let left = work.left + margin;
    let top = work.top + margin;
    let max_x = (work.right - width - margin).max(left);
    let max_y = (work.bottom - height - margin).max(top);
    match edge {
        // Bottom dock: open upward, the popup's bottom edge sits on the anchor.
        FlyoutEdge::Bottom => (
            (anchor_x - width / 2).clamp(left, max_x),
            (anchor_y - height).clamp(top, max_y),
        ),
        // Top dock / Dynamic Island: open downward directly below the anchor.
        FlyoutEdge::Top => (
            (anchor_x - width / 2).clamp(left, max_x),
            anchor_y.clamp(top, max_y),
        ),
        // Side docks: open beside the anchor, vertically centred on it.
        FlyoutEdge::Left => (
            anchor_x.clamp(left, max_x),
            (anchor_y - height / 2).clamp(top, max_y),
        ),
        FlyoutEdge::Right => (
            (anchor_x - width).clamp(left, max_x),
            (anchor_y - height / 2).clamp(top, max_y),
        ),
    }
}

/// Per-target close timestamps. A global "last closed" slot wrongly suppresses
/// a different widget opened right after another one closed (e.g. switching from
/// the clock flyout to the tray flyout), so each tag owns its own one-shot entry.
static LAST_CLOSED: OnceLock<Mutex<HashMap<String, Instant>>> = OnceLock::new();

fn last_closed_lock() -> &'static Mutex<HashMap<String, Instant>> {
    LAST_CLOSED.get_or_init(|| Mutex::new(HashMap::new()))
}

/// Pure timing policy, kept separate from the global map so it can be tested with
/// synthetic timestamps. The entry is *consumed* by the caller after a hit.
#[must_use]
fn suppression_window_hit(recorded_at: Option<Instant>, now: Instant) -> bool {
    recorded_at.is_some_and(|at| now.duration_since(at) < REOPEN_SUPPRESSION_WINDOW)
}

/// Consumes the one-shot suppression entry for `tag`, if it is still fresh.
pub fn should_suppress_reopen(tag: &str) -> bool {
    if tag.is_empty() {
        return false;
    }
    let now = Instant::now();
    if let Ok(mut guard) = last_closed_lock().lock() {
        let hit = suppression_window_hit(guard.get(tag).copied(), now);
        guard.remove(tag);
        return hit;
    }
    false
}

fn record_closed(tag: &str) {
    if tag.is_empty() {
        return;
    }
    if let Ok(mut guard) = last_closed_lock().lock() {
        // Bound the map: drop entries that can no longer suppress anything.
        guard.retain(|_, at| at.elapsed() < REOPEN_SUPPRESSION_WINDOW * 2);
        guard.insert(tag.to_owned(), Instant::now());
    }
}

/// An external native menu (#32768) owns focus while it is on screen. The flyout
/// must stay open through that blur even when it was not opened with `stays_open`.
#[must_use]
fn stays_open_for_external_menu(menu_active: bool, base_stays_open: bool) -> bool {
    menu_active || base_stays_open
}

/// Pure WM_CLOSE policy: close requests commit the in-place editor and start the
/// close animation (which owns the modal-loop exit). A repeated close is ignored.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum WmCloseAction {
    Ignore,
    BeginClose,
}

#[must_use]
fn wm_close_action(msg: u32, already_closing: bool) -> WmCloseAction {
    if msg == WM_CLOSE && !already_closing {
        WmCloseAction::BeginClose
    } else {
        WmCloseAction::Ignore
    }
}

/// Only widgets with time-varying state accept a periodic tick. Other live
/// flyouts (audio, quick settings, device lists) deliberately answer
/// unknown events with `LiveOutcome::Close`; ticking them would dismiss them.
#[must_use]
fn live_tick_tag(tag: &str) -> bool {
    matches!(
        tag,
        "clock" | "stats" | "focus" | "stopwatch" | "countdown" | "netspeed" | "media"
    )
}

/// A live tick must not repaint (or re-layout) a flyout that is already closing,
/// and a plain non-live flyout has no callback to answer `LiveEvent::Update`.
#[must_use]
fn live_tick_allowed(closing: bool, has_live_callback: bool) -> bool {
    !closing && has_live_callback
}

/// Culture weekday order for calendar headers. Indices follow the .NET
/// `DayOfWeek` convention (`0 = Sunday .. 6 = Saturday`) used by
/// `telemetry::weekday_name` / `weekday_of`.
#[must_use]
fn calendar_weekday_order(first_day_of_week: u32) -> [u32; 7] {
    let first = first_day_of_week % 7;
    std::array::from_fn(|i| (first + i as u32) % 7)
}

/// Leading blank cells before day 1 for the culture's first day of week.
#[must_use]
fn calendar_leading_blanks(first_day_of_week: u32, weekday_of_first: u32) -> i32 {
    ((weekday_of_first + 7 - first_day_of_week % 7) % 7) as i32
}

/// A tick rebuilds a calendar flyout from "now"; keep the month/year/selection
/// the user navigated to (only click-driven updates may change them).
fn preserve_calendar_navigation(current: &FlyoutSpec, next: &mut FlyoutSpec) {
    let Some((year, month, selected)) = current.blocks.iter().find_map(|block| match block {
        FlyoutBlock::CalendarGrid {
            year,
            month,
            selected,
            ..
        } => Some((*year, *month, *selected)),
        _ => None,
    }) else {
        return;
    };
    for block in &mut next.blocks {
        if let FlyoutBlock::CalendarGrid {
            year: next_year,
            month: next_month,
            selected: next_selected,
            ..
        } = block
        {
            *next_year = year;
            *next_month = month;
            *next_selected = selected;
            return;
        }
    }
}

/// Short localized weekday label (`Sun`, `Mon`, ...) for a .NET day index
/// under an explicit language tag, avoiding Turkish 3-char collisions
/// (`Pazar`/`Pazartesi` -> `Paz`/`Pzt`, `Cuma`/`Cumartesi` -> `Cum`/`Cmt`).
#[must_use]
fn weekday_short_label_for_lang(day_of_week: u32, lang: &str) -> String {
    if lang.to_ascii_lowercase().starts_with("tr") {
        return match day_of_week % 7 {
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
    telemetry::weekday_short_name(day_of_week)
}

/// Short localized weekday label (`Sun`, `Mon`, ...) for a .NET day index.
#[must_use]
fn weekday_short_label(day_of_week: u32) -> String {
    weekday_short_label_for_lang(day_of_week, loc::language())
}

/// Straight BGRA32 icon payload for `FlyoutBlock::ActionRow` / `AppRow`.
#[derive(Clone, Debug)]
pub struct FlyoutIcon {
    pub width: u32,
    pub height: u32,
    pub bgra: Vec<u8>,
}

/// Action returned by `show_flyout_interactive`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FlyoutAction {
    Primary(u32),
    Secondary(u32),
}

/// Interactive event dispatched to live flyouts (`show_flyout_live`).
///
/// Not `Copy`: `TextCommit` owns the committed text (C2 contract with WS-C widgets).
#[derive(Clone, Debug)]
pub enum LiveEvent {
    Click(u32),
    /// Periodic WM_TIMER tick for widgets with time-varying state (WS-7). The
    /// callback answers with `LiveOutcome::Update(spec)` from `live_spec()`.
    Update,
    Slider {
        id: u32,
        ratio: f32,
    },
    /// Media timeline scrubbing — `ratio` is in `0.0..=1.0`.
    Seek {
        id: u32,
        ratio: f32,
    },
    /// Text input committed with Enter (numeric inputs are clamped to their bounds).
    TextCommit {
        id: u32,
        text: String,
    },
}

/// Result of handling a `LiveEvent`: either replace the flyout spec in place or close cleanly.
pub enum LiveOutcome {
    Update(FlyoutSpec),
    Close,
}

/// Pure half of `dispatch_live`: resolves an outcome to the next spec. `None`
/// means "close the flyout". Keeping it separate lets tests verify the actual
/// decision (including calendar preservation on a periodic tick) without
/// creating a Win32 window.
fn resolve_live_outcome(
    current: &FlyoutSpec,
    outcome: LiveOutcome,
    is_tick: bool,
) -> Option<FlyoutSpec> {
    match outcome {
        LiveOutcome::Update(mut spec) => {
            if is_tick {
                preserve_calendar_navigation(current, &mut spec);
            }
            Some(spec)
        }
        LiveOutcome::Close => None,
    }
}

/// Square image size in DIPs, clamped once for layout and paint.
#[must_use]
fn image_size_dips(value: f32) -> f32 {
    value.clamp(24.0, 160.0)
}

/// Semantic color mode for `FlyoutBlock::ProgressBar`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum BarTone {
    /// Always calm accent (`WidgetBlue` / `WidgetAccent`).
    Accent,
    /// High-is-critical utilization (`<75%` accent, `75-89%` amber, `>=90%` red).
    Usage,
    /// Low-is-critical remaining level (`<=15%` red, `<=30%` amber, `>30%` green).
    Level,
}

/// Declarative content block rendered inside the shared reference parity `DockFlyout`.
#[derive(Clone, Debug)]
pub enum FlyoutBlock {
    /// Prominent hero metric (Clock 28px, Focus 44px, Weather 24px) with optional right metadata.
    Hero {
        value: String,
        caption: Option<String>,
        centered: bool,
        large: bool,
        right_meta: Option<String>,
    },
    /// Integrated horizontal header with tinted Fluent icon well, title/subtitle, and bold right value.
    HeroHeader {
        glyph: u16,
        glyph_color: Rgb,
        title: String,
        subtitle: Option<String>,
        right_value: Option<String>,
    },
    /// `ClockWidget.BuildCalendar`: interactive month/year nav + 7-col calendar grid.
    CalendarGrid {
        year: i32,
        month: u32,
        today: (i32, u32, u32),
        selected: Option<(i32, u32, u32)>,
        prev_year_id: u32,
        prev_month_id: u32,
        title_id: u32,
        next_month_id: u32,
        next_year_id: u32,
        /// Clickable day cell IDs are `day_id_base + cell_index` (`0..42`).
        day_id_base: u32,
    },
    /// `AudioWidget` Output/Input mixer section: label + `%`, device pill + mute btn, draggable slider.
    AudioSliderSection {
        label: String,
        device_name: String,
        value_text: String,
        ratio: f32,
        muted: bool,
        mute_id: u32,
        slider_id: u32,
    },
    /// `WeatherWidget` / `NetSpeedWidget` 2-column metric tiles (`SurfaceSelected`, `r=8`).
    MetricTiles(Vec<(String, String)>),
    /// `KeyboardLayoutWidget` selectable row: short-code badge pill + full display name.
    BadgeRow {
        id: u32,
        code: String,
        title: String,
        active: bool,
    },
    /// `AiUsageWidget.SectionHeader`: title + status dot + right dashboard button.
    StatusHeader {
        title: String,
        status_label: String,
        status_color: Rgb,
        action_id: Option<u32>,
        action_label: Option<String>,
    },
    /// 2-column Control Center toggle tile grid (`(id, title, state_label, active)`).
    ToggleGrid(Vec<(u32, String, String, bool)>),
    /// 2-column Control Center toggle tiles with crisp Fluent icons (`(id, glyph, title, state_label, active)`).
    IconToggleGrid(Vec<(u32, u16, String, String, bool)>),
    /// Compact stat row with leading tinted Fluent icon glyph (e.g. weather forecast or status).
    GlyphStatRow {
        glyph: u16,
        glyph_color: Rgb,
        label: String,
        detail: Option<String>,
        value: String,
    },
    /// Interactive action card with leading tinted Fluent icon glyph, title, subtitle, and badge.
    GlyphActionRow {
        id: u32,
        glyph: u16,
        glyph_color: Rgb,
        title: String,
        subtitle: Option<String>,
        badge: Option<String>,
    },
    StatRow {
        label: String,
        value: String,
    },
    ProgressBar {
        label: String,
        value_text: String,
        ratio: f32,
        tone: BarTone,
    },
    ActionRow {
        id: u32,
        title: String,
        subtitle: Option<String>,
        badge: Option<String>,
        icon: Option<FlyoutIcon>,
        destructive: bool,
    },
    AppRow {
        id: u32,
        title: String,
        icon: Option<FlyoutIcon>,
        pin_id: u32,
        pinned: bool,
        close_id: Option<u32>,
    },
    /// Horizontal button row: `(id, label, primary_selected_style)`.
    ButtonBar(Vec<(u32, String, bool)>),

    // ---- C2 contract blocks (widget flyouts consume these verbatim) ----
    /// Media / timeline scrubber. `ratio` is authoritative for rendering; the
    /// `position_secs`/`duration_secs` pair supplies the `m:ss / m:ss` caption.
    SeekSlider {
        id: u32,
        label: String,
        position_secs: u32,
        duration_secs: u32,
        ratio: f32,
    },
    /// Inline editable text field (e.g. custom countdown minutes). Emits
    /// `LiveEvent::TextCommit` on Enter; numeric fields accept only numbers.
    TextInput {
        id: u32,
        label: String,
        value: String,
        numeric: bool,
        min: f64,
        max: f64,
    },
    /// Multi-series sparkline/area chart: `(series_name, samples)` with an accent color.
    Chart {
        label: String,
        series: Vec<(String, Vec<f32>)>,
        accent: Rgb,
    },
    /// Cover art / country flag. `glyph` is a Segoe Fluent Icons code point used
    /// when `path` is absent or cannot be decoded; `size_dips` is the square size.
    Image {
        glyph: u16,
        path: Option<String>,
        size_dips: f32,
    },
    /// Compact toolbar: `(id, accessible label, Segoe Fluent Icons glyph, active)`.
    IconBar {
        buttons: Vec<(u32, String, u16, bool)>,
    },

    Separator,
    Note(String),
}

#[derive(Clone, Debug)]
pub struct FlyoutSpec {
    pub title: String,
    pub subtitle: Option<String>,
    pub width_dips: f32,
    pub blocks: Vec<FlyoutBlock>,
}

#[derive(Clone, Copy)]
struct HitTarget {
    id: u32,
    rect: RECT,
    menu_id: u32,
    is_slider: bool,
    /// C2 `SeekSlider`: draggable timeline emitting `LiveEvent::Seek`.
    is_seek: bool,
    /// C2 `TextInput`: click focuses an in-place editor.
    is_text: bool,
}

impl HitTarget {
    const fn click(id: u32, rect: RECT) -> Self {
        Self::with_menu(id, id, rect)
    }

    const fn with_menu(id: u32, menu_id: u32, rect: RECT) -> Self {
        Self {
            id,
            rect,
            menu_id,
            is_slider: false,
            is_seek: false,
            is_text: false,
        }
    }

    const fn slider(id: u32, rect: RECT, is_seek: bool) -> Self {
        Self {
            id,
            rect,
            menu_id: id,
            is_slider: true,
            is_seek,
            is_text: false,
        }
    }

    const fn text(id: u32, rect: RECT) -> Self {
        Self {
            id,
            rect,
            menu_id: id,
            is_slider: false,
            is_seek: false,
            is_text: true,
        }
    }
}

#[inline]
const fn rect_i32(left: i32, top: i32, right: i32, bottom: i32) -> RECT {
    RECT {
        left,
        top,
        right,
        bottom,
    }
}

struct FlyoutState<'a> {
    hwnd: HWND,
    tag: String,
    dpi: f32,
    is_dark: bool,
    spec: FlyoutSpec,
    anchor_x_px: i32,
    anchor_y_px: i32,
    edge: FlyoutEdge,
    use_anchor_position: bool,
    screen: RECT,
    width_px: i32,
    height_px: i32,
    x_px: i32,
    target_y_px: i32,
    slide_px: i32,
    anim_start: Instant,
    closing: bool,
    animating: bool,
    suppress_blur: bool,
    /// Dynamic blur guard: `true` while an external native menu is visible.
    /// Reset to `base_stays_open` when that menu closes.
    stays_open: bool,
    /// The caller's `stays_open` preference, restored after external-menu retention.
    base_stays_open: bool,
    hook: Option<HHOOK>,
    shadow_rings: u8,
    on_secondary: Option<&'a mut dyn FnMut(u32) -> bool>,
    on_live: Option<&'a mut dyn FnMut(LiveEvent) -> LiveOutcome>,
    /// Optional handle woken by a background worker to trigger `LiveEvent::Update`.
    live_update: Option<LiveUpdateHandle>,
    /// Whether this live flyout's widget expects `LiveEvent::Update` ticks.
    tick_enabled: bool,
    border: BorderStyle,
    /// Clear Crystal optical glass host (`topmost = false`) + DComp foreground (`topmost = true`),
    /// active strictly when `border.glass_config.enabled` is true.
    spatial_glass: Option<SpatialGlassHost>,
    dcomp_fg: Option<DCompForeground>,
    /// Per-target premultiplied `ID2D1Bitmap` cache for `ActionRow` / `AppRow` icons.
    icon_bitmaps: D2dIconBitmapCache,
    content_top_px: i32,
    viewport_h_px: i32,
    content_h_px: i32,
    scroll_y: i32,
    targets: Vec<HitTarget>,
    hovered_id: Option<u32>,
    /// `(id, track rect, is_seek)` — the third field selects `Seek` vs `Slider` events.
    dragging_slider: Option<(u32, RECT, bool)>,
    /// In-place C2 `TextInput` editor: `(id, buffer)`.
    editing: Option<(u32, String)>,
    keyboard_index: Option<usize>,
    keyboard_navigation: bool,
    tracking: bool,
    hover_anchor: Option<RECT>,
    hover_left_since: Option<Instant>,
    prev_buttons_mask: u8,
    selected: Option<FlyoutAction>,
    recorded_close: bool,
    done: AtomicBool,
}

use opentaskbar_core::wide;

/// Null-terminated, empty-safe wrapper around `user32!DrawTextW` (kept for unit test coverage).
#[cfg(test)]
unsafe fn draw_gdi_text(hdc: HDC, text: &str, rc: &mut RECT, format: u32) {
    if text.is_empty() {
        return;
    }
    let mut buf: Vec<u16> = text.encode_utf16().chain(Some(0)).collect();
    let len = (buf.len() - 1) as i32;
    unsafe {
        DrawTextW(hdc, buf.as_mut_ptr(), len, rc, format);
    }
}

#[cfg(test)]
fn rgb(r: u8, g: u8, b: u8) -> u32 {
    u32::from(r) | (u32::from(g) << 8) | (u32::from(b) << 16)
}

#[cfg(test)]
fn blend_rgb(from: u32, to: u32, t: f32) -> u32 {
    let channel = |shift: u32| {
        let a = ((from >> shift) & 0xff) as f32;
        let b = ((to >> shift) & 0xff) as f32;
        ((a + (b - a) * t.clamp(0.0, 1.0)).round() as u32) & 0xff
    };
    rgb(channel(0) as u8, channel(8) as u8, channel(16) as u8)
}

/// Shared slider track + filled bar + halo + circular thumb painter (`AudioSliderSection` & `SeekSlider`).
#[allow(clippy::too_many_arguments)]
fn draw_slider_track(
    d2d: &DcD2d,
    left: f32,
    width: f32,
    mid_y: f32,
    ratio: f32,
    hot: bool,
    dpi: f32,
    track_col: D2D1_COLOR_F,
    fill_col: D2D1_COLOR_F,
    ring_col: D2D1_COLOR_F,
    thumb_core: D2D1_COLOR_F,
    thumb_halo: D2D1_COLOR_F,
) {
    let sf = |v: f32| v * dpi;
    let tr_h = sf(5.0).max(4.0);
    let tr_r = tr_h * 0.5;
    d2d.fill_rrect(
        left,
        mid_y - tr_r,
        left + width,
        mid_y + tr_r,
        tr_r,
        track_col,
    );
    let fill_w = width * ratio.clamp(0.0, 1.0);
    if fill_w > sf(2.0) {
        d2d.fill_rrect(
            left,
            mid_y - tr_r,
            left + fill_w,
            mid_y + tr_r,
            tr_r,
            fill_col,
        );
    }
    let thumb_r = if hot { sf(6.5) } else { sf(5.5) };
    let tx = (left + fill_w).clamp(left + thumb_r, left + width - thumb_r);
    if hot {
        d2d.fill_circle(tx, mid_y, thumb_r + sf(3.5), thumb_halo);
    }
    d2d.fill_circle(tx, mid_y, thumb_r, thumb_core);
    d2d.stroke_circle(tx, mid_y, thumb_r, ring_col, sf(1.6).max(1.2));
}

/// Background-app row: `[app | pin | close]`. The two actions are 30 DIP square pills
/// separated by a 5 DIP gap; the gap belongs to no target.
fn app_row_targets(left: i32, right: i32, top: i32, dpi: f32) -> [RECT; 3] {
    let action_w = (30.0 * dpi).round() as i32;
    let gap = (5.0 * dpi).round() as i32;
    let bottom = top + (42.0 * dpi).round() as i32;
    let pin_left = right - action_w * 2 - gap;
    let close_left = right - action_w;
    [
        (left, pin_left),
        (pin_left, close_left - gap),
        (close_left, right),
    ]
    .map(|(left, right)| RECT {
        left,
        top,
        right,
        bottom,
    })
}

fn is_single_pua_glyph(s: &str) -> Option<char> {
    let mut chars = s.chars();
    let ch = chars.next()?;
    if chars.next().is_none() && (0xE000..=0xF8FF).contains(&(ch as u32)) {
        Some(ch)
    } else {
        None
    }
}

fn is_audio_input_label(label: &str) -> bool {
    label == loc::t("AudioInput") || label.eq_ignore_ascii_case("input")
}

/// Vertical advance (physical px) of one `FlyoutBlock` in both `layout_flyout` and `paint`.
/// Keeping layout and viewport culling on a single helper ensures culled rows stay pixel-exact.
#[must_use]
fn block_height_px(block: &FlyoutBlock, dpi: f32) -> i32 {
    let s = |v: f32| (v * dpi).round() as i32;
    match block {
        FlyoutBlock::Hero { caption, large, .. } => {
            let val_h = if *large { s(46.0) } else { s(34.0) };
            val_h + if caption.is_some() { s(22.0) } else { s(6.0) }
        }
        FlyoutBlock::HeroHeader { .. } => s(52.0),
        FlyoutBlock::CalendarGrid { .. } => s(30.0) + s(8.0) + s(20.0) + 6 * s(32.0) + s(6.0),
        FlyoutBlock::AudioSliderSection { .. } => s(22.0) + s(32.0) + s(8.0) + s(24.0),
        FlyoutBlock::MetricTiles(tiles) => {
            let rows = tiles.len().div_ceil(2) as i32;
            rows * s(48.0) + (rows - 1).max(0) * s(8.0) + s(6.0)
        }
        FlyoutBlock::BadgeRow { .. } => s(38.0) + s(4.0),
        FlyoutBlock::StatusHeader { .. } => s(28.0) + s(6.0),
        FlyoutBlock::ToggleGrid(tiles) => {
            let rows = tiles.len().div_ceil(2) as i32;
            rows * s(52.0) + (rows - 1).max(0) * s(8.0) + s(6.0)
        }
        FlyoutBlock::IconToggleGrid(tiles) => {
            let rows = tiles.len().div_ceil(2) as i32;
            rows * s(56.0) + (rows - 1).max(0) * s(8.0) + s(6.0)
        }
        FlyoutBlock::GlyphStatRow { .. } => s(30.0),
        FlyoutBlock::GlyphActionRow { subtitle, .. } => {
            (if subtitle.is_some() { s(44.0) } else { s(38.0) }) + s(5.0)
        }
        FlyoutBlock::StatRow { .. } => s(24.0),
        FlyoutBlock::ProgressBar {
            label, value_text, ..
        } => {
            if label.is_empty() && value_text.is_empty() {
                s(16.0)
            } else {
                s(34.0)
            }
        }
        FlyoutBlock::ActionRow { subtitle, .. } => {
            (if subtitle.is_some() { s(42.0) } else { s(34.0) }) + s(4.0)
        }
        FlyoutBlock::AppRow { .. } => s(42.0) + s(3.0),
        FlyoutBlock::ButtonBar(_) => s(34.0) + s(6.0),
        FlyoutBlock::SeekSlider { .. } => s(44.0),
        FlyoutBlock::TextInput { .. } => s(56.0),
        FlyoutBlock::Chart { .. } => s(80.0),
        FlyoutBlock::Image { size_dips, .. } => s(image_size_dips(*size_dips)) + s(8.0),
        FlyoutBlock::IconBar { buttons } => {
            if buttons.is_empty() {
                0
            } else {
                let cols = buttons.len().min(4) as i32;
                let rows = buttons.len().div_ceil(cols as usize) as i32;
                rows * s(48.0) + (rows - 1).max(0) * s(6.0) + s(6.0)
            }
        }
        FlyoutBlock::Separator => s(14.0),
        FlyoutBlock::Note(_) => s(24.0),
    }
}

/// Returns `true` when a block spanning `[block_top, block_bottom)` overlaps the
/// visible scroll clip `[clip_top, clip_bottom)`. Offscreen rows are skipped during paint.
#[must_use]
fn block_intersects_viewport(
    block_top: i32,
    block_bottom: i32,
    clip_top: i32,
    clip_bottom: i32,
) -> bool {
    block_bottom > clip_top && block_top < clip_bottom
}

/// Computes the layout height and content-space hit targets for `spec`.
fn layout_flyout(spec: &FlyoutSpec, width_px: i32, dpi: f32) -> (i32, i32, Vec<HitTarget>) {
    let s = |v: f32| (v * dpi).round() as i32;
    let pad = s(FLYOUT_PAD_DIPS);
    // Equal 16 DIP margins on both sides (the scrollbar overlays the right margin).
    let content_right_pad = pad;
    let has_header = !spec.title.is_empty();
    let header_h = if !has_header {
        0
    } else if spec.subtitle.is_some() {
        s(42.0)
    } else {
        s(24.0)
    };
    let content_top_px = if has_header {
        pad + header_h + s(8.0)
    } else {
        pad
    };

    let mut y = content_top_px;
    let mut targets = Vec::new();
    let content_w = (width_px - pad - content_right_pad).max(s(120.0));

    for block in &spec.blocks {
        match block {
            FlyoutBlock::Hero { caption, large, .. } => {
                let val_h = if *large { s(46.0) } else { s(34.0) };
                y += val_h + if caption.is_some() { s(22.0) } else { s(6.0) };
            }
            FlyoutBlock::HeroHeader { .. } => {
                y += s(52.0);
            }
            FlyoutBlock::CalendarGrid {
                prev_year_id,
                prev_month_id,
                title_id,
                next_month_id,
                next_year_id,
                day_id_base,
                ..
            } => {
                let nav_h = s(30.0);
                let btn_w = s(28.0);
                let gap = s(2.0);
                let left = pad;
                let right = pad + content_w;
                for (id, bl, br) in [
                    (*prev_year_id, left, left + btn_w),
                    (*prev_month_id, left + btn_w + gap, left + btn_w * 2 + gap),
                    (
                        *title_id,
                        left + btn_w * 2 + gap * 2,
                        right - btn_w * 2 - gap * 2,
                    ),
                    (*next_month_id, right - btn_w * 2 - gap, right - btn_w - gap),
                    (*next_year_id, right - btn_w, right),
                ] {
                    targets.push(HitTarget::click(id, rect_i32(bl, y, br, y + nav_h)));
                }
                y += nav_h + s(8.0);
                // Weekday labels row
                y += s(20.0);
                // 6 rows x 7 cols of 32px day cells
                let cell_w = content_w / 7;
                let cell_h = s(32.0);
                for cell in 0..42i32 {
                    let cx = pad + (cell % 7) * cell_w;
                    let cy = y + (cell / 7) * cell_h;
                    targets.push(HitTarget::click(
                        day_id_base + cell as u32,
                        rect_i32(
                            cx + s(2.0),
                            cy + s(2.0),
                            cx + cell_w - s(2.0),
                            cy + cell_h - s(2.0),
                        ),
                    ));
                }
                y += 6 * cell_h + s(6.0);
            }
            FlyoutBlock::AudioSliderSection {
                mute_id, slider_id, ..
            } => {
                let dev_top = y + s(22.0);
                let mute_w = s(32.0);
                targets.push(HitTarget::click(
                    *mute_id,
                    rect_i32(
                        pad + content_w - mute_w,
                        dev_top,
                        pad + content_w,
                        dev_top + mute_w,
                    ),
                ));
                let slider_top = dev_top + mute_w + s(8.0);
                targets.push(HitTarget::slider(
                    *slider_id,
                    rect_i32(pad, slider_top, pad + content_w, slider_top + s(20.0)),
                    false,
                ));
                y = slider_top + s(24.0);
            }
            FlyoutBlock::MetricTiles(tiles) => {
                let rows = tiles.len().div_ceil(2) as i32;
                let tile_h = s(48.0);
                let gap = s(8.0);
                y += rows * tile_h + (rows - 1).max(0) * gap + s(6.0);
            }
            FlyoutBlock::BadgeRow { id, .. } => {
                let h = s(38.0);
                targets.push(HitTarget::click(
                    *id,
                    rect_i32(pad, y, pad + content_w, y + h),
                ));
                y += h + s(4.0);
            }
            FlyoutBlock::StatusHeader { action_id, .. } => {
                let h = s(28.0);
                if let Some(id) = *action_id {
                    let bw = s(64.0);
                    targets.push(HitTarget::click(
                        id,
                        rect_i32(pad + content_w - bw, y, pad + content_w, y + h),
                    ));
                }
                y += h + s(6.0);
            }
            FlyoutBlock::ToggleGrid(tiles) => {
                let rows = tiles.len().div_ceil(2) as i32;
                let tile_h = s(52.0);
                let gap = s(8.0);
                let tw = ((content_w - gap) / 2).max(s(80.0));
                for (i, (id, _, _, _)) in tiles.iter().enumerate() {
                    let left = pad + (i % 2) as i32 * (tw + gap);
                    let top = y + (i / 2) as i32 * (tile_h + gap);
                    targets.push(HitTarget::click(
                        *id,
                        rect_i32(left, top, left + tw, top + tile_h),
                    ));
                }
                y += rows * tile_h + (rows - 1).max(0) * gap + s(6.0);
            }
            FlyoutBlock::IconToggleGrid(tiles) => {
                let rows = tiles.len().div_ceil(2) as i32;
                let tile_h = s(56.0);
                let gap = s(8.0);
                let tw = ((content_w - gap) / 2).max(s(80.0));
                for (i, (id, _, _, _, _)) in tiles.iter().enumerate() {
                    let left = pad + (i % 2) as i32 * (tw + gap);
                    let top = y + (i / 2) as i32 * (tile_h + gap);
                    targets.push(HitTarget::click(
                        *id,
                        rect_i32(left, top, left + tw, top + tile_h),
                    ));
                }
                y += rows * tile_h + (rows - 1).max(0) * gap + s(6.0);
            }
            FlyoutBlock::GlyphStatRow { .. } => y += s(30.0),
            FlyoutBlock::GlyphActionRow { id, subtitle, .. } => {
                let h = if subtitle.is_some() { s(44.0) } else { s(38.0) };
                targets.push(HitTarget::click(
                    *id,
                    rect_i32(pad, y, pad + content_w, y + h),
                ));
                y += h + s(5.0);
            }
            FlyoutBlock::StatRow { .. } => y += s(24.0),
            FlyoutBlock::ProgressBar {
                label, value_text, ..
            } => {
                y += if label.is_empty() && value_text.is_empty() {
                    s(16.0)
                } else {
                    s(34.0)
                };
            }
            FlyoutBlock::ActionRow { id, subtitle, .. } => {
                let h = if subtitle.is_some() { s(42.0) } else { s(34.0) };
                targets.push(HitTarget::click(
                    *id,
                    rect_i32(pad, y, pad + content_w, y + h),
                ));
                y += h + s(4.0);
            }
            FlyoutBlock::AppRow {
                id: menu_id,
                pin_id,
                close_id,
                ..
            } => {
                let rects = app_row_targets(pad, pad + content_w, y, dpi);
                for (id, rect) in [Some(*menu_id), Some(*pin_id), *close_id]
                    .into_iter()
                    .zip(rects)
                {
                    if let Some(id) = id {
                        targets.push(HitTarget::with_menu(id, *menu_id, rect));
                    }
                }
                y += s(42.0) + s(3.0);
            }
            FlyoutBlock::ButtonBar(buttons) => {
                let h = s(34.0);
                let count = buttons.len().max(1) as i32;
                let gap = s(6.0);
                let bw = ((content_w - gap * (count - 1)) / count).max(s(40.0));
                for (i, (id, _, _)) in buttons.iter().enumerate() {
                    let left = pad + (i as i32) * (bw + gap);
                    targets.push(HitTarget::click(*id, rect_i32(left, y, left + bw, y + h)));
                }
                y += h + s(6.0);
            }
            // ---- C2 blocks ----
            FlyoutBlock::SeekSlider { id, .. } => {
                let track_top = y + s(20.0);
                targets.push(HitTarget::slider(
                    *id,
                    rect_i32(pad, track_top, pad + content_w, track_top + s(18.0)),
                    true,
                ));
                y += s(44.0);
            }
            FlyoutBlock::TextInput { id, .. } => {
                let box_top = y + s(18.0);
                targets.push(HitTarget::text(
                    *id,
                    rect_i32(pad, box_top, pad + content_w, box_top + s(32.0)),
                ));
                y += s(56.0);
            }
            FlyoutBlock::Chart { .. } => y += s(80.0),
            FlyoutBlock::Image { size_dips, .. } => y += s(image_size_dips(*size_dips)) + s(8.0),
            FlyoutBlock::IconBar { buttons } => {
                if !buttons.is_empty() {
                    let cols = buttons.len().min(4) as i32;
                    let gap = s(6.0);
                    let bw = ((content_w - gap * (cols - 1)) / cols).max(s(44.0));
                    let bh = s(48.0);
                    for (i, (id, _, _, _)) in buttons.iter().enumerate() {
                        let left = pad + (i as i32 % cols) * (bw + gap);
                        let top = y + (i as i32 / cols) * (bh + gap);
                        targets.push(HitTarget::click(
                            *id,
                            rect_i32(left, top, left + bw, top + bh),
                        ));
                    }
                    let rows = buttons.len().div_ceil(cols as usize) as i32;
                    y += rows * bh + (rows - 1).max(0) * gap + s(6.0);
                }
            }
            FlyoutBlock::Separator => y += s(14.0),
            FlyoutBlock::Note(_) => y += s(24.0),
        }
    }
    // A trailing button bar sits on the outer padding, so drop its inter-block gap
    // to keep the footer padding equal to the header/side padding.
    if matches!(spec.blocks.last(), Some(FlyoutBlock::ButtonBar(_))) {
        y -= s(6.0);
    }
    (content_top_px, (y - content_top_px).max(s(24.0)), targets)
}

/// Convenience wrapper for consumers that only handle primary clicks.
///
/// Call sites without `DockSettings` get the palette hairline; use
/// [`show_flyout_with_border`] to apply the persisted `widget_border_*` settings.
pub fn show_flyout(
    owner_hwnd: usize,
    is_dark: bool,
    anchor_center_x_px: i32,
    anchor_bottom_y_px: i32,
    spec: FlyoutSpec,
) -> Option<u32> {
    show_flyout_with_border(
        owner_hwnd,
        is_dark,
        anchor_center_x_px,
        anchor_bottom_y_px,
        spec,
        BorderStyle::default_for(is_dark),
    )
}

/// Same as [`show_flyout`] with an explicit border stroke color/width.
pub fn show_flyout_with_border(
    owner_hwnd: usize,
    is_dark: bool,
    anchor_center_x_px: i32,
    anchor_bottom_y_px: i32,
    spec: FlyoutSpec,
    border: BorderStyle,
) -> Option<u32> {
    show_primary_with_options(
        owner_hwnd,
        is_dark,
        anchor_center_x_px,
        anchor_bottom_y_px,
        spec,
        border,
        FlyoutOptions::default(),
    )
}

/// [`show_flyout_with_border`] with explicit placement options, e.g.
/// `FlyoutOptions::default().with_edge(edge)` for the invoking dock edge.
pub fn show_primary_with_options(
    owner_hwnd: usize,
    is_dark: bool,
    anchor_center_x_px: i32,
    anchor_bottom_y_px: i32,
    spec: FlyoutSpec,
    border: BorderStyle,
    options: FlyoutOptions,
) -> Option<u32> {
    match show_flyout_interactive_with_options(
        &spec.title.clone(),
        owner_hwnd,
        is_dark,
        anchor_center_x_px,
        anchor_bottom_y_px,
        spec,
        None,
        border,
        options,
    ) {
        Some(FlyoutAction::Primary(id)) => Some(id),
        _ => None,
    }
}

/// Interactive flyout with both a secondary-click callback and explicit
/// lifecycle options. This is the tray entry point for WS-6: `FlyoutOptions::tray()`
/// keeps the popup alive while the app's own native context menu is on screen.
#[allow(clippy::too_many_arguments)]
pub fn show_flyout_interactive_with_options(
    tag: &str,
    owner_hwnd: usize,
    is_dark: bool,
    anchor_center_x_px: i32,
    anchor_bottom_y_px: i32,
    spec: FlyoutSpec,
    on_secondary: Option<&mut dyn FnMut(u32) -> bool>,
    border: BorderStyle,
    options: FlyoutOptions,
) -> Option<FlyoutAction> {
    show_flyout_impl(
        tag,
        owner_hwnd,
        is_dark,
        anchor_center_x_px,
        anchor_bottom_y_px,
        spec,
        on_secondary,
        None,
        border,
        options,
    )
}

/// Live-updating flyout with explicit lifecycle options (stacks / widget ticks).
#[allow(clippy::too_many_arguments)]
pub fn show_flyout_live_with_options(
    tag: &str,
    owner_hwnd: usize,
    is_dark: bool,
    anchor_center_x_px: i32,
    anchor_bottom_y_px: i32,
    spec: FlyoutSpec,
    border: BorderStyle,
    options: FlyoutOptions,
    on_live: impl FnMut(LiveEvent) -> LiveOutcome,
) {
    let mut on_live = on_live;
    let _ = show_flyout_impl(
        tag,
        owner_hwnd,
        is_dark,
        anchor_center_x_px,
        anchor_bottom_y_px,
        spec,
        None,
        Some(&mut on_live),
        border,
        options,
    );
}

/// Screen coordinates are carried through the private message as `(wparam,
/// lparam)`. Cast via `u32` so negative multi-monitor coordinates survive the
/// round trip regardless of pointer width.
#[must_use]
fn pack_point(x: i32, y: i32) -> (WPARAM, LPARAM) {
    (x as u32 as WPARAM, y as u32 as LPARAM)
}

#[must_use]
fn unpack_point(wparam: WPARAM, lparam: LPARAM) -> (i32, i32) {
    (wparam as u32 as i32, lparam as u32 as i32)
}

/// The hook's entire payload path, factored so it can be tested without
/// installing a real low-level hook. `target == 0` is a no-op.
fn post_menu_candidate(
    target: usize,
    point: POINT,
    mut post: impl FnMut(HWND, u32, WPARAM, LPARAM),
) {
    if target == 0 {
        return;
    }
    let (x, y) = pack_point(point.x, point.y);
    post(target as HWND, WM_APP_MENU_CANDIDATE, x, y);
}

/// Kept pure so the `#32768` discrimination can be tested off the UI thread.
#[must_use]
fn is_native_menu_class(class: &[u16]) -> bool {
    String::from_utf16_lossy(class).starts_with("#32768")
}

/// How long a `WindowFromPoint` + `GetClassNameW` result is reused. One animation
/// frame (~16 ms) is enough: a click posts several candidates in that window,
/// but a genuinely new menu opened later is still detected.
const MENU_CLASS_CACHE_TTL: Duration = Duration::from_millis(16);

#[derive(Clone, Copy)]
struct MenuClassProbe {
    point: POINT,
    is_menu: bool,
    at: Option<Instant>,
}

impl MenuClassProbe {
    const fn empty() -> Self {
        Self {
            point: POINT { x: 0, y: 0 },
            is_menu: false,
            at: None,
        }
    }

    fn is_fresh(self, point: POINT, now: Instant) -> bool {
        self.at
            .is_some_and(|at| now.duration_since(at) < MENU_CLASS_CACHE_TTL)
            && self.point.x == point.x
            && self.point.y == point.y
    }
}

/// Thread-safe handle allowing a background worker to wake an open live flyout
/// (`show_flyout_live_with_options`) and dispatch `LiveEvent::Update` on its UI
/// thread immediately without polling.
#[derive(Clone)]
pub struct LiveUpdateHandle {
    inner: Arc<LiveUpdateSignal>,
}

struct LiveUpdateSignal {
    hwnd: AtomicUsize,
    pending: AtomicBool,
    closed: AtomicBool,
}

impl Default for LiveUpdateHandle {
    fn default() -> Self {
        Self::new()
    }
}

impl LiveUpdateHandle {
    #[must_use]
    pub fn new() -> Self {
        Self {
            inner: Arc::new(LiveUpdateSignal {
                hwnd: AtomicUsize::new(0),
                pending: AtomicBool::new(false),
                closed: AtomicBool::new(false),
            }),
        }
    }

    /// Signals that new live data is ready. If the flyout window is already attached,
    /// posts `WM_APP_LIVE_UPDATE`; otherwise leaves `pending` armed so `attach` fires it
    /// as soon as the window is created.
    pub fn notify(&self) {
        if self.inner.closed.load(Ordering::SeqCst) {
            return;
        }
        self.inner.pending.store(true, Ordering::SeqCst);
        let hwnd = self.inner.hwnd.load(Ordering::SeqCst);
        if hwnd != 0 && !self.inner.closed.load(Ordering::SeqCst) {
            unsafe {
                let _ = PostMessageW(hwnd as HWND, WM_APP_LIVE_UPDATE, 0, 0);
            }
        }
    }

    #[must_use]
    pub fn is_closed(&self) -> bool {
        self.inner.closed.load(Ordering::Acquire)
    }

    fn attach(&self, hwnd: usize) {
        if self.inner.closed.load(Ordering::SeqCst) {
            return;
        }
        self.inner.hwnd.store(hwnd, Ordering::SeqCst);
        if self.inner.pending.load(Ordering::SeqCst) && hwnd != 0 {
            unsafe {
                let _ = PostMessageW(hwnd as HWND, WM_APP_LIVE_UPDATE, 0, 0);
            }
        }
    }

    fn clear_pending(&self) {
        self.inner.pending.store(false, Ordering::SeqCst);
    }

    pub fn detach(&self) {
        self.inner.closed.store(true, Ordering::SeqCst);
        self.inner.hwnd.store(0, Ordering::SeqCst);
        self.inner.pending.store(false, Ordering::SeqCst);
    }
}

thread_local! {
    /// UI-thread-only cache: the low-level hook never touches this (it only
    /// posts coordinates), so a hung foreign window cannot stall system input.
    static MENU_CLASS_PROBE: RefCell<MenuClassProbe> = const { RefCell::new(MenuClassProbe::empty()) };
    /// Optional physical-pixel widget rect that opened the next flyout via hover.
    /// Consumed once by `show_flyout_impl` to arm hover-leave auto-dismiss.
    static NEXT_HOVER_ANCHOR: RefCell<Option<RECT>> = const { RefCell::new(None) };
    /// Optional async wakeup handle consumed once by the next live `show_flyout_impl`.
    static NEXT_LIVE_UPDATE_HANDLE: RefCell<Option<LiveUpdateHandle>> = const { RefCell::new(None) };
}

/// Arms (or clears) hover-leave auto-dismiss for the next flyout opened on this UI thread.
pub fn set_next_hover_anchor(rect: Option<(i32, i32, i32, i32)>) {
    NEXT_HOVER_ANCHOR.with(|slot| {
        *slot.borrow_mut() = rect.map(|(left, top, right, bottom)| RECT {
            left,
            top,
            right,
            bottom,
        });
    });
}

/// Binds an async wakeup handle to the next live flyout opened on this UI thread.
pub fn set_next_live_update_handle(handle: Option<LiveUpdateHandle>) {
    NEXT_LIVE_UPDATE_HANDLE.with(|slot| {
        *slot.borrow_mut() = handle;
    });
}

/// Returns whether `pt` lies inside the flyout, its originating widget rect, or the corridor
/// connecting them so moving the cursor from a hovered widget up into its flyout never closes it.
fn point_in_hover_safe_zone(pt: POINT, flyout: RECT, anchor: RECT, pad: i32) -> bool {
    let in_rect = |r: RECT, p: i32| {
        pt.x >= r.left - p && pt.x <= r.right + p && pt.y >= r.top - p && pt.y <= r.bottom + p
    };
    if in_rect(flyout, pad) || in_rect(anchor, pad) {
        return true;
    }
    let bridge = RECT {
        left: flyout.left.min(anchor.left),
        top: flyout.top.min(anchor.top),
        right: flyout.right.max(anchor.right),
        bottom: flyout.bottom.max(anchor.bottom),
    };
    pt.x >= anchor.left.min(flyout.left) - pad
        && pt.x <= anchor.right.max(flyout.right) + pad
        && pt.y >= bridge.top - pad
        && pt.y <= bridge.bottom + pad
}

fn mouse_buttons_mask() -> u8 {
    unsafe {
        let l = u8::from((GetAsyncKeyState(VK_LBUTTON as i32) as u16 & 0x8000) != 0);
        let r = u8::from((GetAsyncKeyState(VK_RBUTTON as i32) as u16 & 0x8000) != 0);
        let m = u8::from((GetAsyncKeyState(VK_MBUTTON as i32) as u16 & 0x8000) != 0);
        l | (r << 1) | (m << 2)
    }
}

/// Returns whether a native `#32768` menu sits under `point`, reusing a probe
/// from the same animation frame. `unsafe` because Win32 window lookup is involved.
fn external_menu_at(point: POINT) -> bool {
    let now = Instant::now();
    if MENU_CLASS_PROBE.with(|cache| cache.borrow().is_fresh(point, now)) {
        return MENU_CLASS_PROBE.with(|cache| cache.borrow().is_menu);
    }
    let under_cursor = unsafe { WindowFromPoint(point) };
    let mut class = [0u16; 64];
    let len = unsafe { GetClassNameW(under_cursor, class.as_mut_ptr(), class.len() as i32) };
    let is_menu = len > 0 && is_native_menu_class(&class[..len as usize]);
    MENU_CLASS_PROBE.with(|cache| {
        *cache.borrow_mut() = MenuClassProbe {
            point,
            is_menu,
            at: Some(now),
        };
    });
    is_menu
}

/// WH_MOUSE_LL observer for clicks (`DockFlyout` + `RetainForExternalContextMenu`).
///
/// Deliberately minimal: it only reads the system-provided point and posts a
/// private message to our own flyout. All `WindowFromPoint`/`GetClassNameW`
/// work happens later in `flyout_wnd_proc` on the UI thread, so a hung foreign
/// window cannot stall the low-level hook and lock the whole system's input.
unsafe extern "system" fn external_menu_hook(code: i32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    if code == HC_ACTION as i32
        && matches!(
            wparam as u32,
            WM_LBUTTONDOWN | WM_LBUTTONUP | WM_RBUTTONDOWN | WM_RBUTTONUP
        )
    {
        // SAFETY: for HC_ACTION mouse messages the system supplies a valid
        // MSLLHOOKSTRUCT pointer for the lifetime of this callback.
        let info = unsafe { &*(lparam as *const MSLLHOOKSTRUCT) };
        let target = HOOK_TARGET.load(Ordering::Acquire);
        // SAFETY: a private message posted to the flyout window registered in
        // HOOK_TARGET; no synchronous Win32 calls against the window under the
        // cursor happen in the hook.
        post_menu_candidate(target, info.pt, |hwnd, msg, x, y| unsafe {
            PostMessageW(hwnd, msg, x, y);
        });
    }
    unsafe { CallNextHookEx(std::ptr::null_mut(), code, wparam, lparam) }
}

#[allow(clippy::too_many_arguments)]
fn show_flyout_impl<'a>(
    tag: &str,
    owner_hwnd: usize,
    is_dark: bool,
    anchor_center_x_px: i32,
    anchor_bottom_y_px: i32,
    spec: FlyoutSpec,
    on_secondary: Option<&'a mut dyn FnMut(u32) -> bool>,
    on_live: Option<&'a mut dyn FnMut(LiveEvent) -> LiveOutcome>,
    border: BorderStyle,
    options: FlyoutOptions,
) -> Option<FlyoutAction> {
    let live_update = NEXT_LIVE_UPDATE_HANDLE.with(|slot| slot.borrow_mut().take());
    if should_suppress_reopen(tag) {
        if let Some(handle) = live_update {
            handle.detach();
        }
        return None;
    }

    // `DockFlyout` parity: remember the window that owned focus before the
    // popup took it, so closing the flyout hands focus back to the dock widget.
    let previous_foreground = unsafe { GetForegroundWindow() };

    // No `CS_DROPSHADOW`: that legacy class shadow is a rectangle, so it stuck
    // out as a square frame behind the 8 DIP rounded corners. `DWMWCP_ROUND`
    // (set below on every path) gives the native Win11 shadow that follows the
    // rounded outline, exactly like the dock context menu.
    let class_name = wide(FLYOUT_CLASS_NAME);
    let hinstance = unsafe { GetModuleHandleW(std::ptr::null()) };
    let wc = WNDCLASSEXW {
        cbSize: size_of::<WNDCLASSEXW>() as u32,
        style: FLYOUT_CLASS_STYLE,
        lpfnWndProc: Some(flyout_wnd_proc),
        hInstance: hinstance,
        hCursor: unsafe { LoadCursorW(std::ptr::null_mut(), IDC_ARROW) },
        lpszClassName: class_name.as_ptr(),
        ..unsafe { std::mem::zeroed() }
    };
    unsafe {
        RegisterClassExW(&wc);
    }

    // Size and clamp against the anchor monitor's *work area* (never behind the
    // taskbar) at that monitor's DPI, so multi-monitor flyouts are not scaled for
    // the dock's display when they open on another one.
    let (screen, monitor_dpi) = monitor_work_area_at(anchor_center_x_px, anchor_bottom_y_px);
    let dpi = monitor_dpi
        .filter(|d| d.is_finite() && *d > 0.0)
        .unwrap_or_else(|| win32::dpi_scale(owner_hwnd))
        .max(0.5);
    let s = |v: f32| (v * dpi).round() as i32;
    let margin = s(10.0);
    let work_w = (screen.right - screen.left - 2 * margin).max(1);
    let width_px = s(spec.width_dips.clamp(240.0, 380.0)).min(work_w);
    let pad = s(FLYOUT_PAD_DIPS);
    let (content_top_px, content_h_px, targets) = layout_flyout(&spec, width_px, dpi);
    let available_h = flyout_available_height(
        options.edge,
        options.use_anchor_position,
        &screen,
        anchor_bottom_y_px,
        margin,
        s(160.0),
    );
    let height_px = (content_top_px + content_h_px + pad)
        .min(s(MAX_FLYOUT_H_DIPS))
        .min(available_h);
    // The viewport may be shorter than its content: the flyout then scrolls instead
    // of pushing the bottom controls off-screen.
    let viewport_h_px = (height_px - content_top_px - pad).max(1);

    let is_tray = tag == "tray";
    let (x, top_y) = flyout_origin(
        options.edge,
        &screen,
        anchor_center_x_px,
        anchor_bottom_y_px,
        width_px,
        height_px,
        margin,
    );
    // A journaled position wins over the computed placement, unless the caller passed
    // an explicit dock/island anchor (a stale saved spot must not move it away, and
    // anchored flyouts skip synchronous journal disk read/write entirely).
    let (x, top_y) = if options.use_anchor_position {
        (x, top_y)
    } else {
        let left = screen.left + margin;
        let top = screen.top + margin;
        let max_x = (screen.right - width_px - margin).max(left);
        let max_y = (screen.bottom - height_px - margin).max(top);
        let pos = opentaskbar_shell::flyout_journal::read_tag(tag)
            .map_or((x, top_y), |(sx, sy)| {
                (sx.clamp(left, max_x), sy.clamp(top, max_y))
            });
        opentaskbar_shell::flyout_journal::write_tag(tag, pos.0, pos.1);
        pos
    };

    let animate = win32::client_area_animations_enabled()
        && FLYOUT_ANIMATIONS_ENABLED.load(Ordering::Relaxed);
    let slide_px = if animate { s(SLIDE_OFFSET_DIPS) } else { 0 };
    let use_glass_backdrop = border.glass_config.enabled;
    let ex_style = WS_EX_TOOLWINDOW
        | WS_EX_TOPMOST
        | if use_glass_backdrop {
            WS_EX_NOREDIRECTIONBITMAP
        } else {
            WS_EX_LAYERED
        };
    let hwnd = unsafe {
        CreateWindowExW(
            ex_style,
            class_name.as_ptr(),
            wide(if is_tray { &spec.title } else { "" }).as_ptr(),
            WS_POPUP,
            x,
            top_y + slide_px,
            width_px,
            height_px,
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            hinstance,
            std::ptr::null(),
        )
    };
    if hwnd.is_null() {
        if let Some(handle) = live_update {
            handle.detach();
        }
        return None;
    }

    let (spatial_glass, dcomp_fg) = if use_glass_backdrop {
        configure_host_backdrop_window(hwnd as usize, is_dark, true);
        // Apply Windows 11 native rounded corners (with DWM border disabled so only
        // SpatialGlassHost's crisp rim renders) to get clean OS drop shadow elevation
        // and match the 8 DIP glass capsule cleanly with zero exposed corner gap.
        set_dwm_u32(hwnd, DWMWA_WINDOW_CORNER_PREFERENCE, DWMWCP_ROUND);
        set_dwm_u32(hwnd, DWMWA_BORDER_COLOR, DWMWA_COLOR_NONE);
        // Active theme packs tint the glass body and rim; stock themes stay neutral.
        let (tint, rim) = design::theme::theme_surface_tint("MenuMaterial", "DockBorderMaterial");
        let glass_config = border.glass_config.with_theme_tint(tint, rim);
        let mut host = SpatialGlassHost::attach(hwnd as usize, false, glass_config);
        if let Some(ref mut g) = host {
            g.sync_rect(
                0.0,
                0.0,
                width_px as f32,
                height_px as f32,
                (8.0 * dpi).round(),
                dpi,
                0.0,
                0.0,
            );
        }
        let fg = DCompForeground::new(hwnd as usize);
        if host.is_some() && fg.is_some() {
            (host, fg)
        } else {
            set_dwm_u32(hwnd, DWMWA_WINDOW_CORNER_PREFERENCE, DWMWCP_ROUND);
            set_dwm_u32(
                hwnd,
                DWMWA_SYSTEMBACKDROP_TYPE,
                crate::design::DWMSBT_TRANSIENTWINDOW,
            );
            (None, None)
        }
    } else {
        unsafe {
            let _ = SetLayeredWindowAttributes(hwnd, 0, if animate { 0 } else { 255 }, LWA_ALPHA);
        }
        set_dwm_u32(hwnd, DWMWA_USE_IMMERSIVE_DARK_MODE, u32::from(is_dark));
        set_dwm_u32(hwnd, DWMWA_WINDOW_CORNER_PREFERENCE, DWMWCP_ROUND);
        (None, None)
    };
    if dcomp_fg.is_none() {
        // Without the DComp glass host `paint` draws one crisp rounded border that
        // follows the `DWMWCP_ROUND` corners; suppress DWM's own frame so the
        // popup never shows a doubled outline.
        set_dwm_u32(hwnd, DWMWA_BORDER_COLOR, DWMWA_COLOR_NONE);
    }

    let hover_anchor = NEXT_HOVER_ANCHOR.with(|slot| slot.borrow_mut().take());
    let has_live_callback = on_live.is_some();
    let tick_enabled = has_live_callback && live_tick_tag(tag);
    let mut state = Box::new(FlyoutState {
        hwnd,
        tag: tag.to_owned(),
        dpi,
        is_dark,
        spec,
        anchor_x_px: anchor_center_x_px,
        anchor_y_px: anchor_bottom_y_px,
        edge: options.edge,
        use_anchor_position: options.use_anchor_position,
        screen,
        width_px,
        height_px,
        x_px: x,
        target_y_px: top_y,
        slide_px,
        anim_start: Instant::now(),
        closing: false,
        animating: animate,
        suppress_blur: false,
        stays_open: options.stays_open,
        base_stays_open: options.stays_open,
        hook: None,
        shadow_rings: options.shadow_rings.min(8),
        on_secondary,
        on_live,
        live_update,
        tick_enabled,
        border,
        spatial_glass,
        dcomp_fg,
        icon_bitmaps: D2dIconBitmapCache::default(),
        content_top_px,
        viewport_h_px,
        content_h_px,
        scroll_y: 0,
        targets,
        hovered_id: None,
        dragging_slider: None,
        editing: None,
        keyboard_index: None,
        keyboard_navigation: is_tray,
        tracking: false,
        hover_anchor,
        hover_left_since: None,
        prev_buttons_mask: mouse_buttons_mask(),
        selected: None,
        recorded_close: false,
        done: AtomicBool::new(false),
    });
    let ptr = (&mut *state) as *mut FlyoutState<'_> as isize;
    unsafe {
        set_window_long_ptr(hwnd, GWLP_USERDATA, ptr);
        SetWindowPos(
            hwnd,
            HWND_TOPMOST,
            x,
            top_y + slide_px,
            width_px,
            height_px,
            SWP_SHOWWINDOW,
        );
        ShowWindow(hwnd, SW_SHOW);
        SetForegroundWindow(hwnd);
        if animate {
            SetTimer(hwnd, TIMER_ANIM, flyout_anim_interval_ms(), None);
        }
        SetTimer(hwnd, TIMER_DISMISS, DISMISS_POLL_MS, None);
        if tick_enabled {
            SetTimer(hwnd, TIMER_LIVE, LIVE_TICK_MS, None);
        }
    }
    if let Some(ref handle) = state.live_update {
        handle.attach(hwnd as usize);
    }

    if options.retain_for_external_context_menu {
        HOOK_TARGET.store(hwnd as usize, Ordering::Release);
        let hook = unsafe {
            SetWindowsHookExW(
                WH_MOUSE_LL,
                Some(external_menu_hook),
                std::ptr::null_mut(),
                0,
            )
        };
        state.hook = if hook.is_null() { None } else { Some(hook) };
    }

    let mut msg: MSG = unsafe { std::mem::zeroed() };
    while !state.done.load(Ordering::Acquire) {
        let ret = unsafe { GetMessageW(&mut msg, std::ptr::null_mut(), 0, 0) };
        if ret <= 0 {
            break;
        }
        unsafe {
            TranslateMessage(&msg);
            DispatchMessageW(&msg);
        }
    }

    let selected = state.selected;
    if let Some(hook) = state.hook.take() {
        unsafe {
            UnhookWindowsHookEx(hook);
        }
    }
    HOOK_TARGET.store(0, Ordering::Release);
    EXTERNAL_MENU_ACTIVE.store(false, Ordering::Release);
    if let Some(ref handle) = state.live_update {
        handle.detach();
    }
    state.icon_bitmaps.clear();
    state.dcomp_fg = None;
    state.spatial_glass = None;
    unsafe {
        KillTimer(hwnd, TIMER_ANIM);
        KillTimer(hwnd, TIMER_DISMISS);
        KillTimer(hwnd, TIMER_LIVE);
        set_window_long_ptr(hwnd, GWLP_USERDATA, 0);
        DestroyWindow(hwnd);
    }
    // Focus return: best effort, only when the original window still exists.
    // Foreground-lock may refuse the request; leaving focus alone is still
    // preferable to stranding it on a destroyed popup HWND.
    if !previous_foreground.is_null()
        && previous_foreground != hwnd
        && unsafe { IsWindow(previous_foreground) } != 0
    {
        unsafe {
            SetForegroundWindow(previous_foreground);
        }
    }
    if selected.is_none() && !state.recorded_close {
        record_closed(tag);
    }
    selected
}

/// `m:ss` caption for `FlyoutBlock::SeekSlider`.
fn format_clock(secs: u32) -> String {
    format!("{}:{:02}", secs / 60, secs % 60)
}

impl FlyoutState<'_> {
    fn apply_live_spec(&mut self, next: FlyoutSpec) {
        let mut client = RECT::default();
        unsafe { GetClientRect(self.hwnd, &mut client) };
        let width_px = client.right.max(self.width_px).max(1);
        let (ct, ch, targets) = layout_flyout(&next, width_px, self.dpi);
        let s = |v: f32| (v * self.dpi).round() as i32;
        let pad = s(FLYOUT_PAD_DIPS);
        let margin = s(10.0);
        let available_h = flyout_available_height(
            self.edge,
            self.use_anchor_position,
            &self.screen,
            self.anchor_y_px,
            margin,
            s(160.0),
        );
        let new_height_px = (ct + ch + pad).min(s(MAX_FLYOUT_H_DIPS)).min(available_h);
        self.spec = next;
        self.content_top_px = ct;
        self.content_h_px = ch;
        self.viewport_h_px = (new_height_px - ct - pad).max(1);
        self.targets = targets;
        if new_height_px != self.height_px {
            self.height_px = new_height_px;
            if self.use_anchor_position {
                let (nx, ny) = flyout_origin(
                    self.edge,
                    &self.screen,
                    self.anchor_x_px,
                    self.anchor_y_px,
                    width_px,
                    new_height_px,
                    margin,
                );
                self.x_px = nx;
                self.target_y_px = ny;
            } else {
                let top = self.screen.top + margin;
                let max_y = (self.screen.bottom - new_height_px - margin).max(top);
                self.target_y_px = self.target_y_px.clamp(top, max_y);
            }
            let cur_y = if self.animating && !self.closing && self.slide_px != 0 {
                let elapsed = self.anim_start.elapsed().as_secs_f32() * 1000.0;
                let t = (elapsed / OPEN_ANIM_MS).clamp(0.0, 1.0);
                let eased = 1.0 - (1.0 - t).powi(3);
                self.target_y_px + ((1.0 - eased) * self.slide_px as f32).round() as i32
            } else {
                self.target_y_px
            };
            unsafe {
                SetWindowPos(
                    self.hwnd,
                    std::ptr::null_mut(),
                    self.x_px,
                    cur_y,
                    width_px,
                    new_height_px,
                    SWP_NOZORDER | SWP_NOACTIVATE,
                );
            }
            if let Some(ref mut glass) = self.spatial_glass {
                glass.sync_rect(
                    0.0,
                    0.0,
                    width_px as f32,
                    new_height_px as f32,
                    (8.0 * self.dpi).round(),
                    self.dpi,
                    0.0,
                    0.0,
                );
            }
        }
        self.scroll_y = self.scroll_y.clamp(0, self.max_scroll());
        unsafe {
            InvalidateRect(self.hwnd, std::ptr::null(), 0);
        }
    }

    fn dispatch_live(&mut self, event: LiveEvent) -> bool {
        let is_tick = matches!(event, LiveEvent::Update);
        let outcome = if let Some(cb) = self.on_live.as_deref_mut() {
            cb(event)
        } else {
            return false;
        };
        // A tick rebuilds `live_spec("clock")` with the current month; resolving
        // the outcome preserves the month the user navigated to.
        match resolve_live_outcome(&self.spec, outcome, is_tick) {
            Some(spec) => self.apply_live_spec(spec),
            None => self.begin_close(),
        }
        true
    }

    /// WM_TIMER live tick. Uses the exact same `dispatch_live` path as clicks,
    /// so a widget's `live_spec()` replaces the content in place (or closes the
    /// flyout when its callback decides the timer has expired).
    fn dispatch_live_tick(&mut self) {
        if self.tick_enabled && live_tick_allowed(self.closing, self.on_live.is_some()) {
            self.dispatch_live(LiveEvent::Update);
        }
    }

    /// Focus a C2 `TextInput` in place; the buffer starts from the block's current value.
    fn begin_editing(&mut self, id: u32) {
        let initial = self.spec.blocks.iter().find_map(|block| match block {
            FlyoutBlock::TextInput { id: bid, value, .. } if *bid == id => Some(value.clone()),
            _ => None,
        });
        if let Some(value) = initial {
            self.editing = Some((id, value));
            unsafe { InvalidateRect(self.hwnd, std::ptr::null(), 0) };
        }
    }

    /// Cancel editing without emitting an event.
    fn cancel_editing(&mut self) {
        if self.editing.take().is_some() {
            unsafe { InvalidateRect(self.hwnd, std::ptr::null(), 0) };
        }
    }

    /// Enter a printable character into the active C2 `TextInput` editor.
    fn editing_char(&mut self, ch: char) {
        let Some(id) = self.editing.as_ref().map(|(id, _)| *id) else {
            return;
        };
        let numeric = self.spec.blocks.iter().any(|block| {
            matches!(
                block,
                FlyoutBlock::TextInput { id: bid, numeric: true, .. } if *bid == id
            )
        });
        let changed = self
            .editing
            .as_mut()
            .is_some_and(|(_, buf)| push_char(buf, ch, numeric, MAX_TEXT_CHARS));
        if changed {
            unsafe { InvalidateRect(self.hwnd, std::ptr::null(), 0) };
        }
    }

    /// Backspace in the active C2 `TextInput` editor.
    fn editing_backspace(&mut self) {
        if let Some((_, buf)) = self.editing.as_mut()
            && buf.pop().is_some()
        {
            unsafe { InvalidateRect(self.hwnd, std::ptr::null(), 0) };
        }
    }

    /// Delete the whole buffer, matching the GPUI `apply_keystroke("delete")` path.
    fn editing_delete(&mut self) {
        if let Some((_, buf)) = self.editing.as_mut()
            && !buf.is_empty()
        {
            buf.clear();
            unsafe { InvalidateRect(self.hwnd, std::ptr::null(), 0) };
        }
    }

    /// Commit the active C2 `TextInput` as `LiveEvent::TextCommit` (clamped for numeric fields).
    fn commit_editing(&mut self) {
        let Some((id, mut buf)) = self.editing.take() else {
            return;
        };
        let bounds = self.spec.blocks.iter().find_map(|block| match block {
            FlyoutBlock::TextInput {
                id: bid,
                numeric,
                min,
                max,
                ..
            } if *bid == id => Some((*numeric, *min, *max)),
            _ => None,
        });
        if let Some((true, min, max)) = bounds {
            normalize_numeric(&mut buf, min, max);
        }
        unsafe { InvalidateRect(self.hwnd, std::ptr::null(), 0) };
        let handled = self.dispatch_live(LiveEvent::TextCommit { id, text: buf });
        if !handled {
            self.selected = Some(FlyoutAction::Primary(id));
            self.begin_close();
        }
    }

    fn begin_close(&mut self) {
        if self.closing {
            return;
        }
        self.closing = true;
        if let Some(ref handle) = self.live_update {
            handle.detach();
        }
        // Arm reopen suppression immediately when dismissal starts so a fast
        // click releasing during `CLOSE_ANIM_MS` cannot reopen the flyout.
        if self.selected.is_none() && !self.recorded_close {
            record_closed(&self.tag);
            self.recorded_close = true;
        }
        if self.slide_px == 0 {
            self.done.store(true, Ordering::Release);
            return;
        }
        self.animating = true;
        self.anim_start = Instant::now();
        unsafe {
            SetTimer(self.hwnd, TIMER_ANIM, flyout_anim_interval_ms(), None);
        }
    }

    fn step_animation(&mut self) {
        if !self.animating {
            return;
        }
        let elapsed = self.anim_start.elapsed().as_secs_f32() * 1000.0;
        if !self.closing {
            let t = (elapsed / OPEN_ANIM_MS).clamp(0.0, 1.0);
            let eased = 1.0 - (1.0 - t).powi(3);
            let alpha = (eased * 255.0).round() as u8;
            let cur_y = self.target_y_px + ((1.0 - eased) * self.slide_px as f32).round() as i32;
            unsafe {
                if self.dcomp_fg.is_none() {
                    let _ = SetLayeredWindowAttributes(self.hwnd, 0, alpha, LWA_ALPHA);
                }
                SetWindowPos(
                    self.hwnd,
                    std::ptr::null_mut(),
                    self.x_px,
                    cur_y,
                    0,
                    0,
                    SWP_NOSIZE | SWP_NOZORDER | SWP_NOACTIVATE,
                );
            }
            if t >= 1.0 {
                self.animating = false;
                unsafe {
                    KillTimer(self.hwnd, TIMER_ANIM);
                }
            }
        } else {
            let t = (elapsed / CLOSE_ANIM_MS).clamp(0.0, 1.0);
            let eased = t * t;
            let alpha = ((1.0 - eased) * 255.0).round() as u8;
            let cur_y = self.target_y_px + (eased * (self.slide_px as f32 * 0.75)).round() as i32;
            unsafe {
                if self.dcomp_fg.is_none() {
                    let _ = SetLayeredWindowAttributes(self.hwnd, 0, alpha, LWA_ALPHA);
                }
                SetWindowPos(
                    self.hwnd,
                    std::ptr::null_mut(),
                    self.x_px,
                    cur_y,
                    0,
                    0,
                    SWP_NOSIZE | SWP_NOZORDER | SWP_NOACTIVATE,
                );
            }
            if t >= 1.0 {
                self.animating = false;
                unsafe {
                    KillTimer(self.hwnd, TIMER_ANIM);
                }
                self.done.store(true, Ordering::Release);
            }
        }
    }

    fn max_scroll(&self) -> i32 {
        (self.content_h_px - self.viewport_h_px).max(0)
    }

    fn hit_at(&self, x: i32, y: i32) -> Option<HitTarget> {
        if y < self.content_top_px || y >= self.content_top_px + self.viewport_h_px {
            return None;
        }
        let cy = y + self.scroll_y;
        self.targets
            .iter()
            .find(|t| pt_in_rect(&t.rect, x, cy))
            .copied()
    }

    fn focus_next_target(&mut self, backwards: bool) {
        let count = self.targets.len();
        if count == 0 {
            return;
        }
        let index = match (self.keyboard_index, backwards) {
            (Some(index), false) => (index + 1) % count,
            (Some(index), true) => (index + count - 1) % count,
            (None, false) => 0,
            (None, true) => count - 1,
        };
        self.keyboard_index = Some(index);
        let rect = self.targets[index].rect;
        let start = rect.bottom - self.content_top_px - self.viewport_h_px;
        let end = rect.top - self.content_top_px;
        self.scroll_y = self
            .scroll_y
            .max(start)
            .min(end)
            .clamp(0, self.max_scroll());
        unsafe { InvalidateRect(self.hwnd, std::ptr::null(), 0) };
    }

    fn paint(&mut self) {
        let dpi = self.dpi;
        let sf = |v: f32| v * dpi;
        let s = |v: f32| (v * dpi).round() as i32;
        let pad = s(FLYOUT_PAD_DIPS);
        let pad_f = pad as f32;
        let right_pad = pad;
        let card_r = sf(10.0);
        let pill_r = sf(8.0);
        let is_dark = self.is_dark;
        let border = self.border;
        let use_spatial_glass = self.dcomp_fg.is_some() && border.glass_config.enabled;
        let focused_id = self.keyboard_index.map(|index| self.targets[index].id);
        let hovered_id = self.hovered_id;
        let dragging_slider = self.dragging_slider;
        let editing_state = &self.editing;
        let keyboard_index = self.keyboard_index;
        let targets = &self.targets;
        let spec = &self.spec;
        let content_top_px = self.content_top_px;
        let viewport_h_px = self.viewport_h_px;
        let content_h_px = self.content_h_px;
        let scroll_y = self.scroll_y;
        let max_s = self.max_scroll();
        let spatial_glass = &mut self.spatial_glass;
        let icon_bitmaps = &mut self.icon_bitmaps;
        let _ = self.shadow_rings;

        let pal = design::active_palette_with_border(is_dark, Some(border.rgb));
        // Theme packs / editor tokens tint glass cards and hovers with the live palette;
        // the stock theme keeps the neutral white veils below.
        let themed = design::theme::has_active_theme_customization();
        let border_w = (border.width_dips * dpi).clamp(1.0, 2.0);
        let surface_c = if use_spatial_glass {
            if themed {
                d2d_rgba(pal.surface, if is_dark { 0.24 } else { 0.12 })
            } else {
                d2d_rgba((30, 33, 40), if is_dark { 0.18 } else { 0.0 })
            }
        } else if border.glass_mode {
            let tint = if themed {
                pal.surface
            } else if is_dark {
                (0x1c_u8, 0x1f_u8, 0x26_u8)
            } else {
                (0xf6_u8, 0xf8_u8, 0xfc_u8)
            };
            d2d_rgba(tint, border.glass_tint_alpha.clamp(0.18, 0.48))
        } else {
            d2d_rgba(pal.surface, 1.0)
        };
        let card_bg = if border.glass_mode {
            if themed {
                d2d_rgba(pal.elevated, if is_dark { 0.42 } else { 0.62 })
            } else if is_dark {
                d2d_rgba((0xff, 0xff, 0xff), 0.08)
            } else {
                d2d_rgba((0xff, 0xff, 0xff), 0.56)
            }
        } else {
            d2d_rgba(pal.elevated, 1.0)
        };
        let hover_bg = if border.glass_mode {
            if themed {
                d2d_rgba(pal.hover, if is_dark { 0.62 } else { 0.80 })
            } else if is_dark {
                d2d_rgba((0xff, 0xff, 0xff), 0.14)
            } else {
                d2d_rgba((0xff, 0xff, 0xff), 0.76)
            }
        } else {
            d2d_rgba(pal.hover, 1.0)
        };
        let border_col = d2d_rgba(pal.border_rgb, if is_dark { 0.85 } else { 1.0 });
        let subtle_border = d2d_rgba(pal.border_rgb, if is_dark { 0.55 } else { 0.85 });
        let accent = d2d_rgba(pal.widget_accent, 1.0);
        let accent_blue = d2d_rgba(pal.widget_blue, 1.0);
        let sel_fill = d2d_rgba(pal.selected_fill, 1.0);
        let sel_stroke = d2d_rgba(pal.selected_stroke, 1.0);
        let sel_fg = d2d_rgba(pal.selected_fg, 1.0);
        let primary_fill = if themed {
            d2d_rgba(pal.accent, if is_dark { 0.30 } else { 1.0 })
        } else if is_dark {
            sel_fill
        } else {
            accent_blue
        };
        let primary_stroke = if themed {
            d2d_rgba(pal.accent, 1.0)
        } else if is_dark {
            sel_stroke
        } else {
            accent_blue
        };
        let primary_fg = if is_dark {
            d2d_rgba(pal.text_primary, 1.0)
        } else {
            d2d_rgba((0xff, 0xff, 0xff), 1.0)
        };
        let text_fg = d2d_rgba(pal.text_primary, 1.0);
        let muted_fg = d2d_rgba(pal.text_secondary, 1.0);
        let dim_fg = if is_dark {
            d2d_rgba((0x5c, 0x62, 0x70), 1.0)
        } else {
            d2d_rgba((0xae, 0xb4, 0xbe), 1.0)
        };
        let danger_bg = d2d_rgba(pal.danger_hover_bg, 1.0);
        let danger_stroke = d2d_rgba(pal.danger, 0.72);
        let danger_fg = d2d_rgba(pal.danger_fg, 1.0);
        let sep_col = d2d_rgba(pal.sep_rgb, if is_dark { 0.85 } else { 1.0 });
        let scroll_col = d2d_rgba(pal.text_secondary, 0.45);
        let track_col = if themed {
            d2d_rgba(pal.border_rgb, 1.0)
        } else if is_dark {
            d2d_rgba((0x34, 0x39, 0x45), 1.0)
        } else {
            d2d_rgba((0xdc, 0xe1, 0xe8), 1.0)
        };
        let thumb_core = d2d_rgba((0xff, 0xff, 0xff), 1.0);
        let thumb_halo = d2d_rgba(pal.widget_accent, 0.22);

        paint_dib_d2d(self.hwnd, self.dcomp_fg.as_mut(), |d2d, w, h| {
            let wf = w as f32;
            let content_w = (w - pad - right_pad).max(s(120.0));
            let content_wf = content_w as f32;
            if let Some(glass) = spatial_glass {
                glass.sync_rect(0.0, 0.0, wf, h as f32, sf(8.0).round(), dpi, 0.0, 0.0);
            }
            if use_spatial_glass {
                d2d.clear(d2d_rgba((0, 0, 0), 0.0));
                d2d.fill_rrect(0.0, 0.0, wf, h as f32, sf(8.0), surface_c);
            } else {
                d2d.clear(surface_c);
            }

            // Crisp top specular highlight along the upper flyout rim.
            d2d.draw_line(
                sf(12.0),
                0.5,
                (wf - sf(12.0)).max(sf(12.0)),
                0.5,
                d2d_rgba(pal.specular_top, if is_dark { 0.22 } else { 0.5 }),
                1.0,
            );

            // Header title + optional subtitle.
            if !spec.title.is_empty() {
                d2d.draw_text(
                    &spec.title,
                    d2d_rect(pad_f, pad_f, wf - pad_f, pad_f + sf(22.0)),
                    sf(14.5),
                    DWRITE_FONT_WEIGHT_SEMI_BOLD,
                    TextAlign::Left,
                    text_fg,
                    false,
                );
                let action_label = spec.blocks.iter().find_map(|block| match block {
                    FlyoutBlock::AppRow {
                        pin_id,
                        close_id,
                        pinned,
                        ..
                    } => {
                        let active = hovered_id.or(focused_id);
                        if active == Some(*pin_id) {
                            Some(loc::t(if *pinned {
                                "UnpinTrayIcon"
                            } else {
                                "PinTrayIcon"
                            }))
                        } else if active.is_some() && active == *close_id {
                            Some(loc::t("CloseWindow"))
                        } else {
                            None
                        }
                    }
                    _ => None,
                });
                if let Some(sub) = action_label.as_deref().or(spec.subtitle.as_deref()) {
                    d2d.draw_text(
                        sub,
                        d2d_rect(pad_f, pad_f + sf(23.0), wf - pad_f, pad_f + sf(41.0)),
                        sf(11.5),
                        DWRITE_FONT_WEIGHT_NORMAL,
                        TextAlign::Left,
                        muted_fg,
                        false,
                    );
                }
            }

            let clip_top = content_top_px;
            let clip_bottom = content_top_px + viewport_h_px;
            unsafe {
                d2d.target().PushAxisAlignedClip(
                    &d2d_rect(0.0, clip_top as f32, wf, clip_bottom as f32),
                    D2D1_ANTIALIAS_MODE_PER_PRIMITIVE,
                );
            }

            let mut y = content_top_px - scroll_y;
            for block in &spec.blocks {
                let block_h = block_height_px(block, dpi);
                if !block_intersects_viewport(y, y + block_h, clip_top, clip_bottom) {
                    y += block_h;
                    continue;
                }
                let yf = y as f32;
                match block {
                    FlyoutBlock::Hero {
                        value,
                        caption,
                        centered,
                        large,
                        right_meta,
                    } => {
                        let pt = if *large { 38.0 } else { 28.0 };
                        let val_h = if *large { s(46.0) } else { s(34.0) };
                        let align = if *centered {
                            TextAlign::Center
                        } else {
                            TextAlign::Left
                        };
                        let right_limit = if right_meta.is_some() && !*centered {
                            pad_f + content_wf - sf(68.0)
                        } else {
                            pad_f + content_wf
                        };
                        d2d.draw_text(
                            value,
                            d2d_rect(pad_f, yf, right_limit, yf + val_h as f32),
                            sf(pt),
                            DWRITE_FONT_WEIGHT_BOLD,
                            align,
                            text_fg,
                            true,
                        );
                        if let Some(meta) = right_meta {
                            let pill_w = sf(64.0).min(content_wf * 0.35);
                            let pill_h = sf(22.0);
                            let pl = pad_f + content_wf - pill_w;
                            let pt_y = yf + ((val_h as f32) - pill_h) * 0.5;
                            d2d.fill_rrect_bordered(
                                pl,
                                pt_y,
                                pl + pill_w,
                                pt_y + pill_h,
                                sf(6.0),
                                card_bg,
                                subtle_border,
                                1.0,
                            );
                            d2d.draw_text(
                                meta,
                                d2d_rect(pl + sf(4.0), pt_y, pl + pill_w - sf(4.0), pt_y + pill_h),
                                sf(10.5),
                                DWRITE_FONT_WEIGHT_SEMI_BOLD,
                                TextAlign::Center,
                                muted_fg,
                                true,
                            );
                        }
                        if let Some(cap) = caption {
                            d2d.draw_text(
                                cap,
                                d2d_rect(
                                    pad_f,
                                    yf + val_h as f32 + sf(2.0),
                                    pad_f + content_wf,
                                    yf + val_h as f32 + sf(20.0),
                                ),
                                sf(12.0),
                                DWRITE_FONT_WEIGHT_NORMAL,
                                align,
                                muted_fg,
                                false,
                            );
                            y += val_h + s(22.0);
                        } else {
                            y += val_h + s(6.0);
                        }
                    }
                    FlyoutBlock::HeroHeader {
                        glyph,
                        glyph_color,
                        title,
                        subtitle,
                        right_value,
                    } => {
                        let well = sf(44.0);
                        let well_fill = d2d_rgba(*glyph_color, if is_dark { 0.16 } else { 0.13 });
                        let well_stroke = d2d_rgba(*glyph_color, if is_dark { 0.34 } else { 0.28 });
                        d2d.fill_rrect_bordered(
                            pad_f,
                            yf,
                            pad_f + well,
                            yf + well,
                            sf(12.0),
                            well_fill,
                            well_stroke,
                            1.0,
                        );
                        let ch = char::from_u32(u32::from(*glyph)).unwrap_or('\u{E706}');
                        d2d.draw_glyph(
                            ch,
                            d2d_rect(pad_f, yf, pad_f + well, yf + well),
                            sf(22.0),
                            d2d_rgba(*glyph_color, 1.0),
                        );

                        let right_w = if right_value.is_some() { sf(82.0) } else { 0.0 };
                        let text_l = pad_f + well + sf(12.0);
                        let text_r = (pad_f + content_wf - right_w).max(text_l + sf(40.0));
                        if let Some(sub) = subtitle {
                            d2d.draw_text(
                                title,
                                d2d_rect(text_l, yf + sf(2.0), text_r, yf + sf(24.0)),
                                sf(15.0),
                                DWRITE_FONT_WEIGHT_SEMI_BOLD,
                                TextAlign::Left,
                                text_fg,
                                false,
                            );
                            d2d.draw_text(
                                sub,
                                d2d_rect(text_l, yf + sf(24.0), text_r, yf + sf(42.0)),
                                sf(12.0),
                                DWRITE_FONT_WEIGHT_NORMAL,
                                TextAlign::Left,
                                muted_fg,
                                false,
                            );
                        } else {
                            d2d.draw_text(
                                title,
                                d2d_rect(text_l, yf, text_r, yf + well),
                                sf(15.0),
                                DWRITE_FONT_WEIGHT_SEMI_BOLD,
                                TextAlign::Left,
                                text_fg,
                                false,
                            );
                        }
                        if let Some(rv) = right_value {
                            d2d.draw_text(
                                rv,
                                d2d_rect(text_r, yf, pad_f + content_wf, yf + well),
                                sf(26.0),
                                DWRITE_FONT_WEIGHT_BOLD,
                                TextAlign::Right,
                                text_fg,
                                true,
                            );
                        }
                        y += s(52.0);
                    }
                    FlyoutBlock::CalendarGrid {
                        year,
                        month,
                        today,
                        selected,
                        prev_year_id,
                        prev_month_id,
                        title_id,
                        next_month_id,
                        next_year_id,
                        day_id_base,
                    } => {
                        let nav_h = s(30.0);
                        let nav_hf = nav_h as f32;
                        let btn_w = s(28.0);
                        let gap = s(2.0);
                        let right = pad + content_w;
                        let nav_btns = [
                            (*prev_year_id, '\u{E76B}', true, pad, pad + btn_w),
                            (
                                *prev_month_id,
                                '\u{E76B}',
                                false,
                                pad + btn_w + gap,
                                pad + btn_w * 2 + gap,
                            ),
                            (
                                *next_month_id,
                                '\u{E76C}',
                                false,
                                right - btn_w * 2 - gap,
                                right - btn_w - gap,
                            ),
                            (*next_year_id, '\u{E76C}', true, right - btn_w, right),
                        ];
                        for (id, glyph, double, l, r) in nav_btns {
                            let lf = l as f32;
                            let rf = r as f32;
                            let hov = hovered_id == Some(id);
                            if hov {
                                d2d.fill_rrect(lf, yf, rf, yf + nav_hf, sf(7.0), hover_bg);
                            }
                            let fg = if hov { text_fg } else { muted_fg };
                            if double {
                                let shift = sf(2.6);
                                d2d.draw_glyph(
                                    glyph,
                                    d2d_rect(lf - shift, yf, rf - shift, yf + nav_hf),
                                    sf(9.5),
                                    fg,
                                );
                                d2d.draw_glyph(
                                    glyph,
                                    d2d_rect(lf + shift, yf, rf + shift, yf + nav_hf),
                                    sf(9.5),
                                    fg,
                                );
                            } else {
                                d2d.draw_glyph(
                                    glyph,
                                    d2d_rect(lf, yf, rf, yf + nav_hf),
                                    sf(11.0),
                                    fg,
                                );
                            }
                        }
                        let tl = (pad + btn_w * 2 + gap * 2) as f32;
                        let tr = (right - btn_w * 2 - gap * 2) as f32;
                        if hovered_id == Some(*title_id) {
                            d2d.fill_rrect(tl, yf, tr, yf + nav_hf, sf(7.0), hover_bg);
                        }
                        let title_str = format!("{} {}", telemetry::month_name(*month), year);
                        d2d.draw_text(
                            &title_str,
                            d2d_rect(tl, yf, tr, yf + nav_hf),
                            sf(13.0),
                            DWRITE_FONT_WEIGHT_SEMI_BOLD,
                            TextAlign::Center,
                            text_fg,
                            true,
                        );
                        y += nav_h + s(8.0);

                        // Weekday headers follow the user's culture (`FirstDayOfWeek`),
                        // not a hard-coded Monday-first column order.
                        let cell_w = content_w / 7;
                        let first_day = telemetry::first_day_of_week();
                        let dows: Vec<String> = calendar_weekday_order(first_day)
                            .into_iter()
                            .map(weekday_short_label)
                            .collect();
                        for (i, dow) in dows.iter().enumerate() {
                            let cl = (pad + (i as i32) * cell_w) as f32;
                            let cr = (pad + ((i as i32) + 1) * cell_w) as f32;
                            d2d.draw_text(
                                dow,
                                d2d_rect(cl, y as f32, cr, y as f32 + sf(18.0)),
                                sf(10.5),
                                DWRITE_FONT_WEIGHT_MEDIUM,
                                TextAlign::Center,
                                muted_fg,
                                false,
                            );
                        }
                        y += s(20.0);

                        let first_wd = telemetry::weekday_of(*year, *month, 1);
                        let mon_offset = calendar_leading_blanks(first_day, first_wd);
                        let prev_m = if *month == 1 { 12 } else { *month - 1 };
                        let prev_y = if *month == 1 { *year - 1 } else { *year };
                        let prev_days = telemetry::days_in_month(prev_y, prev_m) as i32;
                        let cur_days = telemetry::days_in_month(*year, *month) as i32;
                        let next_m = if *month == 12 { 1 } else { *month + 1 };
                        let next_y = if *month == 12 { *year + 1 } else { *year };
                        let cell_h = s(32.0);
                        let cell_r = sf(8.0);

                        for cell in 0..42i32 {
                            let row = cell / 7;
                            let col = cell % 7;
                            let d_idx = cell - mon_offset + 1;
                            let (cy, cm, cd, in_month) = if d_idx < 1 {
                                (prev_y, prev_m, (prev_days + d_idx) as u32, false)
                            } else if d_idx > cur_days {
                                (next_y, next_m, (d_idx - cur_days) as u32, false)
                            } else {
                                (*year, *month, d_idx as u32, true)
                            };
                            let is_today = (cy, cm, cd) == *today;
                            let is_sel =
                                selected.is_some_and(|sel| sel == (cy, cm, cd)) && !is_today;
                            let id = day_id_base + cell as u32;
                            let is_hov = hovered_id == Some(id);

                            let l = (pad + col * cell_w) as f32 + sf(2.0);
                            let t = (y + row * cell_h) as f32 + sf(2.0);
                            let r = (pad + (col + 1) * cell_w) as f32 - sf(2.0);
                            let b = (y + (row + 1) * cell_h) as f32 - sf(2.0);

                            if is_today {
                                d2d.fill_rrect_bordered(
                                    l,
                                    t,
                                    r,
                                    b,
                                    cell_r,
                                    primary_fill,
                                    primary_stroke,
                                    border_w,
                                );
                            } else if is_sel {
                                d2d.fill_rrect_bordered(
                                    l,
                                    t,
                                    r,
                                    b,
                                    cell_r,
                                    card_bg,
                                    accent_blue,
                                    1.2,
                                );
                            } else if is_hov {
                                d2d.fill_rrect(l, t, r, b, cell_r, hover_bg);
                            }

                            let fg = if is_today {
                                primary_fg
                            } else if in_month {
                                text_fg
                            } else {
                                dim_fg
                            };
                            let weight = if is_today || is_sel {
                                DWRITE_FONT_WEIGHT_SEMI_BOLD
                            } else {
                                DWRITE_FONT_WEIGHT_NORMAL
                            };
                            let ds = cd.to_string();
                            d2d.draw_text(
                                &ds,
                                d2d_rect(l, t, r, b),
                                sf(12.0),
                                weight,
                                TextAlign::Center,
                                fg,
                                true,
                            );
                        }
                        y += 6 * cell_h + s(6.0);
                    }
                    FlyoutBlock::AudioSliderSection {
                        label,
                        device_name,
                        value_text,
                        ratio,
                        muted,
                        mute_id,
                        slider_id,
                    } => {
                        let is_input = is_audio_input_label(label);
                        d2d.draw_text(
                            label,
                            d2d_rect(pad_f, yf, pad_f + content_wf - sf(48.0), yf + sf(18.0)),
                            sf(12.5),
                            DWRITE_FONT_WEIGHT_SEMI_BOLD,
                            TextAlign::Left,
                            text_fg,
                            false,
                        );
                        d2d.draw_text(
                            value_text,
                            d2d_rect(
                                pad_f + content_wf - sf(56.0),
                                yf,
                                pad_f + content_wf,
                                yf + sf(18.0),
                            ),
                            sf(11.5),
                            DWRITE_FONT_WEIGHT_SEMI_BOLD,
                            TextAlign::Right,
                            if *muted { danger_fg } else { muted_fg },
                            true,
                        );

                        let dev_top = y + s(22.0);
                        let dev_top_f = dev_top as f32;
                        let dev_h = s(32.0);
                        let dev_hf = dev_h as f32;
                        let mute_w = s(32.0);
                        let mute_wf = mute_w as f32;
                        let dev_r = pad_f + content_wf - mute_wf - sf(8.0);
                        d2d.fill_rrect_bordered(
                            pad_f,
                            dev_top_f,
                            dev_r,
                            dev_top_f + dev_hf,
                            pill_r,
                            card_bg,
                            subtle_border,
                            1.0,
                        );
                        let dev_icon = if is_input { '\u{E720}' } else { '\u{E7F5}' };
                        d2d.draw_glyph(
                            dev_icon,
                            d2d_rect(
                                pad_f + sf(6.0),
                                dev_top_f,
                                pad_f + sf(26.0),
                                dev_top_f + dev_hf,
                            ),
                            sf(12.5),
                            muted_fg,
                        );
                        d2d.draw_text(
                            device_name,
                            d2d_rect(
                                pad_f + sf(28.0),
                                dev_top_f,
                                dev_r - sf(8.0),
                                dev_top_f + dev_hf,
                            ),
                            sf(11.5),
                            DWRITE_FONT_WEIGHT_NORMAL,
                            TextAlign::Left,
                            text_fg,
                            false,
                        );

                        let mute_l = pad_f + content_wf - mute_wf;
                        let mute_hov = hovered_id == Some(*mute_id);
                        let (m_bg, m_bd, m_fg) = if *muted {
                            (danger_bg, danger_stroke, danger_fg)
                        } else if mute_hov {
                            (hover_bg, border_col, text_fg)
                        } else {
                            (card_bg, subtle_border, muted_fg)
                        };
                        d2d.fill_rrect_bordered(
                            mute_l,
                            dev_top_f,
                            mute_l + mute_wf,
                            dev_top_f + dev_hf,
                            pill_r,
                            m_bg,
                            m_bd,
                            1.0,
                        );
                        let mute_glyph = if is_input {
                            if *muted { '\u{EC54}' } else { '\u{E720}' }
                        } else if *muted {
                            '\u{E74F}'
                        } else {
                            '\u{E767}'
                        };
                        d2d.draw_glyph(
                            mute_glyph,
                            d2d_rect(mute_l, dev_top_f, mute_l + mute_wf, dev_top_f + dev_hf),
                            sf(14.0),
                            m_fg,
                        );

                        // Anti-aliased pill slider track + Aurora fill + circular thumb with halo.
                        let slider_top = dev_top + dev_h + s(8.0);
                        let hot_slider = hovered_id == Some(*slider_id)
                            || dragging_slider.is_some_and(|(sid, _, _)| sid == *slider_id);
                        draw_slider_track(
                            d2d,
                            pad_f,
                            content_wf,
                            slider_top as f32 + sf(10.0),
                            if *muted { 0.0 } else { *ratio },
                            hot_slider,
                            dpi,
                            track_col,
                            accent,
                            if *muted { muted_fg } else { accent },
                            thumb_core,
                            thumb_halo,
                        );

                        y = slider_top + s(24.0);
                    }
                    FlyoutBlock::MetricTiles(tiles) => {
                        let rows = tiles.len().div_ceil(2) as i32;
                        let tile_h = s(48.0);
                        let tile_hf = tile_h as f32;
                        let gap = s(8.0);
                        let tw = ((content_w - gap) / 2).max(s(80.0));
                        let twf = tw as f32;
                        for (i, (lbl, val)) in tiles.iter().enumerate() {
                            let col = (i % 2) as i32;
                            let row = (i / 2) as i32;
                            let l = (pad + col * (tw + gap)) as f32;
                            let t = (y + row * (tile_h + gap)) as f32;
                            d2d.fill_rrect_bordered(
                                l,
                                t,
                                l + twf,
                                t + tile_hf,
                                card_r,
                                card_bg,
                                subtle_border,
                                1.0,
                            );
                            d2d.draw_text(
                                lbl,
                                d2d_rect(
                                    l + sf(10.0),
                                    t + sf(6.0),
                                    l + twf - sf(10.0),
                                    t + sf(21.0),
                                ),
                                sf(10.5),
                                DWRITE_FONT_WEIGHT_MEDIUM,
                                TextAlign::Left,
                                muted_fg,
                                false,
                            );
                            d2d.draw_text(
                                val,
                                d2d_rect(
                                    l + sf(10.0),
                                    t + sf(22.0),
                                    l + twf - sf(10.0),
                                    t + tile_hf - sf(6.0),
                                ),
                                sf(13.0),
                                DWRITE_FONT_WEIGHT_SEMI_BOLD,
                                TextAlign::Left,
                                text_fg,
                                true,
                            );
                        }
                        y += rows * tile_h + (rows - 1).max(0) * gap + s(6.0);
                    }
                    FlyoutBlock::BadgeRow {
                        id,
                        code,
                        title,
                        active,
                    } => {
                        let h = s(38.0);
                        let hf = h as f32;
                        let hovered = hovered_id == Some(*id);
                        if hovered || *active {
                            d2d.fill_rrect_bordered(
                                pad_f,
                                yf,
                                pad_f + content_wf,
                                yf + hf,
                                pill_r,
                                if hovered { hover_bg } else { card_bg },
                                if *active { sel_stroke } else { subtle_border },
                                1.0,
                            );
                        }
                        let bw = sf(34.0);
                        let bh = sf(24.0);
                        let bx = pad_f + sf(8.0);
                        let by = yf + (hf - bh) * 0.5;
                        d2d.fill_rrect_bordered(
                            bx,
                            by,
                            bx + bw,
                            by + bh,
                            sf(6.0),
                            if *active { primary_fill } else { card_bg },
                            if *active {
                                primary_stroke
                            } else {
                                subtle_border
                            },
                            1.0,
                        );
                        let code_fg = if *active { primary_fg } else { text_fg };
                        if let Some(ch) = is_single_pua_glyph(code) {
                            d2d.draw_glyph(
                                ch,
                                d2d_rect(bx, by, bx + bw, by + bh),
                                sf(13.0),
                                code_fg,
                            );
                        } else {
                            d2d.draw_text(
                                code,
                                d2d_rect(bx, by, bx + bw, by + bh),
                                sf(11.0),
                                DWRITE_FONT_WEIGHT_SEMI_BOLD,
                                TextAlign::Center,
                                code_fg,
                                true,
                            );
                        }
                        let right_inset = if *active { sf(28.0) } else { sf(10.0) };
                        d2d.draw_text(
                            title,
                            d2d_rect(
                                bx + bw + sf(10.0),
                                yf,
                                pad_f + content_wf - right_inset,
                                yf + hf,
                            ),
                            sf(12.5),
                            if *active {
                                DWRITE_FONT_WEIGHT_SEMI_BOLD
                            } else {
                                DWRITE_FONT_WEIGHT_NORMAL
                            },
                            TextAlign::Left,
                            text_fg,
                            false,
                        );
                        if *active {
                            d2d.draw_glyph(
                                '\u{E73E}',
                                d2d_rect(
                                    pad_f + content_wf - sf(26.0),
                                    yf,
                                    pad_f + content_wf - sf(6.0),
                                    yf + hf,
                                ),
                                sf(12.0),
                                accent,
                            );
                        }
                        y += h + s(4.0);
                    }
                    FlyoutBlock::StatusHeader {
                        title,
                        status_label,
                        status_color,
                        action_id,
                        action_label,
                    } => {
                        let h = s(28.0);
                        let hf = h as f32;
                        d2d.draw_text(
                            title,
                            d2d_rect(pad_f, yf, pad_f + content_wf - sf(90.0), yf + hf),
                            sf(12.5),
                            DWRITE_FONT_WEIGHT_SEMI_BOLD,
                            TextAlign::Left,
                            text_fg,
                            false,
                        );
                        let right_edge = if let (Some(id), Some(lbl)) = (*action_id, action_label) {
                            let bw = sf(64.0);
                            let bl = pad_f + content_wf - bw;
                            let hovered = hovered_id == Some(id);
                            d2d.fill_rrect_bordered(
                                bl,
                                yf + sf(2.0),
                                pad_f + content_wf,
                                yf + hf - sf(2.0),
                                sf(6.0),
                                if hovered { hover_bg } else { card_bg },
                                subtle_border,
                                1.0,
                            );
                            d2d.draw_text(
                                lbl,
                                d2d_rect(bl, yf, pad_f + content_wf, yf + hf),
                                sf(10.5),
                                DWRITE_FONT_WEIGHT_MEDIUM,
                                TextAlign::Center,
                                text_fg,
                                false,
                            );
                            bl - sf(8.0)
                        } else {
                            pad_f + content_wf
                        };
                        d2d.draw_text(
                            status_label,
                            d2d_rect(pad_f + content_wf * 0.33, yf, right_edge, yf + hf),
                            sf(11.0),
                            DWRITE_FONT_WEIGHT_NORMAL,
                            TextAlign::Right,
                            muted_fg,
                            false,
                        );
                        let dot_r = sf(3.2).max(2.5);
                        let dot_x = right_edge - sf(status_label.len() as f32 * 6.2 + 8.0);
                        d2d.fill_circle(dot_x, yf + hf * 0.5, dot_r, d2d_rgba(*status_color, 1.0));
                        y += h + s(6.0);
                    }
                    FlyoutBlock::ToggleGrid(tiles) => {
                        let rows = tiles.len().div_ceil(2) as i32;
                        let tile_h = s(52.0);
                        let tile_hf = tile_h as f32;
                        let gap = s(8.0);
                        let tw = ((content_w - gap) / 2).max(s(80.0));
                        let twf = tw as f32;
                        for (i, (id, title, state_lbl, active)) in tiles.iter().enumerate() {
                            let col = (i % 2) as i32;
                            let row = (i / 2) as i32;
                            let left = (pad + col * (tw + gap)) as f32;
                            let top = (y + row * (tile_h + gap)) as f32;
                            let hovered = hovered_id == Some(*id);
                            let (fill, bd) = if *active {
                                (sel_fill, sel_stroke)
                            } else if hovered {
                                (hover_bg, border_col)
                            } else {
                                (card_bg, subtle_border)
                            };
                            d2d.fill_rrect_bordered(
                                left,
                                top,
                                left + twf,
                                top + tile_hf,
                                card_r,
                                fill,
                                bd,
                                border_w,
                            );
                            d2d.draw_text(
                                title,
                                d2d_rect(
                                    left + sf(11.0),
                                    top + sf(8.0),
                                    left + twf - sf(10.0),
                                    top + sf(26.0),
                                ),
                                sf(12.5),
                                DWRITE_FONT_WEIGHT_SEMI_BOLD,
                                TextAlign::Left,
                                if *active { sel_fg } else { text_fg },
                                false,
                            );
                            d2d.draw_text(
                                state_lbl,
                                d2d_rect(
                                    left + sf(11.0),
                                    top + sf(27.0),
                                    left + twf - sf(10.0),
                                    top + tile_hf - sf(7.0),
                                ),
                                sf(11.0),
                                DWRITE_FONT_WEIGHT_NORMAL,
                                TextAlign::Left,
                                muted_fg,
                                false,
                            );
                        }
                        y += rows * tile_h + (rows - 1).max(0) * gap + s(6.0);
                    }
                    FlyoutBlock::IconToggleGrid(tiles) => {
                        let rows = tiles.len().div_ceil(2) as i32;
                        let tile_h = s(56.0);
                        let tile_hf = tile_h as f32;
                        let gap = s(8.0);
                        let tw = ((content_w - gap) / 2).max(s(80.0));
                        let twf = tw as f32;
                        for (i, (id, glyph, title, state_lbl, active)) in tiles.iter().enumerate() {
                            let col = (i % 2) as i32;
                            let row = (i / 2) as i32;
                            let left = (pad + col * (tw + gap)) as f32;
                            let top = (y + row * (tile_h + gap)) as f32;
                            let hovered = hovered_id == Some(*id);
                            let (fill, bd) = if *active {
                                (sel_fill, sel_stroke)
                            } else if hovered {
                                (hover_bg, border_col)
                            } else {
                                (card_bg, subtle_border)
                            };
                            d2d.fill_rrect_bordered(
                                left,
                                top,
                                left + twf,
                                top + tile_hf,
                                card_r,
                                fill,
                                bd,
                                border_w,
                            );
                            let well = sf(30.0);
                            let wl = left + sf(10.0);
                            let wt = top + (tile_hf - well) * 0.5;
                            let well_bg = if *active {
                                d2d_rgba(pal.widget_blue, if is_dark { 0.24 } else { 0.16 })
                            } else {
                                d2d_rgba(pal.text_secondary, if is_dark { 0.12 } else { 0.10 })
                            };
                            d2d.fill_rrect(wl, wt, wl + well, wt + well, sf(8.0), well_bg);
                            let ch = char::from_u32(u32::from(*glyph)).unwrap_or('\u{E713}');
                            d2d.draw_glyph(
                                ch,
                                d2d_rect(wl, wt, wl + well, wt + well),
                                sf(14.0),
                                if *active { sel_fg } else { muted_fg },
                            );
                            let tl = wl + well + sf(9.0);
                            let tr = left + twf - sf(8.0);
                            d2d.draw_text(
                                title,
                                d2d_rect(tl, top + sf(9.0), tr, top + sf(27.0)),
                                sf(12.0),
                                DWRITE_FONT_WEIGHT_SEMI_BOLD,
                                TextAlign::Left,
                                if *active { sel_fg } else { text_fg },
                                false,
                            );
                            d2d.draw_text(
                                state_lbl,
                                d2d_rect(tl, top + sf(28.0), tr, top + tile_hf - sf(8.0)),
                                sf(11.0),
                                DWRITE_FONT_WEIGHT_NORMAL,
                                TextAlign::Left,
                                muted_fg,
                                false,
                            );
                        }
                        y += rows * tile_h + (rows - 1).max(0) * gap + s(6.0);
                    }
                    FlyoutBlock::GlyphStatRow {
                        glyph,
                        glyph_color,
                        label,
                        detail,
                        value,
                    } => {
                        let h = s(30.0);
                        let hf = h as f32;
                        let icon_sz = sf(22.0);
                        let it = yf + (hf - icon_sz) * 0.5;
                        d2d.fill_rrect(
                            pad_f,
                            it,
                            pad_f + icon_sz,
                            it + icon_sz,
                            sf(6.0),
                            d2d_rgba(*glyph_color, if is_dark { 0.15 } else { 0.12 }),
                        );
                        let ch = char::from_u32(u32::from(*glyph)).unwrap_or('\u{E706}');
                        d2d.draw_glyph(
                            ch,
                            d2d_rect(pad_f, it, pad_f + icon_sz, it + icon_sz),
                            sf(12.0),
                            d2d_rgba(*glyph_color, 1.0),
                        );
                        let text_l = pad_f + icon_sz + sf(9.0);
                        let val_w = sf(78.0);
                        let mid_r = (pad_f + content_wf - val_w).max(text_l + sf(40.0));
                        let combined = match detail {
                            Some(d) if !d.is_empty() => format!("{label} · {d}"),
                            _ => label.clone(),
                        };
                        d2d.draw_text(
                            &combined,
                            d2d_rect(text_l, yf, mid_r, yf + hf),
                            sf(12.0),
                            DWRITE_FONT_WEIGHT_MEDIUM,
                            TextAlign::Left,
                            text_fg,
                            false,
                        );
                        d2d.draw_text(
                            value,
                            d2d_rect(mid_r, yf, pad_f + content_wf, yf + hf),
                            sf(12.0),
                            DWRITE_FONT_WEIGHT_SEMI_BOLD,
                            TextAlign::Right,
                            text_fg,
                            true,
                        );
                        y += h;
                    }
                    FlyoutBlock::GlyphActionRow {
                        id,
                        glyph,
                        glyph_color,
                        title,
                        subtitle,
                        badge,
                    } => {
                        let h = if subtitle.is_some() { s(44.0) } else { s(38.0) };
                        let hf = h as f32;
                        let hovered = hovered_id == Some(*id);
                        d2d.fill_rrect_bordered(
                            pad_f,
                            yf,
                            pad_f + content_wf,
                            yf + hf,
                            card_r,
                            if hovered { hover_bg } else { card_bg },
                            if hovered { border_col } else { subtle_border },
                            1.0,
                        );
                        let well = sf(28.0);
                        let wl = pad_f + sf(8.0);
                        let wt = yf + (hf - well) * 0.5;
                        d2d.fill_rrect(
                            wl,
                            wt,
                            wl + well,
                            wt + well,
                            sf(7.0),
                            d2d_rgba(*glyph_color, if is_dark { 0.16 } else { 0.13 }),
                        );
                        let ch = char::from_u32(u32::from(*glyph)).unwrap_or('\u{E8B7}');
                        d2d.draw_glyph(
                            ch,
                            d2d_rect(wl, wt, wl + well, wt + well),
                            sf(14.0),
                            d2d_rgba(*glyph_color, 1.0),
                        );
                        let text_l = wl + well + sf(10.0);
                        let right_reserve = if badge.is_some() { sf(68.0) } else { sf(24.0) };
                        let text_r = (pad_f + content_wf - right_reserve).max(text_l + sf(40.0));
                        if let Some(sub) = subtitle {
                            d2d.draw_text(
                                title,
                                d2d_rect(text_l, yf + sf(4.0), text_r, yf + sf(22.0)),
                                sf(12.5),
                                DWRITE_FONT_WEIGHT_SEMI_BOLD,
                                TextAlign::Left,
                                text_fg,
                                false,
                            );
                            d2d.draw_text(
                                sub,
                                d2d_rect(text_l, yf + sf(22.0), text_r, yf + hf - sf(4.0)),
                                sf(11.0),
                                DWRITE_FONT_WEIGHT_NORMAL,
                                TextAlign::Left,
                                muted_fg,
                                false,
                            );
                        } else {
                            d2d.draw_text(
                                title,
                                d2d_rect(text_l, yf, text_r, yf + hf),
                                sf(12.5),
                                DWRITE_FONT_WEIGHT_MEDIUM,
                                TextAlign::Left,
                                text_fg,
                                false,
                            );
                        }
                        if let Some(b) = badge {
                            d2d.draw_text(
                                b,
                                d2d_rect(text_r, yf, pad_f + content_wf - sf(22.0), yf + hf),
                                sf(11.0),
                                DWRITE_FONT_WEIGHT_MEDIUM,
                                TextAlign::Right,
                                muted_fg,
                                true,
                            );
                        }
                        d2d.draw_glyph(
                            '\u{E76C}',
                            d2d_rect(
                                pad_f + content_wf - sf(22.0),
                                yf,
                                pad_f + content_wf - sf(6.0),
                                yf + hf,
                            ),
                            sf(10.0),
                            if hovered { text_fg } else { muted_fg },
                        );
                        y += h + s(5.0);
                    }
                    FlyoutBlock::StatRow { label, value } => {
                        let h = s(24.0);
                        let hf = h as f32;
                        let is_section_heading = value.is_empty();
                        d2d.draw_text(
                            label,
                            d2d_rect(
                                pad_f,
                                yf,
                                if is_section_heading {
                                    pad_f + content_wf
                                } else {
                                    pad_f + content_wf * 0.62
                                },
                                yf + hf,
                            ),
                            if is_section_heading {
                                sf(11.5)
                            } else {
                                sf(12.0)
                            },
                            if is_section_heading {
                                DWRITE_FONT_WEIGHT_SEMI_BOLD
                            } else {
                                DWRITE_FONT_WEIGHT_NORMAL
                            },
                            TextAlign::Left,
                            muted_fg,
                            false,
                        );
                        if !value.is_empty() {
                            d2d.draw_text(
                                value,
                                d2d_rect(
                                    pad_f + content_wf * 0.45,
                                    yf,
                                    pad_f + content_wf,
                                    yf + hf,
                                ),
                                sf(12.0),
                                DWRITE_FONT_WEIGHT_SEMI_BOLD,
                                TextAlign::Right,
                                text_fg,
                                true,
                            );
                        }
                        y += h;
                    }
                    FlyoutBlock::ProgressBar {
                        label,
                        value_text,
                        ratio,
                        tone,
                    } => {
                        let compact = label.is_empty() && value_text.is_empty();
                        let bar_top = if compact {
                            yf + sf(4.0)
                        } else {
                            d2d.draw_text(
                                label,
                                d2d_rect(pad_f, yf, pad_f + content_wf * 0.65, yf + sf(16.0)),
                                sf(11.5),
                                DWRITE_FONT_WEIGHT_MEDIUM,
                                TextAlign::Left,
                                muted_fg,
                                false,
                            );
                            d2d.draw_text(
                                value_text,
                                d2d_rect(
                                    pad_f + content_wf * 0.5,
                                    yf,
                                    pad_f + content_wf,
                                    yf + sf(16.0),
                                ),
                                sf(11.5),
                                DWRITE_FONT_WEIGHT_SEMI_BOLD,
                                TextAlign::Right,
                                text_fg,
                                true,
                            );
                            yf + sf(21.0)
                        };
                        let bar_h = sf(6.0).max(5.0);
                        let bar_r = bar_h * 0.5;
                        d2d.fill_rrect(
                            pad_f,
                            bar_top,
                            pad_f + content_wf,
                            bar_top + bar_h,
                            bar_r,
                            track_col,
                        );
                        let fill_w = content_wf * ratio.clamp(0.0, 1.0);
                        if fill_w > sf(2.0) {
                            let meter_rgb = match tone {
                                BarTone::Accent => pal.widget_accent,
                                BarTone::Usage => design::meter_color(&pal, *ratio),
                                BarTone::Level => design::level_color(&pal, *ratio),
                            };
                            d2d.fill_rrect(
                                pad_f,
                                bar_top,
                                pad_f + fill_w.max(bar_h),
                                bar_top + bar_h,
                                bar_r,
                                d2d_rgba(meter_rgb, 1.0),
                            );
                        }
                        y += if compact { s(16.0) } else { s(34.0) };
                    }
                    FlyoutBlock::ActionRow {
                        id,
                        title,
                        subtitle,
                        badge,
                        icon,
                        destructive,
                    } => {
                        let h = if subtitle.is_some() { s(42.0) } else { s(34.0) };
                        let hf = h as f32;
                        let hovered = hovered_id == Some(*id);
                        let (fill, bd) = if hovered {
                            if *destructive {
                                (danger_bg, danger_stroke)
                            } else {
                                (hover_bg, border_col)
                            }
                        } else {
                            (card_bg, subtle_border)
                        };
                        d2d.fill_rrect_bordered(
                            pad_f,
                            yf,
                            pad_f + content_wf,
                            yf + hf,
                            pill_r,
                            fill,
                            bd,
                            1.0,
                        );

                        let mut text_left = pad_f + sf(11.0);
                        if let Some(ic) = icon {
                            let isz = sf(18.0);
                            let iy = yf + (hf - isz) * 0.5;
                            icon_bitmaps.draw_bgra(
                                d2d,
                                ic.width,
                                ic.height,
                                &ic.bgra,
                                d2d_rect(pad_f + sf(9.0), iy, pad_f + sf(9.0) + isz, iy + isz),
                            );
                            text_left = pad_f + sf(35.0);
                        }
                        let right_limit = if badge.is_some() {
                            pad_f + content_wf - sf(62.0)
                        } else {
                            pad_f + content_wf - sf(10.0)
                        };
                        d2d.draw_text(
                            title,
                            d2d_rect(
                                text_left,
                                yf + if subtitle.is_some() { sf(3.0) } else { 0.0 },
                                right_limit,
                                yf + if subtitle.is_some() { sf(21.0) } else { hf },
                            ),
                            sf(12.5),
                            DWRITE_FONT_WEIGHT_MEDIUM,
                            TextAlign::Left,
                            if *destructive { danger_fg } else { text_fg },
                            false,
                        );
                        if let Some(sub) = subtitle {
                            d2d.draw_text(
                                sub,
                                d2d_rect(text_left, yf + sf(21.0), right_limit, yf + hf - sf(3.0)),
                                sf(11.0),
                                DWRITE_FONT_WEIGHT_NORMAL,
                                TextAlign::Left,
                                muted_fg,
                                false,
                            );
                        }
                        if let Some(b) = badge {
                            d2d.draw_text(
                                b,
                                d2d_rect(right_limit, yf, pad_f + content_wf - sf(10.0), yf + hf),
                                sf(11.0),
                                DWRITE_FONT_WEIGHT_MEDIUM,
                                TextAlign::Right,
                                muted_fg,
                                true,
                            );
                        }
                        y += h + s(4.0);
                    }
                    FlyoutBlock::AppRow {
                        id,
                        title,
                        icon,
                        pin_id,
                        pinned,
                        close_id,
                    } => {
                        let [app, pin, close] = app_row_targets(pad, pad + content_w, y, dpi);
                        let row_hovered = hovered_id == Some(*id)
                            || focused_id == Some(*id)
                            || hovered_id == Some(*pin_id)
                            || (*close_id).is_some_and(|cid| hovered_id == Some(cid));
                        if row_hovered {
                            // The hover pill bleeds 4 DIP into the side margins so the
                            // icon and the action pills stay aligned with the header.
                            d2d.fill_rrect(
                                app.left as f32 - sf(4.0),
                                yf,
                                close.right as f32 + sf(4.0),
                                app.bottom as f32,
                                pill_r,
                                hover_bg,
                            );
                        }
                        let icon_size = sf(22.0);
                        let icon_x = app.left as f32;
                        let icon_y = yf + ((app.bottom - y) as f32 - icon_size) * 0.5;
                        if let Some(icon) = icon {
                            icon_bitmaps.draw_bgra(
                                d2d,
                                icon.width,
                                icon.height,
                                &icon.bgra,
                                d2d_rect(icon_x, icon_y, icon_x + icon_size, icon_y + icon_size),
                            );
                        } else {
                            d2d.draw_glyph(
                                '\u{E81E}',
                                d2d_rect(icon_x, icon_y, icon_x + icon_size, icon_y + icon_size),
                                sf(15.0),
                                muted_fg,
                            );
                        }
                        d2d.draw_text(
                            title,
                            d2d_rect(
                                app.left as f32 + sf(32.0),
                                yf,
                                app.right as f32 - sf(6.0),
                                app.bottom as f32,
                            ),
                            sf(12.5),
                            DWRITE_FONT_WEIGHT_MEDIUM,
                            TextAlign::Left,
                            text_fg,
                            false,
                        );

                        for (btn_id, rect, glyph, selected, fill, ink) in [
                            (
                                Some(*pin_id),
                                pin,
                                if *pinned { '\u{E718}' } else { '\u{E840}' },
                                *pinned,
                                sel_fill,
                                sel_fg,
                            ),
                            (*close_id, close, '\u{E711}', false, danger_bg, danger_fg),
                        ] {
                            let Some(btn_id) = btn_id else { continue };
                            let active = hovered_id == Some(btn_id) || focused_id == Some(btn_id);
                            // 30x30 DIP pill: full target width, 6 DIP vertical inset
                            // inside the 42 DIP row. Close uses the danger (red) fill.
                            let inset = sf(6.0);
                            if active || selected {
                                d2d.fill_rrect(
                                    rect.left as f32,
                                    yf + inset,
                                    rect.right as f32,
                                    rect.bottom as f32 - inset,
                                    sf(7.0),
                                    fill,
                                );
                            }
                            d2d.draw_glyph(
                                glyph,
                                d2d_rect(
                                    rect.left as f32,
                                    yf,
                                    rect.right as f32,
                                    rect.bottom as f32,
                                ),
                                sf(12.0),
                                if active || selected { ink } else { muted_fg },
                            );
                        }
                        y = app.bottom + s(3.0);
                    }
                    FlyoutBlock::ButtonBar(buttons) => {
                        let h = s(34.0);
                        let hf = h as f32;
                        let count = buttons.len().max(1) as i32;
                        let gap = s(6.0);
                        let bw = ((content_w - gap * (count - 1)) / count).max(s(40.0));
                        let bwf = bw as f32;
                        for (i, (id, label, primary)) in buttons.iter().enumerate() {
                            let left = (pad + (i as i32) * (bw + gap)) as f32;
                            let hovered = hovered_id == Some(*id) || focused_id == Some(*id);
                            let (fill, bd, fg) = if *primary {
                                (
                                    if hovered && is_dark {
                                        d2d_rgba((0x2b, 0x40, 0x61), 1.0)
                                    } else {
                                        primary_fill
                                    },
                                    primary_stroke,
                                    primary_fg,
                                )
                            } else if hovered {
                                (hover_bg, border_col, text_fg)
                            } else {
                                (card_bg, subtle_border, text_fg)
                            };
                            d2d.fill_rrect_bordered(
                                left,
                                yf,
                                left + bwf,
                                yf + hf,
                                pill_r,
                                fill,
                                bd,
                                border_w,
                            );
                            d2d.draw_text(
                                label,
                                d2d_rect(left + sf(6.0), yf, left + bwf - sf(6.0), yf + hf),
                                sf(12.0),
                                DWRITE_FONT_WEIGHT_SEMI_BOLD,
                                TextAlign::Center,
                                fg,
                                false,
                            );
                        }
                        y += h + s(6.0);
                    }
                    FlyoutBlock::SeekSlider {
                        id,
                        label,
                        position_secs,
                        duration_secs,
                        ratio,
                    } => {
                        d2d.draw_text(
                            label,
                            d2d_rect(pad_f, yf, pad_f + content_wf * 0.5, yf + sf(18.0)),
                            sf(12.0),
                            DWRITE_FONT_WEIGHT_SEMI_BOLD,
                            TextAlign::Left,
                            text_fg,
                            false,
                        );
                        let times = format!(
                            "{} / {}",
                            format_clock(*position_secs),
                            format_clock(*duration_secs)
                        );
                        d2d.draw_text(
                            &times,
                            d2d_rect(
                                pad_f + content_wf * 0.5,
                                yf,
                                pad_f + content_wf,
                                yf + sf(18.0),
                            ),
                            sf(11.0),
                            DWRITE_FONT_WEIGHT_NORMAL,
                            TextAlign::Right,
                            muted_fg,
                            true,
                        );
                        let hot = hovered_id == Some(*id)
                            || dragging_slider.is_some_and(|(sid, _, seek)| sid == *id && seek);
                        draw_slider_track(
                            d2d,
                            pad_f,
                            content_wf,
                            yf + sf(29.0),
                            *ratio,
                            hot,
                            dpi,
                            track_col,
                            accent,
                            accent,
                            thumb_core,
                            thumb_halo,
                        );
                        y += s(44.0);
                    }
                    FlyoutBlock::TextInput {
                        id,
                        label,
                        value,
                        numeric,
                        ..
                    } => {
                        d2d.draw_text(
                            label,
                            d2d_rect(pad_f, yf, pad_f + content_wf, yf + sf(16.0)),
                            sf(11.0),
                            DWRITE_FONT_WEIGHT_MEDIUM,
                            TextAlign::Left,
                            muted_fg,
                            false,
                        );
                        let box_top = yf + sf(18.0);
                        let box_h = sf(32.0);
                        let editing = editing_state.as_ref().is_some_and(|(eid, _)| eid == id);
                        let hovered = hovered_id == Some(*id);
                        d2d.fill_rrect_bordered(
                            pad_f,
                            box_top,
                            pad_f + content_wf,
                            box_top + box_h,
                            pill_r,
                            if editing || hovered {
                                hover_bg
                            } else {
                                card_bg
                            },
                            if editing { accent } else { border_col },
                            if editing { 1.5 } else { 1.0 },
                        );
                        let shown = if editing {
                            let buf = editing_state
                                .as_ref()
                                .map(|(_, b)| b.as_str())
                                .unwrap_or("");
                            format!("{buf}|")
                        } else {
                            value.clone()
                        };
                        d2d.draw_text(
                            &shown,
                            d2d_rect(
                                pad_f + sf(10.0),
                                box_top,
                                pad_f + content_wf - sf(28.0),
                                box_top + box_h,
                            ),
                            sf(12.5),
                            DWRITE_FONT_WEIGHT_MEDIUM,
                            TextAlign::Left,
                            if editing { text_fg } else { muted_fg },
                            *numeric,
                        );
                        if *numeric {
                            d2d.draw_glyph(
                                '\u{E916}',
                                d2d_rect(
                                    pad_f + content_wf - sf(26.0),
                                    box_top,
                                    pad_f + content_wf - sf(6.0),
                                    box_top + box_h,
                                ),
                                sf(12.0),
                                muted_fg,
                            );
                        }
                        y += s(56.0);
                    }
                    FlyoutBlock::Chart {
                        label,
                        series,
                        accent: series_accent,
                    } => {
                        d2d.draw_text(
                            label,
                            d2d_rect(pad_f, yf, pad_f + content_wf, yf + sf(16.0)),
                            sf(12.0),
                            DWRITE_FONT_WEIGHT_SEMI_BOLD,
                            TextAlign::Left,
                            text_fg,
                            false,
                        );
                        let plot_top = yf + sf(18.0);
                        let plot_h = sf(42.0);
                        let plot_bot = plot_top + plot_h;
                        d2d.draw_line(pad_f, plot_bot, pad_f + content_wf, plot_bot, sep_col, 1.0);

                        let palette_colors = [
                            *series_accent,
                            pal.widget_blue,
                            pal.warning,
                            pal.success,
                            pal.danger,
                        ];
                        for (series_idx, (_, points)) in series.iter().enumerate() {
                            if points.is_empty() {
                                continue;
                            }
                            let peak = points.iter().copied().fold(0.0f32, f32::max).max(0.01);
                            let col =
                                d2d_rgba(palette_colors[series_idx % palette_colors.len()], 1.0);
                            let px = |i: usize| {
                                pad_f
                                    + if points.len() == 1 {
                                        content_wf
                                    } else {
                                        i as f32 * content_wf / (points.len() - 1) as f32
                                    }
                            };
                            let py =
                                |v: f32| plot_bot - (v / peak).clamp(0.0, 1.0) * (plot_h - sf(3.0));
                            for i in 1..points.len() {
                                d2d.draw_line(
                                    px(i - 1),
                                    py(points[i - 1]),
                                    px(i),
                                    py(points[i]),
                                    col,
                                    sf(1.6).max(1.2),
                                );
                            }
                        }

                        let mut legend_x = pad_f;
                        for (i, (name, _)) in series.iter().enumerate().take(3) {
                            let swatch = d2d_rgba(palette_colors[i % palette_colors.len()], 1.0);
                            d2d.fill_rrect(
                                legend_x,
                                plot_bot + sf(5.0),
                                legend_x + sf(8.0),
                                plot_bot + sf(11.0),
                                sf(2.0),
                                swatch,
                            );
                            legend_x += sf(12.0);
                            d2d.draw_text(
                                name,
                                d2d_rect(
                                    legend_x,
                                    plot_bot + sf(2.0),
                                    (legend_x + sf(80.0)).min(pad_f + content_wf),
                                    plot_bot + sf(16.0),
                                ),
                                sf(10.5),
                                DWRITE_FONT_WEIGHT_MEDIUM,
                                TextAlign::Left,
                                muted_fg,
                                false,
                            );
                            legend_x += sf(86.0);
                            if legend_x >= pad_f + content_wf {
                                break;
                            }
                        }
                        y += s(80.0);
                    }
                    FlyoutBlock::Image {
                        glyph,
                        path,
                        size_dips,
                    } => {
                        let sz = s(image_size_dips(*size_dips));
                        let szf = sz as f32;
                        d2d.fill_rrect_bordered(
                            pad_f,
                            yf,
                            pad_f + szf,
                            yf + szf,
                            card_r,
                            card_bg,
                            subtle_border,
                            1.0,
                        );
                        if let Some(path) = path.as_deref().filter(|p| !p.is_empty()) {
                            d2d.draw_cover_image(path, d2d_rect(pad_f, yf, pad_f + szf, yf + szf));
                        } else {
                            let ch = char::from_u32(u32::from(*glyph)).unwrap_or('\u{E9AA}');
                            d2d.draw_glyph(
                                ch,
                                d2d_rect(pad_f, yf, pad_f + szf, yf + szf),
                                szf * 0.56,
                                text_fg,
                            );
                        }
                        y += sz + s(8.0);
                    }
                    FlyoutBlock::IconBar { buttons } => {
                        if !buttons.is_empty() {
                            let cols = buttons.len().min(4) as i32;
                            let gap = s(6.0);
                            let bw = ((content_w - gap * (cols - 1)) / cols).max(s(44.0));
                            let bwf = bw as f32;
                            let bh = s(48.0);
                            let bhf = bh as f32;
                            for (i, (id, label, glyph, active)) in buttons.iter().enumerate() {
                                let col = (i as i32) % cols;
                                let row = (i as i32) / cols;
                                let left = (pad + col * (bw + gap)) as f32;
                                let top = (y + row * (bh + gap)) as f32;
                                let hot = hovered_id == Some(*id) || focused_id == Some(*id);
                                let (fill, bd, icon_fg) = if *active {
                                    (sel_fill, sel_stroke, sel_fg)
                                } else if hot {
                                    (hover_bg, border_col, text_fg)
                                } else {
                                    (card_bg, subtle_border, muted_fg)
                                };
                                d2d.fill_rrect_bordered(
                                    left,
                                    top,
                                    left + bwf,
                                    top + bhf,
                                    pill_r,
                                    fill,
                                    bd,
                                    1.0,
                                );
                                let ch = char::from_u32(u32::from(*glyph)).unwrap_or('\u{E713}');
                                d2d.draw_glyph(
                                    ch,
                                    d2d_rect(left, top + sf(4.0), left + bwf, top + sf(28.0)),
                                    sf(14.5),
                                    icon_fg,
                                );
                                d2d.draw_text(
                                    label,
                                    d2d_rect(
                                        left + sf(4.0),
                                        top + sf(28.0),
                                        left + bwf - sf(4.0),
                                        top + bhf - sf(4.0),
                                    ),
                                    sf(10.0),
                                    DWRITE_FONT_WEIGHT_MEDIUM,
                                    TextAlign::Center,
                                    if hot || *active { text_fg } else { muted_fg },
                                    false,
                                );
                            }
                            let rows = buttons.len().div_ceil(cols as usize) as i32;
                            y += rows * bh + (rows - 1).max(0) * gap + s(6.0);
                        }
                    }
                    FlyoutBlock::Separator => {
                        let sy = yf + sf(7.0);
                        d2d.draw_line(pad_f, sy, pad_f + content_wf, sy, sep_col, 1.0);
                        y += s(14.0);
                    }
                    FlyoutBlock::Note(text) => {
                        let h = s(24.0);
                        d2d.draw_text(
                            text,
                            d2d_rect(pad_f, yf, pad_f + content_wf, yf + h as f32),
                            sf(11.5),
                            DWRITE_FONT_WEIGHT_NORMAL,
                            TextAlign::Left,
                            muted_fg,
                            false,
                        );
                        y += h;
                    }
                }
            }
            if let Some(index) = keyboard_index
                && let Some(t) = targets.get(index)
            {
                d2d.stroke_rrect(
                    t.rect.left as f32 + sf(1.0),
                    (t.rect.top - scroll_y) as f32 + sf(1.0),
                    t.rect.right as f32 - sf(1.0),
                    (t.rect.bottom - scroll_y) as f32 - sf(1.0),
                    pill_r,
                    accent,
                    1.5,
                );
            }
            unsafe {
                d2d.target().PopAxisAlignedClip();
            }

            // Thin anti-aliased scrollbar thumb when content overflows.
            if max_s > 0 && content_h_px > 0 {
                let track_h = viewport_h_px as f32;
                // A work-area-capped viewport can be shorter than the 24 DIP minimum
                // thumb; `clamp` panics when min > max.
                let thumb_h =
                    (track_h * track_h / content_h_px as f32).clamp(sf(24.0).min(track_h), track_h);
                let ratio = scroll_y as f32 / max_s as f32;
                let thumb_top = content_top_px as f32 + (track_h - thumb_h) * ratio;
                let sb_w = sf(4.0).max(3.0);
                let sb_left = wf - sf(7.5);
                d2d.fill_rrect(
                    sb_left,
                    thumb_top,
                    sb_left + sb_w,
                    thumb_top + thumb_h,
                    sb_w * 0.5,
                    scroll_col,
                );
            }

            // Non-glass / glass-fallback popups: one crisp border on whole pixels,
            // radius matching Win11 `DWMWCP_ROUND` (DWM's own border is disabled).
            if !use_spatial_glass {
                d2d.stroke_rrect(
                    0.0,
                    0.0,
                    wf,
                    h as f32,
                    sf(8.0),
                    d2d_rgba(border.rgb, 1.0),
                    (border.width_dips * dpi).round(),
                );
            }
        });
    }
}

unsafe extern "system" fn flyout_wnd_proc(
    hwnd: HWND,
    msg: u32,
    wparam: WPARAM,
    lparam: LPARAM,
) -> LRESULT {
    let ptr = unsafe { GetWindowLongPtrW(hwnd, GWLP_USERDATA) as *mut FlyoutState<'_> };
    if ptr.is_null() {
        return unsafe { DefWindowProcW(hwnd, msg, wparam, lparam) };
    }
    let state = unsafe { &mut *ptr };
    match msg {
        // WM_CLOSE must never reach `DefWindowProcW`: that would destroy the HWND
        // while the modal loop still owns a state pointer to it. Commit any open
        // editor and let the close animation end the loop via `state.done`.
        msg if wm_close_action(msg, state.closing) == WmCloseAction::BeginClose => {
            state.commit_editing();
            state.begin_close();
            0
        }
        WM_ERASEBKGND => 1,
        WM_PAINT => {
            state.paint();
            0
        }
        WM_SETCURSOR => {
            let cid = if state.hovered_id.is_some() || state.dragging_slider.is_some() {
                IDC_HAND
            } else {
                IDC_ARROW
            };
            unsafe {
                SetCursor(LoadCursorW(std::ptr::null_mut(), cid));
            }
            1
        }
        WM_MOUSEMOVE => {
            if !state.tracking {
                state.tracking = true;
                let mut tme = TRACKMOUSEEVENT {
                    cbSize: size_of::<TRACKMOUSEEVENT>() as u32,
                    dwFlags: TME_LEAVE,
                    hwndTrack: hwnd,
                    dwHoverTime: 0,
                };
                unsafe {
                    TrackMouseEvent(&mut tme);
                }
            }
            let x = (lparam as u32 & 0xffff) as i16 as i32;
            let y = ((lparam as u32 >> 16) & 0xffff) as i16 as i32;
            if let Some(ref mut glass) = state.spatial_glass {
                glass.set_pointer(x as f32, y as f32);
            }
            if let Some((sid, rect, seek)) = state.dragging_slider {
                let w = (rect.right - rect.left).max(1) as f32;
                let ratio = ((x - rect.left) as f32 / w).clamp(0.0, 1.0);
                let event = if seek {
                    LiveEvent::Seek { id: sid, ratio }
                } else {
                    LiveEvent::Slider { id: sid, ratio }
                };
                state.dispatch_live(event);
                return 0;
            }
            let hit = state.hit_at(x, y).map(|target| target.id);
            if hit != state.hovered_id {
                state.hovered_id = hit;
                unsafe {
                    InvalidateRect(hwnd, std::ptr::null(), 0);
                }
            }
            0
        }
        WM_MOUSELEAVE => {
            state.tracking = false;
            if let Some(ref mut glass) = state.spatial_glass {
                glass.clear_pointer();
            }
            if state.dragging_slider.is_none() && state.hovered_id.take().is_some() {
                unsafe {
                    InvalidateRect(hwnd, std::ptr::null(), 0);
                }
            }
            0
        }
        WM_MOUSEWHEEL => {
            let max_s = state.max_scroll();
            if max_s > 0 {
                let delta = ((wparam >> 16) & 0xffff) as i16;
                let step = (36.0 * state.dpi).round() as i32;
                let next = (state.scroll_y - (i32::from(delta) * step) / 120).clamp(0, max_s);
                if next != state.scroll_y {
                    state.scroll_y = next;
                    if state.keyboard_navigation {
                        state.hovered_id = None;
                    }
                    unsafe {
                        InvalidateRect(hwnd, std::ptr::null(), 0);
                    }
                }
            }
            0
        }
        WM_TIMER if wparam == TIMER_ANIM => {
            state.step_animation();
            0
        }
        WM_TIMER if wparam == TIMER_LIVE => {
            state.dispatch_live_tick();
            0
        }
        WM_APP_LIVE_UPDATE => {
            if let Some(ref handle) = state.live_update {
                handle.clear_pending();
            }
            if live_tick_allowed(state.closing, state.on_live.is_some()) {
                state.dispatch_live(LiveEvent::Update);
            }
            0
        }
        WM_TIMER if wparam == TIMER_DISMISS => {
            if !state.closing
                && !state.suppress_blur
                && !state.stays_open
                && state.dragging_slider.is_none()
                && !external_context_menu_active()
            {
                let mut pt = POINT::default();
                let mut rc = RECT::default();
                let have_geom =
                    unsafe { GetCursorPos(&mut pt) != 0 && GetWindowRect(hwnd, &mut rc) != 0 };
                let buttons = mouse_buttons_mask();
                let newly_pressed = buttons & !state.prev_buttons_mask;
                state.prev_buttons_mask = buttons;
                if have_geom {
                    let inside_flyout =
                        pt.x >= rc.left && pt.x <= rc.right && pt.y >= rc.top && pt.y <= rc.bottom;
                    // Outside click dismiss works reliably even when Windows foreground lock
                    // prevented `SetForegroundWindow` from granting focus to the flyout.
                    if newly_pressed != 0 && !inside_flyout && !external_menu_at(pt) {
                        state.commit_editing();
                        state.begin_close();
                        return 0;
                    }
                    // Hover-opened flyouts also dismiss smoothly after the pointer leaves the
                    // flyout + originating widget corridor for a short grace period.
                    if let Some(anchor) = state.hover_anchor
                        && state.editing.is_none()
                    {
                        let pad = (8.0 * state.dpi).round() as i32;
                        if point_in_hover_safe_zone(pt, rc, anchor, pad) {
                            state.hover_left_since = None;
                        } else {
                            let now = Instant::now();
                            let since = *state.hover_left_since.get_or_insert(now);
                            if now.duration_since(since) >= HOVER_LEAVE_GRACE {
                                state.begin_close();
                                return 0;
                            }
                        }
                    }
                }
            } else {
                state.prev_buttons_mask = mouse_buttons_mask();
                state.hover_left_since = None;
            }
            0
        }
        // The low-level hook only posts coordinates; the class/identity check
        // runs here on the UI thread, where a hung foreign window cannot block
        // system-wide input. Preserves the #32768 retain/close behavior.
        WM_APP_MENU_CANDIDATE => {
            let (x, y) = unpack_point(wparam, lparam);
            let point = POINT { x, y };
            // SAFETY: the lookup/class read runs on the UI thread and is cached
            // for one animation frame; a null point hit yields an empty class
            // and is treated as an ordinary click.
            if external_menu_at(point) {
                EXTERNAL_MENU_ACTIVE.store(true, Ordering::Release);
                // The native menu owns focus now; keep the flyout alive through
                // the resulting WM_KILLFOCUS instead of dismissing underneath it.
                state.stays_open = stays_open_for_external_menu(true, state.base_stays_open);
            } else if EXTERNAL_MENU_ACTIVE.swap(false, Ordering::AcqRel) {
                state.stays_open = stays_open_for_external_menu(false, state.base_stays_open);
                state.begin_close();
            }
            0
        }
        WM_LBUTTONDOWN => {
            if !state.closing {
                let x = (lparam as u32 & 0xffff) as i16 as i32;
                let y = ((lparam as u32 >> 16) & 0xffff) as i16 as i32;
                if let Some(target) = state.hit_at(x, y) {
                    if target.is_text {
                        // C2 TextInput: clicking the field starts in-place editing.
                        if state
                            .editing
                            .as_ref()
                            .is_none_or(|(id, _)| *id != target.id)
                        {
                            state.commit_editing();
                        }
                        state.begin_editing(target.id);
                    } else if (target.is_slider || target.is_seek) && state.on_live.is_some() {
                        unsafe {
                            SetCapture(hwnd);
                        }
                        state.dragging_slider = Some((target.id, target.rect, target.is_seek));
                        let w = (target.rect.right - target.rect.left).max(1) as f32;
                        let ratio = ((x - target.rect.left) as f32 / w).clamp(0.0, 1.0);
                        let event = if target.is_seek {
                            LiveEvent::Seek {
                                id: target.id,
                                ratio,
                            }
                        } else {
                            LiveEvent::Slider {
                                id: target.id,
                                ratio,
                            }
                        };
                        state.dispatch_live(event);
                    } else if state.editing.is_some() {
                        state.commit_editing();
                    }
                } else if state.editing.is_some() {
                    state.commit_editing();
                }
            }
            0
        }
        WM_LBUTTONUP => {
            if state.dragging_slider.take().is_some() {
                unsafe {
                    let _ = ReleaseCapture();
                }
                return 0;
            }
            if !state.closing {
                let x = (lparam as u32 & 0xffff) as i16 as i32;
                let y = ((lparam as u32 >> 16) & 0xffff) as i16 as i32;
                if let Some(target) = state.hit_at(x, y) {
                    if target.is_slider || target.is_text {
                        // Sliders commit on drag; text fields commit on Enter/Escape.
                    } else if state.on_live.is_some() {
                        state.dispatch_live(LiveEvent::Click(target.id));
                    } else {
                        state.selected = Some(FlyoutAction::Primary(target.id));
                        state.begin_close();
                    }
                } else if !state.keyboard_navigation && state.on_live.is_none() {
                    state.begin_close();
                }
            }
            0
        }
        WM_RBUTTONUP => {
            if !state.closing {
                let x = (lparam as u32 & 0xffff) as i16 as i32;
                let y = ((lparam as u32 >> 16) & 0xffff) as i16 as i32;
                if let Some(id) = state.hit_at(x, y).map(|target| target.menu_id) {
                    if let Some(cb) = state.on_secondary.as_deref_mut() {
                        state.suppress_blur = true;
                        let should_close = cb(id);
                        state.suppress_blur = false;
                        if should_close {
                            state.begin_close();
                        } else {
                            unsafe {
                                SetForegroundWindow(hwnd);
                            }
                        }
                    } else {
                        state.selected = Some(FlyoutAction::Secondary(id));
                        state.begin_close();
                    }
                }
            }
            0
        }
        WM_KEYDOWN if state.editing.is_some() && !state.closing => {
            match wparam as u16 {
                VK_RETURN => state.commit_editing(),
                VK_ESCAPE => state.cancel_editing(),
                VK_BACK => state.editing_backspace(),
                VK_DELETE => state.editing_delete(),
                VK_TAB => {
                    state.commit_editing();
                    state.focus_next_target(unsafe { GetKeyState(VK_SHIFT as i32) } < 0);
                }
                _ => return unsafe { DefWindowProcW(hwnd, msg, wparam, lparam) },
            }
            0
        }
        WM_CHAR if state.editing.is_some() && !state.closing => {
            if let Some(ch) = char::from_u32(wparam as u32) {
                state.editing_char(ch);
            }
            0
        }
        WM_KEYDOWN if wparam == VK_ESCAPE as usize => {
            state.begin_close();
            0
        }
        WM_KEYDOWN if state.keyboard_navigation && !state.closing => {
            match wparam as u16 {
                VK_TAB => state.focus_next_target(unsafe { GetKeyState(VK_SHIFT as i32) } < 0),
                VK_RETURN | VK_SPACE => {
                    if let Some(index) = state.keyboard_index {
                        state.selected = Some(FlyoutAction::Primary(state.targets[index].id));
                        state.begin_close();
                    }
                }
                _ => return unsafe { DefWindowProcW(hwnd, msg, wparam, lparam) },
            }
            0
        }
        WM_KILLFOCUS => {
            state.commit_editing();
            if !state.suppress_blur && !state.stays_open && !external_context_menu_active() {
                state.begin_close();
            }
            0
        }
        WM_ACTIVATE if (wparam & 0xffff) == 0 => {
            state.commit_editing();
            if !state.suppress_blur && !state.stays_open && !external_context_menu_active() {
                state.begin_close();
            }
            0
        }
        _ => unsafe { DefWindowProcW(hwnd, msg, wparam, lparam) },
    }
}

#[cfg(test)]
mod tests {
    use super::{
        DT_END_ELLIPSIS, DT_LEFT, DT_SINGLELINE, DT_VCENTER, Duration, FLYOUT_CLASS_NAME,
        FLYOUT_CLASS_STYLE, FlyoutBlock, FlyoutEdge, FlyoutOptions, FlyoutSpec, Instant, LiveEvent,
        LiveOutcome, MENU_CLASS_CACHE_TTL, MENU_CLASS_PROBE, MenuClassProbe, POINT, RECT,
        REOPEN_SUPPRESSION_WINDOW, WM_APP_MENU_CANDIDATE, WmCloseAction, anim_interval_for_fps,
        app_row_targets, blend_rgb, calendar_leading_blanks, calendar_weekday_order, draw_gdi_text,
        flyout_available_height, flyout_origin, format_clock, is_native_menu_class, layout_flyout,
        live_tick_allowed, live_tick_tag, pack_point, post_menu_candidate,
        preserve_calendar_navigation, pt_in_rect, record_closed, resolve_live_outcome,
        should_suppress_reopen, stays_open_for_external_menu, suppression_window_hit, unpack_point,
        weekday_short_label, wm_close_action,
    };
    use windows_sys::Win32::Graphics::Gdi::{CreateCompatibleDC, DeleteDC};
    use windows_sys::Win32::UI::WindowsAndMessaging::{WM_CLOSE, WM_KEYDOWN, WM_PAINT};

    #[test]
    fn draw_gdi_text_ignores_empty_strings_without_crashing() {
        unsafe {
            let hdc = CreateCompatibleDC(std::ptr::null_mut());
            assert!(!hdc.is_null());
            let mut rc = RECT {
                left: 0,
                top: 0,
                right: 120,
                bottom: 24,
            };
            draw_gdi_text(
                hdc,
                "",
                &mut rc,
                DT_LEFT | DT_VCENTER | DT_SINGLELINE | DT_END_ELLIPSIS,
            );
            draw_gdi_text(
                hdc,
                "Today",
                &mut rc,
                DT_LEFT | DT_VCENTER | DT_SINGLELINE | DT_END_ELLIPSIS,
            );
            DeleteDC(hdc);
        }
    }

    #[test]
    fn background_app_targets_do_not_overlap_at_supported_scales() {
        for dpi in [1.0, 1.25, 1.5, 2.0] {
            let right = (300.0 * dpi) as i32;
            let [app, pin, close] = app_row_targets(12, right, 40, dpi);

            assert!(pt_in_rect(&app, pin.left - 1, 40));
            assert!(!pt_in_rect(&app, pin.left, 40));
            assert!(pt_in_rect(&pin, pin.left, 40));
            assert!(!pt_in_rect(&pin, close.left, 40));
            assert!(pt_in_rect(&close, close.left, 40));
            assert!(!pt_in_rect(&close, right, 40));
            assert!(!pt_in_rect(&close, close.left, close.bottom));
        }
    }

    /// C2 field names/types are part of the WS-E↔WS-C contract; keep them constructible.
    #[test]
    fn c2_blocks_have_the_agreed_field_signatures() {
        let blocks = vec![
            FlyoutBlock::SeekSlider {
                id: 11,
                label: "Position".to_owned(),
                position_secs: 42,
                duration_secs: 180,
                ratio: 42.0 / 180.0,
            },
            FlyoutBlock::TextInput {
                id: 12,
                label: "Minutes".to_owned(),
                value: "5".to_owned(),
                numeric: true,
                min: 1.0,
                max: 1440.0,
            },
            FlyoutBlock::Chart {
                label: "Network".to_owned(),
                series: vec![("Down".to_owned(), vec![0.2, 0.7, 0.4])],
                accent: (0x2d, 0xd4, 0xbf),
            },
            FlyoutBlock::Image {
                glyph: 0xE9C4,
                path: None,
                size_dips: 48.0,
            },
            FlyoutBlock::IconBar {
                buttons: vec![(13, "Reset".to_owned(), 0xE72C, false)],
            },
        ];
        let spec = FlyoutSpec {
            title: "C2".to_owned(),
            subtitle: None,
            width_dips: 320.0,
            blocks,
        };
        let (top, height, targets) = layout_flyout(&spec, 320, 1.0);
        assert!(top > 0 && height > 0);
        // SeekSlider is draggable, TextInput focusable, IconBar has one button.
        assert!(
            targets
                .iter()
                .any(|t| t.id == 11 && t.is_slider && t.is_seek)
        );
        assert!(targets.iter().any(|t| t.id == 12 && t.is_text));
        assert!(
            targets
                .iter()
                .any(|t| t.id == 13 && !t.is_slider && !t.is_text)
        );

        // LiveEvent owns text, so the enum cannot be Copy.
        let event = LiveEvent::TextCommit {
            id: 12,
            text: "30".to_owned(),
        };
        match event {
            LiveEvent::TextCommit { id, text } => {
                assert_eq!(id, 12);
                assert_eq!(text, "30");
            }
            _ => panic!("wrong event"),
        }
        assert_eq!(format_clock(125), "2:05");
    }

    #[test]
    fn flyout_class_has_no_rectangular_drop_shadow() {
        use windows_sys::Win32::UI::WindowsAndMessaging::CS_DROPSHADOW;
        assert_eq!(FLYOUT_CLASS_STYLE & CS_DROPSHADOW, 0);
        assert_eq!(FLYOUT_CLASS_NAME, "OpenTaskBarFlyout");
        let code = include_str!("flyout.rs");
        let register = code
            .find("let wc = WNDCLASSEXW {")
            .expect("flyout class registration");
        let window: String = code[register..].chars().take(400).collect();
        assert!(window.contains("style: FLYOUT_CLASS_STYLE"));
    }

    #[test]
    fn flyout_options_defaults_match_the_shared_dock_flyout() {
        let options = FlyoutOptions::default();
        assert!(!options.stays_open);
        assert!(!options.retain_for_external_context_menu);
        assert_eq!(options.edge, FlyoutEdge::Bottom);
        assert_eq!(options.shadow_rings, 4);
        // Presets for the WS-6 tray / folder-stack call sites.
        assert!(super::FlyoutOptions::tray().retain_for_external_context_menu);
        assert!(!super::FlyoutOptions::tray().stays_open);
        assert!(!super::FlyoutOptions::stack().stays_open);
        assert_eq!(
            super::FlyoutOptions::default()
                .with_edge(FlyoutEdge::Left)
                .edge,
            FlyoutEdge::Left
        );
        // Only an explicit edge anchor overrides a journaled position.
        assert!(!options.uses_anchor_position());
        assert!(!super::FlyoutOptions::tray().uses_anchor_position());
        let top = super::FlyoutOptions::tray().with_edge(FlyoutEdge::Top);
        assert_eq!(top.edge, FlyoutEdge::Top);
        assert!(top.uses_anchor_position());
        assert!(top.retain_for_external_context_menu);
        assert_eq!(blend_rgb(0x000000, 0xffffff, 0.5), 0x808080);
    }

    #[test]
    fn flyout_placement_stays_inside_the_work_area() {
        // 1920x1080 monitor whose taskbar reserves the bottom 48 px.
        let work = RECT {
            left: 0,
            top: 0,
            right: 1920,
            bottom: 1032,
        };
        let margin = 10;

        // Top edge (Dynamic Island): opens directly below the anchor.
        let h = flyout_available_height(FlyoutEdge::Top, true, &work, 60, margin, 160);
        assert_eq!(h, 1032 - margin - 60);
        assert_eq!(
            flyout_origin(FlyoutEdge::Top, &work, 960, 60, 300, 400, margin),
            (810, 60)
        );

        // Bottom edge, anchored: only the room above the anchor is usable.
        let h = flyout_available_height(FlyoutEdge::Bottom, true, &work, 700, margin, 160);
        assert_eq!(h, 700 - margin);
        let (_, y) = flyout_origin(FlyoutEdge::Bottom, &work, 960, 700, 300, h, margin);
        assert_eq!(y + h, 700);

        // Unanchored: the whole work area, and the bottom edge never passes rcWork.
        let h = flyout_available_height(FlyoutEdge::Bottom, false, &work, 1080, margin, 160);
        assert_eq!(h, 1032 - 2 * margin);
        let (x, y) = flyout_origin(FlyoutEdge::Bottom, &work, 1910, 1080, 300, h, margin);
        assert!(y + h <= work.bottom - margin, "{y}+{h}");
        assert!(x + 300 <= work.right - margin);

        // A wrong-side anchor keeps a usable minimum instead of a 1 px popup.
        assert_eq!(
            flyout_available_height(FlyoutEdge::Top, true, &work, 1030, margin, 160),
            160
        );

        // Negative virtual-desktop origin (monitor left of the primary).
        let left_work = RECT {
            left: -1920,
            top: 0,
            right: 0,
            bottom: 1032,
        };
        let (x, _) = flyout_origin(FlyoutEdge::Bottom, &left_work, -5, 1000, 300, 200, margin);
        assert_eq!(x, -300 - margin);
    }

    #[test]
    fn default_anim_interval_is_16ms() {
        // Auto mode: 16 ms ticks (~60 Hz) rather than 8 ms (125 Hz).
        assert_eq!(anim_interval_for_fps(0), 16);
        assert_eq!(anim_interval_for_fps(60), 16);
        assert_eq!(anim_interval_for_fps(120), 8);
        assert_eq!(anim_interval_for_fps(15), 66);
        assert_eq!(anim_interval_for_fps(1), 66); // clamped to 15 FPS
    }

    #[test]
    fn deferred_menu_candidate_carries_exact_screen_point() {
        for (x, y) in [(0, 0), (1920, 1080), (-12, 34), (i32::MIN, i32::MAX)] {
            let (wparam, lparam) = pack_point(x, y);
            assert_eq!(unpack_point(wparam, lparam), (x, y));
        }

        let mut calls = Vec::new();
        post_menu_candidate(0x1234, POINT { x: -12, y: 34 }, |hwnd, msg, x, y| {
            calls.push((hwnd, msg, unpack_point(x, y)));
        });
        assert_eq!(
            calls,
            vec![(0x1234 as super::HWND, WM_APP_MENU_CANDIDATE, (-12, 34))]
        );
        assert_ne!(WM_APP_MENU_CANDIDATE, 0x8001); // distinct from retired MENU_DONE
        assert_ne!(WM_APP_MENU_CANDIDATE, 0x8002); // distinct from dock WM_APP_ICONS

        calls.clear();
        post_menu_candidate(0, POINT { x: 5, y: 6 }, |hwnd, msg, x, y| {
            calls.push((hwnd, msg, unpack_point(x, y)));
        });
        assert!(calls.is_empty());
    }

    #[test]
    fn native_menu_class_check_keeps_hash_32768_discrimination() {
        let menu: Vec<u16> = "#32768".encode_utf16().collect();
        let menu_variant: Vec<u16> = "#32768-NativeMenu".encode_utf16().collect();
        let tray: Vec<u16> = "Shell_TrayWnd".encode_utf16().collect();
        let flyout: Vec<u16> = "OpenTaskBarFlyout".encode_utf16().collect();
        assert!(is_native_menu_class(&menu));
        assert!(is_native_menu_class(&menu_variant));
        assert!(!is_native_menu_class(&tray));
        assert!(!is_native_menu_class(&flyout));
        assert!(!is_native_menu_class(&[]));
    }

    #[test]
    fn menu_class_probe_expires_after_one_animation_frame() {
        let point = POINT { x: 42, y: 84 };
        let now = std::time::Instant::now();
        let fresh = MenuClassProbe {
            point,
            is_menu: true,
            at: Some(now),
        };
        assert!(fresh.is_fresh(point, now + MENU_CLASS_CACHE_TTL / 2));
        assert!(!fresh.is_fresh(point, now + MENU_CLASS_CACHE_TTL * 2));
        assert!(!fresh.is_fresh(POINT { x: 43, y: 84 }, now + MENU_CLASS_CACHE_TTL / 2));
        assert!(!MenuClassProbe::empty().is_fresh(point, now));
        MENU_CLASS_PROBE.with(|cell| *cell.borrow_mut() = fresh);
        assert!(MENU_CLASS_PROBE.with(|cell| {
            cell.borrow()
                .is_fresh(point, now + MENU_CLASS_CACHE_TTL / 2)
        }));
    }

    #[test]
    fn hover_safe_zone_covers_flyout_anchor_and_corridor() {
        let flyout = RECT {
            left: 400,
            top: 600,
            right: 700,
            bottom: 920,
        };
        let anchor = RECT {
            left: 500,
            top: 940,
            right: 600,
            bottom: 990,
        };
        // Inside flyout, inside anchor, and in the vertical corridor between them.
        assert!(super::point_in_hover_safe_zone(
            POINT { x: 550, y: 750 },
            flyout,
            anchor,
            8
        ));
        assert!(super::point_in_hover_safe_zone(
            POINT { x: 550, y: 960 },
            flyout,
            anchor,
            8
        ));
        assert!(super::point_in_hover_safe_zone(
            POINT { x: 550, y: 930 },
            flyout,
            anchor,
            8
        ));
        // Clearly outside triggers hover-leave dismissal.
        assert!(!super::point_in_hover_safe_zone(
            POINT { x: 100, y: 500 },
            flyout,
            anchor,
            8
        ));
    }

    #[test]
    fn wm_close_commits_and_begins_close_but_is_not_double_applied() {
        assert_eq!(wm_close_action(WM_CLOSE, false), WmCloseAction::BeginClose);
        assert_eq!(wm_close_action(WM_CLOSE, true), WmCloseAction::Ignore);
        assert_eq!(wm_close_action(WM_PAINT, false), WmCloseAction::Ignore);
        assert_eq!(wm_close_action(WM_KEYDOWN, false), WmCloseAction::Ignore);
    }

    #[test]
    fn suppression_window_is_350ms_and_one_shot_per_target() {
        let now = Instant::now();
        assert!(suppression_window_hit(
            Some(now - REOPEN_SUPPRESSION_WINDOW / 2),
            now
        ));
        assert!(!suppression_window_hit(
            Some(now - REOPEN_SUPPRESSION_WINDOW - Duration::from_millis(1)),
            now
        ));
        assert!(!suppression_window_hit(None, now));

        let a = "sweep-flyout-suppress-a";
        let b = "sweep-flyout-suppress-b";
        assert!(!should_suppress_reopen(a));
        record_closed(a);
        assert!(
            should_suppress_reopen(a),
            "first close suppresses same target"
        );
        assert!(!should_suppress_reopen(a), "entry is consumed exactly once");
        assert!(!should_suppress_reopen(b), "other target is unaffected");
        record_closed(b);
        assert!(should_suppress_reopen(b));
        assert!(!should_suppress_reopen(a));
    }

    #[test]
    fn external_menu_holds_flyout_open_until_it_closes() {
        assert!(stays_open_for_external_menu(true, false));
        assert!(stays_open_for_external_menu(true, true));
        assert!(stays_open_for_external_menu(false, true));
        assert!(!stays_open_for_external_menu(false, false));
    }

    #[test]
    fn stack_flyout_allows_outside_dismiss_and_suppresses_immediate_reopen() {
        let stack_opts = FlyoutOptions::stack();
        assert!(!stack_opts.stays_open);
        assert!(!stays_open_for_external_menu(false, stack_opts.stays_open));

        let tag = "stack-toggle-close-test";
        assert!(!should_suppress_reopen(tag));
        record_closed(tag);
        assert!(
            should_suppress_reopen(tag),
            "closing a stack flyout must immediately suppress reopen for the same tag"
        );
        assert!(!should_suppress_reopen(tag));
    }

    #[test]
    fn redesigned_flyout_blocks_layout_without_truncation() {
        let spec = FlyoutSpec {
            title: "Redesign".to_owned(),
            subtitle: Some("Sub".to_owned()),
            width_dips: 332.0,
            blocks: vec![
                FlyoutBlock::HeroHeader {
                    glyph: 0xE706,
                    glyph_color: (0xfb, 0xbf, 0x24),
                    title: "Istanbul".to_owned(),
                    subtitle: Some("Clear".to_owned()),
                    right_value: Some("21°C".to_owned()),
                },
                FlyoutBlock::IconToggleGrid(vec![
                    (1, 0xE701, "Wi-Fi".to_owned(), "On".to_owned(), true),
                    (2, 0xE702, "Bluetooth".to_owned(), "Off".to_owned(), false),
                ]),
                FlyoutBlock::GlyphStatRow {
                    glyph: 0xE753,
                    glyph_color: (0xa2, 0xc0, 0xff),
                    label: "Tomorrow".to_owned(),
                    detail: Some("Cloudy".to_owned()),
                    value: "18° / 11°".to_owned(),
                },
                FlyoutBlock::GlyphActionRow {
                    id: 50,
                    glyph: 0xE8B7,
                    glyph_color: (0xfb, 0xbf, 0x24),
                    title: "Downloads".to_owned(),
                    subtitle: Some("C:\\Users\\Downloads".to_owned()),
                    badge: None,
                },
                FlyoutBlock::IconBar {
                    buttons: (100..108)
                        .map(|id| (id, format!("Btn {id}"), 0xE713, false))
                        .collect(),
                },
            ],
        };
        let (top, height, targets) = layout_flyout(&spec, 332, 1.0);
        assert!(top > 0 && height > 200);
        // All 8 IconBar buttons must have hit targets across 2 rows of 4 (zero truncation).
        for id in 100..108 {
            assert!(
                targets.iter().any(|t| t.id == id),
                "missing IconBar target {id}"
            );
        }
        assert!(targets.iter().any(|t| t.id == 1));
        assert!(targets.iter().any(|t| t.id == 2));
        assert!(targets.iter().any(|t| t.id == 50));
        assert_eq!(super::is_single_pua_glyph("\u{E7F5}"), Some('\u{E7F5}'));
        assert_eq!(super::is_single_pua_glyph("TR"), None);
    }

    #[test]
    fn live_timer_is_separate_from_animation_and_dismiss() {
        assert_ne!(super::TIMER_LIVE, super::TIMER_ANIM);
        assert_ne!(super::TIMER_LIVE, super::TIMER_DISMISS);
        assert_eq!(super::LIVE_TICK_MS, 1000);
        // A live flyout ticks; a plain flyout and a closing flyout never do.
        assert!(live_tick_allowed(false, true));
        assert!(!live_tick_allowed(true, true));
        assert!(!live_tick_allowed(false, false));
        for tag in [
            "clock",
            "stats",
            "focus",
            "stopwatch",
            "countdown",
            "netspeed",
            "media",
        ] {
            assert!(live_tick_tag(tag), "{tag} must tick");
        }
        // Audio/quick-settings callbacks close on unknown events: ticking
        // them would dismiss the flyout after one second.
        for tag in ["audio", "quicksettings", "bluetoothbattery", "tray"] {
            assert!(!live_tick_tag(tag), "{tag} must not tick");
        }

        // Behavioural check of the exact path the timer drives: an Update
        // outcome produces a new spec, Close produces none.
        let base = FlyoutSpec {
            title: "clock".to_owned(),
            subtitle: None,
            width_dips: 300.0,
            blocks: Vec::new(),
        };
        let rebuilt = FlyoutSpec {
            title: "clock".to_owned(),
            subtitle: None,
            width_dips: 300.0,
            blocks: vec![FlyoutBlock::Hero {
                value: "12:00".to_owned(),
                caption: None,
                centered: false,
                large: false,
                right_meta: None,
            }],
        };
        let outcome = resolve_live_outcome(&base, LiveOutcome::Update(rebuilt), true);
        assert!(
            outcome
                .is_some_and(|spec| matches!(spec.blocks.first(), Some(FlyoutBlock::Hero { .. })))
        );
        assert!(resolve_live_outcome(&base, LiveOutcome::Close, true).is_none());
    }

    #[test]
    fn live_update_event_rebuilds_the_widget_spec() {
        let spec = FlyoutSpec {
            title: "clock".to_owned(),
            subtitle: None,
            width_dips: 300.0,
            blocks: vec![FlyoutBlock::Hero {
                value: "12:34:56".to_owned(),
                caption: None,
                centered: false,
                large: false,
                right_meta: None,
            }],
        };
        let next = spec.clone();
        let respond = |event: LiveEvent| match event {
            LiveEvent::Update => LiveOutcome::Update(next.clone()),
            _ => LiveOutcome::Close,
        };
        match respond(LiveEvent::Update) {
            LiveOutcome::Update(rebuilt) => assert_eq!(rebuilt.title, "clock"),
            LiveOutcome::Close => panic!("live tick must request a spec update"),
        }
    }

    #[test]
    fn live_tick_keeps_the_navigated_calendar_month() {
        let calendar =
            |year: i32, month: u32, selected: Option<(i32, u32, u32)>| FlyoutBlock::CalendarGrid {
                year,
                month,
                today: (2026, 11, 27),
                selected,
                prev_year_id: 1,
                prev_month_id: 2,
                title_id: 3,
                next_month_id: 4,
                next_year_id: 5,
                day_id_base: 100,
            };
        let current = FlyoutSpec {
            title: String::new(),
            subtitle: None,
            width_dips: 316.0,
            blocks: vec![calendar(2025, 3, Some((2025, 3, 14)))],
        };
        let mut next = FlyoutSpec {
            title: String::new(),
            subtitle: None,
            width_dips: 316.0,
            blocks: vec![calendar(2026, 11, None)],
        };
        preserve_calendar_navigation(&current, &mut next);
        let FlyoutBlock::CalendarGrid {
            year,
            month,
            selected,
            ..
        } = &next.blocks[0]
        else {
            panic!("calendar block missing");
        };
        assert_eq!((*year, *month, *selected), (2025, 3, Some((2025, 3, 14))));
    }

    #[test]
    fn calendar_headers_and_leading_blanks_follow_the_culture() {
        assert_eq!(calendar_weekday_order(0), [0, 1, 2, 3, 4, 5, 6]);
        assert_eq!(calendar_weekday_order(1), [1, 2, 3, 4, 5, 6, 0]);
        assert_eq!(calendar_weekday_order(6), [6, 0, 1, 2, 3, 4, 5]);
        // 2024-01-01 was a Monday (.NET index 1).
        assert_eq!(calendar_leading_blanks(1, 1), 0);
        assert_eq!(calendar_leading_blanks(0, 1), 1);
        assert_eq!(calendar_leading_blanks(6, 1), 2);
        assert!(!weekday_short_label(1).is_empty());
        assert!(weekday_short_label(1).chars().count() <= 3);

        // The culture setting actually reaches the header order and labels.
        let first = super::telemetry::first_day_of_week();
        assert!(first < 7);
        let order = calendar_weekday_order(first);
        assert_eq!(order[0], first);
        assert_eq!(order[6], (first + 6) % 7);
        let expected = super::telemetry::weekday_short_name(first);
        assert_eq!(weekday_short_label(first), expected);
    }

    #[test]
    fn turkish_weekday_short_labels_are_distinct() {
        let labels: Vec<String> = (0..7)
            .map(|day| super::weekday_short_label_for_lang(day, "tr-TR"))
            .collect();
        assert_eq!(labels, ["Paz", "Pzt", "Sal", "Çar", "Per", "Cum", "Cmt"]);
        let unique: std::collections::HashSet<&str> = labels.iter().map(String::as_str).collect();
        assert_eq!(unique.len(), 7);
    }

    #[test]
    fn viewport_culling_skips_offscreen_rows_and_keeps_visible_rows() {
        // Clip viewport spans [100, 300).
        let (clip_top, clip_bottom) = (100, 300);
        // Completely above viewport (including exact touch at bottom == clip_top).
        assert!(!super::block_intersects_viewport(
            0,
            60,
            clip_top,
            clip_bottom
        ));
        assert!(!super::block_intersects_viewport(
            60,
            100,
            clip_top,
            clip_bottom
        ));
        // Completely below viewport (including exact touch at top == clip_bottom).
        assert!(!super::block_intersects_viewport(
            300,
            340,
            clip_top,
            clip_bottom
        ));
        assert!(!super::block_intersects_viewport(
            350,
            400,
            clip_top,
            clip_bottom
        ));
        // Partially overlapping top, fully inside, and partially overlapping bottom.
        assert!(super::block_intersects_viewport(
            80,
            110,
            clip_top,
            clip_bottom
        ));
        assert!(super::block_intersects_viewport(
            120,
            160,
            clip_top,
            clip_bottom
        ));
        assert!(super::block_intersects_viewport(
            280,
            320,
            clip_top,
            clip_bottom
        ));
        // Spanning the entire viewport.
        assert!(super::block_intersects_viewport(
            50,
            350,
            clip_top,
            clip_bottom
        ));

        // Verify `block_height_px` stays in exact lockstep with `layout_flyout` across a
        // 40-row scrollable folder stack and that culling skips offscreen rows cleanly.
        let blocks: Vec<FlyoutBlock> = (0..40u32)
            .map(|i| FlyoutBlock::ActionRow {
                id: 100 + i,
                title: format!("Item {i}"),
                subtitle: (i < 5).then(|| "Folder".to_owned()),
                badge: None,
                icon: None,
                destructive: false,
            })
            .collect();
        let spec = FlyoutSpec {
            title: "Downloads".to_owned(),
            subtitle: Some(r"C:\Users\Me\Downloads".to_owned()),
            width_dips: 320.0,
            blocks,
        };
        let (content_top, content_h, _) = layout_flyout(&spec, 320, 1.0);
        let summed_h: i32 = spec
            .blocks
            .iter()
            .map(|b| super::block_height_px(b, 1.0))
            .sum();
        assert_eq!(summed_h, content_h);

        let viewport_h = 300;
        let scroll_y = 400;
        let mut y = content_top - scroll_y;
        let mut visible = 0usize;
        for block in &spec.blocks {
            let h = super::block_height_px(block, 1.0);
            if super::block_intersects_viewport(y, y + h, content_top, content_top + viewport_h) {
                visible += 1;
            }
            y += h;
        }
        assert!(
            visible > 0 && visible < 12,
            "expected ~8-10 visible rows, got {visible}"
        );
    }
}
