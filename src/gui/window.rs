#![allow(non_snake_case, dead_code, unused_imports)]

use std::sync::Arc;
use parking_lot::RwLock;
use windows::core::{w, Interface, PCWSTR};
use windows::Win32::Foundation::*;
use windows::Win32::Graphics::Direct2D::Common::*;
use windows::Win32::Graphics::Direct2D::*;
use windows::Win32::Graphics::DirectWrite::*;
use windows::Win32::Graphics::Dwm::*;
use windows::Win32::Graphics::Gdi::*;
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::Storage::FileSystem::{FILE_ATTRIBUTE_DIRECTORY, FILE_ATTRIBUTE_NORMAL};
use windows::Win32::UI::Input::KeyboardAndMouse::*;
use windows::Win32::UI::Shell::*;
use windows::Win32::UI::WindowsAndMessaging::*;
use fastsearch::index::store::IndexStore;
use fastsearch::index::search::{search as do_search, SearchResult};
use windows::Win32::Graphics::Direct2D::ID2D1RenderTarget;

// ── Layout ────────────────────────────────────────────────────────────────────
const WIN_W:      f32 = 700.0;
const WIN_W_I:    i32 = 700;
const BAR_H:      f32 = 74.0;
const BAR_H_I:    i32 = 74;
const ROW_H:      f32 = 58.0;
const ROW_H_I:    i32 = 58;
const STATUS_H:   f32 = 26.0;
const STATUS_H_I: i32 = 26;
const MAX_VIS:    usize = 8;
const PAD:        f32 = 22.0;
const ICON_SZ:    f32 = 32.0;
const ICON_GAP:   f32 = 14.0;

// ── D2D colour helpers ────────────────────────────────────────────────────────
const fn d2c(r: f32, g: f32, b: f32, a: f32) -> D2D1_COLOR_F {
    D2D1_COLOR_F { r, g, b, a }
}
const fn hex(r: u8, g: u8, b: u8) -> D2D1_COLOR_F {
    d2c(r as f32 / 255.0, g as f32 / 255.0, b as f32 / 255.0, 1.0)
}
const fn hexa(r: u8, g: u8, b: u8, a: f32) -> D2D1_COLOR_F {
    d2c(r as f32 / 255.0, g as f32 / 255.0, b as f32 / 255.0, a)
}

const C_BG:           D2D1_COLOR_F = hex(14,  16,  24);
const C_BAR:          D2D1_COLOR_F = hex(20,  22,  34);
const C_ROW_EVEN:     D2D1_COLOR_F = hex(18,  20,  30);
const C_ROW_ODD:      D2D1_COLOR_F = hex(21,  23,  35);
const C_HOV:          D2D1_COLOR_F = hexa(100, 140, 255, 0.10);
const C_SEL:          D2D1_COLOR_F = hexa( 80, 140, 255, 0.22);
const C_SEL_BAR:      D2D1_COLOR_F = hex( 90, 160, 255);
const C_SEP:          D2D1_COLOR_F = hexa(255, 255, 255, 0.05);
const C_STATUS:       D2D1_COLOR_F = hex(12,  14,  22);
const C_TEXT:         D2D1_COLOR_F = hex(230, 235, 250);
const C_TEXT_DIM:     D2D1_COLOR_F = hex(120, 130, 165);
const C_TEXT_SEL:     D2D1_COLOR_F = hex(255, 255, 255);
const C_TEXT_SEL_DIM: D2D1_COLOR_F = hex(170, 200, 255);
const C_ACCENT:       D2D1_COLOR_F = hex(100, 165, 255);
const C_PLACEHOLDER:  D2D1_COLOR_F = hexa(180, 190, 230, 0.28);
const C_WHITE:        D2D1_COLOR_F = hex(255, 255, 255);

const C_BADGE_APP: D2D1_COLOR_F = hexa( 60, 200, 120, 0.90);
const C_BADGE_DOC: D2D1_COLOR_F = hexa(240, 140,  60, 0.90);
const C_BADGE_IMG: D2D1_COLOR_F = hexa(180,  80, 220, 0.90);
const C_BADGE_VID: D2D1_COLOR_F = hexa(220,  60,  80, 0.90);
const C_BADGE_AUD: D2D1_COLOR_F = hexa( 60, 160, 240, 0.90);
const C_BADGE_ARC: D2D1_COLOR_F = hexa(200, 160,  40, 0.90);
const C_BADGE_DIR: D2D1_COLOR_F = hexa( 80, 120, 220, 0.90);
const C_BADGE_OTH: D2D1_COLOR_F = hexa(100, 100, 130, 0.90);

// GDI colours for the Win32 edit control
const fn gdi_rgb(r: u8, g: u8, b: u8) -> COLORREF {
    COLORREF((b as u32) << 16 | (g as u32) << 8 | r as u32)
}
const GDI_BAR:  COLORREF = gdi_rgb(20,  22,  34);
const GDI_TEXT: COLORREF = gdi_rgb(230, 235, 250);

// ── Custom messages ───────────────────────────────────────────────────────────
const WM_TRAYICON:      u32 = WM_USER + 1;
const WM_SHOW_WINDOW:   u32 = WM_USER + 2;
const WM_TOGGLE_WINDOW: u32 = WM_USER + 3;

// ── Kind ──────────────────────────────────────────────────────────────────────
#[derive(Clone, Copy, PartialEq, Eq)]
enum Kind { App, Document, Image, Video, Audio, Archive, Folder, Other }

