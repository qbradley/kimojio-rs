set pagination off
set confirm off
set print frame-arguments none
set language c
break kimojio-fsm-http2/src/server/h2/endpoint.rs:576 if self->closed_stream_order.buf.inner.cap.__0 == 2048
commands
silent
printf "AFTER_EVICTION\n"
print self->closed_stream_order
print self->tombstones.base.table.table.bucket_mask
print self->tombstones.base.table.table.items
print self->limits.max_closed_stream_tombstones
bt 5
continue
end
break *_RNvCs1njKG4L9aB3_7___rustc12___rust_alloc if $rdi == 18448
commands
silent
printf "ALLOC requested bytes=%lu align=%lu\n", $rdi, $rsi
bt 24
continue
end
run
