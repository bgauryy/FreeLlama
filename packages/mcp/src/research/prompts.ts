// System prompts for the research adapters, which import them from here. The text lives in
// ../local-prompts.ts with every other local-model instruction. Their exact bytes are part of the
// calibration identity (`system_hash`), so edit them only together with a deliberate identity change.
export { BASH_SYSTEM_PROMPT, CODER_SYSTEM_PROMPT, githubSystemPrompt, octocodeSystemPrompt } from "../local-prompts.js";
