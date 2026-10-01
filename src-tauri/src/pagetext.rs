//! "What is on the page": reads the text of a browser window as it is drawn,
//! with Windows' built-in OCR, entirely on this machine -- nothing is saved or
//! sent anywhere. The window is painted into memory (`PrintWindow`, so it works
//! even when other windows cover it), recognised line by line, and then a small
//! ensemble of decision trees votes for each line: page prose or interface
//! chrome (buttons, menus, counters)? The first paragraph of the main column
//! becomes the summary, and the page's own action buttons ("Share", "Reply", ...)
//! become shortcuts: a click is delivered to the browser window at that spot.
//!
//! The trees are hand-set, not trained: there is no labelled data to learn from.
//! They share the same features a trained forest would use.

use serde::Serialize;
use windows::Graphics::Imaging::{BitmapPixelFormat, SoftwareBitmap};
use windows::Media::Ocr::OcrEngine;
use windows::Storage::Streams::DataWriter;
use windows::Win32::Foundation::{HWND, RECT};
use windows::Win32::Graphics::Gdi::{
    CreateCompatibleBitmap, CreateCompatibleDC, DeleteDC, DeleteObject, GetDC, GetDIBits, ReleaseDC, SelectObject,
    BITMAPINFO, BITMAPINFOHEADER, BI_RGB, DIB_RGB_COLORS, HGDIOBJ,
};
use windows::Win32::Storage::Xps::{PrintWindow, PRINT_WINDOW_FLAGS};
use windows::Win32::UI::HiDpi::GetDpiForWindow;
use windows::Win32::UI::WindowsAndMessaging::GetWindowRect;

#[derive(Serialize, Clone, Default)]
pub struct PagePreview {
    /// the opening of the page's main text
    pub lead: Option<String>,
    /// the page's buttons that are safe to press from the island
    pub buttons: Vec<PageButton>,
}

#[derive(Serialize, Clone, Debug)]
pub struct PageButton {
    pub label: String,
    /// where it was read, relative to the window's top-left (physical px)
    pub x: f32,
    pub y: f32,
}

#[derive(Clone, Debug)]
pub struct Line {
    pub text: String,
    pub x: f32,
    pub y: f32,
    pub w: f32,
    pub h: f32,
}

// ---- capture + recognise ---------------------------------------------------

struct Shot {
    bgra: Vec<u8>,
    w: i32,
    h: i32,
    dpi_scale: f32,
}

