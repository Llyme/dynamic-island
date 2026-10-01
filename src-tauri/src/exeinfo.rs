//! Friendly names + icons for executables -- port of `extract_exe_icon` and
//! the FileDescription/ProductName lookup from the Python version. Results are
//! cached per exe path since both lookups touch the disk / shell.

use std::collections::HashMap;
use std::ffi::c_void;
use std::sync::Mutex;
use windows::core::PCWSTR;
use windows::Win32::Graphics::Gdi::{
    DeleteObject, GetDC, GetDIBits, GetObjectW, ReleaseDC, BITMAP, BITMAPINFO, BITMAPINFOHEADER,
    BI_RGB, DIB_RGB_COLORS, HGDIOBJ,
};
use windows::Win32::Storage::FileSystem::{
    GetFileVersionInfoSizeW, GetFileVersionInfoW, VerQueryValueW,
};
use windows::Win32::UI::Shell::{SHGetFileInfoW, SHFILEINFOW, SHGFI_ICON, SHGFI_LARGEICON};
use windows::Win32::UI::WindowsAndMessaging::{DestroyIcon, GetIconInfo, ICONINFO};

#[derive(Clone, Default)]
pub struct ExeInfo {
    /// friendly product/description name, None if the binary carries none
    pub name: Option<String>,
    /// `data:image/png;base64,...`
    pub icon: Option<String>,
}

static CACHE: Mutex<Option<HashMap<String, ExeInfo>>> = Mutex::new(None);

/// Cached lookup: computed once per exe path.
pub fn lookup(path: &str) -> ExeInfo {
    let mut guard = CACHE.lock().unwrap();
    let cache = guard.get_or_insert_with(HashMap::new);
    if let Some(hit) = cache.get(path) {
        return hit.clone();
    }
    let info = ExeInfo { name: friendly_name(path), icon: icon_data_url(path) };
    cache.insert(path.to_string(), info.clone());
    info
}

fn wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(std::iter::once(0)).collect()
}

fn query_string(block: &[u8], sub_block: &str) -> Option<String> {
    unsafe {
        let sub = wide(sub_block);
        let mut ptr: *mut c_void = std::ptr::null_mut();
        let mut len = 0u32;
        if !VerQueryValueW(block.as_ptr() as *const c_void, PCWSTR(sub.as_ptr()), &mut ptr, &mut len)
            .as_bool()
            || ptr.is_null()
            || len == 0
        {
            return None;
        }
        let slice = std::slice::from_raw_parts(ptr as *const u16, len as usize);
        let end = slice.iter().position(|&c| c == 0).unwrap_or(slice.len());
        let s = String::from_utf16_lossy(&slice[..end]).trim().to_string();
        if s.is_empty() {
            None
        } else {
            Some(s)
        }
    }
}

/// ProductName (else FileDescription) from the exe's version resource.
/// Generic OS-branded values ("Microsoft Windows Operating System") are
/// rejected so the filename fallback wins for those.
fn friendly_name(path: &str) -> Option<String> {
    unsafe {
        let p = wide(path);
        let size = GetFileVersionInfoSizeW(PCWSTR(p.as_ptr()), None);
        if size == 0 {
            return None;
        }
        let mut block = vec![0u8; size as usize];
        GetFileVersionInfoW(PCWSTR(p.as_ptr()), 0, size, block.as_mut_ptr() as *mut c_void).ok()?;

        // first (language, codepage) pair the resource declares
        let sub = wide("\\VarFileInfo\\Translation");
        let mut ptr: *mut c_void = std::ptr::null_mut();
        let mut len = 0u32;
        let mut langs: Vec<String> = Vec::new();
        if VerQueryValueW(block.as_ptr() as *const c_void, PCWSTR(sub.as_ptr()), &mut ptr, &mut len)
            .as_bool()
            && !ptr.is_null()
            && len >= 4
        {
            let pairs = std::slice::from_raw_parts(ptr as *const u16, (len / 2) as usize);
            for chunk in pairs.chunks_exact(2) {
                langs.push(format!("{:04x}{:04x}", chunk[0], chunk[1]));
            }
        }
        langs.push("040904b0".into()); // en-US unicode, common fallback

        for lang in &langs {
            for field in ["ProductName", "FileDescription"] {
                let Some(v) = query_string(&block, &format!("\\StringFileInfo\\{lang}\\{field}"))
                else {
                    continue;
                };
                let lower = v.to_lowercase();
                if lower.contains("operating system") || lower.starts_with("microsoft® windows") {
                    continue;
                }
                return Some(v);
            }
        }
        None
    }
}

