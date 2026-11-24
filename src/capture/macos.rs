//! macOS: ScreenCaptureKit (macOS 12.3+) for the picture and, from macOS 13,
//! the sound; VideoToolbox for hardware H.264. Both come with macOS. Their
//! calls are written out here by hand over the Objective-C runtime (`objc2`,
//! which Portside already uses for its window and menu bar).
//!
//! - ScreenCaptureKit is loaded at run time, so Portside still opens on macOS
//!   11 and 12.0–12.2 (sharing just says it needs a newer macOS).
//! - A window is captured on its own ("desktop independent"), so it keeps
//!   streaming when covered or on another Space. A minimized window isn't
//!   drawn by macOS, so nothing new comes until it's back.
//! - A screen share leaves out Portside's windows; the shared sound leaves out
//!   Portside's own (the room's voices).
//! - The first share asks for Screen Recording permission.

#![allow(non_upper_case_globals, non_snake_case, clippy::upper_case_acronyms)]

use std::ffi::{c_void, CStr};
use std::ptr::{null, null_mut};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::sync_channel;
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant};

use block2::RcBlock;
use objc2::encode::{Encode, Encoding, RefEncode};
use objc2::rc::{autoreleasepool, Allocated, Retained};
use objc2::runtime::{AnyClass, AnyObject, AnyProtocol, Bool, ClassBuilder, NSObject, Sel};
use objc2::{msg_send, sel, ClassType};

use super::h264::{avcc_to_annexb, ParamSets};
use std::collections::HashMap;

use super::{out_size, thumb_size, Audio, AudioEncoder, Cadence, Control, Hint, Options, Out, Running, Sink, Source, Thumb, ICON_SIZE, THUMB_H, THUMB_W};

// ------------------------------------------------------- C declarations

type CFTypeRef = *const c_void;
type CFStringRef = *const c_void;
type CFDictionaryRef = *const c_void;
type CFArrayRef = *const c_void;
type CMSampleBufferRef = *mut c_void;
type CVPixelBufferRef = *mut c_void;
type VTCompressionSessionRef = *mut c_void;

#[repr(C)]
#[derive(Clone, Copy, Debug, Default)]
struct CMTime {
    value: i64,
    timescale: i32,
    flags: u32,
    epoch: i64,
}

unsafe impl Encode for CMTime {
    const ENCODING: Encoding = Encoding::Struct("?", &[i64::ENCODING, i32::ENCODING, u32::ENCODING, i64::ENCODING]);
}

impl CMTime {
    fn new(value: i64, timescale: i32) -> CMTime {
        CMTime { value, timescale, flags: 1, epoch: 0 }
    }
    fn invalid() -> CMTime {
        CMTime::default()
    }
}

#[repr(C)]
#[derive(Clone, Copy, Debug, Default)]
struct CGPoint {
    x: f64,
    y: f64,
}
#[repr(C)]
#[derive(Clone, Copy, Debug, Default)]
struct CGSize {
    width: f64,
    height: f64,
}
#[repr(C)]
#[derive(Clone, Copy, Debug, Default)]
struct CGRect {
    origin: CGPoint,
    size: CGSize,
}
unsafe impl Encode for CGPoint {
    const ENCODING: Encoding = Encoding::Struct("CGPoint", &[f64::ENCODING, f64::ENCODING]);
}
unsafe impl Encode for CGSize {
    const ENCODING: Encoding = Encoding::Struct("CGSize", &[f64::ENCODING, f64::ENCODING]);
}
unsafe impl Encode for CGRect {
    const ENCODING: Encoding = Encoding::Struct("CGRect", &[CGPoint::ENCODING, CGSize::ENCODING]);
}
unsafe impl RefEncode for CGRect {
    const ENCODING_REF: Encoding = Encoding::Pointer(&Self::ENCODING);
}

/// A CoreFoundation string passed to an Objective-C method.
#[repr(transparent)]
#[derive(Clone, Copy)]
struct CFStr(CFStringRef);
unsafe impl Encode for CFStr {
    const ENCODING: Encoding = Encoding::Pointer(&Encoding::Struct("__CFString", &[]));
}

#[repr(C)]
struct AudioStreamBasicDescription {
    mSampleRate: f64,
    mFormatID: u32,
    mFormatFlags: u32,
    mBytesPerPacket: u32,
    mFramesPerPacket: u32,
    mBytesPerFrame: u32,
    mChannelsPerFrame: u32,
    mBitsPerChannel: u32,
    mReserved: u32,
}

#[repr(C)]
#[derive(Clone, Copy)]
struct AudioBuffer {
    mNumberChannels: u32,
    mDataByteSize: u32,
    mData: *mut c_void,
}

/// Room for up to 8 buffers (one per channel when planar).
#[repr(C)]
struct AudioBufferList8 {
    mNumberBuffers: u32,
    mBuffers: [AudioBuffer; 8],
}

#[link(name = "CoreFoundation", kind = "framework")]
extern "C" {
    static kCFBooleanTrue: CFTypeRef;
    static kCFBooleanFalse: CFTypeRef;
    static kCFTypeDictionaryKeyCallBacks: c_void;
    static kCFTypeDictionaryValueCallBacks: c_void;
    static kCFTypeArrayCallBacks: c_void;
    fn CFDictionaryCreate(a: CFTypeRef, keys: *const CFTypeRef, values: *const CFTypeRef, n: isize, kcb: *const c_void, vcb: *const c_void) -> CFDictionaryRef;
    fn CFDictionaryGetValue(d: CFDictionaryRef, key: CFTypeRef) -> CFTypeRef;
    fn CFNumberCreate(a: CFTypeRef, ty: isize, value: *const c_void) -> CFTypeRef;
    fn CFNumberGetValue(n: CFTypeRef, ty: isize, value: *mut c_void) -> bool;
    fn CFArrayCreate(a: CFTypeRef, values: *const CFTypeRef, n: isize, cb: *const c_void) -> CFArrayRef;
    fn CFArrayGetCount(a: CFArrayRef) -> isize;
    fn CFArrayGetValueAtIndex(a: CFArrayRef, i: isize) -> CFTypeRef;
    fn CFRetain(x: CFTypeRef) -> CFTypeRef;
    fn CFRelease(x: CFTypeRef);
    fn CFDataCreateMutable(a: CFTypeRef, capacity: isize) -> *mut c_void;
    fn CFDataGetLength(d: *const c_void) -> isize;
    fn CFDataGetBytePtr(d: *const c_void) -> *const u8;
    fn CFStringCreateWithCString(a: CFTypeRef, s: *const i8, encoding: u32) -> CFStringRef;
}
const kCFStringEncodingUTF8: u32 = 0x0800_0100;
const kCFNumberSInt32Type: isize = 3;
const kCFNumberSInt64Type: isize = 4;
const kCFNumberFloat64Type: isize = 6;

