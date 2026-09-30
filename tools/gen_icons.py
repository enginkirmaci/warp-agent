#!/usr/bin/env python3
"""Generate rust/assets/icons/*.svg from the App.xaml StreamGeometry table (single source)."""
import re, pathlib, sys

app_xaml = pathlib.Path(__file__).resolve().parents[1] / "src" / "App.xaml"
out_dir = pathlib.Path(__file__).resolve().parents[1] / "rust" / "assets" / "icons"
out_dir.mkdir(parents=True, exist_ok=True)

names = {
    "IconPlus": "plus", "IconKebab": "kebab", "IconTrash": "trash", "IconClose": "close",
    "IconSend": "send", "IconStop": "stop", "IconChevronDown": "chevron-down",
    "IconChevronRight": "chevron-right", "IconChatOutline": "chat-outline",
    "IconExpand": "expand", "IconArrowDown": "arrow-down", "IconArrowUp": "arrow-up",
    "IconGauge": "gauge", "IconClock": "clock", "IconDoc": "doc", "IconPin": "pin",
    "IconCopy": "copy", "IconCheck": "check", "IconPlay": "play", "IconDash": "dash",
    "IconCircle": "circle", "IconCircleCheck": "circle-check", "IconDot": "dot",
    "IconWarn": "warn", "IconBulb": "bulb", "IconShield": "shield",
    "IconShieldFilled": "shield-filled", "IconCog": "cog", "IconSpark": "spark",
    "IconFolder": "folder", "IconFolderOutline": "folder-outline",
    "IconFolderOpenOutline": "folder-open-outline", "IconSpinner": "spinner",
    "IconPanelLeft": "panel-left", "IconPanelLeftOpen": "panel-left-open", "IconEdit": "edit",
}

text = app_xaml.read_text(encoding="utf-8")
pattern = re.compile(r'<StreamGeometry x:Key="(Icon\w+)">(.*?)</StreamGeometry>', re.S)
count = 0
for key, data in pattern.findall(text):
    if key not in names:
        print(f"skip {key}")
        continue
    d = data.strip()
    fill_rule = ""
    if d.startswith("F0 "):
        fill_rule = ' fill-rule="evenodd"'
        d = d[3:]
    elif d.startswith("F1 "):
        fill_rule = ' fill-rule="nonzero"'
        d = d[3:]
    svg = (f'<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 24 24" width="24" height="24">\n'
           f'  <path{fill_rule} d="{d}"/>\n</svg>\n')
    (out_dir / f"{names[key]}.svg").write_text(svg, encoding="utf-8")
    count += 1
print(f"wrote {count} icons to {out_dir}")
sys.exit(0 if count == len(names) else 1)
