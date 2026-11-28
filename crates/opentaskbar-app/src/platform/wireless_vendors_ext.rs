//! Extended wireless peripheral vendor protocols, model catalogs, and device-kind
//! classification (`Glorious`, `Endgame Gear`, `Finalmouse`, `Zowie / BenQ`,
//! `Keychron Raw HID`, verified 3-family `HyperX`, and multi-brand VID/PID tables).
//!
//! All functions in this module are pure builders, parsers, and lookup tables with
//! zero background threads and zero OS I/O; live HID sessions run inside
//! [`super::wireless_battery`]'s bounded `ALL_SOURCES` probe workers.

use super::wireless_battery::{
    BatteryReading, BatterySource, WirelessDeviceKind, compx_checksum, parse_compx_nordic,
    parse_compx17,
};

// ---------------------------------------------------------------------------
// Extended vendor IDs & protocol constants
// ---------------------------------------------------------------------------

pub const HP_HYPERX_VID: u16 = 0x03F0;
pub const KINGSTON_HYPERX_VID: u16 = 0x0951;

pub const GLORIOUS_VID_SINOWEALTH: u16 = 0x258A;
pub const GLORIOUS_VID_GMMK: u16 = 0x320F;
pub const GLORIOUS_VID_PIXART: u16 = 0x093A;

pub const ENDGAME_GEAR_VID: u16 = 0x3367;
pub const FINALMOUSE_VID: u16 = 0x361D;
pub const ZOWIE_BENQ_VID: u16 = 0x3057;
pub const ZOWIE_ENHANCED_RX_VID: u16 = 0x1EA7;
/// BenQ corporate VID used by the newer ZOWIE EC-CW / U2 / DW receivers.
pub const ZOWIE_BENQ_RX_VID: u16 = 0x04A5;
pub const KEYCHRON_VID: u16 = 0x3434;

pub const KEYCHRON_RAW_HID_USAGE_PAGE: u16 = 0xFF60;
pub const KEYCHRON_VENDOR_USAGE_PAGE: u16 = 0xFFC1;

// ---------------------------------------------------------------------------
// 1. HyperX verified 3-family protocols (HP 0x03F0 + Kingston 0x0951)
// ---------------------------------------------------------------------------

/// Exact per-PID HyperX battery protocols. Each variant owns its request bytes, reply echo
/// and field offsets; a PID is never probed with another family's command.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum HyperXDeviceProtocol {
    /// HP Cloud headsets on `66 89` (Stinger 2, Cloud Mini, II Core): level byte 4,
    /// state byte 5 (`1` = charging).
    HpCloudHeadset64,
    /// HP Cloud III Wireless: `66 0D` answers `66 0D <level>`; charge state is the separate
    /// `66 8A` query (answered as `66 8A`/`66 0C <state>`).
    HpCloudIII,
    /// HP Cloud II Wireless: `06 FF BB 02` level (byte 7), `06 FF BB 03` charge state (byte 4).
    HpCloudII,
    /// Kingston Cloud II Wireless (`0x0951:0x1718`) and siblings: `21 FF 05`, level byte 7
    /// (byte 3 fallback), charging byte 4.
    KingstonCloud20,
    /// Cloud Alpha Wireless (`0x03F0:0x098D`): `21 BB 0B` level (byte 3), `21 BB 0C`
    /// charge state (byte 3).
    CloudAlpha,
    /// Kingston Cloud Flight (`0x0951:0x16C4`): `21 FF 05` answers big-endian millivolts in
    /// bytes 3..5, mapped through a Li-ion discharge curve.
    CloudFlightVoltage,
    /// Pulsefire Haste 2 / Dart on `04 03`: level byte 4, state byte 5.
    PulsefireMouse64,
    /// Cloud III S (`02CC`/`06BE`) and Haste 2 (`0D97`/`0F98`): `50 02` answers
    /// `51 02 <level> <charging>`.
    HpVendor50,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct HyperXDeviceProfile {
    pub vendor_id: u16,
    pub product_id: u16,
    pub default_name: &'static str,
    pub device_kind: WirelessDeviceKind,
    pub protocol: HyperXDeviceProtocol,
}

const fn hx(
    vendor_id: u16,
    product_id: u16,
    default_name: &'static str,
    device_kind: WirelessDeviceKind,
    protocol: HyperXDeviceProtocol,
) -> HyperXDeviceProfile {
    HyperXDeviceProfile {
        vendor_id,
        product_id,
        default_name,
        device_kind,
        protocol,
    }
}

const HEADSET: WirelessDeviceKind = WirelessDeviceKind::Headphones;
const MOUSE: WirelessDeviceKind = WirelessDeviceKind::Mouse;

pub const HYPERX_PROFILES: &[HyperXDeviceProfile] = &[
    // Cloud III Wireless (HP)
    hx(
        HP_HYPERX_VID,
        0x05B7,
        "HyperX Cloud III Wireless",
        HEADSET,
        HyperXDeviceProtocol::HpCloudIII,
    ),
    hx(
        HP_HYPERX_VID,
        0x0C9D,
        "HyperX Cloud III Wireless",
        HEADSET,
        HyperXDeviceProtocol::HpCloudIII,
    ),
    // Cloud III S Wireless (HP)
    hx(
        HP_HYPERX_VID,
        0x02CC,
        "HyperX Cloud III S Wireless",
        HEADSET,
        HyperXDeviceProtocol::HpVendor50,
    ),
    hx(
        HP_HYPERX_VID,
        0x06BE,
        "HyperX Cloud III S Wireless",
        HEADSET,
        HyperXDeviceProtocol::HpVendor50,
    ),
    // Cloud Alpha Wireless (HP / Kingston)
    hx(
        HP_HYPERX_VID,
        0x098D,
        "HyperX Cloud Alpha Wireless",
        HEADSET,
        HyperXDeviceProtocol::CloudAlpha,
    ),
    hx(
        KINGSTON_HYPERX_VID,
        0x1743,
        "HyperX Cloud Alpha Wireless",
        HEADSET,
        HyperXDeviceProtocol::CloudAlpha,
    ),
    // Cloud II Wireless: HP (06 FF BB) vs Kingston (21 FF 05) are different protocols.
    hx(
        HP_HYPERX_VID,
        0x018B,
        "HyperX Cloud II Wireless",
        HEADSET,
        HyperXDeviceProtocol::HpCloudII,
    ),
    hx(
        HP_HYPERX_VID,
        0x0696,
        "HyperX Cloud II Core Wireless",
        HEADSET,
        HyperXDeviceProtocol::HpCloudHeadset64,
    ),
    hx(
        KINGSTON_HYPERX_VID,
        0x1718,
        "HyperX Cloud II Wireless",
        HEADSET,
        HyperXDeviceProtocol::KingstonCloud20,
    ),
    // Cloud Flight (voltage) / Flight S / Stinger 2 / Stinger Core / Mini
    hx(
        KINGSTON_HYPERX_VID,
        0x16C4,
        "HyperX Cloud Flight Wireless",
        HEADSET,
        HyperXDeviceProtocol::CloudFlightVoltage,
    ),
    hx(
        KINGSTON_HYPERX_VID,
        0x16EA,
        "HyperX Cloud Flight S",
        HEADSET,
        HyperXDeviceProtocol::KingstonCloud20,
    ),
    hx(
        HP_HYPERX_VID,
        0x0D93,
        "HyperX Cloud Stinger 2 Wireless",
        HEADSET,
        HyperXDeviceProtocol::HpCloudHeadset64,
    ),
    hx(
        HP_HYPERX_VID,
        0x089B,
        "HyperX Cloud Mini Wireless",
        HEADSET,
        HyperXDeviceProtocol::HpCloudHeadset64,
    ),
    hx(
        KINGSTON_HYPERX_VID,
        0x1733,
        "HyperX Cloud Stinger Core Wireless",
        HEADSET,
        HyperXDeviceProtocol::KingstonCloud20,
    ),
    // Pulsefire Haste 2 (50 02 receivers) / Haste 2 / Haste 2 Mini / Dart (04 03)
    hx(
        HP_HYPERX_VID,
        0x0D97,
        "HyperX Pulsefire Haste 2 Wireless",
        MOUSE,
        HyperXDeviceProtocol::HpVendor50,
    ),
    hx(
        HP_HYPERX_VID,
        0x0F98,
        "HyperX Pulsefire Haste 2 Wireless",
        MOUSE,
        HyperXDeviceProtocol::HpVendor50,
    ),
    hx(
        HP_HYPERX_VID,
        0x02B8,
        "HyperX Pulsefire Haste 2 Wireless",
        MOUSE,
        HyperXDeviceProtocol::PulsefireMouse64,
    ),
    hx(
        HP_HYPERX_VID,
        0x0F8E,
        "HyperX Pulsefire Haste 2 Wireless",
        MOUSE,
        HyperXDeviceProtocol::PulsefireMouse64,
    ),
    hx(
        HP_HYPERX_VID,
        0x0A97,
        "HyperX Pulsefire Haste 2 Mini Wireless",
        MOUSE,
        HyperXDeviceProtocol::PulsefireMouse64,
    ),
    hx(
        KINGSTON_HYPERX_VID,
        0x16E1,
        "HyperX Pulsefire Dart Wireless",
        MOUSE,
        HyperXDeviceProtocol::PulsefireMouse64,
    ),
];

pub fn hyperx_profile(vendor_id: u16, product_id: u16) -> Option<HyperXDeviceProfile> {
    HYPERX_PROFILES
        .iter()
        .copied()
        .find(|p| p.vendor_id == vendor_id && p.product_id == product_id)
}

fn hyperx_level_command(protocol: HyperXDeviceProtocol) -> &'static [u8] {
    match protocol {
        HyperXDeviceProtocol::HpCloudHeadset64 => &[0x66, 0x89],
        HyperXDeviceProtocol::HpCloudIII => &[0x66, 0x0D],
        HyperXDeviceProtocol::HpCloudII => &[0x06, 0xFF, 0xBB, 0x02],
        HyperXDeviceProtocol::KingstonCloud20 | HyperXDeviceProtocol::CloudFlightVoltage => {
            &[0x21, 0xFF, 0x05]
        }
        HyperXDeviceProtocol::CloudAlpha => &[0x21, 0xBB, 0x0B],
        HyperXDeviceProtocol::PulsefireMouse64 => &[0x04, 0x03],
        HyperXDeviceProtocol::HpVendor50 => &[0x50, 0x02],
    }
}

