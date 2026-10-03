use std::fs;
use std::io;
use std::path::Path;

pub fn setup_network_subsystem() -> io::Result<()> {
    // 1. Ensure /etc/resolv.conf has working DNS nameservers
    let resolv_path = Path::new("/etc/resolv.conf");
    let needs_update = match fs::read_to_string(resolv_path) {
        Ok(content) => content.contains("127.0.0.53") || !content.contains("nameserver"),
        Err(_) => true,
    };
    if needs_update {
        let _ = fs::remove_file(resolv_path);
        let _ = fs::write(
            resolv_path,
            "# Configured by UTIM Init Manager\nnameserver 10.0.2.3\nnameserver 8.8.8.8\nnameserver 1.1.1.1\n",
        );
        println!("[UTIM] Configured DNS nameservers in /etc/resolv.conf");
    }

    // 2. Ensure /etc/hosts has localhost and treble-gsi
    let hosts_path = Path::new("/etc/hosts");
    let hosts_needs_update = match fs::read_to_string(hosts_path) {
        Ok(content) => !content.contains("127.0.0.1"),
        Err(_) => true,
    };
    if hosts_needs_update {
        let _ = fs::write(
            hosts_path,
            "127.0.0.1\tlocalhost treble-gsi\n::1\t\tlocalhost ip6-localhost ip6-loopback\n",
        );
    }

    // 3. Ensure network interfaces are UP via libc ioctl
    unsafe {
        let sock = libc::socket(libc::AF_INET, libc::SOCK_DGRAM, 0);
        if sock < 0 {
            return Err(io::Error::last_os_error());
        }

        // Loopback 'lo'
        let mut ifr: libc::ifreq = std::mem::zeroed();
        copy_name(&mut ifr.ifr_name, "lo");
        ifr.ifr_ifru.ifru_flags = (libc::IFF_UP | libc::IFF_RUNNING) as libc::c_short;
        libc::ioctl(sock, libc::SIOCSIFFLAGS, &ifr);

        // Find external interfaces (e.g. eth0) in /sys/class/net
        if let Ok(entries) = fs::read_dir("/sys/class/net") {
            for entry in entries.flatten() {
                let name = entry.file_name().to_string_lossy().to_string();
                if !name.starts_with("eth") && !name.starts_with("en") && !name.starts_with("wlan")
                {
                    continue;
                }
                println!("[UTIM] Configuring primary network interface: {}", name);
                let mut ifr: libc::ifreq = std::mem::zeroed();
                copy_name(&mut ifr.ifr_name, &name);

                // Set IP 10.0.2.15 (network byte order in memory: [10, 0, 2, 15])
                let sin = &mut *(&mut ifr.ifr_ifru.ifru_addr as *mut _ as *mut libc::sockaddr_in);
                sin.sin_family = libc::AF_INET as libc::sa_family_t;
                sin.sin_addr.s_addr = u32::from_ne_bytes([10, 0, 2, 15]);
                libc::ioctl(sock, libc::SIOCSIFADDR, &ifr);

                // Set Netmask 255.255.255.0
                let sin =
                    &mut *(&mut ifr.ifr_ifru.ifru_netmask as *mut _ as *mut libc::sockaddr_in);
                sin.sin_family = libc::AF_INET as libc::sa_family_t;
                sin.sin_addr.s_addr = u32::from_ne_bytes([255, 255, 255, 0]);
                libc::ioctl(sock, libc::SIOCSIFNETMASK, &ifr);

                // Set Broadcast 10.0.2.255
                let sin =
                    &mut *(&mut ifr.ifr_ifru.ifru_broadaddr as *mut _ as *mut libc::sockaddr_in);
                sin.sin_family = libc::AF_INET as libc::sa_family_t;
                sin.sin_addr.s_addr = u32::from_ne_bytes([10, 0, 2, 255]);
                libc::ioctl(sock, libc::SIOCSIFBRDADDR, &ifr);

                // Set Flags UP | RUNNING | BROADCAST | MULTICAST
                ifr.ifr_ifru.ifru_flags =
                    (libc::IFF_UP | libc::IFF_RUNNING | libc::IFF_BROADCAST | libc::IFF_MULTICAST)
                        as libc::c_short;
                libc::ioctl(sock, libc::SIOCSIFFLAGS, &ifr);

                // Add default gateway 10.0.2.2
                let mut rt: libc::rtentry = std::mem::zeroed();
                let dst = &mut *(&mut rt.rt_dst as *mut _ as *mut libc::sockaddr_in);
                dst.sin_family = libc::AF_INET as libc::sa_family_t;

                let genmask = &mut *(&mut rt.rt_genmask as *mut _ as *mut libc::sockaddr_in);
                genmask.sin_family = libc::AF_INET as libc::sa_family_t;

                let gw = &mut *(&mut rt.rt_gateway as *mut _ as *mut libc::sockaddr_in);
                gw.sin_family = libc::AF_INET as libc::sa_family_t;
                gw.sin_addr.s_addr = u32::from_ne_bytes([10, 0, 2, 2]);

                rt.rt_flags = (libc::RTF_UP | libc::RTF_GATEWAY) as libc::c_ushort;
                let c_name = std::ffi::CString::new(name.as_bytes()).unwrap_or_default();
                rt.rt_dev = c_name.as_ptr() as *mut libc::c_char;

                let res = libc::ioctl(sock, libc::SIOCADDRT, &rt);
                if res < 0 {
                    let err = io::Error::last_os_error();
                    if err.raw_os_error() != Some(libc::EEXIST) {
                        eprintln!("[UTIM] Warning: Failed to set default gateway: {}", err);
                    }
                }
            }
        }

        libc::close(sock);
    }

    Ok(())
}

fn copy_name(dest: &mut [libc::c_char], src: &str) {
    let bytes = src.as_bytes();
    let len = bytes.len().min(dest.len() - 1);
    for i in 0..len {
        dest[i] = bytes[i] as libc::c_char;
    }
    dest[len] = 0;
}
