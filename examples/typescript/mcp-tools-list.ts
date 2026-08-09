import { SoloClient } from "../../sdks/typescript/solo-client.js";

const solo = new SoloClient({
  baseUrl: process.env.SOLO_URL ?? "http://127.0.0.1:17821",
  bearerToken: process.env.SOLO_BEARER_TOKEN,
});

const session = await solo.mcpConnect({
  name: "solo-sdk-example",
  version: "0.0.0",
});

const tools = await solo.mcpListTools(session);
for (const tool of tools) {
  console.log(tool.name);
}
