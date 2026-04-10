//! Kernel error detection from `/dev/kmsg`.
//!
//! Reads the kernel ring buffer for suspicious messages indicating hardware
//! or software problems: SATA/ATA errors, NVMe failures, filesystem errors,
//! memory/ECC issues, kernel panics, etc.

use nasty_common::metrics_types::{CategoryCount, KernelError, KernelErrorSummary};
use std::collections::{HashMap, VecDeque};
use std::io::{BufRead, BufReader};

const MAX_RECENT: usize = 50;

/// Pattern definition: a set of substrings to match against, plus the category
/// and a human-readable label for the source extraction.
struct Pattern {
    /// Case-insensitive substrings to match (any match triggers).
    needles: &'static [&'static str],
    /// Category name for grouping.
    category: &'static str,
    /// Prefix to extract source device (e.g. "ata" will extract "ata5" from "ata5: ...").
    source_prefix: Option<&'static str>,
}

const PATTERNS: &[Pattern] = &[
    // SATA / ATA
    Pattern {
        needles: &[
            "failed command:",
            "ATA bus error",
            "hard resetting link",
            "SError:",
            "ata_eh_",
            "irq_stat 0x",
        ],
        category: "sata",
        source_prefix: Some("ata"),
    },
    // NVMe
    Pattern {
        needles: &[
            "nvme nvme",
            "I/O error, dev nvme",
            "controller fatal",
            "failed to set APST",
            "Abort status:",
        ],
        category: "nvme",
        source_prefix: Some("nvme"),
    },
    // Filesystem
    Pattern {
        needles: &[
            "EXT4-fs error",
            "EXT4-fs warning",
            "XFS: Internal error",
            "XFS (", // XFS errors include device in parens
            "BTRFS error",
            "BTRFS warning (device",
            "bcachefs error",
            "bcachefs: error",
        ],
        category: "filesystem",
        source_prefix: None,
    },
    // Memory / ECC / MCE
    Pattern {
        needles: &[
            "EDAC ",
            "mce: [Hardware Error]",
            "Hardware Error",
            "CE memory read error",
            "UE memory read error",
            "Out of memory:",
            "page allocation failure",
        ],
        category: "memory",
        source_prefix: None,
    },
    // Generic kernel problems
    Pattern {
        needles: &[
            "kernel BUG at",
            "BUG: unable to handle",
            "Oops:",
            "Call Trace:",
            "general protection fault",
            "RIP: 0010:",
            "Kernel panic",
        ],
        category: "generic",
        source_prefix: None,
    },
    // Block layer
    Pattern {
        needles: &[
            "I/O error, dev sd",
            "I/O error, dev loop", // skip these in classify
            "blk_update_request: I/O error",
            "Buffer I/O error",
        ],
        category: "block",
        source_prefix: Some("sd"),
    },
    // SCSI
    Pattern {
        needles: &[
            "SCSI error",
            "Medium Error",
            "Unrecovered read error",
            "FAILED Result:",
        ],
        category: "scsi",
        source_prefix: Some("sd"),
    },
];

/// Stateful collector that tracks position in `/dev/kmsg`.
pub struct KernelErrorCollector {
    counts: HashMap<String, u64>,
    recent: VecDeque<KernelError>,
    /// Last sequence number processed (to skip already-seen messages on re-read).
    last_seq: u64,
}

impl KernelErrorCollector {
    pub fn new() -> Self {
        Self {
            counts: HashMap::new(),
            recent: VecDeque::new(),
            last_seq: 0,
        }
    }

    /// Collect kernel errors by parsing `dmesg --raw` output.
    /// Uses dmesg rather than /dev/kmsg directly because /dev/kmsg
    /// requires careful non-blocking fd management across async boundaries.
    pub fn collect(&mut self) -> KernelErrorSummary {
        let output = std::process::Command::new("dmesg")
            .args(["--raw", "--nopager"])
            .output();

        let lines = match output {
            Ok(o) if o.status.success() => {
                let reader = BufReader::new(&o.stdout[..]);
                reader.lines().collect::<Result<Vec<_>, _>>().unwrap_or_default()
            }
            _ => return self.build_summary(),
        };

        for line in &lines {
            let (seq, timestamp, msg) = match parse_raw_line(line) {
                Some(v) => v,
                None => continue,
            };

            // Skip already-processed messages
            if seq <= self.last_seq {
                continue;
            }
            self.last_seq = seq;

            if let Some((category, source)) = classify(msg) {
                *self.counts.entry(category.to_string()).or_insert(0) += 1;

                self.recent.push_back(KernelError {
                    timestamp_usec: timestamp,
                    message: msg.to_string(),
                    category: category.to_string(),
                    source: source.to_string(),
                });
                if self.recent.len() > MAX_RECENT {
                    self.recent.pop_front();
                }
            }
        }

        self.build_summary()
    }

    fn build_summary(&self) -> KernelErrorSummary {
        let total_count: u64 = self.counts.values().sum();
        let by_category: Vec<CategoryCount> = self
            .counts
            .iter()
            .map(|(cat, count)| CategoryCount {
                category: cat.clone(),
                count: *count,
            })
            .collect();

        KernelErrorSummary {
            total_count,
            by_category,
            recent_errors: self.recent.iter().cloned().collect(),
        }
    }
}

/// Parse a raw dmesg line: `<pri>seq,timestamp,...;message`
fn parse_raw_line(line: &str) -> Option<(u64, u64, &str)> {
    // Format: <priority>sequence,timestamp,flags;message
    // Example: <4>12345,6789012345,-;ata5: hard resetting link
    let after_pri = line.find('>')?.checked_add(1)?;
    let rest = line.get(after_pri..)?;

    let semi = rest.find(';')?;
    let header = &rest[..semi];
    let msg = rest.get(semi + 1..)?;

    let mut parts = header.split(',');
    let seq: u64 = parts.next()?.parse().ok()?;
    let timestamp: u64 = parts.next()?.parse().ok()?;

    Some((seq, timestamp, msg))
}

/// Classify a kernel message into a category, returning (category, source).
fn classify(msg: &str) -> Option<(&'static str, String)> {
    // Skip false positives
    if msg.contains("I/O error, dev loop") {
        return None;
    }
    // MODE SENSE spam from iSCSI initiators is harmless
    if msg.contains("MODE SENSE:") {
        return None;
    }

    for pattern in PATTERNS {
        for needle in pattern.needles {
            if msg.contains(needle) {
                let source = pattern
                    .source_prefix
                    .and_then(|pfx| extract_source(msg, pfx))
                    .unwrap_or_else(|| pattern.category.to_string());
                return Some((pattern.category, source));
            }
        }
    }

    None
}

/// Extract a device identifier from the message, e.g. "ata5" from "ata5.00: failed command".
fn extract_source(msg: &str, prefix: &str) -> Option<String> {
    let idx = msg.find(prefix)?;
    let start = idx;
    let rest = &msg[start..];

    // Take prefix + following alphanumeric chars (e.g. "ata5", "nvme0", "sda")
    let end = rest
        .char_indices()
        .skip(prefix.len())
        .find(|(_, c)| !c.is_alphanumeric())
        .map(|(i, _)| i)
        .unwrap_or(rest.len());

    let device = &rest[..end];
    if device.len() > prefix.len() {
        Some(device.to_string())
    } else {
        None
    }
}
