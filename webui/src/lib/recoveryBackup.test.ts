import { describe, expect, it } from 'vitest';
import { CORE_RECOVERY_PATHS, CORE_RECOVERY_SOURCES } from './recoveryBackup';

describe('system recovery preset', () => {
	it('includes the host credential key and its source machine identity', () => {
		expect(CORE_RECOVERY_PATHS).toContain('/var/lib/systemd/credential.secret');
		expect(CORE_RECOVERY_PATHS).toContain('/etc/machine-id');
		expect(CORE_RECOVERY_PATHS).toEqual(CORE_RECOVERY_SOURCES.map(source => source.path));
		expect(new Set(CORE_RECOVERY_PATHS).size).toBe(CORE_RECOVERY_PATHS.length);
	});
});