impl Kind {
    fn sort_key(self) -> u8 {
        match self {
            Kind::App => 0, Kind::Document => 1, Kind::Image => 2,
            Kind::Video => 3, Kind::Audio => 4, Kind::Archive => 5,
            Kind::Folder => 6, Kind::Other => 7,
        }
    }
    fn label(self) -> &'static str {
        match self {
            Kind::App => "APP", Kind::Document => "DOC", Kind::Image => "IMG",
            Kind::Video => "VID", Kind::Audio => "AUD", Kind::Archive => "ZIP",
            Kind::Folder => "DIR", Kind::Other => "",
        }
    }
    fn badge_color(self) -> D2D1_COLOR_F {
        match self {
            Kind::App => C_BADGE_APP, Kind::Document => C_BADGE_DOC,
            Kind::Image => C_BADGE_IMG, Kind::Video => C_BADGE_VID,
            Kind::Audio => C_BADGE_AUD, Kind::Archive => C_BADGE_ARC,
            Kind::Folder => C_BADGE_DIR, Kind::Other => C_BADGE_OTH,
        }
    }
}

fn result_kind(r: &SearchResult) -> Kind {
    if r.is_dir { return Kind::Folder; }
    let ext = r.full_path.extension().and_then(|e| e.to_str()).map(|e| e.to_ascii_lowercase());
    match ext.as_deref() {
        Some("exe") | Some("lnk") | Some("appref-ms") | Some("msi") => Kind::App,
        Some("doc")  | Some("docx") | Some("pdf")  | Some("txt")  |
        Some("xlsx") | Some("xls")  | Some("pptx") | Some("ppt")  |
        Some("odt")  | Some("ods")  | Some("odp")  | Some("rtf")  |
        Some("md")   | Some("csv")  | Some("json") | Some("xml")  |
        Some("yaml") | Some("toml") | Some("ini")  | Some("log")  => Kind::Document,
        Some("png")  | Some("jpg")  | Some("jpeg") | Some("gif")  |
        Some("bmp")  | Some("webp") | Some("svg")  | Some("ico")  |
        Some("tiff") | Some("heic") | Some("raw")  | Some("psd")  => Kind::Image,
        Some("mp4")  | Some("mkv")  | Some("avi")  | Some("mov")  |
        Some("wmv")  | Some("flv")  | Some("webm") | Some("m4v")  => Kind::Video,
        Some("mp3")  | Some("flac") | Some("wav")  | Some("aac")  |
        Some("ogg")  | Some("m4a")  | Some("wma")  | Some("opus") => Kind::Audio,
        Some("zip")  | Some("rar")  | Some("7z")   | Some("tar")  |
        Some("gz")   | Some("bz2")  | Some("xz")   | Some("zst")  => Kind::Archive,
        _ => Kind::Other,
    }
}

fn sort_results(mut v: Vec<SearchResult>) -> Vec<SearchResult> {
    v.sort_by_key(|r| result_kind(r).sort_key());
    v
}

fn display_name(r: &SearchResult, shfi_raw: &str) -> String {
    if result_kind(r) == Kind::App {
        return r.full_path.file_stem().and_then(|s| s.to_str()).unwrap_or(&r.name).to_string();
    }
    if !shfi_raw.is_empty() { shfi_raw.to_string() } else { r.name.clone() }
}

fn path_display(r: &SearchResult) -> String {
    let s = r.full_path.to_string_lossy();
    if s.len() > 75 { format!("\u{2026}{}", &s[s.len().saturating_sub(73)..]) }
    else { s.to_string() }
}

fn loword(value: u32) -> u16 {
    (value & 0xFFFF) as u16
}

fn hiword(value: u32) -> u16 {
    ((value >> 16) & 0xFFFF) as u16
}

// ── Geometry helpers ──────────────────────────────────────────────────────────
#[inline] fn rf(l: f32, t: f32, r: f32, b: f32) -> D2D_RECT_F {
    D2D_RECT_F { left: l, top: t, right: r, bottom: b }
}
#[inline] fn rr(rect: D2D_RECT_F, radius: f32) -> D2D1_ROUNDED_RECT {
    D2D1_ROUNDED_RECT { rect, radiusX: radius, radiusY: radius }
}
#[inline] fn pt(x: f32, y: f32) -> D2D_POINT_2F { D2D_POINT_2F { x, y } }

// ── D2D resources ─────────────────────────────────────────────────────────────
// In windows-rs 0.58, brush creation methods live on ID2D1RenderTarget (base
// interface). ID2D1HwndRenderTarget inherits them via COM but the Rust bindings
// expose them only on the base type, so we store the cast alongside the hwnd rt.
struct D2Res {
    _factory: ID2D1Factory1,
    dw:       IDWriteFactory,
    rt:       ID2D1HwndRenderTarget,
    rt_base:  ID2D1RenderTarget,   // cast of rt — used for CreateSolidColorBrush etc.

    br_bg:          ID2D1SolidColorBrush,
    br_bar:         ID2D1SolidColorBrush,
    br_row_even:    ID2D1SolidColorBrush,
    br_row_odd:     ID2D1SolidColorBrush,
    br_hov:         ID2D1SolidColorBrush,
    br_sel:         ID2D1SolidColorBrush,
    br_sel_bar:     ID2D1SolidColorBrush,
    br_sep:         ID2D1SolidColorBrush,
    br_status:      ID2D1SolidColorBrush,
    br_text:        ID2D1SolidColorBrush,
    br_text_dim:    ID2D1SolidColorBrush,
    br_text_sel:    ID2D1SolidColorBrush,
    br_text_seld:   ID2D1SolidColorBrush,
    br_accent:      ID2D1SolidColorBrush,
    br_placeholder: ID2D1SolidColorBrush,
    br_white:       ID2D1SolidColorBrush,

