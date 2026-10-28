use crate::{
    domain::{self, Observation, Stop},
    engine::{self, Selection},
    imaging::{self, Assembly, Colors, Decoded, Frame},
    net::Source,
    settings::{self, Preferences},
    storage,
};
use chrono::{TimeZone, Utc};
use image::{Rgb, RgbImage};
use std::{
    io::Cursor,
    sync::{Arc, Mutex},
    time::{Duration, Instant},
};
use tokio::io::{AsyncReadExt, AsyncWriteExt};

fn observation(grid: u32) -> Observation {
    Observation::new(Utc.with_ymd_and_hms(2026, 10, 22, 6, 0, 0).unwrap(), grid).unwrap()
}
fn tile(color: [u8; 3]) -> Decoded {
    Decoded {
        pixels: RgbImage::from_pixel(550, 550, Rgb(color)),
        colors: Colors::default(),
        note: None,
    }
}
fn frame(color: [u8; 3]) -> Frame {
    let mut a = Assembly::new(observation(1)).unwrap();
    a.insert(0, 0, tile(color), &Stop::default()).unwrap();
    a.finish().unwrap()
}
fn png(color: [u8; 3]) -> Vec<u8> {
    let mut out = Vec::new();
    {
        let mut e = png::Encoder::new(&mut out, 550, 550);
        e.set_color(png::ColorType::Rgb);
        e.set_depth(png::BitDepth::Eight);
        let mut w = e.write_header().unwrap();
        w.write_image_data(&RgbImage::from_pixel(550, 550, Rgb(color)).into_raw())
            .unwrap();
    }
    out
}
fn archive_count(path: &std::path::Path) -> usize {
    std::fs::read_dir(path)
        .unwrap()
        .filter_map(Result::ok)
        .filter(|e| {
            e.path()
                .extension()
                .is_some_and(|x| x == "png" || x == "webp")
        })
        .count()
}

