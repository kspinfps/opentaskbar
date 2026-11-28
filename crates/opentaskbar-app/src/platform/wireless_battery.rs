//! Kablosuz/Bluetooth pil veri katmanı (Rust implementation of `Services/Hardware/WirelessBattery`).
//!
//! Tek dosyada, dış bağımlılık gerektirmeyen ham Win32 FFI (setupapi.dll, hid.dll,
//! kernel32.dll, advapi32.dll) ve dinamik olarak yüklenen xinput1_4.dll kullanılır.
//! Hiçbir sağlayıcı cihazı özel olarak kilitlemez; HID I/O sınırlı overlapped
//! işlemlerle yapılır. Her sağlayıcı ayrı bir iş parçacığında çalışır ve 1.2 sn
//! içinde sonuç vermezse atlanır. 30 dakikadan eski okumalar önbellekten silinir.
//!
//! Gerçek protokoller: XInput, standart HID pil capacity/usages, Logitech HID++ 2.0,
//! Razer 90 bayt feature report, SteelSeries HID (Nova/Arctis/fare), BLE GATT
//! kayıt defteri önbelleği (WinRT yerine fallback), ayrıca vendor HID pil usages
//! (ASUS ROG, Corsair, HyperX, gamepad, ATK/VXE, 2.4G klavye). BLE GATT okuması
//! gerçek WinRT `BluetoothLEDevice` + Battery Service (0x180F / 0x2A19) üzerinden
//! yapılır; WinRT başlatılamazsa kayıt defteri önbelleğine düşülür.

use std::collections::{HashMap, HashSet};
use std::ffi::c_void;
use std::io::{Read, Write};
use std::mem::size_of;
use std::net::{TcpStream, ToSocketAddrs};
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, OnceLock, mpsc};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use opentaskbar_core::settings::DockSettings;

// ---------------------------------------------------------------------------
// Ortak API tipleri
// ---------------------------------------------------------------------------

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum BatterySource {
    XInput,
    LogitechHidPlusPlus,
    RazerHid,
    SteelSeriesHid,
    StandardHid,
    BluetoothGatt,
    AsusRogHid,
    CorsairHid,
    GamepadHid,
    HyperXHid,
    CompxAtkVxe,
    Keyboard24gHid,
    // WS-G additions (passive / loopback providers).
    Bluetooth,
    SteelSeriesGg,
    LogitechGHub,
}

/// All providers served by `run_probe`. `read_all` polls every one of them;
/// keeping the list next to the enum prevents new variants from being silently
/// left out of live discovery (they would otherwise only come from disk cache).
pub const ALL_SOURCES: [BatterySource; 15] = [
    BatterySource::SteelSeriesHid,
    BatterySource::LogitechHidPlusPlus,
    BatterySource::RazerHid,
    BatterySource::XInput,
    BatterySource::StandardHid,
    BatterySource::Bluetooth,
    BatterySource::BluetoothGatt,
    BatterySource::AsusRogHid,
    BatterySource::CorsairHid,
    BatterySource::GamepadHid,
    BatterySource::HyperXHid,
    BatterySource::CompxAtkVxe,
    BatterySource::Keyboard24gHid,
    BatterySource::LogitechGHub,
    BatterySource::SteelSeriesGg,
];

impl BatterySource {
    /// Widget etiketi (`WirelessDeviceReading.GetSourceLabel` karşılığı).
    pub fn label(self) -> &'static str {
        match self {
            Self::XInput => "XInput",
            Self::LogitechHidPlusPlus => "HID++",
            Self::RazerHid => "Razer",
            Self::SteelSeriesHid => "SteelSeries",
            Self::StandardHid => "HID",
            Self::BluetoothGatt => "BLE GATT",
            Self::AsusRogHid => "ROG",
            Self::CorsairHid => "Corsair",
            Self::GamepadHid => "Gamepad",
            Self::HyperXHid => "HyperX",
            Self::CompxAtkVxe => "ATK/VXE",
            Self::Keyboard24gHid => "2.4G",
            Self::Bluetooth => "Bluetooth",
            Self::SteelSeriesGg => "SteelSeries",
            Self::LogitechGHub => "HID++",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum WirelessDeviceKind {
    Mouse,
    Keyboard,
    Headphones,
    Gamepad,
    #[default]
    Other,
}

#[derive(Clone, Debug, PartialEq)]
pub struct WirelessReading {
    /// VID:PID:serial tabanlı, sağlayıcı önekli kararlı kimlik.
    pub id: String,
    pub name: String,
    /// 0.0 ..= 1.0 aralığında pil seviyesi.
    pub level_01: f32,
    pub sampled_at: Instant,
    pub is_connected: bool,
    pub is_charging: bool,
    pub source: BatterySource,
    pub stale: bool,
    pub device_kind: WirelessDeviceKind,
    /// Optional secondary / spare dock battery (e.g. SteelSeries Arctis Nova Pro / Arctis Pro).
    pub secondary_level: Option<f32>,
}

impl WirelessReading {
    pub fn level_percent(&self) -> u8 {
        (self.level_01 * 100.0).round().clamp(0.0, 100.0) as u8
    }

    /// `None` when the charge is unknown (`level_01 < 0`, the `-1` cache sentinel); an
    /// unknown level must never be presented as `0%`.
    pub fn level_percent_opt(&self) -> Option<u8> {
        (self.level_01.is_finite() && self.level_01 >= 0.0).then(|| self.level_percent())
    }
}

/// Reads either this port's snake_case cache key or the reference PascalCase
/// `WirelessDeviceReading` property of the same name.
fn cache_field<'a>(
    value: &'a serde_json::Value,
    snake: &str,
    pascal: &str,
) -> Option<&'a serde_json::Value> {
    value.get(snake).or_else(|| value.get(pascal))
}

use opentaskbar_core::{civil_from_days, days_from_civil};

/// Parses the `DateTimeOffset` text reference writes for `SampledAt` into Unix seconds.
/// Fractional seconds are irrelevant at the 30-minute stale threshold.
fn reference_timestamp_secs(text: &str) -> Option<u64> {
    let digits = |at: usize, len: usize| -> Option<i64> { text.get(at..at + len)?.parse().ok() };
    let (year, month, day) = (digits(0, 4)?, digits(5, 2)?, digits(8, 2)?);
    let (hour, minute, second) = (digits(11, 2)?, digits(14, 2)?, digits(17, 2)?);
    let mut secs = days_from_civil(year, month, day) * 86_400 + hour * 3_600 + minute * 60 + second;
    if let Some(offset_at) = text[19..].find(['+', '-']) {
        let sign_at = 19 + offset_at;
        let offset = &text[sign_at..];
        let offset_hour = offset.get(1..3)?.parse::<i64>().ok()?;
        let offset_minute = offset.get(4..6)?.parse::<i64>().ok()?;
        // Local time = UTC + offset, therefore UTC = local - offset.
        if offset.starts_with('-') {
            secs += offset_hour * 3_600 + offset_minute * 60;
        } else {
            secs -= offset_hour * 3_600 + offset_minute * 60;
        }
    }
    u64::try_from(secs).ok()
}

fn unix_secs_to_rfc3339(secs: u64) -> String {
    let secs = secs as i64;
    let (year, month, day) = civil_from_days(secs.div_euclid(86_400));
    let time = secs.rem_euclid(86_400);
    format!(
        "{year:04}-{month:02}-{day:02}T{:02}:{:02}:{:02}Z",
        time / 3600,
        (time % 3600) / 60,
        time % 60
    )
}

/// XInput yalnızca kaba seviye verir; yüzde üretmek yerine arayüz bu metni göstermelidir.
pub fn coarse_level_text(source: BatterySource, level_01: f32) -> Option<&'static str> {
    if source != BatterySource::XInput {
        return None;
    }
    match level_01 {
        x if x <= 0.05 => Some("Empty"),
        x if x <= 0.20 => Some("Low"),
        x if x <= 0.60 => Some("Medium"),
        _ => Some("Full"),
    }
}

const PROVIDER_TIMEOUT: Duration = Duration::from_millis(1200);
const STALE_WINDOW: Duration = Duration::from_secs(30 * 60);

// ---------------------------------------------------------------------------
// Win32 FFI — kernel32 / setupapi / hid / advapi32
// ---------------------------------------------------------------------------

type Handle = isize;
const INVALID_HANDLE: Handle = -1;

#[repr(C)]
#[derive(Clone, Copy, Default)]
struct Guid {
    data1: u32,
    data2: u16,
    data3: u16,
    data4: [u8; 8],
}

const HID_CLASS_GUID: Guid = Guid {
    data1: 0x4D1E_55B2,
    data2: 0xF16F,
    data3: 0x11CF,
    data4: [0x88, 0xCB, 0x00, 0x11, 0x11, 0x00, 0x00, 0x30],
};

#[repr(C)]
#[derive(Clone, Copy)]
struct DevPropKey {
    fmtid: Guid,
    pid: u32,
}

#[repr(C)]
struct SpDevinfoData {
    cb_size: u32,
    class_guid: Guid,
    dev_inst: u32,
    reserved: usize,
}

/// `DEVPKEY_Bluetooth_Battery`: `{104EA319-6EE2-4701-BD47-8DDBF425BBE5}, 2`.
const DEVPKEY_BLUETOOTH_BATTERY: DevPropKey = DevPropKey {
    fmtid: Guid {
        data1: 0x104E_A319,
        data2: 0x6EE2,
        data3: 0x4701,
        data4: [0xBD, 0x47, 0x8D, 0xDB, 0xF4, 0x25, 0xBB, 0xE5],
    },
    pid: 2,
};

/// `DEVPKEY_Device_IsPresent`: `{540B947E-8B40-45BC-A8A2-6A0B894CBDA2}, 5`.
const DEVPKEY_DEVICE_IS_PRESENT: DevPropKey = DevPropKey {
    fmtid: Guid {
        data1: 0x540B_947E,
        data2: 0x8B40,
        data3: 0x45BC,
        data4: [0xA8, 0xA2, 0x6A, 0x0B, 0x89, 0x4C, 0xBD, 0xA2],
    },
    pid: 5,
};

/// `DEVPKEY_Device_IsConnected`: `{83DA6326-97A6-4088-9453-A1923F573B29}, 15`. Unlike
/// `IsPresent` (a paired Bluetooth device stays "present" while switched off), this is the
/// physical link state.
const DEVPKEY_DEVICE_IS_CONNECTED: DevPropKey = DevPropKey {
    fmtid: Guid {
        data1: 0x83DA_6326,
        data2: 0x97A6,
        data3: 0x4088,
        data4: [0x94, 0x53, 0xA1, 0x92, 0x3F, 0x57, 0x3B, 0x29],
    },
    pid: 15,
};

/// `DEVPKEY_Device_ContainerId`: `{8C7ED206-3F8A-4827-B3AB-AE9E1FAEFC6C}, 2` — one GUID per
/// physical device, shared by all of its PnP functions / HID collections.
const DEVPKEY_DEVICE_CONTAINER_ID: DevPropKey = DevPropKey {
    fmtid: Guid {
        data1: 0x8C7E_D206,
        data2: 0x3F8A,
        data3: 0x4827,
        data4: [0xB3, 0xAB, 0xAE, 0x9E, 0x1F, 0xAE, 0xFC, 0x6C],
    },
    pid: 2,
};

/// The container every built-in (non-removable) device shares; it is not a physical identity.
const LOCAL_MACHINE_CONTAINER: &str = "00000000-0000-0000-FFFF-FFFFFFFFFFFF";

/// `DEVPKEY_Device_FriendlyName`: `{A45C254E-DF1C-4EFD-8020-67D146A850E0}, 14`.
const DEVPKEY_DEVICE_FRIENDLY_NAME: DevPropKey = DevPropKey {
    fmtid: Guid {
        data1: 0xA45C_254E,
        data2: 0xDF1C,
        data3: 0x4EFD,
        data4: [0x80, 0x20, 0x67, 0xD1, 0x46, 0xA8, 0x50, 0xE0],
    },
    pid: 14,
};

/// `DEVPKEY_Device_DeviceDesc`: `{A45C254E-DF1C-4EFD-8020-67D146A850E0}, 2`.
const DEVPKEY_DEVICE_DESC: DevPropKey = DevPropKey {
    fmtid: Guid {
        data1: 0xA45C_254E,
        data2: 0xDF1C,
        data3: 0x4EFD,
        data4: [0x80, 0x20, 0x67, 0xD1, 0x46, 0xA8, 0x50, 0xE0],
    },
    pid: 2,
};

/// `DEVPKEY_Device_InstanceId`: `{78C34FC8-104A-4ACA-9EA4-524D52996E57}, 256`.
const DEVPKEY_DEVICE_INSTANCE_ID: DevPropKey = DevPropKey {
    fmtid: Guid {
        data1: 0x78C3_4FC8,
        data2: 0x104A,
        data3: 0x4ACA,
        data4: [0x9E, 0xA4, 0x52, 0x4D, 0x52, 0x99, 0x6E, 0x57],
    },
    pid: 256,
};

#[repr(C)]
struct SpDeviceInterfaceData {
    cb_size: u32,
    interface_class_guid: Guid,
    flags: u32,
    reserved: isize,
}

#[repr(C)]
#[derive(Default)]
struct HiddAttributes {
    size: u32,
    vendor_id: u16,
    product_id: u16,
    version_number: u16,
}

#[repr(C)]
struct HidpCaps {
    usage: u16,
    usage_page: u16,
    input_report_byte_length: u16,
    output_report_byte_length: u16,
    feature_report_byte_length: u16,
    reserved: [u16; 17],
    number_link_collection_nodes: u16,
    number_input_button_caps: u16,
    number_input_value_caps: u16,
    number_input_data_indices: u16,
    number_output_button_caps: u16,
    number_output_value_caps: u16,
    number_output_data_indices: u16,
    number_feature_button_caps: u16,
    number_feature_value_caps: u16,
    number_feature_data_indices: u16,
}

#[repr(C)]
struct HidpValueCaps {
    usage_page: u16,
    report_id: u8,
    is_alias: u8,
    bit_field: u16,
    link_collection: u16,
    link_usage: u16,
    link_usage_page: u16,
    is_range: u8,
    is_string_range: u8,
    is_designator_range: u8,
    is_absolute: u8,
    has_null: u8,
    reserved: u8,
    bit_size: u16,
    report_count: u16,
    reserved2: [u16; 5],
    units_exp: u32,
    units: u32,
    logical_min: i32,
    logical_max: i32,
    physical_min: i32,
    physical_max: i32,
    usage_or_usage_min: u16,
    usage_max: u16,
    string_min: u16,
    string_max: u16,
    designator_min: u16,
    designator_max: u16,
    data_index_min: u16,
    data_index_max: u16,
}

#[repr(C)]
struct Overlapped {
    internal: isize,
    internal_high: isize,
    offset: u32,
    offset_high: u32,
    h_event: Handle,
}

const GENERIC_READ: u32 = 0x8000_0000;
const GENERIC_WRITE: u32 = 0x4000_0000;
const FILE_SHARE_READ: u32 = 0x1;
const FILE_SHARE_WRITE: u32 = 0x2;
const OPEN_EXISTING: u32 = 3;
const FILE_FLAG_OVERLAPPED: u32 = 0x4000_0000;
const DIGCF_PRESENT: u32 = 0x2;
const DIGCF_ALLCLASSES: u32 = 0x4;
const DIGCF_DEVICEINTERFACE: u32 = 0x10;
const DEVPROP_TYPE_BYTE: u32 = 0x0000_0003;
const DEVPROP_TYPE_UINT32: u32 = 0x0000_0007;
const DEVPROP_TYPE_GUID: u32 = 0x0000_000D;
const DEVPROP_TYPE_BOOLEAN: u32 = 0x0000_0011;
const DEVPROP_TYPE_STRING: u32 = 0x0000_0012;
const ERROR_IO_PENDING: u32 = 997;
const ERROR_NOT_FOUND: u32 = 1168;
const WAIT_OBJECT_0: u32 = 0;
const HIDP_STATUS_SUCCESS: i32 = 0x0011_0000;
const HIDP_INPUT: u32 = 0;
const HIDP_FEATURE: u32 = 2;
const HKEY_CURRENT_USER: Handle = 0x8000_0001u32 as i32 as isize;
const HKEY_LOCAL_MACHINE: Handle = 0x8000_0002u32 as i32 as isize;
const KEY_READ: u32 = 0x2_0019;
const REG_SZ: u32 = 1;
const REG_BINARY: u32 = 3;
const REG_DWORD: u32 = 4;

#[link(name = "kernel32")]
unsafe extern "system" {
    fn CreateFileW(
        lp_file_name: *const u16,
        dw_desired_access: u32,
        dw_share_mode: u32,
        lp_security_attributes: *const c_void,
        dw_creation_disposition: u32,
        dw_flags_and_attributes: u32,
        h_template_file: Handle,
    ) -> Handle;
    fn CloseHandle(handle: Handle) -> i32;
    fn CreateEventW(
        lp_event_attributes: *const c_void,
        b_manual_reset: i32,
        b_initial_state: i32,
        lp_name: *const u16,
    ) -> Handle;
    fn WaitForSingleObject(handle: Handle, dw_milliseconds: u32) -> u32;
    fn CancelIoEx(handle: Handle, lp_overlapped: *const Overlapped) -> i32;
    fn GetOverlappedResult(
        handle: Handle,
        lp_overlapped: *const Overlapped,
        lp_transferred: *mut u32,
        b_wait: i32,
    ) -> i32;
    fn ReadFile(
        handle: Handle,
        lp_buffer: *mut u8,
        n_bytes_to_read: u32,
        lp_bytes_read: *mut u32,
        lp_overlapped: *mut Overlapped,
    ) -> i32;
    fn WriteFile(
        handle: Handle,
        lp_buffer: *const u8,
        n_bytes_to_write: u32,
        lp_bytes_written: *mut u32,
        lp_overlapped: *mut Overlapped,
    ) -> i32;
    fn GetLastError() -> u32;
    // Local kernel32 aliases for the dynamic XInput loader. These intentionally coexist with
    // the windows_sys declarations in `platform::radio` (different signatures at each call site).
    #[allow(clashing_extern_declarations)]
    fn LoadLibraryW(lp_lib_file_name: *const u16) -> Handle;
    #[allow(clashing_extern_declarations)]
    #[link_name = "GetProcAddress"]
    fn GetProcAddressWb(h_module: Handle, lp_proc_name: *const u8) -> *const c_void;
    fn FreeLibrary(h_module: Handle) -> i32;
    fn Sleep(dw_milliseconds: u32);
}

#[link(name = "setupapi")]
unsafe extern "system" {
    fn SetupDiGetClassDevsW(
        class_guid: *const Guid,
        enumerator: *const u16,
        hwnd_parent: Handle,
        flags: u32,
    ) -> Handle;
    fn SetupDiEnumDeviceInterfaces(
        info_set: Handle,
        device_info_data: *const c_void,
        interface_class_guid: *const Guid,
        member_index: u32,
        device_interface_data: *mut SpDeviceInterfaceData,
    ) -> i32;
    fn SetupDiGetDeviceInterfaceDetailW(
        info_set: Handle,
        device_interface_data: *const SpDeviceInterfaceData,
        detail_data: *mut u8,
        detail_data_size: u32,
        required_size: *mut u32,
        device_info_data: *mut c_void,
    ) -> i32;
    fn SetupDiEnumDeviceInfo(
        info_set: Handle,
        member_index: u32,
        device_info_data: *mut SpDevinfoData,
    ) -> i32;
    fn SetupDiGetDevicePropertyW(
        info_set: Handle,
        device_info_data: *const SpDevinfoData,
        property_key: *const DevPropKey,
        property_type: *mut u32,
        property_buffer: *mut u8,
        property_buffer_size: u32,
        required_size: *mut u32,
        flags: u32,
    ) -> i32;
    fn SetupDiDestroyDeviceInfoList(info_set: Handle) -> i32;
}

// Every `HidD_*` export returns the Win32 `BOOLEAN` (an 8-bit `u8`), not a 32-bit `BOOL`:
// declaring them as `i32` would read undefined upper register bits as "success".
// `HidP_*` functions return a 32-bit `NTSTATUS`.
#[link(name = "hid")]
unsafe extern "system" {
    fn HidD_GetHidGuid(hid_guid: *mut Guid);
    fn HidD_GetAttributes(handle: Handle, attributes: *mut HiddAttributes) -> u8;
    fn HidD_GetPreparsedData(handle: Handle, preparsed_data: *mut isize) -> u8;
    fn HidD_FreePreparsedData(preparsed_data: isize) -> u8;
    fn HidP_GetCaps(preparsed_data: isize, capabilities: *mut HidpCaps) -> i32;
    fn HidP_GetValueCaps(
        report_type: u32,
        value_caps: *mut HidpValueCaps,
        value_caps_length: *mut u16,
        preparsed_data: isize,
    ) -> i32;
    fn HidP_GetUsageValue(
        report_type: u32,
        usage_page: u16,
        link_collection: u16,
        usage: u16,
        usage_value: *mut u32,
        preparsed_data: isize,
        report_buffer: *const u8,
        report_length: u32,
    ) -> i32;
    fn HidD_GetProductString(handle: Handle, buffer: *mut u8, buffer_length: u32) -> u8;
    fn HidD_GetSerialNumberString(handle: Handle, buffer: *mut u8, buffer_length: u32) -> u8;
    fn HidD_GetFeature(handle: Handle, report_buffer: *mut u8, report_buffer_length: u32) -> u8;
    fn HidD_SetFeature(handle: Handle, report_buffer: *const u8, report_buffer_length: u32) -> u8;
    fn HidD_GetInputReport(handle: Handle, report_buffer: *mut u8, report_buffer_length: u32)
    -> u8;
    fn HidD_SetOutputReport(
        handle: Handle,
        report_buffer: *const u8,
        report_buffer_length: u32,
    ) -> u8;
}

#[link(name = "advapi32")]
unsafe extern "system" {
    fn RegOpenKeyExW(
        key: Handle,
        sub_key: *const u16,
        options: u32,
        desired_access: u32,
        result: *mut Handle,
    ) -> i32;
    fn RegCloseKey(key: Handle) -> i32;
    fn RegEnumKeyExW(
        key: Handle,
        index: u32,
        name: *mut u16,
        name_size: *mut u32,
        reserved: *mut u32,
        class_name: *mut u16,
        class_name_size: *mut u32,
        last_write_time: *mut c_void,
    ) -> i32;
    fn RegQueryValueExW(
        key: Handle,
        value_name: *const u16,
        reserved: *mut u32,
        value_type: *mut u32,
        data: *mut u8,
        data_size: *mut u32,
    ) -> i32;
}

// ---------------------------------------------------------------------------
// Küçük yardımcılar (saf wrapper'lar)
// ---------------------------------------------------------------------------

use crate::platform::win32_helpers::wide;

fn ascii_z(text: &str) -> Vec<u8> {
    text.bytes().chain(Some(0)).collect()
}

fn invalid(handle: Handle) -> bool {
    handle == 0 || handle == INVALID_HANDLE
}

fn clamp01(value: f32) -> f32 {
    if value.is_finite() {
        value.clamp(0.0, 1.0)
    } else {
        0.0
    }
}

fn percent_01(value: i32) -> f32 {
    clamp01(value as f32 / 100.0)
}

fn reading(
    id: String,
    name: String,
    level_01: f32,
    is_connected: bool,
    is_charging: bool,
    source: BatterySource,
) -> WirelessReading {
    let device_kind =
        super::wireless_vendors_ext::classify_wireless_device_kind(0, 0, &name, source, &id);
    WirelessReading {
        id,
        name,
        level_01: clamp01(level_01),
        sampled_at: Instant::now(),
        is_connected,
        is_charging,
        source,
        stale: false,
        device_kind,
        secondary_level: None,
    }
}

#[allow(clippy::too_many_arguments)]
fn reading_with_meta(
    id: String,
    name: String,
    level_01: f32,
    is_connected: bool,
    is_charging: bool,
    source: BatterySource,
    device_kind: WirelessDeviceKind,
    secondary_level: Option<f32>,
) -> WirelessReading {
    WirelessReading {
        id,
        name,
        level_01: clamp01(level_01),
        sampled_at: Instant::now(),
        is_connected,
        is_charging,
        source,
        stale: false,
        device_kind,
        secondary_level: secondary_level.map(clamp01),
    }
}

fn utf16_from_bytes(bytes: &[u8]) -> String {
    let mut units = Vec::with_capacity(bytes.len() / 2);
    for pair in bytes.as_chunks::<2>().0 {
        let unit = u16::from_le_bytes([pair[0], pair[1]]);
        if unit == 0 {
            break;
        }
        units.push(unit);
    }
    String::from_utf16_lossy(&units)
}

fn utf16_from_detail(bytes: &[u8]) -> String {
    if bytes.len() <= 4 {
        return String::new();
    }
    utf16_from_bytes(&bytes[4..])
}

/// Kimlik normalize etme: VID:PID:serial, serial yoksa cihaz yolunun stabilize edilmiş hash'i.
fn build_device_id(prefix: &str, vid: u16, pid: u16, serial: &str, path: &str) -> String {
    let mut clean: String = serial
        .chars()
        .filter(|c| c.is_ascii_alphanumeric())
        .collect();
    if clean.len() > 16 {
        clean = clean[clean.len() - 16..].to_owned();
    }
    if clean.is_empty() {
        clean = format!("{:08X}", fnv1a_u32(path.as_bytes()));
    }
    format!(
        "{prefix}:{vid:04X}:{pid:04X}:{}",
        clean.to_ascii_uppercase()
    )
}

fn fnv1a_u32(bytes: &[u8]) -> u32 {
    let mut hash = 0x811C_9DC5u32;
    for &byte in bytes {
        hash ^= u32::from(byte);
        hash = hash.wrapping_mul(0x0100_0193);
    }
    hash
}

fn normalize_name(name: &str) -> String {
    name.split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
        .to_ascii_lowercase()
}

/// Kablolu cihaz filtresi (`IsIgnoredWiredDevice` + genel "wired" kontrolü).
pub fn is_ignored_wired_device(name: &str) -> bool {
    let lower = name.trim().to_ascii_lowercase();
    if lower.is_empty() {
        return true;
    }
    if lower.contains("wraith w75") {
        return true;
    }
    lower.contains("wired") && !lower.contains("wireless")
}

fn is_ignored_wired_product(product: &str) -> bool {
    !product.trim().is_empty() && is_ignored_wired_device(product)
}

fn is_ignored_wired_reading(reading: &WirelessReading) -> bool {
    if is_ignored_wired_device(&reading.name) {
        return true;
    }
    let upper = reading.id.to_ascii_uppercase();
    upper.contains("2E3C") && upper.contains("C368")
}

fn canonical_key(reading: &WirelessReading) -> String {
    physical_key(&reading.id)
}

/// Physical identity of a reading. HID ids (`<provider>:VVVV:PPPP:<serial|container>[:slot]`)
/// collapse across providers onto `hid:VVVV:PPPP:...`; Bluetooth ids are already physical
/// (`bt:<address>[#component]`). Display names never participate, so two identical-name
/// peripherals stay two rows.
fn physical_key(id: &str) -> String {
    let id = id.trim().to_ascii_lowercase();
    if let Some((_, rest)) = id.split_once(':')
        && rest
            .split(':')
            .take(2)
            .filter(|p| p.len() == 4 && p.bytes().all(|b| b.is_ascii_hexdigit()))
            .count()
            == 2
    {
        return format!("hid:{rest}");
    }
    id
}

fn source_priority(source: BatterySource) -> u8 {
    source.priority()
}

fn prefer(candidate: &WirelessReading, existing: &WirelessReading) -> bool {
    if candidate.is_connected != existing.is_connected {
        return candidate.is_connected;
    }
    if candidate.stale != existing.stale {
        return !candidate.stale;
    }
    let (c_known, e_known) = (
        candidate.level_percent_opt().is_some(),
        existing.level_percent_opt().is_some(),
    );
    if c_known != e_known {
        return c_known;
    }
    source_priority(candidate.source) < source_priority(existing.source)
}

struct Budget {
    deadline: Instant,
}

impl Budget {
    fn new(ms: u64) -> Self {
        Self {
            deadline: Instant::now() + Duration::from_millis(ms),
        }
    }

    fn expired(&self, cancel: &AtomicBool) -> bool {
        cancel.load(Ordering::Relaxed) || Instant::now() >= self.deadline
    }
}

// ---------------------------------------------------------------------------
// HID cihaz anlık görüntüsü
// ---------------------------------------------------------------------------

#[derive(Clone)]
struct BatteryValueCap {
    is_feature: bool,
    report_id: u8,
    usage_page: u16,
    usage: u16,
    link_collection: u16,
    logical_min: i32,
    logical_max: i32,
    bit_size: u16,
}

#[derive(Clone)]
struct HidDevice {
    path: String,
    vid: u16,
    pid: u16,
    usage_page: u16,
    usage: u16,
    input_len: u16,
    output_len: u16,
    feature_len: u16,
    product: String,
    serial: String,
    /// `DEVPKEY_Device_ContainerId` of the owning physical device (empty when unknown or
    /// when it is the shared local-machine container).
    container: String,
    battery_caps: Vec<BatteryValueCap>,
}

impl HidDevice {
    /// Stable physical id: serial when the device reports one, otherwise the PnP container
    /// (shared by every HID collection of the same physical device), otherwise the path.
    /// Two distinct devices with the same VID/PID therefore never collapse, while the
    /// several top-level collections of one device do.
    fn id(&self, prefix: &str) -> String {
        let identity = if self.container.is_empty() {
            self.path.as_str()
        } else {
            self.container.as_str()
        };
        build_device_id(prefix, self.vid, self.pid, &self.serial, identity)
    }
}

/// Battery-related usages. `0x84:0x65` is the Power Device page's boolean **Overload**
/// flag and `0x85:0x66` RemainingCapacity is expressed in `CapacityMode` units (often mAh),
/// so neither is a percentage and both are deliberately excluded.
fn match_battery_usage(vc: &HidpValueCaps) -> Option<(u16, u16)> {
    let targets: &[u16] = match vc.usage_page {
        0x0006 => &[0x0020],
        0x0084 => &[0x00D0],
        0x0085 => &[0x0064, 0x0065, 0x0044],
        _ => &[],
    };
    for &target in targets {
        if vc.is_range == 0 {
            if vc.usage_or_usage_min == target {
                return Some((vc.usage_page, target));
            }
        } else if target >= vc.usage_or_usage_min && target <= vc.usage_max {
            return Some((vc.usage_page, target));
        }
    }
    None
}

fn extract_value_caps(
    pp: isize,
    report_type: u32,
    count: u16,
    is_feature: bool,
    out: &mut Vec<BatteryValueCap>,
) {
    if count == 0 || count > 64 {
        return;
    }
    let mut caps: Vec<HidpValueCaps> = std::iter::repeat_with(|| unsafe { std::mem::zeroed() })
        .take(count as usize)
        .collect();
    let mut length = count;
    let status = unsafe { HidP_GetValueCaps(report_type, caps.as_mut_ptr(), &mut length, pp) };
    if status != HIDP_STATUS_SUCCESS {
        return;
    }
    for cap in caps.iter().take(length as usize) {
        if let Some((usage_page, usage)) = match_battery_usage(cap) {
            out.push(BatteryValueCap {
                is_feature,
                report_id: cap.report_id,
                usage_page,
                usage,
                link_collection: cap.link_collection,
                logical_min: cap.logical_min,
                logical_max: cap.logical_max,
                bit_size: cap.bit_size,
            });
        }
    }
}

fn read_hid_string(handle: Handle, is_serial: bool) -> String {
    let mut buffer = vec![0u8; 512];
    let ok = unsafe {
        if is_serial {
            HidD_GetSerialNumberString(handle, buffer.as_mut_ptr(), buffer.len() as u32)
        } else {
            HidD_GetProductString(handle, buffer.as_mut_ptr(), buffer.len() as u32)
        }
    };
    if ok == 0 {
        String::new()
    } else {
        utf16_from_bytes(&buffer).trim().to_owned()
    }
}

fn enumerate_hid_devices() -> Vec<HidDevice> {
    let mut guid = HID_CLASS_GUID;
    unsafe { HidD_GetHidGuid(&mut guid) };
    let set = unsafe {
        SetupDiGetClassDevsW(
            &guid,
            std::ptr::null(),
            0,
            DIGCF_PRESENT | DIGCF_DEVICEINTERFACE,
        )
    };
    if invalid(set) {
        return Vec::new();
    }
    let mut result = Vec::new();
    for index in 0..256u32 {
        let mut interface_data: SpDeviceInterfaceData = unsafe { std::mem::zeroed() };
        interface_data.cb_size = size_of::<SpDeviceInterfaceData>() as u32;
        if unsafe {
            SetupDiEnumDeviceInterfaces(set, std::ptr::null(), &guid, index, &mut interface_data)
        } == 0
        {
            break;
        }
        let mut required = 0u32;
        unsafe {
            SetupDiGetDeviceInterfaceDetailW(
                set,
                &interface_data,
                std::ptr::null_mut(),
                0,
                &mut required,
                std::ptr::null_mut(),
            );
        }
        if required == 0 || required > 4096 {
            continue;
        }
        let mut detail = vec![0u8; required as usize];
        let cb_size = if size_of::<isize>() == 8 { 8u32 } else { 6u32 };
        detail[..4].copy_from_slice(&cb_size.to_le_bytes());
        let mut devinfo: SpDevinfoData = unsafe { std::mem::zeroed() };
        devinfo.cb_size = size_of::<SpDevinfoData>() as u32;
        if unsafe {
            SetupDiGetDeviceInterfaceDetailW(
                set,
                &interface_data,
                detail.as_mut_ptr(),
                required,
                std::ptr::null_mut(),
                (&mut devinfo as *mut SpDevinfoData).cast::<c_void>(),
            )
        } == 0
        {
            continue;
        }
        let path = utf16_from_detail(&detail);
        if path.is_empty() {
            continue;
        }
        let container = setupdi_prop_container(set, &devinfo)
            .filter(|c| !c.eq_ignore_ascii_case(LOCAL_MACHINE_CONTAINER))
            .unwrap_or_default();
        let path_wide = wide(&path);
        let meta = unsafe {
            OwnedHandle::new(CreateFileW(
                path_wide.as_ptr(),
                0,
                FILE_SHARE_READ | FILE_SHARE_WRITE,
                std::ptr::null(),
                OPEN_EXISTING,
                0,
                0,
            ))
        };
        let Some(meta) = meta else { continue };
        let mut attrs = HiddAttributes {
            size: size_of::<HiddAttributes>() as u32,
            ..Default::default()
        };
        if unsafe { HidD_GetAttributes(meta.raw(), &mut attrs) } == 0 {
            continue;
        }
        let mut pp = 0isize;
        if unsafe { HidD_GetPreparsedData(meta.raw(), &mut pp) } == 0 || pp == 0 {
            continue;
        }
        let mut caps: HidpCaps = unsafe { std::mem::zeroed() };
        let mut battery_caps = Vec::new();
        if unsafe { HidP_GetCaps(pp, &mut caps) } == HIDP_STATUS_SUCCESS {
            extract_value_caps(
                pp,
                HIDP_FEATURE,
                caps.number_feature_value_caps,
                true,
                &mut battery_caps,
            );
            extract_value_caps(
                pp,
                HIDP_INPUT,
                caps.number_input_value_caps,
                false,
                &mut battery_caps,
            );
        }
        unsafe { HidD_FreePreparsedData(pp) };
        result.push(HidDevice {
            path,
            vid: attrs.vendor_id,
            pid: attrs.product_id,
            usage_page: caps.usage_page,
            usage: caps.usage,
            input_len: caps.input_report_byte_length,
            output_len: caps.output_report_byte_length,
            feature_len: caps.feature_report_byte_length,
            product: read_hid_string(meta.raw(), false),
            serial: read_hid_string(meta.raw(), true),
            container,
            battery_caps,
        });
        if result.len() >= 256 {
            break;
        }
    }
    unsafe { SetupDiDestroyDeviceInfoList(set) };
    result
}

// ---------------------------------------------------------------------------
// Güvenli HID oturumu (sınırlı overlapped I/O + feature/output report)
// ---------------------------------------------------------------------------

struct OwnedHandle(Handle);

impl OwnedHandle {
    fn new(handle: Handle) -> Option<Self> {
        if invalid(handle) {
            None
        } else {
            Some(Self(handle))
        }
    }

    fn raw(&self) -> Handle {
        self.0
    }
}

impl Drop for OwnedHandle {
    fn drop(&mut self) {
        unsafe { CloseHandle(self.0) };
    }
}

struct HidSession {
    handle: OwnedHandle,
}

impl HidSession {
    fn open(path: &str, feature_only: bool) -> Option<Self> {
        let access = if feature_only {
            0
        } else {
            GENERIC_READ | GENERIC_WRITE
        };
        let flags = if feature_only {
            0
        } else {
            FILE_FLAG_OVERLAPPED
        };
        let wide_path = wide(path);
        let handle = unsafe {
            CreateFileW(
                wide_path.as_ptr(),
                access,
                FILE_SHARE_READ | FILE_SHARE_WRITE,
                std::ptr::null(),
                OPEN_EXISTING,
                flags,
                0,
            )
        };
        let handle = OwnedHandle::new(handle)?;
        if !feature_only && is_system_mouse_or_keyboard(handle.raw()) {
            return None;
        }
        Some(Self { handle })
    }

    /// Opens an overlapped read-only HID session (`GENERIC_READ`, no write access).
    /// Used by passive input-report readers such as `GamepadHid`.
    fn open_read_only(path: &str) -> Option<Self> {
        let wide_path = wide(path);
        let handle = unsafe {
            CreateFileW(
                wide_path.as_ptr(),
                GENERIC_READ,
                FILE_SHARE_READ | FILE_SHARE_WRITE,
                std::ptr::null(),
                OPEN_EXISTING,
                FILE_FLAG_OVERLAPPED,
                0,
            )
        };
        let handle = OwnedHandle::new(handle)?;
        if is_system_mouse_or_keyboard(handle.raw()) {
            return None;
        }
        Some(Self { handle })
    }

