#!/usr/bin/env bash
# ==============================================================================
# build_initramfs.sh - Build Early Bootloader Initramfs for QEMU & Android GKI
#
# Android GKI kernels (kernel-ranchu) compile storage and bus drivers
# (virtio_blk, virtio_pci) as loadable modules rather than built-in drivers.
# This script creates an initramfs that:
# 1. Mounts early filesystems (/proc, /sys, /dev tmpfs)
# 2. Loads virtio kernel modules (virtio_pci_modern_dev, virtio_pci_legacy_dev, virtio_pci, virtio_blk)
# 3. Discovers the root block device (/dev/vda) from /sys/block/vda/dev
# 4. Mounts /dev/vda on /sysroot
# 5. Switches root to /sysroot and executes /init (UTIM PID 1)
# ==============================================================================
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
WORKSPACE_ROOT="$(cd "${SCRIPT_DIR}/.." && pwd)"

OUTPUT_INITRAMFS="${1:-${WORKSPACE_ROOT}/dist/initramfs.cpio.gz}"
if [[ "${OUTPUT_INITRAMFS}" != /* ]]; then
    OUTPUT_INITRAMFS="${PWD}/${OUTPUT_INITRAMFS}"
fi
RAMDISK_BUILD_DIR="${WORKSPACE_ROOT}/build/initramfs"
ANDROID_RAMDISK="/opt/android-sdk/system-images/android-34/google_apis/arm64-v8a/ramdisk.img"
ANDROID_VENDOR="/opt/android-sdk/system-images/android-34/google_apis/arm64-v8a/vendor.img"

echo "============================================================"
echo " Building Universal Treble Linux Initramfs for Android GKI"
echo " Target Output: ${OUTPUT_INITRAMFS}"
echo "============================================================"

mkdir -p "${WORKSPACE_ROOT}/dist/modules"
rm -rf "${RAMDISK_BUILD_DIR}"
mkdir -p "${RAMDISK_BUILD_DIR}"/{bin,dev,proc,sys,lib/modules,sysroot}

# 1. Extract virtio kernel modules from Android emulator ramdisk
if [[ -f "${ANDROID_RAMDISK}" ]]; then
    echo "[*] Extracting virtio kernel modules from ${ANDROID_RAMDISK}..."
    python3 -c '
import subprocess, sys

ramdisk_path = sys.argv[1]
dest_dir = sys.argv[2]
data = subprocess.check_output(["lz4", "-d", "-c", ramdisk_path])

pos = 0
while pos < len(data):
    if data[pos:pos+6] != b"070701":
        idx = data.find(b"070701", pos)
        if idx == -1:
            break
        pos = idx
    header = data[pos:pos+110]
    namesize = int(header[94:102], 16)
    filesize = int(header[54:62], 16)
    name = data[pos+110:pos+110+namesize-1].decode("utf-8", "replace")
    pos += 110 + namesize
    if pos % 4 != 0:
        pos += (4 - (pos % 4))
    filedata = data[pos:pos+filesize]
    if name.startswith("lib/modules/") and name.endswith(".ko"):
        outpath = f"{dest_dir}/{name}"
        with open(outpath, "wb") as f:
            f.write(filedata)
        print(f"    Extracted {name}")
    pos += filesize
    if pos % 4 != 0:
        pos += (4 - (pos % 4))
' "${ANDROID_RAMDISK}" "${RAMDISK_BUILD_DIR}"
fi

# 1b. Extract GPU & Display kernel modules from Android vendor partition
if [[ -f "${ANDROID_VENDOR}" ]]; then
    echo "[*] Extracting GPU and DRM kernel modules from ${ANDROID_VENDOR}..."
    7z e -y "${ANDROID_VENDOR}" \
        lib/modules/virtio-gpu.ko \
        lib/modules/drm_dma_helper.ko \
        lib/modules/virtio_input.ko \
        "-o${RAMDISK_BUILD_DIR}/lib/modules" >/dev/null 2>&1 || true
fi

# Cache extracted modules and populate fallbacks
cp -a "${RAMDISK_BUILD_DIR}/lib/modules/"*.ko "${WORKSPACE_ROOT}/dist/modules/" 2>/dev/null || true
if [[ -d "${WORKSPACE_ROOT}/dist/modules" ]]; then
    cp -a "${WORKSPACE_ROOT}/dist/modules/"*.ko "${RAMDISK_BUILD_DIR}/lib/modules/" 2>/dev/null || true
fi

# 2. Compile static early init executable
echo "[*] Compiling static early init loader (aarch64)..."
cat << 'EOF' > "${RAMDISK_BUILD_DIR}/init.c"
#define _GNU_SOURCE
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <unistd.h>
#include <sys/syscall.h>
#include <fcntl.h>
#include <sys/mount.h>
#include <sys/stat.h>
#include <sys/sysmacros.h>
#include <errno.h>

static int load_module(const char *path) {
    int fd = open(path, O_RDONLY | O_CLOEXEC);
    if (fd < 0) return -1;
    int ret = syscall(SYS_finit_module, fd, "", 0);
    close(fd);
    return ret;
}

static void parse_cmdline_param(const char *key, char *out, size_t out_len) {
    int fd = open("/proc/cmdline", O_RDONLY);
    if (fd < 0) return;
    char buf[1024] = {0};
    ssize_t n = read(fd, buf, sizeof(buf) - 1);
    close(fd);
    if (n <= 0) return;

    char *p = strstr(buf, key);
    if (p) {
        p += strlen(key);
        size_t i = 0;
        while (*p && *p != ' ' && *p != '\n' && *p != '\r' && i + 1 < out_len) {
            out[i++] = *p++;
        }
        out[i] = '\0';
    }
}

int main(int argc, char *argv[]) {
    printf("\n============================================================\n");
    printf(" [UTIM-BOOT] Universal Treble Linux Early Initramfs Loader\n");
    printf("============================================================\n");

    mount("tmpfs", "/dev", "tmpfs", 0, "mode=0755");
    mount("proc", "/proc", "proc", 0, NULL);
    mount("sysfs", "/sys", "sysfs", 0, NULL);

    // Load virtio bus and block drivers required by Android GKI / QEMU
    load_module("/lib/modules/virtio_pci_modern_dev.ko");
    load_module("/lib/modules/virtio_pci_legacy_dev.ko");
    load_module("/lib/modules/virtio_pci.ko");
    load_module("/lib/modules/virtio_blk.ko");
    load_module("/lib/modules/virtio_console.ko");
    load_module("/lib/modules/virtio-rng.ko");

    // Load DRM KMS & input drivers for graphical display
    load_module("/lib/modules/virtio_dma_buf.ko");
    load_module("/lib/modules/drm_dma_helper.ko");
    load_module("/lib/modules/virtio-gpu.ko");
    load_module("/lib/modules/virtio_input.ko");

    // Parse root= and init= from kernel cmdline
    char root_dev[128] = "/dev/vda";
    char init_path[128] = "/init";
    parse_cmdline_param("root=", root_dev, sizeof(root_dev));
    parse_cmdline_param("init=", init_path, sizeof(init_path));

    const char *dev_name = strrchr(root_dev, '/');
    dev_name = dev_name ? dev_name + 1 : root_dev;

    // Poll for block device in sysfs
    char sys_path[256];
    snprintf(sys_path, sizeof(sys_path), "/sys/block/%s/dev", dev_name);

    int fd = -1;
    for (int i = 0; i < 50; i++) {
        fd = open(sys_path, O_RDONLY);
        if (fd >= 0) break;
        usleep(100000);
    }

    if (fd >= 0) {
        char dev_num_buf[64] = {0};
        read(fd, dev_num_buf, sizeof(dev_num_buf) - 1);
        close(fd);

        int major = 0, minor = 0;
        if (sscanf(dev_num_buf, "%d:%d", &major, &minor) == 2) {
            printf("[UTIM-BOOT] Found block device %s (%d:%d)\n", dev_name, major, minor);
            mknod(root_dev, S_IFBLK | 0660, makedev(major, minor));
        }
    } else {
        printf("[UTIM-BOOT] Warning: sysfs entry %s not found after 5s\n", sys_path);
    }

    mkdir("/sysroot", 0755);
    if (mount(root_dev, "/sysroot", "ext4", MS_RDONLY, NULL) != 0) {
        printf("[UTIM-BOOT] Fatal: Failed to mount %s on /sysroot (errno %d)\n", root_dev, errno);
        printf("[UTIM-BOOT] Halting system.\n");
        while (1) sleep(1);
    }
    printf("[UTIM-BOOT] Successfully mounted root device %s on /sysroot\n", root_dev);

    // Unmount early filesystems so UTIM PID 1 can mount them cleanly with proper flags
    umount("/proc");
    umount("/sys");
    umount("/dev");

    // Switch root into /sysroot
    if (chdir("/sysroot") != 0 || chroot(".") != 0 || chdir("/") != 0) {
        printf("[UTIM-BOOT] Fatal: switch_root/chroot to /sysroot failed (errno %d)\n", errno);
        while (1) sleep(1);
    }

    printf("[UTIM-BOOT] Executing %s (UTIM PID 1)...\n", init_path);
    char *new_argv[] = { init_path, "--system", NULL };
    char *new_envp[] = {
        "PATH=/usr/bin:/bin:/usr/sbin:/sbin",
        "TERM=linux",
        NULL
    };

    execve(init_path, new_argv, new_envp);

    // Fallback: try /usr/bin/utim directly
    execve("/usr/bin/utim", new_argv, new_envp);

    printf("[UTIM-BOOT] Fatal: execve %s failed (errno %d)\n", init_path, errno);
    while (1) sleep(1);
    return 0;
}
EOF

aarch64-linux-gnu-gcc -static -O2 "${RAMDISK_BUILD_DIR}/init.c" -o "${RAMDISK_BUILD_DIR}/init"
rm -f "${RAMDISK_BUILD_DIR}/init.c"

# 3. Create CPIO archive
echo "[*] Packing initramfs CPIO archive..."
(cd "${RAMDISK_BUILD_DIR}" && find . | cpio -H newc -o | gzip -9 > "${OUTPUT_INITRAMFS}")

echo "[+] Successfully created initramfs: ${OUTPUT_INITRAMFS}"
ls -lh "${OUTPUT_INITRAMFS}"
