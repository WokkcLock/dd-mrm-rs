; thunk64.asm - DdHotapply frame hook thunk (ASCII comments only: ml64 reads codepage 936)
; Game frame function (sub_1403D93C0) entry 13 bytes are replaced with: jmp frame_thunk.
; Save ALL volatile registers (x64 ABI: RAX RCX RDX R8-R11 + XMM0-5), call C side
; frame_op (frame queue / apply), restore, then jump to frame_trampoline
; (trampoline = original 13-byte push prologue + jmp back to original+13, built at runtime by C).
; Stack: on entry [rsp]=return address, rsp%16==8; keep rsp%16==0 right before `call frame_op`.

.code
EXTERN frame_op:PROC
EXTERN frame_trampoline:PROC

frame_thunk PROC
    sub  rsp, 8                    ; alignment pad
    push rax
    push rcx
    push rdx
    push r8
    push r9
    push r10
    push r11
    sub  rsp, 78h                  ; xmm save area; now rsp%16==0
    movups [rsp+00h], xmm0
    movups [rsp+10h], xmm1
    movups [rsp+20h], xmm2
    movups [rsp+30h], xmm3
    movups [rsp+40h], xmm4
    movups [rsp+50h], xmm5
    call frame_op
    movups xmm5, [rsp+50h]
    movups xmm4, [rsp+40h]
    movups xmm3, [rsp+30h]
    movups xmm2, [rsp+20h]
    movups xmm1, [rsp+10h]
    movups xmm0, [rsp+00h]
    add  rsp, 78h
    pop  r11
    pop  r10
    pop  r9
    pop  r8
    pop  rdx
    pop  rcx
    pop  rax
    add  rsp, 8                    ; undo pad; [rsp]=original return address
    jmp  frame_trampoline          ; run original prologue 13 bytes -> back to original+13
frame_thunk ENDP

END