    fn get_feature(&self, buffer: &mut [u8]) -> bool {
        (unsafe { HidD_GetFeature(self.handle.raw(), buffer.as_mut_ptr(), buffer.len() as u32) })
            != 0
    }

    fn set_feature(&self, buffer: &[u8]) -> bool {
        (unsafe { HidD_SetFeature(self.handle.raw(), buffer.as_ptr(), buffer.len() as u32) }) != 0
    }

    fn get_input_report(&self, buffer: &mut [u8]) -> bool {
        (unsafe {
            HidD_GetInputReport(self.handle.raw(), buffer.as_mut_ptr(), buffer.len() as u32)
        }) != 0
    }

    fn set_output_report(&self, buffer: &[u8]) -> bool {
        (unsafe { HidD_SetOutputReport(self.handle.raw(), buffer.as_ptr(), buffer.len() as u32) })
            != 0
    }

    fn extract_usage_value(
        &self,
        is_feature: bool,
        usage_page: u16,
        link_collection: u16,
        usage: u16,
        report: &[u8],
    ) -> Option<u32> {
        let mut pp = 0isize;
        if unsafe { HidD_GetPreparsedData(self.handle.raw(), &mut pp) } == 0 || pp == 0 {
            return None;
        }
        let mut value = 0u32;
        let ok = unsafe {
            HidP_GetUsageValue(
                if is_feature { HIDP_FEATURE } else { HIDP_INPUT },
                usage_page,
                link_collection,
                usage,
                &mut value,
                pp,
                report.as_ptr(),
                report.len() as u32,
            )
        } == HIDP_STATUS_SUCCESS;
        unsafe { HidD_FreePreparsedData(pp) };
        if ok { Some(value) } else { None }
    }

    fn write_report(&self, report: &[u8], timeout_ms: u32) -> bool {
        if report.is_empty() {
            return false;
        }
        overlapped_io(self.handle.raw(), true, timeout_ms, report, None).is_some_and(|n| n > 0)
    }

    fn read_report(&self, destination: &mut [u8], timeout_ms: u32) -> Option<usize> {
        if destination.is_empty() {
            return None;
        }
        overlapped_io(self.handle.raw(), false, timeout_ms, &[], Some(destination))
    }

    /// Vendor protokolleri için write -> read turu; yanıt doğrulanana kadar en fazla 4 okuma.
    fn transact(
        &self,
        request: &[u8],
        response: &mut [u8],
        timeout_ms: u32,
        validator: impl Fn(&[u8]) -> bool,
        cancel: &AtomicBool,
    ) -> Option<usize> {
        if cancel.load(Ordering::Relaxed) {
            return None;
        }
        if !self.write_report(request, timeout_ms) && !self.set_output_report(request) {
            return None;
        }
        let start = Instant::now();
        for _ in 0..4 {
            if cancel.load(Ordering::Relaxed) {
                return None;
            }
            let elapsed = start.elapsed().as_millis().min(u128::from(u32::MAX)) as u32;
            let remaining = timeout_ms.saturating_sub(elapsed);
            if remaining < 40 {
                break;
            }
            let bytes = self.read_report(response, remaining)?;
            if validator(&response[..bytes]) {
                return Some(bytes);
            }
        }
        None
    }
}

fn is_system_mouse_or_keyboard(handle: Handle) -> bool {
    let mut pp = 0isize;
    if unsafe { HidD_GetPreparsedData(handle, &mut pp) } == 0 || pp == 0 {
        return false;
    }
    let mut caps: HidpCaps = unsafe { std::mem::zeroed() };
    let status = unsafe { HidP_GetCaps(pp, &mut caps) };
    unsafe { HidD_FreePreparsedData(pp) };
    status == HIDP_STATUS_SUCCESS
        && caps.usage_page == 0x0001
        && matches!(caps.usage, 0x0002 | 0x0006)
}

/// Overlapped ReadFile/WriteFile. Zaman aşımında bekleyen IRP buffer'ı hâlâ kullanıyor
/// olabilir; bu nadir durumda buffer/event'i bilerek sızdırıyoruz (use-after-free yok).
fn overlapped_io(
    handle: Handle,
    write: bool,
    timeout_ms: u32,
    request: &[u8],
    destination: Option<&mut [u8]>,
) -> Option<usize> {
    let length = if write {
        request.len()
    } else {
        destination.as_ref().map_or(0, |d| d.len())
    };
    if length == 0 || length > u32::MAX as usize {
        return None;
    }
    let clamped = timeout_ms.clamp(50, 300);
    let event = unsafe { CreateEventW(std::ptr::null(), 1, 0, std::ptr::null()) };
    if invalid(event) {
        return None;
    }
    let mut overlapped: Overlapped = unsafe { std::mem::zeroed() };
    overlapped.h_event = event;
    let mut buffer = vec![0u8; length].into_boxed_slice();
    if write {
        buffer.copy_from_slice(request);
    }
    let ptr = buffer.as_mut_ptr();
    let ok = unsafe {
        if write {
            WriteFile(
                handle,
                ptr,
                length as u32,
                std::ptr::null_mut(),
                &mut overlapped,
            )
        } else {
            ReadFile(
                handle,
                ptr,
                length as u32,
                std::ptr::null_mut(),
                &mut overlapped,
            )
        }
    } != 0;
    let mut transferred = 0u32;
    let mut success = false;
    if ok {
        success = unsafe { GetOverlappedResult(handle, &overlapped, &mut transferred, 0) } != 0;
    } else if unsafe { GetLastError() } == ERROR_IO_PENDING {
        if unsafe { WaitForSingleObject(event, clamped) } == WAIT_OBJECT_0 {
            success = unsafe { GetOverlappedResult(handle, &overlapped, &mut transferred, 0) } != 0;
        } else {
            let cancelled = unsafe { CancelIoEx(handle, &overlapped) } != 0;
            let cancel_error = unsafe { GetLastError() };
            let drained = unsafe { WaitForSingleObject(event, 250) } == WAIT_OBJECT_0;
            if !drained && (!cancelled || cancel_error != ERROR_NOT_FOUND) {
                std::mem::forget(buffer);
                return None;
            }
            let _ = unsafe { GetOverlappedResult(handle, &overlapped, &mut transferred, 0) };
        }
    }
    unsafe { CloseHandle(event) };
    if !success {
        return None;
    }
    if write {
        return Some(transferred as usize);
    }
    let count = (transferred as usize).min(length);
    if let Some(destination) = destination {
        destination[..count].copy_from_slice(&buffer[..count]);
    }
    Some(count)
}

// ---------------------------------------------------------------------------
// Saf protokol yardımcıları
// ---------------------------------------------------------------------------

/// XInput kaba seviyesi -> 0.25 / 0.5 / 0.75 / 1.0 (uydurulmuş yüzde yok).
fn xinput_level_01(level: u8) -> Option<f32> {
    match level {
        0 => Some(0.05),
        1 => Some(0.20),
        2 => Some(0.60),
        3 => Some(1.00),
        _ => None,
    }
}

/// Maps a raw usage value onto `0..=100` across the descriptor's logical range using
/// widened (`i64`) arithmetic. Returns `-1` for an inverted range or an out-of-range value;
/// a value equal to `logical_min` is a genuine `0%`.
fn normalize_logical_to_percent(raw: i64, logical_min: i32, logical_max: i32) -> i32 {
    let (min, max) = (i64::from(logical_min), i64::from(logical_max));
    if max <= min || !(min..=max).contains(&raw) {
        return -1;
    }
    let span = max - min;
    (((raw - min) * 100 + span / 2) / span) as i32
}

/// `HidP_GetUsageValue` returns the raw report bits; a signed logical range needs the
/// field sign-extended from its `BitSize` before range checks.
fn signed_usage_value(raw: u32, cap: &BatteryValueCap) -> i64 {
    if cap.logical_min < 0 && (1..=32).contains(&cap.bit_size) {
        if cap.bit_size == 32 {
            i64::from(raw as i32)
        } else {
            let shift = 32 - u32::from(cap.bit_size);
            i64::from(((raw << shift) as i32) >> shift)
        }
    } else {
        i64::from(raw)
    }
}

/// Descriptor-driven value decode only: no positional byte guessing when extraction fails.
fn direct_byte_report(raw: u32, cap: &BatteryValueCap) -> Option<(i32, bool)> {
    let percent = normalize_logical_to_percent(
        signed_usage_value(raw, cap),
        cap.logical_min,
        cap.logical_max,
    );
    (percent >= 0).then_some((percent, false))
}

fn is_capacity_cap(cap: &BatteryValueCap) -> bool {
    (cap.usage_page == 0x0006 && cap.usage == 0x0020)
        || (cap.usage_page == 0x0085 && matches!(cap.usage, 0x0064 | 0x0065))
}

fn is_charging_cap(cap: &BatteryValueCap) -> bool {
    (cap.usage_page == 0x0085 && cap.usage == 0x0044)
        || (cap.usage_page == 0x0084 && cap.usage == 0x00D0)
}

// -- Logitech HID++ ---------------------------------------------------------

const HIDPP_SHORT_REPORT: u8 = 0x10;
const HIDPP_LONG_REPORT: u8 = 0x11;
const HIDPP_SOFTWARE_ID: u8 = 0x0A;
const HIDPP_FEATURE_BATTERY_STATUS: u16 = 0x1000;
const HIDPP_FEATURE_BATTERY_VOLTAGE: u16 = 0x1001;
const HIDPP_FEATURE_UNIFIED_BATTERY: u16 = 0x1004;
const HIDPP_FEATURE_DEVICE_NAME: u16 = 0x0005;

fn hidpp_root_request(device_index: u8, feature: u16, report_len: usize) -> Vec<u8> {
    hidpp_feature_request(
        device_index,
        0x00,
        0x00,
        &[(feature >> 8) as u8, feature as u8, 0],
        report_len,
    )
}

fn hidpp_feature_request(
    device_index: u8,
    feature_index: u8,
    function: u8,
    params: &[u8; 3],
    report_len: usize,
) -> Vec<u8> {
    let mut packet = vec![0u8; report_len];
    packet[0] = if report_len == 7 {
        HIDPP_SHORT_REPORT
    } else {
        HIDPP_LONG_REPORT
    };
    packet[1] = device_index;
    packet[2] = feature_index;
    packet[3] = ((function & 0x0F) << 4) | (HIDPP_SOFTWARE_ID & 0x0F);
    packet[4..7].copy_from_slice(params);
    packet
}

fn hidpp_is_error(reply: &[u8]) -> bool {
    reply.len() >= 4
        && matches!(reply[0], HIDPP_SHORT_REPORT | HIDPP_LONG_REPORT)
        && matches!(reply[2], 0x8F | 0xFF)
}

fn hidpp_parse_feature_index(reply: &[u8], device_index: u8) -> Option<u8> {
    if reply.len() < 5
        || !matches!(reply[0], HIDPP_SHORT_REPORT | HIDPP_LONG_REPORT)
        || reply[1] != device_index
        || reply[2] != 0
        || (reply[3] & 0x0F) != (HIDPP_SOFTWARE_ID & 0x0F)
    {
        return None;
    }
    let index = reply[4];
    if index == 0 { None } else { Some(index) }
}

fn hidpp_parse_unified(reply: &[u8], device_index: u8, feature_index: u8) -> Option<(f32, bool)> {
    if reply.len() < 7
        || !matches!(reply[0], HIDPP_SHORT_REPORT | HIDPP_LONG_REPORT)
        || reply[1] != device_index
        || reply[2] != feature_index
    {
        return None;
    }
    let soc = i32::from(reply[4]);
    let coarse = reply[5];
    let charging_status = reply[6];
    let percent = if (1..=100).contains(&soc) {
        soc
    } else if coarse != 0 {
        if coarse & 0x08 != 0 {
            90
        } else if coarse & 0x04 != 0 {
            50
        } else if coarse & 0x02 != 0 {
            20
        } else if coarse & 0x01 != 0 {
            5
        } else {
            return None;
        }
    } else if soc == 0 && (1..=4).contains(&charging_status) {
        0
    } else {
        return None;
    };
    Some((percent_01(percent), (1..=4).contains(&charging_status)))
}

fn hidpp_parse_status(reply: &[u8], device_index: u8, feature_index: u8) -> Option<(f32, bool)> {
    if reply.len() < 7
        || !matches!(reply[0], HIDPP_SHORT_REPORT | HIDPP_LONG_REPORT)
        || reply[1] != device_index
        || reply[2] != feature_index
    {
        return None;
    }
    let level = i32::from(reply[4]);
    let status = reply[6];
    let charging = (1..=4).contains(&status);
    let percent = if (1..=100).contains(&level) {
        level
    } else if level == 0 && charging {
        0
    } else {
        return None;
    };
    Some((percent_01(percent), charging))
}

fn estimate_percent_from_voltage_mv(mv: i32) -> i32 {
    if mv >= 4180 {
        return 100;
    }
    if mv <= 3500 {
        return 0;
    }
    if mv < 3700 {
        (((mv - 3500) * 20) / 200).clamp(0, 20)
    } else if mv < 3950 {
        (20 + ((mv - 3700) * 55) / 250).clamp(20, 75)
    } else {
        (75 + ((mv - 3950) * 25) / 230).clamp(75, 100)
    }
}

fn hidpp_parse_voltage(
    reply: &[u8],
    device_index: u8,
    feature_index: u8,
) -> Option<(f32, bool, i32)> {
    if reply.len() < 7
        || !matches!(reply[0], HIDPP_SHORT_REPORT | HIDPP_LONG_REPORT)
        || reply[1] != device_index
        || reply[2] != feature_index
    {
        return None;
    }
    let mv = (i32::from(reply[4]) << 8) | i32::from(reply[5]);
    if !(2800..=4500).contains(&mv) {
        return None;
    }
    Some((
        percent_01(estimate_percent_from_voltage_mv(mv)),
        reply[6] & 0x80 != 0,
        mv,
    ))
}

// -- Razer ------------------------------------------------------------------

const RAZER_PAYLOAD_LEN: usize = 90;
const RAZER_BUFFER_LEN: usize = 91;
const RAZER_STATUS_SUCCESS: u8 = 0x02;
const RAZER_COMMAND_CLASS_POWER: u8 = 0x07;
const RAZER_CMD_BATTERY: u8 = 0x80;
const RAZER_CMD_CHARGING: u8 = 0x84;

fn razer_xor_crc(payload: &[u8]) -> u8 {
    let payload = razer_normalize(payload);
    if payload.len() < RAZER_PAYLOAD_LEN {
        return 0;
    }
    let mut crc = 0u8;
    for &byte in &payload[2..=87] {
        crc ^= byte;
    }
    crc
}

fn razer_normalize(raw: &[u8]) -> &[u8] {
    if raw.len() >= RAZER_BUFFER_LEN && raw[0] == 0 && matches!(raw[1], 0x00..=0x03) {
        &raw[1..RAZER_BUFFER_LEN]
    } else if raw.len() >= RAZER_PAYLOAD_LEN {
        &raw[..RAZER_PAYLOAD_LEN]
    } else {
        raw
    }
}

fn razer_build_command(command: u8, transaction_id: u8, include_report_id: bool) -> Vec<u8> {
    let mut payload = vec![0u8; RAZER_PAYLOAD_LEN];
    payload[0] = 0x00;
    payload[1] = transaction_id;
    payload[5] = 0x02;
    payload[6] = RAZER_COMMAND_CLASS_POWER;
    payload[7] = command;
    payload[88] = razer_xor_crc(&payload);
    if !include_report_id {
        return payload;
    }
    let mut framed = vec![0u8; RAZER_BUFFER_LEN];
    framed[1..].copy_from_slice(&payload);
    framed
}

fn razer_parse_reply(raw: &[u8], expected_command: u8) -> Option<u8> {
    let payload = razer_normalize(raw);
    if payload.len() < RAZER_PAYLOAD_LEN
        || payload[0] != RAZER_STATUS_SUCCESS
        || payload[6] != RAZER_COMMAND_CLASS_POWER
        || payload[7] != expected_command
        || payload[88] != razer_xor_crc(payload)
    {
        return None;
    }
    Some(payload[9])
}

fn razer_reply_tx_id(raw: &[u8]) -> Option<u8> {
    let payload = razer_normalize(raw);
    (payload.len() >= RAZER_PAYLOAD_LEN).then_some(payload[1])
}

fn razer_level_01(raw: u8) -> f32 {
    clamp01(f32::from(raw) / 255.0)
}

// -- SteelSeries ------------------------------------------------------------

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum SsProtocol {
    /// Nova Pro base station: report id `0x06`, `06 B0`; level is a 0..=8 step code.
    NovaPro,
    /// Nova 7 / 7X / 7P / 3P / 3X (Gen 1): `00 B0`, reply `B0 <conn 03> <step 0..4> <charging>`.
    Nova7,
    /// Nova 7 Gen 2 family (`0x2258`, `0x227E`): same frame, byte 2 is a direct percentage.
    NovaDirect,
    /// Nova 5 / 5X: `00 B0`, reply `B0 <offline 02> _ <percent> <charging>`.
    Nova5,
    /// Arctis 7+ / 7P+ / 7X+ / 7+ Destiny: `00 B0`, reply `B0 <offline 01> <step 0..4> <charging>`.
    Arctis7Plus,
    /// Arctis 1 / 7 (2017-2019): report id `0x06`, `06 18`, reply `06 18 <percent>`.
    ArctisLegacy,
    /// Arctis 9: `00 20`, reply byte 3 = raw `0x64..=0x9A` level, byte 4 = charging.
    Arctis9,
    /// Arctis Pro Wireless base: `40 AA`, reply `40 AA <headset 0..4> <spare 0..4>`.
    ArctisPro,
    Mouse,
    Keyboard,
}

struct SsParsed {
    level: f32,
    charging: bool,
    connected: bool,
    spare: Option<f32>,
}

fn ss_protocol_for_pid(pid: u16) -> Option<SsProtocol> {
    match pid {
        0x12E0 | 0x12E2 | 0x12E5 | 0x12E6 => Some(SsProtocol::NovaPro),
        0x2258 | 0x227E => Some(SsProtocol::NovaDirect),
        0x2202 | 0x2204 | 0x2206 | 0x2208 | 0x220A | 0x223A | 0x225A | 0x2266 | 0x226A => {
            Some(SsProtocol::Nova7)
        }
        0x220E | 0x2212 | 0x2216 | 0x2236 => Some(SsProtocol::Arctis7Plus),
        0x2232 | 0x2234 | 0x223E | 0x2240 | 0x2242 => Some(SsProtocol::Nova5),
        0x12B3 | 0x12B6 | 0x1260 | 0x12AD | 0x12D5 | 0x12D7 => Some(SsProtocol::ArctisLegacy),
        0x12C2 => Some(SsProtocol::Arctis9),
        0x1290 => Some(SsProtocol::ArctisPro),
        0x1624 | 0x1626 | 0x162C | 0x162E | 0x1630 => Some(SsProtocol::Keyboard),
        0x1838 | 0x183A | 0x1852 | 0x1854 | 0x1858 | 0x185A | 0x185E | 0x1860 | 0x1842 | 0x1848
        | 0x1830 | 0x1832 | 0x172B => Some(SsProtocol::Mouse),
        _ => None,
    }
}

fn ss_default_name(pid: u16) -> &'static str {
    match pid {
        0x12E0 | 0x12E2 | 0x12E5 | 0x12E6 => "Arctis Nova Pro Wireless",
        0x2258 | 0x227E => "Arctis Nova 7 Wireless Gen 2",
        0x2202 | 0x2204 | 0x2206 | 0x2208 | 0x220A | 0x223A | 0x225A => "Arctis Nova 7 Wireless",
        0x2266 => "Arctis Nova 3P Wireless",
        0x226A => "Arctis Nova 3X Wireless",
        0x2232 | 0x2234 | 0x223E | 0x2240 | 0x2242 => "Arctis Nova 5 Wireless",
        0x2236 => "Arctis 7+ Destiny Wireless",
        0x12B3 | 0x12B6 => "Arctis 1 Wireless",
        0x220E | 0x2212 | 0x2216 => "Arctis 7+ Wireless",
        0x1260 | 0x12AD | 0x12D5 | 0x12D7 => "Arctis 7 Wireless",
        0x12C2 => "Arctis 9 Wireless",
        0x1290 => "Arctis Pro Wireless",
        0x1624 | 0x1626 => "Apex Pro Mini Wireless",
        0x162C | 0x162E | 0x1630 => "Apex Pro TKL Wireless",
        0x1838 | 0x183A | 0x1852 | 0x1854 => "Aerox 3 Wireless",
        0x1858 | 0x185A => "Aerox 5 Wireless",
        0x185E | 0x1860 => "Aerox 9 Wireless",
        0x1842 => "Prime Wireless",
        0x1848 => "Prime Mini Wireless",
        0x1830 | 0x1832 => "Rival 3 Wireless",
        0x172B => "Rival 650 Wireless",
        _ => "SteelSeries Wireless",
    }
}

fn ss_device_kind(protocol: SsProtocol) -> WirelessDeviceKind {
    match protocol {
        SsProtocol::Mouse => WirelessDeviceKind::Mouse,
        SsProtocol::Keyboard => WirelessDeviceKind::Keyboard,
        SsProtocol::NovaPro
        | SsProtocol::Nova7
        | SsProtocol::NovaDirect
        | SsProtocol::Nova5
        | SsProtocol::Arctis7Plus
        | SsProtocol::ArctisLegacy
        | SsProtocol::Arctis9
        | SsProtocol::ArctisPro => WirelessDeviceKind::Headphones,
    }
}

/// Protocols whose request starts with the report id `0x00` followed by the command byte.
fn ss_uses_report_id_zero(protocol: SsProtocol) -> bool {
    matches!(
        protocol,
        SsProtocol::Nova7
            | SsProtocol::NovaDirect
            | SsProtocol::Nova5
            | SsProtocol::Arctis7Plus
            | SsProtocol::Arctis9
    )
}

/// First payload byte the reply echoes (after the Windows report-id byte).
fn ss_echo(protocol: SsProtocol) -> Option<u8> {
    match protocol {
        SsProtocol::Nova7
        | SsProtocol::NovaDirect
        | SsProtocol::Nova5
        | SsProtocol::Arctis7Plus => Some(0xB0),
        SsProtocol::NovaPro => None,
        SsProtocol::ArctisLegacy => Some(0x06),
        SsProtocol::Arctis9 => Some(0xAA),
        SsProtocol::ArctisPro => Some(0x40),
        SsProtocol::Mouse | SsProtocol::Keyboard => Some(0xD2),
    }
}

/// Strips the Windows report-id byte: 65-byte reads (64-byte report + id `0x00`) always,
/// shorter reads only when a `0x00` precedes the protocol's echo byte.
fn ss_payload(protocol: SsProtocol, raw: &[u8]) -> &[u8] {
    if raw.is_empty() {
        return raw;
    }
    if protocol == SsProtocol::NovaPro {
        // Report id 0x06/0x07 is part of the echo (`06 B0`).
        return if raw.len() >= 3 && raw[0] == 0 && matches!(raw[1], 0x06 | 0x07) {
            &raw[1..]
        } else {
            raw
        };
    }
    if raw[0] == 0x00
        && (raw.len() == 65 || ss_echo(protocol).is_some_and(|e| raw.get(1) == Some(&e)))
    {
        &raw[1..]
    } else {
        raw
    }
}

/// `prepend_zero` only applies to protocols whose first request byte is itself a report id
/// (`06 18`, `40 AA`, `D2`): on devices without numbered reports Windows wants a leading 0.
fn ss_build_request(protocol: SsProtocol, report_len: usize, prepend_zero: bool) -> Vec<u8> {
    let length = report_len.max(8);
    let mut buffer = vec![0u8; length];
    let offset = usize::from(prepend_zero);
    match protocol {
        SsProtocol::NovaPro => {
            buffer[0] = 0x06;
            buffer[1] = 0xB0;
        }
        SsProtocol::Nova7
        | SsProtocol::NovaDirect
        | SsProtocol::Nova5
        | SsProtocol::Arctis7Plus => {
            buffer[0] = 0x00;
            buffer[1] = 0xB0;
        }
        SsProtocol::Arctis9 => {
            buffer[0] = 0x00;
            buffer[1] = 0x20;
        }
        SsProtocol::ArctisLegacy => {
            buffer[offset] = 0x06;
            buffer[offset + 1] = 0x18;
        }
        SsProtocol::ArctisPro => {
            buffer[offset] = 0x40;
            buffer[offset + 1] = 0xAA;
        }
        SsProtocol::Mouse | SsProtocol::Keyboard => buffer[offset] = 0xD2,
    }
    buffer
}

fn ss_valid_packet(protocol: SsProtocol, raw: &[u8]) -> bool {
    let payload = ss_payload(protocol, raw);
    match protocol {
        SsProtocol::NovaPro => {
            payload.len() >= 16 && matches!(payload[0], 0x06 | 0x07) && payload[1] == 0xB0
        }
        SsProtocol::Nova7 | SsProtocol::NovaDirect => {
            payload.len() >= 4 && payload[0] == 0xB0 && payload[1] != 0x00
        }
        SsProtocol::Arctis7Plus => {
            payload.len() >= 4 && payload[0] == 0xB0 && matches!(payload[1], 0x01..=0x03)
        }
        SsProtocol::Nova5 => {
            payload.len() >= 5 && payload[0] == 0xB0 && matches!(payload[1], 0x01..=0x03)
        }
        SsProtocol::ArctisLegacy => payload.len() >= 4 && payload[0] == 0x06 && payload[1] == 0x18,
        SsProtocol::Arctis9 => {
            payload.len() >= 5
                && payload[0] == 0xAA
                && matches!(payload[1], 0x00 | 0x01)
                && (payload[1] == 0x00 || (0x64..=0x9A).contains(&payload[3]))
                && payload[4] <= 1
        }
        SsProtocol::ArctisPro => payload.len() >= 4 && payload[0] == 0x40 && payload[1] == 0xAA,
        SsProtocol::Mouse | SsProtocol::Keyboard => payload.len() >= 3 && payload[0] == 0xD2,
    }
}

fn ss_scale8(step: i32) -> f32 {
    percent_01((step.clamp(0, 8) * 100 / 8).max(0))
}

fn ss_scale4(step: i32) -> f32 {
    percent_01((step.clamp(0, 4) * 100 / 4).max(0))
}

fn ss_parse(protocol: SsProtocol, raw: &[u8]) -> Option<SsParsed> {
    if !ss_valid_packet(protocol, raw) {
        return None;
    }
    let reply = ss_payload(protocol, raw);
    match protocol {
        SsProtocol::NovaPro => {
            let headset = i32::from(reply[6]);
            let spare = reply[7];
            let status = reply[15];
            if headset > 8 || !matches!(status, 0x01 | 0x02 | 0x08) {
                return None;
            }
            Some(SsParsed {
                level: ss_scale8(headset),
                charging: status == 0x02,
                connected: matches!(status, 0x02 | 0x08),
                spare: (spare <= 8).then(|| ss_scale8(i32::from(spare))),
            })
        }
        SsProtocol::Nova7 | SsProtocol::NovaDirect => {
            let raw_level = i32::from(reply[2]);
            let level = if protocol == SsProtocol::NovaDirect {
                if raw_level > 100 {
                    return None;
                }
                percent_01(raw_level)
            } else {
                if raw_level > 4 {
                    return None;
                }
                ss_scale4(raw_level)
            };
            Some(SsParsed {
                level,
                charging: reply[3] == 0x01,
                connected: reply[1] == 0x03,
                spare: None,
            })
        }
        SsProtocol::Arctis7Plus => {
            let connected = match reply[1] {
                0x02 | 0x03 => true,
                0x01 => false,
                _ => return None,
            };
            let step = i32::from(reply[2]);
            if step > 4 {
                return None;
            }
            Some(SsParsed {
                level: ss_scale4(step),
                charging: reply[3] == 0x01,
                connected,
                spare: None,
            })
        }
        SsProtocol::Nova5 => {
            let connected = match reply[1] {
                0x01 | 0x03 => true,
                0x02 => false,
                _ => return None,
            };
            let level = i32::from(reply[3]);
            if level > 100 {
                return None;
            }
            Some(SsParsed {
                level: percent_01(level),
                charging: reply[4] == 0x01,
                connected,
                spare: None,
            })
        }
        SsProtocol::ArctisLegacy => {
            let level = i32::from(reply[2]);
            // Level 0 is what the dongle reports while the headset is off.
            if level == 0 || level > 100 {
                return None;
            }
            Some(SsParsed {
                level: percent_01(level),
                charging: false,
                connected: true,
                spare: None,
            })
        }
        SsProtocol::Arctis9 => {
            if reply[1] != 0x01 {
                return Some(SsParsed {
                    level: 0.0,
                    charging: false,
                    connected: false,
                    spare: None,
                });
            }
            let raw_level = i32::from(reply[3]);
            if !(0x64..=0x9A).contains(&raw_level) {
                return None;
            }
            let percent = ((raw_level - 0x64) * 100 + (0x9A - 0x64) / 2) / (0x9A - 0x64);
            Some(SsParsed {
                level: percent_01(percent.clamp(0, 100)),
                charging: reply[4] == 0x01,
                connected: true,
                spare: None,
            })
        }
        SsProtocol::ArctisPro => {
            let headset = i32::from(reply[2]);
            if headset > 4 {
                return None;
            }
            let spare = reply[3];
            Some(SsParsed {
                // Step 0 is a genuine "critical" headset level, not a disconnect.
                level: ss_scale4(headset),
                charging: false,
                connected: true,
                spare: (spare <= 4).then(|| ss_scale4(i32::from(spare))),
            })
        }
        SsProtocol::Mouse | SsProtocol::Keyboard => {
            let raw = reply[1];
            let level = i32::from(raw & 0x7F);
            if level == 0 || level > 100 {
                return None;
            }
            Some(SsParsed {
                level: percent_01(level),
                charging: raw & 0x80 != 0,
                connected: true,
                spare: None,
            })
        }
    }
}

// ---------------------------------------------------------------------------
// Sağlayıcı 1: XInput (xinput1_4.dll, dinamik yükleme)
// ---------------------------------------------------------------------------

#[repr(C)]
#[derive(Default)]
struct XInputGamepad {
    buttons: u16,
    left_trigger: u8,
    right_trigger: u8,
    thumb_lx: i16,
    thumb_ly: i16,
    thumb_rx: i16,
    thumb_ry: i16,
}

#[repr(C)]
#[derive(Default)]
struct XInputState {
    packet_number: u32,
    gamepad: XInputGamepad,
}

#[repr(C)]
#[derive(Default)]
struct XInputBatteryInformation {
    battery_type: u8,
    battery_level: u8,
}

type XInputGetStateFn = unsafe extern "system" fn(u32, *mut XInputState) -> u32;
type XInputGetBatteryFn = unsafe extern "system" fn(u32, u8, *mut XInputBatteryInformation) -> u32;

fn get_proc(module: Handle, name: &str) -> Option<*const c_void> {
    let symbol = ascii_z(name);
    let address = unsafe { GetProcAddressWb(module, symbol.as_ptr()) };
    if address.is_null() {
        None
    } else {
        Some(address)
    }
}

fn probe_xinput(cancel: &AtomicBool) -> Vec<WirelessReading> {
    let mut module = unsafe { LoadLibraryW(wide("xinput1_4.dll").as_ptr()) };
    if invalid(module) {
        module = unsafe { LoadLibraryW(wide("xinput1_3.dll").as_ptr()) };
    }
    if invalid(module) {
        return Vec::new();
    }
    let (Some(state_proc), Some(battery_proc)) = (
        get_proc(module, "XInputGetState"),
        get_proc(module, "XInputGetBatteryInformation"),
    ) else {
        unsafe { FreeLibrary(module) };
        return Vec::new();
    };
    let get_state: XInputGetStateFn = unsafe { std::mem::transmute_copy(&state_proc) };
    let get_battery: XInputGetBatteryFn = unsafe { std::mem::transmute_copy(&battery_proc) };
    let mut result = Vec::new();
    for slot in 0..4u32 {
        if cancel.load(Ordering::Relaxed) {
            break;
        }
        let mut state = XInputState::default();
        if unsafe { get_state(slot, &mut state) } != 0 {
            continue;
        }
        let mut info = XInputBatteryInformation::default();
        if unsafe { get_battery(slot, 0, &mut info) } != 0 {
            continue;
        }
        // 0x02 = alkaline, 0x03 = NiMH; kablü (0x01) ve bilinmeyen (0xFF) atlanır.
        if !matches!(info.battery_type, 0x02 | 0x03) {
            continue;
        }
        let Some(level) = xinput_level_01(info.battery_level) else {
            continue;
        };
        result.push(reading(
            format!("xinput:slot:{slot}"),
            format!("Xbox Wireless Controller ({})", slot + 1),
            level,
            true,
            false,
            BatterySource::XInput,
        ));
    }
    unsafe { FreeLibrary(module) };
    result
}

// ---------------------------------------------------------------------------
// Sağlayıcı 2: standart HID pil capacity (yalnızca okuma)
// ---------------------------------------------------------------------------

fn read_hid_capacity(
    device: &HidDevice,
    session: &HidSession,
    cap: &BatteryValueCap,
) -> Option<(f32, bool)> {
    let report_len = if cap.is_feature {
        device.feature_len.max(2)
    } else {
        device.input_len.max(2)
    };
    if !(2..=512).contains(&report_len) {
        return None;
    }
    let mut buffer = vec![0u8; report_len as usize];
    buffer[0] = cap.report_id;
    let read_ok = if cap.is_feature {
        session.get_feature(&mut buffer)
    } else {
        session.get_input_report(&mut buffer)
    };
    if !read_ok {
        return None;
    }
    // Descriptor extraction is mandatory: when `HidP_GetUsageValue` fails the cap is
    // skipped (the caller tries the next suitable cap) instead of guessing a byte.
    let (percent, _) = session
        .extract_usage_value(
            cap.is_feature,
            cap.usage_page,
            cap.link_collection,
            cap.usage,
            &buffer,
        )
        .and_then(|value| direct_byte_report(value, cap))?;
    // Prefer the Battery System `Charging` flag; `ACPresent` is only a fallback hint.
    let mut charge_caps: Vec<&BatteryValueCap> = device
        .battery_caps
        .iter()
        .filter(|c| {
            is_charging_cap(c) && c.is_feature == cap.is_feature && c.report_id == cap.report_id
        })
        .collect();
    charge_caps.sort_by_key(|c| u8::from(c.usage_page != 0x0085));
    let charging = charge_caps.iter().find_map(|charge_cap| {
        session
            .extract_usage_value(
                charge_cap.is_feature,
                charge_cap.usage_page,
                charge_cap.link_collection,
                charge_cap.usage,
                &buffer,
            )
            .map(|value| value != 0)
    });
    Some((percent_01(percent), charging.unwrap_or(false)))
}

fn probe_standard_hid(
    devices: &[HidDevice],
    cancel: &AtomicBool,
    source: BatterySource,
    filter: impl Fn(&HidDevice) -> bool,
) -> Vec<WirelessReading> {
    let mut result = Vec::new();
    // One reading per physical device: the several HID collections of one device share
    // the container identity, while two devices with the same VID/PID do not.
    let mut seen = HashSet::new();
    for device in devices {
        if cancel.load(Ordering::Relaxed) {
            break;
        }
        if !filter(device) || is_ignored_wired_product(&device.product) {
            continue;
        }
        if seen.contains(&device.id("hid")) {
            continue;
        }
        if !device.battery_caps.iter().any(is_capacity_cap) {
            continue;
        }
        let Some(session) = HidSession::open(&device.path, true) else {
            continue;
        };
        let Some((level, charging)) = device
            .battery_caps
            .iter()
            .filter(|cap| is_capacity_cap(cap))
            .find_map(|cap| read_hid_capacity(device, &session, cap))
        else {
            continue;
        };
        let name = if device.product.trim().is_empty() {
            format!("HID Wireless ({:04X}:{:04X})", device.vid, device.pid)
        } else {
            device.product.trim().to_owned()
        };
        let prefix = match source {
            BatterySource::AsusRogHid => "rog",
            BatterySource::CorsairHid => "corsair",
            BatterySource::HyperXHid => "hyperx",
            BatterySource::GamepadHid => "gamepad",
            BatterySource::CompxAtkVxe => "compx",
            BatterySource::Keyboard24gHid => "keyboard24g",
            _ => "hid",
        };
        let id = device.id(prefix);
        let kind = super::wireless_vendors_ext::classify_wireless_device_kind(
            device.vid, device.pid, &name, source, &id,
        );
        seen.insert(device.id("hid"));
        result.push(reading_with_meta(
            id, name, level, true, charging, source, kind, None,
        ));
    }
    result
}

// ---------------------------------------------------------------------------
// Sağlayıcı 3: Logitech HID++ 2.0
// ---------------------------------------------------------------------------

fn hidpp_matches(device: &HidDevice) -> bool {
    device.vid == 0x046D
        && device.usage_page == 0xFF00
        && (matches!(device.input_len, 7 | 20) || matches!(device.output_len, 7 | 20))
}

fn hidpp_transact(
    session: &HidSession,
    request: &[u8],
    response: &mut [u8],
    device_index: u8,
    expected_feature: u8,
    _budget: &Budget,
    cancel: &AtomicBool,
) -> Option<usize> {
    let validator = |packet: &[u8]| {
        packet.len() >= 4
            && matches!(packet[0], HIDPP_SHORT_REPORT | HIDPP_LONG_REPORT)
            && packet[1] == device_index
            && ((packet[2] == expected_feature && packet[3] == request[3])
                || (hidpp_is_error(packet) && packet.get(3..5) == request.get(2..4)))
    };
    let len = session.transact(request, response, 180, validator, cancel)?;
    if hidpp_is_error(&response[..len]) {
        None
    } else {
        Some(len)
    }
}

