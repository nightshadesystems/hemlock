;
; Hemlock E1031 SFP+ link-status LED program (LEDUP0).
;
; Drives the four SFP+ cage LEDs (Ethernet49-52 / xe0-xe3) from link
; state: green when the link is up, dark otherwise. The 48 copper ports'
; LEDs are PHY-driven and are not on this scan chain.
;
; Link state comes from bit 0 of LED data RAM byte (PORTDATA + physical
; port number), maintained by the SDK linkscan when `led auto on` is set
; (the LED processor's own LINKUP bit is not reliable under software
; linkscan — see sdk56334.asm in the OpenBCM examples).
;
; !! BENCH-VERIFY BEFORE PRODUCTIZING (docs/e1031-led-bringup.md):
;   - XE*_PORT: the physical port numbers linkscan uses for xe0-3
;     (diag `ps` / `led status` rows; expected 50-53 on this config).
;   - Emission order: which cage the first emitted bit lands on; swap
;     the call order below to match the probe program's findings.
;   - CHAIN_BITS: actual scan chain length from the probe.
;   - Polarity: ONE=dark/ZERO=green assumed (active-low chain).
;
; Load sequence (sai_postinit_cmd.soc once verified):
;   led load e1031-sfp-link.hex
;   led auto on
;   led start
;
; Assemble with the OpenBCM SM-Lite LED assembler (see README.md):
;   ledasm e1031-sfp-link
;

XE0_PORT        equ     50
XE1_PORT        equ     51
XE2_PORT        equ     52
XE3_PORT        equ     53

CHAIN_BITS      equ     4
PORTDATA        equ     0xa0

update:
        ld      a,XE0_PORT
        call    led_link
        ld      a,XE1_PORT
        call    led_link
        ld      a,XE2_PORT
        call    led_link
        ld      a,XE3_PORT
        call    led_link
        send    CHAIN_BITS

;
; led_link: emit one LED bit for the physical port in register a.
; Carry <- bit 0 of PORTDATA[port]; active-low output.
;
led_link:
        ld      b,PORTDATA
        add     b,a
        ld      b,(b)
        tst     b,0
        jc      led_green
        pushst  ONE             ; link down -> dark
        pack
        ret
led_green:
        pushst  ZERO            ; link up -> green
        pack
        ret

; pushst constant sources
ZERO            equ     0xE     ; always 0
ONE             equ     0xF     ; always 1
