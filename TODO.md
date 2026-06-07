- There is a function that you can call that invokes a callback on each stackful thread. It is like scheduling each thread but not to run the regular work, to run some other work. Use this to implement a "dump stacks" function that uses backtrace crate or similar to dump the callstack of each stackful thread.
- Stack introspection - add ability to see how much stack you are using, or what the "high water mark" was, and also the ability to tune the stack size when spawning.
- gRPC client and server
- http client and server (over TLS)
- Add polling mode (BusyPoll::Never, Always, or suspend after some idle time)
- opentelemetry integration (logging and metrics)
- panic behavior on nested threads (should they immediately propagate up?)
- cancelation behavior (cancel I/O when scope exits)

