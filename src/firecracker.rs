use anyhow::{Context, Result};
use hyper::{Body, Client, Request, Method};
use hyperlocal::{UnixClientExt, Uri};
use serde_json::json;
use std::process::{Command, Stdio};
use std::time::Duration;
use tokio::time::sleep;
use crate::guest;
use serde::{Serialize, Deserialize};

#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct InstanceMetadata {
    pub id: u8,
    pub name: String,
    pub mode: String,
    pub guest_ip: String,
    pub host_ip: String,
    pub mac_address: String,
    pub tap_device: String,
    pub pid: u32,
}

// We will launch the firecracker binary via Command, wait for the socket, and send REST commands.
pub async fn run_vm(mode: &str, name_opt: Option<String>, image_opt: Option<String>) -> Result<()> {
    // 1. Allocate ID and Networking Parameters
    let id = allocate_vm_id()?;
    let name = name_opt.unwrap_or_else(|| format!("fc-{:02x}", id));
    let base_image = image_opt.unwrap_or_else(|| "ubuntu-rootfs".to_string());
    
    let host_ip = format!("172.16.{}.1", id);
    let guest_ip = format!("172.16.{}.2", id);
    let mac_address = format!("06:00:AC:10:{:02x}:02", id);
    let tap_device = format!("tap-inet-{}", id);
    
    // 2. Setup isolated TAP interface dynamically per VM
    crate::network::setup_vm_tap(&tap_device, &host_ip).await?;
    let socket_path = format!("/tmp/firecracker-{}.socket", name);
    let log_path = format!("/tmp/firecracker-{}.log", name);
    
    // Ensure the log file exists as required by Firecracker
    let _ = std::fs::File::create(&log_path);
    
    // Clean up old socket if it exists
    let _ = std::fs::remove_file(&socket_path);

    // Launch Firecracker daemon in background
    println!("Starting Firecracker daemon...");
    let fc_binary = crate::assets::get_asset_path("firecracker");
    let mut child = Command::new(&fc_binary)
        .arg("--api-sock")
        .arg(&socket_path)
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .context("Failed to spawn firecracker daemon")?;

    // Give it a moment to create the socket
    sleep(Duration::from_millis(500)).await;

    // Use hyperlocal for unix socket client
    let client = Client::unix();

    // 1. Logger
    println!("Configuring VM Logger...");
    let logger_payload = json!({
        "log_path": log_path,
        "level": "Debug",
        "show_level": true,
        "show_log_origin": true
    }).to_string();
    send_request(&client, &socket_path, "/logger", logger_payload).await?;

    // 2. Boot Source
    println!("Configuring Boot Source...");
    let boot_payload = json!({
        "kernel_image_path": crate::assets::get_asset_path("vmlinux.bin"),
        "boot_args": "console=ttyS0 reboot=k panic=1 pci=off keep_bootcon"
    }).to_string();
    send_request(&client, &socket_path, "/boot-source", boot_payload).await?;

    // 3. Drives
    println!("Configuring Drives...");
    let rootfs_dest = format!("/tmp/rootfs-{}.ext4", name);
    // Find either custom image or default to the baseline
    let target_image_path = crate::assets::get_asset_path(&format!("{}.ext4", base_image));
    if !std::path::Path::new(&target_image_path).exists() {
        anyhow::bail!("Rootfs image not found at {}. Run `stoker build` or `stoker download-assets`.", target_image_path);
    }
    
    std::fs::copy(&target_image_path, &rootfs_dest)?;
    
    let drive_payload = json!({
        "drive_id": "rootfs",
        "path_on_host": rootfs_dest,
        "is_root_device": true,
        "is_read_only": false
    }).to_string();
    send_request(&client, &socket_path, "/drives/rootfs", drive_payload).await?;

    // 4. Network Interfaces
    println!("Configuring Network Interface...");
    let net_payload = json!({
        "iface_id": "net1",
        "guest_mac": mac_address,
        "host_dev_name": tap_device
    }).to_string();
    send_request(&client, &socket_path, "/network-interfaces/net1", net_payload).await?;

    // 5. Start Instance
    println!("Sending InstanceStart action...");
    let action_payload = json!({
        "action_type": "InstanceStart"
    }).to_string();
    send_request(&client, &socket_path, "/actions", action_payload).await?;

    println!("MicroVM Booted successfully via Unix API.");
    
    // 6. Connect via Guest module
    guest::setup_guest_network(&guest_ip, &host_ip, mode).await?;
    
    // Save state metadata implementation_plan style
    let meta = InstanceMetadata {
        id,
        name: name.clone(),
        mode: mode.to_string(),
        guest_ip,
        host_ip,
        mac_address,
        tap_device,
        pid: child.id(),
    };
    
    let meta_json = serde_json::to_string(&meta)?;
    std::fs::write(format!("/tmp/stoker-{}.json", name), meta_json)?;

    println!("VM is running in background. PID: {}", child.id());
    Ok(())
}

