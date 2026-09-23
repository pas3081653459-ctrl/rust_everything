import { describe, expect, it } from 'vitest';
import { formatFileSize, formatKB } from '../format';

describe('formatFileSize', () => {
  it('uses binary MB and GB boundaries and removes trailing zeroes', () => {
    expect(formatFileSize(1024 ** 2)).toBe('1 MB');
    expect(formatFileSize(1.5 * 1024 ** 2)).toBe('1.5 MB');
    expect(formatFileSize(1024 ** 3)).toBe('1 GB');
    expect(formatFileSize(8192 * 1024 ** 3)).toBe('8192 GB');
    expect(formatFileSize(0)).toBe('0 B');
    expect(formatFileSize(Number.NaN)).toBeNull();
  });
});

describe('formatKB', () => {
  it('formats whole kilobytes without decimal digits', () => {
    expect(formatKB(2048)).toBe('2.0 KB');
  });

  it('formats small values with a single decimal place', () => {
    expect(formatKB(1536)).toBe('1.5 KB');
  });

  it('returns null for nullish or non-finite inputs', () => {
    expect(formatKB(null)).toBeNull();
    expect(formatKB(undefined)).toBeNull();
    expect(formatKB(Number.POSITIVE_INFINITY)).toBeNull();
  });
});
