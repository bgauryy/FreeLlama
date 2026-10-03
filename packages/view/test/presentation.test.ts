import { createElement } from "react";
import { renderToStaticMarkup } from "react-dom/server";
import { describe, expect, it } from "vitest";
import { Meter } from "../src/client/components";
import { LoadedModels } from "../src/client/Models";
import { statusSchema } from "../src/shared/contracts";
import fixture from "./fixtures/control-api.json";

describe("observation rendering", () => {
  it("does not announce unknown memory as zero utilization", () => {
    const html = renderToStaticMarkup(
      createElement(Meter, { used: null, total: null, label: "GPU memory" }),
    );
    expect(html).toContain("GPU memory: Unknown");
    expect(html).not.toContain('role="meter"');
    expect(html).not.toContain("aria-valuenow");
  });

  it("distinguishes unavailable residency from an observed empty runner list", () => {
    const unavailable = renderToStaticMarkup(
      createElement(LoadedModels, {
        source: {
          state: "unavailable",
          data: null,
          updated_at: null,
          error: "offline",
        },
      }),
    );
    expect(unavailable).toContain("Residency is unknown");
    expect(unavailable).not.toContain("No loaded models");
    const data = statusSchema.parse(fixture.status);
    data.loaded_models = [];
    const empty = renderToStaticMarkup(
      createElement(LoadedModels, {
        source: {
          state: "live",
          data,
          updated_at: new Date().toISOString(),
          error: null,
        },
      }),
    );
    expect(empty).toContain("No loaded models");
  });

  it("renders placement from reported runner bytes and escapes upstream model names", () => {
    const data = statusSchema.parse(fixture.status);
    const runner = data.loaded_models[0];
    if (runner && "name" in runner) runner.name = "<script>untrusted</script>";
    const html = renderToStaticMarkup(
      createElement(LoadedModels, {
        source: {
          state: "live",
          data,
          updated_at: new Date().toISOString(),
          error: null,
        },
      }),
    );
    expect(html).toContain("&lt;script&gt;untrusted&lt;/script&gt;");
    expect(html).not.toContain("<script>");
    expect(html).toContain("0 B");
    expect(html).toContain("GPU memory share");
  });
});
