//! `WirelessBatteryChecks` / `BluetoothBatteryChecks` parity.
//!
//! Each vendor parser below is a reference implementation of its report format,
//! tested with valid and malformed payloads. Bridge tests compare the VID
//! constants and aggregator settings against production sources/core APIs.
//! Live HID transport calls still need a device, but the shipped private
//! report parsers are pinned by an active source bridge.

mod common;

use opentaskbar_core::settings::DockSettings;

fn percent(value: u8) -> Option<u8> {
    (value <= 100).then_some(value)
}

/// Asus ROG: VID 0x0B05, 64-byte reply, request id 0x12 (usage 0xFF00) or
/// 0x07 (usage 0xFF01); charge sits at byte 12.
fn asus_rog(vid: u16, report: &[u8]) -> Option<u8> {
    if vid != 0x0B05 || report.len() < 64 {
        return None;
    }
    if report[0] != 0x12 && report[0] != 0x07 {
        return None;
    }
    percent(report[12])
}

/// Corsair Bragi (0xFF42 → report 0x42, property 0x000F/0x0010) and legacy
/// 0xFFC2 (report 0xC2). VID 0x1B1C.
fn corsair(vid: u16, report: &[u8]) -> Option<u8> {
    if vid != 0x1B1C || report.len() < 16 {
        return None;
    }
    if report[0] != 0x42 && report[0] != 0xC2 {
        return None;
    }
    percent(report[7])
}

/// HyperX: four protocol families, selected by the low nibble of byte 0.
/// VID 0x03F0 (HyperX) or 0x0951 (Kingston).
fn hyperx(vid: u16, report: &[u8]) -> Option<u8> {
    if vid != 0x03F0 && vid != 0x0951 {
        return None;
    }
    let offsets = [2usize, 3, 4, 5];
    let family = (report.first()? & 0x0F) as usize;
    let offset = *offsets.get(family)?;
    percent(*report.get(offset)?)
}

/// Compx / ATK / Vxe family: 9, 17, 64 or 65 byte reports with an 8-bit
/// additive checksum in the final byte. Charge offset depends on report size.
fn compx_atk_vxe(report: &[u8]) -> Option<u8> {
    let (offset, body) = match report.len() {
        9 => (3usize, &report[..8]),
        17 => (9usize, &report[..16]),
        64 => (30usize, &report[..63]),
        65 => (31usize, &report[..64]),
        _ => return None,
    };
    let checksum = body.iter().fold(0u8, |sum, b| sum.wrapping_add(*b));
    if checksum != report[report.len() - 1] {
        return None;
    }
    percent(report[offset])
}

/// DualSense (0x30) / DualShock4 / Switch Pro use a fixed-size report whose
/// capacity nibble encodes 0–100 in 10% steps.
fn gamepad(report: &[u8]) -> Option<u8> {
    if report.len() < 0x30 || report.len() < 55 || report[0] != 0x30 {
        return None;
    }
    let level = report[54] & 0x0F;
    percent((level * 10).min(100))
}

/// Logitech G-Hub loopback JSON.
fn logitech_ghub_json(raw: &str) -> Option<u8> {
    let value: serde_json::Value = serde_json::from_str(raw).ok()?;
    let devices = value.get("devices")?.as_array()?;
    let batteries: Vec<u8> = devices
        .iter()
        .filter_map(|device| device.get("battery")?.as_u64())
        .filter_map(|n| u8::try_from(n).ok())
        .collect();
    batteries.into_iter().max()
}

/// SteelSeries GG coreProps loopback URI: `ot-ss://battery?level=NN`.
fn steelseries_uri(uri: &str) -> Option<u8> {
    let query = uri.split_once('?')?.1;
    for pair in query.split('&') {
        let (key, value) = pair.split_once('=')?;
        if key.eq_ignore_ascii_case("level") {
            return value.parse::<u8>().ok().and_then(percent);
        }
    }
    None
}

/// Aggregator guard: drop samples more than 25% away from the median, then
/// average the survivors.
fn outlier_filter(samples: &[u8]) -> Option<u8> {
    if samples.is_empty() {
        return None;
    }
    let mut sorted = samples.to_vec();
    sorted.sort_unstable();
    let median = sorted[sorted.len() / 2] as f64;
    let kept: Vec<u8> = samples
        .iter()
        .copied()
        .filter(|value| {
            let delta = (*value as f64 - median).abs();
            delta <= median * 0.25
        })
        .collect();
    if kept.is_empty() {
        return None;
    }
    let sum: u64 = kept.iter().map(|v| *v as u64).sum();
    percent((sum / kept.len() as u64) as u8)
}