#[link(name = "CoreMedia", kind = "framework")]
extern "C" {
    static kCMSampleAttachmentKey_NotSync: CFStringRef;
    fn CMSampleBufferGetImageBuffer(sb: CMSampleBufferRef) -> CVPixelBufferRef;
    fn CMSampleBufferGetSampleAttachmentsArray(sb: CMSampleBufferRef, create: u8) -> CFArrayRef;
    fn CMSampleBufferGetPresentationTimeStamp(sb: CMSampleBufferRef) -> CMTime;
    fn CMSampleBufferGetDataBuffer(sb: CMSampleBufferRef) -> *mut c_void;
    fn CMSampleBufferGetFormatDescription(sb: CMSampleBufferRef) -> *mut c_void;
    fn CMSampleBufferIsValid(sb: CMSampleBufferRef) -> u8;
    fn CMBlockBufferGetDataLength(bb: *mut c_void) -> usize;
    fn CMBlockBufferCopyDataBytes(bb: *mut c_void, offset: usize, len: usize, dest: *mut c_void) -> i32;
    fn CMVideoFormatDescriptionGetH264ParameterSetAtIndex(
        fd: *mut c_void,
        index: usize,
        ptr: *mut *const u8,
        size: *mut usize,
        count: *mut usize,
        nal_header_len: *mut i32,
    ) -> i32;
    fn CMAudioFormatDescriptionGetStreamBasicDescription(fd: *mut c_void) -> *const AudioStreamBasicDescription;
    fn CMSampleBufferGetAudioBufferListWithRetainedBlockBuffer(
        sb: CMSampleBufferRef,
        size_needed: *mut usize,
        abl: *mut AudioBufferList8,
        abl_size: usize,
        bb_alloc: CFTypeRef,
        bb_mem_alloc: CFTypeRef,
        flags: u32,
        block_out: *mut *mut c_void,
    ) -> i32;
}

#[link(name = "CoreVideo", kind = "framework")]
extern "C" {
    static kCVImageBufferColorPrimaries_ITU_R_709_2: CFStringRef;
    static kCVImageBufferTransferFunction_ITU_R_709_2: CFStringRef;
    static kCVImageBufferYCbCrMatrix_ITU_R_709_2: CFStringRef;
}

#[link(name = "VideoToolbox", kind = "framework")]
extern "C" {
    static kVTVideoEncoderSpecification_EnableHardwareAcceleratedVideoEncoder: CFStringRef;
    static kVTCompressionPropertyKey_RealTime: CFStringRef;
    static kVTCompressionPropertyKey_ProfileLevel: CFStringRef;
    static kVTProfileLevel_H264_High_AutoLevel: CFStringRef;
    static kVTCompressionPropertyKey_AllowFrameReordering: CFStringRef;
    static kVTCompressionPropertyKey_MaxKeyFrameInterval: CFStringRef;
    static kVTCompressionPropertyKey_AverageBitRate: CFStringRef;
    static kVTCompressionPropertyKey_DataRateLimits: CFStringRef;
    static kVTCompressionPropertyKey_ExpectedFrameRate: CFStringRef;
    static kVTCompressionPropertyKey_ColorPrimaries: CFStringRef;
    static kVTCompressionPropertyKey_TransferFunction: CFStringRef;
    static kVTCompressionPropertyKey_YCbCrMatrix: CFStringRef;
    static kVTCompressionPropertyKey_UsingHardwareAcceleratedVideoEncoder: CFStringRef;
    static kVTCompressionPropertyKey_Quality: CFStringRef;
    static kVTEncodeFrameOptionKey_ForceKeyFrame: CFStringRef;
    fn VTCompressionSessionCreate(
        alloc: CFTypeRef,
        w: i32,
        h: i32,
        codec: u32,
        spec: CFDictionaryRef,
        src_attrs: CFDictionaryRef,
        data_alloc: CFTypeRef,
        cb: Option<unsafe extern "C" fn(*mut c_void, *mut c_void, i32, u32, CMSampleBufferRef)>,
        refcon: *mut c_void,
        out: *mut VTCompressionSessionRef,
    ) -> i32;
    fn VTSessionSetProperty(s: VTCompressionSessionRef, key: CFStringRef, value: CFTypeRef) -> i32;
    fn VTSessionCopyProperty(s: VTCompressionSessionRef, key: CFStringRef, alloc: CFTypeRef, out: *mut CFTypeRef) -> i32;
    fn VTCompressionSessionPrepareToEncodeFrames(s: VTCompressionSessionRef) -> i32;
    fn VTCompressionSessionEncodeFrame(
        s: VTCompressionSessionRef,
        img: CVPixelBufferRef,
        pts: CMTime,
        dur: CMTime,
        props: CFDictionaryRef,
        src_refcon: *mut c_void,
        info: *mut u32,
    ) -> i32;
    fn VTCompressionSessionCompleteFrames(s: VTCompressionSessionRef, until: CMTime) -> i32;
    fn VTCompressionSessionInvalidate(s: VTCompressionSessionRef);
}

#[link(name = "CoreGraphics", kind = "framework")]
extern "C" {
    static kCGDisplayStreamYCbCrMatrix_ITU_R_709_2: CFStringRef;
    fn CGPreflightScreenCaptureAccess() -> bool;
    fn CGRequestScreenCaptureAccess() -> bool;
    fn CGMainDisplayID() -> u32;
    fn CGDisplayCopyDisplayMode(d: u32) -> *mut c_void;
    fn CGDisplayModeGetPixelWidth(m: *mut c_void) -> usize;
    fn CGDisplayModeGetWidth(m: *mut c_void) -> usize;
    fn CGDisplayModeRelease(m: *mut c_void);
    fn CGImageGetWidth(i: CGImageRef) -> usize;
    fn CGImageGetHeight(i: CGImageRef) -> usize;
    fn CGImageRelease(i: CGImageRef);
    fn CGColorSpaceCreateDeviceRGB() -> *mut c_void;
    fn CGColorSpaceRelease(c: *mut c_void);
    fn CGBitmapContextCreate(data: *mut c_void, w: usize, h: usize, bits: usize, row: usize, cs: *mut c_void, info: u32) -> *mut c_void;
    fn CGBitmapContextCreateImage(c: *mut c_void) -> CGImageRef;
    fn CGContextSetInterpolationQuality(c: *mut c_void, q: i32);
    fn CGContextDrawImage(c: *mut c_void, r: CGRect, i: CGImageRef);
    fn CGContextRelease(c: *mut c_void);
}

type CGImageRef = *mut c_void;

#[link(name = "ImageIO", kind = "framework")]
extern "C" {
    static kCGImageDestinationLossyCompressionQuality: CFStringRef;
    fn CGImageDestinationCreateWithData(data: *mut c_void, ty: CFStringRef, count: usize, opts: CFDictionaryRef) -> *mut c_void;
    fn CGImageDestinationAddImage(d: *mut c_void, i: CGImageRef, props: CFDictionaryRef);
    fn CGImageDestinationFinalize(d: *mut c_void) -> bool;
}

extern "C" {
    fn dlopen(path: *const i8, mode: i32) -> *mut c_void;
    fn dlsym(handle: *mut c_void, name: *const i8) -> *mut c_void;
    fn getpid() -> i32;
    fn dispatch_queue_create(label: *const i8, attr: *const c_void) -> *mut c_void;
}

const kCMVideoCodecType_H264: u32 = u32::from_be_bytes(*b"avc1");
/// '420v': NV12, video range — what VideoToolbox encodes from directly.
const kCVPixelFormatType_420YpCbCr8BiPlanarVideoRange: u32 = u32::from_be_bytes(*b"420v");
const kAudioFormatFlagIsFloat: u32 = 1;
const kAudioFormatFlagIsNonInterleaved: u32 = 1 << 5;

fn cfnum_i32(v: i32) -> CFTypeRef {
    unsafe { CFNumberCreate(null(), kCFNumberSInt32Type, &v as *const i32 as *const c_void) }
}
fn cfnum_f64(v: f64) -> CFTypeRef {
    unsafe { CFNumberCreate(null(), kCFNumberFloat64Type, &v as *const f64 as *const c_void) }
}

