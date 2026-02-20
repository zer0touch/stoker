#!/bin/bash
export DEBIAN_FRONTEND=noninteractive

# Since we are using a hyper-minimal Firecracker ext4 rootfs, we must initialize dpkg state manually 
mkdir -p /var/lib/dpkg/info /var/lib/dpkg/updates /var/lib/dpkg/alternatives /var/lib/apt/lists /var/cache/apt/archives
touch /var/lib/dpkg/status

apt-get update
apt-get install -y nginx

mkdir -p /var/www/html
echo "NGINX MicroVM Build Successful!" > /var/www/html/index.nginx-debian.html
systemctl enable nginx
