# WorkflowContext

Work Title: gRPC Client Server Support
Work ID: grpc-client-server-support
Base Branch: main
Target Branch: feature/grpc-client-server-support
Workflow Mode: full
Review Strategy: local
Review Policy: milestones
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
Planning Review Models: gpt-5.5, claude-opus-4.8, gemini-3.1-pro-preview
Custom Workflow Instructions: Use a local-only jj workflow. Do not push. Prefer jj local changes over Git branch operations where the repository is in a detached jj workspace. Use Society of Thought review for self review.
Initial Prompt: Create a PAW workflow to implement gRPC client and server support. These can be inspired by tonic and reqwest crates, but obviously cannot re-use them because of tokio dependency. However if they depend on crates that do not bake in async runtime and we can re-use those same crates ourselves, we should do so. In other words, build on the shoulders of giants as much as possible. One crate will be kimojio-stack-http which should support HTTP/1 and HTTP/2 client and server, with TLS and without TLS if it is not much extra work. Then utilize that to layer on a new crate, kimojio-stack-grpc for grpc client and server. These crates should follow kimojio philosophy: minimize overhead, minimize allocations, and favor high performance and low latency programming models. It is acceptable if the API is less convenient as long as it is usable. We want lowest possible overhead, lowest possible latency, and most predictability. When throughput conflicts with latency, favor latency while still getting the best possible throughput. Validate implementations by using tokio based client and server to pair with kimojio-stack client and server for integration testing. Use Society of Thought review for self review, and local PR strategy. Do not push anything; use jj to make changes locally.
Issue URL: none
Remote: origin
Artifact Lifecycle: commit-and-persist
Artifact Paths: auto-derived
Additional Inputs: The repository is currently in a detached jj workspace. PAW Git branch checkout was intentionally skipped in favor of local jj changes per user request.
