"""Functional end-to-end test: assemble and run a program through the real
GUI with the RARS-conventional F-keys, then verify the Run I/O transcript and
status bar through the UIA tree.

Usage: python functional_test.py [path-to-asaccess.exe]
"""

import sys
import time
from pathlib import Path

import ctypes
import comtypes.client
import win32gui
import win32process
import win32con
from pywinauto import Application

REPO = Path(__file__).resolve().parents[2]
EXE = Path(sys.argv[1]) if len(sys.argv) > 1 else REPO / "target" / "debug" / "asaccess.exe"
TITLE = "AsAccess - RISC-V Assembly IDE"

steps: list[str] = []


def step(msg: str) -> None:
    steps.append(msg)
    print(f"[{time.strftime('%H:%M:%S')}] {msg}", flush=True)


def post_key(hwnd: int, vk: int) -> None:
    win32gui.PostMessage(hwnd, win32con.WM_KEYDOWN, vk, 0)
    time.sleep(0.05)
    win32gui.PostMessage(hwnd, win32con.WM_KEYUP, vk, 0)


# Property ids from UIAutomationClient, matching probe_app.py's approach.
UIA_NamePropertyId = 30005
UIA_ValueValuePropertyId = 30045
UIA_ClassNamePropertyId = 30012
UIA_IsValuePatternAvailablePropertyId = 30039
TreeScope_Descendants = 4


def prop(element, property_id: int):
    try:
        return element.GetCurrentPropertyValue(property_id)
    except Exception:
        return None


def collect_statuses(iuia, hwnd: int, out: list) -> None:
    root = iuia.ElementFromHandle(hwnd)
    found = root.FindAll(TreeScope_Descendants, iuia.CreateTrueCondition())
    for i in range(found.Length):
        e = found.GetElement(i)
        name = prop(e, UIA_NamePropertyId) or ""
        value = prop(e, UIA_ValueValuePropertyId) or ""
        classname = prop(e, UIA_ClassNamePropertyId) or ""
        if (
            classname == "msctls_statusbar32"
            or "executed" in name
            or "Assembled" in name
            or "Assembly failed" in name
            or name.startswith("pc 0x")
            or name.startswith("line ")
            or name == "Run I O output"
        ):
            out.append((name, value, classname))


def main() -> int:
    if not EXE.exists():
        print(f"ERROR: exe not found: {EXE}", file=sys.stderr)
        return 2

    app = Application(backend="uia").start(str(EXE), wait_for_idle=False)
    step(f"app started, pid={app.process}")
    try:
        def find_window_for_pid(pid: int) -> int:
            found = []

            def cb(h: int, _):
                _, wpid = win32process.GetWindowThreadProcessId(h)
                if wpid == pid and win32gui.GetWindowText(h) == TITLE:
                    found.append(h)
                return True

            CMPFUNC = ctypes.WINFUNCTYPE(ctypes.c_bool, ctypes.c_void_p, ctypes.c_void_p)
            win32gui.EnumWindows(CMPFUNC(cb), 0)
            return found[0] if found else 0

        hwnd = 0
        deadline = time.time() + 30
        while time.time() < deadline and not hwnd:
            hwnd = find_window_for_pid(app.process)
            time.sleep(0.25)
        if not hwnd:
            raise RuntimeError("main window never appeared")
        step("main window found")

        time.sleep(1.0)
        post_key(hwnd, 0x72)  # F3: assemble
        step("sent F3 (assemble)")
        time.sleep(2.0)
        post_key(hwnd, 0x74)  # F5: run
        step("sent F5 (run)")
        time.sleep(3.0)

        # Read the status bar via UIA FindAll (server-side walk; recursive
        # client walks hang on this app, as probe_app.py documents).
        from comtypes.gen.UIAutomationClient import IUIAutomation
        iuia = comtypes.client.CreateObject(
            "{ff48dba4-60ef-4201-aa87-54103eef594e}", interface=IUIAutomation
        )
        found: list = []
        collect_statuses(iuia, hwnd, found)
        step(f"walked tree: {len(found)} status-related elements")
        for name, value, classname in found:
            print(f"    name={name!r} value={value!r} class={classname!r}")

        joined = " | ".join(f"{n} {v}" for n, v, _ in found)
        ok = "executed" in joined and "12" in joined
        step("PASS" if ok else "FAIL")
        for line in steps:
            print(line)
        return 0 if ok else 1
    finally:
        try:
            app.kill()
        except Exception:
            pass


if __name__ == "__main__":
    sys.exit(main())
