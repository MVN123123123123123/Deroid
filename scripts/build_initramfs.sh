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
# Pinned supply-chain digests (B-2). Regenerate with sha256sum if the SDK
# image is intentionally upgraded; never bypass with || true.
ANDROID_RAMDISK_SHA256="5c4a40a87ea671396a683a30fde67a3e51c63a21d332a9c5a18ecf20c53e6185"
ANDROID_VENDOR_SHA256="d5f44441eb7ced018ef09c09e25658bd9218d9633817f67d5b07d8f365876821"

echo "============================================================"
echo " Building Universal Treble Linux Initramfs for Android GKI"
echo " Target Output: ${OUTPUT_INITRAMFS}"
echo "============================================================"

mkdir -p "${WORKSPACE_ROOT}/dist/modules"
rm -rf "${RAMDISK_BUILD_DIR}"
mkdir -p "${RAMDISK_BUILD_DIR}"/{bin,dev,proc,sys,lib/modules,sysroot}

# 1. Extract virtio kernel modules from Android emulator ramdisk
if [[ -f "${ANDROID_RAMDISK}" ]]; then
    echo "[*] Verifying ${ANDROID_RAMDISK} against pinned digest..."
    echo "${ANDROID_RAMDISK_SHA256}  ${ANDROID_RAMDISK}" | sha256sum -c - \
        || { echo "FATAL: ANDROID_RAMDISK digest mismatch" >&2; exit 1; }
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
else
    echo "[!] WARNING: ${ANDROID_RAMDISK} not found; reusing cached modules from dist/modules/ (no new modules extracted)" >&2
fi

# 1b. Extract GPU & Display kernel modules from Android vendor partition
if [[ -f "${ANDROID_VENDOR}" ]]; then
    echo "[*] Verifying ${ANDROID_VENDOR} against pinned digest..."
    echo "${ANDROID_VENDOR_SHA256}  ${ANDROID_VENDOR}" | sha256sum -c - \
        || { echo "FATAL: ANDROID_VENDOR digest mismatch" >&2; exit 1; }
    echo "[*] Extracting GPU, DRM, and Network kernel modules from ${ANDROID_VENDOR}..."
    7z e -y "${ANDROID_VENDOR}" \
        lib/modules/virtio-gpu.ko \
        lib/modules/drm_dma_helper.ko \
        lib/modules/virtio_input.ko \
        lib/modules/failover.ko \
        lib/modules/net_failover.ko \
        lib/modules/virtio_net.ko \
        "-o${RAMDISK_BUILD_DIR}/lib/modules" >/dev/null 2>&1
else
    echo "[!] WARNING: ${ANDROID_VENDOR} not found; reusing cached modules from dist/modules/ (no GPU/net modules extracted)" >&2
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
#include <sys/ioctl.h>
#include <sys/socket.h>
#include <netinet/in.h>
#include <arpa/inet.h>
#include <net/if.h>
#include <linux/route.h>
#include <sys/reboot.h>
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

// Whole-token cmdline test (B-1): substring search would match "ro" inside
// "root=", so tokenize on whitespace and compare exactly.
static int cmdline_has_token(const char *tok) {
    int fd = open("/proc/cmdline", O_RDONLY | O_CLOEXEC);
    if (fd < 0) return 0;
    char buf[1024];
    ssize_t n = read(fd, buf, sizeof(buf) - 1);
    close(fd);
    if (n <= 0) return 0;
    buf[n] = '\0';
    size_t tlen = strlen(tok);
    char *p = buf;
    while (*p) {
        while (*p == ' ' || *p == '\t' || *p == '\n' || *p == '\r') p++;
        if (!*p) break;
        if (strncmp(p, tok, tlen) == 0 &&
            (p[tlen] == ' ' || p[tlen] == '\t' || p[tlen] == '\n' ||
             p[tlen] == '\r' || p[tlen] == '\0'))
            return 1;
        while (*p && *p != ' ' && *p != '\t' && *p != '\n' && *p != '\r') p++;
    }
    return 0;
}

// Fail loudly: reboot the machine instead of parking it in a silent
// while(1) halt nobody can observe (B-14). _exit(70) is the backstop —
// PID 1 exiting panics the kernel, which is still louder than a hang.
static void fatal_reboot(void) {
    sync();
    reboot(RB_AUTOBOOT);
    _exit(70);
}

