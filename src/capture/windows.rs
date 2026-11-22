//! Windows: Windows Graphics Capture (the API the Snipping Tool, OBS and
//! Discord use) for a screen or one window, the GPU's video processor to scale
//! and convert to NV12, a hardware H.264 encoder through Media Foundation
//! (NVIDIA, AMD or Intel; Microsoft's software one if there's none), and WASAPI
//! process loopback for the sound.
//!
//! Everything is reached through `windows-rs`, Microsoft's open-source Rust
//! bindings.
//!
//! - A covered or off-screen window keeps streaming: Graphics Capture reads it
//!   from the compositor, not from the screen.
//! - A minimized window isn't drawn by Windows at all, so nothing new comes;
//!   watchers keep the last picture and are told it's minimized.
//! - While a screen is shared, Portside's own windows are marked
//!   "exclude from capture" (Windows 10 2004+), so they don't show up in it.
//! - "All sound" is process loopback *excluding* Portside's process tree, so
//!   the room's voices (which Portside plays) aren't sent back to the room.

use std::mem::ManuallyDrop;
use std::sync::mpsc::sync_channel;
use std::sync::Arc;
use std::time::{Duration, Instant};

use windows::core::{implement, Interface, Ref, BOOL, GUID, HRESULT, PWSTR};
use windows::Graphics::Capture::{Direct3D11CaptureFramePool, GraphicsCaptureItem, GraphicsCaptureSession};
use windows::Graphics::DirectX::Direct3D11::IDirect3DDevice;
use windows::Graphics::DirectX::DirectXPixelFormat;
use windows::Graphics::SizeInt32;
use windows::Win32::Foundation::{CloseHandle, HANDLE, HMODULE, HWND, LPARAM, RECT, WAIT_OBJECT_0};
use windows::Win32::Graphics::Direct3D::{D3D_DRIVER_TYPE_HARDWARE, D3D_FEATURE_LEVEL_11_0};
use windows::Win32::Graphics::Direct3D11::*;
use windows::Win32::Graphics::Dwm::{DwmGetWindowAttribute, DWMWA_CLOAKED};
use windows::Win32::Graphics::Dxgi::Common::{DXGI_FORMAT_B8G8R8A8_UNORM, DXGI_FORMAT_NV12, DXGI_RATIONAL, DXGI_SAMPLE_DESC};
use windows::Win32::Graphics::Dxgi::IDXGIDevice;
use windows::Win32::Graphics::Gdi::{EnumDisplayMonitors, GetMonitorInfoW, HDC, HMONITOR, MONITORINFO};
use windows::Win32::Media::Audio::*;
use windows::Win32::Media::MediaFoundation::*;
use windows::Win32::System::Com::StructuredStorage::PROPVARIANT;
use windows::Win32::System::Com::{CoInitializeEx, CoUninitialize, BLOB, COINIT_MULTITHREADED};
use windows::Win32::System::Threading::{
    CreateEventW, GetCurrentProcessId, OpenProcess, QueryFullProcessImageNameW, SetEvent, WaitForSingleObject, PROCESS_NAME_WIN32,
    PROCESS_QUERY_LIMITED_INFORMATION,
};
use windows::Win32::System::Variant::{VARIANT, VT_BOOL, VT_UI4};
use windows::Win32::System::WinRT::Direct3D11::{CreateDirect3D11DeviceFromDXGIDevice, IDirect3DDxgiInterfaceAccess};
use windows::Win32::System::WinRT::Graphics::Capture::IGraphicsCaptureItemInterop;
use windows::Win32::UI::WindowsAndMessaging::{
    EnumWindows, GetWindow, GetWindowLongPtrW, GetWindowPlacement, WINDOWPLACEMENT, GetWindowTextW, GetWindowThreadProcessId, IsIconic, IsWindow, IsWindowVisible,
    SetWindowDisplayAffinity, GWL_EXSTYLE, GW_OWNER, MONITORINFOF_PRIMARY, WDA_EXCLUDEFROMCAPTURE, WDA_NONE, WS_EX_TOOLWINDOW,
};

use super::{fit, h264::ParamSets, out_size, Audio, AudioEncoder, Cadence, Control, Hint, Options, Out, Running, Sink, Source};

fn err(what: &str, e: windows::core::Error) -> String {
    format!("{what} ({:#010x}: {})", e.code().0 as u32, e.message().trim())
}

/// Process loopback (one app's sound, or all but ours) is Windows 10 2004+.
pub fn audio_modes() -> Vec<&'static str> {
    vec!["app", "system"]
}

// ------------------------------------------------------------------ sources

struct Found {
    me: u32,
    windows: Vec<Source>,
}

unsafe extern "system" fn each_window(hwnd: HWND, lp: LPARAM) -> BOOL {
    let f = &mut *(lp.0 as *mut Found);
    if let Some(s) = window_source(hwnd, f.me) {
        f.windows.push(s);
    }
    BOOL(1)
}

unsafe fn window_source(hwnd: HWND, me: u32) -> Option<Source> {
    if !IsWindowVisible(hwnd).as_bool() {
        return None;
    }
    // Owned windows (dialogs) and tool windows aren't what people share.
    if GetWindow(hwnd, GW_OWNER).is_ok_and(|o| !o.is_invalid()) {
        return None;
    }
    if GetWindowLongPtrW(hwnd, GWL_EXSTYLE) as u32 & WS_EX_TOOLWINDOW.0 != 0 {
        return None;
    }
    // Hidden by the shell (other virtual desktops' UWP shells, suspended apps).
    let mut cloaked = 0u32;
    if DwmGetWindowAttribute(hwnd, DWMWA_CLOAKED, &mut cloaked as *mut u32 as *mut _, 4).is_ok() && cloaked != 0 {
        return None;
    }
    let mut buf = [0u16; 256];
    let n = GetWindowTextW(hwnd, &mut buf);
    let title = String::from_utf16_lossy(&buf[..n.max(0) as usize]).trim().to_string();
    if title.is_empty() {
        return None;
    }
    let mut pid = 0u32;
    GetWindowThreadProcessId(hwnd, Some(&mut pid));
    if pid == me {
        return None; // Portside itself
    }
    let app = process_name(pid);
    if matches!(app.to_ascii_lowercase().as_str(), "textinputhost" | "applicationframehost" | "shellexperiencehost") && title == "Windows Input Experience" {
        return None;
    }
    let mut r = RECT::default();
    let _ = windows::Win32::UI::WindowsAndMessaging::GetClientRect(hwnd, &mut r);
    Some(Source {
        id: format!("window:{}", hwnd.0 as isize),
        screen: false,
        title,
        app,
        pid,
        w: (r.right - r.left).max(0) as u32,
        h: (r.bottom - r.top).max(0) as u32,
        minimized: IsIconic(hwnd).as_bool(),
    })
}

