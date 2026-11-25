//! Address watch: tells subscribers when local IP addresses appear.
//!
//! Anything bound to one specific address (an NVMe-oF port, an iSCSI portal)
//! cannot bind until that address exists. At boot that is routine: Tailscale,
//! DHCP, VLANs and bonds come up after the engine restores its targets. With
//! this watch, those binds happen when the address shows up instead of
//! failing for good.
//!
//! Subscribers get a counter that moves after each burst of new addresses
//! (settled for [`SETTLE`]), and on a slow tick as a safety net for missed
//! events. Their work must be idempotent: a tick means "look again", not
//! "something specific changed".

use std::process::Stdio;
use std::time::Duration;

use tokio::io::{AsyncBufReadExt, BufReader};
use tokio::sync::watch;
use tracing::warn;

/// Quiet period after the last new address before subscribers run, so an
/// interface bringing up several addresses triggers one pass.
const SETTLE: Duration = Duration::from_secs(2);
/// Re-check even without events, in case the monitor missed one.
const SAFETY_TICK: Duration = Duration::from_secs(300);
const RESTART_DELAY: Duration = Duration::from_secs(10);

/// Start watching. Clone the receiver for each subscriber.
pub fn spawn() -> watch::Receiver<u64> {
    let (tx, rx) = watch::channel(0);
    tokio::spawn(run(tx));
    rx
}

async fn run(tx: watch::Sender<u64>) {
    loop {
        match monitor(&tx).await {
            Ok(()) => warn!("address watch: `ip monitor` exited; restarting"),
            Err(e) => warn!("address watch: {e}; restarting"),
        }
        if tx.is_closed() {
            return;
        }
        tokio::time::sleep(RESTART_DELAY).await;
        // Addresses may have appeared while the monitor was down.
        bump(&tx);
    }
}

async fn monitor(tx: &watch::Sender<u64>) -> Result<(), String> {
    let mut child = tokio::process::Command::new("ip")
        .args(["-o", "monitor", "address"])
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .kill_on_drop(true)
        .spawn()
        .map_err(|e| format!("start `ip monitor`: {e}"))?;
    let stdout = child.stdout.take().ok_or("`ip monitor` has no stdout")?;
    let mut lines = BufReader::new(stdout).lines();
    let mut tick = tokio::time::interval(SAFETY_TICK);
    tick.tick().await; // the first tick is immediate
    let mut settling = false;
    loop {
        let settle = async {
            if settling {
                tokio::time::sleep(SETTLE).await;
            } else {
                std::future::pending::<()>().await;
            }
        };
        tokio::select! {
            line = lines.next_line() => match line {
                Ok(Some(line)) => settling |= is_new_address(&line),
                Ok(None) => return Ok(()),
                Err(e) => return Err(format!("read `ip monitor`: {e}")),
            },
            () = settle => {
                settling = false;
                bump(tx);
            }
            _ = tick.tick() => bump(tx),
        }
        if tx.is_closed() {
            return Ok(());
        }
    }
}

fn bump(tx: &watch::Sender<u64>) {
    tx.send_modify(|n| *n = n.wrapping_add(1));
}

/// `ip -o monitor address` prints an added address as `2: eth0 inet …` and a
/// removed one with a `Deleted` prefix. Only additions can unblock a bind.
fn is_new_address(line: &str) -> bool {
    let line = line.trim_start();
    !line.is_empty() && !line.starts_with("Deleted")
}

#[cfg(test)]
mod tests {
    use super::is_new_address;

    #[test]
    fn only_added_addresses_count() {
        assert!(is_new_address(
            "4: tailscale0    inet 100.127.184.102/32 scope global tailscale0\\       valid_lft forever preferred_lft forever"
        ));
        assert!(is_new_address("2: eth0    inet6 fe80::1/64 scope link"));
        assert!(!is_new_address(
            "Deleted 4: tailscale0    inet 100.127.184.102/32 scope global tailscale0"
        ));
        assert!(!is_new_address(""));
    }
}
