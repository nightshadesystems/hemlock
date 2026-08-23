;
; Hemlock E1031 LED scan-chain PROBE program (LEDUP0).
;
; Purpose: map the SFP+ LED scan chain on the bench. Emits NUM_BITS bits
; per refresh, bit i taken from bit 0 of LED data RAM byte (0xA0 + i).
; With `led auto off`, nothing else writes the data RAM, so each byte is
; poked by hand from the diag shell and the faceplate observed:
;
;   led stop
;   led load e1031-led-probe.hex     (or: led prog <hex bytes>)
;   led auto off
;   led start
;   setreg CMIC_LEDUP0_DATA_RAM(0xa0) 1     ; chain bit 0 -> which LED?
;   setreg CMIC_LEDUP0_DATA_RAM(0xa0) 0
;   setreg CMIC_LEDUP0_DATA_RAM(0xa1) 1     ; chain bit 1 -> ...
;   ...
;
; Polarity note: on this board the chain appears active-low (idle/reset
; chain = all LEDs solid green), so all-zero data RAM should reproduce
; the familiar all-green state and poking a byte to 1 should DARKEN one
; LED. Record which offset darkens which cage, and whether it is the
; whole cage LED or one color of a bi-color pair.
;
; Assemble with the OpenBCM SM-Lite LED assembler (see README.md):
;   ledasm e1031-led-probe
;

NUM_BITS        equ     16      ; walk more bits than we expect LEDs
PORTDATA        equ     0xa0
TMP             equ     0xe0

update:
        ld      a,0
bitloop:
        ld      (TMP),a
        ld      b,PORTDATA
        add     b,a
        ld      b,(b)
        tst     b,0
        jc      bit_one
        pushst  ZERO
        pack
        jmp     next
bit_one:
        pushst  ONE
        pack
next:
        ld      a,(TMP)
        inc     a
        cmp     a,NUM_BITS
        jnz     bitloop
        send    NUM_BITS

; pushst constant sources
ZERO            equ     0xE     ; always 0
ONE             equ     0xF     ; always 1
