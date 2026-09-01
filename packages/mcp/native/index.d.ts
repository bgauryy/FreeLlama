// Hand-written type declarations for the native module (see index.js for why these are
// hand-written rather than tool-generated). Mirrors packages/rust-core/src/napi.rs's public function signatures
// exactly — keep the two in sync if you change one. `npm run build` no longer overwrites this
// file (it previously did, silently emptying it — see package.json's build script comment).

/**
 * Runs `freellama doctor` against Ollama directly — no running `freellama serve` required.
 * Cross-checks the Ollama CLI and server versions and confirms the endpoint is reachable.
 * @param endpoint Ollama endpoint, defaults to http://127.0.0.1:11434
 * @returns Pretty-printed JSON string
 */
export function doctor(endpoint?: string | undefined | null): Promise<string>;

/**
 * Machine profile (chip, host memory, memory kind, CPU count, and disk) from `freellama serve`.
 * @param endpoint FreeLlama serve endpoint, defaults to http://127.0.0.1:11435
 */
export function machine(endpoint?: string | undefined | null): Promise<string>;

/**
 * Installed-model inventory with capabilities, residency, and advertised context.
 * @param endpoint FreeLlama serve endpoint, defaults to http://127.0.0.1:11435
 */
export function listModels(endpoint?: string | undefined | null): Promise<string>;

/**
 * Deterministic model selection for a task.
 * @param endpoint FreeLlama serve endpoint, defaults to http://127.0.0.1:11435
 * @param task e.g. "completion" | "code_repair" | "vision" | "embedding"
 * @param objective "fastest" | "balanced" | "quality", defaults to "balanced"
 * @param model Optional explicit model name to force
 * @param sessionId Optional session id for model affinity
 * @param contextTokens Optional minimum context window required
 * @param requiredCapabilities Optional list of capabilities the model must advertise (e.g. ["vision"])
 * @param minConfidence Optional floor ("low" | "medium"); the core gate refuses below it
 * @param executionPreference Guarded backend hint: "auto" | "prefer_cpu" | "prefer_gpu"
 * @param minPlacementEvidence "configured" (default) or fail-closed physical "observed"
 * @param minPlacementEvidence "configured" (default) or fail-closed physical "observed"
 */
export function route(
  endpoint: string | undefined | null,
  task: string,
  objective?: string | undefined | null,
  model?: string | undefined | null,
  sessionId?: string | undefined | null,
  contextTokens?: number | undefined | null,
  requiredCapabilities?: Array<string> | undefined | null,
  minConfidence?: string | undefined | null,
  executionPreference?: string | undefined | null,
  minPlacementEvidence?: string | undefined | null,
): Promise<string>;

/**
 * Side-effect-free install recommendation for a task. Never runs `ollama pull` itself.
 * @param endpoint FreeLlama serve endpoint, defaults to http://127.0.0.1:11435
 * @param task e.g. "completion" | "code_repair" | "vision" | "embedding"
 * @param objective "fastest" | "balanced" | "quality", defaults to "balanced"
 * @param model Optional explicit model name to filter to
 * @param contextTokens Optional minimum context window required
 * @param requiredCapabilities Optional list of capabilities the model must advertise (e.g. ["vision"])
 * @param executionPreference Guarded backend hint: "auto" | "prefer_cpu" | "prefer_gpu"
 * @param minPlacementEvidence "configured" (default) or fail-closed physical "observed"
 */
export function recommend(
  endpoint: string | undefined | null,
  task: string,
  objective?: string | undefined | null,
  model?: string | undefined | null,
  contextTokens?: number | undefined | null,
  requiredCapabilities?: Array<string> | undefined | null,
  executionPreference?: string | undefined | null,
  minPlacementEvidence?: string | undefined | null,
): Promise<string>;

/**
 * Routes AND executes a chat/generate/embed call in one shot — unlike `route`/`recommend`, this
 * actually does work. Provide `prompt` for a single message, or `messages` (an array of
 * `{role, content}` objects) for multi-turn history. For task "embedding", set `input` instead.
 * @param endpoint FreeLlama serve endpoint, defaults to http://127.0.0.1:11435
 * @param task e.g. "completion" | "code_repair" | "vision" | "embedding"
 * @param objective "fastest" | "balanced" | "quality", defaults to "balanced"
 * @param model Optional explicit model name to force
 * @param sessionId Optional session id for model affinity
 * @param contextTokens Optional minimum context window required
 * @param requiredCapabilities Optional list of capabilities the model must advertise
 * @param prompt Single-turn user message
 * @param images Base64-encoded images (no data-URI prefix) attached to the `prompt` message — pair with requiredCapabilities: ["vision"]
 * @param messages Multi-turn message history, wins over `prompt` if both are set
 * @param input Embedding input (string or array of strings), for task "embedding"
 * @param tools Optional tool/function definitions for function-calling tasks
 * @param keepAlive Overrides Ollama's model residency window, e.g. "0" or "-1"
 * @param minConfidence Optional floor ("low" | "medium"); the core gate refuses before generation
 * @param executionPreference Guarded backend hint: "auto" | "prefer_cpu" | "prefer_gpu"
 */
export function runTask(
  endpoint: string | undefined | null,
  task: string,
  objective?: string | undefined | null,
  model?: string | undefined | null,
  sessionId?: string | undefined | null,
  contextTokens?: number | undefined | null,
  requiredCapabilities?: Array<string> | undefined | null,
  prompt?: string | undefined | null,
  images?: Array<string> | undefined | null,
  messages?: unknown | undefined | null,
  input?: unknown | undefined | null,
  tools?: unknown | undefined | null,
  keepAlive?: string | undefined | null,
  minConfidence?: string | undefined | null,
  executionPreference?: string | undefined | null,
  minPlacementEvidence?: string | undefined | null,
): Promise<string>;

/**
 * Object-based managed task API. Supports the full typed task request including lossless message
 * history and request_options (format, think, runtime options, logprobs).
 * @param endpoint FreeLlama serve endpoint, defaults to http://127.0.0.1:11435
 * @param request Complete `/_freellama/v1/tasks` request object
 */
export function runTaskRequest(
  endpoint: string | undefined | null,
  request: unknown,
): Promise<string>;

/**
 * Converts a free-text natural-language intent into a route.
 * @param endpoint FreeLlama serve endpoint, defaults to http://127.0.0.1:11435
 * @param text Free-text description of what's needed
 * @param sessionId Optional session id for model affinity
 */
export function naturalRoute(
  endpoint: string | undefined | null,
  text: string,
  sessionId?: string | undefined | null,
): Promise<string>;
