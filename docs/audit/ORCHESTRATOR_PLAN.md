# Nexus-LLM Orchestrator & AI Network Roadmap

## Executive Summary
This document outlines the phased plan to evolve the Nexus-LLM distributed inference engine into a true local AI network. The existing foundational layers—discovery, Ed25519 trust, placement intelligence, and content-addressed model transfer—are mature. This roadmap builds the orchestration, multi-model execution, and knowledge layers necessary to distribute intelligent tasks across personal hardware.

---

## 1. Orchestrator Hardware Profile & Model Selection
Targeting the baseline constraints (Core 2 Duo, 4 GB RAM / ~2.5 GB usable budget):

* **Model Sizing**: The orchestrator acts as a constrained classifier and router rather than a general reasoning engine. It emits strict JSON decisions validated by the rust application.
* **Primary Recommendation**: **Qwen3-1.7B (Q4_K_M)** (~1.2 GB footprint). Leaves sufficient headroom for a 4k context and provides excellent instruction-following at minimal cost.
* **Secondary Recommendation**: **Llama 3.2 3B (Q4_K_M)** (~2 GB). Offers the best function-calling reliability at the 3B scale, provided the tight RAM constraints permit a smaller context window.
* **Design Rule**: The application code always enforces the "veto." The model predicts a route; the system validates it against known capabilities. Hallucinations trigger fallback to deterministic routing.

---

## 2. Phased Implementation Plan

The architecture naturally extends into four discrete phases, building directly on the shipped Phase 11 placement intelligence.

### Phase 12: Multi-Instance Supervisor & State Advertising
*Enable one capable node to host multiple specialized models concurrently.*

* **Multi-Slot Supervisor**: Refactor `SupervisorManager` from a single `Option<ProcessSupervisor>` to an `Arc<Mutex<HashMap<SlotId, ProcessSupervisor>>>`. Allocate a distinct port (e.g., 8080, 8081) per slot.
* **Budget Tracking**: Modify load checks to validate against the *remaining* RAM budget (`allocatable − Σ active footprints`), using the existing `MemoryPlan` logic.
* **Loaded Model State**: Extend the authenticated `/nexus/control/v1/state` payload. Nodes must report active models with relevant tags (`["coder", "general", "vision"]`), specific endpoints, and remaining context size.
* **Cluster UI**: Update the Cluster tab to present available *services*, not just connected hardware nodes.

### Phase 13: The Orchestrator Router & Agent Bus
*Establish intelligent request routing and model-to-model communication.*

* **Two-Tier Router (`src/router.rs`)**:
  * **Tier 1 (Deterministic)**: Keyword, tag, or preset matching routes directly to the highest-scored node (utilizing `rank_execution_plans` + `LinkQuality`). Operates without LLM overhead.
  * **Tier 2 (Orchestrator-Assisted)**: Ambiguous prompts are routed to the orchestrator model for classification. The orchestrator returns a strictly typed `OrchestratorChoice` JSON blob (route, reason, rewritten prompt). Parsing failures safely degrade to Tier 1.
* **Model-to-Model Bus**: Implement a signed `POST /nexus/control/v1/agent/message` endpoint. Allows models to dispatch discrete tasks (e.g., "summarize log") to specialized peers and await a webhook reply.
* **Task State Management**: Store active task tracking in a durable `tasks.json` alongside the model store.

### Phase 14: Knowledge Base & RAG Capabilities
*Introduce persistent shared memory across the mesh.*

* **Embedded Structured Store**: Implement a lightweight, single-file database (`rusqlite` or `redb`) at `~/.nexus/kb/` on each node.
* **Data Model**: Store content-addressed (SHA-256) document chunks, versioned instruction personas, and distilled episodic memory (logs, summaries, entities).
* **Distributed Embeddings**: Expose a small embedding model (e.g., Qwen3-Embedding-0.6B) as a standard node capability (`capabilities: ["embeddings"]`).
* **Retrieval Service**: Implement in-process vector search (e.g., via `sqlite-vec`). The Tier-2 Router dynamically injects retrieved chunk context into outbound prompts.

### Phase 15: Background Learning & State Sync
*Automate knowledge sharing and session memory distillation.*

* **LAN Gossip Protocol**: Sync KB documents over the existing authenticated blob transfer channels. Employ vector clocks or simple last-writer-wins logic with node-ID tiebreakers.
* **Janitor Agent Task**: On a scheduled or end-of-session basis, the orchestrator distills recent session logs, extracts durable facts/preferences, generates embeddings, and writes the provenanced results to the KB via signed control-plane requests.
* **Agents UI Tab (F6)**: Introduce a dedicated dashboard visualizing the agent bus: task lists, route status, orchestrator decisions, and active inter-node token streams.

---

## 3. Terminal UI (TUI) Improvements
*Immediate high-value enhancements to the operator experience, built in parallel with Phases 12-15.* [ALL IMPLEMENTED & VERIFIED]

1. **Activate Tunnel View** [DONE]: Wired `tunnel_view.rs` to F5 hotkey (`HubTab::Tunnel`).
2. **Persistent Status Bar** [DONE]: 2-line persistent dock displaying model name, active host endpoint, live `tok/s`, a 10-bar context-usage fill gauge (`[███░░░░░░░] 512/4096 (12%)`), status badge, and global hotkeys across all tabs.
3. **Cluster Link Quality Visuals** [DONE]: Surface `rank.rs` link quality (RTT, throughput, `⚡ 3.0ms · 50MB/s`) as a dedicated column in the peers table and detailed probe breakdown card in the inspector modal.
4. **Command Palette** [DONE]: `Ctrl+P` modal with zero-dependency prefix and word-boundary fuzzy ranker indexing tabs, local models, mesh peers, hub actions, and slash commands.
5. **Log Tab** [DONE]: Dedicated `HubTab::Logs` (`[F7] 📜 Logs`) in `logs_view.rs` streaming directly from `~/.nexus/logs/` with level filtering (`ALL`/`INFO`/`WARN`/`ERROR`), search query buffer, auto-tail follow toggling (`Space`), and scroll navigation (`j`/`k`).
6. **Consistent Badging** [DONE]: Centralized `badges.rs` standardizing accessible bracketed text indicators (`[OK]`, `[WARN]`, `[FAIL]`, `[RPC]`, `[OOM]`, `[LOCAL]`, `[READY]`, `[STREAM]`) with semantic Ratatui color styles.

---

## 4. Codebase Hygiene & Technical Debt

* **Cleanup Dead Code**: Either wire up or remove inert configuration keys (`runtime_role`, `capabilities`, `mlock`, `fallback_to_cpu`) and unreachable code paths (`preset.rs` chat engine) to reduce contributor confusion.
* **Beacon Authentication**: Upgrade discovery beacons from CRC-only to authenticated payloads signed with the node's Ed25519 identity key.
* **Integration Testing**: Implement a mock `llama-server` HTTP stub to run full load/chat/unload tests through `control_plane_server` and `SupervisorManager`.
* **Request Concurrency**: Add per-endpoint request queues with backpressure to `LlmClient` to safely handle multiple concurrent models and clients.
* **Documentation**: Refresh the `README.md` to reflect the actual file structure (`store.rs`, `trust_auth.rs`, `node_identity.rs`) and the true scope of the test suite (109+ tests). Ensure AVX disabling in `.cargo/config.toml` is scoped only to legacy targets.
