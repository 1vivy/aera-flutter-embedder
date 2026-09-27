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
const GL_RENDERER: GLenum = 0x1F01;

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
}

/// An EGL display with two contexts: one Flutter renders with on the raster
/// thread and one sharing its objects for texture uploads on the IO thread.
pub struct Gpu {
    _library: Library,
    egl: Egl,
    gles: Gles,
    display: EGLDisplay,
    render: EGLContext,
    resource: EGLContext,
    width: i32,
    height: i32,
    /// Framebuffer object, created on the raster thread on first use.
    fbo: std::sync::atomic::AtomicU32,
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
            let resource = (egl.create_context)(display, config, render, context_attributes.as_ptr());
            if render.is_null() || resource.is_null() {
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
            };
            Ok(Gpu {
                _library: library,
                egl,
                gles,
                display,
                render,
                resource,
                width,
                height,
                fbo: std::sync::atomic::AtomicU32::new(0),
            })
        }
    }

    pub fn make_render_current(&self) -> bool {
        unsafe { (self.egl.make_current)(self.display, std::ptr::null_mut(), std::ptr::null_mut(), self.render) != 0 }
    }

    pub fn make_resource_current(&self) -> bool {
        unsafe { (self.egl.make_current)(self.display, std::ptr::null_mut(), std::ptr::null_mut(), self.resource) != 0 }
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

    /// Copies the finished frame into `out` as top-down ARGB8888 (B, G, R, A
    /// bytes in memory), the layout AERA's browser bridge expects.
    /// `scratch` must hold `width * height * 4` bytes.
    pub fn read_frame(&self, scratch: &mut [u8], out: &mut [u8]) {
        let row = self.width as usize * 4;
        let bytes = row * self.height as usize;
        assert!(scratch.len() >= bytes && out.len() >= bytes);
        unsafe {
            let g = &self.gles;
            (g.bind_framebuffer)(GL_FRAMEBUFFER, self.framebuffer());
            (g.pixel_storei)(GL_PACK_ALIGNMENT, 4);
            (g.read_pixels)(0, 0, self.width, self.height, GL_RGBA, GL_UNSIGNED_BYTE, scratch.as_mut_ptr().cast());
        }
        // GL rows run bottom-up and RGBA; the bridge wants top-down BGRA.
        for (y, source) in scratch[..bytes].chunks_exact(row).enumerate() {
            let target = &mut out[(self.height as usize - 1 - y) * row..][..row];
            for (s, t) in source.chunks_exact(4).zip(target.chunks_exact_mut(4)) {
                t[0] = s[2];
                t[1] = s[1];
                t[2] = s[0];
                t[3] = s[3];
            }
        }
    }

    pub fn finish(&self) {
        unsafe { (self.gles.finish)() }
    }
}
