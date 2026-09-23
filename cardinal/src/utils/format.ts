// Format bytes into KB with one decimal place (legacy function)
export function formatKB(bytes: number | null | undefined): string | null {
  if (bytes == null || !Number.isFinite(bytes)) return null;
  const kb = bytes / 1024;
  return `${kb.toFixed(kb < 10 ? 1 : 0)} KB`;
}

// Binary thresholds match the search engine's size: units.
export function formatFileSize(bytes: number | null | undefined): string | null {
  if (bytes == null || !Number.isFinite(bytes) || bytes < 0) return null;
  const [divisor, unit] = bytes >= 1024 ** 3
    ? [1024 ** 3, 'GB'] as const
    : bytes >= 1024 ** 2
      ? [1024 ** 2, 'MB'] as const
      : bytes >= 1024
        ? [1024, 'KB'] as const
        : [1, 'B'] as const;
  return `${Number((bytes / divisor).toFixed(2))} ${unit}`;
}

// Format timestamp (in seconds) as YYYY-MM-DD HH:mm:ss
export function formatTimestamp(timestampSec: number | null | undefined): string | null {
  if (timestampSec == null || !Number.isFinite(timestampSec)) return null;
  const date = new Date(timestampSec * 1000);

  const year = date.getFullYear();
  const month = String(date.getMonth() + 1).padStart(2, '0');
  const day = String(date.getDate()).padStart(2, '0');
  const hours = String(date.getHours()).padStart(2, '0');
  const minutes = String(date.getMinutes()).padStart(2, '0');
  const seconds = String(date.getSeconds()).padStart(2, '0');

  return `${year}-${month}-${day} ${hours}:${minutes}:${seconds}`;
}
