// napi-rs requires a declaration output path even though this package keeps a reviewed, stable
// declaration file in native/index.d.ts. Use Node for cleanup so the root build works in native
// Windows shells as well as Unix shells.
import { rmSync } from "node:fs";

rmSync(new URL("../native/.napi-generated.d.ts", import.meta.url), { force: true });
