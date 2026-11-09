// SPDX-License-Identifier: MIT
//! The ES1 scene of `xtask test gl` (mode es1), read from `xtask/testdata/gl-probe/es1-scene.toml`.
//! [`Scene::program`] gives the probe's lines: Es1Scene.java and jni/es1.rs run the same steps through GL10 and through libGLESv1_CM.so.
//! [`Scene::judge`] compares a probe readback with [`Scene::expect`], a reference model written from ES 1.1 and the scene's own numbers, never from driver output.
//! Edge pixels (the 1 px band around every primitive) are excluded; interiors and exteriors compare exactly; blended pixels allow 1 LSB.
//! The model handles only what the scene uses (axis-aligned quads and points, flat or per-vertex colour, 2x2 NEAREST texture) and fails on anything else.

use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;

use anyhow::{Context, Result, bail, ensure};
use serde::Deserialize;

#[derive(Deserialize)]
pub struct Scene {
    surface: Surface,
    hj: Hj,
    step: Vec<Step>,
}

#[derive(Deserialize)]
struct Surface {
    width: usize,
    height: usize,
}

#[derive(Deserialize)]
struct Hj {
    names: Vec<String>,
}

#[derive(Deserialize)]
struct Step {
    text: String,
    /// Dropped from the broken run (toml key `break`).
    #[serde(default, rename = "break")]
    breakable: bool,
}

/// One expected pixel: colour, allowed error per channel (LSB), and whether it lies on an edge (excluded from the compare).
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Pixel {
    pub rgba: [u8; 4],
    pub tolerance: u8,
    pub edge: bool,
}

/// The verdict on one probe readback.
#[derive(Debug, PartialEq)]
pub enum Judged {
    /// Every non-edge pixel within tolerance; the number compared.
    Match(usize),
    /// Rows present, some pixel out of tolerance: the count and the first few.
    Mismatch(String),
    /// Rows absent or malformed: the probe produced no readback.
    Missing(String),
}

impl Scene {
    pub fn load(path: &Path) -> Result<Self> {
        let text =
            std::fs::read_to_string(path).with_context(|| format!("reading {}", path.display()))?;
        toml::from_str(&text).with_context(|| format!("parsing {}", path.display()))
    }

    pub fn size(&self) -> (usize, usize) {
        (self.surface.width, self.surface.height)
    }

    /// The probe's program: every step (without the breakable ones when `broken`), then `resolve` of the Happy Jump names.
    pub fn program(&self, broken: bool) -> Vec<String> {
        let mut lines: Vec<String> = self
            .step
            .iter()
            .filter(|s| !(broken && s.breakable))
            .map(|s| s.text.clone())
            .collect();
        lines.push(format!("resolve {}", self.hj.names.join(" ")));
        lines
    }

    /// The Happy Jump GLES1 names check 6 resolves.
    pub fn names(&self) -> &[String] {
        &self.hj.names
    }

    /// The expectation, row-major with row 0 first (GL window order, y up). Always the unbroken scene.
    pub fn expect(&self) -> Result<Vec<Pixel>> {
        let (w, h) = self.size();
        let mut m = Machine::new(w, h);
        for s in &self.step {
            m.step(&s.text)
                .with_context(|| format!("scene step `{}`", s.text))?;
        }
        Ok(m.into_pixels())
    }