fn hidpp_resolve_feature(
    session: &HidSession,
    device_index: u8,
    feature: u16,
    report_len: usize,
    budget: &Budget,
    cancel: &AtomicBool,
) -> Option<u8> {
    let request = hidpp_root_request(device_index, feature, report_len);
    let mut response = vec![0u8; report_len.max(20)];
    let len = hidpp_transact(
        session,
        &request,
        &mut response,
        device_index,
        0x00,
        budget,
        cancel,
    )?;
    hidpp_parse_feature_index(&response[..len], device_index)
}

#[allow(clippy::too_many_arguments)]
fn hidpp_query_feature<T>(
    session: &HidSession,
    device_index: u8,
    feature: u16,
    function: u8,
    report_len: usize,
    response_len: usize,
    budget: &Budget,
    cancel: &AtomicBool,
    parse: fn(&[u8], u8, u8) -> Option<T>,
) -> Option<T> {
    let index = hidpp_resolve_feature(session, device_index, feature, report_len, budget, cancel)?;
    let request = hidpp_feature_request(device_index, index, function, &[0, 0, 0], report_len);
    let mut response = vec![0u8; response_len];
    let len = hidpp_transact(
        session,
        &request,
        &mut response,
        device_index,
        index,
        budget,
        cancel,
    )?;
    parse(&response[..len], device_index, index)
}

/// HID++ 2.0 feature `0x0005` (DeviceName): function 0 returns the name length, function 1
/// returns the next chunk starting at the given character index.
fn hidpp_query_name(
    session: &HidSession,
    device_index: u8,
    report_len: usize,
    response_len: usize,
    budget: &Budget,
    cancel: &AtomicBool,
) -> Option<String> {
    let index = hidpp_resolve_feature(
        session,
        device_index,
        HIDPP_FEATURE_DEVICE_NAME,
        report_len,
        budget,
        cancel,
    )?;
    let mut response = vec![0u8; response_len.max(report_len)];
    let request = hidpp_feature_request(device_index, index, 0x00, &[0, 0, 0], report_len);
    let len = hidpp_transact(
        session,
        &request,
        &mut response,
        device_index,
        index,
        budget,
        cancel,
    )?;
    if len < 5 {
        return None;
    }
    let count = usize::from(response[4]).min(64);
    let mut bytes = Vec::with_capacity(count);
    while bytes.len() < count {
        if budget.expired(cancel) {
            return None;
        }
        let request = hidpp_feature_request(
            device_index,
            index,
            0x01,
            &[bytes.len() as u8, 0, 0],
            report_len,
        );
        let len = hidpp_transact(
            session,
            &request,
            &mut response,
            device_index,
            index,
            budget,
            cancel,
        )?;
        let chunk = response.get(4..len)?;
        if chunk.is_empty() {
            break;
        }
        let take = (count - bytes.len()).min(chunk.len());
        bytes.extend_from_slice(&chunk[..take]);
    }
    let name = String::from_utf8_lossy(&bytes)
        .trim_matches(char::from(0))
        .trim()
        .to_owned();
    (!name.is_empty()).then_some(name)
}

fn probe_logitech(devices: &[HidDevice], cancel: &AtomicBool) -> Vec<WirelessReading> {
    let budget = Budget::new(850);
    let mut result = Vec::new();
    let mut seen = HashSet::new();
    for device in devices.iter().filter(|d| hidpp_matches(d)) {
        if budget.expired(cancel) {
            break;
        }
        let report_len = if device.output_len == 20 || device.input_len == 20 {
            20
        } else {
            7
        };
        let Some(session) = HidSession::open(&device.path, false) else {
            continue;
        };
        for device_index in [0xFFu8, 0x01, 0x02, 0x03, 0x04, 0x05, 0x06] {
            if budget.expired(cancel) {
                break;
            }
            let key = (device.id("receiver"), device_index);
            if seen.contains(&key) {
                continue;
            }
            let response_len = device.input_len.max(report_len as u16) as usize;
            let level = hidpp_query_feature(
                &session,
                device_index,
                HIDPP_FEATURE_UNIFIED_BATTERY,
                0x01,
                report_len,
                response_len,
                &budget,
                cancel,
                hidpp_parse_unified,
            )
            .or_else(|| {
                hidpp_query_feature(
                    &session,
                    device_index,
                    HIDPP_FEATURE_BATTERY_STATUS,
                    0x00,
                    report_len,
                    response_len,
                    &budget,
                    cancel,
                    hidpp_parse_status,
                )
            })
            .or_else(|| {
                hidpp_query_feature(
                    &session,
                    device_index,
                    HIDPP_FEATURE_BATTERY_VOLTAGE,
                    0x00,
                    report_len,
                    response_len,
                    &budget,
                    cancel,
                    hidpp_parse_voltage,
                )
                .map(|(percent, is_charging, _)| (percent, is_charging))
            });
            if let Some((percent, is_charging)) = level {
                // Marked seen only after a successful decode; keyed by receiver identity +
                // slot so two paired devices behind one receiver stay distinct.
                seen.insert(key);
                // A receiver's product string ("USB Receiver") is not the paired device:
                // ask the device itself (feature 0x0005 GetDeviceName).
                let paired_name = hidpp_query_name(
                    &session,
                    device_index,
                    report_len,
                    response_len,
                    &budget,
                    cancel,
                );
                let name = paired_name.unwrap_or_else(|| {
                    if device.product.trim().is_empty() {
                        "Logitech Wireless".to_owned()
                    } else {
                        device.product.trim().to_owned()
                    }
                });
                let id = format!("{}:{device_index:02X}", device.id("logitech"));
                let kind = super::wireless_vendors_ext::classify_wireless_device_kind(
                    device.vid,
                    device.pid,
                    &name,
                    BatterySource::LogitechHidPlusPlus,
                    &id,
                );
                result.push(reading_with_meta(
                    id,
                    name,
                    percent,
                    true,
                    is_charging,
                    BatterySource::LogitechHidPlusPlus,
                    kind,
                    None,
                ));
            }
        }
    }
    result
}

// ---------------------------------------------------------------------------
// Sağlayıcı 4: Razer 90 bayt feature report
// ---------------------------------------------------------------------------

/// One correlated Razer feature exchange: the reply must carry the request's transaction id,
/// the power command class, the requested command, success status and a valid CRC.
fn razer_query(
    session: &HidSession,
    command: u8,
    transaction_id: u8,
    buffer_len: usize,
) -> Option<u8> {
    let mut request = razer_build_command(command, transaction_id, buffer_len >= RAZER_BUFFER_LEN);
    request.resize(buffer_len, 0);
    if !session.set_feature(&request) {
        return None;
    }
    unsafe { Sleep(15) };
    let mut response = vec![0u8; buffer_len];
    if !session.get_feature(&mut response) {
        return None;
    }
    if razer_reply_tx_id(&response)? != transaction_id {
        return None;
    }
    razer_parse_reply(&response, command)
}

fn probe_razer(devices: &[HidDevice], cancel: &AtomicBool) -> Vec<WirelessReading> {
    let mut result = Vec::new();
    // Keyed by physical identity: several Razer collections of one device (or a dongle
    // answering for every transaction id) never produce duplicate rows.
    let mut seen: HashSet<String> = HashSet::new();
    for device in devices
        .iter()
        .filter(|d| d.vid == 0x1532 && d.feature_len >= 90)
    {
        if cancel.load(Ordering::Relaxed) {
            break;
        }
        let id = device.id("razer");
        if seen.contains(&id) {
            continue;
        }
        let Some(session) = HidSession::open(&device.path, true) else {
            continue;
        };
        let buffer_len = (device.feature_len as usize).max(RAZER_BUFFER_LEN);
        // The transaction id selects the device slot (wired / HyperSpeed dongle variants);
        // the first slot that answers a correlated reply is this device's reading.
        for transaction_id in [0x1Fu8, 0x3F, 0xFF, 0x7F, 0x00] {
            if cancel.load(Ordering::Relaxed) {
                break;
            }
            let Some(raw_level) =
                razer_query(&session, RAZER_CMD_BATTERY, transaction_id, buffer_len)
            else {
                continue;
            };
            let charging = razer_query(&session, RAZER_CMD_CHARGING, transaction_id, buffer_len)
                .is_some_and(|value| value != 0);
            let name = if device.product.trim().is_empty() {
                "Razer Wireless".to_owned()
            } else {
                device.product.trim().to_owned()
            };
            let kind = super::wireless_vendors_ext::classify_wireless_device_kind(
                device.vid,
                device.pid,
                &name,
                BatterySource::RazerHid,
                &id,
            );
            seen.insert(id.clone());
            // A successful, CRC-valid reply of 0 is a genuine empty battery (0%).
            result.push(reading_with_meta(
                id.clone(),
                name,
                razer_level_01(raw_level),
                true,
                charging,
                BatterySource::RazerHid,
                kind,
                None,
            ));
            break;
        }
    }
    result
}

// ---------------------------------------------------------------------------
// Sağlayıcı 5: SteelSeries HID
// ---------------------------------------------------------------------------

fn probe_steelseries(devices: &[HidDevice], cancel: &AtomicBool) -> Vec<WirelessReading> {
    let budget = Budget::new(1000);
    let mut result = Vec::new();
    let mut seen = HashSet::new();
    for device in devices {
        if budget.expired(cancel) {
            break;
        }
        // 0xFF43:0x0202 is the original (2017) Arctis 7 control collection.
        if device.vid != 0x1038
            || !matches!(device.usage_page, 0xFFC0 | 0xFF00 | 0xFF02 | 0xFF43)
            || !(8..=65).contains(&device.input_len)
            || !(8..=65).contains(&device.output_len)
        {
            continue;
        }
        let Some(protocol) = ss_protocol_for_pid(device.pid) else {
            continue;
        };
        if device.usage_page == 0xFF43
            && (protocol != SsProtocol::ArctisLegacy || device.usage != 0x0202)
        {
            continue;
        }
        if seen.contains(&device.id("hid")) {
            continue;
        }
        let Some(session) = HidSession::open(&device.path, false) else {
            continue;
        };
        let out_len = device.output_len as usize;
        let in_len = (device.input_len as usize).max(8);
        let try_query = |prepend_zero: bool| {
            drain_input_reports(&session, in_len);
            let request = ss_build_request(protocol, out_len, prepend_zero);
            let mut response = vec![0u8; in_len];
            session
                .transact(
                    &request,
                    &mut response,
                    250,
                    |packet| ss_valid_packet(protocol, packet),
                    cancel,
                )
                .and_then(|len| ss_parse(protocol, &response[..len]))
        };
        // `00 B0` / `00 20` already lead with report id 0; the other commands are tried as a
        // numbered report first and with a leading report id 0 second.
        let parsed = if ss_uses_report_id_zero(protocol) || protocol == SsProtocol::NovaPro {
            try_query(false)
        } else {
            try_query(false).or_else(|| try_query(true))
        };
        let Some(parsed) = parsed else { continue };
        if !parsed.connected {
            continue;
        }
        let name = if device.product.trim().is_empty() {
            ss_default_name(device.pid).to_owned()
        } else {
            device.product.trim().to_owned()
        };
        seen.insert(device.id("hid"));
        result.push(reading_with_meta(
            device.id("steelseries"),
            name,
            parsed.level,
            parsed.connected,
            parsed.charging,
            BatterySource::SteelSeriesHid,
            ss_device_kind(protocol),
            parsed.spare,
        ));
    }
    result
}

// ---------------------------------------------------------------------------
// Sağlayıcı 6: BLE GATT (WinRT yerine kayıt defteri önbelleği fallback'i)
// ---------------------------------------------------------------------------

struct RegKey(Handle);

impl RegKey {
    fn open(root: Handle, path: &str) -> Option<Self> {
        let mut key: Handle = 0;
        let status = unsafe { RegOpenKeyExW(root, wide(path).as_ptr(), 0, KEY_READ, &mut key) };
        if status == 0 && !invalid(key) {
            Some(Self(key))
        } else {
            None
        }
    }

    fn raw(&self) -> Handle {
        self.0
    }
}

impl Drop for RegKey {
    fn drop(&mut self) {
        unsafe { RegCloseKey(self.0) };
    }
}

fn reg_query_raw(key: Handle, name: &str, buffer: &mut [u8]) -> Option<(u32, usize)> {
    let mut size = buffer.len() as u32;
    let mut kind = 0u32;
    // SAFETY: owned buffer outlives the call; name is NUL-terminated.
    let status = unsafe {
        RegQueryValueExW(
            key,
            wide(name).as_ptr(),
            std::ptr::null_mut(),
            &mut kind,
            buffer.as_mut_ptr(),
            &mut size,
        )
    };
    (status == 0).then_some((kind, (size as usize).min(buffer.len())))
}

fn reg_dword(key: Handle, name: &str) -> Option<u32> {
    let mut buf = [0u8; 4];
    let (kind, size) = reg_query_raw(key, name, &mut buf)?;
    (kind == REG_DWORD && size == 4).then(|| u32::from_le_bytes(buf))
}

fn reg_string(key: Handle, name: &str) -> Option<String> {
    let mut buffer = [0u8; 1024];
    let (kind, size) = reg_query_raw(key, name, &mut buffer)?;
    if kind != REG_SZ {
        return None;
    }
    let text = utf16_from_bytes(&buffer[..size]);
    (!text.is_empty()).then_some(text)
}

fn reg_subkeys(key: Handle) -> Vec<String> {
    let mut names = Vec::new();
    for index in 0..128u32 {
        let mut buffer = vec![0u16; 256];
        let mut size = buffer.len() as u32;
        let status = unsafe {
            RegEnumKeyExW(
                key,
                index,
                buffer.as_mut_ptr(),
                &mut size,
                std::ptr::null_mut(),
                std::ptr::null_mut(),
                std::ptr::null_mut(),
                std::ptr::null_mut(),
            )
        };
        if status != 0 {
            break;
        }
        let end = (size as usize).min(buffer.len());
        names.push(String::from_utf16_lossy(&buffer[..end]));
    }
    names
}

/// Last-known value from the Bluetooth registry cache. The value is a snapshot written by
/// the stack at some unknown earlier time, so it is always provenance `stale` and never
/// claims a live connection.
fn ble_registry_reading(key: Handle, id_hint: &str) -> Option<WirelessReading> {
    let percent = [
        "BatteryLevel",
        "BatteryPercent",
        "BatteryPercentage",
        "Percentage",
    ]
    .iter()
    .find_map(|name| reg_percent(key, name))?;
    let name = ["FriendlyName", "Name", "DisplayName"]
        .iter()
        .find_map(|name| reg_string(key, name))
        .unwrap_or_else(|| format!("BLE Device ({id_hint})"));
    if is_ignored_wired_device(&name) {
        return None;
    }
    // BTHPORT device sub-keys are the 12-hex-digit Bluetooth address.
    let id = u64::from_str_radix(id_hint.trim(), 16)
        .ok()
        .filter(|_| id_hint.trim().len() == 12)
        .map(bt_physical_id)
        .unwrap_or_else(|| format!("ble:{:08X}", fnv1a_u32(id_hint.as_bytes())));
    let mut item = reading(
        id,
        name,
        percent_01(percent),
        false,
        false,
        BatterySource::BluetoothGatt,
    );
    item.stale = true;
    Some(item)
}

/// Stable physical Bluetooth identity shared by the GATT and PnP providers.
pub(crate) fn bt_physical_id(address: u64) -> String {
    format!("bt:{:012X}", address & 0xFFFF_FFFF_FFFF)
}

/// Extracts canonical `bt:<MAC12_UPPER>` from a PnP Bluetooth instance id when a 48-bit
/// address is embedded (`DEV_<12 hex>` or `...&<12 hex>_...`).
pub(crate) fn bt_id_from_instance(instance: &str) -> Option<String> {
    bt_address_from_instance(instance).map(bt_physical_id)
}

/// Extracts the 48-bit Bluetooth address embedded in a PnP instance id
/// (`BTHLE\DEV_A0B1C2D3E4F5\...`, `BTHENUM\{...}_LOCALMFG&005D\7&..&0&A0B1C2D3E4F5_C00000000`).
pub(crate) fn bt_address_from_instance(instance: &str) -> Option<u64> {
    let upper = instance.to_ascii_uppercase();
    let parse12 = |text: &str| -> Option<u64> {
        let hex = text.get(..12)?;
        if !hex.bytes().all(|b| b.is_ascii_hexdigit()) {
            return None;
        }
        if text.len() > 12 && text.as_bytes()[12].is_ascii_hexdigit() {
            return None;
        }
        u64::from_str_radix(hex, 16).ok().filter(|&a| a != 0)
    };
    if let Some(at) = upper.find("DEV_")
        && let Some(address) = parse12(&upper[at + 4..])
    {
        return Some(address);
    }
    let last = upper.rsplit('\\').next()?;
    let tail = last.rsplit('&').next()?;
    parse12(tail.split('_').next()?)
}

/// Bounded wait for a WinRT async operation without blocking indefinitely: polls `Status()`
/// and cancels the operation on timeout or provider cancellation. Must be used inside a
/// closure returning `windows::core::Result`.
macro_rules! winrt_wait {
    ($op:expr, $cancel:expr, $timeout_ms:expr) => {{
        let op = $op;
        let deadline = Instant::now() + Duration::from_millis($timeout_ms);
        loop {
            // AsyncStatus: 0 = Started, 1 = Completed, 2 = Canceled, 3 = Error.
            if op.Status()?.0 != 0 {
                break;
            }
            if $cancel.load(Ordering::Relaxed) || Instant::now() >= deadline {
                let _ = op.Cancel();
                return Err(windows::core::Error::from_hresult(windows::core::HRESULT(
                    0x8000_4004u32 as i32,
                )));
            }
            std::thread::sleep(Duration::from_millis(5));
        }
        op.GetResults()?
    }};
}

/// Calls WinRT `IClosable::Close()` on drop so early `?`, `cancel`, or timeout exits never
/// leak open OS Bluetooth or GATT sessions.
struct CloseGuard<T, F: FnOnce(&T)> {
    inner: T,
    close: Option<F>,
}

impl<T, F: FnOnce(&T)> CloseGuard<T, F> {
    fn new(inner: T, close: F) -> Self {
        Self {
            inner,
            close: Some(close),
        }
    }
}

impl<T, F: FnOnce(&T)> std::ops::Deref for CloseGuard<T, F> {
    type Target = T;

    fn deref(&self) -> &Self::Target {
        &self.inner
    }
}

impl<T, F: FnOnce(&T)> Drop for CloseGuard<T, F> {
    fn drop(&mut self) {
        if let Some(close) = self.close.take() {
            close(&self.inner);
        }
    }
}

/// Reads every Battery Service (0x180F) instance of each *connected* BLE device with an
/// uncached over-the-air read of Battery Level (0x2A19). Each device is isolated: a WinRT
/// error on one device (it vanished, access denied, timeout) skips only that device. Every
/// validated reading is published through `emit` as soon as its device completes, so one
/// stalled peripheral never discards the others.
///
/// Returns `None` only when WinRT / Bluetooth enumeration itself is unavailable (the caller
/// may then fall back to the stale registry cache), `Some(count)` otherwise.
fn ble_gatt_readings(cancel: &AtomicBool, emit: &dyn Fn(WirelessReading)) -> Option<usize> {
    use windows::Devices::Bluetooth::GenericAttributeProfile::{
        GattCommunicationStatus, GattDeviceService,
    };
    use windows::Devices::Bluetooth::{
        BluetoothCacheMode, BluetoothConnectionStatus, BluetoothLEDevice,
    };
    use windows::Devices::Enumeration::DeviceInformation;
    use windows::Storage::Streams::DataReader;
    use windows::Win32::System::Com::{COINIT_MULTITHREADED, CoInitializeEx};
    use windows::core::GUID;

    const BATTERY_SERVICE: GUID = GUID::from_u128(0x0000180f_0000_1000_8000_00805f9b34fb);
    const BATTERY_LEVEL: GUID = GUID::from_u128(0x00002a19_0000_1000_8000_00805f9b34fb);

    let hr = unsafe { CoInitializeEx(None, COINIT_MULTITHREADED) };
    if hr.is_err() && hr.0 != 0x8001_0106u32 as i32 {
        return None;
    }
    let initialized = hr.is_ok();

    let result = (|| -> windows::core::Result<usize> {
        let selector = BluetoothLEDevice::GetDeviceSelector()?;
        let devices = winrt_wait!(
            DeviceInformation::FindAllAsyncAqsFilter(&selector)?,
            cancel,
            800
        );
        let mut published = 0usize;
        for info in devices {
            if cancel.load(Ordering::Relaxed) {
                break;
            }
            // Every WinRT call below is per-device: an error ends this device only.
            let per_device = || -> windows::core::Result<Vec<WirelessReading>> {
                let device_id = info.Id()?;
                let mut name = info.Name().map(|n| n.to_string_lossy()).unwrap_or_default();
                let device = CloseGuard::new(
                    winrt_wait!(BluetoothLEDevice::FromIdAsync(&device_id)?, cancel, 500),
                    |d: &BluetoothLEDevice| {
                        let _ = d.Close();
                    },
                );
                if device.ConnectionStatus()? != BluetoothConnectionStatus::Connected {
                    return Ok(Vec::new());
                }
                if name.trim().is_empty() {
                    name = device
                        .Name()
                        .map(|n| n.to_string_lossy())
                        .unwrap_or_default();
                }
                if name.trim().is_empty() {
                    name = "Bluetooth LE Device".to_owned();
                }
                let address = device.BluetoothAddress().unwrap_or(0);
                let services = winrt_wait!(
                    device.GetGattServicesForUuidWithCacheModeAsync(
                        BATTERY_SERVICE,
                        BluetoothCacheMode::Uncached
                    )?,
                    cancel,
                    700
                );
                if services.Status()? != GattCommunicationStatus::Success {
                    return Ok(Vec::new());
                }
                // Guard every returned GattDeviceService up front so an early `break` on
                // cancellation or any error still closes all open OS GATT sessions.
                let gatt_services: Vec<_> = services
                    .Services()?
                    .into_iter()
                    .map(|service| {
                        CloseGuard::new(service, |s: &GattDeviceService| {
                            let _ = s.Close();
                        })
                    })
                    .collect();
                // (attribute handle, level) for every Battery Service instance.
                let mut levels: Vec<(u16, u8)> = Vec::new();
                for service in gatt_services {
                    if cancel.load(Ordering::Relaxed) {
                        break;
                    }
                    let handle = service.AttributeHandle().unwrap_or(u16::MAX);
                    let level = (|| -> windows::core::Result<Option<u8>> {
                        let characteristics = winrt_wait!(
                            service.GetCharacteristicsForUuidWithCacheModeAsync(
                                BATTERY_LEVEL,
                                BluetoothCacheMode::Uncached
                            )?,
                            cancel,
                            500
                        );
                        if characteristics.Status()? != GattCommunicationStatus::Success {
                            return Ok(None);
                        }
                        for characteristic in characteristics.Characteristics()? {
                            let read = (|| -> windows::core::Result<Option<u8>> {
                                let value = winrt_wait!(
                                    characteristic.ReadValueWithCacheModeAsync(
                                        BluetoothCacheMode::Uncached
                                    )?,
                                    cancel,
                                    500
                                );
                                if value.Status()? != GattCommunicationStatus::Success {
                                    return Ok(None);
                                }
                                let buffer = value.Value()?;
                                // Battery Level is exactly one uint8 in 0..=100.
                                if buffer.Length()? != 1 {
                                    return Ok(None);
                                }
                                let reader = DataReader::FromBuffer(&buffer)?;
                                let level = reader.ReadByte()?;
                                Ok((level <= 100).then_some(level))
                            })();
                            if let Ok(Some(level)) = read {
                                return Ok(Some(level));
                            }
                        }
                        Ok(None)
                    })();
                    if let Ok(Some(level)) = level {
                        levels.push((handle, level));
                    }
                }
                if levels.is_empty() || is_ignored_wired_device(&name) {
                    return Ok(Vec::new());
                }
                levels.sort_by_key(|(handle, _)| *handle);
                let base_id = if address != 0 {
                    bt_physical_id(address)
                } else {
                    format!(
                        "ble-gatt:{:08X}",
                        fnv1a_u32(device_id.to_string_lossy().as_bytes())
                    )
                };
                let dev_id_str = device_id.to_string_lossy();
                let kind = super::wireless_vendors_ext::classify_wireless_device_kind(
                    0,
                    0,
                    &name,
                    BatterySource::BluetoothGatt,
                    &dev_id_str,
                );
                let multi = levels.len() > 1;
                Ok(levels
                    .into_iter()
                    .enumerate()
                    .map(|(component, (_, level))| {
                        // Multi-battery peripherals (split earbuds, keyboard + dock) expose
                        // one Battery Service per component: one row each, primary first.
                        let (id, label) = if component == 0 {
                            (
                                base_id.clone(),
                                if multi {
                                    format!("{name} (1)")
                                } else {
                                    name.clone()
                                },
                            )
                        } else {
                            (
                                format!("{base_id}#{}", component + 1),
                                format!("{name} ({})", component + 1),
                            )
                        };
                        reading_with_meta(
                            id,
                            label,
                            percent_01(i32::from(level)),
                            true,
                            false,
                            BatterySource::BluetoothGatt,
                            kind,
                            None,
                        )
                    })
                    .collect())
            };
            if let Ok(items) = per_device() {
                for item in items {
                    published += 1;
                    emit(item);
                }
            }
        }
        Ok(published)
    })();
    if initialized {
        unsafe { windows::Win32::System::Com::CoUninitialize() };
    }
    result.ok()
}

/// BLE GATT provider. Live readings are emitted progressively by [`ble_gatt_readings`];
/// the returned vector only ever holds the stale registry fallback used when WinRT itself
/// is unavailable.
fn probe_ble_gatt(cancel: &AtomicBool, emit: &dyn Fn(WirelessReading)) -> Vec<WirelessReading> {
    if ble_gatt_readings(cancel, emit).is_some() {
        return Vec::new();
    }
    let mut result = Vec::new();
    let roots = [
        (
            HKEY_CURRENT_USER,
            r"Software\Microsoft\Windows\CurrentVersion\Bluetooth\Devices",
        ),
        (
            HKEY_LOCAL_MACHINE,
            r"SYSTEM\CurrentControlSet\Services\BTHPORT\Parameters\Devices",
        ),
    ];
    for (root, path) in roots {
        if cancel.load(Ordering::Relaxed) {
            break;
        }
        let Some(key) = RegKey::open(root, path) else {
            continue;
        };
        for subkey in reg_subkeys(key.raw()) {
            if cancel.load(Ordering::Relaxed) {
                break;
            }
            if let Some(child) = RegKey::open(key.raw(), &subkey)
                && let Some(item) = ble_registry_reading(child.raw(), &subkey)
            {
                result.push(item);
            }
        }
    }
    result
}

// ---------------------------------------------------------------------------
// WS-4: Bluetooth PnP reader + G-Hub WebSocket / SteelSeries coreProps loopback
// ---------------------------------------------------------------------------

/// Accepts only an explicitly loopback WebSocket/HTTP endpoint for the given port.
fn parse_loopback_endpoint(uri: &str, expected_port: u16, allow_websocket: bool) -> Option<String> {
    let lower = uri.to_ascii_lowercase();
    let scheme_end = lower.find("://")?;
    let scheme = &lower[..scheme_end];
    if !(scheme == "http" || (allow_websocket && scheme == "ws")) {
        return None;
    }
    let rest = &lower[scheme_end + 3..];
    let authority = rest.split('/').next()?.split('?').next()?;
    let (host, port) = match authority.rsplit_once(':') {
        Some((host, port)) => (host, port.parse::<u16>().ok()?),
        None => (authority, 80),
    };
    if port != expected_port || !matches!(host, "127.0.0.1" | "localhost" | "[::1]") {
        return None;
    }
    Some(format!("{host}:{port}"))
}

/// Reads a 0..100 battery value from a registry value. Only the two exact encodings the
/// Bluetooth stack writes are accepted: a 1-byte `REG_BINARY` or a 4-byte `REG_DWORD`.
/// Arbitrary-length blobs (whose first byte is not a percentage) are rejected.
fn reg_percent(key: Handle, name: &str) -> Option<i32> {
    let mut buffer = [0u8; 64];
    let (kind, size) = reg_query_raw(key, name, &mut buffer)?;
    let value = match kind {
        REG_DWORD if size == 4 => i64::from(u32::from_le_bytes([
            buffer[0], buffer[1], buffer[2], buffer[3],
        ])),
        REG_BINARY if size == 1 => i64::from(buffer[0]),
        _ => return None,
    };
    (0..=100).contains(&value).then_some(value as i32)
}

fn setupdi_prop_string(
    info_set: Handle,
    devinfo: &SpDevinfoData,
    key: &DevPropKey,
) -> Option<String> {
    let mut prop_type = 0u32;
    let mut required = 0u32;
    let mut buffer = [0u8; 512];
    let ok = unsafe {
        SetupDiGetDevicePropertyW(
            info_set,
            devinfo,
            key,
            &mut prop_type,
            buffer.as_mut_ptr(),
            buffer.len() as u32,
            &mut required,
            0,
        )
    } != 0;
    if !ok || prop_type != DEVPROP_TYPE_STRING {
        return None;
    }
    let len = (required as usize).min(buffer.len());
    let text = utf16_from_bytes(&buffer[..len]).trim().to_owned();
    (!text.is_empty()).then_some(text)
}

fn setupdi_prop_bool(info_set: Handle, devinfo: &SpDevinfoData, key: &DevPropKey) -> Option<bool> {
    let mut prop_type = 0u32;
    let mut required = 0u32;
    let mut buffer = [0u8; 4];
    let ok = unsafe {
        SetupDiGetDevicePropertyW(
            info_set,
            devinfo,
            key,
            &mut prop_type,
            buffer.as_mut_ptr(),
            buffer.len() as u32,
            &mut required,
            0,
        )
    } != 0;
    // DEVPROP_BOOLEAN is one byte: DEVPROP_TRUE = 0xFF, DEVPROP_FALSE = 0x00.
    (ok && prop_type == DEVPROP_TYPE_BOOLEAN && required >= 1).then(|| buffer[0] != 0)
}

fn setupdi_prop_is_present(info_set: Handle, devinfo: &SpDevinfoData) -> Option<bool> {
    setupdi_prop_bool(info_set, devinfo, &DEVPKEY_DEVICE_IS_PRESENT)
}

fn setupdi_prop_is_connected(info_set: Handle, devinfo: &SpDevinfoData) -> Option<bool> {
    setupdi_prop_bool(info_set, devinfo, &DEVPKEY_DEVICE_IS_CONNECTED)
}

/// `DEVPKEY_Device_ContainerId` as an upper-case `XXXXXXXX-XXXX-XXXX-XXXX-XXXXXXXXXXXX`.
fn setupdi_prop_container(info_set: Handle, devinfo: &SpDevinfoData) -> Option<String> {
    let mut prop_type = 0u32;
    let mut required = 0u32;
    let mut buffer = [0u8; 16];
    let ok = unsafe {
        SetupDiGetDevicePropertyW(
            info_set,
            devinfo,
            &DEVPKEY_DEVICE_CONTAINER_ID,
            &mut prop_type,
            buffer.as_mut_ptr(),
            buffer.len() as u32,
            &mut required,
            0,
        )
    } != 0;
    if !ok || prop_type != DEVPROP_TYPE_GUID || required != 16 {
        return None;
    }
    let d1 = u32::from_le_bytes([buffer[0], buffer[1], buffer[2], buffer[3]]);
    let d2 = u16::from_le_bytes([buffer[4], buffer[5]]);
    let d3 = u16::from_le_bytes([buffer[6], buffer[7]]);
    let tail: String = buffer[10..].iter().map(|b| format!("{b:02X}")).collect();
    let guid = format!(
        "{d1:08X}-{d2:04X}-{d3:04X}-{:02X}{:02X}-{tail}",
        buffer[8], buffer[9]
    );
    (guid != "00000000-0000-0000-0000-000000000000").then_some(guid)
}

fn setupdi_prop_bluetooth_battery(info_set: Handle, devinfo: &SpDevinfoData) -> Option<i32> {
    let mut prop_type = 0u32;
    let mut required = 0u32;
    let mut buffer = [0u8; 8];
    let ok = unsafe {
        SetupDiGetDevicePropertyW(
            info_set,
            devinfo,
            &DEVPKEY_BLUETOOTH_BATTERY,
            &mut prop_type,
            buffer.as_mut_ptr(),
            buffer.len() as u32,
            &mut required,
            0,
        )
    } != 0;
    if !ok {
        return None;
    }
    let value = match prop_type {
        DEVPROP_TYPE_BYTE if required == 1 => i64::from(buffer[0]),
        DEVPROP_TYPE_UINT32 if required == 4 => i64::from(u32::from_le_bytes([
            buffer[0], buffer[1], buffer[2], buffer[3],
        ])),
        _ => return None,
    };
    (0..=100).contains(&value).then_some(value as i32)
}

fn is_bluetooth_instance(instance: &str) -> bool {
    let upper = instance.to_ascii_uppercase();
    [
        "BTHENUM\\",
        "BTHLE\\",
        "BTHLEDEVICE\\",
        "BTHHFENUM\\",
        "BLUETOOTH\\",
        "BTHA2DP\\",
    ]
    .iter()
    .any(|prefix| upper.starts_with(prefix))
}

/// One present PnP node relevant to Bluetooth battery reporting.
struct BtPnpNode {
    instance: String,
    container: Option<String>,
    connected: Option<bool>,
    battery: Option<i32>,
    name: Option<String>,
}

/// Physical link state + identity of every physical Bluetooth device, derived from all its
/// PnP functions sharing one container.
#[derive(Default)]
struct BtTopology {
    nodes: Vec<BtPnpNode>,
    /// container -> any function reports `IsConnected == true` (Some(false) when only
    /// "disconnected" answers were seen, absent when nothing was reported).
    container_connected: HashMap<String, bool>,
    container_address: HashMap<String, u64>,
}

impl BtTopology {
    fn address_of(&self, node: &BtPnpNode) -> Option<u64> {
        bt_address_from_instance(&node.instance).or_else(|| {
            node.container
                .as_ref()
                .and_then(|c| self.container_address.get(c).copied())
        })
    }

    /// Verified physical link state: the node's own `IsConnected`, else its container's.
    fn connected(&self, node: &BtPnpNode) -> Option<bool> {
        match node.connected {
            Some(true) => Some(true),
            own => node
                .container
                .as_ref()
                .and_then(|c| self.container_connected.get(c).copied())
                .map(|c| c || own == Some(true))
                .or(own),
        }
    }

    fn physical_id(&self, node: &BtPnpNode) -> String {
        if let Some(address) = self.address_of(node) {
            return bt_physical_id(address);
        }
        if let Some(container) = &node.container {
            return format!("bt-c:{container}");
        }
        format!(
            "bt-pnp:{:08X}",
            fnv1a_u32(node.instance.to_ascii_uppercase().as_bytes())
        )
    }
}

fn collect_bluetooth_topology(cancel: &AtomicBool) -> Option<BtTopology> {
    let set = unsafe {
        SetupDiGetClassDevsW(
            std::ptr::null(),
            std::ptr::null(),
            0,
            DIGCF_ALLCLASSES | DIGCF_PRESENT,
        )
    };
    if invalid(set) {
        return None;
    }
    let mut topology = BtTopology::default();
    for index in 0..2048u32 {
        if cancel.load(Ordering::Relaxed) {
            break;
        }
        let mut devinfo: SpDevinfoData = unsafe { std::mem::zeroed() };
        devinfo.cb_size = size_of::<SpDevinfoData>() as u32;
        if unsafe { SetupDiEnumDeviceInfo(set, index, &mut devinfo) } == 0 {
            break;
        }
        if setupdi_prop_is_present(set, &devinfo) == Some(false) {
            continue;
        }
        let battery = setupdi_prop_bluetooth_battery(set, &devinfo);
        let Some(instance) = setupdi_prop_string(set, &devinfo, &DEVPKEY_DEVICE_INSTANCE_ID) else {
            continue;
        };
        if battery.is_none() && !is_bluetooth_instance(&instance) {
            continue;
        }
        let container = setupdi_prop_container(set, &devinfo)
            .filter(|c| !c.eq_ignore_ascii_case(LOCAL_MACHINE_CONTAINER));
        let connected = setupdi_prop_is_connected(set, &devinfo);
        let name = battery.and_then(|_| {
            setupdi_prop_string(set, &devinfo, &DEVPKEY_DEVICE_FRIENDLY_NAME)
                .or_else(|| setupdi_prop_string(set, &devinfo, &DEVPKEY_DEVICE_DESC))
        });
        if let Some(container) = &container {
            if let Some(state) = connected {
                let entry = topology
                    .container_connected
                    .entry(container.clone())
                    .or_insert(false);
                *entry |= state;
            }
            if let Some(address) = bt_address_from_instance(&instance) {
                topology
                    .container_address
                    .entry(container.clone())
                    .or_insert(address);
            }
        }
        topology.nodes.push(BtPnpNode {
            instance,
            container,
            connected,
            battery,
            name,
        });
    }
    unsafe { SetupDiDestroyDeviceInfoList(set) };
    Some(topology)
}

/// WinRT cross-check of the physical link when PnP did not report `IsConnected`.
fn winrt_bluetooth_connected(address: u64, cancel: &AtomicBool) -> Option<bool> {
    use windows::Devices::Bluetooth::{
        BluetoothConnectionStatus, BluetoothDevice, BluetoothLEDevice,
    };
    use windows::Win32::System::Com::{COINIT_MULTITHREADED, CoInitializeEx};

    let hr = unsafe { CoInitializeEx(None, COINIT_MULTITHREADED) };
    if hr.is_err() && hr.0 != 0x8001_0106u32 as i32 {
        return None;
    }
    let classic = (|| -> windows::core::Result<bool> {
        let device = CloseGuard::new(
            winrt_wait!(
                BluetoothDevice::FromBluetoothAddressAsync(address)?,
                cancel,
                400
            ),
            |d: &BluetoothDevice| {
                let _ = d.Close();
            },
        );
        Ok(device.ConnectionStatus()? == BluetoothConnectionStatus::Connected)
    })();
    let state = match classic {
        Ok(true) => Some(true),
        _ => (|| -> windows::core::Result<bool> {
            let device = CloseGuard::new(
                winrt_wait!(
                    BluetoothLEDevice::FromBluetoothAddressAsync(address)?,
                    cancel,
                    400
                ),
                |d: &BluetoothLEDevice| {
                    let _ = d.Close();
                },
            );
            Ok(device.ConnectionStatus()? == BluetoothConnectionStatus::Connected)
        })()
        .ok()
        .or(classic.ok()),
    };
    if hr.is_ok() {
        unsafe { windows::Win32::System::Com::CoUninitialize() };
    }
    state
}

