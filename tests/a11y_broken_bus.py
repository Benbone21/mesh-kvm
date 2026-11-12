#!/usr/bin/env python3
"""Proves the app window carries on when the accessibility bus is broken.

accesskit_unix 0.13.1 aborted the whole program when the AT-SPI bus couldn't be reached. Here the app
starts against a pretend accessibility service that says accessibility is on and hands out a bus
address that's broken, in two ways: the socket is another D-Bus daemon's, so the GUID is wrong, as
was seen on a real desktop; and nobody listens at it. The app must still be running 5 s later, and
must have said "accessibility is off". Then accessibility is turned off and on again, as when a
screen reader starts, and the app must survive the second failure too. Two more cases keep the fix
honest: with a healthy bus (and a pretend registry) the app must register its window as it always
did, before and after the toggle, and say nothing; and with a bus that's broken at the start but
works by the time accessibility comes on again, the app must register then.

Everything runs inside bubblewrap, which gives it its own /run (so its own /run/user/<uid>, with no
way to the desktop's session bus, accessibility bus, Wayland, PipeWire or systemd), its own /tmp,
network, PIDs and IPC, and a read-only view of everything else. In there it starts its own
dbus-daemon, its own headless compositor (kwin_wayland --virtual, or Xvfb), tests/fake_service.py
as the service, and the app. When the sandbox ends, everything it started ends with it. Before and
after, it reads the real session's accessibility processes and bus address, and fails if they
changed. With no bubblewrap or no headless display it says SKIP and touches nothing.

    python3 -I tests/a11y_broken_bus.py [path/to/mesh-kvm]

The program defaults to target/release/mesh-kvm in the repository and can also be set with
MESH_KVM_EXE. Run it from anywhere; it needs bwrap, dbus-daemon, gdbus, python3-gi, and kwin_wayland
or Xvfb. --expect-abort turns the broken cases around, for showing that a build without the fix does
abort. --case runs just one case, and --verbose shows what the app printed.
"""

import argparse
import json
import os
import re
import secrets
import shutil
import signal
import socket
import subprocess
import sys
import time

REPO = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
SURVIVE = 5.0
AFTER_TOGGLE = 3.0
OFF_LINE = "accessibility is off"
# The two broken buses; a healthy one, where the app must register as before; and a broken one
# that's healthy by the time IsEnabled comes on again, where accessibility must come back.
CASES = ("wrong-guid", "nobody-listens", "healthy", "recovers")
ROOT = "/org/a11y/atspi/accessible/root"
SOCKET = "wayland-a11y-test"

FAKE_A11Y_XML = """
<node>
  <interface name="org.a11y.Bus">
    <method name="GetAddress"><arg type="s" direction="out"/></method>
  </interface>
  <interface name="org.a11y.Status">
    <property name="IsEnabled" type="b" access="readwrite"/>
    <property name="ScreenReaderEnabled" type="b" access="readwrite"/>
  </interface>
</node>
"""

# Enough of at-spi2-registryd for an app to embed its root in the desktop's.
FAKE_REGISTRY_XML = """
<node>
  <interface name="org.a11y.atspi.Socket">
    <method name="Embed"><arg type="(so)" direction="in"/><arg type="(so)" direction="out"/></method>
    <method name="Unembed"><arg type="(so)" direction="in"/></method>
  </interface>
</node>
"""

DBUS_CONFIG = """<!DOCTYPE busconfig PUBLIC "-//freedesktop//DTD D-Bus Bus Configuration 1.0//EN"
 "http://www.freedesktop.org/standards/dbus/1.0/busconfig.dtd">
<busconfig>
  <type>session</type>
  <listen>unix:path={path}</listen>
  <auth>EXTERNAL</auth>
  <policy context="default">
    <allow send_destination="*" eavesdrop="true"/>
    <allow eavesdrop="true"/>
    <allow own="*"/>
  </policy>
</busconfig>
"""
# No <servicedir>: nothing can be started on demand, at-spi-bus-launcher included.


