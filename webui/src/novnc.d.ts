declare module '@novnc/novnc' {
	interface RFBOptions {
		wsProtocols?: string[];
		credentials?: { username?: string; password?: string; target?: string };
	}

	export default class RFB {
		constructor(target: HTMLElement, url: string, options?: RFBOptions);

		scaleViewport: boolean;
		resizeSession: boolean;
		clipViewport: boolean;
		showDotCursor: boolean;
		viewOnly: boolean;
		qualityLevel: number;
		compressionLevel: number;

		disconnect(): void;
		sendCredentials(credentials: { username?: string; password?: string; target?: string }): void;
		sendKey(keysym: number, code: string | null, down?: boolean): void;
		sendCtrlAltDel(): void;
		focus(): void;
		blur(): void;

		addEventListener(event: string, handler: (e: any) => void): void;
		removeEventListener(event: string, handler: (e: any) => void): void;
	}
}
