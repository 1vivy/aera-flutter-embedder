//! Offscreen OpenGL ES through EGL's Mesa surfaceless platform.
//!
//! On the phone this is the same stack AERA Browser uses: Mesa EGL → Zink →
//! Turnip → `/dev/kgsl-3d0`. On a PC it is whatever Mesa driver is present
//! (llvmpipe in CI). Nothing here touches a display; Flutter draws into a
//! framebuffer object and [`Gpu::read_frame`] copies it out.

use std::ffi::{c_void, CStr, CString};
use std::os::raw::{c_char, c_int, c_uint};

use libloading::Library;

type EGLDisplay = *mut c_void;
type EGLConfig = *mut c_void;
type EGLContext = *mut c_void;
type EGLint = i32;
type EGLBoolean = c_uint;
type EGLenum = c_uint;

const EGL_PLATFORM_SURFACELESS_MESA: EGLenum = 0x31DD;
const EGL_NONE: EGLint = 0x3038;
const EGL_RED_SIZE: EGLint = 0x3024;
const EGL_GREEN_SIZE: EGLint = 0x3023;
const EGL_BLUE_SIZE: EGLint = 0x3022;
const EGL_ALPHA_SIZE: EGLint = 0x3021;
const EGL_STENCIL_SIZE: EGLint = 0x3026;
const EGL_SURFACE_TYPE: EGLint = 0x3033;
const EGL_RENDERABLE_TYPE: EGLint = 0x3040;
const EGL_OPENGL_ES2_BIT: EGLint = 0x0004;
const EGL_OPENGL_ES_API: EGLenum = 0x30A0;
const EGL_CONTEXT_CLIENT_VERSION: EGLint = 0x3098;
const EGL_EXTENSIONS: EGLint = 0x3055;

type GLenum = c_uint;
type GLuint = c_uint;
type GLint = c_int;
type GLsizei = c_int;

const GL_FRAMEBUFFER: GLenum = 0x8D40;
const GL_RENDERBUFFER: GLenum = 0x8D41;
const GL_COLOR_ATTACHMENT0: GLenum = 0x8CE0;
const GL_DEPTH_STENCIL_ATTACHMENT: GLenum = 0x821A;
const GL_RGBA8: GLenum = 0x8058;
const GL_DEPTH24_STENCIL8: GLenum = 0x88F0;
const GL_FRAMEBUFFER_COMPLETE: GLenum = 0x8CD5;
const GL_RGBA: GLenum = 0x1908;
const GL_UNSIGNED_BYTE: GLenum = 0x1401;
const GL_PACK_ALIGNMENT: GLenum = 0x0D05;
const GL_PACK_ROW_LENGTH: GLenum = 0x0D02;
const GL_RENDERER: GLenum = 0x1F01;
const GL_EXTENSIONS: GLenum = 0x1F03;
const GL_BGRA_EXT: GLenum = 0x80E1;
const GL_PIXEL_PACK_BUFFER: GLenum = 0x88EB;
const GL_STREAM_READ: GLenum = 0x88E1;
const GL_MAP_READ_BIT: GLenum = 0x0001;

struct Egl {
    get_platform_display: unsafe extern "C" fn(EGLenum, *mut c_void, *const isize) -> EGLDisplay,
    initialize: unsafe extern "C" fn(EGLDisplay, *mut EGLint, *mut EGLint) -> EGLBoolean,
    query_string: unsafe extern "C" fn(EGLDisplay, EGLint) -> *const c_char,
    bind_api: unsafe extern "C" fn(EGLenum) -> EGLBoolean,
    choose_config:
        unsafe extern "C" fn(EGLDisplay, *const EGLint, *mut EGLConfig, EGLint, *mut EGLint) -> EGLBoolean,
    create_context: unsafe extern "C" fn(EGLDisplay, EGLConfig, EGLContext, *const EGLint) -> EGLContext,
    make_current: unsafe extern "C" fn(EGLDisplay, *mut c_void, *mut c_void, EGLContext) -> EGLBoolean,
    get_proc_address: unsafe extern "C" fn(*const c_char) -> *mut c_void,
    get_error: unsafe extern "C" fn() -> EGLint,
}

