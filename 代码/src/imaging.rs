use crate::domain::{Observation, Stop, TILE};
use anyhow::{Context, Result, bail, ensure};
use image::{Rgb, RgbImage};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::io::{BufRead, Seek};

#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct Colors {
    pub gamma: Option<u32>,
    pub primaries: Option<[u32; 8]>,
    pub srgb: Option<u8>,
}
impl Colors {
    fn from_info(info: &png::Info<'_>) -> Self {
        if let Some(intent) = info.srgb {
            return Self {
                gamma: Some(45455),
                primaries: Some([31270, 32900, 64000, 33000, 30000, 60000, 15000, 6000]),
                srgb: Some(intent as u8),
            };
        }
        Self {
            gamma: info.gama_chunk.map(|x| x.into_scaled()),
            primaries: info.chrm_chunk.map(|x| {
                let b = x.to_be_bytes();
                std::array::from_fn(|i| u32::from_be_bytes(b[i * 4..i * 4 + 4].try_into().unwrap()))
            }),
            srgb: info.srgb.map(|v| v as u8),
        }
    }
    pub fn apply<W: std::io::Write>(&self, encoder: &mut png::Encoder<'_, W>) {
        if let Some(gamma) = self.gamma {
            encoder.set_source_gamma(png::ScaledFloat::from_scaled(gamma));
        }
        if let Some(c) = self.primaries {
            let pair = |i| {
                (
                    png::ScaledFloat::from_scaled(c[i]),
                    png::ScaledFloat::from_scaled(c[i + 1]),
                )
            };
            encoder.set_source_chromaticities(png::SourceChromaticities {
                white: pair(0),
                red: pair(2),
                green: pair(4),
                blue: pair(6),
            });
        }
        if let Some(srgb) = self.srgb {
            use png::SrgbRenderingIntent::*;
            encoder.set_source_srgb(match srgb {
                0 => Perceptual,
                1 => RelativeColorimetric,
                2 => Saturation,
                _ => AbsoluteColorimetric,
            });
        }
    }
}

pub struct Decoded {
    pub pixels: RgbImage,
    pub colors: Colors,
    pub note: Option<String>,
}
pub fn decode<R: BufRead + Seek>(input: R, maximum: u32) -> Result<Decoded> {
    let mut decoder = png::Decoder::new(input);
    decoder.set_limits(png::Limits {
        bytes: 512 * 1024 * 1024,
    });
    decoder.set_transformations(png::Transformations::EXPAND);
    let mut reader = decoder.read_info().context("不是完整有效的 PNG")?;
    let info = reader.info();
    ensure!(
        info.width > 0 && info.height > 0 && info.width <= maximum && info.height <= maximum,
        "PNG 尺寸超出范围"
    );
    ensure!(
        info.bit_depth != png::BitDepth::Sixteen && !info.is_animated(),
        "暂不支持 16 位或动画 PNG"
    );
    ensure!(
        info.icc_profile.is_none()
            && info.coding_independent_code_points.is_none()
            && info.mastering_display_color_volume.is_none()
            && info.content_light_level.is_none(),
        "暂不支持 ICC/HDR 色彩声明"
    );
    let colors = Colors::from_info(info);
    let note = info
        .utf8_text
        .iter()
        .find(|t| t.keyword == "EarthLiveV2")
        .map(|t| {
            ensure!(!t.compressed, "归档说明不得为压缩文本");
            Ok(t.get_text()?)
        })
        .transpose()?;
    let size = reader.output_buffer_size().context("PNG 输出尺寸无效")?;
    let mut pixels = Vec::new();
    pixels.try_reserve_exact(size)?;
    pixels.resize(size, 0);
    let output = reader
        .next_frame(&mut pixels)
        .context("PNG 数据或 CRC 校验失败")?;
    ensure!(output.bit_depth == png::BitDepth::Eight, "PNG 解码位深异常");
    pixels.truncate(output.buffer_size());
    reader.finish().context("PNG 结尾校验失败")?;
    let rgb = match output.color_type {
        png::ColorType::Rgb => pixels,
        kind => {
            let channels = match kind {
                png::ColorType::Grayscale => 1,
                png::ColorType::GrayscaleAlpha => 2,
                png::ColorType::Rgba => 4,
                _ => bail!("PNG 色彩展开失败"),
            };
            let mut rgb = Vec::with_capacity(output.width as usize * output.height as usize * 3);
            for px in pixels.chunks_exact(channels) {
                if channels == 2 || channels == 4 {
                    ensure!(
                        px[channels - 1] == 255,
                        "分片含透明像素，拒绝将缺失信息补黑"
                    );
                }
                if channels <= 2 {
                    rgb.extend_from_slice(&[px[0]; 3]);
                } else {
                    rgb.extend_from_slice(&px[..3]);
                }
            }
            rgb
        }
    };
    Ok(Decoded {
        pixels: RgbImage::from_raw(output.width, output.height, rgb).context("PNG 像素长度异常")?,
        colors,
        note,
    })
}