    tf_search: IDWriteTextFormat,
    tf_name:   IDWriteTextFormat,
    tf_path:   IDWriteTextFormat,
    tf_badge:  IDWriteTextFormat,
    tf_status: IDWriteTextFormat,
    tf_icon:   IDWriteTextFormat,
}

// ── State ─────────────────────────────────────────────────────────────────────
struct State {
    index:          Arc<RwLock<IndexStore>>,
    results:        Vec<SearchResult>,
    selected:       usize,
    hover:          Option<usize>,
    edit_hwnd:      HWND,
    excluded:       Vec<String>,
    case_sensitive: bool,
    d2:             Option<D2Res>,
    gdi_font_edit:  HFONT,
    gdi_bar_brush:  HBRUSH,
}

impl State {
    fn win_h_i(&self) -> i32 {
        let rows = self.results.len().min(MAX_VIS);
        BAR_H_I + rows as i32 * ROW_H_I + if rows > 0 { STATUS_H_I } else { 0 }
    }
}

// ── Initialise Direct2D + DirectWrite ─────────────────────────────────────────
unsafe fn create_d2(hwnd: HWND, dpi: f32) -> windows::core::Result<D2Res> {
    // Factory (ID2D1Factory1 needs "Win32_Graphics_Direct2D" feature)
    let factory: ID2D1Factory1 = D2D1CreateFactory(D2D1_FACTORY_TYPE_SINGLE_THREADED, None)?;

    // DirectWrite
    let dw: IDWriteFactory = DWriteCreateFactory(DWRITE_FACTORY_TYPE_SHARED)?;

    // Hwnd render target
    let rt_props = D2D1_RENDER_TARGET_PROPERTIES {
        r#type: D2D1_RENDER_TARGET_TYPE_DEFAULT,
        pixelFormat: D2D1_PIXEL_FORMAT {
            format: windows::Win32::Graphics::Dxgi::Common::DXGI_FORMAT_B8G8R8A8_UNORM,
            alphaMode: D2D1_ALPHA_MODE_PREMULTIPLIED,
        },
        dpiX: dpi, dpiY: dpi,
        usage: D2D1_RENDER_TARGET_USAGE_NONE,
        minLevel: D2D1_FEATURE_LEVEL_DEFAULT,
    };
    let hw_props = D2D1_HWND_RENDER_TARGET_PROPERTIES {
        hwnd,
        pixelSize: D2D_SIZE_U { width: WIN_W_I as u32, height: 900 },
        presentOptions: D2D1_PRESENT_OPTIONS_NONE,
    };
    let rt: ID2D1HwndRenderTarget = factory.CreateHwndRenderTarget(&rt_props, &hw_props)?;
    rt.SetAntialiasMode(D2D1_ANTIALIAS_MODE_PER_PRIMITIVE);
    rt.SetTextAntialiasMode(D2D1_TEXT_ANTIALIAS_MODE_CLEARTYPE);

    // Cast to base for brush creation — this is necessary in windows-rs 0.58
    let rt_base: ID2D1RenderTarget = rt.cast()?;

    macro_rules! br {
        ($c:expr) => {{
            unsafe { rt_base.CreateSolidColorBrush(&$c, None)? }
        }};
    }
    let br_bg          = br!(C_BG);
    let br_bar         = br!(C_BAR);
    let br_row_even    = br!(C_ROW_EVEN);
    let br_row_odd     = br!(C_ROW_ODD);
    let br_hov         = br!(C_HOV);
    let br_sel         = br!(C_SEL);
    let br_sel_bar     = br!(C_SEL_BAR);
    let br_sep         = br!(C_SEP);
    let br_status      = br!(C_STATUS);
    let br_text        = br!(C_TEXT);
    let br_text_dim    = br!(C_TEXT_DIM);
    let br_text_sel    = br!(C_TEXT_SEL);
    let br_text_seld   = br!(C_TEXT_SEL_DIM);
    let br_accent      = br!(C_ACCENT);
    let br_placeholder = br!(C_PLACEHOLDER);
    let br_white       = br!(C_WHITE);

    macro_rules! tf {
        ($face:expr, $sz:expr, $wt:expr) => {{
            let face_w:   Vec<u16> = $face.encode_utf16().chain(Some(0u16)).collect();
            let locale_w: Vec<u16> = "en-us\0".encode_utf16().collect();
            let fmt: IDWriteTextFormat = dw.CreateTextFormat(
                PCWSTR(face_w.as_ptr()), None,
                $wt, DWRITE_FONT_STYLE_NORMAL, DWRITE_FONT_STRETCH_NORMAL,
                $sz, PCWSTR(locale_w.as_ptr()),
            )?;
            fmt.SetWordWrapping(DWRITE_WORD_WRAPPING_NO_WRAP)?;
            fmt.SetParagraphAlignment(DWRITE_PARAGRAPH_ALIGNMENT_CENTER)?;
            fmt
        }};
    }
    let tf_search = tf!("Segoe UI Variable Display", 20.0, DWRITE_FONT_WEIGHT_NORMAL);
    let tf_name   = tf!("Segoe UI Variable Text",    14.0, DWRITE_FONT_WEIGHT_SEMI_BOLD);
    let tf_path   = tf!("Segoe UI Variable Text",    11.0, DWRITE_FONT_WEIGHT_NORMAL);
    let tf_badge  = tf!("Segoe UI Variable Text",     9.5, DWRITE_FONT_WEIGHT_BOLD);
    let tf_status = tf!("Segoe UI Variable Text",    10.5, DWRITE_FONT_WEIGHT_NORMAL);
    let tf_icon   = tf!("Segoe MDL2 Assets",         18.0, DWRITE_FONT_WEIGHT_NORMAL);

    Ok(D2Res {
        _factory: factory, dw, rt, rt_base,
        br_bg, br_bar, br_row_even, br_row_odd,
        br_hov, br_sel, br_sel_bar, br_sep, br_status,
        br_text, br_text_dim, br_text_sel, br_text_seld,
        br_accent, br_placeholder, br_white,
        tf_search, tf_name, tf_path, tf_badge, tf_status, tf_icon,
    })
}