struct Gles {
    gen_framebuffers: unsafe extern "C" fn(GLsizei, *mut GLuint),
    bind_framebuffer: unsafe extern "C" fn(GLenum, GLuint),
    gen_renderbuffers: unsafe extern "C" fn(GLsizei, *mut GLuint),
    bind_renderbuffer: unsafe extern "C" fn(GLenum, GLuint),
    renderbuffer_storage: unsafe extern "C" fn(GLenum, GLenum, GLsizei, GLsizei),
    framebuffer_renderbuffer: unsafe extern "C" fn(GLenum, GLenum, GLenum, GLuint),
    check_framebuffer_status: unsafe extern "C" fn(GLenum) -> GLenum,
    read_pixels: unsafe extern "C" fn(GLint, GLint, GLsizei, GLsizei, GLenum, GLenum, *mut c_void),
    pixel_storei: unsafe extern "C" fn(GLenum, GLint),
    finish: unsafe extern "C" fn(),
    get_string: unsafe extern "C" fn(GLenum) -> *const u8,
    get_error: unsafe extern "C" fn() -> GLenum,
    flush: unsafe extern "C" fn(),
    gen_buffers: unsafe extern "C" fn(GLsizei, *mut GLuint),
    bind_buffer: unsafe extern "C" fn(GLenum, GLuint),
    buffer_data: unsafe extern "C" fn(GLenum, isize, *const c_void, GLenum),
    map_buffer_range: unsafe extern "C" fn(GLenum, isize, isize, GLenum) -> *mut c_void,
    unmap_buffer: unsafe extern "C" fn(GLenum) -> u8,
}

/// A frame copy the GPU has been asked to make into the pack buffer; hand it
/// to `Gpu::finish_read` to wait for it and copy it out.
pub struct Readback {
    region: Region,
    bgra: bool,
    issued: std::time::Duration,
}

/// An EGL display with two contexts: one Flutter renders with on the raster
/// thread and one sharing its objects for texture uploads on the IO thread.
/// A rectangle of the frame in top-down pixel rows, which (because Flutter
/// draws flipped) are also the framebuffer's GL rows.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Region {
    pub x: i32,
    pub y: i32,
    pub width: i32,
    pub height: i32,
}

impl Region {
    pub fn union(self, other: Region) -> Region {
        if self.width <= 0 || self.height <= 0 {
            return other;
        }
        if other.width <= 0 || other.height <= 0 {
            return self;
        }
        let (x, y) = (self.x.min(other.x), self.y.min(other.y));
        let right = (self.x + self.width).max(other.x + other.width);
        let bottom = (self.y + self.height).max(other.y + other.height);
        Region { x, y, width: right - x, height: bottom - y }
    }

    fn clamp(self, width: i32, height: i32) -> Region {
        let (x, y) = (self.x.clamp(0, width), self.y.clamp(0, height));
        let right = (self.x + self.width).clamp(x, width);
        let bottom = (self.y + self.height).clamp(y, height);
        Region { x, y, width: right - x, height: bottom - y }
    }

    pub fn area(self) -> u64 {
        self.width.max(0) as u64 * self.height.max(0) as u64
    }
}

pub struct Gpu {
    _library: Library,
    egl: Egl,
    gles: Gles,
    display: EGLDisplay,
    render: EGLContext,
    width: i32,
    height: i32,
    /// Framebuffer object, created on the raster thread on first use.
    fbo: std::sync::atomic::AtomicU32,
    /// 0 not yet checked, 1 GL_BGRA_EXT readback works, 2 it does not.
    bgra_readback: std::sync::atomic::AtomicU8,
    /// Pixel pack buffer frames are read into, created on first use.
    pack_buffer: std::sync::atomic::AtomicU32,
}

// EGL handles are process-wide; each context is only made current on the
// thread Flutter calls us from.
unsafe impl Send for Gpu {}
unsafe impl Sync for Gpu {}