/// Reads Bluetooth battery percentages from Windows PnP (`DEVPKEY_Bluetooth_Battery`) —
/// AirPods/Beats, Sony WH/WF, Bose, Jabra, Galaxy Buds, HFP/BLE peripherals — with zero
/// PowerShell spawns. `DEVPKEY_Device_IsPresent` only means "paired and enumerated": a
/// switched-off headset keeps its last battery value there. A reading is therefore live
/// only when the physical link is verified (`DEVPKEY_Device_IsConnected` on the node or any
/// function in its `ContainerId`, else WinRT `ConnectionStatus`); unverified values are
/// never presented as live. Rows are keyed by Bluetooth address / container, never by name.
fn bluetooth_pnp_readings(cancel: &AtomicBool) -> Vec<WirelessReading> {
    let mut seen = HashSet::new();
    let mut readings = Vec::new();
    let Some(topology) = collect_bluetooth_topology(cancel) else {
        return readings;
    };
    let mut winrt_checked: HashMap<u64, Option<bool>> = HashMap::new();
    for node in &topology.nodes {
        if cancel.load(Ordering::Relaxed) || readings.len() >= 32 {
            break;
        }
        let Some(level) = node.battery else {
            continue;
        };
        let connected = topology.connected(node).or_else(|| {
            let address = topology.address_of(node)?;
            *winrt_checked
                .entry(address)
                .or_insert_with(|| winrt_bluetooth_connected(address, cancel))
        });
        if connected != Some(true) {
            continue;
        }
        let raw_name = node
            .name
            .clone()
            .unwrap_or_else(|| normalize_name(&node.instance));
        let name = super::wireless_vendors_ext::clean_bluetooth_device_name(&raw_name);
        if is_ignored_wired_device(&name) {
            continue;
        }
        let id = topology.physical_id(node);
        if !seen.insert(id.clone()) {
            continue;
        }
        let kind = super::wireless_vendors_ext::classify_wireless_device_kind(
            0,
            0,
            &name,
            BatterySource::Bluetooth,
            &node.instance,
        );
        readings.push(reading_with_meta(
            id,
            name,
            percent_01(level),
            true,
            false,
            BatterySource::Bluetooth,
            kind,
            None,
        ));
    }

    // Secondary fallback: the PnP registry copy, only for present Bluetooth instances. It is
    // a last-known snapshot with unknown sample time, so it is always marked stale.
    const BATTERY_LEVEL_PROPERTY: &str = "{104ea319-6ee2-4701-bd47-8ddbf425bbe5} 2";
    const BATTERY_LIFE_PROPERTY: &str = "System.Devices.BatteryLife";
    const NAME_KEYS: &[&str] = &["FriendlyName", "Name", "DisplayName"];
    const LEVEL_KEYS: &[&str] = &[
        BATTERY_LEVEL_PROPERTY,
        BATTERY_LIFE_PROPERTY,
        "BatteryLevel",
        "BatteryPercent",
    ];
    const ROOTS: &[(Handle, &str, &str)] = &[
        (
            HKEY_LOCAL_MACHINE,
            r"SYSTEM\CurrentControlSet\Enum\Bluetooth",
            "BLUETOOTH",
        ),
        (
            HKEY_LOCAL_MACHINE,
            r"SYSTEM\CurrentControlSet\Enum\BTHENUM",
            "BTHENUM",
        ),
    ];
    let present: HashMap<String, &BtPnpNode> = topology
        .nodes
        .iter()
        .map(|node| (node.instance.to_ascii_uppercase(), node))
        .collect();
    if present.is_empty() {
        return readings;
    }
    for &(root, path, prefix) in ROOTS {
        if cancel.load(Ordering::Relaxed) || readings.len() >= 32 {
            break;
        }
        let Some(key) = RegKey::open(root, path) else {
            continue;
        };
        for family in reg_subkeys(key.raw()).into_iter().take(64) {
            if cancel.load(Ordering::Relaxed) || readings.len() >= 32 {
                break;
            }
            let Some(family_key) = RegKey::open(key.raw(), &family) else {
                continue;
            };
            for instance in reg_subkeys(family_key.raw()).into_iter().take(64) {
                if cancel.load(Ordering::Relaxed) || readings.len() >= 32 {
                    break;
                }
                let full_instance = format!("{prefix}\\{family}\\{instance}").to_ascii_uppercase();
                let Some(node) = present.get(&full_instance) else {
                    continue;
                };
                // Paired-but-off devices explicitly reporting `IsConnected == false` must not
                // be emitted from the registry fallback as `(cached)` on fresh launch.
                if topology.connected(node) == Some(false)
                    || topology
                        .address_of(node)
                        .and_then(|addr| winrt_checked.get(&addr).copied().flatten())
                        == Some(false)
                {
                    continue;
                }
                let id = topology.physical_id(node);
                if seen.contains(&id) {
                    continue;
                }
                let Some(reg_node) = RegKey::open(family_key.raw(), &instance) else {
                    continue;
                };
                let level = LEVEL_KEYS
                    .iter()
                    .find_map(|name| reg_percent(reg_node.raw(), name))
                    .or_else(|| {
                        RegKey::open(reg_node.raw(), "Device Parameters").and_then(|params| {
                            LEVEL_KEYS
                                .iter()
                                .find_map(|name| reg_percent(params.raw(), name))
                        })
                    });
                let Some(level) = level else {
                    continue;
                };
                let raw_name = NAME_KEYS
                    .iter()
                    .find_map(|name| reg_string(reg_node.raw(), name))
                    .unwrap_or_else(|| normalize_name(&instance));
                let name = super::wireless_vendors_ext::clean_bluetooth_device_name(&raw_name);
                if is_ignored_wired_device(&name) {
                    continue;
                }
                let kind = super::wireless_vendors_ext::classify_wireless_device_kind(
                    0,
                    0,
                    &name,
                    BatterySource::Bluetooth,
                    &full_instance,
                );
                seen.insert(id.clone());
                let mut item = reading_with_meta(
                    id,
                    name,
                    percent_01(level),
                    false,
                    false,
                    BatterySource::Bluetooth,
                    kind,
                    None,
                );
                item.stale = true;
                readings.push(item);
            }
        }
    }
    readings
}

/// Minimal masked text-frame writer for the G HUB loopback WebSocket.
fn ws_send_text(stream: &mut TcpStream, payload: &[u8]) -> Option<()> {
    let mask = [
        (std::process::id() & 0xff) as u8,
        (std::process::id() >> 8) as u8,
        0x5a,
        0xa3,
    ];
    let mut frame = vec![0x81];
    if payload.len() < 126 {
        frame.push(0x80 | payload.len() as u8);
    } else if payload.len() <= usize::from(u16::MAX) {
        frame.push(0x80 | 126);
        frame.extend_from_slice(&(payload.len() as u16).to_be_bytes());
    } else {
        return None;
    }
    frame.extend_from_slice(&mask);
    frame.extend(payload.iter().enumerate().map(|(i, b)| b ^ mask[i % 4]));
    stream.write_all(&frame).ok()
}

/// Reads one server frame. `Some(text)` for a text frame, `Some("")` for control / binary
/// frames that should be skipped, `None` on close or I/O error.
fn ws_read_frame(stream: &mut TcpStream) -> Option<String> {
    let mut head = [0u8; 2];
    stream.read_exact(&mut head).ok()?;
    let opcode = head[0] & 0x0F;
    let masked = head[1] & 0x80 != 0;
    let mut len = (head[1] & 0x7f) as usize;
    if len == 126 {
        let mut ext = [0u8; 2];
        stream.read_exact(&mut ext).ok()?;
        len = u16::from_be_bytes(ext) as usize;
    } else if len == 127 {
        let mut ext = [0u8; 8];
        stream.read_exact(&mut ext).ok()?;
        len = usize::try_from(u64::from_be_bytes(ext)).ok()?;
    }
    if len > 256 * 1024 {
        return None;
    }
    let mut key = [0u8; 4];
    if masked {
        stream.read_exact(&mut key).ok()?;
    }
    let mut body = vec![0u8; len];
    stream.read_exact(&mut body).ok()?;
    if masked {
        for (i, b) in body.iter_mut().enumerate() {
            *b ^= key[i % 4];
        }
    }
    match opcode {
        0x8 => None,
        0x1 => Some(String::from_utf8_lossy(&body).into_owned()),
        _ => Some(String::new()),
    }
}

/// Sends one `GET <path>` request and returns the matching response body (G HUB also pushes
/// unsolicited events on the same socket, which are skipped).
fn ghub_request(stream: &mut TcpStream, msg_id: &str, path: &str) -> Option<String> {
    let request = serde_json::json!({ "msgId": msg_id, "verb": "GET", "path": path }).to_string();
    ws_send_text(stream, request.as_bytes())?;
    for _ in 0..6 {
        let frame = ws_read_frame(stream)?;
        if frame.is_empty() {
            continue;
        }
        let Ok(value) = serde_json::from_str::<serde_json::Value>(&frame) else {
            continue;
        };
        let same_id = value.get("msgId").and_then(|v| v.as_str()) == Some(msg_id);
        let same_path = value.get("path").and_then(|v| v.as_str()) == Some(path);
        if same_id || same_path {
            return Some(frame);
        }
    }
    None
}

/// G HUB loopback (`ws://127.0.0.1:9010`): `/devices/list` for identities and names, then
/// `/battery/<deviceId>/state` per device for the live level.
fn ghub_loopback_readings(cancel: &AtomicBool) -> Vec<WirelessReading> {
    const ENDPOINT: &str = "ws://127.0.0.1:9010";
    if !is_valid_ghub_uri(ENDPOINT) {
        return Vec::new();
    }
    let Some(authority) = parse_loopback_endpoint(ENDPOINT, 9010, true) else {
        return Vec::new();
    };
    let Some(address) = authority.to_socket_addrs().ok().and_then(|mut a| a.next()) else {
        return Vec::new();
    };
    let Ok(mut stream) = TcpStream::connect_timeout(&address, Duration::from_millis(300)) else {
        return Vec::new();
    };
    let _ = stream.set_read_timeout(Some(Duration::from_millis(400)));
    let _ = stream.set_write_timeout(Some(Duration::from_millis(400)));

    let request = format!(
        "GET / HTTP/1.1\r\nHost: {authority}\r\nUpgrade: websocket\r\nConnection: Upgrade\r\n\
         Sec-WebSocket-Key: dGhlIHNhbXBsZSBub25jZQ==\r\nSec-WebSocket-Version: 13\r\n\
         Origin: file://\r\n\r\n"
    );
    if stream.write_all(request.as_bytes()).is_err() {
        return Vec::new();
    }
    let mut headers = Vec::new();
    let mut byte = [0u8; 1];
    while headers.len() < 8192 {
        if stream.read_exact(&mut byte).is_err() || cancel.load(Ordering::Relaxed) {
            return Vec::new();
        }
        headers.push(byte[0]);
        if headers.ends_with(b"\r\n\r\n") {
            break;
        }
    }
    if !String::from_utf8_lossy(&headers).starts_with("HTTP/1.1 101") {
        return Vec::new();
    }
    let Some(list) = ghub_request(&mut stream, "ot-1", "/devices/list") else {
        return Vec::new();
    };
    let mut items = Vec::new();
    for (index, (device_id, name)) in parse_ghub_device_list(&list)
        .into_iter()
        .take(8)
        .enumerate()
    {
        if cancel.load(Ordering::Relaxed) {
            break;
        }
        let path = format!("/battery/{device_id}/state");
        let Some(state) = ghub_request(&mut stream, &format!("ot-b{index}"), &path) else {
            continue;
        };
        if let Some((level, charging)) = parse_ghub_battery_state(&state) {
            items.push(BatteryReading::new(
                &device_id,
                &name,
                level,
                charging,
                BatterySource::LogitechGHub,
            ));
        }
    }
    if items.is_empty() {
        // Older G HUB builds embed the battery fields directly in the device list.
        items = parse_ghub_json(&list);
    }
    map_loopback_readings("ghub", items)
}

fn map_loopback_readings(prefix: &str, items: Vec<BatteryReading>) -> Vec<WirelessReading> {
    items
        .into_iter()
        .map(|b| {
            reading(
                format!("{prefix}:{}", b.id),
                b.name,
                percent_01(b.level),
                b.connected,
                b.charging,
                b.source,
            )
        })
        .collect()
}

fn probe_ghub_loopback(cancel: &AtomicBool) -> Vec<WirelessReading> {
    ghub_loopback_readings(cancel)
}

fn probe_steelseries_loopback(cancel: &AtomicBool) -> Vec<WirelessReading> {
    let program_data = std::env::var_os("ProgramData")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from(r"C:\ProgramData"));
    let path = program_data
        .join("SteelSeries")
        .join("SteelSeries Engine 3")
        .join("coreProps.json");
    let Ok(json) = std::fs::read_to_string(path) else {
        return Vec::new();
    };
    let Some(url) = parse_coreprops_endpoint(&json) else {
        return Vec::new();
    };
    let Some(port) = url_port(&url) else {
        return Vec::new();
    };
    let Some(authority) = parse_loopback_endpoint(&url, port, false) else {
        return Vec::new();
    };
    for relative in ["/battery", "/devices"] {
        if cancel.load(Ordering::Relaxed) {
            break;
        }
        let Some(body) = crate::platform::http::loopback_get(&authority, relative) else {
            continue;
        };
        let readings = parse_steelseries_gg_json(&body);
        if !readings.is_empty() {
            return map_loopback_readings("ss-gg", readings);
        }
    }
    Vec::new()
}

fn url_port(url: &str) -> Option<u16> {
    let authority = url.split("://").nth(1)?.split('/').next()?;
    authority
        .rsplit_once(':')
        .map(|(_, port)| port.parse().ok())?
}

// ---------------------------------------------------------------------------
// Sağlayıcı 7-12: canlı çoklu-marka vendor HID protokolleri
// ---------------------------------------------------------------------------

#[derive(Clone, Debug)]
struct ProbePolicy {
    listen_only: bool,
    multi_brand_enabled: bool,
    disabled_vendors: Vec<String>,
}

impl Default for ProbePolicy {
    fn default() -> Self {
        Self {
            listen_only: false,
            multi_brand_enabled: true,
            disabled_vendors: Vec::new(),
        }
    }
}

impl From<&AggregatorSettings> for ProbePolicy {
    fn from(settings: &AggregatorSettings) -> Self {
        Self {
            listen_only: settings.listen_only,
            multi_brand_enabled: settings.multi_brand_enabled,
            disabled_vendors: settings.disabled_vendors.clone(),
        }
    }
}

fn is_multi_brand_source(source: BatterySource) -> bool {
    matches!(
        source,
        BatterySource::AsusRogHid
            | BatterySource::CorsairHid
            | BatterySource::HyperXHid
            | BatterySource::CompxAtkVxe
            | BatterySource::Keyboard24gHid
            | BatterySource::GamepadHid
    )
}

fn source_vendor_tokens(source: BatterySource) -> &'static [&'static str] {
    match source {
        BatterySource::LogitechHidPlusPlus | BatterySource::LogitechGHub => {
            &["logitech", "hid++", "ghub"]
        }
        BatterySource::RazerHid => &["razer"],
        BatterySource::SteelSeriesHid | BatterySource::SteelSeriesGg => &["steelseries", "ss"],
        BatterySource::AsusRogHid => &["asus", "rog", "asusrog"],
        BatterySource::CorsairHid => &["corsair"],
        BatterySource::HyperXHid => &["hyperx", "hp", "kingston"],
        BatterySource::CompxAtkVxe => &["compx", "atk", "vxe"],
        BatterySource::Keyboard24gHid => &["keyboard24g", "2.4g"],
        BatterySource::GamepadHid => &["gamepad", "sony", "nintendo"],
        BatterySource::XInput => &["xinput", "xbox"],
        BatterySource::Bluetooth | BatterySource::BluetoothGatt => &["bluetooth", "ble"],
        BatterySource::StandardHid => &["hid", "standardhid"],
    }
}

impl ProbePolicy {
    fn has_disabled_token(&self, token: &str) -> bool {
        self.disabled_vendors
            .iter()
            .any(|v| v.trim().eq_ignore_ascii_case(token))
    }

    fn is_source_disabled(&self, source: BatterySource) -> bool {
        if self.listen_only && !source.is_passive() {
            return true;
        }
        if !self.multi_brand_enabled && is_multi_brand_source(source) {
            return true;
        }
        source_vendor_tokens(source)
            .iter()
            .any(|tok| self.has_disabled_token(tok))
    }

    fn allows_source(&self, source: BatterySource) -> bool {
        !self.is_source_disabled(source)
    }

    fn is_vid_disabled(&self, vid: u16, product: &str) -> bool {
        if self.disabled_vendors.is_empty() {
            return false;
        }
        if let Some(tok) = super::wireless_vendors_ext::vendor_token_for_vid(vid)
            && self.has_disabled_token(tok)
        {
            return true;
        }
        super::wireless_vendors_ext::is_glorious_device(vid, 0, product)
            && self.has_disabled_token("glorious")
    }
}

fn build_corsair_legacy_request(report_len: usize) -> Vec<u8> {
    let len = report_len.max(64);
    let mut packet = vec![0u8; len];
    let offset = usize::from(len == 65);
    packet[offset] = 0xC9;
    packet[offset + 1] = 0x64;
    packet
}

fn build_nordic64_request(report_len: usize) -> Vec<u8> {
    let len = if report_len == 65 { 65 } else { 64 };
    let mut packet = vec![0u8; len];
    packet[0] = 0x08;
    packet[1] = COMPX_QUERY_CMD;
    packet[16] = 0x49;
    packet
}

fn build_atk1k_feature_request(report_len: usize, report_id_zero: bool) -> Vec<u8> {
    let mut packet = vec![0u8; report_len.max(9)];
    if report_id_zero {
        packet[0] = 0x00;
        packet[1] = 0x82;
    } else {
        packet[0] = 0x82;
    }
    packet
}

fn build_keyboard_24g_query(expected_len: usize, buffer_len: usize) -> Vec<u8> {
    if expected_len == COMPX_REPORT_LEN {
        return build_compx_request().to_vec();
    }
    let len = buffer_len.max(64);
    let mut packet = vec![0u8; len];
    let offset = usize::from(len == 65);
    packet[offset] = 0x08;
    packet[offset + 1] = COMPX_QUERY_CMD;
    packet[offset + 16] = 0x49;
    packet
}

fn probe_asus_rog(
    devices: &[HidDevice],
    cancel: &AtomicBool,
    policy: &ProbePolicy,
) -> Vec<WirelessReading> {
    if !policy.allows_source(BatterySource::AsusRogHid) {
        return Vec::new();
    }
    let budget = Budget::new(900);
    let mut result = Vec::new();
    // Physical identities (not PIDs): two identical ROG mice stay two rows.
    let mut seen_ids = HashSet::new();
    for device in devices {
        if budget.expired(cancel) {
            break;
        }
        if device.vid != ASUS_VID
            || !matches!(device.usage_page, 0xFF00 | 0xFF01)
            || !matches!(device.input_len, 64 | 65)
            || !matches!(device.output_len, 64 | 65)
            || is_ignored_wired_product(&device.product)
            || seen_ids.contains(&device.id("hid"))
        {
            continue;
        }
        let Some(session) = HidSession::open(&device.path, false) else {
            continue;
        };
        let out_len = (device.output_len as usize).max(64);
        let in_len = (device.input_len as usize).max(64);
        let request = build_asus_request(out_len);
        let mut response = vec![0u8; in_len];
        let Some(bytes) = session.transact(
            &request,
            &mut response,
            220,
            |pkt| {
                let r = normalize_framing(pkt);
                r.len() >= 6 && r[0] == ASUS_CMD_CATEGORY && r[1] == ASUS_CMD_GET_BATTERY
            },
            cancel,
        ) else {
            continue;
        };
        let name = if device.product.trim().is_empty() {
            "ASUS ROG Wireless".to_owned()
        } else {
            device.product.trim().to_owned()
        };
        let id = device.id("rog");
        if let Some(parsed) = parse_asus_rog(&response[..bytes], &id, &name) {
            let kind = super::wireless_vendors_ext::classify_wireless_device_kind(
                device.vid,
                device.pid,
                &parsed.name,
                BatterySource::AsusRogHid,
                &parsed.id,
            );
            seen_ids.insert(device.id("hid"));
            result.push(reading_with_meta(
                parsed.id,
                parsed.name,
                percent_01(parsed.level),
                parsed.connected,
                parsed.charging,
                BatterySource::AsusRogHid,
                kind,
                None,
            ));
        }
    }
    // Standard HID stays available when the vendor exchange answered nothing.
    let fallback = probe_standard_hid(devices, cancel, BatterySource::AsusRogHid, |d| {
        d.vid == ASUS_VID && !seen_ids.contains(&d.id("hid"))
    });
    result.extend(fallback);
    result
}

/// Discards input reports already queued on a vendor channel (another app's answers, a
/// previous property) so the next read belongs to the next request.
fn drain_input_reports(session: &HidSession, buffer_len: usize) {
    let mut scratch = vec![0u8; buffer_len.max(8)];
    for _ in 0..4 {
        if session.read_report(&mut scratch, 50).is_none() {
            break;
        }
    }
}

/// One Bragi `get property` exchange, correlated by route (`endpoint`) and status.
/// The reply echoes the command but not the property, so the queue is drained first and
/// the answer is read twice: a level is only accepted when two consecutive answers agree.
fn bragi_get_property(
    session: &HidSession,
    endpoint: u8,
    property: u16,
    out_len: usize,
    in_len: usize,
    cancel: &AtomicBool,
) -> Option<u16> {
    let mut answers = [None; 2];
    for slot in &mut answers {
        drain_input_reports(session, in_len);
        let request = build_bragi_request(endpoint, property, out_len);
        let mut response = vec![0u8; in_len];
        let len = session.transact(
            &request,
            &mut response,
            220,
            |pkt| is_bragi_reply_for(pkt, endpoint),
            cancel,
        )?;
        *slot = parse_bragi_value(&response[..len], endpoint);
    }
    match answers {
        [Some(a), Some(b)] if a == b => Some(a),
        _ => None,
    }
}

fn probe_corsair(
    devices: &[HidDevice],
    cancel: &AtomicBool,
    policy: &ProbePolicy,
) -> Vec<WirelessReading> {
    if !policy.allows_source(BatterySource::CorsairHid) {
        return Vec::new();
    }
    let budget = Budget::new(950);
    let mut result = Vec::new();
    let mut seen_ids = HashSet::new();
    for device in devices {
        if budget.expired(cancel) {
            break;
        }
        if device.vid != CORSAIR_VID
            || !matches!(
                device.usage_page,
                BRAGI_USAGE_PAGE | CORSAIR_LEGACY_USAGE_PAGE
            )
            || !matches!(device.input_len, 64 | 65)
            || !matches!(device.output_len, 64 | 65)
            || is_ignored_wired_product(&device.product)
            || seen_ids.contains(&device.id("hid"))
        {
            continue;
        }
        let Some(session) = HidSession::open(&device.path, false) else {
            continue;
        };
        let out_len = (device.output_len as usize).max(64);
        let in_len = (device.input_len as usize).max(64);
        let name = if device.product.trim().is_empty() {
            "Corsair Wireless".to_owned()
        } else {
            device.product.trim().to_owned()
        };
        let id = device.id("corsair");
        let kind = super::wireless_vendors_ext::classify_wireless_device_kind(
            device.vid,
            device.pid,
            &name,
            BatterySource::CorsairHid,
            &id,
        );

        if device.usage_page == BRAGI_USAGE_PAGE {
            // Slipstream sub-device (0x09) first, then the directly attached host (0x08).
            for endpoint in [0x09u8, 0x08u8] {
                if budget.expired(cancel) {
                    break;
                }
                let Some(level) = bragi_get_property(
                    &session,
                    endpoint,
                    BRAGI_PROPERTY_LEVEL,
                    out_len,
                    in_len,
                    cancel,
                )
                .and_then(bragi_level_percent) else {
                    continue;
                };
                let charging = bragi_get_property(
                    &session,
                    endpoint,
                    BRAGI_PROPERTY_STATUS,
                    out_len,
                    in_len,
                    cancel,
                )
                .and_then(bragi_charging_state)
                .unwrap_or(false);
                seen_ids.insert(device.id("hid"));
                result.push(reading_with_meta(
                    id.clone(),
                    name.clone(),
                    percent_01(level as i32),
                    true,
                    charging,
                    BatterySource::CorsairHid,
                    kind,
                    None,
                ));
                break;
            }
        } else if device.usage_page == CORSAIR_LEGACY_USAGE_PAGE {
            drain_input_reports(&session, in_len);
            let req = build_corsair_legacy_request(out_len);
            let mut resp = vec![0u8; in_len];
            if let Some(read_len) =
                session.transact(&req, &mut resp, 220, is_corsair_legacy_reply, cancel)
                && let Some(parsed) = parse_corsair_legacy(&resp[..read_len], &id, &name)
            {
                seen_ids.insert(device.id("hid"));
                result.push(reading_with_meta(
                    parsed.id,
                    parsed.name,
                    percent_01(parsed.level),
                    parsed.connected,
                    parsed.charging,
                    BatterySource::CorsairHid,
                    WirelessDeviceKind::Headphones,
                    None,
                ));
            }
        }
    }
    let fallback = probe_standard_hid(devices, cancel, BatterySource::CorsairHid, |d| {
        d.vid == CORSAIR_VID && !seen_ids.contains(&d.id("hid"))
    });
    result.extend(fallback);
    result
}

fn probe_hyperx(
    devices: &[HidDevice],
    cancel: &AtomicBool,
    policy: &ProbePolicy,
) -> Vec<WirelessReading> {
    use super::wireless_vendors_ext::{
        HP_HYPERX_VID, KINGSTON_HYPERX_VID, build_hyperx_charging_request,
        build_hyperx_protocol_request, hyperx_profile, is_valid_hyperx_charging_reply,
        is_valid_hyperx_protocol_reply, parse_hyperx_charging_reply, parse_hyperx_protocol,
    };

    if !policy.allows_source(BatterySource::HyperXHid) {
        return Vec::new();
    }
    let budget = Budget::new(950);
    let mut result = Vec::new();
    let mut seen = HashSet::new();

    for device in devices {
        if budget.expired(cancel) {
            break;
        }
        if !matches!(device.vid, HP_HYPERX_VID | KINGSTON_HYPERX_VID)
            || !matches!(device.usage_page, 0xFF00 | 0xFF13 | 0xFF90 | 0xFFC0)
            || !(20..=65).contains(&device.input_len)
            || !(20..=65).contains(&device.output_len)
            || is_ignored_wired_product(&device.product)
            || seen.contains(&device.id("hid"))
        {
            continue;
        }
        let id = device.id("hyperx");

        if let Some(profile) = hyperx_profile(device.vid, device.pid) {
            let Some(session) = HidSession::open(&device.path, false) else {
                continue;
            };
            let out_len = (device.output_len as usize).max(20);
            let in_len = (device.input_len as usize).max(20);
            drain_input_reports(&session, in_len);
            let request = build_hyperx_protocol_request(profile.protocol, out_len);
            let mut response = vec![0u8; in_len];
            let Some(bytes) = session.transact(
                &request,
                &mut response,
                240,
                |pkt| is_valid_hyperx_protocol_reply(profile.protocol, pkt),
                cancel,
            ) else {
                continue;
            };
            let name = if device.product.trim().is_empty() {
                profile.default_name.to_owned()
            } else {
                device.product.trim().to_owned()
            };
            if let Some(mut parsed) = parse_hyperx_protocol(
                profile.protocol,
                &response[..bytes],
                &id,
                &name,
                profile.device_kind,
            ) {
                // Protocols with a separate charging command (HP Cloud II `03`, Alpha `0C`).
                if let Some(charge_request) =
                    build_hyperx_charging_request(profile.protocol, out_len)
                {
                    let mut charge_response = vec![0u8; in_len];
                    if let Some(len) = session.transact(
                        &charge_request,
                        &mut charge_response,
                        200,
                        |pkt| is_valid_hyperx_charging_reply(profile.protocol, pkt),
                        cancel,
                    ) && let Some(charging) =
                        parse_hyperx_charging_reply(profile.protocol, &charge_response[..len])
                    {
                        parsed.charging = charging;
                    }
                }
                seen.insert(device.id("hid"));
                result.push(reading_with_meta(
                    parsed.id,
                    parsed.name,
                    percent_01(parsed.level),
                    parsed.connected,
                    parsed.charging,
                    BatterySource::HyperXHid,
                    parsed.device_kind,
                    None,
                ));
            }
        } else if let Some(family) = hyperx_family(device.usage_page, device.vid) {
            // Only probe unknown HP/Kingston PIDs when their product string explicitly identifies HyperX.
            let lower = device.product.to_ascii_lowercase();
            if !(lower.contains("hyperx") || lower.contains("cloud") || lower.contains("pulsefire"))
            {
                continue;
            }
            let Some(session) = HidSession::open(&device.path, false) else {
                continue;
            };
            let out_len = (device.output_len as usize).max(64);
            let in_len = (device.input_len as usize).max(64);
            let request = build_hyperx_request(family, out_len);
            let mut response = vec![0u8; in_len];
            if let Some(bytes) = session.transact(
                &request,
                &mut response,
                220,
                |pkt| valid_hyperx(family, pkt),
                cancel,
            ) && let Some(parsed) =
                parse_hyperx(family, &response[..bytes], &id, &device.product)
            {
                let kind = super::wireless_vendors_ext::classify_wireless_device_kind(
                    device.vid,
                    device.pid,
                    &parsed.name,
                    BatterySource::HyperXHid,
                    &parsed.id,
                );
                seen.insert(device.id("hid"));
                result.push(reading_with_meta(
                    parsed.id,
                    parsed.name,
                    percent_01(parsed.level),
                    parsed.connected,
                    parsed.charging,
                    BatterySource::HyperXHid,
                    kind,
                    None,
                ));
            }
        }
    }

    let fallback = probe_standard_hid(devices, cancel, BatterySource::HyperXHid, |d| {
        matches!(d.vid, HP_HYPERX_VID | KINGSTON_HYPERX_VID)
            && !seen.contains(&d.id("hid"))
            && (hyperx_profile(d.vid, d.pid).is_some()
                || d.product.to_ascii_lowercase().contains("hyperx"))
    });
    result.extend(fallback);
    result
}

fn is_verified_keyboard_24g_profile(vid: u16, pid: u16) -> Option<(u16, usize)> {
    match (vid, pid) {
        (0x258A, 0x010C | 0x013C | 0x0152 | 0x00B5) => Some((0xFF00, 64)),
        (0x3554, 0xFA09) => Some((0xFF02, 17)),
        (0x3151, 0x4011 | 0x4015) => Some((0xFF02, 64)),
        _ => None,
    }
}

/// Sends a feature report on one collection and reads the answer as an input report from a
/// sibling collection of the same physical device (vendors that split "command" and
/// "response" across two top-level collections). Returns the first validated input report.
fn paired_feature_exchange(
    devices: &[HidDevice],
    owner: &HidDevice,
    request: &[u8],
    input_page: Option<u16>,
    validator: impl Fn(&[u8]) -> bool,
    cancel: &AtomicBool,
) -> Option<Vec<u8>> {
    let physical = owner.id("hid");
    let readers: Vec<(&HidDevice, HidSession)> = devices
        .iter()
        .filter(|d| {
            d.id("hid") == physical
                && d.input_len >= 5
                && input_page.is_none_or(|page| d.usage_page == page)
        })
        .filter_map(|d| HidSession::open_read_only(&d.path).map(|s| (d, s)))
        .collect();
    if readers.is_empty() {
        return None;
    }
    for (reader, session) in &readers {
        drain_input_reports(session, reader.input_len as usize);
    }
    let writer = HidSession::open(&owner.path, true)?;
    if !writer.set_feature(request) {
        return None;
    }
    for _ in 0..3 {
        for (reader, session) in &readers {
            if cancel.load(Ordering::Relaxed) {
                return None;
            }
            let mut buffer = vec![0u8; reader.input_len as usize];
            if let Some(len) = session.read_report(&mut buffer, 120)
                && validator(&buffer[..len])
            {
                buffer.truncate(len);
                return Some(buffer);
            }
        }
    }
    None
}

