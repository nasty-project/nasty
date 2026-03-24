//! Prometheus text exposition format renderer.

use std::fmt::Write;

use nasty_common::metrics_types::*;

use crate::collect_bcachefs::BcachefsMetrics;

/// Render all metrics in Prometheus text exposition format.
pub fn render(stats: &SystemStats, disks: &[DiskHealth], bcachefs: &[BcachefsMetrics]) -> String {
    let mut out = String::with_capacity(16 * 1024);

    render_cpu(&mut out, &stats.cpu);
    render_memory(&mut out, &stats.memory);
    render_network(&mut out, &stats.network);
    render_disk_io(&mut out, &stats.disk_io);
    render_smart(&mut out, disks);

    for fs in bcachefs {
        render_bcachefs_space(&mut out, fs);
        render_bcachefs_counters(&mut out, fs);
        render_bcachefs_time_stats(&mut out, fs);
        render_bcachefs_devices(&mut out, fs);
        render_bcachefs_compression(&mut out, fs);
        render_bcachefs_background(&mut out, fs);
        render_bcachefs_info(&mut out, fs);
    }

    out
}

// ── System metrics ──────────────────────────────────────────────

fn render_cpu(out: &mut String, cpu: &CpuStats) {
    gauge(out, "nasty_cpu_count", "Number of logical CPU cores", &[], cpu.count as f64);
    gauge(out, "nasty_cpu_load", "CPU load average", &[("window", "1m")], cpu.load_1);
    metric_line(out, "nasty_cpu_load", &[("window", "5m")], cpu.load_5);
    metric_line(out, "nasty_cpu_load", &[("window", "15m")], cpu.load_15);
}

fn render_memory(out: &mut String, mem: &MemoryStats) {
    gauge(out, "nasty_memory_total_bytes", "Total installed RAM", &[], mem.total_bytes as f64);
    gauge(out, "nasty_memory_used_bytes", "RAM in use", &[], mem.used_bytes as f64);
    gauge(out, "nasty_memory_available_bytes", "RAM available", &[], mem.available_bytes as f64);
    gauge(out, "nasty_swap_total_bytes", "Total swap space", &[], mem.swap_total_bytes as f64);
    gauge(out, "nasty_swap_used_bytes", "Swap in use", &[], mem.swap_used_bytes as f64);
}

fn render_network(out: &mut String, interfaces: &[NetIfStats]) {
    if interfaces.is_empty() {
        return;
    }
    header(out, "nasty_net_rx_bytes_total", "counter", "Cumulative bytes received");
    for iface in interfaces {
        metric_line(out, "nasty_net_rx_bytes_total", &[("interface", &iface.name)], iface.rx_bytes as f64);
    }
    header(out, "nasty_net_tx_bytes_total", "counter", "Cumulative bytes transmitted");
    for iface in interfaces {
        metric_line(out, "nasty_net_tx_bytes_total", &[("interface", &iface.name)], iface.tx_bytes as f64);
    }
    header(out, "nasty_net_up", "gauge", "Whether interface is up");
    for iface in interfaces {
        metric_line(out, "nasty_net_up", &[("interface", &iface.name)], if iface.up { 1.0 } else { 0.0 });
    }
}

fn render_disk_io(out: &mut String, disks: &[DiskIoStats]) {
    if disks.is_empty() {
        return;
    }
    header(out, "nasty_disk_read_bytes_total", "counter", "Cumulative bytes read");
    for d in disks {
        metric_line(out, "nasty_disk_read_bytes_total", &[("device", &d.name)], d.read_bytes as f64);
    }
    header(out, "nasty_disk_write_bytes_total", "counter", "Cumulative bytes written");
    for d in disks {
        metric_line(out, "nasty_disk_write_bytes_total", &[("device", &d.name)], d.write_bytes as f64);
    }
    header(out, "nasty_disk_io_in_progress", "gauge", "I/O operations in progress");
    for d in disks {
        metric_line(out, "nasty_disk_io_in_progress", &[("device", &d.name)], d.io_in_progress as f64);
    }
}

