import { clsx, type ClassValue } from 'clsx';
import { twMerge } from 'tailwind-merge';

/**
 * `cn` — shadcn convention: merges tailwind classes intelligently so
 * conditional class names override base ones without specificity hacks.
 */
export function cn(...inputs: ClassValue[]) {
  return twMerge(clsx(inputs));
}
