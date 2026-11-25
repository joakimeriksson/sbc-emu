# Sourced under L4T's kernel with Debian's root (init=/bin/sh): file systems, NVIDIA's GPU driver
# (nvgpu), the library path of NVIDIA's userspace.
mount -t proc proc /proc; mount -t sysfs sys /sys; mount -t devtmpfs dev /dev 2>/dev/null; mount -t tmpfs tmp /tmp
insmod /usr/lib/modules/4.9.299-tegra/kernel/drivers/gpu/nvgpu/nvgpu.ko; sleep 2
# the optional CDE firmware is not shipped: do not wait a minute for a helper that never comes
echo 1 > /sys/class/firmware/timeout
# an interpreted kernel can take longer than nvgpu's 5-second channel watchdog
mount -t debugfs none /sys/kernel/debug && echo 0 > /sys/kernel/debug/gpu.0/timeouts_enabled
export LD_LIBRARY_PATH=/opt/l4t/usr/lib/aarch64-linux-gnu/tegra:/opt/l4t/usr/lib/aarch64-linux-gnu:/opt/l4t/usr/local/cuda-10.2/targets/aarch64-linux/lib
cd /tmp