impl Gpu {
    pub fn new(width: i32, height: i32) -> Result<Gpu, String> {
        unsafe {
            let library = Library::new("libEGL.so.1").map_err(|e| format!("load libEGL.so.1: {e}"))?;
            macro_rules! sym {
                ($name:literal) => {
                    *library
                        .get(concat!($name, "\0").as_bytes())
                        .map_err(|e| format!("{}: {e}", $name))?
                };
            }
            let egl = Egl {
                get_platform_display: sym!("eglGetPlatformDisplay"),
                initialize: sym!("eglInitialize"),
                query_string: sym!("eglQueryString"),
                bind_api: sym!("eglBindAPI"),
                choose_config: sym!("eglChooseConfig"),
                create_context: sym!("eglCreateContext"),
                make_current: sym!("eglMakeCurrent"),
                get_proc_address: sym!("eglGetProcAddress"),
                get_error: sym!("eglGetError"),
            };
            let display = (egl.get_platform_display)(EGL_PLATFORM_SURFACELESS_MESA, std::ptr::null_mut(), std::ptr::null());
            if display.is_null() || (egl.initialize)(display, std::ptr::null_mut(), std::ptr::null_mut()) == 0 {
                return Err(format!("Mesa surfaceless EGL is unavailable (EGL error {:#x})", (egl.get_error)()));
            }
            let extensions = CStr::from_ptr((egl.query_string)(display, EGL_EXTENSIONS)).to_string_lossy().into_owned();
            if !extensions.contains("EGL_KHR_surfaceless_context") {
                return Err("EGL_KHR_surfaceless_context is missing".into());
            }
            (egl.bind_api)(EGL_OPENGL_ES_API);
            let attributes = [
                EGL_RED_SIZE, 8, EGL_GREEN_SIZE, 8, EGL_BLUE_SIZE, 8, EGL_ALPHA_SIZE, 8,
                EGL_STENCIL_SIZE, 8, EGL_SURFACE_TYPE, 0, EGL_RENDERABLE_TYPE, EGL_OPENGL_ES2_BIT, EGL_NONE,
            ];
            let mut config: EGLConfig = std::ptr::null_mut();
            let mut count = 0;
            if (egl.choose_config)(display, attributes.as_ptr(), &mut config, 1, &mut count) == 0 || count < 1 {
                return Err("no RGBA8 GLES EGL config".into());
            }
            let context_attributes = [EGL_CONTEXT_CLIENT_VERSION, 3, EGL_NONE];
            let render = (egl.create_context)(display, config, std::ptr::null_mut(), context_attributes.as_ptr());
            if render.is_null() {
                return Err(format!("eglCreateContext failed ({:#x})", (egl.get_error)()));
            }
            let proc = |name: &str| -> Result<*mut c_void, String> {
                let c = CString::new(name).unwrap();
                let pointer = (egl.get_proc_address)(c.as_ptr());
                if pointer.is_null() { Err(format!("missing GL function {name}")) } else { Ok(pointer) }
            };
            macro_rules! gl {
                ($name:literal) => {
                    std::mem::transmute(proc($name)?)
                };
            }
            let gles = Gles {
                gen_framebuffers: gl!("glGenFramebuffers"),
                bind_framebuffer: gl!("glBindFramebuffer"),
                gen_renderbuffers: gl!("glGenRenderbuffers"),
                bind_renderbuffer: gl!("glBindRenderbuffer"),
                renderbuffer_storage: gl!("glRenderbufferStorage"),
                framebuffer_renderbuffer: gl!("glFramebufferRenderbuffer"),
                check_framebuffer_status: gl!("glCheckFramebufferStatus"),
                read_pixels: gl!("glReadPixels"),
                pixel_storei: gl!("glPixelStorei"),
                finish: gl!("glFinish"),
                get_string: gl!("glGetString"),
                get_error: gl!("glGetError"),
                flush: gl!("glFlush"),
                gen_buffers: gl!("glGenBuffers"),
                bind_buffer: gl!("glBindBuffer"),
                buffer_data: gl!("glBufferData"),
                map_buffer_range: gl!("glMapBufferRange"),
                unmap_buffer: gl!("glUnmapBuffer"),
            };
            Ok(Gpu {
                _library: library,
                egl,
                gles,
                display,
                render,
                width,
                height,
                fbo: std::sync::atomic::AtomicU32::new(0),
                bgra_readback: std::sync::atomic::AtomicU8::new(0),
                pack_buffer: std::sync::atomic::AtomicU32::new(0),
            })
        }
    }

    pub fn make_render_current(&self) -> bool {
        unsafe { (self.egl.make_current)(self.display, std::ptr::null_mut(), std::ptr::null_mut(), self.render) != 0 }
    }

    pub fn clear_current(&self) -> bool {
        unsafe {
            (self.egl.make_current)(self.display, std::ptr::null_mut(), std::ptr::null_mut(), std::ptr::null_mut()) != 0
        }
    }

    pub fn proc_address(&self, name: &CStr) -> *mut c_void {
        unsafe { (self.egl.get_proc_address)(name.as_ptr()) }
    }

