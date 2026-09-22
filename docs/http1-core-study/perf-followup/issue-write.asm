
target/release/deps/roundtrip-f8b1e690de642ca9:     file format elf64-x86-64


Disassembly of section .text:

000000000008b930 <<kimojio_fsm_http1::connection::Core<alloc::vec::Vec<u8>, &[u8], true>>::issue_write::<roundtrip::support::Transport<false, false, roundtrip::support::input::ReplayInput>>>:
   8b930:	55                   	push   %rbp
   8b931:	41 57                	push   %r15
   8b933:	41 56                	push   %r14
   8b935:	41 55                	push   %r13
   8b937:	41 54                	push   %r12
   8b939:	53                   	push   %rbx
   8b93a:	48 81 ec 58 01 00 00 	sub    $0x158,%rsp
   8b941:	4c 8b bf 08 01 00 00 	mov    0x108(%rdi),%r15
   8b948:	49 83 ff 01          	cmp    $0x1,%r15
   8b94c:	75 79                	jne    8b9c7 <<kimojio_fsm_http1::connection::Core<alloc::vec::Vec<u8>, &[u8], true>>::issue_write::<roundtrip::support::Transport<false, false, roundtrip::support::input::ReplayInput>>+0x97>
   8b94e:	48 8b 87 80 03 00 00 	mov    0x380(%rdi),%rax
   8b955:	48 83 f8 fe          	cmp    $0xfffffffffffffffe,%rax
   8b959:	0f 83 24 01 00 00    	jae    8ba83 <<kimojio_fsm_http1::connection::Core<alloc::vec::Vec<u8>, &[u8], true>>::issue_write::<roundtrip::support::Transport<false, false, roundtrip::support::input::ReplayInput>>+0x153>
   8b95f:	48 ff c0             	inc    %rax
   8b962:	48 89 87 80 03 00 00 	mov    %rax,0x380(%rdi)
   8b969:	48 8b 8f 70 03 00 00 	mov    0x370(%rdi),%rcx
   8b970:	48 8b 97 78 03 00 00 	mov    0x378(%rdi),%rdx
   8b977:	48 c7 87 08 01 00 00 	movq   $0x2,0x108(%rdi)
   8b97e:	02 00 00 00
   8b982:	48 89 8f 10 01 00 00 	mov    %rcx,0x110(%rdi)
   8b989:	48 89 97 18 01 00 00 	mov    %rdx,0x118(%rdi)
   8b990:	48 89 87 20 01 00 00 	mov    %rax,0x120(%rdi)
   8b997:	c6 87 28 01 00 00 03 	movb   $0x3,0x128(%rdi)
   8b99e:	48 89 4c 24 08       	mov    %rcx,0x8(%rsp)
   8b9a3:	48 89 54 24 10       	mov    %rdx,0x10(%rsp)
   8b9a8:	48 89 44 24 18       	mov    %rax,0x18(%rsp)
   8b9ad:	c6 44 24 20 03       	movb   $0x3,0x20(%rsp)
   8b9b2:	c6 44 24 28 01       	movb   $0x1,0x28(%rsp)
   8b9b7:	48 8d 44 24 08       	lea    0x8(%rsp),%rax
   8b9bc:	48 89 f7             	mov    %rsi,%rdi
   8b9bf:	48 89 c6             	mov    %rax,%rsi
   8b9c2:	e8 f9 a0 03 00       	call   c5ac0 <<roundtrip::support::Transport<false, false, roundtrip::support::input::ReplayInput> as kimojio_fsm_http1::Ports<alloc::vec::Vec<u8>, &[u8]>>::readiness>
   8b9c7:	4c 8b b7 80 03 00 00 	mov    0x380(%rdi),%r14
   8b9ce:	49 83 fe fe          	cmp    $0xfffffffffffffffe,%r14
   8b9d2:	0f 83 ab 00 00 00    	jae    8ba83 <<kimojio_fsm_http1::connection::Core<alloc::vec::Vec<u8>, &[u8], true>>::issue_write::<roundtrip::support::Transport<false, false, roundtrip::support::input::ReplayInput>>+0x153>
   8b9d8:	49 ff c6             	inc    %r14
   8b9db:	4c 89 b7 80 03 00 00 	mov    %r14,0x380(%rdi)
   8b9e2:	4c 8b a7 70 03 00 00 	mov    0x370(%rdi),%r12
   8b9e9:	4c 8b af 78 03 00 00 	mov    0x378(%rdi),%r13
   8b9f0:	0f b6 af 88 02 00 00 	movzbl 0x288(%rdi),%ebp
   8b9f7:	c6 87 88 02 00 00 ff 	movb   $0xff,0x288(%rdi)
   8b9fe:	81 fd ff 00 00 00    	cmp    $0xff,%ebp
   8ba04:	0f 84 ef 00 00 00    	je     8baf9 <<kimojio_fsm_http1::connection::Core<alloc::vec::Vec<u8>, &[u8], true>>::issue_write::<roundtrip::support::Transport<false, false, roundtrip::support::input::ReplayInput>>+0x1c9>
   8ba0a:	48 89 34 24          	mov    %rsi,(%rsp)
   8ba0e:	48 89 fb             	mov    %rdi,%rbx
   8ba11:	48 8d b7 89 02 00 00 	lea    0x289(%rdi),%rsi
   8ba18:	40 88 ac 24 b0 00 00 	mov    %bpl,0xb0(%rsp)
   8ba1f:	00
   8ba20:	48 8d bc 24 b1 00 00 	lea    0xb1(%rsp),%rdi
   8ba27:	00
   8ba28:	ba a7 00 00 00       	mov    $0xa7,%edx
   8ba2d:	ff 15 2d 86 21 00    	call   *0x21862d(%rip)        # 2a4060 <memcpy@GLIBC_2.14>
   8ba33:	4c 89 a4 24 30 01 00 	mov    %r12,0x130(%rsp)
   8ba3a:	00
   8ba3b:	4c 89 ac 24 38 01 00 	mov    %r13,0x138(%rsp)
   8ba42:	00
   8ba43:	4c 89 b4 24 40 01 00 	mov    %r14,0x140(%rsp)
   8ba4a:	00
   8ba4b:	c6 84 24 48 01 00 00 	movb   $0x1,0x148(%rsp)
   8ba52:	01
   8ba53:	4d 85 ff             	test   %r15,%r15
   8ba56:	74 51                	je     8baa9 <<kimojio_fsm_http1::connection::Core<alloc::vec::Vec<u8>, &[u8], true>>::issue_write::<roundtrip::support::Transport<false, false, roundtrip::support::input::ReplayInput>>+0x179>
   8ba58:	48 8d 9c 24 10 01 00 	lea    0x110(%rsp),%rbx
   8ba5f:	00
   8ba60:	4c 8d b4 24 b8 00 00 	lea    0xb8(%rsp),%r14
   8ba67:	00
   8ba68:	48 8d 3d 11 5d fa ff 	lea    -0x5a2ef(%rip),%rdi        # 31780 <anon.c1601f3057c44623357cf0bc09bf6773.67.llvm.16334870102619885125+0x4>
   8ba6f:	48 8d 15 52 bf 20 00 	lea    0x20bf52(%rip),%rdx        # 2979c8 <__frame_dummy_init_array_entry+0xc8>
   8ba76:	be 03 01 00 00       	mov    $0x103,%esi
   8ba7b:	ff 15 e7 85 21 00    	call   *0x2185e7(%rip)        # 2a4068 <_DYNAMIC+0x258>
   8ba81:	0f 0b                	ud2
   8ba83:	c7 44 24 08 0c 00 00 	movl   $0xc,0x8(%rsp)
   8ba8a:	00
   8ba8b:	48 8d 74 24 08       	lea    0x8(%rsp),%rsi
   8ba90:	e8 9b 7e 02 00       	call   b3930 <<kimojio_fsm_http1::connection::Core<alloc::vec::Vec<u8>, &[u8], true>>::fail>
   8ba95:	31 c0                	xor    %eax,%eax
   8ba97:	48 81 c4 58 01 00 00 	add    $0x158,%rsp
   8ba9e:	5b                   	pop    %rbx
   8ba9f:	41 5c                	pop    %r12
   8baa1:	41 5d                	pop    %r13
   8baa3:	41 5e                	pop    %r14
   8baa5:	41 5f                	pop    %r15
   8baa7:	5d                   	pop    %rbp
   8baa8:	c3                   	ret
   8baa9:	48 c7 83 08 01 00 00 	movq   $0x2,0x108(%rbx)
   8bab0:	02 00 00 00
   8bab4:	4c 89 a3 10 01 00 00 	mov    %r12,0x110(%rbx)
   8babb:	4c 89 ab 18 01 00 00 	mov    %r13,0x118(%rbx)
   8bac2:	4c 89 b3 20 01 00 00 	mov    %r14,0x120(%rbx)
   8bac9:	c6 83 28 01 00 00 01 	movb   $0x1,0x128(%rbx)
   8bad0:	4c 8d 74 24 08       	lea    0x8(%rsp),%r14
   8bad5:	48 8d b4 24 b0 00 00 	lea    0xb0(%rsp),%rsi
   8badc:	00
   8badd:	ba a8 00 00 00       	mov    $0xa8,%edx
   8bae2:	4c 89 f7             	mov    %r14,%rdi
   8bae5:	ff 15 75 85 21 00    	call   *0x218575(%rip)        # 2a4060 <memcpy@GLIBC_2.14>
   8baeb:	48 8b 3c 24          	mov    (%rsp),%rdi
   8baef:	4c 89 f6             	mov    %r14,%rsi
   8baf2:	e8 19 9d 03 00       	call   c5810 <<roundtrip::support::Transport<false, false, roundtrip::support::input::ReplayInput> as kimojio_fsm_http1::Ports<alloc::vec::Vec<u8>, &[u8]>>::write>
   8baf7:	eb 9e                	jmp    8ba97 <<kimojio_fsm_http1::connection::Core<alloc::vec::Vec<u8>, &[u8], true>>::issue_write::<roundtrip::support::Transport<false, false, roundtrip::support::input::ReplayInput>>+0x167>
   8baf9:	48 8d 3d e0 be 20 00 	lea    0x20bee0(%rip),%rdi        # 2979e0 <__frame_dummy_init_array_entry+0xe0>
   8bb00:	ff 15 6a 85 21 00    	call   *0x21856a(%rip)        # 2a4070 <_DYNAMIC+0x260>
   8bb06:	83 fd 01             	cmp    $0x1,%ebp
   8bb09:	74 07                	je     8bb12 <<kimojio_fsm_http1::connection::Core<alloc::vec::Vec<u8>, &[u8], true>>::issue_write::<roundtrip::support::Transport<false, false, roundtrip::support::input::ReplayInput>>+0x1e2>
   8bb0b:	4c 89 f3             	mov    %r14,%rbx
   8bb0e:	85 ed                	test   %ebp,%ebp
   8bb10:	75 1d                	jne    8bb2f <<kimojio_fsm_http1::connection::Core<alloc::vec::Vec<u8>, &[u8], true>>::issue_write::<roundtrip::support::Transport<false, false, roundtrip::support::input::ReplayInput>>+0x1ff>
   8bb12:	48 8b 33             	mov    (%rbx),%rsi
   8bb15:	48 85 f6             	test   %rsi,%rsi
   8bb18:	74 15                	je     8bb2f <<kimojio_fsm_http1::connection::Core<alloc::vec::Vec<u8>, &[u8], true>>::issue_write::<roundtrip::support::Transport<false, false, roundtrip::support::input::ReplayInput>>+0x1ff>
   8bb1a:	48 8b 7b 08          	mov    0x8(%rbx),%rdi
   8bb1e:	ba 01 00 00 00       	mov    $0x1,%edx
   8bb23:	48 89 c3             	mov    %rax,%rbx
   8bb26:	ff 15 4c 85 21 00    	call   *0x21854c(%rip)        # 2a4078 <_DYNAMIC+0x268>
   8bb2c:	48 89 d8             	mov    %rbx,%rax
   8bb2f:	48 89 c7             	mov    %rax,%rdi
   8bb32:	e8 69 ad 20 00       	call   2968a0 <_Unwind_Resume@plt>
   8bb37:	cc                   	int3
   8bb38:	cc                   	int3
   8bb39:	cc                   	int3
   8bb3a:	cc                   	int3
   8bb3b:	cc                   	int3
   8bb3c:	cc                   	int3
   8bb3d:	cc                   	int3
   8bb3e:	cc                   	int3
   8bb3f:	cc                   	int3