fn process_name(pid: u32) -> String {
    unsafe {
        let Ok(h) = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, false, pid) else { return String::new() };
        let mut buf = [0u16; 520];
        let mut len = buf.len() as u32;
        let ok = QueryFullProcessImageNameW(h, PROCESS_NAME_WIN32, PWSTR(buf.as_mut_ptr()), &mut len).is_ok();
        let _ = CloseHandle(h);
        if !ok {
            return String::new();
        }
        let path = String::from_utf16_lossy(&buf[..len as usize]);
        std::path::Path::new(&path).file_stem().map(|s| s.to_string_lossy().to_string()).unwrap_or_default()
    }
}

unsafe extern "system" fn each_monitor(m: HMONITOR, _: HDC, _: *mut RECT, lp: LPARAM) -> BOOL {
    let list = &mut *(lp.0 as *mut Vec<(HMONITOR, RECT, bool)>);
    let mut info = MONITORINFO { cbSize: std::mem::size_of::<MONITORINFO>() as u32, ..Default::default() };
    if GetMonitorInfoW(m, &mut info).as_bool() {
        list.push((m, info.rcMonitor, info.dwFlags & MONITORINFOF_PRIMARY != 0));
    }
    BOOL(1)
}

pub fn sources() -> Result<Vec<Source>, String> {
    if !GraphicsCaptureSession::IsSupported().unwrap_or(false) {
        return Err("Screen sharing needs Windows 10 version 1903 or newer.".into());
    }
    let mut out = Vec::new();
    unsafe {
        let mut mons: Vec<(HMONITOR, RECT, bool)> = Vec::new();
        let _ = EnumDisplayMonitors(None, None, Some(each_monitor), LPARAM(&mut mons as *mut _ as isize));
        // The main screen first.
        mons.sort_by_key(|m| !m.2);
        let many = mons.len() > 1;
        for (i, (m, r, primary)) in mons.iter().enumerate() {
            let (w, h) = ((r.right - r.left).max(0) as u32, (r.bottom - r.top).max(0) as u32);
            let title = match (many, primary) {
                (false, _) => "Your screen".to_string(),
                (true, true) => format!("Screen {} (main)", i + 1),
                (true, false) => format!("Screen {}", i + 1),
            };
            out.push(Source { id: format!("screen:{}", m.0 as isize), screen: true, title, app: String::new(), pid: 0, w, h, minimized: false });
        }
        let mut f = Found { me: GetCurrentProcessId(), windows: Vec::new() };
        let _ = EnumWindows(Some(each_window), LPARAM(&mut f as *mut _ as isize));
        out.extend(f.windows);
    }
    Ok(out)
}

enum Target {
    Screen(HMONITOR),
    Window(HWND),
}

fn target(s: &Source) -> Result<Target, String> {
    let (kind, n) = s.id.split_once(':').ok_or("bad source")?;
    let n: isize = n.parse().map_err(|_| "bad source")?;
    Ok(match kind {
        "screen" => Target::Screen(HMONITOR(n as *mut _)),
        _ => Target::Window(HWND(n as *mut _)),
    })
}

// -------------------------------------------------------------------- start

struct Live {
    control: Arc<Control>,
}

impl Running for Live {}

impl Drop for Live {
    fn drop(&mut self) {
        self.control.stop.store(true, std::sync::atomic::Ordering::Relaxed);
    }
}

pub fn start(opts: Options, control: Arc<Control>, sink: Sink) -> Result<Box<dyn Running>, String> {
    let t = target(&opts.source)?;
    let (ready_tx, ready_rx) = sync_channel::<Result<(), String>>(1);
    let (c, s, o) = (control.clone(), sink.clone(), opts.clone());
    // HWND/HMONITOR are plain numbers; they're only used on the capture thread.
    let raw = match t {
        Target::Screen(m) => (true, m.0 as isize),
        Target::Window(w) => (false, w.0 as isize),
    };
    std::thread::Builder::new()
        .name("share-video".into())
        .spawn(move || {
            let t = if raw.0 { Target::Screen(HMONITOR(raw.1 as *mut _)) } else { Target::Window(HWND(raw.1 as *mut _)) };
            unsafe {
                let _ = CoInitializeEx(None, COINIT_MULTITHREADED);
                let _ = MFStartup(MF_VERSION, MFSTARTUP_LITE);
                match Video::new(&o, &t, &c) {
                    Ok(mut v) => {
                        let _ = ready_tx.send(Ok(()));
                        let why = v.run(&o, &t, &c, &s);
                        v.finish();
                        if let Some(why) = why {
                            s(Out::Ended(why));
                        }
                    }
                    Err(e) => {
                        let _ = ready_tx.send(Err(e));
                    }
                }
                let _ = MFShutdown();
                CoUninitialize();
            }
        })
        .map_err(|e| e.to_string())?;
    match ready_rx.recv_timeout(Duration::from_secs(15)) {
        Ok(Ok(())) => {}
        Ok(Err(e)) => return Err(e),
        Err(_) => {
            control.stop.store(true, std::sync::atomic::Ordering::Relaxed);
            return Err("Screen capture didn't start in time.".into());
        }
    }
    if opts.audio != Audio::Off {
        let pid = match opts.audio {
            Audio::App => opts.source.pid,
            _ => unsafe { GetCurrentProcessId() },
        };
        let include = opts.audio == Audio::App;
        let (c, s) = (control.clone(), sink.clone());
        let _ = std::thread::Builder::new().name("share-audio".into()).spawn(move || unsafe {
            let _ = CoInitializeEx(None, COINIT_MULTITHREADED);
            if let Err(e) = sound(pid, include, &c, &s) {
                s(Out::AudioFailed(e));
            }
            CoUninitialize();
        });
    }
    Ok(Box::new(Live { control }))
}

// -------------------------------------------------------------------- video

struct Gpu {
    device: ID3D11Device,
    context: ID3D11DeviceContext,
    vdevice: ID3D11VideoDevice,
    vcontext: ID3D11VideoContext,
    winrt: IDirect3DDevice,
}