# The real session, read only.

def real_session():
    def run(*command):
        try:
            done = subprocess.run(command, capture_output=True, text=True, timeout=10)
            return done.stdout.strip()
        except (OSError, subprocess.TimeoutExpired) as e:
            return f"({e})"
    # By process name, so no shell whose command line happens to mention them is counted.
    daemons = [line for line in run("pgrep", "-a", "dbus-daemon").splitlines() if "accessibility.conf" in line]
    processes = sorted(set(run("pgrep", "-a", "at-spi").splitlines()) | set(daemons))
    address = run("busctl", "--user", "call", "org.a11y.Bus", "/org/a11y/bus", "org.a11y.Bus", "GetAddress")
    return {"processes": processes, "address": address}


# The pretend accessibility service, run inside the sandbox.

def fake_a11y(address, then, registry_on, embedded_log):
    """Says IsEnabled is true and hands out `address`; after SIGUSR1 turns IsEnabled off and on, it
    hands out `then`. With `registry_on`, it's also the registry on that bus, and writes each
    app that embeds its root to `embedded_log`."""
    import gi
    gi.require_version("Gio", "2.0")
    from gi.repository import Gio, GLib

    enabled = [True]
    handed_out = [address]
    connection = Gio.bus_get_sync(Gio.BusType.SESSION, None)
    node = Gio.DBusNodeInfo.new_for_xml(FAKE_A11Y_XML)

    def method(_conn, _sender, _path, _iface, name, _params, invocation):
        invocation.return_value(GLib.Variant("(s)", (handed_out[0],)))

    def get_property(_conn, _sender, _path, _iface, name):
        return GLib.Variant("b", enabled[0] if name == "IsEnabled" else False)

    for interface in node.interfaces:
        connection.register_object("/org/a11y/bus", interface, method, get_property, None)

    def announce(value):
        enabled[0] = value
        changed = GLib.Variant("(sa{sv}as)", ("org.a11y.Status", {"IsEnabled": GLib.Variant("b", value)}, []))
        connection.emit_signal(None, "/org/a11y/bus", "org.freedesktop.DBus.Properties", "PropertiesChanged", changed)
        connection.flush_sync(None)

    def toggle():
        announce(False)
        handed_out[0] = then or address
        GLib.timeout_add(500, lambda: announce(True) and False)
        return GLib.SOURCE_REMOVE

    if registry_on:
        flags = Gio.DBusConnectionFlags.AUTHENTICATION_CLIENT | Gio.DBusConnectionFlags.MESSAGE_BUS_CONNECTION
        a11y = Gio.DBusConnection.new_for_address_sync(registry_on, flags, None, None)

        def embed(conn, sender, _path, _iface, name, params, invocation):
            if name == "Embed":
                pid = conn.call_sync("org.freedesktop.DBus", "/org/freedesktop/DBus", "org.freedesktop.DBus",
                                     "GetConnectionUnixProcessID", GLib.Variant("(s)", (sender,)),
                                     GLib.VariantType("(u)"), Gio.DBusCallFlags.NONE, -1, None).unpack()[0]
                with open(embedded_log, "a") as f:
                    f.write(f"{params.unpack()[0][0]} {pid}\n")
                invocation.return_value(GLib.Variant("((so))", ((conn.get_unique_name(), ROOT),)))
            else:
                invocation.return_value(None)

        a11y.register_object(ROOT, Gio.DBusNodeInfo.new_for_xml(FAKE_REGISTRY_XML).interfaces[0], embed, None, None)
        Gio.bus_own_name_on_connection(a11y, "org.a11y.atspi.Registry", Gio.BusNameOwnerFlags.NONE, None, None)

    GLib.unix_signal_add(GLib.PRIORITY_DEFAULT, signal.SIGUSR1, toggle)
    loop = GLib.MainLoop()
    GLib.unix_signal_add(GLib.PRIORITY_DEFAULT, signal.SIGTERM, loop.quit)

    def acquired(*_):
        print("ready", flush=True)

    Gio.bus_own_name_on_connection(connection, "org.a11y.Bus", Gio.BusNameOwnerFlags.NONE, acquired, None)
    loop.run()


