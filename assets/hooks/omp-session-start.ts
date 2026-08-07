// synapse memory digest for Oh My Pi — installed by `just agents` in the synapse repo.
import { execFile } from "node:child_process";
import { promisify } from "node:util";
import type { ExtensionAPI } from "@oh-my-pi/pi-coding-agent";

const run = promisify(execFile);

export default function synapseDigest(pi: ExtensionAPI): void {
  let digest = "";

  const read = (cwd: string) =>
    run("syn", ["context"], { cwd, timeout: 2_000 })
      .then((out) => {
        digest = out.stdout.trim();
      })
      .catch(() => {});

  pi.on("before_agent_start", async (event, ctx) => {
    if (!digest) await read(ctx.cwd);
    if (!digest) return;
    // `systemPrompt` is per-turn, so every turn appends or the digest falls out.
    return { systemPrompt: [...event.systemPrompt, digest] };
  });

  // Refreshed off the prompt path, so a `syn save` lands in the next turn.
  pi.on("agent_end", (_event, ctx) => {
    void read(ctx.cwd);
  });
}