fn render_smart(out: &mut String, disks: &[DiskHealth]) {
    if disks.is_empty() {
        return;
    }
    header(out, "nasty_disk_smart_healthy", "gauge", "SMART health status (1=passed, 0=failed)");
    for d in disks {
        metric_line(out, "nasty_disk_smart_healthy",
            &[("device", &d.device), ("model", &d.model)],
            if d.health_passed { 1.0 } else { 0.0 });
    }
    header(out, "nasty_disk_temperature_celsius", "gauge", "Drive temperature in Celsius");
    for d in disks {
        if let Some(temp) = d.temperature_c {
            metric_line(out, "nasty_disk_temperature_celsius",
                &[("device", &d.device)], temp as f64);
        }
    }
    header(out, "nasty_disk_power_on_hours", "gauge", "Accumulated power-on hours");
    for d in disks {
        if let Some(hours) = d.power_on_hours {
            metric_line(out, "nasty_disk_power_on_hours",
                &[("device", &d.device)], hours as f64);
        }
    }
}

// ── bcachefs metrics ────────────────────────────────────────────

fn render_bcachefs_space(out: &mut String, fs: &BcachefsMetrics) {
    let labels = [("filesystem", fs.fs_name.as_str()), ("uuid", fs.uuid.as_str())];
    gauge(out, "nasty_bcachefs_fs_total_bytes", "Total pool capacity", &labels, fs.space.total_bytes as f64);
    gauge(out, "nasty_bcachefs_fs_used_bytes", "Filesystem bytes in use", &labels, fs.space.used_bytes as f64);
    gauge(out, "nasty_bcachefs_fs_available_bytes", "Filesystem bytes available", &labels, fs.space.available_bytes as f64);
}

fn render_bcachefs_counters(out: &mut String, fs: &BcachefsMetrics) {
    if fs.counters.is_empty() {
        return;
    }
    header(out, "nasty_bcachefs_counter", "counter", "bcachefs persistent counter (since mount)");
    let mut sorted: Vec<_> = fs.counters.iter().collect();
    sorted.sort_by_key(|(k, _)| k.as_str());
    for (name, value) in sorted {
        metric_line(out, "nasty_bcachefs_counter",
            &[("filesystem", fs.fs_name.as_str()), ("uuid", fs.uuid.as_str()), ("counter", name)],
            *value as f64);
    }
}

fn render_bcachefs_time_stats(out: &mut String, fs: &BcachefsMetrics) {
    if fs.time_stats.is_empty() {
        return;
    }

    for suffix in &["mean_ns", "min_ns", "max_ns", "stddev_ns", "count"] {
        let metric_name = format!("nasty_bcachefs_time_stat_{suffix}");
        header(out, &metric_name, "gauge", &format!("bcachefs time stat {suffix}"));
        let mut sorted: Vec<_> = fs.time_stats.iter().collect();
        sorted.sort_by_key(|(k, _)| k.as_str());
        for (name, stat) in &sorted {
            let val = match *suffix {
                "mean_ns" => stat.mean_ns as f64,
                "min_ns" => stat.min_ns as f64,
                "max_ns" => stat.max_ns as f64,
                "stddev_ns" => stat.stddev_ns as f64,
                "count" => stat.count as f64,
                _ => 0.0,
            };
            metric_line(out, &metric_name,
                &[("filesystem", fs.fs_name.as_str()), ("uuid", fs.uuid.as_str()), ("op", name)],
                val);
        }
    }
}

