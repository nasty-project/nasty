# Configuring alert thresholds

As an unscoped administrator, open **Alerts**, find a rule and select **Edit**.
You can change its name, threshold and severity, then **Save changes** or **Cancel**.
Metric, comparison condition and enabled state are retained. Enable/disable remains
a separate action. Backup-failure rules retain their fixed event threshold.

The default disk-temperature rules warn **above 50°C** and become critical
**above 60°C**. Editing these rules applies to all monitored disks, including the
boot disk; it is not a per-device or HDD/SSD/NVMe-specific override. Defaults are
unchanged for users who do not edit them. Choose limits appropriate to the devices
you operate rather than simply silencing temperature alerts.

Temperature thresholds are shown in your selected Celsius/Fahrenheit display
unit but stored in Celsius. The editor keeps the unit selected when editing began,
so changing a display preference cannot reinterpret a draft. An unchanged input
does not round or rewrite the stored threshold.

Updates persist through the existing `alert.rules.update` API. Successful rule
updates invalidate cached active alerts and dashboard status so the next refresh
uses the changed rule. Changes do not run repairs or change hardware settings.
