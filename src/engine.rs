//! Flutter engine glue: loads `libflutter_engine.so`, renders with the GPU
//! into AERA's shared frames, and turns bridge packets into Flutter input.

use std::cmp::Reverse;
use std::collections::BinaryHeap;
use std::ffi::{c_char, c_void, CStr, CString};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicPtr, Ordering};
use std::sync::{Condvar, Mutex};
use std::thread::ThreadId;
use std::time::Duration;

use serde_json::{json, Value};

use crate::bridge::{self, kind, Control, Frames, Packet};
use crate::ffi::*;
use crate::gl::{Gpu, Region};
use crate::vk::Vulkan;
use crate::system::{self, System};
use crate::text_input::{Effect, TextInput};

pub struct Config {
    pub engine_library: PathBuf,
    pub assets: PathBuf,
    pub icu_data: PathBuf,
    /// `app.so` for AOT (release/profile) engines; unused by debug engines.
    pub aot_library: PathBuf,
    pub locale: String,
    pub extra_engine_args: Vec<String>,
    pub renderer: Renderer,
}

/// How Flutter draws. `Gl` goes through Mesa's Zink to Vulkan; `Vulkan`
/// hands Flutter a Vulkan device directly (Skia, or Impeller with the
/// engine's `--enable-impeller` flag).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Renderer {
    Gl,
    Vulkan,
}

enum Backend {
    Gl(Gpu),
    Vulkan(Vulkan),
}

struct FrameState {
    /// Last sequence sent to AERA.
    sequence: u32,
    /// A frame is out and AERA has not acknowledged it yet.
    pending: bool,
    /// Flutter's request for the next vsync, answered once AERA has taken the
    /// previous frame and a frame period has passed since the last vsync.
    vsync_baton: Option<isize>,
    last_vsync: u64,
    /// What the previous frame changed. The slot the next frame goes into
    /// still holds the frame before that, so both frames' changes must be
    /// copied; None means copy everything.
    last_damage: Option<Region>,
}

/// AERA does not report its display rate; pace at 60 Hz at most.
const FRAME_PERIOD_NS: u64 = 16_666_667;

#[derive(PartialEq, Eq, PartialOrd, Ord)]
struct Queued {
    target: u64,
    order: u64,
    runner: usize,
    task: u64,
}

struct Shared {
    procs: FlutterEngineProcTable,
    engine: AtomicPtr<_FlutterEngine>,
    gpu: Backend,
    frames: Frames,
    control: Control,
    frame_state: Mutex<FrameState>,
    frame_acked: Condvar,
    closed: AtomicBool,
    platform_thread: ThreadId,
    tasks: Mutex<(BinaryHeap<Reverse<Queued>>, u64)>,
    wake: i32,
    text_input: Mutex<TextInput>,
    system: Mutex<System>,
    stats: Option<Mutex<FrameStats>>,
}

/// Per-frame costs over 120 frames, written to STATS_FILE and also printed
/// when AERA_FLUTTER_STATS is set.
#[derive(Default)]
struct FrameStats {
    frames: u32,
    started: Option<std::time::Instant>,
    ack_wait: Duration,
    readback: Duration,
    convert: Duration,
    area: f64,
    print: bool,
}

/// Where the latest per-frame timings are written, once every 120 frames,
/// so an app can show them on the phone (the demo's Motion page does).
pub const STATS_FILE: &str = "/tmp/aera-flutter-stats";

impl FrameStats {
    fn add(&mut self, ack_wait: Duration, readback: Duration, convert: Duration, area: f64) {
        let started = *self.started.get_or_insert_with(std::time::Instant::now);
        self.frames += 1;
        self.area += area;
        self.ack_wait += ack_wait;
        self.readback += readback;
        self.convert += convert;
        if self.frames == 120 {
            let ms = |d: Duration| d.as_secs_f64() * 1000.0 / 120.0;
            let fps = 119.0 / started.elapsed().as_secs_f64().max(1e-3);
            let line = format!(
                "frames sent {fps:.0}/s, waiting for AERA {:.1} ms, readback {:.1} ms, swizzle {:.1} ms, copied {:.0}%",
                ms(self.ack_wait), ms(self.readback), ms(self.convert), self.area * 100.0 / 120.0
            );
            if self.print {
                eprintln!("aera-flutter: per frame over 120: {line}");
            }
            let temporary = format!("{STATS_FILE}.new");
            if std::fs::write(&temporary, format!("{line}\n")).is_ok() {
                let _ = std::fs::rename(&temporary, STATS_FILE);
            }
            *self = FrameStats { print: self.print, ..FrameStats::default() };
        }
    }
}

