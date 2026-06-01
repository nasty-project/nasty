/**
 * Per-attribute metadata for ATA SMART attributes.
 *
 * Adapted from Scrutiny's curated metadata table
 * (github.com/AnalogJ/scrutiny, MIT license). Scrutiny's `critical`
 * flag derives from Backblaze drive-stats failure-rate analysis
 * (CC-BY 4.0), so the eight attributes flagged here are the ones
 * Backblaze's empirical data shows most reliably predict drive
 * failure — vs. vendor-supplied SMART thresholds which are
 * notoriously stingy.
 *
 * `ideal` describes which raw-value direction is healthy:
 *   - 'low'  — higher values are worse (e.g. reallocated sector count)
 *   - 'high' — lower values are worse (e.g. helium level)
 *   - ''     — no clear direction (vendor-specific encoded values)
 *
 * To refresh: re-run scripts/extract_smart_attribute_metadata.py
 * against the upstream ata_attribute_metadata.go.
 */

export interface AtaAttributeMetadata {
	name: string;
	critical: boolean;
	ideal: 'low' | 'high' | '';
	description: string;
}

export const ATA_ATTRIBUTE_METADATA: Record<number, AtaAttributeMetadata> = {
	1: { name: 'Read Error Rate', critical: false, ideal: 'low', description: '(Vendor specific raw value.) Stores data related to the rate of hardware read errors that occurred when reading data from a disk surface. The raw value has different structure for different vendors and is often not meaningful as a decimal number.' },
	2: { name: 'Throughput Performance', critical: false, ideal: 'high', description: 'Overall (general) throughput performance of a hard disk drive. If the value of this attribute is decreasing there is a high probability that there is a problem with the disk.' },
	3: { name: 'Spin-Up Time', critical: false, ideal: 'low', description: 'Average time of spindle spin up (from zero RPM to fully operational [milliseconds]).' },
	4: { name: 'Start/Stop Count', critical: false, ideal: '', description: 'A tally of spindle start/stop cycles. The spindle turns on, and hence the count is increased, both when the hard disk is turned on after having before been turned entirely off (disconnected from power source) and when the hard disk returns from having previously been put to sleep mode.' },
	5: { name: 'Reallocated Sectors Count', critical: true, ideal: 'low', description: 'Count of reallocated sectors. The raw value represents a count of the bad sectors that have been found and remapped.Thus, the higher the attribute value, the more sectors the drive has had to reallocate. This value is primarily used as a metric of the life expectancy of the drive; a drive which has had any reallocations at all is significantly more likely to fail in the immediate months.' },
	6: { name: 'Read Channel Margin', critical: false, ideal: '', description: 'Margin of a channel while reading data. The function of this attribute is not specified.' },
	7: { name: 'Seek Error Rate', critical: false, ideal: '', description: '(Vendor specific raw value.) Rate of seek errors of the magnetic heads. If there is a partial failure in the mechanical positioning system, then seek errors will arise. Such a failure may be due to numerous factors, such as damage to a servo, or thermal widening of the hard disk. The raw value has different structure for different vendors and is often not meaningful as a decimal number.' },
	8: { name: 'Seek Time Performance', critical: false, ideal: 'high', description: 'Average performance of seek operations of the magnetic heads. If this attribute is decreasing, it is a sign of problems in the mechanical subsystem.' },
	9: { name: 'Power-On Hours', critical: false, ideal: '', description: 'Count of hours in power-on state. The raw value of this attribute shows total count of hours (or minutes, or seconds, depending on manufacturer) in power-on state. By default, the total expected lifetime of a hard disk in perfect condition is defined as 5 years (running every day and night on all days). This is equal to 1825 days in 24/7 mode or 43800 hours. On some pre-2005 drives, this raw value may advance erratically and/or "wrap around" (reset to zero periodically).' },
	10: { name: 'Spin Retry Count', critical: true, ideal: 'low', description: 'Count of retry of spin start attempts. This attribute stores a total count of the spin start attempts to reach the fully operational speed (under the condition that the first attempt was unsuccessful). An increase of this attribute value is a sign of problems in the hard disk mechanical subsystem.' },
	11: { name: 'Recalibration Retries or Calibration Retry Count', critical: false, ideal: 'low', description: 'This attribute indicates the count that recalibration was requested (under the condition that the first attempt was unsuccessful). An increase of this attribute value is a sign of problems in the hard disk mechanical subsystem.' },
	12: { name: 'Power Cycle Count', critical: false, ideal: 'low', description: 'This attribute indicates the count of full hard disk power on/off cycles.' },
	13: { name: 'Soft Read Error Rate', critical: false, ideal: 'low', description: 'Uncorrected read errors reported to the operating system.' },
	22: { name: 'Current Helium Level', critical: false, ideal: 'high', description: 'Specific to He8 drives from HGST. This value measures the helium inside of the drive specific to this manufacturer. It is a pre-fail attribute that trips once the drive detects that the internal environment is out of specification.' },
	170: { name: 'Available Reserved Space', critical: false, ideal: '', description: 'See attribute E8.' },
	171: { name: 'SSD Program Fail Count', critical: false, ideal: '', description: '(Kingston) The total number of flash program operation failures since the drive was deployed.[33] Identical to attribute 181.' },
	172: { name: 'SSD Erase Fail Count', critical: false, ideal: '', description: '(Kingston) Counts the number of flash erase failures. This attribute returns the total number of Flash erase operation failures since the drive was deployed. This attribute is identical to attribute 182.' },
	173: { name: 'SSD Wear Leveling Count', critical: false, ideal: '', description: 'Counts the maximum worst erase count on any block.' },
	174: { name: 'Unexpected Power Loss Count', critical: false, ideal: '', description: 'Also known as "Power-off Retract Count" per conventional HDD terminology. Raw value reports the number of unclean shutdowns, cumulative over the life of an SSD, where an "unclean shutdown" is the removal of power without STANDBY IMMEDIATE as the last command (regardless of PLI activity using capacitor power). Normalized value is always 100.' },
	175: { name: 'Power Loss Protection Failure', critical: false, ideal: '', description: 'Last test result as microseconds to discharge cap, saturated at its maximum value. Also logs minutes since last test and lifetime number of tests. Raw value contains the following data:     Bytes 0-1: Last test result as microseconds to discharge cap, saturates at max value. Test result expected in range 25 <= result <= 5000000, lower indicates specific error code. Bytes 2-3: Minutes since last test, saturates at max value.Bytes 4-5: Lifetime number of tests, not incremented on power cycle, saturates at max value. Normalized value is set to one on test failure or 11 if the capacitor has been tested in an excessive temperature condition, otherwise 100.' },
	176: { name: 'Erase Fail Count', critical: false, ideal: '', description: 'S.M.A.R.T. parameter indicates a number of flash erase command failures.' },
	177: { name: 'Wear Range Delta', critical: false, ideal: '', description: 'Delta between most-worn and least-worn Flash blocks. It describes how good/bad the wearleveling of the SSD works on a more technical way. ' },
	179: { name: 'Used Reserved Block Count Total', critical: false, ideal: '', description: 'Pre-Fail attribute used at least in Samsung devices.' },
	180: { name: 'Unused Reserved Block Count Total', critical: false, ideal: '', description: '"Pre-Fail" attribute used at least in HP devices. ' },
	181: { name: 'Program Fail Count Total', critical: false, ideal: '', description: 'Total number of Flash program operation failures since the drive was deployed.' },
	182: { name: 'Erase Fail Count', critical: false, ideal: '', description: '"Pre-Fail" Attribute used at least in Samsung devices.' },
	183: { name: 'SATA Downshift Error Count or Runtime Bad Block', critical: false, ideal: 'low', description: 'Western Digital, Samsung or Seagate attribute: Either the number of downshifts of link speed (e.g. from 6Gbit/s to 3Gbit/s) or the total number of data blocks with detected, uncorrectable errors encountered during normal operation. Although degradation of this parameter can be an indicator of drive aging and/or potential electromechanical problems, it does not directly indicate imminent drive failure.' },
	184: { name: 'End-to-End error', critical: true, ideal: 'low', description: 'This attribute is a part of Hewlett-Packard"s SMART IV technology, as well as part of other vendors" IO Error Detection and Correction schemas, and it contains a count of parity errors which occur in the data path to the media via the drive"s cache RAM' },
	185: { name: 'Head Stability', critical: false, ideal: '', description: 'Western Digital attribute.' },
	186: { name: 'Induced Op-Vibration Detection', critical: false, ideal: '', description: 'Western Digital attribute.' },
	187: { name: 'Reported Uncorrectable Errors', critical: true, ideal: 'low', description: 'The count of errors that could not be recovered using hardware ECC (see attribute 195).' },
	188: { name: 'Command Timeout', critical: true, ideal: 'low', description: 'The count of aborted operations due to HDD timeout. Normally this attribute value should be equal to zero.' },
	189: { name: 'High Fly Writes', critical: false, ideal: 'low', description: 'HDD manufacturers implement a flying height sensor that attempts to provide additional protections for write operations by detecting when a recording head is flying outside its normal operating range. If an unsafe fly height condition is encountered, the write process is stopped, and the information is rewritten or reallocated to a safe region of the hard drive. This attribute indicates the count of these errors detected over the lifetime of the drive.' },
	190: { name: 'Temperature Difference', critical: false, ideal: '', description: 'Value is equal to (100-temp. Â°C), allowing manufacturer to set a minimum threshold which corresponds to a maximum temperature. This also follows the convention of 100 being a best-case value and lower values being undesirable. However, some older drives may instead report raw Temperature (identical to 0xC2) or Temperature minus 50 here.' },
	191: { name: 'G-sense Error Rate', critical: false, ideal: 'low', description: 'The count of errors resulting from externally induced shock and vibration. ' },
	192: { name: 'Power-off Retract Count', critical: false, ideal: 'low', description: 'Number of power-off or emergency retract cycles.' },
	193: { name: 'Load Cycle Count', critical: false, ideal: 'low', description: 'Count of load/unload cycles into head landing zone position.[45] Some drives use 225 (0xE1) for Load Cycle Count instead.' },
	194: { name: 'Temperature', critical: false, ideal: 'low', description: 'Indicates the device temperature, if the appropriate sensor is fitted. Lowest byte of the raw value contains the exact temperature value (Celsius degrees).' },
	195: { name: 'Hardware ECC Recovered', critical: false, ideal: '', description: '(Vendor-specific raw value.) The raw value has different structure for different vendors and is often not meaningful as a decimal number.' },
	196: { name: 'Reallocation Event Count', critical: true, ideal: 'low', description: 'Count of remap operations. The raw value of this attribute shows the total count of attempts to transfer data from reallocated sectors to a spare area. Both successful and unsuccessful attempts are counted.' },
	197: { name: 'Current Pending Sector Count', critical: true, ideal: 'low', description: 'Count of "unstable" sectors (waiting to be remapped, because of unrecoverable read errors). If an unstable sector is subsequently read successfully, the sector is remapped and this value is decreased. Read errors on a sector will not remap the sector immediately (since the correct value cannot be read and so the value to remap is not known, and also it might become readable later); instead, the drive firmware remembers that the sector needs to be remapped, and will remap it the next time it"s written.' },
	198: { name: '(Offline) Uncorrectable Sector Count', critical: true, ideal: 'low', description: 'The total count of uncorrectable errors when reading/writing a sector. A rise in the value of this attribute indicates defects of the disk surface and/or problems in the mechanical subsystem.' },
	199: { name: 'UltraDMA CRC Error Count', critical: false, ideal: 'low', description: 'The count of errors in data transfer via the interface cable as determined by ICRC (Interface Cyclic Redundancy Check).' },
	200: { name: 'Multi-Zone Error Rate', critical: false, ideal: 'low', description: 'The count of errors found when writing a sector. The higher the value, the worse the disk"s mechanical condition is.' },
	201: { name: 'Soft Read Error Rate', critical: true, ideal: 'low', description: 'Count indicates the number of uncorrectable software read errors.' },
	202: { name: 'Data Address Mark errors', critical: false, ideal: 'low', description: 'Count of Data Address Mark errors (or vendor-specific).' },
	203: { name: 'Run Out Cancel', critical: false, ideal: 'low', description: 'The number of errors caused by incorrect checksum during the error correction.' },
	204: { name: 'Soft ECC Correction', critical: false, ideal: 'low', description: 'Count of errors corrected by the internal error correction software.' },
	205: { name: 'Thermal Asperity Rate', critical: false, ideal: 'low', description: 'Count of errors due to high temperature.' },
	206: { name: 'Flying Height', critical: false, ideal: '', description: 'Height of heads above the disk surface. If too low, head crash is more likely; if too high, read/write errors are more likely.' },
	207: { name: 'Spin High Current', critical: false, ideal: 'low', description: 'Amount of surge current used to spin up the drive.' },
	208: { name: 'Spin Buzz', critical: false, ideal: '', description: 'Count of buzz routines needed to spin up the drive due to insufficient power.' },
	209: { name: 'Offline Seek Performance', critical: false, ideal: '', description: 'Drive"s seek performance during its internal tests.' },
	210: { name: 'Vibration During Write', critical: false, ideal: '', description: 'Found in Maxtor 6B200M0 200GB and Maxtor 2R015H1 15GB disks.' },
	211: { name: 'Vibration During Write', critical: false, ideal: '', description: 'A recording of a vibration encountered during write operations.' },
	212: { name: 'Shock During Write', critical: false, ideal: '', description: 'A recording of shock encountered during write operations.' },
	220: { name: 'Disk Shift', critical: false, ideal: 'low', description: 'Distance the disk has shifted relative to the spindle (usually due to shock or temperature). Unit of measure is unknown.' },
	221: { name: 'G-Sense Error Rate', critical: false, ideal: 'low', description: 'The count of errors resulting from externally induced shock and vibration.' },
	222: { name: 'Loaded Hours', critical: false, ideal: '', description: 'Time spent operating under data load (movement of magnetic head armature).' },
	223: { name: 'Load/Unload Retry Count', critical: false, ideal: '', description: 'Count of times head changes position.' },
	224: { name: 'Load Friction', critical: false, ideal: 'low', description: 'Resistance caused by friction in mechanical parts while operating.' },
	225: { name: 'Load/Unload Cycle Count', critical: false, ideal: 'low', description: 'Total count of load cycles Some drives use 193 (0xC1) for Load Cycle Count instead. See Description for 193 for significance of this number. ' },
	226: { name: 'Load "In"-time', critical: false, ideal: '', description: 'Total time of loading on the magnetic heads actuator (time not spent in parking area).' },
	227: { name: 'Torque Amplification Count', critical: false, ideal: 'low', description: 'Count of attempts to compensate for platter speed variations.[66]' },
	228: { name: 'Power-Off Retract Cycle', critical: false, ideal: 'low', description: 'The number of power-off cycles which are counted whenever there is a "retract event" and the heads are loaded off of the media such as when the machine is powered down, put to sleep, or is idle.' },
	230: { name: 'GMR Head Amplitude ', critical: false, ideal: '', description: 'Amplitude of "thrashing" (repetitive head moving motions between operations).' },
	231: { name: 'Life Left', critical: false, ideal: '', description: 'Indicates the approximate SSD life left, in terms of program/erase cycles or available reserved blocks. A normalized value of 100 represents a new drive, with a threshold value at 10 indicating a need for replacement. A value of 0 may mean that the drive is operating in read-only mode to allow data recovery.' },
	232: { name: 'Endurance Remaining', critical: false, ideal: '', description: 'Number of physical erase cycles completed on the SSD as a percentage of the maximum physical erase cycles the drive is designed to endure.' },
	233: { name: 'Media Wearout Indicator', critical: false, ideal: '', description: 'Intel SSDs report a normalized value from 100, a new drive, to a minimum of 1. It decreases while the NAND erase cycles increase from 0 to the maximum-rated cycles.' },
	234: { name: 'Average erase count', critical: false, ideal: '', description: 'Decoded as: byte 0-1-2 = average erase count (big endian) and byte 3-4-5 = max erase count (big endian).' },
	235: { name: 'Good Block Count', critical: false, ideal: '', description: 'Decoded as: byte 0-1-2 = good block count (big endian) and byte 3-4 = system (free) block count.' },
	240: { name: 'Head Flying Hours', critical: false, ideal: '', description: 'Time spent during the positioning of the drive heads.[15][71] Some Fujitsu drives report the count of link resets during a data transfer.' },
	241: { name: 'Total LBAs Written', critical: false, ideal: '', description: 'Total count of LBAs written.' },
	242: { name: 'Total LBAs Read', critical: false, ideal: '', description: 'Total count of LBAs read.Some S.M.A.R.T. utilities will report a negative number for the raw value since in reality it has 48 bits rather than 32.' },
	243: { name: 'Total LBAs Written Expanded', critical: false, ideal: '', description: 'The upper 5 bytes of the 12-byte total number of LBAs written to the device. The lower 7 byte value is located at attribute 0xF1.' },
	244: { name: 'Total LBAs Read Expanded', critical: false, ideal: '', description: 'The upper 5 bytes of the 12-byte total number of LBAs read from the device. The lower 7 byte value is located at attribute 0xF2.' },
	249: { name: 'NAND Writes (1GiB)', critical: false, ideal: '', description: 'Total NAND Writes. Raw value reports the number of writes to NAND in 1 GB increments.' },
	250: { name: 'Read Error Retry Rate', critical: false, ideal: 'low', description: 'Count of errors while reading from a disk.' },
	251: { name: 'Minimum Spares Remaining', critical: false, ideal: '', description: 'The Minimum Spares Remaining attribute indicates the number of remaining spare blocks as a percentage of the total number of spare blocks available.' },
	252: { name: 'Newly Added Bad Flash Block', critical: false, ideal: '', description: 'The Newly Added Bad Flash Block attribute indicates the total number of bad flash blocks the drive detected since it was first initialized in manufacturing.' },
	254: { name: 'Free Fall Protection', critical: false, ideal: 'low', description: 'Count of "Free Fall Events" detected.' },
};

/**
 * Look up metadata for an ATA attribute ID, with a sensible fallback
 * for vendor-specific attributes Scrutiny's table doesn't cover.
 * Vendor-only IDs (typically 200+) appear on individual drives but
 * have no portable interpretation — we fall through to the drive's
 * own attribute name + a generic description.
 */
export function ataAttributeMetadata(id: number, fallbackName: string): AtaAttributeMetadata {
	return ATA_ATTRIBUTE_METADATA[id] ?? {
		name: fallbackName,
		critical: false,
		ideal: '',
		description: 'Vendor-specific attribute — interpretation varies by drive manufacturer.',
	};
}
