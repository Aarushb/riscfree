"""UIA accessibility probe for the AsAccess wxDragon spike (Phase 0 / Spike 1).

Starts the built debug exe and dumps the UIA element tree (via UI Automation
FindAll, the same backend pywinauto's 'uia' backend is built on) to
uia_dump.txt. Then:
  1. selects the "Registers (List)" notebook tab via the UIA SelectionItem
     pattern and re-dumps, so grid vs list exposure can be compared;
  2. opens File > Settings (WM_COMMAND, as a menu invocation) and dumps it;
  3. opens Help > Keyboard Shortcuts (menu id 3001, item has F1 accelerator)
     and dumps it;
and prints a per-widget summary. Always terminates the app it started.

Notes from this environment:
- pywinauto Application.start() needs wait_for_idle=False here, otherwise
  WaitForInputIdle never returns for the wxWidgets app.
- A naive recursive Python walk of the UIA tree (pywinauto element_info
  children()) never terminated on this app; FindAll (server-side walk) is
  cycle-safe and returns in <0.2s, so it is used instead.

Run with: tests/uiauto/.venv/Scripts/python.exe tests/uiauto/probe_app.py
"""

import sys
import time
from pathlib import Path

import comtypes.client
import win32con
import win32gui
import win32process
from pywinauto import Application

from comtypes.gen.UIAutomationClient import (  # noqa: E402
    IUIAutomation,
    IUIAutomationTablePattern,
    UIA_AccessKeyPropertyId,
    UIA_ClassNamePropertyId,
    UIA_ControlTypePropertyId,
    UIA_GridPatternId,
    UIA_HelpTextPropertyId,
    UIA_IsGridPatternAvailablePropertyId,
    UIA_IsLegacyIAccessiblePatternAvailablePropertyId,
    UIA_IsTablePatternAvailablePropertyId,
    UIA_IsValuePatternAvailablePropertyId,
    UIA_LegacyIAccessibleNamePropertyId,
    UIA_NamePropertyId,
    UIA_SelectionItemPatternId,
    UIA_TablePatternId,
    UIA_ValueValuePropertyId,
    TreeScope_Descendants,
)

REPO = Path(__file__).resolve().parents[2]
EXE = REPO / "target-spike" / "debug" / "asaccess.exe"
OUT = Path(__file__).resolve().parent / "uia_dump.txt"
SLOG = Path(__file__).resolve().parent / "probe_steps.log"
_slog = SLOG.open("w", encoding="utf-8")
_T0 = time.time()


def step(msg):
    """Timestamped step log (flushed) so hangs can be located."""
    line = f"[{time.time() - _T0:7.2f}s] {msg}"
    _slog.write(line + "\n")
    _slog.flush()
    print(line, flush=True)

WM_COMMAND = win32con.WM_COMMAND
ID_SETTINGS = 1004  # File > Settings...
ID_SHORTCUTS = 3001  # Help > Keyboard Shortcuts (F1)
IDCANCEL = 2  # standard wxDialog cancel id

import comtypes.gen.UIAutomationClient as uiacl  # noqa: E402

# Reverse map: numeric control type -> "Button", "Edit", ...
CTYPES = {}
for attr in dir(uiacl):
    if attr.endswith("ControlTypeId"):
        try:
            CTYPES[int(getattr(uiacl, attr))] = attr.replace("UIA_", "").replace(
                "ControlTypeId", ""
            )
        except (TypeError, ValueError):
            pass


def prop(elem, pid):
    try:
        return elem.GetCurrentPropertyValue(pid)
    except Exception:
        return None


def ctype_name(elem):
    v = prop(elem, UIA_ControlTypePropertyId)
    return CTYPES.get(v, f"type{v}")


