// The Rust router owns candidate ranking. Job mode consumes its machine-local evidence;
// a missing calibration leaves the catalog as an explicitly portable starting point.
import { DEFAULT_SERVE_ENDPOINT } from "./config.js";
import { serveAuthHeaders } from "./serve-http.js";

export type CalibratedPick = { model: string; note: string };

export async function calibratedJobPick(
  endpoint: string | undefined, task: string, prefer: "default" | "capable", needs: readonly string[],
  requireQuality = false,
): Promise<CalibratedPick | undefined> {
  try {
    const response = await fetch(`${(endpoint ?? DEFAULT_SERVE_ENDPOINT).replace(/\/$/, "")}/_freellama/v1/routes`, {
      method: "POST",
      redirect: "error",
      headers: { "content-type": "application/json", ...(await serveAuthHeaders()) },
      body: JSON.stringify({ task, objective: prefer === "capable" ? "quality" : "fastest", required_capabilities: needs }),
      signal: AbortSignal.timeout(3000),
    });
    if (!response.ok) return undefined;
    const route = await response.json() as {
      selected_model?: unknown; task_evidence?: unknown; quality_evidence?: unknown;
    };
    if (typeof route.selected_model !== "string" || !route.selected_model.trim() || route.task_evidence !== "strong") return undefined;
    const quality = route.quality_evidence === "strong";
    if ((prefer === "capable" || requireQuality) && !quality) return undefined;
    return {
      model: route.selected_model,
      note: `selected ${route.selected_model} from this machine's calibrated Rust route; ${quality ? "a task quality policy also qualifies this model" : "throughput evidence does not establish this job's accuracy: verify the answer"}`,
    };
  } catch {
    return undefined;
  }
}
