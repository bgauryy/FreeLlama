// Every instruction FreeLlama gives a local model, in one file. Per job: the prompt, system prompt,
// options, think setting and embedding prefixes that `run_task {job}` fills in when the caller
// leaves them out (job_profile.applied lists which), plus the research adapters' system prompts
// (research/prompts.ts re-exports them; their bytes are calibration identity, so never edit them in
// passing). A per-model value is one its publisher documents, cited in `sources`: never add a
// sampling value without one. What the calling agent reads (the MCP prompts) is at the end.
import catalogData from "../../rust-core/src/model_catalog.json" with { type: "json" };
import { OCTOCODE_GITHUB_INSTRUCTIONS, OCTOCODE_LOCAL_INSTRUCTIONS } from "./research/octocode-instructions.js";

type Options = Record<string, number>;

/** What one model's publisher documents, applied only when that model runs the job. */
export type ModelProfile = { systemPrompt?: string; options?: Options; think?: boolean; notes?: string; sources: string[] };

/** Embedding prefixes of one model family: `query` before `nearest.query`, `document` before each text item. */
export type Prefixes = { query?: string; document?: string };

/** The jobs `run_task {job}` runs. */
export const RUN_TASK_JOBS = ["classify", "find-code", "duplicates", "ocr", "image-recognition", "image-search", "coding", "summarize"] as const;
export type JobName = (typeof RUN_TASK_JOBS)[number];
export type PromptInput = { question?: string; kind?: string };

export type LocalJob = {
  job: JobName;
  task: "decision" | "embedding" | "vision" | "coding" | "completion";
  /** The catalog job whose picks run this one. */
  catalogJob: string;
  /** That catalog job's tags in catalog order: job mode runs the first one installed that fits now. */
  defaultPicks: string[];
  /** The user prompt when the caller sends none. Without it the caller must send what `needs` names. */
  prompt?: (input: PromptInput) => string;
  needs?: string;
  systemPrompt?: string;
  options?: Options;
  think?: boolean;
  /** Embedding jobs, by model family (the start of the tag). */
  prefixes?: Record<string, Prefixes>;
  /** Documented per-model values, by catalog tag. */
  models?: Record<string, ModelProfile>;
  notes: string;
  sources: string[];
};

const catalog = catalogData as { jobs: Array<{ job: string; picks: Array<{ tag: string }> }> };
function picksOf(job: string): string[] {
  const found = catalog.jobs.find((entry) => entry.job === job);
  if (!found) throw new Error(`model_catalog.json has no job ${job}`);
  return found.picks.map((pick) => pick.tag);
}

const cookbook = (name: string) => `docs/cookbooks/${name}.md`;
// From each model's card; scratch research of 2026-11-08 quoted the evidence for every value used.
const SOURCES = {
  miniCpmOcr: ["https://huggingface.co/openbmb/MiniCPM-OCR-1.0", "https://huggingface.co/openbmb/MiniCPM-OCR-1.0-gguf"],
  glmOcr: ["https://ollama.com/library/glm-ocr", "https://huggingface.co/zai-org/GLM-OCR"],
  lfm3Vl: ["https://huggingface.co/LiquidAI/LFM3-VL-2B", "https://huggingface.co/LiquidAI/LFM3-VL-2B-GGUF", "https://ollama.com/library/lfm3-vl"],
  lfm3Omni: ["https://huggingface.co/LiquidAI/LFM3-Omni-2B", "https://ollama.com/library/lfm3-omni"],
  embeddingGemma: ["https://huggingface.co/google/embeddinggemma-2", "https://ai.google.dev/gemma/docs/embeddinggemma/inference-embeddinggemma-with-sentence-transformers"],
  nomic: ["https://huggingface.co/nomic-ai/nomic-embed-text-v1.5"],
  strandsDecider: ["https://ollama.com/library/strands-decider", "https://raw.githubusercontent.com/strands-labs/strands-decider/main/README.md"],
  nimble: ["https://ollama.com/library/nimble-v3:latest"],
  bonsai9b: ["https://huggingface.co/prism-ml/Ternary-Bonsai-2-9B-gguf", "https://ollama.com/library/bonsai-2:9b"],
  bonsai27b: ["https://huggingface.co/prism-ml/Ternary-Bonsai-2-27B-gguf", "https://ollama.com/library/bonsai-2"],
  lfm3: ["https://huggingface.co/LiquidAI/LFM3-1.7B", "https://ollama.com/library/lfm3"],
  nemotronBolt: ["https://huggingface.co/nvidia/NVIDIA-Nemotron-3.5-Bolt-E4B-4GB"],
  lfm3Audio: ["https://huggingface.co/LiquidAI/LFM3-Audio-2B", "https://ollama.com/library/lfm3-audio"],
  phonon: ["https://ollama.com/library/phonon:en"],
};

