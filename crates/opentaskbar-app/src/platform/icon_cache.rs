//! Persistent raw-BGRA icon disk cache + short-lived out-of-process icon worker.
//!
//! Multi-tier extraction pipeline:
//! - For `shell:AppsFolder\<AUMID>` (Store/MSIX/inbox apps):
//!   1. `IShellItemImageFactory::GetImage(SIIGBF_ICONONLY | SIIGBF_BIGGERSIZEOK)` -> retry without
//!      `SIIGBF_ICONONLY` if blank/failed.
//!   2. AppsFolder PIDL -> `SHGetFileInfoW(SHGFI_PIDL | SHGFI_ICON | SHGFI_LARGEICON)`.
//!   3. Registered package root / `AppxManifest.xml` -> MRT qualified PNG asset selection.
//! - For classic filesystem specs (`.exe`/`.ico`/`.dll`/`.lnk`):
//!   1. `PrivateExtractIconsW`
//!   2. `IShellItemImageFactory`
//!   3. `SHGetFileInfoW(SHGFI_ICON | SHGFI_LARGEICON)`
//! - Every extracted, cached, or decoded buffer passes a strict visibility gate (`max_alpha > 0`)
//!   and aspect-ratio-preserving square normalization before being returned or cached.

use std::collections::HashMap;
use std::fs;
use std::io::{BufRead, BufReader, Read, Write};
use std::os::windows::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdin, ChildStdout, Command, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, OnceLock, RwLock, mpsc};
use std::time::{Duration, UNIX_EPOCH};

use super::com::ComApartment;
use super::pins::{apps_folder_id, package_install_roots};
use super::win32_helpers::{CREATE_NO_WINDOW, wide};

use windows::Win32::Foundation::SIZE;
use windows::Win32::Graphics::Gdi::{DeleteObject, HBITMAP, HGDIOBJ};
use windows::Win32::System::Com::{COINIT_APARTMENTTHREADED, CoInitializeEx};
use windows::Win32::UI::Shell::Common::ITEMIDLIST;
use windows::Win32::UI::Shell::{
    ILFree, IShellItemImageFactory, SHCreateItemFromParsingName, SHParseDisplayName,
    SIIGBF_BIGGERSIZEOK, SIIGBF_ICONONLY,
};
use windows::core::HSTRING;
use windows_sys::Win32::Foundation::HWND;
use windows_sys::Win32::Graphics::Gdi::{
    BI_RGB, BITMAP as SYS_BITMAP, BITMAPINFO, BITMAPINFOHEADER, CreateCompatibleDC, DIB_RGB_COLORS,
    DeleteDC, DeleteObject as SysDeleteObject, GetDIBits, GetObjectW as SysGetObjectW,
    HBITMAP as SYS_HBITMAP, HDC,
};
use windows_sys::Win32::Storage::FileSystem::{FILE_ATTRIBUTE_DIRECTORY, FILE_ATTRIBUTE_NORMAL};
use windows_sys::Win32::UI::Shell::{
    SHFILEINFOW, SHGFI_ICON, SHGFI_LARGEICON, SHGFI_PIDL, SHGFI_USEFILEATTRIBUTES, SHGetFileInfoW,
};
use windows_sys::Win32::UI::WindowsAndMessaging::{
    DestroyIcon, GCLP_HICON, GCLP_HICONSM, GET_CLASS_LONG_INDEX, GetIconInfo, HICON, ICON_BIG,
    ICON_SMALL, ICON_SMALL2, ICONINFO, PrivateExtractIconsW, SMTO_ABORTIFHUNG, SendMessageTimeoutW,
    WM_GETICON,
};

pub const WORKER_ARG: &str = "--extract-icons";

/// Native rasterisation size (physical px) of every app/folder icon this module extracts.
/// The dock's slot-icon transforms scale surfaces by `ICON_PX`, so keep them in sync.
pub const ICON_PX: u32 = 64;
/// The Recycle Bin is extracted sharper than ordinary slot icons; the dock scales its
/// surface by this value instead of [`ICON_PX`].
pub const RECYCLE_BIN_ICON_PX: u32 = 128;

const CACHE_MAGIC: &[u8; 8] = b"OTICON06";
const CACHE_HEADER_LEN: usize = 24;
const MAX_DIM: u32 = 256;
const MEMORY_CACHE_CAPACITY: usize = 256;
const GENERIC_ICON_CACHE_CAPACITY: usize = 256;
const WORKER_REQUEST_DEADLINE: Duration = Duration::from_millis(1500);
const HWND_ICON_TIMEOUT_MS: u32 = 150;

#[derive(Clone, Debug)]
pub struct RawIcon {
    pub width: u32,
    pub height: u32,
    /// Top-down straight BGRA32 pixels. `Arc` lets the disk and memory caches hand off
    /// their buffer without copying the ~16 KiB payload on every cache hit.
    pub pixels: Arc<[u8]>,
}

// ============================================================================================
// Bounded in-memory LRU icon cache & generic extension icon cache
// ============================================================================================

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
struct MemoryCacheKey {
    spec_lower: String,
    size: u32,
    pack_rev: u64,
}

struct MemoryCacheEntry {
    icon: RawIcon,
    last_used: AtomicU64,
}

struct MemoryIconCache {
    entries: HashMap<MemoryCacheKey, MemoryCacheEntry>,
}

static MEMORY_CACHE: OnceLock<RwLock<MemoryIconCache>> = OnceLock::new();
static MEMORY_CACHE_TICK: AtomicU64 = AtomicU64::new(1);
static GENERIC_ICON_CACHE: OnceLock<RwLock<HashMap<String, RawIcon>>> = OnceLock::new();

fn memory_cache() -> &'static RwLock<MemoryIconCache> {
    MEMORY_CACHE.get_or_init(|| {
        RwLock::new(MemoryIconCache {
            entries: HashMap::with_capacity(64),
        })
    })
}

fn generic_icon_cache() -> &'static RwLock<HashMap<String, RawIcon>> {
    GENERIC_ICON_CACHE.get_or_init(|| RwLock::new(HashMap::with_capacity(32)))
}

fn current_pack_rev() -> u64 {
    if !super::icon_pack::theme_application_icons_enabled() {
        return 0;
    }
    match super::icon_pack::active_pack() {
        None => 0,
        Some(pack) => {
            let mut h: u64 = 0xcbf2_9ce4_8422_2325;
            for b in pack.directory().to_string_lossy().as_bytes() {
                h ^= u64::from(b.to_ascii_lowercase());
                h = h.wrapping_mul(0x0100_0000_01b3);
            }
            for b in pack.name.as_bytes() {
                h ^= u64::from(*b);
                h = h.wrapping_mul(0x0100_0000_01b3);
            }
            h ^= (pack.icons.len() as u64).rotate_left(19);
            h.max(1)
        }
    }
}

fn memory_cache_key(spec: &str, size: u32, pack_rev: u64) -> MemoryCacheKey {
    MemoryCacheKey {
        spec_lower: spec.trim().to_ascii_lowercase(),
        size,
        pack_rev,
    }
}

fn lookup_memory_cached_raw(spec: &str, size: u32) -> Option<RawIcon> {
    if spec.trim().is_empty() || size == 0 || size > MAX_DIM {
        return None;
    }
    let key = memory_cache_key(spec, size, current_pack_rev());
    let guard = memory_cache()
        .read()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    let entry = guard.entries.get(&key)?;
    entry.last_used.store(
        MEMORY_CACHE_TICK.fetch_add(1, Ordering::Relaxed),
        Ordering::Relaxed,
    );
    Some(entry.icon.clone())
}

fn insert_memory_cache(spec: &str, size: u32, icon: &RawIcon) {
    if spec.trim().is_empty()
        || size == 0
        || size > MAX_DIM
        || !has_visible_pixels(&icon.pixels, icon.width, icon.height)
    {
        return;
    }
    let key = memory_cache_key(spec, size, current_pack_rev());
    let tick = MEMORY_CACHE_TICK.fetch_add(1, Ordering::Relaxed);
    let mut guard = memory_cache()
        .write()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    if guard.entries.len() >= MEMORY_CACHE_CAPACITY
        && !guard.entries.contains_key(&key)
        && let Some(evict_key) = guard
            .entries
            .iter()
            .min_by_key(|(_, entry)| entry.last_used.load(Ordering::Relaxed))
            .map(|(k, _)| k.clone())
    {
        guard.entries.remove(&evict_key);
    }
    guard.entries.insert(
        key,
        MemoryCacheEntry {
            icon: icon.clone(),
            last_used: AtomicU64::new(tick),
        },
    );
}

/// Pure in-memory LRU lookup (zero disk I/O, zero IPC).
pub fn lookup_memory_cached(spec: &str, size: u32) -> Option<(u32, u32, Vec<u8>)> {
    lookup_memory_cached_raw(spec, size).map(raw_to_tuple)
}

fn raw_to_tuple(icon: RawIcon) -> (u32, u32, Vec<u8>) {
    (icon.width, icon.height, icon.pixels.to_vec())
}

// ============================================================================================
// Validation & square normalization
// ============================================================================================

/// Rejects empty, wrong-length, out-of-bounds, or all-transparent (`max_alpha == 0`) buffers.
fn has_visible_pixels(pixels: &[u8], width: u32, height: u32) -> bool {
    if width == 0 || height == 0 || width > MAX_DIM || height > MAX_DIM {
        return false;
    }
    let Some(expected) = (width as usize)
        .checked_mul(height as usize)
        .and_then(|n| n.checked_mul(4))
    else {
        return false;
    };
    if pixels.len() != expected {
        return false;
    }
    pixels.as_chunks::<4>().0.iter().any(|p| p[3] != 0)
}

