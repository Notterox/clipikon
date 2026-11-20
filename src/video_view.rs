//! Draws the editor's video frame with OpenGL, inside egui's frame through a paint callback.
//!
//! When the view is larger than the decoded frame, it's scaled up with the chosen [`Upscaler`]:
//! - FSR 1, AMD FidelityFX Super Resolution 1 (MIT): EASU, an edge-adaptive upscale into an offscreen
//!   texture, then RCAS, a contrast-adaptive sharpen onto the screen.
//! - FSRCNNX (see `fsrcnnx`) doubles the frame first, from 1.3× up; FSR 1 then covers any scale beyond 2×.
//!
//! Otherwise it's a plain bilinear blit. Frames are uploaded as they come from the decoder, skipping egui's
//! image conversion.

mod fsrcnnx;

use crate::settings::Upscaler;
use eframe::egui::{self, Rect};
use eframe::egui_glow;
use eframe::glow::{self, HasContext};
use std::sync::{Arc, Mutex};

/// RCAS strength in stops (0 = strongest); AMD's suggested default.
const SHARPNESS_STOPS: f32 = 0.2;
/// FSRCNNX takes over from FSR 1 above this scale, as in its mpv shader: below it, doubling and then
/// scaling back down would lose more than the network adds.
const CNN_MIN_SCALE: f32 = 1.3;

#[derive(Clone, Default)]
pub struct VideoView {
    shared: Arc<Mutex<Shared>>,
}

#[derive(Default)]
struct Shared {
    /// A frame waiting to be uploaded on the next paint.
    pending: Option<(u32, u32, Vec<u8>)>,
    /// Size of the uploaded frame, once there is one.
    size: Option<[u32; 2]>,
    gl: Option<Gl>,
    failed: bool,
}

struct Program {
    program: glow::Program,
    uniforms: Vec<Option<glow::UniformLocation>>,
}

struct Gl {
    vao: glow::VertexArray,
    frame: glow::Texture,
    frame_size: [i32; 2],
    /// Counts uploads, so the network runs once per frame rather than once per paint.
    frame_gen: u64,
    blit: Program,
    easu: Program,
    rcas: Program,
    /// EASU output, the size of the view.
    fbo: glow::Framebuffer,
    fbo_texture: glow::Texture,
    fbo_size: [i32; 2],
    /// Built on first use; None after it failed on this GPU (FSR 1 is used instead).
    cnn: Option<Option<fsrcnnx::Cnn>>,
}

/// The texture to scale up and the shown part of it, in texels.
struct Source {
    texture: glow::Texture,
    size: [i32; 2],
    x: f32,
    y: f32,
    w: f32,
    h: f32,
}

impl VideoView {
    /// Queues a decoded RGBA frame for display.
    pub fn set_frame(&self, width: u32, height: u32, rgba: Vec<u8>) {
        let mut s = self.shared.lock().unwrap();
        s.size = Some([width, height]);
        s.pending = Some((width, height, rgba));
    }

    pub fn has_frame(&self) -> bool {
        self.shared.lock().unwrap().size.is_some()
    }

    /// The shaders didn't compile or link on this GPU; the caller should draw frames another way.
    pub fn failed(&self) -> bool {
        self.shared.lock().unwrap().failed
    }

    /// Draws the `uv` part of the frame (0–1, top-left origin) into `rect`.
    pub fn paint(&self, painter: &egui::Painter, rect: Rect, uv: Rect, upscaler: Upscaler) {
        let shared = self.shared.clone();
        let callback = egui_glow::CallbackFn::new(move |info, p| {
            let gl = p.gl();
            let mut s = shared.lock().unwrap();
            if s.failed {
                return;
            }
            if s.gl.is_none() {
                match unsafe { Gl::new(gl) } {
                    Ok(g) => s.gl = Some(g),
                    Err(e) => {
                        eprintln!("video view: {e}");
                        s.failed = true;
                        return;
                    }
                }
            }
            let pending = s.pending.take();
            let g = s.gl.as_mut().unwrap();
            unsafe {
                if let Some((w, h, rgba)) = pending {
                    g.upload(gl, w as i32, h as i32, &rgba);
                }
                g.draw(gl, &info, uv, upscaler);
            }
        });
        painter.add(egui::PaintCallback { rect, callback: Arc::new(callback) });
    }
}

