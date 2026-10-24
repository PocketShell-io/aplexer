# aplexer scope

Purpose: run, retain and expose agent and shell sessions on one machine through a
versioned CLI/RPC contract, shared by Hub and PocketShell.

This repo: the local session runtime, native identity/incarnation, process lifecycle,
engine/profile definitions, state hooks and local messaging/coordination primitives.

## In scope

Local start/list/attach/capture/send/kill, session metadata and state paths belong
here. Local delegated-task execution is a runtime primitive; it does not own
cross-machine work policy or acceptance. Consumers must use published interfaces
rather than copy state-directory or process-management logic.

## NOT our job (owner)

- Cross-host agent fleet placement, scheduling, retries and machine eligibility — owner: Hub.
- Model/tier/account policy, quota admission/reserves and managed account provisioning — owner: Hub.
- Intakes, task AC, approval ledgers, work alerts and acceptance verification — owner: Hub.
- Network access transport, SSH, NAT relay, files, ports, keys and sync — owner: PocketShell;
  Hub owns its outbound managed host-control transport.
- Mobile/web/desktop product UI and voice/keyboard input UX — owner: PocketShell;
  Hub owns the agent-work dashboard and phone-browser task UI.
- Product-specific identity, credentials or runtime dependence on either consumer — owners:
  Hub and PocketShell each own their product integration, login and availability.
- Business MCP integrations — owner: a separate DataOps MCP server for DataOps (F10);
  Dapier/AISL tools are removed from PocketShell MCP, with future work owned by
  their respective projects after a separate decision.

## Consumer boundary

Hub owns managed agent work and may stop/delete ANY session on a machine,
including manual PocketShell/psc sessions (t1405u), subject to authentication and
existing safety gates. PocketShell owns human access and must work without Hub.
Bounded capture snapshots and task artifacts are allowed Hub evidence;
PocketShell owns live terminal access.

Hub and PocketShell consumer contract fixtures must pass before a runtime release.
No release, runtime or service change is performed by this documentation intake.
See the [PocketShell charter](https://github.com/PocketShell-io/pocketshell/blob/main/SCOPE.md).
A gray-area change needs an ADR with its intake ID and Alexey's approval.
