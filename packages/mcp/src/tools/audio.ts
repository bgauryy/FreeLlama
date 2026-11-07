// audio: speech to text and text to speech on local models, through serve's Ollama passthrough
// (`/v1/audio/*`), which applies the same admission and memory checks as other raw generation.
import type { McpServer } from "@modelcontextprotocol/sdk/server/mcp.js";
import { writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import path from "node:path";
import * as z from "zod";
import { DEFAULT_SERVE_ENDPOINT } from "../config.js";
import { checkAudioCapability } from "../audio-capability.js";
import { endpointParam, errorResult, refusalLine, structuredResult } from "../helpers.js";
import { ensureServe } from "../serve.js";
import { serveAuthHeaders } from "../serve-http.js";
import { MAX_AUDIO_FILE_BYTES, isWav, readAllowedFile } from "../task-files.js";

/** Ollama's speech output: 16-bit mono PCM at 24 kHz, after a 44-byte WAV header. */
const WAV_BYTES_PER_SECOND = 24_000 * 2;

const audioResultSchema = z.object({ text: z.string().optional(), file: z.string().optional(), seconds: z.number().optional() }).loose();

export function registerAudioTool(server: McpServer): void {
  server.registerTool(
    "audio",
    {
      description:
        "Use when: a WAV recording must become text (transcribe; translate = into another language), or text must become speech for the user (speak). " +
        "The server reads `file` by path. Needs a model with audio input, or a speech model for speak. " +
        "Do not use when: a yes/no about the audio is enough (run_task decision with the .wav in files). " +
        "Returns: the text, or the path of the WAV written.",
      inputSchema: z.object({
        action: z.enum(["transcribe", "translate", "speak"]),
        model: z.string().min(1).describe("exact tag"),
        file: z.string().min(1).optional().describe("WAV path (transcribe, translate)"),
        text: z.string().min(1).max(4_096).optional().describe("speak"),
        voice: z.string().min(1).optional().describe("speak; omit for the default"),
        language: z.string().min(1).optional().describe("spoken language, ISO 639-1"),
        toLanguage: z.string().min(1).optional().describe("translate; default en"),
        prompt: z.string().min(1).optional().describe("context words"),
        outputFile: z.string().min(1).optional().describe("speak; default temp dir"),
        endpoint: endpointParam,
      }).strict(),
      outputSchema: audioResultSchema,
      annotations: { destructiveHint: false },
    },
    async ({ action, model, file, text, voice, language, toLanguage, prompt, outputFile, endpoint }, extra) => {
      try {
        const base = (endpoint ?? DEFAULT_SERVE_ENDPOINT).replace(/\/$/, "");
        const misplaced = action === "speak"
          ? Object.entries({ file, language, toLanguage, prompt }).filter(([, value]) => value !== undefined)
          : Object.entries({ text, voice, outputFile, ...(action === "transcribe" ? { toLanguage } : {}) }).filter(([, value]) => value !== undefined);
        if (misplaced.length > 0) {
          return errorResult(new Error(`${misplaced.map(([name]) => `\`${name}\``).join(", ")} ${misplaced.length === 1 ? "does" : "do"} not apply to action "${action}".`));
        }
        await ensureServe(endpoint, { signal: extra.signal });
        const headers = await serveAuthHeaders();
        const started = performance.now();
        if (action === "speak") {
          if (text === undefined) return errorResult(new Error('action "speak" requires `text`.'));
          await checkAudioCapability(base, model, action, headers, extra.signal);
          const response = await fetch(`${base}/v1/audio/speech`, {
            method: "POST",
            headers: { "content-type": "application/json", ...headers },
            body: JSON.stringify({ model, input: text, ...(voice ? { voice } : {}), response_format: "wav" }),
            signal: extra.signal,
          });
          if (!response.ok) return errorResult(new Error(`speak with ${model} failed: ${refusalLine(await response.text(), response.status)}`));
          const audio = Buffer.from(await response.arrayBuffer());
          const target = path.resolve(outputFile ?? path.join(tmpdir(), `freellama-speech-${Date.now()}.wav`));
          writeFileSync(target, audio);
          const seconds = Math.round((Math.max(0, audio.length - 44) / WAV_BYTES_PER_SECOND) * 10) / 10;
          return structuredResult(
            { file: target, bytes: audio.length, seconds, model, elapsed_ms: Math.round(performance.now() - started) },
            { text: `[${model}] wrote ${seconds} s of speech to ${target}` },
          );
        }
        if (file === undefined) return errorResult(new Error(`action "${action}" requires \`file\` (a WAV path).`));
        const bytes = readAllowedFile(file, MAX_AUDIO_FILE_BYTES);
        if (!isWav(bytes)) throw new Error(`audio file ${file} must contain a WAV RIFF/WAVE header`);
        await checkAudioCapability(base, model, action, headers, extra.signal);
        const form = new FormData();
        form.append("file", new Blob([new Uint8Array(bytes)], { type: "audio/wav" }), path.basename(file));
        form.append("model", model);
        form.append("response_format", "json");
        if (language) form.append("language", language);
        if (prompt) form.append("prompt", prompt);
        if (action === "translate" && toLanguage) form.append("to_language", toLanguage);
        const route = action === "translate" ? "translations" : "transcriptions";
        const response = await fetch(`${base}/v1/audio/${route}`, { method: "POST", headers, body: form, signal: extra.signal });
        const body = await response.text();
        if (!response.ok) return errorResult(new Error(`${action} with ${model} failed: ${refusalLine(body, response.status)}`));
        const parsed = JSON.parse(body) as { text?: string };
        // The text is the answer, so it is the text content too: many clients show only that.
        return structuredResult(
          { text: parsed.text ?? "", model, file, elapsed_ms: Math.round(performance.now() - started) },
          { text: `[${model}] ${parsed.text ?? ""}`.trimEnd() },
        );
      } catch (error) {
        return errorResult(error);
      }
    },
  );
}