#[test]
fn webp_lossless_color_provenance_and_cross_format_dedup() {
    let stop = Stop::default();
    for srgb in [None, Some(0), Some(1)] {
        let mut f = frame([20, 67, 123]);
        f.colors = Colors {
            gamma: Some(45455),
            primaries: Some([31270, 32900, 64000, 33000, 30000, 60000, 15000, 6000]),
            srgb,
        };
        for (x, y, p) in f.pixels.enumerate_pixels_mut() {
            *p = Rgb([(x % 256) as u8, (y % 256) as u8, ((x + y) % 256) as u8]);
        }
        let dir = tempfile::tempdir().unwrap();
        let a = storage::save_as(dir.path(), &f, storage::ArchiveFormat::Webp, &stop).unwrap();
        let loaded = storage::load(&a.path, &stop).unwrap();
        assert_eq!(loaded.pixels, f.pixels);
        assert_eq!(loaded.colors, f.colors);
        assert_eq!(loaded.observation, f.observation);
        assert_eq!(
            loaded.fingerprint(&stop).unwrap(),
            f.fingerprint(&stop).unwrap()
        );
        let independent = image::open(&a.path).unwrap().into_rgb8();
        assert_eq!(independent, f.pixels);
        let same = storage::save_as(dir.path(), &f, storage::ArchiveFormat::Png, &stop).unwrap();
        assert!(same.existing);
        assert_eq!(same.path, a.path);
        assert_eq!(
            storage::existing(dir.path(), f.observation(), &stop).unwrap(),
            Some(a.path.clone())
        );
        let mut corrupted = std::fs::read(&a.path).unwrap();
        let pos = corrupted.windows(4).position(|w| w == b"acsp").unwrap();
        corrupted[pos] = b'x';
        std::fs::write(&a.path, &corrupted).unwrap();
        assert!(storage::load(&a.path, &stop).is_err());
    }
}
#[test]
fn webp_revisions_cancel_and_missing_metadata() {
    let stop = Stop::default();
    let dir = tempfile::tempdir().unwrap();
    let a = storage::save_as(
        dir.path(),
        &frame([1, 2, 3]),
        storage::ArchiveFormat::Webp,
        &stop,
    )
    .unwrap();
    let before = std::fs::read(&a.path).unwrap();
    let b = storage::save_as(
        dir.path(),
        &frame([4, 5, 6]),
        storage::ArchiveFormat::Webp,
        &stop,
    )
    .unwrap();
    assert_ne!(a.path, b.path);
    assert_eq!(std::fs::read(&a.path).unwrap(), before);
    let cancel = Stop::default();
    cancel.cancel();
    assert!(
        storage::save_as(
            dir.path(),
            &frame([7, 8, 9]),
            storage::ArchiveFormat::Webp,
            &cancel
        )
        .is_err()
    );
    let mut raw = Vec::new();
    image_webp::WebPEncoder::new(&mut raw)
        .encode(
            &vec![20; 550 * 550 * 3],
            550,
            550,
            image_webp::ColorType::Rgb8,
        )
        .unwrap();
    assert!(crate::webp_archive::decode(&raw, &stop).is_err());
    assert_eq!(std::fs::read_dir(dir.path()).unwrap().count(), 2);
    let png_dir = tempfile::tempdir().unwrap();
    let png = storage::save(png_dir.path(), &frame([1, 2, 3]), &stop).unwrap();
    assert_eq!(
        storage::save_as(
            png_dir.path(),
            &frame([1, 2, 3]),
            storage::ArchiveFormat::Webp,
            &stop
        )
        .unwrap()
        .path,
        png.path
    );
}
#[test]
fn settings_v2_migration_and_new_options_roundtrip() {
    let dir = tempfile::tempdir().unwrap();
    let mut old = Preferences::new(dir.path());
    old.schema = 2;
    old.apply_automatic = false;
    let mut value = serde_json::to_value(&old).unwrap();
    for k in [
        "format",
        "minimize_to_tray",
        "close_to_tray",
        "autostart",
        "start_minimized",
    ] {
        value.as_object_mut().unwrap().remove(k);
    }
    std::fs::write(
        dir.path().join("settings.json"),
        serde_json::to_vec(&value).unwrap(),
    )
    .unwrap();
    let (mut p, w) = settings::load(dir.path());
    assert!(w.is_none());
    assert_eq!(p.schema, 4);
    assert!(p.apply_automatic && p.close_to_tray && p.minimize_to_tray);
    assert!(!p.autostart);
    p.apply_automatic = false;
    p.zoom = 17;
    p.start_minimized = false;
    settings::save(dir.path(), &p).unwrap();
    let (q, w) = settings::load(dir.path());
    assert!(w.is_none());
    assert!(!q.apply_automatic && !q.start_minimized);
    assert_eq!(q.zoom, 17);
    assert!(serde_json::to_value(&q).unwrap().get("format").is_none());
    assert!(dir.path().join("settings-B.json").exists());
}
#[test]
fn settings_v3_migration_preserves_user_choices() {
    let dir = tempfile::tempdir().unwrap();
    let mut value = serde_json::to_value(Preferences::new(dir.path())).unwrap();
    let object = value.as_object_mut().unwrap();
    object.remove("start_minimized");
    object.insert("schema".into(), 3.into());
    object.insert("format".into(), "png".into());
    object.insert("start_hidden".into(), true.into());
    object.insert("minimize_to_tray".into(), false.into());
    object.insert("apply_automatic".into(), false.into());
    object.insert("automatic".into(), true.into());
    object.insert("autostart".into(), true.into());
    object.insert("zoom".into(), 50.into());
    std::fs::write(
        dir.path().join("settings.json"),
        serde_json::to_vec(&value).unwrap(),
    )
    .unwrap();
    let (p, warning) = settings::load(dir.path());
    assert!(warning.is_none());
    assert_eq!(p.schema, 4);
    assert!(p.automatic && p.autostart && p.close_to_tray && p.start_minimized);
    assert!(!p.minimize_to_tray && !p.apply_automatic);
    assert_eq!(p.zoom, 50);
    settings::save(dir.path(), &p).unwrap();
    let saved: serde_json::Value =
        serde_json::from_slice(&std::fs::read(dir.path().join("settings.json")).unwrap()).unwrap();
    assert!(saved.get("format").is_none() && saved.get("start_hidden").is_none());
}
#[test]
fn startup_quoted_paths_and_registry_roundtrip() {
    use crate::platform::*;
    let cmd = startup_command(
        std::path::Path::new("C:/测试 文件/EarthLive.exe"),
        std::path::Path::new("D:/照片 目录"),
    )
    .unwrap();
    assert_eq!(
        cmd,
        "\"C:/测试 文件/EarthLive.exe\" --data-dir \"D:/照片 目录\" --startup"
    );
    assert!(
        startup_command(
            std::path::Path::new(&"x".repeat(270)),
            std::path::Path::new("x")
        )
        .is_err()
    );
    let name = format!("EarthLiveV2.Test.{}", std::process::id());
    assert!(startup_value(&name).unwrap().is_none());
    struct Cleanup(String);
    impl Drop for Cleanup {
        fn drop(&mut self) {
            let _ = set_startup_value(&self.0, None);
        }
    }
    let _cleanup = Cleanup(name.clone());
    set_startup_value(&name, Some(&cmd)).unwrap();
    assert_eq!(startup_value(&name).unwrap(), Some(cmd));
    set_startup_value(&name, None).unwrap();
    assert!(startup_value(&name).unwrap().is_none());
}

#[test]
#[ignore = "Requires explicit source and output paths for a real archive validation run"]
fn real_archive_webp_validation() {
    let source = std::path::PathBuf::from(std::env::var_os("EARTHLIVE_TEST_SOURCE").unwrap());
    let output = std::path::PathBuf::from(std::env::var_os("EARTHLIVE_TEST_OUTPUT").unwrap());
    let stop = Stop::default();
    let f = storage::load(&source, &stop).unwrap();
    let start = Instant::now();
    let w = storage::save_as(&output, &f, storage::ArchiveFormat::Webp, &stop).unwrap();
    let encoding = start.elapsed();
    let b = storage::load(&w.path, &stop).unwrap();
    assert_eq!(f.pixels, b.pixels);
    assert_eq!(f.colors, b.colors);
    assert_eq!(f.observation, b.observation);
    println!(
        "SOURCE={} WEBP={} ENCODE={encoding:?} SHA256={}",
        std::fs::metadata(source).unwrap().len(),
        std::fs::metadata(&w.path).unwrap().len(),
        b.fingerprint(&stop).unwrap()
    );
    println!("OUTPUT={}", w.path.display());
}

