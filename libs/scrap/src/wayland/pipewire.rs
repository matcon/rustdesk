use std::collections::HashMap;
use std::error::Error;
use std::os::unix::io::AsRawFd;
use std::process::Command;
use std::str::FromStr;
use std::sync::{
    atomic::{AtomicBool, AtomicU8, Ordering},
    Arc, Mutex,
};
use std::time::Duration;
use tracing::{debug, error, info, trace, warn};

use dbus::{
    arg::{OwnedFd, PropMap, RefArg, Variant},
    blocking::{Proxy, SyncConnection},
    message::{MatchRule, MessageType},
    Message,
};

use gstreamer as gst;
use gstreamer::prelude::*;
use gstreamer_app::AppSink;

use lazy_static::lazy_static;
use serde::{Deserialize, Serialize};

use base::platform::linux::CMD_SH;
use hbb_common::{anyhow::anyhow, bail, config, serde_json, ResultType};

use super::capturable::PixelProvider;
use super::capturable::{Capturable, Recorder};
use super::display::{clear_wayland_displays_cache, get_displays, Displays};
use super::remote_desktop_portal::OrgFreedesktopPortalRemoteDesktop as remote_desktop_portal;
use super::request_portal::OrgFreedesktopPortalRequestResponse;
use super::screencast_portal::OrgFreedesktopPortalScreenCast as screencast_portal;

lazy_static! {
    pub static ref RDP_SESSION_INFO: Mutex<Option<RdpSessionInfo>> = Mutex::new(None);
}

#[derive(Serialize, Deserialize)]
// For KDE Plasma only, because GNOME provides position info.
struct PipewireDisplayOffsetCache {
    // We need to compare the displays, because:
    // 1. On Archlinux KDE Plasma
    // 2. One display, and connect, remember share choice.
    // 3. Plug in another monitor.
    // 4. The portal will reuse the restore token, no new share choice dialog, but the share screen is different.
    //    The controlling side will see the new monitor.
    // All displays as one string for easy comparison
    // name1-x1-y1-width1-height1;name2-x2-y2-width2-height2;...
    display_key: String,
    restore_token: String,
    offsets: Vec<(i32, i32)>,
}

// KDE Plasma may not provide position info
static HAS_POSITION_ATTR: AtomicBool = AtomicBool::new(false);
static IS_SERVER_RUNNING: AtomicU8 = AtomicU8::new(0); // 0: uninitialized, 1:true, 2: false
static USE_REMOTE_DESKTOP: AtomicU8 = AtomicU8::new(0); // 0: uninitialized, 1:true, 2: false

pub(crate) fn can_use_remote_desktop_portal(portal: &Proxy<'_, &SyncConnection>) -> bool {
    if is_server_running() {
        USE_REMOTE_DESKTOP.store(2, Ordering::SeqCst);
        return false;
    }
    let v = USE_REMOTE_DESKTOP.load(Ordering::SeqCst);
    if v > 0 {
        return v == 1;
    }
    let use_rdp = match remote_desktop_portal::available_device_types(portal) {
        Ok(types) if types > 0 => true,
        _ => {
            debug!("RemoteDesktop portal has no available device types, falling back to ScreenCast portal");
            false
        }
    };
    USE_REMOTE_DESKTOP.store(if use_rdp { 1 } else { 2 }, Ordering::SeqCst);
    use_rdp
}

pub(crate) fn can_use_remote_desktop_portal_cached() -> bool {
    let v = USE_REMOTE_DESKTOP.load(Ordering::SeqCst);
    if v > 0 {
        return v == 1;
    }
    !is_server_running()
}

impl PipewireDisplayOffsetCache {
    fn displays_to_key(displays: &Arc<Displays>) -> String {
        displays
            .displays
            .iter()
            .map(|d| format!("{}-{}-{}-{}-{}", d.name, d.x, d.y, d.width, d.height))
            .collect::<Vec<String>>()
            .join(";")
    }
}

#[inline]
pub fn close_session() {
    let _ = RDP_SESSION_INFO.lock().unwrap().take();
    clear_wayland_displays_cache();
    HAS_POSITION_ATTR.store(false, Ordering::SeqCst);
}

#[inline]
pub fn is_rdp_session_hold() -> bool {
    RDP_SESSION_INFO.lock().unwrap().is_some()
}

pub fn try_close_session() {
    let mut rdp_info = RDP_SESSION_INFO.lock().unwrap();
    let mut close = false;
    if let Some(rdp_info) = &*rdp_info {
        // If screencast is used and restore token is supported, there's no need to keep the session.
        if (!can_use_remote_desktop_portal_cached()) && rdp_info.is_support_restore_token {
            close = true;
        }
    }
    if close {
        *rdp_info = None;
        clear_wayland_displays_cache();
        HAS_POSITION_ATTR.store(false, Ordering::SeqCst);
    }
}

pub struct RdpSessionInfo {
    pub conn: Arc<SyncConnection>,
    pub streams: Vec<PwStreamInfo>,
    pub fd: Option<OwnedFd>,
    pub session: dbus::Path<'static>,
    pub is_support_restore_token: bool,
    pub resolution: Arc<Mutex<Option<(usize, usize)>>>,
}
#[derive(Debug, Clone, Copy)]
pub struct PwStreamInfo {
    pub path: u64,
    source_type: u64,
    position: (i32, i32),
    size: (usize, usize),
}

impl PwStreamInfo {
    pub fn get_size(&self) -> (usize, usize) {
        self.size
    }

    pub fn get_position(&self) -> (i32, i32) {
        self.position
    }
}

#[derive(Debug)]
pub struct DBusError(String);

impl std::fmt::Display for DBusError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let Self(s) = self;
        write!(f, "{}", s)
    }
}

impl Error for DBusError {}

#[derive(Debug)]
pub struct GStreamerError(String);

impl std::fmt::Display for GStreamerError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let Self(s) = self;
        write!(f, "{}", s)
    }
}

impl Error for GStreamerError {}

#[derive(Clone)]
pub struct PipeWireCapturable {
    // connection needs to be kept alive for recording
    dbus_conn: Arc<SyncConnection>,
    fd: Option<OwnedFd>,
    path: u64,
    source_type: u64,
    pub primary: bool,
    pub position: (i32, i32),
    pub logical_size: (usize, usize),
    pub physical_size: (usize, usize),
}

impl PipeWireCapturable {
    fn new(
        conn: Arc<SyncConnection>,
        fd: Option<OwnedFd>,
        resolution: Arc<Mutex<Option<(usize, usize)>>>,
        stream: &PwStreamInfo,
    ) -> Self {
        let displays = super::display::get_displays();
        let matched = displays
            .displays
            .iter()
            .find(|d| d.x == stream.position.0 && d.y == stream.position.1)
            .or_else(|| displays.displays.first());

        let physical_size = if let Some(d) = matched {
            let (w, h) = if d.transform == 90 || d.transform == 270 {
                (d.height as usize, d.width as usize)
            } else {
                (d.width as usize, d.height as usize)
            };
            if w > 0 && h > 0 {
                (w, h)
            } else if stream.size.0 > 0 && stream.size.1 > 0 {
                stream.size
            } else {
                (1920, 1080)
            }
        } else if stream.size.0 > 0 && stream.size.1 > 0 {
            stream.size
        } else {
            get_res(Self {
                dbus_conn: conn.clone(),
                fd: fd.clone(),
                path: stream.path,
                source_type: stream.source_type,
                primary: false,
                position: stream.position,
                logical_size: stream.size,
                physical_size: (0, 0),
            })
            .unwrap_or(stream.size)
        };
        debug!("[pipewire] Resolved capturable size: physical={:?}, logical={:?}", physical_size, stream.size);
        *resolution.lock().unwrap() = Some(physical_size);
        Self {
            dbus_conn: conn,
            fd,
            path: stream.path,
            source_type: stream.source_type,
            primary: false,
            position: stream.position,
            logical_size: stream.size,
            physical_size,
        }
    }
}

impl std::fmt::Debug for PipeWireCapturable {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "PipeWireCapturable {{dbus: {}, fd: {}, path: {}, source_type: {}}}",
            self.dbus_conn.unique_name(),
            self.fd.as_ref().map(|f| f.as_raw_fd()).unwrap_or(-1),
            self.path,
            self.source_type
        )
    }
}

impl Capturable for PipeWireCapturable {
    fn name(&self) -> String {
        let type_str = match self.source_type {
            1 => "Desktop",
            2 => "Window",
            _ => "Unknow",
        };
        format!("Pipewire {}, path: {}", type_str, self.path)
    }

    fn geometry_relative(&self) -> Result<(f64, f64, f64, f64), Box<dyn Error>> {
        Ok((0.0, 0.0, 1.0, 1.0))
    }

    fn before_input(&mut self) -> Result<(), Box<dyn Error>> {
        Ok(())
    }