unsafe fn gpu() -> Result<Gpu, String> {
    let mut device = None;
    let mut context = None;
    D3D11CreateDevice(
        None,
        D3D_DRIVER_TYPE_HARDWARE,
        HMODULE::default(),
        D3D11_CREATE_DEVICE_BGRA_SUPPORT | D3D11_CREATE_DEVICE_VIDEO_SUPPORT,
        Some(&[D3D_FEATURE_LEVEL_11_0]),
        D3D11_SDK_VERSION,
        Some(&mut device),
        None,
        Some(&mut context),
    )
    .map_err(|e| err("Couldn't open the graphics card", e))?;
    let device = device.ok_or("no graphics device")?;
    let context = context.ok_or("no graphics context")?;
    // The encoder uses the device from its own threads.
    if let Ok(mt) = device.cast::<ID3D11Multithread>() {
        let _ = mt.SetMultithreadProtected(true);
    }
    let vdevice: ID3D11VideoDevice = device.cast().map_err(|e| err("The graphics card has no video processor", e))?;
    let vcontext: ID3D11VideoContext = context.cast().map_err(|e| err("The graphics card has no video processor", e))?;
    let dxgi: IDXGIDevice = device.cast().map_err(|e| err("DXGI", e))?;
    let winrt: IDirect3DDevice = CreateDirect3D11DeviceFromDXGIDevice(&dxgi).and_then(|i| i.cast()).map_err(|e| err("Couldn't set up capture", e))?;
    Ok(Gpu { device, context, vdevice, vcontext, winrt })
}

unsafe fn texture(g: &Gpu, w: u32, h: u32, format: windows::Win32::Graphics::Dxgi::Common::DXGI_FORMAT, bind: i32, staging: bool) -> Result<ID3D11Texture2D, String> {
    let desc = D3D11_TEXTURE2D_DESC {
        Width: w,
        Height: h,
        MipLevels: 1,
        ArraySize: 1,
        Format: format,
        SampleDesc: DXGI_SAMPLE_DESC { Count: 1, Quality: 0 },
        Usage: if staging { D3D11_USAGE_STAGING } else { D3D11_USAGE_DEFAULT },
        BindFlags: bind as u32,
        CPUAccessFlags: if staging { D3D11_CPU_ACCESS_READ.0 as u32 } else { 0 },
        MiscFlags: 0,
    };
    let mut t = None;
    g.device.CreateTexture2D(&desc, None, Some(&mut t)).map_err(|e| err("Couldn't make a video surface", e))?;
    t.ok_or_else(|| "no texture".into())
}

/// Scales and converts the captured BGRA picture into the encoder's NV12
/// frames, on the GPU.
struct Convert {
    in_w: u32,
    in_h: u32,
    input: ID3D11Texture2D,
    enumerator: ID3D11VideoProcessorEnumerator,
    processor: ID3D11VideoProcessor,
    in_view: ID3D11VideoProcessorInputView,
}

unsafe fn convert(g: &Gpu, in_w: u32, in_h: u32, out_w: u32, out_h: u32, fps: u32) -> Result<Convert, String> {
    let input = texture(g, in_w, in_h, DXGI_FORMAT_B8G8R8A8_UNORM, D3D11_BIND_RENDER_TARGET.0 | D3D11_BIND_SHADER_RESOURCE.0, false)?;
    let rate = DXGI_RATIONAL { Numerator: fps, Denominator: 1 };
    let desc = D3D11_VIDEO_PROCESSOR_CONTENT_DESC {
        InputFrameFormat: D3D11_VIDEO_FRAME_FORMAT_PROGRESSIVE,
        InputFrameRate: rate,
        InputWidth: in_w,
        InputHeight: in_h,
        OutputFrameRate: rate,
        OutputWidth: out_w,
        OutputHeight: out_h,
        Usage: D3D11_VIDEO_USAGE_PLAYBACK_NORMAL,
    };
    let enumerator = g.vdevice.CreateVideoProcessorEnumerator(&desc).map_err(|e| err("No video processor for this size", e))?;
    let processor = g.vdevice.CreateVideoProcessor(&enumerator, 0).map_err(|e| err("No video processor for this size", e))?;
    let vdesc = D3D11_VIDEO_PROCESSOR_INPUT_VIEW_DESC {
        FourCC: 0,
        ViewDimension: D3D11_VPIV_DIMENSION_TEXTURE2D,
        Anonymous: D3D11_VIDEO_PROCESSOR_INPUT_VIEW_DESC_0 { Texture2D: D3D11_TEX2D_VPIV { MipSlice: 0, ArraySlice: 0 } },
    };
    let mut in_view = None;
    g.vdevice.CreateVideoProcessorInputView(&input, &enumerator, &vdesc, Some(&mut in_view)).map_err(|e| err("Video processor input", e))?;
    let in_view = in_view.ok_or("no input view")?;
    // Full-range RGB in; BT.709 studio-range YUV out (what H.264 decoders expect).
    let rgb_full = D3D11_VIDEO_PROCESSOR_COLOR_SPACE { _bitfield: 0 };
    let yuv_709 = D3D11_VIDEO_PROCESSOR_COLOR_SPACE { _bitfield: (1 << 2) | (1 << 4) };
    g.vcontext.VideoProcessorSetStreamColorSpace(&processor, 0, &rgb_full);
    g.vcontext.VideoProcessorSetOutputColorSpace(&processor, &yuv_709);
    g.vcontext.VideoProcessorSetStreamFrameFormat(&processor, 0, D3D11_VIDEO_FRAME_FORMAT_PROGRESSIVE);
    let black = D3D11_VIDEO_COLOR { Anonymous: D3D11_VIDEO_COLOR_0 { RGBA: D3D11_VIDEO_COLOR_RGBA { R: 0.0, G: 0.0, B: 0.0, A: 1.0 } } };
    g.vcontext.VideoProcessorSetOutputBackgroundColor(&processor, false, &black);
    let src = RECT { left: 0, top: 0, right: in_w as i32, bottom: in_h as i32 };
    g.vcontext.VideoProcessorSetStreamSourceRect(&processor, 0, true, Some(&src));
    let (l, t, r, b) = fit(in_w, in_h, out_w, out_h);
    let dst = RECT { left: l as i32, top: t as i32, right: r as i32, bottom: b as i32 };
    g.vcontext.VideoProcessorSetStreamDestRect(&processor, 0, true, Some(&dst));
    let all = RECT { left: 0, top: 0, right: out_w as i32, bottom: out_h as i32 };
    g.vcontext.VideoProcessorSetOutputTargetRect(&processor, true, Some(&all));
    Ok(Convert { in_w, in_h, input, enumerator, processor, in_view })
}

/// One NV12 frame for the encoder, and its output view.
struct Slot {
    tex: ID3D11Texture2D,
    view: Option<ID3D11VideoProcessorOutputView>,
}

struct Video {
    gpu: Gpu,
    item: GraphicsCaptureItem,
    pool: Direct3D11CaptureFramePool,
    session: GraphicsCaptureSession,
    pool_size: SizeInt32,
    out_w: u32,
    out_h: u32,
    conv: Option<Convert>,
    slots: Vec<Slot>,
    next_slot: usize,
    enc: Encoder,
    /// Portside's windows we hid from the capture, to show again after.
    hidden: Vec<HWND>,
}

