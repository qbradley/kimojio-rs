# WorkflowContext

Work Title: Stack OpenTelemetry
Work ID: stack-opentelemetry
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
Initial Prompt: We want a low level API for metrics and logs compatible with OpenTelemetry. For logs this is a gRPC endpoint of some kind so we need client library. We don't need the higher level conveniences yet, just a simple way to be able to connect and emit logs. I'm not sure what API is for metrics but maybe gRPC too. Perhaps do integration testing using some kind of docker container running an opentelemetry compatible destination to ensure you are able to emit logs and metrics for integration testing. Make a new crate for this kimojio-stack-opentelemetry. We want to abide by kimojio principles: no unnecessary allocations, low overhead, low latency, high performance, and no hidden costs.
Issue URL: none
Remote: origin
Artifact Lifecycle: commit-and-persist
Artifact Paths: auto-derived
Additional Inputs: Use jj locally on top of stackful. Do not push anything. There is already an empty change ready.