impl Gl {
    unsafe fn new(gl: &glow::Context) -> Result<Self, String> {
        unsafe {
            let blit = program(gl, BLIT_FS, &["u_tex", "u_uv"])?;
            let easu = program(gl, EASU_FS, &["u_tex", "u_in_origin", "u_in_size", "u_lo", "u_hi"])?;
            let rcas = program(gl, RCAS_FS, &["u_tex", "u_origin", "u_max", "u_sharp"])?;
            let vao = gl.create_vertex_array()?;
            let frame = texture(gl)?;
            let fbo_texture = texture(gl)?;
            let fbo = gl.create_framebuffer()?;
            Ok(Self { vao, frame, frame_size: [0, 0], frame_gen: 0, blit, easu, rcas, fbo, fbo_texture, fbo_size: [0, 0], cnn: None })
        }
    }

    unsafe fn upload(&mut self, gl: &glow::Context, w: i32, h: i32, rgba: &[u8]) {
        if rgba.len() != (w * h * 4) as usize {
            return;
        }
        unsafe {
            gl.bind_texture(glow::TEXTURE_2D, Some(self.frame));
            gl.pixel_store_i32(glow::UNPACK_ALIGNMENT, 4);
            gl.pixel_store_i32(glow::UNPACK_ROW_LENGTH, 0);
            let data = glow::PixelUnpackData::Slice(Some(rgba));
            if self.frame_size == [w, h] {
                gl.tex_sub_image_2d(glow::TEXTURE_2D, 0, 0, 0, w, h, glow::RGBA, glow::UNSIGNED_BYTE, data);
            } else {
                gl.tex_image_2d(glow::TEXTURE_2D, 0, glow::RGBA8 as i32, w, h, 0, glow::RGBA, glow::UNSIGNED_BYTE, data);
                self.frame_size = [w, h];
            }
        }
        self.frame_gen += 1;
    }

    unsafe fn draw(&mut self, gl: &glow::Context, info: &egui::PaintCallbackInfo, uv: Rect, upscaler: Upscaler) {
        let [fw, fh] = self.frame_size;
        if fw == 0 {
            return;
        }
        let vp = info.viewport_in_pixels();
        let (out_w, out_h) = (vp.width_px, vp.height_px);
        // The shown part of the frame, in texels.
        let mut src = Source {
            texture: self.frame,
            size: self.frame_size,
            x: uv.min.x * fw as f32,
            y: uv.min.y * fh as f32,
            w: uv.width() * fw as f32,
            h: uv.height() * fh as f32,
        };
        let scale = (out_w as f32 / src.w).min(out_h as f32 / src.h);
        unsafe {
            gl.disable(glow::BLEND); // video is opaque
            gl.bind_vertex_array(Some(self.vao));
            gl.active_texture(glow::TEXTURE0);
            if upscaler == Upscaler::Fsrcnnx
                && scale > CNN_MIN_SCALE
                && let Some(doubled) = self.fsrcnnx(gl, &src)
            {
                src = doubled;
            }
            let upscaling = out_w as f32 > src.w * 1.02 || out_h as f32 > src.h * 1.02;
            if upscaler != Upscaler::Bilinear && upscaling {
                self.easu(gl, &src, out_w, out_h);
                self.restore(gl, info);
                gl.bind_texture(glow::TEXTURE_2D, Some(self.fbo_texture));
                let u = &self.rcas.uniforms;
                gl.use_program(Some(self.rcas.program));
                gl.uniform_1_i32(u[0].as_ref(), 0);
                gl.uniform_2_f32(u[1].as_ref(), vp.left_px as f32, vp.from_bottom_px as f32);
                gl.uniform_2_i32(u[2].as_ref(), out_w - 1, out_h - 1);
                gl.uniform_1_f32(u[3].as_ref(), (-SHARPNESS_STOPS).exp2());
            } else {
                self.restore(gl, info);
                gl.bind_texture(glow::TEXTURE_2D, Some(src.texture));
                let u = &self.blit.uniforms;
                gl.use_program(Some(self.blit.program));
                gl.uniform_1_i32(u[0].as_ref(), 0);
                let [sw, sh] = src.size.map(|v| v as f32);
                gl.uniform_4_f32(u[1].as_ref(), src.x / sw, src.y / sh, (src.x + src.w) / sw, (src.y + src.h) / sh);
            }
            gl.draw_arrays(glow::TRIANGLES, 0, 3);
            gl.bind_vertex_array(None);
            gl.bind_texture(glow::TEXTURE_2D, None);
        }
    }