    /// Compares a probe's rows (`y` -> RGBA hex, 8 digits per pixel, row 0 first) with the expectation.
    pub fn judge(&self, rows: &BTreeMap<usize, String>) -> Result<Judged> {
        let (w, h) = self.size();
        let expect = self.expect()?;
        let mut compared = 0;
        let mut bad = Vec::new();
        for y in 0..h {
            let Some(hex) = rows.get(&y) else {
                return Ok(Judged::Missing(format!("{} of {h} rows", rows.len())));
            };
            let Some(bytes) = unhex(hex).filter(|b| b.len() == 4 * w) else {
                return Ok(Judged::Missing(format!("row {y} is not {w} RGBA pixels")));
            };
            for x in 0..w {
                let e = expect[y * w + x];
                if e.edge {
                    continue;
                }
                compared += 1;
                let got: [u8; 4] = std::array::from_fn(|k| bytes[4 * x + k]);
                if got
                    .iter()
                    .zip(e.rgba)
                    .any(|(g, want)| g.abs_diff(want) > e.tolerance)
                {
                    bad.push(format!("({x},{y}) got {got:?} want {:?}", e.rgba));
                }
            }
        }
        if bad.is_empty() {
            return Ok(Judged::Match(compared));
        }
        let first: Vec<&str> = bad.iter().take(4).map(String::as_str).collect();
        Ok(Judged::Mismatch(format!(
            "{} of {compared} pixels differ; first: {}",
            bad.len(),
            first.join("; ")
        )))
    }
}

fn unhex(s: &str) -> Option<Vec<u8>> {
    if !s.len().is_multiple_of(2) {
        return None;
    }
    (0..s.len())
        .step_by(2)
        .map(|i| u8::from_str_radix(s.get(i..i + 2)?, 16).ok())
        .collect()
}

/// Column-major is irrelevant here: m[row][col], w = 1 for every matrix used (affine, or ortho).
type Mat = [[f64; 4]; 4];

const ID: Mat = [
    [1.0, 0.0, 0.0, 0.0],
    [0.0, 1.0, 0.0, 0.0],
    [0.0, 0.0, 1.0, 0.0],
    [0.0, 0.0, 0.0, 1.0],
];

fn mul(a: &Mat, b: &Mat) -> Mat {
    std::array::from_fn(|r| std::array::from_fn(|c| (0..4).map(|k| a[r][k] * b[k][c]).sum()))
}

fn translate(x: f64, y: f64, z: f64) -> Mat {
    let mut m = ID;
    m[0][3] = x;
    m[1][3] = y;
    m[2][3] = z;
    m
}

fn scale(x: f64, y: f64, z: f64) -> Mat {
    let mut m = ID;
    m[0][0] = x;
    m[1][1] = y;
    m[2][2] = z;
    m
}

/// glRotatef (ES 1.1 §5.6): rotation by `deg` about the axis (x, y, z).
fn rotate(deg: f64, x: f64, y: f64, z: f64) -> Mat {
    let len = (x * x + y * y + z * z).sqrt();
    let (x, y, z) = (x / len, y / len, z / len);
    let (s, c) = deg.to_radians().sin_cos();
    let t = 1.0 - c;
    [
        [x * x * t + c, x * y * t - z * s, x * z * t + y * s, 0.0],
        [y * x * t + z * s, y * y * t + c, y * z * t - x * s, 0.0],
        [x * z * t - y * s, y * z * t + x * s, z * z * t + c, 0.0],
        [0.0, 0.0, 0.0, 1.0],
    ]
}

/// glOrthof (ES 1.1 §2.12.1).
fn ortho(l: f64, r: f64, b: f64, t: f64, n: f64, f: f64) -> Mat {
    [
        [2.0 / (r - l), 0.0, 0.0, -(r + l) / (r - l)],
        [0.0, 2.0 / (t - b), 0.0, -(t + b) / (t - b)],
        [0.0, 0.0, -2.0 / (f - n), -(f + n) / (f - n)],
        [0.0, 0.0, 0.0, 1.0],
    ]
}

/// Applies `m` to the point (x, y, 0, 1); the w row of every matrix here is (0 0 0 1).
fn apply(m: &Mat, x: f64, y: f64) -> (f64, f64) {
    let v = [x, y, 0.0, 1.0];
    let at = |i: usize| (0..4).map(|k| m[i][k] * v[k]).sum::<f64>();
    (at(0), at(1))
}