    fn recorder(&self, _capture_cursor: bool) -> Result<Box<dyn Recorder>, Box<dyn Error>> {
        Ok(Box::new(PipeWireRecorder::new(self.clone())?))
    }
}

fn get_res(capturable: PipeWireCapturable) -> Result<(usize, usize), Box<dyn Error>> {
    let rec = PipeWireRecorder::new(capturable)?;
    if let Some(sample) = rec
        .appsink
        .try_pull_sample(gst::ClockTime::from_mseconds(300))
    {
        let cap = sample
            .get_caps()
            .ok_or("Failed get caps")?
            .get_structure(0)
            .ok_or("Failed to get structure")?;
        let w: i32 = cap.get_value("width")?.get_some()?;
        let h: i32 = cap.get_value("height")?.get_some()?;
        let w = w as usize;
        let h = h as usize;
        Ok((w, h))
    } else {
        Err(Box::new(GStreamerError(
            "Error getting screen resolution".into(),
        )))
    }
}

pub struct PipeWireRecorder {
    buffer: Option<gst::MappedBuffer<gst::buffer::Readable>>,
    buffer_cropped: Vec<u8>,
    pix_fmt: String,
    is_cropped: bool,
    pipeline: gst::Pipeline,
    appsink: AppSink,
    width: usize,
    height: usize,
    saved_raw_data: Vec<u8>,
    dma_buffer: Vec<u8>,
}

unsafe fn try_mmap_dmabuf(
    buf: &gst::Buffer,
    w: usize,
    h: usize,
    out: &mut Vec<u8>,
) -> Option<()> {
    if buf.n_memory() == 0 {
        trace!("[dmabuf] buf.n_memory() == 0");
        return None;
    }

    type FnIsDmabuf = unsafe extern "C" fn(*mut std::ffi::c_void) -> i32;
    type FnGetFd = unsafe extern "C" fn(*mut std::ffi::c_void) -> std::os::raw::c_int;

    let handle = hbb_common::libc::dlopen(
        b"libgstallocators-1.0.so.0\0".as_ptr() as *const _,
        hbb_common::libc::RTLD_LAZY,
    );
    if handle.is_null() {
        debug!("[dmabuf] dlopen failed");
        return None;
    }

    let is_dmabuf_sym = hbb_common::libc::dlsym(handle, b"gst_is_dmabuf_memory\0".as_ptr() as *const _);
    let get_dmabuf_fd_sym = hbb_common::libc::dlsym(handle, b"gst_dmabuf_memory_get_fd\0".as_ptr() as *const _);
    let is_fd_sym = hbb_common::libc::dlsym(handle, b"gst_is_fd_memory\0".as_ptr() as *const _);
    let get_fd_sym = hbb_common::libc::dlsym(handle, b"gst_fd_memory_get_fd\0".as_ptr() as *const _);

    let mem_ref = buf.peek_memory(0);
    let mem = mem_ref.as_ptr() as *mut std::ffi::c_void;
    if mem.is_null() {
        trace!("[dmabuf] mem is null");
        hbb_common::libc::dlclose(handle);
        return None;
    }

    let mut dma_fd = -1;
    if !is_dmabuf_sym.is_null() && !get_dmabuf_fd_sym.is_null() {
        let is_dmabuf: FnIsDmabuf = std::mem::transmute(is_dmabuf_sym);
        let ret = is_dmabuf(mem);
        trace!("[dmabuf] is_dmabuf returned {}", ret);
        if ret != 0 {
            let get_fd: FnGetFd = std::mem::transmute(get_dmabuf_fd_sym);
            dma_fd = get_fd(mem);
        }
    }

    if dma_fd < 0 && !is_fd_sym.is_null() && !get_fd_sym.is_null() {
        let is_fd: FnIsDmabuf = std::mem::transmute(is_fd_sym);
        let ret = is_fd(mem);
        trace!("[dmabuf] is_fd returned {}", ret);
        if ret != 0 {
            let get_fd: FnGetFd = std::mem::transmute(get_fd_sym);
            dma_fd = get_fd(mem);
        }
    }

    hbb_common::libc::dlclose(handle);

    trace!("[dmabuf] extracted dma_fd: {}", dma_fd);
    if dma_fd < 0 {
        return None;
    }

    let size = w * h * 4;
    let addr = hbb_common::libc::mmap(
        std::ptr::null_mut(),
        size,
        hbb_common::libc::PROT_READ,
        hbb_common::libc::MAP_SHARED,
        dma_fd,
        0,
    );

    if addr == hbb_common::libc::MAP_FAILED {
        warn!("[dmabuf] mmap failed: {}", std::io::Error::last_os_error());
        return None;
    }

    trace!("[dmabuf] mmap succeeded: addr={:?}, size={}", addr, size);
    let slice = std::slice::from_raw_parts(addr as *const u8, size);
    out.clear();
    out.extend_from_slice(slice);
    hbb_common::libc::munmap(addr, size);

    Some(())
}

// Element creation fails the same way for a plugin that is not installed as for one that is
// broken, so the tag does not claim which. Only the name travels to the peer -- it is what
// says which package to look at -- and the factory's own error stays here in the log.
fn gst_element(name: &str) -> ResultType<gst::Element> {
    gst::ElementFactory::make(name, None).map_err(|e| {
        error!("Failed to create GStreamer element {}: {}", name, e);
        anyhow!(stage_err("gst-plugin", "unavailable", name))
    })
}

impl PipeWireRecorder {
    pub fn new(capturable: PipeWireCapturable) -> ResultType<Self> {
        let pipeline = gst::Pipeline::new(None);

        let src = gst_element("pipewiresrc")?;
        if let Some(ref fd) = capturable.fd {
            let raw_fd = fd.as_raw_fd();
            let dup_fd = unsafe { hbb_common::libc::dup(raw_fd) };
            if dup_fd >= 0 {
                debug!("[gstreamer] Bound pipewiresrc with dup_fd: {} (orig: {}), path: {}", dup_fd, raw_fd, capturable.path);
                src.set_property("fd", &dup_fd)?;
            }
        } else {
            debug!("[gstreamer] Bound pipewiresrc directly via path: {}", capturable.path);
        }
        src.set_property("path", &format!("{}", capturable.path))?;
        let _ = src.set_property("keepalive-time", &1000i32);

        let sink = gst_element("appsink")?;
        sink.set_property("drop", &true)?;
        sink.set_property("max-buffers", &1u32)?;

        pipeline.add_many(&[&src, &sink])?;
        src.link(&sink)?;
        let appsink = sink
            .dynamic_cast::<AppSink>()
            .map_err(|_| GStreamerError("Sink element is expected to be an appsink!".into()))?;

        let caps = if capturable.physical_size.0 > 0 && capturable.physical_size.1 > 0 {
            let (w, h) = (
                capturable.physical_size.0 as i32,
                capturable.physical_size.1 as i32,
            );
            debug!("[gstreamer] Constraining appsink caps to {}x{}", w, h);
            let caps_str = format!(
                "video/x-raw(memory:DMABuf),format=BGRx,width={w},height={h},framerate=0/1; \
                 video/x-raw(memory:DMABuf),format=RGBx,width={w},height={h},framerate=0/1; \
                 video/x-raw,format=BGRx,width={w},height={h}; \
                 video/x-raw,format=RGBx,width={w},height={h}"
            );
            gst::Caps::from_str(&caps_str).ok()
        } else {
            let caps_str = "\
                video/x-raw(memory:DMABuf),format=BGRx,framerate=0/1; \
                video/x-raw(memory:DMABuf),format=RGBx,framerate=0/1; \
                video/x-raw,format=BGRx; \
                video/x-raw,format=RGBx";
            gst::Caps::from_str(caps_str).ok()
        };
        appsink.set_caps(caps.as_ref());

        // [Workaround]
        // Crash may occur if there are multiple pipelines started at the same time.
        // `pipeline.get_state()` can significantly reduce the probability of crashes,
        // but cannot completely resolve this issue.
        // Adding a short sleep period can also reduce the probability of crashes.
        debug!("[gstreamer] Setting pipeline to PLAYING state...");
        pipeline.set_state(gst::State::Playing)?;
        trace!("[gstreamer] set_state called, waiting for state change...");

        // If using screencast_portal (multiple streams possible), wait for state change.
        if !can_use_remote_desktop_portal_cached() {
            // Wait for the state change to actually complete before proceeding.
            // The 2000ms timeout for pipeline state change was chosen based on empirical testing.
            let state_change = pipeline.get_state(gst::ClockTime::from_mseconds(2000));
            trace!("[gstreamer] pipeline state_change result: {:?}", state_change);
            match state_change {
                (Ok(_), gst::State::Playing, _) => {
                    debug!("[gstreamer] Pipeline state confirmed as PLAYING.");
                }
                (result, state, pending) => {
                    warn!(
                        "[gstreamer] Pipeline state change incomplete: result={:?}, state={:?}, pending={:?}",
                        result, state, pending
                    );
                }
            }
            std::thread::sleep(std::time::Duration::from_millis(150));
        }
        debug!("[gstreamer] PipeWireRecorder initialized successfully.");

        Ok(Self {
            pipeline,
            appsink,
            buffer: None,
            pix_fmt: "".into(),
            width: 0,
            height: 0,
            buffer_cropped: vec![],
            is_cropped: false,
            saved_raw_data: Vec::new(),
            dma_buffer: Vec::new(),
        })
    }
}

