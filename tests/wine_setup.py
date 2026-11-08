#!/usr/bin/env python3
"""The Windows installer, mesh-kvm-setup.exe, under Wine.

It installs silently into a fresh prefix and looks at what's left: the exe, the Start menu entry, the
login script and the entry in Windows' installed apps. Then it installs again over a running copy,
turns starting at sign-in off and on, runs a stray copy from Downloads with and without an install,
and uninstalls silently.

Run it inside a network namespace, so that the service's ports are free:

    unshare -rn bash -c 'ip link set lo up && python3 -I tests/wine_setup.py [path/to/mesh-kvm-setup.exe]'

The installer comes from the first argument, then $MESH_KVM_SETUP, then target/windows, where
packaging/windows/build-setup.sh puts it. A second installer, with an exe that differs by a few bytes,
is built here with makensis, for the reinstall. Set WINE_SETUP_PREFIX to put the Wine prefix somewhere
other than /tmp. It needs wine and makensis, and takes a few minutes.
"""
import contextlib
import glob
import json
import os
import pathlib
import shutil
import subprocess
import sys
import time
import urllib.request

REPO = pathlib.Path(__file__).resolve().parent.parent
DEFAULT_SETUP = REPO / "target/windows/mesh-kvm-setup.exe"
PREFIX = pathlib.Path(os.environ.get("WINE_SETUP_PREFIX", "/tmp/claude-1000/mk/setupprefix"))
UNINSTALL_KEY = r"HKCU\Software\Microsoft\Windows\CurrentVersion\Uninstall\mesh-kvm"
LOCAL = "http://127.0.0.1:24880"

failures = 0


def check(name, ok, detail=""):
    global failures
    print(f"{'PASS' if ok else 'FAIL'}: {name}" + (f" ({detail})" if detail and not ok else ""), flush=True)
    if not ok:
        failures += 1


def local():
    with urllib.request.urlopen(LOCAL + "/api/local", timeout=3) as r:
        return json.load(r)


def answers(timeout):
    deadline = time.time() + timeout
    while time.time() < deadline:
        with contextlib.suppress(Exception):
            return local()
        time.sleep(0.5)
    return None


def silent(timeout):
    """True once nothing answers on the API."""
    deadline = time.time() + timeout
    while time.time() < deadline:
        try:
            local()
        except Exception:
            return True
        time.sleep(0.3)
    return False