/// Sets a property, releasing `value` (which we made).
unsafe fn set_prop(s: VTCompressionSessionRef, key: CFStringRef, value: CFTypeRef, ours: bool) -> i32 {
    let r = VTSessionSetProperty(s, key, value);
    if ours {
        CFRelease(value);
    }
    r
}

// --------------------------------------------------- Objective-C helpers

/// ScreenCaptureKit, loaded at run time (macOS 12.3+).
fn sck() -> Result<(), String> {
    static LOADED: OnceLock<bool> = OnceLock::new();
    let ok = *LOADED.get_or_init(|| unsafe {
        let p = c"/System/Library/Frameworks/ScreenCaptureKit.framework/ScreenCaptureKit";
        !dlopen(p.as_ptr(), 1 /* RTLD_LAZY */).is_null() && AnyClass::get(c"SCShareableContent").is_some()
    });
    if ok {
        Ok(())
    } else {
        Err("Screen sharing from a Mac needs macOS 12.3 or newer. You can still watch others.".into())
    }
}

fn class(name: &CStr) -> Result<&'static AnyClass, String> {
    AnyClass::get(name).ok_or_else(|| format!("{} is missing", name.to_string_lossy()))
}

unsafe fn rust_string(s: *mut AnyObject) -> String {
    if s.is_null() {
        return String::new();
    }
    let p: *const i8 = msg_send![&*s, UTF8String];
    if p.is_null() {
        String::new()
    } else {
        CStr::from_ptr(p).to_string_lossy().into_owned()
    }
}

unsafe fn error_text(e: *mut AnyObject) -> String {
    if e.is_null() {
        return "unknown error".into();
    }
    let d: *mut AnyObject = msg_send![&*e, localizedDescription];
    rust_string(d)
}

unsafe fn array_items(a: *mut AnyObject) -> Vec<*mut AnyObject> {
    if a.is_null() {
        return Vec::new();
    }
    let n: usize = msg_send![&*a, count];
    (0..n).map(|i| msg_send![&*a, objectAtIndex: i]).collect()
}

unsafe fn nsarray(items: &[*mut AnyObject]) -> Retained<AnyObject> {
    msg_send![class(c"NSArray").unwrap(), arrayWithObjects: items.as_ptr(), count: items.len()]
}

/// The Mac's Retina factor (pixels per point) for a display.
fn scale_of(display: u32) -> f64 {
    unsafe {
        let m = CGDisplayCopyDisplayMode(display);
        if m.is_null() {
            return 2.0;
        }
        let (px, pt) = (CGDisplayModeGetPixelWidth(m), CGDisplayModeGetWidth(m));
        CGDisplayModeRelease(m);
        if pt == 0 {
            1.0
        } else {
            (px as f64 / pt as f64).max(1.0)
        }
    }
}

/// What ScreenCaptureKit lets us share right now.
struct Content(Retained<AnyObject>);

unsafe fn shareable() -> Result<Content, String> {
    sck()?;
    if !CGPreflightScreenCaptureAccess() {
        CGRequestScreenCaptureAccess();
        return Err("Portside needs Screen Recording permission. Turn it on in System Settings → Privacy & Security → Screen Recording, then quit and reopen Portside.".into());
    }
    let (tx, rx) = sync_channel::<Result<SendObj, String>>(1);
    let block = RcBlock::new(move |content: *mut AnyObject, error: *mut AnyObject| {
        let r = if content.is_null() {
            Err(format!("Couldn't list screens and windows: {}", error_text(error)))
        } else {
            Ok(SendObj(Retained::retain(content).unwrap()))
        };
        let _ = tx.try_send(r);
    });
    let cls = class(c"SCShareableContent")?;
    let _: () = msg_send![cls, getShareableContentExcludingDesktopWindows: Bool::YES, onScreenWindowsOnly: Bool::NO, completionHandler: &*block];
    match rx.recv_timeout(Duration::from_secs(10)) {
        Ok(Ok(o)) => Ok(Content(o.0)),
        Ok(Err(e)) => Err(e),
        Err(_) => Err("macOS didn't list the screens and windows in time.".into()),
    }
}

/// An Objective-C object handed between threads. ScreenCaptureKit's objects
/// are safe to use from any thread.
struct SendObj(Retained<AnyObject>);
unsafe impl Send for SendObj {}

impl Content {
    unsafe fn displays(&self) -> Vec<*mut AnyObject> {
        array_items(msg_send![&*self.0, displays])
    }
    unsafe fn windows(&self) -> Vec<*mut AnyObject> {
        array_items(msg_send![&*self.0, windows])
    }
    unsafe fn applications(&self) -> Vec<*mut AnyObject> {
        array_items(msg_send![&*self.0, applications])
    }
    unsafe fn own_app(&self) -> Option<*mut AnyObject> {
        let me = getpid();
        self.applications().into_iter().find(|a| {
            let pid: i32 = msg_send![&**a, processID];
            pid == me
        })
    }
}

unsafe fn app_of(w: *mut AnyObject) -> (String, i32) {
    let app: *mut AnyObject = msg_send![&*w, owningApplication];
    if app.is_null() {
        return (String::new(), 0);
    }
    let name: *mut AnyObject = msg_send![&*app, applicationName];
    let pid: i32 = msg_send![&*app, processID];
    (rust_string(name), pid)
}

pub fn audio_modes() -> Vec<&'static str> {
    // Sound capture is ScreenCaptureKit on macOS 13+.
    let ok = sck().is_ok()
        && AnyClass::get(c"SCStreamConfiguration").is_some_and(|c| c.responds_to(sel!(setCapturesAudio:)));
    if ok {
        vec!["app", "system"]
    } else {
        Vec::new()
    }
}

pub fn sources() -> Result<Vec<Source>, String> {
    autoreleasepool(|_| unsafe {
        let content = shareable()?;
        let mut out = Vec::new();
        let main = CGMainDisplayID();
        let mut displays = content.displays();
        displays.sort_by_key(|d| {
            let id: u32 = msg_send![&**d, displayID];
            id != main
        });
        let many = displays.len() > 1;
        for (i, d) in displays.iter().enumerate() {
            let id: u32 = msg_send![&**d, displayID];
            let w: isize = msg_send![&**d, width];
            let h: isize = msg_send![&**d, height];
            let s = scale_of(id);
            let title = match (many, id == main) {
                (false, _) => "Your screen".to_string(),
                (true, true) => format!("Screen {} (main)", i + 1),
                (true, false) => format!("Screen {}", i + 1),
            };
            out.push(Source {
                id: format!("screen:{id}"),
                screen: true,
                title,
                app: String::new(),
                pid: 0,
                w: (w as f64 * s) as u32,
                h: (h as f64 * s) as u32,
                minimized: false,
                main: true,
            });
        }
        let me = getpid();
        let s = scale_of(main);
        let mut regular: HashMap<i32, bool> = HashMap::new();
        for w in content.windows() {
            let layer: isize = msg_send![&*w, windowLayer];
            if layer != 0 {
                continue; // menus, the Dock, overlays
            }
            let (app, pid) = app_of(w);
            if pid == me || pid == 0 {
                continue;
            }
            let title = rust_string(msg_send![&*w, title]);
            let frame: CGRect = msg_send![&*w, frame];
            if frame.size.width < 60.0 || frame.size.height < 60.0 {
                continue;
            }
            let on: Bool = msg_send![&*w, isOnScreen];
            let id: u32 = msg_send![&*w, windowID];
            let (pw, ph) = ((frame.size.width * s) as u32, (frame.size.height * s) as u32);
            // Only apps with a Dock icon count (not menu bar helpers or agents).
            let app_window = *regular.entry(pid).or_insert_with(|| regular_app(pid));
            // Untitled but big and on screen: a game or a player; name it for its app.
            let named = if title.is_empty() && on.as_bool() && frame.size.width >= 400.0 && frame.size.height >= 300.0 { app.clone() } else { title.clone() };
            let main = super::is_main_window(&super::WindowFacts { title: &named, app: &app, w: pw, h: ph, app_window, overlay: false });
            out.push(Source {
                id: format!("window:{id}"),
                screen: false,
                title: if title.is_empty() { app.clone() } else { title },
                app,
                pid: pid as u32,
                w: pw,
                h: ph,
                // Off screen: minimized, hidden or on another Space.
                minimized: !on.as_bool(),
                main,
            });
        }
        Ok(out)
    })
}