/// Source-over with GL_SRC_ALPHA, GL_ONE_MINUS_SRC_ALPHA on every channel (alpha included), rounded to unorm8.
fn blend(src: [u8; 4], dst: [u8; 4]) -> [u8; 4] {
    let a = src[3] as f64 / 255.0;
    std::array::from_fn(|i| (src[i] as f64 * a + dst[i] as f64 * (1.0 - a)).round() as u8)
}

/// MODULATE with a texel (ES 1.1 §4.7.1): per-channel product of unorm8 values.
fn modulate(t: [u8; 4], c: [u8; 4]) -> [u8; 4] {
    std::array::from_fn(|i| (t[i] as f64 * c[i] as f64 / 255.0).round() as u8)
}

/// A value that must sit on a pixel edge (integer within 1e-6); the scene's geometry is integer after the matrix stack.
fn int(v: f64) -> Result<i64> {
    ensure!((v - v.round()).abs() < 1e-6, "{v} is not on a pixel edge");
    Ok(v.round() as i64)
}

fn nums(a: &[&str]) -> Result<Vec<f64>> {
    a.iter()
        .map(|s| s.parse::<f64>().with_context(|| format!("number `{s}`")))
        .collect()
}

fn byte(v: f64) -> Result<u8> {
    ensure!(
        (0.0..=255.0).contains(&v) && v.fract() == 0.0,
        "{v} is not a 0-255 channel"
    );
    Ok(v as u8)
}

fn rgba_bytes(c: &[f64]) -> Result<[u8; 4]> {
    ensure!(c.len() == 4, "colour takes 4 channels, got {}", c.len());
    Ok([byte(c[0])?, byte(c[1])?, byte(c[2])?, byte(c[3])?])
}

fn rgba8(a: &[&str]) -> Result<[u8; 4]> {
    rgba_bytes(&nums(a)?)
}

fn one<'a>(a: &[&'a str]) -> Result<&'a str> {
    match a {
        [x] => Ok(*x),
        _ => bail!("expected one name, got {a:?}"),
    }
}

/// Client-array slot: VERTEX 0, COLOR 1, TEXCOORD 2, POINT_SIZE 3.
fn array(name: &str) -> Result<usize> {
    Ok(match name {
        "VERTEX_ARRAY" => 0,
        "COLOR_ARRAY" => 1,
        "TEXTURE_COORD_ARRAY" => 2,
        "POINT_SIZE_ARRAY_OES" => 3,
        _ => bail!("client array {name}"),
    })
}

/// What a fragment is shaded with: the current flat colour, or a 2x2 texture over a rect (corner texcoords, MODULATE with `colour`).
enum Shade {
    Flat([u8; 4]),
    Textured {
        rect: [i64; 4],
        /// (s, t) at the corners (x0,y0), (x1,y0), (x0,y1), (x1,y1).
        st: [[f64; 2]; 4],
        tw: usize,
        th: usize,
        texels: Vec<[u8; 4]>,
        colour: [u8; 4],
    },
}

impl Shade {
    fn at(&self, cx: f64, cy: f64) -> [u8; 4] {
        match self {
            Shade::Flat(c) => *c,
            Shade::Textured {
                rect,
                st,
                tw,
                th,
                texels,
                colour,
            } => {
                let [x0, y0, x1, y1] = *rect;
                let tx = (cx - x0 as f64) / (x1 - x0) as f64;
                let ty = (cy - y0 as f64) / (y1 - y0) as f64;
                let lerp = |k: usize| {
                    let bottom = st[0][k] * (1.0 - tx) + st[1][k] * tx;
                    let top = st[2][k] * (1.0 - tx) + st[3][k] * tx;
                    bottom * (1.0 - ty) + top * ty
                };
                let i = ((lerp(0) * *tw as f64).floor() as i64).clamp(0, *tw as i64 - 1) as usize;
                let j = ((lerp(1) * *th as f64).floor() as i64).clamp(0, *th as i64 - 1) as usize;
                modulate(texels[j * tw + i], *colour)
            }
        }
    }
}