/// Normalizes a validated straight-BGRA icon onto a `target_size x target_size` square canvas,
/// preserving aspect ratio and rejecting blank outputs.
fn normalize_to_square(icon: RawIcon, target_size: u32) -> Option<RawIcon> {
    if target_size == 0
        || target_size > MAX_DIM
        || !has_visible_pixels(&icon.pixels, icon.width, icon.height)
    {
        return None;
    }
    if icon.width == target_size && icon.height == target_size {
        return Some(icon);
    }

    let (scaled_w, scaled_h) = if icon.width >= icon.height {
        let h = ((f64::from(icon.height) * f64::from(target_size) / f64::from(icon.width)).round()
            as u32)
            .clamp(1, target_size);
        (target_size, h)
    } else {
        let w = ((f64::from(icon.width) * f64::from(target_size) / f64::from(icon.height)).round()
            as u32)
            .clamp(1, target_size);
        (w, target_size)
    };

    // Convert straight BGRA to premultiplied RGBA so resampling does not create dark edge halos.
    let mut premul_rgba = Vec::with_capacity(icon.pixels.len());
    for p in icon.pixels.as_chunks::<4>().0 {
        let a = u16::from(p[3]);
        if a == 0 {
            premul_rgba.extend_from_slice(&[0, 0, 0, 0]);
        } else if a == 255 {
            premul_rgba.extend_from_slice(&[p[2], p[1], p[0], 255]);
        } else {
            let r = ((u16::from(p[2]) * a + 127) / 255) as u8;
            let g = ((u16::from(p[1]) * a + 127) / 255) as u8;
            let b = ((u16::from(p[0]) * a + 127) / 255) as u8;
            premul_rgba.extend_from_slice(&[r, g, b, p[3]]);
        }
    }

    let src = image::RgbaImage::from_raw(icon.width, icon.height, premul_rgba)?;
    let resized = image::imageops::resize(
        &src,
        scaled_w,
        scaled_h,
        image::imageops::FilterType::Lanczos3,
    );

    let target = target_size as usize;
    let mut canvas = vec![0u8; target * target * 4];
    let offset_x = ((target_size - scaled_w) / 2) as usize;
    let offset_y = ((target_size - scaled_h) / 2) as usize;
    let resized_raw = resized.as_raw();
    let sw = scaled_w as usize;

    for y in 0..scaled_h as usize {
        for x in 0..sw {
            let s_idx = (y * sw + x) * 4;
            let d_idx = ((offset_y + y) * target + (offset_x + x)) * 4;
            let a = u16::from(resized_raw[s_idx + 3]);
            if a == 0 {
                continue;
            }
            if a == 255 {
                canvas[d_idx] = resized_raw[s_idx + 2];
                canvas[d_idx + 1] = resized_raw[s_idx + 1];
                canvas[d_idx + 2] = resized_raw[s_idx];
                canvas[d_idx + 3] = 255;
            } else {
                let r = u16::from(resized_raw[s_idx]).min(a);
                let g = u16::from(resized_raw[s_idx + 1]).min(a);
                let b = u16::from(resized_raw[s_idx + 2]).min(a);
                canvas[d_idx] = ((b * 255 + a / 2) / a).min(255) as u8;
                canvas[d_idx + 1] = ((g * 255 + a / 2) / a).min(255) as u8;
                canvas[d_idx + 2] = ((r * 255 + a / 2) / a).min(255) as u8;
                canvas[d_idx + 3] = a as u8;
            }
        }
    }

    has_visible_pixels(&canvas, target_size, target_size).then(|| RawIcon {
        width: target_size,
        height: target_size,
        pixels: Arc::from(canvas),
    })
}

// ============================================================================================
// Disk cache (`OTICON06`)
// ============================================================================================

fn cache_dir() -> PathBuf {
    opentaskbar_core::paths::rust_cache_dir()
}

fn hash_key(path: &str) -> u64 {
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    for b in path.as_bytes() {
        h ^= u64::from(b.to_ascii_lowercase());
        h = h.wrapping_mul(0x0100_0000_01b3);
    }
    h
}

fn split_icon_location(spec: &str) -> (&str, i32) {
    if let Some((path, idx_str)) = spec.rsplit_once(',')
        && let Ok(idx) = idx_str.trim().parse::<i32>()
    {
        return (path.trim(), idx);
    }
    (spec.trim(), 0)
}

fn modified_secs(meta: &fs::Metadata) -> u64 {
    meta.modified()
        .ok()
        .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
        .map_or(0, |d| d.as_secs())
}

fn is_shell_spec(spec: &str) -> bool {
    let trimmed = spec.trim();
    trimmed
        .get(..6)
        .is_some_and(|p| p.eq_ignore_ascii_case("shell:"))
        || trimmed.starts_with("::{")
}

fn fingerprint(spec: &str) -> Option<u64> {
    if is_shell_spec(spec) {
        return Some(0);
    }
    let (path, idx) = split_icon_location(spec);
    let meta = fs::metadata(path).ok()?;
    let (len, modified) = if meta.is_dir() {
        // A folder's own mtime changes whenever a child is added or removed (e.g. a
        // download landing in `Downloads`), but its icon only depends on `desktop.ini`.
        // Fingerprint that file instead (0 when absent) so the disk cache survives.
        fs::metadata(Path::new(path).join("desktop.ini"))
            .ok()
            .filter(fs::Metadata::is_file)
            .map_or((0, 0), |ini| (ini.len(), modified_secs(&ini)))
    } else {
        (meta.len(), modified_secs(&meta))
    };
    Some(len.rotate_left(17) ^ modified ^ (idx as u64).rotate_left(7))
}

fn cache_path(spec: &str) -> PathBuf {
    cache_path_for_size(spec, ICON_PX)
}

fn cache_path_for_size(spec: &str, size: u32) -> PathBuf {
    if size == ICON_PX {
        cache_dir().join(format!("{:016x}.oticon", hash_key(spec)))
    } else {
        cache_dir().join(format!("{:016x}_{size}.oticon", hash_key(spec)))
    }
}

pub fn read_cached_raw(spec: &str) -> Option<RawIcon> {
    if let Some(hit) = lookup_memory_cached_raw(spec, ICON_PX) {
        return Some(hit);
    }
    let icon = read_cached_sized(spec, ICON_PX)?;
    insert_memory_cache(spec, ICON_PX, &icon);
    Some(icon)
}

fn decode_cache_bytes(
    bytes: Vec<u8>,
    expected_fp: u64,
    expected_size: Option<u32>,
) -> Option<RawIcon> {
    if bytes.len() < CACHE_HEADER_LEN || &bytes[..8] != CACHE_MAGIC {
        return None;
    }
    let width = u32::from_le_bytes(bytes[8..12].try_into().ok()?);
    let height = u32::from_le_bytes(bytes[12..16].try_into().ok()?);
    let fp = u64::from_le_bytes(bytes[16..24].try_into().ok()?);
    if fp != expected_fp || width == 0 || height == 0 || width > MAX_DIM || height > MAX_DIM {
        return None;
    }
    if let Some(size) = expected_size
        && (width != size || height != size)
    {
        return None;
    }
    let pixels = bytes[CACHE_HEADER_LEN..].to_vec();
    if !has_visible_pixels(&pixels, width, height) {
        return None;
    }
    Some(RawIcon {
        width,
        height,
        pixels: Arc::from(pixels),
    })
}

fn encode_cache_bytes(icon: &RawIcon, fp: u64) -> Option<Vec<u8>> {
    if !has_visible_pixels(&icon.pixels, icon.width, icon.height) {
        return None;
    }
    let mut out = Vec::with_capacity(CACHE_HEADER_LEN + icon.pixels.len());
    out.extend_from_slice(CACHE_MAGIC);
    out.extend_from_slice(&icon.width.to_le_bytes());
    out.extend_from_slice(&icon.height.to_le_bytes());
    out.extend_from_slice(&fp.to_le_bytes());
    out.extend_from_slice(&icon.pixels);
    Some(out)
}

fn read_cached_sized(spec: &str, size: u32) -> Option<RawIcon> {
    // Open the cache file first: on a miss this avoids stat-ing the source executable
    // (the UI thread and the worker both probe the same cache during startup).
    let bytes = fs::read(cache_path_for_size(spec, size)).ok()?;
    let expected_fp = fingerprint(spec)?;
    decode_cache_bytes(bytes, expected_fp, Some(size))
}

fn write_cached_sized(spec: &str, size: u32, icon: &RawIcon) {
    if !has_visible_pixels(&icon.pixels, icon.width, icon.height) {
        return;
    }
    let Some(fp) = fingerprint(spec) else {
        return;
    };
    let Some(out) = encode_cache_bytes(icon, fp) else {
        return;
    };
    let dir = cache_dir();
    if fs::create_dir_all(&dir).is_err() {
        return;
    }
    let dst = cache_path_for_size(spec, size);
    let tmp = dst.with_extension("tmp");
    if fs::write(&tmp, &out).is_ok() {
        let _ = fs::rename(&tmp, &dst);
    }
}

// ============================================================================================
// GDI HBITMAP / DIBSECTION / HICON extraction
// ============================================================================================

struct OwnedDc(HDC);

impl Drop for OwnedDc {
    fn drop(&mut self) {
        if !self.0.is_null() {
            // SAFETY: owns the DC created by `CreateCompatibleDC`.
            unsafe {
                DeleteDC(self.0);
            }
        }
    }
}

struct OwnedIconBitmaps {
    color: SYS_HBITMAP,
    mask: SYS_HBITMAP,
}

impl Drop for OwnedIconBitmaps {
    fn drop(&mut self) {
        // SAFETY: both bitmaps were allocated by `GetIconInfo` for the caller.
        unsafe {
            if !self.color.is_null() {
                SysDeleteObject(self.color);
            }
            if !self.mask.is_null() {
                SysDeleteObject(self.mask);
            }
        }
    }
}

