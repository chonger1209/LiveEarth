//! Windows integration: a separate message loop keeps tray/power events alive.
use crate::service::{Command, Handle};
use anyhow::{Context, Result, ensure};
use std::{
    path::Path,
    sync::{Arc, Mutex},
    thread::JoinHandle,
};
pub struct Tray {
    hwnd: usize,
    join: Option<JoinHandle<()>>,
}
pub fn open_directory(path: &Path) -> Result<()> {
    ensure!(path.is_dir(), "照片目录不存在：{}", path.display());
    std::process::Command::new("explorer.exe")
        .arg(path)
        .spawn()
        .context("无法打开照片目录")?;
    Ok(())
}
pub fn startup_command(exe: &Path, data: &Path) -> Result<String> {
    let exe = exe.to_str().context("程序路径不是有效 Unicode")?;
    let data = data.to_str().context("数据路径不是有效 Unicode")?;
    ensure!(
        !exe.contains('"') && !data.contains('"'),
        "启动路径包含引号"
    );
    let command = format!("\"{exe}\" --data-dir \"{data}\" --startup");
    ensure!(
        command.encode_utf16().count() <= 260,
        "启动命令超过 Windows 的 260 字符限制"
    );
    Ok(command)
}
#[cfg(windows)]
mod native {
    use super::*;
    use std::{cell::RefCell, sync::mpsc};
    use windows::{
        Win32::{
            Foundation::{ERROR_CANCELLED, ERROR_FILE_NOT_FOUND, HWND, LPARAM, LRESULT, WPARAM},
            System::{Com::*, LibraryLoader::GetModuleHandleW, Registry::*},
            UI::{Shell::*, WindowsAndMessaging::*},
        },
        core::{PCWSTR, w},
    };
    const CALLBACK: u32 = WM_APP + 10;
    pub fn window_visible(main: usize) -> bool {
        let hwnd = HWND(main as *mut _);
        unsafe { IsWindowVisible(hwnd).as_bool() && !IsIconic(hwnd).as_bool() }
    }
    pub fn pick_directory(
        owner: usize,
        initial: std::path::PathBuf,
        wake: impl FnOnce() + Send + 'static,
    ) -> Result<mpsc::Receiver<Result<Option<std::path::PathBuf>>>> {
        let (tx, rx) = mpsc::channel();
        std::thread::Builder::new()
            .name("folder-picker".into())
            .spawn(move || {
                let result = (|| -> Result<Option<std::path::PathBuf>> {
                    unsafe {
                        CoInitializeEx(None, COINIT_APARTMENTTHREADED).ok()?;
                        struct Com;
                        impl Drop for Com {
                            fn drop(&mut self) {
                                unsafe {
                                    CoUninitialize();
                                }
                            }
                        }
                        let _com = Com;
                        let dialog: IFileOpenDialog =
                            CoCreateInstance(&FileOpenDialog, None, CLSCTX_INPROC_SERVER)?;
                        dialog.SetOptions(
                            dialog.GetOptions()?
                                | FOS_PICKFOLDERS
                                | FOS_FORCEFILESYSTEM
                                | FOS_PATHMUSTEXIST
                                | FOS_NOCHANGEDIR,
                        )?;
                        dialog.SetTitle(w!("选择照片目录"))?;
                        let path = wide(&initial.to_string_lossy());
                        if let Ok(folder) = SHCreateItemFromParsingName::<_, _, IShellItem>(
                            PCWSTR(path.as_ptr()),
                            None,
                        ) {
                            let _ = dialog.SetFolder(&folder);
                        }
                        if let Err(e) = dialog.Show(Some(HWND(owner as *mut _))) {
                            if e.code() == windows::core::HRESULT::from_win32(ERROR_CANCELLED.0) {
                                return Ok(None);
                            }
                            return Err(e.into());
                        }
                        let name = dialog.GetResult()?.GetDisplayName(SIGDN_FILESYSPATH)?;
                        let path = name.to_string();
                        CoTaskMemFree(Some(name.0.cast()));
                        Ok(Some(path?.into()))
                    }
                })();
                let _ = tx.send(result);
                wake();
            })?;
        Ok(rx)
    }
    const KEY: &windows::core::PCWSTR = &w!("Software\\Microsoft\\Windows\\CurrentVersion\\Run");
    fn wide(s: &str) -> Vec<u16> {
        s.encode_utf16().chain(Some(0)).collect()
    }
    struct Key(HKEY);
    impl Drop for Key {
        fn drop(&mut self) {
            unsafe {
                let _ = RegCloseKey(self.0);
            }
        }
    }
    pub fn startup_value(name: &str) -> Result<Option<String>> {
        let name = wide(name);
        let mut bytes = vec![0u16; 2048];
        let mut size = (bytes.len() * 2) as u32;
        let result = unsafe {
            RegGetValueW(
                HKEY_CURRENT_USER,
                *KEY,
                PCWSTR(name.as_ptr()),
                RRF_RT_REG_SZ,
                None,
                Some(bytes.as_mut_ptr().cast()),
                Some(&mut size),
            )
        };
        if result == ERROR_FILE_NOT_FOUND {
            return Ok(None);
        }
        result.ok().context("读取自启动设置失败")?;
        bytes.truncate(size as usize / 2);
        while bytes.last() == Some(&0) {
            bytes.pop();
        }
        Ok(Some(String::from_utf16(&bytes)?))
    }
    pub fn set_startup_value(name: &str, value: Option<&str>) -> Result<()> {
        let mut key = HKEY::default();
        unsafe {
            RegCreateKeyExW(
                HKEY_CURRENT_USER,
                *KEY,
                None,
                None,
                REG_OPTION_NON_VOLATILE,
                KEY_SET_VALUE,
                None,
                &mut key,
                None,
            )
            .ok()?;
        }
        let key = Key(key);
        let name = wide(name);
        if let Some(value) = value {
            let value = wide(value);
            let bytes: Vec<u8> = value.iter().flat_map(|v| v.to_le_bytes()).collect();
            unsafe {
                RegSetValueExW(key.0, PCWSTR(name.as_ptr()), None, REG_SZ, Some(&bytes)).ok()?;
            }
        } else {
            let result = unsafe { RegDeleteValueW(key.0, PCWSTR(name.as_ptr())) };
            if result != ERROR_FILE_NOT_FOUND {
                result.ok()?;
            }
        }
        Ok(())
    }
    pub fn autostart_enabled(exe: &Path, data: &Path) -> Result<bool> {
        Ok(startup_value("LiveEarth")?
            .is_some_and(|v| startup_command(exe, data).is_ok_and(|c| c == v)))
    }
    pub struct AutostartChange {
        name: String,
        previous: Option<String>,
        applied: Option<String>,
    }
    impl AutostartChange {
        pub fn rollback(self) -> Result<()> {
            ensure!(
                startup_value(&self.name)? == self.applied,
                "自启动项已被其他操作更改，未覆盖较新的登记"
            );
            set_startup_value(&self.name, self.previous.as_deref())
        }
    }
    pub(crate) fn replace_named_autostart(
        name: &str,
        command: Option<String>,
    ) -> Result<AutostartChange> {
        let previous = startup_value(name)?;
        set_startup_value(name, command.as_deref())?;
        Ok(AutostartChange {
            name: name.into(),
            previous,
            applied: command,
        })
    }
    pub fn set_autostart(enable: bool, exe: &Path, data: &Path) -> Result<AutostartChange> {
        let command = enable.then(|| startup_command(exe, data)).transpose()?;
        // Explicit user action may replace the single LiveEarth startup entry.
        // Automatic migration below deliberately retains its ownership checks.
        replace_named_autostart("LiveEarth", command)
    }
    pub fn migrate_autostart(exe: &Path, data: &Path) -> Result<()> {
        migrate_named_autostart("EarthLiveV2", "LiveEarth", exe, data)
    }
    pub(crate) fn migrate_named_autostart(
        old_name: &str,
        new_name: &str,
        exe: &Path,
        data: &Path,
    ) -> Result<()> {
        if !exe
            .file_name()
            .is_some_and(|n| n.to_string_lossy().eq_ignore_ascii_case("LiveEarth.exe"))
        {
            return Ok(());
        }
        let old = startup_command(&exe.with_file_name("EarthLive-V2.exe"), data)?;
        if startup_value(old_name)?.as_deref() != Some(&old) {
            return Ok(());
        }
        let new = startup_command(exe, data)?;
        let previous = startup_value(new_name)?;
        ensure!(
            previous.as_ref().is_none_or(|v| v == &new),
            "LiveEarth 自启动项指向其他安装位置，未覆盖"
        );
        set_startup_value(new_name, Some(&new))?;
        if let Err(error) = set_startup_value(old_name, None) {
            set_startup_value(new_name, previous.as_deref())
                .context("迁移失败，恢复自启动项也失败")?;
            return Err(error.context("移除旧自启动项失败"));
        }
        Ok(())
    }
    struct State {
        main: usize,
        icon: usize,
        handle: Handle,
        ctx: eframe::egui::Context,
        taskbar: u32,
        tip: Mutex<String>,
        exit_shown: std::sync::atomic::AtomicBool,
    }
    thread_local! {static STATE:RefCell<Option<Arc<State>>>=const{RefCell::new(None)};}
    fn notify(hwnd: HWND, state: &State, op: NOTIFY_ICON_MESSAGE, tip: &str) -> bool {
        let mut n = NOTIFYICONDATAW {
            cbSize: std::mem::size_of::<NOTIFYICONDATAW>() as u32,
            hWnd: hwnd,
            uID: 1,
            uFlags: NIF_MESSAGE | NIF_ICON | NIF_TIP,
            uCallbackMessage: CALLBACK,
            hIcon: HICON(state.icon as *mut _),
            ..Default::default()
        };
        let v: Vec<u16> = tip.encode_utf16().take(127).collect();
        n.szTip[..v.len()].copy_from_slice(&v);
        unsafe { Shell_NotifyIconW(op, &n).as_bool() }
    }
    fn show(state: &State) {
        use eframe::egui::ViewportCommand;
        state.ctx.send_viewport_cmd(ViewportCommand::Visible(true));
        state
            .ctx
            .send_viewport_cmd(ViewportCommand::Minimized(false));
        state.ctx.send_viewport_cmd(ViewportCommand::Focus);
        state.ctx.request_repaint();
        unsafe {
            let _ = ShowWindowAsync(HWND(state.main as *mut _), SW_RESTORE);
            let _ = SetForegroundWindow(HWND(state.main as *mut _));
        }
    }
    fn menu(hwnd: HWND, state: &State) {
        unsafe {
            let Ok(menu) = CreatePopupMenu() else { return };
            let enabled = state.handle.snapshot().enabled;
            let _ = AppendMenuW(menu, MF_STRING, 1, w!("打开 LiveEarth"));
            let _ = AppendMenuW(menu, MF_STRING, 2, w!("立即检查最新影像"));
            let _ = AppendMenuW(
                menu,
                MF_STRING,
                3,
                if enabled {
                    w!("停止定时抓取")
                } else {
                    w!("开始定时抓取")
                },
            );
            let _ = AppendMenuW(menu, MF_STRING, 4, w!("打开照片目录"));
            let _ = AppendMenuW(menu, MF_SEPARATOR, 0, PCWSTR::null());
            let _ = AppendMenuW(menu, MF_STRING, 5, w!("退出程序"));
            let mut pos = windows::Win32::Foundation::POINT::default();
            let _ = GetCursorPos(&mut pos);
            let _ = SetForegroundWindow(hwnd);
            let action = TrackPopupMenu(
                menu,
                TPM_RETURNCMD | TPM_RIGHTBUTTON | TPM_NONOTIFY,
                pos.x,
                pos.y,
                Some(0),
                hwnd,
                None,
            )
            .0;
            let _ = DestroyMenu(menu);
            let _ = PostMessageW(Some(hwnd), WM_NULL, WPARAM(0), LPARAM(0));
            match action {
                1 => show(state),
                2 => state.handle.send(Command::CheckNow),
                3 => state.handle.send(Command::Toggle),
                4 => {
                    let Some(prefs) = state.handle.snapshot().preferences else {
                        return;
                    };
                    if let Err(e) = open_directory(&prefs.photos) {
                        state.handle.send(Command::Record(format!("{e:#}"), true));
                    }
                }
                5 => {
                    state.handle.send(Command::Shutdown);
                }
                _ => {}
            }
        }
    }
    unsafe extern "system" fn proc(
        hwnd: HWND,
        msg: u32,
        wparam: WPARAM,
        lparam: LPARAM,
    ) -> LRESULT {
        let state = STATE.with(|v| v.borrow().clone());
        if let Some(state) = state {
            if msg == state.taskbar {
                notify(hwnd, &state, NIM_ADD, "LiveEarth");
                return LRESULT(0);
            }
            match msg {
                CALLBACK => {
                    match lparam.0 as u32 {
                        WM_LBUTTONUP => show(&state),
                        WM_RBUTTONUP | WM_CONTEXTMENU => menu(hwnd, &state),
                        _ => {}
                    }
                    return LRESULT(0);
                }
                WM_TIMER => {
                    let snap = state.handle.snapshot();
                    let tip = format!(
                        "LiveEarth · {} · {}",
                        if snap.enabled {
                            "定时运行"
                        } else {
                            "已停止"
                        },
                        snap.status
                    );
                    let mut previous = state.tip.lock().unwrap_or_else(|p| p.into_inner());
                    if *previous != tip {
                        notify(hwnd, &state, NIM_MODIFY, &tip);
                        *previous = tip;
                    }
                    drop(previous);
                    let main = HWND(state.main as *mut _);
                    // Hidden/occluded windows may not receive another egui frame.
                    // Reveal the closing window once so winit can finish its normal teardown.
                    if snap.exiting
                        && !state
                            .exit_shown
                            .swap(true, std::sync::atomic::Ordering::SeqCst)
                    {
                        show(&state);
                    }
                    if snap.exited {
                        state
                            .ctx
                            .send_viewport_cmd(eframe::egui::ViewportCommand::Close);
                        unsafe {
                            let _ = PostMessageW(Some(main), WM_CLOSE, WPARAM(0), LPARAM(0));
                        }
                    } else if snap
                        .preferences
                        .as_ref()
                        .is_some_and(|p| p.minimize_to_tray)
                        && unsafe { IsIconic(main).as_bool() }
                    {
                        state
                            .ctx
                            .send_viewport_cmd(eframe::egui::ViewportCommand::Visible(false));
                        unsafe {
                            let _ = ShowWindowAsync(main, SW_HIDE);
                        }
                    }
                    return LRESULT(0);
                }
                WM_POWERBROADCAST if wparam.0 == PBT_APMRESUMEAUTOMATIC as usize => {
                    // PBT_APMRESUMEAUTOMATIC: also delivered on unattended wake.
                    state.handle.send(Command::Resume);
                    return LRESULT(1);
                }
                WM_QUERYENDSESSION => {
                    state.handle.send(Command::Shutdown);
                    return LRESULT(1);
                }
                WM_ENDSESSION if wparam.0 != 0 => {
                    state.handle.send(Command::Shutdown);
                    return LRESULT(0);
                }
                WM_CLOSE => {
                    unsafe {
                        let _ = DestroyWindow(hwnd);
                    }
                    return LRESULT(0);
                }
                WM_DESTROY => {
                    notify(hwnd, &state, NIM_DELETE, "");
                    unsafe {
                        PostQuitMessage(0);
                    }
                    return LRESULT(0);
                }
                _ => {}
            }
        }
        unsafe { DefWindowProcW(hwnd, msg, wparam, lparam) }
    }
    impl Tray {
        pub fn start(main: usize, handle: Handle, ctx: eframe::egui::Context) -> Result<Self> {
            let (tx, rx) = mpsc::sync_channel(1);
            let join = std::thread::spawn(move || {
                let setup = (|| -> Result<(HWND, HICON, Vec<u16>)> {
                    let module = unsafe { GetModuleHandleW(None)? };
                    let class = wide(&format!("LiveEarth.Tray.{}", std::process::id()));
                    let wc = WNDCLASSW {
                        lpfnWndProc: Some(proc),
                        hInstance: module.into(),
                        lpszClassName: PCWSTR(class.as_ptr()),
                        ..Default::default()
                    };
                    ensure!(unsafe { RegisterClassW(&wc) } != 0, "创建托盘窗口类失败");
                    let icon = HICON(
                        unsafe {
                            LoadImageW(
                                Some(module.into()),
                                PCWSTR(101usize as *const u16),
                                IMAGE_ICON,
                                32,
                                32,
                                IMAGE_FLAGS(0),
                            )?
                        }
                        .0,
                    );
                    let state = Arc::new(State {
                        main,
                        icon: icon.0 as usize,
                        handle,
                        ctx,
                        taskbar: unsafe { RegisterWindowMessageW(w!("TaskbarCreated")) },
                        tip: Mutex::new(String::new()),
                        exit_shown: std::sync::atomic::AtomicBool::new(false),
                    });
                    STATE.with(|v| *v.borrow_mut() = Some(state.clone()));
                    let hwnd = unsafe {
                        CreateWindowExW(
                            WINDOW_EX_STYLE(0),
                            PCWSTR(class.as_ptr()),
                            w!("LiveEarth background"),
                            WINDOW_STYLE(0),
                            0,
                            0,
                            0,
                            0,
                            None,
                            None,
                            Some(module.into()),
                            None,
                        )?
                    };
                    if !notify(hwnd, &state, NIM_ADD, "LiveEarth · 就绪") {
                        unsafe {
                            let _ = DestroyWindow(hwnd);
                            let _ = DestroyIcon(icon);
                        }
                        anyhow::bail!("Windows 托盘图标注册失败");
                    }
                    unsafe {
                        SetTimer(Some(hwnd), 1, 500, None);
                    }
                    Ok((hwnd, icon, class))
                })();
                match setup {
                    Ok((hwnd, icon, class)) => {
                        let _ = tx.send(Ok(hwnd.0 as usize));
                        let mut msg = MSG::default();
                        unsafe {
                            while GetMessageW(&mut msg, None, 0, 0).0 > 0 {
                                let _ = TranslateMessage(&msg);
                                DispatchMessageW(&msg);
                            }
                            let _ = KillTimer(Some(hwnd), 1);
                            let _ = DestroyIcon(icon);
                            if let Ok(module) = GetModuleHandleW(None) {
                                let _ =
                                    UnregisterClassW(PCWSTR(class.as_ptr()), Some(module.into()));
                            }
                        }
                        STATE.with(|v| *v.borrow_mut() = None);
                    }
                    Err(e) => {
                        let _ = tx.send(Err(format!("{e:#}")));
                        STATE.with(|v| *v.borrow_mut() = None);
                    }
                }
            });
            let hwnd = rx
                .recv()
                .context("托盘线程未响应")?
                .map_err(|e| anyhow::anyhow!(e))?;
            Ok(Self {
                hwnd,
                join: Some(join),
            })
        }
    }
    impl Drop for Tray {
        fn drop(&mut self) {
            unsafe {
                let _ = PostMessageW(
                    Some(HWND(self.hwnd as *mut _)),
                    WM_CLOSE,
                    WPARAM(0),
                    LPARAM(0),
                );
            }
            if let Some(join) = self.join.take() {
                let _ = join.join();
            }
        }
    }
}
#[cfg(windows)]
pub use native::{
    AutostartChange, autostart_enabled, migrate_autostart, pick_directory, set_autostart,
    window_visible,
};
#[cfg(all(test, windows))]
pub(crate) use native::{
    migrate_named_autostart, replace_named_autostart, set_startup_value, startup_value,
};
