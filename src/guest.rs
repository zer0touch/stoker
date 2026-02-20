use anyhow::{Context, Result};
use std::process::Command;
use crate::assets;
use crate::firecracker::InstanceMetadata;

pub fn interactive_ssh(name: &str) -> Result<()> {
    // 1. We must find the IP mapping from the state JSON
    let meta_path = format!("/tmp/stoker-{}.json", name);
    if !std::path::Path::new(&meta_path).exists() {
        anyhow::bail!("No running Firecracker VM found with name: {}", name);
    }
    
    let meta_json = std::fs::read_to_string(&meta_path)?;
    let meta: InstanceMetadata = serde_json::from_str(&meta_json)?;
    let guest_ip = meta.guest_ip;

    let key_path = assets::get_asset_path("ubuntu-24.04.id_rsa");

    if !std::path::Path::new(&key_path).exists() {
        anyhow::bail!("SSH Key not found at {}. Is the VM provisioned?", key_path);
    }

    // 2. We use native std::process::Command to take over the TTY natively.
    // SSH2 crate is designed for background programmatic execution without a PTY.
    // For a real `docker exec`-like interactive shell, chaining the native `ssh` binary is the cleanest TTY handoff in Rust.
    
    println!("Connecting to stoker-{m} at {ip}...", m=name, ip=guest_ip);
    
    let mut child = Command::new("ssh")
        .arg("-i")
        .arg(&key_path)
        .arg("-o")
        .arg("StrictHostKeyChecking=no")
        .arg("-o")
        .arg("UserKnownHostsFile=/dev/null")
        .arg("-o")
        .arg("LogLevel=ERROR")
        .arg(format!("root@{}", guest_ip))
        .spawn()
        .context("Failed to spawn interactive SSH session")?;
        
    let status = child.wait()?;
    if !status.success() {
        std::process::exit(status.code().unwrap_or(1));
    }
    Ok(())
}

pub async fn setup_guest_network(guest_ip: &str, host_ip: &str, mode: &str) -> Result<()> {
    println!("Waiting for SSH on {}...", guest_ip);
    
    let tcp = loop {
        match std::net::TcpStream::connect(format!("{}:22", guest_ip)) {
            Ok(stream) => break stream,
            Err(_) => {
                tokio::time::sleep(tokio::time::Duration::from_millis(500)).await;
            }
        }
    };
    
    let mut sess = ssh2::Session::new()?;
    sess.set_tcp_stream(tcp);
    sess.handshake().context("SSH handshake failed")?;

    let key_path = assets::get_asset_path("ubuntu-24.04.id_rsa");
    sess.userauth_pubkey_file("root", None, std::path::Path::new(&key_path), None)
        .context("SSH auth failed")?;

    println!("SSH connected! Applying nested IP routes...");

    let mut channel = sess.channel_session()?;
    
    // Inject dynamic routing idempotently
    let cmds = format!(
        "ip addr replace {}/30 dev eth0 && ip link set eth0 up && ip route replace default via {} && echo 'nameserver 8.8.8.8' > /etc/resolv.conf",
        guest_ip, host_ip
    );
    
    channel.exec(&cmds)?;
    
    let mut s = String::new();
    let mut err = String::new();
    std::io::Read::read_to_string(&mut channel, &mut s)?;
    std::io::Read::read_to_string(&mut channel.stderr(), &mut err)?;
    channel.wait_close()?;
    
    if channel.exit_status()? != 0 {
        anyhow::bail!("Guest IP configuration failed: stdout: {}, stderr: {}", s, err);
    }
    
    println!("Guest network configured via native SSH.");
    Ok(())
}
