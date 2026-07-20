# Planning documents

Historical planning and architecture documents for the haptic table project, copied here from the Claude.ai "Haptic Table" project knowledge (2026-07) so that all tooling — Claude Code, Cowork, and humans — reads the same sources. These are frozen reference material; current state and priorities live in `/ROADMAP.md`.

Reading order and provenance:

1. **project-outline.md** — the research vision: haptic phenomenology, abstract stimulus spaces, the 2D wave-model question. Names the real-time wave model (e.g. phasor representation) as the top research priority.
2. **implementation-plan-single-plugin.md** — earliest plan: everything in a single VST plugin using the `vst` crate. Superseded by the hybrid architecture; retained for its phase breakdown and success metrics.
3. **static-allocation-architecture.txt** — Rust design sketch for the stimulus engine (ADSR, ParameterMapping, SpatialSweep, ChaoticNetwork, TransducerConfig). Ancestor of the current engine; several of its richer subsystems are still unimplemented and remain on the roadmap.
4. **hybrid-architecture-plan.md** — requirements and plan for the plugin + server split. First statement of the architecture the repo now follows.
5. **minimal-prototype-implementation-guide.md** — the concrete implementation handoff the initial codebase was built from (previously at repo root as `haptic-vst-minimal-prototype.md`).