/** Embedding prefixes each family documents. They apply to text only: images and audio go in bare. */
export const EMBEDDING_PREFIXES = {
  embeddinggemma: {
    query: "task: search result | query: ", document: "title: none | text: ", codeQuery: "task: code retrieval | query: ",
    similarity: "task: sentence similarity | query: ", clustering: "task: clustering | query: ", classification: "task: classification | query: ",
  },
  // A prefix is required; the card names clustering: for removing semantic duplicates.
  "nomic-embed-text": { query: "search_query: ", document: "search_document: ", clustering: "clustering: ", classification: "classification: " },
} as const;
const GEMMA = EMBEDDING_PREFIXES.embeddinggemma;
const NOMIC = EMBEDDING_PREFIXES["nomic-embed-text"];

/** The prompts both OCR picks document. MiniCPM-OCR adds "Seal Recognition:" and "Chart to Table:". */
export const OCR_PROMPTS = { text: "Text Recognition:", table: "Table Recognition:", formula: "Formula Recognition:" } as const;
export type OcrKind = keyof typeof OCR_PROMPTS;

const CODING_SYSTEM = `You make small code changes. Reply with the edits only; explain nothing unless the task asks.
Change an existing file with one or more blocks in exactly this form:
path/to/file.ts
<<<<<<< SEARCH
exact current lines (5 to 30, unique in the file, indentation exact)
=======
replacement lines
>>>>>>> REPLACE
Copy SEARCH lines character for character from the file shown; never shorten them. Prefer several small blocks to one large one.
For a new file, or one under 30 lines, write its path on one line, then the whole file in one fenced code block.
If the task asks a question rather than a change, answer it in a few lines.`;

const SUMMARIZE_SYSTEM = `Summarize the text you are given as short bullet points.
Keep the facts the task asks for. Quote numbers, names, versions and paths exactly.
If the text does not say something the task asks, write "not stated"; never guess.
No introduction and no closing remarks.`;

const IMAGE_SYSTEM = 'Answer the question about the image from what it shows. Be brief: the value, the label, or one or two sentences. If the image does not show it, say "not visible".';

/** bonsai-2:9b with thinking off and the card's non-thinking preset: for short bounded jobs. */
const BONSAI_9B: ModelProfile = {
  think: false,
  options: { temperature: 0.7, top_p: 0.8, top_k: 20, min_p: 0, presence_penalty: 1.5, repeat_penalty: 1 },
  notes: "With think:true use the card's thinking preset (temperature 1.0, top_p 0.95, presence_penalty 1.0) and allow at least 8,192 output tokens; never greedy decoding with thinking on.",
  sources: SOURCES.bonsai9b,
};

/** The chat models of the coding picks, as their cards document them. */
const CHAT_MODELS: Record<string, ModelProfile> = {
  "lfm3:1.7b-q4_K_M": {
    options: { temperature: 0.1, top_k: 50, repeat_penalty: 1.1 },
    notes: "Thinks by default; its card calls thinking off inconsistent, so it stays on. Not for agentic coding or multi-step reasoning.",
    sources: SOURCES.lfm3,
  },
  "hf.co/nvidia/NVIDIA-Nemotron-3.5-Bolt-E4B-4GB:Q4_K_M": {
    options: { temperature: 1, top_p: 0.95 },
    notes: "Reasons by default; a tight num_predict cuts the reasoning mid-sentence.",
    sources: SOURCES.nemotronBolt,
  },
  "bonsai-2:9b": BONSAI_9B,
  "bonsai-2:27b": {
    options: { temperature: 1, top_p: 0.95, top_k: 20, min_p: 0.05, presence_penalty: 0, repeat_penalty: 1 },
    notes: "The card's thinking preset; it documents no switch to turn thinking off.",
    sources: SOURCES.bonsai27b,
  },
};

const CHAT_SOURCES = [...SOURCES.lfm3, ...SOURCES.nemotronBolt, ...SOURCES.bonsai9b, ...SOURCES.bonsai27b];

