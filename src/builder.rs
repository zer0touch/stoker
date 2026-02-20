use anyhow::{Context, Result};
use std::process::Command;
use crate::assets;

pub fn build_image(image_name: &str, script_path: &str) -> Result<()> {
    println!("Building Firecracker image: {}...", image_name);
    
    let base_ext4 = assets::get_asset_path("ubuntu-rootfs.ext4");
    if !std::path::Path::new(&base_ext4).exists() {
        anyhow::bail!("Base rootfs not found at {}. Run `stoker download-assets` first.", base_ext4);
    }
    
    let target_ext4 = assets::get_asset_path(&format!("{}.ext4", image_name));
    
    // 1. Clone the ext4 base to the new target
    println!("Cloning base rootfs to {}...", target_ext4);
    std::fs::copy(&base_ext4, &target_ext4).context("Failed to copy base image")?;
    
    // 2. Expand the image by 2GB to ensure enough space for the build script
    println!("Expanding image size by +2G for build space...");
    let _ = Command::new("truncate").args(&["-s", "+2G", &target_ext4]).status();
    let _ = Command::new("e2fsck").args(&["-f", "-y", &target_ext4]).status();
    let _ = Command::new("resize2fs").args(&[&target_ext4]).status();
    
    // 3. Mount the ext4 loop device natively via system commands (most stable for nested VM overlays)
    let mount_dir = format!("/tmp/stoker-build-{}", image_name);
    let _ = std::fs::create_dir_all(&mount_dir);
    
    println!("Mounting loop filesystem at {}...", mount_dir);
    let status = Command::new("mount")
        .args(&["-o", "loop", &target_ext4, &mount_dir])
        .status()?;
        
    if !status.success() {
        anyhow::bail!("Failed to loop mount the ext4 file. Are you running as root?");
    }
    
    // Ensure we unmount cleanly even if the build fails
    let result = execute_chroot_build(&mount_dir, script_path);
    
    // 3. Unmount
    println!("Unmounting loop filesystem...");
    let _ = Command::new("umount").arg(&mount_dir).status();
    let _ = std::fs::remove_dir_all(&mount_dir);
    
    result?;
    println!("Successfully built stoker image: {}", image_name);
    Ok(())
}

fn execute_chroot_build(mount_dir: &str, script_path: &str) -> Result<()> {
    // Read the script into memory
    let script_content = std::fs::read_to_string(script_path)
        .context(format!("Could not read build script: {}", script_path))?;
        
    // Write it directly into the chroot's root (systemd-nspawn mounts a tmpfs over /tmp so we use /)
    let guest_script_path = format!("{}/stoker-build.sh", mount_dir);
    std::fs::write(&guest_script_path, script_content)?;
    
    // Set executable
    let _ = Command::new("chmod").args(&["+x", &guest_script_path]).status();
    
    println!("Executing build script inside systemd-nspawn container...");
    
    // Use systemd-nspawn instead of raw chroot because it automatically mounts /dev, /proc, /sys correctly for networking and apt-get isolation
    let status = Command::new("systemd-nspawn")
        .args(&["-D", mount_dir, "--as-pid2", "/stoker-build.sh"])
        .status()
        .context("Failed to execute systemd-nspawn. Is it installed inside the VM?")?;
        
    if !status.success() {
        anyhow::bail!("Build script failed inside the container.");
    }
    
    Ok(())
}