// ── Draw helpers ──────────────────────────────────────────────────────────────

unsafe fn draw_text(
    d2:    &D2Res,
    text:  &str,
    fmt:   &IDWriteTextFormat,
    brush: &ID2D1SolidColorBrush,
    rect:  D2D_RECT_F,
) {
    if text.is_empty() { return; }
    let wt: Vec<u16> = text.encode_utf16().collect();
    let lw = (rect.right  - rect.left).max(1.0);
    let lh = (rect.bottom - rect.top ).max(1.0);
    if let Ok(layout) = d2.dw.CreateTextLayout(&wt, fmt, lw, lh) {
        if let Ok(sign) = d2.dw.CreateEllipsisTrimmingSign(fmt) {
            let trim = DWRITE_TRIMMING {
                granularity: DWRITE_TRIMMING_GRANULARITY_CHARACTER,
                delimiter: 0, delimiterCount: 0,
            };
            let _ = layout.SetTrimming(&trim, &sign);
        }
        // DrawTextLayout also lives on the base interface in 0.58
        d2.rt_base.DrawTextLayout(
            pt(rect.left, rect.top), &layout, brush,
            D2D1_DRAW_TEXT_OPTIONS_CLIP,
        );
    }
}

unsafe fn fill_rr(d2: &D2Res, r: D2D_RECT_F, radius: f32, brush: &ID2D1SolidColorBrush) {
    // FillRoundedRectangle is also on the base type in 0.58
    d2.rt_base.FillRoundedRectangle(&rr(r, radius), brush);
}

unsafe fn fill_rect(d2: &D2Res, r: D2D_RECT_F, brush: &ID2D1SolidColorBrush) {
    d2.rt_base.FillRectangle(&r, brush);
}

/// Measure text width with a given format.
unsafe fn measure_text(d2: &D2Res, text: &str, fmt: &IDWriteTextFormat) -> f32 {
    let wt: Vec<u16> = text.encode_utf16().collect();
    if let Ok(layout) = d2.dw.CreateTextLayout(&wt, fmt, 300.0, 40.0) {
        let mut m = DWRITE_TEXT_METRICS::default();
        let _ = layout.GetMetrics(&mut m);
        return m.width.ceil();
    }
    0.0
}

/// Draw a coloured rounded badge pill.
unsafe fn draw_badge(d2: &D2Res, label: &str, color: D2D1_COLOR_F, x: f32, y: f32) {
    if label.is_empty() { return; }
    let tw = measure_text(d2, label, &d2.tf_badge);
    let px = 6.0_f32;
    let bw = tw + px * 2.0;
    let bh = 15.0_f32;
    // Temp brush for the badge background
    if let Ok(bg) = d2.rt_base.CreateSolidColorBrush(&color, None) {
        d2.rt_base.FillRoundedRectangle(&rr(rf(x, y, x + bw, y + bh), 3.5), &bg);
    }
    let wt: Vec<u16> = label.encode_utf16().collect();
    if let Ok(layout) = d2.dw.CreateTextLayout(&wt, &d2.tf_badge, bw, bh + 4.0) {
        d2.rt_base.DrawTextLayout(pt(x + px, y + 1.5), &layout, &d2.br_white,
            D2D1_DRAW_TEXT_OPTIONS_NONE);
    }
}

// ── Icons: drawn via GDI into a DIB, then blitted onto D2D surface ────────────
// Strategy: D2D BeginDraw → EndDraw, then for the icon layer we use a
// secondary GDI pass in WM_PAINT (after D2D) using the same HDC from BeginPaint.
// We store icons to draw in a Vec and process them after EndDraw.
struct IconDraw {
    hicon: HICON,
    x: i32,
    y: i32,
}

