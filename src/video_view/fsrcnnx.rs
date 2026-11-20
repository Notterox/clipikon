//! FSRCNNX (igv, LGPL-3.0-or-later): a small convolutional network that doubles the frame's luma. It runs
//! from its unmodified mpv user-shader file, so only the parts of mpv's hook format that file uses are
//! supported: chained passes that BIND and SAVE textures, with WIDTH/HEIGHT as multiples of the luma.
//!
//! Chroma isn't part of the network: the colour comes from a bilinear upscale, shifted to the network's luma.

use super::{Program, program, texture};
use eframe::glow::{self, HasContext};
use std::collections::HashMap;

const SOURCE: &str = include_str!("../shaders/FSRCNNX_x2_8-0-4-1.glsl");

/// The shown part of the frame in whole texels: x, y, width, height.
pub type Region = [i32; 4];

struct Pass {
    program: glow::Program,
    out_size: Option<glow::UniformLocation>,
    /// Bound slot, its sampler and size uniforms.
    binds: Vec<(usize, Option<glow::UniformLocation>, Option<glow::UniformLocation>)>,
    out: usize,
}

pub struct Cnn {
    luma: Program,
    passes: Vec<Pass>,
    compose: Program,
    /// Intermediate textures (RGBA16F); slot 0 is the input luma. Sizes are `scale` × the region.
    slots: Vec<glow::Texture>,
    scale: Vec<[i32; 2]>,
    /// The network's output slot, at 2× the region.
    result: usize,
    /// The final colour frame at 2× the region (RGBA8).
    pub output: glow::Texture,
    fbo: glow::Framebuffer,
    size: [i32; 2],
    /// What `output` currently holds: frame generation and region.
    done: Option<(u64, Region)>,
}

impl Cnn {
    pub unsafe fn new(gl: &glow::Context) -> Result<Self, String> {
        let descs = parse(SOURCE)?;
        unsafe {
            let luma = program(gl, LUMA_FS, &["u_tex", "u_origin"])?;
            let compose = program(gl, COMPOSE_FS, &["u_frame", "u_luma", "u_frame_size", "u_origin", "u_size", "u_out_size"])?;
            // Slots: a pass writes over an earlier texture of the same name unless it's reading it.
            let mut names: HashMap<String, usize> = HashMap::from([("LUMA".to_string(), 0)]);
            let mut scale = vec![[1, 1]];
            let mut passes = Vec::new();
            for d in &descs {
                let mut uniforms = vec!["u_out_size".to_string()];
                let mut bound = Vec::new();
                for (glsl, name) in &d.binds {
                    bound.push(*names.get(name).ok_or_else(|| format!("FSRCNNX: unknown texture {name}"))?);
                    uniforms.push(format!("{glsl}_raw"));
                    uniforms.push(format!("{glsl}_size"));
                }
                let name = d.save.clone().unwrap_or_else(|| d.hook.clone());
                let out = match names.get(&name) {
                    Some(&s) if s != 0 && !bound.contains(&s) && scale[s] == d.scale => s,
                    _ => {
                        scale.push(d.scale);
                        scale.len() - 1
                    }
                };
                names.insert(name, out);
                let refs: Vec<&str> = uniforms.iter().map(String::as_str).collect();
                let p = program(gl, &pass_source(d), &refs)?;
                let mut u = p.uniforms.into_iter();
                let out_size = u.next().flatten();
                let binds = bound.into_iter().map(|s| (s, u.next().flatten(), u.next().flatten())).collect();
                passes.push(Pass { program: p.program, out_size, binds, out });
            }
            let result = names["LUMA"];
            if scale[result] != [2, 2] {
                return Err("FSRCNNX: the shader doesn't double the luma".into());
            }
            let mut slots = Vec::new();
            for _ in &scale {
                slots.push(texture(gl)?);
            }
            let output = texture(gl)?;
            let fbo = gl.create_framebuffer()?;
            Ok(Self { luma, passes, compose, slots, scale, result, output, fbo, size: [0, 0], done: None })
        }
    }