fn allocate_vm_id() -> Result<u8> {
    let mut used_ids = std::collections::HashSet::new();
    if let Ok(entries) = std::fs::read_dir("/tmp") {
        for entry in entries.flatten() {
            let fname = entry.file_name().to_string_lossy().to_string();
            if fname.starts_with("stoker-") && fname.ends_with(".json") {
                if let Ok(content) = std::fs::read_to_string(entry.path()) {
                    if let Ok(meta) = serde_json::from_str::<InstanceMetadata>(&content) {
                        used_ids.insert(meta.id);
                    }
                }
            }
        }
    }
    
    for id in 0..=254 {
        if !used_ids.contains(&id) {
            return Ok(id);
        }
    }
    anyhow::bail!("No available VM IDs");
}

pub async fn rm_vm(name: &str) -> Result<()> {
    let meta_path = format!("/tmp/stoker-{}.json", name);
    if !std::path::Path::new(&meta_path).exists() {
        anyhow::bail!("No running Firecracker VM found with name '{}'", name);
    }
    
    let meta_json = std::fs::read_to_string(&meta_path)?;
    let meta: InstanceMetadata = serde_json::from_str(&meta_json)?;
    
    // 1. Kill the Firecracker Hypervisor Native PID
    unsafe {
        if libc::kill(meta.pid as i32, libc::SIGKILL) == 0 {
            println!("Terminated Firecracker daemon (PID: {})", meta.pid);
        } else {
            println!("Warning: Could not kill PID {} (it may have already exited)", meta.pid);
        }
    }
    
    // 2. Teardown Network Interfaces
    crate::network::teardown_vm_tap(&meta.tap_device).await?;
    
    // 3. Remove /tmp state footprints to cleanly release IDs
    let _ = std::fs::remove_file(&meta_path);
    let _ = std::fs::remove_file(format!("/tmp/firecracker-{}.socket", name));
    let _ = std::fs::remove_file(format!("/tmp/firecracker-{}.log", name));
    let _ = std::fs::remove_file(format!("/tmp/rootfs-{}.ext4", name));
    
    println!("Cleaned up all resources for stoker-{}", name);
    Ok(())
}

async fn send_request(client: &Client<hyperlocal::UnixConnector>, socket: &str, path: &str, body: String) -> Result<()> {
    let url = Uri::new(socket, path);
    let req = Request::builder()
        .method(Method::PUT)
        .uri(url)
        .header("Accept", "application/json")
        .header("Content-Type", "application/json")
        .body(Body::from(body))?;

    let resp = client.request(req).await?;
    if !resp.status().is_success() {
        let status = resp.status();
        let bytes = hyper::body::to_bytes(resp.into_body()).await?;
        anyhow::bail!("API Request failed: {} - {:?}", status, bytes);
    }
    Ok(())
}

pub fn list_vms() -> Result<()> {
    println!("{:<20} {:<15} {:<15} {:<20} {:<15}", "CONTAINER ID", "IMAGE", "STATUS", "NAMES", "IP");
    
    // Natively scan /tmp for stoker metadata jsons
    if let Ok(entries) = std::fs::read_dir("/tmp") {
        for entry in entries.flatten() {
            let fname = entry.file_name().to_string_lossy().to_string();
            if fname.starts_with("stoker-") && fname.ends_with(".json") {
                if let Ok(content) = std::fs::read_to_string(entry.path()) {
                    if let Ok(meta) = serde_json::from_str::<InstanceMetadata>(&content) {
                        let id_str = format!("fc_{:02x}", meta.id);
                        println!("{:<20} {:<15} {:<15} {:<20} {:<15}", 
                            id_str, 
                            "ubuntu:24.04", 
                            "Up", 
                            meta.name,
                            meta.guest_ip
                        );
                    }
                }
            }
        }
    }
    
    Ok(())
}