unsafe fn capture_item(t: &Target) -> Result<GraphicsCaptureItem, String> {
    let interop = windows::core::factory::<GraphicsCaptureItem, IGraphicsCaptureItemInterop>().map_err(|e| err("Capture isn't available", e))?;
    match t {
        Target::Screen(m) => interop.CreateForMonitor(*m),
        Target::Window(w) => interop.CreateForWindow(*w),
    }
    .map_err(|e| err("Couldn't capture that screen or window", e))
}

unsafe extern "system" fn own_window(hwnd: HWND, lp: LPARAM) -> BOOL {
    let list = &mut *(lp.0 as *mut Vec<HWND>);
    let mut pid = 0u32;
    GetWindowThreadProcessId(hwnd, Some(&mut pid));
    if pid == GetCurrentProcessId() && IsWindowVisible(hwnd).as_bool() {
        list.push(hwnd);
    }
    BOOL(1)
}

impl Video {
    unsafe fn new(o: &Options, t: &Target, c: &Control) -> Result<Video, String> {
        let gpu = gpu()?;
        let item = capture_item(t)?;
        let mut size = item.Size().map_err(|e| err("capture size", e))?;
        // A minimized window reports a tiny size: use the size it has when
        // it's restored, so the stream isn't made for a postage stamp.
        if let Target::Window(w) = t {
            if size.Width < 100 || size.Height < 100 {
                let mut wp = WINDOWPLACEMENT { length: std::mem::size_of::<WINDOWPLACEMENT>() as u32, ..Default::default() };
                if GetWindowPlacement(*w, &mut wp).is_ok() {
                    let r = wp.rcNormalPosition;
                    if r.right - r.left >= 100 && r.bottom - r.top >= 100 {
                        size = SizeInt32 { Width: r.right - r.left, Height: r.bottom - r.top };
                    }
                }
            }
        }
        let (sw, sh) = (size.Width.max(2) as u32, size.Height.max(2) as u32);
        let (out_w, out_h) = out_size(sw, sh, o.max_h);
        let pool = Direct3D11CaptureFramePool::CreateFreeThreaded(&gpu.winrt, DirectXPixelFormat::B8G8R8A8UIntNormalized, 2, size)
            .map_err(|e| err("Couldn't start capture", e))?;
        let session = pool.CreateCaptureSession(&item).map_err(|e| err("Couldn't start capture", e))?;
        let _ = session.SetIsCursorCaptureEnabled(true);
        // No yellow frame around what's shared (Windows 11; older ones ignore it).
        let _ = session.SetIsBorderRequired(false);
        let enc = Encoder::new(&gpu, out_w, out_h, c.fps, c.max_bitrate, o.hint)?;
        let mut slots = Vec::new();
        for _ in 0..4 {
            let tex = texture(&gpu, out_w, out_h, DXGI_FORMAT_NV12, D3D11_BIND_RENDER_TARGET.0 | D3D11_BIND_VIDEO_ENCODER.0, false)
                .or_else(|_| texture(&gpu, out_w, out_h, DXGI_FORMAT_NV12, D3D11_BIND_RENDER_TARGET.0, false))?;
            slots.push(Slot { tex, view: None });
        }
        // Leave Portside out of a whole-screen share.
        let mut hidden = Vec::new();
        if matches!(t, Target::Screen(_)) {
            let mut mine: Vec<HWND> = Vec::new();
            let _ = EnumWindows(Some(own_window), LPARAM(&mut mine as *mut _ as isize));
            for w in mine {
                if SetWindowDisplayAffinity(w, WDA_EXCLUDEFROMCAPTURE).is_ok() {
                    hidden.push(w);
                }
            }
        }
        session.StartCapture().map_err(|e| err("Couldn't start capture", e))?;
        Ok(Video { gpu, item, pool, session, pool_size: size, out_w, out_h, conv: None, slots, next_slot: 0, enc, hidden })
    }

    unsafe fn finish(&mut self) {
        let _ = self.session.Close();
        let _ = self.pool.Close();
        for w in self.hidden.drain(..) {
            let _ = SetWindowDisplayAffinity(w, WDA_NONE);
        }
        self.enc.end();
    }

    /// Runs until stopped. Some(why) if the share ended by itself.
    unsafe fn run(&mut self, _o: &Options, t: &Target, c: &Control, sink: &Sink) -> Option<String> {
        let _ = &self.item;
        let start = Instant::now();
        let mut cadence = Cadence::new();
        let mut have = false; // a picture has been captured
        let mut fresh = false; // ...and not encoded yet
        let mut last_sent = Instant::now();
        let mut key = true;
        let mut paused = false;
        let mut checked = Instant::now();
        let mut rate = c.max_bitrate;
        let mut announced = false;
        while !c.stopped() {
            let now = Instant::now();
            // The window closing ends the share; minimizing pauses it.
            if now.duration_since(checked) > Duration::from_millis(250) {
                checked = now;
                if let Target::Window(w) = t {
                    if !IsWindow(Some(*w)).as_bool() {
                        return Some("The shared window was closed.".into());
                    }
                    let m = IsIconic(*w).as_bool();
                    if m != paused {
                        paused = m;
                        sink(Out::Paused(m));
                    }
                }
            }
            // The newest captured frame; older ones are skipped.
            let mut latest = None;
            while let Ok(f) = self.pool.TryGetNextFrame() {
                latest = Some(f);
            }
            if let Some(f) = latest {
                if let Err(e) = self.take(&f) {
                    return Some(e);
                }
                have = true;
                fresh = true;
            }
            key |= c.take_key();
            let b = c.bitrate.load(std::sync::atomic::Ordering::Relaxed);
            if b != rate {
                rate = b;
                self.enc.set_bitrate(b);
            }
            // A still picture produces no frames: send it again now and then
            // (and at once when someone needs a key frame to start from).
            let idle = now.duration_since(last_sent) > Duration::from_millis(1000);
            if have && (fresh || key || idle) && self.enc.wants_input() && cadence.due(now, c.fps_now()) {
                let ts = now.duration_since(start).as_micros() as i64;
                match self.encode(ts, key) {
                    Ok(true) => {
                        fresh = false;
                        key = false;
                        last_sent = now;
                    }
                    Ok(false) => {}
                    Err(e) => return Some(e),
                }
            }
            match self.enc.drain() {
                Ok(frames) => {
                    for (k, ts, data) in frames {
                        if !announced {
                            if !k {
                                continue; // nothing until the first key frame
                            }
                            announced = true;
                            sink(Out::Started {
                                w: self.out_w,
                                h: self.out_h,
                                fps: c.fps,
                                codec: self.enc.params.codec().unwrap_or_else(|| "avc1.640028".into()),
                                encoder: self.enc.name.clone(),
                                hardware: self.enc.hardware,
                                audio: _o.audio != Audio::Off,
                            });
                        }
                        sink(Out::Video { key: k, ts, data });
                    }
                }
                Err(e) => return Some(e),
            }
            std::thread::sleep(Duration::from_millis(1));
        }
        None
    }