    /// Runs FSRCNNX on the whole texels around `src`; its 2× output becomes the new source.
    unsafe fn fsrcnnx(&mut self, gl: &glow::Context, src: &Source) -> Option<Source> {
        let cnn = self.cnn.get_or_insert_with(|| match unsafe { fsrcnnx::Cnn::new(gl) } {
            Ok(c) => Some(c),
            Err(e) => {
                eprintln!("video view: {e}");
                None
            }
        });
        let c = cnn.as_mut()?;
        let (x, y) = (src.x.floor() as i32, src.y.floor() as i32);
        let r = [x, y, ((src.x + src.w).ceil() as i32).min(src.size[0]) - x, ((src.y + src.h).ceil() as i32).min(src.size[1]) - y];
        if let Err(e) = unsafe { c.run(gl, src.texture, src.size, self.frame_gen, r) } {
            eprintln!("video view: {e}");
            self.cnn = Some(None);
            return None;
        }
        let (dx, dy) = (src.x - x as f32, src.y - y as f32);
        Some(Source { texture: c.output, size: [r[2] * 2, r[3] * 2], x: dx * 2.0, y: dy * 2.0, w: src.w * 2.0, h: src.h * 2.0 })
    }

    /// EASU from `src` into the offscreen texture, at the view's size.
    unsafe fn easu(&mut self, gl: &glow::Context, src: &Source, out_w: i32, out_h: i32) {
        unsafe {
            if self.fbo_size != [out_w, out_h] {
                gl.bind_texture(glow::TEXTURE_2D, Some(self.fbo_texture));
                gl.tex_image_2d(glow::TEXTURE_2D, 0, glow::RGBA8 as i32, out_w, out_h, 0, glow::RGBA, glow::UNSIGNED_BYTE, glow::PixelUnpackData::Slice(None));
                gl.bind_framebuffer(glow::FRAMEBUFFER, Some(self.fbo));
                gl.framebuffer_texture_2d(glow::FRAMEBUFFER, glow::COLOR_ATTACHMENT0, glow::TEXTURE_2D, Some(self.fbo_texture), 0);
                self.fbo_size = [out_w, out_h];
            }
            gl.bind_framebuffer(glow::FRAMEBUFFER, Some(self.fbo));
            gl.disable(glow::SCISSOR_TEST);
            gl.viewport(0, 0, out_w, out_h);
            gl.bind_texture(glow::TEXTURE_2D, Some(src.texture));
            let u = &self.easu.uniforms;
            gl.use_program(Some(self.easu.program));
            gl.uniform_1_i32(u[0].as_ref(), 0);
            gl.uniform_2_f32(u[1].as_ref(), src.x, src.y);
            gl.uniform_2_f32(u[2].as_ref(), src.w, src.h);
            // Taps stay inside the shown part, so cropped-out pixels don't bleed in at its edges.
            let [sw, sh] = src.size;
            gl.uniform_2_i32(u[3].as_ref(), src.x.floor() as i32, src.y.floor() as i32);
            gl.uniform_2_i32(u[4].as_ref(), ((src.x + src.w).ceil() as i32 - 1).min(sw - 1), ((src.y + src.h).ceil() as i32 - 1).min(sh - 1));
            gl.draw_arrays(glow::TRIANGLES, 0, 3);
        }
    }

    /// Back onto egui's framebuffer, in the callback's viewport and clip.
    unsafe fn restore(&self, gl: &glow::Context, info: &egui::PaintCallbackInfo) {
        let vp = info.viewport_in_pixels();
        let clip = info.clip_rect_in_pixels();
        unsafe {
            gl.bind_framebuffer(glow::FRAMEBUFFER, None);
            gl.viewport(vp.left_px, vp.from_bottom_px, vp.width_px, vp.height_px);
            gl.enable(glow::SCISSOR_TEST);
            gl.scissor(clip.left_px, clip.from_bottom_px, clip.width_px, clip.height_px);
        }
    }
}

unsafe fn texture(gl: &glow::Context) -> Result<glow::Texture, String> {
    unsafe {
        let t = gl.create_texture()?;
        gl.bind_texture(glow::TEXTURE_2D, Some(t));
        // No mipmaps: the default minification filter would leave the texture incomplete (and black).
        gl.tex_parameter_i32(glow::TEXTURE_2D, glow::TEXTURE_MIN_FILTER, glow::LINEAR as i32);
        gl.tex_parameter_i32(glow::TEXTURE_2D, glow::TEXTURE_MAG_FILTER, glow::LINEAR as i32);
        gl.tex_parameter_i32(glow::TEXTURE_2D, glow::TEXTURE_WRAP_S, glow::CLAMP_TO_EDGE as i32);
        gl.tex_parameter_i32(glow::TEXTURE_2D, glow::TEXTURE_WRAP_T, glow::CLAMP_TO_EDGE as i32);
        Ok(t)
    }
}