pub struct Frame {
    pub(crate) observation: Observation,
    pub(crate) pixels: RgbImage,
    pub(crate) colors: Colors,
}
impl Frame {
    pub fn observation(&self) -> &Observation {
        &self.observation
    }
    pub fn pixels(&self) -> &RgbImage {
        &self.pixels
    }
    pub fn fingerprint(&self, stop: &Stop) -> Result<String> {
        let mut hash = Sha256::new();
        hash.update(serde_json::to_vec(&self.colors)?);
        hash.update(self.pixels.width().to_le_bytes());
        hash.update(self.pixels.height().to_le_bytes());
        for chunk in self.pixels.as_raw().chunks(1024 * 1024) {
            stop.check()?;
            hash.update(chunk);
        }
        Ok(format!("{:x}", hash.finalize()))
    }
}

pub struct Assembly {
    observation: Observation,
    canvas: RgbImage,
    seen: Vec<bool>,
    colors: Option<Colors>,
}
impl Assembly {
    pub fn new(observation: Observation) -> Result<Self> {
        observation.validate()?;
        let side = observation.side();
        let mut buffer = Vec::new();
        buffer.try_reserve_exact(side as usize * side as usize * 3)?;
        buffer.resize(side as usize * side as usize * 3, 0);
        Ok(Self {
            seen: vec![false; (observation.grid * observation.grid) as usize],
            observation,
            canvas: RgbImage::from_raw(side, side, buffer).unwrap(),
            colors: None,
        })
    }
    pub fn insert(&mut self, x: u32, y: u32, tile: Decoded, stop: &Stop) -> Result<()> {
        let grid = self.observation.grid;
        ensure!(x < grid && y < grid, "分片坐标越界");
        let index = (y * grid + x) as usize;
        ensure!(!self.seen[index], "重复分片");
        ensure!(
            tile.pixels.dimensions() == (TILE, TILE),
            "分片须为 550×550 像素"
        );
        if let Some(colors) = &self.colors {
            ensure!(colors == &tile.colors, "同一帧分片色彩声明不一致");
        } else {
            self.colors = Some(tile.colors);
        }
        let stride = self.canvas.width() as usize * 3;
        for row in 0..TILE as usize {
            stop.check()?;
            let dest = (y as usize * TILE as usize + row) * stride + x as usize * TILE as usize * 3;
            let src = row * TILE as usize * 3;
            self.canvas.as_mut()[dest..dest + TILE as usize * 3]
                .copy_from_slice(&tile.pixels.as_raw()[src..src + TILE as usize * 3]);
        }
        self.seen[index] = true;
        Ok(())
    }
    pub fn finish(self) -> Result<Frame> {
        ensure!(self.seen.iter().all(|v| *v), "分片缺失，禁止发布半张图");
        Ok(Frame {
            observation: self.observation,
            pixels: self.canvas,
            colors: self.colors.context("没有分片")?,
        })
    }
}