/** Per job: what job mode sends a local model when the caller leaves it out. */
export const LOCAL_JOBS: Record<JobName, LocalJob> = {
  classify: {
    job: "classify", task: "decision", catalogJob: "classify", defaultPicks: picksOf("classify"),
    models: {
      "strands-decider:2b-nvfp4": { notes: "State and questions together must fit 4,096 tokens; weak on long, multi-step documents.", sources: SOURCES.strandsDecider },
      "nimble-v3:9b-q4_K_M": { notes: "Documents num_ctx 32768 (routing sizes it here); CC BY-NC 4.0, noncommercial.", sources: SOURCES.nimble },
    },
    notes: "A decision model reads state and questions, no prompt. Ask short questions about the state, one concern each, and give a choice question an explicit fallback option such as other.",
    sources: [...SOURCES.strandsDecider, ...SOURCES.nimble, cookbook("classify")],
  },
  "find-code": {
    job: "find-code", task: "embedding", catalogJob: "find-code", defaultPicks: picksOf("find-code"),
    needs: "nearest.query: the issue, or what the code does",
    prefixes: { embeddinggemma: { query: GEMMA.codeQuery, document: GEMMA.document }, "nomic-embed-text": { query: NOMIC.query, document: NOMIC.document } },
    // Measured 2026-11-08 on the find-code set (10 questions, 1,039 chunks): code retrieval with
    // title: none 8 first / 9 in top 3; search result 8/8; code retrieval with the file as title 7/9.
    notes: "task: code retrieval (EmbeddingGemma's code-search prefix) measured best here: right file first 8/10, in the top 3 9/10. Read the top ranges before relying on them.",
    sources: [...SOURCES.embeddingGemma, ...SOURCES.nomic, cookbook("find-code")],
  },
  duplicates: {
    job: "duplicates", task: "embedding", catalogJob: "duplicates", defaultPicks: picksOf("duplicates"),
    // Symmetric: every item, the query included, takes the same prefix.
    prefixes: {
      // The measured 8/12 used search_document:; the card's clustering: is documented but unmeasured here.
      "nomic-embed-text": { query: NOMIC.document, document: NOMIC.document },
      embeddinggemma: { query: GEMMA.similarity, document: GEMMA.similarity },
    },
    notes: "Uses the measured search_document: (8/12 real issue pairs); nomic's card also documents clustering: for deduplication, unmeasured here. A high score is a candidate, never a verdict.",
    sources: [...SOURCES.nomic, ...SOURCES.embeddingGemma, cookbook("duplicates")],
  },
  ocr: {
    job: "ocr", task: "vision", catalogJob: "ocr", defaultPicks: picksOf("ocr"),
    prompt: ({ kind }) => OCR_PROMPTS[kind as OcrKind] ?? OCR_PROMPTS.text,
    models: {
      "hf.co/openbmb/MiniCPM-OCR-1.0-gguf:Q4_K_M": { notes: "Chinese and English; no sampling settings documented.", sources: SOURCES.miniCpmOcr },
      "glm-ocr:latest": { notes: "Also documents Figure Recognition:; no sampling settings documented.", sources: SOURCES.glmOcr },
    },
    notes: "Both picks document Text Recognition:, Table Recognition: and Formula Recognition:. Crop to the text first.",
    sources: [...SOURCES.miniCpmOcr, ...SOURCES.glmOcr, cookbook("ocr")],
  },
  "image-recognition": {
    job: "image-recognition", task: "vision", catalogJob: "image-recognition", defaultPicks: picksOf("image-recognition"),
    needs: "prompt: your question about the image",
    systemPrompt: IMAGE_SYSTEM,
    models: {
      "lfm3-vl:2b-q4_K_M": {
        options: { temperature: 0.1, top_p: 1, top_k: 50, min_p: 0, repeat_penalty: 1.1 },
        notes: "Thinks before it answers, with no documented switch; images go in at native resolution, so a large one costs many tokens.",
        sources: SOURCES.lfm3Vl,
      },
      "lfm3-omni:2b-q4_K_M": {
        systemPrompt: "Answer questions about the provided image. Base every answer only on what can be seen.",
        options: { temperature: 0 },
        notes: "Non-reasoning: never send think:true. Up to 2,048 tokens per image.",
        sources: SOURCES.lfm3Omni,
      },
      "bonsai-2:9b": BONSAI_9B,
    },
    notes: "Ask a specific question, not \"describe this\". Tested on drawn images only.",
    sources: [...SOURCES.lfm3Vl, ...SOURCES.lfm3Omni, ...SOURCES.bonsai9b, cookbook("image-recognition")],
  },
  "image-search": {
    job: "image-search", task: "embedding", catalogJob: "image-search", defaultPicks: picksOf("image-search"),
    needs: "nearest.query: a description of the image",
    prefixes: { embeddinggemma: { query: GEMMA.query, document: GEMMA.document } },
    notes: "Images are embedded bare: the prefixes apply to text only. Open the top match.",
    sources: [...SOURCES.embeddingGemma, cookbook("image-search")],
  },
  coding: {
    job: "coding", task: "coding", catalogJob: "coding", defaultPicks: picksOf("coding"),
    needs: "prompt: the change to make",
    systemPrompt: CODING_SYSTEM,
    models: CHAT_MODELS,
    notes: "The answer is SEARCH/REPLACE blocks: apply one only where its SEARCH text matches exactly once, then run the tests.",
    sources: [...CHAT_SOURCES, cookbook("coding")],
  },
  summarize: {
    job: "summarize", task: "completion", catalogJob: "coding", defaultPicks: picksOf("coding"),
    prompt: ({ question }) => question ?? "Summarize the text below.",
    systemPrompt: SUMMARIZE_SYSTEM,
    models: CHAT_MODELS,
    notes: "The coding picks: the same chat models apply (docs/cookbooks/batch-text.md). Check every number and name you reuse.",
    sources: [...CHAT_SOURCES, cookbook("batch-text")],
  },
};