#[test]
fn current_pi_metadata_and_srgb_roundtrip() {
    let o = domain::metadata(
        br#"{"date":"2026-10-23 07:20:00","file":"PI_H09_20261023_0720_TRC_FLDK_R10_PGPFD.png"}"#,
        2,
    )
    .unwrap();
    assert_eq!(o.satellite, "H09");
    let mut bytes = Vec::new();
    {
        let mut e = png::Encoder::new(&mut bytes, 550, 550);
        e.set_color(png::ColorType::Rgb);
        e.set_depth(png::BitDepth::Eight);
        e.set_source_srgb(png::SrgbRenderingIntent::Perceptual);
        e.write_header()
            .unwrap()
            .write_image_data(&RgbImage::from_pixel(550, 550, Rgb([32, 64, 96])).into_raw())
            .unwrap();
    }
    let decoded = imaging::decode(Cursor::new(bytes), 550).unwrap();
    let mut a = Assembly::new(observation(1)).unwrap();
    a.insert(0, 0, decoded, &Stop::default()).unwrap();
    let frame = a.finish().unwrap();
    let dir = tempfile::tempdir().unwrap();
    let saved = storage::save(dir.path(), &frame, &Stop::default()).unwrap();
    let loaded = storage::load(&saved.path, &Stop::default()).unwrap();
    assert_eq!(loaded.colors, frame.colors);
    assert_eq!(loaded.pixels, frame.pixels);
}

