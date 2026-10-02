import { describe, expect, it } from "vitest";
import { bytes, duration, percent } from "../src/client/format";

describe("telemetry presentation", () => {
  it("keeps unknown telemetry separate from measured zero", () => {
    expect(bytes(null)).toBe("Unknown");
    expect(bytes(undefined)).toBe("Unknown");
    expect(bytes(0)).toBe("0 B");
    expect(duration(null)).toBe("Unknown");
    expect(duration(0)).toBe("0 ms");
    expect(percent(null, 48)).toBeNull();
    expect(percent(0, 48)).toBe(0);
    expect(percent(0, 0)).toBeNull();
  });
  it("clamps bars while preserving the displayed observations", () => {
    expect(percent(50, 48)).toBe(100);
    expect(percent(-1, 48)).toBe(0);
    expect(bytes(1024 ** 3)).toBe("1 GiB");
    expect(duration(120_000)).toBe("2.0 min");
  });
});