impl Recorder for PipeWireRecorder {
    fn capture(&mut self, timeout_ms: u64) -> Result<PixelProvider<'_>, Box<dyn Error>> {
        if let Some(sample) = self
            .appsink
            .try_pull_sample(gst::ClockTime::from_mseconds(timeout_ms))
        {
            let cap = sample
                .get_caps()
                .ok_or("Failed get caps")?
                .get_structure(0)
                .ok_or("Failed to get structure")?;
            let w: i32 = cap.get_value("width")?.get_some()?;
            let h: i32 = cap.get_value("height")?.get_some()?;
            let w = w as usize;
            let h = h as usize;
            self.pix_fmt = cap
                .get::<&str>("format")?
                .ok_or("Failed to get pixel format")?
                .to_string();

            let buf = sample
                .get_buffer_owned()
                .ok_or_else(|| GStreamerError("Failed to get owned buffer.".into()))?;
            let mut crop = buf
                .get_meta::<gstreamer_video::VideoCropMeta>()
                .map(|m| m.get_rect());
            // only crop if necessary
            if Some((0, 0, w as u32, h as u32)) == crop {
                crop = None;
            }
            // Check if buffer is DMA-BUF
            let is_dma = unsafe { try_mmap_dmabuf(&buf, w, h, &mut self.dma_buffer) };
            if is_dma.is_some() {
                self.buffer = None;
                if let Err(..) = crate::would_block_if_equal(&mut self.saved_raw_data, &self.dma_buffer) {
                    return Ok(PixelProvider::NONE);
                }
                if let Some((x_off, y_off, w_crop, h_crop)) = crop {
                    let x_off = x_off as usize;
                    let y_off = y_off as usize;
                    let w_crop = w_crop as usize;
                    let h_crop = h_crop as usize;
                    self.buffer_cropped.clear();
                    self.buffer_cropped.reserve(w_crop * h_crop * 4);
                    for y in y_off..(y_off + h_crop) {
                        let i = 4 * (w * y + x_off);
                        self.buffer_cropped.extend_from_slice(&self.dma_buffer[i..i + 4 * w_crop]);
                    }
                    self.width = w_crop;
                    self.height = h_crop;
                } else {
                    self.width = w;
                    self.height = h;
                }
                self.is_cropped = crop.is_some();
            } else {
                let buf = buf
                    .into_mapped_buffer_readable()
                    .map_err(|_| GStreamerError("Failed to map buffer.".into()))?;
                if let Err(..) = crate::would_block_if_equal(&mut self.saved_raw_data, buf.as_slice()) {
                    return Ok(PixelProvider::NONE);
                }
                let buf_size = buf.get_size();
                // BGRx is 4 bytes per pixel
                if buf_size != (w * h * 4) {
                    // for some reason the width and height of the caps do not guarantee correct buffer
                    // size, so ignore those buffers, see:
                    // https://gitlab.freedesktop.org/pipewire/pipewire/-/issues/985
                    trace!(
                        "Size of mapped buffer: {} does NOT match size of capturable {}x{}@BGRx, \
                        dropping it!",
                        buf_size,
                        w,
                        h
                    );
                } else {
                    // Copy region specified by crop into self.buffer_cropped
                    // TODO: Figure out if ffmpeg provides a zero copy alternative
                    if let Some((x_off, y_off, w_crop, h_crop)) = crop {
                        let x_off = x_off as usize;
                        let y_off = y_off as usize;
                        let w_crop = w_crop as usize;
                        let h_crop = h_crop as usize;
                        self.buffer_cropped.clear();
                        let data = buf.as_slice();
                        // BGRx is 4 bytes per pixel
                        self.buffer_cropped.reserve(w_crop * h_crop * 4);
                        for y in y_off..(y_off + h_crop) {
                            let i = 4 * (w * y + x_off);
                            self.buffer_cropped.extend(&data[i..i + 4 * w_crop]);
                        }
                        self.width = w_crop;
                        self.height = h_crop;
                    } else {
                        self.width = w;
                        self.height = h;
                    }
                    self.is_cropped = crop.is_some();
                    self.buffer = Some(buf);
                }
            }
        } else {
            return Ok(PixelProvider::NONE);
        }
        if self.buffer.is_none() && self.dma_buffer.is_empty() {
            return Err(Box::new(GStreamerError("No buffer available!".into())));
        }
        let buf: &[u8] = if self.is_cropped {
            self.buffer_cropped.as_slice()
        } else if !self.dma_buffer.is_empty() && self.buffer.is_none() {
            self.dma_buffer.as_slice()
        } else {
            self.buffer
                .as_ref()
                .ok_or("Failed to get buffer as ref")?
                .as_slice()
        };
        match self.pix_fmt.as_str() {
            "BGRx" => Ok(PixelProvider::BGR0(self.width, self.height, buf)),
            "RGBx" => Ok(PixelProvider::RGB0(self.width, self.height, buf)),
            _ => Err(Box::new(GStreamerError(format!(
                "Unreachable! Unknown pix_fmt, {}",
                &self.pix_fmt
            )))),
        }
    }
}

impl Drop for PipeWireRecorder {
    fn drop(&mut self) {
        if let Err(err) = self.pipeline.set_state(gst::State::Null) {
            warn!("Failed to stop GStreamer pipeline: {}.", err);
        }
        // Wait for state change to complete to avoid races during PipeWire teardown.
        let _ = self.pipeline.get_state(gst::ClockTime::from_mseconds(2000));
    }
}

// The portal handshake is four sequential requests whose outcomes arrive as asynchronous
// `Response` signals, so where and why it failed is known only inside the signal handler.
// Recording it here, instead of collapsing every outcome into one `failed` flag, is what lets
// the app side name the real cause rather than guess it from the error text.
#[derive(Clone, Copy)]
enum PortalStage {
    CreateSession = 1,
    SelectDevices = 2,
    SelectSources = 3,
    Start = 4,
    OpenPipeWireRemote = 5,
}

impl PortalStage {
    fn as_str(&self) -> &'static str {
        match self {
            Self::CreateSession => "create-session",
            Self::SelectDevices => "select-devices",
            Self::SelectSources => "select-sources",
            Self::Start => "start",
            Self::OpenPipeWireRemote => "open-pipewire-remote",
        }
    }

    fn from_u8(v: u8) -> Self {
        match v {
            2 => Self::SelectDevices,
            3 => Self::SelectSources,
            4 => Self::Start,
            5 => Self::OpenPipeWireRemote,
            _ => Self::CreateSession,
        }
    }
}

// `wl-stage:<stage>:<kind>:<detail>`, parsed by `map_err_scrap` on the app side. The detail
// reaches the user through a `{}` placeholder in a translated string, so it must not bring
// braces, control characters or unbounded length of its own.
const STAGE_TAG: &str = "wl-stage:";

fn stage_err(stage: &str, kind: &str, detail: &str) -> String {
    let detail: String = detail
        .chars()
        .map(|c| if c.is_control() { ' ' } else { c })
        .filter(|c| *c != '{' && *c != '}')
        .take(200)
        .collect();
    format!("{}{}:{}:{}", STAGE_TAG, stage, kind, detail.trim())
}

// The name alone is usually the generic `org.freedesktop.DBus.Error.Failed`; the message is
// where a backend says what it objected to. This ends up in the log, so carry both.
fn dbus_stage_err(stage: &str, err: &dbus::Error) -> String {
    let detail = match (err.name(), err.message()) {
        (Some(name), Some(message)) if !name.is_empty() && !message.is_empty() => {
            format!("{}: {}", name, message)
        }
        (Some(name), _) if !name.is_empty() => name.to_owned(),
        (_, message) => message.unwrap_or_default().to_owned(),
    };
    let kind = match err.name().unwrap_or_default() {
        "org.freedesktop.DBus.Error.UnknownMethod"
        | "org.freedesktop.DBus.Error.UnknownInterface" => "unsupported",
        _ => "dbus",
    };
    stage_err(stage, kind, &detail)
}

#[derive(Clone)]
struct PortalTrace {
    failed: Arc<AtomicBool>,
    reason: Arc<Mutex<Option<String>>>,
    // The stage whose `Response` we are still waiting for, so the polling loop can tell a
    // non-interactive step apart from the one that waits for a human.
    waiting_for: Arc<AtomicU8>,
}