#[test]
fn utc_time_and_names() {
    let a = domain::parse_time("2026-10-22 14:00", true).unwrap();
    let b = domain::parse_time("2026-10-22T06:00:00Z", false).unwrap();
    assert_eq!(a, b);
    let o = Observation::new(a, 2).unwrap();
    assert_eq!(o.stem(), "20261022T060000Z_Hunknown_D531106_1100x1100");
    assert_eq!(
        o.tile_path(1, 0).unwrap(),
        "/2d/550/2026/10/22/060000_1_0.png"
    );
    assert!(o.tile_path(2, 0).is_err());
    assert!(Observation::new(a + chrono::Duration::seconds(1), 2).is_err());
    assert!(Observation::new(a, 3).is_err());
}
#[test]
fn bounded_history_and_metadata_evidence() {
    let t = observation(1).time;
    assert_eq!(
        domain::range(t, t + chrono::Duration::minutes(20), 10, 1, Utc::now())
            .unwrap()
            .len(),
        3
    );
    assert!(domain::range(t, t - chrono::Duration::minutes(10), 10, 1, Utc::now()).is_err());
    assert!(
        domain::range(
            t,
            t + chrono::Duration::days(100),
            10,
            1,
            Utc::now() + chrono::Duration::days(101)
        )
        .is_err()
    );
    let known =
        br#"{"date":"2026-10-22 06:00:00","file":"HS_H09_20261022_0600_B13_FLDK_R20_S0101.DAT"}"#;
    assert_eq!(domain::metadata(known, 1).unwrap().satellite, "H09");
    assert_eq!(
        domain::metadata(br#"{"date":"2026-10-22 06:00:00"}"#, 1)
            .unwrap()
            .satellite,
        "Hunknown"
    );
    assert!(
        domain::metadata(
            br#"{"date":"2026-10-22 06:00:00","file":"HS_H09_20261022_0610_B13"}"#,
            1
        )
        .is_err()
    );
}
#[test]
fn stitching_coordinates_and_missing() {
    let s = Stop::default();
    let mut a = Assembly::new(observation(2)).unwrap();
    a.insert(1, 1, tile([255, 255, 255]), &s).unwrap();
    a.insert(0, 0, tile([255, 0, 0]), &s).unwrap();
    assert!(a.insert(0, 0, tile([255, 0, 0]), &s).is_err());
    a.insert(1, 0, tile([0, 255, 0]), &s).unwrap();
    a.insert(0, 1, tile([0, 0, 255]), &s).unwrap();
    let f = a.finish().unwrap();
    assert_eq!(f.pixels().get_pixel(549, 549).0, [255, 0, 0]);
    assert_eq!(f.pixels().get_pixel(550, 549).0, [0, 255, 0]);
    assert_eq!(f.pixels().get_pixel(549, 550).0, [0, 0, 255]);
    assert_eq!(f.pixels().get_pixel(550, 550).0, [255; 3]);
    assert!(Assembly::new(observation(2)).unwrap().finish().is_err());
}
#[test]
fn png_validation_and_one_bit_black() {
    let mut bytes = Vec::new();
    {
        let mut e = png::Encoder::new(&mut bytes, 550, 550);
        e.set_color(png::ColorType::Grayscale);
        e.set_depth(png::BitDepth::One);
        e.write_header()
            .unwrap()
            .write_image_data(&vec![0; 69 * 550])
            .unwrap();
    }
    let decoded = imaging::decode(Cursor::new(&bytes), 550).unwrap();
    assert_eq!(decoded.pixels.get_pixel(549, 549).0, [0; 3]);
    assert!(imaging::decode(Cursor::new(b"<html>error</html>"), 550).is_err());
    bytes.truncate(bytes.len() - 10);
    assert!(imaging::decode(Cursor::new(bytes), 550).is_err());
    assert!(imaging::decode(Cursor::new(png([1, 2, 3])), 100).is_err());
}
#[test]
fn reject_transparency_and_color_mismatch() {
    let mut bytes = Vec::new();
    {
        let mut e = png::Encoder::new(&mut bytes, 1, 1);
        e.set_color(png::ColorType::Rgba);
        e.set_depth(png::BitDepth::Eight);
        e.write_header()
            .unwrap()
            .write_image_data(&[1, 2, 3, 0])
            .unwrap();
    }
    assert!(imaging::decode(Cursor::new(bytes), 550).is_err());
    let mut a = Assembly::new(observation(2)).unwrap();
    a.insert(0, 0, tile([0; 3]), &Stop::default()).unwrap();
    let mut second = tile([0; 3]);
    second.colors.gamma = Some(45455);
    assert!(a.insert(1, 0, second, &Stop::default()).is_err());
}
#[test]
fn lossless_archive_idempotency_and_revision() {
    let dir = tempfile::tempdir().unwrap();
    let stop = Stop::default();
    let mut f = frame([23, 45, 67]);
    f.colors.gamma = Some(45455);
    f.colors.primaries = Some([31270, 32900, 64000, 33000, 30000, 60000, 15000, 6000]);
    let one = storage::save(dir.path(), &f, &stop).unwrap();
    assert!(!one.existing);
    let independent = imaging::decode(
        std::io::BufReader::new(std::fs::File::open(&one.path).unwrap()),
        8800,
    )
    .unwrap();
    assert_eq!(independent.colors, f.colors);
    assert_eq!(independent.pixels, f.pixels);
    let decoded = storage::load(&one.path, &stop).unwrap();
    assert_eq!(decoded.pixels(), f.pixels());
    assert_eq!(decoded.colors, f.colors);
    assert!(storage::save(dir.path(), &f, &stop).unwrap().existing);
    f.observation.satellite = "H09".into();
    assert!(storage::save(dir.path(), &f, &stop).unwrap().existing);
    f.pixels.put_pixel(1, 1, Rgb([100, 101, 102]));
    let two = storage::save(dir.path(), &f, &stop).unwrap();
    assert!(!two.existing);
    assert_ne!(one.path, two.path);
    assert!(one.path.exists());
    assert_eq!(archive_count(dir.path()), 2);
}
#[test]
fn corrupt_collision_preserved_and_cancel_clean() {
    let dir = tempfile::tempdir().unwrap();
    let f = frame([12, 34, 56]);
    let collision = dir.path().join(format!("{}.png", f.observation().stem()));
    std::fs::write(&collision, b"existing unrelated file").unwrap();
    let saved = storage::save(dir.path(), &f, &Stop::default()).unwrap();
    assert_ne!(collision, saved.path);
    assert_eq!(
        std::fs::read(&collision).unwrap(),
        b"existing unrelated file"
    );
    let dir2 = tempfile::tempdir().unwrap();
    let stop = Stop::default();
    stop.cancel();
    assert!(storage::save(dir2.path(), &f, &stop).is_err());
    assert_eq!(std::fs::read_dir(dir2.path()).unwrap().count(), 0);
    assert!(stop.publish(|| Ok(())).is_err());
}
#[test]
fn archive_lock_blocks_other_writers() {
    let dir = tempfile::tempdir().unwrap();
    let lease = storage::lock(dir.path()).unwrap();
    assert!(storage::lock(dir.path()).is_err());
    drop(lease);
    assert!(storage::lock(dir.path()).is_ok());
}
#[test]
fn zoom_100_is_pixel_exact_and_center_crop() {
    let mut f = frame([0; 3]);
    f.pixels = RgbImage::from_fn(550, 550, |x, y| Rgb([(x % 256) as u8, (y % 256) as u8, 37]));
    let stop = Stop::default();
    assert_eq!(
        imaging::compose(&f, 550, 550, 100, &stop).unwrap(),
        f.pixels
    );
    let crop = imaging::compose(&f, 200, 100, 100, &stop).unwrap();
    assert_eq!(crop.get_pixel(0, 0), f.pixels.get_pixel(175, 225));
    assert_eq!(crop.get_pixel(199, 99), f.pixels.get_pixel(374, 324));
    let small = imaging::compose(&f, 100, 100, 10, &stop).unwrap();
    assert_eq!(small.get_pixel(0, 0).0, [0; 3]);
    assert_eq!(
        imaging::compose(&f, 200, 100, 200, &stop)
            .unwrap()
            .dimensions(),
        (200, 100)
    );
    assert!(imaging::compose(&f, 100, 100, 9, &stop).is_err());
}
#[test]
fn shrinking_averages_checkerboard_in_linear_light() {
    let mut f = frame([0; 3]);
    f.pixels = RgbImage::from_fn(550, 550, |x, y| {
        Rgb([if (x + y) % 2 == 0 { 0 } else { 255 }; 3])
    });
    let out = imaging::compose(&f, 275, 275, 50, &Stop::default()).unwrap();
    assert_eq!(out.get_pixel(100, 100).0, [188; 3]);
}
#[test]
fn bmp_preserves_pixels_and_refuses_overwrite() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("screen.bmp");
    let pixels = RgbImage::from_pixel(19, 13, Rgb([17, 29, 47]));
    storage::write_bmp(&path, &pixels, false, &Stop::default()).unwrap();
    assert_eq!(image::open(&path).unwrap().to_rgb8(), pixels);
    assert!(storage::write_bmp(&path, &pixels, false, &Stop::default()).is_err());
    assert_eq!(std::fs::metadata(&path).unwrap().len(), 54 + 60 * 13);
}
#[test]
fn settings_ab_recovers_without_destroying_good_backup() {
    let dir = tempfile::tempdir().unwrap();
    let mut p = Preferences::new(dir.path());
    settings::save(dir.path(), &p).unwrap();
    assert!(!dir.path().join("settings-B.json").exists());
    p.zoom = 150;
    settings::save(dir.path(), &p).unwrap();
    let b = std::fs::read(dir.path().join("settings-B.json")).unwrap();
    std::fs::write(dir.path().join("settings.json"), b"broken").unwrap();
    let (mut restored, warning) = settings::load(dir.path());
    assert!(warning.is_some());
    assert_eq!(restored.zoom, 100);
    restored.zoom = 125;
    settings::save(dir.path(), &restored).unwrap();
    assert_eq!(
        std::fs::read(dir.path().join("settings-B.json")).unwrap(),
        b
    );
}

