import { openai } from "@ai-sdk/openai";
import { generateText, tool } from "ai";
import { z } from "zod";

import { SoloClient } from "../../sdks/typescript/solo-client.js";

const solo = new SoloClient({
  baseUrl: process.env.SOLO_URL ?? "http://127.0.0.1:17821",
  bearerToken: process.env.SOLO_BEARER_TOKEN,
});

const memoryContext = tool({
  description: "Retrieve durable Solo memory context before answering.",
  inputSchema: z.object({
    query: z.string().describe("What the model needs memory context about."),
    subject: z.string().optional().describe("Optional person, project, or entity."),
  }),
  execute: async ({ query, subject }) =>
    solo.context(query, { subject, limit: 5 }),
});

const rememberDurableFact = tool({
  description: "Store a durable, user-approved fact in Solo memory.",
  inputSchema: z.object({
    content: z.string().describe("Durable fact to remember."),
    salience: z.number().min(0).max(1).default(0.7),
  }),
  execute: async ({ content, salience }) =>
    solo.remember(content, {
      sourceType: "vercel_ai_sdk",
      salience,
    }),
});

const prompt =
  process.argv.slice(2).join(" ") ||
  "Use my memory to plan Avery's next weekly review.";

const priorContext = await solo.context(prompt, { limit: 5 });

const result = await generateText({
  model: openai(process.env.OPENAI_MODEL ?? "gpt-4.1-mini"),
  system: [
    "You are a Solo-aware assistant.",
    "Use the preloaded Solo context when it is relevant.",
    "Call memoryContext if you need more context.",
    "Call rememberDurableFact only for durable, user-approved facts.",
    `Preloaded Solo context: ${JSON.stringify(priorContext)}`,
  ].join("\n"),
  tools: {
    memoryContext,
    rememberDurableFact,
  },
  prompt,
});

console.log(result.text);