    /// Copies a captured frame into our own texture (kept, so a still picture
    /// can be sent again).
    unsafe fn take(&mut self, f: &windows::Graphics::Capture::Direct3D11CaptureFrame) -> Result<(), String> {
        let size = f.ContentSize().map_err(|e| err("frame", e))?;
        if size.Width <= 0 || size.Height <= 0 {
            return Ok(());
        }
        // The window changed size: capture at the new size; it's fitted into
        // the same output frame.
        if size.Width != self.pool_size.Width || size.Height != self.pool_size.Height {
            self.pool_size = size;
            let _ = self.pool.Recreate(&self.gpu.winrt, DirectXPixelFormat::B8G8R8A8UIntNormalized, 2, size);
        }
        let surface = f.Surface().map_err(|e| err("frame", e))?;
        let access: IDirect3DDxgiInterfaceAccess = surface.cast().map_err(|e| err("frame", e))?;
        let src: ID3D11Texture2D = access.GetInterface().map_err(|e| err("frame", e))?;
        let mut d = D3D11_TEXTURE2D_DESC::default();
        src.GetDesc(&mut d);
        let (w, h) = ((size.Width as u32).min(d.Width), (size.Height as u32).min(d.Height));
        if self.conv.as_ref().is_none_or(|c| c.in_w != w || c.in_h != h) {
            self.conv = Some(convert(&self.gpu, w, h, self.out_w, self.out_h, 60)?);
            for s in &mut self.slots {
                s.view = None;
            }
        }
        let conv = self.conv.as_ref().unwrap();
        let bx = D3D11_BOX { left: 0, top: 0, front: 0, right: w, bottom: h, back: 1 };
        self.gpu.context.CopySubresourceRegion(&conv.input, 0, 0, 0, 0, &src, 0, Some(&bx));
        Ok(())
    }

    /// Converts the kept picture and hands it to the encoder. False if the
    /// encoder wasn't ready for it.
    unsafe fn encode(&mut self, ts_us: i64, key: bool) -> Result<bool, String> {
        let Some(conv) = self.conv.as_ref() else { return Ok(false) };
        let i = self.next_slot;
        self.next_slot = (i + 1) % self.slots.len();
        let slot = &mut self.slots[i];
        if slot.view.is_none() {
            let desc = D3D11_VIDEO_PROCESSOR_OUTPUT_VIEW_DESC {
                ViewDimension: D3D11_VPOV_DIMENSION_TEXTURE2D,
                Anonymous: D3D11_VIDEO_PROCESSOR_OUTPUT_VIEW_DESC_0 { Texture2D: D3D11_TEX2D_VPOV { MipSlice: 0 } },
            };
            let mut v = None;
            self.gpu.vdevice.CreateVideoProcessorOutputView(&slot.tex, &conv.enumerator, &desc, Some(&mut v)).map_err(|e| err("Video processor output", e))?;
            slot.view = v;
        }
        let view = slot.view.as_ref().ok_or("no output view")?;
        let stream = D3D11_VIDEO_PROCESSOR_STREAM { Enable: BOOL(1), pInputSurface: ManuallyDrop::new(Some(conv.in_view.clone())), ..Default::default() };
        let mut streams = [stream];
        let r = self.gpu.vcontext.VideoProcessorBlt(&conv.processor, view, 0, &streams);
        ManuallyDrop::drop(&mut streams[0].pInputSurface);
        r.map_err(|e| err("Couldn't convert the picture", e))?;
        let tex = slot.tex.clone();
        self.enc.push(&self.gpu, &tex, ts_us, key)
    }
}

// ------------------------------------------------------------------ encoder

fn var_u32(v: u32) -> VARIANT {
    let mut x = VARIANT::default();
    unsafe {
        let inner = &mut *x.Anonymous.Anonymous;
        inner.vt = VT_UI4;
        inner.Anonymous.ulVal = v;
    }
    x
}

fn var_bool(v: bool) -> VARIANT {
    let mut x = VARIANT::default();
    unsafe {
        let inner = &mut *x.Anonymous.Anonymous;
        inner.vt = VT_BOOL;
        inner.Anonymous.boolVal = windows::Win32::Foundation::VARIANT_BOOL(if v { -1 } else { 0 });
    }
    x
}

/// A Media Foundation H.264 encoder: a hardware one if the graphics card has
/// one that works with our device, else Microsoft's software one.
struct Encoder {
    mft: IMFTransform,
    api: Option<ICodecAPI>,
    events: Option<IMFMediaEventGenerator>,
    /// Inputs an async (hardware) encoder has asked for.
    need: u32,
    /// The encoder makes its own output samples.
    provides: bool,
    out_bytes: u32,
    /// Frames go to it as GPU surfaces (else copied to memory first).
    on_gpu: bool,
    staging: Option<ID3D11Texture2D>,
    _manager: Option<IMFDXGIDeviceManager>,
    w: u32,
    h: u32,
    fps: u32,
    pub params: ParamSets,
    pub name: String,
    pub hardware: bool,
}

unsafe fn friendly_name(a: &IMFActivate) -> String {
    let Ok(n) = a.GetStringLength(&MFT_FRIENDLY_NAME_Attribute) else { return "H.264 encoder".into() };
    let mut buf = vec![0u16; n as usize + 1];
    if a.GetString(&MFT_FRIENDLY_NAME_Attribute, &mut buf, None).is_err() {
        return "H.264 encoder".into();
    }
    String::from_utf16_lossy(&buf[..n as usize])
}

unsafe fn encoders(hardware: bool) -> Vec<IMFActivate> {
    let input = MFT_REGISTER_TYPE_INFO { guidMajorType: MFMediaType_Video, guidSubtype: MFVideoFormat_NV12 };
    let output = MFT_REGISTER_TYPE_INFO { guidMajorType: MFMediaType_Video, guidSubtype: MFVideoFormat_H264 };
    let flags = if hardware { MFT_ENUM_FLAG_HARDWARE | MFT_ENUM_FLAG_SORTANDFILTER } else { MFT_ENUM_FLAG_SYNCMFT | MFT_ENUM_FLAG_SORTANDFILTER };
    let mut list: *mut Option<IMFActivate> = std::ptr::null_mut();
    let mut n = 0u32;
    if MFTEnumEx(MFT_CATEGORY_VIDEO_ENCODER, flags, Some(&input), Some(&output), &mut list, &mut n).is_err() || list.is_null() {
        return Vec::new();
    }
    let items = std::slice::from_raw_parts_mut(list, n as usize);
    let out: Vec<IMFActivate> = items.iter_mut().filter_map(|a| a.take()).collect();
    windows::Win32::System::Com::CoTaskMemFree(Some(list as *const _));
    out
}