// ── Main paint (D2D pass) ─────────────────────────────────────────────────────
// Returns a list of icons to draw in the GDI pass after EndDraw.
unsafe fn paint_d2d(state: &State) -> Vec<IconDraw> {
    let mut icon_queue: Vec<IconDraw> = Vec::new();
    let d2 = match &state.d2 { Some(d) => d, None => return icon_queue };

    d2.rt.BeginDraw();
    d2.rt_base.Clear(Some(&C_BG));

    // ── Search bar ────────────────────────────────────────────────────────────
    fill_rect(d2, rf(0.0, 0.0, WIN_W, BAR_H), &d2.br_bar);

    // Input pill (very subtle)
    fill_rr(d2, rf(PAD - 2.0, 11.0, WIN_W - PAD + 2.0, BAR_H - 11.0), 8.0, &d2.br_placeholder);

    // Search icon (Segoe MDL2 U+E721)
    {
        let wt: Vec<u16> = "\u{E721}".encode_utf16().collect();
        if let Ok(layout) = d2.dw.CreateTextLayout(&wt, &d2.tf_icon, 28.0, BAR_H) {
            d2.rt_base.DrawTextLayout(pt(PAD + 2.0, 0.0), &layout, &d2.br_accent,
                D2D1_DRAW_TEXT_OPTIONS_NONE);
        }
    }

    // Placeholder (only when no query and no results)
    {
        let mut buf = [0u16; 4];
        let qlen = GetWindowTextW(state.edit_hwnd, &mut buf) as usize;
        if qlen == 0 && state.results.is_empty() {
            let tx = PAD + ICON_SZ + ICON_GAP + 4.0;
            draw_text(d2, "Search apps, files, folders\u{2026}",
                &d2.tf_search, &d2.br_placeholder, rf(tx, 0.0, WIN_W - PAD, BAR_H));
        }
    }

    // Bar bottom separator
    if !state.results.is_empty() {
        fill_rect(d2, rf(0.0, BAR_H - 1.0, WIN_W, BAR_H), &d2.br_sep);
    }

    // ── Result rows ───────────────────────────────────────────────────────────
    let vis = state.results.len().min(MAX_VIS);
    let tx  = PAD + ICON_SZ + ICON_GAP + 8.0;

    for (i, r) in state.results.iter().take(vis).enumerate() {
        let iy     = BAR_H + i as f32 * ROW_H;
        let is_sel = i == state.selected;
        let is_hov = state.hover == Some(i);

        // Row base
        let base = if i % 2 == 0 { &d2.br_row_even } else { &d2.br_row_odd };
        fill_rect(d2, rf(0.0, iy, WIN_W, iy + ROW_H), base);

        // Overlays
        if is_sel {
            fill_rect(d2, rf(0.0, iy, WIN_W, iy + ROW_H), &d2.br_sel);
            fill_rr(d2, rf(0.0, iy + 7.0, 3.5, iy + ROW_H - 7.0), 1.5, &d2.br_sel_bar);
        } else if is_hov {
            fill_rect(d2, rf(0.0, iy, WIN_W, iy + ROW_H), &d2.br_hov);
        }

        // Row separator
        fill_rect(d2, rf(tx - 4.0, iy + ROW_H - 1.0, WIN_W - PAD, iy + ROW_H), &d2.br_sep);

        // ── Shell icon — queue for GDI pass ───────────────────────────────────
        let path_w: Vec<u16> = r.full_path.to_string_lossy()
            .encode_utf16().chain(Some(0)).collect();
        let attr = if r.is_dir { FILE_ATTRIBUTE_DIRECTORY } else { FILE_ATTRIBUTE_NORMAL };
        let mut shfi = SHFILEINFOW::default();
        SHGetFileInfoW(
            PCWSTR(path_w.as_ptr()), attr,
            Some(&mut shfi),
            std::mem::size_of::<SHFILEINFOW>() as u32,
            SHGFI_ICON | SHGFI_LARGEICON | SHGFI_DISPLAYNAME | SHGFI_USEFILEATTRIBUTES,
        );
        if !shfi.hIcon.is_invalid() {
            icon_queue.push(IconDraw {
                hicon: shfi.hIcon,
                x: (PAD + 6.0) as i32,
                y: (iy + (ROW_H - ICON_SZ) / 2.0) as i32,
            });
        }

        // ── Text ──────────────────────────────────────────────────────────────
        let kind  = result_kind(r);
        let label = kind.label();

        // Build display name from shfi
        let name_len  = shfi.szDisplayName.iter().position(|&c| c == 0).unwrap_or(0);
        let shfi_name = String::from_utf16_lossy(&shfi.szDisplayName[..name_len]);
        let name      = display_name(r, &shfi_name);
        let path      = path_display(r);

        let (br_name, br_path) = if is_sel {
            (&d2.br_text_sel, &d2.br_text_seld)
        } else {
            (&d2.br_text, &d2.br_text_dim)
        };

        // Badge width reservation
        let badge_w = if !label.is_empty() {
            measure_text(d2, label, &d2.tf_badge) + 13.0
        } else { 0.0 };

        // Name
        draw_text(d2, &name, &d2.tf_name, br_name,
            rf(tx, iy + 10.0, WIN_W - PAD - badge_w - 4.0, iy + 28.0));

        // Badge
        if badge_w > 0.0 {
            draw_badge(d2, label, kind.badge_color(), WIN_W - PAD - badge_w, iy + 12.5);
        }

        // Path
        draw_text(d2, &path, &d2.tf_path, br_path,
            rf(tx, iy + 33.0, WIN_W - PAD, iy + ROW_H - 5.0));
    }

    // ── Status bar ────────────────────────────────────────────────────────────
    if !state.results.is_empty() {
        let sy = BAR_H + vis as f32 * ROW_H;
        fill_rect(d2, rf(0.0, sy, WIN_W, sy + STATUS_H), &d2.br_status);
        fill_rect(d2, rf(0.0, sy, WIN_W, sy + 1.0), &d2.br_sep);
        let total = state.results.len();
        let msg = if total > MAX_VIS {
            format!("Showing {} of {}   \u{B7}   \u{2191}\u{2193} navigate  \u{B7}  Enter open  \u{B7}  Ctrl+Enter open folder  \u{B7}  Esc close",
                MAX_VIS, total)
        } else {
            format!("{} {}   \u{B7}   \u{2191}\u{2193} navigate  \u{B7}  Enter open  \u{B7}  Ctrl+Enter open folder  \u{B7}  Esc close",
                total, if total == 1 { "result" } else { "results" })
        };
        draw_text(d2, &msg, &d2.tf_status, &d2.br_text_dim,
            rf(PAD, sy + 1.0, WIN_W - PAD, sy + STATUS_H));
    }

    let _ = d2.rt.EndDraw(None, None);
    icon_queue
}

