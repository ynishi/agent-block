# Agent State Primitives — Current Surface and Future Extensions

This document defines the current agent-state primitive surface in
`agent-block` and catalogs candidate extensions ("Data Primitive Futures").

Status: **DRAFT** — proposal layer, not yet a contract.

## Goal

Provide a coherent, opt-in primitive surface for agent state across the
storage / knowledge / coordination / external axes, while preserving the
existing thin-bridge philosophy (Lua for logic, Rust for plumbing).

## Non-goals

- This document does not prescribe which futures will be implemented.
  Section §3 is a **catalog of candidates**, each gated by use-case
  evidence.
- This document does not redesign the existing KV / SQLite primitives. §2
  documents the contract those two primitives have already established and
  which any new primitive in this family must follow.
- This document does not specify orchestration semantics. State primitives
  are infrastructure; orchestration belongs to consumer blocks.

## 1. The Four Extension Axes

Agent state needs split along four axes. The current primitives cover the
Storage axis and part of the External axis; the future catalog (§3) covers
the remaining gaps.

| Axis          | Question being answered                          | Current              | Future candidates       |
|---------------|--------------------------------------------------|----------------------|-------------------------|
| Storage tier  | "Where does this byte live and for how long?"    | KV, SQL, TSDB (#3.8) | object store (#3.4)     |
| Knowledge     | "Find me things related to X"                    | (none)               | vector (#3.1), rule (#3.2) |
| Coordination  | "Reconcile state / events across agents"         | (none)               | CRDT (#3.3), messaging (#3.5) |
| External      | "Read / subscribe to data the runtime does not own" | mcp (partial)     | resource subscribe (#3.7) |

The axes are **chosen to minimize overlap**. A new primitive proposal that
does not clearly land on one axis (or that duplicates an existing one)
should be rejected or merged before getting an interface.

**Watch / notification is not its own axis.** It is an *operation shape*
that applies across all axes (SQL change, MCP resource update, imsg
subscribe). The shared convention is defined in §2.6, and per-primitive
instances are noted in §3.6.

## 2. Existing Primitive Contract

The KV and SQLite primitives encode the conventions any storage-backed
primitive in this surface must follow. This section makes those conventions
explicit so §3 candidates can be added without bikeshedding.

### 2.1 Path / Storage Convention

- Base directory: `AGENT_BLOCK_HOME` (default `~/.agent-block`).
- Per-component file or directory: e.g. `kv.sqlite`, `db.sqlite`.
  Components do not share storage; lifecycle (WAL, page cache, backup) is
  independent per primitive.
- Per-component path override: `AGENT_BLOCK_<NAME>_PATH`
  (`AGENT_BLOCK_KV_PATH`, `AGENT_BLOCK_SQL_PATH`, ...).
- In-memory backend: every storage-backed primitive must support the
  literal `:memory:` sentinel for testing.

Source: `src/bridge/config.rs:36-50`, `src/host.rs:93-96`.

### 2.2 ENV-Driven, No CLI Flags

All primitive configuration is exposed via `AGENT_BLOCK_*` environment
variables. CLI flags are not introduced for primitive tuning; `.env` is the
single configuration surface (auto-loaded by `host::run` via `dotenvy`).

Source: `src/bridge/config.rs:3`, `src/host.rs:280`.

### 2.3 Lua API Surface

- Namespace: `std.<primitive>.*` (e.g. `std.kv.get`, `std.sql.query`).
- Async by default — long-running or blocking ops use
  `tokio::task::spawn_blocking` + `InterruptHandle` so cooperative cancel
  is always wired.
- NULL / sentinel values are explicit (e.g. `std.sql.null` LightUserData,
  JSON-null round-trip).

Source: `src/bridge/sql.rs`, `CHANGELOG.md:500-528`.

### 2.4 LLM Tool Registration

Each primitive that wants to be reachable from an LLM exposes a single
opt-in registrar:

```lua
std.<primitive>.register_tools(opts?)
```

This calls `tool.register(name, schema, handler)` for each operation, with
schemas visible to LLM providers. Registration is **opt-in per call site**,
so headless / non-LLM hosts pay no cost and the LLM-facing surface is
explicit at registration time.

Source: `src/bridge/kv_tools.lua`, `src/bridge/sql_tools.lua`.

### 2.5 Bridge Layout

- Rust side: `src/bridge/<primitive>.rs` exposes `register(lua, ctx)` as a
  thin adapter; the actual implementation lives in `mlua-batteries` (or a
  comparable external crate) when generic, and in-tree only when
  agent-block-specific plumbing is required.
- HostContext (`src/host.rs:93-96`) holds `Arc<Mutex<...>>` connections /
  handles per primitive, registered once at startup.
- Handler-side Isle path: `register_all_handler_side` mirrors the main
  surface so worker isles see the same `std.*` API.

The mlua-batteries vs. in-tree split is decided **at implementation time**
based on whether host plumbing (HostContext, observability, agent-mesh
hooks) is required. No design-time prescription.

### 2.6 Notification / Watch Convention

A primitive may need to push state-change events to Lua. Two shapes
co-exist; both are present in the current `std.mcp.*` surface as the
reference implementation.

**(A) Callback registration** — for unbounded streams of events tied to a
named target (server, topic, etc.):

```lua
std.<primitive>.on_<event>(target, callback)
```

Existing instances: `std.mcp.on_progress(server, cb)`,
`std.mcp.on_log(server, cb)` (`src/bridge/mcp.rs:419, 447`). Callbacks
fire as events arrive; cancellation is by handle / disconnect.

**(B) Watched query / view** — for "current value + change notifications"
tied to a query rather than a server:

```lua
local handle = std.<primitive>.watch(query, params?)
handle:current() -> rows
handle:on_change(callback)
handle:close()
```

No existing instances. Reserved for §3.6.

#### Rules

- A primitive that already has a natural target identity (server name,
  topic) should use shape (A).
- A primitive whose subscription is parametrized by a query or filter
  expression should use shape (B).
- Both shapes deliver events into the same Lua isle as the registering
  call; cross-isle delivery is the consumer's job.
- `register_tools` (§2.4) does not auto-expose watch ops to LLMs by
  default — push-callback semantics do not fit the synchronous tool-call
  model.

## 3. Data Primitive Futures (Catalog)

For each candidate: (a) **need** — the workload that fails without it,
(b) **why not KV / SQL** — the gap in the existing surface, (c) **surface
sketch** — Lua API shape consistent with §2.3, (d) **scope risk** — what
would force this back to "merge" or "drop".

Sketches are intentionally minimal. Each candidate that proceeds gets its
own detailed design doc.

§3.7 (existing-primitive gap) uses a slightly adapted shape because the
"why not KV / SQL" question does not apply to it; see the section for
details.

### 3.1 Vector — `std.vec.*`

- **Need**: retrieve previously stored items by **semantic similarity** to
  a query embedding (RAG, memory recall, deduplication of paraphrases).
- **Why not KV / SQL**: neither indexes high-dimensional float vectors;
  `WHERE cosine(...) > 0.8` is O(N) and untunable in SQLite.
- **Surface**:
  ```lua
  std.vec.put(ns, id, embedding, payload?)
  std.vec.query(ns, embedding, k?, filter?) -> [{id, score, payload}]
  std.vec.delete(ns, id)
  ```
- **Scope risk**: embedding generation is **out of scope** — caller passes
  the vector. If that boundary slides ("primitive should also call the
  embedding model"), the primitive grows into a RAG framework. Resist.

### 3.2 Rule Engine — `std.rule.*`

- **Need**: recursive / multi-hop reasoning over relations
  (transitive closure, dependency resolution, deductive enrichment).
- **Why not SQL**: SQLite's recursive CTE covers many cases. The justifying
  workloads are (i) rule sets large enough that program-as-data beats
  hand-written CTEs, and (ii) interactive rule editing where the agent
  itself authors rules.
- **Surface**:
  ```lua
  std.rule.assert(facts)         -- insert ground tuples
  std.rule.query(program) -> rows
  std.rule.retract(facts)
  ```
- **Scope risk**: high. If concrete workloads are not identified, this
  primitive should be **merged into SQL as a recursive-CTE pattern guide**
  rather than implemented as its own surface.

### 3.3 CRDT — `std.crdt.*`

- **Need**: multiple agents mutate shared state independently and merge
  later without a coordinator (offline-tolerant collaborative state).
- **Why not SQL**: SQLite is single-writer; merge conflicts have no
  built-in resolution semantics.
- **Surface**:
  ```lua
  local doc = std.crdt.doc(id)
  doc:apply(op)
  doc:snapshot() -> bytes
  doc:merge(remote_state)
  ```
- **Scope risk**: blocked on a concrete multi-agent scenario in
  agent-block. Without one, this is foundational infrastructure with no
  consumer; defer.

### 3.4 Object Store — `std.blob.*`

- **Need**: opaque large blobs (model artifacts, file dumps, multi-day
  archive) that exceed convenient SQLite row size and benefit from a
  separate lifecycle.
- **Why not KV / SQL**: KV values are intended for small cached payloads;
  SQLite blob columns work but conflate hot transactional state with cold
  archive.
- **Surface**:
  ```lua
  std.blob.put(key, bytes, opts?)
  std.blob.get(key) -> bytes
  std.blob.delete(key)
  std.blob.list(prefix) -> [key]
  ```
- **Backend**: local filesystem by default (`~/.agent-block/blob/`),
  optional S3-compatible backend via `AGENT_BLOCK_BLOB_BACKEND=s3` and
  `AGENT_BLOCK_BLOB_BUCKET=...`. The Lua surface is identical.
- **Scope risk**: feature creep into a full object store (versioning,
  multipart, signed URLs). v1 is **opaque key → bytes only**.

### 3.5 Inter-Agent Messaging — `std.imsg.*`

- **Need**: async event stream between agents (potentially across
  processes / hosts) — fire-and-forget pub-sub or durable queue.
- **Why not existing `std.bus`**: `std.bus` is the in-process EventBus
  used by the runtime itself; mixing inter-agent traffic into it would
  conflate scope. Existing `agent-mesh-sdk` (WebSocket relay) is RPC /
  control-plane, not broadcast messaging.
- **Surface** — instance of §2.6 shape (A); the conventional pub-sub
  verb `subscribe` is preserved as an alias for `on_message`:
  ```lua
  std.imsg.publish(topic, payload, opts={mode="ephemeral"|"durable"})
  std.imsg.on_message(topic, callback)            -- §2.6 shape (A)
  std.imsg.subscribe(topic, callback, opts?)      -- alias, pub-sub idiom
  ```
- **Backend**: pluggable via `AGENT_BLOCK_IMSG_BACKEND=nats|redis|...`.
  v1 ships **one** backend (likely NATS) and the surface is designed so
  others can be swapped without changing call sites.
- **Scope risk**: the "4-axis hybrid" framing (Kafka + NATS + Redis +
  Pulsar each for a different mode) is not a v1 commitment. v1 is one
  backend behind a stable surface; multi-backend is post-v1 if demand
  exists.

### 3.6 SQL Watch — `std.sql.watch(...)` (extension of #2)

- **Need**: react when a SQL query result set changes (live dashboard,
  trigger when an agent's task table reaches a state).
- **Why not a new primitive**: this is a **derived computation over
  existing SQL state**, not a new storage tier. Implementing it as a
  separate `std.live.*` primitive duplicates the storage and forces sync.
- **Surface** — instance of §2.6 shape (B):
  ```lua
  local handle = std.sql.watch(query, params?)
  handle:current() -> rows
  handle:on_change(callback)
  handle:close()
  ```
- **Implementation**: incremental view maintenance over the existing
  SQLite connection. v1 may polyfill via polling + diff; v2 can switch to
  true incremental dataflow without surface change.
- **Scope risk**: low — bounded by SQL's own surface.

### 3.7 MCP Resource Subscribe — `std.mcp.*` (existing primitive gap)

- **Need**: receive notifications when a remote MCP server's resource
  changes (file content, DB row, server-owned state). This is the
  primary "external state" source for an agent that does not own the
  data itself.
- **Why not other primitives**: storage / knowledge / coordination
  primitives all manage data the runtime owns. MCP resources are owned
  by an external server; the runtime is a **client subscriber**, not a
  data owner. Distinct concern, distinct primitive role.
- **Current state** (`src/bridge/mcp.rs`):
  - **implemented**: `mcp.list_resources(server)`,
    `mcp.read_resource(server, uri)` — one-shot read.
  - **implemented**: `mcp.on_progress`, `mcp.on_log` — event-callback
    pattern (§2.6 shape A) for progress / logging notifications.
  - **missing**: `resources/subscribe` request, handling of
    `notifications/resources/updated` and
    `notifications/resources/list_changed`,
    `notifications/tools/list_changed`,
    `notifications/prompts/list_changed`.
- **Surface** — instance of §2.6 shape (A) extended with
  per-URI subscribe / unsubscribe state:
  ```lua
  std.mcp.subscribe_resource(server, uri)         -- send resources/subscribe
  std.mcp.unsubscribe_resource(server, uri)
  std.mcp.on_resource_update(server, callback)    -- callback({uri, ...})
  std.mcp.on_resources_list_changed(server, callback)
  std.mcp.on_tools_list_changed(server, callback)
  std.mcp.on_prompts_list_changed(server, callback)
  ```
- **Why this is a "future" despite already living in `std.mcp.*`**: the
  §3 Catalog tracks **planned surface additions** as well as new
  primitives. This entry signals that the existing `std.mcp.*` namespace
  is expected to grow; it is not a new primitive.
- **Scope risk**: low for `subscribe_resource` / `on_resource_update`;
  the spec is well-defined. The `*_list_changed` group is optional per
  MCP spec — implement only when a consumer needs them.

### 3.8 TSDB — `std.ts.*`

**Status: implemented (v1)**

- **Need**: append-only time-series log with range + tag-filtered
  retrieval at KV-level ergonomics. Motivating uses: `journal.md`
  parallel machine-readable trace, agent run metrics
  (token / duration / cost), MCP notification log.
- **Why not other primitives**: `std.sql.*` can encode time-series but
  re-discovering the right schema / index / tag layout / agg query each
  time is the "SQLite As TSDB" trap. `std.kv.*` has no range or
  aggregation. A narrow, append-focused primitive carries its own
  contract so callers do not bikeshed.
- **Surface** (instance of §2 contract; SQLite-backed):
  ```lua
  std.ts.append(series, value, tags?, at?)
  std.ts.query(series, { from, to, tags?, agg?, bucket_ms? })
  std.ts.last(series, tags?)
  ```
- **Backend**: single SQLite file under `AGENT_BLOCK_HOME/ts.sqlite`
  (override via `AGENT_BLOCK_TS_PATH`, `:memory:` supported). One
  table `(series, ts, tags JSON, value JSON)` + index
  `(series, ts)`; tag filter via SQLite JSON1.
- **Non-goals**: high-frequency (>100 w/s) workloads, distributed /
  replication, PromQL-like DSL, automatic retention / downsampling
  (deferred to v2). Real TSDB engines (InfluxDB, Timescale) are out
  of scope; agent-host write rate is low enough that SQLite suffices.
- **Scope risk**: low. Same shape as `std.kv.*` / `std.sql.*` (§2),
  no new dependency, no MCP coupling.

### 3.9 Rejected / Deferred

- **CXL memory pool**: hardware-tier memory disaggregation is not
  addressable from userland Lua and depends on cloud-vendor capability.
  **Removed from the catalog.** Revisit only if a hardware path becomes
  practically reachable.
- **DFS-backed disaggregated state as its own primitive**: the storage
  side is subsumed by §3.4 (object store: large opaque blobs); the
  streaming-checkpoint machinery (Flink-style) is a consumer-block
  concern, not a primitive. Not a primitive.
- **Live materialized view as `std.live.*`**: subsumed by §3.6 as a SQL
  extension. Not a separate primitive.

## 4. Determinism and Layer Map

The catalog splits along a determinism axis, with five concrete layers:

- **Non-deterministic layer**: LLM calls, tool dispatch, agent reasoning.
  Out of scope of this doc.
- **Predictable layer**: §3.2 rule, §3.3 CRDT, §3.6 sql.watch. These give
  formal guarantees the LLM cannot.
- **Storage layer**: §2 KV, §2 SQL, §3.1 vec, §3.4 blob. Correctness is a
  property of the store, not of LLM output.
- **Communication layer**: §3.5 imsg.
- **External-source layer**: §3.7 MCP resource subscribe. Truth lives
  outside the runtime; the agent observes it.

When a consumer block needs a deterministic property — "this fact, once
asserted, will always be reachable by query" — it must use the predictable
layer, not the LLM. The boundary is **explicit at the primitive level**
(different `std.*` namespaces).

## 5. Opt-In Model

- **Default-ON**: §2 KV, §2 SQL. Lazy-init on first use; idle cost ~0.
- **Opt-in via Agent-side option** (§5.1): §3.1 vec, §3.2 rule, §3.3 crdt,
  §3.4 blob, §3.5 imsg. Each has non-trivial dep / startup / disk cost.
- **Always registered, errors-if-disabled**: the Lua side
  `std.<primitive>.*` table is always present. Calling a method on a
  disabled primitive returns an `unavailable` error rather than `nil`.
  This keeps discovery uniform and avoids `nil` checks in user code.

§3.6 `std.sql.watch` follows §2 SQL's enable state (no separate flag).
§3.7 MCP resource subscribe follows existing `std.mcp.*` enable state.

### 5.1 What "opt-in" means here

In the agent-block model, **one session = one process invocation** that
loads `.env` once at startup (`src/host.rs:280`). "Opt-in" therefore
primarily means **set an `AGENT_BLOCK_<NAME>_ENABLED=1` (or equivalent)
env entry in `.env` for that session**. No CLI flags (§2.2), no runtime
toggles.

This is an **Agent-side option** — the granularity is "this agent
invocation has this primitive available", not "this call site has it".

Out of scope of this doc:

- ConfigData unification (centralized config schema across primitives) —
  separate topic.
- Agent FeatureFlag system (richer per-agent capability gating, possibly
  with hot-reload) — separate topic. When that lands, the per-primitive
  enable convention here defers to it.

The current `AGENT_BLOCK_<NAME>_ENABLED=1` convention is an interim
shape that fits §2.2 ENV-driven and is replaceable when a richer system
exists.

## 6. Implementation Strategy

### 6.1 Order

Drive by use-case evidence, not by catalog order. The first candidate to
get implemented is the one with a concrete consumer in `blocks/*` or a
named external dependent. As of 0.11.1:

- **§3.7 MCP resource subscribe** is the highest-leverage near-term
  addition: the spec is fixed, the existing `std.mcp.*` plumbing is in
  place, and it unblocks any agent that wants to react to externally
  owned state.
- **§3.1 vec** has the clearest standalone use case (RAG / memory recall)
  and is implementable without other §3 primitives.
- **§3.6 sql.watch** is a small extension to an already-shipped primitive
  and unblocks downstream live-state patterns cheaply.
- **§3.4 blob** waits on a concrete large-object workload.
- **§3.5 imsg** waits on a concrete cross-agent scenario.
- **§3.3 crdt** waits on a concrete multi-agent collaborative scenario.
- **§3.2 rule** is on probation pending a workload that recursive CTEs
  cannot serve cleanly.

### 6.2 Consumer Adoption Rule

No primitive ships without at least one consumer. Currently no `blocks/*`
consumes `std.kv.*` or `std.sql.*` even though the primitives exist
(`Grep("\bkv\.|\bsql\." path=blocks)` → no matches as of 0.11.1). The
addition of any §3 primitive should be paired with at least one example
or block that demonstrates it.

### 6.3 mlua-batteries vs. In-Tree

Decided at implementation time per §2.5. No design-time commitments.

## 7. Open Questions

1. **Default-ON line**: §5 puts §3 primitives all opt-in. If `vec`
   becomes the dominant agent-state pattern, should it move default-ON?
   Cost: dep size, startup time. Defer until adoption signal exists.
2. **Tier movement**: hot (KV) → warm (SQL) → cold (blob) transitions are
   not exposed as a primitive. Is that the consumer's job, or should the
   surface offer migration helpers?
3. **Identity / reference**: do primitives reference each other by
   content hash (Unison-style) or by opaque ID? Affects §3.3 CRDT and
   §3.4 blob interoperation. Defer until two of them are implemented.
4. **Predictable-layer boundary**: today the boundary is implicit
   (different namespaces). Should it be a typed contract (e.g. "this
   value came from a deterministic primitive")? Probably no, but record
   the question.
5. **Capability-typed access**: should inter-component access be
   capability-typed (Cap'n Proto-style) instead of free function calls?
   Probably no for v1, but record.

## 8. Next Steps

1. Land this draft (β).
2. README — add a "State Primitives" section linking here and listing the
   current `std.kv` / `std.sql` surface (currently absent from the API
   tour at `README.md:90-160`).
3. When the first §3 candidate is greenlit, spawn its detailed design
   doc as `docs/architecture/agent-state-<name>.md`.

## References

- Existing primitive contract: `src/bridge/{kv,sql,config,mod}.rs`,
  `src/bridge/{kv_tools,sql_tools}.lua`, `src/host.rs:93-96`
- Existing architecture doc precedent: `docs/architecture/trace-context.md`