unsafe fn video_type(subtype: &GUID, w: u32, h: u32, fps: u32, bitrate: Option<u32>) -> windows::core::Result<IMFMediaType> {
    let t = MFCreateMediaType()?;
    t.SetGUID(&MF_MT_MAJOR_TYPE, &MFMediaType_Video)?;
    t.SetGUID(&MF_MT_SUBTYPE, subtype)?;
    t.SetUINT64(&MF_MT_FRAME_SIZE, ((w as u64) << 32) | h as u64)?;
    t.SetUINT64(&MF_MT_FRAME_RATE, ((fps as u64) << 32) | 1)?;
    t.SetUINT64(&MF_MT_PIXEL_ASPECT_RATIO, (1u64 << 32) | 1)?;
    t.SetUINT32(&MF_MT_INTERLACE_MODE, MFVideoInterlace_Progressive.0 as u32)?;
    t.SetUINT32(&MF_MT_YUV_MATRIX, MFVideoTransferMatrix_BT709.0 as u32)?;
    t.SetUINT32(&MF_MT_VIDEO_NOMINAL_RANGE, MFNominalRange_16_235.0 as u32)?;
    t.SetUINT32(&MF_MT_VIDEO_PRIMARIES, MFVideoPrimaries_BT709.0 as u32)?;
    t.SetUINT32(&MF_MT_TRANSFER_FUNCTION, MFVideoTransFunc_709.0 as u32)?;
    if let Some(b) = bitrate {
        t.SetUINT32(&MF_MT_AVG_BITRATE, b)?;
        t.SetUINT32(&MF_MT_MPEG2_PROFILE, eAVEncH264VProfile_High.0 as u32)?;
    }
    Ok(t)
}

impl Encoder {
    unsafe fn new(g: &Gpu, w: u32, h: u32, fps: u32, bitrate: u32, hint: Hint) -> Result<Encoder, String> {
        let mut token = 0u32;
        let mut manager = None;
        MFCreateDXGIDeviceManager(&mut token, &mut manager).map_err(|e| err("Media Foundation", e))?;
        let manager = manager.ok_or("no device manager")?;
        manager.ResetDevice(&g.device, token).map_err(|e| err("Media Foundation", e))?;
        let mut last = String::from("No H.264 encoder found on this PC.");
        for hardware in [true, false] {
            for a in encoders(hardware) {
                let name = friendly_name(&a);
                match Self::open(&a, &manager, hardware, w, h, fps, bitrate, hint) {
                    Ok(mut e) => {
                        e.name = name;
                        if !e.on_gpu {
                            e.staging = Some(texture(g, w, h, DXGI_FORMAT_NV12, 0, true)?);
                        }
                        return Ok(e);
                    }
                    Err(why) => {
                        last = format!("{name}: {why}");
                        let _ = a.ShutdownObject();
                    }
                }
            }
        }
        Err(format!("Couldn't start a video encoder. {last}"))
    }

    #[allow(clippy::too_many_arguments)]
    unsafe fn open(a: &IMFActivate, manager: &IMFDXGIDeviceManager, hardware: bool, w: u32, h: u32, fps: u32, bitrate: u32, hint: Hint) -> Result<Encoder, String> {
        let mft: IMFTransform = a.ActivateObject().map_err(|e| err("activate", e))?;
        let attrs = mft.GetAttributes().ok();
        let is_async = attrs.as_ref().is_some_and(|x| x.GetUINT32(&MF_TRANSFORM_ASYNC).unwrap_or(0) == 1);
        if is_async {
            if let Some(x) = &attrs {
                x.SetUINT32(&MF_TRANSFORM_ASYNC_UNLOCK, 1).map_err(|e| err("unlock", e))?;
            }
        }
        let mut on_gpu = false;
        if hardware && attrs.as_ref().is_some_and(|x| x.GetUINT32(&MF_SA_D3D11_AWARE).unwrap_or(0) == 1) {
            // Fails when this encoder belongs to another graphics card than
            // the one capturing (laptops with two): try the next one.
            mft.ProcessMessage(MFT_MESSAGE_SET_D3D_MANAGER, manager.as_raw() as usize).map_err(|e| err("not on this graphics card", e))?;
            on_gpu = true;
        }
        let api: Option<ICodecAPI> = mft.cast().ok();
        if let Some(api) = &api {
            let _ = api.SetValue(&CODECAPI_AVEncCommonRateControlMode, &var_u32(eAVEncCommonRateControlMode_CBR.0 as u32));
            let _ = api.SetValue(&CODECAPI_AVEncCommonMeanBitRate, &var_u32(bitrate));
            let _ = api.SetValue(&CODECAPI_AVLowLatencyMode, &var_bool(true));
            let _ = api.SetValue(&CODECAPI_AVEncMPVDefaultBPictureCount, &var_u32(0));
            // Key frames come when watchers need one; a regular one every
            // few seconds covers anything missed.
            let _ = api.SetValue(&CODECAPI_AVEncMPVGOPSize, &var_u32(fps * 4));
            // Sharp text wants quality over speed; games want speed.
            let q = match hint {
                Hint::Text => 80,
                Hint::Detail => 50,
                Hint::Motion => 33,
            };
            let _ = api.SetValue(&CODECAPI_AVEncCommonQualityVsSpeed, &var_u32(q));
        }
        let out = video_type(&MFVideoFormat_H264, w, h, fps, Some(bitrate)).map_err(|e| err("output type", e))?;
        mft.SetOutputType(0, &out, 0).map_err(|e| err("doesn't take this size or rate", e))?;
        let inp = video_type(&MFVideoFormat_NV12, w, h, fps, None).map_err(|e| err("input type", e))?;
        mft.SetInputType(0, &inp, 0).map_err(|e| err("doesn't take NV12 at this size", e))?;
        let info = mft.GetOutputStreamInfo(0).map_err(|e| err("stream info", e))?;
        let provides = info.dwFlags & (MFT_OUTPUT_STREAM_PROVIDES_SAMPLES.0 as u32 | MFT_OUTPUT_STREAM_CAN_PROVIDE_SAMPLES.0 as u32) != 0;
        mft.ProcessMessage(MFT_MESSAGE_NOTIFY_BEGIN_STREAMING, 0).map_err(|e| err("begin", e))?;
        mft.ProcessMessage(MFT_MESSAGE_NOTIFY_START_OF_STREAM, 0).map_err(|e| err("start", e))?;
        let events = if is_async { Some(mft.cast::<IMFMediaEventGenerator>().map_err(|e| err("events", e))?) } else { None };
        let mut params = ParamSets::default();
        if let Ok(t) = mft.GetOutputCurrentType(0) {
            learn_header(&t, &mut params);
        }
        Ok(Encoder {
            mft,
            api,
            events,
            need: 0,
            provides,
            out_bytes: info.cbSize.max(w * h * 3 / 2),
            on_gpu,
            staging: None,
            _manager: Some(manager.clone()),
            w,
            h,
            fps,
            params,
            name: String::new(),
            hardware,
        })
    }

