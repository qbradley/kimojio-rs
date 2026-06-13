# WorkflowContext

Work Title: Cross Thread Waker
Work ID: cross-thread-waker
Base Branch: main
Target Branch: feature/cross-thread-waker
Workflow Mode: full
Review Strategy: prs
Review Policy: milestones
Session Policy: continuous
Final Agent Review: enabled
Final Review Mode: multi-model
Final Review Interactive: smart
Final Review Models: gpt-5.2, gemini-3-pro-preview, claude-opus-4.6
Final Review Specialists: all
Final Review Interaction Mode: parallel
Final Review Specialist Models: none
Planning Docs Review: enabled
Planning Review Mode: multi-model
Planning Review Interactive: smart
Planning Review Models: gpt-5.2, gemini-3-pro-preview, claude-opus-4.6
Custom Workflow Instructions: none
Initial Prompt: Allow Waker to work correctly from any thread. On the local thread, no locks or atomic operations. From other threads, use a signaling mechanism to wake the correct thread and send task identity. Use thread_id from TaskState for cheap same-thread checks and tasks HandleTable index instead of Rc<Task>.
Issue URL: none
Remote: origin
Artifact Lifecycle: commit-and-clean
Artifact Paths: auto-derived
Additional Inputs: none