fn probe_compx_and_mice(
    devices: &[HidDevice],
    cancel: &AtomicBool,
    policy: &ProbePolicy,
) -> Vec<WirelessReading> {
    use super::wireless_vendors_ext::{
        ENDGAME_GEAR_FEATURE_USAGE_PAGE, ENDGAME_GEAR_INPUT_USAGE_PAGE, ENDGAME_GEAR_VID,
        FINALMOUSE_VID, KEYCHRON_VENDOR_USAGE_PAGE, KEYCHRON_VID, ZowieProtocol,
        build_endgame_gear_feature_request, build_endgame_gear_request, build_finalmouse_request,
        build_glorious_request, build_zowie_dw_feature_request, build_zowie_ec_cw_request,
        build_zowie_request, compx_default_model_name, endgame_gear_model_name,
        finalmouse_model_name, is_endgame_gear_input_frame, is_glorious_device,
        is_glorious_keyboard, is_keychron_keyboard, is_valid_endgame_gear_reply,
        is_valid_finalmouse_reply, is_valid_zowie_reply, parse_endgame_gear_battery_input,
        parse_endgame_gear_link_input, parse_endgame_gear_reply, parse_finalmouse_reply,
        parse_glorious_reply, parse_zowie_reply, zowie_model_name, zowie_protocol,
    };

    if !policy.allows_source(BatterySource::CompxAtkVxe) {
        return Vec::new();
    }
    let budget = Budget::new(1000);
    let mut result = Vec::new();
    let mut seen = HashSet::new();

    // Endgame Gear XM2we family: the command is a feature report `08` on the FF02:0002
    // collection, the answer an input report `09` on the FF01 collection of the same device.
    for device in devices.iter().filter(|d| {
        d.vid == ENDGAME_GEAR_VID
            && d.usage_page == ENDGAME_GEAR_FEATURE_USAGE_PAGE
            && d.usage == 0x0002
            && d.feature_len >= 17
    }) {
        if budget.expired(cancel)
            || seen.contains(&device.id("hid"))
            || policy.is_vid_disabled(device.vid, &device.product)
        {
            continue;
        }
        let feat_len = device.feature_len as usize;
        // 0x03 = radio link state; only a linked mouse has a meaningful battery answer.
        let linked = paired_feature_exchange(
            devices,
            device,
            &build_endgame_gear_feature_request(0x03, feat_len),
            Some(ENDGAME_GEAR_INPUT_USAGE_PAGE),
            |pkt| is_endgame_gear_input_frame(pkt, 0x03),
            cancel,
        )
        .and_then(|frame| parse_endgame_gear_link_input(&frame));
        if linked != Some(true) {
            continue;
        }
        let name = if device.product.trim().is_empty() {
            endgame_gear_model_name(device.pid).to_owned()
        } else {
            device.product.trim().to_owned()
        };
        let id = device.id("compx");
        if let Some(parsed) = paired_feature_exchange(
            devices,
            device,
            &build_endgame_gear_feature_request(0x04, feat_len),
            Some(ENDGAME_GEAR_INPUT_USAGE_PAGE),
            |pkt| is_endgame_gear_input_frame(pkt, 0x04),
            cancel,
        )
        .and_then(|frame| parse_endgame_gear_battery_input(&frame, device.pid, &id, &name))
        {
            seen.insert(device.id("hid"));
            result.push(reading_with_meta(
                parsed.id,
                parsed.name,
                percent_01(parsed.level),
                parsed.connected,
                parsed.charging,
                BatterySource::CompxAtkVxe,
                WirelessDeviceKind::Mouse,
                None,
            ));
        }
    }

    for device in devices {
        if budget.expired(cancel) {
            break;
        }
        if is_ignored_wired(device.vid, device.pid, &device.product)
            || is_ignored_wired_product(&device.product)
            || policy.is_vid_disabled(device.vid, &device.product)
            || seen.contains(&device.id("hid"))
        {
            continue;
        }
        // Leave dedicated 2.4G / Keychron / GMMK keyboards to Keyboard24gHid.
        if is_verified_keyboard_24g_profile(device.vid, device.pid).is_some()
            || is_keychron_keyboard(device.vid, device.pid, &device.product, device.usage_page)
            || is_glorious_keyboard(device.vid, device.pid, &device.product)
        {
            continue;
        }

        let id = device.id("compx");

        // 1. Glorious wireless mice: battery function 0x83 over a 64/65-byte feature report.
        if is_glorious_device(device.vid, device.pid, &device.product)
            && matches!(device.usage_page, 0xFF00..=0xFF02)
            && device.feature_len >= 64
        {
            let Some(session) = HidSession::open(&device.path, true) else {
                continue;
            };
            let feat_len = (device.feature_len as usize).max(65);
            let req = build_glorious_request(feat_len);
            if session.set_feature(&req) {
                unsafe { Sleep(20) };
                let mut resp = vec![0u8; feat_len];
                if session.get_feature(&mut resp)
                    && let Some(parsed) =
                        parse_glorious_reply(&resp, device.vid, device.pid, &id, &device.product)
                {
                    seen.insert(device.id("hid"));
                    result.push(reading_with_meta(
                        parsed.id,
                        parsed.name,
                        percent_01(parsed.level),
                        parsed.connected,
                        parsed.charging,
                        parsed.source,
                        parsed.device_kind,
                        None,
                    ));
                }
            }
            continue;
        }

        // 2. Endgame Gear single-collection models (OP1we, XM2w 4k, OP1w 4k).
        if device.vid == ENDGAME_GEAR_VID
            && matches!(device.usage_page, 0xFF00 | 0xFF02 | 0xFFA0)
            && (matches!(device.input_len, 17 | 64 | 65)
                && matches!(device.output_len, 17 | 64 | 65))
        {
            let Some(session) = HidSession::open(&device.path, false) else {
                continue;
            };
            let out_len = device.output_len as usize;
            let in_len = (device.input_len as usize).max(17);
            let name = if device.product.trim().is_empty() {
                endgame_gear_model_name(device.pid).to_owned()
            } else {
                device.product.trim().to_owned()
            };
            let req = build_endgame_gear_request(out_len);
            let mut resp = vec![0u8; in_len];
            let parsed = session
                .transact(&req, &mut resp, 220, is_valid_endgame_gear_reply, cancel)
                .and_then(|n| parse_endgame_gear_reply(&resp[..n], device.pid, &id, &name));
            if let Some(parsed) = parsed {
                seen.insert(device.id("hid"));
                result.push(reading_with_meta(
                    parsed.id,
                    parsed.name,
                    percent_01(parsed.level),
                    parsed.connected,
                    parsed.charging,
                    BatterySource::CompxAtkVxe,
                    WirelessDeviceKind::Mouse,
                    None,
                ));
            }
            continue;
        }

        // 3. Finalmouse (0x361D: UltralightX receiver `04` command -> `05` answer).
        if device.vid == FINALMOUSE_VID
            && matches!(device.usage_page, 0xFF00..=0xFF02)
            && matches!(device.input_len, 64 | 65)
            && matches!(device.output_len, 64 | 65)
        {
            let Some(session) = HidSession::open(&device.path, false) else {
                continue;
            };
            let out_len = (device.output_len as usize).max(64);
            let in_len = (device.input_len as usize).max(64);
            let name = if device.product.trim().is_empty() {
                finalmouse_model_name(device.pid).to_owned()
            } else {
                device.product.trim().to_owned()
            };
            drain_input_reports(&session, in_len);
            let req = build_finalmouse_request(out_len);
            let mut resp = vec![0u8; in_len];
            if let Some(bytes) =
                session.transact(&req, &mut resp, 220, is_valid_finalmouse_reply, cancel)
                && let Some(parsed) = parse_finalmouse_reply(&resp[..bytes], device.pid, &id, &name)
            {
                seen.insert(device.id("hid"));
                result.push(reading_with_meta(
                    parsed.id,
                    parsed.name,
                    percent_01(parsed.level),
                    parsed.connected,
                    parsed.charging,
                    BatterySource::CompxAtkVxe,
                    WirelessDeviceKind::Mouse,
                    None,
                ));
            }
            continue;
        }

        // 4. ZOWIE / BenQ (0x3057, 0x1EA7 and the documented 0x04A5 receivers).
        if let Some(protocol) = zowie_protocol(device.vid, device.pid, &device.product)
            && matches!(device.usage_page, 0xFF00 | 0xFF01 | 0xFF02 | 0xFFA0)
        {
            let name = if device.product.trim().is_empty() {
                zowie_model_name(device.vid, device.pid).to_owned()
            } else {
                device.product.trim().to_owned()
            };
            let reply = match protocol {
                ZowieProtocol::U2Dw if device.feature_len >= 3 => paired_feature_exchange(
                    devices,
                    device,
                    &build_zowie_dw_feature_request(device.feature_len as usize),
                    None,
                    is_valid_zowie_reply,
                    cancel,
                ),
                ZowieProtocol::EcCw | ZowieProtocol::Legacy
                    if matches!(device.input_len, 64 | 65)
                        && matches!(device.output_len, 64 | 65) =>
                {
                    HidSession::open(&device.path, false).and_then(|session| {
                        let out_len = (device.output_len as usize).max(64);
                        let in_len = (device.input_len as usize).max(64);
                        drain_input_reports(&session, in_len);
                        let req = if protocol == ZowieProtocol::EcCw {
                            build_zowie_ec_cw_request(out_len)
                        } else {
                            build_zowie_request(out_len)
                        };
                        let mut resp = vec![0u8; in_len];
                        session
                            .transact(&req, &mut resp, 220, is_valid_zowie_reply, cancel)
                            .map(|bytes| resp[..bytes].to_vec())
                    })
                }
                _ => None,
            };
            if let Some(reply) = reply
                && let Some(parsed) = parse_zowie_reply(&reply, device.vid, device.pid, &id, &name)
            {
                seen.insert(device.id("hid"));
                result.push(reading_with_meta(
                    parsed.id,
                    parsed.name,
                    percent_01(parsed.level),
                    parsed.connected,
                    parsed.charging,
                    BatterySource::CompxAtkVxe,
                    WirelessDeviceKind::Mouse,
                    None,
                ));
            }
            continue;
        }

        // 5. All COMPX_VENDORS (ATK / VXE / VGN / MCHOSE / Scyrox / WLMouse / Lamzu / Ninjutso / Ajazz / Attack Shark / Darmoshark / Zaopin / Incott / Keychron / Dareu / Rapoo)
        if !is_compx_vendor(device.vid) {
            continue;
        }
        let is_compx17 = device.usage_page == 0xFF02
            && device.input_len as usize == COMPX_REPORT_LEN
            && device.output_len as usize == COMPX_REPORT_LEN;
        let is_atk1k = device.vid == 0x373B
            && device.pid == 0x10F0
            && device.usage_page == 0xFF10
            && device.feature_len >= 9
            && device.input_len >= 10;
        let is_nordic64 = (matches!(device.usage_page, 0xFF02 | 0xFF00)
            || (device.vid == KEYCHRON_VID && device.usage_page == KEYCHRON_VENDOR_USAGE_PAGE))
            && matches!(device.input_len, 64 | 65)
            && matches!(device.output_len, 64 | 65);
        if !is_compx17 && !is_atk1k && !is_nordic64 {
            continue;
        }

        let Some(session) = HidSession::open(&device.path, false) else {
            continue;
        };
        let name = if device.product.trim().is_empty() {
            compx_default_model_name(device.vid, device.pid).to_owned()
        } else {
            device.product.trim().to_owned()
        };

        let parsed = if is_compx17 {
            let req = build_compx_request();
            let mut resp = [0u8; COMPX_REPORT_LEN];
            session
                .transact(
                    &req,
                    &mut resp,
                    250,
                    |pkt| pkt.len() >= 10 && pkt[0] == 0x08 && pkt[1] == COMPX_QUERY_CMD,
                    cancel,
                )
                .and_then(|bytes| parse_compx17(&resp[..bytes], &id, &name))
        } else if is_atk1k {
            let feat_len = (device.feature_len as usize).max(9);
            let sent = session.set_feature(&build_atk1k_feature_request(feat_len, true))
                || session.set_feature(&build_atk1k_feature_request(feat_len, false));
            if sent {
                let in_len = (device.input_len as usize).max(65);
                let mut resp = vec![0u8; in_len];
                let mut found = None;
                for _ in 0..4 {
                    if budget.expired(cancel) {
                        break;
                    }
                    let Some(bytes) = session.read_report(&mut resp, 250) else {
                        break;
                    };
                    if let Some(reading) = parse_atk1k(&resp[..bytes], &id, &name) {
                        found = Some(reading);
                        break;
                    }
                }
                found
            } else {
                None
            }
        } else {
            let out_len = (device.output_len as usize).max(64);
            let in_len = (device.input_len as usize).max(64);
            let req = build_nordic64_request(out_len);
            let mut resp = vec![0u8; in_len];
            session
                .transact(
                    &req,
                    &mut resp,
                    250,
                    |pkt| {
                        let r = normalize_nordic(pkt);
                        (r.len() >= 10 && r[0] == 0x08 && r[1] == COMPX_QUERY_CMD)
                            || (r.len() >= 9 && r[0] == COMPX_QUERY_CMD)
                    },
                    cancel,
                )
                .and_then(|bytes| parse_compx_nordic(&resp[..bytes], &id, &name))
        };

        if let Some(parsed) = parsed {
            let kind = super::wireless_vendors_ext::classify_wireless_device_kind(
                device.vid,
                device.pid,
                &parsed.name,
                BatterySource::CompxAtkVxe,
                &parsed.id,
            );
            seen.insert(device.id("hid"));
            result.push(reading_with_meta(
                parsed.id,
                parsed.name,
                percent_01(parsed.level),
                parsed.connected,
                parsed.charging,
                BatterySource::CompxAtkVxe,
                kind,
                None,
            ));
        }
    }
    result
}

fn probe_keyboard_24g(
    devices: &[HidDevice],
    cancel: &AtomicBool,
    policy: &ProbePolicy,
) -> Vec<WirelessReading> {
    use super::wireless_vendors_ext::{
        build_glorious_request, is_glorious_keyboard, keyboard_24g_default_name,
        parse_glorious_reply,
    };

    if !policy.allows_source(BatterySource::Keyboard24gHid) {
        return Vec::new();
    }
    let budget = Budget::new(950);
    let mut result = Vec::new();
    let mut seen = HashSet::new();

    for device in devices {
        if budget.expired(cancel) {
            break;
        }
        if is_ignored_wired(device.vid, device.pid, &device.product)
            || is_ignored_wired_product(&device.product)
            || policy.is_vid_disabled(device.vid, &device.product)
            || seen.contains(&device.id("hid"))
        {
            continue;
        }

        let id = device.id("kbd24g");

        // 2. Glorious GMMK Wireless keyboards (0x320F)
        if is_glorious_keyboard(device.vid, device.pid, &device.product)
            && matches!(device.usage_page, 0xFF00..=0xFF02)
            && device.feature_len >= 64
        {
            let Some(session) = HidSession::open(&device.path, true) else {
                continue;
            };
            let feat_len = (device.feature_len as usize).max(65);
            let req = build_glorious_request(feat_len);
            if session.set_feature(&req) {
                unsafe { Sleep(20) };
                let mut resp = vec![0u8; feat_len];
                if session.get_feature(&mut resp)
                    && let Some(parsed) =
                        parse_glorious_reply(&resp, device.vid, device.pid, &id, &device.product)
                {
                    seen.insert(device.id("hid"));
                    result.push(reading_with_meta(
                        parsed.id,
                        parsed.name,
                        percent_01(parsed.level),
                        parsed.connected,
                        parsed.charging,
                        BatterySource::Keyboard24gHid,
                        WirelessDeviceKind::Keyboard,
                        None,
                    ));
                }
            }
            continue;
        }

        // 3. Verified 2.4G keyboards: AULA F75/F99, Weikav NUT75, Royal Kludge, Compx FA09, MonsGeek, Akko
        let Some((required_page, expected_len)) =
            is_verified_keyboard_24g_profile(device.vid, device.pid)
        else {
            continue;
        };
        if device.usage_page != required_page {
            continue;
        }
        let len_ok = if expected_len == COMPX_REPORT_LEN {
            device.input_len as usize == COMPX_REPORT_LEN
                && device.output_len as usize == COMPX_REPORT_LEN
        } else {
            matches!(device.input_len, 64 | 65) && matches!(device.output_len, 64 | 65)
        };
        if !len_ok {
            continue;
        }
        let Some(session) = HidSession::open(&device.path, false) else {
            continue;
        };
        let out_len = (device.output_len as usize).max(expected_len);
        let in_len = (device.input_len as usize).max(expected_len);
        let req = build_keyboard_24g_query(expected_len, out_len);
        let mut resp = vec![0u8; in_len];
        let Some(bytes) = session.transact(
            &req,
            &mut resp,
            240,
            |pkt| {
                if expected_len == COMPX_REPORT_LEN {
                    pkt.len() >= 10 && pkt[0] == 0x08 && pkt[1] == COMPX_QUERY_CMD
                } else {
                    let p = normalize_keyboard_24g_framing(pkt);
                    (p.len() >= 10 && p[0] == 0x08 && p[1] == COMPX_QUERY_CMD)
                        || (p.len() >= 9 && p[0] == COMPX_QUERY_CMD)
                        || (p.len() >= 3 && p[0] == 0x83 && p[1] <= 1)
                }
            },
            cancel,
        ) else {
            continue;
        };
        let name = if device.product.trim().is_empty() {
            keyboard_24g_default_name(device.vid, device.pid).to_owned()
        } else {
            device.product.trim().to_owned()
        };
        if let Some(parsed) = parse_keyboard_24g(&resp[..bytes], expected_len, &id, &name) {
            seen.insert(device.id("hid"));
            result.push(reading_with_meta(
                parsed.id,
                parsed.name,
                percent_01(parsed.level),
                parsed.connected,
                parsed.charging,
                BatterySource::Keyboard24gHid,
                WirelessDeviceKind::Keyboard,
                None,
            ));
        }
    }

    let fallback = probe_standard_hid(devices, cancel, BatterySource::Keyboard24gHid, |d| {
        if seen.contains(&d.id("hid")) {
            return false;
        }
        let lower = d.product.to_ascii_lowercase();
        (lower.contains("2.4g") || lower.contains("2,4g"))
            && (lower.contains("keyboard") || lower.contains("klavye"))
    });
    result.extend(fallback);
    result
}

fn default_gamepad_name(vid: u16, pid: u16) -> &'static str {
    match (vid, pid) {
        (0x054C, 0x0CE6) => "DualSense Wireless Controller",
        (0x054C, 0x0DF2) => "DualSense Edge Wireless Controller",
        (0x054C, 0x05C4 | 0x09CC) => "DUALSHOCK 4 Wireless Controller",
        (0x057E, 0x2009) => "Switch Pro Controller",
        _ => "Wireless Controller",
    }
}

fn is_supported_gamepad_pid(vid: u16, pid: u16) -> bool {
    matches!(
        (vid, pid),
        (0x054C, 0x0CE6 | 0x0DF2 | 0x05C4 | 0x09CC) | (0x057E, 0x2009)
    )
}

fn probe_gamepad_hid(
    devices: &[HidDevice],
    cancel: &AtomicBool,
    policy: &ProbePolicy,
) -> Vec<WirelessReading> {
    if !policy.allows_source(BatterySource::GamepadHid) {
        return Vec::new();
    }
    let budget = Budget::new(800);
    let mut result = Vec::new();
    let mut seen = HashSet::new();

    for device in devices {
        if budget.expired(cancel) {
            break;
        }
        if !is_supported_gamepad_pid(device.vid, device.pid)
            || device.usage_page != 0x0001
            || !matches!(device.usage, 0x0004 | 0x0005)
            || device.input_len < 16
            || policy.is_vid_disabled(device.vid, &device.product)
            || seen.contains(&device.id("hid"))
        {
            continue;
        }
        // Strictly read-only HID access (safe even in listen_only mode).
        let Some(session) = HidSession::open_read_only(&device.path)
            .or_else(|| HidSession::open(&device.path, true))
        else {
            continue;
        };
        let in_len = device.input_len as usize;
        let id = device.id("gamepad");
        let name = if device.product.trim().is_empty() {
            default_gamepad_name(device.vid, device.pid).to_owned()
        } else {
            device.product.trim().to_owned()
        };

        let mut buffer = vec![0u8; in_len];
        let mut parsed = session
            .read_report(&mut buffer, 120)
            .and_then(|bytes| parse_gamepad(device.vid, device.pid, &buffer[..bytes], &id, &name));

        // DualSense / DS4 over Bluetooth may start in short 0x01 mode until a read-only
        // calibration feature (0x05) or explicit 0x31 / 0x11 input report is queried.
        if parsed.is_none() && device.vid == 0x054C {
            let feat_len = (device.feature_len as usize).max(41);
            if feat_len <= 128 {
                let mut cal = vec![0u8; feat_len];
                cal[0] = 0x05;
                let _ = session.get_feature(&mut cal);
            }
            if parsed.is_none()
                && let Some(bytes) = session.read_report(&mut buffer, 120)
            {
                parsed = parse_gamepad(device.vid, device.pid, &buffer[..bytes], &id, &name);
            }
        }

        if let Some(parsed) = parsed {
            seen.insert(device.id("hid"));
            result.push(reading_with_meta(
                parsed.id,
                parsed.name,
                percent_01(parsed.level),
                parsed.connected,
                parsed.charging,
                BatterySource::GamepadHid,
                WirelessDeviceKind::Gamepad,
                None,
            ));
        }
    }

    let fallback = probe_standard_hid(devices, cancel, BatterySource::GamepadHid, |d| {
        d.usage_page == 0x0001 && matches!(d.usage, 0x0004 | 0x0005) && !seen.contains(&d.id("hid"))
    });
    result.extend(fallback);
    result
}

fn probe_vendor_hid(
    devices: &[HidDevice],
    cancel: &AtomicBool,
    source: BatterySource,
    policy: &ProbePolicy,
) -> Vec<WirelessReading> {
    if !policy.allows_source(source) {
        return Vec::new();
    }
    match source {
        BatterySource::AsusRogHid => probe_asus_rog(devices, cancel, policy),
        BatterySource::CorsairHid => probe_corsair(devices, cancel, policy),
        BatterySource::HyperXHid => probe_hyperx(devices, cancel, policy),
        BatterySource::CompxAtkVxe => probe_compx_and_mice(devices, cancel, policy),
        BatterySource::Keyboard24gHid => probe_keyboard_24g(devices, cancel, policy),
        BatterySource::GamepadHid => probe_gamepad_hid(devices, cancel, policy),
        _ => Vec::new(),
    }
}

// ---------------------------------------------------------------------------
// Toplayıcı: anlık görüntü, izole thread'ler, dedup, 30 dk stale önbellek
// ---------------------------------------------------------------------------

struct CachedReading {
    reading: WirelessReading,
    sampled_at: Instant,
}

static CACHE: OnceLock<Mutex<HashMap<String, CachedReading>>> = OnceLock::new();
static POLL_GATE: OnceLock<Mutex<()>> = OnceLock::new();

fn cache() -> &'static Mutex<HashMap<String, CachedReading>> {
    CACHE.get_or_init(|| Mutex::new(HashMap::new()))
}

fn lock_cache() -> std::sync::MutexGuard<'static, HashMap<String, CachedReading>> {
    cache()
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

/// Physical keys whose retained last-known entry already expired (30 min without a live
/// sample). A stale / disconnected re-report of such a key (registry snapshot, cached PnP
/// value) must not resurrect it with a fresh age; only a live reading clears the mark.
static STALE_TOMBSTONES: OnceLock<Mutex<HashSet<String>>> = OnceLock::new();

fn lock_tombstones() -> MutexGuard<'static, HashSet<String>> {
    STALE_TOMBSTONES
        .get_or_init(|| Mutex::new(HashSet::new()))
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

fn evict_and_collect_stale(
    guard: &mut HashMap<String, CachedReading>,
    now: Instant,
    active: Option<&HashSet<String>>,
    out: &mut Vec<WirelessReading>,
) {
    let mut expired = Vec::new();
    for (key, cached) in guard.iter() {
        if now.saturating_duration_since(cached.sampled_at) > STALE_WINDOW {
            expired.push(key.clone());
            continue;
        }
        if active.is_some_and(|set| set.contains(key)) {
            continue;
        }
        let mut item = cached.reading.clone();
        // Retained entries keep their original sample time and are never "connected".
        item.sampled_at = cached.sampled_at;
        item.stale = true;
        item.is_connected = false;
        item.is_charging = false;
        out.push(item);
    }
    if !expired.is_empty() {
        let mut tombstones = lock_tombstones();
        for key in expired {
            guard.remove(&key);
            if tombstones.len() < 256 {
                tombstones.insert(key);
            }
        }
    }
}

fn cached_snapshot() -> Vec<WirelessReading> {
    let mut entries = Vec::new();
    {
        let mut guard = lock_cache();
        evict_and_collect_stale(&mut guard, Instant::now(), None, &mut entries);
    }
    sort_readings(&mut entries);
    entries
}

fn sort_readings(readings: &mut [WirelessReading]) {
    readings.sort_by(|a, b| {
        a.stale
            .cmp(&b.stale)
            .then_with(|| a.is_charging.cmp(&b.is_charging))
            .then_with(|| {
                a.level_01
                    .partial_cmp(&b.level_01)
                    .unwrap_or(std::cmp::Ordering::Equal)
            })
            .then_with(|| source_priority(a.source).cmp(&source_priority(b.source)))
            .then_with(|| {
                a.name
                    .to_ascii_lowercase()
                    .cmp(&b.name.to_ascii_lowercase())
            })
            .then_with(|| a.id.cmp(&b.id))
    });
}

fn finalize(fresh: Vec<WirelessReading>) -> Vec<WirelessReading> {
    let now = Instant::now();
    let mut deduplicated: HashMap<String, WirelessReading> = HashMap::new();
    for item in fresh {
        if is_ignored_wired_reading(&item) {
            continue;
        }
        let key = canonical_key(&item);
        match deduplicated.get_mut(&key) {
            Some(existing) if prefer(&item, existing) => {
                let mut preferred = item;
                if preferred.secondary_level.is_none() {
                    preferred.secondary_level = existing.secondary_level;
                }
                if preferred.device_kind == WirelessDeviceKind::Other
                    && existing.device_kind != WirelessDeviceKind::Other
                {
                    preferred.device_kind = existing.device_kind;
                }
                *existing = preferred;
            }
            Some(existing) => {
                if existing.secondary_level.is_none() {
                    existing.secondary_level = item.secondary_level;
                }
                if existing.device_kind == WirelessDeviceKind::Other
                    && item.device_kind != WirelessDeviceKind::Other
                {
                    existing.device_kind = item.device_kind;
                }
            }
            None => {
                deduplicated.insert(key, item);
            }
        }
    }
    let mut active: HashSet<String> = HashSet::new();
    let mut output = Vec::new();
    {
        let mut guard = lock_cache();
        for (key, item) in deduplicated {
            if now.saturating_duration_since(item.sampled_at) > STALE_WINDOW {
                continue;
            }
            let live = item.is_connected && !item.stale;
            if !live {
                // Last-known provenance: never overwrite (and renew) a retained entry, and
                // never resurrect one that already expired.
                if guard.contains_key(&key) || lock_tombstones().contains(&key) {
                    continue;
                }
            } else {
                lock_tombstones().remove(&key);
                active.insert(key.clone());
            }
            guard.insert(
                key,
                CachedReading {
                    reading: item.clone(),
                    sampled_at: item.sampled_at,
                },
            );
            if live {
                output.push(item);
            }
        }
        // An XInput slot that stopped answering was disconnected or reassigned to another
        // controller: its coarse state must not linger as a "last known" row.
        guard.retain(|key, cached| {
            cached.reading.source != BatterySource::XInput || active.contains(key)
        });
        evict_and_collect_stale(&mut guard, now, Some(&active), &mut output);
    }
    sort_readings(&mut output);
    output
}

fn run_probe(
    source: BatterySource,
    devices: &[HidDevice],
    cancel: &AtomicBool,
    policy: &ProbePolicy,
    emit: &dyn Fn(WirelessReading),
) -> Vec<WirelessReading> {
    if !policy.allows_source(source) {
        return Vec::new();
    }
    match source {
        BatterySource::XInput => probe_xinput(cancel),
        BatterySource::StandardHid => {
            probe_standard_hid(devices, cancel, BatterySource::StandardHid, |_| true)
        }
        BatterySource::LogitechHidPlusPlus => probe_logitech(devices, cancel),
        BatterySource::RazerHid => probe_razer(devices, cancel),
        BatterySource::SteelSeriesHid => probe_steelseries(devices, cancel),
        BatterySource::BluetoothGatt => probe_ble_gatt(cancel, emit),
        BatterySource::AsusRogHid
        | BatterySource::CorsairHid
        | BatterySource::HyperXHid
        | BatterySource::CompxAtkVxe
        | BatterySource::GamepadHid
        | BatterySource::Keyboard24gHid => probe_vendor_hid(devices, cancel, source, policy),
        // WS-4: real passive transports (SetupDi PnP + loopback JSON) feed live readings.
        BatterySource::Bluetooth => bluetooth_pnp_readings(cancel),
        BatterySource::LogitechGHub => probe_ghub_loopback(cancel),
        BatterySource::SteelSeriesGg => probe_steelseries_loopback(cancel),
    }
}

/// Per-source claim: `0` = free, otherwise the claim time in ms since [`claim_epoch`].
/// A claim older than [`STUCK_CLAIM_MS`] belongs to a provider thread that is wedged in a
/// non-cancellable OS call; it is reclaimed so that one stuck device cannot block its
/// source forever (the wedged thread can no longer release the newer claim).
static SOURCE_IN_FLIGHT: [AtomicU64; 16] = [const { AtomicU64::new(0) }; 16];
const STUCK_CLAIM_MS: u64 = 60_000;

fn claim_epoch() -> u64 {
    static START: OnceLock<Instant> = OnceLock::new();
    let start = *START.get_or_init(Instant::now);
    // `+ 1` keeps every live claim non-zero.
    start.elapsed().as_millis().min(u128::from(u64::MAX - 1)) as u64 + 1
}

fn source_slot(source: BatterySource) -> usize {
    let slot = source as usize;
    debug_assert!(slot < SOURCE_IN_FLIGHT.len());
    slot
}

struct SourceInFlightGuard {
    slot: usize,
    claim: u64,
}

impl Drop for SourceInFlightGuard {
    fn drop(&mut self) {
        // Only the current owner releases; a reclaimed slot is left to its new owner.
        let _ = SOURCE_IN_FLIGHT[self.slot].compare_exchange(
            self.claim,
            0,
            Ordering::AcqRel,
            Ordering::Acquire,
        );
    }
}

fn try_claim_source(source: BatterySource) -> Option<SourceInFlightGuard> {
    let slot = source_slot(source);
    let now = claim_epoch();
    let current = SOURCE_IN_FLIGHT[slot].load(Ordering::Acquire);
    if current != 0 && now.saturating_sub(current) < STUCK_CLAIM_MS {
        return None;
    }
    SOURCE_IN_FLIGHT[slot]
        .compare_exchange(current, now, Ordering::AcqRel, Ordering::Acquire)
        .ok()
        .map(|_| SourceInFlightGuard { slot, claim: now })
}

/// Runs only the policy-permitted providers inside the bounded `ALL_SOURCES` slots
/// and returns raw fresh readings before aggregation. Providers that read several devices
/// over slow transports (BLE GATT) publish each completed device immediately, so a provider
/// still stuck on one peripheral at the deadline never discards the others.
fn read_all_raw(policy: &ProbePolicy) -> Option<Vec<WirelessReading>> {
    let gate = POLL_GATE.get_or_init(|| Mutex::new(()));
    let Ok(_guard) = gate.try_lock() else {
        return None;
    };
    let devices = Arc::new(enumerate_hid_devices());
    let policy = Arc::new(policy.clone());
    let (sender, receiver) = mpsc::channel::<Vec<WirelessReading>>();
    let mut cancellations = Vec::with_capacity(ALL_SOURCES.len());
    let mut threads = Vec::with_capacity(ALL_SOURCES.len());
    for source in ALL_SOURCES {
        if !policy.allows_source(source) {
            continue;
        }
        let Some(in_flight_guard) = try_claim_source(source) else {
            continue;
        };
        let sender = sender.clone();
        let devices = Arc::clone(&devices);
        let policy = Arc::clone(&policy);
        let cancel = Arc::new(AtomicBool::new(false));
        cancellations.push(Arc::clone(&cancel));
        if let Ok(handle) = std::thread::Builder::new()
            .name(format!("ot-wb-{source:?}"))
            .spawn(move || {
                let _in_flight_guard = in_flight_guard;
                let emit = |item: WirelessReading| {
                    let _ = sender.send(vec![item]);
                };
                let result = catch_unwind(AssertUnwindSafe(|| {
                    run_probe(source, &devices, &cancel, &policy, &emit)
                }))
                .unwrap_or_default();
                if !result.is_empty() {
                    let _ = sender.send(result);
                }
            })
        {
            threads.push(handle);
        }
    }
    drop(sender);
    let deadline = Instant::now() + PROVIDER_TIMEOUT;
    let mut fresh = Vec::new();
    while Instant::now() < deadline {
        let remaining = deadline.saturating_duration_since(Instant::now());
        match receiver.recv_timeout(remaining) {
            Ok(items) => fresh.extend(items),
            Err(mpsc::RecvTimeoutError::Disconnected) => break,
            Err(mpsc::RecvTimeoutError::Timeout) => break,
        }
    }
    for cancel in &cancellations {
        cancel.store(true, Ordering::Relaxed);
    }
    // WS-4: give cancelled probes a bounded grace period, then join every finished thread.
    let join_deadline = Instant::now() + Duration::from_millis(400);
    while Instant::now() < join_deadline && threads.iter().any(|handle| !handle.is_finished()) {
        // Completed devices that arrive during the grace period are still published.
        while let Ok(items) = receiver.try_recv() {
            fresh.extend(items);
        }
        std::thread::sleep(Duration::from_millis(5));
    }
    while let Ok(items) = receiver.try_recv() {
        fresh.extend(items);
    }
    for handle in threads {
        if handle.is_finished() {
            let _ = handle.join();
        }
    }
    Some(fresh)
}

/// Tüm kablosuz pil okumalarını toplar. Engelleyici değildir: her sağlayıcı ayrı
/// iş parçacığında 1.2 sn'lik bütçeyle çalışır, zaman aşanlar atlanır.
pub fn read_all() -> Vec<WirelessReading> {
    match read_all_raw(&ProbePolicy::default()) {
        Some(fresh) => finalize(fresh),
        None => cached_snapshot(),
    }
}