    /// GL renderer string, for the startup log. Needs a current context.
    pub fn renderer(&self) -> String {
        unsafe {
            let text = (self.gles.get_string)(GL_RENDERER);
            if text.is_null() { String::new() } else { CStr::from_ptr(text.cast()).to_string_lossy().into_owned() }
        }
    }

    /// The framebuffer Flutter draws into. Called on the raster thread with
    /// the render context current.
    pub fn framebuffer(&self) -> u32 {
        use std::sync::atomic::Ordering;
        let existing = self.fbo.load(Ordering::Acquire);
        if existing != 0 {
            return existing;
        }
        unsafe {
            let g = &self.gles;
            let (mut fbo, mut buffers) = (0, [0u32; 2]);
            (g.gen_framebuffers)(1, &mut fbo);
            (g.gen_renderbuffers)(2, buffers.as_mut_ptr());
            (g.bind_renderbuffer)(GL_RENDERBUFFER, buffers[0]);
            (g.renderbuffer_storage)(GL_RENDERBUFFER, GL_RGBA8, self.width, self.height);
            (g.bind_renderbuffer)(GL_RENDERBUFFER, buffers[1]);
            (g.renderbuffer_storage)(GL_RENDERBUFFER, GL_DEPTH24_STENCIL8, self.width, self.height);
            (g.bind_framebuffer)(GL_FRAMEBUFFER, fbo);
            (g.framebuffer_renderbuffer)(GL_FRAMEBUFFER, GL_COLOR_ATTACHMENT0, GL_RENDERBUFFER, buffers[0]);
            (g.framebuffer_renderbuffer)(GL_FRAMEBUFFER, GL_DEPTH_STENCIL_ATTACHMENT, GL_RENDERBUFFER, buffers[1]);
            if (g.check_framebuffer_status)(GL_FRAMEBUFFER) != GL_FRAMEBUFFER_COMPLETE {
                eprintln!("aera-flutter: framebuffer incomplete");
            }
            self.fbo.store(fbo, Ordering::Release);
            fbo
        }
    }

    /// Asks the GPU to copy the framebuffer (only `region` when given) into
    /// a pixel pack buffer and returns without waiting for it, so the copy
    /// runs while the caller waits for AERA. Raster thread only.
    ///
    /// The copy is top-down ARGB8888, i.e. B, G, R, A bytes in memory, the
    /// layout of AERA's pixel surface. Flutter already draws the frame
    /// upside down (see `engine::flip_vertically`), so GL's bottom-up rows
    /// come out top-down. Where the driver offers GL_EXT_read_format_bgra
    /// the GPU also does the swizzle; otherwise `finish_read` swaps R and B.
    pub fn start_read(&self, region: Option<Region>) -> Readback {
        use std::sync::atomic::Ordering;
        let start = std::time::Instant::now();
        let full = Region { x: 0, y: 0, width: self.width, height: self.height };
        let r = region.unwrap_or(full).clamp(self.width, self.height);
        let mut bgra = match self.bgra_readback.load(Ordering::Relaxed) {
            0 => {
                let supported = self.has_extension("GL_EXT_read_format_bgra");
                self.bgra_readback.store(if supported { 1 } else { 2 }, Ordering::Relaxed);
                supported
            }
            state => state == 1,
        };
        if r.width == 0 || r.height == 0 {
            return Readback { region: r, bgra, issued: start.elapsed() };
        }
        let row = self.width as usize * 4;
        let first = r.y as usize * row + r.x as usize * 4;
        unsafe {
            let g = &self.gles;
            (g.bind_framebuffer)(GL_FRAMEBUFFER, self.framebuffer());
            (g.bind_buffer)(GL_PIXEL_PACK_BUFFER, self.pack_buffer());
            (g.pixel_storei)(GL_PACK_ALIGNMENT, 4);
            (g.pixel_storei)(GL_PACK_ROW_LENGTH, self.width);
            while (g.get_error)() != 0 {}
            // With a pack buffer bound, the pointer is an offset into it.
            let offset = first as *mut c_void;
            if bgra {
                (g.read_pixels)(r.x, r.y, r.width, r.height, GL_BGRA_EXT, GL_UNSIGNED_BYTE, offset);
                if (g.get_error)() != 0 {
                    eprintln!("aera-flutter: BGRA readback refused, swizzling on the CPU");
                    self.bgra_readback.store(2, Ordering::Relaxed);
                    bgra = false;
                }
            }
            if !bgra {
                (g.read_pixels)(r.x, r.y, r.width, r.height, GL_RGBA, GL_UNSIGNED_BYTE, offset);
            }
            // Flutter shares this context. Its own readbacks (toImage,
            // screenshots) assume the default row length and would write past
            // their buffers with ours left set.
            (g.pixel_storei)(GL_PACK_ROW_LENGTH, 0);
            (g.bind_buffer)(GL_PIXEL_PACK_BUFFER, 0);
            (g.flush)();
        }
        Readback { region: r, bgra, issued: start.elapsed() }
    }

