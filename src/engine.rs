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
use crate::gl::Gpu;
use crate::text_input::{Effect, TextInput};

pub struct Config {
    pub engine_library: PathBuf,
    pub assets: PathBuf,
    pub icu_data: PathBuf,
    /// `app.so` for AOT (release/profile) engines; unused by debug engines.
    pub aot_library: PathBuf,
    pub locale: String,
    pub extra_engine_args: Vec<String>,
}

struct FrameState {
    /// Last sequence sent to AERA.
    sequence: u32,
    /// A frame is out and AERA has not acknowledged it yet.
    pending: bool,
}

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
    gpu: Gpu,
    frames: Frames,
    control: Control,
    frame_state: Mutex<FrameState>,
    frame_acked: Condvar,
    scratch: Mutex<Vec<u8>>,
    closed: AtomicBool,
    platform_thread: ThreadId,
    tasks: Mutex<(BinaryHeap<Reverse<Queued>>, u64)>,
    wake: i32,
    text_input: Mutex<TextInput>,
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
        let gpu = Gpu::new(bridge::WIDTH, bridge::HEIGHT)?;
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
            frame_state: Mutex::new(FrameState { sequence: 0, pending: false }),
            frame_acked: Condvar::new(),
            scratch: Mutex::new(vec![0; bridge::FRAME_BYTES]),
            closed: AtomicBool::new(false),
            platform_thread: std::thread::current().id(),
            tasks: Mutex::new((BinaryHeap::new(), 0)),
            wake,
            text_input: Mutex::new(TextInput::default()),
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
        renderer.type_ = kOpenGL;
        unsafe {
            let gl = &mut renderer.__bindgen_anon_1.open_gl;
            gl.struct_size = std::mem::size_of::<FlutterOpenGLRendererConfig>();
            gl.make_current = Some(make_current);
            gl.clear_current = Some(clear_current);
            gl.present = Some(present);
            gl.fbo_callback = Some(fbo_callback);
            gl.make_resource_current = Some(make_resource_current);
            gl.gl_proc_resolver = Some(proc_resolver);
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
        let mut metrics: FlutterWindowMetricsEvent = unsafe { std::mem::zeroed() };
        metrics.struct_size = std::mem::size_of::<FlutterWindowMetricsEvent>();
        metrics.width = bridge::WIDTH as usize;
        metrics.height = bridge::HEIGHT as usize;
        metrics.pixel_ratio = bridge::DEVICE_SCALE;
        unsafe { self.shared.procs.SendWindowMetricsEvent.unwrap()(self.engine(), &metrics) };
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
            let mut timeout_ms = -1;
            {
                let mut tasks = shared.tasks.lock().unwrap();
                while let Some(Reverse(next)) = tasks.0.peek() {
                    if next.target <= now {
                        due.push(tasks.0.pop().unwrap().0);
                    } else {
                        timeout_ms = ((next.target - now) / 1_000_000).clamp(0, 1000) as i32;
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
            kind::TOUCH_DOWN => self.pointer(&[kAdd, kDown], packet),
            kind::TOUCH_MOVE => self.pointer(&[kMove], packet),
            kind::TOUCH_UP => self.pointer(&[kUp, kRemove], packet),
            kind::KEY => {
                let effects = shared.text_input.lock().unwrap().key(packet.value);
                shared.apply(effects);
            }
            kind::BACK => shared.send_message("flutter/navigation", br#"{"method":"popRoute","args":null}"#),
            kind::CLOSE => return false,
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

    fn apply(&self, effects: Vec<Effect>) {
        for effect in effects {
            match effect {
                Effect::ShowKeyboard { purpose } => {
                    let mut packet = Packet::new(kind::KEYBOARD_SHOW);
                    packet.value = purpose;
                    let _ = self.control.send(&packet);
                }
                Effect::HideKeyboard => {
                    let _ = self.control.send(&Packet::new(kind::KEYBOARD_HIDE));
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

unsafe fn shared<'a>(user_data: *mut c_void) -> &'a Shared {
    &*(user_data as *const Shared)
}

unsafe extern "C" fn make_current(user_data: *mut c_void) -> bool {
    shared(user_data).gpu.make_render_current()
}

unsafe extern "C" fn clear_current(user_data: *mut c_void) -> bool {
    shared(user_data).gpu.clear_current()
}

unsafe extern "C" fn make_resource_current(user_data: *mut c_void) -> bool {
    shared(user_data).gpu.make_resource_current()
}

unsafe extern "C" fn fbo_callback(user_data: *mut c_void) -> u32 {
    shared(user_data).gpu.framebuffer()
}

unsafe extern "C" fn proc_resolver(user_data: *mut c_void, name: *const c_char) -> *mut c_void {
    shared(user_data).gpu.proc_address(CStr::from_ptr(name))
}

/// Raster thread: copy the finished frame into the free slot and hand it to
/// AERA, waiting first for AERA to release the previous one.
unsafe extern "C" fn present(user_data: *mut c_void) -> bool {
    let s = shared(user_data);
    let mut state = s.frame_state.lock().unwrap();
    while state.pending && !s.closed.load(Ordering::Acquire) {
        state = s.frame_acked.wait_timeout(state, Duration::from_millis(250)).unwrap().0;
    }
    if s.closed.load(Ordering::Acquire) {
        return false;
    }
    let next = state.sequence.wrapping_add(1);
    {
        let mut scratch = s.scratch.lock().unwrap();
        s.gpu.read_frame(&mut scratch, s.frames.slot(next));
    }
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
    Config {
        engine_library: root.join("usr/lib/libflutter_engine.so"),
        assets: share.join("flutter_assets"),
        icu_data: share.join("icudtl.dat"),
        aot_library: root.join("usr/lib/libapp.so"),
        locale: std::env::var("AERA_LOCALE").unwrap_or_else(|_| "en".into()),
        // AERA's jail seccomp policy denies listen(), so the Dart VM
        // service could never accept connections there anyway.
        extra_engine_args: vec!["--disable-vm-service".into()],
    }
}