def dump_elements(iuia, hwnd, lines, label):
    """FindAll over the whole subtree of hwnd and dump every element."""
    lines.append("")
    lines.append("=" * 110)
    lines.append(label)
    lines.append("=" * 110)
    try:
        root = iuia.ElementFromHandle(hwnd)
    except Exception as exc:
        lines.append(f"<ElementFromHandle failed: {exc}>")
        return
    cond = iuia.CreateTrueCondition()
    found = root.FindAll(TreeScope_Descendants, cond)
    lines.append(f"(subtree element count: {found.Length})")
    for i in range(found.Length):
        e = found.GetElement(i)
        try:
            name = prop(e, UIA_NamePropertyId) or ""
            val = prop(e, UIA_ValueValuePropertyId) or ""
            has_val = bool(prop(e, UIA_IsValuePatternAvailablePropertyId))
            ak = prop(e, UIA_AccessKeyPropertyId) or ""
            help_txt = prop(e, UIA_HelpTextPropertyId) or ""
            cls = prop(e, UIA_ClassNamePropertyId) or ""
            legacy = prop(e, UIA_LegacyIAccessibleNamePropertyId) or ""
            grid_av = bool(prop(e, UIA_IsGridPatternAvailablePropertyId))
            table_av = bool(prop(e, UIA_IsTablePatternAvailablePropertyId))
            legacy_av = bool(prop(e, UIA_IsLegacyIAccessiblePatternAvailablePropertyId))
            extra = ""
            if table_av and table_av is not None:
                try:
                    tbl = e.GetCurrentPattern(UIA_TablePatternId).QueryInterface(
                        IUIAutomationTablePattern
                    )
                    extra = f" | TableRows={tbl.CurrentRowCount} TableCols={tbl.CurrentColumnCount}"
                except Exception:
                    extra = " | TablePattern=binding-failed"
            lines.append(
                f"[{i:3}] {ctype_name(e):<12} Name={name!r} HasValue={has_val} "
                f"Value={val!r} AccKey={ak!r} Help={help_txt!r} Class={cls!r} "
                f"GridPat={grid_av} TablePat={table_av} LegacyIA2={legacy_av} "
                f"LegacyName={legacy!r}{extra}"
            )
        except Exception as exc:
            lines.append(f"[{i:3}] <property error: {exc}>")


def find_top_window(title_exact, pid, timeout=10.0):
    """FindWindow by exact title belonging to our process."""
    deadline = time.time() + timeout
    while time.time() < deadline:
        hwnd = win32gui.FindWindow(None, title_exact)
        if hwnd:
            _, wpid = win32process.GetWindowThreadProcessId(hwnd)
            if wpid == pid:
                return hwnd
        time.sleep(0.25)
    return None


