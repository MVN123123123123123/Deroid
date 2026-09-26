#!/usr/bin/env python3
"""
download_icons.py - Download and prepare high-definition 64x64 PNG icons
from the Android / Pixel launcher icon pack (Lawnicons).

Downloads official Pixel/Android adaptive icon SVGs, renders crisp 64x64 PNGs with
white emblems for adaptive squircles, and installs them to assets and rootfs.
"""

import os
import shutil
import subprocess
import sys
import tempfile
import urllib.request

ICONS = {
    # Key -> (Lawnicons Pixel/Android SVG filename, list of target filenames to generate)
    "phone": (
        "google_phone.svg",
        ["phone.png", "call-start.png"]
    ),
    "messages": (
        "google_messages.svg",
        ["mail-message-new.png", "messages.png"]
    ),
    "browser": (
        "google_chrome.svg",
        ["web-browser.png", "browser.png", "internet-web-browser.png"]
    ),
    "camera": (
        "gcamera.svg",
        ["camera-photo.png", "camera.png"]
    ),
    "gallery": (
        "google_photos.svg",
        ["image-x-generic.png", "gallery.png"]
    ),
    "settings": (
        "generic_settings.svg",
        ["preferences-system.png", "settings.png"]
    ),
    "files": (
        "files_by_google.svg",
        ["system-file-manager.png", "files.png"]
    ),
    "music": (
        "youtube_music_revanced.svg",
        ["audio-x-generic.png", "music.png"]
    ),
    "terminal": (
        "termux.svg",
        ["utilities-terminal.png", "terminal.png"]
    ),
    "treble": (
        "android_12_launcher.svg",
        ["computer.png", "treble.png", "distributor-logo-android.png", "android.png"]
    ),
    "contacts": (
        "google_contacts.svg",
        ["contact-new.png", "contacts.png"]
    ),
    "clock": (
        "aosp_clock.svg",
        ["clock.png"]
    ),
    "apps": (
        "lawnchair.svg",
        ["view-app-grid.png", "apps.png"]
    ),
}

BASE_URL = "https://raw.githubusercontent.com/LawnchairLauncher/lawnicons/develop/svgs/"

def main():
    script_dir = os.path.dirname(os.path.abspath(__file__))
    workspace_root = os.path.dirname(script_dir)
    assets_icons_dir = os.path.join(workspace_root, "assets", "icons", "hicolor", "64x64", "apps")
    assets_svgs_dir = os.path.join(workspace_root, "assets", "icons", "svgs")
    rootfs_icons_dir = os.path.join(workspace_root, "build", "rootfs", "usr", "share", "icons", "hicolor", "64x64", "apps")
    rootfs_pixmaps_dir = os.path.join(workspace_root, "build", "rootfs", "usr", "share", "pixmaps")

    # NOTE: BASE_URL tracks the mutable 'develop' branch with no digest pin.
    # A force-push or compromised upstream silently changes the baked icons.
    # Prefer the vendored SVGs under assets/icons/svgs/; treat this fetch as
    # best-effort and never publish a partially-rendered set.
    print("[!] WARNING: fetching icons from unpinned 'develop' branch (no digest verification).",
          file=sys.stderr)

    if shutil.which("rsvg-convert") is None:
        raise SystemExit("download_icons.py: rsvg-convert not found; cannot render icons")

    os.makedirs(assets_svgs_dir, exist_ok=True)

    tmp_dir = tempfile.mkdtemp(prefix="utlc_icons_tmp_")
    stage_dir = tempfile.mkdtemp(prefix="utlc_icons_stage_")
    # (staged png, [final destinations]) — published only after ALL icons render.
    pending = []
    # (svg cache path, svg text) — cached only after the icon renders OK.
    cache_writes = []
    try:
        print("[*] Fetching and rendering Android 17 / Pixel launcher PNG icons...")

        for key, (svg_filename, target_names) in ICONS.items():
            svg_cache_path = os.path.join(assets_svgs_dir, svg_filename)
            svg_data = None
            fetched = None

            if os.path.exists(svg_cache_path):
                with open(svg_cache_path, "r", encoding="utf-8") as f:
                    svg_data = f.read()
            else:
                url = BASE_URL + svg_filename
                print(f"  [-] Downloading {key} ({svg_filename}) from {url}...")
                req = urllib.request.Request(url, headers={"User-Agent": "curl/7.68.0"})
                with urllib.request.urlopen(req, timeout=30) as resp:
                    fetched = resp.read().decode("utf-8")
                svg_data = fetched

            # Convert monochrome black stroke/fill to crisp white for launcher squircles
            svg_styled = svg_data.replace('stroke="#000"', 'stroke="#FFFFFF"').replace('fill="#000"', 'fill="#FFFFFF"')
            if "viewBox" not in svg_styled:
                svg_styled = svg_styled.replace("<svg ", '<svg viewBox="0 0 192 192" ')

            tmp_svg = os.path.join(tmp_dir, f"{key}.svg")
            staged_png = os.path.join(stage_dir, f"{key}.png")

            with open(tmp_svg, "w", encoding="utf-8") as f:
                f.write(svg_styled)

            subprocess.run(["rsvg-convert", "-w", "64", "-h", "64", tmp_svg, "-o", staged_png], check=True)

            # Render succeeded: stage the PNG and remember the SVG cache write.
            if fetched is not None:
                cache_writes.append((svg_cache_path, fetched))
            for name in target_names:
                dst_asset = os.path.join(assets_icons_dir, name)
                dst_rootfs = os.path.join(rootfs_icons_dir, name)
                dst_pixmap = os.path.join(rootfs_pixmaps_dir, name)
                pending.append((staged_png, [dst_asset, dst_rootfs, dst_pixmap]))

        # All icons rendered: commit the SVG cache, then atomically publish
        # each PNG (copy to .new + os.replace) so a failure never leaves a
        # half-populated icon set behind.
        for svg_cache_path, svg_text in cache_writes:
            with open(svg_cache_path, "w", encoding="utf-8") as f:
                f.write(svg_text)
        for staged_png, dests in pending:
            for dest in dests:
                os.makedirs(os.path.dirname(dest), exist_ok=True)
                tmp_dest = dest + ".new"
                shutil.copyfile(staged_png, tmp_dest)
                os.replace(tmp_dest, dest)
    finally:
        shutil.rmtree(tmp_dir, ignore_errors=True)
        shutil.rmtree(stage_dir, ignore_errors=True)

    print(f"[+] Successfully installed Android 17 / Pixel icons to {assets_icons_dir} and rootfs!")

if __name__ == "__main__":
    main()
