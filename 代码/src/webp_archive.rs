//! Lossless WebP with an ordinary ICC profile and self-contained XMP provenance.
use crate::{
    domain::Stop,
    imaging::{Colors, Decoded, Frame},
};
use anyhow::{Context, Result, ensure};
use serde::{Deserialize, Serialize};
use std::io::{BufRead, Cursor, Seek, Write};
const MAX_METADATA_BYTES: usize = 128 * 1024;
const MAX_IMAGE_MEMORY: usize = 512 * 1024 * 1024;
const PREFIX: &str = r#"<x:xmpmeta xmlns:x="adobe:ns:meta/"><rdf:RDF xmlns:rdf="http://www.w3.org/1999/02/22-rdf-syntax-ns#"><rdf:Description xmlns:el="https://earthlive.local/archive/2/" el:archive=""#;
const SUFFIX: &str = r#""/></rdf:RDF></x:xmpmeta>"#;
#[derive(Serialize, Deserialize)]
struct Envelope {
    colors: Colors,
    note: String,
}

pub fn encode(out: &mut impl Write, frame: &Frame, note: &str, stop: &Stop) -> Result<()> {
    stop.check()?;
    let json = serde_json::to_vec(&Envelope {
        colors: frame.colors.clone(),
        note: note.into(),
    })?;
    let hex: String = json.iter().map(|b| format!("{b:02x}")).collect();
    let mut encoder = image_webp::WebPEncoder::new(CheckedWriter { out, stop });
    encoder.set_icc_profile(profile(&frame.colors)?);
    encoder.set_xmp_metadata(format!("{PREFIX}{hex}{SUFFIX}").into_bytes());
    encoder
        .encode(
            frame.pixels.as_raw(),
            frame.pixels.width(),
            frame.pixels.height(),
            image_webp::ColorType::Rgb8,
        )
        .context("无损 WebP 编码或写入失败")?;
    stop.check()
}
struct CheckedWriter<'a, W> {
    out: &'a mut W,
    stop: &'a Stop,
}
impl<W: Write> Write for CheckedWriter<'_, W> {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        if self.stop.is_cancelled() {
            return Err(std::io::Error::other("已取消"));
        }
        self.out.write(&bytes[..bytes.len().min(65536)])
    }
    fn flush(&mut self) -> std::io::Result<()> {
        self.out.flush()
    }
}
pub fn decode(bytes: &[u8], stop: &Stop) -> Result<Decoded> {
    stop.check()?;
    ensure!(
        bytes.len() >= 12 && &bytes[..4] == b"RIFF" && &bytes[8..12] == b"WEBP",
        "WebP 文件头无效"
    );
    ensure!(
        u32::from_le_bytes(bytes[4..8].try_into()?) as usize + 8 == bytes.len(),
        "WebP 不完整或含多余数据"
    );
    let mut decoder = metadata_decoder(Cursor::new(bytes))?;
    let (w, h) = decoder.dimensions();
    ensure!(
        w > 0
            && h > 0
            && w <= 8800
            && h <= 8800
            && !decoder.is_animated()
            && !decoder.is_lossy()
            && !decoder.has_alpha(),
        "不支持的 WebP 归档"
    );
    let xmp = decoder.xmp_metadata()?.context("缺少 V2 归档说明")?;
    let envelope = parse_envelope(&xmp)?;
    ensure!(
        decoder.icc_profile()?.unwrap_or_default() == profile(&envelope.colors)?,
        "WebP 色彩配置与归档说明不一致"
    );
    decoder.set_memory_limit(MAX_IMAGE_MEMORY);
    let size = decoder.output_buffer_size().context("WebP 尺寸无效")?;
    let mut pixels = Vec::new();
    pixels.try_reserve_exact(size)?;
    pixels.resize(size, 0);
    decoder
        .read_image(&mut pixels)
        .context("WebP 像素解码失败")?;
    stop.check()?;
    Ok(Decoded {
        pixels: image::RgbImage::from_raw(w, h, pixels).context("WebP 像素长度无效")?,
        colors: envelope.colors,
        note: Some(envelope.note),
    })
}

