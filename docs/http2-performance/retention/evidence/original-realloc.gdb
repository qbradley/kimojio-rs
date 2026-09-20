set pagination off
set confirm off
set print frame-arguments none
set language c
break *_RNvCs1njKG4L9aB3_7___rustc14___rust_realloc if $rsi == 32 && $rdx == 4 && $rcx == 64
commands
silent
printf "REALLOC requested old=%lu align=%lu new=%lu\n", $rsi, $rdx, $rcx
bt 16
frame 12
print self->closed_stream_order
print self->limits.max_closed_stream_tombstones
continue
end
run
