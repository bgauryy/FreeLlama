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
/** What a job's default prompt may depend on: the model that will run it. */
export type PromptInput = { model?: string };

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
  /**
   * Documented per-model values, by model family: a key covers every tag of that family (any size
   * suffix or quantization after `:` or `-`); the longest matching key wins, so `bonsai-2:27b`
   * overrides `bonsai-2`. `hf.co/<owner>/` and case are ignored.
   */
  models?: Record<string, ModelProfile>;
  notes: string;
  sources: string[];
};

const catalog = catalogData as { jobs: Array<{ job: string; picks: Array<{ tag: string }> }> };
/** A catalog job's picks; a job the catalog lacks has none, and job mode refuses it when called. */
function picksOf(job: string): string[] {
  return catalog.jobs.find((entry) => entry.job === job)?.picks.map((pick) => pick.tag) ?? [];
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
  qwen38: ["https://huggingface.co/Qwen/Qwen3.8-27B", "https://ollama.com/library/qwen3.8"],
  qwen35: ["https://huggingface.co/Qwen/Qwen3.5-4B", "https://ollama.com/library/qwen3.5"],
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

/**
 * The prompts both OCR picks were trained on. MiniCPM-OCR also documents "Seal Recognition:" and
 * "Chart to Table:" and answers tables in HTML; GLM-OCR also documents "Figure Recognition:".
 */
export const OCR_PROMPTS = { text: "Text Recognition:", table: "Table Recognition:", formula: "Formula Recognition:" } as const;
/** Families trained on OCR_PROMPTS. Any other vision model reads a plain instruction instead. */
const OCR_TRAINED = ["minicpm-ocr", "glm-ocr"];
/** The read-text and read-table requests, worded for a general vision model not trained on the prefixes. */
const OCR_PLAIN = {
  text: "Transcribe all text in the image exactly, line by line. Output only the text.",
  table: "Transcribe the table in the image as a Markdown table. Output only the table.",
};
const ocrPrompt = (kind: keyof typeof OCR_PLAIN, model: string | undefined): string => {
  const name = (model ?? "").toLowerCase().replace(/^(?:hf\.co|huggingface\.co)\/[^/]+\//, "");
  return model === undefined || OCR_TRAINED.some((family) => name.startsWith(family)) ? OCR_PROMPTS[kind] : OCR_PLAIN[kind];
};

// ---- Workflows: what a local model is told to do, step by step, for one kind of task. ----
// run_task {workflow} sends one as the system prompt (the caller's request is the user message);
// run_task {job} without a workflow sends the model's documented system prompt, else the job's
// own (applyJob in job-mode.ts). Short on purpose: these models are 2-30B.

/** How a coding workflow returns a change. */
const EDIT_FORMAT = `Return each change to an existing file as the file's path alone on one line (for "--- file: src/a.ts ---" write src/a.ts), then:
<<<<<<< SEARCH
exact current lines (5 to 30, unique in the file, indentation exact)
=======
replacement lines
>>>>>>> REPLACE
Copy SEARCH lines character for character; prefer several small blocks to one large one. Give a whole file, as its path on one line and the file in one fenced block, only for a new file. If the file was not shown, give the code in one fenced block and say which file and function it belongs in.`;

export type Workflow = {
  /** The job whose model picks run it. */
  job: JobName;
  /** One line for the calling agent. */
  title: string;
  /** What the caller sends: the request goes in `prompt`, inputs in `files`. */
  input: string;
  /** The local model's instructions; for OCR-trained models the trained prefix is sent instead. */
  system?: string;
  /** The user message when the caller's prompt is the input to it (OCR prefixes, field lists). */
  prompt?: (request: string | undefined, model: string | undefined) => string;
  /** Ollama `format` (structured output). */
  format?: "json";
  /** `capable`: run on the strongest catalog pick that fits now, for work a 2B model does badly. */
  prefer?: "capable";
  /** Thinking for this kind of task; false for bounded output, where reasoning only adds time and can leak into the answer. */
  think?: boolean;
};

// No workflow mentions tools or opening files. A "you have no tools" line was tried (2026-11-09) and
// measured on 2026-11-12: on lfm3:1.7b it caused the tool calls it meant to prevent (summarize-log
// 6/6 tool calls with it, 0/6 without; summarize-diff 3/4 vs 0/4; change-code 5/10 vs 1/6).
// Small models act on the noun, not the negation: say what to do, never what is missing.
/** The closing line every text workflow shares: small models pad with what the input lacks, or with greetings. */
const NO_PADDING = "Skip anything the input does not show. No introduction, closing or offer of help.";

export const WORKFLOWS = {
  "read-text": {
    job: "ocr", title: "Read all text in an image, exactly", input: "files: the images",
    prompt: (_request, model) => ocrPrompt("text", model),
  },
  "read-table": {
    job: "ocr", title: "Read a table in an image as Markdown", input: "files: the images",
    prompt: (_request, model) => ocrPrompt("table", model),
  },
  "read-fields": {
    job: "ocr", title: "Pull named fields from a receipt, invoice or form image as JSON", input: "files: the images; prompt: the field names",
    prompt: (request) => `Extract these fields from the image: ${request ?? "all labelled fields"}. Format the output as a JSON object with these keys, values exactly as printed; use "###" for a field the image does not show.`,
  },
  "answer-image": {
    // Bounded output; the vision picks think by default, which only adds time here.
    think: false,
    job: "image-recognition", title: "Answer a question about an image", input: "files: the images; prompt: the question",
    system: `Answer the question about the image.
1. Find the part of the image the question is about.
2. Read any text there exactly as printed.
3. Answer with just the value or label, or one or two sentences.
If the image does not show it, answer "not visible". Never guess at what is cut off.`,
  },
  "describe-screen": {
    job: "image-recognition", title: "Describe a screenshot for a developer: text, errors, state", input: "files: the screenshot; prompt: what to look for (optional)",
    think: false,
    system: `Describe the screenshot for a developer in at most 6 short bullets, in this order: the app or page; headings and main text, quoted exactly; errors, warnings or empty states, quoted exactly; the state of the controls that matter (selected, disabled, filled in); anything broken or misaligned.
If the request names something to look for, answer that first.
Describe only what is visible. ${NO_PADDING}`,
  },
  "change-code": {
    prefer: "capable",
    job: "coding", title: "Make one small code change", input: "files: the file to change; prompt: the change",
    system: `You make one small code change.
1. Find where the change belongs in the code shown.
2. Follow the file's existing names, style and error handling.
3. Change as few lines as the task needs; touch nothing else.
${EDIT_FORMAT}
Explain nothing unless asked.`,
  },
  "fix-bug": {
    prefer: "capable",
    job: "coding", title: "Fix a bug with the smallest change", input: "files: the suspect code; prompt: the error message or symptom",
    system: `You fix one bug with the smallest change.
1. Read the error or symptom, then the code shown.
2. Name the cause in one sentence, quoting the line that causes it.
3. Change only what the fix needs.
Answer with the cause line, then the edits.
${EDIT_FORMAT}
If the code shown cannot cause the symptom, say so and name the file or function to look at instead.`,
  },
  implement: {
    prefer: "capable",
    job: "coding", title: "Add a small feature to existing code", input: "files: the code; prompt: the feature",
    system: `You add one small feature to the code shown.
1. Find the function or place it belongs; reuse helpers the file already has.
2. Follow the file's names, style and error handling.
3. Handle the empty and error cases the feature can meet.
${EDIT_FORMAT}
End with one line: what to test.`,
  },
  "write-tests": {
    prefer: "capable",
    job: "coding", title: "Write unit tests for the code shown", input: "files: the code (and an existing test, for style); prompt: what to cover (optional)",
    system: `You write unit tests for the code shown; you never change that code.
1. List the behaviours: the normal case, edge cases (empty, zero, missing, boundary) and one failure case.
2. Use the test framework and import style the project already uses; copy them from the files shown.
3. Write one small test per behaviour, named after it, with literal expected values.
Answer with the test file's path on one line, then the whole file in one fenced block.`,
  },
  "review-code": {
    prefer: "capable",
    job: "coding", title: "Find real defects in code (not style)", input: "files: the code; prompt: what to focus on (optional)",
    system: `You review the code shown for real defects only: wrong logic, unhandled errors, off-by-one, null or undefined access, leaks, injection, races. Ignore style and naming.
For each defect write one line: path - the faulty code quoted exactly (one line) - the problem - the fix.
Report at most 5, the surest first. If you find none, answer "no defects found". Never invent code you did not see. No introduction or closing.`,
  },
  "explain-code": {
    job: "coding", title: "Explain code to someone about to change it", input: "files: the code; prompt: what to explain (optional)",
    system: `Explain the code shown to a developer who is about to change it, in at most 6 bullets:
what it does (one sentence); its inputs and outputs; side effects (files, network, shared state); its callers, when the code shown includes them; the one thing most likely to break when it changes.
Name functions and variables exactly. ${NO_PADDING}`,
  },
  "summarize-log": {
    think: false,
    prefer: "capable",
    job: "summarize", title: "Summarize a log: errors grouped, counted, first failure", input: "files: the log; prompt: what you are investigating (optional)",
    system: `Summarize the log for a developer debugging it. Write only these lines, in order, skipping any the log gives nothing for:
- one bullet per distinct error or warning: the message quoted exactly, then (×count, first–last timestamp);
- First error: the earliest error line, quoted exactly;
- Cause: one sentence, only if a log line states it.
At most 8 bullets. Skip routine lines. ${NO_PADDING}`,
  },
  "summarize-text": {
    think: false,
    // scripts/local-test/workflows.mjs (2026-11-10, 3 cases): with the old prompt lfm3:1.7b ran
    // 2-4x over length on all three, padding every category with "not stated"; the no-padding
    // wording below took it to 2/3. bonsai-2:9b and Nemotron Bolt pass 3/3 either way, hence capable.
    prefer: "capable",
    job: "summarize", title: "Summarize text for someone who will act on it", input: "files: the text; prompt: what you need from it (optional)",
    system: `Summarize the text for someone who will act on it: its decisions, numbers, deadlines and open questions, plus whatever the request asks for.
Use as few short bullets as the text needs; one is right for one sentence, and the summary is never longer than the text.
Never write a bullet about something the text does not say (no "not provided", "not specified" or "no details"); write "not stated" only for something the request explicitly asks for. Never guess.
Quote numbers, names, versions and paths exactly. No introduction, closing or offer of help.`,
  },
  "summarize-diff": {
    think: false,
    prefer: "capable",
    job: "summarize", title: "Summarize a diff for a reviewer: changes and risks", input: "files: the diff or patch",
    system: `Summarize the diff for a reviewer in at most 8 bullets: one per changed file (what changed, naming functions and settings exactly), then one per behaviour change or risk the diff itself shows (a removed check, a changed default, a new dependency, code changed with no test changed).
If the diff shows no risk, write no risk bullet. ${NO_PADDING}`,
  },
  "extract-json": {
    think: false,
    job: "summarize", title: "Extract named fields from text into JSON", input: "files: the text; prompt: the field names (or a JSON schema)",
    system: `Extract the fields the request names from the text into one JSON object.
Use the values exactly as written in the text; use null for a field the text does not give; never invent a value.
Output only the JSON object.`,
    format: "json",
  },
} satisfies Record<string, Workflow>;

export type WorkflowName = keyof typeof WORKFLOWS;
export const WORKFLOW_NAMES = Object.keys(WORKFLOWS) as WorkflowName[];

/** bonsai-2:9b with thinking off and the card's non-thinking preset: for short bounded jobs. */
const BONSAI_9B: ModelProfile = {
  think: false,
  options: { temperature: 0.7, top_p: 0.8, top_k: 20, min_p: 0, presence_penalty: 1.5, repeat_penalty: 1 },
  notes: "With think:true use the card's thinking preset (temperature 1.0, top_p 0.95, presence_penalty 1.0) and allow at least 8,192 output tokens; never greedy decoding with thinking on.",
  sources: SOURCES.bonsai9b,
};

/** Qwen3.8 / Qwen3.5 with thinking off and their cards' non-thinking preset: for short bounded jobs. */
const qwenNonThinking = (sources: string[]): ModelProfile => ({
  think: false,
  options: { temperature: 0.7, top_p: 0.8, top_k: 20, min_p: 0, presence_penalty: 1.5, repeat_penalty: 1 },
  notes: "Thinks by default; with think:true use the card's thinking preset (temperature 1.0, top_p 0.95, top_k 20; Qwen3.8 presence_penalty 0, Qwen3.5 1.5, or 0 with temperature 0.6 for precise coding) and a large output budget (Qwen3.5 32,768 tokens). Raise presence_penalty (up to 2) if it repeats.",
  sources,
});

/** The chat models of the coding picks, and other families installed for chat, as their cards document them. */
const CHAT_MODELS: Record<string, ModelProfile> = {
  lfm3: {
    options: { temperature: 0.1, top_k: 50, repeat_penalty: 1.1 },
    notes: "Thinks by default; its card calls thinking off inconsistent, so it stays on. Not for agentic coding or multi-step reasoning.",
    sources: SOURCES.lfm3,
  },
  "nvidia-nemotron-3.5-bolt": {
    options: { temperature: 1, top_p: 0.95 },
    // `ollama show` of the hf.co Q4_K_M tag, 2026-11-08: no thinking capability, and the template
    // defaults enable_thinking to false.
    notes: "The card says it reasons by default, but its GGUF template turns thinking off and Ollama reports no thinking capability, so it answers directly; think:true is refused.",
    sources: SOURCES.nemotronBolt,
  },
  "bonsai-2": BONSAI_9B,
  // The card documents the same instruct (non-thinking) preset as the 9B; Ollama lists think
  // values false, low, medium and xhigh for this tag.
  "bonsai-2:27b": {
    ...BONSAI_9B,
    notes: "With think:true (or low, medium, xhigh) use the card's thinking preset (temperature 1.0, top_p 0.95, top_k 20, min_p 0.05, presence_penalty 0) and allow about 16,384 output tokens.",
    sources: SOURCES.bonsai27b,
  },
  "qwen3.8": qwenNonThinking(SOURCES.qwen38),
  "qwen3.5": qwenNonThinking(SOURCES.qwen35),
};

const CHAT_SOURCES = [...SOURCES.lfm3, ...SOURCES.nemotronBolt, ...SOURCES.bonsai9b, ...SOURCES.bonsai27b, ...SOURCES.qwen38, ...SOURCES.qwen35];

/** Per job: what job mode sends a local model when the caller leaves it out. */
export const LOCAL_JOBS: Record<JobName, LocalJob> = {
  classify: {
    job: "classify", task: "decision", catalogJob: "classify", defaultPicks: picksOf("classify"),
    models: {
      "strands-decider": { notes: "State and questions together must fit 4,096 tokens; weak on long, multi-step documents. Its README: answers at confidence 0.9 or more are right about 95% of the time; confirm the rest.", sources: SOURCES.strandsDecider },
      "nimble-v3": { notes: "Documents num_ctx 32768 (routing sizes it here); CC BY-NC 4.0, noncommercial.", sources: SOURCES.nimble },
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
    prompt: ({ model }) => ocrPrompt("text", model),
    // Both publishers' Ollama Modelfiles set temperature 0; the hf.co MiniCPM tag ships no parameters.
    models: {
      "minicpm-ocr": {
        options: { temperature: 0 },
        notes: "Chinese and English; tables come back as HTML. Its Modelfile sets num_ctx 8192.",
        sources: [...SOURCES.miniCpmOcr, "https://raw.githubusercontent.com/OpenBMB/MiniCPM-OCR/main/docs/ollama_deploy.md"],
      },
      "glm-ocr": {
        options: { temperature: 0 },
        notes: "Also documents Figure Recognition:.",
        sources: [...SOURCES.glmOcr, "https://github.com/zai-org/GLM-OCR/blob/main/examples/ollama-deploy/README.md"],
      },
    },
    notes: "The OCR picks were trained on Text Recognition:, Table Recognition: and Formula Recognition:; a general vision model gets the same request in plain words. Crop to the text first.",
    sources: [...SOURCES.miniCpmOcr, ...SOURCES.glmOcr, cookbook("ocr")],
  },
  "image-recognition": {
    job: "image-recognition", task: "vision", catalogJob: "image-recognition", defaultPicks: picksOf("image-recognition"),
    needs: "prompt: your question about the image",
    systemPrompt: WORKFLOWS["answer-image"].system,
    models: {
      "lfm3-vl": {
        options: { temperature: 0.1, top_p: 1, top_k: 50, min_p: 0, repeat_penalty: 1.1 },
        notes: "Thinks before it answers by default (Ollama lists think values true and false; off is unmeasured here); images go in at native resolution, so a large one costs many tokens.",
        sources: SOURCES.lfm3Vl,
      },
      "lfm3-omni": {
        systemPrompt: "Answer questions about the provided image. Base every answer only on what can be seen.",
        options: { temperature: 0 },
        notes: "Non-reasoning: never send think:true. Up to 2,048 tokens per image. The card gives 32K context; the Ollama tag advertises 128K.",
        sources: SOURCES.lfm3Omni,
      },
      "bonsai-2": BONSAI_9B,
      "qwen3.8": qwenNonThinking(SOURCES.qwen38),
      "qwen3.5": qwenNonThinking(SOURCES.qwen35),
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
    systemPrompt: WORKFLOWS["change-code"].system,
    models: CHAT_MODELS,
    notes: "The answer is SEARCH/REPLACE blocks: apply one only where its SEARCH text matches exactly once, then run the tests.",
    sources: [...CHAT_SOURCES, cookbook("coding")],
  },
  summarize: {
    job: "summarize", task: "completion", catalogJob: "coding", defaultPicks: picksOf("coding"),
    prompt: () => "Summarize the text below.",
    systemPrompt: WORKFLOWS["summarize-text"].system,
    // The same bounded output as the summarize-text workflow: thinking only adds time and leaks in.
    think: false,
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

/** Research and sweep results are read by the agent; run_task results carry their own check: lines. */
export const AGENT_CHECKS: Record<string, string> = {
  research: "Discard verification.recommendation \"escalate\"; otherwise open the cited path:line.",
  sweep: "Read the unsure chunks first, then a few flagged and clear ones.",
};

/** Applying the coding job's SEARCH/REPLACE answer. */
export const APPLY_EDITS = "Apply the edits yourself: for each block, check that its SEARCH text occurs exactly once in the named file, indentation included; replace it with the REPLACE text; skip a block that matches zero or several times. A path followed by one fenced block is a whole new file. Then run the tests: the edits are a small model's candidates.";

/** The `freellama` prompt: when and how to offload. */
export const AGENT_GUIDE = `Use FreeLlama to run bounded work on local models and keep it out of your context.

Offload when the answer is cheap to check and the work is 10 or more similar items, or an input over about 2,000 tokens you would otherwise read whole. Do it yourself: judgment you must stand behind, edits across files, one short item.

Name a workflow and run_task does the rest: it picks the first catalog model for the job that is installed and fits now and gives it step-by-step instructions for the task. Workflows: read-text, read-table, read-fields (images); answer-image, describe-screen; change-code, fix-bug, implement, write-tests, review-code, explain-code; summarize-log, summarize-text, summarize-diff, extract-json. For labels, code search and duplicates use job classify, find-code or duplicates. Any field you pass wins; job_profile says what was filled in. Name inputs with files; for one answer per file send tasks:[{id, independent:true, task:{files:[path]}}] with job at the top level. For a whole code tree, log or site use sweep; for a question about a repository, delegate_research.

Verify: local answers are candidates. Each run_task result ends with check: lines worked out from it (uncertain answers, close matches, facts not in the source, edits that do not match the file); act on those first, and discard a research result whose verification.recommendation is "escalate".

Coding: the answer is SEARCH/REPLACE blocks. ${APPLY_EDITS}

Hand a result on: saveAs:"<name>" writes the answer to handoffs/<name>.md and returns handoff_file; a later run_task reads it with files:[handoff_file]. Answers over 8,000 characters come back as a summary with check: lines and an output_file holding the whole answer; output:"inline" returns the whole text, outputPath:"<path>" writes (or replaces) that file inside your project.

Approval: pull a model only after the user approves one exact tag and its size (ollama_pull approvedSizeBytes). Unload only a model you warmed or the user named; delete only a tag the user names. A memory refusal names what holds memory: tell the user and never stop their processes.`;