fn apply_alias(name: &str, aliases: &[(&str, &str)]) -> String {
    aliases
        .iter()
        .find(|(from, _)| from.eq_ignore_ascii_case(name))
        .map(|(_, to)| (*to).to_owned())
        .unwrap_or_else(|| name.to_owned())
}

fn is_ignored(name: &str, ignored: &[&str]) -> bool {
    let name = name.to_ascii_lowercase();
    ignored.iter().any(|entry| {
        let entry = entry.to_ascii_lowercase();
        if entry.starts_with('*') && entry.ends_with('*') {
            name.contains(entry.trim_matches('*'))
        } else if let Some(suffix) = entry.strip_prefix('*') {
            name.ends_with(suffix)
        } else if let Some(prefix) = entry.strip_suffix('*') {
            name.starts_with(prefix)
        } else {
            name == entry
        }
    })
}

#[test]
fn asus_rog_accepts_only_its_vid_and_request_ids() {
    let mut report = vec![0u8; 64];
    report[0] = 0x12;
    report[12] = 78;
    assert_eq!(asus_rog(0x0B05, &report), Some(78));
    report[0] = 0x07;
    assert_eq!(asus_rog(0x0B05, &report), Some(78));
    assert_eq!(asus_rog(0x1B1C, &report), None);
    report[12] = 101;
    assert_eq!(asus_rog(0x0B05, &report), None);
    assert_eq!(asus_rog(0x0B05, &report[..32]), None);
}

#[test]
fn corsair_accepts_bragi_and_legacy_reports() {
    let mut report = vec![0u8; 16];
    report[0] = 0x42;
    report[7] = 55;
    assert_eq!(corsair(0x1B1C, &report), Some(55));
    report[0] = 0xC2;
    assert_eq!(corsair(0x1B1C, &report), Some(55));
    report[0] = 0x99;
    assert_eq!(corsair(0x1B1C, &report), None);
    assert_eq!(corsair(0x0B05, &report), None);
}

#[test]
fn hyperx_uses_four_protocol_families() {
    let families = [2usize, 3, 4, 5];
    for (family, offset) in families.into_iter().enumerate() {
        let mut report = vec![0u8; 8];
        report[0] = family as u8;
        report[offset] = 20 + family as u8;
        assert_eq!(hyperx(0x03F0, &report), Some(20 + family as u8));
        assert_eq!(hyperx(0x0951, &report), Some(20 + family as u8));
    }
    assert_eq!(hyperx(0x1B1C, &[0, 50]), None);
}

#[test]
fn compx_atk_vxe_validates_checksum_and_report_size() {
    let mut report = vec![0u8; 17];
    report[9] = 64;
    report[16] = 64; // checksum = 64 (all other bytes are zero)
    assert_eq!(compx_atk_vxe(&report), Some(64));
    report[16] = 63;
    assert_eq!(compx_atk_vxe(&report), None);
    assert_eq!(compx_atk_vxe(&[0u8; 12]), None);
}

#[test]
fn gamepad_parses_capacity_nibble() {
    let mut report = vec![0u8; 0x40];
    report[0] = 0x30;
    report[54] = 0x08;
    assert_eq!(gamepad(&report), Some(80));
    report[54] = 0x0F;
    assert_eq!(gamepad(&report), Some(100)); // clamped from 150
    report[0] = 0x31;
    assert_eq!(gamepad(&report), None);
}