unsafe fn program(gl: &glow::Context, fs: &str, uniforms: &[&str]) -> Result<Program, String> {
    unsafe {
        let program = gl.create_program()?;
        let mut shaders = Vec::new();
        for (kind, src) in [(glow::VERTEX_SHADER, VS), (glow::FRAGMENT_SHADER, fs)] {
            let shader = gl.create_shader(kind)?;
            gl.shader_source(shader, &format!("#version 140\n{src}"));
            gl.compile_shader(shader);
            if !gl.get_shader_compile_status(shader) {
                return Err(gl.get_shader_info_log(shader));
            }
            gl.attach_shader(program, shader);
            shaders.push(shader);
        }
        gl.link_program(program);
        if !gl.get_program_link_status(program) {
            return Err(gl.get_program_info_log(program));
        }
        for s in shaders {
            gl.detach_shader(program, s);
            gl.delete_shader(s);
        }
        let uniforms = uniforms.iter().map(|n| gl.get_uniform_location(program, n)).collect();
        Ok(Program { program, uniforms })
    }
}

/// One triangle covering the viewport; `v_pos` is 0–1 with a top-left origin.
const VS: &str = r#"
out vec2 v_pos;
void main() {
    vec2 p = vec2(float((gl_VertexID << 1) & 2), float(gl_VertexID & 2));
    v_pos = vec2(p.x, 1.0 - p.y);
    gl_Position = vec4(p * 2.0 - 1.0, 0.0, 1.0);
}
"#;

const BLIT_FS: &str = r#"
uniform sampler2D u_tex;
uniform vec4 u_uv;
in vec2 v_pos;
out vec4 o_color;
void main() {
    o_color = vec4(texture(u_tex, mix(u_uv.xy, u_uv.zw, v_pos)).rgb, 1.0);
}
"#;

/// FSR 1 EASU (ffx_fsr1.h `FsrEasuF`), with texel fetches instead of gathers. The 12 taps around the
/// output pixel's 2×2 source quad (f g / j k):
///     b c
///   e f g h
///   i j k l
///     n o
const EASU_FS: &str = r#"
uniform sampler2D u_tex;
uniform vec2 u_in_origin;
uniform vec2 u_in_size;
uniform ivec2 u_lo;
uniform ivec2 u_hi;
in vec2 v_pos;
out vec4 o_color;

vec3 tap(ivec2 base, int x, int y) {
    return texelFetch(u_tex, clamp(base + ivec2(x, y), u_lo, u_hi), 0).rgb;
}

float luma(vec3 c) { return c.g + 0.5 * (c.r + c.b); }

// Accumulates direction and edge length for one quad corner, weighted by its bilinear weight w.
// A above, B left, C center, D right, E below.
void setF(inout vec2 dir, inout float len, float w, float lA, float lB, float lC, float lD, float lE) {
    float dirX = lD - lB;
    float lenX = clamp(abs(dirX) / max(max(abs(lD - lC), abs(lC - lB)), 1.0 / 65536.0), 0.0, 1.0);
    dir.x += dirX * w;
    len += lenX * lenX * w;
    float dirY = lE - lA;
    float lenY = clamp(abs(dirY) / max(max(abs(lE - lC), abs(lC - lA)), 1.0 / 65536.0), 0.0, 1.0);
    dir.y += dirY * w;
    len += lenY * lenY * w;
}

// One tap of the approximated, direction-stretched lanczos2 kernel.
void tapF(inout vec3 aC, inout float aW, vec2 off, vec2 dir, vec2 len, float lob, float clp, vec3 c) {
    vec2 v = vec2(off.x * dir.x + off.y * dir.y, -off.x * dir.y + off.y * dir.x) * len;
    float d2 = min(dot(v, v), clp);
    float wB = 0.4 * d2 - 1.0;
    float wA = lob * d2 - 1.0;
    wB *= wB;
    wA *= wA;
    wB = 1.5625 * wB - 0.5625;
    float w = wB * wA;
    aC += c * w;
    aW += w;
}

