import { describe, expect, it } from "vitest";
import { parseInstanceIdPath, validateScriptRequestBody } from "../src/routing.js";

describe("validateScriptRequestBody", () => {
  it("accepts a body with a non-empty script string", () => {
    const result = validateScriptRequestBody({ script: "export default {};", params: { a: 1 } });
    expect(result).toEqual({
      ok: true,
      value: { script: "export default {};", params: { a: 1 } },
    });
  });

  it("rejects a missing script field", () => {
    const result = validateScriptRequestBody({ params: {} });
    expect(result.ok).toBe(false);
  });

  it("rejects an empty script string", () => {
    const result = validateScriptRequestBody({ script: "" });
    expect(result.ok).toBe(false);
  });

  it("rejects a non-string script field", () => {
    const result = validateScriptRequestBody({ script: 42 });
    expect(result.ok).toBe(false);
  });

  it("rejects a non-object body", () => {
    expect(validateScriptRequestBody(null).ok).toBe(false);
    expect(validateScriptRequestBody("script").ok).toBe(false);
    expect(validateScriptRequestBody(42).ok).toBe(false);
  });
});

describe("parseInstanceIdPath", () => {
  it("extracts the id segment from /instances/:id", () => {
    expect(parseInstanceIdPath("/instances/abc-123")).toBe("abc-123");
  });

  it("returns null for paths without an id segment", () => {
    expect(parseInstanceIdPath("/instances/")).toBeNull();
    expect(parseInstanceIdPath("/instances")).toBeNull();
  });

  it("returns null for a nested extra segment", () => {
    expect(parseInstanceIdPath("/instances/abc/extra")).toBeNull();
  });

  it("returns null for unrelated paths", () => {
    expect(parseInstanceIdPath("/scripts")).toBeNull();
  });
});
