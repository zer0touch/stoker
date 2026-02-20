use anyhow::{bail, Result};
use futures_util::stream::TryStreamExt;
use rtnetlink::{new_connection, Handle};
use std::net::Ipv4Addr;

pub async fn setup_vm_tap(tap_name: &str, host_ip_str: &str) -> Result<()> {
    let host_ip: Ipv4Addr = host_ip_str.parse()?;
    let prefix_len = 30;

    let (connection, handle, _) = new_connection()?;
    tokio::spawn(connection);

    // 1. Create or ensure Tap device exists
    create_or_reset_tap(&handle, tap_name).await?;

    // 2. Set IP Address (e.g., 172.16.X.1/30)
    set_ip_address(&handle, tap_name, host_ip, prefix_len).await?;

    // 3. Set device UP
    set_link_up(&handle, tap_name).await?;

    // 4. Configure iptables MASQUERADE (idempotent for all instances on eth0)
    enable_ip_forwarding()?;
    setup_nat("eth0")?;

    Ok(())
}

pub async fn teardown_vm_tap(tap_name: &str) -> Result<()> {
    let (connection, handle, _) = new_connection()?;
    tokio::spawn(connection);
    
    let mut links = handle.link().get().match_name(tap_name.to_string()).execute();
    if let Ok(Some(link)) = links.try_next().await {
        handle.link().del(link.header.index).execute().await?;
        println!("Deleted TAP interface natively: {}", tap_name);
    } else {
        println!("TAP interface {} not found, skipping...", tap_name);
    }
    
    Ok(())
}

async fn create_or_reset_tap(handle: &Handle, name: &str) -> Result<()> {
    // Delete natively via netlink if it exists
    let mut links = handle.link().get().match_name(name.to_string()).execute();
    if let Ok(Some(link)) = links.try_next().await {
        let _ = handle.link().del(link.header.index).execute().await;
    }
    
    // Create new TAP interface using native raw `ioctl` since rtnetlink natively favors full network managers
    use std::os::unix::io::AsRawFd;
    use std::fs::OpenOptions;

    let file = OpenOptions::new().read(true).write(true).open("/dev/net/tun")
        .map_err(|e| anyhow::anyhow!("Failed to open /dev/net/tun: {}", e))?;
    
    #[repr(C)]
    struct Ifreq {
        ifr_name: [std::os::raw::c_char; 16],
        ifr_flags: std::os::raw::c_short,
    }

    let mut ifr = Ifreq {
        ifr_name: [0; 16],
        // IFF_TAP | IFF_NO_PI
        ifr_flags: (0x0002 | 0x1000) as std::os::raw::c_short,
    };
    
    let bytes = name.as_bytes();
    for i in 0..std::cmp::min(15, bytes.len()) {
        ifr.ifr_name[i] = bytes[i] as std::os::raw::c_char;
    }

    const TUNSETIFF: libc::c_ulong = 1074025674; // _IOW('T', 202, int) on generic linux
    let res = unsafe { libc::ioctl(file.as_raw_fd(), TUNSETIFF, &mut ifr) };
    if res < 0 {
        bail!("Failed to execute TUNSETIFF ioctl to create {}", name);
    }
    
    const TUNSETPERSIST: libc::c_ulong = 1074025675; // _IOW('T', 203, int)
    let res = unsafe { libc::ioctl(file.as_raw_fd(), TUNSETPERSIST, 1) };
    if res < 0 {
        bail!("Failed to execute TUNSETPERSIST ioctl to make {} persistent", name);
    }

    println!("Created and persisted TAP interface natively: {}", name);
    Ok(())
}

async fn set_ip_address(handle: &Handle, name: &str, ip: Ipv4Addr, prefix: u8) -> Result<()> {
    let mut links = handle.link().get().match_name(name.to_string()).execute();
    if let Ok(Some(link)) = links.try_next().await {
        let index = link.header.index;
        handle
            .address()
            .add(index, std::net::IpAddr::V4(ip), prefix)
            .execute()
            .await?;
        println!("Configured IP {}/{} on {}", ip, prefix, name);
    } else {
        bail!("Could not find interface {}", name);
    }
    Ok(())
}

async fn set_link_up(handle: &Handle, name: &str) -> Result<()> {
    let mut links = handle.link().get().match_name(name.to_string()).execute();
    if let Ok(Some(link)) = links.try_next().await {
        let index = link.header.index;
        handle.link().set(index).up().execute().await?;
        println!("Brought interface {} UP", name);
    }
    Ok(())
}

fn enable_ip_forwarding() -> Result<()> {
    std::fs::write("/proc/sys/net/ipv4/ip_forward", b"1")?;
    println!("Enabled IP Forwarding");
    Ok(())
}

fn setup_nat(out_iface: &str) -> Result<()> {
    let ipt = iptables::new(false).map_err(|e| anyhow::anyhow!("Failed to init iptables: {}", e))?;
    
    // Ensure default FORWARD rule is ACCEPT (or add specific rules)
    // We ignore errors on deleting rules that might not exist
    let _ = ipt.delete("nat", "POSTROUTING", &format!("-o {} -j MASQUERADE", out_iface));
    ipt.append("nat", "POSTROUTING", &format!("-o {} -j MASQUERADE", out_iface))
        .map_err(|e| anyhow::anyhow!("Failed to append iptables rule: {}", e))?;
    
    println!("Configured MASQUERADE NAT on {}", out_iface);
    Ok(())
}