/** Jobs other tools run: what their local model is told, and the cards' notes. */
export const TOOL_JOBS = {
  "speech-to-text": {
    tool: "audio", defaultPicks: picksOf("speech-to-text"),
    // The audio tool calls Ollama's transcription endpoint, which builds the model's prompt; these
    // are lfm3-audio's documented prompts for a direct chat call.
    systemPrompts: {
      transcribe: (language?: string) => (language ? `Perform ASR in ${language}.` : "Perform ASR."),
      translate: (source: string, target: string) => `Translate the following speech from ${source} to ${target} in text form.`,
    },
    notes: "WAV input only. Numbers and names are the usual misses; translation works best to or from English.",
    sources: [...SOURCES.lfm3Audio, cookbook("speech-to-text")],
  },
  "text-to-speech": {
    tool: "audio", defaultPicks: picksOf("text-to-speech"),
    notes: "Send plain text: Markdown marks may be read aloud. One voice (default); 24 kHz mono WAV.",
    sources: [...SOURCES.phonon, cookbook("text-to-speech")],
  },
  "research-agent": {
    tool: "delegate_research", defaultPicks: picksOf("research-agent"),
    notes: "The adapters' system prompts are below. Discard an escalate verdict and verify the citations.",
    sources: [cookbook("research-agent")],
  },
  sweep: {
    tool: "sweep", defaultPicks: picksOf("classify"),
    /** The decision state of one chunk, and the explain step's user message for a confirmed flag. */
    state: (label: string, text: string) => `Source: ${label}\n\n${text}`,
    explain: (prompt: string, label: string, text: string) => `${prompt}\n\nSource: ${label}\n\n${text}`,
    // FreeLlama's own setting for the short explain note, not a model's documented value.
    explainControls: { think: false, temperature: 0 },
    notes: "One yes/no question per concern, phrased about one chunk (\"Does this code ...\"); a noul answer flags at p >= 0.5.",
    sources: [cookbook("sweep")],
  },
};

// ---- Research adapters. Exact bytes: research-identity.test.ts pins their sha256. ----