    fn wants_input(&self) -> bool {
        self.events.is_none() || self.need > 0
    }

    unsafe fn set_bitrate(&self, b: u32) {
        if let Some(api) = &self.api {
            let _ = api.SetValue(&CODECAPI_AVEncCommonMeanBitRate, &var_u32(b));
        }
    }

    unsafe fn push(&mut self, g: &Gpu, tex: &ID3D11Texture2D, ts_us: i64, key: bool) -> Result<bool, String> {
        if !self.wants_input() {
            return Ok(false);
        }
        let sample = MFCreateSample().map_err(|e| err("sample", e))?;
        if self.on_gpu {
            let buf = MFCreateDXGISurfaceBuffer(&ID3D11Texture2D::IID, tex, 0, false).map_err(|e| err("surface buffer", e))?;
            sample.AddBuffer(&buf).map_err(|e| err("sample", e))?;
        } else {
            // Software encoder: copy the frame back from the graphics card.
            let staging = self.staging.as_ref().ok_or("no staging surface")?;
            g.context.CopyResource(staging, tex);
            let mut m = D3D11_MAPPED_SUBRESOURCE::default();
            g.context.Map(staging, 0, D3D11_MAP_READ, 0, Some(&mut m)).map_err(|e| err("read back", e))?;
            let (w, h) = (self.w as usize, self.h as usize);
            let size = w * h * 3 / 2;
            let buf = MFCreateMemoryBuffer(size as u32).map_err(|e| err("buffer", e))?;
            let mut p: *mut u8 = std::ptr::null_mut();
            if buf.Lock(&mut p, None, None).is_ok() {
                let src = m.pData as *const u8;
                let pitch = m.RowPitch as usize;
                for row in 0..h {
                    std::ptr::copy_nonoverlapping(src.add(row * pitch), p.add(row * w), w);
                }
                let uv = src.add(pitch * h);
                for row in 0..h / 2 {
                    std::ptr::copy_nonoverlapping(uv.add(row * pitch), p.add(w * h + row * w), w);
                }
                let _ = buf.Unlock();
                let _ = buf.SetCurrentLength(size as u32);
            }
            g.context.Unmap(staging, 0);
            sample.AddBuffer(&buf).map_err(|e| err("sample", e))?;
        }
        let _ = sample.SetSampleTime(ts_us * 10);
        let _ = sample.SetSampleDuration(10_000_000 / self.fps.max(1) as i64);
        if key {
            if let Some(api) = &self.api {
                let _ = api.SetValue(&CODECAPI_AVEncVideoForceKeyFrame, &var_u32(1));
            }
        }
        match self.mft.ProcessInput(0, &sample, 0) {
            Ok(()) => {
                if self.events.is_some() {
                    self.need -= 1;
                }
                Ok(true)
            }
            // MF_E_NOTACCEPTING: it's still busy with the last one.
            Err(e) if e.code() == HRESULT(0xC00D36B5_u32 as i32) => Ok(false),
            Err(e) => Err(err("The video encoder stopped", e)),
        }
    }

    /// Encoded frames that are ready: (key, timestamp µs, Annex B bytes).
    unsafe fn drain(&mut self) -> Result<Vec<(bool, u64, Vec<u8>)>, String> {
        let mut out = Vec::new();
        if let Some(ev) = self.events.clone() {
            loop {
                let e = match ev.GetEvent(MF_EVENT_FLAG_NO_WAIT) {
                    Ok(e) => e,
                    Err(e) if e.code() == MF_E_NO_EVENTS_AVAILABLE => break,
                    Err(e) => return Err(err("The video encoder stopped", e)),
                };
                match e.GetType().unwrap_or(0) as i32 {
                    t if t == METransformNeedInput.0 => self.need += 1,
                    t if t == METransformHaveOutput.0 => {
                        if let Some(f) = self.output()? {
                            out.push(f);
                        }
                    }
                    _ => {}
                }
            }
        } else {
            while let Some(f) = self.output()? {
                out.push(f);
            }
        }
        Ok(out)
    }

    unsafe fn output(&mut self) -> Result<Option<(bool, u64, Vec<u8>)>, String> {
        loop {
            let sample = if self.provides {
                None
            } else {
                let s = MFCreateSample().map_err(|e| err("sample", e))?;
                let b = MFCreateMemoryBuffer(self.out_bytes).map_err(|e| err("buffer", e))?;
                s.AddBuffer(&b).map_err(|e| err("sample", e))?;
                Some(s)
            };
            let mut db = [MFT_OUTPUT_DATA_BUFFER { dwStreamID: 0, pSample: ManuallyDrop::new(sample), dwStatus: 0, pEvents: ManuallyDrop::new(None) }];
            let mut status = 0u32;
            let r = self.mft.ProcessOutput(0, &mut db, &mut status);
            let sample = ManuallyDrop::take(&mut db[0].pSample);
            drop(ManuallyDrop::take(&mut db[0].pEvents));
            match r {
                Ok(()) => {}
                Err(e) if e.code() == MF_E_TRANSFORM_NEED_MORE_INPUT => return Ok(None),
                Err(e) if e.code() == MF_E_TRANSFORM_STREAM_CHANGE => {
                    // The encoder settled its output format: take it, and its SPS/PPS.
                    if let Ok(t) = self.mft.GetOutputAvailableType(0, 0) {
                        let _ = self.mft.SetOutputType(0, &t, 0);
                        learn_header(&t, &mut self.params);
                    }
                    continue;
                }
                Err(e) => return Err(err("The video encoder stopped", e)),
            }
            let Some(sample) = sample else { return Ok(None) };
            let key = sample.GetUINT32(&MFSampleExtension_CleanPoint).unwrap_or(0) == 1;
            let ts = (sample.GetSampleTime().unwrap_or(0) / 10).max(0) as u64;
            let buf = sample.ConvertToContiguousBuffer().map_err(|e| err("output", e))?;
            let mut p: *mut u8 = std::ptr::null_mut();
            let mut len = 0u32;
            buf.Lock(&mut p, None, Some(&mut len)).map_err(|e| err("output", e))?;
            let data = std::slice::from_raw_parts(p, len as usize).to_vec();
            let _ = buf.Unlock();
            let key = key || super::h264::is_idr(&data);
            let data = self.params.fix(data, key);
            return Ok(Some((key, ts, data)));
        }
    }

