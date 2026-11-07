/** Refuse a known incompatible exact model without submitting an audio inference request.
 * Missing metadata and bounded lookup failures leave the raw endpoint authoritative. */
export async function checkAudioCapability(
  endpoint: string,
  model: string,
  action: "transcribe" | "translate" | "speak",
  headers: Record<string, string>,
  signal?: AbortSignal,
): Promise<void> {
  let capabilities: unknown;
  try {
    const timeout = AbortSignal.timeout(1_500);
    const response = await fetch(`${endpoint}/api/show`, {
      method: "POST",
      headers: { "content-type": "application/json", ...headers },
      body: JSON.stringify({ model, verbose: false }),
      signal: signal ? AbortSignal.any([signal, timeout]) : timeout,
    });
    if (!response.ok) return;
    const metadata: unknown = await response.json();
    if (metadata && typeof metadata === "object" && "capabilities" in metadata) {
      capabilities = metadata.capabilities;
    }
  } catch (error) {
    if (signal?.aborted) throw error;
    return;
  }
  if (!Array.isArray(capabilities) || !capabilities.every((value) => typeof value === "string")) return;
  const required = action === "speak" ? "speech" : "audio";
  if (!capabilities.includes(required)) {
    throw new Error(`model ${model} does not report the ${required} capability required for ${action}; select an installed compatible exact tag`);
  }
}