    /// Doubles `region` of `frame` into `output`, unless it already holds it. The caller restores the
    /// framebuffer, viewport and program afterwards.
    pub unsafe fn run(&mut self, gl: &glow::Context, frame: glow::Texture, frame_size: [i32; 2], frame_gen: u64, r: Region) -> Result<(), String> {
        if self.done == Some((frame_gen, r)) {
            return Ok(());
        }
        let [_, _, w, h] = r;
        unsafe {
            gl.bind_framebuffer(glow::FRAMEBUFFER, Some(self.fbo));
            gl.disable(glow::SCISSOR_TEST);
            if self.size != [w, h] {
                for (t, s) in self.slots.iter().zip(&self.scale) {
                    gl.bind_texture(glow::TEXTURE_2D, Some(*t));
                    gl.tex_image_2d(glow::TEXTURE_2D, 0, glow::RGBA16F as i32, w * s[0], h * s[1], 0, glow::RGBA, glow::HALF_FLOAT, glow::PixelUnpackData::Slice(None));
                }
                gl.bind_texture(glow::TEXTURE_2D, Some(self.output));
                gl.tex_image_2d(glow::TEXTURE_2D, 0, glow::RGBA8 as i32, w * 2, h * 2, 0, glow::RGBA, glow::UNSIGNED_BYTE, glow::PixelUnpackData::Slice(None));
                self.target(gl, self.slots[0]);
                if gl.check_framebuffer_status(glow::FRAMEBUFFER) != glow::FRAMEBUFFER_COMPLETE {
                    return Err("FSRCNNX: can't render to half-float textures".into());
                }
                self.size = [w, h];
            }

            self.target(gl, self.slots[0]);
            gl.viewport(0, 0, w, h);
            gl.active_texture(glow::TEXTURE0);
            gl.bind_texture(glow::TEXTURE_2D, Some(frame));
            gl.use_program(Some(self.luma.program));
            let u = &self.luma.uniforms;
            gl.uniform_1_i32(u[0].as_ref(), 0);
            gl.uniform_2_i32(u[1].as_ref(), r[0], r[1]);
            gl.draw_arrays(glow::TRIANGLES, 0, 3);

            for p in &self.passes {
                let [sx, sy] = self.scale[p.out];
                self.target(gl, self.slots[p.out]);
                gl.viewport(0, 0, w * sx, h * sy);
                gl.use_program(Some(p.program));
                gl.uniform_2_f32(p.out_size.as_ref(), (w * sx) as f32, (h * sy) as f32);
                for (unit, (slot, sampler, size)) in p.binds.iter().enumerate() {
                    gl.active_texture(glow::TEXTURE0 + unit as u32);
                    gl.bind_texture(glow::TEXTURE_2D, Some(self.slots[*slot]));
                    gl.uniform_1_i32(sampler.as_ref(), unit as i32);
                    let [bx, by] = self.scale[*slot];
                    gl.uniform_2_f32(size.as_ref(), (w * bx) as f32, (h * by) as f32);
                }
                gl.draw_arrays(glow::TRIANGLES, 0, 3);
            }

            self.target(gl, self.output);
            gl.viewport(0, 0, w * 2, h * 2);
            gl.active_texture(glow::TEXTURE1);
            gl.bind_texture(glow::TEXTURE_2D, Some(self.slots[self.result]));
            gl.active_texture(glow::TEXTURE0);
            gl.bind_texture(glow::TEXTURE_2D, Some(frame));
            gl.use_program(Some(self.compose.program));
            let u = &self.compose.uniforms;
            gl.uniform_1_i32(u[0].as_ref(), 0);
            gl.uniform_1_i32(u[1].as_ref(), 1);
            gl.uniform_2_f32(u[2].as_ref(), frame_size[0] as f32, frame_size[1] as f32);
            gl.uniform_2_f32(u[3].as_ref(), r[0] as f32, r[1] as f32);
            gl.uniform_2_f32(u[4].as_ref(), w as f32, h as f32);
            gl.uniform_2_f32(u[5].as_ref(), (w * 2) as f32, (h * 2) as f32);
            gl.draw_arrays(glow::TRIANGLES, 0, 3);
            let units = self.passes.iter().map(|p| p.binds.len()).max().unwrap_or(0).max(2);
            for unit in (1..units).rev() {
                gl.active_texture(glow::TEXTURE0 + unit as u32);
                gl.bind_texture(glow::TEXTURE_2D, None);
            }
            gl.active_texture(glow::TEXTURE0);
        }
        self.done = Some((frame_gen, r));
        Ok(())
    }

    unsafe fn target(&self, gl: &glow::Context, t: glow::Texture) {
        unsafe { gl.framebuffer_texture_2d(glow::FRAMEBUFFER, glow::COLOR_ATTACHMENT0, glow::TEXTURE_2D, Some(t), 0) };
    }
}

struct PassDesc {
    hook: String,
    /// GLSL prefix and texture name: they differ only for HOOKED, the hooked texture.
    binds: Vec<(String, String)>,
    save: Option<String>,
    scale: [i32; 2],
    body: String,
}

