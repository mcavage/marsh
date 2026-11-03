// pi-acp sends Pi's terminal startup banner ("pi vX", "---", "## Context",
// "## Skills" lists) as an `agent_message_chunk` right after session/new, which
// ACP clients render as the first words of the agent's reply. The banner is
// not part of any message in ACP mode. The same text stays available, as
// non-message data, in the session/new response (`_meta.piAcp.startupInfo`).
// Fail the image build if a pi-acp release changes the code this removes, so
// the change is reviewed instead of silently reverting.
import { readFileSync, writeFileSync } from 'node:fs';
import { dirname, join } from 'node:path';
import { fileURLToPath } from 'node:url';

export const banner = `    this.startupInfoSent = true;
    this.emit({
      sessionUpdate: "agent_message_chunk",
      content: { type: "text", text: this.startupInfo }
    });
`;
export const patched = `    this.startupInfoSent = true;
`;

export function patch(source) {
  if (source.split(banner).length !== 2) {
    throw new Error('pi-acp startup banner code not found exactly once; review the ACP banner patch');
  }
  return source.replace(banner, () => patched);
}

if (process.argv[1] === fileURLToPath(import.meta.url)) {
  const file = join(dirname(fileURLToPath(import.meta.url)), 'node_modules/pi-acp/dist/index.js');
  writeFileSync(file, patch(readFileSync(file, 'utf8')));
  console.log('pi-acp: startup banner is no longer sent as an agent message');
}
