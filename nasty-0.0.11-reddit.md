This one's all about driving your bcachefs pool from the WebUI instead of dropping to a terminal, plus a much stronger Docker story.

## bcachefs device lifecycle, fully in the UI

* Scrub with a live progress bar, plus offline fsck (dry-run or repair) right from the browser
* Mount failures now tell you *why* - naming the missing disk by slot and label - and offer a guarded degraded mount
* Richer per-device table with selectable columns (size, errors, type, model, serial), and SMART/disk info shown before you add a candidate disk
* Pull a disk and it shows up as **missing** with a force-remove; reconnect one that belongs to the pool and you get **Bring online / re-attach** instead of being pushed toward a wipe
* Editable tiering targets (foreground/background/promote/metadata) and per-device durability
* Compression levels for zstd and gzip - quick on ingress, deep in the background

## Docker, for real

* Manage Docker networks from the UI, including putting a container directly on your LAN with its own IP (macvlan/ipvlan) - and the host-container shim that normally makes that painful is wired up for you
* Live compose validation in the editor - YAML and schema checked as you type, using the same check deploy runs
* A first-class, relocatable **/appdata** location: bind it in compose and your container data survives moving to another filesystem. One click relocates it onto a new SSD. Snapshot-clean and separate from Docker's internals.

## Security

* Encryption-at-rest sweep is finished - every stored secret (SMB/CHAP, OIDC, NUT, notification channels, DNS-API tokens) is now sealed with systemd-creds, TPM-backed where available

## Quality-of-life

* Reconcile "stalled" alerts stopped crying wolf - they only fire on genuine lack of progress now
* Wiping a disk leaves it actually clean (no more "repair the disk!" lectures from leftover GPT tables)
* Discovery fixes so NASty shows up in your file manager's network view right after enabling SMB or renaming the host - no reboot needed
* Dependency tree brought fully current

Fresh ISOs (x86_64 + aarch64) are on the releases page. **Proxmox users:** NASty needs UEFI - switch the VM from SeaBIOS to OVMF before installing.