def main():
    if not EXE.exists():
        print(f"ERROR: exe not found: {EXE}", file=sys.stderr)
        return 2

    lines = []
    step("starting app (wait_for_idle=False)")
    app = Application(backend="uia").start(str(EXE), wait_for_idle=False)
    pid = app.process
    step(f"app started, pid={pid}")
    iuia = comtypes.client.CreateObject(
        "{ff48dba4-60ef-4201-aa87-54103eef594e}", interface=IUIAutomation
    )
    step("IUIAutomation created")
    try:
        # Wait for the main frame (class wxWindowNR, exact title).
        deadline = time.time() + 60
        hwnd = None
        while time.time() < deadline and hwnd is None:
            hwnd = find_top_window("AsAccess - RISC-V Assembly IDE", pid, timeout=1.0)
        if hwnd is None:
            raise RuntimeError("main window never appeared")
        step(f"main window hwnd={hwnd}")
        time.sleep(2.0)  # let wx finish layout so the UIA tree is complete

        dump_elements(
            iuia, hwnd, lines, "DUMP 1: main window, 'Registers (Grid)' tab active"
        )
        step("dump 1 done")

        # --- Switch to the Registers (List) tab via UIA SelectionItem ---
        try:
            root = iuia.ElementFromHandle(hwnd)
            found = root.FindAll(TreeScope_Descendants, iuia.CreateTrueCondition())
            target = None
            for i in range(found.Length):
                e = found.GetElement(i)
                if (
                    ctype_name(e) == "TabItem"
                    and (prop(e, UIA_NamePropertyId) or "") == "Registers (List)"
                ):
                    target = e
                    break
            if target is None:
                raise RuntimeError("TabItem 'Registers (List)' not found")
            sel = target.GetCurrentPattern(UIA_SelectionItemPatternId).QueryInterface(
                uiacl.IUIAutomationSelectionItemPattern
            )
            sel.Select()
            time.sleep(1.5)
            dump_elements(
                iuia,
                hwnd,
                lines,
                "DUMP 2: after selecting 'Registers (List)' tab "
                "(UIA SelectionItem pattern; dump of same hwnd subtree)",
            )
            step("dump 2 done")
        except Exception as exc:
            lines.append(f"[tab switch failed: {exc}]")

        # --- File > Settings dialog (WM_COMMAND = menu invocation) ---
        try:
            win32gui.PostMessage(hwnd, WM_COMMAND, ID_SETTINGS, 0)
            dlg = find_top_window("Settings", pid)
            if dlg is None:
                raise RuntimeError("Settings dialog window not found")
            time.sleep(1.0)
            dump_elements(iuia, dlg, lines, "DUMP 3: Settings dialog")
            step("dump 3 done")
            win32gui.PostMessage(dlg, WM_COMMAND, IDCANCEL, 0)
            time.sleep(0.8)
        except Exception as exc:
            lines.append(f"[settings dialog probe failed: {exc}]")

        # --- Help > Keyboard Shortcuts (F1 accelerator menu item) ---
        try:
            win32gui.PostMessage(hwnd, WM_COMMAND, ID_SHORTCUTS, 0)
            dlg = find_top_window("Keyboard Shortcuts", pid)
            if dlg is None:
                raise RuntimeError("Keyboard Shortcuts dialog window not found")
            time.sleep(1.0)
            dump_elements(iuia, dlg, lines, "DUMP 4: Keyboard Shortcuts dialog")
            step("dump 4 done")
            win32gui.PostMessage(dlg, WM_COMMAND, IDCANCEL, 0)
            time.sleep(0.8)
        except Exception as exc:
            lines.append(f"[shortcuts dialog probe failed: {exc}]")

    finally:
        step("entering finally: killing app")
        try:
            app.kill()
        except Exception:
            step("app.kill() raised")
        step("app killed")

    # --- Probe's own summary ---
    summary = ["", "=" * 110, "SUMMARY (probe's own reading)", "=" * 110]

    # Parse dump lines: "[ 12] Button Name='x' HasValue=True Value='' ..." plus
    # current DUMP header tracking.
    import re

    row_re = re.compile(r"^\[\s*(\d+)\]\s+(\w+)\s+Name='(.*?)'\s+HasValue=(\w+)")
    current_dump = "?"
    records = []  # (dump_header, ctype, name, has_value, line)
    for ln in lines:
        if ln.startswith("DUMP"):
            current_dump = ln.strip()
            continue
        m = row_re.match(ln.strip())
        if m:
            records.append((current_dump, m.group(2), m.group(3), m.group(4), ln.strip()))

    total = len(records)
    empty_named = sum(1 for r in records if r[2] == "")
    summary.append(f"elements dumped: {total}; with empty Name: {empty_named}")

    def show(label, pred, limit=8):
        hits = [r for r in records if pred(r)]
        summary.append(f"\n{label}: {len(hits)} element(s)")
        summary.extend("  " + r[4] for r in hits[:limit])

    show(
        "EDITOR (Document elements)",
        lambda r: r[1] == "Document",
    )
    show(
        "GRID table (Table/Pane named 'Register values grid table')",
        lambda r: "Register values grid table" in r[2],
    )
    show(
        "LIST table (named 'Register values list table')",
        lambda r: "Register values list table" in r[2],
    )
    show("TABLE/LIST ROWS as UIA DataItem elements", lambda r: r[1] == "DataItem", 24)
    show("STATUS BAR elements", lambda r: r[1] == "StatusBar" or "Status bar" in r[2], 6)
    show(
        "BUTTONS (non-empty Name)",
        lambda r: r[1] == "Button" and r[2] != "",
        16,
    )
    show("MENU ITEMS (menubar level)", lambda r: r[1] == "MenuItem", 12)
    show("TAB ITEMS", lambda r: r[1] == "TabItem", 6)
    show(
        "SETTINGS dialog contents (DUMP 3)",
        lambda r: "DUMP 3" in r[0],
        20,
    )
    show(
        "SHORTCUTS dialog contents (DUMP 4)",
        lambda r: "DUMP 4" in r[0],
        20,
    )

    text = "\n".join(lines) + "\n" + "\n".join(summary) + "\n"
    OUT.write_text(text, encoding="utf-8")
    print(f"wrote {OUT}")
    print("\n".join(summary))
    return 0


if __name__ == "__main__":
    sys.exit(main())
