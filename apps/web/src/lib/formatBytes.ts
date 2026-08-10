export type FormatBytesOptions = {
  nullValue?: string;
  undefinedValue?: string;
  precision?: 'compact' | 'fixed1';
};

export function formatBytes(
  value: number | null | undefined,
  {
    nullValue = 'none',
    undefinedValue = 'pending',
    precision = 'compact',
  }: FormatBytesOptions = {},
): string {
  if (value === null) return nullValue;
  if (value === undefined) return undefinedValue;
  if (value < 1024) return `${value} B`;

  const units = ['KB', 'MB', 'GB', 'TB'];
  let size = value / 1024;
  let index = 0;
  while (size >= 1024 && index < units.length - 1) {
    size /= 1024;
    index += 1;
  }

  const digits = precision === 'fixed1' || size < 10 ? 1 : 0;
  return `${size.toFixed(digits)} ${units[index]}`;
}