/** Instructions for the restricted-shell adapter. */
export const BASH_SYSTEM_PROMPT = "You answer questions about the code in this read-only workspace (the current directory) by looking, not guessing. Reply with exactly one JSON object per turn and nothing else.\n\nActions, in order of preference:\n{\"action\":\"search\",\"pattern\":\"EMBEDDINGS_PER_JOB\"} finds text in files and prints path:line:text. Literal and case-sensitive; add \"regex\":true for a|b or \\s, \"ignoreCase\":true, \"glob\":\"*.rs\", or \"path\":\"packages/mcp\" to narrow it.\n{\"action\":\"read\",\"path\":\"packages/mcp/src/tools/run-task.ts\",\"start\":310,\"end\":340} shows numbered lines of one file.\n{\"action\":\"list\",\"path\":\"packages\"} shows files and folders two levels deep.\n{\"action\":\"shell\",\"command\":\"rg -n -w build_decision_request -g '*.rs'\"} runs one read-only pipeline when the actions above cannot. Use rg (rg -n PATTERN PATH; -w whole word, -l file names only, -t rust or -t ts, -g '!tests/') or grep -rn PATTERN PATH --exclude-dir=node_modules, plus find, head, sed -n '10,40p' FILE, wc and git log/show/grep/blame. No network, redirection, $(...), variables or loops.\n{\"action\":\"page\",\"step\":2,\"page\":2} shows the next page of a long earlier result.\n{\"action\":\"finish\",\"answer\":\"32. Source: packages/mcp/src/tools/run-task.ts:322\"} ends the task.\n\nHow to research:\n1. Search for the exact name in the question (a function, constant, type, flag or error text). A path in the question may be partial; search finds the real one.\n2. Read the lines around the best hit to confirm it.\n3. Finish once the answer is confirmed, usually after 2 to 4 actions. Never repeat an action: its result is already above.\nKeep every result small: search before reading, read 30 to 80 lines around a hit rather than a whole file, use rg -l or rg -c to see where matches are before printing them, and end long shell output with | head -n 40 or | tail -n 40.\nFor \"which files call X\", search \"X(\" and list every file except the one that defines X. For \"is there an X\", a search with no matches over the whole workspace means no.\n\nAnswer format: the direct answer first (the value, name, list or yes/no the question asks for), then \"Source: path:line\" for each file it comes from, copied from the tool output. Answer \"not found\" only after a search of the whole workspace returned no matches.";

/** How a research agent calls tools: one JSON action per turn, in place of a shell command. */
const ACTION_PROTOCOL = (example: string) => `You do not run a shell: ignore the "Call: octocode ..." form, the exit codes and the login hint above. Put "cursor" inside a query to read more of a tool's result; use the page action only for stored output of your own earlier step.
Return exactly one JSON object per turn, and nothing else:
Run a tool, in place of the shell command above: {"action":"octocode","tool":"${example}","queries":[{...}]}
Read another page of an earlier step's stored output: {"action":"page","step":2,"page":2}
Finish: {"action":"finish","answer":"concise answer with its evidence"}
You see page 1 of each result and the total; a shortened observation is recoverable with page, so never re-run a tool for it.
Most questions need 2 to 6 calls. An invalid action is rejected and costs a turn. Never edit or write anything.`;

/** Instructions for the local octocode adapter, rooted at `workspace`. */
export function octocodeSystemPrompt(workspace: string): string {
  return `You are a local code-research agent in a read-only workspace at ${workspace}. Paths are relative to it.\n\n${OCTOCODE_LOCAL_INSTRUCTIONS}\n${ACTION_PROTOCOL("localSearch")}`;
}

/** The GitHub CLI as a second tool: a model knows its syntax, and some lookups are one command. */
const GH_ACTION = `You may also run the GitHub CLI, read-only, as {"action":"gh","args":[...]} (arguments as an array; no shell):
{"action":"gh","args":["search","code","SystemOneRequest","--repo","ollama/ollama"]} finds code; add "--filename","types.go" or "--language","go" to narrow it.
{"action":"gh","args":["api","repos/ollama/ollama/contents/api/types.go","--jq",".content"]} reads a file (base64), or use ghGetFileContent for decoded text.
{"action":"gh","args":["repo","view","colinhacks/zod","--json","name,description,primaryLanguage"]}, ["pr","view","18606","--repo","ollama/ollama"], ["issue","list","--repo","owner/repo","--search","keyword"], ["release","list","--repo","owner/repo"].
Only search, repo view/list, pr view/list/diff/checks, issue view/list, release view/list and api (GET) run. Finish with the direct answer first, then "Source: owner/repo/path" or the PR or issue URL.`;