fn capture(hwnd: HWND) -> Option<Shot> {
    unsafe {
        let mut r = RECT::default();
        GetWindowRect(hwnd, &mut r).ok()?;
        let (w, h) = (r.right - r.left, r.bottom - r.top);
        if w < 300 || h < 300 || w > 7000 || h > 5000 {
            return None;
        }
        let screen = GetDC(None);
        let mem = CreateCompatibleDC(screen);
        let bmp = CreateCompatibleBitmap(screen, w, h);
        let old = SelectObject(mem, bmp);
        // PW_RENDERFULLCONTENT: also captures GPU-composited browser windows
        let painted = PrintWindow(hwnd, mem, PRINT_WINDOW_FLAGS(2)).as_bool();
        let mut bi = BITMAPINFO {
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
        let mut buf = vec![0u8; (w as usize) * (h as usize) * 4];
        let rows = GetDIBits(mem, bmp, 0, h as u32, Some(buf.as_mut_ptr() as *mut _), &mut bi, DIB_RGB_COLORS);
        SelectObject(mem, old);
        let _ = DeleteObject(HGDIOBJ(bmp.0));
        let _ = DeleteDC(mem);
        ReleaseDC(None, screen);
        if !painted || rows == 0 {
            return None;
        }
        // GDI leaves alpha at 0; the OCR bitmap wants it opaque
        for px in buf.chunks_exact_mut(4) {
            px[3] = 255;
        }
        // an all-black paint means the window refused to draw itself
        if buf.iter().step_by(4099).all(|b| *b == 0) {
            return None;
        }
        let dpi = GetDpiForWindow(hwnd).max(96) as f32;
        Some(Shot { bgra: buf, w, h, dpi_scale: dpi / 96.0 })
    }
}

fn recognise(shot: &Shot) -> Option<Vec<Line>> {
    let engine = OcrEngine::TryCreateFromUserProfileLanguages().ok()?;
    if shot.w as u32 > OcrEngine::MaxImageDimension().ok()? || shot.h as u32 > OcrEngine::MaxImageDimension().ok()? {
        return None;
    }
    let writer = DataWriter::new().ok()?;
    writer.WriteBytes(&shot.bgra).ok()?;
    let buffer = writer.DetachBuffer().ok()?;
    let bitmap = SoftwareBitmap::CreateCopyFromBuffer(&buffer, BitmapPixelFormat::Bgra8, shot.w, shot.h).ok()?;
    let result = engine.RecognizeAsync(&bitmap).ok()?.get().ok()?;
    let mut out = Vec::new();
    for line in result.Lines().ok()? {
        let text = line.Text().ok()?.to_string_lossy();
        let (mut x0, mut y0, mut x1, mut y1) = (f32::MAX, f32::MAX, 0f32, 0f32);
        for word in line.Words().ok()? {
            let b = word.BoundingRect().ok()?;
            x0 = x0.min(b.X);
            y0 = y0.min(b.Y);
            x1 = x1.max(b.X + b.Width);
            y1 = y1.max(b.Y + b.Height);
        }
        if x1 > x0 && !text.trim().is_empty() {
            out.push(Line { text: text.trim().to_string(), x: x0, y: y0, w: x1 - x0, h: y1 - y0 });
        }
    }
    Some(out)
}

/// Cheap fingerprint of a capture, so an unchanged page is not read again.
fn fingerprint(shot: &Shot) -> u64 {
    let mut hash: u64 = 1469598103934665603;
    for b in shot.bgra.iter().step_by(1009) {
        hash = (hash ^ *b as u64).wrapping_mul(1099511628211);
    }
    hash
}

/// (summary, fingerprint of what was read). `None` when the window can't be read;
/// the fingerprint lets the caller skip the OCR when nothing on screen changed.
pub fn read_window(hwnd: HWND, page_title: &str, last_fingerprint: Option<u64>) -> Option<(Option<PagePreview>, u64)> {
    let shot = capture(hwnd)?;
    let fp = fingerprint(&shot);
    if last_fingerprint == Some(fp) {
        return Some((None, fp));
    }
    let lines = recognise(&shot)?;
    // the browser's own tab strip + address bar are the top of the window
    let content_top = 105.0 * shot.dpi_scale;
    Some((extract(&lines, shot.w as f32, content_top, page_title), fp))
}

// ---- which lines are the page? ----------------------------------------------

struct Feat {
    chars: usize,
    words: usize,
    h_ratio: f32,
    x_rel: f32,
    alpha: f32,
    digit: f32,
    upper: f32,
    ends_punct: bool,
    ui_word: bool,
    link_like: bool,
}

const UI_WORDS: &[&str] = &[
    "reply", "share", "join", "log in", "login", "sign up", "sign in", "comments", "comment", "award", "upvote",
    "downvote", "home", "popular", "explore", "menu", "search", "follow", "following", "subscribe", "more", "save",
    "report", "hide", "open", "get app", "notifications", "settings", "inbox", "create", "advertise", "help",
    "cookies", "privacy", "terms", "sort by", "best", "top", "new", "old", "like", "likes", "views", "back",
];

fn features(l: &Line, h_med: f32, width: f32) -> Feat {
    let chars = l.text.chars().count();
    let letters = l.text.chars().filter(|c| c.is_alphabetic()).count();
    let digits = l.text.chars().filter(|c| c.is_ascii_digit()).count();
    let upper = l.text.chars().filter(|c| c.is_uppercase()).count();
    let lower_text = l.text.to_lowercase();
    let words = l.text.split_whitespace().count();
    Feat {
        chars,
        words,
        h_ratio: l.h / h_med.max(1.0),
        x_rel: l.x / width.max(1.0),
        alpha: letters as f32 / chars.max(1) as f32,
        digit: digits as f32 / chars.max(1) as f32,
        upper: upper as f32 / letters.max(1) as f32,
        ends_punct: l.text.trim_end().ends_with(['.', '!', '?', ',', ':', ';', '"', '\u{201d}']),
        ui_word: words <= 3 && UI_WORDS.iter().any(|w| lower_text == *w || lower_text.starts_with(&format!("{w} "))),
        link_like: lower_text.contains("http") || lower_text.contains("www.") || l.text.contains('@'),
    }
}

/// Seven small decision trees, each looking at different features; a line is
/// page prose when most of them say so. Text far bigger than the rest is a
/// heading or logo, never prose.
fn prose_votes(f: &Feat) -> usize {
    if f.h_ratio > 1.35 {
        return 0;
    }
    let trees: [bool; 7] = [
        // 1: interface words never; else length in words
        if f.ui_word { false } else { f.words >= 6 },
        // 2: mostly letters, then length / sentence punctuation
        if f.alpha < 0.7 { false } else if f.chars >= 40 { true } else { f.ends_punct && f.chars >= 25 },
        // 3: normal-sized text with a few words
        f.words >= 5 && f.h_ratio > 0.7,
        // 4: counters and timestamps are numbers, prose is not
        if f.digit > 0.25 { false } else { f.chars >= 30 },
        // 5: SHOUTING is chrome
        if f.upper > 0.6 { false } else { f.words >= 7 || (f.ends_punct && f.words >= 4) },
        // 6: fine print is small
        if f.h_ratio < 0.75 { false } else { f.words >= 6 },
        // 7: links and addresses are not sentences
        if f.link_like { false } else { f.chars >= 35 && f.x_rel < 0.92 },
    ];
    trees.iter().filter(|t| **t).count()
}

fn median(mut v: Vec<f32>) -> f32 {
    if v.is_empty() {
        return 1.0;
    }
    v.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
    v[v.len() / 2]
}

fn norm(s: &str) -> String {
    s.chars().filter(|c| c.is_alphanumeric()).flat_map(|c| c.to_lowercase()).collect()
}

/// The page's text as far as it can be told apart from the interface.
pub fn extract(lines: &[Line], width: f32, content_top: f32, page_title: &str) -> Option<PagePreview> {
    let body: Vec<&Line> = lines.iter().filter(|l| l.y >= content_top).collect();
    if body.len() < 3 {
        return None;
    }
    let h_med = median(body.iter().map(|l| l.h).collect());
    let title = norm(page_title);
    let is_title = |l: &Line| {
        let n = norm(&l.text);
        n.len() >= 6 && !title.is_empty() && (title.contains(&n) || n.contains(&title))
    };

    let mut prose: Vec<&Line> = Vec::new();
    for l in &body {
        let f = features(l, h_med, width);
        if prose_votes(&f) >= 4 && !is_title(l) {
            prose.push(l);
        }
    }

    // the main column: where most prose starts horizontally
    const BUCKET: f32 = 24.0;
    let mut weight: std::collections::HashMap<i32, usize> = std::collections::HashMap::new();
    for l in &prose {
        *weight.entry((l.x / BUCKET).round() as i32).or_insert(0) += l.text.chars().count();
    }
    let column_x = weight.into_iter().max_by_key(|(_, w)| *w).map(|(b, _)| b as f32 * BUCKET)?;
    let in_column = |l: &Line| (l.x - column_x).abs() <= 2.0 * BUCKET;

    let mut column: Vec<&Line> = prose.into_iter().filter(|l| in_column(l)).collect();
    column.sort_by(|a, b| a.y.partial_cmp(&b.y).unwrap_or(std::cmp::Ordering::Equal));

    // opening paragraph(s): stop at a big vertical gap once there is enough
    let mut lead = String::new();
    let mut prev_bottom: Option<f32> = None;
    for l in &column {
        if let Some(pb) = prev_bottom {
            if l.y - pb > 1.8 * h_med && lead.chars().count() >= 60 {
                break;
            }
        }
        if !lead.is_empty() {
            lead.push(' ');
        }
        lead.push_str(&l.text);
        prev_bottom = Some(l.y + l.h);
        if lead.chars().count() >= 260 {
            break;
        }
    }
    let lead = (!lead.is_empty()).then(|| {
        if lead.chars().count() > 320 {
            format!("{}\u{2026}", lead.chars().take(317).collect::<String>().trim_end())
        } else {
            lead
        }
    });

    let buttons = buttons(&body);
    (lead.is_some() || !buttons.is_empty()).then_some(PagePreview { lead, buttons })
}

/// Buttons that are safe to press on someone's behalf. Anything that spends
/// money, sends, posts, deletes or signs in is deliberately not on the list.
const SAFE_BUTTONS: &[&str] = &[
    "share", "reply", "join", "follow", "subscribe", "save", "like", "upvote", "play", "pause", "next", "previous",
    "download", "show more", "load more", "read more", "see more", "view more", "comments", "copy link",
    "bookmark", "expand", "collapse", "skip", "watch later", "open in app",
];

fn button_key(text: &str) -> String {
    text.trim().trim_matches(|c: char| !c.is_alphanumeric()).to_lowercase()
}

/// The page's own buttons among the recognised lines: short, on the safe list.
fn buttons(body: &[&Line]) -> Vec<PageButton> {
    let mut found: Vec<PageButton> = Vec::new();
    for l in body {
        let key = button_key(&l.text);
        if l.w > 320.0 || !SAFE_BUTTONS.contains(&key.as_str()) {
            continue;
        }
        if found.iter().any(|b| button_key(&b.label) == key) {
            continue;
        }
        let mut label = key.clone();
        if let Some(c) = label.get_mut(0..1) {
            c.make_ascii_uppercase();
        }
        found.push(PageButton { label, x: l.x + l.w / 2.0, y: l.y + l.h / 2.0 });
    }
    found.sort_by(|a, b| {
        (a.y / 40.0).floor().partial_cmp(&(b.y / 40.0).floor()).unwrap_or(std::cmp::Ordering::Equal)
            .then(a.x.partial_cmp(&b.x).unwrap_or(std::cmp::Ordering::Equal))
    });
    found.truncate(5);
    found
}

/// Press the button called `label` in the browser window. The page is read again
/// first (it may have scrolled since the card was drawn) and the match nearest to
/// where the button used to be gets the click. The click is posted to the window,
/// so it lands even when the island or another window covers that spot, and the
/// real mouse pointer never moves.
pub fn click_button(hwnd: HWND, label: &str, near_x: f32, near_y: f32) -> bool {
    use windows::Win32::Foundation::{LPARAM, POINT, WPARAM};
    use windows::Win32::Graphics::Gdi::ScreenToClient;
    use windows::Win32::UI::WindowsAndMessaging::{
        PostMessageW, WM_LBUTTONDOWN, WM_LBUTTONUP, WM_MOUSEMOVE,
    };
    let Some(shot) = capture(hwnd) else { return false };
    let Some(lines) = recognise(&shot) else { return false };
    let want = button_key(label);
    let best = lines
        .iter()
        .filter(|l| button_key(&l.text) == want && l.w <= 320.0)
        .min_by(|a, b| {
            let da = (a.x + a.w / 2.0 - near_x).hypot(a.y + a.h / 2.0 - near_y);
            let db = (b.x + b.w / 2.0 - near_x).hypot(b.y + b.h / 2.0 - near_y);
            da.partial_cmp(&db).unwrap_or(std::cmp::Ordering::Equal)
        });
    let Some(l) = best else { return false };
    unsafe {
        let mut win = RECT::default();
        if GetWindowRect(hwnd, &mut win).is_err() {
            return false;
        }
        let mut pt = POINT { x: win.left + (l.x + l.w / 2.0) as i32, y: win.top + (l.y + l.h / 2.0) as i32 };
        if !ScreenToClient(hwnd, &mut pt).as_bool() {
            return false;
        }
        let pos = LPARAM(((pt.y as u32 & 0xFFFF) << 16 | (pt.x as u32 & 0xFFFF)) as isize);
        let held = WPARAM(1) /* MK_LBUTTON */;
        let ok = PostMessageW(hwnd, WM_MOUSEMOVE, WPARAM(0), pos).is_ok()
            && PostMessageW(hwnd, WM_LBUTTONDOWN, held, pos).is_ok()
            && PostMessageW(hwnd, WM_LBUTTONUP, WPARAM(0), pos).is_ok();
        ok
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Manual: opens a test page in its own Edge app window and clicks its "Share" button.
    #[test]
    #[ignore]
    fn live_click() {
        let page = std::env::var("CLICKTEST_PAGE").expect("CLICKTEST_PAGE");
        std::process::Command::new(r"C:/Program Files (x86)/Microsoft/Edge/Application/msedge.exe").arg(format!("--app=file:///{page}")).spawn().expect("edge");
        let find = || {
            crate::scan::visible_windows().into_iter().find(|w| w.title.contains("ClickTest"))
        };
        let mut w = None;
        for _ in 0..30 {
            std::thread::sleep(std::time::Duration::from_millis(500));
            w = find();
            if w.is_some() {
                break;
            }
        }
        let w = w.expect("test window never appeared");
        std::thread::sleep(std::time::Duration::from_millis(1500));
        let ok = click_button(w.hwnd, "Share", 0.0, 0.0);
        std::thread::sleep(std::time::Duration::from_millis(800));
        let after = find().map(|w| w.title).unwrap_or_default();
        println!("click_button={ok} title_after={after}");
        unsafe {
            let _ = windows::Win32::UI::WindowsAndMessaging::PostMessageW(
                w.hwnd, windows::Win32::UI::WindowsAndMessaging::WM_CLOSE,
                windows::Win32::Foundation::WPARAM(0), windows::Win32::Foundation::LPARAM(0));
        }
    }

    /// Manual: reads the browser window that is open right now and prints the gist.
    /// `cargo test --release live_read -- --ignored --nocapture`
    #[test]
    #[ignore]
    fn live_read() {
        let w = crate::scan::visible_windows()
            .into_iter()
            .find(|w| !w.minimized && w.exe.to_lowercase().ends_with("msedge.exe"))
            .expect("no Edge window");
        let title = crate::browse::clean_title(&w.title);
        let t0 = std::time::Instant::now();
        let (preview, _) = read_window(w.hwnd, &title, None).expect("capture/ocr failed");
        println!("took {:?}", t0.elapsed());
        match preview {
            Some(p) => println!("lead: {:?}
buttons: {:?}", p.lead, p.buttons),
            None => println!("nothing extracted"),
        }
    }

    fn line(text: &str, x: f32, y: f32, h: f32) -> Line {
        Line { text: text.to_string(), x, y, w: text.len() as f32 * 7.0, h }
    }

    #[test]
    fn picks_prose_over_interface() {
        let lines = vec![
            line("Home", 20.0, 200.0, 14.0),
            line("Popular", 20.0, 230.0, 14.0),
            line("How's my gloves reacting around the world?", 90.0, 160.0, 26.0),
            line("Question", 90.0, 200.0, 13.0),
            line("Hey guys, let me know what you think about glove's animation. And please", 90.0, 640.0, 15.0),
            line("let me know if you have any better idea to make it better or add another feature.", 90.0, 662.0, 15.0),
            line("99", 100.0, 700.0, 14.0),
            line("Share", 300.0, 700.0, 14.0),
            
        ];
        let p = extract(&lines, 1400.0, 105.0, "How's my gloves reacting around the world?").unwrap();
        let lead = p.lead.unwrap();
        assert!(lead.starts_with("Hey guys, let me know"), "{lead}");
        assert!(lead.contains("another feature"));
        assert!(!lead.contains("Home") && !lead.contains("Share"));
        let labels: Vec<_> = p.buttons.iter().map(|b| b.label.as_str()).collect();
        assert_eq!(labels, vec!["Share"]);
    }

    #[test]
    fn nothing_readable_gives_nothing() {
        let lines = vec![line("Home", 20.0, 200.0, 14.0), line("Menu", 20.0, 240.0, 14.0)];
        assert!(extract(&lines, 1400.0, 105.0, "x").is_none());
    }
}
