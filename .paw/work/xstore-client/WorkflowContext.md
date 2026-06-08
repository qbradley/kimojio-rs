# WorkflowContext

Work Title: XStore Client
Work ID: xstore-client
Base Branch: stackful
Target Branch: stackful
Workflow Mode: full
Review Strategy: local
Review Policy: final-pr-only
Session Policy: continuous
Final Agent Review: enabled
Final Review Mode: society-of-thought
Final Review Interactive: smart
Final Review Models: ignored
Final Review Specialists: all
Final Review Interaction Mode: parallel
Final Review Specialist Models: none
Planning Docs Review: enabled
Planning Review Mode: multi-model
Planning Review Interactive: smart
Planning Review Models: gpt-5.5, gemini-3.1-pro-preview, claude-opus-4.8
Custom Workflow Instructions: none
Initial Prompt: Build a low-overhead XStore client using kimojio-stack-http. Requirements source: /tmp/xstore-low-overhead-client-prd.md. Do not mention the upstream product name in implementation artifacts or code. Use local PR strategy, Society-of-Thought final review, final-review-only pauses, no push, and conform to Kimojio design principles: no unnecessary allocations, low overhead, low latency, high performance, and no hidden costs.
Issue URL: none
Remote: origin
Artifact Lifecycle: commit-and-persist
Artifact Paths: auto-derived
Additional Inputs: Use jj locally on top of stackful/current change. Do not push anything. Avoid upstream product naming in implementation artifacts and code.
