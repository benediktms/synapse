// synapse session-start digest for Oh My Pi — installed by `syn xtask install-agents`.
import { execFile } from "node:child_process";
import { promisify } from "node:util";
import type { ExtensionAPI } from "@oh-my-pi/pi-coding-agent";

const run = promisify(execFile);

export default function synapseDigest(pi: ExtensionAPI): void {
  let digest: string | undefined;

  pi.on("before_agent_start", async (event) => {
    if (digest === undefined) {
      digest = await run("syn", ["context"], { timeout: 10_000 })
        .then((out) => out.stdout.trim())
        .catch(() => "");
    }
    if (!digest) return;
    // `systemPrompt` is per-turn, so every turn appends or the digest falls out.
    return { systemPrompt: [...event.systemPrompt, digest] };
  });
}