fn icon_data_url(path: &str) -> Option<String> {
    use base64::Engine;
    unsafe {
        let p = wide(path);
        let mut sfi = SHFILEINFOW::default();
        let got = SHGetFileInfoW(
            PCWSTR(p.as_ptr()),
            Default::default(),
            Some(&mut sfi),
            std::mem::size_of::<SHFILEINFOW>() as u32,
            SHGFI_ICON | SHGFI_LARGEICON,
        );
        if got == 0 || sfi.hIcon.is_invalid() {
            return None;
        }
        let mut ii = ICONINFO::default();
        let ok = GetIconInfo(sfi.hIcon, &mut ii).is_ok();
        let result = if ok && !ii.hbmColor.is_invalid() {
            read_icon_rgba(&ii)
        } else {
            None
        };
        if !ii.hbmColor.is_invalid() {
            let _ = DeleteObject(HGDIOBJ(ii.hbmColor.0));
        }
        if !ii.hbmMask.is_invalid() {
            let _ = DeleteObject(HGDIOBJ(ii.hbmMask.0));
        }
        let _ = DestroyIcon(sfi.hIcon);

        let (w, h, rgba) = result?;
        let mut png_bytes = Vec::new();
        {
            let mut enc = png::Encoder::new(&mut png_bytes, w, h);
            enc.set_color(png::ColorType::Rgba);
            enc.set_depth(png::BitDepth::Eight);
            let mut writer = enc.write_header().ok()?;
            writer.write_image_data(&rgba).ok()?;
        }
        Some(format!(
            "data:image/png;base64,{}",
            base64::engine::general_purpose::STANDARD.encode(png_bytes)
        ))
    }
}

unsafe fn read_icon_rgba(ii: &ICONINFO) -> Option<(u32, u32, Vec<u8>)> {
    let mut bmp = BITMAP::default();
    if GetObjectW(
        HGDIOBJ(ii.hbmColor.0),
        std::mem::size_of::<BITMAP>() as i32,
        Some(&mut bmp as *mut _ as *mut c_void),
    ) == 0
    {
        return None;
    }
    let (w, h) = (bmp.bmWidth, bmp.bmHeight);
    if w <= 0 || h <= 0 {
        return None;
    }
    let mut info = BITMAPINFO {
        bmiHeader: BITMAPINFOHEADER {
            biSize: std::mem::size_of::<BITMAPINFOHEADER>() as u32,
            biWidth: w,
            biHeight: -h, // top-down
            biPlanes: 1,
            biBitCount: 32,
            biCompression: BI_RGB.0,
            ..Default::default()
        },
        ..Default::default()
    };
    let mut px = vec![0u8; (w * h * 4) as usize];
    let hdc = GetDC(None);
    let lines = GetDIBits(
        hdc,
        ii.hbmColor,
        0,
        h as u32,
        Some(px.as_mut_ptr() as *mut c_void),
        &mut info,
        DIB_RGB_COLORS,
    );
    ReleaseDC(None, hdc);
    if lines == 0 {
        return None;
    }
    // BGRA -> RGBA; icons without an alpha channel come back all-zero alpha
    let has_alpha = px.chunks_exact(4).any(|c| c[3] != 0);
    for c in px.chunks_exact_mut(4) {
        c.swap(0, 2);
        if !has_alpha {
            c[3] = 255;
        }
    }
    Some((w as u32, h as u32, px))
}