struct Reply {
    status: u16,
    bytes: Vec<u8>,
    headers: String,
    delay: Duration,
}
impl Reply {
    fn ok(bytes: Vec<u8>) -> Self {
        Self {
            status: 200,
            bytes,
            headers: String::new(),
            delay: Duration::ZERO,
        }
    }
    fn error(status: u16) -> Self {
        Self {
            status,
            bytes: Vec::new(),
            headers: String::new(),
            delay: Duration::ZERO,
        }
    }
}
struct Server {
    origin: String,
    hits: Arc<Mutex<Vec<String>>>,
    task: tokio::task::JoinHandle<()>,
}
impl Drop for Server {
    fn drop(&mut self) {
        self.task.abort();
    }
}
async fn server(handler: impl Fn(&str) -> Reply + Send + Sync + 'static) -> Server {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let origin = format!("http://{}", listener.local_addr().unwrap());
    let hits = Arc::new(Mutex::new(Vec::new()));
    let recorded = hits.clone();
    let handler = Arc::new(handler);
    let task = tokio::spawn(async move {
        let mut connections = tokio::task::JoinSet::new();
        loop {
            tokio::select! {
                accepted = listener.accept() => {
                    let Ok((mut socket, _)) = accepted else { break };
                    let handler = handler.clone();
                    let recorded = recorded.clone();
                    connections.spawn(async move {
                        let mut bytes = Vec::new();
                        let mut buffer = [0u8; 1024];
                        while bytes.len() < 8192 {
                            let Ok(n) = socket.read(&mut buffer).await else { return };
                            if n == 0 { return; }
                            bytes.extend_from_slice(&buffer[..n]);
                            if bytes.windows(4).any(|x| x == b"\r\n\r\n") { break; }
                        }
                        let request = String::from_utf8_lossy(&bytes);
                        let path = request.split_whitespace().nth(1).unwrap_or("/");
                        recorded.lock().unwrap().push(path.to_string());
                        let reply = handler(path);
                        tokio::time::sleep(reply.delay).await;
                        let header = format!(
                            "HTTP/1.1 {} Test\r\nContent-Length: {}\r\nConnection: close\r\n{}\r\n",
                            reply.status, reply.bytes.len(), reply.headers,
                        );
                        let _ = socket.write_all(header.as_bytes()).await;
                        let _ = socket.write_all(&reply.bytes).await;
                    });
                }
                Some(_) = connections.join_next() => {}
            }
        }
    });
    Server { origin, hits, task }
}
#[tokio::test]
async fn real_http_pipeline_stitches_and_skips_valid_archive() {
    let colors = [
        png([255, 0, 0]),
        png([0, 255, 0]),
        png([0, 0, 255]),
        png([255; 3]),
    ];
    let server = server(move |path| {
        if path == "/latest.json" {
            return Reply::ok(
                br#"{"date":"2026-10-22 06:00:00","file":"HS_H09_20261022_0600_B13"}"#.to_vec(),
            );
        }
        let i = if path.ends_with("_0_0.png") {
            0
        } else if path.ends_with("_1_0.png") {
            1
        } else if path.ends_with("_0_1.png") {
            2
        } else {
            3
        };
        Reply::ok(colors[i].clone())
    })
    .await;
    let dir = tempfile::tempdir().unwrap();
    let stop = Stop::default();
    let source = Source::local(&server.origin).unwrap();
    let report = engine::run(
        source.clone(),
        Selection::Latest(2),
        dir.path(),
        false,
        &stop,
        &|_| {},
    )
    .await
    .unwrap();
    assert_eq!(report.saved, 1);
    assert_eq!(report.last.as_ref().unwrap().extension().unwrap(), "webp");
    let frame = storage::load(&report.last.unwrap(), &stop).unwrap();
    assert_eq!(frame.pixels().get_pixel(550, 0).0, [0, 255, 0]);
    assert_eq!(server.hits.lock().unwrap().len(), 5);
    let report = engine::run(
        source,
        Selection::History(vec![observation(2)]),
        dir.path(),
        false,
        &stop,
        &|_| {},
    )
    .await
    .unwrap();
    assert_eq!(report.existed, 1);
    assert_eq!(server.hits.lock().unwrap().len(), 5);
}
#[tokio::test]
async fn default_capture_reuses_legacy_png_without_network_or_conversion() {
    let dir = tempfile::tempdir().unwrap();
    let stop = Stop::default();
    let f = frame([13, 28, 56]);
    let old = storage::save(dir.path(), &f, &stop).unwrap().path;
    let bytes = std::fs::read(&old).unwrap();
    let server = server(|_| Reply::error(503)).await;
    let report = engine::run(
        Source::local(&server.origin).unwrap(),
        Selection::History(vec![f.observation().clone()]),
        dir.path(),
        false,
        &stop,
        &|_| {},
    )
    .await
    .unwrap();
    assert_eq!(report.existed, 1);
    assert_eq!(report.saved, 0);
    assert_eq!(report.last.unwrap(), old);
    assert_eq!(std::fs::read(&old).unwrap(), bytes);
    assert_eq!(archive_count(dir.path()), 1);
    assert!(server.hits.lock().unwrap().is_empty());
}
#[tokio::test]
async fn missing_tile_never_publishes_partial_frame() {
    let bytes = png([1, 2, 3]);
    let server = server(move |path| {
        if path.ends_with("_1_1.png") {
            Reply::error(404)
        } else {
            Reply::ok(bytes.clone())
        }
    })
    .await;
    let dir = tempfile::tempdir().unwrap();
    let report = engine::run(
        Source::local(&server.origin).unwrap(),
        Selection::History(vec![observation(2)]),
        dir.path(),
        false,
        &Stop::default(),
        &|_| {},
    )
    .await
    .unwrap();
    assert_eq!(report.missing, 1);
    assert_eq!(archive_count(dir.path()), 0);
    assert!(!std::fs::read_dir(dir.path()).unwrap().any(|e| {
        e.unwrap()
            .file_name()
            .to_string_lossy()
            .starts_with(".incomplete")
    }));
}
#[tokio::test]
async fn cancel_slow_request_promptly() {
    let server = server(|_| Reply {
        delay: Duration::from_secs(5),
        ..Reply::ok(vec![0])
    })
    .await;
    let source = Source::local(&server.origin).unwrap();
    let stop = Stop::default();
    let control = stop.clone();
    tokio::spawn(async move {
        tokio::time::sleep(Duration::from_millis(70)).await;
        control.cancel();
    });
    let start = Instant::now();
    assert!(source.get("/slow", 100, &stop).await.is_err());
    assert!(start.elapsed() < Duration::from_millis(700));
}
#[tokio::test]
async fn forbidden_not_retried_and_retry_after_respected() {
    let server = server(|path| {
        if path == "/403" {
            Reply::error(403)
        } else {
            Reply {
                headers: "Retry-After: 120\r\n".into(),
                ..Reply::error(429)
            }
        }
    })
    .await;
    let source = Source::local(&server.origin).unwrap();
    assert!(source.get("/403", 100, &Stop::default()).await.is_err());
    assert!(source.get("/429", 100, &Stop::default()).await.is_err());
    assert_eq!(server.hits.lock().unwrap().len(), 2);
}
#[tokio::test]
async fn response_size_is_bounded() {
    let server = server(|_| Reply::ok(vec![0; 1000])).await;
    assert!(
        Source::local(&server.origin)
            .unwrap()
            .get("/large", 100, &Stop::default())
            .await
            .is_err()
    );
}
#[tokio::test]
async fn history_keeps_successes_around_gap() {
    let bytes = png([5, 6, 7]);
    let server = server(move |path| {
        if path.contains("061000") {
            Reply::error(404)
        } else {
            Reply::ok(bytes.clone())
        }
    })
    .await;
    let start = observation(1).time;
    let frames = domain::range(
        start,
        start + chrono::Duration::minutes(20),
        10,
        1,
        Utc::now(),
    )
    .unwrap();
    let dir = tempfile::tempdir().unwrap();
    let report = engine::run(
        Source::local(&server.origin).unwrap(),
        Selection::History(frames),
        dir.path(),
        false,
        &Stop::default(),
        &|_| {},
    )
    .await
    .unwrap();
    assert_eq!(report.saved, 2);
    assert_eq!(report.missing, 1);
    assert_eq!(archive_count(dir.path()), 2);
}