fn hyperx_charging_command(protocol: HyperXDeviceProtocol) -> Option<&'static [u8]> {
    match protocol {
        HyperXDeviceProtocol::HpCloudIII => Some(&[0x66, 0x8A]),
        HyperXDeviceProtocol::HpCloudII => Some(&[0x06, 0xFF, 0xBB, 0x03]),
        HyperXDeviceProtocol::CloudAlpha => Some(&[0x21, 0xBB, 0x0C]),
        _ => None,
    }
}

fn hyperx_packet(command: &[u8], report_len: usize) -> Vec<u8> {
    let len = report_len.clamp(20, 65);
    let mut packet = vec![0u8; len];
    packet[..command.len()].copy_from_slice(command);
    packet
}

pub fn build_hyperx_protocol_request(protocol: HyperXDeviceProtocol, report_len: usize) -> Vec<u8> {
    hyperx_packet(hyperx_level_command(protocol), report_len)
}

/// Separate charge-state query for the protocols that have one.
pub fn build_hyperx_charging_request(
    protocol: HyperXDeviceProtocol,
    report_len: usize,
) -> Option<Vec<u8>> {
    hyperx_charging_command(protocol).map(|command| hyperx_packet(command, report_len))
}

fn normalize_hyperx_protocol_framing(raw: &[u8]) -> &[u8] {
    if raw.len() >= 6 && raw[0] == 0x00 && matches!(raw[1], 0x66 | 0x21 | 0x04 | 0x06 | 0x51) {
        &raw[1..]
    } else {
        raw
    }
}

pub fn is_valid_hyperx_protocol_reply(protocol: HyperXDeviceProtocol, raw: &[u8]) -> bool {
    let payload = normalize_hyperx_protocol_framing(raw);
    match protocol {
        HyperXDeviceProtocol::HpCloudHeadset64 => {
            payload.len() >= 6 && payload[..2] == [0x66, 0x89]
        }
        HyperXDeviceProtocol::HpCloudIII => payload.len() >= 3 && payload[..2] == [0x66, 0x0D],
        HyperXDeviceProtocol::HpCloudII => {
            payload.len() >= 8 && payload[..4] == [0x06, 0xFF, 0xBB, 0x02]
        }
        HyperXDeviceProtocol::KingstonCloud20 => {
            payload.len() >= 8 && payload[..3] == [0x21, 0xFF, 0x05]
        }
        HyperXDeviceProtocol::CloudFlightVoltage => {
            payload.len() >= 5 && payload[..3] == [0x21, 0xFF, 0x05]
        }
        HyperXDeviceProtocol::CloudAlpha => {
            payload.len() >= 4 && payload[..3] == [0x21, 0xBB, 0x0B]
        }
        HyperXDeviceProtocol::PulsefireMouse64 => {
            payload.len() >= 6 && payload[..2] == [0x04, 0x03]
        }
        HyperXDeviceProtocol::HpVendor50 => payload.len() >= 4 && payload[..2] == [0x51, 0x02],
    }
}

pub fn is_valid_hyperx_charging_reply(protocol: HyperXDeviceProtocol, raw: &[u8]) -> bool {
    let payload = normalize_hyperx_protocol_framing(raw);
    match protocol {
        HyperXDeviceProtocol::HpCloudIII => {
            payload.len() >= 3 && payload[0] == 0x66 && matches!(payload[1], 0x8A | 0x0C)
        }
        HyperXDeviceProtocol::HpCloudII => {
            payload.len() >= 5 && payload[..4] == [0x06, 0xFF, 0xBB, 0x03]
        }
        HyperXDeviceProtocol::CloudAlpha => {
            payload.len() >= 4 && payload[..3] == [0x21, 0xBB, 0x0C]
        }
        _ => false,
    }
}

/// Charge state from a validated charging reply (`1` = charging, `0` = on battery).
pub fn parse_hyperx_charging_reply(protocol: HyperXDeviceProtocol, raw: &[u8]) -> Option<bool> {
    if !is_valid_hyperx_charging_reply(protocol, raw) {
        return None;
    }
    let payload = normalize_hyperx_protocol_framing(raw);
    let state = match protocol {
        HyperXDeviceProtocol::HpCloudIII => payload[2],
        HyperXDeviceProtocol::HpCloudII => payload[4],
        HyperXDeviceProtocol::CloudAlpha => payload[3],
        _ => return None,
    };
    (state <= 1).then_some(state == 1)
}

/// Li-ion discharge curve for the Cloud Flight's millivolt report.
pub fn hyperx_flight_percent_from_mv(mv: u32) -> u32 {
    const CURVE: [(u32, u32); 10] = [
        (3300, 0),
        (3500, 5),
        (3600, 10),
        (3700, 30),
        (3750, 45),
        (3800, 60),
        (3900, 75),
        (4000, 85),
        (4100, 95),
        (4150, 100),
    ];
    if mv <= CURVE[0].0 {
        return 0;
    }
    for pair in CURVE.windows(2) {
        let ((v0, p0), (v1, p1)) = (pair[0], pair[1]);
        if mv <= v1 {
            return p0 + (mv - v0) * (p1 - p0) / (v1 - v0);
        }
    }
    100
}

pub fn parse_hyperx_protocol(
    protocol: HyperXDeviceProtocol,
    raw_reply: &[u8],
    id: &str,
    name: &str,
    kind: WirelessDeviceKind,
) -> Option<BatteryReading> {
    if !is_valid_hyperx_protocol_reply(protocol, raw_reply) {
        return None;
    }
    let reply = normalize_hyperx_protocol_framing(raw_reply);
    let mut voltage_mv = None;
    let (level, charging) = match protocol {
        HyperXDeviceProtocol::HpCloudHeadset64 | HyperXDeviceProtocol::PulsefireMouse64 => {
            let level = reply[4];
            let status = reply[5];
            if level > 100 || status > 2 {
                return None;
            }
            let charging = status == 1;
            if level == 0 && !charging {
                return None;
            }
            (u32::from(level), charging)
        }
        HyperXDeviceProtocol::KingstonCloud20 => {
            let level = if (1..=100).contains(&reply[7]) {
                reply[7]
            } else {
                reply[3]
            };
            if level == 0 || level > 100 {
                return None;
            }
            (u32::from(level), reply[4] == 1)
        }
        HyperXDeviceProtocol::HpCloudIII => {
            if reply[2] > 100 {
                return None;
            }
            (u32::from(reply[2]), false)
        }
        HyperXDeviceProtocol::HpCloudII => {
            if reply[7] > 100 {
                return None;
            }
            (u32::from(reply[7]), false)
        }
        HyperXDeviceProtocol::CloudAlpha => {
            if reply[3] > 100 {
                return None;
            }
            (u32::from(reply[3]), false)
        }
        HyperXDeviceProtocol::CloudFlightVoltage => {
            let mv = u32::from(u16::from_be_bytes([reply[3], reply[4]]));
            if !(2800..=4600).contains(&mv) {
                return None;
            }
            voltage_mv = Some(mv);
            // The headset reports the charger-raised cell voltage (0x10xx / 0x11xx) while
            // plugged in.
            (
                hyperx_flight_percent_from_mv(mv),
                matches!(reply[3], 0x10 | 0x11),
            )
        }
        HyperXDeviceProtocol::HpVendor50 => {
            let (level, state) = (reply[2], reply[3]);
            if level > 100 || state > 1 {
                return None;
            }
            (u32::from(level), state == 1)
        }
    };
    let clean_name = if name.trim().is_empty() {
        "HyperX Wireless"
    } else {
        name.trim()
    };
    let mut reading =
        BatteryReading::new(id, clean_name, level, charging, BatterySource::HyperXHid);
    reading.device_kind = kind;
    reading.voltage_mv = voltage_mv;
    Some(reading)
}

// ---------------------------------------------------------------------------
// 2. Glorious Wireless Mice & GMMK Keyboards (0x258A / 0x320F / 0x093A)
// ---------------------------------------------------------------------------

pub fn glorious_model_name(vid: u16, pid: u16) -> Option<&'static str> {
    match (vid, pid) {
        (GLORIOUS_VID_SINOWEALTH, 0x2011) => Some("Glorious Model O Wireless"),
        (GLORIOUS_VID_SINOWEALTH, 0x2012) => Some("Glorious Model D Wireless"),
        (GLORIOUS_VID_SINOWEALTH, 0x2013) => Some("Glorious Model O- Wireless"),
        (GLORIOUS_VID_SINOWEALTH, 0x2014) => Some("Glorious Model D- Wireless"),
        (GLORIOUS_VID_SINOWEALTH, 0x2015) => Some("Glorious Model I 2 Wireless"),
        (GLORIOUS_VID_SINOWEALTH, 0x2016) => Some("Glorious Model O 2 Wireless"),
        (GLORIOUS_VID_SINOWEALTH, 0x2017) => Some("Glorious Model O PRO"),
        (GLORIOUS_VID_SINOWEALTH, 0x2018) => Some("Glorious Series One PRO"),
        (GLORIOUS_VID_SINOWEALTH, 0x2019) => Some("Glorious Model D 2 Wireless"),
        (GLORIOUS_VID_SINOWEALTH, 0x2020) => Some("Glorious Model D 2 PRO"),
        // 258A:2022 is the original Model O Wireless 2.4 GHz receiver.
        (GLORIOUS_VID_SINOWEALTH, 0x2022) => Some("Glorious Model O Wireless"),
        (GLORIOUS_VID_PIXART, 0x821A) => Some("Glorious Model O Wireless"),
        (GLORIOUS_VID_PIXART, 0x822A) => Some("Glorious Model D Wireless"),
        (GLORIOUS_VID_PIXART, 0x823A) => Some("Glorious Model O- Wireless"),
        (GLORIOUS_VID_PIXART, 0x824A) => Some("Glorious Model I 2 Wireless"),
        (GLORIOUS_VID_GMMK, 0x5044 | 0x5046) => Some("Glorious GMMK PRO Wireless"),
        (GLORIOUS_VID_GMMK, 0x5088 | 0x5092) => Some("Glorious GMMK 3 Wireless"),
        _ => None,
    }
}

