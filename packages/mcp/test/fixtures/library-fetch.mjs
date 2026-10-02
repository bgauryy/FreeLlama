// Replace only the external catalog HTTP boundary; MCP transport, parsers, and local lookups run normally.
const originalFetch = globalThis.fetch;
const pages = {
  "/search": `<li class="result"><a href="/library/qwen3-vl"><p class="max-w-lg">Vision model</p>
    <span class="text-indigo-600">vision</span><span class="text-cyan-500">cloud</span></a></li>`,
  "/library/qwen3-vl": `<meta name="description" content="Vision and text models.">
    <span class="text-indigo-600">vision</span><textarea id="editor">Supports image understanding.</textarea>`,
  "/library/qwen3-vl/tags": `<a href="/library/qwen3-vl:4b">qwen3-vl:4b
    <span>abcdef123456</span> • 3GB • 32K context window • Text, Image input • today</a>
    <a href="/library/qwen3-vl:235b">qwen3-vl:235b • 150GB • 32K context window • Text, Image input • today</a>`,
};
globalThis.fetch = async (input, init) => {
  const url = new URL(input instanceof Request ? input.url : String(input));
  if (url.origin !== "https://ollama.com") return originalFetch(input, init);
  const page = pages[url.pathname];
  if (page === undefined) throw new Error(`Unexpected library fixture URL: ${url}`);
  return new Response(page, { headers: { "Content-Type": "text/html; charset=utf-8" } });
};