fn render_bcachefs_devices(out: &mut String, fs: &BcachefsMetrics) {
    if fs.devices.is_empty() {
        return;
    }
    header(out, "nasty_bcachefs_device_io_latency_ns", "gauge", "Current device I/O latency in nanoseconds");
    for dev in &fs.devices {
        let label_str = dev.label.as_deref().unwrap_or("");
        metric_line(out, "nasty_bcachefs_device_io_latency_ns",
            &[("filesystem", fs.fs_name.as_str()), ("device", &dev.name), ("label", label_str), ("direction", "read")],
            dev.io_latency_read_ns as f64);
        metric_line(out, "nasty_bcachefs_device_io_latency_ns",
            &[("filesystem", fs.fs_name.as_str()), ("device", &dev.name), ("label", label_str), ("direction", "write")],
            dev.io_latency_write_ns as f64);
    }
}

fn render_bcachefs_compression(out: &mut String, fs: &BcachefsMetrics) {
    if fs.compression.is_empty() {
        return;
    }
    header(out, "nasty_bcachefs_compressed_bytes", "gauge", "Compressed data size on disk");
    for c in &fs.compression {
        metric_line(out, "nasty_bcachefs_compressed_bytes",
            &[("filesystem", fs.fs_name.as_str()), ("algorithm", &c.algorithm)],
            c.compressed_bytes as f64);
    }
    header(out, "nasty_bcachefs_uncompressed_bytes", "gauge", "Uncompressed (logical) data size");
    for c in &fs.compression {
        metric_line(out, "nasty_bcachefs_uncompressed_bytes",
            &[("filesystem", fs.fs_name.as_str()), ("algorithm", &c.algorithm)],
            c.uncompressed_bytes as f64);
    }
}

fn render_bcachefs_background(out: &mut String, fs: &BcachefsMetrics) {
    let labels = [("filesystem", fs.fs_name.as_str()), ("uuid", fs.uuid.as_str())];
    if let Some(bytes) = fs.background.btree_cache_size_bytes {
        gauge(out, "nasty_bcachefs_btree_cache_size_bytes", "Btree cache memory usage", &labels, bytes as f64);
    }
}

fn render_bcachefs_info(out: &mut String, fs: &BcachefsMetrics) {
    // Info metric: gauge=1 with labels carrying configuration values.
    let compression = fs.options.get("compression").map(|s| s.as_str()).unwrap_or("none");
    let data_replicas = fs.options.get("data_replicas").map(|s| s.as_str()).unwrap_or("1");
    let metadata_replicas = fs.options.get("metadata_replicas").map(|s| s.as_str()).unwrap_or("1");
    let data_checksum = fs.options.get("data_checksum").map(|s| s.as_str()).unwrap_or("crc32c");
    let encrypted = fs.options.get("encrypted").map(|s| s.as_str()).unwrap_or("0");

    header(out, "nasty_bcachefs_fs_info", "gauge", "bcachefs pool configuration (labels carry values)");
    metric_line(out, "nasty_bcachefs_fs_info",
        &[("filesystem", fs.fs_name.as_str()), ("uuid", fs.uuid.as_str()),
          ("compression", compression), ("data_replicas", data_replicas),
          ("metadata_replicas", metadata_replicas), ("data_checksum", data_checksum),
          ("encrypted", encrypted)],
        1.0);
}

// ── Formatting helpers ──────────────────────────────────────────

fn header(out: &mut String, name: &str, metric_type: &str, help: &str) {
    let _ = writeln!(out, "# HELP {name} {help}");
    let _ = writeln!(out, "# TYPE {name} {metric_type}");
}

fn gauge(out: &mut String, name: &str, help: &str, labels: &[(&str, &str)], value: f64) {
    header(out, name, "gauge", help);
    metric_line(out, name, labels, value);
}

fn metric_line(out: &mut String, name: &str, labels: &[(&str, &str)], value: f64) {
    if labels.is_empty() {
        let _ = writeln!(out, "{name} {value}");
    } else {
        let _ = write!(out, "{name}{{");
        for (i, (k, v)) in labels.iter().enumerate() {
            if i > 0 {
                let _ = write!(out, ",");
            }
            let _ = write!(out, "{k}=\"{v}\"");
        }
        let _ = writeln!(out, "}} {value}");
    }
}