/// Does the app show in the Dock (a regular app, not a menu bar helper)?
unsafe fn regular_app(pid: i32) -> bool {
    let Some(cls) = AnyClass::get(c"NSRunningApplication") else { return true };
    let app: *mut AnyObject = msg_send![cls, runningApplicationWithProcessIdentifier: pid];
    if app.is_null() {
        return false;
    }
    let policy: isize = msg_send![&*app, activationPolicy];
    policy == 0 // NSApplicationActivationPolicyRegular
}

// ------------------------------------------------------------- previews

/// The picker's previews: one still from ScreenCaptureKit (macOS 14+), or
/// from Core Graphics on 12.3 and 13; shrunk and made into JPEG by ImageIO.
/// The icon is the app's own (from its Dock icon).
pub fn thumbnails(ids: &[String], icons: bool, each: &mut dyn FnMut(&str, Thumb)) {
    autoreleasepool(|_| unsafe {
        let Ok(content) = shareable() else { return };
        let own: Vec<*mut AnyObject> = content.own_app().into_iter().collect();
        let mut icon_of: HashMap<i32, Option<Vec<u8>>> = HashMap::new();
        for id in ids {
            let Some((is_screen, obj, n)) = find(&content, id) else { continue };
            let mut th = Thumb::default();
            let on: bool = is_screen || {
                let b: Bool = msg_send![&*obj, isOnScreen];
                b.as_bool()
            };
            if on {
                let img = autoreleasepool(|_| still(is_screen, obj, n, &own));
                if !img.is_null() {
                    th.image = encode_image(img, THUMB_W, THUMB_H, false);
                    CGImageRelease(img);
                }
            }
            if icons && !is_screen {
                let (_, pid) = app_of(obj);
                th.icon = icon_of.entry(pid).or_insert_with(|| app_icon(pid)).clone();
            }
            each(id, th);
        }
    })
}

/// A still of a display or window (ours to release), or null.
unsafe fn still(is_screen: bool, obj: *mut AnyObject, n: u32, own: &[*mut AnyObject]) -> CGImageRef {
    let Some(manager) = AnyClass::get(c"SCScreenshotManager") else { return legacy_still(is_screen, n) };
    let Ok(filter_cls) = class(c"SCContentFilter") else { return null_mut() };
    let none: Vec<*mut AnyObject> = Vec::new();
    let a: Allocated<AnyObject> = msg_send![filter_cls, alloc];
    let (filter, w, h): (Option<Retained<AnyObject>>, f64, f64) = if is_screen {
        let f = msg_send![a, initWithDisplay: &*obj, excludingApplications: &*nsarray(own), exceptingWindows: &*nsarray(&none)];
        let w: isize = msg_send![&*obj, width];
        let h: isize = msg_send![&*obj, height];
        (f, w as f64, h as f64)
    } else {
        let f = msg_send![a, initWithDesktopIndependentWindow: &*obj];
        let frame: CGRect = msg_send![&*obj, frame];
        (f, frame.size.width, frame.size.height)
    };
    let Some(filter) = filter else { return null_mut() };
    // Points are enough: the picture is shrunk to a preview anyway.
    let (tw, th) = thumb_size(w.max(1.0) as u32, h.max(1.0) as u32, THUMB_W * 2, THUMB_H * 2);
    let Ok(config_cls) = class(c"SCStreamConfiguration") else { return null_mut() };
    let config: Retained<AnyObject> = msg_send![config_cls, new];
    let _: () = msg_send![&*config, setWidth: tw as usize];
    let _: () = msg_send![&*config, setHeight: th as usize];
    let _: () = msg_send![&*config, setShowsCursor: Bool::NO];
    let _: () = msg_send![&*config, setScalesToFit: Bool::YES];
    let (tx, rx) = sync_channel::<usize>(1);
    let block = RcBlock::new(move |img: *mut c_void, _error: *mut AnyObject| {
        if !img.is_null() {
            CFRetain(img);
        }
        let _ = tx.try_send(img as usize);
    });
    let _: () = msg_send![manager, captureImageWithFilter: &*filter, configuration: &*config, completionHandler: &*block];
    rx.recv_timeout(Duration::from_secs(3)).map(|p| p as CGImageRef).unwrap_or(null_mut())
}

/// macOS 12.3 and 13 have no ScreenCaptureKit stills; Core Graphics does it
/// (looked up at run time: newer SDKs no longer declare these).
unsafe fn legacy_still(is_screen: bool, n: u32) -> CGImageRef {
    const RTLD_DEFAULT: *mut c_void = -2isize as *mut c_void;
    if is_screen {
        let f = dlsym(RTLD_DEFAULT, c"CGDisplayCreateImage".as_ptr());
        if f.is_null() {
            return null_mut();
        }
        let f: unsafe extern "C" fn(u32) -> CGImageRef = std::mem::transmute(f);
        f(n)
    } else {
        let f = dlsym(RTLD_DEFAULT, c"CGWindowListCreateImage".as_ptr());
        if f.is_null() {
            return null_mut();
        }
        let f: unsafe extern "C" fn(CGRect, u32, u32, u32) -> CGImageRef = std::mem::transmute(f);
        // CGRectNull: just the window's own bounds.
        let null_rect = CGRect { origin: CGPoint { x: f64::INFINITY, y: f64::INFINITY }, size: CGSize::default() };
        // Including only this window; without its shadow, at point size.
        f(null_rect, 1 << 3, n, (1 << 0) | (1 << 4))
    }
}

unsafe fn app_icon(pid: i32) -> Option<Vec<u8>> {
    let cls = AnyClass::get(c"NSRunningApplication")?;
    let app: *mut AnyObject = msg_send![cls, runningApplicationWithProcessIdentifier: pid];
    if app.is_null() {
        return None;
    }
    let icon: *mut AnyObject = msg_send![&*app, icon];
    if icon.is_null() {
        return None;
    }
    let px = (ICON_SIZE * 2) as f64; // Retina
    let mut rect = CGRect { origin: CGPoint::default(), size: CGSize { width: px, height: px } };
    let img: *mut c_void = msg_send![&*icon, CGImageForProposedRect: &mut rect as *mut CGRect, context: null_mut::<AnyObject>(), hints: null_mut::<AnyObject>()];
    if img.is_null() {
        return None;
    }
    encode_image(img, ICON_SIZE * 2, ICON_SIZE * 2, true)
}

