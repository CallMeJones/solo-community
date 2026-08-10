const URL_PATTERN = /^https?:\/\/[^\s]+$/i;
const HTTP_URL_REQUIRED = 'http(s) URL required';

export function soloApiUrlError(value: string): string | null {
  if (!URL_PATTERN.test(value)) return HTTP_URL_REQUIRED;

  try {
    const parsed = new URL(value);
    if (parsed.protocol !== 'http:' && parsed.protocol !== 'https:') return HTTP_URL_REQUIRED;
    if (parsed.username || parsed.password) {
      return 'Credentials are not allowed in the URL; use the bearer token field';
    }
    // URL.search/hash are empty for a bare trailing `?`/`#`, so inspect the
    // serialized URL to reject those delimiters as well.
    if (parsed.href.includes('?') || parsed.href.includes('#')) {
      return 'Query strings and fragments are not allowed';
    }
    return null;
  } catch {
    return HTTP_URL_REQUIRED;
  }
}