pub struct Embedder {
    shared: Box<Shared>,
    _library: libloading::Library,
    _strings: Vec<CString>,
}

impl Embedder {
    pub fn start(config: Config, frames: Frames, control: Control) -> Result<Embedder, String> {
        let library = unsafe { libloading::Library::new(&config.engine_library) }
            .map_err(|e| format!("load {}: {e}", config.engine_library.display()))?;
        let mut procs: FlutterEngineProcTable = unsafe { std::mem::zeroed() };
        procs.struct_size = std::mem::size_of::<FlutterEngineProcTable>();
        unsafe {
            let get: libloading::Symbol<unsafe extern "C" fn(*mut FlutterEngineProcTable) -> FlutterEngineResult> =
                library.get(b"FlutterEngineGetProcAddresses\0").map_err(|e| e.to_string())?;
            if get(&mut procs) != kSuccess {
                return Err("FlutterEngineGetProcAddresses failed".into());
            }
        }
        let gpu = match config.renderer {
            Renderer::Gl => Backend::Gl(Gpu::new(bridge::WIDTH, bridge::HEIGHT)?),
            Renderer::Vulkan => Backend::Vulkan(Vulkan::new(bridge::WIDTH as u32, bridge::HEIGHT as u32)?),
        };
        let wake = unsafe { libc::eventfd(0, libc::EFD_CLOEXEC | libc::EFD_NONBLOCK) };
        if wake < 0 {
            return Err("eventfd failed".into());
        }
        let shared = Box::new(Shared {
            procs,
            engine: AtomicPtr::new(std::ptr::null_mut()),
            gpu,
            frames,
            control,
            frame_state: Mutex::new(FrameState { sequence: 0, pending: false, vsync_baton: None, last_vsync: 0, last_damage: None }),
            frame_acked: Condvar::new(),
            stats: Some(Mutex::new(FrameStats { print: std::env::var_os("AERA_FLUTTER_STATS").is_some(), ..FrameStats::default() })),
            closed: AtomicBool::new(false),
            platform_thread: std::thread::current().id(),
            tasks: Mutex::new((BinaryHeap::new(), 0)),
            wake,
            text_input: Mutex::new(TextInput::default()),
            system: Mutex::new(System::default()),
        });

        let mut strings = Vec::new();
        let mut keep = |s: &str| {
            let c = CString::new(s).unwrap();
            let p = c.as_ptr();
            strings.push(c);
            p
        };
        let assets = keep(&config.assets.to_string_lossy());
        let icu = keep(&config.icu_data.to_string_lossy());
        let tag = keep("flutter");
        let mut argv = vec![keep("aera-flutter")];
        for arg in &config.extra_engine_args {
            argv.push(keep(arg));
        }

        let user_data = &*shared as *const Shared as *mut c_void;
        let mut renderer: FlutterRendererConfig = unsafe { std::mem::zeroed() };
        // Must stay alive until FlutterEngineInitialize has returned.
        let mut device_extensions: Vec<*const std::ffi::c_char> = match &shared.gpu {
            Backend::Vulkan(vulkan) => vulkan.reported_device_extensions(),
            Backend::Gl(_) => Vec::new(),
        };
        let mut instance_extensions: Vec<*const std::ffi::c_char> = match &shared.gpu {
            Backend::Vulkan(vulkan) => vulkan.reported_instance_extensions(),
            Backend::Gl(_) => Vec::new(),
        };
        if let Backend::Vulkan(vulkan) = &shared.gpu {
            eprintln!("aera-flutter: Vulkan on {}", vulkan.device_name());
            let (instance, physical_device, device, queue_family_index, queue) = vulkan.handles();
            renderer.type_ = kVulkan;
            unsafe {
                let vk = &mut renderer.__bindgen_anon_1.vulkan;
                vk.struct_size = std::mem::size_of::<FlutterVulkanRendererConfig>();
                vk.version = vulkan.api_version();
                vk.instance = instance;
                vk.physical_device = physical_device;
                vk.device = device;
                vk.queue_family_index = queue_family_index;
                vk.queue = queue;
                vk.enabled_instance_extension_count = instance_extensions.len();
                vk.enabled_instance_extensions = instance_extensions.as_mut_ptr();
                vk.enabled_device_extension_count = device_extensions.len();
                vk.enabled_device_extensions = device_extensions.as_mut_ptr();
                vk.get_instance_proc_address_callback = Some(vulkan_proc_address);
                vk.get_next_image_callback = Some(vulkan_next_image);
                vk.present_image_callback = Some(vulkan_present);
            }
        } else {
        renderer.type_ = kOpenGL;
        unsafe {
            let gl = &mut renderer.__bindgen_anon_1.open_gl;
            gl.struct_size = std::mem::size_of::<FlutterOpenGLRendererConfig>();
            gl.make_current = Some(make_current);
            gl.clear_current = Some(clear_current);
            gl.present_with_info = Some(present);
            gl.fbo_with_frame_info_callback = Some(fbo_callback);
            gl.populate_existing_damage = Some(existing_damage);
            gl.make_resource_current = Some(make_resource_current);
            gl.gl_proc_resolver = Some(proc_resolver);
            gl.surface_transformation = Some(flip_vertically);
        }
        }

        let platform_runner = FlutterTaskRunnerDescription {
            struct_size: std::mem::size_of::<FlutterTaskRunnerDescription>(),
            user_data,
            runs_task_on_current_thread_callback: Some(runs_on_platform_thread),
            post_task_callback: Some(post_platform_task),
            identifier: 1,
            destruction_callback: None,
        };
        let runners = FlutterCustomTaskRunners {
            struct_size: std::mem::size_of::<FlutterCustomTaskRunners>(),
            platform_task_runner: &platform_runner,
            ..unsafe { std::mem::zeroed() }
        };

        let mut args: FlutterProjectArgs = unsafe { std::mem::zeroed() };
        args.struct_size = std::mem::size_of::<FlutterProjectArgs>();
        args.assets_path = assets;
        args.icu_data_path = icu;
        args.command_line_argc = argv.len() as i32;
        args.command_line_argv = argv.as_ptr();
        args.platform_message_callback = Some(platform_message);
        args.custom_task_runners = &runners;
        args.vsync_callback = Some(request_vsync);
        args.shutdown_dart_vm_when_done = true;
        args.log_tag = tag;

        let procs = &shared.procs;
        if unsafe { procs.RunsAOTCompiledDartCode.unwrap()() } {
            let path = CString::new(config.aot_library.to_string_lossy().as_bytes()).unwrap();
            let mut source: FlutterEngineAOTDataSource = unsafe { std::mem::zeroed() };
            source.type_ = kFlutterEngineAOTDataSourceTypeElfPath;
            source.__bindgen_anon_1.elf_path = path.as_ptr();
            let mut data: FlutterEngineAOTData = std::ptr::null_mut();
            if unsafe { procs.CreateAOTData.unwrap()(&source, &mut data) } != kSuccess {
                return Err(format!("could not load AOT code from {}", config.aot_library.display()));
            }
            args.aot_data = data;
            strings.push(path);
        }

        let mut engine: FlutterEngine = std::ptr::null_mut();
        let result = unsafe {
            procs.Initialize.unwrap()(FLUTTER_ENGINE_VERSION as usize, &renderer, &args, user_data, &mut engine)
        };
        if result != kSuccess {
            return Err(format!("FlutterEngineInitialize failed ({result})"));
        }
        shared.engine.store(engine, Ordering::Release);
        let result = unsafe { procs.RunInitialized.unwrap()(engine) };
        if result != kSuccess {
            return Err(format!("FlutterEngineRunInitialized failed ({result})"));
        }

        let embedder = Embedder { shared, _library: library, _strings: strings };
        embedder.send_metrics();
        embedder.send_locale(&config.locale);
        embedder.send_lifecycle("AppLifecycleState.resumed");
        Ok(embedder)
    }