#[test]
fn autostart_rename_is_scoped_idempotent_and_preserves_conflicts() {
    use crate::platform::*;
    let temp = tempfile::tempdir().unwrap();
    let exe = temp.path().join("LiveEarth.exe");
    let data = temp.path().join("data");
    let old = format!("LiveEarth.Test.Old.{}", std::process::id());
    let new = format!("LiveEarth.Test.New.{}", std::process::id());
    struct Cleanup(Vec<String>);
    impl Drop for Cleanup {
        fn drop(&mut self) {
            for key in &self.0 {
                let _ = set_startup_value(key, None);
            }
        }
    }
    assert!(startup_value(&old).unwrap().is_none() && startup_value(&new).unwrap().is_none());
    let _cleanup = Cleanup(vec![old.clone(), new.clone()]);
    migrate_named_autostart(&old, &new, &exe, &data).unwrap();
    assert!(startup_value(&new).unwrap().is_none());
    let old_command = startup_command(&exe.with_file_name("EarthLive-V2.exe"), &data).unwrap();
    set_startup_value(&old, Some(&old_command)).unwrap();
    migrate_named_autostart(&old, &new, &exe, &temp.path().join("other-data")).unwrap();
    assert!(startup_value(&new).unwrap().is_none());
    assert_eq!(
        startup_value(&old).unwrap().as_deref(),
        Some(old_command.as_str())
    );
    migrate_named_autostart(&old, &new, &exe, &data).unwrap();
    let new_command = startup_command(&exe, &data).unwrap();
    assert_eq!(
        startup_value(&new).unwrap().as_deref(),
        Some(new_command.as_str())
    );
    assert!(startup_value(&old).unwrap().is_none());
    migrate_named_autostart(&old, &new, &exe, &data).unwrap();
    set_startup_value(&old, Some(&old_command)).unwrap();
    set_startup_value(&new, Some("other installation")).unwrap();
    assert!(migrate_named_autostart(&old, &new, &exe, &data).is_err());
    assert_eq!(
        startup_value(&old).unwrap().as_deref(),
        Some(old_command.as_str())
    );
    assert_eq!(
        startup_value(&new).unwrap().as_deref(),
        Some("other installation")
    );
}

