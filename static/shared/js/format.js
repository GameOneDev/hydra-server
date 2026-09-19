/** Value formatting, shared so the same number never reads two ways. */

const UNITS = ["B", "KB", "MB", "GB", "TB", "PB"];

/** Binary sizes, the unit everything on disk is measured in. */
export function bytes(value) {
  const n = Number(value) || 0;
  if (n <= 0) return "0 B";
  const exponent = Math.min(UNITS.length - 1, Math.floor(Math.log2(n) / 10));
  const scaled = n / 1024 ** exponent;
  const digits = exponent === 0 ? 0 : scaled < 10 ? 2 : scaled < 100 ? 1 : 0;
  return `${scaled.toFixed(digits)} ${UNITS[exponent]}`;
}

export function number(value) {
  return new Intl.NumberFormat().format(Number(value) || 0);
}

/** Compact form for axis ticks and dense tiles: 12.4k, 3.1M. */
export function compact(value) {
  return new Intl.NumberFormat(undefined, { notation: "compact", maximumFractionDigits: 1 })
    .format(Number(value) || 0);
}

export function percent(ratio, digits = 0) {
  return `${((Number(ratio) || 0) * 100).toFixed(digits)}%`;
}

export function dateTime(iso) {
  if (!iso) return "—";
  const date = new Date(iso);
  return Number.isNaN(+date) ? "—" : date.toLocaleString();
}

export function date(iso) {
  if (!iso) return "—";
  const value = new Date(iso);
  return Number.isNaN(+value)
    ? "—"
    : value.toLocaleDateString(undefined, { year: "numeric", month: "short", day: "numeric" });
}

const RELATIVE_STEPS = [
  [60, "second", 1],
  [3600, "minute", 60],
  [86400, "hour", 3600],
  [604800, "day", 86400],
  [2629800, "week", 604800],
  [31557600, "month", 2629800],
  [Infinity, "year", 31557600],
];

/** "3 minutes ago" / "in 2 days" — the form an operator scans fastest. */
export function relative(iso) {
  if (!iso) return "never";
  const value = new Date(iso);
  if (Number.isNaN(+value)) return "never";

  const seconds = (value.getTime() - Date.now()) / 1000;
  const magnitude = Math.abs(seconds);
  if (magnitude < 45) return "just now";

  const [, unit, divisor] = RELATIVE_STEPS.find(([limit]) => magnitude < limit);
  const formatter = new Intl.RelativeTimeFormat(undefined, { numeric: "auto" });
  return formatter.format(Math.round(seconds / divisor), unit);
}

/** Playtime and uptime: days and hours, never 4913 minutes. */
export function duration(seconds) {
  const total = Math.max(0, Math.round(Number(seconds) || 0));
  const days = Math.floor(total / 86400);
  const hours = Math.floor((total % 86400) / 3600);
  const minutes = Math.floor((total % 3600) / 60);

  if (days) return `${days}d ${hours}h`;
  if (hours) return `${hours}h ${minutes}m`;
  if (minutes) return `${minutes}m`;
  return `${total}s`;
}

export function plural(count, singular, pluralForm = `${singular}s`) {
  return `${number(count)} ${Math.abs(Number(count)) === 1 ? singular : pluralForm}`;
}

export function quota(value) {
  return Number(value) > 0 ? bytes(value) : "unlimited";
}

/**
 * A JSON key as a human label: "safetyBackup" -> "Safety backup".
 *
 * Sentence case, not title case — but only where a capital follows a lowercase
 * letter, so an acronym someone wrote deliberately ("clientIP") survives.
 */
export function label(key) {
  return String(key)
    .replace(/([a-z0-9])([A-Z])/g, (_, before, capital) => `${before} ${capital.toLowerCase()}`)
    .replace(/^./, (char) => char.toUpperCase())
    .trim();
}

/** Enough of a SHA-256 to recognise, not so much it wraps. */
export function shortHash(hash = "") {
  return hash.slice(0, 10);
}

/** Whether the store ever answered with a name for this game. */
export function isGameNamed(game) {
  return Boolean(game?.name?.trim());
}

/** Game display name, falling back to the raw id the launcher sent. */
export function gameName(game) {
  if (!game) return "Unknown game";
  return (isGameNamed(game) && game.name.trim()) || game.objectId || "Unknown game";
}

export function gameSub(game) {
  if (!game?.shop) return "";
  return `${game.shop}/${game.objectId}`;
}
