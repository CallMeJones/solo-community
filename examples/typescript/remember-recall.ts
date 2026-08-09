import { SoloClient } from "../../sdks/typescript/solo-client.js";

const solo = new SoloClient({
  baseUrl: process.env.SOLO_URL ?? "http://127.0.0.1:17821",
  bearerToken: process.env.SOLO_BEARER_TOKEN,
});

const content =
  process.argv.slice(2).join(" ") ||
  "Avery prefers planning notes with owners and dates.";

const saved = await solo.remember(content, {
  sourceType: "sdk_example",
  salience: 0.7,
});
console.log(`remembered ${saved.memory_id}`);

const recall = await solo.recall("planning notes owners dates", { limit: 3 });
for (const hit of recall.hits) {
  console.log(`- ${hit.memory_id}: ${hit.content}`);
}

const context = await solo.context("planning notes owners dates", {
  subject: "Avery",
  limit: 3,
});
console.log(
  `context: recall=${context.sections.recall.count} facts=${context.sections.facts.count} themes=${context.sections.themes.count}`,
);