class Wine:
    def __init__(self, log):
        self.env = {k: v for k, v in os.environ.items() if k not in ("DISPLAY", "WAYLAND_DISPLAY")}
        self.env.update(WINEPREFIX=str(PREFIX), WINEDEBUG="-all", WINEDLLOVERRIDES="mscoree,mshtml=")
        self.env["XDG_RUNTIME_DIR"] = str(PREFIX.parent / "wine_setup_run")
        os.makedirs(self.env["XDG_RUNTIME_DIR"], mode=0o700, exist_ok=True)
        self.log = log

    def run(self, *args, timeout=180):
        return subprocess.run(["wine", *args], env=self.env, stdout=self.log, stderr=subprocess.STDOUT, timeout=timeout)

    def start(self, *args):
        return subprocess.Popen(["wine", *args], env=self.env, stdout=self.log, stderr=subprocess.STDOUT)

    def reg(self, *args):
        return subprocess.run(["wine", "reg", *args], env=self.env, capture_output=True, text=True, timeout=60).stdout

    def stop_all(self):
        subprocess.run(["wineserver", "-k"], env=self.env, stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
        subprocess.run(["wineserver", "-w"], env=self.env, stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)


def processes():
    """The command line of each mesh-kvm process in this prefix."""
    return list(running().values())


def running():
    """Each mesh-kvm process in this prefix, by process id."""
    found = {}
    for pid in glob.glob("/proc/[0-9]*"):
        with contextlib.suppress(OSError):
            environ = pathlib.Path(pid, "environ").read_bytes()
            command = pathlib.Path(pid, "cmdline").read_bytes()
            if f"WINEPREFIX={PREFIX}".encode() in environ and b"mesh-kvm" in command.lower() and b"setup" not in command.lower():
                found[pid] = command.replace(b"\0", b" ").decode(errors="replace").strip()
    return found


def wait_until(cond, timeout):
    deadline = time.time() + timeout
    while time.time() < deadline:
        if cond():
            return True
        time.sleep(0.3)
    return cond()


def registry(wine):
    """The uninstall entry, as name: value."""
    values = {}
    for line in wine.reg("query", UNINSTALL_KEY).splitlines():
        parts = line.strip().split(None, 2)
        if len(parts) == 3 and parts[1].startswith("REG_"):
            values[parts[0]] = parts[2]
    return values


def build_second_setup(setup, scratch):
    """The same installer with an exe that differs, so a reinstall can be told apart."""
    exe = REPO / "target/x86_64-pc-windows-gnu/release/mesh-kvm.exe"
    version = next(l.split('"')[1] for l in (REPO / "Cargo.toml").read_text().splitlines() if l.startswith("version"))
    changed = scratch / "mesh-kvm.exe"
    # Anything after the end of an exe is left alone by Windows.
    changed.write_bytes(exe.read_bytes() + b"\0mesh-kvm reinstall test\0")
    out = scratch / "mesh-kvm-setup-again.exe"
    subprocess.run(["makensis", "-V1", "-INPUTCHARSET", "UTF8", f"-DVERSION={version}", f"-DEXE={changed}", f"-DOUTFILE={out}",
                    str(REPO / "packaging/windows/mesh-kvm.nsi")], check=True)
    return out, changed.read_bytes(), version


def test(setup):
    if not setup.exists():
        sys.exit(f"installer not found: {setup}")
    shutil.rmtree(PREFIX, ignore_errors=True)
    PREFIX.mkdir(parents=True)
    scratch = PREFIX.parent / "wine_setup_files"
    shutil.rmtree(scratch, ignore_errors=True)
    scratch.mkdir()
    log = open(PREFIX.parent / "wine_setup.log", "w")
    wine = Wine(log)
    try:
        wine.run("wineboot", "-i")
        user = next(p.name for p in (PREFIX / "drive_c/users").iterdir() if p.name != "Public")
        home = PREFIX / "drive_c/users" / user
        folder = home / "AppData/Local/mesh-kvm"
        installed = folder / "mesh-kvm.exe"
        programs = home / "AppData/Roaming/Microsoft/Windows/Start Menu/Programs"
        startup = lambda: [p for p in programs.glob("[Ss]tart[Uu]p/mesh-kvm-autostart.vbs")]
        shortcut = programs / "mesh-kvm.lnk"
        windows_folder = f"C:\\users\\{user}\\AppData\\Local\\mesh-kvm"
        built = (REPO / "target/x86_64-pc-windows-gnu/release/mesh-kvm.exe").read_bytes()
        again, changed, version = build_second_setup(setup, scratch)
        shutil.copy(setup, scratch / "mesh-kvm-setup.exe")
        setup_path = "Z:" + str(scratch / "mesh-kvm-setup.exe").replace("/", "\\")
        again_path = "Z:" + str(scratch / "mesh-kvm-setup-again.exe").replace("/", "\\")

        # A first install, silently.
        result = wine.run(setup_path, "/S")
        check("a silent install finishes", result.returncode == 0, str(result.returncode))
        check("the exe is where it always was", installed.exists() and installed.read_bytes() == built)
        check("the uninstaller is beside it", (folder / "uninstall.exe").exists())
        check("the Start menu has mesh-kvm", shortcut.exists(), str(list(programs.iterdir())))
        check("the shortcut opens the app from the installed exe", shortcut.exists() and "--app".encode("utf-16-le") in shortcut.read_bytes()
              and f"{windows_folder}\\mesh-kvm.exe".lower().encode("utf-16-le") in shortcut.read_bytes().lower())
        check("no Start menu script is left as well", not (programs / "mesh-kvm.vbs").exists())
        check("starting at sign-in is on by default", len(startup()) == 1, str(startup()))
        values = registry(wine)
        check("it's listed among the installed apps", values.get("DisplayName") == "mesh-kvm", str(values))
        check("with its version", values.get("DisplayVersion") == version, str(values))
        check("and its publisher", values.get("Publisher") == "mesh-kvm", str(values))
        check("and the uninstaller", values.get("UninstallString") == f'"{windows_folder}\\uninstall.exe"', str(values))
        check("and a quiet way to uninstall", values.get("QuietUninstallString") == f'"{windows_folder}\\uninstall.exe" /S', str(values))
        check("and its icon", values.get("DisplayIcon") == f"{windows_folder}\\mesh-kvm.exe,0", str(values))
        check("and its size", int(values.get("EstimatedSize", "0"), 16) > 1000, str(values))
        check("with no change or repair buttons", values.get("NoModify") == "0x1" and values.get("NoRepair") == "0x1", str(values))
        st = answers(90)
        check("a silent install starts mesh-kvm, as a sign-in would", st is not None and st.get("version") == version, str(st and st.get("version")))
        settings = folder / "layout.json"
        wait_until(settings.exists, 30)
        check("the service keeps its settings beside the exe", settings.exists())
        (folder / "autostart-disabled").unlink(missing_ok=True)

        # Again, over the running copy.
        before = running()
        check("mesh-kvm is running before the reinstall", any("--role" in p for p in before.values()), str(before))
        result = wine.run(again_path, "/S")
        check("a reinstall over a running copy finishes", result.returncode == 0, str(result.returncode))
        check("the new exe is in place", installed.read_bytes() == changed)
        still = [before[pid] for pid in before if pid in running()]
        check("the copy that ran before has stopped", not still, str(still))
        logged = (folder / "mesh-kvm.log").read_text(errors="replace") if (folder / "mesh-kvm.log").exists() else ""
        check("it was asked to quit, which gives back its mutes", "Quit on this device" in logged, logged[-600:])
        check("the log says when", any(line[:4].isdigit() and line[4] == "-" for line in logged.splitlines()), logged[-300:])
        check("the settings are kept", settings.exists())
        st = answers(90)
        check("and it's running again", st is not None, "see wine_setup.log")

        # --quit by itself: the window, the tray and the service.
        result = wine.run(str(installed), "--quit", timeout=60)
        check("--quit stops mesh-kvm here", result.returncode == 0 and silent(10), str(result.returncode))
        check("with nothing of it left running", wait_until(lambda: not processes(), 15), str(processes()))

        # Starting at sign-in, off and on, and kept from one install to the next.
        wine.run(again_path, "/S", "/AUTOSTART=off")
        check("it can be installed with starting at sign-in off", not startup() and (folder / "autostart-disabled").exists(), str(startup()))
        check("which starts nothing", silent(5))
        wine.run(again_path, "/S")
        check("a reinstall keeps it off", not startup(), str(startup()))
        wine.run(again_path, "/S", "/AUTOSTART=on")
        check("and it can be turned on again", len(startup()) == 1 and not (folder / "autostart-disabled").exists(), str(startup()))
        check("which starts it", answers(90) is not None)
        wine.run(str(installed), "--quit", timeout=60)
        silent(10)

        # A stray copy, as from Downloads, while mesh-kvm is installed.
        downloads = home / "Downloads"
        downloads.mkdir(exist_ok=True)
        stray = downloads / "mesh-kvm.exe"
        stray.write_bytes(built)
        stray_path = f"C:\\users\\{user}\\Downloads\\mesh-kvm.exe"
        started = time.time()
        wine.run(stray_path, timeout=60)
        check("a copy from Downloads ends at once", time.time() - started < 30, str(time.time() - started))
        check("and leaves the installed exe alone", installed.read_bytes() == changed)
        wait_until(lambda: any("--app" in p for p in processes()), 10)
        time.sleep(2)
        now = processes()
        check("it doesn't run itself while an install exists", not any("Downloads" in p for p in now), str(now))
        wine.run(str(installed), "--quit", timeout=60)
        wine.stop_all()

        # A silent uninstall keeps the settings.
        result = wine.run(f"{windows_folder}\\uninstall.exe", "/S")
        wait_until(lambda: not installed.exists(), 30)
        check("a silent uninstall finishes", result.returncode == 0, str(result.returncode))
        check("the exe is gone", not installed.exists())
        check("the uninstaller is gone", wait_until(lambda: not (folder / "uninstall.exe").exists(), 30))
        check("the Start menu entry is gone", not shortcut.exists() and not (programs / "mesh-kvm.vbs").exists())
        check("the login script is gone", not startup(), str(startup()))
        check("it's no longer among the installed apps", registry(wine) == {}, str(registry(wine)))
        check("the settings stay", settings.exists(), str(list(folder.iterdir()) if folder.exists() else None))
        check("downloaded updates go", not (folder / "updates").exists())
        check("nothing of it is running", not processes(), str(processes()))

        # A stray copy with nothing installed runs, but doesn't install itself.
        wine.start(stray_path)
        st = answers(90)
        check("with nothing installed, a stray copy still runs", st is not None)
        time.sleep(5)
        check("and doesn't install itself", not installed.exists() and not shortcut.exists() and not startup(),
              str(list(folder.iterdir())))
        wine.run(stray_path, "--quit", timeout=60)
        check("and quits like any other", silent(10))
    finally:
        wine.stop_all()
        log.close()
    return 1 if failures else 0


if __name__ == "__main__":
    path = pathlib.Path(sys.argv[1] if len(sys.argv) > 1 else os.environ.get("MESH_KVM_SETUP", DEFAULT_SETUP))
    code = test(path)
    print("ALL PASS" if code == 0 else f"{failures} FAILED")
    sys.exit(code)
