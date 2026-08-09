const baseUrl = process.env.SOLO_URL ?? "http://127.0.0.1:17821";
const bearerToken = process.env.SOLO_BEARER_TOKEN;

const headers = {
  Accept: "application/json",
  "Content-Type": "application/json",
  ...(bearerToken ? { Authorization: `Bearer ${bearerToken}` } : {}),
};

async function request(path, options = {}) {
  const response = await fetch(`${baseUrl}${path}`, {
    ...options,
    headers: { ...headers, ...(options.headers ?? {}) },
  });
  const text = await response.text();
  const body = tryParseJson(text) ?? {};
  if (!response.ok) {
    throw new Error(`Solo HTTP ${response.status}: ${body.error ?? text}`);
  }
  return body;
}

function tryParseJson(text) {
  if (!text) return undefined;
  try {
    return JSON.parse(text);
  } catch {
    return undefined;
  }
}

const content =
  process.argv.slice(2).join(" ") ||
  "Avery prefers planning notes with owners and dates.";

const status = await request("/v1/status", { method: "GET" });
const saved = await request("/memory", {
  method: "POST",
  body: JSON.stringify({
    content,
    source_type: "sdk_direct_http",
    salience: 0.7,
  }),
});
const context = await request("/memory/context", {
  method: "POST",
  body: JSON.stringify({
    query: "planning notes owners dates",
    subject: "Avery",
    limit: 3,
  }),
});

console.log({
  library: status.library.name,
  memory_id: saved.memory_id,
  recall_count: context.sections.recall.count,
  facts_count: context.sections.facts.count,
});
