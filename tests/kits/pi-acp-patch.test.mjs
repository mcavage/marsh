// The Pi Kit's patch-pi-acp.mjs keeps pi-acp's startup banner out of agent messages.
import assert from 'node:assert/strict';
import { test } from 'node:test';
import { banner, patch } from '../../kits/marsh-pi/patch-pi-acp.mjs';

// The shape of pi-acp 0.0.34's PiAcpSession around the banner.
const adapter = `class Session {
  sendStartupInfoIfPending() {
    if (this.startupInfoSent || !this.startupInfo) return;
${banner}  }
  other() {
    this.emit({
      sessionUpdate: "agent_message_chunk",
      content: { type: "text", text: "Cleared queued prompts." }
    });
  }
}
`;

test('the startup banner is no longer emitted as an agent message', () => {
  const out = patch(adapter);
  assert.ok(!out.includes('this.startupInfo }'));
  assert.ok(out.includes('this.startupInfoSent = true;'));
  assert.ok(out.includes('Cleared queued prompts.'), 'other agent messages are untouched');
  assert.doesNotThrow(() => new Function(out.replace('class Session', 'return class Session')));
});

test('a pi-acp release that changes the banner code fails the build', () => {
  assert.throws(() => patch('class Session {}'), /not found exactly once/);
  assert.throws(() => patch(adapter + adapter), /not found exactly once/);
});