fn parse(src: &str) -> Result<Vec<PassDesc>, String> {
    let mut passes: Vec<PassDesc> = Vec::new();
    let mut in_body = false;
    for line in src.lines() {
        let Some(directive) = line.strip_prefix("//!") else {
            if let Some(p) = passes.last_mut() {
                in_body = true;
                p.body.push_str(line);
                p.body.push('\n');
            }
            continue;
        };
        if in_body || passes.is_empty() {
            passes.push(PassDesc { hook: String::new(), binds: Vec::new(), save: None, scale: [1, 1], body: String::new() });
            in_body = false;
        }
        let p = passes.last_mut().unwrap();
        let (key, value) = directive.split_once(' ').unwrap_or((directive, ""));
        let value = value.trim();
        match key {
            "HOOK" => p.hook = value.into(),
            "BIND" => p.binds.push((value.into(), value.into())),
            "SAVE" => p.save = Some(value.into()),
            "WIDTH" => p.scale[0] = multiple(value)?,
            "HEIGHT" => p.scale[1] = multiple(value)?,
            "DESC" | "COMPONENTS" | "WHEN" => {} // the caller decides when to upscale
            _ => return Err(format!("FSRCNNX: unsupported directive {key}")),
        }
    }
    for p in &mut passes {
        if p.hook != "LUMA" {
            return Err(format!("FSRCNNX: unsupported hook {}", p.hook));
        }
        if p.body.contains("HOOKED_") {
            p.binds.push(("HOOKED".into(), p.hook.clone()));
        }
    }
    Ok(passes)
}

/// Evaluates a WIDTH/HEIGHT expression (reverse Polish, e.g. `LUMA.w 2 *`) as a multiple of the luma size.
fn multiple(expr: &str) -> Result<i32, String> {
    let mut stack: Vec<f32> = Vec::new();
    for t in expr.split_whitespace() {
        let v = match t {
            "+" | "-" | "*" | "/" => {
                let (Some(b), Some(a)) = (stack.pop(), stack.pop()) else { return Err(format!("FSRCNNX: bad size {expr}")) };
                match t {
                    "+" => a + b,
                    "-" => a - b,
                    "*" => a * b,
                    _ => a / b,
                }
            }
            "LUMA.w" | "LUMA.h" | "HOOKED.w" | "HOOKED.h" => 1.0,
            n => n.parse().map_err(|_| format!("FSRCNNX: bad size {expr}"))?,
        };
        stack.push(v);
    }
    match stack[..] {
        [v] if v >= 1.0 && v.fract() == 0.0 => Ok(v as i32),
        _ => Err(format!("FSRCNNX: bad size {expr}")),
    }
}

/// A pass's fragment shader: mpv's texture macros, then the pass's `hook()`.
fn pass_source(d: &PassDesc) -> String {
    let mut s = String::from("uniform vec2 u_out_size;\nout vec4 o_color;\n");
    for (n, _) in &d.binds {
        s += &format!(
            "uniform sampler2D {n}_raw;\nuniform vec2 {n}_size;\n\
             #define {n}_pt (1.0 / {n}_size)\n#define {n}_pos (gl_FragCoord.xy / u_out_size)\n#define {n}_mul 1.0\n\
             #define {n}_tex(pos) texture({n}_raw, pos)\n#define {n}_texOff(off) {n}_tex({n}_pos + {n}_pt * vec2(off))\n"
        );
    }
    s += &d.body;
    s += "void main() { o_color = hook(); }\n";
    s
}

/// Luma (BT.709 weights) of the region, which starts at `u_origin` in the frame.
const LUMA_FS: &str = r#"
uniform sampler2D u_tex;
uniform ivec2 u_origin;
out vec4 o_color;
void main() {
    vec3 c = texelFetch(u_tex, u_origin + ivec2(gl_FragCoord.xy), 0).rgb;
    o_color = vec4(dot(c, vec3(0.2126, 0.7152, 0.0722)), 0.0, 0.0, 1.0);
}
"#;

/// Bilinear colour at 2×, shifted so its luma becomes the network's.
const COMPOSE_FS: &str = r#"
uniform sampler2D u_frame;
uniform sampler2D u_luma;
uniform vec2 u_frame_size;
uniform vec2 u_origin;
uniform vec2 u_size;
uniform vec2 u_out_size;
out vec4 o_color;
void main() {
    vec2 px = clamp(u_origin + gl_FragCoord.xy / u_out_size * u_size, u_origin + 0.5, u_origin + u_size - 0.5);
    vec3 c = texture(u_frame, px / u_frame_size).rgb;
    float y = texelFetch(u_luma, ivec2(gl_FragCoord.xy), 0).x;
    o_color = vec4(clamp(c + (y - dot(c, vec3(0.2126, 0.7152, 0.0722))), 0.0, 1.0), 1.0);
}
"#;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_the_shader() {
        let passes = parse(SOURCE).unwrap();
        assert_eq!(passes.len(), 14);
        assert!(passes[..13].iter().all(|p| p.scale == [1, 1] && p.save.is_some()));
        let last = passes.last().unwrap();
        assert_eq!((last.scale, last.save.as_deref()), ([2, 2], None));
        assert!(pass_source(last).contains("#define SUBCONV1_texOff"));
    }

    #[test]
    fn size_expressions() {
        assert_eq!(multiple("LUMA.w 2 *"), Ok(2));
        assert_eq!(multiple("HOOKED.h"), Ok(1));
        assert!(multiple("OUTPUT.w").is_err());
    }
}
