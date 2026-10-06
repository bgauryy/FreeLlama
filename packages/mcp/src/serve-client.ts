// Managed-serve NAPI calls, each wrapped to autostart the default local serve once on demand.
import * as native from "./native.js";
import { withServe } from "./serve.js";

export const createSession = withServe(native.createSession);
export const deleteSession = withServe(native.deleteSession);
export const killSession = withServe(native.killSession);
export const listModels = withServe(native.listModels);
export const route = withServe(native.route);
export const runTaskRequest = withServe(native.runTaskRequest);
export const runTaskBatchRequest = withServe(native.runTaskBatchRequest);
export const warmModelRequest = withServe(native.warmModelRequest);
export const listTaskJobs = withServe(native.listTaskJobs);
export const getTaskJob = withServe(native.getTaskJob);
export const cancelTaskJob = withServe(native.cancelTaskJob);
export const removeTaskJob = withServe(native.removeTaskJob);
export const createScope = withServe(native.createScope);
export const getScope = withServe(native.getScope);
export const forkScope = withServe(native.forkScope);
export const deleteScope = withServe(native.deleteScope);