// ── Open result ───────────────────────────────────────────────────────────────
unsafe fn open_result(state: &State, folder_only: bool) {
    let r = match state.results.get(state.selected) { Some(r) => r, None => return };
    if folder_only {
        let folder = if r.is_dir { r.full_path.clone() }
            else { r.full_path.parent().map(|p| p.to_path_buf()).unwrap_or_else(|| r.full_path.clone()) };
        let _ = std::process::Command::new("explorer.exe").arg(&folder).spawn();
    } else {
        let pw: Vec<u16> = r.full_path.to_string_lossy().encode_utf16().chain(Some(0)).collect();
        ShellExecuteW(
            Some(HWND::default()), w!("open"),
            PCWSTR(pw.as_ptr()), PCWSTR::null(), PCWSTR::null(), SW_SHOWNORMAL,
        );
    }
}

// ── Window resize ─────────────────────────────────────────────────────────────
unsafe fn resize_window(hwnd: HWND, state: &mut State) {
    let h = state.win_h_i();
    SetWindowPos(hwnd, None, 0, 0, WIN_W_I, h,
        SWP_NOMOVE | SWP_NOZORDER | SWP_NOACTIVATE).ok();
    if let Some(d2) = &state.d2 {
        let _ = d2.rt.Resize(&D2D_SIZE_U { width: WIN_W_I as u32, height: h as u32 });
    }
}

unsafe fn show_centered(hwnd: HWND) {
    let sw = GetSystemMetrics(SM_CXSCREEN);
    let sh = GetSystemMetrics(SM_CYSCREEN);
    let wx = (sw - WIN_W_I) / 2;
    let wy = (sh as f64 * 0.25) as i32;
    let ptr = GetWindowLongPtrW(hwnd, GWLP_USERDATA) as *mut State;
    let h = if ptr.is_null() { BAR_H_I } else { (*ptr).win_h_i() };
    SetWindowPos(hwnd, Some(HWND_TOPMOST), wx, wy, WIN_W_I, h, SWP_SHOWWINDOW | SWP_NOACTIVATE).ok();
    ShowWindow(hwnd, SW_SHOW);
    SetForegroundWindow(hwnd);
    if !ptr.is_null() {
        SetFocus(Some((*ptr).edit_hwnd));
        SendMessageW((*ptr).edit_hwnd, 0x00B1u32, Some(WPARAM(0)), Some(LPARAM(-1)));// EM_SETSEL
    }
}

// ── GDI font for edit control ─────────────────────────────────────────────────
unsafe fn make_gdi_font(name: &str, pt_size: i32, weight: i32) -> HFONT {
    let mut wn: Vec<u16> = name.encode_utf16().collect();
    wn.push(0);

    let hdc = GetDC(Some(HWND::default()));
    let dpi = GetDeviceCaps(Some(hdc), LOGPIXELSY);
    ReleaseDC(Some(HWND::default()), hdc);

    let h = -((pt_size * dpi) / 72);

    CreateFontW(
        h, 0, 0, 0, weight, 0, 0, 0,
        DEFAULT_CHARSET,
        OUT_DEFAULT_PRECIS,
        CLIP_DEFAULT_PRECIS,
        CLEARTYPE_QUALITY,
        (DEFAULT_PITCH.0 | FF_DONTCARE.0) as u32,
        PCWSTR(wn.as_ptr())
    )
}