/// The framebuffer model: GL state as the steps set it, and the pixels they write.
struct Machine {
    w: usize,
    h: usize,
    fb: Vec<[u8; 4]>,
    tol: Vec<u8>,
    edge: Vec<bool>,
    scissor_on: bool,
    scissor: [i64; 4],
    blend_on: bool,
    tex_on: bool,
    texels: Option<(usize, usize, Vec<[u8; 4]>)>,
    /// Client-array enables by slot (see [`array`]).
    client: [bool; 4],
    colour: [u8; 4],
    /// GL_PROJECTION when set, else GL_MODELVIEW.
    projection: bool,
    proj: Mat,
    mv: Mat,
    mv_stack: Vec<Mat>,
    verts: Vec<[f64; 2]>,
    colours: Vec<[u8; 4]>,
    texs: Vec<[f64; 2]>,
    psizes: Vec<f64>,
}

impl Machine {
    fn new(w: usize, h: usize) -> Self {
        Machine {
            w,
            h,
            fb: vec![[0; 4]; w * h],
            tol: vec![0; w * h],
            edge: vec![false; w * h],
            scissor_on: false,
            scissor: [0, 0, w as i64, h as i64],
            blend_on: false,
            tex_on: false,
            texels: None,
            client: [false; 4],
            colour: [255; 4],
            projection: false,
            proj: ID,
            mv: ID,
            mv_stack: Vec::new(),
            verts: Vec::new(),
            colours: Vec::new(),
            texs: Vec::new(),
            psizes: Vec::new(),
        }
    }

    fn into_pixels(self) -> Vec<Pixel> {
        (0..self.w * self.h)
            .map(|p| Pixel {
                rgba: self.fb[p],
                tolerance: self.tol[p],
                edge: self.edge[p],
            })
            .collect()
    }

    fn cur(&mut self) -> &mut Mat {
        if self.projection {
            &mut self.proj
        } else {
            &mut self.mv
        }
    }

    /// Post-multiply the current matrix (GL semantics: the new transform applies first to vertices).
    fn post(&mut self, m: Mat) {
        let c = *self.cur();
        *self.cur() = mul(&c, &m);
    }

    /// Window coordinates of a vertex (full viewport, depth ignored).
    fn window(&self, v: [f64; 2]) -> (f64, f64) {
        let (x, y) = apply(&mul(&self.proj, &self.mv), v[0], v[1]);
        (
            (x + 1.0) * self.w as f64 / 2.0,
            (y + 1.0) * self.h as f64 / 2.0,
        )
    }

    fn in_scissor(&self, px: i64, py: i64) -> bool {
        let [sx, sy, sw, sh] = self.scissor;
        !self.scissor_on || (sx <= px && px < sx + sw && sy <= py && py < sy + sh)
    }