    fn engine(&self) -> FlutterEngine {
        self.shared.engine.load(Ordering::Acquire)
    }

    fn send_metrics(&self) {
        self.shared.send_metrics();
    }

    fn send_locale(&self, locale: &str) {
        let mut parts = locale.split(['_', '-']);
        let language = CString::new(parts.next().filter(|s| !s.is_empty()).unwrap_or("en")).unwrap();
        let country = parts.next().map(|c| CString::new(c).unwrap());
        let mut entry: FlutterLocale = unsafe { std::mem::zeroed() };
        entry.struct_size = std::mem::size_of::<FlutterLocale>();
        entry.language_code = language.as_ptr();
        entry.country_code = country.as_ref().map_or(std::ptr::null(), |c| c.as_ptr());
        let mut list = [&entry as *const FlutterLocale];
        unsafe { self.shared.procs.UpdateLocales.unwrap()(self.engine(), list.as_mut_ptr(), 1) };
    }

    fn send_lifecycle(&self, state: &str) {
        self.shared.send_message("flutter/lifecycle", state.as_bytes());
    }

    fn pointer(&self, phases: &[FlutterPointerPhase], packet: &Packet) {
        let time = unsafe { self.shared.procs.GetCurrentTime.unwrap()() } / 1000;
        let events: Vec<FlutterPointerEvent> = phases
            .iter()
            .map(|&phase| {
                let mut event: FlutterPointerEvent = unsafe { std::mem::zeroed() };
                event.struct_size = std::mem::size_of::<FlutterPointerEvent>();
                event.phase = phase;
                event.timestamp = time as usize;
                event.x = packet.x as f64 * bridge::DEVICE_SCALE;
                event.y = packet.y as f64 * bridge::DEVICE_SCALE;
                event.device = packet.value as i32;
                event.device_kind = kFlutterPointerDeviceKindTouch;
                event
            })
            .collect();
        unsafe { self.shared.procs.SendPointerEvent.unwrap()(self.engine(), events.as_ptr(), events.len()) };
    }

