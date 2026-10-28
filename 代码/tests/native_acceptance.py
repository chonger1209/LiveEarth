"""Opt-in Windows acceptance checks. Operates only on the process it launches."""
import argparse
import ctypes as c
from ctypes import wintypes as w
import io
import json
from pathlib import Path
import struct
import subprocess
import time
from PIL import Image, ImageCms, ImageChops


class NativeMouse:
    """Real mouse input, restricted to a verified foreground test window."""
    class Mouse(c.Structure):
        _fields_ = [("dx", w.LONG), ("dy", w.LONG), ("data", w.DWORD),
                    ("flags", w.DWORD), ("time", w.DWORD), ("extra", c.c_size_t)]
    class Payload(c.Union):
        pass
    class Key(c.Structure):
        _fields_ = [("vk", w.WORD), ("scan", w.WORD), ("flags", w.DWORD),
                    ("time", w.DWORD), ("extra", c.c_size_t)]
    Payload._fields_ = [("mouse", Mouse), ("key", Key)]
    class Input(c.Structure):
        pass
    Input._fields_ = [("kind", w.DWORD), ("payload", Payload)]

    def __init__(self, user, hwnd, scale):
        self.user, self.hwnd, self.scale = user, hwnd, scale
        user.SetForegroundWindow.argtypes = [w.HWND]
        user.GetForegroundWindow.restype = w.HWND
        user.ClientToScreen.argtypes = [w.HWND, c.POINTER(w.POINT)]
        user.SendInput.argtypes = [w.UINT, c.POINTER(self.Input), c.c_int]
        user.SendInput.restype = w.UINT

    def move(self, x, y):
        self.user.SetForegroundWindow(self.hwnd)
        time.sleep(0.1)
        # Background runners cannot always claim the foreground. Allow activation
        # by the user or Windows automation, but never send input to another app.
        deadline = time.monotonic() + 30
        while self.user.GetForegroundWindow() != self.hwnd and time.monotonic() < deadline:
            time.sleep(0.1)
        assert self.user.GetForegroundWindow() == self.hwnd, "test window must own input"
        point = w.POINT(round(x * self.scale), round(y * self.scale))
        assert self.user.ClientToScreen(self.hwnd, c.byref(point))
        # After a modal closes, an unchanged cursor position need not generate
        # WM_MOUSEMOVE. Re-enter the owner before pressing its next control.
        assert self.user.SetCursorPos(point.x + 2, point.y)
        time.sleep(0.05)
        assert self.user.SetCursorPos(point.x, point.y)
        time.sleep(0.1)

    def button(self, down):
        if down:
            assert self.user.GetForegroundWindow() == self.hwnd
        event = self.Input(0, self.Payload(mouse=self.Mouse(0, 0, 0, 2 if down else 4, 0, 0)))
        assert self.user.SendInput(1, c.byref(event), c.sizeof(event)) == 1

    def click(self, x, y):
        self.move(x, y)
        self.button(True)
        try:
            time.sleep(0.15)
        finally:
            self.button(False)
        time.sleep(0.3)

    def key(self, vk, down=True, scan=0):
        flags = (0 if down else 2) | (4 if scan else 0)
        event = self.Input(1, self.Payload(key=self.Key(vk, scan, flags, 0, 0)))
        if down:
            assert self.user.GetForegroundWindow() == self.hwnd
        assert self.user.SendInput(1, c.byref(event), c.sizeof(event)) == 1

    def replace_text(self, text):
        self.key(0x11)
        try:
            self.key(0x41)
            self.key(0x41, False)
        finally:
            self.key(0x11, False)
        raw = text.encode("utf-16-le")
        for offset in range(0, len(raw), 2):
            scan = int.from_bytes(raw[offset:offset + 2], "little")
            self.key(0, scan=scan)
            self.key(0, False, scan)