/// Converts a GDI `HBITMAP` (DIBSECTION or DDB, 32bpp or lower bit depth, top-down or bottom-up)
/// into top-down straight BGRA32. Does not delete `bitmap`.
///
/// Always uses `GetDIBits` with a negative `biHeight` rather than reading `DIBSECTION::dsBm.bmBits`
/// directly, because Windows `GetObjectW(hbm, sizeof(DIBSECTION), &dib)` normalizes
/// `dsBmih.biHeight` to a positive value even for top-down DIB sections (which would otherwise
/// invert top-down Shell/UWP icons).
fn hbitmap_to_bgra(bitmap: SYS_HBITMAP, premultiplied: bool) -> Option<RawIcon> {
    if bitmap.is_null() {
        return None;
    }
    // SAFETY: reads GDI bitmap metadata/pixels without mutating or deleting `bitmap`.
    unsafe {
        let mut bmp: SYS_BITMAP = std::mem::zeroed();
        if SysGetObjectW(
            bitmap,
            size_of::<SYS_BITMAP>() as i32,
            (&mut bmp as *mut SYS_BITMAP).cast(),
        ) == 0
        {
            return None;
        }
        let width = bmp.bmWidth.max(0) as u32;
        let height = bmp.bmHeight.unsigned_abs();
        if width == 0 || height == 0 || width > MAX_DIM || height > MAX_DIM {
            return None;
        }
        let total_bytes = (width as usize)
            .checked_mul(height as usize)?
            .checked_mul(4)?;
        let mut pixels = vec![0u8; total_bytes];
        let mut info = BITMAPINFO {
            bmiHeader: BITMAPINFOHEADER {
                biSize: size_of::<BITMAPINFOHEADER>() as u32,
                biWidth: width as i32,
                biHeight: -(height as i32),
                biPlanes: 1,
                biBitCount: 32,
                biCompression: BI_RGB,
                ..std::mem::zeroed()
            },
            bmiColors: [std::mem::zeroed()],
        };
        let dc = OwnedDc(CreateCompatibleDC(std::ptr::null_mut()));
        if dc.0.is_null() {
            return None;
        }
        let rows = GetDIBits(
            dc.0,
            bitmap,
            0,
            height,
            pixels.as_mut_ptr().cast(),
            &mut info,
            DIB_RGB_COLORS,
        );
        if rows != height as i32 {
            return None;
        }
        let source_has_alpha = bmp.bmBitsPixel >= 32;

        if !source_has_alpha {
            for p in pixels.as_chunks_mut::<4>().0 {
                p[3] = 255;
            }
        } else if premultiplied {
            premultiplied_to_straight(&mut pixels);
        } else {
            for p in pixels.as_chunks_mut::<4>().0 {
                if p[3] == 0 {
                    p[0] = 0;
                    p[1] = 0;
                    p[2] = 0;
                }
            }
        }

        has_visible_pixels(&pixels, width, height).then(|| RawIcon {
            width,
            height,
            pixels: Arc::from(pixels),
        })
    }
}

/// Extracts an `HICON` to top-down straight BGRA32, supporting 32bpp alpha icons,
/// legacy color+AND-mask icons, and monochrome mask-only icons (`hbmColor == null`).
fn hicon_to_bgra(hicon: HICON) -> Option<RawIcon> {
    if hicon.is_null() {
        return None;
    }
    // SAFETY: reads icon color/mask bitmaps via `GetIconInfo` and frees both on scope exit.
    unsafe {
        let mut ii: ICONINFO = std::mem::zeroed();
        if GetIconInfo(hicon, &mut ii) == 0 {
            return None;
        }
        let bitmaps = OwnedIconBitmaps {
            color: ii.hbmColor,
            mask: ii.hbmMask,
        };
        let dc = OwnedDc(CreateCompatibleDC(std::ptr::null_mut()));
        if dc.0.is_null() {
            return None;
        }

        if !bitmaps.color.is_null() {
            let mut bmp: SYS_BITMAP = std::mem::zeroed();
            if SysGetObjectW(
                bitmaps.color,
                size_of::<SYS_BITMAP>() as i32,
                (&mut bmp as *mut SYS_BITMAP).cast(),
            ) == 0
            {
                return None;
            }
            let width = bmp.bmWidth.max(0) as u32;
            let height = bmp.bmHeight.unsigned_abs();
            if width == 0 || height == 0 || width > MAX_DIM || height > MAX_DIM {
                return None;
            }
            let total_bytes = (width as usize)
                .checked_mul(height as usize)?
                .checked_mul(4)?;
            let mut pixels = vec![0u8; total_bytes];
            let mut info = BITMAPINFO {
                bmiHeader: BITMAPINFOHEADER {
                    biSize: size_of::<BITMAPINFOHEADER>() as u32,
                    biWidth: width as i32,
                    biHeight: -(height as i32),
                    biPlanes: 1,
                    biBitCount: 32,
                    biCompression: BI_RGB,
                    ..std::mem::zeroed()
                },
                bmiColors: [std::mem::zeroed()],
            };
            let rows = GetDIBits(
                dc.0,
                bitmaps.color,
                0,
                height,
                pixels.as_mut_ptr().cast(),
                &mut info,
                DIB_RGB_COLORS,
            );
            if rows != height as i32 {
                return None;
            }

            let has_alpha = pixels.as_chunks::<4>().0.iter().any(|p| p[3] != 0);
            if !has_alpha {
                if !bitmaps.mask.is_null() {
                    let mut mask = vec![0u8; total_bytes];
                    let mut mask_info = BITMAPINFO {
                        bmiHeader: BITMAPINFOHEADER {
                            biSize: size_of::<BITMAPINFOHEADER>() as u32,
                            biWidth: width as i32,
                            biHeight: -(height as i32),
                            biPlanes: 1,
                            biBitCount: 32,
                            biCompression: BI_RGB,
                            ..std::mem::zeroed()
                        },
                        bmiColors: [std::mem::zeroed()],
                    };
                    let mrows = GetDIBits(
                        dc.0,
                        bitmaps.mask,
                        0,
                        height,
                        mask.as_mut_ptr().cast(),
                        &mut mask_info,
                        DIB_RGB_COLORS,
                    );
                    if mrows != height as i32 {
                        return None;
                    }
                    for (p, m) in pixels
                        .as_chunks_mut::<4>()
                        .0
                        .iter_mut()
                        .zip(mask.as_chunks::<4>().0)
                    {
                        // Mask white (non-zero) = transparent, black (0) = opaque.
                        if m[0] != 0 || m[1] != 0 || m[2] != 0 {
                            p[0] = 0;
                            p[1] = 0;
                            p[2] = 0;
                            p[3] = 0;
                        } else {
                            p[3] = 255;
                        }
                    }
                } else if bmp.bmBitsPixel < 32 {
                    for p in pixels.as_chunks_mut::<4>().0 {
                        p[3] = 255;
                    }
                }
            } else {
                // Zero RGB on fully transparent pixels to prevent dark filter halos.
                for p in pixels.as_chunks_mut::<4>().0 {
                    if p[3] == 0 {
                        p[0] = 0;
                        p[1] = 0;
                        p[2] = 0;
                    }
                }
            }

            return has_visible_pixels(&pixels, width, height).then(|| RawIcon {
                width,
                height,
                pixels: Arc::from(pixels),
            });
        }

        // Monochrome mask-only icon: `hbmMask` is double-height (upper = AND mask, lower = XOR mask).
        if bitmaps.mask.is_null() {
            return None;
        }
        let mut bmp: SYS_BITMAP = std::mem::zeroed();
        if SysGetObjectW(
            bitmaps.mask,
            size_of::<SYS_BITMAP>() as i32,
            (&mut bmp as *mut SYS_BITMAP).cast(),
        ) == 0
        {
            return None;
        }
        let width = bmp.bmWidth.max(0) as u32;
        let total_height = bmp.bmHeight.unsigned_abs();
        if width == 0
            || total_height < 2
            || !total_height.is_multiple_of(2)
            || width > MAX_DIM
            || total_height / 2 > MAX_DIM
        {
            return None;
        }
        let height = total_height / 2;
        let total_bytes = (width as usize)
            .checked_mul(total_height as usize)?
            .checked_mul(4)?;
        let half_bytes = total_bytes / 2;
        let mut mask_buf = vec![0u8; total_bytes];
        let mut info = BITMAPINFO {
            bmiHeader: BITMAPINFOHEADER {
                biSize: size_of::<BITMAPINFOHEADER>() as u32,
                biWidth: width as i32,
                biHeight: -(total_height as i32),
                biPlanes: 1,
                biBitCount: 32,
                biCompression: BI_RGB,
                ..std::mem::zeroed()
            },
            bmiColors: [std::mem::zeroed()],
        };
        let rows = GetDIBits(
            dc.0,
            bitmaps.mask,
            0,
            total_height,
            mask_buf.as_mut_ptr().cast(),
            &mut info,
            DIB_RGB_COLORS,
        );
        if rows != total_height as i32 {
            return None;
        }

        let (and_half, xor_half) = mask_buf.split_at(half_bytes);
        let mut pixels = vec![0u8; half_bytes];
        for ((dst, and_px), xor_px) in pixels
            .as_chunks_mut::<4>()
            .0
            .iter_mut()
            .zip(and_half.as_chunks::<4>().0)
            .zip(xor_half.as_chunks::<4>().0)
        {
            let and_one = and_px[0] != 0;
            let xor_one = xor_px[0] != 0;
            match (and_one, xor_one) {
                (true, false) => *dst = [0, 0, 0, 0],
                (false, false) => *dst = [0, 0, 0, 255],
                (false, true) | (true, true) => *dst = [255, 255, 255, 255],
            }
        }

        has_visible_pixels(&pixels, width, height).then(|| RawIcon {
            width,
            height,
            pixels: Arc::from(pixels),
        })
    }
}

fn from_shell_hbitmap(bitmap: HBITMAP, size: u32) -> Option<RawIcon> {
    let raw = hbitmap_to_bgra(bitmap.0, true);
    // SAFETY: `bitmap` is owned by the caller from `IShellItemImageFactory::GetImage`.
    unsafe {
        let _ = DeleteObject(HGDIOBJ(bitmap.0));
    }
    raw.and_then(|icon| normalize_to_square(icon, size))
}

// ============================================================================================
// Built-in icon helpers & multi-tier extraction pipeline
// ============================================================================================

fn imageres_spec(res_id: i32) -> String {
    let root = std::env::var("SystemRoot").unwrap_or_else(|_| r"C:\Windows".to_owned());
    format!(r"{root}\System32\imageres.dll,{res_id}")
}