// ── Entry point ───────────────────────────────────────────────────────────────
pub fn run(index: Arc<RwLock<IndexStore>>, excluded: Vec<String>) {
    unsafe {
        let hinst = GetModuleHandleW(None).unwrap();

        let wc = WNDCLASSEXW {
            cbSize: std::mem::size_of::<WNDCLASSEXW>() as u32,
            style: CS_HREDRAW | CS_VREDRAW | CS_DBLCLKS,
            lpfnWndProc: Some(wnd_proc),
            hInstance: hinst.into(),
            hCursor: LoadCursorW(None, IDC_ARROW).unwrap(),
            hbrBackground: HBRUSH(GetStockObject(NULL_BRUSH).0),
            lpszClassName: w!("FastSeekWnd"),
            ..Default::default()
        };
        RegisterClassExW(&wc);

        let sw = GetSystemMetrics(SM_CXSCREEN);
        let sh = GetSystemMetrics(SM_CYSCREEN);
        let wx = (sw - WIN_W_I) / 2;
        let wy = (sh as f64 * 0.25) as i32;

        let hwnd = CreateWindowExW(
            WS_EX_TOPMOST | WS_EX_TOOLWINDOW | WS_EX_LAYERED,
            w!("FastSeekWnd"), w!("FastSeek"),
            WS_POPUP,
            wx, wy, WIN_W_I, BAR_H_I,
            None, None, Some(hinst.into()), None,
        ).unwrap();

        // DWM: rounded corners + dark
        let corner: u32 = 2;
        DwmSetWindowAttribute(hwnd, DWMWA_WINDOW_CORNER_PREFERENCE, &corner as *const _ as _, 4).ok();
        let dark: u32 = 1;
        DwmSetWindowAttribute(hwnd, DWMWA_USE_IMMERSIVE_DARK_MODE, &dark as *const _ as _, 4).ok();
        SetLayeredWindowAttributes(hwnd, COLORREF(0), 245, LWA_ALPHA).ok();

        // DPI
        let dpi = {
            let hdc = GetDC(Some(hwnd));
            let d = GetDeviceCaps(Some(hdc), LOGPIXELSX) as f32;
            ReleaseDC(Some(hwnd), hdc);
            d
        };

        // Edit control (Win32, invisible bg — sits on top of D2D bar)
        let edit_x = (PAD + ICON_SZ + ICON_GAP + 6.0) as i32;
        let edit_h = 36i32;
        let edit = CreateWindowExW(
            WINDOW_EX_STYLE::default(), w!("EDIT"), w!(""),
            WS_CHILD | WS_VISIBLE | WINDOW_STYLE(ES_LEFT as u32 | ES_AUTOHSCROLL as u32),
            edit_x, (BAR_H_I - edit_h) / 2,
            WIN_W_I - edit_x - PAD as i32, edit_h,
            Some(hwnd), Some(HMENU(1 as _)), Some(hinst.into()), None,
        ).unwrap();
        let gdi_font_edit = make_gdi_font("Segoe UI Variable Display", 18, 400);
        SendMessageW(
            edit,
            WM_SETFONT,
            Some(WPARAM(gdi_font_edit.0 as usize)),
            Some(LPARAM(1)),
        );

        // Tray
        let mut nid = NOTIFYICONDATAW::default();
        nid.cbSize = std::mem::size_of::<NOTIFYICONDATAW>() as u32;
        nid.hWnd = hwnd; nid.uID = 1;
        nid.uFlags = NIF_ICON | NIF_MESSAGE | NIF_TIP;
        nid.uCallbackMessage = WM_TRAYICON;
        nid.hIcon = LoadIconW(None, IDI_APPLICATION).unwrap();
        let tip: Vec<u16> = "FastSeek  (Alt+Space)\0".encode_utf16().collect();
        nid.szTip[..tip.len()].copy_from_slice(&tip);
        Shell_NotifyIconW(NIM_ADD, &nid);

        let d2 = create_d2(hwnd, dpi).ok();
        let gdi_bar_brush = CreateSolidBrush(GDI_BAR);

        let state = Box::new(State {
            index, results: Vec::new(), selected: 0, hover: None,
            edit_hwnd: edit, excluded, case_sensitive: false,
            d2, gdi_font_edit, gdi_bar_brush,
        });
        let ptr = Box::into_raw(state);
        SetWindowLongPtrW(hwnd, GWLP_USERDATA, ptr as isize);

        crate::hotkey::register_and_listen(hwnd);

        let mut msg = MSG::default();
        while GetMessageW(&mut msg, Some(hwnd), 0, 0).as_bool() {
            if !IsDialogMessageW(hwnd, &msg).as_bool() {
                let _ = TranslateMessage(&msg);
                DispatchMessageW(&msg);
            }
        }

        let s = Box::from_raw(ptr);
        let _ = DeleteObject(s.gdi_font_edit.into());
        let _ = DeleteObject(s.gdi_bar_brush.into());
    }
}