def check_archive(source, archive):
    with Image.open(source) as png, Image.open(archive) as webp:
        assert png.size == webp.size
        a, b = png.convert("RGB"), webp.convert("RGB")
        assert ImageChops.difference(a, b).getbbox() is None
        profile = ImageCms.ImageCmsProfile(io.BytesIO(webp.info["icc_profile"]))
        assert "EarthLive source colors" in ImageCms.getProfileDescription(profile)
        transform = ImageCms.buildTransformFromOpenProfiles(profile, ImageCms.createProfile("sRGB"), "RGB", "RGB")
        sample = Image.new("RGB", (256, 1))
        sample.putdata([(n, n, n) for n in range(256)])
        corrected = list(ImageCms.applyTransform(sample, transform).get_flattened_data())
        # Source PNG has gamma 0.45455; compare to the analytical gamma -> sRGB curve.
        gamma = png.info["gamma"]
        errors = []
        for n, rgb in enumerate(corrected):
            linear = (n / 255) ** (1 / gamma)
            srgb = 12.92 * linear if linear <= 0.0031308 else 1.055 * linear ** (1 / 2.4) - 0.055
            errors.extend(abs(channel - round(srgb * 255)) for channel in rgb)
        assert max(errors) <= 1, max(errors)
        print(json.dumps({"independent_libwebp_pixels": "exact", "lcms_profile": "accepted", "gamma_to_srgb_max_error": max(errors), "png_bytes": Path(source).stat().st_size, "webp_bytes": Path(archive).stat().st_size}), flush=True)


def check_resources(exe):
    k = c.WinDLL("kernel32", use_last_error=True)
    k.LoadLibraryExW.argtypes = [w.LPCWSTR, w.HANDLE, w.DWORD]
    k.LoadLibraryExW.restype = w.HMODULE
    k.FindResourceW.argtypes = [w.HMODULE, c.c_void_p, c.c_void_p]
    k.FindResourceW.restype = c.c_void_p
    k.LoadResource.argtypes = [w.HMODULE, c.c_void_p]
    k.LoadResource.restype = c.c_void_p
    k.LockResource.argtypes = [c.c_void_p]
    k.LockResource.restype = c.c_void_p
    k.SizeofResource.argtypes = [w.HMODULE, c.c_void_p]
    k.SizeofResource.restype = w.DWORD
    k.FreeLibrary.argtypes = [w.HMODULE]
    module = k.LoadLibraryExW(str(exe), None, 2)
    assert module, c.get_last_error()
    def resource(kind, identifier):
        handle = k.FindResourceW(module, c.c_void_p(identifier), c.c_void_p(kind))
        assert handle, (kind, identifier, c.get_last_error())
        size = k.SizeofResource(module, handle)
        return c.string_at(k.LockResource(k.LoadResource(module, handle)), size)
    try:
        group = resource(14, 101)
        assert struct.unpack_from("<HHH", group) == (0, 1, 9)
        sizes = []
        for i in range(9):
            width, height, _, _, planes, depth, length, identifier = struct.unpack_from("<BBBBHHIH", group, 6 + 14 * i)
            raw = resource(3, identifier)
            assert len(raw) == length and planes == 1 and depth == 32
            with Image.open(io.BytesIO(raw)) as icon:
                side = width or 256
                assert icon.size == (side, height or 256) and icon.mode == "RGBA"
                sizes.append(side)
        assert sizes == [16, 20, 24, 32, 40, 48, 64, 128, 256]
        print("EXE icon resource sizes:", sizes, flush=True)
    finally:
        k.FreeLibrary(module)