    /// Runs the platform thread until AERA closes the bridge.
    pub fn run(self) -> i32 {
        let shared = &*self.shared;
        let mut code = 0;
        while !shared.closed.load(Ordering::Acquire) {
            // Run every platform task that is due, then sleep until the next
            // one, a bridge packet or a newly posted task.
            let now = unsafe { shared.procs.GetCurrentTime.unwrap()() };
            let mut due = Vec::new();
            let mut timeout_ms = self.answer_vsync(now);
            {
                let mut tasks = shared.tasks.lock().unwrap();
                while let Some(Reverse(next)) = tasks.0.peek() {
                    if next.target <= now {
                        due.push(tasks.0.pop().unwrap().0);
                    } else {
                        let wait = ((next.target - now) / 1_000_000).clamp(0, 1000) as i32;
                        timeout_ms = if timeout_ms < 0 { wait } else { timeout_ms.min(wait) };
                        break;
                    }
                }
            }
            for task in due {
                let task = FlutterTask { runner: task.runner as FlutterTaskRunner, task: task.task };
                unsafe { shared.procs.RunTask.unwrap()(self.engine(), &task) };
            }
            let mut fds = [
                libc::pollfd { fd: shared.control.fd(), events: libc::POLLIN, revents: 0 },
                libc::pollfd { fd: shared.wake, events: libc::POLLIN, revents: 0 },
            ];
            let ready = unsafe { libc::poll(fds.as_mut_ptr(), 2, timeout_ms) };
            if ready < 0 {
                continue;
            }
            if fds[1].revents & libc::POLLIN != 0 {
                let mut value = 0u64;
                unsafe { libc::read(shared.wake, (&mut value as *mut u64).cast(), 8) };
            }
            if fds[0].revents & (libc::POLLHUP | libc::POLLERR) != 0 && fds[0].revents & libc::POLLIN == 0 {
                break;
            }
            if fds[0].revents & libc::POLLIN != 0 {
                match shared.control.recv(Some(Duration::ZERO)) {
                    Ok(Some(packet)) if packet.valid_from_host() => {
                        if !self.handle(&packet) {
                            break;
                        }
                    }
                    Ok(Some(_)) | Err(_) => {
                        code = 78;
                        break;
                    }
                    Ok(None) => {}
                }
            }
        }
        shared.closed.store(true, Ordering::Release);
        shared.frame_acked.notify_all();
        unsafe { shared.procs.Shutdown.unwrap()(self.engine()) };
        code
    }