pub fn is_glorious_keyboard(vid: u16, pid: u16, product: &str) -> bool {
    if vid == GLORIOUS_VID_GMMK && glorious_model_name(vid, pid).is_some() {
        return true;
    }
    let lower = product.to_ascii_lowercase();
    vid == GLORIOUS_VID_GMMK && lower.contains("gmmk")
}

pub fn is_glorious_device(vid: u16, pid: u16, product: &str) -> bool {
    if glorious_model_name(vid, pid).is_some() {
        return true;
    }
    if !matches!(
        vid,
        GLORIOUS_VID_SINOWEALTH | GLORIOUS_VID_GMMK | GLORIOUS_VID_PIXART
    ) {
        return false;
    }
    let lower = product.to_ascii_lowercase();
    lower.contains("glorious")
        || lower.contains("gmmk")
        || lower.contains("model o")
        || lower.contains("model d")
        || lower.contains("model i")
}

/// Glorious wireless battery function id.
pub const GLORIOUS_FN_BATTERY: u8 = 0x83;
/// First reply byte while the mouse is asleep / out of range.
pub const GLORIOUS_ASLEEP: u8 = 0xA1;

/// Builds the 64/65-byte Glorious battery query feature report:
/// `[report 00] 00 00 02 02 00 83`.
pub fn build_glorious_request(report_len: usize) -> Vec<u8> {
    let len = report_len.max(64);
    let mut packet = vec![0u8; len];
    let offset = usize::from(len >= 65);
    packet[offset + 2] = 0x02;
    packet[offset + 3] = 0x02;
    packet[offset + 5] = GLORIOUS_FN_BATTERY;
    packet
}

/// Parses a 64/65-byte Glorious wireless mouse / GMMK keyboard feature report reply.
pub fn parse_glorious_reply(
    raw: &[u8],
    vid: u16,
    pid: u16,
    id: &str,
    name: &str,
) -> Option<BatteryReading> {
    let payload = if raw.len() >= 65 && raw[0] == 0x00 {
        &raw[1..]
    } else {
        raw
    };
    if payload.len() < 9 {
        return None;
    }
    // Mouse reply: payload[0] = awake state (`0xA1` asleep), payload[2..4] = `02 02`,
    // payload[5] = function echo `0x83`, payload[6] = charging (0/1), payload[7] = percent.
    let (level, charging) = if payload[2] == 0x02 && payload[3] == 0x02 {
        if payload[0] == GLORIOUS_ASLEEP || payload[5] != GLORIOUS_FN_BATTERY {
            return None;
        }
        let state = payload[6];
        if state > 1 {
            return None;
        }
        let level = payload[7];
        let charging = state == 0x01;
        if level > 100 || (level == 0 && !charging) {
            return None;
        }
        (level, charging)
    } else if payload[0] == 0x04 && payload[1] == 0x02 {
        // Alternate GMMK Wireless 64-byte status reply: [0x04, 0x02, level, charging]
        let level = payload[2];
        let status = payload[3];
        if level > 100 || status > 2 {
            return None;
        }
        let charging = status == 1;
        if level == 0 && !charging {
            return None;
        }
        (level, charging)
    } else {
        return None;
    };

    let clean_name = if name.trim().is_empty() {
        glorious_model_name(vid, pid).unwrap_or("Glorious Wireless")
    } else {
        name.trim()
    };
    let keyboard = is_glorious_keyboard(vid, pid, clean_name);
    let source = if keyboard {
        BatterySource::Keyboard24gHid
    } else {
        BatterySource::CompxAtkVxe
    };
    let mut reading = BatteryReading::new(id, clean_name, u32::from(level), charging, source);
    reading.device_kind = if keyboard {
        WirelessDeviceKind::Keyboard
    } else {
        WirelessDeviceKind::Mouse
    };
    Some(reading)
}

// ---------------------------------------------------------------------------
// 3. Endgame Gear (VID 0x3367: OP1we, XM2we, XM2w 4k, OP1w 4k)
// ---------------------------------------------------------------------------

pub fn endgame_gear_model_name(pid: u16) -> &'static str {
    match pid {
        0x194C | 0x194D => "Endgame Gear XM2we",
        0x1954 | 0x1955 => "Endgame Gear OP1we",
        0x1964 | 0x1965 => "Endgame Gear XM2w 4k",
        0x1968 | 0x1969 => "Endgame Gear OP1w 4k",
        _ => "Endgame Gear Wireless Mouse",
    }
}

/// XM2we family: the command collection (feature report `0x08`) and the answer collection
/// (input report `0x09`) are separate top-level collections of one physical device.
pub const ENDGAME_GEAR_FEATURE_USAGE_PAGE: u16 = 0xFF02;
pub const ENDGAME_GEAR_INPUT_USAGE_PAGE: u16 = 0xFF01;
const ENDGAME_FRAME_LEN: usize = 17;

/// Compx-style 17-byte command frame sent as feature report `0x08`: `08 <cmd> 00.. <sum>`,
/// where the checksum is `0x4D - sum(bytes[1..16])` (report-id independent). `0x03` asks
/// the radio link state, `0x04` the battery.
pub fn build_endgame_gear_feature_request(command: u8, feature_len: usize) -> Vec<u8> {
    let mut packet = vec![0u8; feature_len.max(ENDGAME_FRAME_LEN)];
    packet[0] = 0x08;
    packet[1] = command;
    packet[ENDGAME_FRAME_LEN - 1] = compx_checksum(&packet[..ENDGAME_FRAME_LEN]);
    packet
}

/// A checksummed input report `0x09` answering `command`.
pub fn is_endgame_gear_input_frame(raw: &[u8], command: u8) -> bool {
    raw.len() >= ENDGAME_FRAME_LEN
        && raw[0] == 0x09
        && raw[1] == command
        && compx_checksum(&raw[..ENDGAME_FRAME_LEN]) == raw[ENDGAME_FRAME_LEN - 1]
}

/// Radio link state answer (`09 03 ...`): byte 6 non-zero = mouse linked to the receiver.
pub fn parse_endgame_gear_link_input(raw: &[u8]) -> Option<bool> {
    is_endgame_gear_input_frame(raw, 0x03).then(|| raw[6] != 0)
}

/// Battery answer (`09 04 ...`): byte 6 = percent, byte 7 = charging (0/1), bytes 8..10 =
/// big-endian millivolts.
pub fn parse_endgame_gear_battery_input(
    raw: &[u8],
    pid: u16,
    id: &str,
    name: &str,
) -> Option<BatteryReading> {
    if !is_endgame_gear_input_frame(raw, 0x04) {
        return None;
    }
    let level = raw[6];
    let charge_flag = raw[7];
    let voltage = u32::from(u16::from_be_bytes([raw[8], raw[9]]));
    if level > 100 || charge_flag > 1 || (level == 0 && charge_flag == 0 && voltage == 0) {
        return None;
    }
    let clean_name = if name.trim().is_empty() {
        endgame_gear_model_name(pid)
    } else {
        name.trim()
    };
    let mut reading = BatteryReading::new(
        id,
        clean_name,
        u32::from(level),
        charge_flag == 1,
        BatterySource::CompxAtkVxe,
    );
    reading.voltage_mv = Some(voltage).filter(|mv| (2800..=4500).contains(mv));
    reading.device_kind = WirelessDeviceKind::Mouse;
    Some(reading)
}

pub fn build_endgame_gear_request(report_len: usize) -> Vec<u8> {
    if report_len == 17 {
        let mut packet = vec![0u8; 17];
        packet[0] = 0x08;
        packet[1] = 0x04;
        packet[16] = 0x49;
        return packet;
    }
    let len = report_len.max(64);
    let mut packet = vec![0u8; len];
    let offset = usize::from(len >= 65);
    packet[offset] = 0x04;
    packet[offset + 1] = 0x02;
    packet
}

pub fn is_valid_endgame_gear_reply(raw: &[u8]) -> bool {
    let payload = if raw.len() >= 10 && raw[0] == 0x00 && matches!(raw[1], 0x04 | 0x08) {
        &raw[1..]
    } else {
        raw
    };
    (payload.len() >= 10 && payload[0] == 0x08 && payload[1] == 0x04)
        || (payload.len() >= 4 && payload[0] == 0x04 && payload[1] == 0x02)
}

pub fn parse_endgame_gear_reply(
    raw: &[u8],
    pid: u16,
    id: &str,
    name: &str,
) -> Option<BatteryReading> {
    let clean_name = if name.trim().is_empty() {
        endgame_gear_model_name(pid)
    } else {
        name.trim()
    };
    if let Some(mut r) = parse_compx17(raw, id, clean_name) {
        r.device_kind = WirelessDeviceKind::Mouse;
        return Some(r);
    }
    let payload = if raw.len() >= 5 && raw[0] == 0x00 && raw[1] == 0x04 {
        &raw[1..]
    } else {
        raw
    };
    if payload.len() >= 4 && payload[0] == 0x04 && payload[1] == 0x02 {
        let status = payload[2];
        let level = payload[3];
        if level > 100 || status > 2 {
            return None;
        }
        let charging = status == 1;
        let voltage_mv = payload
            .get(4..6)
            .map(|b| u16::from_be_bytes([b[0], b[1]]) as u32)
            .filter(|mv| (2800..=4500).contains(mv));
        if level == 0 && !charging && voltage_mv.is_none() {
            return None;
        }
        let mut reading = BatteryReading::new(
            id,
            clean_name,
            u32::from(level),
            charging,
            BatterySource::CompxAtkVxe,
        );
        reading.voltage_mv = voltage_mv;
        reading.device_kind = WirelessDeviceKind::Mouse;
        return Some(reading);
    }
    if let Some(mut r) = parse_compx_nordic(raw, id, clean_name) {
        r.device_kind = WirelessDeviceKind::Mouse;
        return Some(r);
    }
    None
}

// ---------------------------------------------------------------------------
// 4. Finalmouse (VID 0x361D: UltralightX / Starlight-12)
// ---------------------------------------------------------------------------

pub fn finalmouse_model_name(pid: u16) -> &'static str {
    match pid {
        // 361D:0100 is the UltralightX 8K receiver.
        0x0100 => "Finalmouse UltralightX",
        0x0101 => "Finalmouse Starlight Pro",
        0x0200 | 0x0201 => "Finalmouse UltralightX",
        0x0202 | 0x0203 => "Finalmouse UltralightX Pro",
        0x0204 => "Finalmouse UltralightX Competition",
        _ => "Finalmouse Wireless",
    }
}