def check_tray(exe, directory, *, minimize_to_tray=True, start_minimized=True, startup=True, download=False, controls=False, directory_only=False, measure_cpu=False):
    directory.mkdir(parents=True, exist_ok=True)
    settings = {"schema": 4, "grid": 1, "zoom": 100, "interval_minutes": 1,
                "automatic": download, "apply_automatic": False, "beijing": True,
                "photos": str(directory / "photos"), "last": None,
                "minimize_to_tray": minimize_to_tray, "close_to_tray": True,
                "start_minimized": start_minimized, "autostart": False}
    (directory / "settings.json").write_text(json.dumps(settings), encoding="utf-8")
    user = c.WinDLL("user32", use_last_error=True)
    callback_type = c.WINFUNCTYPE(w.BOOL, w.HWND, w.LPARAM)
    user.EnumWindows.argtypes = [callback_type, w.LPARAM]
    user.GetWindowThreadProcessId.argtypes = [w.HWND, c.POINTER(w.DWORD)]
    user.GetWindowTextW.argtypes = [w.HWND, w.LPWSTR, c.c_int]
    user.IsWindowVisible.argtypes = [w.HWND]
    user.IsIconic.argtypes = [w.HWND]
    user.ShowWindowAsync.argtypes = [w.HWND, c.c_int]
    user.PostMessageW.argtypes = [w.HWND, w.UINT, w.WPARAM, w.LPARAM]
    user.SendMessageW.argtypes = [w.HWND, w.UINT, w.WPARAM, w.LPARAM]
    user.SendMessageW.restype = w.LPARAM
    process = subprocess.Popen([str(exe), "--data-dir", str(directory)] + (["--startup"] if startup else []))
    def windows():
        found = {}
        @callback_type
        def visit(hwnd, _):
            pid = w.DWORD()
            user.GetWindowThreadProcessId(hwnd, c.byref(pid))
            if pid.value == process.pid:
                title = c.create_unicode_buffer(512)
                user.GetWindowTextW(hwnd, title, 512)
                found[title.value] = hwnd
            return True
        user.EnumWindows(visit, 0)
        return found
    def wait(condition, seconds=15):
        end = time.monotonic() + seconds
        while not condition():
            assert process.poll() is None, f"process exited {process.returncode}"
            assert time.monotonic() < end, "native acceptance timeout"
            time.sleep(0.05)
    measurements = []
    def sample_cpu(state, seconds=6):
        if not measure_cpu:
            return
        kernel = c.WinDLL("kernel32", use_last_error=True)
        kernel.GetProcessTimes.argtypes = [w.HANDLE] + [c.POINTER(w.FILETIME)] * 4
        kernel.GetProcessTimes.restype = w.BOOL
        def cpu_seconds():
            times = [w.FILETIME() for _ in range(4)]
            assert kernel.GetProcessTimes(int(process._handle), *(c.byref(t) for t in times))
            return sum((t.dwHighDateTime << 32) | t.dwLowDateTime for t in times[2:]) / 10_000_000
        before, started = cpu_seconds(), time.monotonic()
        time.sleep(seconds)
        elapsed = time.monotonic() - started
        percent = (cpu_seconds() - before) * 100 / elapsed
        measurements.append({"state": state, "seconds": round(elapsed, 3), "one_core_percent": round(percent, 3)})
        (directory / "cpu.json").write_text(json.dumps(measurements, indent=2), encoding="utf-8")
        print(f"CPU {state}: {percent:.3f}% of one logical processor", flush=True)
        assert percent < 10, f"idle window busy-looped in {state}: {percent:.1f}% of one core"
    tray = None
    try:
        wait(lambda: "LiveEarth background" in windows() and "LiveEarth" in windows())
        found = windows()
        main, tray = found["LiveEarth"], found["LiveEarth background"]
        time.sleep(1)
        hidden = startup and start_minimized and minimize_to_tray
        if hidden:
            wait(lambda: not user.IsWindowVisible(main))
        elif startup and start_minimized:
            wait(lambda: user.IsWindowVisible(main) and user.IsIconic(main))
        else:
            wait(lambda: user.IsWindowVisible(main) and not user.IsIconic(main))
        print(f"Startup={startup}, minimize={start_minimized}, tray={minimize_to_tray}: PASS", flush=True)
        # A canceled Windows session-end must leave this isolated application alive.
        for window in (main, tray):
            assert user.SendMessageW(window, 0x0011, 0, 0) == 1  # WM_QUERYENDSESSION
            user.SendMessageW(window, 0x0016, 0, 0)  # WM_ENDSESSION, canceled
        time.sleep(0.4)
        assert process.poll() is None, "canceled shutdown terminated the application"
        print("Canceled Windows shutdown retains process: PASS", flush=True)
        sample_cpu("startup_hidden" if hidden else "startup_visible")
        if download:
            wait(lambda: json.loads((directory / "settings.json").read_text(encoding="utf-8")).get("last") is not None, 60)
            assert any((directory / "photos").glob("*.webp"))
            assert not any((directory / "photos").glob("*.png"))
            assert not user.IsWindowVisible(main)
            print("Real automatic WebP capture while hidden: PASS", flush=True)
        user.PostMessageW(tray, 0x800A, 0, 0x0202)
        wait(lambda: user.IsWindowVisible(main) and not user.IsIconic(main))
        # Wait for the GUI to consume the queued egui restore command before
        # simulating the next, independent user action.
        time.sleep(0.8)
        print("Tray left-click restores window: PASS", flush=True)
        sample_cpu("visible")
        if controls:
            user.SetThreadDpiAwarenessContext.argtypes = [c.c_void_p]
            user.SetThreadDpiAwarenessContext.restype = c.c_void_p
            user.SetThreadDpiAwarenessContext(c.c_void_p(-4))
            user.GetDpiForWindow.argtypes = [w.HWND]
            user.GetDpiForWindow.restype = w.UINT
            scale = user.GetDpiForWindow(main) / 96
            print("Control test DPI scale:", scale, flush=True)
            mouse = NativeMouse(user, main, scale)
            click = mouse.click
            click(700, 500)  # Neutral preview area: consume any activation-only first click.
            def saved():
                return json.loads((directory / "settings.json").read_text(encoding="utf-8"))
            if not directory_only:
                # Coordinates are in egui points from the reviewed latest-view screenshot.
                click(15, 358)
                wait(lambda: saved()["apply_automatic"] is True)
                click(15, 358)
                wait(lambda: saved()["apply_automatic"] is False)
                before = saved()["zoom"]
                mouse.move(130, 383)
                mouse.button(True)
                try:
                    time.sleep(0.15)
                    mouse.move(160, 383)
                    time.sleep(0.2)
                    assert saved()["zoom"] == before, "slider persisted before release"
                finally:
                    mouse.button(False)
                wait(lambda: saved()["zoom"] != before)
                selected_zoom = saved()["zoom"]
                print("Checkbox auto-save and slider save on release: PASS", flush=True)
                click(80, 294)
                wait(lambda: saved()["automatic"] is True)
                click(15, 358)
                time.sleep(0.3)
                assert saved()["apply_automatic"] is False
                wait(lambda: saved().get("last") is not None, 60)
                click(15, 358)
                time.sleep(0.3)
                assert saved()["apply_automatic"] is False, "waiting timer unlocked capture settings"
                click(80, 294)
                wait(lambda: saved()["automatic"] is False)
                time.sleep(0.5)
                click(15, 358)
                wait(lambda: saved()["apply_automatic"] is True)
                assert saved()["zoom"] == selected_zoom
                print("Running/waiting controls locked, stopped controls unlock: PASS", flush=True)
            before_directory = saved()["photos"]
            click(170, 222)
            wait(lambda: "选择照片目录" in windows())
            dialog = windows()["选择照片目录"]
            user.IsWindowEnabled.argtypes = [w.HWND]
            assert not user.IsWindowEnabled(main), "modal picker must disable its owner"
            user.EnumChildWindows.argtypes = [w.HWND, callback_type, w.LPARAM]
            user.GetClassNameW.argtypes = [w.HWND, w.LPWSTR, c.c_int]
            user.GetDlgCtrlID.argtypes = [w.HWND]
            user.PostMessageW(dialog, 0x0010, 0, 0)
            wait(lambda: "选择照片目录" not in windows() and user.IsWindowEnabled(main))
            time.sleep(0.5)  # Let the GUI receive the picker result and re-enable controls.
            assert saved()["photos"] == before_directory
            print("Native folder picker opens and cancel preserves directory: PASS", flush=True)
            click(700, 500)
            chosen = directory / "LiveEarth-验收照片"
            chosen.mkdir(exist_ok=True)
            old_logs = {p: p.read_bytes() for p in Path(before_directory).glob("运行记录-*.log")}
            click(170, 222)
            wait(lambda: "选择照片目录" in windows())
            dialog = windows()["选择照片目录"]
            fields = {}
            @callback_type
            def locate(hwnd, _):
                fields[user.GetDlgCtrlID(hwnd)] = hwnd
                return True
            user.EnumChildWindows(dialog, locate, 0)
            assert 1152 in fields and 1 in fields, "native folder edit and confirmation button"
            user.GetWindowRect.argtypes = [w.HWND, c.POINTER(w.RECT)]
            user.ScreenToClient.argtypes = [w.HWND, c.POINTER(w.POINT)]
            rect = w.RECT()
            assert user.GetWindowRect(fields[1152], c.byref(rect))
            center = w.POINT((rect.left + rect.right) // 2, (rect.top + rect.bottom) // 2)
            assert user.ScreenToClient(dialog, c.byref(center))
            dialog_mouse = NativeMouse(user, dialog, user.GetDpiForWindow(dialog) / 96)
            dialog_mouse.click(center.x / dialog_mouse.scale, center.y / dialog_mouse.scale)
            dialog_mouse.replace_text(str(chosen))
            def entered_path():
                value = c.create_unicode_buffer(32768)
                user.GetWindowTextW(fields[1152], value, len(value))
                return value.value
            wait(lambda: entered_path() == str(chosen))
            # A real click also commits the edit's focus change; BM_CLICK can
            # leave the shell dialog's typed path uncommitted.
            assert user.GetWindowRect(fields[1], c.byref(rect))
            center = w.POINT((rect.left + rect.right) // 2, (rect.top + rect.bottom) // 2)
            assert user.ScreenToClient(dialog, c.byref(center))
            dialog_mouse.click(center.x / dialog_mouse.scale, center.y / dialog_mouse.scale)
            wait(lambda: "选择照片目录" not in windows() and saved()["photos"] == str(chosen))
            assert all(p.read_bytes() == data for p, data in old_logs.items())
            assert any("设置已保存" in p.read_text(encoding="utf-8") for p in chosen.glob("运行记录-*.log"))
            click(280, 222)
            wait(lambda: any(chosen.glob("*.webp")) and any("抓取完成" in p.read_text(encoding="utf-8") for p in chosen.glob("运行记录-*.log")), 60)
            assert Path(saved()["last"]).parent == chosen
            assert all(p.read_bytes() == data for p, data in old_logs.items()), "old logs must stay unchanged after directory switch"
            print("Folder confirmation, capture and daily log switch preserve old files: PASS", flush=True)
            def explorer_windows():
                found = {}
                @callback_type
                def visit(hwnd, _):
                    title, cls = c.create_unicode_buffer(1024), c.create_unicode_buffer(256)
                    user.GetWindowTextW(hwnd, title, 1024)
                    user.GetClassNameW(hwnd, cls, 256)
                    if cls.value == "CabinetWClass":
                        found[hwnd] = title.value
                    return True
                user.EnumWindows(visit, 0)
                return found
            before_explorer = explorer_windows()
            time.sleep(0.5)
            click(60, 222)
            wait(lambda: any(title in (chosen.name, str(chosen)) for title in explorer_windows().values()))
            for hwnd, title in explorer_windows().items():
                if hwnd not in before_explorer and title in (chosen.name, str(chosen)):
                    user.PostMessageW(hwnd, 0x0010, 0, 0)  # Only a new test-created Explorer window.
            print("Open directory launches the selected photo folder: PASS", flush=True)
        user.ShowWindowAsync(main, 6)
        if minimize_to_tray:
            wait(lambda: not user.IsWindowVisible(main))
        else:
            wait(lambda: user.IsWindowVisible(main) and user.IsIconic(main))
        print("Minimize respects saved tray preference: PASS", flush=True)
        sample_cpu("minimized_to_tray" if minimize_to_tray else "minimized_to_taskbar")
        user.PostMessageW(tray, 0x800A, 0, 0x0202)
        wait(lambda: user.IsWindowVisible(main) and not user.IsIconic(main))
        time.sleep(0.8)
        user.PostMessageW(main, 0x0010, 0, 0)
        wait(lambda: not user.IsWindowVisible(main))
        assert process.poll() is None
        print("Close to tray retains process: PASS", flush=True)
        sample_cpu("closed_to_tray", seconds=8)
        photo_directory = Path(json.loads((directory / "settings.json").read_text(encoding="utf-8"))["photos"])
        logs = list(photo_directory.glob("运行记录-*.log"))
        assert len(logs) == 1
        assert ("设置已保存" if controls else "程序启动") in logs[0].read_text(encoding="utf-8")
        if not measure_cpu:
            logs[0].unlink()
            # An explicit power notification is delivered only to this application's window.
            assert user.SendMessageW(tray, 0x0218, 18, 0) == 1
            wait(lambda: logs[0].exists() and "系统已唤醒" in logs[0].read_text(encoding="utf-8"))
            print("Daily log deleted while running and recreated: PASS", flush=True)
            print("Synthetic resume notification accepted: PASS", flush=True)
        else:
            user.PostMessageW(tray, 0x800A, 0, 0x0202)
            wait(lambda: user.IsWindowVisible(main) and not user.IsIconic(main))
            time.sleep(0.8)
            sample_cpu("restored_after_close")
            user.PostMessageW(main, 0x0010, 0, 0)
            wait(lambda: not user.IsWindowVisible(main))
    finally:
        if tray and process.poll() is None:
            user.PostMessageW(tray, 0x0010, 0, 0)  # Explicit background-window close.
        try:
            process.wait(timeout=45)
        except subprocess.TimeoutExpired:
            process.terminate()  # Only the isolated test process created above.
            process.wait(timeout=10)
            raise
    assert process.returncode == 0
    assert "程序已退出" in logs[0].read_text(encoding="utf-8")
    print("Graceful exit, no residual test process: PASS", flush=True)


def check_gui(exe, directory, original):
    directory = directory.resolve()
    for view in ["latest", "history", "settings", "about"]:
        data = directory / view
        data.mkdir(parents=True, exist_ok=True)
        settings = {"schema": 4, "grid": 4, "zoom": 50, "interval_minutes": 10,
                    "automatic": False, "apply_automatic": True, "beijing": True,
                    "photos": str(data / "photos"), "last": str(original.resolve()),
                    "minimize_to_tray": False, "close_to_tray": True,
                    "start_minimized": True, "autostart": False}
        (data / "settings.json").write_text(json.dumps(settings), encoding="utf-8")
        shot = directory / f"{view}.png"
        arguments = [str(exe), "--data-dir", str(data), "--smoke-screenshot", str(shot), "--smoke-view", view]
        if view != "latest":
            arguments += ["--smoke-compact"]
        subprocess.run(arguments, check=True, timeout=40)
        assert shot.exists()
        with Image.open(shot) as picture:
            print(f"GUI {view}: {picture.size} -> {shot}", flush=True)


if __name__ == "__main__":
    parser = argparse.ArgumentParser()
    parser.add_argument("--exe", type=Path, required=True)
    parser.add_argument("--data", type=Path, required=True)
    parser.add_argument("--png", type=Path)
    parser.add_argument("--webp", type=Path)
    parser.add_argument("--screens", type=Path)
    parser.add_argument("--directories", action="store_true", help="Run only directory interaction checks")
    parser.add_argument("--controls", action="store_true", help="Check auto-save, capture locks and directories with real input")
    parser.add_argument("--tray-performance", action="store_true", help="Check hidden/visible CPU and tray lifecycle without screenshots or network downloads")
    args = parser.parse_args()
    if bool(args.png) != bool(args.webp):
        parser.error("--png and --webp must be supplied together")
    if args.screens and not args.png:
        parser.error("--screens requires --png as the preview source")
    if args.png:
        check_archive(args.png, args.webp)
    if not args.tray_performance:
        check_resources(args.exe)
    if args.tray_performance:
        check_tray(args.exe, args.data / "tray-performance", measure_cpu=True)
    elif args.directories:
        check_tray(args.exe, args.data / "directories", minimize_to_tray=False, startup=False, controls=True, directory_only=True)
    elif args.controls:
        check_tray(args.exe, args.data / "controls", minimize_to_tray=False, startup=False, controls=True)
    else:
        check_tray(args.exe, args.data / "tray", download=True)
        check_tray(args.exe, args.data / "taskbar", minimize_to_tray=False)
        check_tray(args.exe, args.data / "manual", minimize_to_tray=False, startup=False)
        check_tray(args.exe, args.data / "normal", start_minimized=False)
    if args.screens:
        check_gui(args.exe, args.screens, args.png)