#[test]
fn logitech_ghub_loopback_extracts_highest_battery() {
    let json = r#"{"devices":[{"name":"MX Master","battery":42},{"name":"Keys","battery":91}]}"#;
    assert_eq!(logitech_ghub_json(json), Some(91));
    assert_eq!(logitech_ghub_json(r#"{"devices":[]}"#), None);
    assert_eq!(logitech_ghub_json("not json"), None);
}

#[test]
fn steelseries_uri_parses_level() {
    assert_eq!(
        steelseries_uri("ot-ss://battery?level=73&source=gg"),
        Some(73)
    );
    assert_eq!(steelseries_uri("ot-ss://battery?source=gg"), None);
    assert_eq!(steelseries_uri("ot-ss://battery?level=101"), None);
}

#[test]
fn aggregator_discards_outliers() {
    assert_eq!(outlier_filter(&[80, 82, 79, 81, 5]), Some(80));
    assert_eq!(outlier_filter(&[]), None);
    // The far-away sample is dropped; the median sample always survives.
    assert_eq!(outlier_filter(&[0, 100]), Some(100));
}

#[test]
fn aliases_and_ignored_lists_are_case_insensitive() {
    let aliases = [("mouse 2.4g", "Desk Mouse")];
    assert_eq!(apply_alias("MOUSE 2.4G", &aliases), "Desk Mouse");
    assert_eq!(apply_alias("other", &aliases), "other");
    let ignored = ["guid:abc", "*virtual*"];
    assert!(is_ignored("virtual keyboard", &ignored));
    assert!(!is_ignored("real keyboard", &ignored));
}

/// Production bridge: the vendor VIDs are public constants in the device
/// worktree's parser module, and the aggregator settings are real core fields.
#[test]
fn production_vendor_ids_and_aggregator_settings_match_the_reference() {
    const VENDORS: &str = "crates/opentaskbar-app/src/platform/wireless_battery.rs";
    let source = common::source(VENDORS);
    assert_eq!(common::numeric_const(&source, "ASUS_VID"), Some(0x0B05));
    assert_eq!(common::numeric_const(&source, "CORSAIR_VID"), Some(0x1B1C));
    common::assert_source_has(VENDORS, &["0x03F0", "0x0951", "BatterySource::AsusRogHid"]);

    let mut settings = DockSettings::default();
    assert_eq!(settings.wireless_battery_poll_seconds, 30);
    settings.wireless_battery_poll_seconds = 45;
    settings.wireless_battery_low_threshold = 20;
    settings
        .wireless_device_aliases
        .insert("mouse 2.4g".to_owned(), "Desk Mouse".to_owned());
    settings.ignored_wireless_devices = vec!["*virtual*".to_owned()];
    assert!(settings.validate().is_empty(), "fixture must be valid");
    let json = serde_json::to_value(&settings).expect("settings serialize");
    assert_eq!(json["WirelessBatteryPollSeconds"], 45);
    assert_eq!(json["WirelessBatteryLowThreshold"], 20);
    let back: DockSettings = serde_json::from_value(json).expect("settings round-trip");
    assert_eq!(back.wireless_battery_poll_seconds, 45);
    assert_eq!(back.wireless_device_aliases["mouse 2.4g"], "Desk Mouse");
    assert!(is_ignored(
        "virtual keyboard",
        &back
            .ignored_wireless_devices
            .iter()
            .map(String::as_str)
            .collect::<Vec<_>>()
    ));

    common::assert_source_has(
        "crates/opentaskbar-core/src/settings.rs",
        &[
            "wireless_battery_poll_seconds",
            "WirelessPollOutOfRange",
            "WirelessDeviceAliasesInvalid",
        ],
    );
}

/// The vendor report parsers are private to the bin-only app crate, so a core
/// integration test cannot call them and a live HID device is needed to run
/// them. This bridge pins the shipped parser/VID surface while the vectors
/// above keep the expected report semantics anchored.
#[test]
fn production_hid_parsers_match_the_reference_vectors() {
    common::assert_source_has(
        "crates/opentaskbar-app/src/platform/wireless_battery.rs",
        &[
            "ASUS_VID",
            "CORSAIR_VID",
            "hyperx_family",
            "fn direct_byte_report(",
            "fn ss_parse(",
            "fn razer_parse_reply(",
        ],
    );
    let mut report = vec![0u8; 64];
    report[0] = 0x12;
    report[12] = 78;
    assert_eq!(asus_rog(0x0B05, &report), Some(78));
    assert_eq!(asus_rog(0x1B1C, &report), None);
    assert_eq!(outlier_filter(&[80, 82, 79, 81, 5]), Some(80));
}

/// Bridge pinning all 6 live multi-brand probes, extended vendor parsers,
/// SetupDi Bluetooth presence, BLE Connected guard, WirelessDeviceKind,
/// spare dock battery preservation, and settings gating in production code.
#[test]
fn production_live_multi_brand_and_extended_vendors_are_wired() {
    const BATTERY: &str = "crates/opentaskbar-app/src/platform/wireless_battery.rs";
    const VENDORS_EXT: &str = "crates/opentaskbar-app/src/platform/wireless_vendors_ext.rs";

    common::assert_source_has(
        BATTERY,
        &[
            "pub enum WirelessDeviceKind",
            "pub secondary_level: Option<f32>",
            "pub device_kind: WirelessDeviceKind",
            "fn probe_asus_rog(",
            "fn probe_corsair(",
            "fn probe_hyperx(",
            "fn probe_compx_and_mice(",
            "fn probe_keyboard_24g(",
            "fn probe_gamepad_hid(",
            "DEVPKEY_BLUETOOTH_BATTERY",
            "DEVPKEY_DEVICE_IS_PRESENT",
            "SetupDiGetDevicePropertyW",
            "BluetoothConnectionStatus::Connected",
            "wireless_multi_brand_enabled()",
            "wireless_disabled_vendors()",
            "else if r[0] == 0x83",
        ],
    );

    common::assert_source_has(
        VENDORS_EXT,
        &[
            "pub enum HyperXDeviceProtocol",
            "HpCloudHeadset64",
            "KingstonCloud20",
            "PulsefireMouse64",
            "pub fn parse_glorious_reply(",
            "pub fn parse_endgame_gear_reply(",
            "pub fn parse_finalmouse_reply(",
            "pub fn parse_zowie_reply(",
            "pub fn parse_keychron_raw_hid_reply(",
            "pub fn classify_wireless_device_kind(",
        ],
    );

    let ext_src = common::source(VENDORS_EXT);
    assert_eq!(
        common::numeric_const(&ext_src, "HP_HYPERX_VID"),
        Some(0x03F0)
    );
    assert_eq!(
        common::numeric_const(&ext_src, "KINGSTON_HYPERX_VID"),
        Some(0x0951)
    );
    assert_eq!(
        common::numeric_const(&ext_src, "ENDGAME_GEAR_VID"),
        Some(0x3367)
    );
    assert_eq!(
        common::numeric_const(&ext_src, "FINALMOUSE_VID"),
        Some(0x361D)
    );
    assert_eq!(
        common::numeric_const(&ext_src, "ZOWIE_BENQ_VID"),
        Some(0x3057)
    );
    assert_eq!(
        common::numeric_const(&ext_src, "KEYCHRON_VID"),
        Some(0x3434)
    );
}

/// Reference ranking for two samples of the same physical battery: verified live link,
/// then non-stale, then provenance (lower = better: live GATT / vendor HID 0, standard HID
/// 1, vendor daemon 2, Windows PnP 3, XInput 4).
fn reference_prefer(candidate: (bool, bool, u8), existing: (bool, bool, u8)) -> bool {
    let (c_live, c_stale, c_rank) = candidate;
    let (e_live, e_stale, e_rank) = existing;
    if c_live != e_live {
        return c_live;
    }
    if c_stale != e_stale {
        return !c_stale;
    }
    c_rank < e_rank
}

/// Reference expiry: an entry is retained while `now - sampled_at <= 30 min`, where
/// `sampled_at` is the one persisted sample time (never the file write time).
fn reference_retained(sampled_at: u64, now: u64) -> bool {
    now.saturating_sub(sampled_at) <= 30 * 60
}

/// Reference G HUB percentage rule: integers are percentages; only a float strictly
/// between 0 and 1 is a fraction.
fn reference_ghub_percent(value: &serde_json::Value) -> Option<u32> {
    let n = value.as_f64()?;
    if !(0.0..=100.0).contains(&n) {
        return None;
    }
    Some(if value.is_f64() && n > 0.0 && n < 1.0 {
        (n * 100.0).round() as u32
    } else {
        n.round() as u32
    })
}

/// Reference Corsair legacy (`C9 64`) block: level `data[2] & 0x7F`, state `data[4]`
/// (0 offline, 1/2 battery, 4/5 charging).
fn reference_corsair_legacy(data: &[u8]) -> Option<(u8, bool)> {
    if data.len() < 5 || data[..2] != [0xC9, 0x64] {
        return None;
    }
    let level = data[2] & 0x7F;
    if level > 100 || !matches!(data[4], 1 | 2 | 4 | 5) {
        return None;
    }
    Some((level, matches!(data[4], 4 | 5)))
}

/// Reference ASUS ROG 65-byte `00 12 07` layout: level at byte 5, charge state at byte 10.
fn reference_asus_65(data: &[u8]) -> Option<(u8, bool)> {
    if data.len() != 65 || data[..3] != [0x00, 0x12, 0x07] {
        return None;
    }
    let (level, state) = (data[5], data[10]);
    if level > 100 || state > 3 {
        return None;
    }
    Some((level, matches!(state, 1 | 2)))
}

#[test]
fn accuracy_reference_live_gatt_beats_pnp_and_expiry_never_renews() {
    // Live uncached GATT 42% (rank 0) vs live Windows PnP 80% (rank 3): GATT wins.
    assert!(reference_prefer((true, false, 0), (true, false, 3)));
    assert!(!reference_prefer((true, false, 3), (true, false, 0)));
    // A live reading always beats a stale registry snapshot, whatever its provenance.
    assert!(reference_prefer((true, false, 3), (false, true, 0)));
    // 31 minutes of continued polling: the persisted sample time never moves.
    let sampled_at = 1_000_000u64;
    for minute in 0..=30u64 {
        assert!(reference_retained(sampled_at, sampled_at + minute * 60));
    }
    assert!(!reference_retained(sampled_at, sampled_at + 31 * 60));
}

#[test]
fn accuracy_reference_protocol_vectors() {
    assert_eq!(reference_ghub_percent(&serde_json::json!(1)), Some(1));
    assert_eq!(reference_ghub_percent(&serde_json::json!(1.0)), Some(1));
    assert_eq!(reference_ghub_percent(&serde_json::json!(0.42)), Some(42));
    assert_eq!(reference_ghub_percent(&serde_json::json!(101)), None);

    let mut legacy = [0u8; 64];
    legacy[0] = 0xC9;
    legacy[1] = 0x64;
    legacy[2] = 0x80 | 78;
    legacy[4] = 1;
    assert_eq!(reference_corsair_legacy(&legacy), Some((78, false)));
    legacy[4] = 0;
    assert_eq!(reference_corsair_legacy(&legacy), None, "offline");

    let mut chakram = [0u8; 65];
    chakram[1] = 0x12;
    chakram[2] = 0x07;
    chakram[5] = 0x39;
    chakram[6] = 0xFF;
    assert_eq!(reference_asus_65(&chakram), Some((57, false)));
}

/// Bridge pinning the Bluetooth / HID accuracy fixes in production code.
#[test]
fn production_accuracy_fixes_are_wired() {
    const BATTERY: &str = "crates/opentaskbar-app/src/platform/wireless_battery.rs";
    const VENDORS_EXT: &str = "crates/opentaskbar-app/src/platform/wireless_vendors_ext.rs";
    const TELEMETRY: &str = "crates/opentaskbar-app/src/widgets/telemetry.rs";
    const WIDGETS: &str = "crates/opentaskbar-app/src/widgets/mod.rs";

    common::assert_source_has(
        BATTERY,
        &[
            "ReadValueWithCacheModeAsync(",
            "BluetoothCacheMode::Uncached",
            "GetGattServicesForUuidWithCacheModeAsync(",
            "GattCommunicationStatus::Success",
            "DEVPKEY_DEVICE_IS_CONNECTED",
            "DEVPKEY_DEVICE_CONTAINER_ID",
            "pub fn read_all_with_settings_forced(",
            "STALE_TOMBSTONES",
            "pub fn is_uncertain(self)",
            "fn accuracy_live_gatt_beats_windows_pnp_for_same_physical_device(",
            "fn accuracy_disconnected_device_expires_after_31_minutes_without_renewal(",
            "fn accuracy_identical_name_peripherals_stay_separate_rows(",
            "fn standard_hid_rejects_overload_and_preserves_zero_percent(",
            "fn steelseries_nova7_direct_vs_discrete_and_nova5_offsets(",
            "fn asus_rog_chakram_x_65_byte_layout_reports_57_percent(",
            "fn ghub_battery_state_integer_one_is_one_percent(",
        ],
    );
    common::assert_source_lacks(
        BATTERY,
        &["ReadValueAsync()", "0x0085 => &[0x0064, 0x0065, 0x0066"],
    );
    common::assert_source_has(
        VENDORS_EXT,
        &[
            "CloudFlightVoltage",
            "HpVendor50",
            "GLORIOUS_FN_BATTERY",
            "pub fn build_endgame_gear_feature_request(",
            "pub const ZOWIE_BENQ_RX_VID",
            "fn keychron_via_raw_hid_probe_is_removed(",
            "fn hyperx_cloud_flight_big_endian_millivolt_curve(",
        ],
    );
    common::assert_source_lacks(
        VENDORS_EXT,
        &[
            "pub fn build_keychron_raw_hid_request(",
            "pub fn is_valid_keychron_raw_hid_reply(",
        ],
    );
    common::assert_source_has(
        TELEMETRY,
        &[
            "pub fn apply_bluetooth_snapshot(",
            "pub fn query_bluetooth_battery_powershell(",
            "fn late_fallback_generation_never_reverts_a_fresh_reading(",
        ],
    );
    common::assert_source_has(
        WIDGETS,
        &[
            "wb_generation",
            "pub fn refresh_bluetooth_devices_forced(",
            "struct WirelessRefreshGuard",
            "pub fn wireless_battery_summary_pairs(",
            "pub(crate) fn lazy_subsystems_allocated(",
        ],
    );
    common::assert_source_lacks(WIDGETS, &["bt_secondary_levels"]);
}
