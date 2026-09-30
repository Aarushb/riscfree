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
import win32api
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
            or "Stopped" in name
            or "Breakpoint" in name
            or "Program finished" in name
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

        from comtypes.gen.UIAutomationClient import IUIAutomation
        iuia = comtypes.client.CreateObject(
            "{ff48dba4-60ef-4201-aa87-54103eef594e}", interface=IUIAutomation
        )

        # Editor tab navigation: Tab must move focus out of the editor and
        # must not insert a tab character (the focus-trap fix). The key is
        # posted to the Scintilla child itself, as a real keystroke would be.
        time.sleep(1.0)
        scintillas = []
        win32gui.EnumChildWindows(
            hwnd, lambda h, _param: scintillas.append(h), None
        )
        scintillas = [
            h for h in scintillas if win32gui.GetClassName(h) == "Scintilla"
        ]
        tab_ok = False
        if scintillas:
            # The Scintilla child sits inside a wx wrapper window; the wx
            # event chain (and thus the tab handler) lives on the wrapper.
            wrapper = win32gui.GetParent(scintillas[0])

            def attached() -> "object":
                tid_app, _pid = win32process.GetWindowThreadProcessId(hwnd)
                tid_cur = win32api.GetCurrentThreadId()
                win32process.AttachThreadInput(tid_cur, tid_app, True)
                return tid_app, tid_cur

            def detach(tid_app: int, tid_cur: int) -> None:
                win32process.AttachThreadInput(tid_cur, tid_app, False)

            def app_focus() -> int:
                tid_app, tid_cur = attached()
                try:
                    return win32gui.GetFocus() or 0
                finally:
                    detach(tid_app, tid_cur)

            def set_app_focus(target: int) -> None:
                tid_app, tid_cur = attached()
                try:
                    win32gui.SetFocus(target)
                finally:
                    detach(tid_app, tid_cur)

            set_app_focus(wrapper)
            started_on_editor = app_focus() == wrapper
            root = iuia.ElementFromHandle(hwnd)
            els = root.FindAll(TreeScope_Descendants, iuia.CreateTrueCondition())
            before_value = None
            for i in range(els.Length):
                e = els.GetElement(i)
                if prop(e, UIA_ClassNamePropertyId) == "Scintilla":
                    before_value = prop(e, UIA_ValueValuePropertyId)
                    break
            post_key(wrapper, 0x09)  # Tab
            time.sleep(0.8)
            focus_after = app_focus()
            els = root.FindAll(TreeScope_Descendants, iuia.CreateTrueCondition())
            after_value = None
            for i in range(els.Length):
                e = els.GetElement(i)
                if prop(e, UIA_ClassNamePropertyId) == "Scintilla":
                    after_value = prop(e, UIA_ValueValuePropertyId)
                    break
            left = focus_after not in (0, wrapper, scintillas[0])
            tab_ok = started_on_editor and left and before_value == after_value
            step(
                f"tab from editor: started_on_editor={started_on_editor}, "
                f"focus left={left}, text unchanged="
                f"{before_value == after_value} "
                f"({'PASS' if tab_ok else 'FAIL'})"
            )
        else:
            step("no Scintilla child found for tab check")

        time.sleep(0.3)
        post_key(hwnd, 0x72)  # F3: assemble
        step("sent F3 (assemble)")
        time.sleep(2.0)
        post_key(hwnd, 0x74)  # F5: run
        step("sent F5 (run)")
        time.sleep(3.0)

        # Read the status bar via UIA FindAll (server-side walk; recursive
        # client walks hang on this app, as probe_app.py documents).
        found: list = []
        collect_statuses(iuia, hwnd, found)
        step(f"walked tree: {len(found)} status-related elements")
        for name, value, classname in found:
            print(f"    name={name!r} value={value!r} class={classname!r}")

        joined = " | ".join(f"{n} {v}" for n, v, _ in found)
        ok = tab_ok and "executed" in joined and "12" in joined

        # Breakpoint flow: reset (F12), Ctrl+D toggles on the row at the
        # current PC (row 0 after reset), F5 stops there immediately.
        time.sleep(0.3)
        post_key(hwnd, 0x7B)  # F12: reset
        step("sent F12 (reset)")
        time.sleep(1.0)
        # Toggle Breakpoint via WM_COMMAND (the menu handler). Posted synthetic
        # Ctrl chords do not update the key state menu accelerators check.
        win32gui.PostMessage(hwnd, win32con.WM_COMMAND, 2009, 0)
        step("sent WM_COMMAND Toggle Breakpoint")
        time.sleep(0.5)
        post_key(hwnd, 0x74)  # F5: run to breakpoint
        step("sent F5 (run to breakpoint)")
        time.sleep(2.0)

        found2: list = []
        collect_statuses(iuia, hwnd, found2)
        joined2 = " | ".join(f"{n} {v}" for n, v, _ in found2)
        step(f"breakpoint walk: {joined2[:200]}")
        ok = ok and "Stopped" in joined2
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
