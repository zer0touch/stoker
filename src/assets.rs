use anyhow::{Context, Result};
use futures_util::StreamExt;
use reqwest::Client;
use std::fs::{self, File};
use std::io::Write;
use std::path::Path;

const ASSET_DIR: &str = "/home/reprah007.linux/firecracker-assets";
const KERNEL_URL: &str = "https://s3.amazonaws.com/spec.ccfc.min/firecracker-ci/v1.13/aarch64/vmlinux-5.10.239";
const ROOTFS_URL: &str = "https://s3.amazonaws.com/spec.ccfc.min/img/aarch64/ubuntu_with_ssh/fsfiles/xenial.rootfs.ext4";
const SSH_KEY_URL: &str = "https://s3.amazonaws.com/spec.ccfc.min/img/aarch64/ubuntu_with_ssh/fsfiles/xenial.rootfs.id_rsa";
const FIRECRACKER_URL: &str = "https://github.com/firecracker-microvm/firecracker/releases/download/v1.10.1/firecracker-v1.10.1-aarch64.tgz";

// Note: To remain purely native without shelling to `mount`, we assume the user provides an SSH-enabled rootfs.
// We will download the generic one and rely on standard credentials or pre-baked images.
// For the scope of this CLI, we will download the assets to a shared directory.

pub async fn download_all() -> Result<()> {
    fs::create_dir_all(ASSET_DIR).context("Failed to create assets directory")?;

    let client = Client::new();

    download_file(&client, KERNEL_URL, &get_asset_path("vmlinux.bin")).await?;
    download_file(&client, ROOTFS_URL, &get_asset_path("ubuntu-rootfs.ext4")).await?;
    download_file(&client, FIRECRACKER_URL, &get_asset_path("firecracker-aarch64.tgz")).await?;

    let fc_binary = get_asset_path("firecracker");
    if !Path::new(&fc_binary).exists() {
        println!("Extracting Firecracker binary...");
        let status = std::process::Command::new("tar")
            .arg("-xzf")
            .arg(get_asset_path("firecracker-aarch64.tgz"))
            .arg("-C")
            .arg(ASSET_DIR)
            .status()
            .context("Failed to extract firecracker")?;
            
        if status.success() {
            std::fs::rename(
                format!("{}/release-v1.10.1-aarch64/firecracker-v1.10.1-aarch64", ASSET_DIR),
                &fc_binary
            )?;
            let _ = std::fs::remove_dir_all(format!("{}/release-v1.10.1-aarch64", ASSET_DIR));
        } else {
            anyhow::bail!("Failed to extract firecracker binary");
        }
    }
    
    let key_path = get_asset_path("ubuntu-24.04.id_rsa");
    if !Path::new(&key_path).exists() {
        println!("Downloading SSH key...");
        download_file(&client, SSH_KEY_URL, &key_path).await?;
        
        // Set permissions to 400 (read-only for owner)
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mut perms = std::fs::metadata(&key_path)?.permissions();
            perms.set_mode(0o400);
            std::fs::set_permissions(&key_path, perms)?;
        }
    }
    
    Ok(())
}

async fn download_file(client: &Client, url: &str, dest: &str) -> Result<()> {
    if Path::new(dest).exists() {
        println!("File {} already exists. Skipping.", dest);
        return Ok(());
    }

    println!("Downloading {}...", url);
    let mut response = client.get(url).send().await?.error_for_status()?;

    let mut file = File::create(dest)?;
    while let Some(chunk) = response.chunk().await? {
        file.write_all(&chunk)?;
    }

    println!("Saved to {}", dest);
    Ok(())
}

// Ensure the asset path exists and returns it
pub fn get_asset_path(filename: &str) -> String {
    format!("{}/{}", ASSET_DIR, filename)
}

pub fn list_images() -> Result<()> {
    println!("{:<30} {:<15}", "IMAGE", "SIZE");
    if let Ok(entries) = fs::read_dir(ASSET_DIR) {
        for entry in entries.flatten() {
            let fname = entry.file_name().to_string_lossy().to_string();
            if fname.ends_with(".ext4") {
                let name = fname.trim_end_matches(".ext4");
                let size_str = if let Ok(meta) = std::fs::metadata(entry.path()) {
                    format!("{:.2} MB", meta.len() as f64 / 1_048_576.0)
                } else {
                    "Unknown".to_string()
                };
                println!("{:<30} {:<15}", name, size_str);
            }
        }
    }
    Ok(())
}