/// A CGImage, shrunk to fit `max_w`×`max_h`, as JPEG (or PNG, which keeps
/// transparency).
unsafe fn encode_image(img: CGImageRef, max_w: u32, max_h: u32, png: bool) -> Option<Vec<u8>> {
    let (w, h) = (CGImageGetWidth(img) as u32, CGImageGetHeight(img) as u32);
    if w == 0 || h == 0 {
        return None;
    }
    let (tw, th) = thumb_size(w, h, max_w, max_h);
    let cs = CGColorSpaceCreateDeviceRGB();
    // Premultiplied BGRA, 32-bit little-endian.
    let ctx = CGBitmapContextCreate(null_mut(), tw as usize, th as usize, 8, 0, cs, 2 | (2 << 12));
    CGColorSpaceRelease(cs);
    if ctx.is_null() {
        return None;
    }
    CGContextSetInterpolationQuality(ctx, 3); // high
    CGContextDrawImage(ctx, CGRect { origin: CGPoint::default(), size: CGSize { width: tw as f64, height: th as f64 } }, img);
    let small = CGBitmapContextCreateImage(ctx);
    CGContextRelease(ctx);
    if small.is_null() {
        return None;
    }
    let data = CFDataCreateMutable(null(), 0);
    let ty = CFStringCreateWithCString(null(), if png { c"public.png" } else { c"public.jpeg" }.as_ptr(), kCFStringEncodingUTF8);
    let dest = CGImageDestinationCreateWithData(data, ty, 1, null());
    let mut out = None;
    if !dest.is_null() {
        let q = cfnum_f64(0.75);
        let keys = [kCGImageDestinationLossyCompressionQuality as CFTypeRef];
        let vals = [q];
        let props = CFDictionaryCreate(null(), keys.as_ptr(), vals.as_ptr(), 1, &kCFTypeDictionaryKeyCallBacks, &kCFTypeDictionaryValueCallBacks);
        CGImageDestinationAddImage(dest, small, props);
        if CGImageDestinationFinalize(dest) {
            let n = CFDataGetLength(data) as usize;
            out = Some(std::slice::from_raw_parts(CFDataGetBytePtr(data), n).to_vec());
        }
        CFRelease(props);
        CFRelease(q);
        CFRelease(dest);
    }
    CFRelease(ty);
    CFRelease(data);
    CGImageRelease(small);
    out
}

// ------------------------------------------------------------- encoder

/// What the VideoToolbox callback needs. Lives as long as the session.
struct EncShared {
    sink: Sink,
    params: Mutex<ParamSets>,
    started: AtomicBool,
    info: Mutex<(u32, u32, u32, bool, bool)>, // w, h, fps, hardware, audio
    control: Arc<Control>,
}

unsafe extern "C" fn encoded(refcon: *mut c_void, _src: *mut c_void, status: i32, _flags: u32, sb: CMSampleBufferRef) {
    if status != 0 || sb.is_null() || CMSampleBufferIsValid(sb) == 0 {
        return;
    }
    let shared = &*(refcon as *const EncShared);
    // Key frame: no "not sync" attachment.
    let mut key = true;
    let att = CMSampleBufferGetSampleAttachmentsArray(sb, 0);
    if !att.is_null() && CFArrayGetCount(att) > 0 {
        let d = CFArrayGetValueAtIndex(att, 0);
        let v = CFDictionaryGetValue(d, kCMSampleAttachmentKey_NotSync);
        if !v.is_null() && v == kCFBooleanTrue {
            key = false;
        }
    }
    let fd = CMSampleBufferGetFormatDescription(sb);
    let mut len_size = 4usize;
    if key && !fd.is_null() {
        let mut sets = Vec::new();
        let mut count = 0usize;
        let mut nal_len = 4i32;
        for i in 0..2 {
            let mut p: *const u8 = null();
            let mut n = 0usize;
            if CMVideoFormatDescriptionGetH264ParameterSetAtIndex(fd, i, &mut p, &mut n, &mut count, &mut nal_len) == 0 && !p.is_null() {
                sets.push(std::slice::from_raw_parts(p, n).to_vec());
            }
        }
        len_size = nal_len.clamp(1, 4) as usize;
        if sets.len() == 2 {
            let pps = sets.pop().unwrap();
            let sps = sets.pop().unwrap();
            shared.params.lock().unwrap().set(sps, pps);
        }
    }
    let bb = CMSampleBufferGetDataBuffer(sb);
    if bb.is_null() {
        return;
    }
    let n = CMBlockBufferGetDataLength(bb);
    let mut avcc = vec![0u8; n];
    if CMBlockBufferCopyDataBytes(bb, 0, n, avcc.as_mut_ptr() as *mut c_void) != 0 {
        return;
    }
    let mut annexb = Vec::with_capacity(n + 64);
    avcc_to_annexb(&avcc, len_size, &mut annexb);
    let pts = CMSampleBufferGetPresentationTimeStamp(sb);
    let ts = if pts.timescale > 0 { (pts.value as i128 * 1_000_000 / pts.timescale as i128).max(0) as u64 } else { 0 };
    let mut params = shared.params.lock().unwrap();
    let data = params.fix(annexb, key);
    if !shared.started.load(Ordering::Relaxed) {
        if !key {
            return;
        }
        shared.started.store(true, Ordering::Relaxed);
        let (w, h, fps, hardware, audio) = *shared.info.lock().unwrap();
        (shared.sink)(Out::Started {
            w,
            h,
            fps,
            codec: params.codec().unwrap_or_else(|| "avc1.640028".into()),
            encoder: if hardware { "Apple VideoToolbox (hardware)".into() } else { "Apple VideoToolbox".into() },
            hardware,
            audio,
        });
    }
    drop(params);
    let _ = &shared.control;
    (shared.sink)(Out::Video { key, ts, data });
}

struct Encoder {
    session: VTCompressionSessionRef,
    shared: *const EncShared,
    hardware: bool,
}

impl Encoder {
    unsafe fn new(w: u32, h: u32, fps: u32, bitrate: u32, hint: Hint, shared: Arc<EncShared>) -> Result<Encoder, String> {
        let keys = [kVTVideoEncoderSpecification_EnableHardwareAcceleratedVideoEncoder];
        let vals = [kCFBooleanTrue];
        let spec = CFDictionaryCreate(null(), keys.as_ptr(), vals.as_ptr(), 1, &kCFTypeDictionaryKeyCallBacks, &kCFTypeDictionaryValueCallBacks);
        let refcon = Arc::into_raw(shared);
        let mut session: VTCompressionSessionRef = null_mut();
        let r = VTCompressionSessionCreate(null(), w as i32, h as i32, kCMVideoCodecType_H264, spec, null(), null(), Some(encoded), refcon as *mut c_void, &mut session);
        CFRelease(spec);
        if r != 0 || session.is_null() {
            drop(Arc::from_raw(refcon));
            return Err(format!("Couldn't start the video encoder (VideoToolbox error {r})."));
        }
        set_prop(session, kVTCompressionPropertyKey_RealTime, kCFBooleanTrue, false);
        set_prop(session, kVTCompressionPropertyKey_ProfileLevel, kVTProfileLevel_H264_High_AutoLevel, false);
        set_prop(session, kVTCompressionPropertyKey_AllowFrameReordering, kCFBooleanFalse, false);
        set_prop(session, kVTCompressionPropertyKey_MaxKeyFrameInterval, cfnum_i32((fps * 4) as i32), true);
        set_prop(session, kVTCompressionPropertyKey_ExpectedFrameRate, cfnum_i32(fps as i32), true);
        set_prop(session, kVTCompressionPropertyKey_ColorPrimaries, kCVImageBufferColorPrimaries_ITU_R_709_2, false);
        set_prop(session, kVTCompressionPropertyKey_TransferFunction, kCVImageBufferTransferFunction_ITU_R_709_2, false);
        set_prop(session, kVTCompressionPropertyKey_YCbCrMatrix, kCVImageBufferYCbCrMatrix_ITU_R_709_2, false);
        let q = match hint {
            Hint::Text => 0.85,
            Hint::Detail => 0.6,
            Hint::Motion => 0.4,
        };
        set_prop(session, kVTCompressionPropertyKey_Quality, cfnum_f64(q), true);
        let mut e = Encoder { session, shared: refcon, hardware: false };
        e.set_bitrate(bitrate);
        let mut hw: CFTypeRef = null();
        if VTSessionCopyProperty(session, kVTCompressionPropertyKey_UsingHardwareAcceleratedVideoEncoder, null(), &mut hw) == 0 && !hw.is_null() {
            e.hardware = hw == kCFBooleanTrue;
            CFRelease(hw);
        }
        VTCompressionSessionPrepareToEncodeFrames(session);
        Ok(e)
    }