/// Extracts the native Windows Shell Recycle Bin icon (Empty or Full) from `imageres.dll`
/// as a crisp [`RECYCLE_BIN_ICON_PX`]-square BGRA32 image.
pub fn extract_recycle_bin_icon(full: bool) -> Option<RawIcon> {
    // Windows 10/11 `imageres.dll` resource IDs: -55 = Empty Recycle Bin, -54 = Full Recycle Bin.
    let res_id = if full { -54 } else { -55 };
    extract_private_icon(&imageres_spec(res_id), RECYCLE_BIN_ICON_PX)
        .filter(|raw| raw.width == RECYCLE_BIN_ICON_PX && raw.height == RECYCLE_BIN_ICON_PX)
}

/// Extracts the native Windows folder icon (`imageres.dll,-3`) as an [`ICON_PX`]-square
/// BGRA32 image. Synchronous and cheap (no Shell/COM), so folder stacks can show it on
/// the first frame while their real, possibly customised, icon resolves in the worker.
pub fn extract_default_folder_icon() -> Option<RawIcon> {
    extract_private_icon(&imageres_spec(-3), ICON_PX)
        .filter(|raw| raw.width == ICON_PX && raw.height == ICON_PX)
}

/// Extracts directly from `.exe` / `.ico` / `.dll` via `PrivateExtractIconsW` (mirrors `GetExecutableIcon`).
fn extract_private_icon(spec: &str, size: u32) -> Option<RawIcon> {
    let (path, index) = split_icon_location(spec);
    let lower = path.to_lowercase();
    if !(lower.ends_with(".exe") || lower.ends_with(".ico") || lower.ends_with(".dll")) {
        return None;
    }
    let wide_path = wide(path);
    let mut hicon: HICON = std::ptr::null_mut();
    let mut icon_id: u32 = 0;
    // SAFETY: PrivateExtractIconsW writes one owned HICON destroyed below.
    unsafe {
        let count = PrivateExtractIconsW(
            wide_path.as_ptr(),
            index,
            size as i32,
            size as i32,
            &mut hicon,
            &mut icon_id,
            1,
            0,
        );
        if count == 0 || count == u32::MAX || hicon.is_null() {
            return None;
        }
        let icon = hicon_to_bgra(hicon);
        DestroyIcon(hicon);
        icon.and_then(|raw| normalize_to_square(raw, size))
    }
}

/// Tier 1 for shell items: `IShellItemImageFactory::GetImage(SIIGBF_ICONONLY | SIIGBF_BIGGERSIZEOK)`,
/// retrying without `SIIGBF_ICONONLY` if blank or failed.
fn extract_shell_image_factory(parsing_name: &str, size: u32) -> Option<RawIcon> {
    // SAFETY: COM STA initialized by caller; each HBITMAP is freed in `from_shell_hbitmap`.
    unsafe {
        let factory: IShellItemImageFactory =
            SHCreateItemFromParsingName(&HSTRING::from(parsing_name), None).ok()?;
        let dim = SIZE {
            cx: size as i32,
            cy: size as i32,
        };
        for flags in [SIIGBF_ICONONLY | SIIGBF_BIGGERSIZEOK, SIIGBF_BIGGERSIZEOK] {
            if let Ok(bitmap) = factory.GetImage(dim, flags)
                && let Some(icon) = from_shell_hbitmap(bitmap, size)
            {
                return Some(icon);
            }
        }
        None
    }
}

/// Tier 2 for AppsFolder items: parse PIDL and extract via `SHGetFileInfoW(SHGFI_PIDL | SHGFI_ICON | SHGFI_LARGEICON)`.
fn extract_shell_icon_pidl(parsing_name: &str, size: u32) -> Option<RawIcon> {
    let mut pidl: *mut ITEMIDLIST = std::ptr::null_mut();
    // SAFETY: COM STA initialized by caller; PIDL is freed with `ILFree` and HICON with `DestroyIcon`.
    unsafe {
        if SHParseDisplayName(&HSTRING::from(parsing_name), None, &mut pidl, 0, None).is_err()
            || pidl.is_null()
        {
            return None;
        }
        let mut shfi: SHFILEINFOW = std::mem::zeroed();
        let ok = SHGetFileInfoW(
            pidl.cast::<u16>().cast_const(),
            0,
            &mut shfi,
            size_of::<SHFILEINFOW>() as u32,
            SHGFI_PIDL | SHGFI_ICON | SHGFI_LARGEICON,
        );
        ILFree(Some(pidl.cast_const()));
        if ok == 0 || shfi.hIcon.is_null() {
            return None;
        }
        let icon = hicon_to_bgra(shfi.hIcon);
        DestroyIcon(shfi.hIcon);
        icon.and_then(|raw| normalize_to_square(raw, size))
    }
}

/// Tier 3 for classic filesystem items: `SHGetFileInfoW(SHGFI_ICON | SHGFI_LARGEICON)`.
fn extract_shell_icon_path(path: &str, size: u32) -> Option<RawIcon> {
    let wide_path = wide(path);
    // SAFETY: `wide_path` is NUL-terminated UTF-16; returned HICON is destroyed below.
    unsafe {
        let mut shfi: SHFILEINFOW = std::mem::zeroed();
        let ok = SHGetFileInfoW(
            wide_path.as_ptr(),
            0,
            &mut shfi,
            size_of::<SHFILEINFOW>() as u32,
            SHGFI_ICON | SHGFI_LARGEICON,
        );
        if ok == 0 || shfi.hIcon.is_null() {
            return None;
        }
        let icon = hicon_to_bgra(shfi.hIcon);
        DestroyIcon(shfi.hIcon);
        icon.and_then(|raw| normalize_to_square(raw, size))
    }
}

// ============================================================================================
// Tier 3 for UWP/MSIX AppsFolder: AppxManifest.xml + MRT qualified PNG asset resolution
// ============================================================================================

fn manifest_logo_candidates(xml: &str, app_id: &str) -> Vec<String> {
    let Ok(doc) = roxmltree::Document::parse(xml) else {
        return Vec::new();
    };
    let mut logos = Vec::new();
    let apps: Vec<_> = doc
        .descendants()
        .filter(|n| n.tag_name().name() == "Application")
        .collect();
    let app_node = apps
        .iter()
        .copied()
        .find(|n| {
            n.attribute("Id")
                .is_some_and(|id| id.eq_ignore_ascii_case(app_id))
        })
        .or_else(|| (apps.len() == 1).then(|| apps[0]));

    if let Some(app) = app_node
        && let Some(visual) = app
            .children()
            .find(|n| n.tag_name().name() == "VisualElements")
    {
        for attr in ["Square44x44Logo", "Square150x150Logo", "Logo"] {
            if let Some(val) = visual
                .attribute(attr)
                .map(str::trim)
                .filter(|s| !s.is_empty())
            {
                logos.push(val.to_owned());
            }
        }
    }

    if let Some(prop_logo) = doc
        .descendants()
        .find(|n| n.tag_name().name() == "Properties")
        .and_then(|props| props.children().find(|n| n.tag_name().name() == "Logo"))
        .and_then(|n| n.text())
        .map(str::trim)
        .filter(|s| !s.is_empty())
    {
        logos.push(prop_logo.to_owned());
    }

    logos
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
struct MrtRank {
    /// Non-contrast preferred over high-contrast.
    non_contrast: bool,
    /// 4 = targetsize+unplated, 3 = targetsize+lightunplated, 2 = targetsize, 1 = scale, 0 = plain.
    tier: u8,
    /// Primary size fitness (higher is better).
    size_fitness: i32,
    /// Secondary tiebreaker (higher is better).
    tiebreak: i32,
}

fn score_mrt_qualifiers(qualifier_text: &str, target_size: u32) -> MrtRank {
    let lower = qualifier_text.to_ascii_lowercase();
    let tokens: Vec<&str> = lower
        .split(['_', '.', '\\', '/'])
        .filter(|t| !t.is_empty())
        .collect();

    let is_contrast = tokens.iter().any(|t| t.starts_with("contrast-"));
    let is_unplated = tokens.contains(&"altform-unplated");
    let is_light_unplated = tokens.contains(&"altform-lightunplated");
    let targetsize = tokens
        .iter()
        .find_map(|t| t.strip_prefix("targetsize-")?.parse::<u32>().ok());
    let scale = tokens
        .iter()
        .find_map(|t| t.strip_prefix("scale-")?.parse::<u32>().ok());

    if let Some(ts) = targetsize {
        let tier = if is_unplated {
            4
        } else if is_light_unplated {
            3
        } else {
            2
        };
        // Prefer exact match, then preferred unplated sizes (>= target_size smallest overshoot,
        // otherwise largest available size below target_size).
        let (size_fitness, tiebreak) = if ts == target_size {
            (10_000, 0)
        } else if ts > target_size {
            (5_000 - (ts - target_size) as i32, ts as i32)
        } else {
            (ts as i32, -((target_size - ts) as i32))
        };
        return MrtRank {
            non_contrast: !is_contrast,
            tier,
            size_fitness,
            tiebreak,
        };
    }

    if let Some(sc) = scale {
        // Prefer scale-200 > scale-150 > scale-400/300 > scale-125 > scale-100.
        let size_fitness = match sc {
            200 => 500,
            150 => 450,
            400 => 400,
            300 => 380,
            125 => 300,
            100 => 250,
            other => 100 + (other.min(1000) as i32 / 10),
        };
        return MrtRank {
            non_contrast: !is_contrast,
            tier: 1,
            size_fitness,
            tiebreak: sc as i32,
        };
    }

    MrtRank {
        non_contrast: !is_contrast,
        tier: 0,
        size_fitness: 0,
        tiebreak: 0,
    }
}

/// Ranks MRT-qualified PNG file paths matching `base_stem` (e.g. `Square44x44Logo`), best first.
fn rank_mrt_asset_candidates(
    base_stem: &str,
    candidates: &[PathBuf],
    target_size: u32,
) -> Vec<PathBuf> {
    let base_lower = base_stem.to_ascii_lowercase();
    let prefix = format!("{base_lower}.");
    let mut ranked: Vec<(MrtRank, PathBuf)> = Vec::new();

    for path in candidates {
        let is_png = path
            .extension()
            .and_then(|e| e.to_str())
            .is_some_and(|e| e.eq_ignore_ascii_case("png"));
        if !is_png {
            continue;
        }
        let Some(stem) = path.file_stem().and_then(|s| s.to_str()) else {
            continue;
        };
        let stem_lower = stem.to_ascii_lowercase();
        let parent_qualifier = path
            .parent()
            .and_then(Path::file_name)
            .and_then(|n| n.to_str())
            .unwrap_or_default();

        let qualifier = if stem_lower == base_lower {
            parent_qualifier.to_owned()
        } else if let Some(rest) = stem_lower.strip_prefix(&prefix) {
            if parent_qualifier.is_empty() {
                rest.to_owned()
            } else {
                format!("{parent_qualifier}_{rest}")
            }
        } else {
            continue;
        };

        let rank = score_mrt_qualifiers(&qualifier, target_size);
        ranked.push((rank, path.clone()));
    }

    ranked.sort_by(|a, b| b.0.cmp(&a.0).then_with(|| a.1.cmp(&b.1)));
    ranked.into_iter().map(|(_, p)| p).collect()
}

fn collect_mrt_candidates(package_root: &Path, relative_logo: &str, size: u32) -> Vec<PathBuf> {
    let normalized = relative_logo.replace('/', "\\");
    let rel_path = Path::new(&normalized);
    let Some(base_stem) = rel_path.file_stem().and_then(|s| s.to_str()) else {
        return Vec::new();
    };
    let parent_dir = rel_path
        .parent()
        .map(|p| package_root.join(p))
        .unwrap_or_else(|| package_root.to_path_buf());

    let mut files = Vec::new();
    if let Ok(entries) = fs::read_dir(&parent_dir) {
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_file() {
                files.push(path);
            } else if path.is_dir() {
                // Handle qualifier subdirectories like `Assets\scale-200\Square44x44Logo.png`.
                let dir_name = entry.file_name().to_string_lossy().to_ascii_lowercase();
                if (dir_name.contains("scale-")
                    || dir_name.contains("targetsize-")
                    || dir_name.contains("altform-"))
                    && let Ok(sub_entries) = fs::read_dir(&path)
                {
                    for sub in sub_entries.flatten() {
                        let sub_path = sub.path();
                        if sub_path.is_file() {
                            files.push(sub_path);
                        }
                    }
                }
            }
        }
    }

    rank_mrt_asset_candidates(base_stem, &files, size)
}