# The run inside the sandbox.

def wait_for(what, check, patience=10.0):
    started = time.monotonic()
    while time.monotonic() - started < patience:
        if check():
            return
        time.sleep(0.05)
    raise RuntimeError(f"{what} didn't come up within {patience:.0f} s")


def port_open(port):
    try:
        socket.create_connection(("127.0.0.1", port), timeout=0.2).close()
        return True
    except OSError:
        return False


def start_dbus(runtime, name):
    path = os.path.join(runtime, name)
    os.makedirs(os.path.dirname(path), exist_ok=True)
    config = path + ".conf"
    with open(config, "w") as f:
        f.write(DBUS_CONFIG.format(path=path))
    daemon = subprocess.Popen(["dbus-daemon", "--config-file", config, "--nofork", "--print-address=1"],
                              stdout=subprocess.PIPE, text=True)
    return daemon, daemon.stdout.readline().strip()


def inside(case, exe, display):
    uid = os.getuid()
    runtime = f"/run/user/{uid}"
    os.makedirs(runtime, mode=0o700, exist_ok=True)
    os.chmod(runtime, 0o700)
    home = os.path.join(runtime, "home")
    os.makedirs(home, exist_ok=True)
    env = {"PATH": os.environ["PATH"], "HOME": home, "XDG_RUNTIME_DIR": runtime, "LANG": "C.UTF-8",
           "XDG_CONFIG_HOME": os.path.join(home, ".config"), "XDG_DATA_HOME": os.path.join(home, ".local/share"),
           "XDG_CACHE_HOME": os.path.join(home, ".cache"), "XDG_STATE_HOME": os.path.join(home, ".local/state"),
           "DBUS_SYSTEM_BUS_ADDRESS": f"unix:path={runtime}/no-system-bus"}
    os.environ.clear()
    os.environ.update(env)

    _session, session_address = start_dbus(runtime, "bus")
    os.environ["DBUS_SESSION_BUS_ADDRESS"] = session_address
    # A daemon of its own, like the desktop's accessibility bus.
    _other, other_address = start_dbus(runtime, "at-spi/bus_1")
    nobody = f"unix:path={runtime}/at-spi/nobody-listens"
    then = ""
    if case == "wrong-guid":
        # The socket is that daemon's, but the address says another GUID, as after a re-bind.
        real_guid = re.search(r"guid=([0-9a-f]{32})", other_address).group(1)
        wrong = secrets.token_hex(16)
        while wrong == real_guid:
            wrong = secrets.token_hex(16)
        a11y_address = re.sub(r"guid=[0-9a-f]{32}", f"guid={wrong}", other_address)
    elif case == "nobody-listens":
        a11y_address = nobody
    elif case == "healthy":
        a11y_address = other_address
    else:
        a11y_address, then = nobody, other_address
    embedded_log = os.path.join(runtime, "embedded")
    open(embedded_log, "w").close()

    fake = subprocess.Popen([sys.executable, "-I", os.path.abspath(__file__), "--fake-a11y", a11y_address,
                             "--then", then, "--registry-on", other_address, "--embedded-log", embedded_log],
                            stdout=subprocess.PIPE, text=True)
    if fake.stdout.readline().strip() != "ready":
        raise RuntimeError("the pretend accessibility service didn't start")

    log = open(os.path.join(runtime, "compositor.log"), "w")
    if display == "kwin_wayland":
        subprocess.Popen(["kwin_wayland", "--virtual", "--socket", SOCKET, "--width", "1280", "--height", "800"],
                         stdout=log, stderr=log, env=dict(os.environ, KWIN_COMPOSE="Q"))
        wait_for("kwin_wayland", lambda: os.path.exists(os.path.join(runtime, SOCKET)))
        app_display = {"WAYLAND_DISPLAY": SOCKET}
    else:
        subprocess.Popen(["Xvfb", ":91", "-nolisten", "tcp", "-screen", "0", "1280x800x24"], stdout=log, stderr=log)
        wait_for("Xvfb", lambda: os.path.exists("/tmp/.X11-unix/X91"))
        app_display = {"DISPLAY": ":91"}

    service = subprocess.Popen([sys.executable, "-I", os.path.join(REPO, "tests", "fake_service.py")],
                               stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL, cwd=REPO)
    wait_for("tests/fake_service.py", lambda: port_open(24880))

    stderr_path = os.path.join(runtime, "app.stderr")
    with open(stderr_path, "w") as stderr:
        app = subprocess.Popen([exe, "--app"], stdout=subprocess.DEVNULL, stderr=stderr, cwd=home,
                               env=dict(os.environ, LIBGL_ALWAYS_SOFTWARE="1", **app_display))
    time.sleep(SURVIVE)
    alive_at_start = app.poll() is None
    first = open(stderr_path).read()
    embeds_at_start = app_embeds(embedded_log, app.pid)
    windows_at_start = children(other_address, embeds_at_start)
    if alive_at_start:
        os.kill(fake.pid, signal.SIGUSR1)
        time.sleep(AFTER_TOGGLE)
    alive_after_toggle = app.poll() is None
    said = open(stderr_path).read()
    embeds = app_embeds(embedded_log, app.pid)
    windows = children(other_address, embeds)
    if app.poll() is None:
        app.terminate()
    print(json.dumps({"case": case, "display": display, "alive_at_start": alive_at_start,
                      "alive_after_toggle": alive_after_toggle, "exit": app.poll(),
                      "off_lines_at_start": first.count(OFF_LINE), "off_lines": said.count(OFF_LINE),
                      "embeds_at_start": len(embeds_at_start), "embeds": len(embeds), "embedded": embeds,
                      "windows_at_start": windows_at_start, "windows": windows,
                      "stderr": said[-3000:], "fake_service_running": service.poll() is None}), flush=True)