/** Instructions for the coder adapter: it edits an isolated copy and runs only allowed commands. */
export const CODER_SYSTEM_PROMPT = [
  "You change code in an isolated copy of the user's project (the current directory) and test the change. The user's own files are never touched: they review your diff afterwards. Reply with exactly one JSON object per turn and nothing else.",
  "",
  "Actions, in order of preference:",
  '{"action":"search","pattern":"parsePort"} finds text in files and prints path:line:text. Literal and case-sensitive; add "regex":true, "ignoreCase":true, "glob":"*.ts" or "path":"src" to narrow it.',
  '{"action":"read","path":"src/config.ts","start":40,"end":70} shows numbered lines of one file.',
  '{"action":"edit","path":"src/config.ts","search":"  const port = 8080;\\n  return port;","replace":"  const port = Number(env.PORT ?? 8080);\\n  return port;"} replaces text that occurs exactly once in the file. Copy "search" from the read output without the line numbers and with its exact indentation: 5 to 30 lines, enough to be unique.',
  '{"action":"write","path":"src/limits.ts","content":"export const MAX = 10;\\n"} creates a file, or replaces a whole small file.',
  '{"action":"run","command":"yarn test"} runs one of the allowed commands listed with the task, exactly as written there, and shows its exit code and output.',
  '{"action":"list","path":"src"} shows files and folders two levels deep.',
  '{"action":"shell","command":"git diff --stat"} runs one read-only pipeline (rg, grep, find, head, sed -n, wc, git log/diff/show). It cannot change files; edit and write do.',
  '{"action":"page","step":2,"page":2} shows the next page of a long earlier result.',
  '{"action":"finish","answer":"Changed src/config.ts to read PORT from the environment, because the port was fixed. Test: yarn test passed."} ends the task.',
  "",
  "How to work:",
  "1. Search for the names in the task, then read the lines you will change.",
  "2. Edit with a search block copied exactly from the read output. If an edit fails, read those lines again and copy them; never guess.",
  "3. Run an allowed command to test the change. If it fails, read the error, fix the code and run it again.",
  "4. Finish with what changed, why, and the last test result. With no allowed command, say the change is untested.",
  "Keep results small: read line ranges, not whole files, and end long shell output with | head -n 40.",
].join("\n");

/** Instructions for the GitHub octocode adapter; it reads only through GitHub, not the workspace. */
export function githubSystemPrompt(): string {
  return `You are a code-research agent that answers questions about GitHub repositories and packages, read-only.\n\n${OCTOCODE_GITHUB_INSTRUCTIONS}\n${ACTION_PROTOCOL("ghSearchCode")}\n${GH_ACTION}`;
}

// ---- What the calling agent reads: the MCP prompts (builtin-prompts.ts builds their calls). ----

/** How to check each job's result, one line each. */
export const AGENT_CHECKS: Record<string, string> = {
  ocr: "Check every number and name you will use against the image; garbled text means crop to the text and retry.",
  image: "Verify anything you act on; tested on drawn images only.",
  classify: "Spot-check 3 to 5 answers and treat a low probability as unknown.",
  "find-code": "Read the top ranges before relying on them; on a miss, widen top or rephrase.",
  duplicates: "A high score is a candidate: read both texts before closing anything.",
  summarize: "Check each number and name you reuse against the source.",
  research: "Discard verification.recommendation \"escalate\"; otherwise verify the cited lines.",
  sweep: "Read 3 to 5 flagged chunks and 2 clear ones before trusting the rest.",
};

/** Applying the coding job's SEARCH/REPLACE answer. */
export const APPLY_EDITS = "Apply the edits yourself: for each block, check that its SEARCH text occurs exactly once in the named file, indentation included; replace it with the REPLACE text; skip a block that matches zero or several times. A path followed by one fenced block is a whole new file. Then run the tests: the edits are a small model's candidates.";

/** The `freellama` prompt: when and how to offload. */
export const AGENT_GUIDE = `Use FreeLlama to run bounded work on local models and keep it out of your context.

Offload when the answer is cheap to check and the work is 10 or more similar items, or an input over about 2,000 tokens you would otherwise read whole. Do it yourself: judgment you must stand behind, edits across files, one short item.

Pick a job and pass it to run_task: classify (labels, yes/no), find-code (which file), duplicates, ocr (text in an image), image-recognition (what an image shows), image-search, coding (one small change), summarize. FreeLlama runs the first catalog model that is installed and fits now, with its prompt for that job; any field you pass wins, and job_profile.applied says what it filled in. Name inputs with files; for one answer per file send tasks:[{id, independent:true, task:{files:[path]}}] with job at the top level. For a whole code tree, log or site use sweep; for a question about a repository, delegate_research.

Verify: local answers are candidates. Check 3 to 5 (more where a mistake costs more) before relying on the rest; treat low decision probabilities as unknown; discard a research result whose verification.recommendation is "escalate".

Coding: the answer is SEARCH/REPLACE blocks. ${APPLY_EDITS}

Hand a result on: saveAs:"<name>" writes the answer to handoffs/<name>.md and returns handoff_file; a later run_task reads it with files:[handoff_file].

Approval: pull a model only after the user approves one exact tag and its size (ollama_pull approvedSizeBytes). Unload only a model you warmed or the user named; delete only a tag the user names. A memory refusal names what holds memory: tell the user and never stop their processes.`;