void main() {
    // Source position of this output pixel's center, relative to texel centers.
    vec2 pp = v_pos * u_in_size + u_in_origin - 0.5;
    vec2 fp = floor(pp);
    pp -= fp;
    ivec2 q = ivec2(fp);
    vec3 b = tap(q, 0, -1), c = tap(q, 1, -1);
    vec3 e = tap(q, -1, 0), f = tap(q, 0, 0), g = tap(q, 1, 0), h = tap(q, 2, 0);
    vec3 i = tap(q, -1, 1), j = tap(q, 0, 1), k = tap(q, 1, 1), l = tap(q, 2, 1);
    vec3 n = tap(q, 0, 2), o = tap(q, 1, 2);
    float bL = luma(b), cL = luma(c), eL = luma(e), fL = luma(f), gL = luma(g), hL = luma(h);
    float iL = luma(i), jL = luma(j), kL = luma(k), lL = luma(l), nL = luma(n), oL = luma(o);

    vec2 dir = vec2(0.0);
    float len = 0.0;
    setF(dir, len, (1.0 - pp.x) * (1.0 - pp.y), bL, eL, fL, gL, jL);
    setF(dir, len, pp.x * (1.0 - pp.y), cL, fL, gL, hL, kL);
    setF(dir, len, (1.0 - pp.x) * pp.y, fL, iL, jL, kL, nL);
    setF(dir, len, pp.x * pp.y, gL, jL, kL, lL, oL);

    // Normalize the direction (no edge: along x).
    float dirR = dot(dir, dir);
    bool zro = dirR < 1.0 / 32768.0;
    dir = zro ? vec2(1.0, 0.0) : dir * inversesqrt(dirR);
    // Edge strength shapes the kernel: stretched along the edge, narrower and with less negative lobe across.
    len = len * 0.5;
    len *= len;
    float stretch = dot(dir, dir) / max(abs(dir.x), abs(dir.y));
    vec2 len2 = vec2(1.0 + (stretch - 1.0) * len, 1.0 - 0.5 * len);
    float lob = 0.5 + ((1.0 / 4.0 - 0.04) - 0.5) * len;
    float clp = 1.0 / lob;

    vec3 aC = vec3(0.0);
    float aW = 0.0;
    tapF(aC, aW, vec2(0.0, -1.0) - pp, dir, len2, lob, clp, b);
    tapF(aC, aW, vec2(1.0, -1.0) - pp, dir, len2, lob, clp, c);
    tapF(aC, aW, vec2(-1.0, 1.0) - pp, dir, len2, lob, clp, i);
    tapF(aC, aW, vec2(0.0, 1.0) - pp, dir, len2, lob, clp, j);
    tapF(aC, aW, vec2(0.0, 0.0) - pp, dir, len2, lob, clp, f);
    tapF(aC, aW, vec2(-1.0, 0.0) - pp, dir, len2, lob, clp, e);
    tapF(aC, aW, vec2(1.0, 1.0) - pp, dir, len2, lob, clp, k);
    tapF(aC, aW, vec2(2.0, 1.0) - pp, dir, len2, lob, clp, l);
    tapF(aC, aW, vec2(2.0, 0.0) - pp, dir, len2, lob, clp, h);
    tapF(aC, aW, vec2(1.0, 0.0) - pp, dir, len2, lob, clp, g);
    tapF(aC, aW, vec2(1.0, 2.0) - pp, dir, len2, lob, clp, o);
    tapF(aC, aW, vec2(0.0, 2.0) - pp, dir, len2, lob, clp, n);

    // Deringing: stay within the 2x2 quad's range.
    vec3 mn = min(min(f, g), min(j, k));
    vec3 mx = max(max(f, g), max(j, k));
    o_color = vec4(min(mx, max(mn, aC / aW)), 1.0);
}
"#;

/// FSR 1 RCAS (ffx_fsr1.h `FsrRcasF`, without the optional denoise): sharpens with the largest negative
/// lobe that doesn't clip the 5-tap cross
///     b
///   d e f
///     h
const RCAS_FS: &str = r#"
uniform sampler2D u_tex;
uniform vec2 u_origin;
uniform ivec2 u_max;
uniform float u_sharp;
out vec4 o_color;

vec3 at(ivec2 p) { return texelFetch(u_tex, clamp(p, ivec2(0), u_max), 0).rgb; }

void main() {
    ivec2 p = ivec2(gl_FragCoord.xy - u_origin);
    vec3 b = at(p + ivec2(0, 1)), d = at(p + ivec2(-1, 0)), e = at(p), f = at(p + ivec2(1, 0)), h = at(p + ivec2(0, -1));
    vec3 mn4 = min(min(b, d), min(f, h));
    vec3 mx4 = max(max(b, d), max(f, h));
    vec3 hitMin = min(mn4, e) / max(4.0 * mx4, vec3(1.0 / 65536.0));
    vec3 hitMax = (1.0 - max(mx4, e)) / min(4.0 * mn4 - 4.0, vec3(-1.0 / 65536.0));
    vec3 lobeRGB = max(-hitMin, hitMax);
    float lobe = max(-0.1875, min(max(lobeRGB.r, max(lobeRGB.g, lobeRGB.b)), 0.0)) * u_sharp;
    o_color = vec4((lobe * (b + d + f + h) + e) / (4.0 * lobe + 1.0), 1.0);
}
"#;