def gdbus(address, dest, path, method, *args):
    done = subprocess.run(["gdbus", "call", "--address", address, "--dest", dest, "--object-path", path,
                           "--method", method, *args], capture_output=True, text=True, timeout=10)
    return done.stdout if done.returncode == 0 else done.stderr.strip()


def app_embeds(embedded_log, pid):
    """The names the app embedded its root from. KWin, being a Qt program, embeds its own too."""
    lines = [line.split() for line in open(embedded_log).read().splitlines()]
    return [name for name, by in lines if int(by) == pid]


def children(address, embedded):
    """How many children the last app to embed has under its root, as a screen reader would see."""
    if not embedded:
        return None
    said = gdbus(address, embedded[-1], ROOT, "org.freedesktop.DBus.Properties.Get", "org.a11y.atspi.Accessible", "ChildCount")
    found = re.search(r"<(\d+)>", said)
    return int(found.group(1)) if found else said


# The run outside.

def sandboxed(case, exe, display):
    uid = os.getuid()
    command = ["bwrap", "--ro-bind", "/", "/", "--dev", "/dev", "--proc", "/proc",
               "--tmpfs", "/run", "--dir", f"/run/user/{uid}", "--tmpfs", "/tmp", "--dir", "/tmp/.X11-unix",
               "--unshare-all", "--die-with-parent", "--new-session", "--clearenv",
               "--setenv", "PATH", "/usr/local/bin:/usr/bin:/bin",
               "--", sys.executable, "-I", os.path.abspath(__file__), "--inside", case, "--inner-exe", exe, "--display", display]
    done = subprocess.run(command, capture_output=True, text=True, timeout=120)
    for line in done.stdout.splitlines():
        if line.startswith("{"):
            return json.loads(line)
    raise RuntimeError(f"the sandbox gave no result (exit {done.returncode}):\n{done.stdout}\n{done.stderr}")


