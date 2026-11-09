#!/usr/bin/env python3
"""Configure, probe, judge and remove the synthetic header-injection rule (SEC-17).

  synthetic_injection.py set     --run RUN --sandbox KIT_VM
  synthetic_injection.py probe   --run RUN [--scope kit|services]   # the command line to run
  synthetic_injection.py verdict --run RUN --kit-echo FILE --services-echo FILE
  synthetic_injection.py remove  --run RUN --sandbox KIT_VM

`set` stores a sbx custom secret scoped to the Kit VM (never global): host
httpbin.org, placeholder `nfx-ph-kit`, value `nfx-<run>`. The Kit image's
`curl` wrapper (-K /etc/nfx-curlrc) makes a PLAIN `curl -s https://httpbin.org/headers` in a Kit job send
`X-Nfx-Synthetic: nfx-ph-kit`, which the proxy turns into `nfx-<run>` (positive
control: the plan's command, unmodified). `probe --scope services` prints the
equivalent trigger for a services container: the pinned curl image sending the
same placeholder header explicitly; it must come back UNSUBSTITUTED (negative
control, non-vacuous because the trigger is the same). `verdict` judges both
echoes: invalid when the positive control lacks the value or the services echo
lacks the placeholder (the trigger never reached the endpoint); failed when the
services echo carries the value; otherwise passed. `remove` deletes the secret by
placeholder and sandbox scope and checks `sbx secret ls`. The value is synthetic.
"""
import argparse
import json
import pathlib
import subprocess
import sys

HERE = pathlib.Path(__file__).resolve().parent
CONFIG = json.loads((HERE / "egress-injection.json").read_text())


def settings(run: str) -> dict:
    return {key: (value.replace("{run}", run) if isinstance(value, str) else value) for key, value in CONFIG.items()}


def env_value(key: str) -> str:
    return next(line.split("=", 1)[1] for line in (HERE / "egress.env").read_text().splitlines()
                if line.startswith(key + "="))


def probe_command(run: str, scope: str = "kit") -> str:
    cfg = settings(run)
    echo = env_value("EGRESS_ECHO_URL")
    if scope == "kit":
        return f"curl -s -m 20 {echo}"  # the plan's command; the Kit's curl wrapper supplies the placeholder header
    return (f"docker run --rm {env_value('EGRESS_CURL_IMAGE')} -s -m 20 -H '{cfg['header']}: {cfg['placeholder']}' {echo}")


def header_values(echo_text: str, header: str) -> list[str]:
    try:
        headers = json.loads(echo_text)["headers"]
    except (ValueError, KeyError, TypeError):
        return []
    return [v for k, v in headers.items() if k.lower() == header.lower()]


def verdict(run: str, kit_echo: str, services_echo: str) -> dict:
    """SEC-17 c1: the services echo must lack the synthetic VALUE anywhere (any header, any
    field, even as a substring), while the Kit echo had it and the services request really
    carried the same trigger (otherwise the control proves nothing)."""
    cfg = settings(run)
    kit = header_values(kit_echo, cfg["header"])
    if kit != [cfg["value"]]:
        return {"result": "invalid", "why": "positive control: the Kit echo lacks the substituted value", "kit": kit}
    if cfg["value"] in services_echo:
        return {"result": "failed", "why": "the synthetic value appears in the services echo", "where": locate(services_echo, cfg["value"])}
    services = header_values(services_echo, cfg["header"])
    if services != [cfg["placeholder"]]:
        return {"result": "invalid", "why": "the services trigger never reached the endpoint", "services": services}
    return {"result": "passed", "kit": kit, "services": services}


def locate(echo_text: str, value: str) -> list[str]:
    """Which echoed headers (or `raw`) contain VALUE."""
    try:
        headers = json.loads(echo_text)["headers"]
    except (ValueError, KeyError, TypeError):
        return ["raw"]
    return sorted(name for name, got in headers.items() if value in str(got)) or ["raw"]


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    parser.add_argument("action", choices=["set", "probe", "verdict", "remove"])
    parser.add_argument("--scope", choices=["kit", "services"], default="kit")
    parser.add_argument("--kit-echo")
    parser.add_argument("--services-echo")
    parser.add_argument("--run", required=True)
    parser.add_argument("--sandbox")
    parser.add_argument("--sbx", default="sbx")
    args = parser.parse_args()
    cfg = settings(args.run)
    if args.action == "probe":
        print(probe_command(args.run, args.scope))
        return 0
    if args.action == "verdict":
        outcome = verdict(args.run, pathlib.Path(args.kit_echo).read_text(), pathlib.Path(args.services_echo).read_text())
        print(json.dumps(outcome))
        return {"passed": 0, "failed": 1, "invalid": 2}[outcome["result"]]
    if args.action == "set":
        if not args.sandbox:
            parser.error("set needs --sandbox (the rule is Kit-scoped)")
        subprocess.run([args.sbx, "secret", "set-custom", "--no-input", "--sandbox", args.sandbox, "--host", cfg["host"],
                        "--placeholder", cfg["placeholder"], "--value-stdin"],
                       input=cfg["value"].encode(), check=True)
        print(json.dumps({"created": "sbx secret", "placeholder": cfg["placeholder"], "sandbox": args.sandbox}))
        return 0
    if not args.sandbox:
        parser.error("remove needs --sandbox (the rule is Kit-scoped)")
    subprocess.run([args.sbx, "secret", "rm", "--placeholder", cfg["placeholder"], "--sandbox", args.sandbox, "-f"],
                   check=True, capture_output=True)
    left = subprocess.run([args.sbx, "secret", "ls"], capture_output=True, text=True).stdout
    if cfg["placeholder"] in left:
        raise SystemExit(f"synthetic_injection: {cfg['placeholder']} is still listed by `sbx secret ls`")
    print(json.dumps({"removed": cfg["placeholder"], "sandbox": args.sandbox}))
    return 0


if __name__ == "__main__":
    sys.exit(main())
