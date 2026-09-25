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
    with open(rootfs_status, "w", encoding="utf-8") as f:
        f.write("\n\n".join(entries) + "\n")

    print(f"[+] Successfully registered {len(entries)} packages in {rootfs_status}.")

if __name__ == "__main__":
    main()