/// ULX receiver framing: the command goes out as report `0x04` (`04 02` = battery) and the
/// answer comes back as report `0x05` (`05 02 <level> <charging> <mV be>`).
pub fn build_finalmouse_request(report_len: usize) -> Vec<u8> {
    let len = report_len.max(64);
    let mut packet = vec![0u8; len];
    let offset = usize::from(len >= 65);
    packet[offset] = 0x04;
    packet[offset + 1] = 0x02;
    packet
}

fn finalmouse_payload(raw: &[u8]) -> &[u8] {
    if raw.len() >= 5 && raw[0] == 0x00 && matches!(raw[1], 0x04 | 0x05) {
        &raw[1..]
    } else {
        raw
    }
}

/// Accepts the `05 02` answer; an `04 02` frame only counts when it carries data (a bare
/// echo of our own request is not an answer).
pub fn is_valid_finalmouse_reply(raw: &[u8]) -> bool {
    let payload = finalmouse_payload(raw);
    payload.len() >= 4
        && payload[1] == 0x02
        && (payload[0] == 0x05 || (payload[0] == 0x04 && payload[2..].iter().any(|&b| b != 0)))
}

pub fn parse_finalmouse_reply(
    raw: &[u8],
    pid: u16,
    id: &str,
    name: &str,
) -> Option<BatteryReading> {
    if !is_valid_finalmouse_reply(raw) {
        return None;
    }
    let payload = finalmouse_payload(raw);
    let level = payload[2];
    let charge_flag = payload[3];
    if level > 100 || charge_flag > 2 {
        return None;
    }
    let charging = charge_flag == 1;
    let voltage_mv = payload.get(4..6).and_then(|b| {
        let be = u16::from_be_bytes([b[0], b[1]]) as u32;
        if (2800..=4500).contains(&be) {
            return Some(be);
        }
        let le = u16::from_le_bytes([b[0], b[1]]) as u32;
        (2800..=4500).contains(&le).then_some(le)
    });
    if level == 0 && !charging && voltage_mv.is_none() {
        return None;
    }
    let clean_name = if name.trim().is_empty() {
        finalmouse_model_name(pid)
    } else {
        name.trim()
    };
    let mut reading = BatteryReading::new(
        id,
        clean_name,
        u32::from(level),
        charging,
        BatterySource::CompxAtkVxe,
    );
    reading.voltage_mv = voltage_mv;
    reading.device_kind = WirelessDeviceKind::Mouse;
    Some(reading)
}

// ---------------------------------------------------------------------------
// 5. Zowie / BenQ (VID 0x3057 / 0x1EA7: U2, EC-CW, FK2-DW, ZA13-DW)
// ---------------------------------------------------------------------------

pub fn zowie_model_name(vid: u16, pid: u16) -> &'static str {
    match (vid, pid) {
        (ZOWIE_BENQ_VID, 0x0201 | 0x0202) => "ZOWIE EC1-CW",
        (ZOWIE_BENQ_VID, 0x0203 | 0x0204) => "ZOWIE EC2-CW",
        (ZOWIE_BENQ_VID, 0x0205 | 0x0206) => "ZOWIE EC3-CW",
        (ZOWIE_BENQ_VID, 0x0210 | 0x0211) => "ZOWIE U2 Wireless",
        (ZOWIE_BENQ_VID, 0x0212 | 0x0213) => "ZOWIE FK2-DW",
        (ZOWIE_BENQ_VID, 0x0214 | 0x0215) => "ZOWIE ZA13-DW",
        (ZOWIE_BENQ_VID, 0x0216 | 0x0217) => "ZOWIE S2-DW",
        (ZOWIE_BENQ_VID, 0x0218 | 0x0219) => "ZOWIE EC2-DW",
        (ZOWIE_ENHANCED_RX_VID, 0x0907 | 0x0908) => "ZOWIE Enhanced Receiver",
        (ZOWIE_BENQ_RX_VID, _) => "ZOWIE Wireless Receiver",
        _ => "ZOWIE Wireless Mouse",
    }
}

fn has_product_token(lower: &str, token: &str) -> bool {
    let bytes = lower.as_bytes();
    for (start, _) in lower.match_indices(token) {
        let end = start + token.len();
        let left_ok = start == 0 || !bytes[start - 1].is_ascii_alphanumeric();
        let right_ok = end == bytes.len() || !bytes[end].is_ascii_alphanumeric();
        if left_ok && right_ok {
            return true;
        }
    }
    false
}

fn is_zowie_product(product: &str) -> bool {
    const ZOWIE_MODELS: &[&str] = &[
        "ec1-cw", "ec2-cw", "ec3-cw", "ec1-dw", "ec2-dw", "ec3-dw", "fk2-dw", "za13-dw", "s2-dw",
        "u2-dw", "u2",
    ];
    let lower = product.to_ascii_lowercase();
    lower.contains("zowie")
        || ZOWIE_MODELS
            .iter()
            .any(|model| has_product_token(&lower, model))
}

pub fn is_zowie_device(vid: u16, pid: u16, product: &str) -> bool {
    match vid {
        ZOWIE_BENQ_VID => true,
        ZOWIE_ENHANCED_RX_VID => matches!(pid, 0x0907 | 0x0908) || is_zowie_product(product),
        // 0x04A5 is BenQ's corporate VID (monitors, ScreenBar, etc.): only explicit ZOWIE
        // branding or token-bounded ZOWIE wireless mouse models may match.
        ZOWIE_BENQ_RX_VID => is_zowie_product(product),
        _ => false,
    }
}

/// ZOWIE battery exchanges.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ZowieProtocol {
    /// 0x3057 / 0x1EA7 receivers: `05 01 02` -> `05 01 02 <level> <charging>` or the
    /// 4-step `05 02 <step> <charging>` indicator.
    Legacy,
    /// 0x04A5 EC-CW receivers: output `00 A5 03` -> `A5 03 <level> <charging>`.
    EcCw,
    /// 0x04A5 U2 / *-DW receivers: feature `08 B4 15`, answered as input report `09 B4 15
    /// <level> <charging>` on the receiver's `0x09` collection.
    U2Dw,
}

pub fn zowie_protocol(vid: u16, pid: u16, product: &str) -> Option<ZowieProtocol> {
    if !is_zowie_device(vid, pid, product) {
        return None;
    }
    if vid != ZOWIE_BENQ_RX_VID {
        return Some(ZowieProtocol::Legacy);
    }
    let lower = product.to_ascii_lowercase();
    if has_product_token(&lower, "u2")
        || has_product_token(&lower, "u2-dw")
        || lower.contains("-dw")
        || has_product_token(&lower, "dw")
    {
        Some(ZowieProtocol::U2Dw)
    } else {
        Some(ZowieProtocol::EcCw)
    }
}

pub fn build_zowie_request(report_len: usize) -> Vec<u8> {
    let len = report_len.max(64);
    let mut packet = vec![0u8; len];
    let offset = usize::from(len >= 65);
    packet[offset] = 0x05;
    packet[offset + 1] = 0x01;
    packet[offset + 2] = 0x02;
    packet
}

/// EC-CW (0x04A5): report id `0x00` followed by `A5 03`.
pub fn build_zowie_ec_cw_request(report_len: usize) -> Vec<u8> {
    let mut packet = vec![0u8; report_len.max(64)];
    packet[1] = 0xA5;
    packet[2] = 0x03;
    packet
}

/// U2 / DW (0x04A5): feature report `0x08` carrying `B4 15`.
pub fn build_zowie_dw_feature_request(report_len: usize) -> Vec<u8> {
    let mut packet = vec![0u8; report_len.max(3)];
    packet[0] = 0x08;
    packet[1] = 0xB4;
    packet[2] = 0x15;
    packet
}

fn zowie_payload(raw: &[u8]) -> &[u8] {
    if raw.len() >= 5 && raw[0] == 0x00 && matches!(raw[1], 0x05 | 0xA5) {
        &raw[1..]
    } else {
        raw
    }
}

pub fn is_valid_zowie_reply(raw: &[u8]) -> bool {
    let payload = zowie_payload(raw);
    (payload.len() >= 5 && payload[..3] == [0x05, 0x01, 0x02])
        || (payload.len() >= 4 && payload[..2] == [0x05, 0x02])
        || (payload.len() >= 4 && payload[..2] == [0xA5, 0x03])
        || (payload.len() >= 5 && payload[..3] == [0x09, 0xB4, 0x15])
}

pub fn parse_zowie_reply(
    raw: &[u8],
    vid: u16,
    pid: u16,
    id: &str,
    name: &str,
) -> Option<BatteryReading> {
    if !is_valid_zowie_reply(raw) {
        return None;
    }
    let payload = zowie_payload(raw);
    let percent = |level: u8, charge_flag: u8| -> Option<(u8, bool)> {
        if level > 100 || charge_flag > 1 {
            return None;
        }
        let charging = charge_flag == 1;
        if level == 0 && !charging {
            return None;
        }
        Some((level, charging))
    };
    let (level, charging) = match (payload[0], payload[1]) {
        (0x05, 0x01) => percent(payload[3], payload[4])?,
        (0x05, 0x02) => {
            // 4-step enhanced receiver LED battery indicator (1..=4 -> 25/50/75/100%).
            let step = payload[2];
            let charge_flag = payload[3];
            if !(1..=4).contains(&step) || charge_flag > 1 {
                return None;
            }
            (step * 25, charge_flag == 1)
        }
        (0xA5, 0x03) => percent(payload[2], payload[3])?,
        (0x09, 0xB4) => percent(payload[3], payload[4])?,
        _ => return None,
    };

    let clean_name = if name.trim().is_empty() {
        zowie_model_name(vid, pid)
    } else {
        name.trim()
    };
    let mut reading = BatteryReading::new(
        id,
        clean_name,
        u32::from(level),
        charging,
        BatterySource::CompxAtkVxe,
    );
    reading.device_kind = WirelessDeviceKind::Mouse;
    Some(reading)
}

// ---------------------------------------------------------------------------
// 6. Keychron Wireless Keyboards (VID 0x3434, Raw HID 0xFF60 / 0xFFC1)
// ---------------------------------------------------------------------------
//
// There is deliberately no Keychron raw-HID battery *probe*: `07 xx` on the VIA raw-HID
// channel is VIA's custom *set*-value command, so writing it to a keyboard could change a
// setting. Keychron batteries come from the BLE Battery Service (BLE mode) or the Standard
// HID battery usages (2.4 GHz receiver); this parser only decodes the Compx/Nordic battery
// frame a Keychron receiver may push on its own.

