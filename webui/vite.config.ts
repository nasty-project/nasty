import { sveltekit } from '@sveltejs/kit/vite';
import adapter from '@sveltejs/adapter-static';
import tailwindcss from '@tailwindcss/vite';
import { defineConfig } from 'vite';
import { fileURLToPath } from 'node:url';

export default defineConfig({
	plugins: [tailwindcss(), sveltekit({
		adapter: adapter({ fallback: 'index.html' })
	})],
	resolve: {
		// Kit 3 no longer supplies $lib; preserve our existing imports explicitly.
		alias: { $lib: fileURLToPath(new URL('./src/lib', import.meta.url)) }
	}
});