    /// Answers Flutter's pending vsync request once a frame period has passed.
    /// It does not wait for AERA to acknowledge the last frame, so Flutter
    /// builds the next frame while AERA shows this one; `present` waits for
    /// the acknowledgement before touching the shared slots. Returns how long the platform loop
    /// may sleep before checking again, or -1 when nothing is waiting.
    fn answer_vsync(&self, now: u64) -> i32 {
        let shared = &*self.shared;
        let mut state = shared.frame_state.lock().unwrap();
        let Some(baton) = state.vsync_baton else { return -1 };
        let earliest = state.last_vsync + FRAME_PERIOD_NS;
        if now < earliest {
            return ((earliest - now) / 1_000_000).max(1) as i32;
        }
        state.vsync_baton = None;
        state.last_vsync = now;
        drop(state);
        unsafe { shared.procs.OnVsync.unwrap()(self.engine(), baton, now, now + FRAME_PERIOD_NS) };
        -1
    }

    /// Returns false when the worker should stop.
    fn handle(&self, packet: &Packet) -> bool {
        let shared = &*self.shared;
        match packet.kind {
            kind::ACK => {
                let mut state = shared.frame_state.lock().unwrap();
                if !state.pending || packet.sequence != state.sequence {
                    return false;
                }
                state.pending = false;
                shared.frame_acked.notify_all();
            }
            kind::TOUCH_DOWN => {
                let effects = shared.system.lock().unwrap().from_host(packet);
                shared.apply_system(effects);
                self.pointer(&[kAdd, kDown], packet)
            }
            kind::TOUCH_MOVE => self.pointer(&[kMove], packet),
            kind::TOUCH_UP => self.pointer(&[kUp, kRemove], packet),
            kind::KEY => {
                let effects = shared.text_input.lock().unwrap().key(packet.value);
                shared.apply(effects);
            }
            kind::BACK => shared.send_message("flutter/navigation", br#"{"method":"popRoute","args":null}"#),
            kind::CLOSE => return false,
            kind::FORWARD | kind::RELOAD | kind::STOP | kind::OPEN | kind::SET_ZOOM => {
                let effects = shared.system.lock().unwrap().from_host(packet);
                shared.apply_system(effects);
            }
            // Browser-only requests (addresses, zoom, cookies, downloads)
            // have no meaning for a Flutter app.
            _ => {}
        }
        true
    }
}

impl Shared {
    fn engine(&self) -> FlutterEngine {
        self.engine.load(Ordering::Acquire)
    }

    fn send_message(&self, channel: &str, data: &[u8]) {
        let channel = CString::new(channel).unwrap();
        let message = FlutterPlatformMessage {
            struct_size: std::mem::size_of::<FlutterPlatformMessage>(),
            channel: channel.as_ptr(),
            message: data.as_ptr(),
            message_size: data.len(),
            response_handle: std::ptr::null(),
        };
        unsafe { self.procs.SendPlatformMessage.unwrap()(self.engine(), &message) };
    }

    fn send_metrics(&self) {
        let mut metrics: FlutterWindowMetricsEvent = unsafe { std::mem::zeroed() };
        metrics.struct_size = std::mem::size_of::<FlutterWindowMetricsEvent>();
        metrics.width = bridge::WIDTH as usize;
        metrics.height = bridge::HEIGHT as usize;
        metrics.pixel_ratio = bridge::DEVICE_SCALE;
        metrics.physical_view_inset_bottom = self.system.lock().unwrap().bottom_inset();
        unsafe { self.procs.SendWindowMetricsEvent.unwrap()(self.engine(), &metrics) };
    }

    fn apply_system(&self, effects: Vec<system::Effect>) {
        for effect in effects {
            match effect {
                system::Effect::Host(packet) => {
                    let _ = self.control.send(&packet);
                }
                system::Effect::Dart(call) => self.send_message(system::CHANNEL, call.to_string().as_bytes()),
                system::Effect::Metrics => self.send_metrics(),
            }
        }
    }