    fn step(&mut self, text: &str) -> Result<()> {
        let t: Vec<&str> = text.split_whitespace().collect();
        let Some((op, a)) = t.split_first() else {
            bail!("empty program line");
        };
        match *op {
            "clear" => {
                let c = rgba8(a)?;
                self.clear(c);
            }
            "color" => self.colour = rgba8(a)?,
            "enable" => *self.cap(one(a)?)? = true,
            "disable" => *self.cap(one(a)?)? = false,
            "client_enable" => self.client[array(one(a)?)?] = true,
            "client_disable" => self.client[array(one(a)?)?] = false,
            "scissor" => {
                let v = nums(a)?;
                ensure!(v.len() == 4, "scissor takes 4 numbers");
                self.scissor = [int(v[0])?, int(v[1])?, int(v[2])?, int(v[3])?];
            }
            "matrix" => {
                self.projection = match *a {
                    ["MODELVIEW"] => false,
                    ["PROJECTION"] => true,
                    _ => bail!("matrix mode {a:?}"),
                }
            }
            "load_identity" => *self.cur() = ID,
            "ortho" => {
                let v = nums(a)?;
                ensure!(v.len() == 6, "ortho takes 6 numbers");
                self.post(ortho(v[0], v[1], v[2], v[3], v[4], v[5]));
            }
            "push" => {
                ensure!(
                    !self.projection,
                    "push on GL_PROJECTION is not in the scene"
                );
                self.mv_stack.push(self.mv);
            }
            "pop" => {
                ensure!(!self.projection, "pop on GL_PROJECTION is not in the scene");
                self.mv = self
                    .mv_stack
                    .pop()
                    .context("pop of an empty matrix stack")?;
            }
            "translate" | "scale" | "rotate" => {
                let v = nums(a)?;
                let m = match (*op, v.len()) {
                    ("translate", 3) => translate(v[0], v[1], v[2]),
                    ("scale", 3) => scale(v[0], v[1], v[2]),
                    ("rotate", 4) => rotate(v[0], v[1], v[2], v[3]),
                    _ => bail!("{op} takes {} numbers", if *op == "rotate" { 4 } else { 3 }),
                };
                self.post(m);
            }
            "vertices" => {
                let v = nums(a)?;
                ensure!(v.len().is_multiple_of(2), "vertices come in pairs");
                self.verts = v.chunks(2).map(|c| [c[0], c[1]]).collect();
            }
            "colors" => {
                let v = nums(a)?;
                ensure!(v.len().is_multiple_of(4), "colors come in fours");
                self.colours = v.chunks(4).map(rgba_bytes).collect::<Result<_>>()?;
            }
            "texcoords" => {
                let v = nums(a)?;
                ensure!(v.len().is_multiple_of(2), "texcoords come in pairs");
                self.texs = v.chunks(2).map(|c| [c[0], c[1]]).collect();
            }
            "point_sizes" => self.psizes = nums(a)?,
            "elements" => {
                let idx = nums(a)?
                    .into_iter()
                    .map(|v| int(v).map(|i| i as usize))
                    .collect::<Result<Vec<_>>>()?;
                self.elements(&idx)?;
            }
            "points" => {
                let v = nums(a)?;
                ensure!(v.len() == 2, "points takes FIRST COUNT");
                self.points(int(v[0])? as usize, int(v[1])? as usize)?;
            }
            "blend_func" => ensure!(
                *a == ["SRC_ALPHA", "ONE_MINUS_SRC_ALPHA"],
                "blend_func {a:?}"
            ),
            "bind_texture" => ensure!(
                *a == ["1"],
                "bind_texture {a:?}: the scene binds texture 1 only"
            ),
            "texture" => {
                let v = nums(a)?;
                ensure!(v.len() >= 2, "texture takes W H and texels");
                let (tw, th) = (int(v[0])? as usize, int(v[1])? as usize);
                let data = &v[2..];
                ensure!(
                    data.len() == 4 * tw * th,
                    "texture {tw}x{th} needs {} channels",
                    4 * tw * th
                );
                let texels = data.chunks(4).map(rgba_bytes).collect::<Result<_>>()?;
                self.texels = Some((tw, th, texels));
            }
            "texture_filter" => ensure!(*a == ["NEAREST"], "texture_filter {a:?}: NEAREST only"),
            "read" => {
                let v = nums(a)?;
                ensure!(
                    v.len() == 2 && v[0] as usize == self.w && v[1] as usize == self.h,
                    "read {a:?}: the surface"
                );
            }
            _ => bail!("unknown step `{op}`"),
        }
        Ok(())
    }

    fn cap(&mut self, name: &str) -> Result<&mut bool> {
        Ok(match name {
            "SCISSOR_TEST" => &mut self.scissor_on,
            "BLEND" => &mut self.blend_on,
            "TEXTURE_2D" => &mut self.tex_on,
            _ => bail!("capability {name}"),
        })
    }