// ICC v2 RGB matrix/TRC profile. The PNG chromaticities are transformed to D50
// with Bradford adaptation; curves retain PNG gamma or the sRGB transfer curve.
pub(crate) fn profile(colors: &Colors) -> Result<Vec<u8>> {
    if colors.gamma.is_none() && colors.primaries.is_none() && colors.srgb.is_none() {
        return Ok(Vec::new());
    }
    let c = colors
        .primaries
        .unwrap_or([31270, 32900, 64000, 33000, 30000, 60000, 15000, 6000])
        .map(|v| v as f64 / 100000.0);
    let xyz = |x: f64, y: f64| -> Result<[f64; 3]> {
        ensure!(
            x.is_finite() && y > 0.0 && x >= 0.0 && x + y <= 1.00001,
            "PNG 色度无效"
        );
        Ok([x / y, 1.0, (1.0 - x - y) / y])
    };
    let white = xyz(c[0], c[1])?;
    let prim = [xyz(c[2], c[3])?, xyz(c[4], c[5])?, xyz(c[6], c[7])?];
    let matrix = std::array::from_fn(|i| std::array::from_fn(|j| prim[j][i]));
    let scale = mul(inverse(matrix)?, white);
    let bradford = [
        [0.8951, 0.2664, -0.1614],
        [-0.7502, 1.7135, 0.0367],
        [0.0389, -0.0685, 1.0296],
    ];
    let inv = inverse(bradford)?;
    let d50 = [0.9642, 1.0, 0.8249];
    let src = mul(bradford, white);
    let dst = mul(bradford, d50);
    ensure!(src.iter().all(|x| x.abs() > 1e-10), "PNG 白点无效");
    let adapted: [[f64; 3]; 3] = std::array::from_fn(|j| {
        let xyz = prim[j].map(|x| x * scale[j]);
        let cone = mul(bradford, xyz);
        mul(inv, std::array::from_fn(|i| cone[i] * dst[i] / src[i]))
    });
    let mut curve = b"curv\0\0\0\0".to_vec();
    curve.extend_from_slice(&4096u32.to_be_bytes());
    let gamma = colors.gamma.unwrap_or(45455) as f64 / 100000.0;
    ensure!(gamma > 0.0, "PNG gamma 无效");
    for i in 0..4096 {
        let x = i as f64 / 4095.0;
        let y = if colors.srgb.is_some() {
            if x <= 0.04045 {
                x / 12.92
            } else {
                ((x + 0.055) / 1.055).powf(2.4)
            }
        } else {
            x.powf(1.0 / gamma)
        };
        curve.extend_from_slice(&((y * 65535.0).round() as u16).to_be_bytes());
    }
    let xyz_tag = |v: [f64; 3]| {
        let mut b = b"XYZ \0\0\0\0".to_vec();
        for x in v {
            b.extend_from_slice(&((x * 65536.0).round() as i32).to_be_bytes());
        }
        b
    };
    let title = b"EarthLive source colors\0";
    let mut desc = b"desc\0\0\0\0".to_vec();
    desc.extend_from_slice(&(title.len() as u32).to_be_bytes());
    desc.extend_from_slice(title);
    desc.extend_from_slice(&[0; 78]);
    let mut copyright = b"text\0\0\0\0".to_vec();
    copyright.extend_from_slice(b"Generated from source PNG color declarations\0");
    let tags = [
        (*b"desc", desc),
        (*b"cprt", copyright),
        (*b"wtpt", xyz_tag(d50)),
        (*b"rXYZ", xyz_tag(adapted[0])),
        (*b"gXYZ", xyz_tag(adapted[1])),
        (*b"bXYZ", xyz_tag(adapted[2])),
        (*b"rTRC", curve.clone()),
        (*b"gTRC", curve.clone()),
        (*b"bTRC", curve),
    ];
    let mut out = vec![0u8; 128 + 4 + tags.len() * 12];
    out[8..12].copy_from_slice(&0x02400000u32.to_be_bytes());
    out[12..16].copy_from_slice(b"mntr");
    out[16..20].copy_from_slice(b"RGB ");
    out[20..24].copy_from_slice(b"XYZ ");
    for (i, v) in [2026u16, 10, 23, 0, 0, 0].into_iter().enumerate() {
        out[24 + i * 2..26 + i * 2].copy_from_slice(&v.to_be_bytes());
    }
    out[36..40].copy_from_slice(b"acsp");
    out[40..44].copy_from_slice(b"MSFT");
    out[64..68].copy_from_slice(&(colors.srgb.unwrap_or(1) as u32).to_be_bytes());
    for (i, x) in d50.into_iter().enumerate() {
        out[68 + i * 4..72 + i * 4].copy_from_slice(&((x * 65536.0).round() as i32).to_be_bytes());
    }
    out[80..84].copy_from_slice(b"ELV2");
    out[128..132].copy_from_slice(&(tags.len() as u32).to_be_bytes());
    for (i, (sig, tag)) in tags.into_iter().enumerate() {
        while !out.len().is_multiple_of(4) {
            out.push(0);
        }
        let offset = out.len() as u32;
        let index = 132 + i * 12;
        out[index..index + 4].copy_from_slice(&sig);
        out[index + 4..index + 8].copy_from_slice(&offset.to_be_bytes());
        out[index + 8..index + 12].copy_from_slice(&(tag.len() as u32).to_be_bytes());
        out.extend_from_slice(&tag);
    }
    while !out.len().is_multiple_of(4) {
        out.push(0);
    }
    let len = out.len() as u32;
    out[..4].copy_from_slice(&len.to_be_bytes());
    Ok(out)
}
fn mul(a: [[f64; 3]; 3], b: [f64; 3]) -> [f64; 3] {
    a.map(|r| r.into_iter().zip(b).map(|(x, y)| x * y).sum())
}
fn inverse(a: [[f64; 3]; 3]) -> Result<[[f64; 3]; 3]> {
    let [r, s, t] = a;
    let cof = [
        [
            s[1] * t[2] - s[2] * t[1],
            s[2] * t[0] - s[0] * t[2],
            s[0] * t[1] - s[1] * t[0],
        ],
        [
            r[2] * t[1] - r[1] * t[2],
            r[0] * t[2] - r[2] * t[0],
            r[1] * t[0] - r[0] * t[1],
        ],
        [
            r[1] * s[2] - r[2] * s[1],
            r[2] * s[0] - r[0] * s[2],
            r[0] * s[1] - r[1] * s[0],
        ],
    ];
    let det = r[0] * cof[0][0] + r[1] * cof[0][1] + r[2] * cof[0][2];
    ensure!(det.abs() > 1e-10, "PNG 色度矩阵不可逆");
    Ok(std::array::from_fn(|i| {
        std::array::from_fn(|j| cof[j][i] / det)
    }))
}