    fn apply(&self, effects: Vec<Effect>) {
        for effect in effects {
            match effect {
                Effect::ShowKeyboard { purpose } => {
                    let mut packet = Packet::new(kind::KEYBOARD_SHOW);
                    packet.value = purpose;
                    let _ = self.control.send(&packet);
                    let effects = self.system.lock().unwrap().keyboard_changed(true);
                    self.apply_system(effects);
                }
                Effect::HideKeyboard => {
                    let _ = self.control.send(&Packet::new(kind::KEYBOARD_HIDE));
                    let effects = self.system.lock().unwrap().keyboard_changed(false);
                    self.apply_system(effects);
                }
                Effect::ToFlutter(call) => self.send_message("flutter/textinput", call.to_string().as_bytes()),
            }
        }
    }

    fn respond(&self, handle: *const FlutterPlatformMessageResponseHandle, data: &[u8]) {
        if !handle.is_null() {
            unsafe {
                self.procs.SendPlatformMessageResponse.unwrap()(self.engine(), handle, data.as_ptr(), data.len())
            };
        }
    }

    fn on_message(&self, channel: &str, data: &[u8]) -> Option<Vec<u8>> {
        // JSON method channels answer `[result]` on success.
        let call: Value = serde_json::from_slice(data).ok()?;
        let method = call["method"].as_str()?;
        match channel {
            "flutter/textinput" => {
                let effects = self.text_input.lock().unwrap().handle(method, &call["args"]);
                self.apply(effects);
                Some(b"[null]".to_vec())
            }
            system::CHANNEL => {
                let (reply, effects) = self.system.lock().unwrap().call(method, &call["args"]);
                self.apply_system(effects);
                Some(if reply.is_null() { Vec::new() } else { reply.to_string().into_bytes() })
            }
            "flutter/platform" => match method {
                "SystemNavigator.pop" => {
                    self.closed.store(true, Ordering::Release);
                    self.wake();
                    Some(b"[null]".to_vec())
                }
                "Clipboard.hasStrings" => Some(json!([{"value": false}]).to_string().into_bytes()),
                _ => None,
            },
            _ => None,
        }
    }

    fn wake(&self) {
        let one = 1u64;
        unsafe { libc::write(self.wake, (&one as *const u64).cast(), 8) };
    }
}

impl Shared {
    fn gl(&self) -> &Gpu {
        match &self.gpu {
            Backend::Gl(gpu) => gpu,
            Backend::Vulkan(_) => unreachable!("GL callback with the Vulkan renderer"),
        }
    }