    /// glClear over the scissor box (or the whole surface), clipped to it.
    fn clear(&mut self, c: [u8; 4]) {
        let [sx, sy, sw, sh] = if self.scissor_on {
            self.scissor
        } else {
            [0, 0, self.w as i64, self.h as i64]
        };
        let x0 = sx.clamp(0, self.w as i64) as usize;
        let x1 = (sx + sw).clamp(0, self.w as i64) as usize;
        let y0 = sy.clamp(0, self.h as i64) as usize;
        let y1 = (sy + sh).clamp(0, self.h as i64) as usize;
        for y in y0..y1 {
            for x in x0..x1 {
                let p = y * self.w + x;
                self.fb[p] = c;
                self.tol[p] = 0;
            }
        }
    }

    /// Draws the integer rect `[x0,x1) x [y0,y1)`: interior pixels take `shade` (blended when BLEND is on), the 1 px band around the rect is marked edge.
    fn fill(&mut self, rect: [i64; 4], shade: &Shade) -> Result<()> {
        let [x0, y0, x1, y1] = rect;
        ensure!(x1 - x0 >= 1 && y1 - y0 >= 1, "empty rect {rect:?}");
        for py in y0 - 1..=y1 {
            for px in x0 - 1..=x1 {
                if px < 0 || py < 0 || px >= self.w as i64 || py >= self.h as i64 {
                    continue;
                }
                let p = py as usize * self.w + px as usize;
                let interior = x0 < px && px < x1 - 1 && y0 < py && py < y1 - 1;
                if !interior {
                    self.edge[p] = true;
                    continue;
                }
                if !self.in_scissor(px, py) {
                    continue;
                }
                let src = shade.at(px as f64 + 0.5, py as f64 + 0.5);
                if self.blend_on {
                    self.fb[p] = blend(src, self.fb[p]);
                    self.tol[p] = 1;
                } else {
                    self.fb[p] = src;
                    self.tol[p] = 0;
                }
            }
        }
        Ok(())
    }

    /// glDrawElements(GL_TRIANGLES): the triangles must tile one axis-aligned rect whose corners are the vertices used.
    fn elements(&mut self, idx: &[usize]) -> Result<()> {
        ensure!(
            !idx.is_empty() && idx.len().is_multiple_of(3),
            "{} indices are not triangles",
            idx.len()
        );
        ensure!(self.client[0], "elements without VERTEX_ARRAY");
        let used: BTreeSet<usize> = idx.iter().copied().collect();
        let mut pos: BTreeMap<usize, (f64, f64)> = BTreeMap::new();
        for &i in &used {
            let v = *self
                .verts
                .get(i)
                .with_context(|| format!("vertex {i} not set"))?;
            pos.insert(i, self.window(v));
        }
        let xs: Vec<f64> = pos.values().map(|p| p.0).collect();
        let ys: Vec<f64> = pos.values().map(|p| p.1).collect();
        let (xmin, xmax) = (
            xs.iter().copied().fold(f64::INFINITY, f64::min),
            xs.iter().copied().fold(f64::NEG_INFINITY, f64::max),
        );
        let (ymin, ymax) = (
            ys.iter().copied().fold(f64::INFINITY, f64::min),
            ys.iter().copied().fold(f64::NEG_INFINITY, f64::max),
        );
        let rect = [int(xmin)?, int(ymin)?, int(xmax)?, int(ymax)?];
        let near = |a: f64, b: f64| (a - b).abs() < 1e-6;
        for &(x, y) in pos.values() {
            ensure!(
                (near(x, xmin) || near(x, xmax)) && (near(y, ymin) || near(y, ymax)),
                "vertex ({x},{y}) is not a rect corner"
            );
        }
        let area: f64 = idx
            .chunks(3)
            .map(|t| {
                let (a, b, c) = (pos[&t[0]], pos[&t[1]], pos[&t[2]]);
                ((b.0 - a.0) * (c.1 - a.1) - (c.0 - a.0) * (b.1 - a.1)).abs() / 2.0
            })
            .sum();
        ensure!(
            near(area, (xmax - xmin) * (ymax - ymin)),
            "triangles do not tile their rect"
        );
        let shade = if self.tex_on {
            ensure!(
                !self.client[1],
                "textured quads take one colour, not a colour array"
            );
            ensure!(self.client[2], "texture on without TEXTURE_COORD_ARRAY");
            let corner = |x: f64, y: f64| -> Result<[f64; 2]> {
                let i = pos
                    .iter()
                    .find(|(_, p)| near(p.0, x) && near(p.1, y))
                    .map(|(i, _)| *i)
                    .context("no vertex at a rect corner")?;
                self.texs.get(i).copied().context("texcoord not set")
            };
            let st = [
                corner(xmin, ymin)?,
                corner(xmax, ymin)?,
                corner(xmin, ymax)?,
                corner(xmax, ymax)?,
            ];
            let (tw, th, texels) = self.texels.clone().context("texture on, none set")?;
            Shade::Textured {
                rect,
                st,
                tw,
                th,
                texels,
                colour: self.colour,
            }
        } else if self.client[1] {
            let mut cols = used
                .iter()
                .map(|&i| self.colours.get(i).copied().context("colour not set"));
            let first = cols.next().context("no vertices")??;
            for c in cols {
                ensure!(c? == first, "per-vertex colour is not flat");
            }
            Shade::Flat(first)
        } else {
            Shade::Flat(self.colour)
        };
        self.fill(rect, &shade)
    }

