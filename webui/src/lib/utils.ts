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
 * WebUI so a disabled submit button gives the operator a per-input
 * signal as to *which* fields are blocking — not just a greyed button
 * with no explanation. Pass `true` when the value is empty or fails
 * its inline validation; the consumer is responsible for picking what
 * "empty" means (trim, falsy, etc.). Returns `''` when no decoration
 * is wanted so callers can inline it in a class= attribute without
 * a wrapping conditional.
 */
export function requiredFieldCls(missing: boolean): string {
	return missing ? "border-amber-500 ring-1 ring-amber-500/50" : "";
}