    unsafe fn end(&mut self) {
        let _ = self.mft.ProcessMessage(MFT_MESSAGE_NOTIFY_END_OF_STREAM, 0);
        let _ = self.mft.ProcessMessage(MFT_MESSAGE_NOTIFY_END_STREAMING, 0);
    }
}

unsafe fn learn_header(t: &IMFMediaType, params: &mut ParamSets) {
    if let Ok(n) = t.GetBlobSize(&MF_MT_MPEG_SEQUENCE_HEADER) {
        let mut b = vec![0u8; n as usize];
        if t.GetBlob(&MF_MT_MPEG_SEQUENCE_HEADER, &mut b, None).is_ok() {
            params.learn(&b);
        }
    }
}

// -------------------------------------------------------------------- sound

#[implement(IActivateAudioInterfaceCompletionHandler)]
struct Activated {
    done: HANDLE,
}

impl IActivateAudioInterfaceCompletionHandler_Impl for Activated_Impl {
    fn ActivateCompleted(&self, _op: Ref<IActivateAudioInterfaceAsyncOperation>) -> windows::core::Result<()> {
        unsafe { SetEvent(self.done) }
    }
}

/// Captures one process tree's sound (`include`), or everything but it, until
/// stopped, as stereo Opus.
unsafe fn sound(pid: u32, include: bool, c: &Control, sink: &Sink) -> Result<(), String> {
    let mut params = AUDIOCLIENT_ACTIVATION_PARAMS {
        ActivationType: AUDIOCLIENT_ACTIVATION_TYPE_PROCESS_LOOPBACK,
        Anonymous: AUDIOCLIENT_ACTIVATION_PARAMS_0 {
            ProcessLoopbackParams: AUDIOCLIENT_PROCESS_LOOPBACK_PARAMS {
                TargetProcessId: pid,
                ProcessLoopbackMode: if include { PROCESS_LOOPBACK_MODE_INCLUDE_TARGET_PROCESS_TREE } else { PROCESS_LOOPBACK_MODE_EXCLUDE_TARGET_PROCESS_TREE },
            },
        },
    };
    let mut pv = PROPVARIANT::default();
    {
        let inner = &mut *pv.Anonymous.Anonymous;
        inner.vt = windows::Win32::System::Variant::VT_BLOB;
        inner.Anonymous.blob = BLOB { cbSize: std::mem::size_of::<AUDIOCLIENT_ACTIVATION_PARAMS>() as u32, pBlobData: &mut params as *mut _ as *mut u8 };
    }
    let done = CreateEventW(None, false, false, None).map_err(|e| err("event", e))?;
    let handler: IActivateAudioInterfaceCompletionHandler = Activated { done }.into();
    let op = ActivateAudioInterfaceAsync(VIRTUAL_AUDIO_DEVICE_PROCESS_LOOPBACK, &IAudioClient::IID, Some(&pv), &handler)
        .map_err(|e| err("This version of Windows can't capture an app's sound (needs Windows 10 2004 or newer)", e))?;
    // The PROPVARIANT only points at `params`; don't let it free anything.
    std::mem::forget(pv);
    if WaitForSingleObject(done, 10_000) != WAIT_OBJECT_0 {
        let _ = CloseHandle(done);
        return Err("Windows didn't open the sound capture.".into());
    }
    let _ = CloseHandle(done);
    let mut hr = HRESULT(0);
    let mut unk = None;
    op.GetActivateResult(&mut hr, &mut unk).map_err(|e| err("sound capture", e))?;
    hr.ok().map_err(|e| err("Couldn't capture that sound", e))?;
    let client: IAudioClient = unk.ok_or("no audio client")?.cast().map_err(|e| err("sound capture", e))?;

    // Process loopback doesn't say its format; it converts to the one asked for.
    let (rate, ch) = (48_000u32, 2u16);
    let fmt = WAVEFORMATEX {
        wFormatTag: WAVE_FORMAT_PCM as u16,
        nChannels: ch,
        nSamplesPerSec: rate,
        nAvgBytesPerSec: rate * ch as u32 * 2,
        nBlockAlign: ch * 2,
        wBitsPerSample: 16,
        cbSize: 0,
    };
    client
        .Initialize(
            AUDCLNT_SHAREMODE_SHARED,
            AUDCLNT_STREAMFLAGS_LOOPBACK | AUDCLNT_STREAMFLAGS_EVENTCALLBACK | AUDCLNT_STREAMFLAGS_AUTOCONVERTPCM | AUDCLNT_STREAMFLAGS_SRC_DEFAULT_QUALITY,
            200_000, // 20 ms
            0,
            &fmt,
            None,
        )
        .map_err(|e| err("Couldn't start sound capture", e))?;
    let ready = CreateEventW(None, false, false, None).map_err(|e| err("event", e))?;
    client.SetEventHandle(ready).map_err(|e| err("sound capture", e))?;
    let cap: IAudioCaptureClient = client.GetService().map_err(|e| err("sound capture", e))?;
    let mut enc = AudioEncoder::new(rate, sink.clone())?;
    client.Start().map_err(|e| err("Couldn't start sound capture", e))?;
    let mut samples: Vec<f32> = Vec::new();
    while !c.stopped() {
        WaitForSingleObject(ready, 100);
        loop {
            let n = match cap.GetNextPacketSize() {
                Ok(n) => n,
                Err(_) => break,
            };
            if n == 0 {
                break;
            }
            let mut data: *mut u8 = std::ptr::null_mut();
            let mut frames = 0u32;
            let mut flags = 0u32;
            if cap.GetBuffer(&mut data, &mut frames, &mut flags, None, None).is_err() {
                break;
            }
            samples.clear();
            let count = frames as usize * ch as usize;
            if flags & AUDCLNT_BUFFERFLAGS_SILENT.0 as u32 != 0 || data.is_null() {
                samples.resize(count, 0.0);
            } else {
                let pcm = std::slice::from_raw_parts(data as *const i16, count);
                samples.extend(pcm.iter().map(|s| *s as f32 / 32768.0));
            }
            let _ = cap.ReleaseBuffer(frames);
            enc.feed_interleaved(&samples, ch as usize);
        }
    }
    let _ = client.Stop();
    let _ = CloseHandle(ready);
    Ok(())
}
