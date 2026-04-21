<script lang="ts">
	import { onMount, onDestroy } from 'svelte';
	import { EditorView, Decoration, keymap, lineNumbers, highlightActiveLine, drawSelection, type DecorationSet } from '@codemirror/view';
	import { EditorState, StateField, StateEffect } from '@codemirror/state';
	import { oneDark } from '@codemirror/theme-one-dark';
	import { yaml } from '@codemirror/lang-yaml';
	import { json } from '@codemirror/lang-json';
	import { defaultKeymap, history, historyKeymap } from '@codemirror/commands';

	interface Props {
		value: string;
		lang?: 'yaml' | 'json';
		placeholder?: string;
		readonly?: boolean;
		class?: string;
		/** Line numbers to mark as errors (1-based) */
		errorLines?: number[];
		oninput?: (value: string) => void;
	}

	let {
		value = $bindable(''),
		lang = 'yaml',
		placeholder = '',
		readonly = false,
		class: className = '',
		errorLines = [],
		oninput,
	}: Props = $props();

	let container: HTMLDivElement;
	let view: EditorView | null = null;
	let skipUpdate = false;

	// eslint-disable-next-line -- lang is intentionally captured once (doesn't change after mount)
	let langExtension: ReturnType<typeof yaml> | ReturnType<typeof json>;
	$effect(() => { langExtension = lang === 'json' ? json() : yaml(); });

	// Error line highlighting via StateField + line decorations
	const setErrorLines = StateEffect.define<number[]>();

	const errorLineMark = Decoration.line({ class: 'cm-error-line' });

	const errorLineField = StateField.define<DecorationSet>({
		create() { return Decoration.none; },
		update(decorations, tr) {
			for (const effect of tr.effects) {
				if (effect.is(setErrorLines)) {
					const marks: ReturnType<typeof errorLineMark.range>[] = [];
					for (const lineNo of effect.value) {
						if (lineNo >= 1 && lineNo <= tr.state.doc.lines) {
							const line = tr.state.doc.line(lineNo);
							marks.push(errorLineMark.range(line.from));
						}
					}
					return Decoration.set(marks);
				}
			}
			return decorations.map(tr.changes);
		},
		provide: f => EditorView.decorations.from(f),
	});

	onMount(() => {
		const extensions = [
			lineNumbers(),
			highlightActiveLine(),
			drawSelection(),
			history(),
			keymap.of([...defaultKeymap, ...historyKeymap]),
			langExtension,
			oneDark,
			errorLineField,
			EditorView.theme({
				'&': {
					fontSize: '13px',
					height: '100%',
				},
				'.cm-content': {
					fontFamily: "'JetBrains Mono', 'Fira Code', 'Cascadia Code', monospace",
					padding: '8px 0',
				},
				'.cm-gutters': {
					backgroundColor: 'transparent',
					borderRight: '1px solid hsl(var(--border))',
				},
				'.cm-scroller': {
					overflow: 'auto',
				},
				'&.cm-focused': {
					outline: 'none',
				},
				'.cm-line': {
					padding: '0 8px',
				},
				'.cm-error-line': {
					backgroundColor: 'rgba(220, 38, 38, 0.15)',
					borderLeft: '3px solid rgb(220, 38, 38)',
					paddingLeft: '5px',
				},
			}),
			EditorView.updateListener.of((update) => {
				if (update.docChanged && !skipUpdate) {
					value = update.state.doc.toString();
					oninput?.(value);
				}
			}),
		];

		if (readonly) {
			extensions.push(EditorState.readOnly.of(true));
		}

		view = new EditorView({
			state: EditorState.create({
				doc: value,
				extensions,
			}),
			parent: container,
		});

		// Apply initial error lines
		if (errorLines.length > 0) {
			view.dispatch({ effects: setErrorLines.of(errorLines) });
		}
	});

	onDestroy(() => {
		view?.destroy();
	});

	// Sync external value changes into the editor
	$effect(() => {
		if (view && value !== view.state.doc.toString()) {
			skipUpdate = true;
			view.dispatch({
				changes: { from: 0, to: view.state.doc.length, insert: value },
			});
			skipUpdate = false;
		}
	});

	// Update error line highlights when errorLines change
	$effect(() => {
		if (view) {
			view.dispatch({ effects: setErrorLines.of(errorLines) });
		}
	});
</script>

<div
	bind:this={container}
	class="overflow-hidden rounded-md border border-input {className}"
></div>
