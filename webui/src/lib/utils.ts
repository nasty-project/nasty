import { clsx, type ClassValue } from "clsx";
import { twMerge } from "tailwind-merge";

export function cn(...inputs: ClassValue[]) {
	return twMerge(clsx(inputs));
}

// eslint-disable-next-line @typescript-eslint/no-explicit-any
export type WithoutChild<T> = T extends { child?: any } ? Omit<T, "child"> : T;
// eslint-disable-next-line @typescript-eslint/no-explicit-any
export type WithoutChildren<T> = T extends { children?: any } ? Omit<T, "children"> : T;
export type WithoutChildrenOrChild<T> = WithoutChildren<WithoutChild<T>>;
export type WithElementRef<T, U extends HTMLElement = HTMLElement> = T & { ref?: U | null };

/**
 * Tailwind class string that highlights a form input as "needs filling
 * in before submit will work". Used on required fields across the
 * WebUI so a submit attempt with a missing field gives the operator a
 * per-input signal as to *which* fields are blocking — instead of a
 * silent greyed button.
 *
 * `tried` defers the decoration until the operator has tried to
 * submit at least once. Without that gate, every form opens with
 * every required field lit up amber which reads like an alarm before
 * the operator has done anything (reported). The expected pattern is
 * "clean on open → amber after a failed Install click → clean again
 * once filled in". Default is `true` so call sites that want the
 * always-on behaviour don't have to pass it.
 *
 * `missing` is computed by the call site — the helper doesn't model
 * what "missing" means (trim? falsy? mismatch? wrong format?).
 */
export function requiredFieldCls(missing: boolean, tried: boolean = true): string {
	return tried && missing ? "border-amber-500 ring-1 ring-amber-500/50" : "";
}
