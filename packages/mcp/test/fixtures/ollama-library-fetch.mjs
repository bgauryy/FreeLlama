// Test-only transport: exercise the built MCP path with deterministic Ollama HTML fixtures.
const originalFetch = globalThis.fetch;
globalThis.fetch = (input, init) => {
  const url = new URL(typeof input === "string" || input instanceof URL ? input : input.url);
  if (url.origin === "https://ollama.com") {
    return originalFetch(`${process.env.TEST_OLLAMA_LIBRARY_ORIGIN}${url.pathname}${url.search}`, init);
  }
  return originalFetch(input, init);
};
