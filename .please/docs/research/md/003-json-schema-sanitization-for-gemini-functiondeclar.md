---
id: 003
title: "JSON Schema sanitization for Gemini functionDeclarations across reference bridges"
date: 2026-09-03
summary: "How CLIProxyAPI, gemini-cli, opencode, and claude-code-router/@musistudio/llms sanitize JSON Schema before sending tool definitions to Gemini / Antigravity, and specifically what each does with a tuple array (prefixItems, no items)."
tags: [gemini, antigravity, json-schema, tool-calling, cliproxyapi, gemini-cli, opencode, prefixitems]
---

# JSON Schema sanitization for Gemini functionDeclarations

Question: how do reference implementations sanitize JSON Schema before emitting Gemini
`functionDeclarations`, and do they handle `{"type":"array","prefixItems":[...]}` with no `items`?

## 1. router-for-me/CLIProxyAPI (@ d577e630)

All cleaning lives in one file: `internal/util/gemini_schema.go`. Four entry points share
`cleanJSONSchema(jsonStr, jsonSchemaCleanOptions)`:

| Function | Options |
|---|---|
| `CleanJSONSchemaForGemini` | `addMissingArrayItems`, `removeGeminiMetadata`, `flattenUnions`, `forceEnumStringType` |
| `CleanJSONSchemaForAntigravity` / `...Tool(_, requirePlaceholder)` | `addPlaceholder`, `addMissingArrayItems`, `antigravitySemantics`, `flattenUnions`, `dropAllEnums` |
| `CleanJSONSchemaForAntigravityResponse` | `antigravitySemantics`, `flattenUnions`, `dropBooleanEnums`, `preserveAdditionalPropertiesFalse` (no `addMissingArrayItems`) |

Pipeline (`cleanJSONSchema`): normalize malformed nodes → inline `$ref` / ref hints → `const`→`enum`
→ enum→string → enum/additionalProperties/constraint hints → merge `if/then/else`, `allOf` → flatten
`anyOf`/`oneOf` → flatten type arrays → remove unsupported keywords → cleanup `required` → empty-schema
placeholder.

Removed outright (`removeUnsupportedKeywords`): `$schema`, `$defs`, `definitions`, `const`, `$ref`,
`$id`, `additionalProperties`, `propertyNames`, `patternProperties`, `if`, `then`, `else`, `$comment`,
`enumDescriptions`, `enumTitles`, `prefill`, `deprecated`, `encrypted`, all `x-*`, plus `not` for
Antigravity. Moved into `description` then deleted (`unsupportedConstraints`): `minLength`, `maxLength`,
`exclusiveMinimum`, `exclusiveMaximum`, `pattern`, `minItems`, `maxItems`, `uniqueItems`, `contains`,
`format`, `default`, `examples` (+ `minimum`, `maximum`, `multipleOf` for Antigravity).

Tuple arrays: **`prefixItems` is never converted to `items` and never removed.** It is only recursed
into for repair (`repairSchemaNode`, list keys `anyOf/oneOf/allOf/prefixItems`). Missing `items` is
filled with a hardcoded string schema, independent of `prefixItems`:

```go
// Gemini and Antigravity reject tool array schemas without an items definition.
if addMissingArrayItems && isArrayDeclaredType(clone["type"]) {
    if _, hasItems := clone["items"]; !hasItems {
        clone["items"] = map[string]any{"type": "string"}
        modified = true
    }
}
```

So `{"type":"array","prefixItems":[{"type":"string"},{"type":"number"}]}` →
`{"type":"array","prefixItems":[...],"items":{"type":"string"}}`. Response schemas keep the array with
no `items` (test `TestCleanJSONSchema_ResponseArrayMissingItemsUnchanged`).

Call sites: `translator/gemini/claude/gemini_claude_request.go:244`,
`translator/gemini/openai/chat-completions/gemini_openai_request.go:361`,
`translator/antigravity/claude/antigravity_claude_request.go:808`,
`runtime/executor/antigravity_executor_request.go:209/235`, `util/responses_tools.go:308`.
All emit `parametersJsonSchema`; the Antigravity executor later renames it to `parameters`.

## 2. google-gemini/gemini-cli (@ 55b495d6)

No tool-schema sanitizer at HEAD. MCP tool schemas pass through verbatim:
`packages/core/src/tools/mcp-client.ts:1429` sets `parametersJsonSchema: this.toolDef.inputSchema`.
`prefixItems` appears only in `packages/core/src/utils/schemaValidator.test.ts` (draft-2020-12 local
validation via Ajv). The historical `sanitizeParameters` (in `tool-registry.ts`) is gone; forks still
carry it (`ConardLi/easy-llm-cli`) and it only dropped `default` next to `anyOf` and non
`enum`/`date-time` string `format` — no array/`items`/`prefixItems` logic ever.

## 3. Others

- **anomalyco/opencode** (@ f12e14cf) `packages/llm/src/protocols/utils/gemini-tool-schema.ts`:
  array with no `items` gets `items = {}`, and an item schema with no "schema intent" key gets
  `type: "string"`. `prefixItems` counts as schema intent but is NOT converted. The Moonshot
  projection in the sibling `tool-schema.ts` is the only one that does the real tuple conversion:
  `prefixItems` → `items` (single → that schema, many → `anyOf`), dropped if `items` already exists.
- **musistudio/claude-code-router** (@ 5ad5083b): no schema code; transforms live in
  `@musistudio/llms`, `src/utils/gemini.util.ts` `cleanupParameters` (@ ac0fafec) — an allowlist
  (`type, format, title, description, nullable, enum, maxItems, minItems, properties, required,
  minProperties, maxProperties, minLength, maxLength, pattern, example, anyOf, propertyOrdering,
  default, items, minimum, maximum`) that deletes everything else, so **`prefixItems` is silently
  dropped and no `items` is synthesized** — a tuple array becomes a bare `{"type":"array"}`.

## Takeaway for a Gemini bridge

Only one reference (opencode's Moonshot path) derives `items` from `prefixItems`. The pragmatic,
production-proven behavior for Gemini/Antigravity is CLIProxyAPI's: if `type` includes `array` and
`items` is absent, inject `{"type":"string"}` — and, unlike CLIProxyAPI, also drop or fold
`prefixItems` since Gemini's schema dialect does not define it.