fn parse_envelope(xmp: &[u8]) -> Result<Envelope> {
    ensure!(xmp.len() <= MAX_METADATA_BYTES, "归档说明过大");
    let text = std::str::from_utf8(xmp)?;
    let hex = text
        .strip_prefix(PREFIX)
        .and_then(|s| s.strip_suffix(SUFFIX))
        .context("不是 V2 XMP 归档")?;
    ensure!(
        hex.len().is_multiple_of(2) && hex.is_ascii(),
        "归档说明编码无效"
    );
    let json = hex
        .as_bytes()
        .as_chunks::<2>()
        .0
        .iter()
        .map(|v| u8::from_str_radix(std::str::from_utf8(v).unwrap(), 16))
        .collect::<std::result::Result<Vec<_>, _>>()?;
    let envelope: Envelope = serde_json::from_slice(&json)?;
    Ok(envelope)
}

fn metadata_decoder<R: BufRead + Seek>(reader: R) -> Result<image_webp::WebPDecoder<R>> {
    let mut decoder = image_webp::WebPDecoder::new(reader)?;
    decoder.set_memory_limit(MAX_METADATA_BYTES);
    Ok(decoder)
}

pub(crate) fn read_note(reader: impl BufRead + Seek) -> Result<String> {
    let mut decoder = metadata_decoder(reader)?;
    let xmp = decoder.xmp_metadata()?.context("缺少归档说明")?;
    Ok(parse_envelope(&xmp)?.note)
}