// ── Window procedure ──────────────────────────────────────────────────────────
unsafe extern "system" fn wnd_proc(hwnd: HWND, msg: u32, wp: WPARAM, lp: LPARAM) -> LRESULT {
    let ptr = GetWindowLongPtrW(hwnd, GWLP_USERDATA) as *mut State;

    match msg {
        WM_ERASEBKGND => return LRESULT(1),

        WM_PAINT => {
            let mut ps = PAINTSTRUCT::default();
            let hdc = BeginPaint(hwnd, &mut ps);

            if !ptr.is_null() {
                // D2D pass — returns icons to draw on top
                let icons = paint_d2d(&*ptr);

                // GDI pass — draw shell icons via BeginPaint HDC
                // D2D already flushed to the window, so we can safely draw icons
                // on the same HDC; they composite correctly because D2D uses the
                // HWND back-buffer and we're in the same WM_PAINT.
                for id in icons {
                    DrawIconEx(hdc, id.x, id.y, id.hicon,
                        ICON_SZ as i32, ICON_SZ as i32, 0, Some(HBRUSH::default()), DI_NORMAL);
                    DestroyIcon(id.hicon);
                }
            }

            EndPaint(hwnd, &ps);
            return LRESULT(0);
        }

        WM_CTLCOLOREDIT => {
            let hdc = HDC(wp.0 as *mut core::ffi::c_void);
            SetBkMode(hdc, TRANSPARENT);
            SetTextColor(hdc, GDI_TEXT);
            if !ptr.is_null() {
                SelectObject(hdc, (*ptr).gdi_font_edit.into());
                return LRESULT((*ptr).gdi_bar_brush.0 as isize);
            }
            return LRESULT(GetStockObject(NULL_BRUSH).0 as isize);
        }

        WM_COMMAND => {
            let notif = (wp.0 >> 16) as u16;
            let id    = (wp.0 & 0xffff) as u16;
            if id == 1 && notif as u32 == EN_CHANGE && !ptr.is_null() {
                let s = &mut *ptr;
                let len = GetWindowTextLengthW(s.edit_hwnd) as usize;
                let mut buf = vec![0u16; len + 1];
                GetWindowTextW(s.edit_hwnd, &mut buf);
                let q = String::from_utf16_lossy(&buf[..len]);
                {
                    let store = s.index.read();
                    let raw = do_search(&store, q.trim(), 120, s.case_sensitive, &s.excluded);
                    s.results = sort_results(raw);
                }
                s.selected = 0; s.hover = None;
                resize_window(hwnd, s);
                InvalidateRect(Some(hwnd), None, FALSE.into());
            }
            return LRESULT(0);
        }

        WM_KEYDOWN => {
            if ptr.is_null() { return LRESULT(0); }
            let s = &mut *ptr;
            match VIRTUAL_KEY(wp.0 as u16) {
                VK_ESCAPE => { ShowWindow(hwnd, SW_HIDE); }
                VK_DOWN => {
                    if s.selected + 1 < s.results.len().min(MAX_VIS) {
                        s.selected += 1;
                        InvalidateRect(Some(hwnd), None, FALSE.into()).ok();
                    }
                }
                VK_UP => {
                    if s.selected > 0 {
                        s.selected -= 1;
                        InvalidateRect(Some(hwnd), None, FALSE.into()).ok();
                    }
                }
                VK_RETURN => {
                    let ctrl = GetKeyState(VK_CONTROL.0 as i32) as u16 & 0x8000 != 0;
                    open_result(s, ctrl);
                    ShowWindow(hwnd, SW_HIDE);
                }
                _ => return DefWindowProcW(hwnd, msg, wp, lp),
            }
            return LRESULT(0);
        }

        WM_MOUSEMOVE => {
            if !ptr.is_null() {
                let s = &mut *ptr;
                let y = (lp.0 & 0xffff) as i16 as i32;
                let ry = y - BAR_H_I;
                let new_hov = if ry >= 0 {
                    let row = (ry / ROW_H_I) as usize;
                    if row < s.results.len().min(MAX_VIS) { Some(row) } else { None }
                } else { None };
                if new_hov != s.hover {
                    s.hover = new_hov;
                    InvalidateRect(Some(hwnd), None, FALSE.into()).ok();
                }
            }
            return LRESULT(0);
        }

        WM_LBUTTONDOWN => {
            if !ptr.is_null() {
                let s = &mut *ptr;
                let y = (lp.0 & 0xffff) as i16 as i32;
                let ry = y - BAR_H_I;
                if ry >= 0 {
                    let row = (ry / ROW_H_I) as usize;
                    if row < s.results.len().min(MAX_VIS) {
                        s.selected = row;
                        InvalidateRect(Some(hwnd), None, FALSE.into()).ok();
                    }
                }
                SetFocus(Some((*ptr).edit_hwnd)).ok();
            }
            return LRESULT(0);
        }

        WM_LBUTTONDBLCLK => {
            if !ptr.is_null() {
                let y = (lp.0 & 0xffff) as i16 as i32;
                if y >= BAR_H_I {
                    open_result(&*ptr, false);
                    ShowWindow(hwnd, SW_HIDE);
                }
            }
            return LRESULT(0);
        }

        WM_ACTIVATE => {
            if u32::from(loword(wp.0 as u32)) == WA_INACTIVE {
                ShowWindow(hwnd, SW_HIDE);
            }
            return LRESULT(0);
        }
        
        WM_SHOW_WINDOW => {
            show_centered(hwnd);
            return LRESULT(0);
        }
        WM_TOGGLE_WINDOW => {
            if IsWindowVisible(hwnd).as_bool() {
                ShowWindow(hwnd, SW_HIDE);
            } else {
                show_centered(hwnd);
            }
            return LRESULT(0);
        }

        WM_SIZE => {
            if !ptr.is_null() {
                if let Some(d2) = &(*ptr).d2 {
                    let w = loword(lp.0 as u32) as u32;
                    let h = hiword(lp.0 as u32) as u32;
                    let _ = d2.rt.Resize(&D2D_SIZE_U { width: w, height: h });
                }
            }
            return LRESULT(0);
        }

        WM_TRAYICON => {
            let tm = (lp.0 & 0xffff) as u32;
            if tm == WM_RBUTTONUP || tm == WM_LBUTTONUP {
                let mut cur = POINT::default();
                GetCursorPos(&mut cur).ok();
                let hmenu = CreatePopupMenu().unwrap();
                AppendMenuW(hmenu, MF_STRING,    1001, w!("Show FastSeek  (Alt+Space)")).ok();
                AppendMenuW(hmenu, MF_SEPARATOR, 0,    PCWSTR::null()).ok();
                AppendMenuW(hmenu, MF_STRING,    1002, w!("Exit")).ok();
                SetForegroundWindow(hwnd);
                let cmd = TrackPopupMenu(hmenu,
                    TPM_RETURNCMD | TPM_NONOTIFY | TPM_RIGHTBUTTON,
                    cur.x, cur.y, Some(0), hwnd, None);
                DestroyMenu(hmenu).ok();
                match cmd.0 {
                    1001 => show_centered(hwnd),
                    1002 => { PostMessageW(Some(hwnd), WM_CLOSE, WPARAM(0), LPARAM(0)).ok(); }
                    _ => {}
                }
            }
            return LRESULT(0);
        }

        WM_DESTROY => {
            if !ptr.is_null() {
                let mut nid = NOTIFYICONDATAW::default();
                nid.cbSize = std::mem::size_of::<NOTIFYICONDATAW>() as u32;
                nid.hWnd = hwnd; nid.uID = 1;
                Shell_NotifyIconW(NIM_DELETE, &nid);
            }
            PostQuitMessage(0);
        }

        _ => return DefWindowProcW(hwnd, msg, wp, lp),
    }
    LRESULT(0)
}