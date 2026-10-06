// Context-management and canonical-JSON primitives for benchmark/local scripts. The build compiles
// this module to dist/research/benchmark.js; benchmark scripts import it instead of
// re-implementing the adapters' context policy.
export { canonicalJson, isRecord } from "./canonical-json.js";
export { ContextPolicy } from "./context-policy.js";
export { fitToContext } from "./fit.js";
export { cpLength, trimSpace } from "./text.js";