fn extract_manifest_mrt_icon(aumid: &str, size: u32) -> Option<RawIcon> {
    let (family, app_id) = aumid.split_once('!')?;
    for root in package_install_roots(family) {
        let Ok(xml) = fs::read_to_string(root.join("AppxManifest.xml")) else {
            continue;
        };
        for relative_logo in manifest_logo_candidates(&xml, app_id) {
            for candidate in collect_mrt_candidates(&root, &relative_logo, size) {
                if let Some(icon) = decode_image_file(&candidate, size) {
                    return Some(icon);
                }
            }
        }
    }
    None
}

fn extract_best_sized(spec: &str, size: u32) -> Option<RawIcon> {
    let trimmed = spec.trim();
    if trimmed.is_empty() || size == 0 || size > MAX_DIM {
        return None;
    }

    if let Some(aumid) = apps_folder_id(trimmed) {
        let canonical = format!("shell:AppsFolder\\{aumid}");
        return extract_shell_image_factory(&canonical, size)
            .or_else(|| extract_shell_icon_pidl(&canonical, size))
            .or_else(|| extract_manifest_mrt_icon(aumid, size));
    }

    if is_shell_spec(trimmed) {
        return extract_shell_image_factory(trimmed, size)
            .or_else(|| extract_shell_icon_pidl(trimmed, size));
    }

    let (path, _) = split_icon_location(trimmed);
    extract_private_icon(trimmed, size)
        .or_else(|| extract_shell_image_factory(path, size))
        .or_else(|| extract_shell_icon_path(path, size))
}

// ============================================================================================
// Window HWND icon fallback (`WM_GETICON` + `GetClassLongPtrW`)
// ============================================================================================

fn get_class_long_ptr(hwnd: HWND, index: GET_CLASS_LONG_INDEX) -> usize {
    #[cfg(target_pointer_width = "64")]
    // SAFETY: reading window class icon handle from a borrowed HWND.
    unsafe {
        windows_sys::Win32::UI::WindowsAndMessaging::GetClassLongPtrW(hwnd, index)
    }
    #[cfg(target_pointer_width = "32")]
    // SAFETY: reading window class icon handle from a borrowed HWND.
    unsafe {
        windows_sys::Win32::UI::WindowsAndMessaging::GetClassLongW(hwnd, index) as usize
    }
}

/// Extracts an application icon directly from a live top-level HWND via bounded
/// `SendMessageTimeoutW(WM_GETICON)` and `GetClassLongPtrW(GCLP_HICON / GCLP_HICONSM)`.
/// Rejects 0-alpha blank icons and normalizes to `size x size`.
pub fn extract_hwnd_icon_bgra(hwnd: isize, size: u32) -> Option<(u32, u32, Vec<u8>)> {
    if hwnd == 0 || size == 0 || size > MAX_DIM {
        return None;
    }
    let handle = hwnd as HWND;
    // SAFETY: bounded timeout messages & class queries; returned HICONs are owned by the window
    // and must NOT be destroyed by us.
    unsafe {
        for wparam in [ICON_BIG as usize, ICON_SMALL2 as usize, ICON_SMALL as usize] {
            let mut result: usize = 0;
            let sent = SendMessageTimeoutW(
                handle,
                WM_GETICON,
                wparam,
                0,
                SMTO_ABORTIFHUNG,
                HWND_ICON_TIMEOUT_MS,
                &mut result,
            );
            if sent != 0 && result != 0 {
                let hicon = result as HICON;
                if let Some(icon) = hicon_to_bgra(hicon).and_then(|r| normalize_to_square(r, size))
                {
                    return Some(raw_to_tuple(icon));
                }
            }
        }
        for index in [GCLP_HICON, GCLP_HICONSM] {
            let result = get_class_long_ptr(handle, index);
            if result != 0 {
                let hicon = result as HICON;
                if let Some(icon) = hicon_to_bgra(hicon).and_then(|r| normalize_to_square(r, size))
                {
                    return Some(raw_to_tuple(icon));
                }
            }
        }
    }
    None
}

// ============================================================================================
// Instant non-blocking generic icon lookup for folder stacks (`SHGFI_USEFILEATTRIBUTES`)
// ============================================================================================

fn generic_extension_key(path: &Path, is_dir: bool) -> String {
    if is_dir {
        return "<dir>".to_owned();
    }
    path.extension()
        .and_then(|ext| ext.to_str())
        .map(str::trim)
        .filter(|ext| !ext.is_empty() && ext.len() <= 32)
        .map(|ext| format!(".{}", ext.to_ascii_lowercase()))
        .unwrap_or_else(|| "<file>".to_owned())
}

fn fetch_generic_shell_icon(dummy_name: &str, attributes: u32, size: u32) -> Option<RawIcon> {
    let _com = ComApartment::sta();
    let wide_name = wide(dummy_name);
    // SAFETY: `SHGFI_USEFILEATTRIBUTES` never touches the filesystem or hydrates cloud files.
    unsafe {
        let mut shfi: SHFILEINFOW = std::mem::zeroed();
        let ok = SHGetFileInfoW(
            wide_name.as_ptr(),
            attributes,
            &mut shfi,
            size_of::<SHFILEINFOW>() as u32,
            SHGFI_USEFILEATTRIBUTES | SHGFI_ICON | SHGFI_LARGEICON,
        );
        if ok == 0 || shfi.hIcon.is_null() {
            return None;
        }
        let icon = hicon_to_bgra(shfi.hIcon);
        DestroyIcon(shfi.hIcon);
        icon.and_then(|raw| normalize_to_square(raw, size))
    }
}

/// Returns an instant, extension-cached generic icon using `SHGFI_USEFILEATTRIBUTES`
/// (zero disk reads, zero OneDrive hydration).
pub fn instant_generic_icon_for_path(path: &Path, is_dir: bool) -> Option<(u32, u32, Vec<u8>)> {
    let key = generic_extension_key(path, is_dir);
    if let Some(cached) = generic_icon_cache()
        .read()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .get(&key)
        .cloned()
    {
        return Some(raw_to_tuple(cached));
    }

    let extracted = if is_dir {
        fetch_generic_shell_icon("folder", FILE_ATTRIBUTE_DIRECTORY, ICON_PX)
            .or_else(extract_default_folder_icon)
    } else {
        let dummy = if key == "<file>" {
            "file".to_owned()
        } else {
            format!("file{key}")
        };
        fetch_generic_shell_icon(&dummy, FILE_ATTRIBUTE_NORMAL, ICON_PX)
            .or_else(|| fetch_generic_shell_icon("file", FILE_ATTRIBUTE_NORMAL, ICON_PX))
    }?;

    {
        let mut guard = generic_icon_cache()
            .write()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if guard.len() >= GENERIC_ICON_CACHE_CAPACITY && !guard.contains_key(&key) {
            guard.clear();
        }
        guard.insert(key, extracted.clone());
    }
    Some(raw_to_tuple(extracted))
}

// ============================================================================================
// Out-of-process icon worker (`OpenTaskBar.exe --extract-icons`) with per-request deadline
// ============================================================================================

fn parse_worker_request(line: &str) -> (u32, &str) {
    let trimmed = line.trim_end_matches(['\r', '\n']);
    if let Some((size_str, spec)) = trimmed.split_once('\t')
        && let Ok(size) = size_str.parse::<u32>()
    {
        return (size.clamp(12, MAX_DIM), spec);
    }
    (ICON_PX, trimmed)
}

