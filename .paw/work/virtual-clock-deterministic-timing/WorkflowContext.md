# WorkflowContext

Work Title: Virtual Clock Deterministic Timing
Work ID: virtual-clock-deterministic-timing
Base Branch: main
Target Branch: feature/virtual-clock-deterministic-timing
Workflow Mode: full
Review Strategy: local
Review Policy: milestones
Session Policy: continuous
Final Agent Review: enabled
Final Review Mode: society-of-thought
Final Review Interactive: smart
Final Review Models: gpt-5.4, claude-opus-4.6, gemini-3-pro-preview
Final Review Specialists: all
Final Review Interaction Mode: parallel
Final Review Specialist Models: gpt-5.4, claude-opus-4.6, gemini-3-pro-preview
Planning Docs Review: enabled
Planning Review Mode: multi-model
Planning Review Interactive: smart
Planning Review Models: gpt-5.4, claude-opus-4.6, gemini-3-pro-preview
Custom Workflow Instructions: none
Initial Prompt: Implement deterministic virtual clock timing for kimojio runtime based on upstream design document. Key constraint: existing kimojio users must not need any code changes when upgrading - use type aliases and non-generic wrappers so the public API remains backward compatible.
Issue URL: none
Remote: origin
Artifact Lifecycle: commit-and-clean
Artifact Paths: auto-derived
Additional Inputs: Design document at /tmp/design.md
