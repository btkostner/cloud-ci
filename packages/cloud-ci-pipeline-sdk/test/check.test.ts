import { describe, expect, it } from "vitest";
import { CheckRegistry } from "../src/check.js";
import {
  CheckAlreadySealedError,
  CheckSealedError,
  DuplicateCheckNameError,
} from "../src/errors.js";

describe("CheckRegistry / Check state machine", () => {
  it("starts unsealed with zero members", () => {
    const registry = new CheckRegistry();
    const check = registry.create("ci/build", { required: true });
    expect(check.sealed).toBe(false);
    expect(check.memberCount).toBe(0);
    expect(check.name).toBe("ci/build");
    expect(check.required).toBe(true);
  });

  it("tracks member count as distinct node ids attach", () => {
    const registry = new CheckRegistry();
    const check = registry.create("ci/test", { required: true });
    check.attach("web#test");
    check.attach("api#test");
    expect(check.memberCount).toBe(2);
  });

  it("is idempotent: attaching the same node id twice does not double-count", () => {
    const registry = new CheckRegistry();
    const check = registry.create("ci/test", { required: true });
    check.attach("web#test");
    check.attach("web#test");
    expect(check.memberCount).toBe(1);
  });

  it("transitions unsealed -> sealed on seal()", () => {
    const registry = new CheckRegistry();
    const check = registry.create("ci/build", { required: true });
    check.seal();
    expect(check.sealed).toBe(true);
  });

  it("rejects attaching a node to an already-sealed check", () => {
    const registry = new CheckRegistry();
    const check = registry.create("ci/build", { required: true });
    check.seal();
    expect(() => check.attach("web#build")).toThrow(CheckSealedError);
  });

  it("fixes the member set once sealed: members attached before sealing are preserved", () => {
    const registry = new CheckRegistry();
    const check = registry.create("ci/build", { required: true });
    check.attach("web#build");
    check.seal();
    expect(check.memberCount).toBe(1);
  });

  it("rejects sealing an already-sealed check", () => {
    const registry = new CheckRegistry();
    const check = registry.create("ci/build", { required: true });
    check.seal();
    expect(() => check.seal()).toThrow(CheckAlreadySealedError);
  });

  it("accepts an explicit conclusion/summary on seal()", () => {
    const registry = new CheckRegistry();
    const check = registry.create("ci/docs-lint", { required: true });
    check.seal({ conclusion: "skipped", summary: "no markdown files changed" });
    expect(check.sealed).toBe(true);
  });

  it("rejects creating two checks with the same name in one run", () => {
    const registry = new CheckRegistry();
    registry.create("ci/build", { required: true });
    expect(() => registry.create("ci/build", { required: false })).toThrow(DuplicateCheckNameError);
  });

  it("sealRemaining() seals every check the script did not seal itself, leaving explicitly-sealed checks untouched", () => {
    const registry = new CheckRegistry();
    const autoSealed = registry.create("ci/test", { required: true });
    const explicitlySealed = registry.create("ci/docs-lint", { required: true });
    explicitlySealed.seal({ conclusion: "skipped" });

    expect(autoSealed.sealed).toBe(false);
    registry.sealRemaining();
    expect(autoSealed.sealed).toBe(true);
    expect(explicitlySealed.sealed).toBe(true);
  });

  it("sealRemaining() does not throw for an already-sealed check (no double-seal attempt)", () => {
    const registry = new CheckRegistry();
    const check = registry.create("ci/docs-lint", { required: true });
    check.seal({ conclusion: "skipped" });
    expect(() => registry.sealRemaining()).not.toThrow();
  });

  it("a sealed check with no attached members stays sealed with zero members (coordinator decides its conclusion)", () => {
    const registry = new CheckRegistry();
    const check = registry.create("ci/docs-lint", { required: true });
    registry.sealRemaining();
    expect(check.sealed).toBe(true);
    expect(check.memberCount).toBe(0);
  });
});