/// Worker entry point (`OpenTaskBar.exe --extract-icons`).
pub fn run_worker() {
    // SAFETY: COM STA for IShellItemImageFactory, isolated to this short-lived worker process.
    let _ = unsafe { CoInitializeEx(None, COINIT_APARTMENTTHREADED) };
    let stdin = std::io::stdin();
    let stdout = std::io::stdout();
    let mut reader = BufReader::new(stdin.lock());
    let mut writer = stdout.lock();
    let mut line = String::new();
    while reader.read_line(&mut line).unwrap_or(0) > 0 {
        let (size, spec) = parse_worker_request(&line);
        let icon = if spec.is_empty() {
            None
        } else {
            extract_best_sized(spec, size)
        };
        let ok = match icon {
            Some(ic) => writer
                .write_all(&[1u8])
                .and_then(|()| writer.write_all(&ic.width.to_le_bytes()))
                .and_then(|()| writer.write_all(&ic.height.to_le_bytes()))
                .and_then(|()| writer.write_all(&(ic.pixels.len() as u32).to_le_bytes()))
                .and_then(|()| writer.write_all(&ic.pixels)),
            None => writer.write_all(&[0u8]),
        };
        if ok.is_err() || writer.flush().is_err() {
            break;
        }
        line.clear();
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum WorkerError {
    Timeout,
    Disconnected,
}

struct Worker {
    child: Child,
    stdin: Option<ChildStdin>,
    rx: mpsc::Receiver<Result<Option<RawIcon>, ()>>,
}

/// Terminates and reaps a child that cannot be handed to `Worker`. `Child`'s
/// `Drop` does neither, so without this the worker process would outlive a
/// failed pipe setup.
fn cleanup_failed_child(child: &mut Child) {
    let _ = child.kill();
    let _ = child.wait();
}

fn read_worker_frame(stdout: &mut BufReader<ChildStdout>) -> Result<Option<RawIcon>, ()> {
    let mut status = [0u8; 1];
    stdout.read_exact(&mut status).map_err(|_| ())?;
    if status[0] == 0 {
        return Ok(None);
    }
    let mut hdr = [0u8; 12];
    stdout.read_exact(&mut hdr).map_err(|_| ())?;
    let width = u32::from_le_bytes(hdr[0..4].try_into().map_err(|_| ())?);
    let height = u32::from_le_bytes(hdr[4..8].try_into().map_err(|_| ())?);
    let len = u32::from_le_bytes(hdr[8..12].try_into().map_err(|_| ())?) as usize;
    let expected = (width as usize)
        .saturating_mul(height as usize)
        .saturating_mul(4);
    if width == 0 || height == 0 || width > MAX_DIM || height > MAX_DIM || len != expected {
        return Err(());
    }
    let mut pixels = vec![0u8; len];
    stdout.read_exact(&mut pixels).map_err(|_| ())?;
    if !has_visible_pixels(&pixels, width, height) {
        return Ok(None);
    }
    Ok(Some(RawIcon {
        width,
        height,
        pixels: Arc::from(pixels),
    }))
}

impl Worker {
    fn spawn() -> Option<Self> {
        let exe = std::env::current_exe().ok()?;
        let child = Command::new(exe)
            .arg(WORKER_ARG)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .creation_flags(CREATE_NO_WINDOW)
            .spawn()
            .ok()?;
        Self::from_child(child)
    }

    /// Takes ownership of a spawned child; on any missing pipe the child is
    /// killed and waited for instead of being dropped alive.
    fn from_child(mut child: Child) -> Option<Self> {
        let Some(stdin) = child.stdin.take() else {
            cleanup_failed_child(&mut child);
            return None;
        };
        let Some(stdout) = child.stdout.take() else {
            cleanup_failed_child(&mut child);
            return None;
        };
        let (tx, rx) = mpsc::channel();
        let spawn_result = std::thread::Builder::new()
            .name("opentaskbar-icon-worker-rx".into())
            .spawn(move || {
                let mut reader = BufReader::new(stdout);
                loop {
                    let frame = read_worker_frame(&mut reader);
                    let is_err = frame.is_err();
                    if tx.send(frame).is_err() || is_err {
                        break;
                    }
                }
            });
        if spawn_result.is_err() {
            cleanup_failed_child(&mut child);
            return None;
        }
        Some(Self {
            child,
            stdin: Some(stdin),
            rx,
        })
    }

    fn request_sized(
        &mut self,
        spec: &str,
        size: u32,
        deadline: Duration,
    ) -> Result<Option<RawIcon>, WorkerError> {
        if spec.contains(['\r', '\n', '\t']) {
            return Ok(None);
        }
        let stdin = self.stdin.as_mut().ok_or(WorkerError::Disconnected)?;
        writeln!(stdin, "{size}\t{spec}").map_err(|_| WorkerError::Disconnected)?;
        stdin.flush().map_err(|_| WorkerError::Disconnected)?;
        match self.rx.recv_timeout(deadline) {
            Ok(Ok(icon)) => Ok(icon),
            Ok(Err(())) | Err(mpsc::RecvTimeoutError::Disconnected) => {
                Err(WorkerError::Disconnected)
            }
            Err(mpsc::RecvTimeoutError::Timeout) => Err(WorkerError::Timeout),
        }
    }
}

impl Drop for Worker {
    fn drop(&mut self) {
        self.stdin.take();
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

/// Decodes an image file (PNG/ICO/JPG/BMP) into straight BGRA32, validates visible pixels,
/// and normalizes it onto a square `size x size` canvas.
fn decode_image_file(path: &Path, size: u32) -> Option<RawIcon> {
    let decoded = image::open(path).ok()?.to_rgba8();
    let (width, height) = decoded.dimensions();
    if width == 0 || height == 0 {
        return None;
    }
    let mut pixels = Vec::with_capacity(width as usize * height as usize * 4);
    for pixel in decoded.as_raw().as_chunks::<4>().0 {
        pixels.extend_from_slice(&[pixel[2], pixel[1], pixel[0], pixel[3]]);
    }
    if width <= MAX_DIM && height <= MAX_DIM {
        let raw = RawIcon {
            width,
            height,
            pixels: pixels.into(),
        };
        return normalize_to_square(raw, size);
    }
    // Downscale oversized source images (> MAX_DIM) before square normalization.
    let (scaled_w, scaled_h) = if width >= height {
        let h = ((f64::from(height) * f64::from(size) / f64::from(width)).round() as u32)
            .clamp(1, size);
        (size, h)
    } else {
        let w = ((f64::from(width) * f64::from(size) / f64::from(height)).round() as u32)
            .clamp(1, size);
        (w, size)
    };
    let resized = image::imageops::resize(
        &decoded,
        scaled_w,
        scaled_h,
        image::imageops::FilterType::Lanczos3,
    );
    let mut small_bgra = Vec::with_capacity(scaled_w as usize * scaled_h as usize * 4);
    for pixel in resized.as_raw().as_chunks::<4>().0 {
        small_bgra.extend_from_slice(&[pixel[2], pixel[1], pixel[0], pixel[3]]);
    }
    normalize_to_square(
        RawIcon {
            width: scaled_w,
            height: scaled_h,
            pixels: small_bgra.into(),
        },
        size,
    )
}

/// Decodes a custom-pack image (PNG/ICO/JPG/BMP) into straight BGRA32.
fn decode_pack_icon(path: &Path, size: u32) -> Option<RawIcon> {
    decode_image_file(path, size)
}

fn fetch_from_worker_sized(worker: &mut Option<Worker>, spec: &str, size: u32) -> Option<RawIcon> {
    for _ in 0..2 {
        if worker.is_none() {
            *worker = Worker::spawn();
        }
        let active = worker.as_mut()?;
        match active.request_sized(spec, size, WORKER_REQUEST_DEADLINE) {
            Ok(icon) => return icon.and_then(|raw| normalize_to_square(raw, size)),
            Err(WorkerError::Timeout) => {
                // Kill the stalled worker and do not waste another deadline on the same item.
                *worker = None;
                return None;
            }
            Err(WorkerError::Disconnected) => *worker = None,
        }
    }
    None
}

fn resolve_single_with_worker(
    worker: &mut Option<Worker>,
    spec: &str,
    size: u32,
) -> Option<RawIcon> {
    let trimmed = spec.trim();
    if trimmed.is_empty() || size == 0 || size > MAX_DIM {
        return None;
    }
    if let Some(hit) = lookup_memory_cached_raw(trimmed, size) {
        return Some(hit);
    }
    if let Some(icon) = super::icon_pack::find_for_spec(trimmed)
        .and_then(|path| decode_pack_icon(Path::new(&path), size))
    {
        insert_memory_cache(trimmed, size, &icon);
        return Some(icon);
    }
    if let Some(cached) = read_cached_sized(trimmed, size) {
        insert_memory_cache(trimmed, size, &cached);
        return Some(cached);
    }
    let extracted = fetch_from_worker_sized(worker, trimmed, size)?;
    write_cached_sized(trimmed, size, &extracted);
    insert_memory_cache(trimmed, size, &extracted);
    Some(extracted)
}

/// Resolves the first candidate spec that yields a valid visible icon at `size x size`.
pub fn resolve_first_valid(specs: &[String], size: u32) -> Option<(u32, u32, Vec<u8>)> {
    if specs.is_empty() || size == 0 || size > MAX_DIM {
        return None;
    }
    let mut worker: Option<Worker> = None;
    for spec in specs {
        if let Some(icon) = resolve_single_with_worker(&mut worker, spec, size) {
            return Some(raw_to_tuple(icon));
        }
    }
    None
}

pub fn resolve_batch_raw(specs: &[String]) -> HashMap<String, Option<RawIcon>> {
    let mut result = HashMap::with_capacity(specs.len());
    let mut worker: Option<Worker> = None;

    for spec in specs {
        if result.contains_key(spec) {
            continue;
        }
        let raw = resolve_single_with_worker(&mut worker, spec, ICON_PX);
        result.insert(spec.clone(), raw);
    }
    result
}

/// Resolves a batch of icon specs on a background thread, falling back to an instant
/// extension-based generic icon whenever a filesystem item fails or hits the worker deadline.
pub fn resolve_batch_with_fallback(
    specs: &[String],
    size: u32,
) -> HashMap<String, (u32, u32, Vec<u8>)> {
    let mut result = HashMap::with_capacity(specs.len());
    if size == 0 || size > MAX_DIM {
        return result;
    }
    let mut worker: Option<Worker> = None;

    for spec in specs {
        if result.contains_key(spec) {
            continue;
        }
        if let Some(icon) = resolve_single_with_worker(&mut worker, spec, size) {
            result.insert(spec.clone(), raw_to_tuple(icon));
            continue;
        }
        let trimmed = spec.trim();
        if trimmed.is_empty() || is_shell_spec(trimmed) {
            continue;
        }
        let (path_str, _) = split_icon_location(trimmed);
        let path = Path::new(path_str);
        let is_dir = path.is_dir();
        if let Some((w, h, pixels)) = instant_generic_icon_for_path(path, is_dir)
            && let Some(normalized) = normalize_to_square(
                RawIcon {
                    width: w,
                    height: h,
                    pixels: Arc::from(pixels),
                },
                size,
            )
        {
            result.insert(spec.clone(), raw_to_tuple(normalized));
        }
    }
    result
}

// ============================================================================================
// ApplicationIconProvider (`Services/App/ApplicationIconProvider.cs` parity)
// ============================================================================================

/// Canonical icon sizes used by `SnappedSize` (12..256 px).
pub const ICON_SIZES: &[u32] = &[12, 16, 20, 24, 32, 40, 48, 64, 96, 128, 256];

/// Snaps a requested DIP size to the nearest supported rasterisation size, clamped to 12..256.
pub fn snap_icon_size(requested: u32) -> u32 {
    let requested = requested.clamp(12, 256);
    ICON_SIZES
        .iter()
        .copied()
        .min_by_key(|s| ((*s as i64) - (requested as i64)).abs())
        .unwrap_or(64)
}

/// Converts straight (unassociated) BGRA32 to premultiplied alpha in place.
pub fn straight_to_premultiplied(pixels: &mut [u8]) {
    for p in pixels.as_chunks_mut::<4>().0 {
        let a = p[3] as u16;
        if a != 255 {
            p[0] = ((p[0] as u16 * a + 127) / 255) as u8;
            p[1] = ((p[1] as u16 * a + 127) / 255) as u8;
            p[2] = ((p[2] as u16 * a + 127) / 255) as u8;
        }
    }
}

/// Converts premultiplied BGRA32 back to straight alpha (clamped; loses information for a=0).
pub fn premultiplied_to_straight(pixels: &mut [u8]) {
    for p in pixels.as_chunks_mut::<4>().0 {
        let a = p[3] as u16;
        if a == 0 {
            p[0] = 0;
            p[1] = 0;
            p[2] = 0;
        } else if a != 255 {
            let b = (p[0] as u16).min(a);
            let g = (p[1] as u16).min(a);
            let r = (p[2] as u16).min(a);
            p[0] = ((b * 255 + a / 2) / a).min(255) as u8;
            p[1] = ((g * 255 + a / 2) / a).min(255) as u8;
            p[2] = ((r * 255 + a / 2) / a).min(255) as u8;
        }
    }
}

/// Key for a hover icon variant (`ConditionalWeakTable` replacement).
pub fn hover_variant_key(spec: &str, hovered: bool) -> String {
    if hovered {
        format!("{spec}#hover")
    } else {
        spec.to_owned()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use windows_sys::Win32::Graphics::Gdi::{CreateBitmap, CreateDIBSection};
    use windows_sys::Win32::UI::WindowsAndMessaging::CreateIconIndirect;

    #[test]
    fn missing_pipes_do_not_leak_the_worker_process() {
        // `Stdio::null()` makes both `take()` calls fail, exercising the error
        // path that used to drop a live `Child` without killing or waiting.
        let child = Command::new("powershell.exe")
            .args([
                "-NoProfile",
                "-NonInteractive",
                "-Command",
                "Start-Sleep -Seconds 30",
            ])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .creation_flags(CREATE_NO_WINDOW)
            .spawn();
        let Ok(child) = child else {
            return; // process creation is environment-dependent; nothing to assert
        };
        assert!(Worker::from_child(child).is_none());
    }

    #[test]
    fn failed_spawn_cleanup_kills_and_reaps_the_child() {
        let Ok(mut child) = Command::new("powershell.exe")
            .args([
                "-NoProfile",
                "-NonInteractive",
                "-Command",
                "Start-Sleep -Seconds 30",
            ])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .creation_flags(CREATE_NO_WINDOW)
            .spawn()
        else {
            return;
        };
        assert!(child.try_wait().ok().flatten().is_none());
        cleanup_failed_child(&mut child);
        assert!(
            child.try_wait().ok().flatten().is_some(),
            "cleanup must terminate and reap the process"
        );
    }

    #[test]
    fn visibility_gate_rejects_blank_and_malformed_buffers() {
        let blank = vec![0u8; 4 * 4 * 4];
        assert!(!has_visible_pixels(&blank, 4, 4));
        assert!(!has_visible_pixels(&[255, 0, 0, 255], 0, 1));
        assert!(!has_visible_pixels(&[255, 0, 0, 255], 2, 2));

        let mut visible = blank;
        visible[3] = 1;
        assert!(has_visible_pixels(&visible, 4, 4));
    }

    #[test]
    fn hbitmap_to_bgra_handles_top_down_and_bottom_up_dibsections() {
        for (bi_height, top_down) in [(-2i32, true), (2i32, false)] {
            // SAFETY: creates a 2x2 32bpp DIBSECTION, fills two rows with distinct colors,
            // reads them back via `hbitmap_to_bgra`, and deletes the GDI bitmap.
            unsafe {
                let info = BITMAPINFO {
                    bmiHeader: BITMAPINFOHEADER {
                        biSize: size_of::<BITMAPINFOHEADER>() as u32,
                        biWidth: 2,
                        biHeight: bi_height,
                        biPlanes: 1,
                        biBitCount: 32,
                        biCompression: BI_RGB,
                        ..std::mem::zeroed()
                    },
                    bmiColors: [std::mem::zeroed()],
                };
                let mut bits: *mut core::ffi::c_void = std::ptr::null_mut();
                let hbm = CreateDIBSection(
                    std::ptr::null_mut(),
                    &info,
                    DIB_RGB_COLORS,
                    &mut bits,
                    std::ptr::null_mut(),
                    0,
                );
                assert!(!hbm.is_null());
                assert!(!bits.is_null());

                let slice = std::slice::from_raw_parts_mut(bits.cast::<u8>(), 2 * 2 * 4);
                // Memory row 0 = blue, memory row 1 = red.
                let row0 = [255u8, 0, 0, 255, 255, 0, 0, 255];
                let row1 = [0u8, 0, 255, 255, 0, 0, 255, 255];
                slice[..8].copy_from_slice(&row0);
                slice[8..16].copy_from_slice(&row1);

                let icon = hbitmap_to_bgra(hbm, false).expect("valid DIBSECTION");
                SysDeleteObject(hbm);

                assert_eq!((icon.width, icon.height), (2, 2));
                let expected_top = if top_down {
                    [255, 0, 0, 255]
                } else {
                    [0, 0, 255, 255]
                };
                let expected_bot = if top_down {
                    [0, 0, 255, 255]
                } else {
                    [255, 0, 0, 255]
                };
                assert_eq!(&icon.pixels[0..4], &expected_top);
                assert_eq!(&icon.pixels[8..12], &expected_bot);
            }
        }
    }

    #[test]
    fn hbitmap_to_bgra_rejects_zero_alpha_dibsection() {
        // SAFETY: creates a 2x2 32bpp DIBSECTION initialized to zero alpha and verifies rejection.
        unsafe {
            let info = BITMAPINFO {
                bmiHeader: BITMAPINFOHEADER {
                    biSize: size_of::<BITMAPINFOHEADER>() as u32,
                    biWidth: 2,
                    biHeight: -2,
                    biPlanes: 1,
                    biBitCount: 32,
                    biCompression: BI_RGB,
                    ..std::mem::zeroed()
                },
                bmiColors: [std::mem::zeroed()],
            };
            let mut bits: *mut core::ffi::c_void = std::ptr::null_mut();
            let hbm = CreateDIBSection(
                std::ptr::null_mut(),
                &info,
                DIB_RGB_COLORS,
                &mut bits,
                std::ptr::null_mut(),
                0,
            );
            assert!(!hbm.is_null());
            let slice = std::slice::from_raw_parts_mut(bits.cast::<u8>(), 16);
            slice.fill(0);
            let icon = hbitmap_to_bgra(hbm, true);
            SysDeleteObject(hbm);
            assert!(icon.is_none(), "all-transparent DIB must be rejected");
        }
    }

    #[test]
    fn monochrome_hicon_decodes_and_xor_masks() {
        // 8x8 monochrome icon -> 8x16 1bpp mask bitmap (16-bit word-aligned rows = 2 bytes/row).
        // Upper 8 rows = AND mask: row 0 has leftmost 4 pixels opaque (0), rightmost 4 transparent (1).
        // Lower 8 rows = XOR mask: row 0 has bits 0..2 black (0), bits 2..4 white (1).
        let mut mask_bits = [0u8; 32];
        // Upper half (AND): all transparent (0xFF) except row 0 high nibble = 0x0F.
        for row in 0..8 {
            mask_bits[row * 2] = if row == 0 { 0x0F } else { 0xFF };
        }
        // Lower half (XOR): row 0 bits 2..4 = 1 -> 0b0011_0000 = 0x30.
        mask_bits[8 * 2] = 0x30;

        // SAFETY: builds a monochrome HICON with `hbmColor == null`, decodes it, and frees handles.
        unsafe {
            let hbm_mask = CreateBitmap(8, 16, 1, 1, mask_bits.as_ptr().cast());
            assert!(!hbm_mask.is_null());
            let ii = ICONINFO {
                fIcon: 1,
                xHotspot: 0,
                yHotspot: 0,
                hbmMask: hbm_mask,
                hbmColor: std::ptr::null_mut(),
            };
            let hicon = CreateIconIndirect(&ii);
            SysDeleteObject(hbm_mask);
            assert!(!hicon.is_null());

            let icon = hicon_to_bgra(hicon).expect("monochrome HICON must decode");
            DestroyIcon(hicon);

            assert_eq!((icon.width, icon.height), (8, 8));
            // Pixel (0,0): AND=0, XOR=0 -> opaque black.
            assert_eq!(&icon.pixels[0..4], &[0, 0, 0, 255]);
            // Pixel (2,0): AND=0, XOR=1 -> opaque white.
            assert_eq!(&icon.pixels[8..12], &[255, 255, 255, 255]);
            // Pixel (4,0): AND=1, XOR=0 -> transparent.
            assert_eq!(&icon.pixels[16..20], &[0, 0, 0, 0]);
        }
    }

    #[test]
    fn normalize_to_square_preserves_aspect_ratio_and_centers() {
        // 8x4 solid green bar -> scaled to 16x8, centered vertically at y=4..12 on 16x16 canvas.
        let mut pixels = Vec::with_capacity(8 * 4 * 4);
        for _ in 0..(8 * 4) {
            pixels.extend_from_slice(&[0, 255, 0, 255]);
        }
        let icon = RawIcon {
            width: 8,
            height: 4,
            pixels: Arc::from(pixels),
        };
        let norm = normalize_to_square(icon, 16).expect("normalized square");
        assert_eq!((norm.width, norm.height), (16, 16));
        // Top padding row (y=0) is transparent.
        assert_eq!(norm.pixels[3], 0);
        // Center row (y=8, x=8) is opaque green.
        let center = (8 * 16 + 8) * 4;
        assert_eq!(norm.pixels[center + 1], 255);
        assert_eq!(norm.pixels[center + 3], 255);
        // Bottom padding row (y=15) is transparent.
        let bottom = (15 * 16 + 8) * 4;
        assert_eq!(norm.pixels[bottom + 3], 0);
    }

    #[test]
    fn mrt_asset_selection_prefers_unplated_targetsize_then_scale() {
        let candidates = vec![
            PathBuf::from(r"C:\Pkg\Assets\Square44x44Logo.png"),
            PathBuf::from(r"C:\Pkg\Assets\Square44x44Logo.scale-100.png"),
            PathBuf::from(r"C:\Pkg\Assets\Square44x44Logo.scale-150.png"),
            PathBuf::from(r"C:\Pkg\Assets\Square44x44Logo.scale-200.png"),
            PathBuf::from(r"C:\Pkg\Assets\Square44x44Logo.targetsize-48_altform-unplated.png"),
            PathBuf::from(r"C:\Pkg\Assets\Square44x44Logo.targetsize-64_altform-unplated.png"),
            PathBuf::from(r"C:\Pkg\Assets\Square44x44Logo.targetsize-256_altform-unplated.png"),
            PathBuf::from(
                r"C:\Pkg\Assets\Square44x44Logo.targetsize-64_altform-unplated_contrast-white.png",
            ),
            PathBuf::from(r"C:\Pkg\Assets\OtherLogo.targetsize-64_altform-unplated.png"),
        ];

        let ranked_64 = rank_mrt_asset_candidates("Square44x44Logo", &candidates, 64);
        assert_eq!(
            ranked_64[0],
            PathBuf::from(r"C:\Pkg\Assets\Square44x44Logo.targetsize-64_altform-unplated.png")
        );
        assert_eq!(
            ranked_64[1],
            PathBuf::from(r"C:\Pkg\Assets\Square44x44Logo.targetsize-256_altform-unplated.png")
        );
        assert_eq!(
            ranked_64[2],
            PathBuf::from(r"C:\Pkg\Assets\Square44x44Logo.targetsize-48_altform-unplated.png")
        );
        assert!(!ranked_64.iter().any(|p| {
            p.file_name()
                .and_then(|n| n.to_str())
                .is_some_and(|n| n.starts_with("OtherLogo"))
        }));

        let scale_only = vec![
            PathBuf::from(r"C:\Pkg\Assets\Square44x44Logo.scale-100.png"),
            PathBuf::from(r"C:\Pkg\Assets\Square44x44Logo.scale-150.png"),
            PathBuf::from(r"C:\Pkg\Assets\Square44x44Logo.scale-200.png"),
        ];
        let ranked_scale = rank_mrt_asset_candidates("Square44x44Logo", &scale_only, 64);
        assert_eq!(
            ranked_scale[0],
            PathBuf::from(r"C:\Pkg\Assets\Square44x44Logo.scale-200.png")
        );
        assert_eq!(
            ranked_scale[1],
            PathBuf::from(r"C:\Pkg\Assets\Square44x44Logo.scale-150.png")
        );
        assert_eq!(
            ranked_scale[2],
            PathBuf::from(r"C:\Pkg\Assets\Square44x44Logo.scale-100.png")
        );
    }

    #[test]
    fn oticon06_invalidates_oticon05_and_rejects_zero_alpha_payloads() {
        let valid = RawIcon {
            width: 2,
            height: 2,
            pixels: Arc::from(vec![
                10, 20, 30, 255, 10, 20, 30, 255, 10, 20, 30, 255, 10, 20, 30, 255,
            ]),
        };
        let encoded = encode_cache_bytes(&valid, 42).expect("encode valid");
        assert_eq!(&encoded[..8], b"OTICON06");
        let decoded = decode_cache_bytes(encoded.clone(), 42, Some(2)).expect("decode valid");
        assert_eq!((decoded.width, decoded.height), (2, 2));

        // Legacy OTICON05 magic must be rejected.
        let mut legacy = encoded.clone();
        legacy[..8].copy_from_slice(b"OTICON05");
        assert!(decode_cache_bytes(legacy, 42, Some(2)).is_none());

        // Zero-alpha payload must be rejected on both encode and decode.
        let blank = RawIcon {
            width: 2,
            height: 2,
            pixels: Arc::from(vec![0u8; 16]),
        };
        assert!(encode_cache_bytes(&blank, 42).is_none());
        let mut corrupt_blank = encoded;
        corrupt_blank[CACHE_HEADER_LEN..].fill(0);
        assert!(decode_cache_bytes(corrupt_blank, 42, Some(2)).is_none());
    }

    #[test]
    fn instant_generic_icon_for_path_returns_visible_icons_without_touching_disk() {
        let fake_dir = Path::new(r"Z:\NonExistent\OneDriveCloudOnly\Folder");
        let (dw, dh, dpixels) =
            instant_generic_icon_for_path(fake_dir, true).expect("generic folder icon");
        assert_eq!((dw, dh), (ICON_PX, ICON_PX));
        assert!(has_visible_pixels(&dpixels, dw, dh));

        let fake_file = Path::new(r"Z:\NonExistent\OneDriveCloudOnly\document.txt");
        let (fw, fh, fpixels) =
            instant_generic_icon_for_path(fake_file, false).expect("generic txt icon");
        assert_eq!((fw, fh), (ICON_PX, ICON_PX));
        assert!(has_visible_pixels(&fpixels, fw, fh));
    }

    #[test]
    fn memory_cache_stores_and_retrieves_entries() {
        let spec = r"C:\Test\MemoryCacheHit.exe";
        let icon = RawIcon {
            width: 4,
            height: 4,
            pixels: Arc::from(vec![255u8; 4 * 4 * 4]),
        };
        insert_memory_cache(spec, 4, &icon);
        let (w, h, px) = lookup_memory_cached(r"c:\test\memorycachehit.exe", 4)
            .expect("case-insensitive memory cache hit");
        assert_eq!((w, h), (4, 4));
        assert_eq!(px.len(), 64);
    }
}

#[cfg(test)]
mod icon_provider_tests {
    use super::*;

    #[test]
    fn snaps_sizes_to_reference_ladder() {
        assert_eq!(snap_icon_size(0), 12);
        assert_eq!(snap_icon_size(13), 12);
        assert_eq!(snap_icon_size(30), 32);
        assert_eq!(snap_icon_size(200), 256);
        assert_eq!(snap_icon_size(9999), 256);
    }

    #[test]
    fn premultiplied_roundtrip() {
        let mut pixels = vec![200u8, 100, 50, 128, 255, 255, 255, 255];
        let original = pixels.clone();
        straight_to_premultiplied(&mut pixels);
        assert_eq!(pixels[3], 128);
        assert!(pixels[0] <= original[0]);
        premultiplied_to_straight(&mut pixels);
        // 8-bit premultiply roundtrip loses at most one level per channel.
        for (got, want) in pixels.iter().zip(original.iter()) {
            assert!(got.abs_diff(*want) <= 1, "{got} vs {want}");
        }
    }

    #[test]
    fn cache_dir_uses_the_shared_rust_cache_folder() {
        assert_eq!(cache_dir(), opentaskbar_core::paths::rust_cache_dir());
        assert!(cache_dir().ends_with(Path::new("rust-cache").join("icon-cache")));
        assert_eq!(cache_path("app.exe").parent(), Some(cache_dir().as_path()));
    }

    #[test]
    fn folder_fingerprint_ignores_children_but_tracks_desktop_ini() {
        let dir = std::env::temp_dir().join(format!(
            "opentaskbar-icon-fp-{}-{:x}",
            std::process::id(),
            hash_key(&format!("{:?}", std::time::SystemTime::now()))
        ));
        fs::create_dir_all(&dir).unwrap();
        let spec = dir.to_string_lossy().into_owned();
        let empty = fingerprint(&spec);
        assert!(empty.is_some());

        fs::write(dir.join("download.bin"), b"payload").unwrap();
        assert_eq!(
            fingerprint(&spec),
            empty,
            "a new child must not bust the folder icon cache"
        );

        fs::write(dir.join("desktop.ini"), b"[.ShellClassInfo]\r\n").unwrap();
        assert_ne!(
            fingerprint(&spec),
            empty,
            "desktop.ini decides the folder icon"
        );

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn hover_variants_have_distinct_keys() {
        assert_ne!(
            hover_variant_key("app.exe", false),
            hover_variant_key("app.exe", true)
        );
    }
}