// ---------------------------------------------------------------------------
// Donanım gerektirmeyen saf unit testler
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn try_claim_source_prevents_duplicates_and_releases_on_drop() {
        let first = try_claim_source(BatterySource::SteelSeriesGg);
        assert!(first.is_some(), "initial claim must succeed");
        assert!(
            try_claim_source(BatterySource::SteelSeriesGg).is_none(),
            "duplicate concurrent claim for the same source must be rejected"
        );
        let other = try_claim_source(BatterySource::LogitechGHub);
        assert!(
            other.is_some(),
            "different sources must be claimable concurrently"
        );
        drop(first);
        let reclaimed = try_claim_source(BatterySource::SteelSeriesGg);
        assert!(
            reclaimed.is_some(),
            "slot must be released when guard is dropped"
        );
        drop(reclaimed);
        drop(other);
    }

    #[test]
    fn live_polling_covers_every_probe_provider() {
        // The loopback providers used to exist only in `run_probe`, so they
        // could never appear in a live snapshot. Guard the complete list.
        for source in [
            BatterySource::XInput,
            BatterySource::LogitechHidPlusPlus,
            BatterySource::RazerHid,
            BatterySource::SteelSeriesHid,
            BatterySource::StandardHid,
            BatterySource::BluetoothGatt,
            BatterySource::AsusRogHid,
            BatterySource::CorsairHid,
            BatterySource::GamepadHid,
            BatterySource::HyperXHid,
            BatterySource::CompxAtkVxe,
            BatterySource::Keyboard24gHid,
            BatterySource::Bluetooth,
            BatterySource::LogitechGHub,
            BatterySource::SteelSeriesGg,
        ] {
            assert!(
                ALL_SOURCES.contains(&source),
                "{source:?} must be polled by read_all()"
            );
        }
    }

    #[test]
    fn disk_cache_path_is_the_shared_reference_file() {
        let aggregator = WirelessBatteryAggregator::new(AggregatorSettings::default());
        assert_eq!(
            aggregator.cache_path.as_deref(),
            Some(
                opentaskbar_core::paths::shared_app_data_dir()
                    .join("wireless-battery-cache.json")
                    .as_path()
            )
        );
    }

    #[test]
    fn reads_reference_bom_and_pascalcase_cache_fields() {
        let unique = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos();
        let dir = std::env::temp_dir().join(format!(
            "ot-wb-reference-cache-{}-{unique}",
            std::process::id()
        ));
        let _ = std::fs::create_dir_all(&dir);
        let path = dir.join("wireless-battery-cache.json");
        let sampled = unix_secs_to_rfc3339(
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap_or_default()
                .as_secs()
                - 30,
        );
        let document = format!(
            "\u{feff}{{\"reference-key\":{{\"Id\":\"hid:046D:C52B\",\"Name\":\"Mouse\",\"BatteryLevel\":72,\"IsCharging\":true,\"Source\":2,\"SampledAt\":\"{sampled}\"}}}}"
        );
        std::fs::write(&path, document).expect("reference-format cache fixture must be writable");
        let mut aggregator = WirelessBatteryAggregator::new(AggregatorSettings::default())
            .with_cache_path(path.clone());
        aggregator.ensure_loaded(Instant::now());
        let cached = aggregator
            .cache
            .get("hid:046d:c52b")
            .expect("BOM + PascalCase entry must load");
        assert_eq!(cached.name, "Mouse");
        assert_eq!(cached.level, 72);
        assert!(cached.charging);
        assert_eq!(cached.source, BatterySource::SteelSeriesHid);
        assert!(cached.stale);

        aggregator.save(Instant::now());
        let saved = std::fs::read_to_string(&path).unwrap();
        assert!(
            saved.starts_with('\u{feff}'),
            "reference writes a UTF-8 BOM"
        );
        assert!(saved.contains("\"BatteryLevel\":72"));
        assert!(saved.contains("\"SampledAt\":"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn reference_timestamp_parser_handles_offsets() {
        assert_eq!(
            reference_timestamp_secs("2026-11-27T12:34:56+00:00"),
            reference_timestamp_secs("2026-11-27T12:34:56Z")
        );
        assert_eq!(
            reference_timestamp_secs("2026-11-27T12:34:56.1234567+03:00"),
            reference_timestamp_secs("2026-11-27T09:34:56Z")
        );
        assert_eq!(reference_timestamp_secs("bad"), None);
    }

    #[test]
    fn xinput_coarse_levels_map_without_fake_percent() {
        assert_eq!(xinput_level_01(0), Some(0.05));
        assert_eq!(xinput_level_01(1), Some(0.20));
        assert_eq!(xinput_level_01(2), Some(0.60));
        assert_eq!(xinput_level_01(3), Some(1.00));
        assert_eq!(xinput_level_01(4), None);
        assert_eq!(coarse_level_text(BatterySource::XInput, 0.20), Some("Low"));
        assert_eq!(coarse_level_text(BatterySource::RazerHid, 0.5), None);
    }

    #[test]
    fn device_id_normalizes_serial_and_falls_back_to_path_hash() {
        assert_eq!(
            build_device_id("hid", 0x046D, 0xC52B, "  SN#12345  ", r"\\?\hid#ignored"),
            "hid:046D:C52B:SN12345"
        );
        let first = build_device_id("hid", 0x046D, 0xC52B, "", r"\\?\hid#one");
        let second = build_device_id("hid", 0x046D, 0xC52B, "", r"\\?\hid#two");
        assert_ne!(first, second);
        assert_eq!(
            first,
            build_device_id("hid", 0x046D, 0xC52B, "", r"\\?\hid#one")
        );
        assert!(first.starts_with("hid:046D:C52B:"));
        assert_eq!(first.len(), "hid:046D:C52B:00000000".len());
    }

    #[test]
    fn wired_device_name_filter() {
        assert!(is_ignored_wired_device(""));
        assert!(is_ignored_wired_device("  Wraith W75 Keyboard  "));
        assert!(is_ignored_wired_device("HyperX Cloud II Wired"));
        assert!(is_ignored_wired_device("Wraith W75 Wireless Headset"));
        assert!(!is_ignored_wired_device("Xbox Wireless Controller"));
        let wired = reading(
            "hid:2E3C:C368:1".into(),
            "Unknown".into(),
            0.5,
            true,
            false,
            BatterySource::StandardHid,
        );
        assert!(is_ignored_wired_reading(&wired));
    }

    #[test]
    fn standard_hid_logical_range_scaling() {
        assert_eq!(normalize_logical_to_percent(50, 0, 100), 50);
        assert_eq!(normalize_logical_to_percent(5, 0, 10), 50);
        assert_eq!(normalize_logical_to_percent(3, 1, 4), 67);
        assert_eq!(normalize_logical_to_percent(101, 0, 100), -1);
        assert_eq!(normalize_logical_to_percent(200, 0, 1000), 20);
        assert_eq!(normalize_logical_to_percent(70, 0, 10), -1);
    }

    #[test]
    fn razer_crc_checksum_validates_reply() {
        let request = razer_build_command(RAZER_CMD_BATTERY, 0x1F, false);
        assert_eq!(request.len(), RAZER_PAYLOAD_LEN);
        assert_eq!(request[88], razer_xor_crc(&request));
        let mut framed = vec![0u8; RAZER_BUFFER_LEN];
        framed[1..].copy_from_slice(&request);
        assert_eq!(razer_xor_crc(&framed), request[88]);
        let mut reply = vec![0u8; RAZER_BUFFER_LEN];
        reply[1] = RAZER_STATUS_SUCCESS;
        reply[7] = RAZER_COMMAND_CLASS_POWER;
        reply[8] = RAZER_CMD_BATTERY;
        reply[10] = 200;
        reply[89] = razer_xor_crc(&reply);
        assert_eq!(razer_parse_reply(&reply, RAZER_CMD_BATTERY), Some(200));
        reply[20] ^= 0x01;
        assert_eq!(razer_parse_reply(&reply, RAZER_CMD_BATTERY), None);
        assert!((razer_level_01(255) - 1.0).abs() < f32::EPSILON);
    }

    #[test]
    fn logitech_unified_and_voltage_parsing() {
        let mut reply = [0u8; 7];
        reply[0] = HIDPP_LONG_REPORT;
        reply[1] = 0xFF;
        reply[2] = 0x03;
        reply[4] = 75;
        reply[5] = 0;
        reply[6] = 0x01;
        assert_eq!(hidpp_parse_unified(&reply, 0xFF, 3), Some((0.75, true)));
        assert_eq!(hidpp_parse_status(&reply, 0xFF, 3), Some((0.75, true)));
        reply[4] = 0;
        reply[5] = 0x02;
        assert_eq!(hidpp_parse_unified(&reply, 0xFF, 3), Some((0.20, true)));
        assert_eq!(estimate_percent_from_voltage_mv(3500), 0);
        assert_eq!(estimate_percent_from_voltage_mv(4180), 100);
        assert!((estimate_percent_from_voltage_mv(3700) - 20).abs() <= 1);
    }

    #[test]
    fn steelseries_step_scaling_and_protocol_parsing() {
        assert!((ss_scale8(4) - 0.5).abs() < f32::EPSILON);
        assert!((ss_scale4(2) - 0.5).abs() < f32::EPSILON);
        assert_eq!(ss_protocol_for_pid(0x12E0), Some(SsProtocol::NovaPro));
        assert_eq!(ss_protocol_for_pid(0x2202), Some(SsProtocol::Nova7));
        assert_eq!(ss_protocol_for_pid(0x1858), Some(SsProtocol::Mouse));
        let mut reply = vec![0u8; 16];
        reply[0] = 0x06;
        reply[1] = 0xB0;
        reply[6] = 8;
        reply[7] = 4;
        reply[15] = 0x08;
        let parsed = ss_parse(SsProtocol::NovaPro, &reply).expect("valid nova pro reply");
        assert!((parsed.level - 1.0).abs() < f32::EPSILON);
        assert!(parsed.connected && !parsed.charging);
        assert_eq!(parsed.spare, Some(0.5));
    }
}

// ============================================================================
// WS-G: pure vendor report parsers + aggregator / diagnostics.
// ============================================================================

impl BatterySource {
    /// Measurement provenance rank; lower wins when two providers report the same physical
    /// battery (after "verified live connection" and "not stale"):
    /// 0 = live over-the-air / verified direct vendor exchange (uncached BLE GATT, vendor
    ///     HID protocols), 1 = descriptor-driven standard HID usage, 2 = live vendor
    ///     software daemon (G HUB / SteelSeries GG), 3 = Windows PnP
    ///     `DEVPKEY_Bluetooth_Battery` (stack-cached), 4 = coarse 4-state XInput.
    pub fn priority(self) -> u8 {
        match self {
            Self::BluetoothGatt
            | Self::SteelSeriesHid
            | Self::CompxAtkVxe
            | Self::LogitechHidPlusPlus
            | Self::RazerHid
            | Self::HyperXHid
            | Self::CorsairHid
            | Self::AsusRogHid
            | Self::Keyboard24gHid
            | Self::GamepadHid => 0,
            Self::StandardHid => 1,
            Self::SteelSeriesGg | Self::LogitechGHub => 2,
            Self::Bluetooth => 3,
            Self::XInput => 4,
        }
    }

    /// Readings whose source is coarse or stack-cached, where a single implausible large
    /// drop is more likely a glitch than a real discharge. Verified live GATT / vendor HID
    /// readings are never held back.
    pub fn is_uncertain(self) -> bool {
        matches!(self, Self::Bluetooth | Self::XInput)
    }

    /// Providers that only observe (never send feature reports) — used by `listen_only`.
    pub fn is_passive(self) -> bool {
        matches!(
            self,
            Self::Bluetooth
                | Self::BluetoothGatt
                | Self::XInput
                | Self::StandardHid
                | Self::GamepadHid
        )
    }
}

#[derive(Clone, Debug)]
pub struct BatteryReading {
    pub id: String,
    pub name: String,
    pub level: i32,
    pub connected: bool,
    pub charging: bool,
    pub stale: bool,
    pub source: BatterySource,
    pub sampled_at: Instant,
    sampled_unix: Option<u64>,
    pub voltage_mv: Option<u32>,
    pub secondary_level: Option<i32>,
    pub level_override: Option<String>,
    pub device_kind: WirelessDeviceKind,
}

impl BatteryReading {
    pub fn new(id: &str, name: &str, level: u32, charging: bool, source: BatterySource) -> Self {
        let trimmed = name.trim();
        let device_kind =
            super::wireless_vendors_ext::classify_wireless_device_kind(0, 0, trimmed, source, id);
        Self {
            id: id.to_owned(),
            name: trimmed.to_owned(),
            level: level.min(100) as i32,
            connected: true,
            charging,
            stale: false,
            source,
            sampled_at: Instant::now(),
            sampled_unix: None,
            voltage_mv: None,
            secondary_level: None,
            level_override: None,
            device_kind,
        }
    }

    /// Age of the sample. The persisted Unix sample time is authoritative (it survives
    /// reloads and the per-poll aggregator rebuild); the monotonic instant is the fallback.
    fn age_secs(&self, now: Instant, wall_now: u64) -> u64 {
        match self.sampled_unix {
            Some(sampled) => wall_now.saturating_sub(sampled),
            None => now.saturating_duration_since(self.sampled_at).as_secs(),
        }
    }

    /// `true` for a verified live, connected sample (not last-known provenance).
    fn is_live(&self) -> bool {
        self.connected && !self.stale
    }
}

/// HID interface descriptor (populated by the transport layer).
#[derive(Clone, Debug, Default)]
pub struct HidInterfaceDescriptor {
    pub vendor_id: u16,
    pub product_id: u16,
    pub usage_page: u16,
    pub usage: u16,
    pub input_report_length: u16,
    pub output_report_length: u16,
    pub feature_report_length: u16,
    pub product_name: String,
    pub path: String,
}

// ---------------------------------------------------------------------------------------------
// 1. ASUS ROG (VID 0x0B05, usage pages 0xFF00/0xFF01, command 0x12/0x07, 64/65-byte reports)
// ---------------------------------------------------------------------------------------------

pub const ASUS_VID: u16 = 0x0B05;
pub const ASUS_CMD_CATEGORY: u8 = 0x12;
pub const ASUS_CMD_GET_BATTERY: u8 = 0x07;

pub fn build_asus_request(report_len: usize) -> Vec<u8> {
    let len = report_len.max(64);
    let mut packet = vec![0u8; len];
    let offset = usize::from(len == 65);
    packet[offset] = ASUS_CMD_CATEGORY;
    packet[offset + 1] = ASUS_CMD_GET_BATTERY;
    packet
}

fn normalize_framing(reply: &[u8]) -> &[u8] {
    if reply.len() >= 7
        && reply[0] == 0
        && reply[1] == ASUS_CMD_CATEGORY
        && reply[2] == ASUS_CMD_GET_BATTERY
    {
        &reply[1..]
    } else {
        reply
    }
}

/// Parses a 64-byte ASUS ROG battery reply.
pub fn parse_asus_rog(reply: &[u8], id: &str, name: &str) -> Option<BatteryReading> {
    let r = normalize_framing(reply);
    if r.len() < 6 || r[0] != ASUS_CMD_CATEGORY || r[1] != ASUS_CMD_GET_BATTERY {
        return None;
    }
    let level = r[4];
    let state = if reply.len() == 65 && reply[0] == 0 {
        reply[10]
    } else {
        r[5]
    };
    if level > 100 || state > 3 || (level == 0 && state != 1 && state != 2) {
        return None;
    }
    Some(BatteryReading::new(
        id,
        if name.trim().is_empty() {
            "ASUS ROG Wireless"
        } else {
            name
        },
        level as u32,
        state == 1 || state == 2,
        BatterySource::AsusRogHid,
    ))
}

pub fn is_asus_interface(d: &HidInterfaceDescriptor) -> bool {
    d.vendor_id == ASUS_VID
        && matches!(d.usage_page, 0xFF00 | 0xFF01)
        && matches!(d.input_report_length, 64 | 65)
        && matches!(d.output_report_length, 64 | 65)
}

// ---------------------------------------------------------------------------------------------
// 2. Corsair (VID 0x1B1C, Bragi 0xFF42 property 0x000F/0x0010, legacy 0xFFC2)
// ---------------------------------------------------------------------------------------------

pub const CORSAIR_VID: u16 = 0x1B1C;
pub const BRAGI_USAGE_PAGE: u16 = 0xFF42;
pub const CORSAIR_LEGACY_USAGE_PAGE: u16 = 0xFFC2;
pub const BRAGI_PROPERTY_LEVEL: u16 = 0x000F;
pub const BRAGI_PROPERTY_STATUS: u16 = 0x0010;

pub fn build_bragi_request(endpoint: u8, property: u16, report_len: usize) -> Vec<u8> {
    let len = report_len.max(64);
    let mut packet = vec![0u8; len];
    let offset = usize::from(len == 65);
    packet[offset] = endpoint;
    packet[offset + 1] = 0x02;
    packet[offset + 2] = (property & 0xFF) as u8;
    packet[offset + 3] = (property >> 8) as u8;
    packet
}

/// Strips a leading report-id byte: `00`/`01`/`02` before a routed Bragi frame
/// (`<route|channel> 02 <status>`), or `00` before a legacy `C9 64` / `64 00` block.
fn corsair_framing(reply: &[u8]) -> &[u8] {
    if reply.len() >= 7 {
        let routed = matches!(reply[0], 0x00..=0x02)
            && matches!(reply[1], 0x00 | 0x01 | 0x08 | 0x09)
            && reply[2] == 0x02;
        let legacy =
            reply[0] == 0x00 && (reply[1] == 0xC9 || (reply[1] == 0x64 && reply[2] == 0x00));
        if routed || legacy {
            return &reply[1..];
        }
    }
    reply
}

fn valid_bragi(reply: &[u8]) -> bool {
    let r = corsair_framing(reply);
    r.len() >= 6 && r[1] == 0x02 && r[2] == 0
}

/// A Bragi `get property` reply for the given route: byte 0 is the route (`0x08` host /
/// `0x09` Slipstream child) or its channel (`route - 0x08`), byte 1 the `0x02` command echo,
/// byte 2 the status (`0x00` = OK; `0x05` no such property, `0x09` refused are rejected).
pub fn is_bragi_reply_for(reply: &[u8], endpoint: u8) -> bool {
    let r = corsair_framing(reply);
    r.len() >= 6
        && (r[0] == endpoint || r[0] == endpoint.wrapping_sub(0x08))
        && r[1] == 0x02
        && r[2] == 0x00
}

/// Little-endian 16-bit property value of a validated Bragi reply.
pub fn parse_bragi_value(reply: &[u8], endpoint: u8) -> Option<u16> {
    if !is_bragi_reply_for(reply, endpoint) {
        return None;
    }
    let r = corsair_framing(reply);
    Some(u16::from_le_bytes([r[3], r[4]]))
}

/// Property `0x000F` is the level in tenths of a percent (`0..=1000`); `0` is a real 0%.
pub fn bragi_level_percent(raw: u16) -> Option<u32> {
    (raw <= 1000).then(|| (u32::from(raw) + 5) / 10)
}

/// Property `0x0010` is the charge state: `1` = charging, `2` = on battery, `3` = full.
/// Anything else is not a charge state (and never a level).
pub fn bragi_charging_state(raw: u16) -> Option<bool> {
    match raw {
        1 => Some(true),
        2 | 3 => Some(false),
        _ => None,
    }
}

/// Level from a single Bragi `0x000F` reply (route-agnostic, used by [`probe_report`]).
pub fn parse_corsair_bragi_level(reply: &[u8]) -> Option<u32> {
    let r = corsair_framing(reply);
    if !valid_bragi(r) {
        return None;
    }
    bragi_level_percent(u16::from_le_bytes([r[3], r[4]]))
}

/// Charge state from a single Bragi `0x0010` reply.
pub fn parse_corsair_bragi_charging(reply: &[u8]) -> Option<bool> {
    let r = corsair_framing(reply);
    if !valid_bragi(r) {
        return None;
    }
    bragi_charging_state(u16::from_le_bytes([r[3], r[4]]))
}

/// Legacy Void / HS-series status block answering `C9 64`.
pub fn is_corsair_legacy_reply(reply: &[u8]) -> bool {
    let r = corsair_framing(reply);
    r.len() >= 5 && (r.starts_with(&[0x64, 0x00]) || r.starts_with(&[0xC9, 0x64]))
}

/// Legacy layout: `data[2] & 0x7F` = level (bit 7 is the mic-boom flag), `data[4]` = state
/// (`0` offline → rejected, `1`/`2` on battery, `4`/`5` charging).
pub fn parse_corsair_legacy(reply: &[u8], id: &str, name: &str) -> Option<BatteryReading> {
    if !is_corsair_legacy_reply(reply) {
        return None;
    }
    let r = corsair_framing(reply);
    let level = r[2] & 0x7f;
    let charging = matches!(r[4], 4 | 5);
    if !matches!(r[4], 1 | 2 | 4 | 5) || level > 100 {
        return None;
    }
    Some(BatteryReading::new(
        id,
        if name.trim().is_empty() {
            "Corsair Wireless Headset"
        } else {
            name
        },
        level as u32,
        charging,
        BatterySource::CorsairHid,
    ))
}

pub fn is_corsair_interface(d: &HidInterfaceDescriptor) -> bool {
    d.vendor_id == CORSAIR_VID
        && matches!(d.usage_page, BRAGI_USAGE_PAGE | CORSAIR_LEGACY_USAGE_PAGE)
        && matches!(d.input_report_length, 64 | 65)
}

// ---------------------------------------------------------------------------------------------
// 3. HyperX (VID 0x03F0 / Kingston 0x0951, four protocol families)
// ---------------------------------------------------------------------------------------------

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum HyperXFamily {
    Ff00,
    Ff13,
    Ff90,
    Ffc0,
}

pub fn hyperx_family(usage_page: u16, vendor_id: u16) -> Option<HyperXFamily> {
    if !matches!(vendor_id, 0x03F0 | 0x0951) {
        return None;
    }
    match usage_page {
        0xFF00 => Some(HyperXFamily::Ff00),
        0xFF13 => Some(HyperXFamily::Ff13),
        0xFF90 => Some(HyperXFamily::Ff90),
        0xFFC0 => Some(HyperXFamily::Ffc0),
        _ => None,
    }
}

pub fn build_hyperx_request(family: HyperXFamily, report_len: usize) -> Vec<u8> {
    let len = report_len.max(64);
    let mut packet = vec![0u8; len];
    let offset = usize::from(len == 65);
    // Each family uses a distinct command prefix; the final byte is the battery query.
    (packet[offset], packet[offset + 1], packet[offset + 2]) = match family {
        HyperXFamily::Ff00 => (0x03, 0x00, 0x08),
        HyperXFamily::Ff13 => (0x05, 0x00, 0x08),
        HyperXFamily::Ff90 => (0x06, 0x00, 0x08),
        HyperXFamily::Ffc0 => (0xC0, 0x00, 0x08),
    };
    packet
}

fn valid_hyperx(family: HyperXFamily, r: &[u8]) -> bool {
    let expected = match family {
        HyperXFamily::Ff00 => 0x03,
        HyperXFamily::Ff13 => 0x05,
        HyperXFamily::Ff90 => 0x06,
        HyperXFamily::Ffc0 => 0xC0,
    };
    let start = usize::from(r.len() >= 65 && r[0] == 0);
    r.len() >= start + 4 && r[start] == expected && r[start + 2] == 0x08
}

/// Parses a HyperX reply for any of the four protocol families.
pub fn parse_hyperx(
    family: HyperXFamily,
    reply: &[u8],
    id: &str,
    name: &str,
) -> Option<BatteryReading> {
    let start = usize::from(reply.len() >= 65 && reply[0] == 0);
    let r = &reply[start..];
    if !valid_hyperx(family, r) {
        return None;
    }
    let level = r[3];
    let charging = r.get(4).is_some_and(|b| b & 0x01 != 0);
    if level == 0 || level > 100 {
        return None;
    }
    Some(BatteryReading::new(
        id,
        if name.trim().is_empty() {
            "HyperX Wireless"
        } else {
            name
        },
        level as u32,
        charging,
        BatterySource::HyperXHid,
    ))
}

// ---------------------------------------------------------------------------------------------
// 4. Compx / ATK / VXE / Mchose / Lamzu / Keychron ... (13+ VIDs, 17/64/65/9-byte reports)
// ---------------------------------------------------------------------------------------------

pub const COMPX_VENDORS: &[u16] = &[
    0x3554, 0x373B, 0x36A7, 0x36B0, 0x3710, 0x372E, 0x359B, 0x248A, 0x3151, 0x373E, 0x3434, 0x260D,
    0x24AE,
];
pub const COMPX_REPORT_LEN: usize = 17;
pub const COMPX_QUERY_CMD: u8 = 0x04;

/// Compx checksum: `0x4D - sum(bytes[1..16])`.
pub fn compx_checksum(packet: &[u8]) -> u8 {
    let mut sum = 0u32;
    for &b in packet.iter().skip(1).take(COMPX_REPORT_LEN - 2) {
        sum = sum.wrapping_add(b as u32);
    }
    (0x4Du32.wrapping_sub(sum & 0xFF)) as u8
}

pub fn build_compx_request() -> [u8; COMPX_REPORT_LEN] {
    let mut packet = [0u8; COMPX_REPORT_LEN];
    packet[0] = 0x08;
    packet[1] = COMPX_QUERY_CMD;
    packet[16] = 0x49;
    packet
}

fn normalize_nordic(reply: &[u8]) -> &[u8] {
    if reply.len() >= 10 && reply[0] == 0 && matches!(reply[1], 0x08 | 0x04) {
        &reply[1..]
    } else {
        reply
    }
}

/// Standard 17-byte Compx/ATK/VXE report with checksum.
pub fn parse_compx17(reply: &[u8], id: &str, name: &str) -> Option<BatteryReading> {
    if reply.len() != COMPX_REPORT_LEN || reply[0] != 0x08 || reply[1] != COMPX_QUERY_CMD {
        return None;
    }
    if compx_checksum(reply) != reply[COMPX_REPORT_LEN - 1] {
        return None;
    }
    let level = reply[6];
    let charging = reply[7];
    let voltage = u16::from_be_bytes([reply[8], reply[9]]) as u32;
    if level > 100 || charging > 1 || (level == 0 && charging == 0 && voltage == 0) {
        return None;
    }
    let mut reading = BatteryReading::new(
        id,
        if name.trim().is_empty() {
            "ATK Wireless Mouse"
        } else {
            name
        },
        level as u32,
        charging == 1,
        BatterySource::CompxAtkVxe,
    );
    if voltage > 0 {
        reading.voltage_mv = Some(voltage);
    }
    Some(reading)
}

/// 64/65-byte Nordic-framed variant.
/// The framing is identified *before* any field validation: an exact 17-byte read is the
/// checksummed Compx frame; a 64/65-byte read is either the same `08 04` frame padded (its
/// 17-byte checksum must still hold) or the short Nordic `04` layout.
pub fn parse_compx_nordic(reply: &[u8], id: &str, name: &str) -> Option<BatteryReading> {
    if reply.len() == COMPX_REPORT_LEN {
        return parse_compx17(reply, id, name);
    }
    if !matches!(reply.len(), 64 | 65) {
        return None;
    }
    let r = normalize_nordic(reply);
    let base = if r.len() >= COMPX_REPORT_LEN && r[0] == 0x08 && r[1] == COMPX_QUERY_CMD {
        if compx_checksum(r) != r[COMPX_REPORT_LEN - 1] {
            return None;
        }
        6
    } else if r.len() >= 9 && r[0] == COMPX_QUERY_CMD {
        5
    } else {
        return None;
    };
    let level = *r.get(base)?;
    let charging = *r.get(base + 1)?;
    let voltage = u16::from_be_bytes([*r.get(base + 2)?, *r.get(base + 3)?]) as u32;
    if level > 100 || charging > 1 || (level == 0 && charging == 0 && voltage == 0) {
        return None;
    }
    let mut reading = BatteryReading::new(
        id,
        if name.trim().is_empty() {
            "Wireless Gaming Mouse"
        } else {
            name
        },
        level as u32,
        charging == 1,
        BatterySource::CompxAtkVxe,
    );
    if (2800..=4500).contains(&voltage) {
        reading.voltage_mv = Some(voltage);
    }
    Some(reading)
}

/// 9-byte ATK 1K dongle feature report (`0x82 0x08 0x02 0x00 0xFF 0x03 ... packed`).
pub fn parse_atk1k(reply: &[u8], id: &str, name: &str) -> Option<BatteryReading> {
    let r = if reply.len() >= 10 && reply[0] == 0 && reply[1] == 0x82 {
        &reply[1..]
    } else {
        reply
    };
    if r.len() < 9 || r[..6] != [0x82, 0x08, 0x02, 0x00, 0xFF, 0x03] {
        return None;
    }
    let packed = r[8];
    // The 7-bit field can encode up to 127: anything above 100 is rejected here, never
    // clamped into a fake 100%.
    let level = packed & 0x7F;
    let charging = packed & 0x80 != 0;
    if level > 100 || (level == 0 && !charging) {
        return None;
    }
    Some(BatteryReading::new(
        id,
        if name.trim().is_empty() {
            "ATK 1K Wireless Mouse"
        } else {
            name
        },
        level as u32,
        charging,
        BatterySource::CompxAtkVxe,
    ))
}

pub fn is_compx_vendor(vendor_id: u16) -> bool {
    COMPX_VENDORS.contains(&vendor_id)
}

// ---------------------------------------------------------------------------------------------
// 5. 2.4 GHz keyboards (Sinowealth/Compx/Akko receivers, 17/64/65-byte reports)
// ---------------------------------------------------------------------------------------------

fn normalize_keyboard_24g_framing(raw: &[u8]) -> &[u8] {
    if raw.len() >= 4 && raw[0] == 0x00 && matches!(raw[1], 0x08 | 0x04 | 0x83) {
        &raw[1..]
    } else {
        raw
    }
}

pub fn parse_keyboard_24g(
    reply: &[u8],
    report_len: usize,
    id: &str,
    name: &str,
) -> Option<BatteryReading> {
    if report_len == COMPX_REPORT_LEN {
        return parse_compx17(reply, id, name).map(|mut r| {
            r.source = BatterySource::Keyboard24gHid;
            r.device_kind = WirelessDeviceKind::Keyboard;
            r
        });
    }
    let r = normalize_keyboard_24g_framing(reply);
    if r.len() < 3 {
        return None;
    }
    let (level, charging) = if r.len() >= 10 && r[0] == 0x08 && r[1] == COMPX_QUERY_CMD {
        if r.len() >= COMPX_REPORT_LEN && compx_checksum(r) != r[COMPX_REPORT_LEN - 1] {
            return None;
        }
        let level = r[6];
        let charge_flag = r[7];
        let voltage_mv = u16::from_be_bytes([r[8], r[9]]);
        if level > 100 || charge_flag > 1 || (level == 0 && charge_flag == 0 && voltage_mv == 0) {
            return None;
        }
        (level, charge_flag == 1)
    } else if r.len() >= 9 && r[0] == COMPX_QUERY_CMD {
        let level = r[5];
        let charge_flag = r[6];
        let voltage_mv = u16::from_be_bytes([r[7], r[8]]);
        if level > 100 || charge_flag > 1 || (level == 0 && charge_flag == 0 && voltage_mv == 0) {
            return None;
        }
        (level, charge_flag == 1)
    } else if r[0] == 0x83 {
        // Sinowealth 64-byte keyboard battery response: [0x83, chargingFlag (0/1), batteryPercent (1..100), ...]
        if r[1] > 1 {
            return None;
        }
        let charging = r[1] == 1;
        let level = r[2];
        if level == 0 || level > 100 {
            return None;
        }
        (level, charging)
    } else {
        return None;
    };
    let clean_name = if name.trim().is_empty() {
        "2.4G Wireless Keyboard"
    } else {
        name.trim()
    };
    if is_ignored_wired(0, 0, clean_name) {
        return None;
    }
    let mut reading = BatteryReading::new(
        id,
        clean_name,
        u32::from(level),
        charging,
        BatterySource::Keyboard24gHid,
    );
    reading.device_kind = WirelessDeviceKind::Keyboard;
    Some(reading)
}

pub fn is_keyboard_24g(vendor_id: u16, product_id: u16) -> bool {
    if vendor_id == 0x258A && (0x2011..=0x2022).contains(&product_id) {
        return false;
    }
    matches!(vendor_id, 0x258A)
        || (vendor_id == 0x3554 && product_id == 0xFA09)
        || (vendor_id == 0x3151 && matches!(product_id, 0x4011 | 0x4015))
}

// ---------------------------------------------------------------------------------------------
// 6. Gamepads: DualSense (0x01 USB / 0x31 BT), DualShock4 (0x01 / 0x11), Switch Pro (0x30)
// ---------------------------------------------------------------------------------------------

/// `true` when everything after the report id is zero: a padded short report (e.g. the
/// Bluetooth "simple" `0x01` report read into a full-size buffer), never a real state.
fn is_zero_padded(report: &[u8]) -> bool {
    report.iter().skip(1).all(|&b| b == 0)
}

/// DualSense status byte (Linux `hid-playstation`): low nibble = capacity step, high nibble =
/// state (`0` discharging, `1` charging, `2` full; `0xA`/`0xB`/`0xF` are error states).
/// Framing is transport-specific: USB report `0x01` is exactly 64 bytes (status at 53),
/// Bluetooth extended report `0x31` is 78 bytes (status at 54).
pub fn parse_dualsense(report: &[u8], id: &str, name: &str) -> Option<BatteryReading> {
    let offset = match report.first() {
        Some(0x01) if report.len() == 64 => 53,
        Some(0x31) if report.len() == 78 => 54,
        _ => return None,
    };
    if is_zero_padded(report) {
        return None;
    }
    let raw = report[offset];
    let step = u32::from(raw & 0x0F);
    let (level, charging) = match (raw >> 4) & 0x0F {
        0x0 => ((step * 10 + 5).min(100), false),
        0x1 => ((step * 10 + 5).min(100), true),
        0x2 => (100, false),
        _ => return None,
    };
    Some(BatteryReading::new(
        id,
        if name.trim().is_empty() {
            "DualSense"
        } else {
            name
        },
        level,
        charging,
        BatterySource::GamepadHid,
    ))
}

/// DualShock 4 status byte (Linux `hid-playstation`): low nibble = capacity, bit 4 = cable.
/// On cable `0..=9` charging (midpoint), `10` charging at 100%, `11` full; other values are
/// errors. On battery `0..=9` midpoint, `10+` = 100%. USB report `0x01` is 64 bytes (status
/// at 30), Bluetooth report `0x11` is 78 bytes (status at 32).
pub fn parse_dualshock4(report: &[u8], id: &str, name: &str) -> Option<BatteryReading> {
    let offset = match report.first() {
        Some(0x01) if report.len() == 64 => 30,
        Some(0x11) if report.len() == 78 => 32,
        _ => return None,
    };
    if is_zero_padded(report) {
        return None;
    }
    let raw = report[offset];
    let cable = raw & 0x10 != 0;
    let step = u32::from(raw & 0x0F);
    let (level, charging) = if cable {
        match step {
            0..=9 => (step * 10 + 5, true),
            10 => (100, true),
            11 => (100, false),
            _ => return None,
        }
    } else if step < 10 {
        (step * 10 + 5, false)
    } else {
        (100, false)
    };
    Some(BatteryReading::new(
        id,
        if name.trim().is_empty() {
            "DualShock 4"
        } else {
            name
        },
        level,
        charging,
        BatterySource::GamepadHid,
    ))
}

/// Switch Pro full report `0x30`, byte 2 high nibble: bits 7..5 = capacity state
/// (`4` full, `3` medium, `2` low, `1` critical, `0` empty; `5..=7` reserved → rejected),
/// bit 4 = charging. Coarse states map to honest band values, empty stays 0%.
pub fn parse_switch_pro(report: &[u8], id: &str, name: &str) -> Option<BatteryReading> {
    if report.len() < 13 || report[0] != 0x30 || is_zero_padded(report) {
        return None;
    }
    let raw = report[2];
    let charging = raw & 0x10 != 0;
    let level = match (raw >> 5) & 0x07 {
        4 => 100,
        3 => 60,
        2 => 30,
        1 => 10,
        0 => 0,
        _ => return None,
    };
    Some(BatteryReading::new(
        id,
        if name.trim().is_empty() {
            "Nintendo Switch Pro"
        } else {
            name
        },
        level,
        charging,
        BatterySource::GamepadHid,
    ))
}

pub fn parse_gamepad(
    vendor: u16,
    product: u16,
    report: &[u8],
    id: &str,
    name: &str,
) -> Option<BatteryReading> {
    match (vendor, product) {
        (0x054C, 0x0CE6 | 0x0DF2) => parse_dualsense(report, id, name),
        (0x054C, 0x05C4 | 0x09CC) => parse_dualshock4(report, id, name),
        (0x057E, 0x2009) => parse_switch_pro(report, id, name),
        _ => None,
    }
}

// ---------------------------------------------------------------------------------------------
// 7. Logitech G-Hub loopback JSON + HID++ battery feature parsers
// ---------------------------------------------------------------------------------------------

pub fn is_valid_ghub_uri(uri: &str) -> bool {
    let lower = uri.to_ascii_lowercase();
    lower.contains("127.0.0.1") || lower.contains("localhost") || lower.starts_with("http://[::1]")
}

/// Parses G-Hub loopback JSON into readings (`ParseGHubJson` / `ExtractGHubDevices`).
pub fn parse_ghub_json(json: &str) -> Vec<BatteryReading> {
    let Ok(root) = serde_json::from_str::<serde_json::Value>(json) else {
        return Vec::new();
    };
    let mut out = Vec::new();
    extract_loopback_devices(&root, BatterySource::LogitechGHub, &mut out);
    out
}

fn json_str<'a>(v: &'a serde_json::Value, keys: &[&str]) -> Option<&'a str> {
    for k in keys {
        if let Some(s) = v.get(k).and_then(|x| x.as_str()) {
            return Some(s);
        }
    }
    None
}

fn json_num(v: &serde_json::Value, keys: &[&str]) -> Option<f64> {
    keys.iter()
        .find_map(|k| json_percent(v.get(k)?).map(f64::from))
}

/// Battery percentage from a JSON number. Integers are always percentages (`1` = 1%, never
/// 100%); only an explicit floating-point value strictly between 0 and 1 is a fraction.
fn json_percent(value: &serde_json::Value) -> Option<u32> {
    let number = value.as_f64()?;
    if !number.is_finite() || !(0.0..=100.0).contains(&number) {
        return None;
    }
    let is_float = value.is_f64();
    let percent = if is_float && number > 0.0 && number < 1.0 {
        number * 100.0
    } else {
        number
    };
    Some(percent.round() as u32)
}

/// `/devices/list` reply -> `(deviceId, displayName)` for battery-capable devices.
pub fn parse_ghub_device_list(json: &str) -> Vec<(String, String)> {
    let Ok(root) = serde_json::from_str::<serde_json::Value>(json) else {
        return Vec::new();
    };
    let payload = root.get("payload").unwrap_or(&root);
    let Some(items) = payload
        .get("deviceInfos")
        .or_else(|| payload.get("devices"))
        .and_then(|v| v.as_array())
    else {
        return Vec::new();
    };
    items
        .iter()
        .filter(|item| {
            item.get("capabilities")
                .and_then(|c| c.get("hasBatteryStatus"))
                .and_then(|b| b.as_bool())
                != Some(false)
        })
        .filter_map(|item| {
            let id = json_str(item, &["id", "deviceId"])?.trim().to_owned();
            if id.is_empty() || id.contains(['/', '?', '#']) {
                return None;
            }
            let name = json_str(item, &["extendedDisplayName", "displayName", "name"])
                .map(str::trim)
                .filter(|n| !n.is_empty())
                .unwrap_or("Logitech Wireless")
                .to_owned();
            Some((id, name))
        })
        .collect()
}

/// `/battery/<deviceId>/state` reply -> `(percent, charging)`. A non-success result code or
/// a missing / out-of-range percentage yields `None`.
pub fn parse_ghub_battery_state(json: &str) -> Option<(u32, bool)> {
    let root = serde_json::from_str::<serde_json::Value>(json).ok()?;
    if let Some(code) = root
        .get("result")
        .and_then(|r| r.get("code"))
        .and_then(|c| c.as_str())
        && !code.eq_ignore_ascii_case("SUCCESS")
    {
        return None;
    }
    let payload = root.get("payload").unwrap_or(&root);
    let level = json_percent(payload.get("percentage")?)?;
    let charging = payload
        .get("charging")
        .and_then(|c| c.as_bool())
        .unwrap_or(false);
    Some((level, charging))
}

fn extract_loopback_devices(
    node: &serde_json::Value,
    source: BatterySource,
    out: &mut Vec<BatteryReading>,
) {
    match node {
        serde_json::Value::Array(items) => {
            for item in items {
                extract_loopback_devices(item, source, out);
            }
        }
        serde_json::Value::Object(map) => {
            let name = json_str(node, &["name", "deviceName", "displayName"])
                .unwrap_or(if source == BatterySource::SteelSeriesGg {
                    "SteelSeries Wireless"
                } else {
                    "Logitech Wireless"
                })
                .to_owned();
            let level = json_num(node, &["batteryLevel", "battery", "level", "percentage"])
                .or_else(|| {
                    json_num(
                        node.get("battery")?,
                        &["level", "percentage", "chargingLevel"],
                    )
                });
            if let Some(level) = level {
                // `json_num` already applied the integer-vs-fraction rule.
                let level = level.round() as u32;
                if level <= 100 {
                    let id = json_str(node, &["id", "deviceId", "serial"])
                        .map(str::to_owned)
                        .unwrap_or_else(|| format!("ghub:{name}"));
                    let charging = node
                        .get("charging")
                        .and_then(|x| x.as_bool())
                        .unwrap_or(false);
                    out.push(BatteryReading::new(&id, &name, level, charging, source));
                    return;
                }
            }
            for value in map.values() {
                extract_loopback_devices(value, source, out);
            }
        }
        _ => {}
    }
}

/// HID++ `BatteryStatus` feature reply (0x1000) -> percent.
pub fn parse_hidpp_battery_status(reply: &[u8]) -> Option<(u32, bool)> {
    if reply.len() < 7 || reply[0] != 0x10 {
        return None;
    }
    let level = reply[2];
    if level > 100 {
        return None;
    }
    Some((level as u32, reply[3] & 0x01 != 0))
}

// ---------------------------------------------------------------------------------------------
// 8. SteelSeries GG coreProps loopback + HID parsers
// ---------------------------------------------------------------------------------------------

/// Finds a loopback endpoint in `coreProps.json` data (`"gcEndpoint": "http://127.0.0.1:..."`) .
pub fn parse_coreprops_endpoint(json: &str) -> Option<String> {
    let root: serde_json::Value = serde_json::from_str(json).ok()?;
    fn find(node: &serde_json::Value) -> Option<String> {
        match node {
            serde_json::Value::Object(map) => {
                for (k, v) in map {
                    let lower = k.to_ascii_lowercase();
                    if lower.contains("endpoint")
                        && lower.contains("gc")
                        && let Some(url) = v.as_str()
                    {
                        return Some(url.to_owned());
                    }
                }
                for v in map.values() {
                    if let Some(found) = find(v) {
                        return Some(found);
                    }
                }
                None
            }
            serde_json::Value::Array(items) => items.iter().find_map(find),
            _ => None,
        }
    }
    find(&root).filter(|u| is_valid_ss_endpoint(u))
}

pub fn is_valid_ss_endpoint(url: &str) -> bool {
    let lower = url.to_ascii_lowercase();
    (lower.contains("127.0.0.1") || lower.contains("localhost") || lower.contains("[::1]"))
        && lower.starts_with("http")
}

/// Parses SteelSeries GG battery JSON (loopback `/battery` payload).
pub fn parse_steelseries_gg_json(json: &str) -> Vec<BatteryReading> {
    let Ok(root) = serde_json::from_str::<serde_json::Value>(json) else {
        return Vec::new();
    };
    let mut out = Vec::new();
    extract_loopback_devices(&root, BatterySource::SteelSeriesGg, &mut out);
    out
}

/// SteelSeries HID status byte map: 0x01 offline, 0x02 charging, 0x08 discharging.
pub fn parse_steelseries_hid(reply: &[u8], id: &str, name: &str) -> Option<BatteryReading> {
    if reply.len() < 4 {
        return None;
    }
    let status = reply[1];
    let level = reply[2];
    if !matches!(status, 0x01 | 0x02 | 0x08) || level > 100 {
        return None;
    }
    Some(BatteryReading::new(
        id,
        if name.trim().is_empty() {
            "SteelSeries Wireless"
        } else {
            name
        },
        level as u32,
        status == 0x02,
        BatterySource::SteelSeriesHid,
    ))
}

// ---------------------------------------------------------------------------------------------
// 9. BLE GATT BatteryLife / DevPkeyIsPresent / AepId filter
// ---------------------------------------------------------------------------------------------

pub const BLE_BATTERY_SERVICE: u16 = 0x180F;
pub const BLE_BATTERY_LEVEL_CHAR: u16 = 0x2A19;

/// True when a PnP association id looks like a BLE AEP id (`..._aep_...` / `BTHENUM` GATT).
pub fn is_ble_aep_id(id: &str) -> bool {
    let lower = id.to_ascii_lowercase();
    lower.contains("aep")
        || (lower.contains("bthenum") && lower.contains("180f"))
        || lower.contains("blegatt")
}

/// Parses a BLE BatteryLevel characteristic payload (one byte) with optional PnP percentage.
pub fn parse_ble_gatt(payload: &[u8], id: &str, name: &str) -> Option<BatteryReading> {
    let &[level] = payload else {
        return None;
    };
    if level > 100 {
        return None;
    }
    Some(BatteryReading::new(
        id,
        if name.trim().is_empty() {
            "BLE Device"
        } else {
            name
        },
        level as u32,
        false,
        BatterySource::BluetoothGatt,
    ))
}

// ---------------------------------------------------------------------------------------------
// 10. Aggregator: cache, outlier guard, dedup, settings, diagnostics
// ---------------------------------------------------------------------------------------------

pub const OUTLIER_DROP_THRESHOLD: i32 = 25;
pub const OUTLIER_FLOOR: i32 = 25;
pub const MAX_CACHE_BYTES: usize = 65_536;
pub const WRAITH_VID: u16 = 0x2E3C;
pub const WRAITH_W75_PID: u16 = 0xC368;

