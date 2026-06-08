# WorkflowContext

Work Title: gRPC Streaming
Work ID: grpc-streaming
Base Branch: stackful
Target Branch: feature/grpc-streaming
Workflow Mode: full
Review Strategy: local
Review Policy: final-pr-only
Session Policy: continuous
Final Agent Review: enabled
Final Review Mode: society-of-thought
Final Review Interactive: smart
Final Review Models: ignored (society-of-thought)
Final Review Specialists: all
Final Review Interaction Mode: parallel
Final Review Specialist Models: none
Planning Docs Review: enabled
Planning Review Mode: multi-model
Planning Review Interactive: smart
Planning Review Models: gpt-5.5, gemini-3.1-pro-preview, claude-opus-4.8
Custom Workflow Instructions: Preserve kimojio performance principles: avoid unnecessary memory allocations, minimize latency, preserve backpressure, avoid hidden background work or implicit buffering, and do not push or create remote PRs.
Initial Prompt: Add server-side streaming support for kimojio-stack-grpc using private feature guidance. Support unary request to ordered streaming response RPCs, long-lived response streams, async client consumption, async server stream production, HTTP/2 backpressure, cancellation detection, final status propagation, initial metadata propagation, and concurrent server-streaming RPCs. Do not implement client streaming, bidirectional streaming, custom per-message compression, or advanced streaming APIs beyond standard server-streaming semantics.
Issue URL: none
Remote: origin
Artifact Lifecycle: commit-and-persist
Artifact Paths: .paw/work/grpc-streaming/
Additional Inputs: private feature brief used only as implementation guidance; source names are intentionally omitted from workflow artifacts.