    /// glDrawArrays(GL_POINTS): non-AA squares of the point size, centred on each vertex.
    fn points(&mut self, first: usize, count: usize) -> Result<()> {
        ensure!(!self.tex_on, "textured points are not in the scene");
        for i in first..first + count {
            let v = *self
                .verts
                .get(i)
                .with_context(|| format!("point vertex {i} not set"))?;
            let (cx, cy) = self.window(v);
            let size = if self.client[3] {
                *self.psizes.get(i).context("point size not set")?
            } else {
                1.0
            };
            let half = size / 2.0;
            let rect = [
                int(cx - half)?,
                int(cy - half)?,
                int(cx + half)?,
                int(cy + half)?,
            ];
            let colour = if self.client[1] {
                *self.colours.get(i).context("point colour not set")?
            } else {
                self.colour
            };
            self.fill(rect, &Shade::Flat(colour))?;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scene() -> Scene {
        let path = crate::checkout_root()
            .expect("checkout root")
            .join("xtask/testdata/gl-probe/es1-scene.toml");
        Scene::load(&path).expect("the ES1 scene parses")
    }

    fn rows_of(pixels: &[Pixel], w: usize, h: usize) -> BTreeMap<usize, String> {
        (0..h)
            .map(|y| {
                let hex: String = (0..w)
                    .flat_map(|x| pixels[y * w + x].rgba)
                    .map(|b| format!("{b:02x}"))
                    .collect();
                (y, hex)
            })
            .collect()
    }

    #[test]
    fn break_drops_only_the_marked_steps() {
        let s = scene();
        let full = s.program(false);
        let broken = s.program(true);
        assert_eq!(
            full.len() - broken.len(),
            2,
            "two breakable steps: the COLOR_ARRAY enable and the pop"
        );
        assert!(full.iter().any(|l| l == "pop") && !broken.iter().any(|l| l == "pop"));
        assert!(
            full.iter().any(|l| l == "client_enable COLOR_ARRAY")
                && !broken.iter().any(|l| l == "client_enable COLOR_ARRAY")
        );
        assert_eq!(
            full.last().map(String::as_str),
            Some(format!("resolve {}", s.names().join(" ")).as_str())
        );
        assert_eq!(s.names().len(), 20, "Happy Jump imports 20 GLES1 names");
    }

    /// Expected values derived by hand from the spec (see the toml comments), not from the model.
    #[test]
    fn expectation_matches_hand_derived_values() {
        let s = scene();
        let (w, _) = s.size();
        let px = s.expect().expect("the scene models");
        let at = |x: usize, y: usize| px[y * w + x];
        // background B (30 60 200) right of the scissor; A (200 30 40) left of x = 32
        assert_eq!(at(63, 0).rgba, [30, 60, 200, 255]);
        assert_eq!(at(0, 0).rgba, [200, 30, 40, 255]);
        assert!(!at(63, 0).edge && !at(0, 0).edge);
        // Q1 flat C (16 200 90) on [8,24)^2; its edge column is excluded
        assert_eq!(at(16, 16).rgba, [16, 200, 90, 255]);
        assert!(at(8, 8).edge);
        // D (120 40 230) on [40,48)x[8,16)
        assert_eq!(at(45, 12).rgba, [120, 40, 230, 255]);
        // texture quad [40,56)^2: texel boundaries at 48; row 0 is t in [0,0.5)
        assert_eq!(at(47, 47).rgba, [255, 0, 0, 255]);
        assert_eq!(at(48, 47).rgba, [0, 255, 0, 255]);
        assert_eq!(at(47, 48).rgba, [0, 0, 255, 255]);
        assert_eq!(at(48, 48).rgba, [255, 255, 0, 255]);
        // point square [10,14)x[50,54): interior 11..12 x 51..52 is white
        assert_eq!(at(11, 51).rgba, [255, 255, 255, 255]);
        assert_eq!(at(12, 52).rgba, [255, 255, 255, 255]);
        // blend S (0 255 0, alpha 128) over A at (20,48): R 200*127/255=99.6, G 128+30*127/255=142.9, B 40*127/255=19.9, A 128*128/255+127=191.25; 1 LSB
        let b = at(20, 48);
        assert_eq!(
            (b.rgba, b.tolerance, b.edge),
            ([100, 143, 20, 191], 1, false)
        );
        // matrix rect lands on [30,32)x[32,40): x = 33 is just outside its band and B; x = 10 is A
        assert_eq!(at(33, 36).rgba, [30, 60, 200, 255]);
        assert_eq!(at(10, 36).rgba, [200, 30, 40, 255]);
    }

    #[test]
    fn judge_accepts_the_expectation_and_names_a_flipped_pixel() {
        let s = scene();
        let (w, h) = s.size();
        let px = s.expect().expect("the scene models");
        let rows = rows_of(&px, w, h);
        let compared = px.iter().filter(|p| !p.edge).count();
        assert_eq!(s.judge(&rows).expect("judged"), Judged::Match(compared));
        let mut bad = px.clone();
        let p = 16 * w + 16;
        bad[p].rgba[1] ^= 0x10;
        match s.judge(&rows_of(&bad, w, h)).expect("judged") {
            Judged::Mismatch(m) => assert!(m.contains("(16,16)"), "{m}"),
            other => panic!("want a mismatch, got {other:?}"),
        }
    }

    #[test]
    fn judge_reports_missing_rows_apart_from_mismatch() {
        let s = scene();
        let (w, h) = s.size();
        let px = s.expect().expect("the scene models");
        let mut rows = rows_of(&px, w, h);
        rows.remove(&(h - 1));
        assert!(matches!(
            s.judge(&rows).expect("judged"),
            Judged::Missing(_)
        ));
    }

    #[test]
    fn blend_and_modulate_round_per_channel() {
        assert_eq!(
            blend([0, 255, 0, 128], [200, 30, 40, 255]),
            [100, 143, 20, 191]
        );
        assert_eq!(
            modulate([255, 0, 0, 255], [255, 255, 255, 255]),
            [255, 0, 0, 255]
        );
        assert_eq!(
            modulate([255, 255, 255, 255], [128, 0, 0, 255]),
            [128, 0, 0, 255]
        );
    }
}
