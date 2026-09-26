#!/usr/bin/env python3
# ==============================================================================
# populate_dpkg_status.py - Register installed debs into /var/lib/dpkg/status
# Ensures APT and dpkg recognize installed base packages and utim-init-dummy
# ==============================================================================
import glob
import os
import subprocess
import sys

def main():
    rootfs_dir = sys.argv[1] if len(sys.argv) > 1 else "build/rootfs"
    rootfs_status = os.path.join(rootfs_dir, "var/lib/dpkg/status")
    archives_dir = os.path.join(rootfs_dir, "var/cache/apt/archives")
    tmp_debs_dir = os.path.join(rootfs_dir, "tmp/debs")

    entries = []
    seen_packages = set()

    def process_deb(deb_path):
        if not os.path.exists(deb_path):
            return
        try:
            out = subprocess.check_output(["dpkg-deb", "-f", deb_path]).decode("utf-8").strip()
            lines = out.split("\n")
            if not lines or not lines[0].startswith("Package:"):
                return
            pkg_name = lines[0].split(":", 1)[1].strip()
            if pkg_name in seen_packages:
                return
            seen_packages.add(pkg_name)
            new_lines = [lines[0], "Status: install ok installed"] + lines[1:]
            entries.append("\n".join(new_lines))
        except Exception as e:
            sys.stderr.write(f"Warning: Failed to parse {deb_path}: {e}\n")

    # 1. Custom and dummy debs from tmp/debs
    for deb_path in sorted(glob.glob(os.path.join(tmp_debs_dir, "*.deb"))):
        process_deb(deb_path)

    # 2. Base debs from apt archives cache
    for deb_path in sorted(glob.glob(os.path.join(archives_dir, "*.deb"))):
        process_deb(deb_path)

    if not entries:
        print(f"[*] No deb packages found to register in {rootfs_status}.")
        return

    os.makedirs(os.path.dirname(rootfs_status), exist_ok=True)
    # Merge with the existing status database instead of replacing it (B-9):
    # base packages survive even when the archives cache is pruned (B-11).
    # Already-present packages keep their existing stanza.
    existing = ""
    if os.path.exists(rootfs_status):
        with open(rootfs_status, encoding="utf-8") as f:
            existing = f.read()
    existing_names = set()
    for chunk in existing.split("\n\n"):
        for line in chunk.splitlines():
            if line.startswith("Package:"):
                existing_names.add(line.split(":", 1)[1].strip())
                break
    fresh = [e for e in entries
             if e.split("\n", 1)[0].split(":", 1)[1].strip() not in existing_names]
    merged = existing.rstrip("\n")
    if merged:
        merged += "\n\n"
    merged += "\n\n".join(fresh) + "\n" if fresh else ("\n" if merged else "")
    if not merged.strip():
        print(f"[*] No deb packages found to register in {rootfs_status}.")
        return
    # Atomic same-directory rename: a crash never leaves a truncated db.
    tmp = rootfs_status + ".new"
    with open(tmp, "w", encoding="utf-8") as f:
        f.write(merged)
    os.replace(tmp, rootfs_status)

    print(f"[+] Successfully registered {len(fresh)} new packages in {rootfs_status}.")

if __name__ == "__main__":
    main()
