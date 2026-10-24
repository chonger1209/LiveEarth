use anyhow::{Result, ensure};
use std::path::Path;

#[cfg(windows)]
pub fn primary_size() -> Result<(u32, u32)> {
    use windows::Win32::UI::{
        HiDpi::{DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2, SetThreadDpiAwarenessContext},
        WindowsAndMessaging::{GetSystemMetrics, SM_CXSCREEN, SM_CYSCREEN},
    };
    unsafe {
        let old = SetThreadDpiAwarenessContext(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2);
        let width = GetSystemMetrics(SM_CXSCREEN);
        let height = GetSystemMetrics(SM_CYSCREEN);
        if !old.0.is_null() {
            SetThreadDpiAwarenessContext(old);
        }
        ensure!(width > 0 && height > 0, "无法读取主屏物理尺寸");
        Ok((width as u32, height as u32))
    }
}
#[cfg(not(windows))]
pub fn primary_size() -> Result<(u32, u32)> {
    anyhow::bail!("当前版本仅提供 Windows 桌面集成")
}

#[cfg(windows)]
pub fn apply(path: &Path) -> Result<()> {
    use std::os::windows::ffi::OsStrExt;
    use windows::{
        Win32::{
            System::Com::{
                CLSCTX_ALL, COINIT_APARTMENTTHREADED, CoCreateInstance, CoInitializeEx,
                CoUninitialize,
            },
            UI::Shell::{DWPOS_CENTER, DesktopWallpaper, IDesktopWallpaper},
        },
        core::PCWSTR,
    };
    ensure!(path.is_file(), "壁纸文件不存在");
    let path = std::fs::canonicalize(path)?;
    let wide: Vec<u16> = path.as_os_str().encode_wide().chain(Some(0)).collect();
    unsafe {
        CoInitializeEx(None, COINIT_APARTMENTTHREADED).ok()?;
        struct ComGuard;
        impl Drop for ComGuard {
            fn drop(&mut self) {
                unsafe {
                    CoUninitialize();
                }
            }
        }
        let _guard = ComGuard;
        let desktop: IDesktopWallpaper = CoCreateInstance(&DesktopWallpaper, None, CLSCTX_ALL)?;
        let old = desktop.GetPosition()?;
        desktop.SetPosition(DWPOS_CENTER)?;
        if let Err(error) = desktop.SetWallpaper(PCWSTR::null(), PCWSTR(wide.as_ptr())) {
            let _ = desktop.SetPosition(old);
            return Err(error.into());
        }
    }
    Ok(())
}
#[cfg(not(windows))]
pub fn apply(_: &Path) -> Result<()> {
    anyhow::bail!("当前版本仅提供 Windows 桌面集成")
}
