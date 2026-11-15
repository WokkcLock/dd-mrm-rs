; thunk64.asm (v14): fs_get_file_info WRAPPER hook (0x247F30, 14-byte prologue).
;
; B-route DLL (E1c/E1d/E1e verified): the ONLY hook is fs_get_file_info
; (sub_140247F30). The game's own FS layer caches file sizes with session
; lifetime; after an external tool rewrites persist.game.json the cached size
; goes stale, and the next load path allocates by stale size then hits the
; string-length assert (heirarchical_file.cpp:718). Fix = after the ORIGINAL
; function returns, overwrite out+0x440 (the size/found field) with the true
; on-disk size when they disagree.
;
; Signature (from IDA, E1d): sub_140247F30(fs=this, nameBox, path, out)
;   rcx = fs root object
;   rdx = name box; the actual name char[] lives at nameBox+0x20
;        ("steam_profile_N" or "profile_N")
;   r8  = path char* ("persist.game.json" etc.)
;   r9  = out record (0x444 bytes); out+0x440 = u32 size, 0 = not found
;
; Frame (same proven shape as the old frame_thunk):
;   entry rsp%16==8 (game CALL pushed return address)
;     sub  rsp, 8                ; align
;     push rcx, rdx, r8, r9, r10, r11        ; volatile ints (0x30)
;     sub  rsp, 0x80            ; xmm save area + rax slot + spare
;   slot layout at rsp after sub: r11=[rsp+80h] r10=[+88h] r9=[+90h]
;   r8=[+98h] rdx=[+A0h] rcx=[+A8h]; [rsp+60h] = original rax slot
;   call r11 -> trampoline -> original function completes, returns here
;   save rax; forward entry args to fsfix_op(nameBox, path, out); restore.
; At 'call fsfix_op' rsp%16==0 (same arithmetic as frame_thunk v16b).

EXTERN fsfix_op:PROC
EXTERN g_trampoline_ptr:QWORD

PUBLIC fsfix_thunk
PUBLIC check_align

.DATA
check_align QWORD 8          ; documentation: entry rsp must be 8 mod 16

.CODE

fsfix_thunk PROC
    sub     rsp, 8
    push    rcx                           ; -> [rsp+0xA8] fs
    push    rdx                           ; -> [rsp+0xA0] name box
    push    r8                            ; -> [rsp+0x98] path
    push    r9                            ; -> [rsp+0x90] out record
    push    r10
    push    r11
    sub     rsp, 080h
    movups  [rsp], xmm0
    movups  [rsp+010h], xmm1
    movups  [rsp+020h], xmm2
    movups  [rsp+030h], xmm3
    movups  [rsp+040h], xmm4
    movups  [rsp+050h], xmm5

    mov     r11, [g_trampoline_ptr]       ; trampoline (indirect, A2024 lesson)
    call    r11                            ; run ORIGINAL fs_get_file_info

    mov     [rsp+060h], rax                ; keep original return value
    mov     rcx, [rsp+0A0h]                ; name box (fsfix_op reads +0x20)
    mov     rdx, [rsp+098h]                ; path char*
    mov     r8,  [rsp+090h]                ; out record
    call    fsfix_op
    mov     rax, [rsp+060h]                ; restore original return value

    movups  xmm0, [rsp]
    movups  xmm1, [rsp+010h]
    movups  xmm2, [rsp+020h]
    movups  xmm3, [rsp+030h]
    movups  xmm4, [rsp+040h]
    movups  xmm5, [rsp+050h]
    add     rsp, 080h
    pop     r11
    pop     r10
    pop     r9
    pop     r8
    pop     rdx
    pop     rcx
    add     rsp, 8
    ret
fsfix_thunk ENDP

END