impl PortalTrace {
    fn new() -> Self {
        Self {
            failed: Arc::new(AtomicBool::new(false)),
            reason: Arc::new(Mutex::new(None)),
            waiting_for: Arc::new(AtomicU8::new(PortalStage::CreateSession as u8)),
        }
    }

    fn fail(&self, stage: PortalStage, kind: &str, detail: &str) {
        self.record(stage_err(stage.as_str(), kind, detail));
        self.failed.store(true, Ordering::SeqCst);
    }

    // The first failure is the cause; whatever follows it is a consequence.
    fn record(&self, tag: String) {
        if let Ok(mut reason) = self.reason.lock() {
            if reason.is_none() {
                *reason = Some(tag);
            }
        }
    }

    fn waiting(&self, stage: PortalStage) {
        self.waiting_for.store(stage as u8, Ordering::SeqCst);
    }

    fn waiting_stage(&self) -> PortalStage {
        PortalStage::from_u8(self.waiting_for.load(Ordering::SeqCst))
    }

    fn take_reason(&self) -> Option<String> {
        self.reason.lock().ok().and_then(|mut r| r.take())
    }
}

fn handle_response<F>(
    conn: &SyncConnection,
    path: dbus::Path<'static>,
    mut f: F,
    trace: PortalTrace,
    stage: PortalStage,
) -> Result<dbus::channel::Token, dbus::Error>
where
    F: FnMut(
            OrgFreedesktopPortalRequestResponse,
            &SyncConnection,
            &Message,
        ) -> Result<(), Box<dyn Error>>
        + Send
        + Sync
        + 'static,
{
    let mut m = MatchRule::new();
    m.path = Some(path);
    m.msg_type = Some(MessageType::Signal);
    m.sender = Some("org.freedesktop.portal.Desktop".into());
    m.interface = Some("org.freedesktop.portal.Request".into());
    conn.add_match(m, move |r: OrgFreedesktopPortalRequestResponse, c, m| {
        debug!("Response from DBus: response: {:?}, message: {:?}", r, m);
        match r.response {
            0 => {}
            1 => {
                warn!("DBus response: User cancelled interaction.");
                trace.fail(stage, "declined", "");
                return true;
            }
            2 => {
                warn!("DBus response: User interaction ended in some other way.");
                trace.fail(stage, "ended", "");
                return true;
            }
            c => {
                warn!("DBus response: Unknown error, code: {}.", c);
                trace.fail(stage, "portal-error", &c.to_string());
                return true;
            }
        }
        if let Err(err) = f(r, c, m) {
            let text = err.to_string();
            warn!("Error requesting screen capture via dbus: {}", text);
            if text.starts_with(STAGE_TAG) {
                trace.record(text);
                trace.failed.store(true, Ordering::SeqCst);
            } else {
                trace.fail(trace.waiting_stage(), "internal", &text);
            }
        }
        true
    })
}

// The request object path a portal method call will use, derived from our unique
// bus name and the `handle_token` we pass in the call arguments. Knowing it up
// front lets us subscribe to the `Response` signal *before* making the call.
// https://flatpak.github.io/xdg-desktop-portal/docs/doc-org.freedesktop.portal.Request.html
fn get_request_path(
    conn: &SyncConnection,
    handle_token: &str,
) -> Result<dbus::Path<'static>, dbus::Error> {
    let sender = conn.unique_name().trim_start_matches(':').replace('.', "_");
    dbus::Path::new(format!(
        "/org/freedesktop/portal/desktop/request/{}/{}",
        sender, handle_token
    ))
    .map_err(|_| dbus::Error::new_failed("Failed to construct portal request path"))
}

pub fn get_portal(conn: &SyncConnection) -> Proxy<&SyncConnection> {
    conn.with_proxy(
        "org.freedesktop.portal.Desktop",
        "/org/freedesktop/portal/desktop",
        Duration::from_millis(1000),
    )
}

fn streams_from_response(response: OrgFreedesktopPortalRequestResponse) -> Vec<PwStreamInfo> {
    debug!("Portal streams response: {:?}", response.results);
    (move || {
        Some(
            response
                .results
                .get("streams")?
                .as_iter()?
                .next()?
                .as_iter()?
                .filter_map(|stream| {
                    let mut itr = stream.as_iter()?;
                    let path = itr.next()?.as_u64()?;
                    let (keys, values): (Vec<(usize, &dyn RefArg)>, Vec<(usize, &dyn RefArg)>) =
                        itr.next()?
                            .as_iter()?
                            .enumerate()
                            .partition(|(i, _)| i % 2 == 0);
                    let attributes = keys
                        .iter()
                        .filter_map(|(_, key)| Some(key.as_str()?.to_owned()))
                        .zip(
                            values
                                .iter()
                                .map(|(_, arg)| *arg)
                                .collect::<Vec<&dyn RefArg>>(),
                        )
                        .collect::<HashMap<String, &dyn RefArg>>();
                    let mut info = PwStreamInfo {
                        path,
                        source_type: attributes
                            .get("source_type")
                            .map_or(Some(0), |v| v.as_u64())?,
                        position: (0, 0),
                        size: (0, 0),
                    };
                    let v = attributes
                        .get("size")?
                        .as_iter()?
                        .filter_map(|v| {
                            Some(
                                v.as_iter()?
                                    .map(|x| x.as_i64().unwrap_or(0))
                                    .collect::<Vec<i64>>(),
                            )
                        })
                        .next();
                    if let Some(v) = v {
                        if v.len() == 2 {
                            info.size.0 = v[0] as _;
                            info.size.1 = v[1] as _;
                        }
                    }
                    if let Some(pos) = attributes.get("position") {
                        let v = pos
                            .as_iter()?
                            .filter_map(|v| {
                                Some(
                                    v.as_iter()?
                                        .map(|x| x.as_i64().unwrap_or(0))
                                        .collect::<Vec<i64>>(),
                                )
                            })
                            .next();
                        if let Some(v) = v {
                            if v.len() == 2 {
                                info.position.0 = v[0] as _;
                                info.position.1 = v[1] as _;
                                HAS_POSITION_ATTR.store(true, Ordering::SeqCst);
                            }
                        }
                    }
                    Some(info)
                })
                .collect::<Vec<PwStreamInfo>>(),
        )
    })()
    .unwrap_or_default()
}

static mut INIT: bool = false;
const RESTORE_TOKEN: &str = "restore_token";
const RESTORE_TOKEN_CONF_KEY: &str = "wayland-restore-token";
const PIPEWIRE_DISPLAY_OFFSET_CONF_KEY: &str = "wayland-pipewire-display-offset";

pub fn get_available_cursor_modes() -> Result<u32, dbus::Error> {
    let conn = SyncConnection::new_session()?;
    let portal = get_portal(&conn);
    portal.available_cursor_modes()
}

