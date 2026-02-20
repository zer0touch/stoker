use clap::{Parser, Subcommand};
use anyhow::Result;

#[cfg(target_os = "linux")]
mod network;
#[cfg(target_os = "linux")]
mod firecracker;
#[cfg(target_os = "linux")]
mod guest;
#[cfg(target_os = "linux")]
mod assets;
#[cfg(target_os = "linux")]
mod builder;

#[derive(Parser, Debug)]
#[command(name = "stoker")]
#[command(about = "A docker-like CLI for managing Firecracker microVMs natively in Rust", long_about = None)]
struct Cli {
    #[command(subcommand)]
    command: Commands,
}

#[derive(Subcommand, Debug)]
enum Commands {
    /// Downloads necessary kernel, rootfs, and ssh keys
    DownloadAssets,
    /// Starts a microVM instance
    Run {
        /// Mode of network (internet or local)
        #[arg(long, default_value = "internet")]
        mode: String,
        /// Optional custom name for the VM
        #[arg(long)]
        name: Option<String>,
        /// Target image name to boot (default: ubuntu-rootfs)
        #[arg(long)]
        image: Option<String>,
    },
    /// Builds a custom microVM filesystem image using a bash script
    Build {
        /// Name of the new resulting image
        #[arg(long)]
        image_name: String,
        /// Path to the bash script to execute inside the build container
        #[arg(long)]
        script_path: String,
    },
    /// Connects interactively to an active microVM
    Ssh {
        /// Custom name or ID of the VM to connect to
        name: String,
    },
    /// Removes a microVM and releases its IP subnet
    Rm {
        /// Name of the VM to remove
        name: String,
    },
    /// Lists active microVMs
    List,
    /// Lists available microVM images
    Images,
    /// Provisions the Lima virtual machine environment end-to-end from macOS
    Setup,
}

#[tokio::main]
async fn main() -> Result<()> {
    let cli = Cli::parse();

    #[cfg(target_os = "macos")]
    {
        use std::process::Command;
        use std::env;

        let args: Vec<String> = env::args().collect();
        let inner_args = if args.len() > 1 {
            args[1..].join(" ")
        } else {
            String::new()
        };
        
        if inner_args.is_empty() {
            println!("Please provide a command. e.g. stoker run --mode internet");
            return Ok(());
        }

        if args.get(1).map(|s| s.as_str()) == Some("setup") {
            return macos_setup().await;
        }

        let cmd_str = format!("sudo stoker {}", inner_args);
        
        // Hide the limactl complexity if it's the `ssh` or `list` command
        if args.get(1).map(|s| s.as_str()) == Some("ssh") || args.get(1).map(|s| s.as_str()) == Some("list") || args.get(1).map(|s| s.as_str()) == Some("images") {
            // Be entirely seamless to the user
        } else {
            println!("Proxying to Lima VM: limactl shell firecracker-vm bash -l -c '{}'", cmd_str);
        }
        
        let mut child = Command::new("limactl")
            .args(&["shell", "firecracker-vm", "bash", "-l", "-c", &cmd_str])
            .spawn()?;
        
        let status = child.wait()?;
        if !status.success() {
            std::process::exit(status.code().unwrap_or(1));
        }
        return Ok(());
    }

    #[cfg(target_os = "linux")]
    {
        match cli.command {
            Commands::DownloadAssets => {
                println!("Downloading Firecracker assets natively...");
                assets::download_all().await?;
                println!("Assets downloaded successfully.");
            }
            Commands::Run { mode, name, image } => {
                println!("Starting stoker {} VM...", mode);
                firecracker::run_vm(&mode, name, image).await?;
            }
            Commands::Build { image_name, script_path } => {
                builder::build_image(&image_name, &script_path)?;
            }
            Commands::Ssh { name } => {
                guest::interactive_ssh(&name)?;
            }
            Commands::Rm { name } => {
                println!("Removing VM '{}'...", name);
                firecracker::rm_vm(&name).await?;
                println!("VM '{}' successfully removed.", name);
            }
            Commands::List => {
                firecracker::list_vms()?;
            }
            Commands::Images => {
                assets::list_images()?;
            }
            Commands::Setup => {
                // Setup is exclusively a macOS proxy command to build the Lima VM.
                println!("The `setup` command is only available on macOS to build the host VM.");
            }
        }
    }

    #[cfg(not(any(target_os = "macos", target_os = "linux")))]
    {
        println!("Unsupported OS!");
    }

    Ok(())
}