pub fn is_ignored_wired(vendor: u16, product: u16, name: &str) -> bool {
    (vendor == WRAITH_VID && matches!(product, WRAITH_W75_PID | 0))
        || name.to_ascii_lowercase().contains("wraith w75")
}

/// Privacy-safe HID diagnostic summary: only numeric IDs, usages and report lengths.
pub fn privacy_diagnostic_summary(descriptors: &[HidInterfaceDescriptor]) -> String {
    let mut lines: Vec<String> = descriptors
        .iter()
        .filter(|d| !is_ignored_wired(d.vendor_id, d.product_id, ""))
        .map(|d| {
            format!(
                "VID:{:04X} PID:{:04X} UsagePage:0x{:04X} Usage:0x{:04X} In:{} Out:{} Feat:{}",
                d.vendor_id,
                d.product_id,
                d.usage_page,
                d.usage,
                d.input_report_length,
                d.output_report_length,
                d.feature_report_length
            )
        })
        .collect();
    lines.sort();
    lines.dedup();
    if lines.is_empty() {
        "HID Interfaces: 0".to_owned()
    } else {
        lines.join("\n")
    }
}

/// Dedup by physical identity ([`physical_key`]), never by display name: two identical-name
/// peripherals at different levels stay two rows.
pub fn deduplicate(readings: Vec<BatteryReading>) -> Vec<BatteryReading> {
    let mut by_key: HashMap<String, BatteryReading> = HashMap::new();
    for r in readings {
        let key = physical_key(&r.id);
        match by_key.get_mut(&key) {
            Some(existing) if ws_g_prefer(&r, existing) => {
                let mut preferred = r;
                merge_metadata(&mut preferred, existing);
                *existing = preferred;
            }
            Some(existing) => merge_metadata(existing, &r),
            None => {
                by_key.insert(key, r);
            }
        }
    }
    by_key.into_values().collect()
}

/// Ranking for two samples of the same physical battery: verified live connection first,
/// then a known level, then non-stale provenance, then measurement provenance
/// ([`BatterySource::priority`]) — so a live uncached GATT 42% beats a PnP 80%.
fn ws_g_prefer(candidate: &BatteryReading, existing: &BatteryReading) -> bool {
    if candidate.is_live() != existing.is_live() {
        return candidate.is_live();
    }
    if candidate.connected != existing.connected {
        return candidate.connected;
    }
    let c_valid = candidate.level >= 0 || candidate.level_override.is_some();
    let e_valid = existing.level >= 0 || existing.level_override.is_some();
    if c_valid != e_valid {
        return c_valid;
    }
    if candidate.stale != existing.stale {
        return !candidate.stale;
    }
    candidate.source.priority() < existing.source.priority()
}

fn merge_metadata(primary: &mut BatteryReading, secondary: &BatteryReading) {
    if primary.voltage_mv.is_none() {
        primary.voltage_mv = secondary.voltage_mv;
    }
    if primary.secondary_level.is_none() {
        primary.secondary_level = secondary.secondary_level;
    }
    if primary.device_kind == WirelessDeviceKind::Other
        && secondary.device_kind != WirelessDeviceKind::Other
    {
        primary.device_kind = secondary.device_kind;
    }
}

#[derive(Clone, Default)]
pub struct OutlierGuard {
    pending: HashMap<String, i32>,
}

/// The guard is process-wide: a fresh aggregator is built on every poll, but
/// the "one poll of doubt" state must survive between those polls or the
/// >=25-point drop protection is silently bypassed on every second sample.
static PERSISTENT_GUARD: OnceLock<Mutex<OutlierGuard>> = OnceLock::new();

/// Access to the live outlier guard (also used by tests to verify persistence).
pub fn persistent_guard() -> MutexGuard<'static, OutlierGuard> {
    PERSISTENT_GUARD
        .get_or_init(|| Mutex::new(OutlierGuard::default()))
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

impl OutlierGuard {
    /// Returns `(accepted_level, was_guarded)`. A single implausible >=25-point drop from a
    /// trusted >25% reading is held for one poll; two consecutive drops are accepted.
    pub fn check(&mut self, key: &str, prev: i32, fresh: i32) -> (i32, bool) {
        if prev > OUTLIER_FLOOR && prev - fresh >= OUTLIER_DROP_THRESHOLD {
            if self
                .pending
                .get(key)
                .is_some_and(|p| prev - *p >= OUTLIER_DROP_THRESHOLD)
            {
                self.pending.remove(key);
                (fresh, false)
            } else {
                self.pending.insert(key.to_owned(), fresh);
                (prev, true)
            }
        } else {
            self.pending.remove(key);
            (fresh, false)
        }
    }
}

/// Settings consumed by the aggregator (the corresponding `DockSettings` fields are WS-A owned).
#[derive(Clone, Debug)]
pub struct AggregatorSettings {
    pub poll_seconds: u32,
    pub low_threshold: u32,
    pub ignored_devices: Vec<String>,
    pub aliases: HashMap<String, String>,
    pub listen_only: bool,
    pub multi_brand_enabled: bool,
    pub disabled_vendors: Vec<String>,
}

impl Default for AggregatorSettings {
    fn default() -> Self {
        Self {
            poll_seconds: 30,
            low_threshold: 20,
            ignored_devices: Vec::new(),
            aliases: HashMap::new(),
            listen_only: false,
            multi_brand_enabled: true,
            disabled_vendors: Vec::new(),
        }
    }
}

impl AggregatorSettings {
    /// `validate()` range for `WirelessBatteryPollSeconds` is 15..=300.
    pub fn poll_seconds_clamped(&self) -> u32 {
        self.poll_seconds.clamp(15, 300)
    }

    pub fn is_ignored(&self, reading: &BatteryReading) -> bool {
        Self::is_ignored_fields(&self.ignored_devices, &reading.id, &reading.name)
    }

    pub fn display_name(&self, reading: &BatteryReading) -> String {
        Self::display_name_fields(&self.aliases, &reading.id, &reading.name)
    }

    pub fn allows(&self, source: BatterySource) -> bool {
        ProbePolicy::from(self).allows_source(source)
    }

    /// Applies the same ignore / alias / listen-only / multi-brand / vendor policy to live readings.
    pub fn allows_reading(&self, reading: &WirelessReading) -> bool {
        !Self::is_ignored_fields(&self.ignored_devices, &reading.id, &reading.name)
            && self.allows(reading.source)
    }

    pub fn display_name_of(&self, reading: &WirelessReading) -> String {
        Self::display_name_fields(&self.aliases, &reading.id, &reading.name)
    }

    pub(crate) fn is_ignored_id_or_name(&self, id: &str, name: &str) -> bool {
        Self::is_ignored_fields(&self.ignored_devices, id, name)
    }

    pub(crate) fn display_name_id_or_name(&self, id: &str, name: &str) -> String {
        Self::display_name_fields(&self.aliases, id, name)
    }

    /// Full policy for one aggregated row (fresh or retained): ignore list, listen-only /
    /// multi-brand / disabled-vendor source gating and the per-VID vendor switch.
    fn allows_battery_reading(&self, policy: &ProbePolicy, reading: &BatteryReading) -> bool {
        let vid = reading
            .id
            .split(':')
            .nth(1)
            .filter(|part| part.len() == 4)
            .and_then(|vid| u16::from_str_radix(vid, 16).ok())
            .unwrap_or(0);
        !self.is_ignored(reading)
            && policy.allows_source(reading.source)
            && !policy.is_vid_disabled(vid, &reading.name)
    }

    fn is_ignored_fields(ignored: &[String], id: &str, name: &str) -> bool {
        let name = name.to_ascii_lowercase();
        let id = id.to_ascii_lowercase();
        let physical = physical_key(&id);
        ignored.iter().any(|x| {
            let x = x.trim().to_ascii_lowercase();
            !x.is_empty() && (id == x || physical == physical_key(&x) || name.contains(&x))
        })
    }

    fn display_name_fields(aliases: &HashMap<String, String>, id: &str, name: &str) -> String {
        let id = id.to_ascii_lowercase();
        aliases
            .get(&id)
            .or_else(|| {
                let physical = physical_key(&id);
                aliases
                    .iter()
                    .find(|(key, _)| physical_key(key) == physical)
                    .map(|(_, alias)| alias)
            })
            .cloned()
            .unwrap_or_else(|| name.to_owned())
    }
}

impl From<&DockSettings> for AggregatorSettings {
    fn from(settings: &DockSettings) -> Self {
        Self {
            poll_seconds: settings.wireless_battery_poll_seconds.max(0) as u32,
            low_threshold: settings.wireless_battery_low_threshold.max(0) as u32,
            ignored_devices: settings.ignored_wireless_devices.clone(),
            aliases: settings
                .wireless_device_aliases
                .iter()
                .map(|(id, alias)| (id.to_ascii_lowercase(), alias.clone()))
                .collect(),
            listen_only: settings.wireless_battery_listen_only,
            multi_brand_enabled: settings.wireless_multi_brand_enabled(),
            disabled_vendors: settings.wireless_disabled_vendors(),
        }
    }
}

fn wall_clock_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}

/// Converts a live transport reading into the aggregator's richer type. The sample's Unix
/// time is fixed here, once; it is what gets persisted and what expiry is measured against.
fn to_battery_reading(item: &WirelessReading) -> BatteryReading {
    let level = item.level_percent_opt();
    let mut reading = BatteryReading::new(
        &item.id,
        &item.name,
        u32::from(level.unwrap_or(0)),
        item.is_charging,
        item.source,
    );
    reading.sampled_at = item.sampled_at;
    reading.sampled_unix =
        Some(wall_clock_secs().saturating_sub(item.sampled_at.elapsed().as_secs()));
    if level.is_none() {
        reading.level = -1;
    }
    reading.connected = item.is_connected;
    reading.stale = item.stale;
    reading.device_kind = item.device_kind;
    reading.secondary_level = item
        .secondary_level
        .map(|v| (v * 100.0).round().clamp(0.0, 100.0) as i32);
    reading
}

fn to_wireless_reading(item: BatteryReading) -> WirelessReading {
    let secondary_level = item
        .secondary_level
        .filter(|&v| (0..=100).contains(&v))
        .map(percent_01);
    let unknown = item.level < 0;
    let mut wireless = reading_with_meta(
        item.id,
        item.name,
        percent_01(item.level.max(0)),
        item.connected,
        item.charging,
        item.source,
        item.device_kind,
        secondary_level,
    );
    wireless.sampled_at = item.sampled_at;
    if unknown {
        // Unknown charge stays unknown (`level_percent_opt() == None`), never 0%.
        wireless.level_01 = -1.0;
    }
    wireless.stale = item.stale;
    wireless
}

static AGG_GATE: OnceLock<Mutex<()>> = OnceLock::new();
static LAST_AGGREGATE: Mutex<Option<(Instant, Vec<WirelessReading>, bool)>> = Mutex::new(None);

#[cfg(test)]
fn cached_aggregate() -> Option<Vec<WirelessReading>> {
    LAST_AGGREGATE
        .lock()
        .ok()?
        .as_ref()
        .map(|(_, readings, _)| readings.clone())
}

fn cached_aggregate_outcome() -> Option<WirelessPollOutcome> {
    LAST_AGGREGATE
        .lock()
        .ok()?
        .as_ref()
        .map(|(_, readings, had_native_bluetooth)| WirelessPollOutcome {
            readings: readings.clone(),
            had_native_bluetooth: *had_native_bluetooth,
        })
}

/// Result of a native wireless battery poll, preserving whether native Bluetooth providers
/// observed any candidate readings before `IgnoredWirelessDevices` filtering.
pub(crate) struct WirelessPollOutcome {
    pub readings: Vec<WirelessReading>,
    pub had_native_bluetooth: bool,
}

/// Whether the PowerShell Bluetooth PnP fallback is allowed under `settings` (returns
/// `false` when `"bluetooth"` / `"ble"` is in `WirelessDisabledVendors`).
pub(crate) fn allows_bluetooth_fallback(settings: &DockSettings) -> bool {
    AggregatorSettings::from(settings).allows(BatterySource::Bluetooth)
}

/// The aggregate snapshot may be reused only for a non-forced poll inside its interval.
fn aggregate_is_reusable(sampled_at: Instant, interval: Duration, force: bool) -> bool {
    !force && sampled_at.elapsed() < interval
}

/// Live transports + the real aggregator (outlier guard, 30-minute stale
/// retention, ignore/alias policy and the on-disk JSON cache). Enforces
/// `listen_only`, `wireless_multi_brand_enabled()`, and `wireless_disabled_vendors()`
/// BEFORE opening/writing any HID device, then merges cleanly through
/// [`WirelessBatteryAggregator`].
pub fn read_all_with_settings(settings: &DockSettings) -> Vec<WirelessReading> {
    read_all_with_settings_forced(settings, false)
}

/// Same as [`read_all_with_settings`]; `force` (manual refresh) bypasses the reuse of the
/// last aggregate inside the poll interval so the user always gets a fresh poll.
pub fn read_all_with_settings_forced(settings: &DockSettings, force: bool) -> Vec<WirelessReading> {
    read_all_with_settings_forced_outcome(settings, force).readings
}

/// Full outcome of [`read_all_with_settings_forced`] so callers know whether native
/// Bluetooth providers already observed devices before ignore-filtering.
pub(crate) fn read_all_with_settings_forced_outcome(
    settings: &DockSettings,
    force: bool,
) -> WirelessPollOutcome {
    let agg_settings = AggregatorSettings::from(settings);
    let interval = Duration::from_secs(agg_settings.poll_seconds_clamped() as u64);

    if let Ok(last) = LAST_AGGREGATE.lock()
        && let Some((sampled_at, readings, had_native_bluetooth)) = last.as_ref()
        && aggregate_is_reusable(*sampled_at, interval, force)
    {
        return WirelessPollOutcome {
            readings: readings.clone(),
            had_native_bluetooth: *had_native_bluetooth,
        };
    }

    let gate = AGG_GATE.get_or_init(|| Mutex::new(()));
    let Ok(_guard) = gate.try_lock() else {
        return cached_aggregate_outcome().unwrap_or(WirelessPollOutcome {
            readings: Vec::new(),
            had_native_bluetooth: false,
        });
    };
    // Another poll may have completed while this caller waited for the gate.
    if let Ok(last) = LAST_AGGREGATE.lock()
        && let Some((sampled_at, readings, had_native_bluetooth)) = last.as_ref()
        && aggregate_is_reusable(*sampled_at, interval, force)
    {
        return WirelessPollOutcome {
            readings: readings.clone(),
            had_native_bluetooth: *had_native_bluetooth,
        };
    }

    let policy = ProbePolicy::from(&agg_settings);
    let Some(raw_fresh) = read_all_raw(&policy) else {
        return cached_aggregate_outcome().unwrap_or_else(|| WirelessPollOutcome {
            readings: cached_snapshot(),
            had_native_bluetooth: false,
        });
    };
    let had_native_bluetooth = raw_fresh.iter().any(|r| {
        !is_ignored_wired_reading(r)
            && r.level_percent_opt().is_some()
            && matches!(
                r.source,
                BatterySource::Bluetooth | BatterySource::BluetoothGatt
            )
    });
    let fresh: Vec<BatteryReading> = raw_fresh
        .iter()
        .filter(|r| !is_ignored_wired_reading(r))
        .map(to_battery_reading)
        .collect();
    let mut aggregator = WirelessBatteryAggregator::new(agg_settings);
    // The aggregator object is rebuilt per poll; the guard is not. Carry the
    // pending-drop state in and write it back so consecutive drops are detected.
    aggregator.guard = persistent_guard().clone();
    let merged = aggregator
        .merge(fresh)
        .into_iter()
        .map(to_wireless_reading)
        .collect::<Vec<_>>();
    *persistent_guard() = aggregator.guard.clone();

    if let Ok(mut last) = LAST_AGGREGATE.lock() {
        *last = Some((Instant::now(), merged.clone(), had_native_bluetooth));
    }
    WirelessPollOutcome {
        readings: merged,
        had_native_bluetooth,
    }
}

/// A previous live sample younger than this is a valid reference for the drop guard; an
/// older one (device was off / disconnected / battery swapped since) is not.
const DROP_GUARD_REFERENCE_SECS: u64 = 10 * 60;

pub struct WirelessBatteryAggregator {
    pub settings: AggregatorSettings,
    pub cache: HashMap<String, BatteryReading>,
    pub guard: OutlierGuard,
    cache_path: Option<PathBuf>,
    loaded: bool,
}

impl WirelessBatteryAggregator {
    pub fn new(settings: AggregatorSettings) -> Self {
        // Same file name and folder as `WirelessBatteryAggregator`.
        let cache_path = Some(
            opentaskbar_core::paths::shared_app_data_dir().join("wireless-battery-cache.json"),
        );
        Self {
            settings,
            cache: HashMap::new(),
            guard: OutlierGuard::default(),
            cache_path,
            loaded: false,
        }
    }

    pub fn with_cache_path(mut self, path: PathBuf) -> Self {
        self.cache_path = Some(path);
        self
    }

    /// reference writes JSON with `Encoding.UTF8`, which emits a BOM. other readers
    /// strip it, so the Rust reader has to tolerate both forms.
    fn cache_json(raw: &str) -> Option<serde_json::Value> {
        serde_json::from_str(raw.strip_prefix('\u{feff}').unwrap_or(raw)).ok()
    }

    fn ensure_loaded(&mut self, now: Instant) {
        self.ensure_loaded_at(now, wall_clock_secs());
    }

    /// Loads the disk cache. `SampledAt` is the single authoritative sample time; the file's
    /// write time is never substituted for it (that would renew a disconnected device's age
    /// on every reload). Only the legacy snake_case format without `SampledAt` derives the
    /// sample time from `written_at - age_secs`.
    fn ensure_loaded_at(&mut self, now: Instant, wall_now: u64) {
        if self.loaded {
            return;
        }
        self.loaded = true;
        let Some(path) = &self.cache_path else {
            return;
        };
        let Ok(meta) = std::fs::metadata(path) else {
            return;
        };
        if meta.len() == 0 || meta.len() > MAX_CACHE_BYTES as u64 {
            return;
        }
        let Ok(json) = std::fs::read_to_string(path) else {
            return;
        };
        let Some(root) = Self::cache_json(&json) else {
            return;
        };
        let Some(map) = root.as_object() else {
            return;
        };
        for (key, value) in map {
            if self.cache.len() >= 64 {
                break;
            }
            let level = cache_field(value, "level", "BatteryLevel")
                .and_then(|x| x.as_i64())
                .unwrap_or(-1);
            if !(-1..=100).contains(&level) {
                continue;
            }
            let level = level as i32;
            let source_id = cache_field(value, "source", "Source")
                .and_then(|x| x.as_u64())
                .unwrap_or(0) as u8;
            let sampled = value
                .get("SampledAt")
                .and_then(|x| x.as_str())
                .and_then(reference_timestamp_secs)
                .or_else(|| {
                    let written = cache_field(value, "written_at", "WrittenAt")?.as_u64()?;
                    let age = cache_field(value, "age_secs", "AgeSecs")
                        .and_then(|x| x.as_u64())
                        .unwrap_or(0);
                    Some(written.saturating_sub(age))
                });
            let Some(sampled) = sampled else {
                continue;
            };
            // A clock moving backwards must never produce a future-dated entry.
            let sampled = sampled.min(wall_now);
            let age_secs = wall_now.saturating_sub(sampled);
            if Duration::from_secs(age_secs) > STALE_WINDOW {
                // Expired on disk: remember it so a stale re-report cannot restart its age.
                let id = cache_field(value, "id", "Id")
                    .and_then(|x| x.as_str())
                    .unwrap_or(key);
                let mut tombstones = lock_tombstones();
                if tombstones.len() < 256 {
                    tombstones.insert(physical_key(id));
                }
                continue;
            }
            let mut reading = BatteryReading::new(
                cache_field(value, "id", "Id")
                    .and_then(|x| x.as_str())
                    .unwrap_or(key),
                cache_field(value, "name", "Name")
                    .and_then(|x| x.as_str())
                    .unwrap_or(""),
                level.max(0) as u32,
                cache_field(value, "charging", "IsCharging")
                    .and_then(|x| x.as_bool())
                    .unwrap_or(false),
                source_from_id(source_id),
            );
            if level < 0 {
                // A cached -1 means "level unknown"; promote it to an explicit
                // override instead of presenting a false 0% reading to the UI.
                reading.level = -1;
                let override_text = cache_field(value, "level_override", "LevelOverrideText")
                    .and_then(|x| x.as_str())
                    .filter(|s| !s.trim().is_empty())
                    .unwrap_or("-1");
                reading.level_override = Some(override_text.to_owned());
            }
            if let Some(spare) = cache_field(value, "secondary_level", "SecondaryBatteryLevel")
                .and_then(|x| x.as_i64())
                .filter(|&v| (0..=100).contains(&v))
            {
                reading.secondary_level = Some(spare as i32);
            }
            // `Instant - Duration` panics before boot + age; fall back to `now` (the Unix
            // sample time below stays authoritative for expiry either way).
            reading.sampled_at = now
                .checked_sub(Duration::from_secs(age_secs))
                .unwrap_or(now);
            reading.sampled_unix = Some(sampled);
            reading.connected = value
                .get("connected")
                .and_then(|v| v.as_bool())
                .unwrap_or(false);
            reading.stale = value.get("stale").and_then(|v| v.as_bool()).unwrap_or(true);
            self.cache.insert(physical_key(&reading.id), reading);
        }
    }

    fn save(&self, now: Instant) {
        self.save_at(now, wall_clock_secs());
    }

    fn save_at(&self, now: Instant, wall_now: u64) {
        let Some(path) = &self.cache_path else {
            return;
        };
        if let Some(dir) = path.parent() {
            let _ = std::fs::create_dir_all(dir);
        }
        let mut root = serde_json::Map::new();
        for (key, r) in &self.cache {
            let sampled = r
                .sampled_unix
                .unwrap_or_else(|| wall_now.saturating_sub(r.age_secs(now, wall_now)));
            // Both spellings are written: snake_case for this port's existing
            // readers, PascalCase for `WirelessDeviceReading` deserialization.
            root.insert(
                key.clone(),
                serde_json::json!({
                    "id": r.id,
                    "name": r.name,
                    "level": r.level,
                    "charging": r.charging,
                    "source": source_to_id(r.source),
                    "connected": r.connected,
                    "stale": r.stale,
                    "secondary_level": r.secondary_level,
                    "Id": r.id,
                    "Name": r.name,
                    "BatteryLevel": r.level,
                    "IsCharging": r.charging,
                    "Source": source_to_id(r.source),
                    "SampledAt": unix_secs_to_rfc3339(sampled),
                    "LevelOverrideText": r.level_override,
                    "SecondaryBatteryLevel": r.secondary_level,
                }),
            );
        }
        let json = serde_json::to_string(&serde_json::Value::Object(root)).unwrap_or_default();
        // Write to a sibling temporary file then rename: readers never observe a
        // half-written JSON document when the dock restarts mid-save.
        let temp = path.with_extension("json.tmp");
        // `File.WriteAllText(..., Encoding.UTF8)` emits a UTF-8 BOM.
        let with_bom = format!("\u{feff}{json}");
        if std::fs::write(&temp, with_bom.as_bytes()).is_err() {
            let _ = std::fs::remove_file(&temp);
            return;
        }
        if std::fs::rename(&temp, path).is_err() {
            let _ = std::fs::remove_file(&temp);
        }
    }

    /// Merges fresh readings with the cache: outlier protection, stale retention (30 min),
    /// alias/ignore settings and disk persistence.
    pub fn merge(&mut self, fresh: Vec<BatteryReading>) -> Vec<BatteryReading> {
        self.merge_at(fresh, Instant::now(), wall_clock_secs())
    }

    fn merge_at(
        &mut self,
        fresh: Vec<BatteryReading>,
        now: Instant,
        wall_now: u64,
    ) -> Vec<BatteryReading> {
        self.ensure_loaded_at(now, wall_now);
        let policy = ProbePolicy::from(&self.settings);
        let settings = self.settings.clone();
        // Current ignore / vendor / source policy applies to retained entries too, and
        // every entry expires 30 minutes after its one authoritative sample time.
        let mut expired = Vec::new();
        self.cache.retain(|key, r| {
            if !settings.allows_battery_reading(&policy, r) {
                return false;
            }
            if Duration::from_secs(r.age_secs(now, wall_now)) > STALE_WINDOW {
                expired.push(key.clone());
                return false;
            }
            true
        });
        if !expired.is_empty() {
            let mut tombstones = lock_tombstones();
            for key in expired {
                if tombstones.len() < 256 {
                    tombstones.insert(key);
                }
            }
        }
        let fresh: Vec<BatteryReading> = fresh
            .into_iter()
            .filter(|r| settings.allows_battery_reading(&policy, r))
            .map(|mut r| {
                if r.sampled_unix.is_none() {
                    r.sampled_unix = Some(
                        wall_now
                            .saturating_sub(now.saturating_duration_since(r.sampled_at).as_secs()),
                    );
                }
                r
            })
            .collect();
        let mut active = HashSet::new();
        for mut r in deduplicate(fresh) {
            let key = physical_key(&r.id);
            if Duration::from_secs(r.age_secs(now, wall_now)) > STALE_WINDOW {
                continue;
            }
            if !r.is_live() {
                // Last-known provenance (registry snapshot, unverified link): it may
                // seed an entry once, but never overwrites or renews a retained one and
                // never resurrects an expired one.
                if self.cache.contains_key(&key) || lock_tombstones().contains(&key) {
                    continue;
                }
                r.stale = true;
                r.connected = false;
                r.charging = false;
                self.guard.pending.remove(&key);
                self.cache.insert(key, r);
                continue;
            }
            lock_tombstones().remove(&key);
            // Large-drop guard: only for coarse / stack-cached sources, only against a
            // recent live sample of the same source. Verified live GATT / vendor HID drops
            // and post-disconnect or battery-swap transitions are taken as-is.
            if r.source.is_uncertain() && r.level >= 0 {
                let reference = self.cache.get(&key).filter(|p| {
                    p.is_live()
                        && p.source == r.source
                        && p.level >= 0
                        && p.age_secs(now, wall_now) <= DROP_GUARD_REFERENCE_SECS
                });
                if let Some(prev) = reference {
                    if self.guard.check(&key, prev.level, r.level).1 {
                        // Held: the previous sample (and its original sample time) stays.
                        active.insert(key);
                        continue;
                    }
                } else {
                    self.guard.pending.remove(&key);
                }
            } else {
                self.guard.pending.remove(&key);
            }
            active.insert(key.clone());
            self.cache.insert(key, r);
        }
        self.cache
            .retain(|key, r| r.source != BatterySource::XInput || active.contains(key));
        self.guard.pending.retain(|key, _| active.contains(key));
        let mut output = Vec::new();
        for (key, r) in &mut self.cache {
            if !active.contains(key) {
                r.stale = true;
                r.connected = false;
                r.charging = false;
            }
            let mut row = r.clone();
            row.name = self.settings.display_name(r);
            output.push(row);
        }
        // Deterministic source-priority ordering: freshest, charging devices
        // from the most trustworthy transports first.
        output.sort_by(|a, b| {
            a.stale
                .cmp(&b.stale)
                .then_with(|| b.charging.cmp(&a.charging))
                .then_with(|| a.source.priority().cmp(&b.source.priority()))
                .then_with(|| a.level.cmp(&b.level))
                .then_with(|| {
                    a.name
                        .to_ascii_lowercase()
                        .cmp(&b.name.to_ascii_lowercase())
                })
                .then_with(|| a.id.cmp(&b.id))
        });
        self.save_at(now, wall_now);
        output
    }
}

fn source_to_id(source: BatterySource) -> u8 {
    match source {
        BatterySource::Bluetooth => 0,
        BatterySource::XInput => 1,
        BatterySource::SteelSeriesHid => 2,
        BatterySource::SteelSeriesGg => 3,
        BatterySource::LogitechHidPlusPlus => 4,
        BatterySource::LogitechGHub => 5,
        BatterySource::CompxAtkVxe => 6,
        BatterySource::RazerHid => 7,
        BatterySource::BluetoothGatt => 8,
        BatterySource::StandardHid => 9,
        BatterySource::HyperXHid => 10,
        BatterySource::CorsairHid => 11,
        BatterySource::AsusRogHid => 12,
        BatterySource::GamepadHid => 13,
        BatterySource::Keyboard24gHid => 14,
    }
}

fn source_from_id(id: u8) -> BatterySource {
    match id {
        0 => BatterySource::Bluetooth,
        1 => BatterySource::XInput,
        2 => BatterySource::SteelSeriesHid,
        3 => BatterySource::SteelSeriesGg,
        4 => BatterySource::LogitechHidPlusPlus,
        5 => BatterySource::LogitechGHub,
        6 => BatterySource::CompxAtkVxe,
        7 => BatterySource::RazerHid,
        8 => BatterySource::BluetoothGatt,
        9 => BatterySource::StandardHid,
        10 => BatterySource::HyperXHid,
        11 => BatterySource::CorsairHid,
        12 => BatterySource::AsusRogHid,
        13 => BatterySource::GamepadHid,
        14 => BatterySource::Keyboard24gHid,
        _ => BatterySource::Bluetooth,
    }
}

/// Routes one raw HID report to the right vendor parser based on its descriptor.
pub fn probe_report(descriptor: &HidInterfaceDescriptor, report: &[u8]) -> Option<BatteryReading> {
    let id = format!(
        "hid:{:04X}:{:04X}",
        descriptor.vendor_id, descriptor.product_id
    );
    let name = descriptor.product_name.as_str();
    if is_asus_interface(descriptor) {
        return parse_asus_rog(report, &id, name);
    }
    if is_corsair_interface(descriptor) {
        if descriptor.usage_page == BRAGI_USAGE_PAGE {
            let level = parse_corsair_bragi_level(report)?;
            let charging = parse_corsair_bragi_charging(report).unwrap_or(false);
            let mut r = BatteryReading::new(&id, name, level, charging, BatterySource::CorsairHid);
            r.level = level as i32;
            return Some(r);
        }
        return parse_corsair_legacy(report, &id, name);
    }
    if let Some(family) = hyperx_family(descriptor.usage_page, descriptor.vendor_id) {
        return parse_hyperx(family, report, &id, name);
    }
    if is_compx_vendor(descriptor.vendor_id) {
        return parse_compx_nordic(report, &id, name).or_else(|| parse_compx17(report, &id, name));
    }
    if is_keyboard_24g(descriptor.vendor_id, descriptor.product_id) {
        return parse_keyboard_24g(report, descriptor.input_report_length as usize, &id, name);
    }
    if matches!(descriptor.vendor_id, 0x054C | 0x057E) {
        return parse_gamepad(
            descriptor.vendor_id,
            descriptor.product_id,
            report,
            &id,
            name,
        );
    }
    if descriptor.vendor_id == 0x1038 {
        return parse_steelseries_hid(report, &id, name);
    }
    None
}

#[cfg(test)]
mod ws_g_tests {
    use super::*;

    #[test]
    fn ws4_loopback_endpoints_are_strictly_local() {
        assert_eq!(
            parse_loopback_endpoint("ws://127.0.0.1:9010", 9010, true).as_deref(),
            Some("127.0.0.1:9010")
        );
        assert_eq!(
            parse_loopback_endpoint("http://localhost:57342", 57342, false).as_deref(),
            Some("localhost:57342")
        );
        assert!(parse_loopback_endpoint("ws://evil.example.com:9010", 9010, true).is_none());
        assert!(parse_loopback_endpoint("http://127.0.0.1:9011", 9010, false).is_none());
        assert_eq!(url_port("http://127.0.0.1:57342/battery"), Some(57342));
    }

    #[test]
    fn ws4_ghub_and_steelseries_parsers_feed_aggregator_shape() {
        let ghub = r#"{"devices":[{"name":"G Pro","id":"mouse-1","battery":75,"charging":true}]}"#;
        let parsed = parse_ghub_json(ghub);
        assert_eq!(parsed.len(), 1);
        assert_eq!(parsed[0].level, 75);
        let wireless = reading(
            format!("ghub:{}", parsed[0].id),
            parsed[0].name.clone(),
            percent_01(parsed[0].level),
            parsed[0].connected,
            parsed[0].charging,
            parsed[0].source,
        );
        assert_eq!(wireless.level_percent(), 75);
        assert_eq!(wireless.source, BatterySource::LogitechGHub);

        let gg = r#"{"batteryLevel":42,"deviceName":"Arctis"}"#;
        let parsed = parse_steelseries_gg_json(gg);
        assert_eq!(parsed.len(), 1);
        assert_eq!(parsed[0].level, 42);
    }

    #[test]
    fn ws4_aggregator_converts_live_readings_and_honours_poll_interval() {
        let dock = opentaskbar_core::settings::DockSettings {
            wireless_battery_poll_seconds: 15,
            ..Default::default()
        };
        let agg = AggregatorSettings::from(&dock);
        let live = reading(
            "bt-pnp:1".into(),
            "Headset".into(),
            0.8,
            true,
            false,
            BatterySource::Bluetooth,
        );
        let battery = to_battery_reading(&live);
        assert_eq!(battery.level, 80);
        let restored = to_wireless_reading(battery);
        assert_eq!(restored.level_percent(), 80);
        assert_eq!(restored.source, BatterySource::Bluetooth);
        assert_eq!(agg.poll_seconds_clamped(), 15);
        // The cache gate is keyed by the setting; no hardware is needed here.
        assert!(cached_aggregate().is_none_or(|items| items.len() <= 64));
    }

    fn desc(vid: u16, pid: u16, page: u16, len: u16) -> HidInterfaceDescriptor {
        HidInterfaceDescriptor {
            vendor_id: vid,
            product_id: pid,
            usage_page: page,
            usage: 0,
            input_report_length: len,
            output_report_length: len,
            feature_report_length: len,
            product_name: "Test Device".into(),
            path: String::new(),
        }
    }

    #[test]
    fn asus_rog_vendor() {
        let mut report = vec![0u8; 64];
        report[0] = 0x12;
        report[1] = 0x07;
        report[4] = 68;
        report[5] = 1;
        let r = parse_asus_rog(&report, "asus", "ROG Harpe").unwrap();
        assert_eq!(r.level, 68);
        assert!(r.charging);
        assert_eq!(r.source, BatterySource::AsusRogHid);
        assert!(is_asus_interface(&desc(0x0B05, 0x1234, 0xFF00, 64)));
        let req = build_asus_request(65);
        assert_eq!(&req[1..3], &[0x12, 0x07]);
    }

    #[test]
    fn corsair_bragi_and_legacy() {
        let mut level = vec![0u8; 64];
        level[0] = 0x09;
        level[1] = 0x02;
        level[2] = 0;
        level[3] = 0xE8;
        level[4] = 0x03; // 1000 permille = 100%
        assert_eq!(parse_corsair_bragi_level(&level), Some(100));
        let mut status = level.clone();
        status[3] = 1;
        status[4] = 0;
        assert_eq!(parse_corsair_bragi_charging(&status), Some(true));
        status[3] = 2;
        assert_eq!(parse_corsair_bragi_charging(&status), Some(false));
        let mut legacy = vec![0u8; 64];
        legacy[0] = 0xC9;
        legacy[1] = 0x64;
        legacy[2] = 0x80 | 78; // bit 7 = mic boom flag, not battery
        legacy[4] = 0x01;
        let r = parse_corsair_legacy(&legacy, "corsair", "Void Elite").unwrap();
        assert_eq!(r.level, 78);
        assert!(!r.charging);
        assert!(is_corsair_interface(&desc(0x1B1C, 1, 0xFF42, 64)));
    }

    #[test]
    fn hyperx_four_families() {
        for (page, _prefix) in [
            (0xFF00, 0x03),
            (0xFF13, 0x05),
            (0xFF90, 0x06),
            (0xFFC0, 0xC0),
        ] {
            let family = hyperx_family(page, 0x03F0).unwrap();
            let mut report = build_hyperx_request(family, 64);
            report[3] = 77;
            let parsed = parse_hyperx(family, &report, "hyperx", "Cloud").unwrap();
            assert_eq!(parsed.level, 77);
            assert!(hyperx_family(page, 0x0951).is_some());
        }
        assert!(hyperx_family(0xFF00, 0x0951).is_some());
        assert!(hyperx_family(0xFF00, 0x1234).is_none());
    }

    #[test]
    fn compx_checksum_and_reports() {
        let request = build_compx_request();
        assert_eq!(compx_checksum(&request), 0x49);
        let mut reply = request;
        reply[6] = 55;
        reply[7] = 1;
        reply[8] = 0x0E;
        reply[9] = 0x74; // 3700 mV
        reply[16] = compx_checksum(&reply);
        assert!(parse_compx17(&reply, "atk", "ATK A9").is_some());
        let mut corrupt = build_compx_request();
        corrupt[6] = 42;
        corrupt[16] = 0; // wrong checksum -> reject (full 17-byte report)
        assert!(parse_compx17(&corrupt, "atk", "mouse").is_none());
        let mut atk9 = [0u8; 9];
        atk9[..6].copy_from_slice(&[0x82, 0x08, 0x02, 0, 0xFF, 0x03]);
        atk9[8] = 0x80 | 42;
        assert_eq!(parse_atk1k(&atk9, "atk1k", "ATK 1K").unwrap().level, 42);
        assert!(is_compx_vendor(0x3434) && is_compx_vendor(0x373B));
    }

    #[test]
    fn keyboard_24g_protocol() {
        assert!(is_keyboard_24g(0x258A, 0x0001));
        assert!(is_keyboard_24g(0x3554, 0xFA09));
        let mut report = build_compx_request();
        report[6] = 60;
        report[16] = compx_checksum(&report);
        let r = parse_keyboard_24g(&report, 17, "kb", "AULA").unwrap();
        assert_eq!(r.source, BatterySource::Keyboard24gHid);
        assert_eq!(r.level, 60);
    }