int main(int argc, char *argv[]) {
    printf("\n============================================================\n");
    printf(" [UTIM-BOOT] Universal Treble Linux Early Initramfs Loader\n");
    printf("============================================================\n");

    if (mount("devtmpfs", "/dev", "devtmpfs", 0, "mode=0755") != 0) {
        mount("tmpfs", "/dev", "tmpfs", 0, "mode=0755");
        mknod("/dev/console", S_IFCHR | 0600, makedev(5, 1));
        mknod("/dev/null", S_IFCHR | 0666, makedev(1, 3));
        mknod("/dev/kmsg", S_IFCHR | 0660, makedev(1, 11));
    }
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

    // Load network drivers for virtio-net
    load_module("/lib/modules/failover.ko");
    load_module("/lib/modules/net_failover.ko");
    load_module("/lib/modules/virtio_net.ko");

    // Configure loopback and eth0 network interfaces early
    int net_sock = socket(AF_INET, SOCK_DGRAM, 0);
    if (net_sock >= 0) {
        struct ifreq ifr;
        memset(&ifr, 0, sizeof(ifr));
        strncpy(ifr.ifr_name, "lo", IFNAMSIZ - 1);
        ifr.ifr_flags = IFF_UP | IFF_RUNNING;
        ioctl(net_sock, SIOCSIFFLAGS, &ifr);

        memset(&ifr, 0, sizeof(ifr));
        strncpy(ifr.ifr_name, "eth0", IFNAMSIZ - 1);
        struct sockaddr_in *sin = (struct sockaddr_in *)&ifr.ifr_addr;
        sin->sin_family = AF_INET;
        sin->sin_addr.s_addr = inet_addr("10.0.2.15");
        ioctl(net_sock, SIOCSIFADDR, &ifr);

        sin = (struct sockaddr_in *)&ifr.ifr_netmask;
        sin->sin_family = AF_INET;
        sin->sin_addr.s_addr = inet_addr("255.255.255.0");
        ioctl(net_sock, SIOCSIFNETMASK, &ifr);

        sin = (struct sockaddr_in *)&ifr.ifr_broadaddr;
        sin->sin_family = AF_INET;
        sin->sin_addr.s_addr = inet_addr("10.0.2.255");
        ioctl(net_sock, SIOCSIFBRDADDR, &ifr);

        ifr.ifr_flags = IFF_UP | IFF_RUNNING | IFF_BROADCAST | IFF_MULTICAST;
        ioctl(net_sock, SIOCSIFFLAGS, &ifr);

        struct rtentry rt;
        memset(&rt, 0, sizeof(rt));
        sin = (struct sockaddr_in *)&rt.rt_dst;
        sin->sin_family = AF_INET;
        sin->sin_addr.s_addr = INADDR_ANY;

        sin = (struct sockaddr_in *)&rt.rt_genmask;
        sin->sin_family = AF_INET;
        sin->sin_addr.s_addr = INADDR_ANY;

        sin = (struct sockaddr_in *)&rt.rt_gateway;
        sin->sin_family = AF_INET;
        sin->sin_addr.s_addr = inet_addr("10.0.2.2");

        rt.rt_flags = RTF_UP | RTF_GATEWAY;
        rt.rt_dev = "eth0";
        ioctl(net_sock, SIOCADDRT, &rt);

        close(net_sock);
        printf("[UTIM-BOOT] Configured network stack: eth0 (10.0.2.15/24) via gateway 10.0.2.2\n");
    }

    // Parse root= and init= from kernel cmdline, and honour rw/ro (B-1):
    // boot writable by default, "ro" forces a read-only mount.
    char root_dev[128] = "/dev/vda";
    char init_path[128] = "/init";
    parse_cmdline_param("root=", root_dev, sizeof(root_dev));
    parse_cmdline_param("init=", init_path, sizeof(init_path));
    int root_rw = 1;
    if (cmdline_has_token("ro")) root_rw = 0;
    if (cmdline_has_token("rw")) root_rw = 1;

    const char *dev_name = strrchr(root_dev, '/');
    dev_name = dev_name ? dev_name + 1 : root_dev;

    // Poll for block device in sysfs (/sys/class/block/<name>/dev or /sys/block/<name>/dev)
    char sys_path[256];
    snprintf(sys_path, sizeof(sys_path), "/sys/class/block/%s/dev", dev_name);

    // Wait for the block device in sysfs, with a hard 1 s deadline (B-14).
    // No full inotify watch: 10 x 100 ms keeps the TCG boot budget intact.
    int fd = -1;
    for (int i = 0; i < 10; i++) {
        fd = open(sys_path, O_RDONLY);
        if (fd < 0) {
            snprintf(sys_path, sizeof(sys_path), "/sys/block/%s/dev", dev_name);
            fd = open(sys_path, O_RDONLY);
        }
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
        printf("[UTIM-BOOT] Warning: sysfs entry %s not found after 1s deadline\n", sys_path);
    }

    mkdir("/sysroot", 0755);
    if (mount(root_dev, "/sysroot", "ext4", root_rw ? 0 : MS_RDONLY, NULL) != 0) {
        printf("[UTIM-BOOT] Fatal: Failed to mount %s on /sysroot (errno %d)\n", root_dev, errno);
        printf("[UTIM-BOOT] Rebooting.\n");
        fatal_reboot();
    }
    printf("[UTIM-BOOT] Successfully mounted root device %s on /sysroot (%s)\n",
           root_dev, root_rw ? "rw" : "ro");

    // Seed DNS/hosts only on a writable mount, and fail loudly (B-1).
    if (root_rw) {
        mkdir("/sysroot/etc", 0755);
        unlink("/sysroot/etc/resolv.conf");
        FILE *f_res = fopen("/sysroot/etc/resolv.conf", "w");
        if (!f_res) {
            printf("[UTIM-BOOT] FATAL: cannot write /sysroot/etc/resolv.conf (errno %d)\n", errno);
            fatal_reboot();
        }
        fprintf(f_res, "# Configured by UTIM Early Bootloader\n");
        fprintf(f_res, "nameserver 10.0.2.3\n");
        fprintf(f_res, "nameserver 8.8.8.8\n");
        fprintf(f_res, "nameserver 1.1.1.1\n");
        fclose(f_res);

        FILE *f_hosts = fopen("/sysroot/etc/hosts", "w");
        if (!f_hosts) {
            printf("[UTIM-BOOT] FATAL: cannot write /sysroot/etc/hosts (errno %d)\n", errno);
            fatal_reboot();
        }
        fprintf(f_hosts, "127.0.0.1\tlocalhost treble-gsi\n");
        fprintf(f_hosts, "::1\t\tlocalhost ip6-localhost ip6-loopback\n");
        fclose(f_hosts);
    } else {
        printf("[UTIM-BOOT] Root mounted read-only; skipping resolv.conf/hosts injection\n");
    }

    // Unmount early filesystems so UTIM PID 1 can mount them cleanly with proper flags
    umount("/proc");
    umount("/sys");
    umount("/dev");

    // Switch root into /sysroot via pivot_root (fallback to chroot)
    mkdir("/sysroot/oldroot", 0755);
    if (syscall(SYS_pivot_root, "/sysroot", "/sysroot/oldroot") == 0) {
        chdir("/");
        umount2("/oldroot", MNT_DETACH);
        rmdir("/oldroot");
    } else if (chdir("/sysroot") != 0 || chroot(".") != 0 || chdir("/") != 0) {
        printf("[UTIM-BOOT] Fatal: switch_root/chroot to /sysroot failed (errno %d)\n", errno);
        fatal_reboot();
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
    fatal_reboot();
    return 0;
}
EOF

aarch64-linux-gnu-gcc -static -Os -ffunction-sections -fdata-sections \
    -fno-asynchronous-unwind-tables -Wl,--gc-sections -Wl,-s \
    "${RAMDISK_BUILD_DIR}/init.c" -o "${RAMDISK_BUILD_DIR}/init"
rm -f "${RAMDISK_BUILD_DIR}/init.c"

# 3. Create CPIO archive (reproducible: fixed mtimes, sorted members)
echo "[*] Packing initramfs CPIO archive..."
: "${SOURCE_DATE_EPOCH:=0}"
find "${RAMDISK_BUILD_DIR}" -exec touch -h -d "@${SOURCE_DATE_EPOCH}" {} +
(cd "${RAMDISK_BUILD_DIR}" && find . -print0 | LC_ALL=C sort -z \
    | cpio --null -H newc -o --reproducible --quiet | gzip -9n > "${OUTPUT_INITRAMFS}")

echo "[+] Successfully created initramfs: ${OUTPUT_INITRAMFS}"
ls -lh "${OUTPUT_INITRAMFS}"