pub fn is_keychron_keyboard(vid: u16, pid: u16, product: &str, usage_page: u16) -> bool {
    if vid != KEYCHRON_VID {
        return false;
    }
    let lower = product.to_ascii_lowercase();
    if lower.contains("mouse") || lower.contains("keychron m") || (pid & 0xFF00) == 0xD000 {
        return false;
    }
    matches!(
        usage_page,
        KEYCHRON_RAW_HID_USAGE_PAGE | KEYCHRON_VENDOR_USAGE_PAGE
    ) && ((0x0100..=0x0FFF).contains(&pid)
        || lower.contains("keychron")
        || lower.contains("lemokey")
        || lower.contains("keyboard"))
}

pub fn keychron_model_name(pid: u16, product: &str) -> &'static str {
    if !product.trim().is_empty() {
        return "Keychron Wireless Keyboard";
    }
    match pid & 0xFF00 {
        0x0100..=0x02FF => "Keychron K Pro Wireless",
        0x0300..=0x04FF => "Keychron K Max Wireless",
        0x0500..=0x06FF => "Keychron Q Pro Wireless",
        0x0700..=0x08FF => "Keychron Q Max Wireless",
        0x0900..=0x0AFF => "Keychron V Max Wireless",
        0xD000 => "Keychron Wireless Mouse",
        _ => "Keychron Wireless Keyboard",
    }
}

/// Decodes only the Compx/Nordic battery frame (`08 04 ...` / `04 ...`) a Keychron 2.4 GHz
/// receiver reports. The VIA `07 01` raw-HID form is intentionally not parsed (and never
/// requested): `07` is VIA's custom *set*-value command.
pub fn parse_keychron_raw_hid_reply(
    raw: &[u8],
    pid: u16,
    id: &str,
    name: &str,
) -> Option<BatteryReading> {
    let clean_name = if name.trim().is_empty() {
        keychron_model_name(pid, name)
    } else {
        name.trim()
    };
    let mut reading = parse_compx_nordic(raw, id, clean_name)?;
    reading.source = BatterySource::Keyboard24gHid;
    reading.device_kind = WirelessDeviceKind::Keyboard;
    Some(reading)
}

// ---------------------------------------------------------------------------
// 7. Compx / ATK / VXE / MCHOSE / Lamzu / WLMouse / Scyrox model name lookup
// ---------------------------------------------------------------------------

pub fn compx_default_model_name(vendor_id: u16, product_id: u16) -> &'static str {
    match (vendor_id, product_id) {
        (0x373B, 0x1119) => "ATK A9 Ultra",
        (0x373B, 0x1101) => "ATK F1 Pro Max",
        (0x373B, 0x1102) => "ATK X1 Ultra",
        (0x373B, 0x1108) => "ATK Z1 Ultra",
        (0x373B, 0x10F0) => "ATK 1K Wireless Mouse",
        (0x373B, _) => "ATK Wireless Mouse",
        (0x36A7, _) => "MCHOSE Wireless Mouse",
        (0x36B0, _) => "Scyrox Wireless Mouse",
        (0x3710, _) => "WLMouse Beast X Wireless",
        (0x372E, _) => "Lamzu Wireless Mouse",
        (0x359B, _) => "Ninjutso Sora Wireless",
        (0x248A, _) => "Ajazz / Attack Shark Wireless Mouse",
        (0x3151, _) => "Darmoshark / Zaopin Wireless Mouse",
        (0x373E, _) => "Lamzu / Pulsar 8K Wireless Mouse",
        (0x3434, _) => "Keychron Wireless Mouse",
        (0x260D, _) => "Dareu Wireless Mouse",
        (0x24AE, _) => "Rapoo Wireless Mouse",
        (0x3554, 0xF58A) => "VXE Dragonfly R1 Pro",
        (0x3554, 0xF58B) => "VXE MAD R Major",
        (0x3554, 0xF505) => "VGN Dragonfly F1 MOBA",
        _ => "VXE Wireless Mouse",
    }
}

pub fn keyboard_24g_default_name(vendor_id: u16, product_id: u16) -> &'static str {
    match (vendor_id, product_id) {
        (0x258A, 0x010C) => "AULA F75 Wireless Keyboard",
        (0x258A, 0x013C) => "AULA F99 Wireless Keyboard",
        (0x258A, 0x0152) => "Weikav NUT75 Wireless Keyboard",
        (0x258A, 0x00B5) => "Royal Kludge Wireless Keyboard",
        (0x3554, 0xFA09) => "2.4G Wireless Mechanical Keyboard",
        (0x3151, 0x4011) => "MonsGeek Wireless Keyboard",
        (0x3151, 0x4015) => "Akko Wireless Keyboard",
        (KEYCHRON_VID, _) => keychron_model_name(product_id, ""),
        (GLORIOUS_VID_GMMK, _) => {
            glorious_model_name(vendor_id, product_id).unwrap_or("Glorious GMMK Wireless")
        }
        _ => "2.4G Wireless Keyboard",
    }
}

// ---------------------------------------------------------------------------
// 8. Bluetooth PnP name cleanup + WirelessDeviceKind classification
// ---------------------------------------------------------------------------

