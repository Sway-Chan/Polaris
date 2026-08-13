/**
 * cn —— Tailwind class 合并工具（Polaris lib/utils.ts 移植，签名不变）。
 * clsx 处理条件类，tailwind-merge 解决冲突（如 'px-2 px-4' → 'px-4'）。
 */
import { type ClassValue, clsx } from 'clsx';
import { twMerge } from 'tailwind-merge';

export function cn(...inputs: ClassValue[]): string {
  return twMerge(clsx(inputs));
}