    #[test]
    fn gamepad_vendor_reports() {
        // USB report 0x01 is exactly 64 bytes; sticks rest at 0x80.
        let mut ds = vec![0u8; 64];
        ds[0] = 0x01;
        ds[1..5].fill(0x80);
        ds[53] = 0x05; // step 5 -> midpoint 55%, discharging
        assert_eq!(parse_dualsense(&ds, "ds", "DualSense").unwrap().level, 55);
        ds[53] = 0x1A; // charging (0x1), step 10 -> min(105, 100)
        let charged = parse_dualsense(&ds, "ds", "DualSense").unwrap();
        assert_eq!(charged.level, 100);
        assert!(charged.charging);
        ds[53] = 0x23; // full state overrides the step
        let full = parse_dualsense(&ds, "ds", "DualSense").unwrap();
        assert_eq!(full.level, 100);
        assert!(!full.charging);
        ds[53] = 0xA0; // voltage error state
        assert!(parse_dualsense(&ds, "ds", "DualSense").is_none());
        // A zero-padded buffer (BT simple 0x01 report) is not "5%" / "0%".
        let mut padded = vec![0u8; 64];
        padded[0] = 0x01;
        assert!(parse_dualsense(&padded, "ds", "DualSense").is_none());
        // BT simple report read into a 78-byte buffer is not USB framing.
        let mut bt_simple = vec![0u8; 78];
        bt_simple[0] = 0x01;
        bt_simple[1] = 0x80;
        assert!(parse_dualsense(&bt_simple, "ds", "DualSense").is_none());

        let mut ds4 = vec![0u8; 64];
        ds4[0] = 0x01;
        ds4[1..5].fill(0x80);
        ds4[30] = 0x1A; // cable + step 10 -> 100% charging
        let r = parse_dualshock4(&ds4, "ds4", "DualShock 4").unwrap();
        assert_eq!(r.level, 100);
        assert!(r.charging);
        ds4[30] = 0x1B; // cable + full
        let r = parse_dualshock4(&ds4, "ds4", "DualShock 4").unwrap();
        assert_eq!(r.level, 100);
        assert!(!r.charging);
        ds4[30] = 0x03; // battery, step 3 -> 35%
        assert_eq!(parse_dualshock4(&ds4, "ds4", "DS4").unwrap().level, 35);

        let mut sw = vec![0u8; 13];
        sw[0] = 0x30;
        sw[1] = 0x81;
        sw[2] = 0x70; // charging (0x10) + state 3 (medium)
        let r = parse_switch_pro(&sw, "sw", "Switch Pro").unwrap();
        assert_eq!(r.level, 60);
        assert!(r.charging);
        sw[2] = 0x20; // state 1 = critical
        assert_eq!(parse_switch_pro(&sw, "sw", "Switch Pro").unwrap().level, 10);
        sw[2] = 0x00; // state 0 = empty, preserved as 0%
        assert_eq!(parse_switch_pro(&sw, "sw", "Switch Pro").unwrap().level, 0);
        sw[2] = 0xA0; // state 5 = reserved
        assert!(parse_switch_pro(&sw, "sw", "Switch Pro").is_none());
        ds[53] = 0x05;
        assert!(parse_gamepad(0x054C, 0x0CE6, &ds, "x", "x").is_some());
    }

    #[test]
    fn logitech_ghub_loopback_json() {
        assert!(is_valid_ghub_uri("http://127.0.0.1:9010"));
        assert!(!is_valid_ghub_uri("https://evil.example.com"));
        let json =
            r#"{"devices":[{"name":"G Pro X","batteryLevel":73,"charging":false,"id":"1"}]}"#;
        let readings = parse_ghub_json(json);
        assert_eq!(readings.len(), 1);
        assert_eq!(readings[0].level, 73);
        assert_eq!(readings[0].source, BatterySource::LogitechGHub);
        let mut hidpp = [0u8; 7];
        hidpp[0] = 0x10;
        hidpp[2] = 60;
        hidpp[3] = 1;
        assert_eq!(parse_hidpp_battery_status(&hidpp), Some((60, true)));
    }

    #[test]
    fn steelseries_coreprops_and_gg() {
        let json = r#"{"gcEndpoint":"http://127.0.0.1:8256/","other":1}"#;
        assert_eq!(
            parse_coreprops_endpoint(json).as_deref(),
            Some("http://127.0.0.1:8256/")
        );
        assert!(parse_coreprops_endpoint(r#"{"gcEndpoint":"https://evil.com"}"#).is_none());
        let gg = r#"{"headsets":[{"deviceName":"Arctis 7","battery":88,"charging":true}]}"#;
        let readings = parse_steelseries_gg_json(gg);
        assert_eq!(readings.len(), 1);
        assert_eq!(readings[0].level, 88);
        let mut hid = [0u8; 4];
        hid[1] = 0x02;
        hid[2] = 40;
        assert_eq!(parse_steelseries_hid(&hid, "ss", "Nova").unwrap().level, 40);
    }

    #[test]
    fn ble_gatt_battery() {
        assert!(is_ble_aep_id("BTHENUM#{180F}_aep_1234"));
        // GATT BatteryLevel is a valid 0..100 byte; 0% is a genuine reading.
        assert_eq!(parse_ble_gatt(&[0], "ble", "Device").unwrap().level, 0);
        assert!(parse_ble_gatt(&[101], "ble", "Device").is_none());
        let r = parse_ble_gatt(&[92], "ble", "Device").unwrap();
        assert_eq!(r.level, 92);
        assert_eq!(r.source, BatterySource::BluetoothGatt);
    }

    #[test]
    fn aggregator_outlier_guard() {
        let mut guard = OutlierGuard::default();
        let (level, guarded) = guard.check("dev", 90, 60);
        assert_eq!(level, 90);
        assert!(guarded);
        let (level, guarded) = guard.check("dev", 90, 55);
        assert_eq!(level, 55);
        assert!(!guarded);
        let (level, guarded) = guard.check("dev", 90, 89);
        assert_eq!(level, 89);
        assert!(!guarded);
    }

    #[test]
    fn outlier_guard_survives_a_fresh_aggregator() {
        let key = "ws4-persistent-guard";
        persistent_guard().pending.remove(key);
        let (level, guarded) = persistent_guard().check(key, 90, 60);
        assert_eq!(level, 90);
        assert!(guarded, "first implausible drop must be held");

        // A new aggregator (as built on every poll) must inherit the doubt.
        let mut aggregator = WirelessBatteryAggregator::new(AggregatorSettings::default());
        aggregator.guard = persistent_guard().clone();
        let (level, guarded) = aggregator.guard.check(key, 90, 55);
        assert_eq!(level, 55);
        assert!(!guarded, "second consecutive drop must be accepted");
        *persistent_guard() = aggregator.guard;
        persistent_guard().pending.remove(key);
    }

    #[test]
    fn disk_cache_handles_future_skew_and_minus_one_override() {
        let dir = std::env::temp_dir().join(format!("otb-wb-cache-{}", std::process::id()));
        let _ = std::fs::create_dir_all(&dir);
        let path = dir.join("cache.json");
        let wall_now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs();
        let document = serde_json::json!({
            "unknown": {
                "id": "unknown", "name": "Unknown", "level": -1,
                "charging": false, "source": 0, "age_secs": 0,
                "written_at": wall_now + 600
            },
            "fresh": {
                "id": "fresh", "name": "Headset", "level": 80,
                "charging": true, "source": 2, "age_secs": 0,
                "written_at": wall_now
            }
        });
        std::fs::write(&path, document.to_string()).unwrap();
        let mut aggregator = WirelessBatteryAggregator::new(AggregatorSettings::default())
            .with_cache_path(path.clone());
        aggregator.ensure_loaded(Instant::now());
        let unknown = &aggregator.cache["unknown"];
        assert_eq!(unknown.level, -1);
        assert_eq!(unknown.level_override.as_deref(), Some("-1"));
        assert!(unknown.stale);
        assert_eq!(aggregator.cache["fresh"].level, 80);

        aggregator.save(Instant::now());
        assert!(path.is_file());
        assert!(!path.with_extension("json.tmp").exists());
        let saved = std::fs::read_to_string(&path).unwrap();
        assert!(saved.contains("\"SampledAt\""));
        let _ = std::fs::remove_file(&path);
        let _ = std::fs::remove_dir(&dir);
    }

    #[test]
    fn aggregator_settings_privacy_and_merge() {
        let mut settings = AggregatorSettings {
            poll_seconds: 999,
            ..Default::default()
        };
        settings.aliases.insert("id1".into(), "My Mouse".into());
        settings.ignored_devices.push("hidden".into());
        assert_eq!(settings.poll_seconds_clamped(), 300);
        let mut hidden =
            BatteryReading::new("hidden", "Hidden", 50, false, BatterySource::Bluetooth);
        let mut good = BatteryReading::new("id1", "Mouse", 80, false, BatterySource::AsusRogHid);
        assert!(settings.is_ignored(&hidden));
        assert_eq!(settings.display_name(&good), "My Mouse");
        hidden.name = "Hidden".into();
        let desc = [
            desc(0x2E3C, 0xC368, 0xFF00, 64),
            desc(0x0B05, 1, 0xFF00, 64),
        ];
        let summary = privacy_diagnostic_summary(&desc);
        assert!(!summary.contains("2E3C"));
        assert!(summary.contains("0B05"));
        let cache_path = std::env::temp_dir().join("otb-wireless-battery-test-cache.json");
        let _ = std::fs::remove_file(&cache_path);
        let mut agg = WirelessBatteryAggregator::new(settings).with_cache_path(cache_path.clone());
        good.name = "My Mouse".into();
        let result = agg.merge(vec![hidden, good]);
        assert_eq!(result.len(), 1);
        assert_eq!(result[0].name, "My Mouse");
        let _ = std::fs::remove_file(cache_path);
    }

    #[test]
    fn aggregator_settings_from_dock_settings() {
        let mut dock = opentaskbar_core::settings::DockSettings {
            wireless_battery_poll_seconds: 45,
            wireless_battery_low_threshold: 30,
            ..Default::default()
        };
        dock.ignored_wireless_devices.push("hidden".into());
        dock.wireless_device_aliases
            .insert("ID1".into(), "My Headset".into());
        dock.wireless_battery_listen_only = true;

        let agg = super::AggregatorSettings::from(&dock);
        assert_eq!(agg.poll_seconds, 45);
        assert_eq!(agg.poll_seconds_clamped(), 45);
        assert_eq!(agg.low_threshold, 30);
        assert!(agg.listen_only);
        let active = super::WirelessReading {
            id: "id1".into(),
            name: "Headset".into(),
            level_01: 0.5,
            sampled_at: Instant::now(),
            is_connected: true,
            is_charging: false,
            source: BatterySource::AsusRogHid,
            stale: false,
            device_kind: WirelessDeviceKind::Headphones,
            secondary_level: None,
        };
        assert!(!agg.allows_reading(&active)); // listen-only excludes active HID probes
        assert_eq!(agg.display_name_of(&active), "My Headset");
        let hidden = super::WirelessReading {
            name: "Hidden Device".into(),
            ..active.clone()
        };
        assert!(!agg.allows_reading(&hidden));
        let passive = super::WirelessReading {
            source: BatterySource::Bluetooth,
            ..active
        };
        assert!(agg.allows_reading(&passive));
    }

    #[test]
    fn keyboard_24g_sinowealth_0x83_and_checksum_validation() {
        // Sinowealth 64-byte response: [0x83, charging=1, level=77]
        let mut sino = vec![0u8; 64];
        sino[0] = 0x83;
        sino[1] = 1;
        sino[2] = 77;
        let r = parse_keyboard_24g(&sino, 64, "kbd:aula", "AULA F75").expect("Sinowealth 0x83");
        assert_eq!(r.level, 77);
        assert!(r.charging);
        assert_eq!(r.source, BatterySource::Keyboard24gHid);
        assert_eq!(r.device_kind, WirelessDeviceKind::Keyboard);

        // Leading 0x00 framed Sinowealth 0x83
        let mut framed = vec![0u8; 65];
        framed[1] = 0x83;
        framed[2] = 0;
        framed[3] = 52;
        let r2 = parse_keyboard_24g(&framed, 64, "kbd:rk", "Royal Kludge")
            .expect("framed Sinowealth 0x83");
        assert_eq!(r2.level, 52);
        assert!(!r2.charging);

        // Invalid Sinowealth (0% or >100% or invalid charging flag)
        sino[2] = 0;
        assert!(parse_keyboard_24g(&sino, 64, "kbd:bad", "AULA").is_none());
        sino[2] = 101;
        assert!(parse_keyboard_24g(&sino, 64, "kbd:bad", "AULA").is_none());
        sino[1] = 2;
        sino[2] = 50;
        assert!(parse_keyboard_24g(&sino, 64, "kbd:bad", "AULA").is_none());

        // Glorious mouse PIDs on 0x258A must NOT be classified as 2.4G keyboards
        assert!(!is_keyboard_24g(0x258A, 0x2016));
        assert!(is_keyboard_24g(0x258A, 0x010C));
    }

    #[test]
    fn steelseries_expanded_pids_and_spare_dock_battery_roundtrip() {
        assert_eq!(ss_protocol_for_pid(0x12E2), Some(SsProtocol::NovaPro));
        assert_eq!(ss_protocol_for_pid(0x12E6), Some(SsProtocol::NovaPro));
        assert_eq!(ss_protocol_for_pid(0x2240), Some(SsProtocol::Nova5));
        assert_eq!(ss_protocol_for_pid(0x162C), Some(SsProtocol::Keyboard));
        assert_eq!(ss_protocol_for_pid(0x1854), Some(SsProtocol::Mouse));
        assert_eq!(
            ss_device_kind(SsProtocol::Keyboard),
            WirelessDeviceKind::Keyboard
        );
        assert_eq!(
            ss_device_kind(SsProtocol::NovaPro),
            WirelessDeviceKind::Headphones
        );

        let live = reading_with_meta(
            "steelseries:1038:12E0:ABC".into(),
            "Arctis Nova Pro Wireless".into(),
            0.75,
            true,
            false,
            BatterySource::SteelSeriesHid,
            WirelessDeviceKind::Headphones,
            Some(0.20),
        );
        let br = to_battery_reading(&live);
        assert_eq!(br.secondary_level, Some(20));
        assert_eq!(br.device_kind, WirelessDeviceKind::Headphones);

        let cache_path =
            std::env::temp_dir().join(format!("otb-ss-spare-{}.json", std::process::id()));
        let _ = std::fs::remove_file(&cache_path);
        let mut agg = WirelessBatteryAggregator::new(AggregatorSettings::default())
            .with_cache_path(cache_path.clone());
        let merged = agg.merge(vec![br]);
        assert_eq!(merged.len(), 1);
        let restored = to_wireless_reading(merged[0].clone());
        assert_eq!(restored.secondary_level, Some(0.20));
        assert_eq!(restored.device_kind, WirelessDeviceKind::Headphones);
        let _ = std::fs::remove_file(cache_path);
    }

    #[test]
    fn dualsense_and_ds4_bluetooth_reports_parse() {
        // DualSense BT 0x31 report (battery at byte 54)
        let mut ds_bt = vec![0u8; 78];
        ds_bt[0] = 0x31;
        ds_bt[54] = 0x07; // discharging, step 7 -> midpoint 75%
        let parsed = parse_gamepad(0x054C, 0x0DF2, &ds_bt, "gp:edge", "DualSense Edge")
            .expect("DualSense BT 0x31");
        assert_eq!(parsed.level, 75);
        assert!(!parsed.charging);
        assert_eq!(parsed.device_kind, WirelessDeviceKind::Gamepad);

        // DualShock 4 BT 0x11 report (battery at byte 32)
        let mut ds4_bt = vec![0u8; 78];
        ds4_bt[0] = 0x11;
        ds4_bt[32] = 0x16; // cable + step 6 -> midpoint 65% charging
        let parsed4 = parse_gamepad(0x054C, 0x09CC, &ds4_bt, "gp:ds4", "DualShock 4")
            .expect("DualShock 4 BT 0x11");
        assert_eq!(parsed4.level, 65);
        assert!(parsed4.charging);
        assert_eq!(parsed4.device_kind, WirelessDeviceKind::Gamepad);
    }

    #[test]
    fn corsair_bragi_both_endpoints_and_probe_policy_gating() {
        let req08 = build_bragi_request(0x08, BRAGI_PROPERTY_LEVEL, 64);
        assert_eq!(req08[0], 0x08);
        assert_eq!(&req08[1..4], &[0x02, 0x0F, 0x00]);
        let req09 = build_bragi_request(0x09, BRAGI_PROPERTY_STATUS, 65);
        assert_eq!(req09[0], 0x00);
        assert_eq!(req09[1], 0x09);
        assert_eq!(&req09[2..5], &[0x02, 0x10, 0x00]);

        let listen_only_policy = ProbePolicy {
            listen_only: true,
            multi_brand_enabled: true,
            disabled_vendors: Vec::new(),
        };
        assert!(!listen_only_policy.allows_source(BatterySource::AsusRogHid));
        assert!(!listen_only_policy.allows_source(BatterySource::CorsairHid));
        assert!(!listen_only_policy.allows_source(BatterySource::HyperXHid));
        assert!(!listen_only_policy.allows_source(BatterySource::CompxAtkVxe));
        assert!(!listen_only_policy.allows_source(BatterySource::Keyboard24gHid));
        assert!(!listen_only_policy.allows_source(BatterySource::RazerHid));
        assert!(listen_only_policy.allows_source(BatterySource::GamepadHid));
        assert!(listen_only_policy.allows_source(BatterySource::Bluetooth));

        let no_multi_brand = ProbePolicy {
            listen_only: false,
            multi_brand_enabled: false,
            disabled_vendors: Vec::new(),
        };
        assert!(!no_multi_brand.allows_source(BatterySource::AsusRogHid));
        assert!(!no_multi_brand.allows_source(BatterySource::CompxAtkVxe));
        assert!(no_multi_brand.allows_source(BatterySource::LogitechHidPlusPlus));

        let disabled_vendor = ProbePolicy {
            listen_only: false,
            multi_brand_enabled: true,
            disabled_vendors: vec!["Corsair".into(), "glorious".into(), "zowie".into()],
        };
        assert!(!disabled_vendor.allows_source(BatterySource::CorsairHid));
        assert!(disabled_vendor.allows_source(BatterySource::AsusRogHid));
        assert!(disabled_vendor.is_vid_disabled(0x258A, "Glorious Model O 2"));
        assert!(disabled_vendor.is_vid_disabled(0x3057, "ZOWIE U2"));
        assert!(!disabled_vendor.is_vid_disabled(0x372E, "Lamzu Maya"));
    }

    #[test]
    fn razer_reply_tx_id_distinguishes_hyperspeed_slots() {
        let mut reply_a = vec![0u8; RAZER_BUFFER_LEN];
        reply_a[1] = RAZER_STATUS_SUCCESS;
        reply_a[2] = 0x1F;
        reply_a[7] = RAZER_COMMAND_CLASS_POWER;
        reply_a[8] = RAZER_CMD_BATTERY;
        reply_a[10] = 180;
        reply_a[89] = razer_xor_crc(&reply_a);
        assert_eq!(razer_reply_tx_id(&reply_a), Some(0x1F));
        assert_eq!(razer_parse_reply(&reply_a, RAZER_CMD_BATTERY), Some(180));

        let mut reply_b = reply_a.clone();
        reply_b[2] = 0x3F;
        reply_b[10] = 90;
        reply_b[89] = razer_xor_crc(&reply_b);
        assert_eq!(razer_reply_tx_id(&reply_b), Some(0x3F));
        assert_eq!(razer_parse_reply(&reply_b, RAZER_CMD_BATTERY), Some(90));
    }

    fn temp_cache(tag: &str) -> PathBuf {
        let unique = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos();
        std::env::temp_dir().join(format!("otb-wb-{tag}-{}-{unique}.json", std::process::id()))
    }

    #[test]
    fn accuracy_live_gatt_beats_windows_pnp_for_same_physical_device() {
        let gatt = reading_with_meta(
            bt_physical_id(0xA0B1_C2D3_E4F5),
            "Buds".into(),
            0.42,
            true,
            false,
            BatterySource::BluetoothGatt,
            WirelessDeviceKind::Headphones,
            None,
        );
        let pnp = reading_with_meta(
            bt_physical_id(0xA0B1_C2D3_E4F5),
            "Buds".into(),
            0.80,
            true,
            false,
            BatterySource::Bluetooth,
            WirelessDeviceKind::Headphones,
            None,
        );
        assert!(prefer(&gatt, &pnp));
        assert!(!prefer(&pnp, &gatt));
        let path = temp_cache("gatt-vs-pnp");
        let mut agg = WirelessBatteryAggregator::new(AggregatorSettings::default())
            .with_cache_path(path.clone());
        let merged = agg.merge(vec![to_battery_reading(&pnp), to_battery_reading(&gatt)]);
        assert_eq!(merged.len(), 1, "one physical battery -> one row");
        assert_eq!(merged[0].level, 42);
        assert_eq!(merged[0].source, BatterySource::BluetoothGatt);
        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn accuracy_disconnected_device_expires_after_31_minutes_without_renewal() {
        let path = temp_cache("expiry31");
        let id = "hid:046D:C52B:EXPIRY31";
        let now = Instant::now();
        let wall0 = wall_clock_secs();
        let mut live =
            BatteryReading::new(id, "Mouse", 70, false, BatterySource::LogitechHidPlusPlus);
        live.sampled_unix = Some(wall0);
        let mut agg = WirelessBatteryAggregator::new(AggregatorSettings::default())
            .with_cache_path(path.clone());
        let first = agg.merge_at(vec![live], now, wall0);
        assert_eq!(first.len(), 1);
        assert!(first[0].connected && !first[0].stale);
        for minute in 1..=31u64 {
            let wall = wall0 + minute * 60;
            // The device keeps being "seen" as a last-known value (registry / cached PnP).
            let mut echo =
                BatteryReading::new(id, "Mouse", 70, false, BatterySource::LogitechHidPlusPlus);
            echo.connected = false;
            echo.stale = true;
            echo.sampled_unix = Some(wall);
            // Production rebuilds the aggregator (and reloads the disk cache) every poll.
            let mut agg = WirelessBatteryAggregator::new(AggregatorSettings::default())
                .with_cache_path(path.clone());
            let rows = agg.merge_at(vec![echo], now, wall);
            if minute <= 30 {
                assert_eq!(rows.len(), 1, "minute {minute}: retained as last-known");
                assert!(rows[0].stale && !rows[0].connected);
                assert_eq!(
                    rows[0].sampled_unix,
                    Some(wall0),
                    "minute {minute}: sample time must never renew"
                );
            } else {
                assert!(rows.is_empty(), "minute {minute}: must have expired");
            }
        }
        let mut echo =
            BatteryReading::new(id, "Mouse", 70, false, BatterySource::LogitechHidPlusPlus);
        echo.connected = false;
        echo.stale = true;
        let mut agg = WirelessBatteryAggregator::new(AggregatorSettings::default())
            .with_cache_path(path.clone());
        assert!(
            agg.merge_at(vec![echo], now, wall0 + 40 * 60).is_empty(),
            "a stale re-report must not resurrect an expired device"
        );
        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn accuracy_identical_name_peripherals_stay_separate_rows() {
        let path = temp_cache("same-name");
        let a = BatteryReading::new(
            "logitech:046D:C548:AAAA1111:01",
            "MX Master 3",
            30,
            false,
            BatterySource::LogitechHidPlusPlus,
        );
        let b = BatteryReading::new(
            "logitech:046D:C548:BBBB2222:01",
            "MX Master 3",
            90,
            false,
            BatterySource::LogitechHidPlusPlus,
        );
        let mut agg = WirelessBatteryAggregator::new(AggregatorSettings::default())
            .with_cache_path(path.clone());
        let rows = agg.merge(vec![a, b]);
        assert_eq!(rows.len(), 2);
        let mut levels: Vec<i32> = rows.iter().map(|r| r.level).collect();
        levels.sort_unstable();
        assert_eq!(levels, vec![30, 90]);
        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn forced_refresh_bypasses_the_aggregate_reuse_window() {
        let just_now = Instant::now();
        assert!(aggregate_is_reusable(
            just_now,
            Duration::from_secs(30),
            false
        ));
        assert!(!aggregate_is_reusable(
            just_now,
            Duration::from_secs(30),
            true
        ));
    }

    #[test]
    fn unknown_charge_is_preserved_not_clamped_to_zero() {
        let mut unknown =
            BatteryReading::new("hid:1:2", "Pad", 0, false, BatterySource::StandardHid);
        unknown.level = -1;
        let wireless = to_wireless_reading(unknown);
        assert_eq!(wireless.level_percent_opt(), None);
        let back = to_battery_reading(&wireless);
        assert_eq!(back.level, -1);
    }

    #[test]
    fn large_drop_guard_holds_only_uncertain_sources() {
        let path = temp_cache("drop-guard");
        let now = Instant::now();
        let wall = wall_clock_secs();
        let mk = |source, level| {
            let mut r = BatteryReading::new("bt:0000000000AB", "Buds", level, false, source);
            r.sampled_unix = Some(wall);
            r
        };
        let mut agg = WirelessBatteryAggregator::new(AggregatorSettings::default())
            .with_cache_path(path.clone());
        agg.merge_at(vec![mk(BatterySource::BluetoothGatt, 90)], now, wall);
        let rows = agg.merge_at(vec![mk(BatterySource::BluetoothGatt, 40)], now, wall + 30);
        assert_eq!(rows[0].level, 40, "verified live GATT drop is taken as-is");

        let pnp_key = "bt:0000000000ac";
        let mk_pnp = |level| {
            let mut r =
                BatteryReading::new(pnp_key, "Head", level, false, BatterySource::Bluetooth);
            r.sampled_unix = Some(wall);
            r
        };
        agg.merge_at(vec![mk_pnp(90)], now, wall);
        let held = agg.merge_at(vec![mk_pnp(40)], now, wall + 30);
        let row = held.iter().find(|r| r.id == pnp_key).unwrap();
        assert_eq!(row.level, 90, "one implausible cached PnP drop is held");
        assert_eq!(
            row.sampled_unix,
            Some(wall),
            "held sample keeps its sample time"
        );
        agg.guard.pending.remove(&physical_key(pnp_key));
        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn asus_rog_chakram_x_65_byte_layout_reports_57_percent() {
        let mut reply = vec![0u8; 65];
        reply[0] = 0x00;
        reply[1] = ASUS_CMD_CATEGORY;
        reply[2] = ASUS_CMD_GET_BATTERY;
        reply[5] = 0x39;
        reply[6] = 0xFF; // not a charge state on this layout
        reply[10] = 0x00;
        let parsed = parse_asus_rog(&reply, "rog", "ROG Chakram X").expect("Chakram X");
        assert_eq!(parsed.level, 57);
        assert!(!parsed.charging);
    }

    #[test]
    fn ghub_battery_state_integer_one_is_one_percent() {
        let state = |pct: &str| {
            format!(
                r#"{{"msgId":"ot-b0","path":"/battery/dev00000001/state","result":{{"code":"SUCCESS"}},"payload":{{"percentage":{pct},"charging":true}}}}"#
            )
        };
        assert_eq!(parse_ghub_battery_state(&state("1")), Some((1, true)));
        assert_eq!(parse_ghub_battery_state(&state("1.0")), Some((1, true)));
        assert_eq!(parse_ghub_battery_state(&state("0.5")), Some((50, true)));
        assert_eq!(parse_ghub_battery_state(&state("100")), Some((100, true)));
        assert_eq!(parse_ghub_battery_state(&state("101")), None);
        assert_eq!(
            parse_ghub_battery_state(r#"{"result":{"code":"NO_SUCH_PATH"},"payload":{}}"#),
            None
        );
        let list = r#"{"path":"/devices/list","payload":{"deviceInfos":[{"id":"dev00000001","extendedDisplayName":"PRO X SUPERLIGHT 2","capabilities":{"hasBatteryStatus":true}},{"id":"dev00000002","displayName":"G915","capabilities":{"hasBatteryStatus":false}}]}}"#;
        assert_eq!(
            parse_ghub_device_list(list),
            vec![("dev00000001".to_owned(), "PRO X SUPERLIGHT 2".to_owned())]
        );
        // Integer 1 inside loopback JSON is 1%, and a nameless SteelSeries device is not
        // labelled "Logitech".
        let gg = parse_steelseries_gg_json(r#"{"devices":[{"battery":1}]}"#);
        assert_eq!(gg[0].level, 1);
        assert_eq!(gg[0].name, "SteelSeries Wireless");
    }

    #[test]
    fn steelseries_nova7_direct_vs_discrete_and_nova5_offsets() {
        assert_eq!(ss_protocol_for_pid(0x227E), Some(SsProtocol::NovaDirect));
        assert_eq!(ss_protocol_for_pid(0x2258), Some(SsProtocol::NovaDirect));
        assert_eq!(ss_protocol_for_pid(0x2202), Some(SsProtocol::Nova7));
        assert_eq!(ss_protocol_for_pid(0x2236), Some(SsProtocol::Arctis7Plus));
        assert_eq!(ss_protocol_for_pid(0x12C2), Some(SsProtocol::Arctis9));

        let req = ss_build_request(SsProtocol::Nova7, 65, true);
        assert_eq!(&req[..2], &[0x00, 0xB0], "Nova 7 uses report id 0 + B0");
        let req = ss_build_request(SsProtocol::NovaPro, 64, false);
        assert_eq!(&req[..2], &[0x06, 0xB0], "Nova Pro uses report id 6");
        let req = ss_build_request(SsProtocol::Arctis9, 65, false);
        assert_eq!(&req[..2], &[0x00, 0x20]);

        let mut frame = vec![0u8; 65];
        frame[1] = 0xB0;
        frame[2] = 0x03; // connected
        frame[3] = 2; // discrete step 2 of 4
        frame[4] = 0x00;
        let discrete = ss_parse(SsProtocol::Nova7, &frame).expect("Nova 7 discrete");
        assert!((discrete.level - 0.5).abs() < f32::EPSILON);
        assert!(discrete.connected && !discrete.charging);
        frame[3] = 42;
        assert!(
            ss_parse(SsProtocol::Nova7, &frame).is_none(),
            "42 is not a 0..4 step"
        );
        let direct = ss_parse(SsProtocol::NovaDirect, &frame).expect("Gen 2 direct %");
        assert!((direct.level - 0.42).abs() < 1e-6);
        frame[2] = 0x01;
        assert!(!ss_parse(SsProtocol::NovaDirect, &frame).unwrap().connected);

        let mut arctis7p = vec![0u8; 65];
        arctis7p[1] = 0xB0;
        arctis7p[2] = 0x03; // online
        arctis7p[3] = 3; // 75%
        arctis7p[4] = 0x01; // charging
        let a7p = ss_parse(SsProtocol::Arctis7Plus, &arctis7p).expect("Arctis 7+ online");
        assert!((a7p.level - 0.75).abs() < f32::EPSILON);
        assert!(a7p.connected && a7p.charging);
        arctis7p[2] = 0x01; // offline
        assert!(
            !ss_parse(SsProtocol::Arctis7Plus, &arctis7p)
                .unwrap()
                .connected
        );
        // Zero-filled echoed request buffer must be rejected, never reported as connected 0%.
        let mut echoed = vec![0u8; 65];
        echoed[1] = 0xB0;
        assert!(ss_parse(SsProtocol::Arctis7Plus, &echoed).is_none());
        assert!(ss_parse(SsProtocol::Nova5, &echoed).is_none());

        let mut nova5 = vec![0u8; 65];
        nova5[1] = 0xB0;
        nova5[2] = 0x01; // byte 1: online
        nova5[4] = 63; // byte 3: level
        nova5[5] = 0x01; // byte 4: charging
        let parsed = ss_parse(SsProtocol::Nova5, &nova5).expect("Nova 5");
        assert!((parsed.level - 0.63).abs() < 1e-6);
        assert!(parsed.charging && parsed.connected);
        nova5[2] = 0x02; // offline
        assert!(!ss_parse(SsProtocol::Nova5, &nova5).unwrap().connected);

        let mut arctis9 = vec![0u8; 65];
        arctis9[1] = 0xAA; // Arctis 9 response header
        arctis9[2] = 0x01; // online
        arctis9[4] = 0x9A;
        arctis9[5] = 0x01;
        let parsed = ss_parse(SsProtocol::Arctis9, &arctis9).expect("Arctis 9");
        assert!((parsed.level - 1.0).abs() < f32::EPSILON);
        assert!(parsed.connected && parsed.charging);
        arctis9[2] = 0x00; // offline
        assert!(!ss_parse(SsProtocol::Arctis9, &arctis9).unwrap().connected);
        arctis9[2] = 0x01;
        arctis9[4] = 0x50;
        assert!(ss_parse(SsProtocol::Arctis9, &arctis9).is_none());
        arctis9[1] = 0x00; // missing 0xAA header must be rejected
        arctis9[4] = 0x9A;
        assert!(ss_parse(SsProtocol::Arctis9, &arctis9).is_none());
    }

    fn value_caps(page: u16, usage: u16) -> HidpValueCaps {
        let mut caps: HidpValueCaps = unsafe { std::mem::zeroed() };
        caps.usage_page = page;
        caps.usage_or_usage_min = usage;
        caps
    }

    #[test]
    fn standard_hid_rejects_overload_and_preserves_zero_percent() {
        assert_eq!(match_battery_usage(&value_caps(0x0084, 0x0065)), None);
        assert_eq!(match_battery_usage(&value_caps(0x0085, 0x0066)), None);
        assert_eq!(
            match_battery_usage(&value_caps(0x0085, 0x0065)),
            Some((0x0085, 0x0065))
        );
        assert_eq!(
            match_battery_usage(&value_caps(0x0006, 0x0020)),
            Some((0x0006, 0x0020))
        );
        let cap = BatteryValueCap {
            is_feature: true,
            report_id: 1,
            usage_page: 0x0085,
            usage: 0x0065,
            link_collection: 0,
            logical_min: 0,
            logical_max: 100,
            bit_size: 8,
        };
        assert_eq!(direct_byte_report(0, &cap), Some((0, false)), "0% is real");
        assert_eq!(direct_byte_report(101, &cap), None);
        let signed = BatteryValueCap {
            logical_min: -128,
            logical_max: 127,
            ..cap.clone()
        };
        assert_eq!(signed_usage_value(0xFF, &signed), -1);
        assert_eq!(direct_byte_report(0x80, &signed), Some((0, false)));
        let signed32 = BatteryValueCap {
            logical_min: -100,
            logical_max: 100,
            bit_size: 32,
            ..cap.clone()
        };
        assert_eq!(signed_usage_value(0xFFFF_FFFF, &signed32), -1);
        assert_eq!(signed_usage_value((-100i32) as u32, &signed32), -100);
        assert_eq!(
            direct_byte_report((-100i32) as u32, &signed32),
            Some((0, false))
        );
        assert_eq!(direct_byte_report(0, &signed32), Some((50, false)));
        assert_eq!(direct_byte_report(100, &signed32), Some((100, false)));
        assert_eq!(
            normalize_logical_to_percent(i64::from(u32::MAX), 0, 100),
            -1
        );
    }

    #[test]
    fn bragi_status_reply_is_never_a_level() {
        assert_eq!(bragi_charging_state(1), Some(true));
        assert_eq!(bragi_charging_state(2), Some(false));
        assert_eq!(bragi_charging_state(500), None);
        // A `1` here is 0.1% -> 0%, never "1%" from a misrouted status answer.
        assert_eq!(bragi_level_percent(1), Some(0));
        assert_eq!(bragi_level_percent(550), Some(55));
        assert_eq!(bragi_level_percent(1001), None);
        let mut reply = vec![0u8; 64];
        reply[0] = 0x01; // channel 1 = route 0x09
        reply[1] = 0x02;
        reply[2] = 0x00;
        reply[3] = 0x26;
        reply[4] = 0x02;
        assert!(is_bragi_reply_for(&reply, 0x09));
        assert!(!is_bragi_reply_for(&reply, 0x08));
        assert_eq!(parse_bragi_value(&reply, 0x09), Some(550));
        reply[2] = 0x05; // "no such property"
        assert!(!is_bragi_reply_for(&reply, 0x09));
    }

    #[test]
    fn compx_atk_frames_require_checksum_and_reject_levels_above_100() {
        let mut padded = vec![0u8; 64];
        let mut frame = build_compx_request();
        frame[6] = 61;
        frame[16] = compx_checksum(&frame);
        padded[..17].copy_from_slice(&frame);
        assert_eq!(parse_compx_nordic(&padded, "c", "Mouse").unwrap().level, 61);
        padded[16] ^= 0x01;
        assert!(parse_compx_nordic(&padded, "c", "Mouse").is_none());
        let mut atk = [0u8; 9];
        atk[..6].copy_from_slice(&[0x82, 0x08, 0x02, 0x00, 0xFF, 0x03]);
        atk[8] = 0x7F; // 127 in the 7-bit field
        assert!(parse_atk1k(&atk, "atk", "ATK").is_none());
    }

    #[test]
    fn bluetooth_identity_comes_from_the_address_not_the_name() {
        assert_eq!(
            bt_address_from_instance(r"BTHLE\Dev_a0b1c2d3e4f5\8&1234&0&A0B1C2D3E4F5"),
            Some(0xA0B1_C2D3_E4F5)
        );
        assert_eq!(
            bt_address_from_instance(
                r"BTHENUM\{0000111E-0000-1000-8000-00805F9B34FB}_LOCALMFG&005D\7&2A1F6C3&0&A0B1C2D3E4F5_C00000000"
            ),
            Some(0xA0B1_C2D3_E4F5)
        );
        assert_eq!(
            bt_address_from_instance(r"BTHHFENUM\BthHFPAudio\8&3A2B&0&97"),
            None
        );
        assert_eq!(bt_physical_id(0xA0B1_C2D3_E4F5), "bt:A0B1C2D3E4F5");
        assert_eq!(physical_key("bt:A0B1C2D3E4F5"), "bt:a0b1c2d3e4f5");
        assert_eq!(
            physical_key("razer:1532:00B7:0A1B2C3D"),
            physical_key("hid:1532:00B7:0A1B2C3D")
        );
    }
}
