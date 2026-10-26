use std::{env, fs, io::Cursor, path::PathBuf};
fn put16(b: &mut [u8], p: usize, v: u16) {
    b[p..p + 2].copy_from_slice(&v.to_le_bytes());
}
fn put32(b: &mut [u8], p: usize, v: u32) {
    b[p..p + 4].copy_from_slice(&v.to_le_bytes());
}
fn directory(b: &mut Vec<u8>, ids: &[u32]) -> usize {
    let p = b.len();
    b.resize(p + 16 + ids.len() * 8, 0);
    put16(b, p + 14, ids.len() as u16);
    for (i, id) in ids.iter().enumerate() {
        put32(b, p + 16 + i * 8, *id);
    }
    p
}
type ResourceGroup = (u32, Vec<(u32, Vec<u8>)>);
fn resource_object(items: &[ResourceGroup]) -> Vec<u8> {
    let mut data = Vec::new();
    let mut reloc = Vec::new();
    let root = directory(&mut data, &items.iter().map(|v| v.0).collect::<Vec<_>>());
    for (i, (_, resources)) in items.iter().enumerate() {
        let names = directory(
            &mut data,
            &resources.iter().map(|v| v.0).collect::<Vec<_>>(),
        );
        put32(&mut data, root + 20 + i * 8, 0x80000000 | names as u32);
        for (j, (_, payload)) in resources.iter().enumerate() {
            let lang = directory(&mut data, &[0x0409]);
            put32(&mut data, names + 20 + j * 8, 0x80000000 | lang as u32);
            let entry = data.len();
            data.resize(entry + 16, 0);
            put32(&mut data, lang + 20, entry as u32);
            while !data.len().is_multiple_of(4) {
                data.push(0);
            }
            let offset = data.len();
            put32(&mut data, entry, offset as u32);
            put32(&mut data, entry + 4, payload.len() as u32);
            reloc.push(entry as u32);
            data.extend_from_slice(payload);
            while !data.len().is_multiple_of(4) {
                data.push(0);
            }
        }
    }
    let raw = 60;
    let relocation = raw + data.len();
    let symbols = relocation + reloc.len() * 10;
    let mut obj = vec![0u8; raw];
    put16(&mut obj, 0, 0x8664);
    put16(&mut obj, 2, 1);
    put32(&mut obj, 8, symbols as u32);
    put32(&mut obj, 12, 1);
    obj[20..28].copy_from_slice(b".rsrc\0\0\0");
    put32(&mut obj, 36, data.len() as u32);
    put32(&mut obj, 40, raw as u32);
    put32(&mut obj, 44, relocation as u32);
    put16(&mut obj, 52, reloc.len() as u16);
    put32(&mut obj, 56, 0x40300040);
    obj.extend_from_slice(&data);
    for p in reloc {
        obj.extend_from_slice(&p.to_le_bytes());
        obj.extend_from_slice(&0u32.to_le_bytes());
        obj.extend_from_slice(&3u16.to_le_bytes());
    }
    obj.extend_from_slice(b".rsrc\0\0\0");
    obj.extend_from_slice(&0u32.to_le_bytes());
    obj.extend_from_slice(&1u16.to_le_bytes());
    obj.extend_from_slice(&0u16.to_le_bytes());
    obj.extend_from_slice(&[3, 0]);
    obj.extend_from_slice(&4u32.to_le_bytes());
    obj
}
fn version_block(
    key: &str,
    kind: u16,
    value_length: u16,
    value: &[u8],
    children: &[Vec<u8>],
) -> Vec<u8> {
    let mut bytes = vec![0; 6];
    put16(&mut bytes, 2, value_length);
    put16(&mut bytes, 4, kind);
    for c in key.encode_utf16().chain(Some(0)) {
        bytes.extend_from_slice(&c.to_le_bytes());
    }
    while !bytes.len().is_multiple_of(4) {
        bytes.push(0);
    }
    bytes.extend_from_slice(value);
    for child in children {
        while !bytes.len().is_multiple_of(4) {
            bytes.push(0);
        }
        bytes.extend_from_slice(child);
    }
    let length = bytes.len() as u16;
    put16(&mut bytes, 0, length);
    bytes
}
fn version_resource([major, minor, patch]: [u16; 3]) -> Vec<u8> {
    let numeric = format!("{major}.{minor}.{patch}.0");
    let display = env::var("CARGO_PKG_VERSION").unwrap();
    let strings = [
        ("FileDescription", "LiveEarth"),
        ("ProductName", "LiveEarth"),
        ("OriginalFilename", "LiveEarth.exe"),
        ("InternalName", "LiveEarth"),
        ("FileVersion", numeric.as_str()),
        ("ProductVersion", display.as_str()),
    ]
    .map(|(key, value)| {
        let wide: Vec<u16> = value.encode_utf16().chain(Some(0)).collect();
        let data: Vec<u8> = wide.iter().flat_map(|c| c.to_le_bytes()).collect();
        version_block(key, 1, wide.len() as u16, &data, &[])
    });
    let table = version_block("040904B0", 1, 0, &[], &strings);
    let string_info = version_block("StringFileInfo", 1, 0, &[], &[table]);
    let translation = version_block("Translation", 0, 4, &[9, 4, 176, 4], &[]);
    let var_info = version_block("VarFileInfo", 1, 0, &[], &[translation]);
    let fixed: Vec<u8> = [
        0xFEEF04BDu32,
        0x10000,
        (u32::from(major) << 16) | u32::from(minor),
        u32::from(patch) << 16,
        (u32::from(major) << 16) | u32::from(minor),
        u32::from(patch) << 16,
        0x3F,
        0,
        0x40004,
        1,
        0,
        0,
        0,
    ]
    .into_iter()
    .flat_map(u32::to_le_bytes)
    .collect();
    version_block(
        "VS_VERSION_INFO",
        0,
        fixed.len() as u16,
        &fixed,
        &[string_info, var_info],
    )
}
fn main() {
    let version = ["MAJOR", "MINOR", "PATCH"].map(|part| {
        env::var(format!("CARGO_PKG_VERSION_{part}"))
            .unwrap()
            .parse::<u16>()
            .expect("Windows 版本的每个数字必须在 0–65535 范围内")
    });
    println!("cargo:rerun-if-changed=assets/app-icon.png");
    let out = PathBuf::from(env::var_os("OUT_DIR").unwrap());
    let original = image::open("assets/app-icon.png").expect("selected app icon");
    let mut icons = Vec::new();
    for size in [16u32, 20, 24, 32, 40, 48, 64, 128, 256] {
        let resized = original.resize_exact(size, size, image::imageops::FilterType::Lanczos3);
        let mut bytes = Cursor::new(Vec::new());
        resized
            .write_to(&mut bytes, image::ImageFormat::Png)
            .unwrap();
        let bytes = bytes.into_inner();
        if size == 256 {
            fs::write(out.join("window-icon.png"), &bytes).unwrap();
        }
        icons.push((size, bytes));
    }
    let mut group = vec![0u8; 6 + icons.len() * 14];
    put16(&mut group, 2, 1);
    put16(&mut group, 4, icons.len() as u16);
    let mut ico = vec![0u8; 6 + icons.len() * 16];
    put16(&mut ico, 2, 1);
    put16(&mut ico, 4, icons.len() as u16);
    for (i, (size, bytes)) in icons.iter().enumerate() {
        let p = 6 + i * 14;
        group[p] = *size as u8;
        group[p + 1] = *size as u8;
        put16(&mut group, p + 4, 1);
        put16(&mut group, p + 6, 32);
        put32(&mut group, p + 8, bytes.len() as u32);
        put16(&mut group, p + 12, (i + 1) as u16);
        let p = 6 + i * 16;
        ico[p] = *size as u8;
        ico[p + 1] = *size as u8;
        put16(&mut ico, p + 4, 1);
        put16(&mut ico, p + 6, 32);
        put32(&mut ico, p + 8, bytes.len() as u32);
        let offset = ico.len();
        put32(&mut ico, p + 12, offset as u32);
        ico.extend_from_slice(bytes);
    }
    fs::write(out.join("app-icon.ico"), ico).unwrap();
    if env::var("CARGO_CFG_TARGET_OS").unwrap() == "windows" {
        assert_eq!(env::var("CARGO_CFG_TARGET_ARCH").unwrap(), "x86_64");
        let resources = vec![
            (
                3,
                icons
                    .into_iter()
                    .enumerate()
                    .map(|(i, (_, b))| ((i + 1) as u32, b))
                    .collect(),
            ),
            (14, vec![(101, group)]),
            (16, vec![(1, version_resource(version))]),
        ];
        let obj = out.join("icon-resource.o");
        fs::write(&obj, resource_object(&resources)).unwrap();
        println!("cargo:rustc-link-arg-bin=LiveEarth={}", obj.display());
    }
}