#[test]
fn retention_keeps_newest_and_unrelated_files_and_honors_cancel() {
    let dir = tempfile::tempdir().unwrap();
    let stop = Stop::default();
    let mut f = frame([12, 34, 56]);
    let mut paths = Vec::new();
    for _ in 0..3 {
        paths.push(
            storage::save_as(dir.path(), &f, storage::ArchiveFormat::Webp, &stop)
                .unwrap()
                .path,
        );
        f.observation.time += chrono::Duration::minutes(10);
    }
    let unrelated = dir.path().join("family.webp");
    std::fs::write(&unrelated, b"not an archive").unwrap();
    let misleading = dir.path().join(format!("{}.webp", f.observation.stem()));
    std::fs::write(&misleading, b"not an archive either").unwrap();
    let count = std::cell::Cell::new(0);
    storage::trim(dir.path(), 0, &paths[2], &stop, || {
        count.set(count.get() + 1)
    })
    .unwrap();
    assert!(paths.iter().all(|p| p.exists()));
    let cancelled = Stop::default();
    cancelled.cancel();
    assert!(storage::trim(dir.path(), 1, &paths[2], &cancelled, || {}).is_err());
    assert!(paths.iter().all(|p| p.exists()));
    storage::trim(dir.path(), 2, &paths[2], &stop, || {
        count.set(count.get() + 1)
    })
    .unwrap();
    assert!(!paths[0].exists());
    assert!(paths[1].exists() && paths[2].exists());
    assert_eq!(count.get(), 1);
    assert!(unrelated.exists() && misleading.exists());
    // Rename and deletion outside the application are reconciled by native notifications.
    std::fs::remove_file(&paths[1]).unwrap();
    let deadline = Instant::now() + Duration::from_secs(2);
    loop {
        let files = crate::archive_index::files(dir.path(), &stop).unwrap();
        if !files.contains(&paths[1]) {
            break;
        }
        assert!(Instant::now() < deadline);
        std::thread::sleep(Duration::from_millis(10));
    }
    assert_eq!(
        crate::archive_index::files(dir.path(), &stop).unwrap(),
        vec![paths[2].clone()]
    );
}

#[test]
fn guide_preferences_preserve_new_old_and_dismissed_choices() {
    let dir = tempfile::tempdir().unwrap();
    let mut preferences = Preferences::new(dir.path());
    assert!(preferences.show_guide_on_start);
    settings::save(dir.path(), &preferences).unwrap();
    assert!(settings::load(dir.path()).0.show_guide_on_start);
    preferences.show_guide_on_start = false;
    settings::save(dir.path(), &preferences).unwrap();
    assert!(!settings::load(dir.path()).0.show_guide_on_start);
    let mut old = serde_json::to_value(&preferences).unwrap();
    old.as_object_mut().unwrap().remove("show_guide_on_start");
    std::fs::write(
        dir.path().join("settings.json"),
        serde_json::to_vec(&old).unwrap(),
    )
    .unwrap();
    let (loaded, warning) = settings::load(dir.path());
    assert!(warning.is_none());
    assert!(
        !loaded.show_guide_on_start,
        "an existing user must not get a forced guide"
    );
    assert_eq!(loaded.grid, preferences.grid);
    assert_eq!(loaded.photos, preferences.photos);
}

