# Future kimojio-stack io_uring Work

The current `kimojio-stack` io_uring layer now covers basic file/socket I/O,
registered fd/buffer I/O, vectored I/O, splice/tee, polling/epoll control,
timeouts/cancellation, futex wait/wake, `openat2`, `statx`, `ftruncate`, and
`UringCmd80`.

Remaining `rustix-uring` features to consider later:

- Provided-buffer and multishot I/O: `ProvideBuffers`, `RemoveBuffers`,
  `register_buf_ring`, `RecvMulti`, `RecvMsgMulti`, `AcceptMulti`,
  `SendBundle`, `RecvBundle`, and `RecvMultiBundle`.
- Zero-copy networking: `SendZc`, `SendMsgZc`, and completion notification
  handling through `IORING_CQE_F_NOTIF`.
- Inter-ring messaging: `MsgRingData` and `MsgRingSendFd`.
- Additional fixed-resource management: async `FilesUpdate`, `FixedFdInstall`,
  resource tags, bulk fixed-file/fixed-buffer registration, and explicit
  unregister-all APIs.
- Ring setup and submission tuning: SQPOLL, IOPOLL, cooperative/deferred
  taskrun, single-issuer mode, custom CQ sizing, submit-all, restrictions,
  eventfd notifications, personalities, IOWQ affinity, and IOWQ worker limits.
- Per-SQE behavior flags not yet exposed in high-level APIs: `IO_DRAIN`,
  `IO_LINK`, `IO_HARDLINK`, `ASYNC`, `BUFFER_SELECT`, `SKIP_SUCCESS`,
  personalities, per-operation `ioprio`, and more `buf_group`/`file_index`
  variants.
- Rich CQE handling: selected buffer IDs, `MORE`, `SOCK_NONEMPTY`, `NOTIF`,
  `RecvMsgOut`, and 32-byte CQEs.
- Other lower-level operations: `UringCmd16` and broader safe abstractions for
  device-specific uring commands.
