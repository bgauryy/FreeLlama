import { describe, expect, it } from "vitest";
import { tablesToMarkdown } from "../../src/table-markdown.js";

describe("read-table output as Markdown", () => {
  it("turns MiniCPM-OCR's OTSL into a Markdown table, merged cells empty", () => {
    // Recorded from hf.co/openbmb/MiniCPM-OCR-1.0-gguf:Q4_K_M on a rendered receipt, 2026-11-12.
    const otsl = "<otsl><fcel>ACME HARDWARE<lcel><lcel><nl><fcel>Item<fcel>Qty<fcel>Price<nl><fcel>Nails (box)<fcel>2<fcel>7.50<nl><fcel>TOTAL: 52.74 USD<lcel><lcel><nl></otsl>";
    expect(tablesToMarkdown(otsl)).toBe([
      "| ACME HARDWARE |  |  |",
      "| --- | --- | --- |",
      "| Item | Qty | Price |",
      "| Nails (box) | 2 | 7.50 |",
      "| TOTAL: 52.74 USD |  |  |",
    ].join("\n"));
  });

  it("converts unwrapped OTSL inside other text, and an HTML table with colspan and entities", () => {
    expect(tablesToMarkdown("[m] <fcel>a<fcel>b|c<nl><ecel><fcel>2<nl>")).toBe("[m] | a | b\\|c |\n| --- | --- |\n|  | 2 |");
    const html = '<table class="t"><thead><tr><th>Item</th><th>Price</th></tr></thead><tbody><tr><td colspan="2">Tools &amp; <b>parts</b></td></tr></tbody></table>';
    expect(tablesToMarkdown(`Total below\n${html}`)).toBe("Total below\n| Item | Price |\n| --- | --- |\n| Tools & parts |  |");
  });

  it("leaves text without tables alone", () => {
    expect(tablesToMarkdown("| a | b |\n| --- | --- |")).toBe("| a | b |\n| --- | --- |");
    expect(tablesToMarkdown("no <fcel> row end")).toBe("no <fcel> row end");
  });
});