// Destination-driven rendering: allocation is always the viewport, even at 200%.
// Area sampling when shrinking avoids the aliasing of point/bilinear-only reduction.
pub fn compose(frame: &Frame, width: u32, height: u32, zoom: u16, stop: &Stop) -> Result<RgbImage> {
    ensure!((10..=200).contains(&zoom), "缩放范围为 10%–200%");
    ensure!(
        width > 0
            && height > 0
            && u64::from(width) * u64::from(height) <= 64_000_000
            && width <= 16384
            && height <= 16384,
        "壁纸尺寸无效或过大"
    );
    let mut out = RgbImage::new(width, height);
    let src = &frame.pixels;
    let scaled = (f64::from(src.width()) * f64::from(zoom) / 100.0)
        .round()
        .max(1.0);
    let left = ((f64::from(width) - scaled) / 2.0).floor();
    let top = ((f64::from(height) - scaled) / 2.0).floor();
    let scale = scaled / f64::from(src.width());
    let gamma = frame
        .colors
        .gamma
        .filter(|_| frame.colors.srgb.is_none())
        .filter(|v| *v > 0)
        .map(|v| v as f64 / 100000.0);
    let linear: [f64; 256] = std::array::from_fn(|i| {
        let v = i as f64 / 255.0;
        if let Some(g) = gamma {
            v.powf(1.0 / g)
        } else if v <= 0.04045 {
            v / 12.92
        } else {
            ((v + 0.055) / 1.055).powf(2.4)
        }
    });
    let encode = |v: f64| -> u8 {
        let v = if let Some(g) = gamma {
            v.powf(g)
        } else if v <= 0.0031308 {
            v * 12.92
        } else {
            1.055 * v.powf(1.0 / 2.4) - 0.055
        };
        (v * 255.0).round().clamp(0.0, 255.0) as u8
    };
    for y in 0..height {
        stop.check()?;
        if f64::from(y) < top || f64::from(y) >= top + scaled {
            continue;
        }
        for x in 0..width {
            if f64::from(x) < left || f64::from(x) >= left + scaled {
                continue;
            }
            if zoom == 100 {
                out.put_pixel(
                    x,
                    y,
                    *src.get_pixel((f64::from(x) - left) as u32, (f64::from(y) - top) as u32),
                );
                continue;
            }
            let mut total = [0.0; 3];
            if zoom < 100 {
                let x0 = ((f64::from(x) - left) / scale).clamp(0.0, f64::from(src.width()));
                let x1 = ((f64::from(x) - left + 1.0) / scale).clamp(0.0, f64::from(src.width()));
                let y0 = ((f64::from(y) - top) / scale).clamp(0.0, f64::from(src.height()));
                let y1 = ((f64::from(y) - top + 1.0) / scale).clamp(0.0, f64::from(src.height()));
                for sy in y0.floor() as u32..(y1.ceil() as u32).min(src.height()) {
                    let wy = (y1.min(f64::from(sy + 1)) - y0.max(f64::from(sy))).max(0.0);
                    for sx in x0.floor() as u32..(x1.ceil() as u32).min(src.width()) {
                        let weight = wy
                            * (x1.min(f64::from(sx + 1)) - x0.max(f64::from(sx))).max(0.0)
                            / ((x1 - x0) * (y1 - y0));
                        let px = src.get_pixel(sx, sy);
                        for c in 0..3 {
                            total[c] += linear[px[c] as usize] * weight;
                        }
                    }
                }
            } else {
                let sx = ((f64::from(x) - left + 0.5) / scale - 0.5)
                    .clamp(0.0, f64::from(src.width() - 1));
                let sy = ((f64::from(y) - top + 0.5) / scale - 0.5)
                    .clamp(0.0, f64::from(src.height() - 1));
                let ax = sx.floor() as u32;
                let ay = sy.floor() as u32;
                for (px, wx) in [
                    (ax, 1.0 - sx.fract()),
                    ((ax + 1).min(src.width() - 1), sx.fract()),
                ] {
                    for (py, wy) in [
                        (ay, 1.0 - sy.fract()),
                        ((ay + 1).min(src.height() - 1), sy.fract()),
                    ] {
                        let p = src.get_pixel(px, py);
                        for c in 0..3 {
                            total[c] += linear[p[c] as usize] * wx * wy;
                        }
                    }
                }
            }
            out.put_pixel(x, y, Rgb(total.map(encode)));
        }
    }
    Ok(out)
}