    fn vulkan(&self) -> &Vulkan {
        match &self.gpu {
            Backend::Vulkan(vulkan) => vulkan,
            Backend::Gl(_) => unreachable!("Vulkan callback with the GL renderer"),
        }
    }
}

unsafe fn shared<'a>(user_data: *mut c_void) -> &'a Shared {
    &*(user_data as *const Shared)
}

unsafe extern "C" fn make_current(user_data: *mut c_void) -> bool {
    shared(user_data).gl().make_render_current()
}

unsafe extern "C" fn clear_current(user_data: *mut c_void) -> bool {
    shared(user_data).gl().clear_current()
}

/// There is no resource context: Flutter then uploads images on the raster
/// thread with the render context. A second EGL context sharing the render
/// context leaked every texture uploaded through it (tens of MB a second in an
/// app decoding one 360x700 image per frame, under both softpipe and
/// llvmpipe), which AERA's 1.5 GiB memory limit turns into a crash.
unsafe extern "C" fn make_resource_current(_user_data: *mut c_void) -> bool {
    false
}

/// Draws every frame upside down. GL reads rows bottom-up, so the readback
/// then lands top-down in AERA's frame slot with no CPU pass.
unsafe extern "C" fn flip_vertically(_user_data: *mut c_void) -> FlutterTransformation {
    FlutterTransformation {
        scaleX: 1.0,
        skewX: 0.0,
        transX: 0.0,
        skewY: 0.0,
        scaleY: -1.0,
        transY: bridge::HEIGHT as f64,
        pers0: 0.0,
        pers1: 0.0,
        pers2: 1.0,
    }
}

/// Flutter wants to start a frame. Called on the UI thread; the platform loop
/// answers it (see `Embedder::answer_vsync`).
unsafe extern "C" fn request_vsync(user_data: *mut c_void, baton: isize) {
    let s = shared(user_data);
    s.frame_state.lock().unwrap().vsync_baton = Some(baton);
    s.wake();
}

unsafe extern "C" fn fbo_callback(user_data: *mut c_void, _info: *const FlutterFrameInfo) -> u32 {
    shared(user_data).gl().framebuffer()
}

unsafe extern "C" fn proc_resolver(user_data: *mut c_void, name: *const c_char) -> *mut c_void {
    shared(user_data).gl().proc_address(CStr::from_ptr(name))
}

/// Our framebuffer keeps the previous frame, so nothing needs repainting
/// beyond what Flutter itself marks as changed.
unsafe extern "C" fn existing_damage(_user_data: *mut c_void, _fbo: isize, damage: *mut FlutterDamage) {
    // The engine only turns on partial repaint when this holds at least one
    // rectangle, so report an empty one.
    static mut NOTHING: FlutterRect = FlutterRect { left: 0.0, top: 0.0, right: 0.0, bottom: 0.0 };
    let damage = &mut *damage;
    damage.num_rects = 1;
    damage.damage = std::ptr::addr_of_mut!(NOTHING);
}

/// Bounding box of Flutter's damage rectangles, or None for the whole frame.
fn damage_bounds(damage: &FlutterDamage) -> Option<Region> {
    if damage.num_rects == 0 || damage.damage.is_null() {
        return None;
    }
    let rects = unsafe { std::slice::from_raw_parts(damage.damage, damage.num_rects) };
    rects.iter().fold(Some(Region::default()), |bounds, r| {
        let x = r.left.floor() as i32;
        let y = r.top.floor() as i32;
        let rect = Region { x, y, width: r.right.ceil() as i32 - x, height: r.bottom.ceil() as i32 - y };
        bounds.map(|b| b.union(rect))
    })
}

/// Raster thread: copy the changed part of the finished frame into the free
/// slot and hand it to AERA, waiting first for AERA to release the previous
/// one.
unsafe extern "C" fn present(user_data: *mut c_void, info: *const FlutterPresentInfo) -> bool {
    let s = shared(user_data);
    let damage = if std::env::var_os("AERA_FLUTTER_FULL_FRAMES").is_some() { None } else { damage_bounds(&(*info).frame_damage) };
    // Start the GPU copy before waiting for AERA, so the two overlap. The
    // slot itself is only written once AERA has released it.
    let (next, region) = {
        let mut state = s.frame_state.lock().unwrap();
        let region = match (damage, state.last_damage) {
            (Some(now), Some(before)) => Some(now.union(before)),
            _ => None,
        };
        state.last_damage = damage;
        (state.sequence.wrapping_add(1), region)
    };
    let readback = s.gl().start_read(region);
    let Some((state, ack_wait)) = wait_for_ack(s) else { return false };
    let (readback, convert) = s.gl().finish_read(readback, s.frames.slot(next));
    if let Some(stats) = &s.stats {
        let area = region.map_or(1.0, |r| r.area() as f64 / (bridge::WIDTH as f64 * bridge::HEIGHT as f64));
        stats.lock().unwrap().add(ack_wait, readback, convert, area);
    }
    send_frame(s, state, next)
}

/// Hands the filled slot for `next` to AERA.
fn send_frame(s: &Shared, mut state: std::sync::MutexGuard<'_, FrameState>, next: u32) -> bool {
    let mut packet = Packet::new(kind::FRAME);
    packet.sequence = next;
    packet.x = bridge::WIDTH;
    packet.y = bridge::HEIGHT;
    packet.value = bridge::FRAME_BYTES as u32;
    if s.control.send(&packet).is_err() {
        s.closed.store(true, Ordering::Release);
        s.wake();
        return false;
    }
    state.sequence = next;
    state.pending = true;
    true
}

/// Waits until AERA has released the previous frame. None when closing.
fn wait_for_ack(s: &Shared) -> Option<(std::sync::MutexGuard<'_, FrameState>, Duration)> {
    let waited = std::time::Instant::now();
    let mut state = s.frame_state.lock().unwrap();
    while state.pending && !s.closed.load(Ordering::Acquire) {
        state = s.frame_acked.wait_timeout(state, Duration::from_millis(250)).unwrap().0;
    }
    if s.closed.load(Ordering::Acquire) {
        return None;
    }
    Some((state, waited.elapsed()))
}

unsafe extern "C" fn vulkan_proc_address(user_data: *mut c_void, instance: *mut c_void, name: *const std::ffi::c_char) -> *mut c_void {
    shared(user_data).vulkan().instance_proc_address(instance, CStr::from_ptr(name))
}

unsafe extern "C" fn vulkan_next_image(user_data: *mut c_void, _info: *const FlutterFrameInfo) -> FlutterVulkanImage {
    let (image, format) = shared(user_data).vulkan().next_image();
    FlutterVulkanImage { struct_size: std::mem::size_of::<FlutterVulkanImage>(), image, format }
}

/// Raster thread: Flutter finished drawing `image` and waited for the GPU.
unsafe extern "C" fn vulkan_present(user_data: *mut c_void, image: *const FlutterVulkanImage) -> bool {
    let s = shared(user_data);
    let Some((mut state, ack_wait)) = wait_for_ack(s) else { return false };
    let next = state.sequence.wrapping_add(1);
    let start = std::time::Instant::now();
    if let Err(error) = s.vulkan().read_frame((*image).image, s.frames.slot(next)) {
        eprintln!("aera-flutter: {error}");
        return false;
    }
    if let Some(stats) = &s.stats {
        stats.lock().unwrap().add(ack_wait, start.elapsed(), Duration::ZERO, 1.0);
    }
    state.last_damage = None;
    send_frame(s, state, next)
}

unsafe extern "C" fn runs_on_platform_thread(user_data: *mut c_void) -> bool {
    std::thread::current().id() == shared(user_data).platform_thread
}

unsafe extern "C" fn post_platform_task(task: FlutterTask, target: u64, user_data: *mut c_void) {
    let s = shared(user_data);
    {
        let mut tasks = s.tasks.lock().unwrap();
        tasks.1 += 1;
        let order = tasks.1;
        tasks.0.push(Reverse(Queued { target, order, runner: task.runner as usize, task: task.task }));
    }
    s.wake();
}

unsafe extern "C" fn platform_message(message: *const FlutterPlatformMessage, user_data: *mut c_void) {
    let s = shared(user_data);
    let message = &*message;
    let channel = CStr::from_ptr(message.channel).to_string_lossy();
    let data = if message.message.is_null() {
        &[][..]
    } else {
        std::slice::from_raw_parts(message.message, message.message_size)
    };
    let reply = s.on_message(&channel, data).unwrap_or_default();
    s.respond(message.response_handle, &reply);
}

/// Default runtime layout inside the payload (the jail's `/`).
pub fn default_config(root: &Path) -> Config {
    let share = root.join("usr/share/flutter");
    // AERA's jail seccomp policy denies listen(), so the Dart VM service
    // could never accept connections there anyway.
    let mut extra_engine_args = vec!["--disable-vm-service".to_string()];
    // `usr/share/flutter/renderer` (or AERA_FLUTTER_RENDERER) picks the
    // renderer: `gl` (default), `vulkan` (Skia) or `impeller` (Impeller on
    // Vulkan).
    let choice = std::env::var("AERA_FLUTTER_RENDERER")
        .ok()
        .or_else(|| std::fs::read_to_string(share.join("renderer")).ok())
        .unwrap_or_default();
    let renderer = match choice.trim() {
        "vulkan" => Renderer::Vulkan,
        "impeller" => {
            extra_engine_args.push("--enable-impeller=true".into());
            Renderer::Vulkan
        }
        _ => Renderer::Gl,
    };
    Config {
        engine_library: root.join("usr/lib/libflutter_engine.so"),
        assets: share.join("flutter_assets"),
        icu_data: share.join("icudtl.dat"),
        aot_library: root.join("usr/lib/libapp.so"),
        locale: std::env::var("AERA_LOCALE").unwrap_or_else(|_| "en".into()),
        extra_engine_args,
        renderer,
    }
}