    /// Waits for the copy `start_read` began and writes it into `out`, a
    /// top-down BGRA frame; pixels outside the region are left as they are.
    /// Returns the raster thread's time spent reading and swizzling.
    /// `out` holds rows of `stride` bytes (AERA's surface stride).
    pub fn finish_read(&self, readback: Readback, out: &mut [u8], stride: usize) -> (std::time::Duration, std::time::Duration) {
        let start = std::time::Instant::now();
        let r = readback.region;
        let row = self.width as usize * 4;
        assert!(stride >= row && out.len() >= stride * self.height as usize);
        if r.width == 0 || r.height == 0 {
            return (readback.issued, Default::default());
        }
        let first = r.y as usize * row + r.x as usize * 4;
        let target = r.y as usize * stride + r.x as usize * 4;
        let span = (r.height as usize - 1) * row + r.width as usize * 4;
        let line = r.width as usize * 4;
        unsafe {
            let g = &self.gles;
            (g.bind_buffer)(GL_PIXEL_PACK_BUFFER, self.pack_buffer());
            let mapped = (g.map_buffer_range)(GL_PIXEL_PACK_BUFFER, first as isize, span as isize, GL_MAP_READ_BIT);
            if mapped.is_null() {
                eprintln!("aera-flutter: could not map the frame readback buffer");
            } else {
                let source = std::slice::from_raw_parts(mapped as *const u8, span);
                for y in 0..r.height as usize {
                    out[target + y * stride..][..line].copy_from_slice(&source[y * row..][..line]);
                }
                (g.unmap_buffer)(GL_PIXEL_PACK_BUFFER);
            }
            (g.bind_buffer)(GL_PIXEL_PACK_BUFFER, 0);
        }
        let read = start.elapsed();
        if !readback.bgra {
            for line in out[target..].chunks_mut(stride).take(r.height as usize) {
                for pixel in line[..r.width as usize * 4].chunks_exact_mut(4) {
                    pixel.swap(0, 2);
                }
            }
        }
        (readback.issued + read, start.elapsed() - read)
    }

    fn pack_buffer(&self) -> GLuint {
        use std::sync::atomic::Ordering;
        let existing = self.pack_buffer.load(Ordering::Acquire);
        if existing != 0 {
            return existing;
        }
        let mut buffer = 0;
        unsafe {
            (self.gles.gen_buffers)(1, &mut buffer);
            (self.gles.bind_buffer)(GL_PIXEL_PACK_BUFFER, buffer);
            let size = self.width as isize * self.height as isize * 4;
            (self.gles.buffer_data)(GL_PIXEL_PACK_BUFFER, size, std::ptr::null(), GL_STREAM_READ);
            (self.gles.bind_buffer)(GL_PIXEL_PACK_BUFFER, 0);
        }
        self.pack_buffer.store(buffer, Ordering::Release);
        buffer
    }

    fn has_extension(&self, name: &str) -> bool {
        unsafe {
            let text = (self.gles.get_string)(GL_EXTENSIONS);
            !text.is_null() && CStr::from_ptr(text.cast()).to_string_lossy().split(' ').any(|e| e == name)
        }
    }

    pub fn finish(&self) {
        unsafe { (self.gles.finish)() }
    }
}

#[cfg(test)]
mod tests {
    use super::Region;

    #[test]
    fn region_union_and_clamp() {
        let a = Region { x: 10, y: 10, width: 10, height: 10 };
        let b = Region { x: 30, y: 5, width: 5, height: 5 };
        assert_eq!(a.union(b), Region { x: 10, y: 5, width: 25, height: 15 });
        assert_eq!(Region::default().union(a), a);
        let wide = Region { x: -5, y: 2090, width: 2000, height: 50 };
        assert_eq!(wide.clamp(1080, 2100), Region { x: 0, y: 2090, width: 1080, height: 10 });
    }
}