/// Strips Windows Bluetooth audio transport suffixes (`Hands-Free AG Audio`,
/// `Stereo`, `Avrcp Transport`, etc.) so PnP readings show clean device names.
pub fn clean_bluetooth_device_name(raw: &str) -> String {
    let text = raw.trim();
    for suffix in [
        " Hands-Free AG Audio",
        " Hands-Free HF Audio",
        " Hands-Free Audio",
        " Hands-Free",
        " Avrcp Transport",
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

/// Classifies a wireless peripheral into a true [`WirelessDeviceKind`] using
/// exact VID/PID tables first, then brand/model heuristics, Bluetooth service
/// UUIDs, and finally provider source fallback.
pub fn classify_wireless_device_kind(
    vid: u16,
    pid: u16,
    name: &str,
    source: BatterySource,
    instance_or_id: &str,
) -> WirelessDeviceKind {
    // 1. Exact VID/PID hardware tables.
    if vid != 0 && pid != 0 {
        if let Some(profile) = hyperx_profile(vid, pid) {
            return profile.device_kind;
        }
        match (vid, pid) {
            // Sony DualSense / DualShock 4 & Nintendo Switch Pro / Joy-Con
            (0x054C, 0x0CE6 | 0x0DF2 | 0x05C4 | 0x09CC) | (0x057E, 0x2009 | 0x2006 | 0x2007) => {
                return WirelessDeviceKind::Gamepad;
            }
            // SteelSeries Apex Pro Wireless keyboards
            (0x1038, 0x1624 | 0x1626 | 0x162C | 0x162E | 0x1630) => {
                return WirelessDeviceKind::Keyboard;
            }
            // SteelSeries Aerox / Prime / Rival wireless mice
            (
                0x1038,
                0x1838 | 0x183A | 0x1852 | 0x1854 | 0x1858 | 0x185A | 0x185E | 0x1860 | 0x1842
                | 0x1848 | 0x1830 | 0x1832 | 0x172B,
            ) => return WirelessDeviceKind::Mouse,
            // SteelSeries Arctis / Nova headsets
            (
                0x1038,
                0x12E0 | 0x12E2 | 0x12E5 | 0x12E6 | 0x2202 | 0x2204 | 0x2206 | 0x2208 | 0x220A
                | 0x223A | 0x2258 | 0x225A | 0x227E | 0x2266 | 0x226A | 0x220E | 0x2212 | 0x2216
                | 0x2232 | 0x2234 | 0x2236 | 0x223E | 0x2240 | 0x2242 | 0x12B3 | 0x12B6 | 0x1260
                | 0x12AD | 0x12D5 | 0x12D7 | 0x12C2 | 0x1290,
            ) => return WirelessDeviceKind::Headphones,
            // Logitech Headsets
            (0x046D, 0x0A87 | 0x0A90 | 0x0AB5 | 0x0ABA | 0x0AC4 | 0x0AD0 | 0x0AF7 | 0x0AFE) => {
                return WirelessDeviceKind::Headphones;
            }
            // Logitech Keyboards
            (0x046D, 0xC33E | 0xC343 | 0xC34A | 0xC354 | 0xB35B | 0xB363 | 0xB366 | 0xB378) => {
                return WirelessDeviceKind::Keyboard;
            }
            // Logitech Mice
            (
                0x046D,
                0xC088 | 0xC08B | 0xC08D | 0xC094 | 0xC096 | 0xC09B | 0xC09C | 0xB023 | 0xB028
                | 0xB034 | 0xB037,
            ) => return WirelessDeviceKind::Mouse,
            // 2.4G keyboards (AULA, Weikav, Royal Kludge, Compx FA09, MonsGeek, Akko)
            (0x258A, 0x010C | 0x013C | 0x0152 | 0x00B5)
            | (0x3554, 0xFA09)
            | (0x3151, 0x4011 | 0x4015) => return WirelessDeviceKind::Keyboard,
            // Glorious GMMK keyboards vs mice
            (GLORIOUS_VID_GMMK, 0x5044 | 0x5046 | 0x5088 | 0x5092) => {
                return WirelessDeviceKind::Keyboard;
            }
            (GLORIOUS_VID_SINOWEALTH, 0x2011..=0x2022)
            | (GLORIOUS_VID_PIXART, 0x821A | 0x822A | 0x823A | 0x824A) => {
                return WirelessDeviceKind::Mouse;
            }
            // Endgame Gear, Finalmouse, Zowie
            (ENDGAME_GEAR_VID, _) | (FINALMOUSE_VID, _) | (ZOWIE_BENQ_VID, _) => {
                return WirelessDeviceKind::Mouse;
            }
            (ZOWIE_ENHANCED_RX_VID, 0x0907 | 0x0908) => return WirelessDeviceKind::Mouse,
            _ => {}
        }
    }

    // 2. Name-based model classification (order resolves ambiguous strings like "G PRO X SUPERLIGHT").
    let lower = name.to_ascii_lowercase();
    let has = |words: &[&str]| words.iter().any(|w| lower.contains(w));

    // Check explicit mouse models first when they share brand tokens with headsets ("G PRO X SUPERLIGHT", "Keychron M3", "Pulsefire").
    if has(&[
        "superlight",
        "keychron m",
        "pulsefire",
        "model o",
        "model d",
        "model i",
        "op1we",
        "op1w",
        "xm2we",
        "xm2w",
        "ultralightx",
        "starlight",
        "ec1-cw",
        "ec2-cw",
        "ec3-cw",
        "ec2-dw",
        "fk2-dw",
        "za13-dw",
        "s2-dw",
        "zowie",
    ]) {
        return WirelessDeviceKind::Mouse;
    }

    // Gamepads
    if has(&[
        "dualsense",
        "dualshock",
        "xbox",
        "switch pro",
        "joy-con",
        "gamepad",
        "controller",
        "joystick",
        "wolverine",
        "raikiri",
        "8bitdo",
        "scuf",
    ]) {
        return WirelessDeviceKind::Gamepad;
    }

    // Headphones & earbuds (gaming headsets + mainstream BT audio)
    if has(&[
        "airpods",
        "beats",
        "wh-1000",
        "wf-1000",
        "xm4",
        "xm5",
        "xm3",
        "linkbuds",
        "inzone",
        "bose",
        "quietcomfort",
        "qc35",
        "qc45",
        "sennheiser",
        "momentum",
        "jabra",
        "evolve",
        "soundcore",
        "anker",
        "galaxy buds",
        "pixel buds",
        "nothing ear",
        "arctis",
        "nova pro",
        "nova 7",
        "nova 5",
        "nova 3",
        "cloud ii",
        "cloud iii",
        "cloud alpha",
        "cloud flight",
        "cloud stinger",
        "cloud mini",
        "blackshark",
        "barracuda",
        "kraken",
        "kaira",
        "nari",
        "hs80",
        "hs70",
        "hs65",
        "hs55",
        "virtuoso",
        "void ",
        "delta s",
        "cetra",
        "pelta",
        "strix go",
        "g733",
        "g535",
        "g435",
        "g935",
        "pro x 2",
        "pro x wireless",
        "astro a",
        "zone wireless",
        "hands-free",
        "headset",
        "headphone",
        "earbud",
        "earphone",
        "buds",
        "audio gateway",
        "kulaklık",
        "kulaklik",
    ]) {
        return WirelessDeviceKind::Headphones;
    }

    // Keyboards
    if has(&[
        "keyboard",
        "klavye",
        "keypad",
        "mx keys",
        "mx mechanical",
        "g915",
        "g715",
        "g613",
        "apex pro",
        "blackwidow",
        "deathstalker",
        "huntsman",
        "pro type",
        "azoth",
        "falchion",
        "strix scope",
        "claymore",
        "k65",
        "k70",
        "k100",
        "aula",
        "f75",
        "f87",
        "f99",
        "nut75",
        "lucky65",
        "royal kludge",
        "rk61",
        "rk68",
        "rk84",
        "keychron k",
        "keychron q",
        "keychron v",
        "lemokey",
        "monsgeek",
        "akko",
        "gmmk",
        "k99",
        "g75",
        "v75",
        "rs7",
        "wooting",
        "nuphy",
        "rainy75",
    ]) {
        return WirelessDeviceKind::Keyboard;
    }

    // Mice
    if has(&[
        "mouse",
        "fare",
        "trackball",
        "g pro",
        "mx master",
        "mx anywhere",
        "mx vertical",
        "g502",
        "g305",
        "g309",
        "g703",
        "g903",
        "viper",
        "deathadder",
        "basilisk",
        "naga",
        "orochi",
        "cobra",
        "aerox",
        "rival",
        "prime wireless",
        "prime mini",
        "harpe",
        "keris",
        "gladius",
        "chakram",
        "spatha",
        "dark core",
        "harpoon",
        "nightsabre",
        "katar",
        "scimitar",
        "m75",
        "dragonfly",
        "mad r",
        "atk ",
        "vxe ",
        "vgn ",
        "mchose",
        "scyrox",
        "wlmouse",
        "beast x",
        "sword x",
        "lamzu",
        "atlantis",
        "maya",
        "thorn",
        "paro",
        "ninjutso",
        "sora",
        "ajazz",
        "attack shark",
        "darmoshark",
        "zaopin",
        "incott",
        "dareu",
        "rapoo",
        "pulsar",
        "x2v2",
        "x2h",
        "xlite",
    ]) {
        return WirelessDeviceKind::Mouse;
    }

    // 3. Bluetooth HFP/A2DP/AVRCP service UUID fallback.
    let id_lower = instance_or_id.to_ascii_lowercase();
    if ["0000111e", "0000110b", "0000110d", "0000110e"]
        .iter()
        .any(|uuid| id_lower.contains(uuid))
    {
        return WirelessDeviceKind::Headphones;
    }

    // 4. Source fallback.
    match source {
        BatterySource::GamepadHid | BatterySource::XInput => WirelessDeviceKind::Gamepad,
        BatterySource::Keyboard24gHid => WirelessDeviceKind::Keyboard,
        BatterySource::CompxAtkVxe | BatterySource::RazerHid => WirelessDeviceKind::Mouse,
        BatterySource::SteelSeriesHid | BatterySource::SteelSeriesGg | BatterySource::HyperXHid => {
            WirelessDeviceKind::Headphones
        }
        _ => WirelessDeviceKind::Other,
    }
}

/// Maps a USB Vendor ID to its canonical lowercase vendor token used by
/// `DockSettings::wireless_disabled_vendors()`.
pub fn vendor_token_for_vid(vid: u16) -> Option<&'static str> {
    match vid {
        0x046D => Some("logitech"),
        0x1532 => Some("razer"),
        0x1038 => Some("steelseries"),
        0x0B05 => Some("asus"),
        0x1B1C => Some("corsair"),
        HP_HYPERX_VID | KINGSTON_HYPERX_VID => Some("hyperx"),
        0x3554 => Some("compx"),
        0x373B => Some("atk"),
        0x36A7 => Some("mchose"),
        0x36B0 => Some("scyrox"),
        0x3710 => Some("wlmouse"),
        0x372E | 0x373E => Some("lamzu"),
        0x359B => Some("ninjutso"),
        0x248A => Some("ajazz"),
        0x3151 => Some("darmoshark"),
        KEYCHRON_VID => Some("keychron"),
        0x260D => Some("dareu"),
        0x24AE => Some("rapoo"),
        GLORIOUS_VID_GMMK | GLORIOUS_VID_PIXART => Some("glorious"),
        ENDGAME_GEAR_VID => Some("endgame"),
        FINALMOUSE_VID => Some("finalmouse"),
        ZOWIE_BENQ_VID | ZOWIE_ENHANCED_RX_VID | ZOWIE_BENQ_RX_VID => Some("zowie"),
        0x054C => Some("sony"),
        0x057E => Some("nintendo"),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hyperx_all_three_verified_families_parse_cleanly() {
        // 1. HpCloudHeadset64 (Stinger 2 / Mini / II Core)
        let profile = hyperx_profile(HP_HYPERX_VID, 0x0D93).expect("Stinger 2 profile");
        assert_eq!(profile.protocol, HyperXDeviceProtocol::HpCloudHeadset64);
        assert_eq!(profile.device_kind, WirelessDeviceKind::Headphones);
        let req = build_hyperx_protocol_request(profile.protocol, 64);
        assert_eq!(&req[..2], &[0x66, 0x89]);
        let mut reply = vec![0u8; 64];
        reply[0] = 0x66;
        reply[1] = 0x89;
        reply[4] = 84;
        reply[5] = 1;
        let parsed =
            parse_hyperx_protocol(profile.protocol, &reply, "hx:1", "", profile.device_kind)
                .expect("HpCloudHeadset64 reply");
        assert_eq!(parsed.level, 84);
        assert!(parsed.charging);
        assert_eq!(parsed.device_kind, WirelessDeviceKind::Headphones);
        assert_eq!(parsed.source, BatterySource::HyperXHid);

        // 2. KingstonCloud20 (Kingston Cloud II 0x0951:0x1718)
        let k_profile = hyperx_profile(KINGSTON_HYPERX_VID, 0x1718).expect("Kingston Cloud II");
        assert_eq!(k_profile.protocol, HyperXDeviceProtocol::KingstonCloud20);
        let k_req = build_hyperx_protocol_request(k_profile.protocol, 20);
        assert_eq!(&k_req[..3], &[0x21, 0xFF, 0x05]);
        let mut k_reply = vec![0u8; 20];
        k_reply[0] = 0x21;
        k_reply[1] = 0xFF;
        k_reply[2] = 0x05;
        k_reply[4] = 0;
        k_reply[7] = 63;
        let k_parsed = parse_hyperx_protocol(
            k_profile.protocol,
            &k_reply,
            "hx:2",
            "HyperX Cloud II Wireless",
            k_profile.device_kind,
        )
        .expect("KingstonCloud20 reply");
        assert_eq!(k_parsed.level, 63);
        assert!(!k_parsed.charging);

        // Fallback to byte 3 when byte 7 is 0
        k_reply[7] = 0;
        k_reply[3] = 48;
        k_reply[4] = 1;
        let k_fallback = parse_hyperx_protocol(
            k_profile.protocol,
            &k_reply,
            "hx:2b",
            "HyperX Cloud II",
            k_profile.device_kind,
        )
        .expect("KingstonCloud20 byte 3 fallback");
        assert_eq!(k_fallback.level, 48);
        assert!(k_fallback.charging);

        // 3. PulsefireMouse64 (Haste 2 / Dart) with leading 0x00 report ID framing
        let m_profile = hyperx_profile(HP_HYPERX_VID, 0x02B8).expect("Pulsefire Haste 2");
        assert_eq!(m_profile.protocol, HyperXDeviceProtocol::PulsefireMouse64);
        assert_eq!(m_profile.device_kind, WirelessDeviceKind::Mouse);
        let m_req = build_hyperx_protocol_request(m_profile.protocol, 64);
        assert_eq!(&m_req[..2], &[0x04, 0x03]);
        let mut m_reply = vec![0u8; 65];
        m_reply[0] = 0x00;
        m_reply[1] = 0x04;
        m_reply[2] = 0x03;
        m_reply[5] = 91;
        m_reply[6] = 0;
        let m_parsed = parse_hyperx_protocol(
            m_profile.protocol,
            &m_reply,
            "hx:3",
            "Pulsefire Haste 2",
            m_profile.device_kind,
        )
        .expect("PulsefireMouse64 reply");
        assert_eq!(m_parsed.level, 91);
        assert_eq!(m_parsed.device_kind, WirelessDeviceKind::Mouse);

        // Reject invalid >100% or sleeping 0% non-charging
        m_reply[5] = 0;
        assert!(
            parse_hyperx_protocol(
                m_profile.protocol,
                &m_reply,
                "hx:3",
                "",
                m_profile.device_kind
            )
            .is_none()
        );
    }

    #[test]
    fn hyperx_exact_pid_protocols_are_separated() {
        let pid = |vid, pid| hyperx_profile(vid, pid).map(|p| p.protocol);
        assert_eq!(
            pid(HP_HYPERX_VID, 0x018B),
            Some(HyperXDeviceProtocol::HpCloudII)
        );
        assert_eq!(
            pid(KINGSTON_HYPERX_VID, 0x1718),
            Some(HyperXDeviceProtocol::KingstonCloud20)
        );
        assert_eq!(
            pid(HP_HYPERX_VID, 0x098D),
            Some(HyperXDeviceProtocol::CloudAlpha)
        );
        assert_eq!(
            pid(KINGSTON_HYPERX_VID, 0x16C4),
            Some(HyperXDeviceProtocol::CloudFlightVoltage)
        );
        assert_eq!(
            pid(HP_HYPERX_VID, 0x05B7),
            Some(HyperXDeviceProtocol::HpCloudIII)
        );
        for p in [0x02CC, 0x06BE, 0x0D97, 0x0F98] {
            assert_eq!(
                pid(HP_HYPERX_VID, p),
                Some(HyperXDeviceProtocol::HpVendor50)
            );
        }

        // HP Cloud II: 06 FF BB 02 level (byte 7) + 06 FF BB 03 charge state (byte 4).
        let cloud2 = HyperXDeviceProtocol::HpCloudII;
        assert_eq!(
            &build_hyperx_protocol_request(cloud2, 20)[..4],
            &[0x06, 0xFF, 0xBB, 0x02]
        );
        assert_eq!(
            &build_hyperx_charging_request(cloud2, 20).unwrap()[..4],
            &[0x06, 0xFF, 0xBB, 0x03]
        );
        let mut reply = vec![0u8; 20];
        reply[..4].copy_from_slice(&[0x06, 0xFF, 0xBB, 0x02]);
        reply[7] = 66;
        let r = parse_hyperx_protocol(cloud2, &reply, "hx", "", WirelessDeviceKind::Headphones)
            .expect("HP Cloud II level");
        assert_eq!(r.level, 66);
        let mut charge = vec![0u8; 20];
        charge[..4].copy_from_slice(&[0x06, 0xFF, 0xBB, 0x03]);
        charge[4] = 1;
        assert_eq!(parse_hyperx_charging_reply(cloud2, &charge), Some(true));
        assert!(
            parse_hyperx_protocol(cloud2, &charge, "hx", "", WirelessDeviceKind::Headphones)
                .is_none(),
            "a charge-state reply is never a level"
        );

        // Cloud Alpha: 21 BB 0B -> level byte 3; 21 BB 0C -> charge byte 3.
        let alpha = HyperXDeviceProtocol::CloudAlpha;
        let mut a = vec![0u8; 20];
        a[..3].copy_from_slice(&[0x21, 0xBB, 0x0B]);
        a[3] = 37;
        assert_eq!(
            parse_hyperx_protocol(alpha, &a, "hx", "", WirelessDeviceKind::Headphones)
                .unwrap()
                .level,
            37
        );
        a[2] = 0x0C;
        a[3] = 1;
        assert_eq!(parse_hyperx_charging_reply(alpha, &a), Some(true));

        // Cloud III: 66 0D <level>; charge state 66 8A / 66 0C <state>.
        let cloud3 = HyperXDeviceProtocol::HpCloudIII;
        let mut c3 = vec![0u8; 64];
        c3[..3].copy_from_slice(&[0x66, 0x0D, 58]);
        assert_eq!(
            parse_hyperx_protocol(cloud3, &c3, "hx", "", WirelessDeviceKind::Headphones)
                .unwrap()
                .level,
            58
        );
        c3[..3].copy_from_slice(&[0x66, 0x0C, 0x01]);
        assert_eq!(parse_hyperx_charging_reply(cloud3, &c3), Some(true));

        // Cloud III S / Haste 2: 50 02 -> 51 02 <level> <charging>.
        let v50 = HyperXDeviceProtocol::HpVendor50;
        assert_eq!(&build_hyperx_protocol_request(v50, 64)[..2], &[0x50, 0x02]);
        let mut h2 = vec![0u8; 64];
        h2[..4].copy_from_slice(&[0x51, 0x02, 0, 0]);
        let empty = parse_hyperx_protocol(v50, &h2, "hx", "", WirelessDeviceKind::Mouse)
            .expect("0% is a valid level");
        assert_eq!(empty.level, 0);
        h2[0] = 0x50; // our own request echoed back is not an answer
        assert!(!is_valid_hyperx_protocol_reply(v50, &h2));
    }

    #[test]
    fn hyperx_cloud_flight_big_endian_millivolt_curve() {
        let flight = HyperXDeviceProtocol::CloudFlightVoltage;
        let mut reply = vec![0u8; 20];
        reply[..3].copy_from_slice(&[0x21, 0xFF, 0x05]);
        reply[3..5].copy_from_slice(&3800u16.to_be_bytes());
        let r = parse_hyperx_protocol(flight, &reply, "hx", "", WirelessDeviceKind::Headphones)
            .expect("Flight mV");
        assert_eq!(r.level, 60);
        assert_eq!(r.voltage_mv, Some(3800));
        assert!(!r.charging);
        reply[3..5].copy_from_slice(&4150u16.to_be_bytes()); // 0x1036: charger-raised
        let r = parse_hyperx_protocol(flight, &reply, "hx", "", WirelessDeviceKind::Headphones)
            .unwrap();
        assert_eq!(r.level, 100);
        assert!(r.charging);
        assert_eq!(hyperx_flight_percent_from_mv(3200), 0);
        assert_eq!(hyperx_flight_percent_from_mv(3650), 20);
        reply[3..5].copy_from_slice(&5000u16.to_be_bytes());
        assert!(
            parse_hyperx_protocol(flight, &reply, "hx", "", WirelessDeviceKind::Headphones)
                .is_none()
        );
    }

    #[test]
    fn glorious_mouse_and_gmmk_feature_reports_parse() {
        assert!(is_glorious_device(GLORIOUS_VID_SINOWEALTH, 0x2016, ""));
        assert!(!is_glorious_device(
            GLORIOUS_VID_SINOWEALTH,
            0x010C,
            "AULA F75"
        ));
        assert_eq!(
            glorious_model_name(GLORIOUS_VID_SINOWEALTH, 0x2022),
            Some("Glorious Model O Wireless")
        );
        let req = build_glorious_request(65);
        assert_eq!(req.len(), 65);
        assert_eq!(&req[..7], &[0x00, 0x00, 0x00, 0x02, 0x02, 0x00, 0x83]);

        let mut reply = vec![0u8; 65];
        reply[3] = 0x02;
        reply[4] = 0x02;
        reply[6] = 0x83; // battery function echo
        reply[7] = 0x01; // charging
        reply[8] = 74;
        let parsed = parse_glorious_reply(&reply, GLORIOUS_VID_SINOWEALTH, 0x2016, "gl:1", "")
            .expect("Glorious Model O 2 reply");
        assert_eq!(parsed.name, "Glorious Model O 2 Wireless");
        assert_eq!(parsed.level, 74);
        assert!(parsed.charging);
        assert_eq!(parsed.device_kind, WirelessDeviceKind::Mouse);

        // Asleep mouse (0xA1) and a reply to another function must be rejected.
        reply[1] = 0xA1;
        assert!(
            parse_glorious_reply(&reply, GLORIOUS_VID_SINOWEALTH, 0x2016, "gl:1", "").is_none()
        );
        reply[1] = 0x00;
        reply[6] = 0x82;
        assert!(
            parse_glorious_reply(&reply, GLORIOUS_VID_SINOWEALTH, 0x2016, "gl:1", "").is_none()
        );

        // GMMK Wireless keyboard reply
        let mut gmmk = vec![0u8; 65];
        gmmk[1] = 0x04;
        gmmk[2] = 0x02;
        gmmk[3] = 88;
        gmmk[4] = 0;
        let kbd = parse_glorious_reply(&gmmk, GLORIOUS_VID_GMMK, 0x5088, "gl:kbd", "")
            .expect("GMMK 3 reply");
        assert_eq!(kbd.device_kind, WirelessDeviceKind::Keyboard);
        assert_eq!(kbd.source, BatterySource::Keyboard24gHid);
        assert_eq!(kbd.level, 88);
    }

    #[test]
    fn endgame_gear_finalmouse_and_zowie_parsers() {
        // Endgame Gear native reply + model lookup
        assert_eq!(endgame_gear_model_name(0x1954), "Endgame Gear OP1we");
        assert_eq!(endgame_gear_model_name(0x1964), "Endgame Gear XM2w 4k");
        let mut egg = vec![0u8; 65];
        egg[1] = 0x04;
        egg[2] = 0x02;
        egg[3] = 0x01; // charging
        egg[4] = 66;
        egg[5] = 0x0F;
        egg[6] = 0x3C; // 3900 mV
        let egg_r = parse_endgame_gear_reply(&egg, 0x1964, "egg:1", "").expect("EGG reply");
        assert_eq!(egg_r.name, "Endgame Gear XM2w 4k");
        assert_eq!(egg_r.level, 66);
        assert!(egg_r.charging);
        assert_eq!(egg_r.voltage_mv, Some(3900));
        assert_eq!(egg_r.device_kind, WirelessDeviceKind::Mouse);

        // Finalmouse ULX receiver: 04 command -> 05 answer
        assert_eq!(finalmouse_model_name(0x0100), "Finalmouse UltralightX");
        let req = build_finalmouse_request(64);
        assert_eq!(&req[..2], &[0x04, 0x02]);
        assert!(
            !is_valid_finalmouse_reply(&req),
            "bare request echo is not an answer"
        );
        let mut fm = vec![0u8; 64];
        fm[0] = 0x05;
        fm[1] = 0x02;
        fm[2] = 92;
        fm[3] = 0;
        fm[4] = 0x10;
        fm[5] = 0x04; // 4100 mV
        let fm_r = parse_finalmouse_reply(&fm, 0x0100, "fm:1", "").expect("Finalmouse reply");
        assert_eq!(fm_r.name, "Finalmouse UltralightX");
        assert_eq!(fm_r.level, 92);
        assert!(!fm_r.charging);
        assert_eq!(fm_r.voltage_mv, Some(4100));
        assert_eq!(fm_r.device_kind, WirelessDeviceKind::Mouse);

        // Zowie / BenQ percentage + 4-step enhanced receiver replies
        assert!(is_zowie_device(ZOWIE_BENQ_VID, 0x0210, ""));
        assert!(is_zowie_device(ZOWIE_ENHANCED_RX_VID, 0x0907, ""));
        assert!(!is_zowie_device(
            ZOWIE_ENHANCED_RX_VID,
            0x1234,
            "Generic Mouse"
        ));
        let mut zowie = vec![0u8; 64];
        zowie[0] = 0x05;
        zowie[1] = 0x01;
        zowie[2] = 0x02;
        zowie[3] = 79;
        zowie[4] = 0;
        let z_r = parse_zowie_reply(&zowie, ZOWIE_BENQ_VID, 0x0212, "zw:1", "").expect("Zowie pct");
        assert_eq!(z_r.name, "ZOWIE FK2-DW");
        assert_eq!(z_r.level, 79);
        assert_eq!(z_r.device_kind, WirelessDeviceKind::Mouse);

        let zowie_step = [0x00, 0x05, 0x02, 3, 1, 0];
        let z_step = parse_zowie_reply(&zowie_step, ZOWIE_ENHANCED_RX_VID, 0x0907, "zw:2", "")
            .expect("Zowie 4-step");
        assert_eq!(z_step.level, 75);
        assert!(z_step.charging);
    }

    #[test]
    fn endgame_gear_xm2we_pairs_feature_08_with_input_09() {
        let request = build_endgame_gear_feature_request(0x04, 17);
        assert_eq!(&request[..2], &[0x08, 0x04]);
        assert_eq!(request[16], 0x49, "Compx checksum 0x4D - 0x04");
        let mut answer = [0u8; 17];
        answer[0] = 0x09;
        answer[1] = 0x04;
        answer[6] = 70;
        answer[7] = 0;
        answer[8..10].copy_from_slice(&3850u16.to_be_bytes());
        answer[16] = compx_checksum(&answer);
        assert!(is_endgame_gear_input_frame(&answer, 0x04));
        assert!(!is_endgame_gear_input_frame(&answer, 0x03));
        let r = parse_endgame_gear_battery_input(&answer, 0x194C, "egg", "").expect("XM2we");
        assert_eq!(r.level, 70);
        assert_eq!(r.name, "Endgame Gear XM2we");
        assert_eq!(r.voltage_mv, Some(3850));
        answer[16] ^= 0xFF;
        assert!(parse_endgame_gear_battery_input(&answer, 0x194C, "egg", "").is_none());

        let mut link = [0u8; 17];
        link[0] = 0x09;
        link[1] = 0x03;
        link[16] = compx_checksum(&link);
        assert_eq!(parse_endgame_gear_link_input(&link), Some(false));
        link[6] = 1;
        link[16] = compx_checksum(&link);
        assert_eq!(parse_endgame_gear_link_input(&link), Some(true));
    }

    #[test]
    fn zowie_04a5_receivers_use_their_documented_exchanges() {
        assert!(!is_zowie_device(ZOWIE_BENQ_RX_VID, 0x0001, "USB Hub"));
        assert_eq!(zowie_protocol(ZOWIE_BENQ_RX_VID, 0x0001, "USB Hub"), None);
        assert!(
            !is_zowie_device(ZOWIE_BENQ_RX_VID, 0x1001, "BenQ ScreenBar Halo"),
            "BenQ ScreenBar must never be probed as ZOWIE"
        );
        assert!(
            !is_zowie_device(ZOWIE_BENQ_RX_VID, 0x1002, "BenQ PD2705U"),
            "BenQ monitors must never be probed as ZOWIE"
        );
        assert!(
            !is_zowie_device(ZOWIE_BENQ_RX_VID, 0x1003, "BenQ GW2480U2"),
            "substring 'U2' inside monitor model must never match ZOWIE U2"
        );
        assert!(is_zowie_device(ZOWIE_BENQ_RX_VID, 0x0002, "ZOWIE U2"));
        assert!(is_zowie_device(ZOWIE_BENQ_RX_VID, 0x0001, "ZOWIE EC2-CW"));
        assert!(is_zowie_device(
            ZOWIE_BENQ_RX_VID,
            0x0003,
            "BenQ ZOWIE FK2-DW"
        ));
        assert!(is_zowie_device(ZOWIE_BENQ_RX_VID, 0x0004, "EC1-DW"));
        assert!(is_zowie_device(ZOWIE_BENQ_RX_VID, 0x0005, "U2-DW Receiver"));
        assert_eq!(
            zowie_protocol(ZOWIE_BENQ_RX_VID, 0x0001, "ZOWIE EC2-CW Receiver"),
            Some(ZowieProtocol::EcCw)
        );
        assert_eq!(
            zowie_protocol(ZOWIE_BENQ_RX_VID, 0x0002, "ZOWIE U2 Receiver"),
            Some(ZowieProtocol::U2Dw)
        );
        assert_eq!(&build_zowie_ec_cw_request(64)[..3], &[0x00, 0xA5, 0x03]);
        assert_eq!(&build_zowie_dw_feature_request(9)[..3], &[0x08, 0xB4, 0x15]);

        let ec = [0x00, 0xA5, 0x03, 64, 0, 0];
        let r = parse_zowie_reply(&ec, ZOWIE_BENQ_RX_VID, 0x0001, "zw", "").expect("EC-CW");
        assert_eq!(r.level, 64);
        let dw = [0x09, 0xB4, 0x15, 41, 1, 0];
        let r = parse_zowie_reply(&dw, ZOWIE_BENQ_RX_VID, 0x0002, "zw", "").expect("U2/DW");
        assert_eq!(r.level, 41);
        assert!(r.charging);
    }

    #[test]
    fn keychron_via_raw_hid_probe_is_removed() {
        assert!(is_keychron_keyboard(
            KEYCHRON_VID,
            0x0720,
            "Keychron Q1 Max",
            0xFF60
        ));
        assert!(!is_keychron_keyboard(
            KEYCHRON_VID,
            0xD001,
            "Keychron M3 Mouse",
            0xFFC1
        ));
        // `07 01` is VIA's custom set-value command: never decoded as a battery answer.
        let mut via = vec![0u8; 33];
        via[1] = 0x07;
        via[2] = 0x01;
        via[3] = 1;
        via[4] = 81;
        assert!(parse_keychron_raw_hid_reply(&via, 0x0720, "kc:1", "Keychron Q1 Max").is_none());

        // The Compx/Nordic frame a Keychron receiver reports is still decoded.
        let mut nordic = vec![0u8; 64];
        nordic[0] = COMPX_QUERY;
        nordic[5] = 81;
        nordic[6] = 1;
        let parsed = parse_keychron_raw_hid_reply(&nordic, 0x0720, "kc:1", "Keychron Q1 Max")
            .expect("Keychron Nordic frame");
        assert_eq!(parsed.level, 81);
        assert!(parsed.charging);
        assert_eq!(parsed.source, BatterySource::Keyboard24gHid);
        assert_eq!(parsed.device_kind, WirelessDeviceKind::Keyboard);
    }

    const COMPX_QUERY: u8 = 0x04;

    #[test]
    fn wireless_device_kind_classification_covers_all_categories() {
        // Logitech Superlight (Mouse) vs G PRO X 2 (Headphones) vs MX Keys (Keyboard)
        assert_eq!(
            classify_wireless_device_kind(
                0x046D,
                0xC547,
                "Logitech G PRO X SUPERLIGHT 2",
                BatterySource::LogitechHidPlusPlus,
                ""
            ),
            WirelessDeviceKind::Mouse
        );
        assert_eq!(
            classify_wireless_device_kind(
                0x046D,
                0x0AF7,
                "Logitech G PRO X 2 LIGHTSPEED",
                BatterySource::LogitechHidPlusPlus,
                ""
            ),
            WirelessDeviceKind::Headphones
        );
        assert_eq!(
            classify_wireless_device_kind(
                0x046D,
                0xB35B,
                "MX Keys Mini",
                BatterySource::LogitechHidPlusPlus,
                ""
            ),
            WirelessDeviceKind::Keyboard
        );

        // Mainstream Bluetooth headphones
        for headset in [
            "AirPods Pro (2nd generation)",
            "WH-1000XM5",
            "WF-1000XM5",
            "Bose QuietComfort Ultra",
            "Sennheiser MOMENTUM 4",
            "Jabra Evolve2 75",
            "Soundcore Q45",
            "Galaxy Buds2 Pro",
            "Arctis Nova Pro Wireless",
            "Corsair HS80 RGB Wireless",
            "Razer BlackShark V2 Pro",
        ] {
            assert_eq!(
                classify_wireless_device_kind(0, 0, headset, BatterySource::Bluetooth, ""),
                WirelessDeviceKind::Headphones,
                "expected Headphones for {headset}"
            );
        }

        // Gamepads & Keyboards & Mice
        assert_eq!(
            classify_wireless_device_kind(
                0x054C,
                0x0CE6,
                "DualSense",
                BatterySource::GamepadHid,
                ""
            ),
            WirelessDeviceKind::Gamepad
        );
        assert_eq!(
            classify_wireless_device_kind(
                0x1038,
                0x162C,
                "Apex Pro TKL Wireless",
                BatterySource::SteelSeriesHid,
                ""
            ),
            WirelessDeviceKind::Keyboard
        );
        assert_eq!(
            classify_wireless_device_kind(
                0x372E,
                0x1001,
                "Lamzu Maya X",
                BatterySource::CompxAtkVxe,
                ""
            ),
            WirelessDeviceKind::Mouse
        );

        // Bluetooth name suffix cleanup
        assert_eq!(
            clean_bluetooth_device_name("WH-1000XM5 Hands-Free AG Audio"),
            "WH-1000XM5"
        );
        assert_eq!(
            clean_bluetooth_device_name("AirPods Pro Stereo"),
            "AirPods Pro"
        );
    }
}
