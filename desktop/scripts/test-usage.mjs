import assert from "node:assert/strict";
import { readFileSync } from "node:fs";
import ts from "typescript";
const source = readFileSync(new URL("../src/progress.ts", import.meta.url), "utf8");
const code = ts.transpileModule(source, { compilerOptions: { target: ts.ScriptTarget.ES2020, module: ts.ModuleKind.ES2020 } }).outputText;
const { parseProgressPayload: parse } = await import(`data:text/javascript;base64,${Buffer.from(code).toString("base64")}`);
const usage = { provider_model: "openai:test", api_calls: 2, input_tokens: 1000, output_tokens: 100, cached_input_tokens: 400, local_cache_hits: 1, local_cache_misses: 1, http_retries: 1, recovery_retries: 0, usage_responses: 1, unreported_requests: 1, estimated_cost_usd: 0.0022 };
assert.equal(parse({ UsageUpdated: usage }).usage.input_tokens, 1000);
assert.equal(parse({ UsageUpdated: { ...usage, estimated_cost_usd: null } }).usage.estimated_cost_usd, null);
for (const bad of [ { ...usage, api_calls: -1 }, { ...usage, input_tokens: NaN }, { ...usage, api_calls: 1.5 }, { ...usage, provider_model: null }, { ...usage, estimated_cost_usd: -1 }, { ...usage, local_cache_hits: undefined } ]) {
  assert.equal(parse({ UsageUpdated: bad }), null);
}
assert.deepEqual(parse("Completed"), { type: "Completed" });
assert.deepEqual(parse({ Started: { total: 5 } }), { type: "Started", total: 5 });
console.log("10 usage parser checks passed");
