# WorkflowContext

Work Title: High Performance TLS Pool
Work ID: high-performance-tls-pool
Base Branch: main
Target Branch: feature/high-performance-tls-pool
Workflow Mode: full
Review Strategy: local
Review Policy: final-pr-only
Session Policy: continuous
Final Agent Review: enabled
Final Review Mode: society-of-thought
Final Review Interactive: smart
Final Review Models: none
Final Review Specialists: all
Final Review Interaction Mode: parallel
Final Review Specialist Models: none
Planning Docs Review: enabled
Planning Review Mode: multi-model
Planning Review Interactive: smart
Planning Review Models: gpt-5.5, gemini-3.1-pro-preview, claude-opus-4.8
Custom Workflow Instructions: none
Initial Prompt: Create a high performance thread pool based TLS library layered on top of the openssl crate. The main type is TlsPool, which creates TlsStream instances. Reads and writes delegate work to a thread pool when policy predicts a latency or throughput win, with completion reported back by callbacks from pool threads. The design should minimize lock contention, cross-thread sharing, memory allocations, sleeping workers, and unnecessary threads. It should default to no dependency on kimojio or kimojio-stack, with future integration layers for tokio, kimojio, and kimojio-stack. Baseline testing should start with rpc_write style tests using different client/server threads plus three-client/three-server scenarios.
Issue URL: none
Remote: origin
Artifact Lifecycle: commit-and-persist
Artifact Paths: auto-derived
Additional Inputs: FreeBSD kTLS investigation showed per-session worker affinity, complete-record readiness before decrypt offload, and asynchronous record queues are critical. Prior kimojio-stack-tls synchronous OpenSSL offload remained slower than inline because it waited for each offloaded operation and copied buffers.