#[cfg(target_os = "macos")]
async fn macos_setup() -> Result<()> {
    use std::io::Write;
    use std::process::Command;
    
    println!("Setting up Lima VM for Firecracker...");
    
    let yaml = r#"
images:
- location: "https://cloud-images.ubuntu.com/releases/24.04/release/ubuntu-24.04-server-cloudimg-amd64.img"
  arch: "x86_64"
- location: "https://cloud-images.ubuntu.com/releases/24.04/release/ubuntu-24.04-server-cloudimg-arm64.img"
  arch: "aarch64"
cpus: 4
memory: "8GiB"
vmType: "vz"
nestedVirtualization: true
mounts:
- location: "~"
  writable: false
containerd:
  system: false
  user: false
provision:
- mode: system
  script: |
    #!/bin/bash
    export DEBIAN_FRONTEND=noninteractive
    apt-get update
    apt-get install -y iptables build-essential curl pkg-config libssl-dev
"#;

    let yaml_path = "/tmp/firecracker-vm.yaml";
    let mut file = std::fs::File::create(yaml_path)?;
    file.write_all(yaml.as_bytes())?;
    
    println!("Creating Lima VM (this may take a few minutes)...");
    let mut child = Command::new("limactl")
        .args(&["start", "--name=firecracker-vm", "--tty=false", yaml_path])
        .spawn()?;
        
    let status = child.wait()?;
    if !status.success() {
        anyhow::bail!("Failed to create Lima VM");
    }
    
    println!("Lima VM created successfully.");
    
    let current_dir = std::env::current_dir()?.to_string_lossy().to_string();
    
    println!("Compiling stoker inside Lima VM...");
    let compile_cmd = format!("curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh -s -- -y && source $HOME/.cargo/env && mkdir -p ~/stoker && cp -r {}/* ~/stoker/ && cd ~/stoker && cargo build --release && sudo cp target/release/stoker /usr/local/bin/stoker", current_dir);
    
    let mut child2 = Command::new("limactl")
        .args(&["shell", "firecracker-vm", "bash", "-l", "-c", &compile_cmd])
        .spawn()?;
        
    let status2 = child2.wait()?;
    if !status2.success() {
        anyhow::bail!("Failed to compile stoker inside Lima VM");
    }
    
    println!("stoker setup complete! You can now run `stoker download-assets`.");
    
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::Parser;

    #[test]
    fn test_cli_run_defaults() {
        let args = vec!["stoker", "run"];
        let cli = Cli::try_parse_from(args).unwrap();
        match cli.command {
            Commands::Run { mode, name, image } => {
                assert_eq!(mode, "internet");
                assert_eq!(name, None);
                assert_eq!(image, None);
            }
            _ => panic!("Expected Run command"),
        }
    }

    #[test]
    fn test_cli_run_custom() {
        let args = vec!["stoker", "run", "--name", "my-server", "--image", "nginx-image", "--mode", "local"];
        let cli = Cli::try_parse_from(args).unwrap();
        match cli.command {
            Commands::Run { mode, name, image } => {
                assert_eq!(mode, "local");
                assert_eq!(name, Some("my-server".to_string()));
                assert_eq!(image, Some("nginx-image".to_string()));
            }
            _ => panic!("Expected Run command"),
        }
    }

    #[test]
    fn test_cli_build() {
        let args = vec!["stoker", "build", "--image-name", "custom-build", "--script-path", "/path/to/script.sh"];
        let cli = Cli::try_parse_from(args).unwrap();
        match cli.command {
            Commands::Build { image_name, script_path } => {
                assert_eq!(image_name, "custom-build");
                assert_eq!(script_path, "/path/to/script.sh");
            }
            _ => panic!("Expected Build command"),
        }
    }

    #[test]
    fn test_cli_ssh() {
        let args = vec!["stoker", "ssh", "my-server"];
        let cli = Cli::try_parse_from(args).unwrap();
        match cli.command {
            Commands::Ssh { name } => {
                assert_eq!(name, "my-server");
            }
            _ => panic!("Expected Ssh command"),
        }
    }
}
