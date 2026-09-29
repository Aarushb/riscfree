"""One-off: verify the F1 menu accelerator opens the Keyboard Shortcuts dialog."""
import subprocess
import sys
import time
from pathlib import Path

import win32con
import win32gui

REPO = Path(__file__).resolve().parents[2]
EXE = REPO / "target-spike" / "debug" / "asaccess.exe"
VK_F1 = 0x70
WM_KEYDOWN, WM_KEYUP = 0x0100, 0x0101


def find_by_title(substr):
    hits = []

    def cb(h, _):
        t = win32gui.GetWindowText(h)
        if substr in t and win32gui.IsWindowVisible(h):
            hits.append((h, t))

    win32gui.EnumWindows(cb, None)
    return hits


proc = subprocess.Popen([str(EXE)])
try:
    hwnd = None
    deadline = time.time() + 30
    while time.time() < deadline and hwnd is None:
        time.sleep(0.5)
        hits = find_by_title("AsAccess")
        if hits:
            hwnd = hits[0][0]
    assert hwnd, f"main window missing; windows seen: {find_by_title('')[:10]}"
    print("main window:", hwnd)
    time.sleep(2.0)
    win32gui.PostMessage(hwnd, WM_KEYDOWN, VK_F1, 0)
    time.sleep(0.15)
    win32gui.PostMessage(hwnd, WM_KEYUP, VK_F1, 0)
    dlg = None
    deadline = time.time() + 8
    while time.time() < deadline and dlg is None:
        hits = find_by_title("Keyboard Shortcuts")
        if hits:
            dlg = hits[0][0]
        else:
            time.sleep(0.3)
    print("F1 OPENS SHORTCUTS DIALOG:", dlg is not None)
    if dlg:
        win32gui.PostMessage(dlg, win32con.WM_COMMAND, 2, 0)
        time.sleep(0.8)
finally:
    proc.terminate()
    time.sleep(1.0)
    if proc.poll() is None:
        proc.kill()
    print("app killed")
