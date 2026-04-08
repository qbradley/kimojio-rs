# WorkflowContext

Work Title: Idle Advance Callback
Work ID: idle-advance-callback
Base Branch: feature/virtual-clock-deterministic-timing
Target Branch: feature/idle-advance-callback
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
Initial Prompt: Replace virtual_clock_advance_idle(Duration) and virtual_clock_advance_idle_default(Option<Duration>) with a single FnMut(Option<Instant>) -> Option<Duration> callback that the runtime calls at idle points. The callback receives the next pending timer deadline (if any) so callers can implement patterns like "advance to next timer" trivially. This collapses three APIs into one, is strictly more powerful, and simplifies the design.
Issue URL: none
Remote: origin
Artifact Lifecycle: commit-and-clean
Artifact Paths: auto-derived
Additional Inputs: none