    unsafe fn set_bitrate(&self, b: u32) {
        set_prop(self.session, kVTCompressionPropertyKey_AverageBitRate, cfnum_i32(b as i32), true);
        // A hard cap over each second, so a burst can't swamp the link.
        let bytes = cfnum_f64(b as f64 / 8.0 * 1.5);
        let secs = cfnum_f64(1.0);
        let items = [bytes, secs];
        let arr = CFArrayCreate(null(), items.as_ptr(), 2, &kCFTypeArrayCallBacks);
        set_prop(self.session, kVTCompressionPropertyKey_DataRateLimits, arr, true);
        CFRelease(bytes);
        CFRelease(secs);
    }

    unsafe fn encode(&self, px: CVPixelBufferRef, ts_us: i64, fps: u32, key: bool) {
        let props = if key {
            let k = [kVTEncodeFrameOptionKey_ForceKeyFrame];
            let v = [kCFBooleanTrue];
            CFDictionaryCreate(null(), k.as_ptr(), v.as_ptr(), 1, &kCFTypeDictionaryKeyCallBacks, &kCFTypeDictionaryValueCallBacks)
        } else {
            null()
        };
        let pts = CMTime::new(ts_us, 1_000_000);
        let dur = CMTime::new(1_000_000 / fps.max(1) as i64, 1_000_000);
        VTCompressionSessionEncodeFrame(self.session, px, pts, dur, props, null_mut(), null_mut());
        if !props.is_null() {
            CFRelease(props);
        }
    }
}

impl Drop for Encoder {
    fn drop(&mut self) {
        unsafe {
            VTCompressionSessionCompleteFrames(self.session, CMTime::invalid());
            VTCompressionSessionInvalidate(self.session);
            CFRelease(self.session as CFTypeRef);
            drop(Arc::from_raw(self.shared));
        }
    }
}

// ------------------------------------------------------- stream output

/// What the capture callbacks work on, shared with the refresh thread.
struct Capture {
    enc: Encoder,
    /// The newest picture, kept to send again when it doesn't change.
    last: CVPixelBufferRef,
    cadence: Cadence,
    last_sent: Instant,
    key: bool,
    rate: u32,
    start: Instant,
    fps: u32,
}

unsafe impl Send for Capture {}

impl Capture {
    unsafe fn keep(&mut self, px: CVPixelBufferRef) {
        if !self.last.is_null() {
            CFRelease(self.last as CFTypeRef);
        }
        self.last = CFRetain(px as CFTypeRef) as CVPixelBufferRef;
    }

    /// Sends the kept picture if it's time (or a key frame is wanted).
    unsafe fn maybe_send(&mut self, fresh: bool, c: &Control) {
        if self.last.is_null() {
            return;
        }
        self.key |= c.take_key();
        let b = c.bitrate.load(Ordering::Relaxed);
        if b != self.rate {
            self.rate = b;
            self.enc.set_bitrate(b);
        }
        let now = Instant::now();
        let idle = now.duration_since(self.last_sent) > Duration::from_millis(1000);
        if !(fresh || self.key || idle) || !self.cadence.due(now, c.fps_now()) {
            return;
        }
        let ts = now.duration_since(self.start).as_micros() as i64;
        self.enc.encode(self.last, ts, self.fps, self.key);
        self.key = false;
        self.last_sent = now;
    }
}

impl Drop for Capture {
    fn drop(&mut self) {
        if !self.last.is_null() {
            unsafe { CFRelease(self.last as CFTypeRef) };
        }
    }
}

/// One stream's callbacks.
struct Handler {
    video: Option<Arc<Mutex<Capture>>>,
    audio: Option<Mutex<Option<AudioEncoder>>>,
    control: Arc<Control>,
    sink: Sink,
    /// Told once when the stream stops by itself.
    ended: AtomicBool,
    paused: AtomicBool,
    window: bool,
}

/// Frame status values (SCFrameStatus).
const FRAME_COMPLETE: i64 = 0;
const FRAME_IDLE: i64 = 1;
const FRAME_SUSPENDED: i64 = 3;

unsafe fn frame_status(sb: CMSampleBufferRef) -> i64 {
    static KEY: OnceLock<usize> = OnceLock::new();
    let key = *KEY.get_or_init(|| {
        let p = dlsym(-2isize as *mut c_void /* RTLD_DEFAULT */, c"SCStreamFrameInfoStatus".as_ptr()) as *const CFTypeRef;
        if p.is_null() {
            0
        } else {
            *p as usize
        }
    });
    if key == 0 {
        return FRAME_COMPLETE;
    }
    let att = CMSampleBufferGetSampleAttachmentsArray(sb, 0);
    if att.is_null() || CFArrayGetCount(att) == 0 {
        return FRAME_COMPLETE;
    }
    let v = CFDictionaryGetValue(CFArrayGetValueAtIndex(att, 0), key as CFTypeRef);
    if v.is_null() {
        return FRAME_COMPLETE;
    }
    let mut n = 0i64;
    CFNumberGetValue(v, kCFNumberSInt64Type, &mut n as *mut i64 as *mut c_void);
    n
}

impl Handler {
    unsafe fn on_sample(&self, sb: CMSampleBufferRef, kind: isize) {
        if self.control.stopped() || sb.is_null() || CMSampleBufferIsValid(sb) == 0 {
            return;
        }
        match kind {
            0 => {
                let Some(v) = &self.video else { return };
                let status = frame_status(sb);
                if self.window {
                    // A minimized window stops being drawn.
                    let paused = status == FRAME_SUSPENDED;
                    if self.paused.swap(paused, Ordering::Relaxed) != paused {
                        (self.sink)(Out::Paused(paused));
                    }
                }
                let mut cap = v.lock().unwrap();
                if status == FRAME_COMPLETE {
                    let px = CMSampleBufferGetImageBuffer(sb);
                    if !px.is_null() {
                        cap.keep(px);
                        cap.maybe_send(true, &self.control);
                    }
                } else if status == FRAME_IDLE {
                    cap.maybe_send(false, &self.control);
                }
            }
            1 => {
                let Some(a) = &self.audio else { return };
                let mut guard = a.lock().unwrap();
                let fd = CMSampleBufferGetFormatDescription(sb);
                if fd.is_null() {
                    return;
                }
                let asbd = CMAudioFormatDescriptionGetStreamBasicDescription(fd);
                if asbd.is_null() {
                    return;
                }
                let asbd = &*asbd;
                if asbd.mFormatFlags & kAudioFormatFlagIsFloat == 0 || asbd.mBitsPerChannel != 32 {
                    return; // ScreenCaptureKit gives 32-bit float
                }
                if guard.is_none() {
                    *guard = AudioEncoder::new(asbd.mSampleRate as u32, self.sink.clone()).ok();
                }
                let Some(enc) = guard.as_mut() else { return };
                let mut abl: AudioBufferList8 = std::mem::zeroed();
                let mut block: *mut c_void = null_mut();
                let r = CMSampleBufferGetAudioBufferListWithRetainedBlockBuffer(
                    sb,
                    null_mut(),
                    &mut abl,
                    std::mem::size_of::<AudioBufferList8>(),
                    null(),
                    null(),
                    1, // 16-byte alignment
                    &mut block,
                );
                if r != 0 {
                    return;
                }
                let bufs = &abl.mBuffers[..(abl.mNumberBuffers as usize).min(8)];
                if asbd.mFormatFlags & kAudioFormatFlagIsNonInterleaved != 0 && !bufs.is_empty() {
                    let plane = |b: &AudioBuffer| std::slice::from_raw_parts(b.mData as *const f32, b.mDataByteSize as usize / 4);
                    let l = plane(&bufs[0]);
                    let r = if bufs.len() > 1 { plane(&bufs[1]) } else { l };
                    let n = l.len().min(r.len());
                    enc.feed_planar(&l[..n], &r[..n]);
                } else if let Some(b) = bufs.first() {
                    let s = std::slice::from_raw_parts(b.mData as *const f32, b.mDataByteSize as usize / 4);
                    enc.feed_interleaved(s, asbd.mChannelsPerFrame.max(1) as usize);
                }
                if !block.is_null() {
                    CFRelease(block);
                }
            }
            _ => {}
        }
    }