pub fn try_mutter_screencast() -> ResultType<(
    SyncConnection,
    Option<OwnedFd>,
    Vec<PwStreamInfo>,
    dbus::Path<'static>,
    bool,
)> {
    unsafe {
        if !INIT {
            gstreamer::init()?;
            INIT = true;
        }
    }
    let conn = SyncConnection::new_session()
        .map_err(|e| anyhow!("Failed to connect to session bus: {}", e))?;

    let proxy = conn.with_proxy(
        "org.gnome.Mutter.ScreenCast",
        "/org/gnome/Mutter/ScreenCast",
        Duration::from_millis(1000),
    );

    let (session_path,): (dbus::Path<'static>,) = proxy
        .method_call(
            "org.gnome.Mutter.ScreenCast",
            "CreateSession",
            (HashMap::<String, Variant<Box<dyn RefArg>>>::new(),),
        )
        .map_err(|e| anyhow!("CreateSession on Mutter failed: {}", e))?;

    debug!("[mutter] Created Mutter ScreenCast session: {}", session_path);

    let session_proxy = conn.with_proxy(
        "org.gnome.Mutter.ScreenCast",
        &session_path,
        Duration::from_millis(2000),
    );

    let wayland_displays = super::display::get_displays();
    let mut connectors = Vec::new();
    for d in wayland_displays.displays.iter() {
        if !d.name.is_empty() {
            connectors.push(d.name.clone());
        }
    }
    if connectors.is_empty() {
        connectors.push("eDP-1".to_string());
    }

    let node_id_arc: Arc<Mutex<Option<u32>>> = Arc::new(Mutex::new(None));
    let node_id_res = node_id_arc.clone();

    let mut m = MatchRule::new();
    m.msg_type = Some(MessageType::Signal);
    m.interface = Some("org.gnome.Mutter.ScreenCast.Stream".into());
    m.member = Some("PipeWireStreamAdded".into());
    conn.add_match(m, move |(node_id,): (u32,), _: &SyncConnection, _: &Message| {
        debug!("[mutter] Received PipeWireStreamAdded signal: node_id={}", node_id);
        *node_id_res.lock().unwrap() = Some(node_id);
        true
    })
    .map_err(|e| anyhow!("Failed to add match for PipeWireStreamAdded: {}", e))?;

    for conn_name in &connectors {
        let (stream_path,): (dbus::Path<'static>,) = session_proxy
            .method_call(
                "org.gnome.Mutter.ScreenCast.Session",
                "RecordMonitor",
                (conn_name, HashMap::<String, Variant<Box<dyn RefArg>>>::new()),
            )
            .map_err(|e| anyhow!("RecordMonitor for {} failed: {}", conn_name, e))?;
        debug!("[mutter] Recorded monitor {} -> stream: {}", conn_name, stream_path);
    }

    let (): () = session_proxy
        .method_call(
            "org.gnome.Mutter.ScreenCast.Session",
            "Start",
            (),
        )
        .map_err(|e| anyhow!("Start session failed: {}", e))?;

    for _ in 0..30 {
        conn.process(Duration::from_millis(100))
            .map_err(|e| anyhow!("D-Bus process error: {}", e))?;
        if node_id_arc.lock().unwrap().is_some() {
            break;
        }
    }

    let node_id = node_id_arc
        .lock()
        .unwrap()
        .ok_or_else(|| anyhow!("Timed out waiting for PipeWireStreamAdded from Mutter"))?;

    let primary_display = wayland_displays.displays.first();
    let size = primary_display
        .map(|d| {
            if d.transform == 90 || d.transform == 270 {
                (d.height as usize, d.width as usize)
            } else {
                (d.width as usize, d.height as usize)
            }
        })
        .unwrap_or((3840, 2160));

    let streams = vec![PwStreamInfo {
        path: node_id as u64,
        size,
        position: (0, 0),
        source_type: 1,
    }];

    Ok((conn, None, streams, session_path, false))
}

// mostly inspired by https://gitlab.gnome.org/-/snippets/39
pub fn request_remote_desktop(
    capture_cursor: bool,
) -> ResultType<(
    SyncConnection,
    OwnedFd,
    Vec<PwStreamInfo>,
    dbus::Path<'static>,
    bool,
)> {
    unsafe {
        if !INIT {
            gstreamer::init()?;
            INIT = true;
        }
    }
    let conn =
        SyncConnection::new_session().map_err(|e| anyhow!(dbus_stage_err("session-bus", &e)))?;
    let portal = get_portal(&conn);
    let mut args: PropMap = HashMap::new();
    let fd: Arc<Mutex<Option<OwnedFd>>> = Arc::new(Mutex::new(None));
    let fd_res = fd.clone();
    let streams: Arc<Mutex<Vec<PwStreamInfo>>> = Arc::new(Mutex::new(Vec::new()));
    let streams_res = streams.clone();
    let trace = PortalTrace::new();
    let trace_res = trace.clone();
    let session: Arc<Mutex<Option<dbus::Path>>> = Arc::new(Mutex::new(None));
    let session_res = session.clone();
    let create_session_handle_token = "u1";
    args.insert(
        "session_handle_token".to_string(),
        Variant(Box::new(create_session_handle_token.to_string())),
    );
    args.insert(
        "handle_token".to_string(),
        Variant(Box::new(create_session_handle_token.to_string())),
    );

    let mut is_support_restore_token = false;
    if let Ok(version) = screencast_portal::version(&portal) {
        if version >= 4 {
            is_support_restore_token = true;
        }
    }

    // The following code may be improved.
    // https://flatpak.github.io/xdg-desktop-portal/#:~:text=To%20avoid%20a%20race%20condition
    // To avoid a race condition
    // between the caller subscribing to the signal after receiving the reply for the method call and the signal getting emitted,
    // a convention for Request object paths has been established that allows
    // the caller to subscribe to the signal before making the method call.
    handle_response(
        &conn,
        get_request_path(&conn, create_session_handle_token)
            .map_err(|e| anyhow!(dbus_stage_err("create-session", &e)))?,
        on_create_session_response(
            fd.clone(),
            streams.clone(),
            session.clone(),
            trace.clone(),
            is_support_restore_token,
            capture_cursor,
        ),
        trace.clone(),
        PortalStage::CreateSession,
    )
    .map_err(|e| anyhow!(dbus_stage_err("create-session", &e)))?;
    let use_rdp = can_use_remote_desktop_portal(&portal);
    if !use_rdp {
        let _ = screencast_portal::create_session(&portal, args)
            .map_err(|e| anyhow!(dbus_stage_err("create-session", &e)))?;
    } else {
        let _ = remote_desktop_portal::create_session(&portal, args)
            .map_err(|e| anyhow!(dbus_stage_err("create-session", &e)))?;
    }

    // wait 3 minutes for user interaction
    for _ in 0..1800 {
        conn.process(Duration::from_millis(100))
            .map_err(|e| anyhow!(dbus_stage_err(trace_res.waiting_stage().as_str(), &e)))?;
        // Once we got a file descriptor we are done!
        if fd_res.lock().unwrap().is_some() {
            break;
        }

        if trace_res.failed.load(Ordering::SeqCst) {
            break;
        }
    }
    let fd_res = fd_res.lock().unwrap();
    let streams_res = streams_res.lock().unwrap();
    let session_res = session_res.lock().unwrap();
    let have_fd = fd_res.is_some();

    if let Some(fd_res) = fd_res.clone() {
        if let Some(session) = session_res.clone() {
            if !streams_res.is_empty() {
                return Ok((
                    conn,
                    fd_res,
                    streams_res.clone(),
                    session,
                    is_support_restore_token,
                ));
            }
        }
    }
    bail!(trace_res.take_reason().unwrap_or_else(|| {
        if have_fd {
            stage_err("streams", "empty", "")
        } else {
            stage_err(trace_res.waiting_stage().as_str(), "no-response", "")
        }
    }))
}

fn on_create_session_response(
    fd: Arc<Mutex<Option<OwnedFd>>>,
    streams: Arc<Mutex<Vec<PwStreamInfo>>>,
    session: Arc<Mutex<Option<dbus::Path<'static>>>>,
    trace: PortalTrace,
    is_support_restore_token: bool,
    capture_cursor: bool,
) -> impl Fn(
    OrgFreedesktopPortalRequestResponse,
    &SyncConnection,
    &dbus::Message,
) -> Result<(), Box<dyn Error>> {
    move |r: OrgFreedesktopPortalRequestResponse, c, _| {
        let ses: dbus::Path = r
            .results
            .get("session_handle")
            .ok_or_else(|| {
                DBusError(format!(
                    "Failed to obtain session_handle from response: {:?}",
                    r
                ))
            })?
            .as_str()
            .ok_or_else(|| DBusError("Failed to convert session_handle to string.".into()))?
            .to_string()
            .into();

        let mut session = match session.lock() {
            Ok(session) => session,
            Err(_) => return Err(Box::new(DBusError("Failed to lock session.".into()))),
        };
        session.replace(ses.clone());

        let portal = get_portal(c);
        let mut args: PropMap = HashMap::new();
        let use_rdp = can_use_remote_desktop_portal(&portal);
        if !use_rdp {
            if is_support_restore_token {
                let restore_token = config::LocalConfig::get_option(RESTORE_TOKEN_CONF_KEY);
                if !restore_token.is_empty() {
                    args.insert(RESTORE_TOKEN.to_string(), Variant(Box::new(restore_token)));
                }
                // persist_mode may be configured by the user.
                args.insert("persist_mode".to_string(), Variant(Box::new(2u32)));
            }
            let select_sources_handle_token = "u3";
            args.insert(
                "handle_token".to_string(),
                Variant(Box::new(select_sources_handle_token.to_string())),
            );
            // https://flatpak.github.io/xdg-desktop-portal/docs/doc-org.freedesktop.portal.ScreenCast.html
            args.insert("multiple".into(), Variant(Box::new(true)));
            args.insert("types".into(), Variant(Box::new(1u32))); //| 2u32)));

            if capture_cursor {
                get_available_cursor_modes().ok().map(|modes| {
                    if modes & 0x2 != 0 {
                        args.insert("cursor_mode".to_string(), Variant(Box::new(2u32)));
                    }
                });
            }

            trace.waiting(PortalStage::SelectSources);
            handle_response(
                c,
                get_request_path(c, select_sources_handle_token)?,
                on_select_sources_response(
                    fd.clone(),
                    streams.clone(),
                    trace.clone(),
                    ses.clone(),
                    is_support_restore_token,
                ),
                trace.clone(),
                PortalStage::SelectSources,
            )?;
            let _ = portal
                .select_sources(ses.clone(), args)
                .map_err(|e| DBusError(dbus_stage_err("select-sources", &e)))?;
        } else {
            // TODO: support persist_mode for remote_desktop_portal
            // https://flatpak.github.io/xdg-desktop-portal/docs/doc-org.freedesktop.portal.RemoteDesktop.html

            let select_devices_handle_token = "u2";
            args.insert(
                "handle_token".to_string(),
                Variant(Box::new(select_devices_handle_token.to_string())),
            );
            args.insert("types".to_string(), Variant(Box::new(7u32)));

            trace.waiting(PortalStage::SelectDevices);
            handle_response(
                c,
                get_request_path(c, select_devices_handle_token)?,
                on_select_devices_response(
                    fd.clone(),
                    streams.clone(),
                    trace.clone(),
                    ses.clone(),
                    is_support_restore_token,
                ),
                trace.clone(),
                PortalStage::SelectDevices,
            )?;
            let _ = portal
                .select_devices(ses.clone(), args)
                .map_err(|e| DBusError(dbus_stage_err("select-devices", &e)))?;
        }

        Ok(())
    }
}

fn on_select_devices_response(
    fd: Arc<Mutex<Option<OwnedFd>>>,
    streams: Arc<Mutex<Vec<PwStreamInfo>>>,
    trace: PortalTrace,
    session: dbus::Path<'static>,
    is_support_restore_token: bool,
) -> impl Fn(
    OrgFreedesktopPortalRequestResponse,
    &SyncConnection,
    &dbus::Message,
) -> Result<(), Box<dyn Error>> {
    move |_: OrgFreedesktopPortalRequestResponse, c, _| {
        let portal = get_portal(c);
        let mut args: PropMap = HashMap::new();
        let select_sources_handle_token = "u3";
        args.insert(
            "handle_token".to_string(),
            Variant(Box::new(select_sources_handle_token.to_string())),
        );
        // https://flatpak.github.io/xdg-desktop-portal/docs/doc-org.freedesktop.portal.ScreenCast.html
        if !can_use_remote_desktop_portal_cached() {
            args.insert("multiple".into(), Variant(Box::new(true)));
        }
        args.insert("types".into(), Variant(Box::new(1u32))); //| 2u32)));

        let session = session.clone();
        trace.waiting(PortalStage::SelectSources);
        handle_response(
            c,
            get_request_path(c, select_sources_handle_token)?,
            on_select_sources_response(
                fd.clone(),
                streams.clone(),
                trace.clone(),
                session.clone(),
                is_support_restore_token,
            ),
            trace.clone(),
            PortalStage::SelectSources,
        )?;
        let _ = portal
            .select_sources(session.clone(), args)
            .map_err(|e| DBusError(dbus_stage_err("select-sources", &e)))?;

        Ok(())
    }
}

fn on_select_sources_response(
    fd: Arc<Mutex<Option<OwnedFd>>>,
    streams: Arc<Mutex<Vec<PwStreamInfo>>>,
    trace: PortalTrace,
    session: dbus::Path<'static>,
    is_support_restore_token: bool,
) -> impl Fn(
    OrgFreedesktopPortalRequestResponse,
    &SyncConnection,
    &dbus::Message,
) -> Result<(), Box<dyn Error>> {
    move |_: OrgFreedesktopPortalRequestResponse, c, _| {
        let portal = get_portal(c);
        let mut args: PropMap = HashMap::new();
        let start_handle_token = "u4";
        args.insert(
            "handle_token".to_string(),
            Variant(Box::new(start_handle_token.to_string())),
        );
        trace.waiting(PortalStage::Start);
        handle_response(
            c,
            get_request_path(c, start_handle_token)?,
            on_start_response(
                fd.clone(),
                streams.clone(),
                session.clone(),
                trace.clone(),
                is_support_restore_token,
            ),
            trace.clone(),
            PortalStage::Start,
        )?;
        let use_rdp = can_use_remote_desktop_portal(&portal);
        if !use_rdp {
            let _ = screencast_portal::start(&portal, session.clone(), "", args)
                .map_err(|e| DBusError(dbus_stage_err("start", &e)))?;
        } else {
            let _ = remote_desktop_portal::start(&portal, session.clone(), "", args)
                .map_err(|e| DBusError(dbus_stage_err("start", &e)))?;
        }

        Ok(())
    }
}

fn on_start_response(
    fd: Arc<Mutex<Option<OwnedFd>>>,
    streams: Arc<Mutex<Vec<PwStreamInfo>>>,
    session: dbus::Path<'static>,
    trace: PortalTrace,
    is_support_restore_token: bool,
) -> impl Fn(
    OrgFreedesktopPortalRequestResponse,
    &SyncConnection,
    &dbus::Message,
) -> Result<(), Box<dyn Error>> {
    move |r: OrgFreedesktopPortalRequestResponse, c, _| {
        let portal = get_portal(c);
        let use_rdp = can_use_remote_desktop_portal(&portal);
        if !use_rdp {
            if is_support_restore_token {
                if let Some(restore_token) = r.results.get(RESTORE_TOKEN) {
                    if let Some(restore_token) = restore_token.as_str() {
                        config::LocalConfig::set_option(
                            RESTORE_TOKEN_CONF_KEY.to_owned(),
                            restore_token.to_owned(),
                        );
                    }
                }
            }
        }

        streams
            .clone()
            .lock()
            .unwrap()
            .append(&mut streams_from_response(r));
        // Past this point the user has granted the request; anything that fails now is the
        // hand-over of the PipeWire fd, which is a different thing to go looking at.
        trace.waiting(PortalStage::OpenPipeWireRemote);
        fd.clone().lock().unwrap().replace(
            portal
                .open_pipe_wire_remote(session.clone(), HashMap::new())
                .map_err(|e| DBusError(dbus_stage_err("open-pipewire-remote", &e)))?,
        );

        Ok(())
    }
}

pub fn get_capturables() -> Result<Vec<PipeWireCapturable>, Box<dyn Error>> {
    let mut rdp_connection = match RDP_SESSION_INFO.lock() {
        Ok(conn) => conn,
        Err(err) => return Err(Box::new(err)),
    };

    if rdp_connection.is_none() {
        let (conn, fd, streams, session, is_support_restore_token) = match try_mutter_screencast() {
            Ok(tuple) => {
                info!("[pipewire] Using direct Mutter ScreenCast (promptless)");
                tuple
            }
            Err(e) => {
                debug!("[pipewire] Mutter ScreenCast unavailable ({}), falling back to portal", e);
                let (conn, fd, streams, session, is_support_restore_token) = request_remote_desktop(false)?;
                (conn, Some(fd), streams, session, is_support_restore_token)
            }
        };
        let conn = Arc::new(conn);

        let rdp_info = RdpSessionInfo {
            conn,
            streams,
            fd,
            session,
            is_support_restore_token,
            resolution: Arc::new(Mutex::new(None)),
        };
        *rdp_connection = Some(rdp_info);
    }

    let rdp_info = match rdp_connection.as_mut() {
        Some(res) => res,
        None => {
            return Err(Box::new(DBusError("RDP response is None.".into())));
        }
    };

    Ok(rdp_info
        .streams
        .iter()
        .map(|s| {
            PipeWireCapturable::new(
                rdp_info.conn.clone(),
                rdp_info.fd.clone(),
                rdp_info.resolution.clone(),
                s,
            )
        })
        .collect())
}

// If `is_server_running()` is true, then `screencast_portal::start` is called.
// Otherwise, `remote_desktop_portal::start` is called.
//
// If `is_server_running()` is true, `--service` process is running,
// then we can use uinput as the input method.
// Otherwise, we have to use remote_desktop_portal's input method.
//
// `screencast_portal` supports restore_token and persist_mode if the version is greater than or equal to 4.
// `remote_desktop_portal` does not support restore_token and persist_mode.
pub(crate) fn is_server_running() -> bool {
    let v = IS_SERVER_RUNNING.load(Ordering::SeqCst);
    if v > 0 {
        return v == 1;
    }

    let app_name = config::APP_NAME.read().unwrap().clone().to_lowercase();
    let output = match Command::new(CMD_SH.as_str())
        .arg("-c")
        .arg(&format!("ps aux | grep {}", app_name))
        .output()
    {
        Ok(output) => output,
        Err(_) => {
            return false;
        }
    };

    let output_str = String::from_utf8_lossy(&output.stdout);
    let is_running = output_str.contains(&format!("{} --server", app_name));
    IS_SERVER_RUNNING.store(if is_running { 1 } else { 2 }, Ordering::SeqCst);
    is_running
}

// The logical size reported by portal may be different from the size reported by `get_displays()`.
// So we need to use the workaround here.
// 1. openSUSE, KDE Plasma
// 2. Kubuntu 24.04 TLS, after running `sudo apt install plasma-workspace-wayland`
// Maybe it's a bug, and we can remove this workaround in the future.
pub fn try_fix_logical_size(shared_displays: &mut Vec<crate::Display>) {
    if !is_server_running() {
        return;
    }

    let wayland_displays = get_displays();
    if wayland_displays.displays.is_empty() {
        return;
    }

    for sd in shared_displays.iter_mut() {
        if let crate::Display::WAYLAND(d) = sd {
            let capturable = &mut d.0;
            for wd in wayland_displays.displays.iter() {
                if capturable.position.0 == wd.x && capturable.position.1 == wd.y {
                    if let Some(logical_size) = wd.logical_size {
                        if capturable.physical_size.0 != wd.width as usize
                            || capturable.physical_size.1 != wd.height as usize
                        {
                            // If "Full Workspace" is selected in the portal dialog,
                            // the physical size reported by portal may not match the display info.
                            debug!(
                            "Physical size of capturable ({:?}) does not match display info: ({:?}) - ({:?}). Skipping logical size fix.",
                            capturable.position,
                            capturable.physical_size,
                            (wd.width as usize, wd.height as usize)
                        );
                            break;
                        }

                        if capturable.logical_size.0 != logical_size.0 as usize
                            || capturable.logical_size.1 != logical_size.1 as usize
                        {
                            warn!(
                            "Fixing logical size of capturable from {:?} to {:?} based on display info {:?}.",
                            capturable.logical_size,
                            logical_size,
                            wd
                        );
                            capturable.logical_size =
                                (logical_size.0 as usize, logical_size.1 as usize);
                        }
                    }
                    break;
                }
            }
        }
    }
}

pub fn fill_displays(
    mouse_move_to: impl Fn(i32, i32),
    get_cursor_pos: fn() -> Option<(i32, i32)>,
    shared_displays: &mut Vec<crate::Display>,
) -> ResultType<()> {
    if !is_server_running() {
        return Ok(());
    }

    let mut rdp_connection = RDP_SESSION_INFO.lock().unwrap();
    let rdp_info = match rdp_connection.as_mut() {
        Some(res) => res,
        None => {
            // Unreachable
            bail!("RDP session info is None when filling display positions.");
        }
    };

    let all_displays = get_displays();
    if !HAS_POSITION_ATTR.load(Ordering::SeqCst) {
        if all_displays.displays.len() > 1 {
            debug!("Multiple Wayland displays detected, adjusting stream positions accordingly.");
            try_fill_positions(
                mouse_move_to,
                get_cursor_pos,
                &all_displays,
                shared_displays,
                &mut rdp_info.streams,
            )?;
        }
        HAS_POSITION_ATTR.store(true, Ordering::SeqCst);
    }

    if all_displays.displays.len() > 1 {
        sort_streams(&all_displays, shared_displays, &mut rdp_info.streams);
    }

    shared_displays.iter_mut().next().map(|d| {
        if let crate::Display::WAYLAND(d) = d {
            d.0.primary = true;
        }
    });

    Ok(())
}

fn try_fill_positions(
    mouse_move_to: impl Fn(i32, i32),
    get_cursor_pos: fn() -> Option<(i32, i32)>,
    displays: &Arc<Displays>,
    shared_displays: &mut Vec<crate::Display>,
    streams: &mut Vec<PwStreamInfo>,
) -> ResultType<()> {
    let pipewire_display_offset = config::LocalConfig::get_option(PIPEWIRE_DISPLAY_OFFSET_CONF_KEY);
    if !pipewire_display_offset.is_empty() {
        if try_fill_positions_from_cache(
            pipewire_display_offset,
            displays,
            shared_displays,
            streams,
        ) {
            return Ok(());
        }
        config::LocalConfig::set_option(PIPEWIRE_DISPLAY_OFFSET_CONF_KEY.to_owned(), "".to_owned());
    }

    let mut multi_matched_indices = Vec::new();
    for (i, sd) in shared_displays.iter_mut().enumerate() {
        if let crate::Display::WAYLAND(d) = sd {
            let capturable = &mut d.0;
            let mut match_count = 0;
            for wd in displays.displays.iter() {
                if capturable.physical_size.0 == wd.width as usize
                    && capturable.physical_size.1 == wd.height as usize
                {
                    capturable.position = (wd.x, wd.y);
                    if let Some(pw_stream) = streams.get_mut(i) {
                        pw_stream.position = (wd.x, wd.y);
                    }
                    match_count += 1;
                }
            }
            if match_count == 0 {
                warn!(
                    "No matching display found for capturable with size {:?}.",
                    capturable.physical_size
                );
            } else if match_count > 1 {
                multi_matched_indices.push(i);
            }
        }
    }

    if !multi_matched_indices.is_empty() {
        fill_multi_matched_positions(
            mouse_move_to,
            get_cursor_pos,
            displays,
            shared_displays,
            streams,
            multi_matched_indices,
        )?;
    }

    save_positions_to_cache(displays, shared_displays);
    Ok(())
}

fn try_fill_positions_from_cache(
    cache_str: String,
    displays: &Arc<Displays>,
    shared_displays: &mut Vec<crate::Display>,
    streams: &mut Vec<PwStreamInfo>,
) -> bool {
    let Ok(cache) = serde_json::from_str::<PipewireDisplayOffsetCache>(&cache_str) else {
        return false;
    };

    if cache.offsets.len() != shared_displays.len() {
        return false;
    }

    let display_key = PipewireDisplayOffsetCache::displays_to_key(displays);
    if cache.display_key != display_key {
        return false;
    }

    let restore_token = config::LocalConfig::get_option(RESTORE_TOKEN_CONF_KEY);
    if cache.restore_token != restore_token {
        return false;
    }

    for (i, sd) in shared_displays.iter_mut().enumerate() {
        if let crate::Display::WAYLAND(d) = sd {
            let capturable = &mut d.0;
            if let Some((x_off, y_off)) = cache.offsets.get(i) {
                capturable.position = (*x_off, *y_off);
                if let Some(pw_stream) = streams.get_mut(i) {
                    pw_stream.position = (*x_off, *y_off);
                }
            }
        }
    }
    true
}

fn save_positions_to_cache(displays: &Arc<Displays>, shared_displays: &Vec<crate::Display>) {
    let restore_token = config::LocalConfig::get_option(RESTORE_TOKEN_CONF_KEY);
    if restore_token.is_empty() {
        return;
    }

    let mut offsets = Vec::new();
    for sd in shared_displays.iter() {
        if let crate::Display::WAYLAND(d) = sd {
            let capturable = &d.0;
            offsets.push((capturable.position.0, capturable.position.1));
        }
    }

    let display_key = PipewireDisplayOffsetCache::displays_to_key(displays);
    let cache = PipewireDisplayOffsetCache {
        display_key,
        restore_token,
        offsets,
    };

    if let Ok(s) = serde_json::to_string(&cache) {
        config::LocalConfig::set_option(PIPEWIRE_DISPLAY_OFFSET_CONF_KEY.to_owned(), s);
    }
}

fn compare_left_up_corner(w: usize, d1: &[u8], d2: &[u8]) -> bool {
    if w == 0 {
        return false;
    }
    if d1.len() != d2.len() {
        return false;
    }
    let bpp = 4; // BGR0/RGB0
    let stride = w.saturating_mul(bpp);
    if stride == 0 || d1.len() < stride || d2.len() < stride {
        return false;
    }
    let h = d1.len() / stride;
    if h == 0 {
        return false;
    }

    let roi_w = std::cmp::min(36, w);
    let roi_h = std::cmp::min(36, h);
    let mut diff_px = 0usize;
    let total_px = roi_w * roi_h;
    // Minimum number of differing pixels required to consider images different.
    const MIN_DIFF_PIXELS: usize = 8;
    // Divisor for threshold calculation: allows up to 1/8 of ROI pixels to differ before returning true.
    const DIFF_THRESHOLD_DIVISOR: usize = 8;
    let threshold = std::cmp::max(MIN_DIFF_PIXELS, total_px / DIFF_THRESHOLD_DIVISOR);

    for y in 0..roi_h {
        let row_off = y * stride;
        for x in 0..roi_w {
            let i = row_off + x * bpp;
            let a = &d1[i..i + bpp];
            let b = &d2[i..i + bpp];
            if a != b {
                diff_px += 1;
                if diff_px >= threshold {
                    return true;
                }
            }
        }
    }
    false
}

fn fill_multi_matched_positions(
    mouse_move_to: impl Fn(i32, i32),
    get_cursor_pos: fn() -> Option<(i32, i32)>,
    displays: &Arc<Displays>,
    shared_displays: &mut Vec<crate::Display>,
    streams: &mut Vec<PwStreamInfo>,
    multi_matched_indices: Vec<usize>,
) -> ResultType<()> {
    debug!(
        "Multiple capturables ({:?}) match the same display size, attempting to disambiguate positions.",
    &multi_matched_indices);
    if multi_matched_indices.is_empty() {
        return Ok(());
    }

    let is_support_embeded_cursor = get_available_cursor_modes()
        .ok()
        .map(|modes| modes & 0x2 != 0)
        .unwrap_or(false);
    if is_support_embeded_cursor {
        fill_multi_matched_positions_cursor(
            mouse_move_to,
            get_cursor_pos,
            displays,
            shared_displays,
            streams,
            multi_matched_indices,
        )?;
    }

    Ok(())
}

fn mouse_move_to_(
    mouse_move_to: &impl Fn(i32, i32),
    get_cursor_pos: fn() -> Option<(i32, i32)>,
    x: i32,
    y: i32,
) {
    const MOVE_MOUSE_TIMEOUT: Duration = Duration::from_millis(150);
    let start = std::time::Instant::now();
    while start.elapsed() < MOVE_MOUSE_TIMEOUT {
        mouse_move_to(x, y);
        std::thread::sleep(Duration::from_millis(20));
        if let Some((x1, y1)) = get_cursor_pos() {
            if x1 == x && y1 == y {
                return;
            }
        }
    }
    warn!(
        "Failed to move mouse to ({}, {}) within timeout: {:?}.",
        x, y, &MOVE_MOUSE_TIMEOUT
    );
}

fn fill_multi_matched_positions_cursor(
    mouse_move_to: impl Fn(i32, i32),
    get_cursor_pos: fn() -> Option<(i32, i32)>,
    displays: &Arc<Displays>,
    shared_displays: &mut Vec<crate::Display>,
    streams: &mut Vec<PwStreamInfo>,
    multi_matched_indices: Vec<usize>,
) -> ResultType<()> {
    // This creates a new remote desktop session for cursor-based position detection.
    // The session is temporary, used only for disambiguation, and is dropped after detection completes.
    let (conn, fd, streams_with_cursor, _session, _is_support_restore_token) =
        request_remote_desktop(true)?;
    let conn = Arc::new(conn);

    let mut matched_indices = Vec::new();
    const CAPTURE_TIMEOUT_MS: u64 = 1_000;
    for idx in multi_matched_indices {
        match (
            shared_displays.get_mut(idx),
            streams.get_mut(idx),
            streams_with_cursor.get(idx),
        ) {
            (Some(crate::Display::WAYLAND(d)), Some(pw_stream), Some(pw_stream_with_cursor)) => {
                // Check if only one display matches the size
                let mut match_count = 0;
                for (i, wd) in displays.displays.iter().enumerate() {
                    if matched_indices.contains(&i) {
                        continue;
                    }
                    if d.0.physical_size.0 == wd.width as usize
                        && d.0.physical_size.1 == wd.height as usize
                    {
                        match_count += 1;
                    }
                }
                if match_count == 0 {
                    error!(
                        "No matching display found for capturable with size {:?}.",
                        d.0.physical_size
                    );
                    continue;
                }
                if match_count == 1 {
                    for (i, wd) in displays.displays.iter().enumerate() {
                        if matched_indices.contains(&i) {
                            continue;
                        }
                        if d.0.physical_size.0 == wd.width as usize
                            && d.0.physical_size.1 == wd.height as usize
                        {
                            d.0.position = (wd.x, wd.y);
                            pw_stream.position = (wd.x, wd.y);
                            matched_indices.push(i);
                            debug!(
                                "Disambiguated position for capturable with size {:?} to ({}, {}).",
                                d.0.physical_size, wd.x, wd.y
                            );
                            break;
                        }
                    }
                    continue;
                }

                // Move the mouse to a neutral position first,
                // to avoid interference from previous position.
                mouse_move_to_(&mouse_move_to, get_cursor_pos, 300, 300);

                let mut rec = PipeWireRecorder::new(PipeWireCapturable {
                    dbus_conn: conn.clone(),
                    fd: Some(fd.clone()),
                    path: pw_stream_with_cursor.path,
                    source_type: pw_stream_with_cursor.source_type,
                    primary: false,
                    position: pw_stream_with_cursor.position,
                    logical_size: pw_stream_with_cursor.size,
                    physical_size: (0, 0),
                })?;
                // Take first frame and copy owned buffer to avoid borrow across second capture
                let (is_bgr, w, first_buf): (bool, usize, Vec<u8>) =
                    match rec.capture(CAPTURE_TIMEOUT_MS) {
                        Ok(PixelProvider::BGR0(w, _, data1)) => (true, w, data1.to_vec()),
                        Ok(PixelProvider::RGB0(w, _, data1)) => (false, w, data1.to_vec()),
                        Ok(_) => {
                            error!("Unexpected pixel format on first capture.");
                            continue;
                        }
                        Err(e) => {
                            error!(
                                "Failed to capture screen for position disambiguation: {}",
                                e
                            );
                            continue;
                        }
                    };

                let matched_len = matched_indices.len();
                for (i, wd) in displays.displays.iter().enumerate() {
                    if matched_indices.contains(&i) {
                        continue;
                    }

                    if wd.width as usize == d.0.physical_size.0
                        && wd.height as usize == d.0.physical_size.1
                    {
                        mouse_move_to_(&mouse_move_to, get_cursor_pos, wd.x + 8, wd.y + 8);
                        rec.saved_raw_data.clear();
                        match rec.capture(CAPTURE_TIMEOUT_MS) {
                            Ok(PixelProvider::BGR0(_, _, data2)) if is_bgr => {
                                if compare_left_up_corner(w, &first_buf, data2) {
                                    d.0.position = (wd.x, wd.y);
                                    pw_stream.position = (wd.x, wd.y);
                                    matched_indices.push(i);
                                    debug!(
                                        "Disambiguated position for capturable with size {:?} to ({}, {}).",
                                        d.0.physical_size, wd.x, wd.y
                                    );
                                    break;
                                }
                            }
                            Ok(PixelProvider::RGB0(_, _, data2)) if !is_bgr => {
                                if compare_left_up_corner(w, &first_buf, data2) {
                                    d.0.position = (wd.x, wd.y);
                                    pw_stream.position = (wd.x, wd.y);
                                    matched_indices.push(i);
                                    debug!(
                                        "Disambiguated position for capturable with size {:?} to ({}, {}).",
                                        d.0.physical_size, wd.x, wd.y
                                    );
                                    break;
                                }
                            }
                            Ok(_) => {
                                // unreachable
                                error!("Pixel format changed between captures, cannot disambiguate position.");
                            }
                            Err(e) => {
                                error!(
                                    "Failed to capture screen for position disambiguation: {}",
                                    e
                                );
                            }
                        }
                    }
                }
                if matched_len == matched_indices.len() {
                    error!(
                        "Failed to disambiguate position for capturable with size {:?}.",
                        d.0.physical_size
                    );
                }
            }
            _ => {}
        }
    }

    Ok(())
}

fn sort_streams(
    displays: &Arc<Displays>,
    shared_displays: &mut Vec<crate::Display>,
    streams: &mut Vec<PwStreamInfo>,
) {
    if streams.is_empty() {
        // unreachable
        error!("No streams available to sort.");
        return;
    }

    // put the main display first, then the rest by the order of displays
    let mut display_order: Vec<(i32, i32)> = Vec::new();
    if let Some(d) = displays.displays.get(displays.primary) {
        display_order.push((d.x, d.y));
    }
    for (i, d) in displays.displays.iter().enumerate() {
        if i != displays.primary {
            display_order.push((d.x, d.y));
        }
    }

    let mut sorted_streams = Vec::new();
    let mut sorted_shared_displays = Vec::new();
    // Move matching items in order without cloning
    for (x, y) in display_order.into_iter() {
        for i in 0..streams.len() {
            if streams[i].position.0 == x && streams[i].position.1 == y {
                sorted_streams.push(streams.remove(i));
                // shared_displays.len() must be equal to streams.len()
                // But we still check the length to avoid panic
                if shared_displays.len() > i {
                    sorted_shared_displays.push(shared_displays.remove(i));
                }
                break;
            }
        }
    }
    *streams = sorted_streams;
    *shared_displays = sorted_shared_displays;
}

#[cfg(test)]
mod tests {
    use super::stage_err;

    #[test]
    fn stage_err_keeps_the_detail_safe_for_a_placeholder() {
        assert_eq!(
            stage_err("start", "declined", ""),
            "wl-stage:start:declined:"
        );
        // Braces of its own would break the placeholder lookup on the peer.
        assert_eq!(
            stage_err("create-session", "dbus", "org.freedesktop.{Error}"),
            "wl-stage:create-session:dbus:org.freedesktop.Error"
        );
        assert_eq!(
            stage_err("select-sources", "internal", "one\ntwo"),
            "wl-stage:select-sources:internal:one two"
        );
        assert_eq!(
            stage_err("start", "internal", &"x".repeat(300)),
            format!("wl-stage:start:internal:{}", "x".repeat(200))
        );
    }
}
