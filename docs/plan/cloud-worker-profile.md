# Cloud worker profile: first implementation slice

Status: implemented on the cloud-placement branch, **not end-to-end cloud
support in the marsh CLI**. `cloud {}`, cloud dispatch, lineage, and workspace
return still require the remaining [design slices](../design/cloud-placement.md).

## Authority boundary

`marsh-runtime::SbxRuntimeProfile` is trusted executor configuration, not part of
`JobSpec` or exported environment. The default `Local` profile retains its exact
`http://gateway.docker.internal:3128` endpoint and existing local MCP behavior.

The internal worker startup mode `--serve-cloud-v2 GENERATION` requires trusted
root bootstrap. It selects `CloudV2`, requiring the exact native proxy pair
`http://192.0.2.1:8889` and a safe root-owned CA bundle before advertising Ready.
The existing create path revalidates the plumbing and binds the CA read-only.
Missing/forged/mixed endpoints and unqualified cloud MCP settings are rejected.
Only fixed allowlisted plumbing is captured; bypass and raw-key variables are
not forwarded and are rejected if offered as runtime plumbing. There is no arbitrary proxy URL setting, automatic
profile detection, local fallback, or production proxy-clearing path.

This is runtime plumbing, not Kit descriptor conversion. Provider/OAuth/MCP
qualification and daemon-controlled cloud lifecycle remain separate gates.

## Real-caller qualification

On 2026-10-16, fresh baseline and candidate ARM64 static workers were built from
this checkout using the pinned Rust 1.95 build image. The baseline was built
before the Rust changes; its ordinary mode failed with the native cloud proxy.
It was retired before candidate testing, honoring quarantine/no-replay rules.

Two candidate jobs then ran through the real retained worker transport in a
credential-free native v2 cloud sandbox, with the native environment intact:

- Distinct fresh containers, uid 1000, no tested runtime sockets or raw provider
  credentials; empty, ordinary UTF-8, and non-UTF-8 argv preserved. The UTF-8
  case retained the image's `/bin/sh` entrypoint. The byte case used a read-only
  helper mount whose bytes matched the qualified byte-exec artifact.
- Binary input/output and separate stderr exact, output before stdin EOF, and
  exit 17 retained. The next fresh container observed the preceding file write.
- HTTPS succeeded for the Kit's allowed host and failed with CONNECT 403 for
  its denied host. Python used hostname/certificate verification and the
  automatically supplied CA bundle; Docker inspection confirmed a read-only CA
  mount. No extra curl/tools grant or TLS bypass was used in this qualification.
- Worker cleanup reports were verified, and independent Docker inventories
  confirmed both job containers absent.
- Nonroot startup, missing/forged proxy settings, local endpoints in cloud mode,
  and unqualified MCP settings failed with 125 and no Ready frame. No containers
  were created by those refusals.

Candidate worker SHA256:
`d13b230408ba628b26d346db07f257efb654fe10b860aec588b41bf1689aec19`.
Baseline worker SHA256:
`77119c6d04a0e83d4eefcfb23c96458d4b70881e1b1b492d8f03179725bbeeb9`.
The candidate/source/input fingerprints remained stable throughout the checks.
The first successful run exercised two byte-carrier jobs. A review identified
the missing native path; the final successful run explicitly exercised one
native UTF-8 job and one byte-carrier job. An intermediate caller assertion
incorrectly assumed a helper filename and failed; it was corrected to verify
the actual read-only helper mount and artifact hash. Product Rust was unchanged.
Execution evidence and disposable caller are retained locally under
`.pi-agent/deliver/cloud-worker-profile/`; these are not a shipping cloud test
framework or product adapter. Public cloud acceptance will use the real daemon/
CLI path when that path exists, rather than fake it for this slice.

Both named cloud sandboxes, the stock bake sandbox, and the newly created
template were independently absent after cleanup. The uniquely named host
fixture-image tag was also removed.

Supporting checks: focused runtime/worker tests passed (114); clippy with the
pinned Rust 1.95 image passed; formatting and doctest checks passed. Host Rust
1.99 clippy reports its new `assert_is_empty` lint against the surrounding
test-assertion style (also reused by the new boundary check). A repository-wide
lint migration is outside this slice; the declared Rust 1.95 gate passed. The
live cloud journey, not unit-test volume, is the primary evidence.