#[tokio::test]
async fn redirects_follow_same_origin_and_reject_cross_origin_or_loops() {
    let sink = server(|_| Reply::ok(b"should not be requested".to_vec())).await;
    let other = format!("{}/blocked", sink.origin);
    let source = server(move |path| match path {
        "/start" => Reply {
            status: 302,
            headers: "Location: /finish\r\n".into(),
            bytes: vec![],
            delay: Duration::ZERO,
        },
        "/finish" => Reply::ok(b"ok".to_vec()),
        "/cross" => Reply {
            status: 302,
            headers: format!("Location: {other}\r\n"),
            bytes: vec![],
            delay: Duration::ZERO,
        },
        _ => Reply {
            status: 302,
            headers: "Location: /loop\r\n".into(),
            bytes: vec![],
            delay: Duration::ZERO,
        },
    })
    .await;
    let client = Source::local(&source.origin).unwrap();
    let stop = Stop::default();
    assert_eq!(client.get("/start", 32, &stop).await.unwrap(), b"ok");
    for path in ["/cross", "/loop"] {
        let error = client.get(path, 32, &stop).await.unwrap_err();
        assert_eq!(
            crate::service::classify(&error),
            crate::service::Fault::Transient
        );
    }
    assert!(
        sink.hits.lock().unwrap().is_empty(),
        "a refused redirect must send no cross-origin request"
    );
    let error = anyhow::Error::new(crate::net::HttpFailure(302));
    assert_eq!(
        crate::service::classify(&error),
        crate::service::Fault::Transient
    );
}

#[test]
fn webp_metadata_is_bounded_before_reading_oversized_chunks() {
    use std::{
        cell::Cell,
        io::{self, BufRead, Read, Seek, SeekFrom},
        rc::Rc,
    };
    struct ObservedRead {
        cursor: Cursor<Vec<u8>>,
        largest: Rc<Cell<usize>>,
    }
    impl Read for ObservedRead {
        fn read(&mut self, buffer: &mut [u8]) -> io::Result<usize> {
            self.largest.set(self.largest.get().max(buffer.len()));
            Read::read(&mut self.cursor, buffer)
        }
    }
    impl BufRead for ObservedRead {
        fn fill_buf(&mut self) -> io::Result<&[u8]> {
            self.cursor.fill_buf()
        }
        fn consume(&mut self, count: usize) {
            self.cursor.consume(count);
        }
    }
    impl Seek for ObservedRead {
        fn seek(&mut self, pos: SeekFrom) -> io::Result<u64> {
            self.cursor.seek(pos)
        }
    }
    let mut bytes = b"RIFF\0\0\0\0WEBPVP8X".to_vec();
    bytes.extend_from_slice(&10u32.to_le_bytes());
    bytes.extend_from_slice(&[4, 0, 0, 0, 0, 0, 0, 0, 0, 0]);
    bytes.extend_from_slice(b"VP8L");
    bytes.extend_from_slice(&5u32.to_le_bytes());
    bytes.extend_from_slice(&[0x2f, 0, 0, 0, 0, 0]);
    bytes.extend_from_slice(b"XMP ");
    bytes.extend_from_slice(&(4u32 * 1024 * 1024).to_le_bytes());
    let size = (bytes.len() - 8) as u32;
    bytes[4..8].copy_from_slice(&size.to_le_bytes());
    let largest = Rc::new(Cell::new(0));
    let reader = ObservedRead {
        cursor: Cursor::new(bytes.clone()),
        largest: largest.clone(),
    };
    let error = crate::webp_archive::read_note(reader).unwrap_err();
    assert!(matches!(
        error.downcast_ref::<image_webp::DecodingError>(),
        Some(image_webp::DecodingError::MemoryLimitExceeded)
    ));
    assert!(
        largest.get() <= 131072,
        "must reject before requesting a large read buffer"
    );
    let error = crate::webp_archive::decode(&bytes, &Stop::default())
        .err()
        .unwrap();
    assert!(matches!(
        error.downcast_ref::<image_webp::DecodingError>(),
        Some(image_webp::DecodingError::MemoryLimitExceeded)
    ));
}

#[tokio::test]
async fn invalid_source_payloads_are_retryable_and_never_saved() {
    use crate::service::{Fault, classify};
    use chrono::Timelike;
    let future = (Utc::now() + chrono::Duration::hours(1))
        .with_minute(0)
        .unwrap()
        .with_second(0)
        .unwrap()
        .with_nanosecond(0)
        .unwrap();
    let future_json =
        format!(r#"{{"date":"{}"}}"#, future.format("%Y-%m-%d %H:%M:%S")).into_bytes();
    for body in [b"temporary invalid metadata".to_vec(), future_json] {
        let server = server(move |_| Reply::ok(body.clone())).await;
        let error = Source::local(&server.origin)
            .unwrap()
            .latest(1, &Stop::default())
            .await
            .unwrap_err();
        assert_eq!(classify(&error), Fault::Transient);
    }
    let dir = tempfile::tempdir().unwrap();
    let server = server(|_| Reply::ok(b"temporary invalid PNG".to_vec())).await;
    let error = engine::run(
        Source::local(&server.origin).unwrap(),
        Selection::History(vec![observation(1)]),
        dir.path(),
        false,
        &Stop::default(),
        &|_| {},
    )
    .await
    .unwrap_err();
    assert_eq!(classify(&error), Fault::Transient);
    assert_eq!(archive_count(dir.path()), 0);
}