    unsafe fn on_stop(&self, error: *mut AnyObject) {
        if self.control.stopped() || self.ended.swap(true, Ordering::Relaxed) {
            return;
        }
        let why = error_text(error);
        if self.video.is_some() {
            (self.sink)(Out::Ended(if self.window { format!("The shared window is gone ({why}).") } else { format!("Screen capture stopped: {why}") }));
        } else {
            (self.sink)(Out::AudioFailed(why));
        }
    }
}

/// The Objective-C class ScreenCaptureKit calls back: an NSObject holding a
/// pointer to a [`Handler`].
fn output_class() -> &'static AnyClass {
    static CLS: OnceLock<usize> = OnceLock::new();
    let p = *CLS.get_or_init(|| {
        let mut b = ClassBuilder::new(c"PortsideShareOutput", NSObject::class()).expect("class name taken");
        b.add_ivar::<*mut c_void>(c"handler");
        unsafe extern "C-unwind" fn did_output(this: &AnyObject, _: Sel, _stream: *mut AnyObject, sb: *mut c_void, kind: isize) {
            if let Some(h) = handler_of(this) {
                autoreleasepool(|_| h.on_sample(sb, kind));
            }
        }
        unsafe extern "C-unwind" fn did_stop(this: &AnyObject, _: Sel, _stream: *mut AnyObject, error: *mut AnyObject) {
            if let Some(h) = handler_of(this) {
                h.on_stop(error);
            }
        }
        unsafe {
            b.add_method(sel!(stream:didOutputSampleBuffer:ofType:), did_output as unsafe extern "C-unwind" fn(_, _, _, _, _));
            b.add_method(sel!(stream:didStopWithError:), did_stop as unsafe extern "C-unwind" fn(_, _, _, _));
        }
        if let Some(p) = AnyProtocol::get(c"SCStreamOutput") {
            b.add_protocol(p);
        }
        if let Some(p) = AnyProtocol::get(c"SCStreamDelegate") {
            b.add_protocol(p);
        }
        b.register() as *const AnyClass as usize
    });
    unsafe { &*(p as *const AnyClass) }
}

unsafe fn handler_of(this: &AnyObject) -> Option<&Handler> {
    let ivar = this.class().instance_variable(c"handler")?;
    let p = *ivar.load::<*mut c_void>(this);
    (p as *const Handler).as_ref()
}

/// A running SCStream and what it calls back into.
struct Stream {
    stream: Retained<AnyObject>,
    output: Retained<AnyObject>,
    handler: *mut Handler,
}

unsafe impl Send for Stream {}

impl Stream {
    unsafe fn start(filter: &AnyObject, config: &AnyObject, handler: Handler, video: bool, audio: bool) -> Result<Stream, String> {
        let handler = Box::into_raw(Box::new(handler));
        let output: Retained<AnyObject> = msg_send![output_class(), new];
        if let Some(ivar) = output.class().instance_variable(c"handler") {
            *ivar.load_ptr::<*mut c_void>(&output) = handler as *mut c_void;
        }
        let a: Allocated<AnyObject> = msg_send![class(c"SCStream")?, alloc];
        let stream: Option<Retained<AnyObject>> = msg_send![a, initWithFilter: filter, configuration: config, delegate: &*output];
        let Some(stream) = stream else {
            drop(Box::from_raw(handler));
            return Err("Couldn't start screen capture.".into());
        };
        let queue = dispatch_queue_create(c"portside.share".as_ptr(), null());
        for (want, kind) in [(video, 0isize), (audio, 1isize)] {
            if !want {
                continue;
            }
            let mut err: *mut AnyObject = null_mut();
            let ok: Bool = msg_send![&*stream, addStreamOutput: &*output, type: kind, sampleHandlerQueue: queue, error: &mut err];
            if !ok.as_bool() {
                drop(Box::from_raw(handler));
                return Err(format!("Couldn't start screen capture: {}", error_text(err)));
            }
        }
        let (tx, rx) = sync_channel::<Option<String>>(1);
        let block = RcBlock::new(move |error: *mut AnyObject| {
            let _ = tx.try_send(if error.is_null() { None } else { Some(error_text(error)) });
        });
        let _: () = msg_send![&*stream, startCaptureWithCompletionHandler: &*block];
        match rx.recv_timeout(Duration::from_secs(15)) {
            Ok(None) => Ok(Stream { stream, output, handler }),
            Ok(Some(e)) => {
                drop(Box::from_raw(handler));
                Err(format!("macOS wouldn't start the capture: {e}"))
            }
            Err(_) => {
                drop(Box::from_raw(handler));
                Err("Screen capture didn't start in time.".into())
            }
        }
    }
}

impl Drop for Stream {
    fn drop(&mut self) {
        unsafe {
            let (tx, rx) = sync_channel::<()>(1);
            let block = RcBlock::new(move |_e: *mut AnyObject| {
                let _ = tx.try_send(());
            });
            let _: () = msg_send![&*self.stream, stopCaptureWithCompletionHandler: &*block];
            // Callbacks may still be running on the queue until it's stopped.
            let _ = rx.recv_timeout(Duration::from_secs(3));
            if let Some(ivar) = self.output.class().instance_variable(c"handler") {
                *ivar.load_ptr::<*mut c_void>(&self.output) = null_mut();
            }
            drop(Box::from_raw(self.handler));
        }
    }
}

// ----------------------------------------------------------------- start

struct Live {
    control: Arc<Control>,
    _video: Option<Stream>,
    _audio: Option<Stream>,
}

impl Running for Live {}

impl Drop for Live {
    fn drop(&mut self) {
        self.control.stop.store(true, Ordering::Relaxed);
    }
}

pub fn start(opts: Options, control: Arc<Control>, sink: Sink) -> Result<Box<dyn Running>, String> {
    autoreleasepool(|_| unsafe { start_inner(opts, control, sink) })
}