def headless_display():
    for display in ("kwin_wayland", "Xvfb"):
        if shutil.which(display):
            return display
    return None


def main():
    parser = argparse.ArgumentParser()
    parser.add_argument("exe", nargs="?", default=os.environ.get("MESH_KVM_EXE", os.path.join(REPO, "target", "release", "mesh-kvm")))
    parser.add_argument("--expect-abort", action="store_true")
    parser.add_argument("--verbose", action="store_true", help="show what the app printed")
    parser.add_argument("--case", choices=CASES, action="append", help="run only this case (can be repeated)")
    parser.add_argument("--fake-a11y")
    parser.add_argument("--then", default="")
    parser.add_argument("--registry-on")
    parser.add_argument("--embedded-log")
    parser.add_argument("--inside", choices=CASES)
    parser.add_argument("--inner-exe")
    parser.add_argument("--display")
    args = parser.parse_args()
    if args.fake_a11y:
        return fake_a11y(args.fake_a11y, args.then, args.registry_on, args.embedded_log)
    if args.inside:
        return inside(args.inside, args.inner_exe, args.display)

    exe = os.path.abspath(args.exe)
    missing = [tool for tool in ("bwrap", "dbus-daemon", "gdbus") if not shutil.which(tool)]
    try:
        import gi  # noqa: F401
    except ImportError:
        missing.append("python3-gi")
    display = headless_display()
    if display is None:
        missing.append("a headless display (kwin_wayland or Xvfb)")
    if missing:
        print(f"SKIP: the app against a broken accessibility bus, as this machine has no {', '.join(missing)}. Nothing was started.")
        return 0
    if not os.access(exe, os.X_OK):
        print(f"FAIL: {exe} isn't there; build it first (cargo build --release).")
        return 1

    before = real_session()
    print(f"The real session before: {json.dumps(before)}", flush=True)
    failures = 0
    try:
        for case in args.case or CASES:
            result = sandboxed(case, exe, display)
            alive = result["alive_at_start"] and result["alive_after_toggle"]
            if case == "healthy":
                # The same with or without the fix: registered, with its window, before and after.
                ok = (alive and result["off_lines"] == 0 and result["embeds_at_start"] == 1 and result["embeds"] == 2
                      and result["windows_at_start"] == 1 and result["windows"] == 1)
                what = (f"registered with the accessibility bus ({result['embeds']} times, window seen: "
                        f"{result['windows_at_start']}, {result['windows']}) and said nothing")
            elif args.expect_abort:
                ok = not result["alive_at_start"] and "panicked" in result["stderr"]
                what = "aborts at start, as a build without the fix does"
            elif case == "recovers":
                ok = (alive and result["off_lines"] == 1 and result["embeds_at_start"] == 0 and result["embeds"] == 1
                      and result["windows"] == 1)
                what = (f"said \"{OFF_LINE}\" once, then registered with its window when IsEnabled came on again "
                        f"(window seen: {result['windows']})")
            else:
                ok = (alive and result["off_lines_at_start"] >= 1 and result["off_lines"] >= 2
                      and result["embeds"] == 0)
                what = (f"still running after {SURVIVE:.0f} s and after IsEnabled went off and on, "
                        f"and said \"{OFF_LINE}\" {result['off_lines']} times")
            print(f"{'PASS' if ok else 'FAIL'}: {case} under {display}: {what}", flush=True)
            if not ok:
                failures += 1
            if not ok or args.verbose:
                print(json.dumps(result, indent=1))
    finally:
        after = real_session()
        print(f"The real session after:  {json.dumps(after)}", flush=True)
        if after != before:
            print("FAIL: the real session's accessibility processes or bus address changed.")
            failures += 1
        else:
            print("PASS: the real session's accessibility processes and bus address are unchanged.")
    return 1 if failures else 0


if __name__ == "__main__":
    sys.exit(main())