unsafe fn find(content: &Content, id: &str) -> Option<(bool, *mut AnyObject, u32)> {
    let (kind, n) = id.split_once(':')?;
    let n: u32 = n.parse().ok()?;
    if kind == "screen" {
        content.displays().into_iter().find(|d| {
            let i: u32 = msg_send![&**d, displayID];
            i == n
        }).map(|d| (true, d, n))
    } else {
        content.windows().into_iter().find(|w| {
            let i: u32 = msg_send![&**w, windowID];
            i == n
        }).map(|w| (false, w, n))
    }
}

unsafe fn start_inner(opts: Options, control: Arc<Control>, sink: Sink) -> Result<Box<dyn Running>, String> {
    let content = shareable()?;
    let (is_screen, obj, _) = find(&content, &opts.source.id).ok_or("That screen or window isn't there any more. Pick again.")?;
    let own: Vec<*mut AnyObject> = content.own_app().into_iter().collect();
    let none: Vec<*mut AnyObject> = Vec::new();
    let filter_cls = class(c"SCContentFilter")?;

    // The picture.
    let (filter, src_w, src_h, display) = if is_screen {
        let a: Allocated<AnyObject> = msg_send![filter_cls, alloc];
        let f: Option<Retained<AnyObject>> =
            msg_send![a, initWithDisplay: &*obj, excludingApplications: &*nsarray(&own), exceptingWindows: &*nsarray(&none)];
        let id: u32 = msg_send![&*obj, displayID];
        let w: isize = msg_send![&*obj, width];
        let h: isize = msg_send![&*obj, height];
        let s = scale_of(id);
        (f, (w as f64 * s) as u32, (h as f64 * s) as u32, id)
    } else {
        let a: Allocated<AnyObject> = msg_send![filter_cls, alloc];
        let f: Option<Retained<AnyObject>> = msg_send![a, initWithDesktopIndependentWindow: &*obj];
        let frame: CGRect = msg_send![&*obj, frame];
        let s = scale_of(CGMainDisplayID());
        (f, (frame.size.width * s) as u32, (frame.size.height * s) as u32, CGMainDisplayID())
    };
    let filter = filter.ok_or("Couldn't capture that screen or window.")?;
    let (out_w, out_h) = out_size(src_w.max(2), src_h.max(2), opts.max_h);
    let fps = control.fps;

    let config: Retained<AnyObject> = msg_send![class(c"SCStreamConfiguration")?, new];
    let _: () = msg_send![&*config, setWidth: out_w as usize];
    let _: () = msg_send![&*config, setHeight: out_h as usize];
    let _: () = msg_send![&*config, setMinimumFrameInterval: CMTime::new(1, fps as i32)];
    let _: () = msg_send![&*config, setPixelFormat: kCVPixelFormatType_420YpCbCr8BiPlanarVideoRange];
    let _: () = msg_send![&*config, setColorMatrix: CFStr(kCGDisplayStreamYCbCrMatrix_ITU_R_709_2)];
    let _: () = msg_send![&*config, setShowsCursor: Bool::YES];
    let _: () = msg_send![&*config, setQueueDepth: 5isize];
    let _: () = msg_send![&*config, setScalesToFit: Bool::YES];
    if config.class().responds_to(sel!(setPreservesAspectRatio:)) {
        let _: () = msg_send![&*config, setPreservesAspectRatio: Bool::YES];
    }

    let want_audio = opts.audio != Audio::Off && !audio_modes().is_empty();
    let shared = Arc::new(EncShared {
        sink: sink.clone(),
        params: Mutex::default(),
        started: AtomicBool::new(false),
        info: Mutex::new((out_w, out_h, fps, false, want_audio)),
        control: control.clone(),
    });
    let enc = Encoder::new(out_w, out_h, fps, control.max_bitrate, opts.hint, shared.clone())?;
    shared.info.lock().unwrap().3 = enc.hardware;
    let cap = Arc::new(Mutex::new(Capture {
        enc,
        last: null_mut(),
        cadence: Cadence::new(),
        last_sent: Instant::now(),
        key: true,
        rate: control.max_bitrate,
        start: Instant::now(),
        fps,
    }));
    let handler = Handler {
        video: Some(cap.clone()),
        audio: None,
        control: control.clone(),
        sink: sink.clone(),
        ended: AtomicBool::new(false),
        paused: AtomicBool::new(false),
        window: !is_screen,
    };
    let video = Stream::start(&filter, &config, handler, true, false)?;

    // A still picture brings no new frames: send the last one again now and
    // then, and at once when a watcher needs a key frame.
    let (c2, cap2) = (control.clone(), cap.clone());
    std::thread::spawn(move || {
        while !c2.stopped() {
            std::thread::sleep(Duration::from_millis(50));
            if let Ok(mut c) = cap2.lock() {
                unsafe { c.maybe_send(false, &c2) };
            }
        }
    });

    // The sound: its own stream, of one app or of everything but Portside.
    let mut audio = None;
    if want_audio {
        match sound_stream(&content, &opts, display, &own, &control, &sink) {
            Ok(s) => audio = Some(s),
            Err(e) => sink(Out::AudioFailed(e)),
        }
    } else if opts.audio != Audio::Off {
        sink(Out::AudioFailed("Sharing sound from a Mac needs macOS 13 or newer.".into()));
    }
    Ok(Box::new(Live { control, _video: Some(video), _audio: audio }))
}

unsafe fn sound_stream(content: &Content, opts: &Options, display: u32, own: &[*mut AnyObject], control: &Arc<Control>, sink: &Sink) -> Result<Stream, String> {
    let d = content
        .displays()
        .into_iter()
        .find(|x| {
            let i: u32 = msg_send![&**x, displayID];
            i == display
        })
        .or_else(|| content.displays().into_iter().next())
        .ok_or("No screen to capture sound from.")?;
    let none: Vec<*mut AnyObject> = Vec::new();
    let a: Allocated<AnyObject> = msg_send![class(c"SCContentFilter")?, alloc];
    let filter: Option<Retained<AnyObject>> = if opts.audio == Audio::App {
        let app = content
            .applications()
            .into_iter()
            .find(|x| {
                let pid: i32 = msg_send![&**x, processID];
                pid as u32 == opts.source.pid
            })
            .ok_or("That app isn't running any more.")?;
        msg_send![a, initWithDisplay: &*d, includingApplications: &*nsarray(&[app]), exceptingWindows: &*nsarray(&none)]
    } else {
        msg_send![a, initWithDisplay: &*d, excludingApplications: &*nsarray(own), exceptingWindows: &*nsarray(&none)]
    };
    let filter = filter.ok_or("Couldn't capture the sound.")?;
    let config: Retained<AnyObject> = msg_send![class(c"SCStreamConfiguration")?, new];
    let _: () = msg_send![&*config, setCapturesAudio: Bool::YES];
    let _: () = msg_send![&*config, setExcludesCurrentProcessAudio: Bool::YES];
    let _: () = msg_send![&*config, setSampleRate: 48_000isize];
    let _: () = msg_send![&*config, setChannelCount: 2isize];
    // The picture of this stream isn't used: keep it tiny and rare.
    let _: () = msg_send![&*config, setWidth: 2usize];
    let _: () = msg_send![&*config, setHeight: 2usize];
    let _: () = msg_send![&*config, setMinimumFrameInterval: CMTime::new(1, 1)];
    let handler = Handler {
        video: None,
        audio: Some(Mutex::new(None)),
        control: control.clone(),
        sink: sink.clone(),
        ended: AtomicBool::new(false),
        paused: AtomicBool::new(false),
        window: false,
    };
    Stream::start(&filter, &config, handler, false, true)
}
